default: check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo nextest run

check: fmt-check clippy test

nix-check:
    nix flake check --no-write-lock-file -L

# Real scheduler/broker/Goose/container workflow in a disposable NixOS VM.
issues-acp-check:
    nix build --no-link -L '.#checks.x86_64-linux.issues-acp'
