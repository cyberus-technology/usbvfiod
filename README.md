[![Hercules CI](https://hercules-ci.com/api/v1/site/github/account/cyberus-technology/project/usbvfiod/badge)](https://hercules-ci.com/github/cyberus-technology/usbvfiod/status)

# usbvfiod

**usbvfiod** is a Rust-based tool designed to enable USB device
passthrough to [Cloud
Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor)
virtual machines using the [vfio-user
protocol](https://github.com/tmakatos/qemu/blob/master/docs/devel/vfio-user.rst). Other
VMMs might also work, but but are currently not the main target.

This project is still under active development. We are planning to work on this
project in the following order:

1. **Validating our Assumptions** (Done)
   - We are looking for suitable libraries to use and finalize our design.
1. **Towards USB Storage Passthrough** (Done)
   - We build up a virtual XHCI controller and the necessary plumbing
     to pass-through USB devices from the host.
   - Our initial test target will be USB storage devices.
1. **Broaden Device Support** (Done)
   - We broaden the set of USB devices we support and actively test.
   - Our current focus is enabling USB-2 devices and devices with interrupt
     endpoints.
1. **Hotplug Devices** (🚧 **Ongoing** 🚧)
   - We enable attach and detach of exposed devices during runtime.
1. **Everything Beyond**
   - Many topics remain open. We stay flexible regarding upcoming features.

If you want to use this code, please check back later or [get in
touch](https://cyberus-technology.de/en/contact), if you need
professional support.

## Documentation

Find the overview of documentation [here](./docs/overview.md).

## Development

The following section is meant for developers.

### Testing with Cloud Hypervisor

An easy way to get a testing setup is to connect `usbvfiod` with Cloud
Hypervisor. For this, start `usbvfiod` in one terminal:

```console
$ cargo run -- --socket-path /tmp/usbvfiod.sock -vv
2025-04-25T09:41:40.891734Z  INFO usbvfiod: We're up!
```

In another terminal, start Cloud Hypervisor. Any recent version will
do:

```console
$ cloud-hypervisor \
   --memory size=4G,shared=on \
   --serial tty \
   --user-device socket=/tmp/usbvfiod.sock \
   --console off \
   --kernel KERNEL \
   --initramfs INITRD \
   --cmdline KERNEL_CMDLINE
```

`KERNEL`, `INITRD`, and `KERNEL_CMDLINE` are placeholders for a Linux
kernel image (`bzImage`), a initrd or initramfs and the corresponding
command line.

> [!TIP]
> To get a kernel and initramfs to play with, you can use the [NixOS](https://nixos.org/)
> [netboot](https://nixos.org/manual/nixos/stable/index.html#sec-booting-from-pxe) binaries.
>
> You will find a kernel (`bzImage`) and initrd. The required command
> line for booting is in `result/netboot.ipxe`. You want to add
> `console=ttyS0` to get console output.
>
> ```console
> $ nix-build -A netboot.x86_64-linux '<nixpkgs/nixos/release.nix>'
> $ ls result/
> bzImage initrd netboot.ipxe
> ...
> $ grep -o "init=[^$]*" result/netboot.ipxe
> init=/nix/store/.../init initrd=initrd nohibernate loglevel=4
> ```

### Attaching USB Devices

For the time being, USB devices can only be attached when `usbvfiod`
is started. `usbvfiod` takes the path to the USB device node. These
paths are of the form `/dev/bus/usb/$BUS/$DEVICE`.

To figure out the bus and device numbers of a specific USB device, use
the `lsusb` utility (typically installed via the `usbutils` package):

```console
$ lsusb
Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub
Bus 001 Device 002: ID 8087:0033 Intel Corp. AX211 Bluetooth
Bus 002 Device 001: ID 1d6b:0003 Linux Foundation 3.0 root hub
Bus 002 Device 003: ID 18a5:0243 Verbatim, Ltd Flash Drive (Store'n'Go)
```

So for attaching the flash drive you would add `--device
/dev/bus/usb/002/003` as a parameter to `usbvfiod`. `usbvfiod` must
have permission to read and write the device node.

### Format Checks

`.toml` files in the repository are formatted using
[taplo](https://taplo.tamasfe.dev/). To re-format `.toml` files, you
can use:

```console
$ taplo format file.toml
```

### Temporarily Ignoring Pre-Commit Checks

When committing incomplete or work-in-progress changes, the pre-commit
checks can become annoying. In this case, use:

```console
$ git commit --no-verify
```

### Kernel Debug Messages for XHCI

The XHCI driver in the Linux kernel prints helpful messages with `xhci_dbg`.
If your kernel supports `CONFIG_DYNAMIC_DEBUG` (the NixOS netboot kernel does),
you can enable the messages for XHCI with:

```
$ echo "file drivers/usb/host/xhci* +p" | sudo tee /sys/kernel/debug/dynamic_debug/control
```

Then, you can filter the kernel log for the relevant messages:

```
$ sudo dmesg | grep -E xhci\|usb
```

> [!TIP]
> You can also enable the dynamic debug messages on boot by adding
> `xhci_hcd.dyndbg==pmfl xhci_pci.dyndbg==pmfl` to the command line.
>
> Alternatively, provide `dyndbg==pfml` as option to `modprobe` on
> invocation or through a `modprobe` config.
