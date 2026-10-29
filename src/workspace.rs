//! Workspace preparation: clone the repo and checkout the PR branch
//! before the agent starts, so it doesn't waste LLM turns on setup.
//!
//! Forge-specific operations run on the host; sandbox children adopt the
//! prepared checkout and structured inputs without forge credentials.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tempfile::{Builder, TempDir};
use tokio::process::Command;

use crate::process::{CommandExt as _, LONG_COMMAND_TIMEOUT, output_limited};

const MAX_MATERIALIZED_DIFF_BYTES: usize = 16 * 1024 * 1024;
/// PR discussion can be long; this bounds what a sandbox child will parse.
const MAX_STAGED_REVIEW_BYTES: u64 = 32 * 1024 * 1024;

/// A prepared workspace with the PR branch checked out, ready for the agent.
pub struct PreparedWorkspace {
    /// Path to the workspace directory (the agent's working directory).
    pub path: PathBuf,
    /// Owns this execution's directory, including cleanup on drop. Staged
    /// workspaces belong to the host, which removes them.
    directory: Option<TempDir>,
    /// The commit hash of the checked-out PR branch.
    pub commit_hash: String,
    /// The base commit hash the PR was branched from.
    pub base_commit: String,
    pub pr_context: crate::reporting::PrContext,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequestContext {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    state: String,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
}

impl PullRequestContext {
    fn is_merged(&self) -> bool {
        self.state == "MERGED" || self.merged_at.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct RepositoryContext {
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<DefaultBranchRef>,
}

#[derive(Debug, Deserialize)]
struct DefaultBranchRef {
    name: String,
}

impl PreparedWorkspace {
    /// Clean up only the temporary directory owned by this execution.
    pub async fn cleanup(self) -> Result<()> {
        let Some(directory) = self.directory else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || directory.close())
            .await
            .context("Workspace cleanup task failed")?
            .context("Failed to remove workspace directory")
    }

    /// Uses a workspace the host prepared, after checking that its checkout
    /// is still at the staged commit.
    pub async fn adopt(staged: &StagedReview) -> Result<Self> {
        ensure!(
            staged.workspace.is_absolute() && staged.workspace.join(".git").exists(),
            "Staged workspace {} is not a git checkout",
            staged.workspace.display()
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&staged.workspace)
            .output_bounded("resolving the staged review commit")
            .await
            .context("Failed to run `git rev-parse HEAD`")?;
        ensure!(head.status.success(), "git rev-parse HEAD failed");
        ensure!(
            String::from_utf8_lossy(&head.stdout).trim() == staged.commit_hash,
            "Staged workspace is not at the staged commit {}",
            staged.commit_hash
        );
        Ok(Self {
            path: staged.workspace.clone(),
            directory: None,
            commit_hash: staged.commit_hash.clone(),
            base_commit: staged.base_commit.clone(),
            pr_context: staged.pr_context.clone(),
        })
    }
}

/// Pull request text used to pick a domain skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDetails {
    pub title: String,
    pub body: String,
}

pub async fn fetch_pr_details(repo: &str, pr_number: u64) -> Result<PrDetails> {
    let value = crate::forge::client(repo)?
        .adapter()
        .pr_context(repo, pr_number)
        .await?;
    let text = |field: &str| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(PrDetails {
        title: text("title"),
        body: text("body"),
    })
}

/// Everything a sandboxed review child would otherwise fetch from its forge.
/// The host prepares it so the child needs no forge credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedReview {
    /// The checkout, as a path inside the sandbox.
    pub workspace: PathBuf,
    pub commit_hash: String,
    pub base_commit: String,
    pub pr_context: crate::reporting::PrContext,
    /// `None` when an explicit skill makes discovery unnecessary or the
    /// details could not be fetched.
    pub pr_details: Option<PrDetails>,
    /// `None` when deduplication will not run or discussion could not be
    /// fetched; deduplication is then skipped.
    pub existing_comments: Option<Vec<crate::reporting::ExistingPrComment>>,
}

