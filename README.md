# usbvfiod

**usbvfiod** is a Rust-based tool designed to enable USB device
passthrough to [Cloud
Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor)
virtual machines using the [vfio-user
protocol](https://github.com/tmakatos/qemu/blob/master/docs/devel/vfio-user.rst). Other
VMMs might also work, but but are currently not the main target.

This project is still under active development and **not usable
yet**. We are planning to work on this project in the following order:

1. **Validating our Assumptions** (🚧 **Ongoing** 🚧)
   - We are looking for suitable libraries to use and finalize our design.
2. **Towards USB Storage Passthrough**
   - We build up a virtual XHCI controller and the necessary plumbing
     to pass-through USB devices from the host.
   - Our initial test target will be USB storage devices.
3. **Broaden Device Support**
   - We broaden the set of USB devices we support and actively test.

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
$ cargo run -- --socket-path /tmp/usbvfiod-socket -vv
2025-04-25T09:41:40.891734Z  INFO usbvfiod: We're up!
```

In another terminal, start Cloud Hypervisor. Any recent version will
do:

```console
$ cloud-hypervisor --memory size=4G,shared=on --serial tty --user-device socket=/tmp/ubvfvfiod-socket --console off \
    -kernel KERNEL -initramfs INITRAMFS -cmdline CMDLINE
```

To get a kernel and initramfs to play with, you can use the NixOS netboot binaries:

```console
# Enter a nixpkgs checkout.
$ nix-build ./nixos/release.nix -A netboot.x86_64-linux
$ ls -l result/
total 0
lrwxrwxrwx  6 root root 64 Jan  1  1970 bzImage -> /nix/store/6ma0apc1gyk5bprqyjfzzpibqqdnwi9k-linux-6.6.68/bzImage
lrwxrwxrwx  2 root root 57 Jan  1  1970 initrd -> /nix/store/qwywr5l8awbxh0g431mxdaah7mzh64rq-initrd/initrd
lrwxrwxrwx  2 root root 69 Jan  1  1970 netboot.ipxe -> /nix/store/2ii3vw4ab0wyr56c45hmbafndixh5x6q-netboot.ipxe/netboot.ipxe
...
```

You will find a kernel (`bzImage`) and initrd, you can use for Cloud
Hypervisor. The required command line for booting is in
`result/netboot.ipxe`. You want to add a `console=ttyS0` to get
console output.

### Format Checks

`.toml` files in the repository are formatted using
[taplo](https://taplo.tamasfe.dev/). To re-format `.toml` files, you
can use:

```console
$ taplo format file.toml
```
