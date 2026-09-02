{
  lib,
  pkgs,
  cloud-hypervisor,
  usbvfiod,
  ...
}:
let
  # For the VM that we start in Cloud Hypervisor, we re-use the netboot image.
  netbootNixos =
    debug:
    lib.nixosSystem {
      inherit (pkgs.stdenv.hostPlatform) system;

      modules = [
        "${pkgs.path}/nixos/modules/installer/netboot/netboot-minimal.nix"

        # Cloud Hypervisor Guest Convenience
        (
          { config, ... }:
          {

            boot = {
              initrd.kernelModules = [ "virtio_console" ];
              zfs.forceImportRoot = false;

              kernelParams = [
                # currently we can not handle the automatic suspend that is triggered so we disable dynamic power management
                # https://github.com/torvalds/linux/blob/master/Documentation/driver-api/usb/power-management.rst
                "usbcore.autosuspend=-1"

                # Faster logging than serial would provide.
                "console=hvc0"

                # Keep a console available for early boot until we can write hvc.
                "console=tty0"
              ]
              ++ (
                if debug then
                  [
                    # Enable dyndbg messages for the XHCI driver.
                    "xhci_pci.dyndbg==pmfl"
                    "xhci_hcd.dyndbg==pmfl"
                  ]
                else
                  [ ]
              );
            };

            services.journald.console = "hvc0";

            # Enable debug verbosity.
            boot.consoleLogLevel = lib.mkIf debug 8;

            # Convenience packages for interactive use
            environment.systemPackages = with pkgs; [
              pciutils
              usbutils
              e2fsprogs
              lsof
            ];

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
          }
        )
      ];
    };

  mkNetboot =
    debug:
    let
      inherit (netbootNixos debug) config;

      kernelTarget = pkgs.linux.target;
    in
    {
      initrd = "${config.system.build.netbootRamdisk}/initrd";
      kernel = "${config.system.build.kernel}/${kernelTarget}";
      cmdline = "init=${config.system.build.toplevel}/init " + builtins.toString config.boot.kernelParams;
    };

  # Putting the socket in a world-readable location is obviously not a
  # good choice for a production setup, but for this test it works
  # well.
  usbvfiodSocket = "/tmp/usbvfiod.sock";
  usbvfiodSocketHotplug = "/tmp/hotplug.sock";

  guestLogFile = "/tmp/console.log";
  qemuLogFile = "/tmp/qemu-vc.log";

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
      {
        from = "host";
        host.port = 2000;
        guest.port = 22;
      }
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
      def __init__(self, vm_host: BaseMachine) -> None:
        self.vm_host = vm_host

      def execute(self, *commands: str, timeout: int | None = None) -> str:
        vm_host = self.vm_host
        output = ""
        for command in commands:
          (status, out) = vm_host.execute("ssh -q -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 '" + command + "'", timeout=timeout)
          output += out
        return output

      def succeed(self, *commands: str, timeout: int | None = None) -> str:
        vm_host = self.vm_host
        output = ""
        for command in commands:
            with vm_host.nested(f"must succeed in cloud-hypervisor: {command}"):
                (status, out) = vm_host.execute("ssh -q -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 '" + command + "'", timeout=timeout)
                if status != 0:
                    print('\nnested.succeed() FAILED')

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
          try:
            retry(check_success, timeout)
          except Exception as e:
            print('\nnested.wait_until_succeeds() FAILED')

            print(f'\n<<<<<LATEST COMMAND OUTPUT>>>>>\n\n{output}\n\n<<<<<END LATEST COMMAND OUTPUT>>>>>\n')

            (guest_status, guest_out) = vm_host.execute("cat ${guestLogFile}")
            print(f'\n<<<<<GUEST LOGS>>>>>\n\n{guest_out}\n\n<<<<<END GUEST LOGS>>>>>\n')
            raise Exception(f"cloud-hypervisor command failed/timed out: {command}") from e
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
    buildDependenciesOnly =
      {
        # Verified systems, which should work.
        "x86_64-linux" = false;
        # `aarch64-linux` fails on Hercules CI due to nested virtualization usage.
        # The build might be working, but after a 1 hour timeout, the machine barely gets into stage-2.
        # So for now, skip running the actual test.
        "aarch64-linux" = true;
      }
      .${pkgs.stdenv.hostPlatform.system} or true # Also ignore failure on any systems not otherwise listed.
    ;
  };

  # Some static values for ...
  # ... creating blockdevice backing files.
  imagePathPart = "/tmp/image";
  imageSize = "48M";
  # ... identifying QEMU's virtual Devices.
  blockdeviceVendorId = "46f4";
  blockdeviceProductId = "0001";
  hidVendorId = "0627";
  hidProductId = "0001";

  # Attrs for all supported USB versions and information for test construction.
  usbVersions = {
    "3" = {
      controller = "xHCI Host Controller";
      busName = "xhci";
      addr = "10";
    };
    "2" = {
      controller = "EHCI Host Controller";
      busName = "ehci";
      addr = "11";
    };
    "1.1" = {
      controller = "UHCI Host Controller";
      busName = "uhci";
      addr = "12";
    };
  };

  # Fill in a template for a udev rule.
  mkUdevRule = pciAddr: controller: port: symlink: ''
    ACTION=="add|change|bind", ATTRS{serial}=="0000:00:${pciAddr}.0", SUBSYSTEM=="usb", ATTRS{product}=="${controller}", ATTR{devpath}=="${port}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/${symlink}"
  '';

  # Create the partial usbvfiod argument string for either fs paths or fd ids
  mkSocketFlag =
    useFileDescriptor:
    if useFileDescriptor == false then
      "--socket-path ${usbvfiodSocket} --hotplug-socket-path ${usbvfiodSocketHotplug}"
    else
      "--fd 3 --hotplug-fd 4";

  # configure usbvfiod with `--socket-path` or `--fd`
  mkSystemdConfig = virtualDevices: debug: useFileDescriptor: {
    systemd.services = {
      usbvfiod = {
        wantedBy = lib.mkIf (!useFileDescriptor) [ "multi-user.target" ];
        serviceConfig = {
          User = "usbaccess";
          Group = "usbaccess";
          Restart = "on-failure";
          RestartSec = "2s";
          ExecStart = ''
            ${lib.getExe usbvfiod} ${lib.optionalString debug "-v"} ${mkSocketFlag useFileDescriptor} ${lib.concatStringsSep " " (builtins.map mkDeviceFlag virtualDevices)}
          '';
        };
        environment = {
          RUST_BACKTRACE = "full";
        };
      };

      cloud-hypervisor =
        let
          netboot = mkNetboot debug;
        in
        {
          wantedBy = [ "multi-user.target" ];
          requires = if useFileDescriptor then [ "usbvfiod.socket" ] else [ "usbvfiod.service" ];
          after = lib.mkIf (!useFileDescriptor) [ "usbvfiod.service" ];
          serviceConfig = {
            Restart = "on-failure";
            RestartSec = "2s";
            ExecStart = ''
              ${lib.getExe cloud-hypervisor} --memory size=2G,shared=on --console file=${guestLogFile} --serial off \
                --kernel ${netboot.kernel} \
                --cmdline ${lib.escapeShellArg netboot.cmdline} \
                --initramfs ${netboot.initrd} \
                --user-device socket=${usbvfiodSocket} \
                --net "tap=tap0,mac=,ip=192.168.100.1,mask=255.255.255.0"
            '';
          };
        };
    };

    systemd.sockets = lib.mkIf useFileDescriptor {
      usbvfiod = {
        description = "sockets to trigger usbvfiod service start and provide communication channels";
        wantedBy = [ "sockets.target" ];
        socketConfig = {
          ListenStream = [
            "${usbvfiodSocket}"
            "${usbvfiodSocketHotplug}"
          ];
          SocketMode = 0660;
          SocketUser = "usbaccess";
          SocketGroup = "usbaccess";
          Accept = "no";
        };
      };
    };
  };

  # Fill in a template for the qemu.options list for a blockdevice.
  mkQemuBlockdevice =
    driveId: driveFile: deviceBus: devicePort:
    "-drive if=none,id=${driveId},format=raw,file=${driveFile} -device usb-storage,bus=${deviceBus}.0,port=${devicePort},drive=${driveId}";

  # Fill in a template for the qemu.options list for a USB keyboard.
  mkQemuKeyboard = deviceBus: devicePort: "-device usb-kbd,bus=${deviceBus}.0,port=${devicePort}";

  # Add a usb to serial adapter.
  mkQemuSerialAdapter =
    deviceId: deviceSocket: deviceBus: devicePort:
    "-chardev socket,id=char${deviceId},path=/tmp/${deviceSocket}.sock,server=on,wait=off -device usb-serial,chardev=char${deviceId},id=${deviceId},bus=${deviceBus}.0,port=${devicePort}";

  # Create a usb device on our QEMU bus-id corresponding with the declared usb version.
  mkUsbDeviceType =
    testname: device:
    let
      deviceBus = usbVersions.${device.usbVersion}.busName;
    in
    if (!device.udevRule.enable || device.udevRule.symlink == "") then
      abort "udevRule is necessary to attach create qemu device before/on startup"
    else if (device.type == "block") then
      mkQemuBlockdevice "${deviceBus}-${device.udevRule.symlink}"
        "${imagePathPart}-${testname}-${device.udevRule.symlink}.img"
        "${deviceBus}"
        "${builtins.toString device.usbPort}"
    else if (device.type == "hid") then
      mkQemuKeyboard "${deviceBus}" "${builtins.toString device.usbPort}"
    else if (device.type == "serial") then
      mkQemuSerialAdapter "${device.udevRule.symlink}" "${testname}-${device.udevRule.symlink}" "${
        deviceBus
      }" "${builtins.toString device.usbPort}"
    else
      builtins.abort ''wrong device type; types supported are "block", "hid" and "serial"'';

  # Respect if attached at host on boot option is true to create the QEMU device option.
  mkUsbDevice =
    testname: device:
    if device.attachedOnStartup == "host" || device.attachedOnStartup == "guest" then
      mkUsbDeviceType testname device
    else
      ""; # Device should be handled via QEMU QMP in the testScript.

  # Create a testScript snippet to make a clean blockdevice image file.
  mkPrepareOneBlockdeviceImage =
    testname: device:
    let
      filepath = "${imagePathPart}-${testname}-${device.udevRule.symlink}.img";
    in
    ''
      import subprocess
      subprocess.run(["rm", "${filepath}"])
      print("Creating file image at ${filepath}")
      subprocess.run(["dd", "bs=1", "count=1", "seek=${imageSize}", "if=/dev/zero", "of=${filepath}"], timeout=30, check=True)
    '';

  # Create a testScript snippet to connect to the socket of the sub-serial chardev.
  mkPrepareOneSerialdevice =
    testname: device:
    let
      socket = "${device.udevRule.symlink}";
    in
    ''
      import socket
      ${socket} = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
      ${socket}.connect("/tmp/${testname}-${device.udevRule.symlink}.sock")
    '';

  # Decide if a virtual device needs to be prepared before qemu starts.
  mkPrepareDevicePreQemu =
    testname: device:
    if device.type == "block" then mkPrepareOneBlockdeviceImage testname device else "";

  # Decide if a virtual device needs to be prepared after qemu starts.
  mkPrepareDevicePostQemu =
    testname: device: if device.type == "serial" then mkPrepareOneSerialdevice testname device else "";

  # Generate usbvfiod argument flags to hand over the device through their udev generated symlink.
  mkDeviceFlag =
    device:
    if
      device.attachedOnStartup == "guest" && (!device.udevRule.enable || device.udevRule.symlink == "")
    then
      abort "udevRule is necessary to attach device before startup of usbvfiod"
    else if device.attachedOnStartup == "guest" then
      ''--device "/dev/bus/usb/${device.udevRule.symlink}"''
    else
      "";

  # Input type check for list of virtualDevices in the attrs.
  sanityCheckDevice =
    device:
    assert (device.type == "block" || device.type == "hid" || device.type == "serial");
    assert (device.usbVersion == "1.1" || device.usbVersion == "2" || device.usbVersion == "3");
    assert (builtins.typeOf device.usbPort == "int" || builtins.typeOf device.usbPort == "string");
    assert (builtins.typeOf device.udevRule.enable == "bool");
    assert (builtins.typeOf device.udevRule.symlink == "string");
    assert (
      device.attachedOnStartup == "none"
      || device.attachedOnStartup == "host"
      || device.attachedOnStartup == "guest"
    );
    true;

  # Input type check for the attrs arg.
  sanityCheckArgs =
    args:
    assert (builtins.typeOf args.name == "string");
    assert (builtins.typeOf args.debug == "bool");
    assert (builtins.typeOf args.virtualDevices == "list");
    assert (builtins.typeOf args.testScript == "string");
    assert (builtins.all sanityCheckDevice args.virtualDevices);
    args;

  # If possible use default values for not set things.
  mkDefaults =
    args:
    let
      deviceCount = builtins.length args.virtualDevices;

      # The defined default values to generate a test argument attrs.
      virtualDevice = {
        type = "block";
        usbVersion = "3";
        usbPort = 1;
        udevRule.enable = true;
        udevRule.symlink = "testdevice";
        attachedOnStartup = "guest";
      };

      attrs = {
        debug = true;
        useFileDescriptor = false;
      }
      // args
      // {
        virtualDevices = builtins.genList (
          i: lib.recursiveUpdate virtualDevice (builtins.elemAt args.virtualDevices i)
        ) deviceCount;
      };

    in
    attrs;

  # See mkUsbTest (this runs without any arg checks).
  mkUsbTestChecked =
    args:
    pkgs.testers.runNixOSTest {
      inherit (args) name;

      inherit globalTimeout passthru;

      nodes.machine = _: {
        imports = [
          basicMachineConfig
          (mkSystemdConfig args.virtualDevices args.debug args.useFileDescriptor)
        ];

        services = {
          # The framework automatically forwards all journal output to ttyS0,
          # slowing down the test significantly if there is a lot of logs.
          journald.extraConfig = lib.mkForce ''
            ForwardToConsole=yes
            TTYPath=/dev/hvc1
          '';
          # Create a udev rule for every device listed that enables it.
          udev.extraRules = lib.concatStrings (
            builtins.map (
              device:
              if device.udevRule.enable then
                let
                  usbPort = builtins.toString device.usbPort;
                in
                if (usbPort == "" || device.udevRule.symlink == "") then
                  abort "A udev rules requires to set a usbPort and a symlink string"
                else
                  ''
                    ${mkUdevRule usbVersions.${device.usbVersion}.addr usbVersions.${device.usbVersion}.controller
                      usbPort
                      device.udevRule.symlink
                    }
                  ''
              else
                ""
            ) args.virtualDevices
          );
        };

        virtualisation = {
          cores = 2;
          memorySize = 4096;
          # Removing this Keyboard makes the optional USB Keyboard the default to send QMP key-events.
          qemu.virtioKeyboard = false;
          qemu.options = [
            # Add the xhci controller to use USB 3.0.
            "-device qemu-xhci,id=${usbVersions."3".busName},addr=${usbVersions."3".addr}"

            # Add the ehci controller to use USB 2.0.
            "-device usb-ehci,id=${usbVersions."2".busName},addr=${usbVersions."2".addr}"

            # Add the uhci controller to use USB 1.1.
            "-device piix3-usb-uhci,id=${usbVersions."1.1".busName},addr=${usbVersions."1.1".addr}"

            # Add a virtio-console device to use it for bulk logs instead of serial.
            # Set a addr to have the test-frameworks default virtio-console remain
            # at hvc0 and not accidentally switch hvc0 and hvc1 thus breaking the test.
            "-device virtio-serial,addr=13,id=virtserial"
            "-chardev file,id=charvirtcon,path=${qemuLogFile}"
            "-device virtconsole,chardev=charvirtcon,bus=virtserial.0"

            # Enable the QEMU QMP interface to trigger HID events or hotplug devices at runtime.
            "-chardev socket,id=qmp,path=/tmp/qmp.sock,server=on,wait=off"
            "-mon chardev=qmp,mode=control,pretty=on"
          ]
          # Handle each entry of the args.virtualDevices list.
          ++ (builtins.map (mkUsbDevice args.name) args.virtualDevices);
        };
      };
      testScript = ''
        ${nestedPythonClass}

        # prepare devices before qemu is started
        ${lib.concatStringsSep "\n" (builtins.map (mkPrepareDevicePreQemu args.name) args.virtualDevices)}

        start_all()

        # prepare devices that depend on qemu already being started
        ${lib.concatStringsSep "\n" (builtins.map (mkPrepareDevicePostQemu args.name) args.virtualDevices)}

        machine.wait_for_unit("cloud-hypervisor.service")

        # Check sshd in systemd.services.cloud-hypervisor is usable prior to testing over ssh.
        machine.wait_until_succeeds("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2 'exit 0'", timeout=3000)

        cloud_hypervisor = Nested(vm_host=machine)

        code = r''''${args.testScript}''''

        try:
          exec(code, globals(), locals())
        finally:
          logs = open("${qemuLogFile}", "r").read()
          print(f'\n<<<<<MACHINE LOGS>>>>>\n{logs}\n<<<<<END MACHINE LOGS>>>>>\n')

        # Include the provided script verbatim as dead code for the linter checks.
        raise SystemExit
        ${args.testScript}
      '';
    };

