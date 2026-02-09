# ATTENTION:
#
# This is a integration test meant to be used to test manually with a windows image.
# This will and is not meant to be used in the build Sandbox.
# This should also only be used with the environment variable XDG_RUNTIME_DIR set to
# some non tmpfs. Per default `nix run` will try to fit virtualisation.diskSize into
# your /run which is probably way too small.
#
# you will likely want to run these three aliases in separate shell:
# wintest/wintestDriver: set env and run interactive driver
# portforward: portforward the rdp port from CHV to your local machine
# rdpme: run xfreerdp to get the windows desktop

{
  lib,
  pkgs,
  usbvfiod,
  ...
}:
let
  # Some static values for ...
  # ... creating blockdevice backing files.
  imagePathPart = "/tmp/image";
  imageSize = "8M";
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

  # Putting the socket in a world-readable location is obviously not a
  # good choice for a production setup, but for this test it works
  # well.
  usbvfiodSocket = "/tmp/usbvfiod";
  usbvfiodSocketHotplug = "/tmp/hotplug";

  guestLogFile = "/tmp/console.log";
  qemuLogFile = "/tmp/qemu-vc.log";

  # Fill in a template for a udev rule.
  mkUdevRule = pciAddr: controller: port: symlink: ''
    ACTION=="add|change", ATTRS{serial}=="0000:00:${pciAddr}.0", SUBSYSTEM=="usb", ATTRS{product}=="${controller}", ATTR{devpath}=="${port}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/${symlink}"
  '';

  # Fill in a template for the qemu.options list for a blockdevice.
  mkQemuBlockdevice =
    driveId: driveFile: deviceBus: devicePort:
    "-drive if=none,id=${driveId},format=raw,file=${driveFile} -device usb-storage,bus=${deviceBus}.0,port=${devicePort},drive=${driveId}";

  windowsImageRaw = pkgs.callPackage ./windows-image-raw.nix { inherit pkgs; };

  fd = "blockdevice";
in
{
  windows-test = pkgs.testers.runNixOSTest {
    name = "windows-test";
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

    nodes.machine = _: {
      imports = [ ./chv-vm-module.nix ];

      # backdoor and utility
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

      virtualisation = {
        cores = 8;
        memorySize = 4096 * 4;
        diskSize = 1024 * 100;
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

          # Enable the QEMU QMP interface to trigger HID events or plug blockdevices at runtime.
          "-chardev socket,id=qmp,path=/tmp/qmp.sock,server=on,wait=off"
          "-mon chardev=qmp,mode=control,pretty=on"

          # a blockdevice device
          #"${mkQemuBlockdevice "someId" imagePathPart usbVersions."3".busName "1"}"
        ];
      };

      local.services.ch-vm = {
        enable = true;
        name = "win11"; # ref'ed
        startOnBoot = false;
        vcpus = 4;
        memory = "8G,shared=on";
        diskPath = "/etc/win11.raw";
      };

      # preparing etc will be done in boot stage 2
      # and will result in the following logs:
      /*
        machine: Guest root shell did not produce any data yet...
        machine:   To debug, enter the VM and run 'systemctl status backdoor.service'.
      */
      # this could be replaced by a copy in the testScript
      environment.etc."win11.raw" = {
        source = "${windowsImageRaw.outPath}/image.raw";
        mode = "0600";
      };

      services.udev.extraRules = ''
        ${mkUdevRule usbVersions."3".addr usbVersions."3".controller "1" fd}
      '';
      systemd.services = {
        usbvfiod = {
          wantedBy = [ "multi-user.target" ];
          serviceConfig = {
            User = "usbaccess";
            Group = "usbaccess";
            Restart = "on-failure";
            RestartSec = "2s";
            ExecStart = ''
              ${lib.getExe usbvfiod} -vv \
                --socket-path ${usbvfiodSocket} \
                --hotplug-socket-path ${usbvfiodSocketHotplug} \
            '';
            #                --device "/dev/bus/usb/${fd}"
          };
        };
      };

      networking.nat = {
        enable = true;
        externalInterface = "eth0";
        internalInterfaces = [ "tap0" ];
      };
      boot.kernel.sysctl = {
        "net.ipv4.conf.all.forwarding" = true;
        "net.ipv4.conf.default.forwarding" = true;
      };
      networking.firewall = {
        enable = false;
        allowedUDPPorts = [
          53
          67
        ]; # For DNS and DHCP if needed
        allowedTCPPorts = [ 53 ];
      };

    };

    testScript = ''
      # prepare blockdevice images if necessary
      #import os
      #os.system("rm ${imagePathPart}")
      #print("Creating file image at ${imagePathPart}")
      #os.system("dd bs=1  count=1 seek=${imageSize} if=/dev/zero of=${imagePathPart}")

      print("STARTING QEMU")
      start_all()

      out = machine.succeed("ls -ls /tmp")
      print(out)

      machine.wait_for_unit("usbvfiod.service")

      machine.wait_for_unit("default.target")

      print("STARTING WINDOWS11")
      machine.systemctl("start guestVM@win11.service")
      machine.wait_for_unit("guestVM@win11.service")
    '';

  };
}
