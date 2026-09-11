{
  testutils,
}:
let
  testScript = ''
    # Wait until the USB drive is recognized.
    out = cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=TWO_MINUTES)
    search("ID ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
    cloud_hypervisor.wait_until_succeeds("lsblk /dev/sda", timeout=TWO_MINUTES)

    # Run the controller reset loop a few times.
    for i in range(1, 4):
      print(f"CONTROLLER RESET LOOP {i}")

      # Remove the scsi device to avoid I/O errors when unloading the xhci_pci driver.
      cloud_hypervisor.succeed("echo 1 > /sys/block/sda/device/delete", timeout=ONE_MINUTE)

      # Wait for all device events to finish.
      cloud_hypervisor.succeed("udevadm settle", timeout=ONE_MINUTE)

      # Reload the guest xhci_pci driver to trigger a host controller reset.
      cloud_hypervisor.succeed("modprobe -r xhci_pci", timeout=TWO_MINUTES)
      cloud_hypervisor.succeed("modprobe xhci_pci", timeout=TWO_MINUTES)

      # Confirm raw block I/O still works after the controller reset.
      out = cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=TWO_MINUTES)
      search("ID ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
      cloud_hypervisor.wait_until_succeeds("lsblk /dev/sda", timeout=TWO_MINUTES)
      cloud_hypervisor.succeed(f"printf after-reset-{i} > /tmp/after-reset-{i}.txt", timeout=ONE_MINUTE)
      cloud_hypervisor.succeed(f"dd if=/tmp/after-reset-{i}.txt of=/dev/sda bs=512 seek=2048 count=1 conv=sync,fsync status=none", timeout=ONE_MINUTE)
      cloud_hypervisor.succeed("sync", timeout=ONE_MINUTE)
      cloud_hypervisor.succeed("echo 3 > /proc/sys/vm/drop_caches", timeout=ONE_MINUTE)
      cloud_hypervisor.succeed(f"dd if=/dev/sda of=/tmp/read-after-reset-{i}.txt bs=512 skip=2048 count=1 status=none", timeout=ONE_MINUTE)
      cloud_hypervisor.succeed(f"grep -ao after-reset-{i} /tmp/read-after-reset-{i}.txt", timeout=ONE_MINUTE)
  '';
in
builtins.listToAttrs (
  builtins.map (usbVersion: {
    name = "controller-reset-usb-${builtins.replaceStrings [ "." ] [ "_" ] usbVersion}";
    value = testutils.mkUsbTest {
      name = "controller-reset-usb-${usbVersion}";
      virtualDevices = [
        {
          type = "block";
          inherit usbVersion;
        }
      ];
      inherit testScript;
    };
  }) (builtins.attrNames testutils.usbVersions)
)
