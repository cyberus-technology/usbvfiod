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
testutils.mkUsbTest {
  name = "multiple-blockdevices";
  debug = false;
  virtualDevices =
    builtins.concatMap
      (
        usb:
        builtins.map
          (num: {
            type = "blockdevice";
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
    search(r'sda\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdb\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdc\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdd\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sde\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdf\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdg\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
    search(r'sdh\s+\d+:\d+\s+0\s+${testutils.imageSize}\s+0\s+disk', out)
  '';
} systemd-config
