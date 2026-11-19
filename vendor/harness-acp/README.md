# harness-acp

A shared Rust adapter for bounded application tasks using Goose over ACP.
The initial backend starts a pinned `goose acp` process. `Launcher`,
`Connection`, and `Transport` separate its lifecycle from the protocol so a
future VM launcher can supply the same byte stream.

## Contract

Call `run(&LaunchSpec, &Task, CancellationToken)` for one fresh conversation.
There is no automatic reconnect, retry, or replay. Applications own phase
scheduling, durable state, domain validation, and publication.

The adapter negotiates ACP v1, encodes an explicit Goose recipe, creates a
session, and prompts it. An output schema requires a successful `final_output`
tool response linked to its exact call ID, followed by host schema validation.
Arbitrary assistant JSON and an `end_turn` stop reason alone are not accepted
as structured results. A task without a schema returns visible assistant text.

`RunResult` distinguishes completion, cancellation, timeout, invalid output,
provider/agent errors, protocol errors, and launch errors. It retains bounded
partial evidence on failure. `usage` is optional: missing information is not
zero. Goose's custom `accumulatedInputTokens`, `accumulatedOutputTokens`, and
`accumulatedCost` fields are session totals. Standard `used` is context usage;
`promptUsage` is the prompt response's usage and must not be treated as the
sum of all provider calls. Upstream totals may themselves be incomplete when a
provider omits usage. `run_observed` reports sanitized updates; an application
may cancel its token when an observed budget is exhausted. This is not a
provider-side spending reservation.

## Execution boundary

The subprocess inherits only `LaunchSpec.env`. Supply an explicit `PATH` if
tools require executable lookup, provider credentials, and any required CA
configuration. Declarative provider definitions are copied into private task
configuration. The launcher overrides HOME, XDG paths, and GOOSE_PATH_ROOT,
disables keyring access, and disables the implicit developer extension.
Recipes explicitly select extensions; no configured or project plugin MCP
servers are inherited. Client filesystem, terminal, and approval capabilities
are refused. Explicitly exposed tools still run with the subprocess's OS
permissions: this library is not an OS sandbox.

Goose also registers internal operations independently of extensions. In
particular, `load_skill` remains advertised, and project plugins can supply
hooks. **Restricted callers must use a fresh empty private `Task.cwd` and root
their inspection MCP handlers in the separate repository checkout.** An
extension allowlist does not disable project hooks. The private HOME and empty
agent working directory prevent discovery of filesystem skills and project
hooks; built-in skill guidance remains available. These discovery paths use
the exact working directory, not ancestor directories. The pinned Goose
conformance test also verifies that an absolute skill name cannot read an
arbitrary host file. Callers intentionally exposing developer tools may
choose the real checkout as cwd and must provide their own OS confinement.

`Session::connect` and `Session::prompt` support follow-up prompts in the same
conversation. Instructions, schema, and recipe settings are fixed at session
creation. An application can narrow its MCP handler's dispatch policy between
turns. Any failed or dropped prompt closes the session rather than reconnecting
or replaying it. Usage in successive results is cumulative; compute deltas.

Use `McpBridge` for an authenticated loopback endpoint backed by application
tools. It rejects unregistered tools and invalid arguments before dispatch.
Each callback must enforce its own path and side-effect restrictions. Its
extension descriptor contains a secret bearer credential and must not be
logged. For VM execution the bridge endpoint must be placed or forwarded
deliberately; unrestricted host tools would escape the intended VM boundary.

Cancellation sends `session/cancel`, waits a bounded grace period, then kills
and reaps the owned process group. Dropping execution also kills that group.
External services and tools that deliberately detach into a new process group
require an OS sandbox/cgroup for stronger lifetime guarantees.

## Evidence

Transcripts contain allowed visible ACP updates only, excluding thought
chunks. Known credential environment values and extension headers are
redacted. Raw process stderr is drained without retention because provider
logs may contain credentials or hidden reasoning. The accepted structured
output retains its original values for application validation; the host must
apply its disclosure/redaction policy before publishing or storing it in an
unrestricted artifact. Sanitization does not attempt to recognize every
possible secret embedded in repository contents.

## Development

`nix develop -c just final-check` runs formatting, Clippy, deterministic
protocol tests, the real pinned Goose executable against a loopback mock
provider, and Nix checks. No paid model requests are made. The standalone
conformance test requires `GOOSE_TEST_BINARY` and runs with:

```console
cargo test --test goose -- --ignored
```

The supported Goose revision is
`bf074d6d114690de6cbe7edc925cb26efdf6f16a` (1.60.0). Recipe encoding and custom
usage notifications are Goose-specific unstable ACP extensions; upgrade the
binary and protocol fixtures together.

## Distribution

This directory is the canonical local JJ repository for the shared library.
Until a remote release location is chosen, each consumer carries a reproducible
source snapshot in `vendor/harness-acp`. Cargo and Nix builds need no sibling
checkout. Do not patch the copies independently.

Refresh a consumer after changing and verifying this repository:

```console
nix develop -c python3 scripts/vendor.py ../fiach/vendor/harness-acp
nix develop -c python3 scripts/vendor.py ../vdh/vendor/harness-acp
nix develop -c python3 scripts/vendor.py ../vuln_review/vendor/harness-acp
```

Each snapshot records the canonical revision and file hashes in `SNAPSHOT.json`.
The same command with `--check` verifies the copy against the canonical source.
This explicit source-copy helper is needed while there is no published Git or
registry dependency; it never modifies user configuration or installs tools.
