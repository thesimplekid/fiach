# Issue triage and draft fixes

Fiach can watch GitHub issues, apply project-defined labels, mark duplicate issues
or existing PR coverage, and investigate bugs eligible for automatic fixes.
It never closes issues, merges PRs, or adds closing references to generated PRs.

Copy [example.issues.toml](example.issues.toml), set your repositories and areas,
then run:

```sh
fiach --config issues.toml issues             # one pass
fiach --config issues.toml issues --issue 123 # one issue, one configured repository
fiach --config issues.toml issues --watch     # continuous polling
```

`TYPESAFE_API_KEY` and authenticated `gh` are required. The default `publish = false`
prints JSON decisions and saves state without GitHub writes or coding-agent runs.
Set `publish = true` to apply labels and a single bot-owned status comment.
Set up `[issues.worker]` to enable coding; without it, eligible issues receive the
ready label but no coding job starts. `auto_fix = false` enables classification
and marking without coding jobs. Adding `[issues]` to the regular daemon config
runs one sequential issue worker alongside the existing PR-review scheduler.
Issue jobs currently do not appear in the review web dashboard.

The initial pass considers all open issues; subsequent passes reevaluate changed
issue bodies/discussions, changed configuration, and changes to the issue/PR
inventory. `max_items` limits changed issues processed per pass, not evidence:
all inventory pages are collected, closed PRs are discarded, and historical
issues remain available for duplicate checks. Cached issues do not consume the
work limit; polling rotates after a batch so failures cannot monopolize it.
An open PR's update timestamp invalidates earlier decisions even when
it has no linked issue. Bot-authored status comments are excluded from issue
content so the workflow does not trigger itself. One process owns `state_path`;
use the same path across restarts and never run multiple publishers with separate
state files for the same repositories.

Validated Jev responses are saved individually in that database. Comparisons
reuse unchanged evidence across passes and restarts; adding one candidate does
not repeat every older model request. Cache keys include the repository, Jev
endpoint/model, prompts, and evidence. Changed human comments (including edits
and deletions) invalidate affected comparisons; the bot's marked status comments
do not. Discussions are collected in repository-wide pages, including comments
on closed issues. An incomplete discussion scan stops the pass.

If classification fails or exhausts its budget, completed requests remain
cached. Retrying resumes the missing work with a fresh per-issue budget. A
persistent cooldown starts at 60 seconds and doubles to a maximum of one hour;
changed evidence or configuration can bypass it. The normal polling interval
still applies. Partial scans never authorize marking or coding. Keep the state
database across restarts to retain both progress and retry history.

## Classification and policy

Labels have three independent dimensions: kind, project areas (multiple labels
allowed), and next action. Each area has a description, optional path scopes, and
an `auto_fix` permission. Paths use repository-relative Git glob syntax (`*`
within a directory, `**` across directories). They guide Jev during triage and
are enforced by the host against the actual patch before verification. Every
changed file must match an allowed area, and any matching disabled area wins.
Deletes and both sides of renames are checked. Description-only areas are valid
for triage, but automatic fixes require explicit paths for every area.
Every applicable area must permit automation, at least one area
must match, and no area's applicability may be uncertain before coding starts.
Unknown applicability, missing context, and product-direction choices go back to
maintainers. A detailed feature request does not establish project intent.

Duplicate comparisons use both open/closed issues and open PRs. Fiach compares
each entry in the complete inventory; plausible candidates get full discussion,
and PR coverage requires a diff. Definite partial overlap is reported as related
work. Uncertain coverage requires a maintainer decision and prevents a new fix.
A confirmed duplicate is marked and linked, never closed. An open covering PR
gets `already-being-addressed`. Oversized PR diffs are recorded as unresolved
coverage, with the PR number and evidence limit in the explanation. Other comparisons continue; unresolved coverage
requires a maintainer decision unless another candidate establishes a duplicate or
covering PR. It never authorizes an automatic fix. Pagination, command, or Jev
failures stop that attempt instead of interpreting incomplete evidence as permission
to fix. Progress logs identify comparison milestones and candidate PR evidence
fetches.

GitHub candidate details are reused within an inventory pass. PR diffs are reused
only after checking that the PR is still open and both its head and base revisions
match. Every new inventory, including checks before fix publication, clears these
bounded in-memory caches; target-issue reads and publication checks stay fresh.

