# Architecture Overview

This document gives a brief overview of the problem we are trying to
solve and the high-level architecture. The audience of this document
are developers and integrators.

## Goals

[Cloud Hypervisor](https://www.cloudhypervisor.org/) is a practical
virtual machine monitor (VMM) for embedded scenarios. It is secure,
stable and generally easy to work with. USB support is important for
embedded use cases, yet Cloud Hypervisor has no native support for
USB.

The goal of this project is to add USB support, specifically
pass-through of physical USB devices, to Cloud Hypervisor. We want to
do this while keeping changes to Cloud Hypervisor itself to a minimum.

While we target arbitrary USB devices in the long run, our initial
focus is on USB storage devices. Once these are well supported, we
will broaden our scope.

## Component Overview

The following components are initially involved for USB pass-through:

```mermaid
graph LR
    A[cloud-hypervisor]
    B[usbvfiod]
    A -- vfio-user --- B
    C[Linux kernel]
    B -- USB API --- C
```

`usbvfiod` connects to Cloud Hypervisor via the [`vfio-user`
protocol](https://www.qemu.org/docs/master/interop/vfio-user.html). It
emulates a [USB host
controller](https://en.wikipedia.org/wiki/Extensible_Host_Controller_Interface)
for the VM running inside of Cloud Hypervisor.

This setup is somewhat similar to
[`virtiofsd`](https://virtio-fs.gitlab.io/). `virtiofsd` provides a
`virtio-fs` device using
[`vhost-user`](https://qemu-project.gitlab.io/qemu/interop/vhost-user.html)
instead of `vfio-user`, but otherwise has a very similar setup.

`usbvfiod` accesses USB devices using the Linux kernel's [userspace USB
API](https://www.kernel.org/doc/html/latest/driver-api/usb/usb.html#the-usb-character-device-nodes). As
such, `usbvfiod` runs as a normal Linux process.

Each `usbvfiod` process emulates a _single_ USB host controller. This
means that you need at least one `usbvfiod` process for each VM with
USB support. While it is technically possible to serve multiple VMs
from the same `usbvfiod` process, we decided against this approach,
because it creates a large single point of failure and security
liability.

`usbvfiod` adheres to the `vfio-user` [Backend Program
Conventions](https://www.qemu.org/docs/master/interop/vfio-user.html#backend-program-conventions).

### Sandboxing

The above design requires that `usbvfiod` has access to
`/dev/bus/usb/*` devices to talk to USB devices. This conveys overly
broad access to all USB devices in the system. While this is fine
initially, eventually we want to increase the sandboxability of
`usbvfiod`.

For this, we want to introduce another component (`usbpolicyd`) that
has access to `/dev/bus/usb`. This component will open USB device
nodes to pass-through according to a policy and hand already opened
file descriptors to the right `usbvfiod` instance.

This setup allows separating policy (`usbpolicyd`) from mechanism
(`usbvfiod`). It also allows `usbvfiod` to run in a sandbox without
access to `/dev` at all, which increases the security of the system.

We will further flesh out this idea once the basic flow of USB
pass-through works.

## View from the VM

From the virtual machine's (VM) view, each attached `usbvfiod`
instance creates one [XHCI
controller](https://en.wikipedia.org/wiki/Extensible_Host_Controller_Interface)
on its virtual PCI bus. Passed-through USB devices will appear as USB
devices of this virtual XHCI controller.

For simplicity, we initially aim for a flat USB hierarchy without hubs
(besides the root hub). This limits the design to less than 127
devices. Our typical setup will only involve a handful of devices and
setups with more devices may initially not be optimized.
