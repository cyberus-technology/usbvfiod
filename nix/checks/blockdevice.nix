{
  testutils,
}:
let
  testScript = ''
    # Confirm USB controller pops up in boot logs
    out = cloud_hypervisor.succeed("journalctl -b", timeout=60)
    search("usb usb1: Product: xHCI Host Controller", out)
    search("hub 1-0:1\\.0: [0-9]+ ports? detected", out)

    # Confirm some diagnostic information
    out = cloud_hypervisor.succeed("cat /proc/interrupts", timeout=60)
    search(" +[1-9][0-9]* +PCI-MSIX.*xhci_hcd", out)

    # Wait until the usb drive we expect is recognized.
    out = cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=120)
    search("ID ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId} QEMU QEMU USB HARDDRIVE", out)
    out = cloud_hypervisor.succeed("sfdisk -l", timeout=60)
    search("Disk /dev/sda:", out)

    # Test partitioning
    cloud_hypervisor.succeed("echo ',,L' | sfdisk --label=gpt /dev/sda", timeout=60)

    # The Script is sometimes too fast for the nested guest to detect the new partition.
    cloud_hypervisor.wait_until_succeeds("lsblk /dev/sda1", timeout=60)

    # Make a filesystem
    out = cloud_hypervisor.succeed("mkfs.ext4 -v /dev/sda1", timeout=60)
    print(out)
    cloud_hypervisor.succeed("fsck -t ext4 -V -r /dev/sda1 -- -y", timeout=60)
    cloud_hypervisor.wait_until_succeeds("mount /dev/sda1 /mnt", timeout=60)

    # Create a file and compute a checksum
    cloud_hypervisor.succeed("dd if=/dev/urandom of=/tmp/file count=32 bs=1M", timeout=60)
    out = cloud_hypervisor.succeed("sha256sum /tmp/file", timeout=60)
    hash_tmp = out.split()
    print(f"hash_tmp: {hash_tmp}")

    # Copy the file on the blockdevice
    cloud_hypervisor.succeed("cp /tmp/file /mnt/file", timeout=60)
    cloud_hypervisor.succeed("sync", timeout=60)
    cloud_hypervisor.succeed("echo 3 > /proc/sys/vm/drop_caches", timeout=60)
    cloud_hypervisor.succeed("umount /mnt", timeout=60)
    cloud_hypervisor.succeed("mount /dev/sda1 /mnt", timeout=60)

    # Check if the file checksum changed
    out = cloud_hypervisor.succeed("sha256sum /mnt/file", timeout=60)
    hash_mnt = out.split()
    print(f"hash_mnt: {hash_mnt}")
    if (hash_tmp[0] != hash_mnt[0]):
      raise RequestedAssertionFailed("The checksum changed after copying to the mounted USB blockdevice.")
  '';
in
builtins.listToAttrs (
  builtins.map (usbVersion: {
    name = "blockdevice-usb-${builtins.replaceStrings [ "." ] [ "_" ] usbVersion}";
    value = testutils.mkUsbTest {
      name = "blockdevice-usb-${usbVersion}";
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
