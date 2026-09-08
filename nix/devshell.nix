{
  craneLib,
  pkgs,
  shellHook,
}:
let
  vfio-user-socket = "/tmp/usbvfiod-vfio-user.sock";
  hotplug-socket = "/tmp/usbvfiod-hotplug.sock";

  sshhost = pkgs.writeShellScriptBin "sshhost" ''
    ssh -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
  '';

  sshguest = pkgs.writeShellScriptBin "sshguest" ''
    ssh -p 2000 root@localhost -o ProxyCommand="ssh -W %h:%p -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no" -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
  '';

  runusbvfiod = pkgs.writeShellScriptBin "runusbvfiod" ''
    cargo build --bin usbvfiod
    rm ${vfio-user-socket}
    rm ${hotplug-socket}
    ./target/debug/usbvfiod --socket-path ${vfio-user-socket} --hotplug-socket-path ${hotplug-socket} --no-color
  '';

  runchv = pkgs.writeShellScriptBin "runchv" ''
    nix-build -A netboot.x86_64-linux '<nixpkgs/nixos/release.nix>'
    NETBOOT=$(grep -o "init=[^$]*" result/netboot.ipxe)
    echo "$NETBOOT"
    cloud-hypervisor --memory size=4G,shared=on --serial tty --user-device socket=${vfio-user-socket} --console off --kernel result/bzImage --initramfs result/initrd --cmdline "$NETBOOT console=ttyS0"
  '';

  attach = pkgs.writeShellScriptBin "attach" ''
    cargo build --bin remote
    ./target/debug/remote --socket ${hotplug-socket} --attach /dev/bus/usb/$1/$2
  '';
in
{
  default = craneLib.devShell {
    inherit shellHook;
    packages = [
      sshhost
      sshguest
      runusbvfiod
      runchv
      attach
    ];
  };
}
