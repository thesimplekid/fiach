{
  description = "CTF Reviewer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self
    , nixpkgs
    , rust-overlay
    , flake-utils
    , ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];

        lib = pkgs.lib;
        stdenv = pkgs.stdenv;
        isDarwin = stdenv.isDarwin;
        libsDarwin = lib.optionals isDarwin [
          # Additional darwin specific inputs can be set here
        ];

        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Toolchains
        # latest stable
        stable_toolchain = pkgs.rust-bin.stable."1.94.0".default.override {
          targets = [
            "wasm32-unknown-unknown"
          ];
          extensions = [
            "rustfmt"
            "clippy"
            "rust-analyzer"
            "llvm-tools-preview"
          ];
        };

        # Nightly used for formatting
        nightly_toolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.default.override {
            extensions = [
              "rustfmt"
              "clippy"
              "rust-analyzer"
              "rust-src"
              "llvm-tools-preview"
            ];
            targets = [ "wasm32-unknown-unknown" ];
          }
        );

        # Common inputs
        envVars = {
          NIX_PATH = "nixpkgs=${nixpkgs}";
        };

        baseBuildInputs =
          with pkgs;
          [
            git
            pkg-config
            curl
            just
            protobuf
            nixpkgs-fmt
            typos

            cargo-nextest

            # Needed for building native dependencies
            openssl
            sqlite
            zlib
          ]
          ++ libsDarwin;

        commonShellHook = ''
          export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [ pkgs.zlib ]}:$LD_LIBRARY_PATH
        '';

        nativeBuildInputs = [
        ]
        ++ lib.optionals isDarwin [
        ];
      in
      {
        packages.default = (pkgs.makeRustPlatform {
          cargo = stable_toolchain;
          rustc = stable_toolchain;
        }).buildRustPackage {
          pname = "fiach";
          version = "0.1.0";
          
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "goose-1.30.0" = "sha256-cKp7IN4nIACN75XWymZy0a07hdRy5xgkozErcJHcfFs=";
              "rmcp-1.2.0" = "sha256-pOmwJ6Lp3H5mM5IK/0OxLiVIwhIXKX/giw3qzJM6xAI=";
              "sacp-11.0.0" = "sha256-dcjr32HbUUBPDSMhz+xMBmfXEd3vImT+KVeKXfjXaOU=";
            };
          };

          nativeBuildInputs = with pkgs; [ pkg-config protobuf ];
          buildInputs = with pkgs; [ openssl sqlite zlib ] ++ libsDarwin;
        };

        devShells =
          let
            stable = pkgs.mkShell (
              {
                shellHook = commonShellHook;
                buildInputs = baseBuildInputs ++ [
                  stable_toolchain
                ];
                inherit nativeBuildInputs;

                # Environment variables for building
                PROTOC = "${pkgs.protobuf}/bin/protoc";
                PROTOC_INCLUDE = "${pkgs.protobuf}/include";
              }
              // envVars
            );

            nightly = pkgs.mkShell (
              {
                shellHook = commonShellHook;
                buildInputs = baseBuildInputs ++ [
                  nightly_toolchain
                ];
                inherit nativeBuildInputs;

                PROTOC = "${pkgs.protobuf}/bin/protoc";
                PROTOC_INCLUDE = "${pkgs.protobuf}/include";
              }
              // envVars
            );
          in
          {
            inherit stable nightly;
            default = stable;
          };
      }
    ) // {
      nixosModules.default = { config, lib, pkgs, ... }: 
      let
        cfg = config.services.fiach;
      in {
        options.services.fiach = {
          enable = lib.mkEnableOption "Fiach Daemon";
          
          repos = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            description = "List of repositories to monitor (e.g., ['org/repo'])";
          };
          
          interval = lib.mkOption {
            type = lib.types.int;
            default = 300;
            description = "Polling interval in seconds";
          };

          model = lib.mkOption {
            type = lib.types.str;
            default = "openrouter/google/gemini-3.1-pro-preview";
            description = "OpenRouter model to use";
          };

          environmentFile = lib.mkOption {
            type = lib.types.path;
            description = "Path to environment file containing GITHUB_TOKEN and OPENROUTER_API_KEY";
          };

          persona = lib.mkOption {
            type = lib.types.str;
            default = "builtin:security";
            description = "Persona source to use (e.g., 'builtin:security' or an absolute path)";
          };

          reportMode = lib.mkOption {
            type = lib.types.enum [ "local" "pr-comment" "sync-pr" ];
            default = "local";
            description = "Mode for reporting findings. Options: local, pr-comment, sync-pr";
          };

          syncRepo = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "GitHub repository to sync reports to (e.g., 'owner/security-audits'). Required if reportMode is sync-pr.";
          };

          notifyOnEmpty = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Whether to create PRs or comments even if no vulnerabilities were found.";
          };

          dataDir = lib.mkOption {
            type = lib.types.str;
            default = "/var/lib/fiach";
            description = "Directory to store state database and reports";
          };

          contextGroups = lib.mkOption {
            type = lib.types.attrsOf (lib.types.submodule {
              options = {
                repos = lib.mkOption {
                  type = lib.types.listOf lib.types.str;
                  default = [];
                };
              };
            });
            default = {};
            description = "Context groups mapped by target repo";
          };

          sandbox = {
            enable = lib.mkEnableOption "Sandboxed PR reviews via systemd-nspawn";
            networkMode = lib.mkOption {
              type = lib.types.enum [ "host" "private" "veth" ];
              default = "host";
              description = ''
                Network mode for the sandbox.

                "veth" gives the container an isolated network
                namespace and routes outbound traffic via NAT. The module
                automatically configures the host's systemd-networkd, IP
                forwarding, and firewall to make this work.

                "host" (default) shares the host's network namespace -- the simplest
                escape hatch if veth-based NAT cannot be used. The container
                then has full access to all host network interfaces.

                "private" gives the container only loopback (no internet); only
                useful for offline use cases.
              '';
            };
            extraArgs = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [];
              description = "Extra arguments to pass to systemd-nspawn";
            };
          };
        };

        config = lib.mkIf cfg.enable (lib.mkMerge [
          {
            # When sandbox is enabled, systemd-nspawn needs real root (UID 0) to
            # create mount/PID/network namespaces.  DynamicUser=true only gives
            # an unprivileged transient UID with ambient capabilities, which is
            # not enough.  We therefore create a dedicated system user and run
            # the service as root when sandboxing is active.
            users.users.fiach = lib.mkIf (!cfg.sandbox.enable) {
              isSystemUser = true;
              group = "fiach";
              home = cfg.dataDir;
            };
            users.groups.fiach = lib.mkIf (!cfg.sandbox.enable) {};

            systemd.services.fiach = let
              fiachPkg = self.packages.${pkgs.stdenv.system}.default;

              # Inside the sandboxed container we have a private network
              # namespace.  systemd-nspawn names the container side of the
              # veth pair "host0".  This script gives host0 a static IP,
              # adds a default route via the host (10.64.0.1), and writes
              # a resolv.conf pointing at public DNS resolvers.
              sandboxEntrypoint = pkgs.writeShellScriptBin "fiach-sandbox-entrypoint" ''
                set -e

                if [ "${cfg.sandbox.networkMode}" = "veth" ]; then
                  # Bring up loopback and the container side of the veth pair.
                  ${pkgs.iproute2}/bin/ip link set lo up || true

                  # host0 may take a moment to appear after the container starts.
                  for _ in 1 2 3 4 5 6 7 8 9 10; do
                    if ${pkgs.iproute2}/bin/ip link show host0 >/dev/null 2>&1; then
                      break
                    fi
                    sleep 0.1
                  done

                  ${pkgs.iproute2}/bin/ip link set host0 up
                  ${pkgs.iproute2}/bin/ip addr add 10.64.0.2/30 dev host0
                  ${pkgs.iproute2}/bin/ip route add default via 10.64.0.1
                fi

                # Static DNS so we don't depend on the host's resolv.conf.
                # Cloudflare primary, Quad9 fallback.
                cat > /etc/resolv.conf <<EOF
                nameserver 1.1.1.1
                nameserver 9.9.9.9
                EOF

                mkdir -p /tmp/.local/state/goose/logs
                mkdir -p /root/.local/state/goose/logs

                exec /bin/fiach "$@"
              '';

              # The sandbox root filesystem tree containing required tools
              sandboxSkills = pkgs.runCommand "fiach-sandbox-skills" {} ''
                mkdir -p $out/etc/fiach
                cp -R ${./.agents/skills} $out/etc/fiach/skills
              '';

              sandboxRootfs = pkgs.buildEnv {
                name = "fiach-sandbox-rootfs";
                paths = with pkgs; [
                  fiachPkg
                  bashInteractive
                  coreutils
                  git
                  gh
                  ripgrep
                  gnugrep
                  findutils
                  gnused
                  cacert
                  iana-etc
                  iproute2
                  python3
                  sandboxEntrypoint
                  sandboxSkills
                ];
                pathsToLink = [ "/bin" "/etc" "/share" ];
              };

              tomlFormat = pkgs.formats.toml {};
              configFile = tomlFormat.generate "fiach.toml" {
                daemon = {
                  repos = cfg.repos;
                  interval = cfg.interval;
                  model = cfg.model;
                  persona = cfg.persona;
                  db_path = "${cfg.dataDir}/fiach.redb";
                  out_dir = "${cfg.dataDir}/reports";
                  report_mode = cfg.reportMode;
                } // lib.optionalAttrs (cfg.syncRepo != null) {
                  sync_repo = cfg.syncRepo;
                } // lib.optionalAttrs cfg.notifyOnEmpty {
                  notify_on_empty = cfg.notifyOnEmpty;
                } // lib.optionalAttrs cfg.sandbox.enable {
                  sandbox_rootfs = "${sandboxRootfs}";
                  sandbox_network = cfg.sandbox.networkMode;
                  sandbox_extra_args = cfg.sandbox.extraArgs;
                };
                context_groups = cfg.contextGroups;
              };
            in {
              description = "Fiach Daemon";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];

              path = with pkgs; [ git gh systemd ];
              serviceConfig = {
                ExecStart = "${fiachPkg}/bin/fiach --config ${configFile} daemon";
                EnvironmentFile = cfg.environmentFile;
                StateDirectory = "fiach";
                WorkingDirectory = cfg.dataDir;
                Environment = [
                  "HOME=${cfg.dataDir}"
                  "GH_CONFIG_DIR=${cfg.dataDir}/.config/gh"
                ];
                Restart = "always";
                RestartSec = "10s";
              } // (if cfg.sandbox.enable then {
                # systemd-nspawn needs real root to create namespaces and mounts.
                # DynamicUser's transient UID with ambient caps is insufficient.
              } else {
                DynamicUser = true;
                User = "fiach";
                Group = "fiach";
              });
            };
          }

          # Host-side network configuration for the veth sandbox network mode.
          # systemd-nspawn names the host end of the veth pair "vb-<container>".
          # We give it a fixed /30 address and NAT outbound traffic so the
          # container can reach the public internet via the host's default
          # route.  The container side picks up the matching static IP via the
          # entrypoint script defined above.
          (lib.mkIf (cfg.sandbox.enable && cfg.sandbox.networkMode == "veth") {
            boot.kernel.sysctl = {
              "net.ipv4.ip_forward" = lib.mkDefault 1;
              "net.ipv6.conf.all.forwarding" = lib.mkDefault 1;
            };

            systemd.network.enable = true;
            systemd.network.networks."80-fiach-container" = {
              matchConfig.Name = "vb-*";
              networkConfig = {
                Address = "10.64.0.1/30";
                IPMasquerade = "both";
                LinkLocalAddressing = "no";
                LLDP = "no";
                EmitLLDP = "no";
              };
            };

            networking.firewall.trustedInterfaces = [ "vb-+" ];
          })
        ]);
      };
    };
}
