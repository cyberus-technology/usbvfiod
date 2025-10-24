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
          # Enable dyndbg messages for the XHCI driver.
          "xhci_pci.dyndbg==pmfl"
          "xhci_hcd.dyndbg==pmfl"
        ];

        # Enable debug verbosity.
        boot.consoleLogLevel = 8;

        # Convenience packages for interactive use
        environment.systemPackages = with pkgs; [ pciutils usbutils ];

        # network configuration for interactive debugging
        networking.interfaces."ens1" = {
          ipv4.addresses = [
            {
              address = "192.168.100.2";
              prefixLength = 24;
            }
          ];
          ipv4.routes = [
            {
              address = "0.0.0.0";
              prefixLength = 0;
              via = "192.168.100.1";
            }
          ];
          useDHCP = false;
        };

        # ssh access for interactive debugging
        services.openssh = {
          enable = true;
          settings = {
            PermitRootLogin = "yes";
            PermitEmptyPasswords = "yes";
          };
        };
        security.pam.services.sshd.allowNullPassword = true;

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
  vendorId = "46f4";
  productId = "0001";

  # Provide a raw file as usb stick test image.
  blockDeviceFile = "/tmp/image.img";
  blockDeviceSize = "8M";


  testMachineConfig.basicMachineConfig = {
    environment.systemPackages = with pkgs; [
      jq
      usbutils
    ];

    users.groups.usbaccess = { };

    users.users.usbaccess = {
      isSystemUser = true;
      group = "usbaccess";
    };

    boot.kernelModules = [ "kvm" ];
  };
  testMachineConfig.interactiveDebugging = {
    # interactive debugging
    services.openssh = {
      enable = true;
      settings = {
        PermitRootLogin = "yes";
        PermitEmptyPasswords = "yes";
      };
    };
    security.pam.services.sshd.allowNullPassword = true;
    virtualisation.forwardPorts = [
      { from = "host"; host.port = 2000; guest.port = 22; }
    ];
  };
  testMachineConfig.sytemdServices = {
    systemd.services = {
      usbvfiod = {
        enable = true;
        wantedBy = [ "multi-user.target" ];

        serviceConfig = {
          User = "usbaccess";
          Group = "usbaccess";
          ExecStart = ''
            ${lib.getExe usbvfiod} -v --socket-path ${usbvfiodSocket} --device "/dev/bus/usb/teststorage"
          '';
        };
      };

      cloud-hypervisor = {
        enable = true;
        wantedBy = [ "multi-user.target" ];
        requires = [ "usbvfiod.service" ];
        after = [ "usbvfiod.service" ];

        serviceConfig = {
          Restart = "on-failure";
          RestartSec = "2s";
          ExecStart = ''
            ${lib.getExe pkgs.cloud-hypervisor} --memory size=2G,shared=on --console off \
              --kernel ${netboot.kernel} \
              --cmdline ${lib.escapeShellArg netboot.cmdline} \
              --initramfs ${netboot.initrd} \
              --user-device socket=${usbvfiodSocket} \
              --net "tap=tap0,mac=,ip=192.168.100.1,mask=255.255.255.0"
          '';
        };
      };
    };
  };
  # The nested CI runs are really slow.
  globalTimeout = 3600;


  make-smoke-test = qemu-device: pkgs.nixosTest {
    name = "usbvfiod Smoke Test with ${qemu-device}";

    inherit globalTimeout;

    nodes.machine = _: {
      imports = [ testMachineConfig.basicMachineConfig testMachineConfig.interactiveDebugging testMachineConfig.sytemdServices ];

      services.udev.extraRules = ''
        ACTION=="add", SUBSYSTEM=="usb", ATTRS{idVendor}=="${vendorId}", ATTRS{idProduct}=="${productId}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/teststorage"
      '';

      virtualisation = {
        cores = 2;
        memorySize = 4096;
        qemu.options = [
          # A virtual USB controller in the host ...
          "-device ${qemu-device},id=usbcontroller,addr=10"
          # ... with an attached usb stick.
          "-drive if=none,id=usbstick,format=raw,file=${blockDeviceFile}"
          "-device usb-storage,bus=usbcontroller.0,drive=usbstick"
        ];
      };
    };

    testScript = ''
      import re
      import os
      from test_driver.errors import RequestedAssertionFailed

      class Nested():
        """Extending Nix Test Framework to enable using known functions on a nested VM.
        Commands are executed over ssh.
        Heavily inspired by nixos-tests (https://nixos.org/manual/nixos/stable/index.html#ssec-machine-objects) and their implementation.
        """
        def __init__(self, vm_host: Machine) -> None:
          self.vm_host = vm_host

        def succeed(self, *commands: str, timeout: int | None = None) -> str:
          vm_host = self.vm_host
          output = ""
          for command in commands:
              with vm_host.nested(f"must succeed: {command}"):
                  (status, out) = vm_host.execute("ssh -q -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 '" + command + "'", timeout=timeout)
                  if status != 0:
                      vm_host.log(f"output: {out}")
                      raise RequestedAssertionFailed(
                          f"command `{command}` failed (exit code {status})"
                      )
                  output += out
          return output        

        def wait_until_succeeds(self, command: str, timeout: int = 900):
          vm_host = self.vm_host
          output = ""

          def check_success(_last_try: bool) -> bool:
            nonlocal output
            status, output = vm_host.execute("ssh -q -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 '" + command + "'", timeout=timeout)
            return status == 0

          with vm_host.nested(f"waiting for success in cloud-hypervisor: {command}"):
            retry(check_success, timeout)
            return(output)

      def search(pattern: str, string: str):
        if re.search(pattern, string):
          return
        else:
          raise RequestedAssertionFailed(
            f"pattern `{pattern}` not found in {string}"
          )
      
      # only relevant for interactive testing when `dd seek=` will not reset the image file by overwriting
      os.system("rm ${blockDeviceFile}")

      print("Creating file image at ${blockDeviceFile}")
      os.system("dd bs=1  count=1 seek=${blockDeviceSize} if=/dev/zero of=${blockDeviceFile}")
      
      start_all()

      machine.wait_for_unit("cloud-hypervisor.service")

      # check sshd in systemd.services.cloud-hypervisor is usable prior to testing over ssh
      machine.wait_until_succeeds("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 'exit 0'", timeout=3000)

      cloud_hypervisor = Nested(vm_host=machine)

      # Confirm USB controller pops up in boot logs
      out = cloud_hypervisor.succeed("journalctl -b")
      search("usb usb1: Product: xHCI Host Controller", out)
      search("hub 1-0:1\\.0: [0-9]+ ports? detected", out)

      # Confirm some diagnostic information
      out = cloud_hypervisor.succeed("cat /proc/interrupts")
      search(" +[1-9][0-9]* +PCI-MSIX.*xhci_hcd", out)
      out = cloud_hypervisor.succeed("lsusb")
      search("ID ${vendorId}:${productId} QEMU QEMU USB HARDDRIVE", out)
      out = cloud_hypervisor.succeed("sfdisk -l")
      search("Disk /dev/sda:", out)
      
      # Test partitioning
      cloud_hypervisor.succeed("echo ',,L' | sfdisk --label=gpt /dev/sda")
      
      # Test filesystem
      cloud_hypervisor.succeed("mkfs.ext4 /dev/sda1")
      cloud_hypervisor.succeed("mount /dev/sda1 /mnt")
      cloud_hypervisor.succeed("echo 123TEST123 > /mnt/file.txt")
      cloud_hypervisor.succeed("umount /mnt")
      cloud_hypervisor.succeed("mount /dev/sda1 /mnt")
      out = cloud_hypervisor.succeed("cat /mnt/file.txt")
      search("123TEST123", out)
    '';
  };
