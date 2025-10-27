{
  description = "USB Pass-Through vfio-user Tools";

  inputs = {
    dried-nix-flakes.url = "github:cyberus-technology/dried-nix-flakes";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs:
    let
      dnf = (inputs.dried-nix-flakes.for inputs).override {
        # Expose only platforms that cloud-hypervisor supports.
        # The `x86_64-linux` attribute is used arbitrarily to access the derivation's attributes.
        systems = inputs.nixpkgs.legacyPackages.x86_64-linux.cloud-hypervisor.meta.platforms;

        # Hercules CI runs can be configured using the `herculesCI` attribute.
        # See: https://docs.hercules-ci.com/hercules-ci-agent/evaluation
        extraOutputsWithoutSystems = [
          "herculesCI"
        ];
      };
      inherit (dnf)
        exportOutputs
        ;
    in
    exportOutputs (
      { self, nixpkgs, crane, fenix, advisory-db, git-hooks, currentSystem, ... }:

      let
        pkgs = nixpkgs.legacyPackages;

        inherit (nixpkgs) lib;

        craneLib = crane.mkLib pkgs;
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;
        };

        craneLibLLvmTools = craneLib.overrideToolchain
          (fenix.packages.complete.withComponents [
            "cargo"
            "llvm-tools"
            "rustc"
          ]);

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the actual crate itself, reusing the dependency
        # artifacts from above.
        usbvfiod = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          meta = {
            mainProgram = "usbvfiod";
          };
        });
      in
      {
        checks = {
          pre-commit-check = git-hooks.lib.${currentSystem}.run {
            src = ./.;
            hooks = {
              nixpkgs-fmt.enable = true;
              rustfmt.enable = true;
              typos.enable = true;
              deadnix.enable = true;
              statix.enable = true;
            };
          };

          # Build the crate as part of `nix flake check` for convenience
          inherit usbvfiod;

          # Run clippy (and deny all warnings) on the crate source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
          usbvfiod-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          usbvfiod-doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });

          # Check formatting
          usbvfiod-fmt = craneLib.cargoFmt {
            inherit src;
          };

          usbvfiod-toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
            # taplo arguments can be further customized below as needed
            # taploExtraArgs = "--config ./taplo.toml";
          };

          # Audit dependencies
          usbvfiod-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          # Audit licenses
          usbvfiod-deny = craneLib.cargoDeny {
            inherit src;
          };

          # Run tests with cargo-nextest
          # Consider setting `doCheck = false` on `usbvfiod` if you do not want
          # the tests to run twice
          usbvfiod-nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
            cargoNextestPartitionsExtraArgs = "--no-tests=pass";
          });
        } // (import ./nix/tests.nix {
          inherit lib pkgs;
          usbvfiod = self.packages.default;
        });

        packages = {
          default = usbvfiod;
        } // lib.optionalAttrs (!pkgs.stdenv.isDarwin) {
          usbvfiod-llvm-coverage = craneLibLLvmTools.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        apps.default = {
          type = "app";
          program = "${usbvfiod}/bin/usbvfiod";
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          inherit (self)
            checks
            ;

          # Additional dev-shell environment variables can be set directly
          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = [
            # pkgs.ripgrep
          ];

          shellHook = ''
            ${self.checks.pre-commit-check.shellHook}
            alias sshhost=ssh\ -p\ 2000\ root@localhost\ -o\ UserKnownHostsFile=/dev/null\ -o\ StrictHostKeyChecking=no
            alias sshguest=ssh\ -o\ ProxyCommand="ssh\ -W\ %h:%p\ -p\ 2000\ root@localhost\ -o\ UserKnownHostsFile=/dev/null\ -o\ StrictHostKeyChecking=no"\ -o\ UserKnownHostsFile=/dev/null\ -o\ StrictHostKeyChecking=no\ root@192.168.100.2
          '';
        };

        herculesCI = {
          ciSystems = [
            "x86_64-linux"
            "aarch64-linux"
          ];
        };
      }
    );
}
