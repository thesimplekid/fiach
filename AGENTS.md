# AGENTS.md — fiach

High-signal constraints and instructions for AI agents working in this repository.

## Architecture & Layout

- **Pre-stage then delegate**: The app isolates work in a temp dir using `gh` CLI *before* giving control to the `goose` agent. This prevents wasting agent turns on git operations.
- `src/main.rs`: Entrypoint, clap CLI parsing, tracing init.
- `src/daemon.rs`: Polling loop, PR discovery via GitHub CLI (`updated:>=`), deduplication.
- `src/review.rs`: Sets up the Goose agent (provider, session, persona), streams output.
- `src/workspace.rs`: Uses `gh` to clone repo/PR to a temp dir.
- `src/disclose.rs`: Report modes (local, `pr-comment`, `sync-pr`).
- `src/state.rs`: Uses `redb` to track reviewed commit hashes.
- `.agents/skills/`: Domain skills (e.g., `cashu`).

## Dependencies & Overrides (CRITICAL)

- **DO NOT** remove or change the `[patch.crates-io]` section in `Cargo.toml`. `goose` requires specific git revisions of `sacp` and `rmcp` to build.
- `rmcp` must stay pinned to `1.2.0` because `goose` uses struct expressions on types that became `#[non_exhaustive]` in 1.3.0.

## Development Setup

- Requires **Rust 1.94.0** stable (provided via Nix shell `nix develop`).
- Requires `gh` CLI to be authenticated (`gh auth login`).
- Requires `.env` file containing `OPENROUTER_API_KEY` and `GITHUB_TOKEN`.

## Coding Conventions

- **Error Handling**: Use `anyhow::Result` everywhere. No custom error enums. Use `.context("...")` and `bail!("...")`. **Never** use `unwrap()` or `expect()` outside of tests. Non-fatal failures use `let _ = expr;`.
- **Async & Subprocesses**: Use Tokio. Use `tokio::process::Command` (not `std::process::Command`).
- **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`). **Prefer structured fields** over formatted strings: `info!(repo = %repo, "Starting");` (use `%` for Display, `?` for Debug).
- **Imports**: Group into 3 sections separated by blank lines: 1) `std`, 2) external, 3) `crate::`/`super::`. Use one `use` per crate with nested paths.
- **String Templates**: Use `const &str` with raw strings (`r#"..."#`) for large prompts. Fill placeholders using `str::replace("{key}", &value)`. Do not use `format!` for prompt templates.
- **Testing**: Tests live at the bottom of the source files in `#[cfg(test)] mod tests`. Some modules lack unit tests due to heavy `gh` CLI / OpenRouter dependencies. Test both success and error paths.

## Commands

- **Build**: `cargo build`
- **Format**: `cargo fmt` (No `rustfmt.toml`, uses defaults)
- **Lint**: `cargo clippy --all-targets -- -D warnings`
- **Test**: `cargo nextest run` (Nix shell provides `nextest`)
