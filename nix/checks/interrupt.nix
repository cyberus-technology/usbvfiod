{
  testutils,
}:
builtins.listToAttrs (
  builtins.map (usbVersion: {
    name = "interrupt-endpoint-usb-${builtins.replaceStrings [ "." ] [ "_" ] usbVersion}";
    value = testutils.mkUsbTest {
      name = "interrupt-endpoint-usb-${usbVersion}";
      virtualDevices = [
        {
          type = "hid-device";
          inherit usbVersion; # note: this changes the /dev/input/by-id path used in the script (xhci/ehci bus number)
          usbPort = 1; # note: this changes the /dev/input/by-id path used in the script
          udevRule.symlink = "keyboard";
        }
      ];
      testScript = ''
        import time
        import threading

        # A function that can send input events in the background.
        def create_input():
          for i in range(1, 4):
            time.sleep(1)
            machine.send_key("ctrl")
            print(f"input loop `{i}` done")

        # Check the Keyboard is in detected in the guest.
        cloud_hypervisor.succeed("lsusb -d ${testutils.hidVendorId}:${testutils.hidProductId}", timeout=60)

        # Generate inputs in the background.
        t1 = threading.Thread(target=create_input)
        t1.start()
        print("started sending input events")

        # Catch one key down event and one key up event inputs.
        # It is theoretically possible all events appear and are consumed by the input subsystem before we have the opportunity to listen.
        out = cloud_hypervisor.succeed("hexdump --length 144 --two-bytes-hex /dev/input/by-id/usb-QEMU_QEMU_USB_Keyboard_68284-0000\\:00\\:${
          testutils.usbVersions."${usbVersion}".addr
        }.0-1-event-kbd", timeout=60)

        # Check if the hexdump contains a ctrl event sequence
        # https://docs.kernel.org/input/input.html#event-interface
        search("0001    001d    0001", out) # EV_KEY KEY_LEFTCTRL pressed
        search("0001    001d    0000", out) # EV_KEY KEY_LEFTCTRL released
        print("done")

        # Make a clean exit since the test will wait for thread termination either way.
        t1.join()
      '';
    };
  }) (builtins.attrNames testutils.usbVersions)
)
