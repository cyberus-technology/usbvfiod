# This file contains integration tests for usbvfiod.
{ lib, pkgs, usbvfiod }:
let
  # For the VM that we start in Cloud Hypervisor, we re-use the netboot image.
  netbootNixos = lib.nixosSystem {
    inherit (pkgs) system;

    modules = [
      "${pkgs.path}/nixos/modules/installer/netboot/netboot-minimal.nix"

      # Cloud Hypervisor Guest Convenience
      ({ config, ... }: {

        boot.kernelParams = [
          # Use the serial console for kernel output.
          #
          # The virtio-console is an option as well, but is not
          # compiled into the NixOS kernel and would be inconvenient.
          "console=ttyS0"
          # Enable dyndbg messages for the XHCI driver.
          "xhci_pci.dyndbg==pmfl"
          "xhci_hcd.dyndbg==pmfl"
        ];

        # Enable debug verbosity.
        boot.consoleLogLevel = 8;

        # Convenience packages for interactive use
        environment.systemPackages = [ pkgs.pciutils pkgs.usbutils ];

        # Add user services that run on automatic login.
        systemd.user.services = {
          dump-diagnostics = {
            description = "Dump diagnostic information";
            wantedBy = [ "default.target" ];

            serviceConfig = {
              ExecStart = pkgs.writeShellScript "diagnostic-dump" ''
                echo Dumping Diagnostics
                cat /proc/interrupts
              '';
              StandardOutput = "journal+console";
              StandardError = "journal+console";
            };
          };
        };

        # Silence the useless stateVersion warning. We have no state to keep.
        system.stateVersion = config.system.nixos.release;
      })
    ];
  };

  netboot =
    let
      inherit (netbootNixos) config;

      kernelTarget = pkgs.stdenv.hostPlatform.linux-kernel.target;
    in
    {
      initrd = "${config.system.build.netbootRamdisk}/initrd";
      kernel = "${config.system.build.kernel}/${kernelTarget}";
      cmdline = "init=${config.system.build.toplevel}/init "
        + builtins.toString config.boot.kernelParams;
    };

  # Putting the socket in a world-readable location is obviously not a
  # good choice for a production setup, but for this test it works
  # well.
  usbvfiodSocket = "/tmp/usbvfio";
  cloudHypervisorLog = "/tmp/chv.log";

  # Provide a raw file as usb stick test image.
  usbDiskImage = pkgs.writeText "usb-stick-image.raw" ''
    This is an uninitialized drive.
  '';
in
{
  integration-smoke = pkgs.nixosTest {
    name = "usbvfiod Smoke Test";

    nodes.machine = { pkgs, ... }: {
      environment.systemPackages = [
        pkgs.usbutils
      ];

      boot.kernelModules = [ "kvm" ];
      systemd.services.usbvfiod = {
        wantedBy = [ "multi-user.target" ];

        serviceConfig = {
          ExecStart = ''
            ${lib.getExe usbvfiod} -v --socket-path ${usbvfiodSocket}
          '';
        };
      };

      systemd.services.cloud-hypervisor = {
        wantedBy = [ "multi-user.target" ];
        requires = [ "usbvfiod.service" ];
        after = [ "usbvfiod.service" ];

        serviceConfig = {
          ExecStart = ''
            ${lib.getExe pkgs.cloud-hypervisor} --memory size=2G,shared=on --console off --serial file=${cloudHypervisorLog} \
              --kernel ${netboot.kernel} \
              --cmdline ${lib.escapeShellArg netboot.cmdline} \
              --initramfs ${netboot.initrd} \
              --user-device socket=${usbvfiodSocket}
          '';
        };
      };

      virtualisation = {
        cores = 2;
        memorySize = 4096;
        qemu.options = [
          # A virtual USB XHCI controller in the host ...
          "-device qemu-xhci,id=host-xhci,addr=10"
          # ... with an attached usb stick.
          "-drive if=none,id=usbstick,format=raw,snapshot=on,file=${usbDiskImage}"
          "-device usb-storage,bus=host-xhci.0,drive=usbstick"
        ];
      };
    };

    # The nested CI runs are really slow.
    globalTimeout = 3600;
    testScript = ''
      start_all()

      machine.wait_for_unit("cloud-hypervisor.service")

      # Check whether the USB controller pops up.
      machine.wait_until_succeeds("grep -Fq 'usb usb1: Product: xHCI Host Controller' ${cloudHypervisorLog}", timeout=3000)
      machine.wait_until_succeeds("grep -Fq 'hub 1-0:1.0: 1 port detected' ${cloudHypervisorLog}")

      # Read the diagnostic information after login.
      machine.wait_until_succeeds("grep -Eq '\s+1\s+PCI-MSIX.*xhci_hcd' ${cloudHypervisorLog}")

      # Check whether the virtual usb stick is available in the host.
      machine.succeed('grep -Fq "uninitialized drive" /dev/sda')

      # Check whether lsusb can be used
      machine.succeed("lsusb | grep -q 'ID'")

      # find the USB Device
      machine.succeed("""
        for dev in /dev/sd*; do
          if udevadm info --query=all --name=$dev | grep -q 'ID_BUS=usb'; then
            echo "$dev" > /tmp/usb_device

            # get Vendor/Model ID
            vendor=$(udevadm info --query=all --name=$dev | grep -oP 'ID_USB_VENDOR_ID=\\K\\w+')
            model=$(udevadm info --query=all --name=$dev | grep -oP 'ID_USB_MODEL_ID=\\K\\w+')

            echo "$vendor" > /tmp/usb_vendor
            echo "$model" > /tmp/usb_model
            break
          fi
        done
      """)

      # show the information about path and permission
      machine.succeed("""
        {
          echo "----- Current Identity -----"
          whoami
          id

          echo "----- USB Block Device (/dev/sdX) -----"
          if [ -f /tmp/usb_device ]; then
            dev=$(cat /tmp/usb_device) 
            echo "Device path: $dev"
            echo "--- USB Block Device (/dev/sdX) Permissions ---"
            ls -l $dev
          else
            echo "No USB device found!" >&2
            echo "No USB device found!"
            exit 1
          fi

          echo "----- USB Bus-Exposed Device Files -----"
          if [ -f /tmp/usb_vendor ] && [ -f /tmp/usb_model ]; then
            vendor=$(cat /tmp/usb_vendor)
            model=$(cat /tmp/usb_model)
            # Find the matching usb
            usb_line=$(lsusb | grep "$vendor:$model")
            if [ -n "$usb_line" ]; then
              echo "$usb_line"
              # Extract bus and device number
              bus=$(echo "$usb_line" | awk '{print $2}')
              device=$(echo "$usb_line" | awk '{print $4}' | tr -d ':')
              char_dev="/dev/bus/usb/$bus/$device"

              if [ -e "$char_dev" ]; then
                echo "Character device path: $char_dev"
                echo "--- USB Bus-Exposed Device Permissions ---"
                ls -l "$char_dev"
              else
                echo "Device node $char_dev not found"
              fi

            else
              echo "No matching lsusb entry found for vendor=$vendor model=$model"
            fi
          else
            echo "Missing vendor or model info" >&2
            echo "Missing vendor or model info"
            exit 1
          fi
          
        } > /tmp/test_report.txt
      """)

      print("-------- Report from Guest --------")
      stdout = machine.execute("cat /tmp/test_report.txt")[1]
      print(stdout)

    '';
  };
}
