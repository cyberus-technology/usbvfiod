# Usage

The package currently offers two binaries:
* `usbvfiod`: the vfio-user server emulating an xHCI controller
* `remote`: a utility to control USB host device attachment

The server communicates via two Unix sockets:
* a `vfio-user` socket for communication between `usbvfiod` and the VMM (Cloud Hypervisor)
* a `hotplug` socket to attach/detach/list devices exposed by `usbvfiod` from the host (optional)

> [!TODO]
> Currently, the server creates both sockets. The `vfio-user` standard
> requires that a server can also accept the `vfio-user` socket as a file
> descriptor, but we have not yet implemented this functionality.

## Obtaining the Package

The software is available through the [GitHub
website](https://github.com/cyberus-technology/usbvfiod) as a `nix
flake`, or can be built from source as `cargo` project. We also
release versions to [crates.io](https://crates.io/crates/usbvfiod)
and maintain [a `NixOS`
package](https://search.nixos.org/packages?channel=unstable&query=usbvfiod).

> [!NOTE]
> There are currently no other officially maintained distribution packages
> available.

## Invocation

You can run the server through `nix` directly from GitHub, or install the
binaries in your system through `cargo install usbvfiod`.

```console
nix run github:cyberus-technology/usbvfiod#usbvfiod -- \
  --socket-path /path/to/usbvfiod.sock                 \
  --hotplug-socket-path /path/to/usb-hotplug.sock
2026-04-10T07:00:16.353894Z  INFO usbvfiod: We're up!
```

> [!NOTE]
> Instead of (or additionally to) providing a `hotplug` socket, you can
> specify devices to expose on the controller directly with
> `--device /dev/bus/usb/<BUS>/<DEV>`

Connect the virtual xHCI controller to a Cloud Hypervisor instance through the
`vfio-user` socket, by adding `--user-device socket=/path/to/usbvfiod.sock` to
the Cloud Hypervisor command line invocation. The socket must exist (`usbvfiod`
must be running) before Cloud Hypervisor can attach to it.

Use the `remote` binary to list, attach, and detach devices through the
`hotplug` socket.

```console
nix run github:cyberus-technology/usbvfiod#remote -- \
  --socket /path/to/usb-hotplug.sock                 \
  --list
No attached devices
```

Attach a USB device to the controller through the `hotplug` socket. The 
example uses a keyboard recognized at `/dev/bus/usb/009/003`.

```
nix run github:cyberus-technology/usbvfiod#remote -- \
  --socket /tmp/usb-hotplug.sock                     \
  --attach /dev/bus/usb/009/003
Requesting attachment of device 009:003
2026-04-10T07:01:16.353894Z  INFO usbvfiod::device::xhci::port: Attached Full Speed (12 Mbps) device (9, 3) to port 5 (USB2 port)
    SuccessfulOperation
```

Detach the device using the recorded device ID also shown on `--list`.

```console
nix run github:cyberus-technology/usbvfiod#remote -- \
  --socket /tmp/usb-hotplug.sock                     \
  --detach 9 3
Requesting detach of device 009:003
2026-04-10T07:02:17.889529Z  INFO usbvfiod::device::xhci::port: Detached device (9, 3) from port 5
SuccessfulOperation
```

## Device Access Restrictions

The `usbvfiod` server and `remote` binary do not require elevated privileges.
Only access to the USB character device nodes is required for the component
opening the device. You can manage the permissions for specific devices through
`udev` rules (`TAG+="uaccess"`), or you can invoke the `remote` binary with
elevated privileges (e.g., `sudo`). The `hotplug` socket must be accessible to
the `remote` binary.
