# Security Considerations

USB device passthrough presents some security challenges, as malicious devices
could potentially compromise system integrity and enable privilege escalation.
This project was specifically designed with security as a core principle,
allowing multiple layers of defense to limit the impact of malicious actors.
The design philosophy prioritizes isolation and privilege separation mechanisms
to ensure that compromised guest systems cannot easily escape to the host.

## Threat Model

Attackers:
* Malicious USB devices
* Malicious Guest VM drivers

Trusted parties:
* Host processes with socket access
* Host Kernel USB API

We specifically trust the VMM and utilities calling the `hotplug` socket.

Risks:
* Leaks from the process address space
* VM Escapes to the host user space and subsequent privilege escalation

## Design Mitigations

The vfio-user server runs as a separate user-space process. Its operation is
tightly coupled to the VMM it serves, but it is isolated from the VMM process
address space and privileges. One process only ever serves one VMM.

The software is written in a memory-safe language, to limit the possibilities
of bugs leading to code execution. The nature of protocol boundaries to the
kernel and `vfio-user` client mandates some carefully chosen bindings to uphold
those guarantees.

### Host and Guest Memory Access

The server has full access to the Guest VM's physical backing memory.
Guest-controlled DMA (Direct Memory Access) operations of the virtual
controller are only supported from/to this memory. The Guest VM is generally
allowed arbitrary modifications to its memory, or to "shoot itself in the
foot" through the virtual controller. Access to Guest memory regions outside
the backing memory (i.e., "P2P DMA") is deliberately not supported through the
`vfio-user` server.

All Host-DMA is gated through the Kernel user-space USB API. No direct control
over the host USB controller is granted to the server.

### Privilege Separation

The server accepts file descriptors to opened devices through the `hotplug`
socket. This ensures the server itself does not require the privileges to open
device nodes, and can run with very low privileges over the whole process
lifetime.

The provided `remote` utility incurs short-lived processes with the required
permissions to open a device node and the `hotplug` socket. The Linux Kernel
user-space USB API is based on file permissions and does not mandate additional
capabilities.

The VMM is a separate process with its own security boundary. It must be
granted access to the `vfio-user` socket.

The sockets used for communication must be sufficiently protected from
malicious or accidental interference. Malformed access may lead to denial of
service of the virtual controller.

## Hardening Options

The design allows for various orthogonal hardening options.

### Service Sandboxing

The vfio-user server can be run with strict sandboxing to further limit its capabilities:
* **Process isolation**: Ensure the server runs as a non-root user with minimal group memberships.
* **User namespace isolation**: Run the server in a dedicated user namespace where it has minimal privileges.
* **System call filtering**: The `vfio-user` server allowed syscalls can be further restricted, e.g., through [systemd](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#System%20Call%20Filtering).
* **Memory restrictions**: Limit the server's memory footprint using OS-level `cgroups` or similar mechanisms.

### Limiting Host USB Kernel Drivers

The core USB subsystem drivers must be available in the host, in order to
support user-space processes as the `vfio-user` server to claim and work
with the device. This is a tradeoff with full controller pass-through, which
can omit the whole USB support. No specific USB subsystem drivers (storage,
network, etc.) are required, though, and can be omitted in the host. This
avoids the risk of bugs in kernel drivers attaching to malicious devices.
The Guest VM is responsible for the device support, and must ensure its own
hardening against such devices.

### Authorizing USB Devices

Unknown USB devices should be ignored by the host until explicitly authorized.
The Linux kernel can be configured to reject all USB devices by default through
a command line parameter:

```
usbcore.authorized_default=0
```

This kernel parameter prevents any USB device from being automatically
attached, ensuring that only explicitly authorized devices
can be used. Devices must then be explicitly enabled through
the `usbcore.authorized` sysfs interface. See the [Kernel USB
documentation](https://docs.kernel.org/usb/authorization.html) for details.
