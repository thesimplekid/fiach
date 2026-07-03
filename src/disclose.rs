use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::process::Command;

use crate::reporting::{DisclosurePolicy, InlineComment, ReportingArtifact};

const PR_REACTION_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, clap::ValueEnum, Default, serde::Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ReportMode {
    #[default]
    Local,
    PrComment,
    SyncPr,
    Hybrid,
}

impl std::fmt::Display for ReportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::PrComment => write!(f, "pr-comment"),
            Self::SyncPr => write!(f, "sync-pr"),
            Self::Hybrid => write!(f, "hybrid"),
        }
    }
}

impl std::str::FromStr for ReportMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" => Ok(ReportMode::Local),
            "pr-comment" => Ok(ReportMode::PrComment),
            "sync-pr" => Ok(ReportMode::SyncPr),
            "hybrid" => Ok(ReportMode::Hybrid),
            _ => Err(format!("Invalid report mode: {}", s)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscloseConfig {
    pub mode: ReportMode,
    pub sync_repo: Option<String>,
    pub notify_on_empty: bool,
    pub reactions: ReactionConfig,
}

#[derive(Debug, Clone)]
pub struct ReactionConfig {
    pub review_start: Option<String>,
    pub no_findings: Option<String>,
}

impl ReactionConfig {
    pub fn with_defaults(review_start: Option<String>, no_findings: Option<String>) -> Self {
        let defaults = Self::default();
        Self {
            review_start: review_start.or(defaults.review_start),
            no_findings: no_findings.or(defaults.no_findings),
        }
    }
}

impl Default for ReactionConfig {
    fn default() -> Self {
        Self {
            review_start: Some("eyes".to_string()),
            no_findings: Some("+1".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct ExistingSyncPr {
    number: u64,
    url: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct DisclosureTarget<'a> {
    pub repo: &'a str,
    pub pr_number: u64,
    pub commit_hash: &'a str,
    pub review_kind: &'a str,
}

pub async fn handle_disclosure(
    report_path: &Path,
    target: DisclosureTarget<'_>,
    findings_found: bool,
    config: &DiscloseConfig,
) -> Result<Option<String>> {
    match config.mode {
        ReportMode::Local => {
            tracing::info!("ReportMode is Local. Report saved to {:?}", report_path);
            Ok(Some(report_path.to_string_lossy().to_string()))
        }
        ReportMode::PrComment => {
            if !findings_found && !config.notify_on_empty {
                tracing::info!(
                    "No findings found and notify_on_empty is false. Skipping PR comment."
                );
                return Ok(None);
            }
            post_pr_comment(report_path, target.repo, target.pr_number)
                .await
                .map(Some)
        }
        ReportMode::SyncPr => {
            if !findings_found && !config.notify_on_empty {
                tracing::info!("No findings found and notify_on_empty is false. Skipping Sync PR.");
                return Ok(None);
            }

            let sync_repo = config
                .sync_repo
                .as_ref()
                .context("sync_repo must be provided for SyncPr mode")?;
            create_sync_pr(
                report_path,
                target.repo,
                target.pr_number,
                target.commit_hash,
                target.review_kind,
                sync_repo,
            )
            .await
            .map(Some)
        }
        ReportMode::Hybrid => {
            if !findings_found && !config.notify_on_empty {
                tracing::info!(
                    "No findings found and notify_on_empty is false. Skipping hybrid disclosure."
                );
                return Ok(None);
            }
            if policy_comments_allowed(target.review_kind) {
                post_pr_comment(report_path, target.repo, target.pr_number)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            }
        }
    }
}

pub async fn handle_structured_disclosure(
    report_path: &Path,
    target: DisclosureTarget<'_>,
    artifact: &ReportingArtifact,
    policy: &DisclosurePolicy,
    config: &DiscloseConfig,
) -> Result<Option<String>> {
    match structured_disclosure_route(target, artifact, policy, config) {
        StructuredDisclosureRoute::Local => {
            tracing::info!("ReportMode is Local. Report saved to {:?}", report_path);
            Ok(Some(report_path.to_string_lossy().to_string()))
        }
        StructuredDisclosureRoute::Skip => Ok(None),
        StructuredDisclosureRoute::PrReview => {
            let accepted = artifact.publishable_findings(policy);
            let comments = accepted
                .iter()
                .flat_map(|finding| finding.inline_comments.clone())
                .collect::<Vec<_>>();
            let body = crate::reporting::review_summary_body(&accepted);
            post_pr_review(target.repo, target.pr_number, &body, comments)
                .await
                .map(Some)
        }
        StructuredDisclosureRoute::SyncPr => {
            let sync_repo = config
                .sync_repo
                .as_ref()
                .context("sync_repo must be provided for SyncPr mode")?;
            create_sync_pr(
                report_path,
                target.repo,
                target.pr_number,
                target.commit_hash,
                target.review_kind,
                sync_repo,
            )
            .await
            .map(Some)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredDisclosureRoute {
    Local,
    Skip,
    PrReview,
    SyncPr,
}

fn structured_disclosure_route(
    target: DisclosureTarget<'_>,
    artifact: &ReportingArtifact,
    policy: &DisclosurePolicy,
    config: &DiscloseConfig,
) -> StructuredDisclosureRoute {
    let accepted_pr_findings = artifact.publishable_findings(policy);
    let duplicate_only =
        accepted_pr_findings.is_empty() && !artifact.already_reported_findings(policy).is_empty();
    let findings_found = !accepted_pr_findings.is_empty() && !artifact.verifier_failed;

    match config.mode {
        ReportMode::Local => StructuredDisclosureRoute::Local,
        ReportMode::PrComment => {
            if duplicate_only {
                tracing::info!(
                    "All verified PR findings were already reported; skipping PR review"
                );
                return StructuredDisclosureRoute::Skip;
            }
            if !findings_found && !config.notify_on_empty {
                tracing::info!("No verified findings approved for disclosure; skipping PR review");
                return StructuredDisclosureRoute::Skip;
            }
            if !policy.pr_context.comments_allowed() {
                tracing::info!(
                    state = %policy.pr_context.state,
                    merged = policy.pr_context.merged,
                    "PR lifecycle state does not allow comments; skipping PR disclosure"
                );
                return StructuredDisclosureRoute::Skip;
            }

            StructuredDisclosureRoute::PrReview
        }
        ReportMode::SyncPr => {
            if !findings_found && !config.notify_on_empty {
                tracing::info!("No verified findings approved for disclosure; skipping Sync PR");
                return StructuredDisclosureRoute::Skip;
            }

            StructuredDisclosureRoute::SyncPr
        }
        ReportMode::Hybrid => {
            if artifact.verifier_failed {
                tracing::info!("Verifier failed; skipping hybrid disclosure");
                return StructuredDisclosureRoute::Skip;
            }

            if duplicate_only {
                tracing::info!(
                    "All verified PR findings were already reported; skipping hybrid PR disclosure"
                );
                return StructuredDisclosureRoute::Skip;
            }

            if !accepted_pr_findings.is_empty() {
                if policy.pr_context.comments_allowed() {
                    return StructuredDisclosureRoute::PrReview;
                }
                tracing::info!(
                    state = %policy.pr_context.state,
                    merged = policy.pr_context.merged,
                    "PR lifecycle state does not allow comments; skipping PR-introduced hybrid disclosure"
                );
            }

            let non_pr_findings = artifact
                .confirmed_findings_including_non_pr(policy)
                .into_iter()
                .filter(|finding| !finding.verdict.introduced_by_pr)
                .count();

            if is_security_review_kind(target.review_kind) && non_pr_findings > 0 {
                StructuredDisclosureRoute::SyncPr
            } else if config.notify_on_empty && policy.pr_context.comments_allowed() {
                StructuredDisclosureRoute::PrReview
            } else {
                StructuredDisclosureRoute::Skip
            }
        }
    }
}

fn is_security_review_kind(review_kind: &str) -> bool {
    review_kind == "security" || review_kind == "builtin:security"
}

fn policy_comments_allowed(review_kind: &str) -> bool {
    is_security_review_kind(review_kind)
        || review_kind == "pr-review"
        || review_kind == "builtin:pr-review"
}

#[derive(Serialize)]
struct ReviewCommentPayload {
    path: String,
    line: u32,
    side: &'static str,
    body: String,
}

#[derive(Serialize)]
struct ReviewPayload {
    event: &'static str,
    body: String,
    comments: Vec<ReviewCommentPayload>,
}

async fn post_pr_review(
    repo: &str,
    pr_number: u64,
    body: &str,
    comments: Vec<InlineComment>,
) -> Result<String> {
    tracing::info!(
        repo = %repo,
        pr = pr_number,
        comments = comments.len(),
        "Posting structured PR review"
    );

    let payload = ReviewPayload {
        event: "COMMENT",
        body: body.to_string(),
        comments: comments
            .into_iter()
            .map(|comment| ReviewCommentPayload {
                path: comment.path,
                line: comment.line,
                side: "RIGHT",
                body: comment.body,
            })
            .collect(),
    };

    let input = tempfile::NamedTempFile::new().context("Failed to create review payload file")?;
    std::fs::write(input.path(), serde_json::to_vec(&payload)?)
        .context("Failed to write review payload")?;

    let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews");
    let output = Command::new("gh")
        .args(["api", &endpoint, "--method", "POST", "--input"])
        .arg(input.path())
        .output()
        .await
        .context("Failed to run `gh api` for PR review")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api PR review failed: {stderr}");
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR review response")?;
    let url = value
        .get("html_url")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    tracing::info!("Successfully posted structured PR review: {}", url);
    Ok(url)
}

pub async fn post_pr_reaction(repo: &str, pr_number: u64, reaction: &str) -> Result<()> {
    let content = normalize_reaction_content(reaction)?;
    tracing::info!(
        repo = %repo,
        pr = pr_number,
        reaction = content,
        "Posting PR reaction"
    );

    let endpoint = format!("repos/{repo}/issues/{pr_number}/reactions");
    let mut cmd = Command::new("gh");
    cmd.kill_on_drop(true)
        .args([
            "api",
            &endpoint,
            "--method",
            "POST",
            "-H",
            "Accept: application/vnd.github+json",
            "-f",
        ])
        .arg(format!("content={content}"));

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(PR_REACTION_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .context("Timed out posting PR reaction")?
    .context("Failed to run `gh api` for PR reaction")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api PR reaction failed: {stderr}");
    }

    tracing::info!(
        repo = %repo,
        pr = pr_number,
        reaction = content,
        "Posted PR reaction"
    );

    Ok(())
}

/// Review outcomes that mean "nothing actionable" to the person who
/// triggered the review by mention.
pub fn is_non_actionable_status(status: &str) -> bool {
    matches!(status, "none" | "rejected" | "already-reported")
}

/// Reacts to a specific comment or review via its GraphQL node id, so the
/// person who @-mentioned the trigger account can see the bot picked it up.
pub async fn post_mention_reaction(subject_node_id: &str, reaction: &str) -> Result<()> {
    mutate_mention_reaction(subject_node_id, reaction, "addReaction").await
}

/// Removes the bot's own reaction from a comment or review.
pub async fn remove_mention_reaction(subject_node_id: &str, reaction: &str) -> Result<()> {
    mutate_mention_reaction(subject_node_id, reaction, "removeReaction").await
}

/// Posts the final outcome reaction on the trigger mention comment and clears
/// the review-start reaction, so the comment shows only the outcome.
pub async fn finalize_mention_reaction(
    subject_node_id: &str,
    outcome_reaction: &str,
    review_start_reaction: Option<&str>,
) -> Result<()> {
    post_mention_reaction(subject_node_id, outcome_reaction).await?;

    if let Some(start_reaction) = review_start_reaction
        && should_clear_start_reaction(start_reaction, outcome_reaction)
        && let Err(error) = remove_mention_reaction(subject_node_id, start_reaction).await
    {
        // The outcome reaction is already up; a leftover start reaction is
        // cosmetic, so do not fail the caller over it.
        tracing::warn!(
            subject = %subject_node_id,
            error = %error,
            "Failed to remove review-start reaction from trigger mention comment"
        );
    }

    Ok(())
}

/// Never remove the start reaction when it uses the same emoji as the
/// outcome, or we would delete the reaction we just posted.
fn should_clear_start_reaction(review_start: &str, outcome: &str) -> bool {
    match (
        normalize_reaction_content(review_start),
        normalize_reaction_content(outcome),
    ) {
        (Ok(start), Ok(outcome)) => start != outcome,
        _ => false,
    }
}

async fn mutate_mention_reaction(
    subject_node_id: &str,
    reaction: &str,
    mutation: &str,
) -> Result<()> {
    let content = graphql_reaction_content(reaction)?;
    tracing::info!(
        subject = %subject_node_id,
        reaction = content,
        mutation = mutation,
        "Updating trigger mention reaction"
    );

    let query = format!(
        "mutation($subjectId: ID!, $content: ReactionContent!) {{ \
         {mutation}(input: {{subjectId: $subjectId, content: $content}}) {{ \
         clientMutationId }} }}"
    );
    let mut cmd = Command::new("gh");
    cmd.kill_on_drop(true)
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .args(["-f", &format!("subjectId={subject_node_id}")])
        .args(["-f", &format!("content={content}")]);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(PR_REACTION_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .context("Timed out updating trigger mention reaction")?
    .context("Failed to run `gh api graphql` for trigger mention reaction")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api graphql {mutation} failed: {stderr}");
    }

    Ok(())
}

fn graphql_reaction_content(reaction: &str) -> Result<&'static str> {
    Ok(match normalize_reaction_content(reaction)? {
        "+1" => "THUMBS_UP",
        "-1" => "THUMBS_DOWN",
        "laugh" => "LAUGH",
        "confused" => "CONFUSED",
        "heart" => "HEART",
        "hooray" => "HOORAY",
        "rocket" => "ROCKET",
        "eyes" => "EYES",
        other => bail!("no GraphQL reaction mapping for {other:?}"),
    })
}

fn normalize_reaction_content(reaction: &str) -> Result<&'static str> {
    let trimmed = reaction.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "+1" | "thumbs_up" | "thumbsup" | ":+1:" | ":thumbsup:" | "👍" => Ok("+1"),
        "-1" | "thumbs_down" | "thumbsdown" | ":-1:" | ":thumbsdown:" | "👎" => Ok("-1"),
        "laugh" | "laughing" | "smile" | ":laugh:" | ":smile:" | "😄" => Ok("laugh"),
        "confused" | ":confused:" | "😕" => Ok("confused"),
        "heart" | ":heart:" | "❤️" | "❤" => Ok("heart"),
        "hooray" | "tada" | ":hooray:" | ":tada:" | "🎉" => Ok("hooray"),
        "rocket" | ":rocket:" | "🚀" => Ok("rocket"),
        "eyes" | ":eyes:" | "👀" => Ok("eyes"),
        _ => bail!(
            "unsupported GitHub reaction {trimmed:?}; supported reactions are +1, -1, laugh, confused, heart, hooray, rocket, and eyes"
        ),
    }
}

async fn post_pr_comment(report_path: &Path, repo: &str, pr_number: u64) -> Result<String> {
    tracing::info!(
        repo = %repo,
        pr = pr_number,
        "Posting comment to PR"
    );

    let report_path_str = report_path
        .to_str()
        .context("Report path must be valid UTF-8")?;

    let output = Command::new("gh")
        .args([
            "pr",
            "comment",
            &pr_number.to_string(),
            "--repo",
            repo,
            "--body-file",
            report_path_str,
        ])
        .output()
        .await
        .context("Failed to run `gh pr comment`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh pr comment failed: {stderr}");
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    tracing::info!("Successfully posted comment to PR #{}: {}", pr_number, url);
    Ok(url)
}

async fn create_sync_pr(
    report_path: &Path,
    repo: &str,
    pr_number: u64,
    commit_hash: &str,
    review_kind: &str,
    sync_repo: &str,
) -> Result<String> {
    tracing::info!(
        original_repo = %repo,
        pr = pr_number,
        sync_repo = %sync_repo,
        commit_hash = %commit_hash,
        "Creating disclosure PR in sync repository"
    );

    let tmp_dir = tempfile::Builder::new()
        .prefix("fiach-sync-")
        .tempdir()
        .context("Failed to create temporary directory for sync PR")?;

    let repo_dir = tmp_dir.path().join("repo");
    let repo_dir_str = repo_dir
        .to_str()
        .context("Sync repository path must be valid UTF-8")?;

    // Clone the sync repo
    let output = Command::new("gh")
        .args(["repo", "clone", sync_repo, repo_dir_str])
        .output()
        .await
        .context("Failed to run `gh repo clone`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to clone sync repo {}: {}", sync_repo, stderr);
    }

    let report_content =
        std::fs::read_to_string(report_path).context("Failed to read report file")?;

    // Extract title from frontmatter (basic parsing)
    let title = extract_title(&report_content)
        .unwrap_or_else(|| format!("Vulnerability in {}#{}", repo, pr_number));

    let branch_name = sync_branch_name(repo, pr_number, review_kind);
    let base_branch = current_git_branch(&repo_dir).await?;
    let existing_open_pr = find_open_sync_pr(&repo_dir, &branch_name, &base_branch).await?;

    // Check if the branch exists on remote
    let output = Command::new("git")
        .args(["ls-remote", "--heads", "origin", &branch_name])
        .current_dir(&repo_dir)
        .output()
        .await
        .context("Failed to run git ls-remote")?;

    let branch_exists = !output.stdout.is_empty();

    checkout_report_branch(
        &repo_dir,
        &branch_name,
        &base_branch,
        branch_exists && existing_open_pr.is_some(),
    )
    .await?;

    let existing_report_path = repo_dir
        .join(repo)
        .join(sync_report_file_name(pr_number, review_kind));

    let final_report_content = if existing_report_path.exists() {
        let old_content = std::fs::read_to_string(&existing_report_path)
            .context("Failed to read existing report")?;
        if old_content == report_content {
            tracing::info!("Report content is identical to existing report, skipping update");

            if let Some(pr) = existing_open_pr {
                return Ok(pr.url);
            }
            return Ok("unknown".to_string());
        }

        // If they are different, combine them
        combine_reports(&old_content, &report_content)
    } else {
        report_content.clone()
    };

    let dest_dir = repo_dir.join(repo);

    std::fs::create_dir_all(&dest_dir).with_context(|| {
        format!(
            "Failed to create destination directories at {}",
            dest_dir.display()
        )
    })?;

    std::fs::write(&existing_report_path, final_report_content)
        .context("Failed to write report file")?;

    // Git add
    let output = Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
        .output()
        .await
        .context("Failed to run git add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to git add: {}", stderr);
    }

    // Git commit
    let short_hash = if commit_hash.len() > 7 {
        &commit_hash[..7]
    } else {
        commit_hash
    };
    let safe_repo_name = repo.replace("/", "-");
    let commit_msg = format!(
        "audit({}-pr{}): {} ({})",
        safe_repo_name, pr_number, title, short_hash
    );
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=fiach",
            "-c",
            "user.email=fiach@localhost",
            "commit",
            "--no-gpg-sign",
            "-m",
            &commit_msg,
        ])
        .current_dir(&repo_dir)
        .output()
        .await
        .context("Failed to run git commit")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // It might be empty if no changes
        if stderr.contains("nothing to commit")
            || String::from_utf8_lossy(&output.stdout).contains("nothing to commit")
        {
            tracing::info!("No changes to commit, skipping PR creation");

            if let Some(pr) = existing_open_pr {
                return Ok(pr.url);
            }
            return Ok("unknown".to_string());
        }
        bail!("Failed to git commit: {}", stderr);
    }

    // Git push
    let output = Command::new("git")
        .args(["push", "-u", "origin", &branch_name, "--force"])
        .current_dir(&repo_dir)
        .output()
        .await
        .context("Failed to run git push")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to git push: {}", stderr);
    }

    if let Some(pr) = find_open_sync_pr(&repo_dir, &branch_name, &base_branch).await? {
        let pr_url = pr.url;
        tracing::info!("Updated existing Sync PR: {}", pr_url);
        return Ok(pr_url);
    }

    // gh pr create
    let pr_body = format!(
        "Automated review report for {}#{} at commit {}",
        repo, pr_number, commit_hash
    );
    let display_title = format!("{}#{} ({}): {}", repo, pr_number, short_hash, title);
    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            &display_title,
            "--body",
            &pr_body,
            "--base",
            &base_branch,
            "--head",
            &branch_name,
        ])
        .current_dir(&repo_dir)
        .output()
        .await
        .context("Failed to run gh pr create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create PR: {}", stderr);
    }

    let pr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    tracing::info!("Successfully created Sync PR: {}", pr_url);

    Ok(pr_url)
}

