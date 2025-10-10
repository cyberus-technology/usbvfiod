# This file contains integration tests for usbvfiod.
{ lib, pkgs, usbvfiod }:
let
  # For the VM that we start in Cloud Hypervisor, we re-use the netboot image.
  netbootNixos = lib.nixosSystem {
    inherit (pkgs) system;

    modules = [
      "${pkgs.path}/nixos/modules/installer/netboot/netboot-minimal.nix"

      # Cloud Hypervisor Guest Convenience
      ({ config, ... }: {

        boot.kernelParams = [
          # Use the serial console for kernel output.
          #
          # The virtio-console is an option as well, but is not
          # compiled into the NixOS kernel and would be inconvenient.
          "console=ttyS0"
          # Enable dyndbg messages for the XHCI driver.
          "xhci_pci.dyndbg==pmfl"
          "xhci_hcd.dyndbg==pmfl"
        ];

        # Enable debug verbosity.
        boot.consoleLogLevel = 8;

        # allow disk access for users
        users.users.nixos.extraGroups = [ "disk" ];

        # Convenience packages for interactive use
        environment.systemPackages = with pkgs; [ pciutils usbutils ];

        # Silence the useless stateVersion warning. We have no state to keep.
        system.stateVersion = config.system.nixos.release;
      })
    ];
  };

  netboot =
    let
      inherit (netbootNixos) config;

      kernelTarget = pkgs.stdenv.hostPlatform.linux-kernel.target;
    in
    {
      initrd = "${config.system.build.netbootRamdisk}/initrd";
      kernel = "${config.system.build.kernel}/${kernelTarget}";
      cmdline = "init=${config.system.build.toplevel}/init "
        + builtins.toString config.boot.kernelParams;
    };

  # Putting the socket in a world-readable location is obviously not a
  # good choice for a production setup, but for this test it works
  # well.
  usbvfiodSocket = "/tmp/usbvfio";
  cloudHypervisorLog = "/tmp/chv.log";
  vendorId = "46f4";
  productId = "0001";

  # Provide a raw file as usb stick test image.
  blockDeviceFile = "/tmp/image.img";
  blockDeviceBlockSize = "512";
  blockDeviceBlockCount = "16384";

in
{
  interactive-smoke = pkgs.nixosTest {
    name = "usbvfiod Smoke Test";

    nodes.machine = { pkgs, ... }: {
      environment.systemPackages = with pkgs; [
        jq
        usbutils
        tmux
        (pkgs.writeScriptBin "usbvfiod" ''
          ${lib.getExe usbvfiod} -v --socket-path ${usbvfiodSocket} --device "/dev/bus/usb/teststorage"
        '')
        (pkgs.writeScriptBin "chv" ''
          ${lib.getExe pkgs.cloud-hypervisor} --memory size=2G,shared=on --console off --serial tty \
              --kernel ${netboot.kernel} \
              --cmdline ${lib.escapeShellArg netboot.cmdline} \
              --initramfs ${netboot.initrd} \
              --user-device socket=${usbvfiodSocket}
        '')
        (pkgs.writeScriptBin "test" ''
          tmux new-session \; \
            attach \; \
            send-keys 'usbvfiod' C-m \; \
            split-window -v \; \
            run-shell 'sleep 1' \; \
            send-keys 'chv' C-m \; \
        '')
      ];

      services.udev.extraRules = ''
        ACTION=="add", SUBSYSTEM=="usb", ATTRS{idVendor}=="${vendorId}", ATTRS{idProduct}=="${productId}", MODE="0660", GROUP="usbaccess", SYMLINK+="bus/usb/teststorage"
      '';

      users.groups.usbaccess = { };

      users.users.usbaccess = {
        isSystemUser = true;
        group = "usbaccess";
      };

      services.getty.autologinUser = "root";

      boot.kernelModules = [ "kvm" ];

      virtualisation = {
        cores = 2;
        memorySize = 4096;
        qemu.options = [
          # A virtual USB XHCI controller in the host ...
          "-device qemu-xhci,id=host-xhci,addr=10"
          # ... with an attached usb stick.
          "-drive if=none,id=usbstick,format=raw,file=${blockDeviceFile}"
          "-usb"
          "-device usb-storage,bus=host-xhci.0,drive=usbstick"
        ];
      };
    };

    # The nested CI runs are really slow.
    globalTimeout = 3600;
    testScript = ''
      import os
      print("Creating file image at ${blockDeviceFile}")
      os.system("dd bs=${blockDeviceBlockSize} count=${blockDeviceBlockCount} if=/dev/urandom of=${blockDeviceFile}")

      start_all()
    '';
  };
}