The issue workflow honors GitHub's rate-limit reset and Retry-After headers. It
stops the pass when quota is exhausted and, in watch mode, waits until requests
are permitted again. Secondary limits without a deadline use increasing backoff.
The wait can be cancelled. One-shot runs report the failure instead of sleeping.
This cooldown applies to issue API calls; the separate PR review poller continues
independently. Inventory is still fetched in full each pass, so this reduces
repeated candidate requests rather than implementing incremental synchronization.

Jev sometimes returns probabilities rounded to hundredths whose total is 0.99
or 1.01. Fiach accepts only drift consistent with half a hundredth per option
when all entries use that precision. For these answers, it subtracts 0.005 from
the selected probability and confidence before applying decision thresholds;
it never scales probabilities upward to force a total of one. Borderline answers
remain uncertain. Missing options, invalid values, and larger or unexplained
discrepancies still stop the attempt with a question-specific diagnostic.

Only the configured managed labels are reconciled. Unrelated human labels remain.
Missing configured labels are created. The bot edits only a comment bearing its
marker **and** authored by the authenticated account. Use a dedicated bot account.

## Automatic fixes

The host clones the default branch before starting an isolated Goose coding
session. The agent must either explain what information/decision is missing, or
return a patch, separate regression-test paths, and a reproduction command.
The host applies the patch to a pristine checkout and rejects protected paths
(hidden top-level paths, Git metadata, agent instructions, and Cargo.lock).

Two fresh containers run the identical regression command: first on the original
code with only regression tests applied (must fail), then on the complete patch
(must pass). A separate Goose session inspects the patch and check evidence and
must approve it. A regression requiring tests and production changes in the same
file needs a maintainer for this first version. Additional build/test tools must
be present in the rootfs; unavailable dependencies stop verification.

Before publishing, Fiach checks issue content again, repeats duplicate/PR checks,
and confirms the default branch has not moved. It creates a deterministic
`fiach/issue-N` branch without overwriting existing branches, then opens a draft
PR. Publication is journaled before pushing. After interruption, it reconciles
existing PRs and validates the saved branch, issue, base, and routing before
finishing publication. Recovery may reconcile an existing PR while automation is
disabled, but cannot create a PR unless `auto_fix = true` and a worker is configured.
Paused publication is retained and can resume after re-enabling automation. The
area policy must match the policy under which the patch was checked; old journals
without that evidence or changed policies require maintainer intervention.
A missing or changed pending branch stops for inspection.
Closed prior bot PRs require a maintainer decision rather than another PR.

Failed/interrupted coding attempts are not repeated for unchanged issue content.
A substantive issue/comment update permits another investigation. Keep the state
file: deleting it loses the attempt history and is not a normal retry mechanism.

The host GitHub credential is never forwarded to issue containers. Coding and
verification containers receive provider keys; regression containers receive no
provider keys. The default `veth` mode uses Fiach's shared subnet allocator and
requires the same host NAT/firewall setup as review sandboxes. `host` networking
is an explicit compatibility option and exposes host-local network services.
No repository code or reproduction command executes on the host.

Jev has a per-pass cost limit; publication rechecks use another pass. Coding has
turn and wall-time limits, not a dollar limit in this version. Set provider-side
spending limits as appropriate. Worker logs, structured reports and transcripts
are retained in `scratch_dir/fiach-artifacts-*`; temporary clones and rootfs copies
are removed. Successful candidate patches and verification reports are also
stored beside the database under `issues.artifacts/OWNER_REPO/NUMBER/`. These
artifacts can contain repository/issue data; apply your own retention policy.

## NixOS

```nix
services.fiach = {
  enable = true;
  repos = [ "owner/repo" ];
  sandbox.enable = true;
  sandbox.networkMode = "veth";
  sandbox.extraPackages = [ pkgs.cargo pkgs.rustc pkgs.pkg-config ];
  issues = {
    publish = true;
    auto_fix = true;
    scratch_dir = "/data/rust/tmp/fiach-issues";
    repos = [{
      repo = "owner/repo";
      areas = [{
        label = "area:core";
        description = "Core domain logic";
        paths = [ "src/core/**" ];
        auto_fix = true;
      }];
    }];
  };
};
```

The module supplies the rootfs and provider/model defaults; override them under
`issues.worker`. Include the repository's pinned toolchain and required system
libraries in `sandbox.extraPackages`. Existing provider/GitHub environment-file
configuration applies. Issue workers share the service's resource limits.
