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
    if [ "$BUILD" = "debug" ]; then
      cargo build --bin usbvfiod
    else
      cargo build --bin usbvfiod --release
    fi

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

    ./target/debug/remote --socket ${hotplug-socket} --attach /dev/bus/usb/$1/$2
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

  rustcProfilingFlags = ''--config 'build.rustflags = ["-C", "force-frame-pointers=yes", "-C", "symbol-mangling-version=v0"]' '';

  # Prepare cached binaries for other scripts and give some very likely needed output.
  preparegrind = pkgs.writeShellScriptBin "preparegrind" ''
    if [ "$BUILD" = "debug" ]; then
      cargo build --bin usbvfiod ${rustcProfilingFlags}
    else
      cargo build --bin usbvfiod --release ${rustcProfilingFlags}
    fi
    cargo build --bin remote ${rustcProfilingFlags}
    nix-build -A netboot.x86_64-linux '<nixpkgs/nixos/release.nix>' --out-link result-netboot

    echo ""
    echo "INFO: lsusb output to get bus and device numbers:"
    lsusb
    echo ""
    echo "INFO: example commands in the nested guest to produce some load on a blockdevice:"
    echo "dd if=/dev/urandom of=/dev/sda count=100 bs=4M status=progress oflag=direct && poweroff"
    echo "dd if=/dev/sda of=/dev/null count=100 bs=4M status=progress oflag=direct && poweroff"
  '';

  # Alternative to `runusbvfiod` that will generate cachegrind files.
  cachegrind = pkgs.writeShellScriptBin "cachegrind" ''
    if [ "$BUILD" = "debug" ]; then
      cargo build --bin usbvfiod ${rustcProfilingFlags}
    else
      cargo build --bin usbvfiod --release ${rustcProfilingFlags}
    fi
    rm -rf ${vfio-user-socket} ${hotplug-socket}

    ${pkgs.valgrind}/bin/valgrind --tool=cachegrind \
      --cachegrind-out-file=cachegrind.out.%p \
      --trace-children=yes \
      ./target/debug/usbvfiod --socket-path ${vfio-user-socket} --hotplug-socket-path ${hotplug-socket} --no-color
  '';

  # Alternative to `runusbvfiod` that will generate callgrind files.
  callgrind = pkgs.writeShellScriptBin "callgrind" ''
    if [ "$BUILD" = "debug" ]; then
      cargo build --bin usbvfiod ${rustcProfilingFlags}
    else
      cargo build --bin usbvfiod --release ${rustcProfilingFlags}
    fi
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
    shellHook = ''
      ${shellHook}
      ${infoMessage}/bin/infoMessage
      echo "INFO: Another devShell \`profiling\` is also available."
    '';

    packages = commonPackages;

    BUILD = "debug";
  };

  profiling = craneLib.devShell {
    # used as guidance: https://nnethercote.github.io/perf-book/profiling.html
    shellHook = ''
      ${shellHook}
      ${infoMessage}/bin/infoMessage
      echo "INFO: This devShell, meant for profiling, also provides the following."
      echo "  valgrind      profiling toolkit"
      echo "  kcachegrind   GUI for valgrind reports"
      echo "  preparegrind  script to cargo/nix build (cache things) and print some help (i.e. lsusb output, dd command)"
      echo "  cachegrind    usbvfiod wrapper using valgrind with tool=cachegrind"
      echo "  callgrind     usbvfiod wrapper using valgrind with tool=callgrind"
    '';

    packages = commonPackages ++ [
      pkgs.valgrind
      pkgs.kdePackages.kcachegrind
      preparegrind
      cachegrind
      callgrind
    ];

    BUILD = "debug";
  };
}
