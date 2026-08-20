# Fiach

**Fiach** (Irish for *Hunter* or *Seeker*) is an autonomous, AI-powered PR reviewer built in Rust using the [goose](https://github.com/block/goose) agent framework. 

It acts as a background daemon that monitors configured GitHub repositories, checks out active Pull Requests, and uses an LLM (via OpenRouter) to review the code against a fully customizable **Persona**. If the agent finds actionable issues (like security vulnerabilities or code quality violations), `fiach` can automatically report them by commenting on the PR or opening a dedicated disclosure PR on a centralized tracking repository.

---

## 🚀 Features

- **Custom Personas:** Define exactly what the agent should look for using a Markdown file. Use different personas for security audits, general PR code review, code quality checks, or architecture reviews.
- **Reporting Modes:**
  - `local` (Default): Saves the generated report to disk.
  - `pr-comment`: Posts the report directly as a comment on the target PR.
  - `sync-pr`: Clones a designated disclosure repository (e.g., `owner/security-audits`) and opens a new Pull Request containing the findings.
- **Smart Daemon:** Automatically polls for open PRs that have been active in the **last 4 months (120 days)**.
- **Configuration File:** Uses `fiach.toml` for easy setup of `daemon`, `review`, and additional repository contexts. Copy `example.fiach.toml` to `fiach.toml` to get started.
- **Skip PRs:** Ability to skip specific PRs by number or `repo#number` format.
- **Mention Trigger:** Set `trigger_mention` (or `--trigger-mention`) to a GitHub username so the daemon only reviews PRs where that account has been @-mentioned (e.g. commenting `@fiach-bot`), instead of every open PR. Each mention triggers one review — mention again to re-review. Only mentions from `allowed_mention_users` count (or, when unset, from commenters matching `allowed_author_associations`), and a trusted mention also lets the review run on PRs from authors outside the allowlist, so a maintainer can summon the bot on a new contributor's PR. When the review starts, the triggering comment receives the `review_start_reaction` (default 👀) so the mentioner knows the bot saw it, and when it finishes with nothing actionable (no findings, all findings rejected by the verifier, or already reported) the 👀 is replaced with the `no_findings_reaction` (default 👍).
- **Author Allowlist:** Restrict daemon reviews to trusted GitHub author associations before executing any reviewer workspace.
- **Worker Limit:** Run multiple PR reviews concurrently with an optional cap for resource-constrained hosts.
- **Verifier Pass:** Re-check actionable findings against the complete PR diff before any disclosure mode runs.
- **State Tracking:** Uses a lightweight, embedded Rust database (`redb`) to remember which commit hashes have already been reviewed, preventing redundant LLM calls.
- **Workspace Isolation:** Clones the repository and checks out the PR branch into a temporary directory *before* giving control to the AI agent, saving valuable context window and turns.
- **Per-Review Sandbox Logs:** Sandboxed reviews write `nspawn.log` next to their report artifacts for easier debugging.
- **Interactive Web Server:** The daemon includes a built-in HTTP server to monitor its status, view review history, and manually trigger reviews on-demand without waiting for the next polling cycle.

---

## 🛠 Prerequisites

- **Rust:** `1.94.1` (or use the provided Nix flake: `nix develop`)
- **GitHub CLI (`gh`):** Must be installed and authenticated (`gh auth login`).
- **Environment Variables:**
  - `OPENROUTER_API_KEY`: For default OpenRouter LLM access.
  - `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GOOGLE_API_KEY`: Required only when using the matching direct provider.
  - `GITHUB_TOKEN`: Host-only token for PR discovery and permitted disclosure actions.
  - `FIACH_REVIEW_GITHUB_TOKEN`: Separate read-only token used inside review sandboxes for cloning repositories and reading PR metadata.

### Credential Scope

The review agent can execute shell commands. In sandbox mode, `fiach` forwards configured provider API keys and maps `FIACH_REVIEW_GITHUB_TOKEN` to `GITHUB_TOKEN` inside the `systemd-nspawn` container so the review can reach the selected LLM provider and read GitHub data. The host `GITHUB_TOKEN` is never forwarded to the model-controlled process.

Non-sandboxed reviews share the host process environment and do not provide this credential boundary. Use sandbox mode whenever the host token can perform write or disclosure actions.

The sandbox also bootstraps its own runtime environment for service deployments:
- a CA bundle path for `git` and `gh`
- writable Goose state and log directories
- packaged domain skills for environments where the review workspace does not contain `.agents/skills`

Treat every credential forwarded into the review sandbox—the read-only review token and provider keys—as readable by the agent. Use least-privilege credentials:
- Scope the host `GITHUB_TOKEN` only to the repositories and disclosure actions `fiach` needs.
- Configure `FIACH_REVIEW_GITHUB_TOKEN` as a distinct fine-grained token with read-only repository contents, metadata, and pull-request access. Sandboxed reviews fail closed when it is absent; do not reuse the host disclosure token.
- Avoid broad write access, org-wide scopes, or access to unrelated private repositories.
- Use an `OPENROUTER_API_KEY` with the smallest practical billing and account exposure.
- If using a direct provider, apply the same least-privilege and billing limits to `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GOOGLE_API_KEY`.
- Do not reuse high-trust personal credentials for the daemon.

You can copy the environment template to get started:
```bash
cp .env.example .env
```

### Development Checks

The repository pins Rust 1.94.1 in `rust-toolchain.toml`; the Nix shell uses the
same version. Run the fast contributor checks with:

```bash
nix develop --command just check
```

Run the package, NixOS module, Rust formatting, Nix formatting, and spelling
checks exposed by the flake with:

```bash
nix flake check --no-write-lock-file -L
```

---

## 📖 Usage Examples

### 1. The Autonomous Security Daemon (Sync PR Mode)

This is the primary use case. Run `fiach` as a background daemon that monitors multiple repositories. Out of the box, the daemon searches for **any open PR updated in the last 4 months**. 

When it finds an actionable, verifier-approved finding, it will push a report PR to a centralized repository (`my-org/security-audits`).

```bash
cargo run -- daemon \
  --repos "my-org/core-backend,my-org/frontend-app" \
  --report-mode sync-pr \
  --sync-repo "my-org/security-audits" \
  --interval 300 \
  --provider "openrouter" \
  --model "google/gemini-3.1-pro-preview" \
  --skip-prs "123,my-org/core-backend#456" \
  --allowed-author-associations "COLLABORATOR,CONTRIBUTOR,MEMBER,OWNER" \
  --max-workers 4
```

### 2. Single PR Review (PR Comment Mode)

If you just want to run a one-off review on a specific PR and have the bot comment its findings directly on that PR:

```bash
cargo run -- review \
  --repo "org/repo" \
  --pr 1835 \
  --report-mode pr-comment
```

### 3. General PR Code Review

For a non-security review focused on correctness, regressions, API compatibility, and maintainability:

```bash
cargo run -- review \
  --repo "org/repo" \
  --pr 1835 \
  --persona "builtin:pr-review" \
  --report-mode pr-comment
```

### 4. Local Only (Testing a New Persona)

Testing a new code-quality persona and just want to see the markdown output saved to your current directory:

```bash
cargo run -- review \
  --repo "my-org/repo" \
  --pr 42 \
  --persona "builtin:code-quality" \
  --report-mode local
```

### 5. Interacting with the Daemon Web Server

When running the daemon, an interactive web server starts automatically on port `3000` (configurable via `--port`). This allows you to inspect the daemon's history and trigger reviews on demand.
Set `FIACH_SERVER_TOKEN` to require `Authorization: Bearer <token>` or `X-Fiach-Token: <token>` on review and job endpoints. Liveness, readiness, and aggregate metrics remain unauthenticated on the loopback-only listener.

- **Check health:**
  ```bash
  curl http://localhost:3000/health
  ```
- **Check readiness:** validates both the durable state store and the in-process scheduler.
  ```bash
  curl http://localhost:3000/ready
  ```
- **Read Prometheus metrics:** exposes scheduler jobs, durable review status counts, and recorded review cost.
  ```bash
  curl http://localhost:3000/metrics
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
  The accepted response is process-local job metadata:
  ```json
  {"job_id":"<opaque-id>","status":"queued"}
  ```
  Manual and polled reviews share one FIFO scheduler. Its queue capacity is
  `max(2 × max_workers, 16)`; manual work does not preempt a running review.
- **Inspect a queued or running job:**
  ```bash
  curl http://localhost:3000/jobs/<job_id>
  ```
  Jobs report `queued`, `running`, `completed`, `failed`, `skipped`, or
  `cancelled`, along with their target and timestamps. The newest 1,000
  terminal jobs are retained in memory. Job IDs and statuses do not survive a
  daemon restart; an unknown or evicted ID returns `404`. Durable review claims
  left `in_progress` by a stopped daemon are marked failed during startup so
  normal retry policy can reclaim them immediately.
- **Get JSON metadata for a specific review:**
  ```bash
  curl "http://localhost:3000/review?owner=my-org&repo=repo&pr=42"
  ```
- **Read the Markdown report for a specific review:**
  ```bash
  curl "http://localhost:3000/review/content?owner=my-org&repo=repo&pr=42"
  ```
  Report content is loaded from the authoritative absolute artifact path stored
  with the review record, including reports produced by sandbox workers.

### State database migration

The redb schema is versioned and migrated automatically when the daemon opens
the database. Before a legacy database is migrated, Fiach writes a sibling
`<database>.v1.backup` file. Legacy flat reports and sandbox run artifacts are
recorded when they can be found; missing files leave the corresponding artifact
path empty. Migration uses one database transaction, so a failed migration does
not replace the original records.

---

## 📝 Crafting a Persona

Fiach is entirely prompt-driven. You can configure the daemon to use different personas via the `--persona` flag.

If omitted, it defaults to `--persona builtin:security`. You can also pass `--persona builtin:pr-review`, `--persona builtin:code-quality`, or an absolute path to a custom Markdown file.

To run multiple independent passes for each PR, configure `personas`:

```toml
[daemon]
personas = ["builtin:pr-review", "builtin:security"]
max_workers = 2
```

Each persona gets its own state key and report path, so a successful PR review pass does not suppress the security pass for the same PR/commit. In `sync-pr` mode, non-default personas also get persona-scoped sync branches and report files. `max_workers` applies to the expanded review-job queue: two personas across ten PRs creates twenty review jobs.

A custom persona file can contain these placeholders which are filled at runtime:
- `{repo}` — The target repository.
- `{pr_number}` — The PR being reviewed.
- `{base_branch}` — The base branch the PR is merging into.
- `{report_path}` — The absolute path where Fiach writes the final rendered Markdown report.
- `{skill_hint}` — Instructions for loading optional domain skills.

### Review Lanes

Review lanes are narrower focus prompts that run inside one persona review. Use them when you want one report and one verifier/disclosure pass, but want the finder to split its attention across several independent lenses.

This differs from `personas`: multiple personas create separate review jobs, state keys, and reports; lanes feed back into the same parent finder, which submits one combined structured result.

Example for a general PR review:

```toml
[daemon]
persona = "builtin:pr-review"
review_lanes = ["correctness", "concurrency", "api-compat", "tests"]
max_review_lanes = 2
```

Example for a security review with a custom domain lane:

```toml
[daemon]
persona = "builtin:security"
review_lanes = ["input-boundaries", "cashu-mint", "state-atomicity"]
max_review_lanes = 2

[daemon.review_lane_prompts]
cashu-mint = """
Focus on Cashu mint correctness:
- quote idempotency
- blinded signature issuance
- keyset transitions
- accounting invariants
Only report concrete PR-introduced issues.
"""
```

Lane names and custom prompt keys are normalized before matching, so `Cashu Mint`, `cashu_mint`, and `cashu-mint` all refer to the same lane. If a lane has no custom prompt, Fiach uses a built-in focus prompt for known lanes such as `security`, `correctness`, `concurrency`, `api-compat`, `tests`, `performance`, and `observability`; unknown lanes get a generic focus prompt.

The built-in `summary` lane produces a neutral structured description of what
the PR changes. It does not make a review decision and is stored separately
from findings. Buzz delivery enables this lane automatically.

The finder launches Goose delegate subagents for the lanes, collects their notes, deduplicates candidates, and then the parent finder is the only agent allowed to call `submit_finding` or `submit_no_findings`. The verifier, Markdown report, duplicate suppression, and PR comments remain unified.

### Buzz Review Threads

Fiach can publish completed reviews to Buzz through the embedded
`buzz-client` and `buzz-sdk` libraries in the trusted host process. General
review personas create a root message in the public channel containing the PR
summary, followed by one direct thread reply for each verifier-approved,
non-duplicate finding. Security personas create a separate thread in the
configured private channel, and only when they have verified findings.
Security content never falls back to the public channel.

```toml
[daemon]
personas = ["builtin:pr-review", "builtin:security"]

[daemon.buzz]
relay_url = "https://buzz.example.com"
public_channel = "00000000-0000-0000-0000-000000000001"
security_channel = "00000000-0000-0000-0000-000000000002"
private_key_env = "FIACH_BUZZ_PRIVATE_KEY"
```

Set `FIACH_BUZZ_PRIVATE_KEY` in the Fiach service environment and ensure that
identity is a member of both channels. `auth_tag_env` may name an environment
variable containing a NIP-OA auth tag when the identity is managed. Buzz
delivery is additive to `report_mode`; a delivery failure is logged without
changing the completed GitHub review result. Re-reviews of the same PR and
persona reuse the original Buzz summary thread. Fiach stores the root event and
delivered finding identities in its review database, then appends only new
findings after later commits.

The daemon can also answer questions about a completed review when its Buzz
identity is tagged inside the corresponding review thread:

```toml
[daemon.buzz.questions]
enabled = true
provider = "openrouter"             # Defaults to daemon.provider
model = "openai/gpt-5-mini"         # Defaults to daemon.model
allowed_pubkeys = []                # Empty allows any channel member
max_question_bytes = 4096
timeout_secs = 120
```

For example, reply in the thread with `@Fiach why was this rated high
severity?`. Fiach requires the cryptographic Buzz `p` tag produced by a real
mention; matching plain text is not sufficient. It uses the same private key
and pubkey that publish summaries and findings, and replies beneath the
question in the same thread while mentioning the asker.

Questions are matched to the persisted thread root and channel before any model
request. Public threads receive only their public review artifact, while
security-thread questions remain scoped to the configured private channel. The
question model receives the persisted structured review evidence without
shell, reporting, or GitHub disclosure tools. Processed event IDs and the relay
cursor are stored in the review database so reconnects do not duplicate
answers. The Buzz identity must have a profile/channel membership that lets
clients resolve its display name into the correct pubkey.

For the NixOS module, enable the dedicated Buzz options:

```nix
services.fiach = {
  enable = true;
  personas = [ "builtin:pr-review" "builtin:security" ];

  buzz = {
    enable = true;
    relayUrl = "https://buzz.example.com";
    publicChannel = "00000000-0000-0000-0000-000000000001";
    securityChannel = "00000000-0000-0000-0000-000000000002";
    questions.enable = true;
  };
};
```

The module writes `[daemon.buzz]` and defaults Buzz-enabled services without an
explicit `personas` list to the general PR-review and security personas. No
Buzz CLI package is required. Put `FIACH_BUZZ_PRIVATE_KEY` (and the optional
auth-tag variable) in `services.fiach.environmentFile`.

### Structured Reporting and Verification

Fiach uses structured reporting as the default review path for all personas. The finder agent does not directly post to GitHub and does not decide disclosure from Markdown frontmatter. Instead, it submits candidates through in-process reporting tools:

- `submit_finding` records one candidate finding.
- `submit_no_findings` records that the finder found no candidates.

When candidate findings exist and `verify_findings = true`, Fiach runs a separate verifier session. The verifier may use the same provider/model as the finder, or a separate model configured with:

```toml
[daemon]
provider = "openrouter"
model = "google/gemini-3.1-pro-preview"
verifier_provider = "openrouter" # optional
verifier_model = "anthropic/claude-sonnet-4" # optional
```

The verifier reviews all candidates in one pass and submits one `submit_verdict` per finding. Structured verdicts are authoritative for metadata and disclosure. Markdown reports are still written for humans, alongside JSON artifacts:

- `report.md`
- `report.structured.json`
- `report.policy.json`

Workspace preparation materializes at most 16 MiB of the aggregate PR patch in
`.pr_diff.txt`, preventing oversized PRs from consuming unbounded host memory.
When that cap is reached the artifact contains a truncation marker; the agent
can inspect omitted changes one file at a time with `safe_diff.sh`.

For `report_mode = "pr-comment"`, Fiach posts a GitHub PR review only when all host-side policy checks pass:

- the PR is still open,
- the verifier confirmed the finding,
- the verifier marked it as introduced by the PR,
- command transcript evidence is present,
- the comment anchor is valid in the PR diff.

This is the same flow for `builtin:security`, `builtin:pr-review`, and `builtin:code-quality`: persona changes what the finder looks for, while the host-side reporting and disclosure policy stays deterministic. Invalid inline anchors are downgraded into the review summary. Merged or closed PRs never receive PR comments. `report_mode = "sync-pr"` can still publish the local rendered report to the configured sync repository.

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
            
            # The repos to poll
            repos = [ "my-org/core-app" "my-org/website" ];
            
            # Polling settings
            interval = 300;
            filterByUpdated = true;
            updatedWithinDays = 120;
            prStates = [ "open" "merged" ];
            prLimit = 1000;
            maxWorkers = 1;
            verifyFindings = true;
            provider = "openrouter";
            
            # The persona to use (defaults to builtin:security if omitted)
            # Use builtin:pr-review for general non-security PR review.
            persona = "builtin:security";

            # Optional focused lanes inside the persona. These feed one combined report.
            reviewLanes = [ "input-boundaries" "cashu-mint" "state-atomicity" ];
            maxReviewLanes = 2;
            reviewLanePrompts.cashu-mint = ''
              Focus on Cashu mint correctness:
              - quote idempotency
              - blinded signature issuance
              - keyset transitions
              - accounting invariants
              Only report concrete PR-introduced issues.
            '';
            
            # Model to use
            model = "openrouter/anthropic/claude-3-7-sonnet";

            # Optional verifier override. Defaults to provider/model above when unset.
            verifierProvider = "openrouter";
            verifierModel = "anthropic/claude-sonnet-4";
            
            # Disclosure Configuration
            reportMode = "sync-pr";
            syncRepo = "my-org/security-audits";
            
            # Environment file containing host-only GITHUB_TOKEN,
            # read-only FIACH_REVIEW_GITHUB_TOKEN, and the selected provider API key.
            # FIACH_SERVER_TOKEN is optional and protects the local web control API.
            environmentFile = "/run/secrets/fiach-env";

            # Optional tracing filter. Use fiach=debug or fiach=trace when debugging.
            logFilter = "fiach=info,goose=warn,rmcp=warn,sacp=warn,reqwest=warn,hyper=warn";

            # Sandbox Isolation (Highly Recommended)
            # Isolates each PR review inside a systemd-nspawn container.
            sandbox = {
              enable = true;
              
              # Network Mode:
              # - "host" (default): Most reliable service-mode option today. Shares the host network stack.
              # - "bridge": Attach each sandbox to an existing br-nspawn bridge.
              # - "veth": Better namespace isolation. Allows outbound internet but blocks access to
              #   host-local services. This is namespace isolation, not egress filtering, and it does
              #   NOT restrict the sandbox to GitHub/OpenRouter only.
              # - "private": Fully offline.
              networkMode = "veth";
            };
          };
        }
      ];
    };
  };
}
```

### Available Configuration Options

The following options are available under `services.fiach`:

| Option | Type | Default | Description |
|---|---|---|---|
| `enable` | boolean | `false` | Enable the Fiach Daemon. |
| `repos` | list of string | *none* | List of repositories to monitor (e.g., `["org/repo"]`). |
| `port` | integer | `3000` | Port for the interactive web server. |
| `interval` | integer | `300` | Polling interval in seconds. |
| `updatedWithinDays` | integer | `120` | Number of days to look back for updated PRs when `filterByUpdated` is enabled. |
| `filterByUpdated` | boolean | `true` | Whether to include the `updated:>=` GitHub search filter when discovering PRs. Set to `false` to query only by PR state and draft setting. |
| `prStates` | list of string | `["open"]` | List of PR states to poll (e.g., `["open"]`, `["open", "merged"]`). |
| `prLimit` | integer | `1000` | Maximum number of PRs to fetch from GitHub per polling cycle. |
| `skipPrs` | list of string | `[]` | PR numbers or repo-qualified PRs to skip. |
| `allowedAuthorAssociations` | list of string | `["COLLABORATOR", "CONTRIBUTOR", "MEMBER", "OWNER"]` | GitHub PR author associations allowed to trigger daemon reviews. |
| `maxWorkers` | integer | `1` | Maximum number of review jobs to run concurrently per polling query. `0` means unlimited. With multiple personas, each PR/persona pair is a review job. |
| `drafts` | boolean | `false` | Whether to include draft PRs. |
| `provider` | string | `"openrouter"` | Goose provider to use, such as `"openrouter"`, `"anthropic"`, `"openai"`, or `"google"`. |
| `model` | string | `"google/gemini-3.1-pro-preview"` | Model to use with the selected provider. |
| `verifierProvider` | string or null | `null` | Provider to use for the verifier pass. Defaults to `provider` when unset. |
| `verifierModel` | string or null | `null` | Model to use for the verifier pass. Defaults to `model` when unset. |
| `environmentFile` | path | *none* | Path to an environment file containing host-only `GITHUB_TOKEN`, read-only `FIACH_REVIEW_GITHUB_TOKEN` when sandboxing is enabled, the selected provider API key, and optionally `FIACH_SERVER_TOKEN`. |
| `logFilter` | string | `"fiach=info,goose=warn,rmcp=warn,sacp=warn,reqwest=warn,hyper=warn"` | Tracing filter passed to `RUST_LOG` for the daemon and sandboxed review children. |
| `persona` | string | `"builtin:security"` | Single persona source to use (e.g., `"builtin:security"`, `"builtin:pr-review"`, `"builtin:code-quality"`, or an absolute path). |
| `personas` | list of string or null | `null` | Persona sources to run independently for each PR. Takes precedence over `persona`. |
| `reviewLanes` | list of string | `[]` | Focused review lanes to run as Goose subagents inside each persona review before the parent finder submits one combined structured result. |
| `reviewLanePrompts` | attrset of string | `{}` | Custom prompt text keyed by review lane name. Keys are normalized like `reviewLanes` before matching. |
| `maxReviewLanes` | integer | `3` | Maximum number of review lane subagents to run concurrently inside each review. |
| `withSkill` | string or null | `null` | Optional skill name to instruct the agent to use. |
| `reportMode` | enum (`"local"`, `"pr-comment"`, `"sync-pr"`, `"hybrid"`) | `"local"` | Mode for reporting findings. `hybrid` comments on PR-introduced findings and syncs non-PR security findings. |
| `syncRepo` | string or null | `null` | GitHub repository to sync reports to. Required if `reportMode` is `"sync-pr"`, and for non-PR security findings in `hybrid` mode. |
| `notifyOnEmpty` | boolean | `false` | Whether to create PRs or comments even if no findings were found. |
| `verifyFindings` | boolean | `true` | Run a verifier pass before disclosure when findings are present. |
| `timeoutMins` | integer | `30` | Timeout in minutes for each review session. |
| `maxRetries` | integer | `3` | Maximum number of retries for LLM provider failures and failed review attempts. |
| `retryDelaySecs` | integer | `10` | Initial delay in seconds before retrying an LLM failure. |
| `maxCostUsd` | float or null | `null` | Maximum observed budget in USD for each review. Cancels finder, verifier, and duplicate-suppression work at the limit. |
| `inputPricePerM` | float or null | `null` | Override input token price per 1M tokens in USD. |
| `outputPricePerM` | float or null | `null` | Override output token price per 1M tokens in USD. |
| `dataDir` | string | `"/var/lib/fiach"` | Directory to store state database and reports. |
| `contextGroups` | attrset | `{}` | Context groups mapped by target repo (contains `repos` list). |
| `sandbox.enable` | boolean | `false` | Enable Sandboxed PR reviews via systemd-nspawn. |
| `sandbox.networkMode`| enum (`"host"`, `"bridge"`, `"private"`, `"veth"`) | `"veth"` | Network mode for the sandbox. `veth` isolates host-local services while retaining outbound NAT. |
| `sandbox.extraArgs` | list of string | `[]` | Extra arguments to pass to `systemd-nspawn`. |
| `sandbox.memoryMax` | systemd size | `"8G"` | Aggregate memory limit for the daemon and all sandbox workers. |
| `sandbox.cpuQuota` | systemd percent | `"400%"` | Aggregate CPU quota for the daemon and all sandbox workers. |
| `sandbox.tasksMax` | positive integer | `4096` | Aggregate task limit for the daemon and all sandbox workers. |

Cost limits are enforced from provider-reported usage or configured token prices. Because usage is
reported after a model response, a single in-flight response can overshoot the limit. If neither
provider cost nor model pricing is available, Fiach logs that it cannot enforce the limit; set
`inputPricePerM` and `outputPricePerM` to make enforcement deterministic.

### NixOS Sandbox Network Examples

Default isolated veth networking:

```nix
services.fiach = {
  sandbox = {
    enable = true;
    networkMode = "veth";
  };
};
```

Bridge networking for hosts that already provide a `br-nspawn` bridge:

```nix
services.fiach = {
  sandbox = {
    enable = true;
    networkMode = "bridge";
  };
};
```

When using `bridge`, the host must create and maintain `br-nspawn`, addressing, DHCP or static addressing, forwarding, and NAT. Fiach passes `--network-bridge=br-nspawn` to `systemd-nspawn`; it does not create the bridge from the NixOS module.

### Sandbox Networking Limits

`systemd-nspawn` can isolate the sandbox from the host network namespace, but it does not provide destination allowlisting such as "only GitHub and OpenRouter".

- `sandbox.networkMode = "host"` is an explicit compatibility escape hatch that shares host networking.
- `sandbox.networkMode = "bridge"` attaches each sandbox to an existing `br-nspawn` bridge.
- `sandbox.networkMode = "veth"` is the default. It gives each sandbox its own `10.64.<index>.0/30` veth subnet, blocks access to host-local services, and configures NixOS NAT for outbound internet access from the `10.64.0.0/16` sandbox pool.
- `sandbox.networkMode = "private"` disables all network access, which also prevents GitHub and OpenRouter access.
- Restricting outbound traffic to specific destinations requires host-side enforcement such as `nftables`/`iptables` rules on the `ve-*` interfaces, or a proxy-based egress policy.
- IP allowlists can be managed in NixOS firewall configuration, but they are brittle for CDN-backed services.
- If you need domain-level guarantees, use a dedicated proxy or egress gateway; `systemd-nspawn` alone is not sufficient.

`veth` supports concurrent workers by allocating a unique `/30` per active sandbox. Because the allocator uses the `10.64.1.0/30` through `10.64.254.0/30` pool, NixOS deployments using `veth` must set `maxWorkers` between `1` and `254`; `maxWorkers = 0` is rejected for this mode.

### Sandbox Write Scope

In daemon sandbox mode, the sandbox no longer gets write access to the whole `fiach` data directory.

- The sandbox writes only per-review artifacts in a dedicated run directory.
- The sandbox stdout/stderr stream is saved as `reports/runs/<repo>_PR<number>/nspawn.log` by default.
- Review state in `fiach.redb` is recorded by the host daemon after the sandbox exits successfully.
- Disclosure side effects are also performed by the host daemon after it validates the sandbox output.
- GitHub disclosure is journaled before publication and tagged with a hidden stable marker. After an interrupted publication, the host reconciles GitHub history before retrying and reuses an existing review or comment instead of duplicating it.

This reduces the impact of a malicious or prompt-injected agent: it can still produce a bad report, but it cannot directly corrupt the review database from inside the sandbox.

### Domain Skills In Sandbox Mode

`fiach` looks for domain skills in this order:
- `./.agents/skills` in the active review workspace
- `/etc/fiach/skills` packaged into the sandbox rootfs

This lets NixOS/systemd deployments keep using bundled skills such as `rust-security` even when the target PR repository does not contain its own `.agents/skills` directory.

## 🏗 Project Layout

- `src/main.rs`: Clap input parsing and runtime service wiring.
- `src/scheduler.rs`: Bounded FIFO scheduling and process-local job lifecycle.
- `src/execution.rs`: Common local/sandbox executor outcome contract.
- `src/finalizer.rs`: Host-only rendering, disclosure, reactions, and persistence.
- `src/github.rs`: Object-safe GitHub boundary and the production `gh` adapter.
- `src/daemon.rs`: Polling and sandbox/nspawn orchestration.
- `src/review.rs`: Finder, verifier, and duplicate-adjudication agent passes.
- `src/server.rs`: Axum-based interactive web server for daemon management and reporting.
- `src/workspace.rs`: Manages cloning the repo and checking out the PR into a temporary directory.
- `src/disclose.rs`: Handles the `ReportMode` logic (commenting or creating Sync PRs).
- `src/state.rs`: Typed, versioned, async redb state adapter.
