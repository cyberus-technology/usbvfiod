{
  testutils,
}:
testutils.mkUsbTest {
  name = "multiple-blockdevices";
  debug = false;
  virtualDevices =
    builtins.concatMap
      (
        usb:
        builtins.map
          (num: {
            type = "block";
            usbVersion = "${usb}";
            usbPort = num;
            udevRule.symlink = "usb-${usb}-device-${builtins.toString num}";
          })
          [
            1
            2
            3
            4
          ]
      )
      [
        "2"
        "3"
      ];
  testScript = ''
    out = cloud_hypervisor.succeed("lsusb --tree", timeout=ONE_MINUTE)
    t.assertRegex(out, r'Port 001: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M')
    t.assertRegex(out, r'Port 002: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M')
    t.assertRegex(out, r'Port 003: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M')
    t.assertRegex(out, r'Port 004: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 480M')
    t.assertRegex(out, r'Port 001: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M')
    t.assertRegex(out, r'Port 002: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M')
    t.assertRegex(out, r'Port 003: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M')
    t.assertRegex(out, r'Port 004: Dev \d+, If 0, Class=Mass Storage, Driver=usb-storage, 5000M')

    out = cloud_hypervisor.succeed("lsblk", timeout=ONE_MINUTE)
    t.assertRegex(out, r'sda\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdb\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdc\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdd\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sde\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdf\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdg\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
    t.assertRegex(out, r'sdh\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk')
  '';
}