in
{
  # some simple const values
  inherit
    blockdeviceVendorId
    blockdeviceProductId
    hidVendorId
    hidProductId
    usbvfiodSocket
    usbvfiodSocketHotplug
    guestLogFile
    imageSize
    ;

  # attrs to generate tests for all three usb version
  inherit usbVersions;

  # helper functions
  inherit mkNetboot mkDeviceFlag;

  /**
    Create a pkgs.testers.runNixOSTest with specific purpose of testing Usbvfiod.
    The Functions purpose is to remove duplicated lines, make comparing tests easier
    and write new tests with less boilerplate.

    For the testscript this function provides an object from the nested running
    `cloud_hypervisor` vm, that can use `.succeed()` and `.wait_until_succeeds()`
    just like the qemu nixos test `machine` object can.

    When using more than one device, each shall define its `usbPort` and
    `udevRule.symlink` (the default value is static).

    For each serial device this function provides a python socket object with the
    name equal to the symlink. Use this object to send and receive data over the
    usb-serial adapter.
    Note: Serial devices cannot use `attachedOnStartup = "none";`.
    Note: Serial symlink strings have to follow python variable naming restrictions.

    # Inputs

    `args`

    : 1\. Function argument

    # Type

    ```
    mkUsbTest :: {
      name :: String
      debug :: Bool
      useFileDescriptor :: Bool
      virtualDevices :: [
        {
        type :: "block" || "hid" || "serial"
        usbVersion :: "1.1" || "2" || "3"
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

    ## `mkUsbTest` minimal example

    ```nix
    myTest = mkUsbTest {
      name = "foo";
      virtualDevices = [
        {
          type = "block";
        }
      ];
      testScript = ''
        cloud_hypervisor.succeed("echo hello", timeout=60)
      '';
    };
    ```

    ## `mkUsbTest` full example

    ```nix
    myTest = mkUsbTest {
      name = "foo";
      debug = true;
      useFileDescriptor = false;
      virtualDevices = [
        {
          type = "block";
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
        search("ID ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
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
}
