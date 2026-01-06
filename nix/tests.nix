/**
  This file contains integration tests for usbvfiod.
*/
{ lib, pkgs, usbvfiod }:
let
  # For the VM that we start in Cloud Hypervisor, we re-use the netboot image.
  netbootNixos = debug: lib.nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;

    modules = [
      "${pkgs.path}/nixos/modules/installer/netboot/netboot-minimal.nix"

      # Cloud Hypervisor Guest Convenience
      ({ config, ... }: {

        boot = {
          initrd.kernelModules = [ "virtio_console" ];

          kernelParams = [
            # currently we can not handle the automatic suspend that is triggered so we disable dynamic power management
            # https://github.com/torvalds/linux/blob/master/Documentation/driver-api/usb/power-management.rst
            "usbcore.autosuspend=-1"

            # Faster logging than serial would provide.
            "console=hvc0"

            # Keep a console available for early boot until we can write hvc.
            "console=tty0"
          ] ++ (if debug then [
            # Enable dyndbg messages for the XHCI driver.
            "xhci_pci.dyndbg==pmfl"
            "xhci_hcd.dyndbg==pmfl"
          ]
          else [ ]);
        };

        services.journald.console = "hvc0";

        # Enable debug verbosity.
        boot.consoleLogLevel = lib.mkIf debug 8;

        # Convenience packages for interactive use
        environment.systemPackages = with pkgs; [ pciutils usbutils ];

        # network configuration for interactive debugging
        networking.interfaces."ens2" = {
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

  mkNetboot = debug:
    let
      inherit (netbootNixos debug) config;

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
  usbvfiodSocketHotplug = "/tmp/hotplug";

  guestLogFile = "/tmp/console.log";

  # Will very likely be used in every test.
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

  # To execute commands on the nested guest with a partial copy of the NixOS test framework.
  # currently: succeed() and wait_until_succeeds()
  # This will also add a QoL 'string in string' search function.
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

                    (guest_status, guest_out) = vm_host.execute("cat ${guestLogFile}")
                    print(f'\n<<<<<GUEST LOGS>>>>>\n\n{guest_out}\n\n<<<<<END GUEST LOGS>>>>>\n')

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
    }.${pkgs.stdenv.hostPlatform.system} or true /* Also ignore failure on any systems not otherwise listed. */;
  };

  # Some static values for ...
  # ... creating blockdevice backing files.
  imagePathPart = "/tmp/image";
  imageSize = "8M";
  # ... identifying QEMU's virtual Devices.
  blockdeviceVendorId = "46f4";
  blockdeviceProductId = "0001";
  hidVendorId = "0627";
  hidProductId = "0001";

  # Fill in a template for a udev rule.
  mkUdevRule = controller: port: symlink: ''
    ACTION=="add|change", SUBSYSTEM=="usb", ATTRS{product}=="${controller}" ATTR{devpath}=="${port}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/${symlink}"
  '';

  # Fill in a template for the qemu.options list for a blockdevice.
  mkQemuBlockdevice = driveId: driveFile: deviceBus: devicePort: ''-drive if=none,id=${driveId},format=raw,file=${driveFile} -device usb-storage,bus=${deviceBus}.0,port=${devicePort},drive=${driveId}'';

  # Fill in a template for the qemu.options list for a USB keyboard.
  mkQemuKeyboard = deviceBus: devicePort: ''-device usb-kbd,bus=${deviceBus}.0,port=${devicePort}'';

  # Create a blockdevice or USB keyboard on our QEMU bus-id corresponding with the declared usb version.
  mkUsbDeviceType = testname: device:
    let
      deviceBus = { "2" = "ehci"; "3" = "xhci"; }.${device.usbVersion};
    in
    if (!device.udevRule.enable || device.udevRule.symlink == "")
    then abort "udevRule is necessary to attach create qemu device before/on startup"
    else if (device.type == "blockdevice")
    then
      mkQemuBlockdevice
        "${deviceBus}-${device.udevRule.symlink}"
        "${imagePathPart}-${testname}-${device.udevRule.symlink}.img"
        "${deviceBus}"
        "${builtins.toString device.usbPort}"
    else if (device.type == "hid-device")
    then
      mkQemuKeyboard
        "${deviceBus}"
        "${builtins.toString device.usbPort}"
    else builtins.abort ''wrong device type; types supported are "blockdevice" and "hid-device"'';

  # Respect if attached at host on boot option is true to create the QEMU device option.
  mkUsbDevice = testname: device:
    if device.attachedOnStartup == "host" || device.attachedOnStartup == "guest"
    then mkUsbDeviceType testname device
    else ""; # Device should be handled via QEMU QMP in the testScript.

  # Create a testScript snippet to make a clean blockdevice image file.
  mkPrepareOneBlockdeviceImage = testname: device:
    let
      filepath = "${imagePathPart}-${testname}-${device.udevRule.symlink}.img";
    in
    ''
      os.system("rm ${filepath}")
      print("Creating file image at ${filepath}")
      os.system("dd bs=1  count=1 seek=${imageSize} if=/dev/zero of=${filepath}")
    '';

  # Decide if a virtual device needs a backing image file.
  mkPrepareBlockdeviceImages = testname: device:
    if device.type == "blockdevice" # for now only blockdevices need a backing file
    then mkPrepareOneBlockdeviceImage testname device
    else "";

  # Generate usbvfiod argument flags to hand over the device through their udev generated symlink.
  mkDeviceFlag = device:
    if device.attachedOnStartup == "guest" && (!device.udevRule.enable || device.udevRule.symlink == "")
    then abort "udevRule is necessary to attach device before startup of usbvfiod"
    else if device.attachedOnStartup == "guest"
    then ''--device "/dev/bus/usb/${device.udevRule.symlink}"''
    else "";

  # Input type check for list of virtualDevices in the attrs.
  sanityCheckDevice = device:
    assert (device.type == "blockdevice" || device.type == "hid-device");
    assert (device.usbVersion == "2" || device.usbVersion == "3");
    assert (builtins.typeOf device.usbPort == "int" || builtins.typeOf device.usbPort == "string");
    assert (builtins.typeOf device.udevRule.enable == "bool");
    assert (builtins.typeOf device.udevRule.symlink == "string");
    assert (device.attachedOnStartup == "none" || device.attachedOnStartup == "host" || device.attachedOnStartup == "guest");
    true;

  # Input type check for the attrs arg.
  sanityCheckArgs = args:
    assert (builtins.typeOf args.name == "string");
    assert (builtins.typeOf args.debug == "bool");
    assert (builtins.typeOf args.virtualDevices == "list");
    assert (builtins.typeOf args.testScript == "string");
    assert (builtins.all sanityCheckDevice args.virtualDevices);
    args;

  # If possible use default values for not set things.
  mkDefaults = args:
    let
      deviceCount = builtins.length args.virtualDevices;

      # The defined default values to generate a test argument attrs.
      virtualDevice = {
        type = "blockdevice";
        usbVersion = "3";
        usbPort = 1;
        udevRule.enable = true;
        udevRule.symlink = "testdevice";
        attachedOnStartup = "guest";
      };

      attrs = {
        debug = true;
      } // args // {
        virtualDevices = builtins.genList (i: lib.recursiveUpdate virtualDevice (builtins.elemAt args.virtualDevices i)) deviceCount;
      };

    in
    attrs;



  /**
    Create a pkgs.testers.runNixOSTest with specific purpose of testing Usbvfiod.
    The Functions purpose is to remove duplicated lines, make comparing tests easier and write new tests with less boilerplate.

    # Inputs

    `args`

    : 1\. Function argument

    # Type

    ```
    mkUsbTest :: {
      name :: String
      debug :: Bool
      virtualDevices :: [
        {
        type :: "blockdevice" || "hid-device"
        usbVersion :: "2" || "3"
        usbPort :: Integer || String
        udevRule.enable :: Bool
        udevRule.symlink :: String
        attachedOnStartup :: "host" || "guest" || "none"
        }
      ]
      testScript :: String
    } -> a
    ```

    # Examples
    :::{.example}

    When Using more than one device each device should define its usbPort and udevRule.symlink (the default value is static).

    ## `mkUsbTest` usage example

    ```nix
    myTest = mkUsbTest {
      name = "foo";
      debug = true;
      virtualDevices = [
        {
          type = "blockdevice";
          usbVersion = "2";
          usbPort = 1;
          udevRule.enable = true;
          udevRule.symlink = "teststorage";
          attachedOnStartup = "guest";
        }
      ];
      testScript = ''
        # Confirm USB controller pops up in boot logs
        out = cloud_hypervisor.succeed("journalctl -b", timeout=60)
        search("usb usb1: Product: xHCI Host Controller", out)
        search("hub 1-0:1\\.0: [0-9]+ ports? detected", out)

        # Confirm some diagnostic information
        out = cloud_hypervisor.succeed("cat /proc/interrupts", timeout=60)
        search(" +[1-9][0-9]* +PCI-MSIX.*xhci_hcd", out)
        out = cloud_hypervisor.succeed("lsusb", timeout=60)
        search("ID ${blockdeviceVendorId}:${blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
        out = cloud_hypervisor.succeed("sfdisk -l", timeout=60)
        search("Disk /dev/sda:", out)

        # Test partitioning
        cloud_hypervisor.succeed("echo ',,L' | sfdisk --label=gpt /dev/sda", timeout=60)

        # Test filesystem
        cloud_hypervisor.succeed("mkfs.ext4 /dev/sda1", timeout=60)
        cloud_hypervisor.succeed("mount /dev/sda1 /mnt", timeout=60)
        cloud_hypervisor.succeed("echo 123TEST123 > /mnt/file.txt", timeout=60)
        cloud_hypervisor.succeed("umount /mnt", timeout=60)
        cloud_hypervisor.succeed("mount /dev/sda1 /mnt", timeout=60)
        out = cloud_hypervisor.succeed("cat /mnt/file.txt", timeout=60)
        search("123TEST123", out)
      '';
    };
    ```

  */
  mkUsbTest = args: mkUsbTestChecked (sanityCheckArgs (mkDefaults args));

  # See mkUsbTest (this runs without any arg checks).
  mkUsbTestChecked =
    let
      ehciProductName = "EHCI Host Controller";
      xhciProductName = "xHCI Host Controller";
    in
    args: pkgs.testers.runNixOSTest {
      inherit (args) name;

      inherit globalTimeout passthru;

      nodes.machine = _: {
        imports = [ basicMachineConfig ];

        # Create a udev rule for every device listed that enables it.
        services.udev.extraRules =
          lib.concatStrings (
            builtins.map
              (device:
                if device.udevRule.enable then
                  let
                    controller = { "2" = ehciProductName; "3" = xhciProductName; }.${device.usbVersion};
                    usbPort = builtins.toString device.usbPort;
                  in
                  if (usbPort == "" || device.udevRule.symlink == "")
                  then abort "A udev rules requires to set a usbPort and a symlink string"
                  else
                    ''
                      ${mkUdevRule controller usbPort device.udevRule.symlink}
                    ''
                else ""
              )
              args.virtualDevices)
        ;

        virtualisation = {
          cores = 2;
          memorySize = 4096;
          # Removing this Keyboard makes the optional USB Keyboard the default to send QMP key-events.
          qemu.virtioKeyboard = false;
          qemu.options = [
            # Add the xhci controller to use USB 3.0.
            "-device qemu-xhci,id=xhci,addr=10"

            # Add the ehci controller to use USB 2.0.
            "-device usb-ehci,id=ehci,addr=11"

            # Enable the QEMU QMP interface to trigger HID events or plug blockdevices at runtime.
            "-chardev socket,id=qmp,path=/tmp/qmp.sock,server=on,wait=off"
            "-mon chardev=qmp,mode=control,pretty=on"
          ]
          # Handle each entry of the args.virtualDevices list.
          ++ (builtins.map (mkUsbDevice args.name) args.virtualDevices);
        };

        systemd.services = {
          usbvfiod = {
            wantedBy = [ "multi-user.target" ];
            serviceConfig = {
              User = "usbaccess";
              Group = "usbaccess";
              Restart = "on-failure";
              RestartSec = "2s";
              ExecStart = ''
                ${lib.getExe usbvfiod} ${if args.debug then "-v" else ""} --socket-path ${usbvfiodSocket} --hotplug-socket-path ${usbvfiodSocketHotplug} ${lib.concatStringsSep " " (builtins.map mkDeviceFlag args.virtualDevices)}
              '';
            };
          };

          cloud-hypervisor =
            let
              netboot = mkNetboot args.debug;
            in
            {
              wantedBy = [ "multi-user.target" ];
              requires = [ "usbvfiod.service" ];
              after = [ "usbvfiod.service" ];
              serviceConfig = {
                Restart = "on-failure";
                RestartSec = "2s";
                ExecStart = ''
                  ${lib.getExe pkgs.cloud-hypervisor} --memory size=2G,shared=on --console file=${guestLogFile} --serial off \
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

      testScript = ''
        ${nestedPythonClass}
        import os

        # prepare blockdevice images if necessary
        ${lib.concatStringsSep "\n" (builtins.map (mkPrepareBlockdeviceImages args.name) args.virtualDevices)}

        start_all()

        machine.wait_for_unit("cloud-hypervisor.service")

        # Check sshd in systemd.services.cloud-hypervisor is usable prior to testing over ssh.
        machine.wait_until_succeeds("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 'exit 0'", timeout=3000)

        cloud_hypervisor = Nested(vm_host=machine)

        ${args.testScript}
      '';
    };

  singleBlockDeviceTestScript = ''
    # Confirm USB controller pops up in boot logs
    out = cloud_hypervisor.succeed("journalctl -b", timeout=60)
    search("usb usb1: Product: xHCI Host Controller", out)
    search("hub 1-0:1\\.0: [0-9]+ ports? detected", out)

    # Confirm some diagnostic information
    out = cloud_hypervisor.succeed("cat /proc/interrupts", timeout=60)
    search(" +[1-9][0-9]* +PCI-MSIX.*xhci_hcd", out)
    out = cloud_hypervisor.succeed("lsusb", timeout=60)
    search("ID ${blockdeviceVendorId}:${blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
    out = cloud_hypervisor.succeed("sfdisk -l", timeout=60)
    search("Disk /dev/sda:", out)

    # Test partitioning
    cloud_hypervisor.succeed("echo ',,L' | sfdisk --label=gpt /dev/sda", timeout=60)

    # Test filesystem
    cloud_hypervisor.succeed("mkfs.ext4 /dev/sda1", timeout=60)
    cloud_hypervisor.succeed("mount /dev/sda1 /mnt", timeout=60)
    cloud_hypervisor.succeed("echo 123TEST123 > /mnt/file.txt", timeout=60)
    cloud_hypervisor.succeed("umount /mnt", timeout=60)
    cloud_hypervisor.succeed("mount /dev/sda1 /mnt", timeout=60)
    out = cloud_hypervisor.succeed("cat /mnt/file.txt", timeout=60)
    search("123TEST123", out)
  '';

in
{
  blockdevice-usb-3 = mkUsbTest {
    name = "blockdevice-usb-3";
    virtualDevices = [
      {
        type = "blockdevice";
        usbVersion = "3";
      }
    ];
    testScript = singleBlockDeviceTestScript;
  };

  blockdevice-usb-2 = mkUsbTest {
    name = "blockdevice-usb-2";
    virtualDevices = [
      {
        type = "blockdevice";
        usbVersion = "2";
      }
    ];
    testScript = singleBlockDeviceTestScript;
  };

  interrupt-endpoints = mkUsbTest {
    name = "interrupt-endpoints";
    virtualDevices = [
      {
        type = "hid-device";
        usbVersion = "3"; # note: this changes the /dev/input/by-id path used in the script (xhci/ehci bus number)
        usbPort = 1; # note: this changes the /dev/input/by-id path used in the script
        udevRule.symlink = "keyboard";
      }
    ];
    testScript = ''
      import time
      import threading

      # A function that can send input events in the background.
      def create_input():
        for i in range(1, 4):
          time.sleep(1)
          os.system("""${pkgs.socat}/bin/socat - UNIX-CONNECT:/tmp/qmp.sock >> /dev/null <<EOF
      {"execute": "qmp_capabilities"}
      {"execute": "send-key", "arguments": {"keys": [ { "type": "qcode", "data": "ctrl" } ]}}
      EOF""")
          print(f"input loop `{i}` done")

      # Check the Keyboard is in detected in the guest.
      cloud_hypervisor.succeed("lsusb -d ${hidVendorId}:${hidProductId}", timeout=60)

      # Generate inputs in the background.
      t1 = threading.Thread(target=create_input)
      t1.start()
      print("started sending input events")

      # Catch one key down event and one key up event inputs.
      # It is theoretically possible all events appear and are consumed by the input subsystem before we have the opportunity to listen.
      out = cloud_hypervisor.succeed("hexdump --length 144 --two-bytes-hex /dev/input/by-id/usb-QEMU_QEMU_USB_Keyboard_68284-0000\\:00\\:10.0-1-event-kbd", timeout=60)

      # Check if the hexdump contains a ctrl event sequence
      # https://docs.kernel.org/input/input.html#event-interface
      search("0001    001d    0001", out) # EV_KEY KEY_LEFTCTRL pressed
      search("0001    001d    0000", out) # EV_KEY KEY_LEFTCTRL released
      print("done")

      # Make a clean exit since the test will wait for thread termination either way.
      t1.join()
    '';
  };

  multiple-blockdevices = mkUsbTest {
    name = "multiple-blockdevices";
    virtualDevices =
      builtins.concatMap
        (usb:
          builtins.map
            (num:
              {
                type = "blockdevice";
                usbVersion = "${usb}";
                usbPort = num;
                udevRule.symlink = "usb-${usb}-device-${builtins.toString num}";
              }
            ) [ 1 2 3 4 ]
        ) [ "2" "3" ];
    testScript = ''
      out = cloud_hypervisor.succeed("lsusb --tree", timeout=60)
      search(r'Port 001: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M', out)
      search(r'Port 002: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M', out)
      search(r'Port 003: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M', out)
      search(r'Port 004: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M', out)
      search(r'Port 001: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M', out)
      search(r'Port 002: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M', out)
      search(r'Port 003: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M', out)
      search(r'Port 004: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M', out)

      out = cloud_hypervisor.succeed("lsblk", timeout=60)
      search(r'sda\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdb\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdc\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdd\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sde\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdf\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdg\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
      search(r'sdh\s+\d+:\d+\s+0\s+8M\s+0\s+disk', out)
    '';
  };

  hot-attach = mkUsbTest {
    name = "hot-attach";
    virtualDevices = [
      {
        type = "blockdevice";
        usbVersion = "3";
        usbPort = 1;
        udevRule.symlink = "usbdevice";
        attachedOnStartup = "host";
      }
    ];
    testScript = ''
      # Check no device is attached.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${usbvfiodSocketHotplug} --list", timeout=60)
      search("No attached devices", out)
      print(out)

      cloud_hypervisor.succeed('! lsblk /dev/sda', timeout = 60)

      # Attach a device.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${usbvfiodSocketHotplug} --attach /dev/bus/usb/usbdevice", timeout=60)
      print(out)

      # Confirm the usb device attached to usbvfiod.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${usbvfiodSocketHotplug} --list", timeout=60)
      search("One attached device:", out)
      search(r"\d+:\d+", out)
      print(out)

      # Confirm it is known in the guest.
      cloud_hypervisor.wait_until_succeeds('lsblk /dev/sda')
    '';
  };
}
