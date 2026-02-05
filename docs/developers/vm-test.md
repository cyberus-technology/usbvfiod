# Overview
We provide a number of NixOS integration tests that run in our CI and can be used in an interactive local session.

> [!NOTE]
> An overview about nix vm tests is helpful and can be found here: https://nix.dev/tutorials/nixos/integration-testing-using-virtual-machines.html

## Interactive testing with ssh access
The integration tests include a static port forward of SSH to the developers machine port 2000. This provides a `root` login to the virtual machine with empty password.
```mermaid
graph LR;
  id1[Developer Machine]
  id2[QEMU Host]
  id3[Cloud Hypervisor Guest]
  id1--ssh root@localhost:2000--->id2
  id2--ssh root@192.168.100.2--->id3
```

### How to
Build and run the interactive VM test driver:
```
nix run .\#checks.x86_64-linux.<name>.driverInteractive
```
This will start a python environment where you can run the `test_script()`. Manually starting the VM with `start_all()` is also possible, but QEMU USB storage hardware emulation will not work as, because the backing file for the emulated blockdevices needs to be created first. This currently happens in the `test_script()`.

The QEMU VM will be accessible with the alias `sshhost` (available in the Nix development shell) or:
```
ssh -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
```
Using the QEMU VM as a proxy the nested cloud-hypervisor vm will be accessible with the alias `sshguest`:
```
ssh -o ProxyCommand="ssh -W %h:%p -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no" -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2
```

## Live journal logging on stdout
To speed up the CI all journal logs are collected and printed at the end of a test. When writing new tests the following section in QEMU's NixOS config should be removed to have slower but live logging:
```
# The framework automatically forwards all journal output to ttyS0,
# slowing down the test significantly if there is a lot of logs.
journald.extraConfig = lib.mkForce ''
  ForwardToConsole=yes
  TTYPath=/dev/hvc1
'';
```
