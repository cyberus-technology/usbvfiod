# Quick Start

The specification reference is available at [https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

## Testing with Cloud Hypervisor

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
kernel image (`bzImage`), an initrd or initramfs and the corresponding
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

## Attaching USB Devices
Currently USB devices can be attached when (1) `usbvfiod`
is started and (2) at runtime by sending a specific command to the hotplug socket.

In both cases pass-through devices are identified by the path to the USB device node. These
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

To attach the specific USB device you would:
- (1) add `--device /dev/bus/usb/002/003` as a parameter to `usbvfiod` for attachment at startup, and/or
- (2) add `--hotplug-socket-path /tmp/hotplug.sock` as a parameter to `usbvfiod` and later run the provided `remote` binary as `remote --socket /tmp/hotplug.sock --attach /dev/bus/usb/002/003`.

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