fn sync_branch_name(repo: &str, pr_number: u64, review_kind: &str) -> String {
    let safe_repo_name = repo.replace("/", "-");
    if review_kind == crate::state::DEFAULT_REVIEW_KIND {
        format!("report/{safe_repo_name}-pr{pr_number}")
    } else {
        format!(
            "report/{safe_repo_name}-pr{pr_number}-{}",
            sync_safe_component(review_kind)
        )
    }
}

fn sync_report_file_name(pr_number: u64, review_kind: &str) -> String {
    if review_kind == crate::state::DEFAULT_REVIEW_KIND {
        format!("pr-{pr_number}.md")
    } else {
        format!("pr-{pr_number}-{}.md", sync_safe_component(review_kind))
    }
}

fn sync_safe_component(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_separator = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            out.push('-');
            last_was_separator = true;
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "custom".to_string()
    } else {
        trimmed.to_string()
    }
}

async fn current_git_branch(repo_dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo_dir)
        .output()
        .await
        .context("Failed to determine sync repo default branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to determine sync repo default branch: {}", stderr);
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!("Sync repo clone is not on a branch");
    }

    Ok(branch)
}

async fn checkout_report_branch(
    repo_dir: &Path,
    branch_name: &str,
    base_branch: &str,
    update_existing_pr_branch: bool,
) -> Result<()> {
    let remote_ref = if update_existing_pr_branch {
        let branch_ref = format!("refs/heads/{branch_name}:refs/remotes/origin/{branch_name}");
        let output = Command::new("git")
            .args(["fetch", "origin", &branch_ref])
            .current_dir(repo_dir)
            .output()
            .await
            .context("Failed to fetch remote report branch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to fetch remote report branch: {}", stderr);
        }

        tracing::info!(
            branch = %branch_name,
            "Updating existing open sync PR branch"
        );
        format!("origin/{branch_name}")
    } else {
        let base_ref = format!("refs/heads/{base_branch}:refs/remotes/origin/{base_branch}");
        let output = Command::new("git")
            .args(["fetch", "origin", &base_ref])
            .current_dir(repo_dir)
            .output()
            .await
            .context("Failed to fetch sync repo base branch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to fetch sync repo base branch: {}", stderr);
        }

        tracing::info!(
            branch = %branch_name,
            base = %base_branch,
            "Creating report branch from sync repo base branch"
        );
        format!("origin/{base_branch}")
    };

    let output = Command::new("git")
        .args(["checkout", "-B", branch_name, &remote_ref])
        .current_dir(repo_dir)
        .output()
        .await
        .context("Failed to checkout report branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to checkout report branch: {}", stderr);
    }

    Ok(())
}

