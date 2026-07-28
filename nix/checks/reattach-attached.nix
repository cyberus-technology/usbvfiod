{
  usbvfiod,
  testutils,
}:
{
  reattach-attached = testutils.mkUsbTest {
    name = "reattach-attached";
    virtualDevices = [
      {
        type = "block";
      }
    ];
    testScript = ''
      # The device is attached to the controller by usbvfiod on startup.
      cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=120)

      # Reattach the same host device while the controller still has it attached.
      # The remote already reset the USB device before sending the request, so the
      # controller must detach the old instance and attach the new one.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --attach /dev/bus/usb/testdevice", timeout=60)
      print(out)
      search("SuccessfulOperation", out)

      # Confirm the controller explicitly detached the conflicting device before
      # continuing with the attach.
      out = machine.succeed("journalctl -u usbvfiod.service -b --no-pager", timeout=60)
      print(out)
      search("A device with the same identifier is already attached and will be detached first", out)
      search("Detached device", out)

      cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=120)
    '';
  };
}
