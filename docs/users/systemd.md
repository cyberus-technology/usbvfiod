# Systemd setup with socket units

**Why?**

An upstream service will be started automatically when someone connects to the socket. For example, Cloud Hypervisor will start and be given a socket path managed by a systemd socket unit. When the Cloud Hypervisor process connects to the socket, systemd will start the usbvfiod service unit.

When the Cloud Hypervisor recreates the VM due to a reboot, the socket connection will be broken.
Upon breaking the vfio-user connection usbvfiod will exit and clean up it's remaining sockets.
Cloud Hypervisor's VM will fail to restart due to the missing socket. Because Cloud Hypervisor is restarted on failure systemd will only then look at its dependencies and trigger a start for usbvfiod. After usbvfiod is restarted and has recreated the socket, the Cloud Hypervisor service can leave the restart on failure loop.
If usbvfiod exited with a non zero exit code it will be blocked by preexisting files (not previously cleaned up sockets) that will not be overwritten. Because Cloud Hypervisor in the `--socket-path` variant depends on usbvfiod, it might be blocked infinitely due to that missing cleanup.

Using systemd to manage the socket and providing it over the `--fd` argument decouples `usbvfiod.service` from its socket. It is not a direct dependency for the `cloud-hypervisor.service` anymore, the socket unit is. With usbvfiod's behavior to exit if the single vfio-user client disconnects it is now the only service to restart on a guest reboot.

When depending on the socket unit, the file system path for the socketis guaranteed to exists when the socket unit succeeds. A dependence on the service itself does not give any guarantee the socket has been created before dependent services start. This may lead to spurious service startup failures.

## Following the integration test example

This section aims to provide enough information about the usage of socket units with usbvfiod and Cloud Hypervisor to avoid searching for the relevant bits of information in the systemd manual.

The following systemd config snippets are created with a nix integration test and should help you get up and running. They include some noise in the `Service.Environment` fields but are kept as is to avoid untested examples. Values for `Socket.Group`, `Socket.Mode`, `Socket.User`, `Service.Group`, `Service.User` are not strict for the sake of simplicity in the integration test.

The example will manage both of usbvfiods available socket options with systemd: the vfio-user server and the hotplug server.

### Systemd Unit: usbvfiod.socket

The Socket Unit's most important field is `Socket.ListenStream`. The order (if multiple fields exist as do in this example) will be used in the environment of the service when providing the file descriptors. The fields `Socket.SocketGroup`, `Socket.SocketMode` and `Socket.SocketUser` are for permission management of the resulting Sockets.

Use the same name for this socket unit and the associated service to let systemd associate them automatically (equal to explicitly declaring `Socket.Service=usbvfiod.service`). Alternatively set `Socket.Service` explicitly.

```ini
[Unit]
Description=sockets to trigger usbvfiod service start and provide communication channels

[Socket]
Accept=no
ListenStream=/tmp/usbvfiod
ListenStream=/tmp/hotplug
SocketGroup=usbaccess
SocketMode=660
SocketUser=usbaccess

[Install]
WantedBy=sockets.target
```

### Systemd Unit: usbvfiod.service

Following after the three default file descriptors `stdin`, `stdout` and `stderr` systemd provides the above defined order of `Socket.ListenStream` file descriptors. They are available in the `usbvfiod.service` configuration field `Service.ExecStart`, to be used as `--fd 3` (the vfio-user socket) and `--hotplug-fd 4` (the hotplug socket).

```ini
[Unit]

[Service]
Environment="LOCALE_ARCHIVE=/nix/store/54jg0kk0cn5h7j19r0l8x36fmbfywwjj-glibc-locales-2.42-67/lib/locale/locale-archive"
Environment="PATH=/nix/store/di26b1kkbammy0sj70nq5qzvfrh78wxl-coreutils-9.11/bin:/nix/store/fcqbwp5hx5wh3lzf0457wmf0divknz7y-findutils-4.11.0/bin:/nix/store/aak8d9mrdv9sgn0lcg7xss7wxdg9sqh3-gnugrep-3.12/bin:/nix/store/7mfsmbhsd3arnypipwm251rcd0b45riy-gnused-4.10/bin:/nix/store/r6sz8p6sd6c73fp9z8nzl04dri7lyx8n-systemd-261.1/bin:/nix/store/di26b1kkbammy0sj70nq5qzvfrh78wxl-coreutils-9.11/sbin:/nix/store/fcqbwp5hx5wh3lzf0457wmf0divknz7y-findutils-4.11.0/sbin:/nix/store/aak8d9mrdv9sgn0lcg7xss7wxdg9sqh3-gnugrep-3.12/sbin:/nix/store/7mfsmbhsd3arnypipwm251rcd0b45riy-gnused-4.10/sbin:/nix/store/r6sz8p6sd6c73fp9z8nzl04dri7lyx8n-systemd-261.1/sbin"
Environment="RUST_BACKTRACE=full"
Environment="TZDIR=/nix/store/2nndxyf3phkb5aggxm3c16sapa0f49kz-tzdata-2026c/share/zoneinfo"
ExecStart=/nix/store/ipqd1zhfdai5m9gzw40syjk4dbamnnhp-usbvfiod-0.2.0/bin/usbvfiod -v --fd 3 --hotplug-fd 4 --device "/dev/bus/usb/testdevice"

Group=usbaccess
Restart=on-failure
RestartSec=2s
User=usbaccess
```