async fn find_open_sync_pr(
    repo_dir: &Path,
    branch_name: &str,
    base_branch: &str,
) -> Result<Option<ExistingSyncPr>> {
    let pr_list = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch_name,
            "--state",
            "open",
            "--json",
            "number,url,baseRefName",
        ])
        .current_dir(repo_dir)
        .output()
        .await
        .context("Failed to run gh pr list")?;

    if !pr_list.status.success() {
        let stderr = String::from_utf8_lossy(&pr_list.stderr);
        bail!("Failed to list sync PRs: {}", stderr);
    }

    let prs = parse_open_sync_prs(&pr_list.stdout)?;

    if let Some(pr) = prs
        .iter()
        .find(|pr| pr.base_ref_name == base_branch)
        .cloned()
    {
        return Ok(Some(pr));
    }

    if let Some(mut pr) = prs.into_iter().next() {
        tracing::warn!(
            pr = pr.number,
            current_base = %pr.base_ref_name,
            target_base = %base_branch,
            "Retargeting sync PR to repository default branch"
        );

        let pr_number = pr.number.to_string();
        let output = Command::new("gh")
            .args(["pr", "edit", &pr_number, "--base", base_branch])
            .current_dir(repo_dir)
            .output()
            .await
            .context("Failed to run gh pr edit")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to retarget sync PR: {}", stderr);
        }

        pr.base_ref_name = base_branch.to_string();
        return Ok(Some(pr));
    }

    Ok(None)
}

