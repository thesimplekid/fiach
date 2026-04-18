//! Workspace preparation: clone the repo and checkout the PR branch
//! before the agent starts, so it doesn't waste LLM turns on setup.
//!
//! This mirrors the pattern from `ctf-pr-reviewer/review.sh` where the shell
//! script clones the repo, copies it to a per-PR workspace, and runs
//! `gh pr checkout` before launching the AI agent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// A prepared workspace with the PR branch checked out, ready for the agent.
pub struct PreparedWorkspace {
    /// Path to the workspace directory (the agent's working directory).
    pub path: PathBuf,
    /// Path to the parent directory (which will be cleaned up).
    pub parent_dir: PathBuf,
    /// Whether this is a temporary directory we created (should be cleaned up).
    pub is_temp: bool,
    /// The commit hash of the checked-out PR branch.
    pub commit_hash: String,
    /// The base commit hash the PR was branched from.
    pub base_commit: String,
}

impl Drop for PreparedWorkspace {
    fn drop(&mut self) {
        if self.is_temp && self.parent_dir.exists() {
            tracing::info!(path = %self.parent_dir.display(), "Cleaning up workspace on drop");
            if let Err(e) = std::fs::remove_dir_all(&self.parent_dir) {
                tracing::error!("Failed to remove workspace directory: {}", e);
            }
        }
    }
}

impl PreparedWorkspace {
    /// Clean up the workspace if it was a temporary directory.
    pub async fn cleanup(mut self) -> Result<()> {
        if self.is_temp && self.parent_dir.exists() {
            tracing::info!(path = %self.parent_dir.display(), "Cleaning up workspace");
            tokio::fs::remove_dir_all(&self.parent_dir)
                .await
                .context("Failed to remove workspace directory")?;
            self.is_temp = false; // Prevent double cleanup on drop
        }
        Ok(())
    }
}

/// Prepare a workspace by cloning the repo and checking out the PR.
///
/// Steps:
/// 1. Create a temporary workspace directory
/// 2. `gh repo clone {repo} {workspace_dir}`
/// 3. `gh pr checkout {pr_number}` inside the workspace
///
/// The agent will start with its working directory set to the prepared workspace,
/// already on the PR branch — no clone/checkout tool calls needed.
pub async fn prepare(
    repo: &str,
    pr_number: u64,
    base_dir: Option<&Path>,
    context_groups: Option<&crate::config::ContextGroup>,
) -> Result<PreparedWorkspace> {
    let safe_name = repo.replace('/', "_");
    let parent_dir = match base_dir {
        Some(dir) => dir.join(format!("ctf-review-{safe_name}-PR{pr_number}")),
        None => std::env::temp_dir().join(format!("ctf-review-{safe_name}-PR{pr_number}")),
    };

    // Clean up any stale workspace from a previous run
    if parent_dir.exists() {
        tracing::debug!(path = %parent_dir.display(), "Removing stale workspace");
        tokio::fs::remove_dir_all(&parent_dir)
            .await
            .context("Failed to remove stale workspace")?;
    }

    std::fs::create_dir_all(&parent_dir).with_context(|| {
        format!(
            "Failed to create workspace parent directory at {}",
            parent_dir.display()
        )
    })?;

    let repo_name = repo.split('/').next_back().unwrap_or(repo);
    let workspace_dir = parent_dir.join(repo_name);

    // 1. Clone the main repo
    tracing::debug!(repo = %repo, path = %workspace_dir.display(), "Cloning target repository");
    let clone_output = Command::new("gh")
        .args(["repo", "clone", repo])
        .arg(&workspace_dir)
        .output()
        .await
        .context("Failed to run `gh repo clone` — is `gh` installed?")?;

    if !clone_output.status.success() {
        let stderr = String::from_utf8_lossy(&clone_output.stderr);
        bail!("gh repo clone failed: {stderr}");
    }

    // Clone context groups
    if let Some(ctx_group) = context_groups {
        for ctx_repo in &ctx_group.repos {
            let ctx_repo_name = ctx_repo.split('/').next_back().unwrap_or(ctx_repo);
            let ctx_dir = parent_dir.join(ctx_repo_name);
            tracing::debug!(repo = %ctx_repo, path = %ctx_dir.display(), "Cloning context repository");
            let ctx_clone_output = Command::new("gh")
                .args(["repo", "clone", ctx_repo])
                .arg(&ctx_dir)
                .output()
                .await
                .context("Failed to clone context repository")?;

            if !ctx_clone_output.status.success() {
                let stderr = String::from_utf8_lossy(&ctx_clone_output.stderr);
                tracing::warn!("Failed to clone context repo {}: {}", ctx_repo, stderr);
            }
        }
    }

    // 2. Checkout the PR branch
    tracing::debug!(pr = pr_number, "Checking out PR branch");
    let checkout_output = Command::new("gh")
        .args(["pr", "checkout", &pr_number.to_string()])
        .current_dir(&workspace_dir)
        .output()
        .await
        .context("Failed to run `gh pr checkout`")?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        bail!("gh pr checkout failed: {stderr}");
    }

    // 3. Get the base commit hash (baseRefOid)
    tracing::debug!(pr = pr_number, "Getting base commit hash");
    let base_branch_output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "baseRefOid",
            "--jq",
            ".baseRefOid",
        ])
        .current_dir(&workspace_dir)
        .output()
        .await
        .context("Failed to run `gh pr view`")?;

    if !base_branch_output.status.success() {
        let stderr = String::from_utf8_lossy(&base_branch_output.stderr);
        bail!("gh pr view failed: {stderr}");
    }

    let base_commit = String::from_utf8_lossy(&base_branch_output.stdout)
        .trim()
        .to_string();

    // 4. Create safe_diff.sh helper script to handle large diffs
    let safe_diff_script = r#"#!/usr/bin/env bash
