{
  usbvfiod,
  testutils,
}:
let
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
      inherit testScript;
    };
  }) (builtins.attrNames testutils.usbVersions)
)