fn parse_open_sync_prs(stdout: &[u8]) -> Result<Vec<ExistingSyncPr>> {
    serde_json::from_slice(stdout).context("Failed to parse gh pr list output")
}

fn combine_reports(old: &str, new: &str) -> String {
    let (old_frontmatter, old_body) = split_report(old);
    let (new_frontmatter, new_body) = split_report(new);

    // Keep the new frontmatter as the primary one, but combine bodies.
    // We prepend the new body and append the old one.
    format!(
        "---\n{}---\n\n{}\n\n---\n## Previous Review Context\n\n{}\n\n---\n## Previous Frontmatter\n```yaml\n{}```",
        new_frontmatter, new_body, old_body, old_frontmatter
    )
}

fn split_report(content: &str) -> (String, String) {
    let mut frontmatter = String::new();
    let mut body = String::new();
    let mut in_frontmatter = false;
    let mut count = 0;

    for line in content.lines() {
        if line.trim() == "---" {
            count += 1;
            if count == 1 {
                in_frontmatter = true;
                continue;
            } else if count == 2 {
                in_frontmatter = false;
                continue;
            }
        }
        if in_frontmatter {
            frontmatter.push_str(line);
            frontmatter.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    (frontmatter, body.trim().to_string())
}

fn extract_title(content: &str) -> Option<String> {
    for line in content.lines() {
        if line.starts_with("title:") {
            let title = line.trim_start_matches("title:").trim();
            // Remove surrounding quotes if they exist
            if title.starts_with('"') && title.ends_with('"') && title.len() >= 2 {
                return Some(title[1..title.len() - 1].to_string());
            }
            return Some(title.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use crate::reporting::{
        AffectedLocation, CommandTranscript, DisclosurePolicy, DuplicateDecision, Finding,
        FindingInput, PrContext, ReportingArtifact, Verdict,
    };

    fn policy() -> DisclosurePolicy {
        DisclosurePolicy {
            pr_context: PrContext {
                state: "open".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: "head".to_string(),
            },
            diff_anchors: BTreeMap::from([("src/lib.rs".to_string(), BTreeSet::from([10]))]),
        }
    }

    fn target(review_kind: &str) -> DisclosureTarget<'_> {
        DisclosureTarget {
            repo: "owner/repo",
            pr_number: 42,
            commit_hash: "abc123",
            review_kind,
        }
    }

    fn disclose_config(mode: ReportMode) -> DiscloseConfig {
        DiscloseConfig {
            mode,
            sync_repo: Some("owner/security-sync".to_string()),
            notify_on_empty: false,
            reactions: ReactionConfig::default(),
        }
    }

    fn artifact(introduced_by_pr: bool) -> ReportingArtifact {
        let finding = Finding::from_input(
            0,
            FindingInput {
                title: "Bug".to_string(),
                severity: "high".to_string(),
                confidence: "high".to_string(),
                affected_locations: vec![AffectedLocation {
                    path: "src/lib.rs".to_string(),
                    start_line: Some(10),
                    end_line: None,
                }],
                evidence: "evidence".to_string(),
                skills_used: Vec::new(),
                body_markdown: "body".to_string(),
            },
        )
        .unwrap();

        ReportingArtifact {
            findings: vec![finding],
            verdicts: vec![Verdict {
                finding_id: "F-1".to_string(),
                confirmed: true,
                introduced_by_pr,
                present_on_pr_branch: true,
                present_on_base: !introduced_by_pr,
                present_on_default_branch: !introduced_by_pr,
                disclosure_decision: if introduced_by_pr {
                    "disclose".to_string()
                } else {
                    "suppress".to_string()
                },
                title_override: None,
                severity_override: None,
                impact_override: None,
                final_comment_body: Some("body".to_string()),
                affected_locations: Vec::new(),
                command_transcripts: vec![CommandTranscript {
                    command: "cargo test".to_string(),
                    branch_or_commit: "head".to_string(),
                    key_output: "failed".to_string(),
                    interpretation: "reproduced".to_string(),
                }],
                rationale: "confirmed".to_string(),
            }],
            ..Default::default()
        }
    }

    fn no_findings_artifact() -> ReportingArtifact {
        ReportingArtifact::default()
    }

    fn duplicate_artifact() -> ReportingArtifact {
        let mut artifact = artifact(true);
        artifact.duplicate_decisions.push(DuplicateDecision {
            finding_id: "F-1".to_string(),
            already_reported: true,
            matching_comment_ids: vec![101],
            confidence: "high".to_string(),
            rationale: "same root issue already reported".to_string(),
        });
        artifact
    }

    #[test]
    fn hybrid_security_pr_introduced_finding_posts_to_pr() {
        let route = structured_disclosure_route(
            target("security"),
            &artifact(true),
            &policy(),
            &disclose_config(ReportMode::Hybrid),
        );

        assert_eq!(route, StructuredDisclosureRoute::PrReview);
    }

    #[test]
    fn hybrid_security_non_pr_finding_syncs_to_sync_repo() {
        let route = structured_disclosure_route(
            target("security"),
            &artifact(false),
            &policy(),
            &disclose_config(ReportMode::Hybrid),
        );

        assert_eq!(route, StructuredDisclosureRoute::SyncPr);
    }

    #[test]
    fn hybrid_pr_review_non_pr_finding_does_not_sync() {
        let route = structured_disclosure_route(
            target("pr-review"),
            &artifact(false),
            &policy(),
            &disclose_config(ReportMode::Hybrid),
        );

        assert_eq!(route, StructuredDisclosureRoute::Skip);
    }

    #[test]
    fn duplicate_only_pr_comment_skips_even_when_notify_on_empty() {
        let mut config = disclose_config(ReportMode::PrComment);
        config.notify_on_empty = true;

        let route = structured_disclosure_route(
            target("pr-review"),
            &duplicate_artifact(),
            &policy(),
            &config,
        );

        assert_eq!(route, StructuredDisclosureRoute::Skip);
    }

    #[test]
    fn mixed_duplicate_and_new_findings_still_posts_review() {
        let mut artifact = artifact(true);
        let second = Finding::from_input(
            1,
            FindingInput {
                title: "New bug".to_string(),
                severity: "high".to_string(),
                confidence: "high".to_string(),
                affected_locations: vec![AffectedLocation {
                    path: "src/lib.rs".to_string(),
                    start_line: Some(10),
                    end_line: None,
                }],
                evidence: "evidence".to_string(),
                skills_used: Vec::new(),
                body_markdown: "new body".to_string(),
            },
        )
        .unwrap();
        artifact.findings.push(second);
        artifact.verdicts.push(Verdict {
            finding_id: "F-2".to_string(),
            confirmed: true,
            introduced_by_pr: true,
            present_on_pr_branch: true,
            present_on_base: false,
            present_on_default_branch: false,
            disclosure_decision: "disclose".to_string(),
            title_override: None,
            severity_override: None,
            impact_override: None,
            final_comment_body: Some("new body".to_string()),
            affected_locations: Vec::new(),
            command_transcripts: vec![CommandTranscript {
                command: "cargo test".to_string(),
                branch_or_commit: "head".to_string(),
                key_output: "failed".to_string(),
                interpretation: "reproduced".to_string(),
            }],
            rationale: "confirmed".to_string(),
        });
        artifact.duplicate_decisions.push(DuplicateDecision {
            finding_id: "F-1".to_string(),
            already_reported: true,
            matching_comment_ids: vec![101],
            confidence: "high".to_string(),
            rationale: "same root issue already reported".to_string(),
        });

        let route = structured_disclosure_route(
            target("pr-review"),
            &artifact,
            &policy(),
            &disclose_config(ReportMode::PrComment),
        );

        assert_eq!(route, StructuredDisclosureRoute::PrReview);
        let publishable = artifact.publishable_findings(&policy());
        assert_eq!(publishable.len(), 1);
        assert_eq!(publishable[0].finding_id, "F-2");
    }

    #[test]
    fn existing_structured_report_modes_keep_routes() {
        let policy = policy();
        let introduced = artifact(true);
        let non_introduced = artifact(false);
        let empty = no_findings_artifact();

        assert_eq!(
            structured_disclosure_route(
                target("security"),
                &introduced,
                &policy,
                &disclose_config(ReportMode::Local),
            ),
            StructuredDisclosureRoute::Local
        );
        assert_eq!(
            structured_disclosure_route(
                target("security"),
                &introduced,
                &policy,
                &disclose_config(ReportMode::PrComment),
            ),
            StructuredDisclosureRoute::PrReview
        );
        assert_eq!(
            structured_disclosure_route(
                target("security"),
                &introduced,
                &policy,
                &disclose_config(ReportMode::SyncPr),
            ),
            StructuredDisclosureRoute::SyncPr
        );
        assert_eq!(
            structured_disclosure_route(
                target("security"),
                &non_introduced,
                &policy,
                &disclose_config(ReportMode::PrComment),
            ),
            StructuredDisclosureRoute::Skip
        );
        assert_eq!(
            structured_disclosure_route(
                target("security"),
                &empty,
                &policy,
                &disclose_config(ReportMode::SyncPr),
            ),
            StructuredDisclosureRoute::Skip
        );
    }

    #[test]
    fn parse_open_sync_prs_reads_base_branch() {
        let prs = parse_open_sync_prs(
            br#"[
                {
                    "number": 5,
                    "url": "https://github.com/thesimplekid/cdk-reviews/pull/5",
                    "baseRefName": "main"
                }
            ]"#,
        )
        .expect("valid PR JSON should parse");

        assert_eq!(
            prs,
            vec![ExistingSyncPr {
                number: 5,
                url: "https://github.com/thesimplekid/cdk-reviews/pull/5".to_string(),
                base_ref_name: "main".to_string(),
            }]
        );
    }

    #[test]
    fn parse_open_sync_prs_rejects_invalid_json() {
        let result = parse_open_sync_prs(b"not json");

        assert!(result.is_err());
    }

    #[test]
    fn normalize_reaction_content_accepts_names_and_emoji_aliases() {
        assert_eq!(normalize_reaction_content("eyes").unwrap(), "eyes");
        assert_eq!(normalize_reaction_content(":rocket:").unwrap(), "rocket");
        assert_eq!(normalize_reaction_content("🎉").unwrap(), "hooray");
        assert_eq!(normalize_reaction_content("thumbs_up").unwrap(), "+1");
    }

    #[test]
    fn normalize_reaction_content_rejects_unsupported_reactions() {
        let result = normalize_reaction_content("wave");

        assert!(result.is_err());
    }

    #[test]
    fn non_actionable_statuses_cover_clean_rejected_and_already_reported() {
        assert!(is_non_actionable_status("none"));
        assert!(is_non_actionable_status("rejected"));
        assert!(is_non_actionable_status("already-reported"));
        assert!(!is_non_actionable_status("confirmed"));
        assert!(!is_non_actionable_status("failed"));
        assert!(!is_non_actionable_status("unverified"));
        assert!(!is_non_actionable_status("markdown-only"));
    }

    #[test]
    fn graphql_reaction_content_maps_rest_names_to_enum_values() {
        assert_eq!(graphql_reaction_content("eyes").unwrap(), "EYES");
        assert_eq!(graphql_reaction_content("👀").unwrap(), "EYES");
        assert_eq!(graphql_reaction_content("+1").unwrap(), "THUMBS_UP");
        assert_eq!(graphql_reaction_content("rocket").unwrap(), "ROCKET");
        assert!(graphql_reaction_content("wave").is_err());
    }

    #[test]
    fn start_reaction_is_only_cleared_when_distinct_from_outcome() {
        assert!(should_clear_start_reaction("eyes", "+1"));
        assert!(!should_clear_start_reaction("eyes", "eyes"));
        assert!(!should_clear_start_reaction("👀", "eyes"));
        assert!(!should_clear_start_reaction("wave", "+1"));
    }

    #[test]
    fn reaction_config_defaults_to_eyes_and_thumbs_up() {
        let config = ReactionConfig::with_defaults(None, None);

        assert_eq!(config.review_start.as_deref(), Some("eyes"));
        assert_eq!(config.no_findings.as_deref(), Some("+1"));
    }

    #[test]
    fn sync_paths_are_legacy_for_default_review_kind() {
        assert_eq!(
            sync_branch_name("owner/repo", 42, crate::state::DEFAULT_REVIEW_KIND),
            "report/owner-repo-pr42"
        );
        assert_eq!(
            sync_report_file_name(42, crate::state::DEFAULT_REVIEW_KIND),
            "pr-42.md"
        );
    }

    #[test]
    fn sync_paths_are_scoped_for_persona_review_kind() {
        assert_eq!(
            sync_branch_name("owner/repo", 42, "pr-review"),
            "report/owner-repo-pr42-pr-review"
        );
        assert_eq!(sync_report_file_name(42, "security"), "pr-42-security.md");
        assert_eq!(
            sync_report_file_name(42, "Custom Persona.md"),
            "pr-42-custom-persona-md.md"
        );
    }
}
