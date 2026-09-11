{
  testutils,
}:
let
  # Using the default symlink should have also been possible but this feels
  # easier to read.
  symlink = "serialdevice";
in
testutils.mkUsbTest {
  name = "usb-serial-adapter";
  debug = true;
  virtualDevices = [
    {
      type = "serial";
      attachedOnStartup = "guest";
      udevRule.symlink = symlink;
    }
  ];
  testScript = ''
    # confirm usb device in the host
    out = machine.succeed("lsusb", timeout=ONE_MINUTE)
    print(out)
    search("Future Technology Devices International", out)
    search("Ltd FT232 Serial", out) # Using the rest of the name that is cut off errors for some reason.

    # confirm usb device in the guest
    out = cloud_hypervisor.succeed("lsusb", timeout=ONE_MINUTE)
    print(out)
    search("Future Technology Devices International", out)
    search("Ltd FT232 Serial", out) # Using the rest of the name that is cut off errors for some reason.

    # confirm attachment to the serial driver
    out = cloud_hypervisor.succeed("ls -l /dev/ttyUSB0", timeout=ONE_MINUTE)
    print(out)

    for i in range(1, 4):
      # Use this screen session to send and receive.
      # -dm ignore $STY for session creation in detached mode
      # -S sessionname
      cloud_hypervisor.succeed(f'screen -dm -S serialsession{i} /dev/ttyUSB0 115200', timeout=ONE_MINUTE)
      out = cloud_hypervisor.wait_until_succeeds('lsof /dev/ttyUSB0')
      print(out)

      # Write inside the nested guest...
      message = "guest is writing into the serial"
      # -X send command `stuff` and arg to the with -S specified session
      cloud_hypervisor.succeed(f'screen -S serialsession{i} -X stuff "{message}"', timeout=ONE_MINUTE)

      # ...and use the socket buffer to receive in the host.
      out = ${symlink}.recv(32)
      print("socket in host received: " + out.decode())
      search("guest is writing into the serial", out.decode())

      # Write from the host...
      ${symlink}.sendall(b"host is writing into the emulated usb-serial device")

      # ...and expect the string in the active screen sessions output.
      out = cloud_hypervisor.succeed(f'screen -S serialsession{i} -X hardcopy /tmp/screen_output.txt', timeout=ONE_MINUTE)
      print(out)
      out = cloud_hypervisor.succeed('cat /tmp/screen_output.txt', timeout=ONE_MINUTE)
      print("ttyUSB in guest received: " + out)
      search("host is writing into the emulated usb-serial device", out)

      # clean exit
      cloud_hypervisor.succeed(f'screen -S serialsession{i} -X quit', timeout=ONE_MINUTE)
      cloud_hypervisor.succeed('while lsof /dev/ttyUSB0; do sleep 0.5; done', timeout=ONE_MINUTE)
  '';
}
