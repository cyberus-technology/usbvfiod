{
  testutils,
}:
let
  iface = testutils.usbNetInterface;
  gateway = testutils.usbNetGatewayIp;
  guestIp = testutils.usbNetGuestIp;
  port = builtins.toString testutils.usbNetHostPort;

  testScript = ''
    import hashlib
    import os
    import socket

    def sysfs_stat(name):
      return int(cloud_hypervisor.succeed(f"cat /sys/class/net/${iface}/statistics/{name}", timeout=ONE_MINUTE))

    def wait_for_listener():
      cloud_hypervisor.wait_until_succeeds("ss -Hltn sport = :${port} | grep -q LISTEN", timeout=ONE_MINUTE)

    # The device enumerates in the guest and the CDC Ethernet driver binds to the device.
    out = cloud_hypervisor.succeed("lsusb", timeout=ONE_MINUTE)
    print(out)
    cloud_hypervisor.wait_until_succeeds("test -e /sys/class/net/${iface}", timeout=TWO_MINUTES)
    out = cloud_hypervisor.succeed("readlink /sys/class/net/${iface}/device/driver", timeout=ONE_MINUTE)
    t.assertIn("cdc_ether", out)

    # Bring the interface up.
    cloud_hypervisor.succeed("ip addr replace ${guestIp}/24 dev ${iface}", timeout=ONE_MINUTE)
    cloud_hypervisor.succeed("ip link set ${iface} up", timeout=ONE_MINUTE)
    cloud_hypervisor.wait_until_succeeds("grep -q 1 /sys/class/net/${iface}/carrier", timeout=ONE_MINUTE)

    rx_before = sysfs_stat("rx_bytes")
    tx_before = sysfs_stat("tx_bytes")

    # Basic connectivity with the user-mode network gateway.
    cloud_hypervisor.succeed("ping -I ${iface} -c 5 ${gateway}", timeout=ONE_MINUTE)

    # Sweep all ICMP payload sizes up to the MTU. The device is full-speed with a
    # max packet size of 64 bytes, so the sweep covers every frame length modulo 64,
    # including frames that end exactly on a packet boundary.
    cloud_hypervisor.succeed(
      "for s in $(seq 0 1472); do ping -I ${iface} -c 1 -W 5 -s $s ${gateway} > /dev/null || { echo failed at payload size $s; exit 1; }; done",
      timeout=TWO_MINUTES * 5)

    size = 4 * 1024 * 1024

    # Guest to host
    cloud_hypervisor.succeed(f"head -c {size} /dev/urandom > /tmp/tx.bin", timeout=ONE_MINUTE)
    expected = cloud_hypervisor.succeed("sha256sum /tmp/tx.bin", timeout=ONE_MINUTE).split()[0]
    cloud_hypervisor.succeed("systemd-run --unit=usbnet-tx socat -u FILE:/tmp/tx.bin TCP-LISTEN:${port},reuseaddr", timeout=ONE_MINUTE)
    wait_for_listener()
    received = bytearray()
    with socket.create_connection(("127.0.0.1", ${port}), timeout=600) as s:
      while chunk := s.recv(65536):
        received += chunk
    print(f"host received {len(received)} bytes")
    t.assertEqual(len(received), size)
    t.assertEqual(hashlib.sha256(received).hexdigest(), expected)

    # Host to guest
    payload = os.urandom(size)
    cloud_hypervisor.succeed("systemd-run --unit=usbnet-rx socat -u TCP-LISTEN:${port},reuseaddr CREATE:/tmp/rx.bin", timeout=ONE_MINUTE)
    wait_for_listener()
    with socket.create_connection(("127.0.0.1", ${port}), timeout=600) as s:
      s.sendall(payload)
    cloud_hypervisor.wait_until_succeeds("! systemctl is-active usbnet-rx", timeout=TWO_MINUTES * 5)
    out = cloud_hypervisor.succeed("sha256sum /tmp/rx.bin", timeout=ONE_MINUTE)
    t.assertEqual(out.split()[0], hashlib.sha256(payload).hexdigest())

    # Make sure the traffic actually went over the usb-net interface.
    out = cloud_hypervisor.succeed("ip -s -s link show ${iface}", timeout=ONE_MINUTE)
    print(out)
    t.assertGreater(sysfs_stat("rx_bytes") - rx_before, size)
    t.assertGreater(sysfs_stat("tx_bytes") - tx_before, size)
  '';
in
builtins.listToAttrs (
  builtins.map
    (usbVersion: {
      name = "usb-net-usb-${builtins.replaceStrings [ "." ] [ "_" ] usbVersion}";
      value = testutils.mkUsbTest {
        name = "usb-net-usb-${usbVersion}";
        virtualDevices = [
          {
            type = "net";
            inherit usbVersion;
            udevRule.symlink = "netdevice";
          }
        ];
        inherit testScript;
      };
    })
    # QEMU's usb-net is full-speed only and cannot attach to the EHCI controller.
    (builtins.filter (usbVersion: usbVersion != "2") (builtins.attrNames testutils.usbVersions))
)
