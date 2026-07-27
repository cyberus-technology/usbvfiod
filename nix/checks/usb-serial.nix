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
  name = "usb-serial-adapter";
  debug = true;
  virtualDevices = [
    {
      type = "serial";
      attachedOnStartup = "guest";
    }
  ];
  testScript = ''
    # confirm serial usb device in the host
    out = machine.succeed("lsusb")
    print(out)
    search("Future Technology Devices International", out)
    search("Ltd FT232 Serial", out) # Using the rest of the name that is cut off errors for some reason.

    # confirm serial usb device in the guest
    out = cloud_hypervisor.succeed("lsusb")
    print(out)
    search("Future Technology Devices International", out)
    search("Ltd FT232 Serial", out) # Using the rest of the name that is cut off errors for some reason.

    # log some device specifics
    out = cloud_hypervisor.succeed("lsusb -v -d 0403:6001")
    print(out)

    out = cloud_hypervisor.succeed("ls -ls /dev/ttyUSB0")
    print(out)

    cloud_hypervisor.succeed('echo "preparations done"')

    import time
    import threading

    def read_host():
      print("starting to listen in host")
      out = subprocess.run(['head', '-c', '32', '/tmp/usbserial.out'], capture_output=True)
      out = out.stdout.decode()
      print("received string on the host:" + out)
      search("guest is writing into the serial", out)

    def read_cloud_hypervisor():
      print("starting to listen in machine")
      out = cloud_hypervisor.succeed('head -c 29 /dev/ttyUSB0')
      print(out)
      search("host is writing into the pipe", out)

    # listen on the host
    t_host = threading.Thread(target=read_host)
    t_host.start()

    # do a guest write
    cloud_hypervisor.succeed('echo "guest is writing into the serial" > /dev/ttyUSB0')
    print("waiting for the runner listener to return")
    t_host.join()

    # listen on the guest
    t_cloud_hypervisor = threading.Thread(target=read_cloud_hypervisor)
    t_cloud_hypervisor.start()

    # write in host
    with open('/tmp/usbserial.in', "w") as fd:
      # for some reason removing any of those lines breaks the test
      fd.write('some\n')
      fd.flush()
      time.sleep(1)

      fd.write('host is writing into the pipe\n')
      fd.flush()

    print("waiting for the cloud_hypervisor listener to return")
    t_cloud_hypervisor.join()

    print("done")
  '';
} systemd-config
