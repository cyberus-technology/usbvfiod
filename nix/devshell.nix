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

  # Prepare cached binaries for other scripts and give some very likely needed output.
  preparegrind = pkgs.writeShellScriptBin "preparegrind" ''
    cargo build --bin usbvfiod
    cargo build --bin remote
    nix-build -A netboot.x86_64-linux '<nixpkgs/nixos/release.nix>'

    echo ""
    echo "FIND YOUR BUS/DEVICE NUMBERS"
    lsusb
    echo ""
    echo "LIST OF COMMANDS TO USE IN THE GUEST ON THE BLOCKDEVICE:"
    echo "sudo su"
    echo "dd if=/dev/urandom of=/dev/sda count=100 bs=4M status=progress oflag=direct && poweroff"
    echo "dd if=/dev/sda of=/dev/null count=100 bs=4M status=progress oflag=direct && poweroff"
  '';

  # Alternative to `runusbvfiod` that will generate cachegrind files.
  cachegrind = pkgs.writeShellScriptBin "cachegrind" ''
    cargo build --bin usbvfiod
    rm -rf ${vfio-user-socket} ${hotplug-socket}
    ${pkgs.valgrind}/bin/valgrind --tool=cachegrind \
      --cachegrind-out-file=cachegrind.out.%p \
      --trace-children=yes \
      ./target/debug/usbvfiod --socket-path ${vfio-user-socket} --hotplug-socket-path ${hotplug-socket} --no-color
  '';

  # Alternative to `runusbvfiod` that will generate callgrind files.
  callgrind = pkgs.writeShellScriptBin "callgrind" ''
    cargo build --bin usbvfiod
    rm -rf ${vfio-user-socket} ${hotplug-socket}
    ${pkgs.valgrind}/bin/valgrind --tool=callgrind \
      --callgrind-out-file=callgrind.out.%p \
      --compress-strings=no \
      --separate-threads=yes \
      --dump-line=yes \
      --collect-systime=yes \
      --collect-bus=yes \
      ./target/debug/usbvfiod --socket-path ${vfio-user-socket} --hotplug-socket-path ${hotplug-socket} --no-color
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

  profiling = craneLib.devShell {
    # used as guidance: https://nnethercote.github.io/perf-book/profiling.html
    shellHook = ''
      ${shellHook}
      echo "INFO: This devShell is meant to be used for profiling."
      echo "-----"
      echo "INFO: It is recommended to apply the following to .cargo/config.toml:"
      cat << EOF
      [build]
      rustflags = [
        "-C",
        "force-frame-pointers=yes",
        "-C",
        "symbol-mangling-version=v0",
      ]
      EOF
      echo "-----"
    '';
    packages = [
      sshhost
      sshguest
      runusbvfiod
      runchv
      attach

      pkgs.valgrind
      pkgs.kdePackages.kcachegrind
      preparegrind
      cachegrind
      callgrind
    ];
  };
}
