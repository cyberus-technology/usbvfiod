# USB Policy Daemon

## Overview

The **USB Policy Daemon** (`usbpolicyd`) acts as a mediation layer between the Linux USB subsystem and the virtual USB controller implementations (**usbvfiod**). It monitors for USB device attachment/detachment events, evaluates a rule-set, and manages the connection to the virtual controllers of the devices based on those rules.

### Key Components

* **`nusb`**: The abstraction library used to enumerate, watch, open, claim, and reset USB devices.
* **`usbvfiod`**: The virtual controller daemon. It listens for commands via UNIX sockets and uses `SCM_RIGHTS` to receive open file descriptors for USB devices, which it then wraps into `nusb` handles.
* **Systemd**: Manages the daemon lifecycle and triggers configuration reloads.
* **Rules**: The rule set is a plain JSON configuration file in the filesystem.

### Core Concepts

* **Global State:** The daemon maintains a list of "known devices". This includes devices successfully handed off to `usbvfiod` and devices that were attached but subsequently detached by the virtual controller (or rejected by rules).
* **Socket Identification:** `usbvfiod` instances are identified uniquely by their UNIX socket path. The daemon must keep track of which socket a device was last handed off to.
* **Idempotency & Interference:** The daemon must be careful not to interfere with devices that `usbvfiod` is already using. When checking the state of a known device during a reload, the daemon queries the `usbvfiod` socket to see if the device is still listed as attached. If it is, the daemon leaves the device alone.
* **Device Reset & Handoff:** The handoff process involves a specific sequence: claim the device, reset it, close the handle, re-open the handle, then pass the FD to `usbvfiod`.

## Lifecycle & State Management

### Device Lifecycle

The lifecycle is event-driven and linear.

1.  **Attach Event (Kernel)**
    *   `usbpolicyd` receives an event via `nusb` (e.g., via `udev` or `libudev` integration).
    *   The daemon opens the device using `nusb`.
1.  **Policy Evaluation**
    *   Evaluate the new device against the current set of rules.
    *   If a match is found:
        *   **Action**: Prepare to hand off the device to `usbvfiod`.
1.  **Device Reset**
    *   The daemon claims the device using `nusb`.
    *   The daemon performs a USB **Reset** on the device.
1.  **Re-Open & Handoff**
    *   The daemon re-opens the *same* device node (e.g., `/dev/bus/usb/001/005`).
    *   The daemon connects to the UNIX socket specified in the rule.
    *   The daemon uses the `SCM_RIGHTS` mechanism to send the raw file descriptor over the socket to `usbvfiod`.
    *   Once the socket transmission is complete, the daemon closes its copy of the file descriptor.
1.  **Detach Event (Kernel)**
    *   If the daemon detects a kernel detach event for a previously handed-off device, it issues a detach event through the previously recorded socket.

### Rule Reload Lifecycle (SIGHUP)

When `systemctl reload` is executed, systemd sends a `SIGHUP` signal to the daemon. Other system components may make use of the signal mechanism as well after rewriting the configuration or changing system environment.

1.  **Re-evaluation**: The daemon reads the configuration file.
1.  **Reconciliation Loop**:
    *   The daemon iterates through `known_devices`.
    *   For each device, it checks if the current configuration contains a matching rule.
    *   Case A: No Rule Match: **Detach Orphaned**
        *   The daemon queries the `usbvfiod` socket associated with the **device** (if any) to check if the device is currently listed as attached.
            * This can be skipped and an opportunistic detach can be performed for the device instead.
        *   If the device IS attached to a virtual controller:
            * The daemon sends a Detach command to the socket (to force release).
            * The associated socket is cleared for the device
    *   Case B: Rule Match Found:
        *   The daemon checks the socket associated with the **rule** to check if the device is already attached.
            * If the socket in the rule and the currently registered socket for the device differ, the daemon sends a Detach command to the old socket first.
        *   If the device is NOT attached to the virtual controller:
            * The daemon performs the "Reset -> Re-open -> Handoff" sequence to transfer ownership.
            * The associated socket is recorded for the device
        *   If the device IS attached to the virtual controller:
            * No action is performed. The old socket association stands.

### VM and Virtual Controller Lifecycle

The virtual controllers referred to by rules may not exist when a matching device is detected. VM lifecycle orchestration is responsible for issuing a rule reload and re-evaluation after bringing up a new VM with virtual controller, to ensure the matching devices are handed over. Stopping a virtual controller does not mandate any interaction with the policy daemon. It will gracefully handle any mismatch detected on later device events or rule re-evaluation.

## Configuration File

The configuration is defined as a JSON object. Each rule must provide a `socket_path` key describing the target of hotplug operations for devices matching this rule. All other keys are used to identify and match a device and are up for discussion. In the first iteration, only vendor and product IDs are supported for device matching. IDs are strings of hex digits, either with or without the `0x` prefix. The format is a plain array of rule objects.

```json
[
  {
    "vendor_id": "0x1234",
    "product_id": "0x5678",
    "socket_path": "/run/usbvfiod-vm1/hotplug.sock"
  },
  {
    "vendor_id": "43ab",
    "product_id": "cd65",
    "socket_path": "/run/usbvfiod-vm2/hotplug.sock"
  }
]
```

### Evaluation of Conflicting Rules

Rules are evaluated in the order they appear in the configuration. The first rule matching a device is applied and further rules are ignored.

## Systemd Integration

### Service Definition

The daemon should be configured to react to `SIGHUP` for configuration reloads.

```.ini
[Unit]
Description=USB Policy Daemon

[Service]
Type=simple
User=usbaccess
ExecStart=/usr/bin/usbpolicyd
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
```

### Permissions

*   **Device Access**: The daemon must have permissions to open USB device nodes.
*   **Socket Access**: The daemon must have permissions to connect to the sockets defined in the rules (e.g., `chmod 660 /run/usbvfiod/hotplug.sock` or run the daemon as a user in the group owning the socket).


## Error Handling

All errors in the policy daemon in regard to device or socket access are non-fatal and will be handled gracefully.

* **Socket Not Found**: If the socket path in the rule does not exist when the daemon attempts to connect, the attach operation fails. The device is not handed off, and the daemon logs an error.
* **Reset Failure**: If the USB reset fails, the daemon logs the error and aborts the handoff for that device.
* **Permission Denied**: If `nusb` fails to claim the device or open it for the second time, the daemon logs the error and abandons the device.
