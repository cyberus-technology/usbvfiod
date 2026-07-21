{
  lib,
  cloud-hypervisor,
  usbvfiod,
  testutils,
}:
let
  systemd-config = args: {
    systemd.services = {
      usbvfiod = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          User = "usbaccess";
          Group = "usbaccess";
          Restart = "on-failure";
          RestartSec = "2s";
          ExecStart = ''
            ${lib.getExe usbvfiod} ${
              if args.debug then "-v" else ""
            } --socket-path ${testutils.usbvfiodSocket} --hotplug-socket-path ${testutils.usbvfiodSocketHotplug} ${lib.concatStringsSep " " (builtins.map testutils.mkDeviceFlag args.virtualDevices)}
          '';
        };
        environment = {
          RUST_BACKTRACE = "full";
        };
      };

      cloud-hypervisor =
        let
          netboot = testutils.mkNetboot args.debug;
        in
        {
          wantedBy = [ "multi-user.target" ];
          requires = [ "usbvfiod.service" ];
          after = [ "usbvfiod.service" ];
          serviceConfig = {
            Restart = "on-failure";
            RestartSec = "2s";
            ExecStart = ''
              ${lib.getExe cloud-hypervisor} --memory size=2G,shared=on --console file=${testutils.guestLogFile} --serial off \
                --kernel ${netboot.kernel} \
                --cmdline ${lib.escapeShellArg netboot.cmdline} \
                --initramfs ${netboot.initrd} \
                --user-device socket=${testutils.usbvfiodSocket} \
                --net "tap=tap0,mac=,ip=192.168.100.1,mask=255.255.255.0"
            '';
          };
        };
    };
  };
in
builtins.listToAttrs (
  builtins.map (usbVersion: {
    name = "attach-detach-usb-${builtins.replaceStrings [ "." ] [ "_" ] usbVersion}";
    value = testutils.mkUsbTest {
      name = "attach-detach-usb-${usbVersion}";
      virtualDevices = [
        {
          type = "blockdevice";
          inherit usbVersion;
          udevRule.symlink = "usbdevice";
          attachedOnStartup = "host";
        }
      ];
      testScript = ''
        # Run the attach-detach loop a few times.
        for i in range(1,20):
          print(f"ATTACH DETACH LOOP {i}")
          # List and print all attached devices.
          out = machine.wait_until_succeeds("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --list", timeout=60)
          search("No attached devices", out)

          # Attach a device.
          out = machine.succeed("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --attach /dev/bus/usb/usbdevice", timeout=60)
          print(out)

          # List attached devices.
          out = machine.succeed("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --list", timeout=60)
          print(out)

          # Get the bus and device numbers.
          (bus_nr, device_nr) = re.search(r'(\d{3}):(\d{3})',out).groups()
          print(f"Bus number: {bus_nr}, Device number: {device_nr}")

          # Wait for the guest to find the usb device.
          if (i % 2 == 0):
            cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=120)

          # Wait for the guest to find the blockdevice.
          if (i % 4 == 0):
            cloud_hypervisor.wait_until_succeeds("lsblk /dev/sd*", timeout=120)

          # Detach the device.
          out = machine.succeed(f"${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --detach {bus_nr} {device_nr}", timeout=60)
          print(out)
      '';
    } systemd-config;
  }) (builtins.attrNames testutils.usbVersions)
)