impl StagedReview {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("Reading staged review {}", path.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_STAGED_REVIEW_BYTES,
            "Invalid staged review {}",
            path.display()
        );
        serde_json::from_slice(&std::fs::read(path)?).context("Parsing staged review")
    }
}

fn create_workspace_directory(
    repo: &str,
    pr_number: u64,
    base_dir: Option<&Path>,
) -> Result<TempDir> {
    let safe_name = repo.replace('/', "_");
    let prefix = format!("ctf-review-{safe_name}-PR{pr_number}-");
    let mut builder = Builder::new();
    builder.prefix(&prefix);
    match base_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).context("Failed to create workspace base directory")?;
            builder.tempdir_in(dir)
        }
        None => builder.tempdir(),
    }
    .context("Failed to create unique workspace directory")
}

/// Prepare a workspace by cloning the repo and checking out the PR.
///
/// Steps:
/// 1. Create a temporary workspace directory
/// 2. Clone through the selected forge adapter.
/// 3. Check out the pull request head, including fork PRs.
///
/// The agent will start with its working directory set to the prepared workspace,
/// already on the PR branch — no clone/checkout tool calls needed.
pub async fn prepare(
    repo: &str,
    pr_number: u64,
    base_dir: Option<&Path>,
    context_groups: Option<&crate::config::ContextGroup>,
) -> Result<PreparedWorkspace> {
    // Keep ownership throughout preparation so errors and cancellation also clean up.
    let directory = create_workspace_directory(repo, pr_number, base_dir)?;
    let parent_dir = directory.path();

    let repo_name = repo.split('/').next_back().unwrap_or(repo);
    let workspace_dir = parent_dir.join(repo_name);

    let forge = crate::forge::client(repo)?;
    forge
        .adapter()
        .clone_workspace(repo, pr_number, &workspace_dir)
        .await?;

    if let Some(group) = context_groups {
        for context_repo in &group.repos {
            let target = crate::forge::Repository::parse(context_repo)?;
            let name = target
                .slug
                .split('/')
                .next_back()
                .context("Missing context repository name")?;
            let destination = parent_dir.join(name);
            if let Err(error) = crate::forge::client(context_repo)?
                .clone_repository(context_repo, &destination)
                .await
            {
                tracing::warn!(repo = %context_repo, error = %error, "Failed to clone context repository");
            }
        }
    }

    let pr_context: PullRequestContext =
        serde_json::from_value(forge.adapter().pr_context(repo, pr_number).await?)
            .context("Failed to parse PR context")?;
    let base_commit = pr_context.base_ref_oid.clone();
    let merged = pr_context.is_merged();
    let default_branch = match forge.repository_context(repo).await {
        Ok(value) => serde_json::from_value::<RepositoryContext>(value)?
            .default_branch_ref
            .map(|branch| branch.name)
            .unwrap_or_else(|| pr_context.base_ref_name.clone()),
        Err(error) => {
            tracing::warn!(repo, error = %error, "Could not fetch default branch; using PR base branch");
            pr_context.base_ref_name.clone()
        }
    };

    // 4. Materialize the complete PR diff for prompts that need a single
    // source of truth for changed lines.
    let mut diff_command = Command::new("git");
    diff_command
        .args(["diff", &format!("{base_commit}...HEAD")])
        .current_dir(&workspace_dir);
    let pr_diff_output = output_limited(
        &mut diff_command,
        "generating the pull request diff",
        LONG_COMMAND_TIMEOUT,
        MAX_MATERIALIZED_DIFF_BYTES,
    )
    .await
    .context("Failed to run `git diff` for .pr_diff.txt")?;

    let pr_diff_path = workspace_dir.join(".pr_diff.txt");
    if pr_diff_output.status.success() || pr_diff_output.stdout_truncated {
        let mut diff = pr_diff_output.stdout;
        if pr_diff_output.stdout_truncated {
            diff.extend_from_slice(
                b"\n\n--- FIACH DIFF TRUNCATED AT 16 MIB ---\nUse git diff --name-only and ./safe_diff.sh <file> to inspect omitted files.\n",
            );
            tracing::warn!(
                pr = pr_number,
                limit_bytes = MAX_MATERIALIZED_DIFF_BYTES,
                "Pull request diff exceeded the materialization limit"
            );
        }
        tokio::fs::write(&pr_diff_path, diff)
            .await
            .context("Failed to write .pr_diff.txt")?;
    } else {
        let stderr = String::from_utf8_lossy(&pr_diff_output.stderr);
        tracing::warn!(
            pr = pr_number,
            error = %stderr,
            "Failed to generate .pr_diff.txt; writing an empty diff artifact"
        );
        tokio::fs::write(&pr_diff_path, b"")
            .await
            .context("Failed to write empty .pr_diff.txt")?;
    }

    // 5. Create safe_diff.sh helper script to handle large diffs
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

# Materialize one file at a time without storing the diff in a shell variable.
TMP_DIFF=$(mktemp)
trap 'rm -f "$TMP_DIFF"' EXIT
git diff "$BASE_BRANCH"...HEAD -- "$FILE" > "$TMP_DIFF"
TOTAL_LINES=$(wc -l < "$TMP_DIFF")

if [ "$TOTAL_LINES" -eq 0 ]; then
    echo "No changes found in $FILE"
    exit 0
fi

if [ "$TOTAL_LINES" -le "$LINES_PER_PAGE" ]; then
    cat "$TMP_DIFF"
else
    TOTAL_PAGES=$(( (TOTAL_LINES + LINES_PER_PAGE - 1) / LINES_PER_PAGE ))
    START_LINE=$(( (PAGE - 1) * LINES_PER_PAGE + 1 ))
    END_LINE=$(( PAGE * LINES_PER_PAGE ))
    
    echo "--- DIFF TOO LARGE ($TOTAL_LINES lines) ---"
    echo "Showing PAGE $PAGE of $TOTAL_PAGES (lines $START_LINE to $END_LINE)"
    echo "To see the next page, run: ./safe_diff.sh $FILE $((PAGE + 1))"
    echo "-------------------------------------------"
    sed -n "${START_LINE},${END_LINE}p" "$TMP_DIFF"
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
        .output_bounded("making safe_diff.sh executable")
        .await
        .context("Failed to make safe_diff.sh executable")?;

    if !chmod_output.status.success() {
        let stderr = String::from_utf8_lossy(&chmod_output.stderr);
        bail!("chmod +x safe_diff.sh failed: {stderr}");
    }

    // 6. Get the commit hash
    let rev_parse_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&workspace_dir)
        .output_bounded("resolving the review commit")
        .await
        .context("Failed to run `git rev-parse HEAD`")?;

    if !rev_parse_output.status.success() {
        let stderr = String::from_utf8_lossy(&rev_parse_output.stderr);
        bail!("git rev-parse HEAD failed: {stderr}");
    }

    let commit_hash = String::from_utf8_lossy(&rev_parse_output.stdout)
        .trim()
        .to_string();

    ensure!(
        commit_hash == pr_context.head_ref_oid,
        "PR head changed during workspace preparation; retry the review"
    );
    tracing::debug!(path = %workspace_dir.display(), commit = %commit_hash, "Workspace ready");

    Ok(PreparedWorkspace {
        path: workspace_dir,
        directory: Some(directory),
        commit_hash: commit_hash.clone(),
        base_commit,
        pr_context: crate::reporting::PrContext {
            state: pr_context.state,
            merged,
            base_ref_name: pr_context.base_ref_name,
            default_branch,
            base_commit: pr_context.base_ref_oid,
            head_commit: commit_hash,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(directory: TempDir) -> PreparedWorkspace {
        PreparedWorkspace {
            path: directory.path().join("repo"),
            directory: Some(directory),
            commit_hash: "head".to_string(),
            base_commit: "base".to_string(),
            pr_context: crate::reporting::PrContext {
                state: "OPEN".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: "head".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn overlapping_reviews_own_independent_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("ctf-review-owner_repo-PR42");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("sentinel"), "legacy").unwrap();

        let first =
            prepared(create_workspace_directory("owner/repo", 42, Some(root.path())).unwrap());
        let first_path = first.directory.as_ref().unwrap().path().to_path_buf();
        std::fs::write(first_path.join("sentinel"), "first").unwrap();
        let second =
            prepared(create_workspace_directory("owner/repo", 42, Some(root.path())).unwrap());
        let second_path = second.directory.as_ref().unwrap().path().to_path_buf();
        std::fs::write(second_path.join("sentinel"), "second").unwrap();

        assert_ne!(first_path, second_path);
        assert_eq!(
            std::fs::read_to_string(first_path.join("sentinel")).unwrap(),
            "first"
        );
        first.cleanup().await.unwrap();
        assert!(!first_path.exists());
        assert_eq!(
            std::fs::read_to_string(second_path.join("sentinel")).unwrap(),
            "second"
        );
        drop(second);
        assert!(!second_path.exists());
        assert_eq!(
            std::fs::read_to_string(legacy.join("sentinel")).unwrap(),
            "legacy"
        );
    }

    #[test]
    fn workspace_base_can_be_created_but_cannot_be_a_file() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("new/base");
        let directory = create_workspace_directory("owner/repo", 42, Some(&nested)).unwrap();
        assert_eq!(directory.path().parent(), Some(nested.as_path()));

        let file = root.path().join("file");
        std::fs::write(&file, "preserve").unwrap();
        assert!(create_workspace_directory("owner/repo", 42, Some(&file)).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "preserve");
    }

    async fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn staged(workspace: PathBuf, commit_hash: String) -> StagedReview {
        StagedReview {
            workspace,
            commit_hash: commit_hash.clone(),
            base_commit: "base".to_string(),
            pr_context: crate::reporting::PrContext {
                state: "OPEN".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: commit_hash,
            },
            pr_details: Some(PrDetails {
                title: "Fix".to_string(),
                body: "Body".to_string(),
            }),
            existing_comments: None,
        }
    }

    #[tokio::test]
    async fn adopting_a_staged_workspace_checks_its_commit_and_keeps_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]).await;
        std::fs::write(repo.join("file"), "x").unwrap();
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-m", "head"]).await;
        let head = git(&repo, &["rev-parse", "HEAD"]).await;

        let adopted = PreparedWorkspace::adopt(&staged(repo.clone(), head.clone()))
            .await
            .unwrap();
        assert_eq!(adopted.commit_hash, head);
        assert_eq!(adopted.pr_context.head_commit, head);
        adopted.cleanup().await.unwrap();
        assert!(
            repo.join("file").exists(),
            "the host owns staged workspaces"
        );

        let moved = PreparedWorkspace::adopt(&staged(repo.clone(), "0".repeat(40))).await;
        assert!(moved.is_err());
        let relative = PreparedWorkspace::adopt(&staged(PathBuf::from("repo"), head)).await;
        assert!(relative.is_err());
    }

    #[test]
    fn staged_review_round_trips_and_rejects_unknown_fields() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("review.json");
        let review = staged(PathBuf::from("/review/repo"), "head".to_string());
        std::fs::write(&path, serde_json::to_vec(&review).unwrap()).unwrap();
        let loaded = StagedReview::load(&path).unwrap();
        assert_eq!(loaded.workspace, review.workspace);
        assert_eq!(loaded.pr_details, review.pr_details);

        let mut value = serde_json::to_value(&review).unwrap();
        value["github_token"] = serde_json::json!("x");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(StagedReview::load(&path).is_err());

        let link = root.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(StagedReview::load(&link).is_err());
    }

    #[test]
    fn pull_request_context_derives_merged_from_merged_at() {
        let unmerged = PullRequestContext {
            head_ref_oid: "head".to_owned(),
            base_ref_oid: "base".to_string(),
            base_ref_name: "main".to_string(),
            state: "CLOSED".to_string(),
            merged_at: None,
        };
        let merged = PullRequestContext {
            merged_at: Some("2026-06-15T17:09:12Z".to_string()),
            ..unmerged.clone()
        };

        assert!(!unmerged.is_merged());
        assert!(merged.is_merged());
    }
}
