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

          # currently we can not handle the automatic suspend that is triggered so we disable dynamic power management
          # https://github.com/torvalds/linux/blob/master/Documentation/driver-api/usb/power-management.rst
          "usbcore.autosuspend=-1"
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

  # prepared node config snippets that can be individually imported in test nodes
  testMachineConfig = {
    basicMachineConfig = {
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

      # interactive debugging over ssh
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

    # usbvfiod and cloud-hypervisor services
    systemdServices = {
      systemd.services = {
        usbvfiod = {
          wantedBy = [ "multi-user.target" ];

          serviceConfig = {
            User = "usbaccess";
            Group = "usbaccess";
            ExecStart = ''
              ${lib.getExe usbvfiod} -v --socket-path ${usbvfiodSocket} --device "/dev/bus/usb/testdevice"
            '';
          };
        };

        cloud-hypervisor = {
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
  };

  nestedPythonClass = ''
    import re
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
            with vm_host.nested(f"must succeed in cloud-hypervisor: {command}"):
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
  '';

  # The nested CI runs are really slow.
  globalTimeout = 3600;

  passthru = {
    # Limit running tests on known successful platforms.
    # This is used to work around CI issues, where both `ignoreFailure` and `requireFailure`
    # for HerculesCI have weird interaction with reporting back the status to GitHub.
    # This is also making sure the test is still available for end-users to run on their systems.
    # Using buildDependenciesOnly means the actual test will not be ran, but all dependencies will be built.
    buildDependenciesOnly = {
      # Verified systems, which should work.
      "x86_64-linux" = false;
      # `aarch64-linux` fails on Hercules CI due to nested virtualization usage.
      # The build might be working, but after a 1 hour timeout, the machine barely gets into stage-2.
      # So for now, skip running the actual test.
      "aarch64-linux" = true;
    }.${pkgs.system} or true /* Also ignore failure on any systems not otherwise listed. */;
  };

  make-blockdevice-test = qemu-usb-controller: pkgs.testers.runNixOSTest {
    name = "usbvfiod blockdevice test with ${qemu-usb-controller}";

    inherit globalTimeout passthru;

    nodes.machine = _: {
      imports = [ testMachineConfig.basicMachineConfig testMachineConfig.systemdServices ];

      services.udev.extraRules = ''
        ACTION=="add", SUBSYSTEM=="usb", ATTRS{idVendor}=="${vendorId}", ATTRS{idProduct}=="${productId}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/testdevice"
      '';

      virtualisation = {
        cores = 2;
        memorySize = 4096;
        qemu.options = [
          # A virtual USB controller in the host ...
          "-device ${qemu-usb-controller},id=usbcontroller,addr=10"
          # ... with an attached usb stick.
          "-drive if=none,id=usbstick,format=raw,file=${blockDeviceFile}"
          "-device usb-storage,bus=usbcontroller.0,drive=usbstick"
        ];
      };
    };

    testScript = ''
      ${nestedPythonClass}
      import os

      # only relevant for interactive testing when `dd seek=` will not reset the image file by overwriting
      os.system("rm ${blockDeviceFile}")

      print("Creating file image at ${blockDeviceFile}")
      os.system("dd bs=1  count=1 seek=${blockDeviceSize} if=/dev/zero of=${blockDeviceFile}")
      
      start_all()

      machine.wait_for_unit("cloud-hypervisor.service")

      # Check sshd in systemd.services.cloud-hypervisor is usable prior to testing over ssh
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
  blockdevice-usb-3 = make-blockdevice-test "qemu-xhci";

  blockdevice-usb-2 = make-blockdevice-test "usb-ehci";

  interrupt-endpoints =
    let
      hid-vendorId = "0627";
      hid-productId = "0001";
    in
    pkgs.testers.runNixOSTest {
      name = "usbvfiod testing a HID device";

      inherit globalTimeout passthru;

      testScript = ''
        ${nestedPythonClass}
        import os
        import time
        import threading

        start_all()
        machine.wait_for_unit("cloud-hypervisor.service")

        # Check sshd in systemd.services.cloud-hypervisor is usable prior to testing over ssh.
        machine.wait_until_succeeds("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 'exit 0'", timeout=3000)

        # A function that can send input events in the background.
        def create_input():
          for i in range(1, 4):
            time.sleep(1)
            os.system("""${pkgs.socat}/bin/socat - UNIX-CONNECT:/tmp/qmp.sock >> /dev/null <<EOF
        {"execute": "qmp_capabilities"}
        {"execute": "send-key", "arguments": {"keys": [ { "type": "qcode", "data": "ctrl" } ]}}
        EOF""")
            print(f"input loop `{i}` done")

        cloud_hypervisor = Nested(vm_host=machine)

        # Check the Keyboard is in detected in the guest.
        cloud_hypervisor.succeed("lsusb -d ${hid-vendorId}:${hid-productId}")

        # Generate inputs in the background.
        t1 = threading.Thread(target=create_input)
        t1.start()
        print("started sending input events")

        # Catch one key down event and one key up event inputs.
        # It is theoretically possible all events appear and are consumed by the input subsystem before we have the opportunity to listen.
        out = cloud_hypervisor.succeed("hexdump --length 144 --two-bytes-hex /dev/input/by-id/usb-QEMU_QEMU_USB_Keyboard_68284-0000\\:00\\:10.0-1-event-kbd")
      
        # Check if the hexdump contains a ctrl event sequence
        # https://docs.kernel.org/input/input.html#event-interface
        search("0001    001d    0001", out) # EV_KEY KEY_LEFTCTRL pressed
        search("0001    001d    0000", out) # EV_KEY KEY_LEFTCTRL released
        print("done")
      
        # Make a clean exit since the test will wait for thread termination either way.
        t1.join()
      '';

      nodes.machine = _:
        {
          imports = [ testMachineConfig.basicMachineConfig testMachineConfig.systemdServices ];

          services.udev.extraRules = ''
            ACTION=="add|change", SUBSYSTEM=="usb", ATTRS{product}=="QEMU USB Keyboard", ATTRS{idVendor}=="${hid-vendorId}", ATTRS{idProduct}=="${hid-productId}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/testdevice"
          '';

          virtualisation = {
            cores = 2;
            memorySize = 4096;
            # QEMU QMP send-key commands will be sent through default non-usb (vfio) devices if not explicitly disabled
            qemu.virtioKeyboard = false;
            qemu.options = [
              # A virtual USB UHCI controller in the host ...
              "-device qemu-xhci,id=xhci,addr=10"
              # ... with an attached usb keyboard.
              "-device usb-kbd,bus=xhci.0"

              # Enable QEMU QMP interactions.
              "-chardev socket,id=qmp,path=/tmp/qmp.sock,server=on,wait=off"
              "-mon chardev=qmp,mode=control,pretty=on"
            ];
          };
        };
    };

  multiple-blockdevices =
    pkgs.testers.runNixOSTest {
      name = "usbvfiod with multiple blockdevices";

      inherit globalTimeout;

      nodes.machine = _: {
        imports = [ testMachineConfig.basicMachineConfig ];

        virtualisation = {
          cores = 2;
          memorySize = 4096;
          qemu.options = [
            # virtual USB controllers in the host
            "-device usb-ehci,id=ehci"
            "-device qemu-xhci,id=xhci"
            # ... with attached usb sticks.
            "-drive if=none,id=ehci-01,format=raw,file=${blockDeviceFile}1"
            "-device usb-storage,bus=ehci.0,port=1,drive=ehci-01"
            "-drive if=none,id=ehci-02,format=raw,file=${blockDeviceFile}2"
            "-device usb-storage,bus=ehci.0,port=2,drive=ehci-02"
            "-drive if=none,id=ehci-03,format=raw,file=${blockDeviceFile}3"
            "-device usb-storage,bus=ehci.0,port=3,drive=ehci-03"
            "-drive if=none,id=ehci-04,format=raw,file=${blockDeviceFile}4"
            "-device usb-storage,bus=ehci.0,port=4,drive=ehci-04"

            "-drive if=none,id=xhci-01,format=raw,file=${blockDeviceFile}5"
            "-device usb-storage,bus=xhci.0,port=1,drive=xhci-01"
            "-drive if=none,id=xhci-02,format=raw,file=${blockDeviceFile}6"
            "-device usb-storage,bus=xhci.0,port=2,drive=xhci-02"
            "-drive if=none,id=xhci-03,format=raw,file=${blockDeviceFile}7"
            "-device usb-storage,bus=xhci.0,port=3,drive=xhci-03"
            "-drive if=none,id=xhci-04,format=raw,file=${blockDeviceFile}8"
            "-device usb-storage,bus=xhci.0,port=4,drive=xhci-04"
          ];
        };
      };

      testScript = ''
        ${nestedPythonClass}
        import os

        # only relevant for interactive testing when `dd seek=` will not reset the image file by overwriting
        print("Creating file images at ${blockDeviceFile}x")
        for i in range(1,10):
          os.system("rm ${blockDeviceFile}")
          os.system(f"dd bs=1  count=1 seek=${blockDeviceSize} if=/dev/zero of=${blockDeviceFile}{i}")
      
        start_all()
      '';
    };
}
