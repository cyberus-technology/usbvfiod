{
  testutils,
}:
let
  testname = "usb-serial-adapter";
  symlink = "serialdevice";

  pipename = "${testname}-${symlink}";
in
testutils.mkUsbTest {
  name = "usb-serial-adapter";
  debug = false;
  virtualDevices = [
    {
      type = "serial";
      attachedOnStartup = "guest";
      udevRule.symlink = symlink;
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

    exception_holder = {}
    def read_host():
      try:
        print("starting to listen in the host")
        out = subprocess.run(['head', '-c', '32', '/tmp/${pipename}.out'], capture_output=True)
        print(out)
        out = out.stdout.decode()
        print("received string on the host:" + out)
        search("guest is writing into the serial", out)
      except Exception as e:
        exception_holder['exc_host'] = e

    def read_cloud_hypervisor():
      try:
        print("starting to listen in cloud hypervisor")
        out = cloud_hypervisor.succeed('head -c 29 /dev/ttyUSB0')
        print("received string on the guest:" + out)
        search("host is writing into the pipe", out)
      except Exception as e:
        exception_holder['exc_chv'] = e

    # listen on the host
    t_host = threading.Thread(target=read_host)
    t_host.start()

    time.sleep(3)

    # do a guest write
    cloud_hypervisor.succeed('echo "guest is writing into the serial" > /dev/ttyUSB0')
    print("waiting for the runner listener to return")
    t_host.join()
    if 'exc_host' in exception_holder:
      raise exception_holder['exc_host']

    time.sleep(3)

    # listen on the guest
    t_cloud_hypervisor = threading.Thread(target=read_cloud_hypervisor)
    t_cloud_hypervisor.start()

    time.sleep(3)

    # write in host
    with open('/tmp/${pipename}.in', "w") as fd:
      # for some reason removing any of those lines breaks the test
      fd.write('some\n')
      fd.flush()
      time.sleep(1)

      fd.write('host is writing into the pipe\n')
      fd.flush()

    time.sleep(3)

    print("waiting for the cloud_hypervisor listener to return")
    t_cloud_hypervisor.join()
    if 'exc_chv' in exception_holder:
      raise exception_holder['exc_chv']

    print("done")
  '';
}
