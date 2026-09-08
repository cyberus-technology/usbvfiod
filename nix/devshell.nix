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
    case "$BUILD" in
      debug)   CARGO_FLAGS="" ;;
      release) CARGO_FLAGS="--release" ;;
      *) echo "unknown BUILD: $BUILD" >&2; exit 1 ;;
    esac

    cargo build --bin usbvfiod $CARGO_FLAGS

    rm ${vfio-user-socket}
    rm ${hotplug-socket}
    ./target/$BUILD/usbvfiod --socket-path ${vfio-user-socket} --hotplug-socket-path ${hotplug-socket} -vv
  '';

  runchv =
    let
      symlink = "result-netboot";
    in
    pkgs.writeShellScriptBin "runchv" ''
      nix-build -A netboot.x86_64-linux '<nixpkgs/nixos/release.nix>' --out-link ${symlink}
      NETBOOT=$(grep -o "init=[^$]*" ${symlink}/netboot.ipxe)
      echo "$NETBOOT"
      ${pkgs.cloud-hypervisor}/bin/cloud-hypervisor --memory size=4G,shared=on --serial tty --user-device socket=${vfio-user-socket} --console off --kernel ${symlink}/bzImage --initramfs ${symlink}/initrd --cmdline "$NETBOOT console=ttyS0"
    '';

  attach = pkgs.writeShellScriptBin "attach" ''
    if [ "$#" -ne 2 ]; then
      echo "Error: expected 2 arguments: /dev/bus/usb/\$1/\$2"
      exit 1
    fi

    cargo build --bin remote

    ./target/debug/remote --socket ${hotplug-socket} --attach /dev/bus/usb/"$1"/"$2"
  '';

  infoMessage = pkgs.writeShellScriptBin "infoMessage" ''
    echo "INFO: Set \$BUILD to 'release' for release binaries (default: 'debug')."
    echo "INFO: This devShell provides a few simple QoL scripts."
    echo "  sshhost       connect to a qemu guest of a nix integration test"
    echo "  sshguest      connect to a nested cloud hypervisor guest of a nix integration test"
    echo "  runusbvfiod   usbvfiod wrapper with hotplug enabled; foreground job using stdio for logs"
    echo "  runchv        cloud hypervisor wrapper expecting a usbvfiod socket; foreground job and uses stdio for interaction & logs"
    echo "  attach        remote wrapper to attach a usb device to the running usbvfiod; uses stdio for logs"
  '';

  commonPackages = [
    pkgs.cloud-hypervisor
    sshhost
    sshguest
    runusbvfiod
    runchv
    attach
  ];
in
{
  default = craneLib.devShell {
    shellHook = ''
      ${shellHook}
      ${infoMessage}/bin/infoMessage
      echo "INFO: Another devShell \`profiling\` is also available."
    '';

    packages = commonPackages;

    BUILD = "debug";
  };
}