FILE=$1
PAGE=${2:-1}
LINES_PER_PAGE=1500

if [ -z "$FILE" ]; then
    echo "Usage: ./safe_diff.sh <file_path> [page_number]"
    exit 1
fi

# Get the base commit we are comparing against.
BASE_BRANCH="${BASE_BRANCH:-main}"

# Generate the full diff
FULL_DIFF=$(git diff $BASE_BRANCH..HEAD -- "$FILE")
TOTAL_LINES=$(echo "$FULL_DIFF" | wc -l)

if [ "$TOTAL_LINES" -eq 0 ]; then
    echo "No changes found in $FILE"
    exit 0
fi

if [ "$TOTAL_LINES" -le "$LINES_PER_PAGE" ]; then
    echo "$FULL_DIFF"
else
    TOTAL_PAGES=$(( (TOTAL_LINES + LINES_PER_PAGE - 1) / LINES_PER_PAGE ))
    START_LINE=$(( (PAGE - 1) * LINES_PER_PAGE + 1 ))
    END_LINE=$(( PAGE * LINES_PER_PAGE ))
    
    echo "--- DIFF TOO LARGE ($TOTAL_LINES lines) ---"
    echo "Showing PAGE $PAGE of $TOTAL_PAGES (lines $START_LINE to $END_LINE)"
    echo "To see the next page, run: ./safe_diff.sh $FILE $((PAGE + 1))"
    echo "-------------------------------------------"
    echo "$FULL_DIFF" | sed -n "${START_LINE},${END_LINE}p"
fi
"#;

    let safe_diff_path = workspace_dir.join("safe_diff.sh");
    tokio::fs::write(&safe_diff_path, safe_diff_script)
        .await
        .context("Failed to write safe_diff.sh script")?;

    // Make script executable
    let chmod_output = Command::new("chmod")
        .args(["+x", "safe_diff.sh"])
        .current_dir(&workspace_dir)
        .output()
        .await
        .context("Failed to make safe_diff.sh executable")?;

    if !chmod_output.status.success() {
        let stderr = String::from_utf8_lossy(&chmod_output.stderr);
        bail!("chmod +x safe_diff.sh failed: {stderr}");
    }

    // 5. Get the commit hash
    let rev_parse_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&workspace_dir)
        .output()
        .await
        .context("Failed to run `git rev-parse HEAD`")?;

    if !rev_parse_output.status.success() {
        let stderr = String::from_utf8_lossy(&rev_parse_output.stderr);
        bail!("git rev-parse HEAD failed: {stderr}");
    }

    let commit_hash = String::from_utf8_lossy(&rev_parse_output.stdout)
        .trim()
        .to_string();

    tracing::debug!(path = %workspace_dir.display(), commit = %commit_hash, "Workspace ready");

    Ok(PreparedWorkspace {
        path: workspace_dir,
        parent_dir,
        is_temp: true,
        commit_hash,
        base_commit,
    })
}
