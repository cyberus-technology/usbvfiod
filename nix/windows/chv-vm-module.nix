# This is a systemd service that starts a cloud-hypervisor VM.
# It also sets up networking as a dependency.
# Most importantly, it setups the systemd service to expose a TTY we can attach to.
# This allows us to communicate with a running VM.

{
  config,
  pkgs,
  lib,
  ...
}:

let
  cfg = config.local.services.ch-vm;
  mkCloudCfg = pkgs.callPackage ./make-cloud-hypervisor-config.nix { };


  # this will need updated cargo deps to work
  /*
  chvMaster = pkgs.cloud-hypervisor.overrideAttrs {
    src = pkgs.fetchFromGitHub {
      owner = "cloud-hypervisor";
      repo = "cloud-hypervisor";
      rev = "279344800ee366c8d3d738b60072373d8c0eb49b";
      hash = "sha256-ZoXENG6XxJFYR+Nky7B0kwyRa856gWXINii7c2HBcSM="
    };
  };
  */
in
{
  options.local.services.ch-vm = {
    enable = lib.mkEnableOption "GWP Guest VMs on cloud-hypervisor";
    name = lib.mkOption {
      type = lib.types.str;
      description = ''
        The name of the VM.
      '';
      default = "runtime-vm";
    };
    startOnBoot = lib.mkOption {
      type = lib.types.bool;
      description = ''
        If enabled, starts this VM on boot.
      '';
      default = false;
    };
    memory = lib.mkOption {
      type = lib.types.str;
      description = ''
        The amount of memory the hypervisor should allocate for this VM.
        Use common strings like 1024M or 4G.
      '';
      default = "4G";
    };
    vcpus = lib.mkOption {
      type = lib.types.int;
      description = ''
        The number of vCPUs to allocate to this VM.
      '';
      default = 1;
    };
    network.guestAddress = lib.mkOption {
      type = lib.types.str;
      description = ''
        Set the internal IP of the guest VM.
      '';
      example = "10.255.255.250";
    };
    diskPath = lib.mkOption {
      type = lib.types.path;
      description = ''
        Set the disk Image path to boot.
      '';
      example = "/tmp/windows-image.raw";
    };
  };

  config = lib.mkIf cfg.enable {

    system.build.guestVMs =
      let
        cloudCfg = mkCloudCfg {
          kernel = "${lib.getOutput "fd" pkgs.OVMF-cloud-hypervisor}/FV/CLOUDHV.fd";
          # Make sure this is owned by root and chmod 644
          diskPath = builtins.toString cfg.diskPath;
          inherit (cfg) memory name vcpus;
        };
      in
      {
        inherit cloudCfg;
      };

    # necessary to not need sudo for CH
    security.wrappers.cloud-hypervisor = {
      owner = "root";
      group = "root";
      source = "${pkgs.cloud-hypervisor}/bin/cloud-hypervisor";
      setuid = true;
    };

    # Create a systemd service for the guest VM:
    systemd.services."guestVM@${cfg.name}" =
      let
        inherit (config.system.build.guestVMs) cloudCfg;
      in
      # cloud-hypervisor systemd services
      {
        description = "Cloud Hypervisor Guest VM: %i";
        wantedBy = lib.mkIf cfg.startOnBoot [ "multi-user.target" ];
        environment = { };

        serviceConfig = {
            # TBD Type = "exec";
            Restart = "on-failure";
            RestartSec = "2s";
            # Disable OOM Killer for VMs.
            OOMScoreAdjust = -1000;
            # Need to wrap that into a shell script, otherwise it tries to parse the brackets as a section header
            ExecStart = pkgs.writeShellScript "execStartCHV" ''
              set -x
              RUNTIME_ARGS="--user-device socket=/tmp/usbvfiod"

              /run/wrappers/bin/cloud-hypervisor ${cloudCfg} $RUNTIME_ARGS
            '';
            ExecStop = [
              "${pkgs.cloud-hypervisor}/bin/ch-remote --api-socket /run/ch-${cfg.name}.sock power-button"
              "${pkgs.bash}/bin/sh -c 'while kill -0 $MAINPID 2>/dev/null; do sleep 1; done'"
            ];
            TimeoutStopSec = 30;
            RestrictAddressFamilies = "~AF_INET6 AF_PACKET AF_NETLINK";
            SystemCallFilter = "~@clock @module @mount @swap";
          };

        postStop = ''
            # Delete the socket if it exists, since we currently sometimes see that it wasn't deleted by the CHV
            if [[ -e /run/ch-${cfg.name}.sockssh ]]; then
              echo "Delete CH socket"
              "${pkgs.coreutils}/bin/rm /run/ch-${cfg.name}.sock"
              "${pkgs.coreutils}/bin/rm /tmp/ch-${cfg.name}.sock"
            fi
        '';
      };
  };
}