in
{
  integration-smoke = make-smoke-test "qemu-xhci";

  integration-smoke-usb-2 = make-smoke-test "usb-ehci";

  integration-smoke-usb-interrupts = pkgs.nixosTest {
    name = "usbvfiod Smoke Test using a HID device";

    inherit globalTimeout;

    extraPythonPackages = p: [ p.qemu ];

    testScript = ''
      import os

      os.system("rm ${blockDeviceFile}")
      os.system("dd bs=1  count=1 seek=${blockDeviceSize} if=/dev/zero of=${blockDeviceFile}")
      
      start_all()
      machine.wait_for_unit("cloud-hypervisor.service")

      # use qemu qmp interface to send event into QemuConsole (the default PS/2 HID device)
      os.system("""nix run nixpkgs#socat -- - UNIX-CONNECT:/tmp/qmp.sock <<EOF
      {"execute": "qmp_capabilities"}
      {"execute": "send-key", "arguments": {"keys": [ { "type": "qcode", "data": "ctrl" } ]}}
      EOF""")

      # TODO evaluate if inputs were received in cloud-hypervisor (it currently doesn't due to usbvfio and todo!())
    '';

    nodes.machine = { ... }:
      let
        hid-vendorId = "0627";
        hid-productId = "0001";
      in
      {
        imports = [ testMachineConfig.basicMachineConfig testMachineConfig.interactiveDebugging testMachineConfig.sytemdServices ];

        virtualisation.forwardPorts = [
          #qemu qmp interface
          { from = "host"; host.port = 2001; guest.port = 4444; }
        ];

        services.udev.extraRules = ''
          ACTION=="add", SUBSYSTEM=="usb", ATTRS{idVendor}=="${hid-vendorId}", ATTRS{idProduct}=="${hid-productId}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/teststorage"
        '';

        virtualisation = {
          cores = 2;
          memorySize = 4096;
          qemu.options = [
            # A virtual USB UHCI controller in the host ...
            "-device qemu-xhci,id=xhci,addr=10"
            # ... with an attached usb keyboard
            #"-device usb-kbd,bus=xhci.0,id=keybrd" # TODO how to make inputs
            #"-device usb-mouse,bus=xhci.0" # TODO CommandTrb EvaluateContext not yet implemented & how to make inputs

            # make qmp interactions available
            "-chardev socket,id=qmp,port=4444,path=/tmp/qmp.sock,server=on,wait=off"
            "-mon chardev=qmp,mode=control,pretty=on"
          ];
        };
      };
  };
}
