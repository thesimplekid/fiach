{
  description = "Fiach";

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
    flake-utils.lib.eachDefaultSystem
      (
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
                "goose-1.37.0" = "sha256-YEK4cGcSx2ppEtR60x/4wwtJFMDO9jnK5bAo2RW3E64=";
              };
            };

            nativeBuildInputs = with pkgs; [ pkg-config protobuf ];
            buildInputs = with pkgs; [ openssl sqlite zlib ] ++ libsDarwin;
          };

          checks.nixos-module =
            let
              envFile = pkgs.writeText "fiach-env" ''
                GITHUB_TOKEN=ghp_example
                OPENROUTER_API_KEY=sk-example
                FIACH_SERVER_TOKEN=server-example
              '';
              testSystem = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  {
                    system.stateVersion = "25.11";
                    services.fiach = {
                      enable = true;
                      repos = [ "owner/repo" ];
                      environmentFile = envFile;
                      port = 4321;
                      maxWorkers = 2;
                      skipPrs = [ "123" "owner/repo#456" ];
                      drafts = true;
                      timeoutMins = 45;
                      maxRetries = 7;
                      retryDelaySecs = 20;
                      maxCostUsd = 12.5;
                      inputPricePerM = 1.25;
                      outputPricePerM = 5.75;
                      withSkill = "cashu-security";
                      sandbox = {
                        enable = true;
                        networkMode = "veth";
                      };
                    };
                  }
                ];
              };
              execStart = testSystem.config.systemd.services.fiach.serviceConfig.ExecStart;
              configPath =
                let
                  match = builtins.match ".*--config ([^ ]+) daemon.*" execStart;
                in
                if match == null then throw "Could not extract fiach config path from ExecStart" else builtins.head match;
              fiachNetwork = testSystem.config.systemd.network.networks."10-fiach-container";
              fiachNetworkName = fiachNetwork.matchConfig.Name;
              fiachNetworkAddress = fiachNetwork.networkConfig.Address or "";
              fiachServicePath = lib.concatMapStringsSep " " toString testSystem.config.systemd.services.fiach.path;
            in
            pkgs.runCommand "fiach-nixos-module-test" { } ''
              set -eu

              exec_start=${lib.escapeShellArg execStart}
              config_path=${lib.escapeShellArg configPath}
              fiach_network_name=${lib.escapeShellArg fiachNetworkName}
              fiach_network_address=${lib.escapeShellArg fiachNetworkAddress}
              fiach_service_path=${lib.escapeShellArg fiachServicePath}

              printf '%s' "$exec_start" | grep -F -- '--port 4321' >/dev/null
              printf '%s' "$fiach_network_name" | grep -F 've-fiach-*' >/dev/null
              test -z "$fiach_network_address"
              printf '%s' "$fiach_service_path" | grep -F 'iproute2' >/dev/null

              grep -F 'port = 4321' "$config_path" >/dev/null
              grep -F 'max_workers = 2' "$config_path" >/dev/null
              grep -F 'sandbox_network = "veth"' "$config_path" >/dev/null
              grep -F 'sandbox_rootfs = ' "$config_path" >/dev/null
              grep -F 'skip_prs = [' "$config_path" >/dev/null
              grep -F '"123"' "$config_path" >/dev/null
              grep -F '"owner/repo#456"' "$config_path" >/dev/null
              grep -F 'drafts = true' "$config_path" >/dev/null
              grep -F 'timeout_mins = 45' "$config_path" >/dev/null
              grep -F 'max_retries = 7' "$config_path" >/dev/null
              grep -F 'retry_delay_secs = 20' "$config_path" >/dev/null
              grep -F 'max_cost_usd = 12.5' "$config_path" >/dev/null
              grep -F 'input_price_per_m = 1.25' "$config_path" >/dev/null
              grep -F 'output_price_per_m = 5.75' "$config_path" >/dev/null
              grep -F 'with_skill = "cashu-security"' "$config_path" >/dev/null

              touch "$out"
            '';

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
        in
        {
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

            port = lib.mkOption {
              type = lib.types.int;
              default = 3000;
              description = "Port for the interactive web server.";
            };

            updatedWithinDays = lib.mkOption {
              type = lib.types.int;
              default = 120;
              description = "Number of days to look back for updated PRs when services.fiach.filterByUpdated is enabled.";
            };

            filterByUpdated = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Whether to include the updated:>= GitHub search filter when discovering PRs.";
            };

            prStates = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "open" ];
              description = "List of PR states to poll (e.g. ['open', 'closed', 'merged'])";
            };

            prLimit = lib.mkOption {
              type = lib.types.int;
              default = 1000;
              description = "Maximum number of PRs to fetch from GitHub per polling cycle";
            };

            allowedAuthorAssociations = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "COLLABORATOR" "CONTRIBUTOR" "MEMBER" "OWNER" ];
              description = "GitHub PR author associations allowed to trigger daemon reviews";
            };

            maxWorkers = lib.mkOption {
              type = lib.types.int;
              default = 1;
              description = "Maximum number of PR reviews to run concurrently per polling query. 0 means unlimited.";
            };

            model = lib.mkOption {
              type = lib.types.str;
              default = "google/gemini-3.1-pro-preview";
              description = "Model to use with the selected provider";
            };

            provider = lib.mkOption {
              type = lib.types.enum [ "openrouter" "anthropic" "openai" "google" ];
              default = "openrouter";
              description = "Goose provider to use for reviews";
            };

            verifierModel = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Model to use for the verifier pass. Defaults to services.fiach.model when unset.";
            };

            verifierProvider = lib.mkOption {
              type = lib.types.nullOr (lib.types.enum [ "openrouter" "anthropic" "openai" "google" ]);
              default = null;
              description = "Goose provider to use for the verifier pass. Defaults to services.fiach.provider when unset.";
            };

            dedupeExistingComments = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Run duplicate suppression against existing PR discussion before posting verified findings.";
            };

            dedupeModel = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Model to use for duplicate suppression. Defaults through verifierModel then model when unset.";
            };

            dedupeProvider = lib.mkOption {
              type = lib.types.nullOr (lib.types.enum [ "openrouter" "anthropic" "openai" "google" ]);
              default = null;
              description = "Goose provider to use for duplicate suppression. Defaults through verifierProvider then provider when unset.";
            };

            environmentFile = lib.mkOption {
              type = lib.types.path;
              description = "Path to environment file containing GITHUB_TOKEN, the selected provider API key, and optionally FIACH_SERVER_TOKEN.";
            };

            logFilter = lib.mkOption {
              type = lib.types.str;
              default = "fiach=info,goose=warn,rmcp=warn,sacp=warn,reqwest=warn,hyper=warn";
              description = "Tracing filter passed to RUST_LOG for the daemon and sandboxed review children.";
            };

            persona = lib.mkOption {
              type = lib.types.str;
              default = "builtin:security";
              description = "Single persona source to use when services.fiach.personas is unset";
            };

            personas = lib.mkOption {
              type = lib.types.nullOr (lib.types.listOf lib.types.str);
              default = null;
              description = "Persona sources to run independently for each PR. When set, this takes precedence over services.fiach.persona.";
            };

            reportMode = lib.mkOption {
              type = lib.types.enum [ "local" "pr-comment" "sync-pr" "hybrid" ];
              default = "local";
              description = "Mode for reporting findings. Options: local, pr-comment, sync-pr, hybrid";
            };

            maxTurns = lib.mkOption {
              type = lib.types.int;
              default = 60;
              description = "Maximum number of turns for the agent (prevents runaway costs)";
            };

            timeoutMins = lib.mkOption {
              type = lib.types.int;
              default = 30;
              description = "Timeout in minutes for each review session.";
            };

            maxRetries = lib.mkOption {
              type = lib.types.int;
              default = 3;
              description = "Maximum number of retries for LLM provider failures and failed review attempts.";
            };

            retryDelaySecs = lib.mkOption {
              type = lib.types.int;
              default = 10;
              description = "Initial delay in seconds before retrying an LLM failure.";
            };

            maxCostUsd = lib.mkOption {
              type = lib.types.nullOr lib.types.float;
              default = null;
              description = "Maximum budget in USD for each review.";
            };

            inputPricePerM = lib.mkOption {
              type = lib.types.nullOr lib.types.float;
              default = null;
              description = "Override input token price per 1M tokens in USD.";
            };

            outputPricePerM = lib.mkOption {
              type = lib.types.nullOr lib.types.float;
              default = null;
              description = "Override output token price per 1M tokens in USD.";
            };

            withSkill = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Optional skill name to instruct the agent to use.";
            };

            syncRepo = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "GitHub repository to sync reports to (e.g., 'owner/security-audits'). Required if reportMode is sync-pr, and for non-PR security findings in hybrid mode.";
            };

            notifyOnEmpty = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether to create PRs or comments even if no findings were found.";
            };

            reviewStartReaction = lib.mkOption {
              type = lib.types.str;
              default = "eyes";
              description = "GitHub reaction to add to a PR when Fiach starts reviewing it. Supports +1, -1, laugh, confused, heart, hooray, rocket, and eyes.";
            };

            noFindingsReaction = lib.mkOption {
              type = lib.types.str;
              default = "+1";
              description = "GitHub reaction to add to a PR when Fiach completes a review with no findings. Supports +1, -1, laugh, confused, heart, hooray, rocket, and eyes.";
            };

            verifyFindings = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Run a verifier pass before disclosure when findings are present.";
            };

            skipPrs = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "PR numbers or repo-qualified PRs to skip, e.g. '123' or 'org/repo#456'.";
            };

            drafts = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether to include draft PRs.";
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
                    default = [ ];
                  };
                };
              });
              default = { };
              description = "Context groups mapped by target repo";
            };

            sandbox = {
              enable = lib.mkEnableOption "Sandboxed PR reviews via systemd-nspawn";
              networkMode = lib.mkOption {
                type = lib.types.enum [ "host" "bridge" "private" "veth" ];
                default = "host";
                description = ''
                  Network mode for the sandbox.

                  "bridge" attaches each sandbox to an existing br-nspawn bridge
                  using systemd-nspawn's --network-bridge=br-nspawn. Use this
                  when the host already provisions bridge addressing, DHCP, NAT,
                  and forwarding.

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
                default = [ ];
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
              users.groups.fiach = lib.mkIf (!cfg.sandbox.enable) { };
              assertions = [
                {
                  assertion = !(cfg.sandbox.enable && cfg.sandbox.networkMode == "veth" && (cfg.maxWorkers == 0 || cfg.maxWorkers > 254));
                  message = "services.fiach.sandbox.networkMode = \"veth\" requires maxWorkers between 1 and 254 so each concurrent sandbox can receive a unique /30 subnet.";
                }
              ];

              systemd.services.fiach =
                let
                  fiachPkg = self.packages.${pkgs.stdenv.system}.default;

                  # Inside the sandboxed container we have a private network
                  # namespace.  systemd-nspawn names the container side of the
                  # veth pair "host0".  This script gives host0 a static IP,
                  # adds a default route via the host, and writes a resolv.conf
                  # pointing at public DNS resolvers.
                  sandboxEntrypoint = pkgs.writeShellScriptBin "fiach-sandbox-entrypoint" ''
                    set -e

                    : "''${FIACH_SANDBOX_DNS_PRIMARY:=1.1.1.1}"
                    : "''${FIACH_SANDBOX_DNS_SECONDARY:=9.9.9.9}"

                    check_tcp() {
                      ${pkgs.coreutils}/bin/timeout 3 ${pkgs.bash}/bin/bash -c ":</dev/tcp/$1/$2" >/dev/null 2>&1
                    }

                    if [ "${cfg.sandbox.networkMode}" = "veth" ]; then
                      : "''${FIACH_SANDBOX_HOST_GATEWAY:?missing FIACH_SANDBOX_HOST_GATEWAY}"
                      : "''${FIACH_SANDBOX_GUEST_CIDR:?missing FIACH_SANDBOX_GUEST_CIDR}"

                      # Bring up loopback and the container side of the veth pair.
                      ${pkgs.iproute2}/bin/ip link set lo up || true

                      # host0 may take a moment to appear after the container starts.
                      for _ in $(${pkgs.coreutils}/bin/seq 1 50); do
                        if ${pkgs.iproute2}/bin/ip link show host0 >/dev/null 2>&1; then
                          break
                        fi
                        sleep 0.1
                      done

                      ${pkgs.iproute2}/bin/ip link set host0 up
                      ${pkgs.iproute2}/bin/ip addr replace "$FIACH_SANDBOX_GUEST_CIDR" dev host0
                      ${pkgs.iproute2}/bin/ip route replace default via "$FIACH_SANDBOX_HOST_GATEWAY"
                    fi

                    # Static DNS so we don't depend on the host's resolv.conf.
                    # Cloudflare primary, Quad9 fallback.
                    cat > /etc/resolv.conf <<EOF
                    nameserver $FIACH_SANDBOX_DNS_PRIMARY
                    nameserver $FIACH_SANDBOX_DNS_SECONDARY
                    EOF

                    if [ "${cfg.sandbox.networkMode}" = "veth" ]; then
                      # Do not fail early here: gh/git/provider calls produce
                      # better errors than a generic sandbox preflight. Keep a
                      # single best-effort probe for host-side diagnostics.
                      if ! check_tcp api.github.com 443; then
                        echo "sandbox veth network probe warning: api.github.com:443 was not reachable before review start" >&2
                        echo "--- network probes ---" >&2
                        echo "dns_primary_tcp_53=$(
                          if check_tcp "$FIACH_SANDBOX_DNS_PRIMARY" 53; then
                            echo ok
                          else
                            echo failed
                          fi
                        )" >&2
                        echo "dns_secondary_tcp_53=$(
                          if check_tcp "$FIACH_SANDBOX_DNS_SECONDARY" 53; then
                            echo ok
                          else
                            echo failed
                          fi
                        )" >&2
                        echo "api_github_com_tcp_443=$(
                          if check_tcp api.github.com 443; then
                            echo ok
                          else
                            echo failed
                          fi
                        )" >&2
                        echo "--- ip addr ---" >&2
                        ${pkgs.iproute2}/bin/ip addr show >&2 || true
                        echo "--- ip route ---" >&2
                        ${pkgs.iproute2}/bin/ip route show >&2 || true
                        echo "--- ip neigh ---" >&2
                        ${pkgs.iproute2}/bin/ip neigh show >&2 || true
                        echo "--- resolv.conf ---" >&2
                        cat /etc/resolv.conf >&2 || true
                      fi
                    fi

                    mkdir -p /tmp/.local/state/goose/logs
                    mkdir -p /root/.local/state/goose/logs

                    exec /bin/fiach "$@"
                  '';

                  # The sandbox root filesystem tree containing required tools
                  sandboxSkills = pkgs.runCommand "fiach-sandbox-skills" { } ''
                    mkdir -p $out/etc/fiach
                    cp -R ${./.agents/skills} $out/etc/fiach/skills
                  '';

                  # systemd-nspawn rejects directory roots that do not look like an
                  # OS tree. The sandbox only needs a small command environment, but
                  # newer systemd releases still require /usr and os-release.
                  sandboxOsRelease = pkgs.runCommand "fiach-sandbox-os-release" { } ''
                    mkdir -p $out/etc $out/usr/lib
                    cat > $out/etc/os-release <<EOF
                    NAME="Fiach Sandbox"
                    ID=fiach-sandbox
                    PRETTY_NAME="Fiach Sandbox"
                    EOF
                    ln -s ../../etc/os-release $out/usr/lib/os-release
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
                      sandboxOsRelease
                    ];
                    pathsToLink = [ "/bin" "/etc" "/share" "/usr" ];
                  };

                  tomlFormat = pkgs.formats.toml { };
                  personaConfig =
                    if cfg.personas != null then {
                      personas = cfg.personas;
                    } else {
                      persona = cfg.persona;
                    };
                  configFile = tomlFormat.generate "fiach.toml" {
                    daemon = {
                      repos = cfg.repos;
                      port = cfg.port;
                      interval = cfg.interval;
                      updated_within_days = cfg.updatedWithinDays;
                      filter_by_updated = cfg.filterByUpdated;
                      pr_state = cfg.prStates;
                      pr_limit = cfg.prLimit;
                      skip_prs = cfg.skipPrs;
                      allowed_author_associations = cfg.allowedAuthorAssociations;
                      max_workers = cfg.maxWorkers;
                      drafts = cfg.drafts;
                      provider = cfg.provider;
                      model = cfg.model;
                      db_path = "${cfg.dataDir}/fiach.redb";
                      out_dir = "${cfg.dataDir}/reports";
                      report_mode = cfg.reportMode;
                      verify_findings = cfg.verifyFindings;
                      dedupe_existing_comments = cfg.dedupeExistingComments;
                      max_turns = cfg.maxTurns;
                      timeout_mins = cfg.timeoutMins;
                      max_retries = cfg.maxRetries;
                      retry_delay_secs = cfg.retryDelaySecs;
                    } // personaConfig // lib.optionalAttrs (cfg.verifierProvider != null) {
                      verifier_provider = cfg.verifierProvider;
                    } // lib.optionalAttrs (cfg.verifierModel != null) {
                      verifier_model = cfg.verifierModel;
                    } // lib.optionalAttrs (cfg.dedupeProvider != null) {
                      dedupe_provider = cfg.dedupeProvider;
                    } // lib.optionalAttrs (cfg.dedupeModel != null) {
                      dedupe_model = cfg.dedupeModel;
                    } // lib.optionalAttrs (cfg.withSkill != null) {
                      with_skill = cfg.withSkill;
                    } // lib.optionalAttrs (cfg.syncRepo != null) {
                      sync_repo = cfg.syncRepo;
                    } // lib.optionalAttrs (cfg.maxCostUsd != null) {
                      max_cost_usd = cfg.maxCostUsd;
                    } // lib.optionalAttrs (cfg.inputPricePerM != null) {
                      input_price_per_m = cfg.inputPricePerM;
                    } // lib.optionalAttrs (cfg.outputPricePerM != null) {
                      output_price_per_m = cfg.outputPricePerM;
                    } // lib.optionalAttrs cfg.notifyOnEmpty {
                      notify_on_empty = cfg.notifyOnEmpty;
                    } // {
                      review_start_reaction = cfg.reviewStartReaction;
                      no_findings_reaction = cfg.noFindingsReaction;
                    } // lib.optionalAttrs cfg.sandbox.enable {
                      sandbox_rootfs = "${sandboxRootfs}";
                      sandbox_network = cfg.sandbox.networkMode;
                      sandbox_extra_args = cfg.sandbox.extraArgs;
                    };
                    context_groups = cfg.contextGroups;
                  };
                in
                {
                  description = "Fiach Daemon";
                  after = [ "network-online.target" ];
                  wants = [ "network-online.target" ];
                  wantedBy = [ "multi-user.target" ];

                  path = with pkgs; [ git gh iproute2 systemd ];
                  serviceConfig = {
                    ExecStart = "${fiachPkg}/bin/fiach --config ${configFile} daemon --port ${toString cfg.port}";
                    EnvironmentFile = cfg.environmentFile;
                    StateDirectory = "fiach";
                    WorkingDirectory = cfg.dataDir;
                    Environment = [
                      "HOME=${cfg.dataDir}"
                      "GH_CONFIG_DIR=${cfg.dataDir}/.config/gh"
                      "RUST_LOG=${cfg.logFilter}"
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
            # systemd-nspawn names the host end of a plain --network-veth pair
            # "ve-<machine>". Fiach uses short machine names with the "fiach-"
            # prefix so these interface names avoid kernel truncation.
            # Fiach assigns each host link a per-sandbox /30 address at runtime.
            # NixOS NAT provides outbound internet access for the sandbox pool,
            # while networkd keeps the dynamic host-side veth links configured.
            (lib.mkIf (cfg.sandbox.enable && cfg.sandbox.networkMode == "veth") {
              boot.kernel.sysctl = {
                "net.ipv4.ip_forward" = lib.mkDefault 1;
                "net.ipv6.conf.all.forwarding" = lib.mkDefault 1;
              };

              systemd.network.enable = true;
              # This must sort before systemd's default 80-container-ve.network;
              # otherwise networkd can claim ve-fiach-* and assign its default
              # container subnet before Fiach's runtime 10.64.x.1/30 address is kept.
              systemd.network.networks."10-fiach-container" = {
                matchConfig.Name = "ve-fiach-*";
                networkConfig = {
                  IPMasquerade = "both";
                  KeepConfiguration = "static";
                  LinkLocalAddressing = "no";
                  LLDP = "no";
                  EmitLLDP = "no";
                };
              };

              networking.nat = {
                enable = lib.mkDefault true;
                internalIPs = [ "10.64.0.0/16" ];
              };

              networking.firewall.trustedInterfaces = [ "ve-fiach-+" ];
            })
          ]);
        };
    };
}
