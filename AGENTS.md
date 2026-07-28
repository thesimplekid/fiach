# AGENTS.md — fiach

High-signal constraints and instructions for AI agents working in this repository.

## Architecture & Layout

- **Pre-stage then delegate**: The app isolates work in a temp dir using `gh` CLI *before* giving control to the `goose` agent. This prevents wasting agent turns on git operations.
- **Structured reporting first**: Finder agents submit candidates through the in-process `fiach-reporting` frontend tool surface (`submit_finding` / `submit_no_findings`). Markdown is rendered by the host from structured data.
- **Separate verifier session**: Candidate findings are adjudicated in a second Goose session. The verifier submits one `submit_verdict` per finding; structured verdicts are authoritative for metadata and disclosure.
- **Host-only disclosure**: Models never post to GitHub. `src/disclose.rs` applies deterministic policy checks before any GitHub side effect.
- `src/main.rs`: Entrypoint, clap CLI parsing, tracing init.
- `src/daemon.rs`: Polling loop, PR discovery via GitHub CLI (`updated:>=`), deduplication.
- `src/review.rs`: Sets up finder/verifier Goose sessions, reporting tools, artifact rendering, retries, and token/cost accounting.
- `src/reporting.rs`: Structured finding/verdict schemas, validation, Markdown rendering, diff-anchor parsing, and disclosure policy helpers.
- `src/workspace.rs`: Uses `gh` to clone repo/PR to a temp dir and records PR lifecycle/base/default-branch context.
- `src/disclose.rs`: Report modes (`local`, `pr-comment`, `sync-pr`) and centralized GitHub review/sync disclosure.
- `src/state.rs`: Uses `redb` to track reviewed commit hashes.
- `flake.nix`: Package, dev shell, and NixOS module. The module can configure finder and verifier provider/model settings.
- `.agents/skills/`: Domain skills (e.g., `cashu`).

## Repository Workflow

- Before repository operations, determine whether the workspace is Jujutsu-backed (`.jj/` or `jj root`) or plain Git.
- This repo is JJ-backed. Prefer JJ-native commands (`jj status`, `jj diff`, `jj describe`, etc.) for local work.
- `jj status`/`jj diff` may need to snapshot into `.git/objects`; request escalation if sandboxing makes `.git` read-only.

## Reporting & Disclosure Rules

- Finder pass:
  - Submit candidate findings with `submit_finding`.
  - Submit a no-finding result with `submit_no_findings`.
  - Do not rely on Markdown frontmatter as the source of truth.
- Verifier pass:
  - Runs only when structured candidates exist and `verify_findings` is enabled.
  - Uses `verifier_provider` / `verifier_model` when set, otherwise falls back to finder `provider` / `model`.
  - Must submit one `submit_verdict` per candidate.
  - Verified disclosure requires command transcript evidence.
- PR comments are allowed only for open PRs with confirmed, PR-introduced, verifier-approved findings.
- Merged or closed PRs must not receive inline or top-level PR comments.
- Invalid inline anchors are downgraded into the review summary; they must not block other valid comments.
- Sandbox child reviews write artifacts only. Host daemon performs DB persistence and disclosure after validating sandbox output.

## Dependencies & Overrides (CRITICAL)

- **DO NOT** casually change dependency pins or overrides in `Cargo.toml` / `Cargo.lock`; Goose and RMCP versions are sensitive.
- Keep the manifest `rmcp = "1.2.0"` pin unless you have verified Goose compatibility and the lockfile outcome. The current lockfile may resolve a newer RMCP through Goose's dependency graph; do not "clean this up" as drive-by churn.

## Development Setup

- Requires **Rust 1.94.1** stable (provided via Nix shell `nix develop`).
- Requires `gh` CLI to be authenticated (`gh auth login`).
- Requires `.env` or service environment containing `GITHUB_TOKEN` and the selected provider API key (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GOOGLE_API_KEY`).

## Coding Conventions

- **Error Handling**: Use `anyhow::Result` everywhere. No custom error enums. Use `.context("...")` and `bail!("...")`. **Never** use `unwrap()` or `expect()` outside of tests. Non-fatal failures use `let _ = expr;`.
- **Async & Subprocesses**: Use Tokio. Use `tokio::process::Command` (not `std::process::Command`).
- **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`). **Prefer structured fields** over formatted strings: `info!(repo = %repo, "Starting");` (use `%` for Display, `?` for Debug).
- **Imports**: Group into 3 sections separated by blank lines: 1) `std`, 2) external, 3) `crate::`/`super::`. Use one `use` per crate with nested paths.
- **String Templates**: Use `const &str` with raw strings (`r#"..."#`) for large prompts. Fill placeholders using `str::replace("{key}", &value)`. Do not use `format!` for prompt templates.
- **Structured Data**: Prefer typed structs and `serde` validation for findings, verdicts, PR metadata, and disclosure policy. Avoid ad hoc string parsing for new reporting behavior.
- **Testing**: Tests live at the bottom of the source files in `#[cfg(test)] mod tests`. Some modules lack unit tests due to heavy `gh` CLI / OpenRouter dependencies. Test both success and error paths.

## Commands

- **Build**: `nix develop -c cargo build`
- **Format Rust**: `nix develop -c cargo fmt` (No `rustfmt.toml`, uses defaults)
- **Format Nix**: `nix develop -c nixpkgs-fmt flake.nix`
- **Lint**: `nix develop -c cargo clippy --all-targets -- -D warnings`
- **Test**: `nix develop -c cargo nextest run` (Nix shell provides `nextest`)
- **Check flake**: `nix flake show --no-write-lock-file`
