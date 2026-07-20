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
  name = "forceful removal";
  virtualDevices = [
    {
      type = "blockdevice";
      usbVersion = "3";
      usbPort = 1;
      udevRule.enable = true;
      udevRule.symlink = "hotplug";
      attachedOnStartup = "none";
    }
  ];
  testScript = ''
    import subprocess

    # create a blockdevice
    subprocess.run(["dd", "bs=1", "count=1", "seek=${testutils.imageSize}", "if=/dev/zero", "of=/tmp/hotplug.img"], timeout=30, check=True)

    for i in range(1,17):
      print(f"ATTACH DETACH LOOP {i}")

      # Expect no attached devices.
      out = machine.wait_until_succeeds("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --list", timeout=60)
      search("No attached devices", out)

      # plug in a blockdevice
      print(machine.send_monitor_command("drive_add 0 id=hotplug,if=none,file=/tmp/hotplug.img,format=raw"))
      print(machine.send_monitor_command("device_add usb-storage,id=hotplug-dev,bus=${
        testutils.usbVersions."3".busName
      }.0,drive=hotplug,port=1"))

      # wait for qemu host to find the blockdevice
      machine.wait_until_succeeds("lsusb | grep 'QEMU QEMU USB HARDDRIVE'", timeout=120)
      machine.wait_until_succeeds("lsblk /dev/sd*", timeout=120)

      machine.wait_until_succeeds("ls /dev/bus/usb/hotplug", timeout=60)

      # Attach a device.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --attach /dev/bus/usb/hotplug", timeout=60)
      print(out)

      # List attached devices.
      out = machine.succeed("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --list", timeout=60)
      print(out)

      # Check the list output is what we expect: one device
      listed_devices_count = len(re.findall(r'(\d{3}):(\d{3})',out))
      if listed_devices_count != 1:
        raise RequestedAssertionFailed(
          f"The `remote --list` output contains a wrong count of devices (expected 1, got {listed_devices_count})"
        )

      # Get the bus and device numbers.
      (bus_nr, device_nr) = re.search(r'(\d{3}):(\d{3})',out).groups()
      print(f"Bus number: {bus_nr}, Device number: {device_nr}")

      # Wait for the guest to find the usb device.
      if (i % 2 == 0):
        cloud_hypervisor.wait_until_succeeds("lsusb -d ${testutils.blockdeviceVendorId}:${testutils.blockdeviceProductId}", timeout=120)

      # Wait for the guest to find the blockdevice.
      if (i % 4 == 0):
        cloud_hypervisor.wait_until_succeeds("lsblk /dev/sd*", timeout=120)

      # Plug out a blockdevice.
      print(machine.send_monitor_command("device_del hotplug-dev"))

      # Wait for the qemu host to realize the missing usb device.
      machine.wait_until_fails("lsusb | grep 'QEMU QEMU USB HARDDRIVE'")

      # Trigger the guest to access the device and get the nusb error that would otherwise come eventually.
      # If it did not already happen the xHC should then start the detach process of the device.
      cloud_hypervisor.wait_until_succeeds("lsusb -vvv", timeout=120)

      # Wait until the xHC can confirm there is no device attached anymore.
      machine.wait_until_succeeds("${usbvfiod}/bin/remote --socket ${testutils.usbvfiodSocketHotplug} --list | grep 'No attached devices'", timeout=60)
  '';
} systemd-config
