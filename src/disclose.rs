use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[derive(Debug, Clone, clap::ValueEnum, Default, serde::Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ReportMode {
    #[default]
    Local,
    PrComment,
    SyncPr,
}

impl std::fmt::Display for ReportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::PrComment => write!(f, "pr-comment"),
            Self::SyncPr => write!(f, "sync-pr"),
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
            _ => Err(format!("Invalid report mode: {}", s)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscloseConfig {
    pub mode: ReportMode,
    pub sync_repo: Option<String>,
    pub notify_on_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct ExistingSyncPr {
    number: u64,
    url: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
}

pub async fn handle_disclosure(
    report_path: &Path,
    repo: &str,
    pr_number: u64,
    commit_hash: &str,
    vulnerabilities_found: bool,
    config: &DiscloseConfig,
) -> Result<Option<String>> {
    match config.mode {
        ReportMode::Local => {
            tracing::info!("ReportMode is Local. Report saved to {:?}", report_path);
            Ok(Some(report_path.to_string_lossy().to_string()))
        }
        ReportMode::PrComment => {
            if !vulnerabilities_found && !config.notify_on_empty {
                tracing::info!(
                    "No vulnerabilities found and notify_on_empty is false. Skipping PR comment."
                );
                return Ok(None);
            }
            post_pr_comment(report_path, repo, pr_number)
                .await
                .map(Some)
        }
        ReportMode::SyncPr => {
            if !vulnerabilities_found && !config.notify_on_empty {
                tracing::info!(
                    "No vulnerabilities found and notify_on_empty is false. Skipping Sync PR."
                );
                return Ok(None);
            }

            let sync_repo = config
                .sync_repo
                .as_ref()
                .context("sync_repo must be provided for SyncPr mode")?;
            create_sync_pr(report_path, repo, pr_number, commit_hash, sync_repo)
                .await
                .map(Some)
        }
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

    let safe_repo_name = repo.replace("/", "-");
    let branch_name = format!("report/{}-pr{}", safe_repo_name, pr_number);
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

    let existing_report_path = repo_dir.join(repo).join(format!("pr-{}.md", pr_number));

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
        "Automated vulnerability report for {}#{} at commit {}",
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
}
