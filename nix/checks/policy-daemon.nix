{
  pkgs,
  lib,
  testutils,
  usbvfiod,
}:
let
  vfio_user_socket_path = instance: "/run/usb-passthrough/${instance}/vfio_user.sock";
  hotplug_socket_path = instance: "/run/usb-passthrough/${instance}/hotplug.sock";
  policy_file_path = "/run/usb-passthrough/policies.json";

  instances = [
    "vm1"
    "vm2"
  ];

  policies = vm: ''
    [{
    "vendor_id": "${testutils.blockdeviceVendorId}",
    "product_id": "${testutils.blockdeviceProductId}",
    "socket_path": "${hotplug_socket_path vm}"
    }]
  '';

  machine = {
    imports = [
      testutils.basicMachineConfig
    ]
    ++ (map (instance: {
      # A user/group per vmm instance, gets vfio-user socket access.
      users.groups."vmm-${instance}" = { };
      users.users."vmm-${instance}" = {
        isSystemUser = true;
        group = "vmm-${instance}";
      };

      systemd.services = {
        "usbvfiod@${instance}" = {
          serviceConfig = {
            # The controller runs as a separate/dynamic user.
            DynamicUser = true;
            Restart = "on-failure";
            RestartSec = "2s";
            ExecStart = ''
              ${lib.getExe usbvfiod} -v --fd 3 --hotplug-fd 4
            '';
          };
          environment = {
            RUST_BACKTRACE = "full";
          };
        };
      };

      systemd.sockets = {
        "usbvfiod@${instance}" = {
          description = "USB pass-through controller sockets for ${instance}";
          wantedBy = [ "sockets.target" ];
          socketConfig = {
            ListenStream = [
              "${vfio_user_socket_path instance}"
              "${hotplug_socket_path instance}"
            ];
            DirectoryMode = 0711;
            SocketMode = 0660;
            SocketUser = "vmm-${instance}";
            SocketGroup = "usbpolicy";
            Accept = "no";
          };
        };
      };
    }) instances);

    # A user/group for the policy daemon to access all hotplug sockets.
    users.groups.usbpolicy = { };
    users.users.usbpolicy = {
      isSystemUser = true;
      group = "usbpolicy";
    };

    systemd.services.usb-policy-daemon = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        User = "usbpolicy";
        Group = "usbpolicy";
        SupplementaryGroups = [ "usbaccess" ];
        Restart = "on-failure";
        RestartSec = "2s";
        ExecStart = ''
          ${usbvfiod}/bin/policy_daemon -v --config ${policy_file_path}
        '';
        ExecReload = ''
          ${pkgs.util-linux}/bin/kill -HUP $MAINPID
        '';
      };
      environment = {
        RUST_BACKTRACE = "full";
      };
    };

    # All USB devices are accessible by the `usbaccess` group.
    services.udev.extraRules = ''
      SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", MODE="0660", GROUP="usbaccess"
    '';

    # Policy configurations for use in the test.
    environment.etc = {
      "usb-policy-vm1.json".text = policies "vm1";
      "usb-policy-vm2.json".text = policies "vm2";
    };

    virtualisation = {
      cores = 2;
      memorySize = 1024;
      qemu.options = with testutils.usbVersions."3"; [
        # Add the xhci controller to use USB 3.0.
        "-device qemu-xhci,id=${busName},addr=${addr}"

        # Enable the QEMU QMP interface to trigger HID events or hotplug devices at runtime.
        "-chardev socket,id=qmp,path=/tmp/qmp.sock,server=on,wait=off"
        "-object monitor-qmp,chardev=qmp,pretty=on,id=qmp0"
      ];
    };
  };

  testScript = ''
    import subprocess
    import datetime

    ONE_MINUTE=datetime.timedelta(minutes=1)
    ONE_SECOND=datetime.timedelta(seconds=1)

    def assert_attached(socket: str, attached: bool):
      out = machine.succeed(f"${usbvfiod}/bin/remote --socket {socket} --list", timeout=ONE_MINUTE)
      if attached:
        t.assertIn("One attached device", out)
      else:
        t.assertIn("No attached devices", out)

    # Create a blockdevice.
    subprocess.run(["dd", "bs=1", "count=1", "seek=${testutils.imageSize}", "if=/dev/zero", "of=/tmp/hotplug.img"], timeout=30, check=True)

    # Policy daemon loads with no active rules yet.
    machine.wait_for_unit('usb-policy-daemon.service')

    # Expect no attached devices.
    assert_attached("${hotplug_socket_path "vm1"}", False)
    assert_attached("${hotplug_socket_path "vm2"}", False)

    # Configure rules.
    machine.succeed("ln -sf /etc/usb-policy-vm1.json ${policy_file_path}")
    machine.succeed("systemctl reload usb-policy-daemon.service", timeout=ONE_MINUTE)

    # Attach USB device.
    print(machine.send_monitor_command("drive_add 0 id=hotplug,if=none,file=/tmp/hotplug.img,format=raw"))
    print(machine.send_monitor_command("device_add usb-storage,id=hotplug-dev,bus=${
      testutils.usbVersions."3".busName
    }.0,drive=hotplug,port=1"))

    # Wait for qemu host to find the blockdevice.
    machine.wait_until_succeeds("lsusb | grep 'QEMU QEMU USB HARDDRIVE'", timeout=ONE_MINUTE)

    # Wait a little before checking the socket(s).
    machine.sleep(ONE_SECOND)

    # Expect device attached to vm1, no attached device in vm2.
    assert_attached("${hotplug_socket_path "vm1"}", True)
    assert_attached("${hotplug_socket_path "vm2"}", False)

    # Change rules / reload.
    machine.succeed("ln -sf /etc/usb-policy-vm2.json ${policy_file_path}")
    machine.succeed("systemctl reload usb-policy-daemon.service", timeout=ONE_MINUTE)

    # Wait a little before checking the socket(s).
    machine.sleep(ONE_SECOND)

    # Expect device attached to vm2, no attached device in vm1.
    assert_attached("${hotplug_socket_path "vm1"}", False)
    assert_attached("${hotplug_socket_path "vm2"}", True)

    # Restart usbvfiod of vm2 (losing device).
    machine.succeed("systemctl restart usbvfiod@vm2.service", timeout=ONE_MINUTE)

    # Expect no attached devices.
    assert_attached("${hotplug_socket_path "vm1"}", False)
    assert_attached("${hotplug_socket_path "vm2"}", False)

    # Reload rules, reattaching lost device.
    machine.succeed("systemctl reload usb-policy-daemon.service", timeout=ONE_MINUTE)

    # Wait a little before checking the socket(s).
    machine.sleep(ONE_SECOND)

    # Expect device attached to vm2, no attached device in vm1.
    assert_attached("${hotplug_socket_path "vm1"}", False)
    assert_attached("${hotplug_socket_path "vm2"}", True)

    # Detach USB device.
    print(machine.send_monitor_command("device_del hotplug-dev"))

    # Wait for the host to realize the device is gone.
    machine.wait_until_fails("lsusb | grep 'QEMU QEMU USB HARDDRIVE'")

    # Wait a little before checking the socket(s).
    machine.sleep(ONE_SECOND)

    # Expect no attached devices.
    assert_attached("${hotplug_socket_path "vm1"}", False)
    assert_attached("${hotplug_socket_path "vm2"}", False)
  '';
in
{
  policy-daemon = pkgs.testers.runNixOSTest {
    inherit testScript;
    inherit (testutils) passthru;

    name = "policy-daemon";
    nodes.machine = _: machine;
  };
}
