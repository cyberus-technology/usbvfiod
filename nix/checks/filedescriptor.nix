{
  testutils,
}:
testutils.mkUsbTest {
  debug = true;
  name = "systemd-managed-filedescriptor";
  useFileDescriptor = true;
  virtualDevices = [
    {
      type = "block";
      usbVersion = "3";
    }
  ];
  testScript = ''
    out = cloud_hypervisor.succeed("lsusb", timeout=60)
    print(out)
    search("ID ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)

    # A nested guest reboot will break the vfio-user connection and usbvfiod will exit 0.
    # When Cloud Hypervisor re-connects to the systemd socket usbvfiod will be restarted.
    out = cloud_hypervisor.succeed("systemctl reboot", timeout=60)
    print(out)

    # confirm nested guest shutdown via usbvfiod exit message triggered by the closed vfio-user connection
    machine.wait_until_succeeds("journalctl -b -u usbvfiod.service | grep 'Deactivated successfully.' ", timeout=60)

    out = cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=240)

    # When using the socket path setup, we encounter failed to start logs for the cloud hypervisor service.
    # We do not expect to see this log when using the file descriptor setup.
    out = machine.fail("journalctl -b -u cloud-hypervisor.service | grep -q 'cloud-hypervisor.service: Failed with result'", timeout=60)
    print(out)
  '';
}
