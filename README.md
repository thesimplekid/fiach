# Fiach

**Fiach** (Irish for *Hunter* or *Seeker*) is an autonomous, AI-powered PR reviewer built in Rust using the [goose](https://github.com/block/goose) agent framework. 

It acts as a background daemon that monitors configured GitHub repositories, checks out active Pull Requests, and uses an LLM (via OpenRouter) to review the code against a fully customizable **Persona**. If the agent finds actionable issues (like security vulnerabilities or code quality violations), `fiach` can automatically report them by commenting on the PR or opening a dedicated disclosure PR on a centralized tracking repository.

---

## 🚀 Features

- **Custom Personas:** Define exactly what the agent should look for using a Markdown file. Use different personas for Security CTF-style audits, Code Quality checks, or Architecture reviews.
- **Reporting Modes:**
  - `local` (Default): Saves the generated report to disk.
  - `pr-comment`: Posts the report directly as a comment on the target PR.
  - `sync-pr`: Clones a designated disclosure repository (e.g., `owner/security-audits`) and opens a new Pull Request containing the findings.
- **Smart Daemon:** Automatically polls for open PRs that have been active in the **last 4 months (120 days)**.
- **Configuration File:** Uses `fiach.toml` for easy setup of `daemon`, `review`, and additional repository contexts. Copy `example.fiach.toml` to `fiach.toml` to get started.
- **Skip PRs:** Ability to skip specific PRs by number or `repo#number` format.
- **State Tracking:** Uses a lightweight, embedded Rust database (`redb`) to remember which commit hashes have already been reviewed, preventing redundant LLM calls.
- **Workspace Isolation:** Clones the repository and checks out the PR branch into a temporary directory *before* giving control to the AI agent, saving valuable context window and turns.
- **Interactive Web Server:** The daemon includes a built-in HTTP server to monitor its status, view review history, and manually trigger reviews on-demand without waiting for the next polling cycle.

---

## 🛠 Prerequisites

- **Rust:** `1.94.0` (or use the provided Nix flake: `nix develop`)
- **GitHub CLI (`gh`):** Must be installed and authenticated (`gh auth login`).
- **Environment Variables:**
  - `OPENROUTER_API_KEY`: For LLM access.
  - `GITHUB_TOKEN`: For repository cloning and PR interaction.

### Credential Scope

The review agent can execute shell commands. In sandbox mode, `fiach` forwards `OPENROUTER_API_KEY` and `GITHUB_TOKEN` into the `systemd-nspawn` container so the review can reach OpenRouter and GitHub.

The sandbox also bootstraps its own runtime environment for service deployments:
- a CA bundle path for `git` and `gh`
- writable Goose state and log directories
- packaged domain skills for environments where the review workspace does not contain `.agents/skills`

Treat both credentials as readable by the agent during a review. Use least-privilege credentials:
- Prefer a fine-grained `GITHUB_TOKEN` scoped only to the repositories `fiach` must review and disclose to.
- Avoid broad write access, org-wide scopes, or access to unrelated private repositories.
- Use an `OPENROUTER_API_KEY` with the smallest practical billing and account exposure.
- Do not reuse high-trust personal credentials for the daemon.

You can copy the environment template to get started:
```bash
cp .env.example .env
```

---

## 📖 Usage Examples

### 1. The Autonomous Security Daemon (Sync PR Mode)

This is the primary use case. Run `fiach` as a background daemon that monitors multiple repositories. Out of the box, the daemon searches for **any open PR updated in the last 4 months**. 

When it finds an actionable vulnerability, it will push a disclosure PR to a centralized repository (`my-org/security-audits`).

```bash
cargo run -- daemon \
  --repos "my-org/core-backend,my-org/frontend-app" \
  --report-mode sync-pr \
  --sync-repo "my-org/security-audits" \
  --interval 300 \
  --model "openrouter/google/gemini-3.1-pro-preview" \
  --skip-prs "123,my-org/core-backend#456"
```

### 2. Single PR Review (PR Comment Mode)

If you just want to run a one-off review on a specific PR and have the bot comment its findings directly on that PR:

```bash
cargo run -- review \
  --repo "org/repo" \
  --pr 1835 \
  --report-mode pr-comment
```

### 3. Local Only (Testing a New Persona)

Testing a new code-quality persona and just want to see the markdown output saved to your current directory:

```bash
cargo run -- review \
  --repo "my-org/repo" \
  --pr 42 \
  --persona "builtin:code-quality" \
  --report-mode local
```

### 4. Interacting with the Daemon Web Server

When running the daemon, an interactive web server starts automatically on port `3000` (configurable via `--port`). This allows you to inspect the daemon's history and trigger reviews on demand.

- **Check health:**
  ```bash
  curl http://localhost:3000/health
  ```
- **List all reviewed PRs:**
  ```bash
  curl http://localhost:3000/reviews
  ```
- **Trigger a manual review immediately:**
  ```bash
  curl -X POST -H "Content-Type: application/json" \
       -d '{"owner":"my-org", "repo":"repo", "pr":42}' \
       http://localhost:3000/review
  ```
- **Get JSON metadata for a specific review:**
  ```bash
  curl "http://localhost:3000/review?owner=my-org&repo=repo&pr=42"
  ```
- **Read the Markdown report for a specific review:**
  ```bash
  curl "http://localhost:3000/review/content?owner=my-org&repo=repo&pr=42"
  ```

---

## 📝 Crafting a Persona

Fiach is entirely prompt-driven. You can configure the daemon to use different personas via the `--persona` flag. 

If omitted, it defaults to `--persona builtin:security`. You can also pass `--persona builtin:code-quality` or an absolute path to a custom Markdown file.

A custom persona file can contain these placeholders which are filled at runtime:
- `{repo}` — The target repository.
- `{pr_number}` — The PR being reviewed.
- `{base_branch}` — The base branch the PR is merging into.
- `{report_path}` — The absolute path where the agent MUST write its final report.
- `{skill_hint}` — Instructions for loading optional domain skills.

### The Notification Trigger
The reporting engine (for `pr-comment` and `sync-pr`) decides whether to notify humans by parsing the YAML frontmatter of the generated report. Instruct your agent to output:

```yaml
---
title: "SQL Injection in User Auth"
notify: true
status: confirmed
severity: high
---
```

If `notify: true` is present, or if `findings_count > 0`, `fiach` will trigger the configured disclosure mode. If no issues are found, the agent should output `notify: false`, and `fiach` will remain silent (unless you pass `--notify-on-empty`).

*See `personas/security-persona.md` for a complete example of an aggressive, CTF-style vulnerability hunting prompt.*

---

## ❄️ NixOS Deployment

`fiach` comes with a NixOS module for easy deployment as a systemd background service.

In your `flake.nix` or `configuration.nix`:

```nix
{
  inputs.fiach.url = "github:your-org/fiach";

  outputs = { self, nixpkgs, fiach, ... }: {
    nixosConfigurations.my-server = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        fiach.nixosModules.default
        {
          services.fiach = {
            enable = true;
            
            # The repos to poll (active in the last 4 months)
            repos = [ "my-org/core-app" "my-org/website" ];
            
            # The persona to use (defaults to builtin:security if omitted)
            persona = "builtin:security";
            
            # Model to use
            model = "openrouter/anthropic/claude-3-7-sonnet";
            
            # Disclosure Configuration
            reportMode = "sync-pr";
            syncRepo = "my-org/security-audits";
            
            # Environment file containing OPENROUTER_API_KEY and GITHUB_TOKEN (KEY=VALUE format)
            environmentFile = "/run/secrets/fiach-env";

            # Sandbox Isolation (Highly Recommended)
            # Isolates each PR review inside a systemd-nspawn container.
            sandbox = {
              enable = true;
              
              # Network Mode:
              # - "host" (default): Most reliable service-mode option today. Shares the host network stack.
              # - "veth": Better namespace isolation. Allows outbound internet but blocks access to
              #   host-local services. This is namespace isolation, not egress filtering, and it does
              #   NOT restrict the sandbox to GitHub/OpenRouter only.
              # - "private": Fully offline.
              networkMode = "host";
            };
          };
        }
      ];
    };
  };
}
```

### Sandbox Networking Limits

`systemd-nspawn` can isolate the sandbox from the host network namespace, but it does not provide destination allowlisting such as "only GitHub and OpenRouter".

- `sandbox.networkMode = "host"` is the current default because it is the most reliable option for service deployments.
- `sandbox.networkMode = "veth"` blocks access to host-local services but still allows outbound internet access.
- `sandbox.networkMode = "private"` disables all network access, which also prevents GitHub and OpenRouter access.
- Restricting outbound traffic to specific destinations requires host-side enforcement such as `nftables`/`iptables` rules on the `ve-*` interfaces, or a proxy-based egress policy.
- IP allowlists can be managed in NixOS firewall configuration, but they are brittle for CDN-backed services.
- If you need domain-level guarantees, use a dedicated proxy or egress gateway; `systemd-nspawn` alone is not sufficient.

`veth` remains available and is the intended tighter-isolation mode, but `host` is the documented default until the `veth` path is hardened further.

### Sandbox Write Scope

In daemon sandbox mode, the sandbox no longer gets write access to the whole `fiach` data directory.

- The sandbox writes only per-review artifacts in a dedicated run directory.
- Review state in `fiach.redb` is recorded by the host daemon after the sandbox exits successfully.
- Disclosure side effects are also performed by the host daemon after it validates the sandbox output.

This reduces the impact of a malicious or prompt-injected agent: it can still produce a bad report, but it cannot directly corrupt the review database from inside the sandbox.

### Domain Skills In Sandbox Mode

`fiach` looks for domain skills in this order:
- `./.agents/skills` in the active review workspace
- `/etc/fiach/skills` packaged into the sandbox rootfs

This lets NixOS/systemd deployments keep using bundled skills such as `rust-security` even when the target PR repository does not contain its own `.agents/skills` directory.

## 🏗 Project Layout

- `src/main.rs`: CLI argument parsing and orchestration.
- `src/daemon.rs`: Polling loop, PR discovery via GitHub CLI (`updated:>=`), and deduplication.
- `src/review.rs`: Sets up the Goose agent, injects the persona, and streams LLM output.
- `src/server.rs`: Axum-based interactive web server for daemon management and reporting.
- `src/workspace.rs`: Manages cloning the repo and checking out the PR into a temporary directory.
- `src/disclose.rs`: Handles the `ReportMode` logic (commenting or creating Sync PRs).
- `src/state.rs`: Manages the `redb` database for tracking reviewed commit hashes.
