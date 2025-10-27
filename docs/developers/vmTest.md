# Overview
We provide an integration test that runs in CI and in an interactive local session.

> [!NOTE]
> An overview about nix vm tests is helpful and can be found here: https://nix.dev/tutorials/nixos/integration-testing-using-virtual-machines.html

The integration test includes a static port forward of SSH to the developers machine port 2000. This provides a `root` login to the virtual machine with empty password.
```mermaid
graph LR;
  id1[Developer Machine]
  id2[QEMU Host]
  id3[Cloud Hypervisor Guest]
  id1--ssh root@localhost:2000--->id2
  id2--ssh root@192.168.100.2--->id3
```


## Quick Start
To build and run the interactive vm test (build logs flag is optional):
```
nix run .\#checks.x86_64-linux.integration-smoke.driverInteractive --print-build-logs
```

This will start a python environment where you can run the `test_script()` (or other functions like `start_all()`).

The QEMU vm will be accessible with the alias `sshhost` (available in the Nix devshell) or:
```
ssh -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
```

Using the QEMU vm as a proxy the nested cloud-hypervisor vm will be accessible with the alias `sshguest`:
```
ssh -o ProxyCommand="ssh -W %h:%p -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no" -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.100.2
```