### Systemd Unit: cloud-hypervisor.service

The `cloud-hypervisor.service` declares a dependency on `usbvfiod.socket` (via `Unit.Requires`) to ensure the path in the filesystem is created before starting Cloud Hypervisor. A dependency directly on `usbvfiod.service` is not necessary.

When starting Cloud Hypervisor, the vfio-user socket path is provided in the same way as it is without a systemd socket unit (see the `Service.ExecStart` field below). In this example the `--user-device` receives the first of the two defined `Socket.ListenStream` paths, because the `usbvfiod.service` configuration above used file descriptor 3 for the vfio-user server.

When the `Install.WantedBy` starts Cloud Hypervisor, it connects to the socket path provided with the `--user-device` argument. Establishing a connection to a socket unit defined path will prompt systemd to start the service unit behind the socket and pass on all communication.

```ini
[Unit]
Requires=usbvfiod.socket

[Service]
Environment="LOCALE_ARCHIVE=/nix/store/54jg0kk0cn5h7j19r0l8x36fmbfywwjj-glibc-locales-2.42-67/lib/locale/locale-archive"
Environment="PATH=/nix/store/di26b1kkbammy0sj70nq5qzvfrh78wxl-coreutils-9.11/bin:/nix/store/fcqbwp5hx5wh3lzf0457wmf0divknz7y-findutils-4.11.0/bin:/nix/store/aak8d9mrdv9sgn0lcg7xss7wxdg9sqh3-gnugrep-3.12/bin:/nix/store/7mfsmbhsd3arnypipwm251rcd0b45riy-gnused-4.10/bin:/nix/store/r6sz8p6sd6c73fp9z8nzl04dri7lyx8n-systemd-261.1/bin:/nix/store/di26b1kkbammy0sj70nq5qzvfrh78wxl-coreutils-9.11/sbin:/nix/store/fcqbwp5hx5wh3lzf0457wmf0divknz7y-findutils-4.11.0/sbin:/nix/store/aak8d9mrdv9sgn0lcg7xss7wxdg9sqh3-gnugrep-3.12/sbin:/nix/store/7mfsmbhsd3arnypipwm251rcd0b45riy-gnused-4.10/sbin:/nix/store/r6sz8p6sd6c73fp9z8nzl04dri7lyx8n-systemd-261.1/sbin"
Environment="TZDIR=/nix/store/2nndxyf3phkb5aggxm3c16sapa0f49kz-tzdata-2026c/share/zoneinfo"
ExecStart=/nix/store/kxnddbaypz5sic0dgqx60lp4k9z662yg-cloud-hypervisor-53.0/bin/cloud-hypervisor --memory size=2G,shared=on --console file=/tmp/console.log --serial off \
  --kernel /nix/store/nmyhzfvkn5zagl7lc16w9m6d54v4696d-linux-6.18.41/bzImage \
  --cmdline 'init=/nix/store/wgjjv8axl1rlfk277h0p4al3x1jn1w95-nixos-system-nixos-kexec-26.11.20260803.104240a/init usbcore.autosuspend=-1 console=hvc0 console=tty0 xhci_pci.dyndbg==pmfl xhci_hcd.dyndbg==pmfl nohibernate root=fstab loglevel=8 lsm=landlock,yama,bpf' \
  --initramfs /nix/store/swc8c4c3kv1w21mlzcrw7kfpj64pq515-initrd/initrd \
  --user-device socket=/tmp/usbvfiod \
  --net "tap=tap0,mac=,ip=192.168.100.1,mask=255.255.255.0"

Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
```

### Using hotplug events to start usbvfiod

In these example configurations the hotplug socket is also managed through systemd. Udev rules trigger commands for managing device attachment state using the hotplug socket. Doing this triggers systemd to start the `usbvfiod.service` independent of Cloud Hypervisor service status.

In this scenario either one can start the `usbvfiod.service`: a hotplug event or a starting Cloud Hypervisor.


Further reading:
- https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html
- https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html
- https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html
- https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html
- https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html
