# This creates a cloud-hypervisor GNU command line string that can be passed to cloud-hypervisor
# to start a guest VM. Used by the guest-vm module.
# It is close to a "compatibility layer" to a particle config but only for virtio_{blk,net,fs}.
{ lib, ... }:
{
  name,
  kernel,
  diskPath,
  vcpus,
  memory,
}:
let
  mkOptionName = k: if builtins.stringLength k == 1 then "-${k}" else "--${k}";
in

lib.cli.toGNUCommandLineShell { mkList = k: v: [ (mkOptionName k) ] ++ v; } ({
  inherit kernel;
  disk = "path=${diskPath}";
  cpus = "boot=${toString vcpus},kvm_hyperv=on";
  memory = "size=${memory}";
  api-socket = "/run/ch-${name}.sock";
  serial = "tty";
  console = "off";
  net = "tap=tap0,mac=,ip=192.168.249.1,mask=255.255.255.0";
})
