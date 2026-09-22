use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command};

use crate::process::output_limited;

use super::{Item, config::Project};

pub(super) const MARKER: &str = "<!-- fiach-issue-triage -->";
const LIMIT: usize = 8 * 1024 * 1024;

pub(super) struct Github {
    pub login: String,
}

impl Github {
    pub async fn new() -> Result<Self> {
        let user = api("user", "GET", None).await?;
        Ok(Self {
            login: string(&user, "login")?,
        })
    }

    pub async fn inventory(&self, repo: &str) -> Result<Vec<Item>> {
        let mut result = Vec::new();
        // The work batch limit must not truncate evidence. Stream all pages, discarding
        // closed PRs before retaining inventory; closed issues remain duplicate candidates.
        for page in 1.. {
            let value = api(&format!("repos/{repo}/issues?state=all&sort=created&direction=asc&per_page=100&page={page}"), "GET", None).await?;
            let values = value.as_array().context("Expected GitHub issue list")?;
            for value in values {
                let item = item(value)?;
                if !item.is_pr || item.open {
                    result.push(item);
                }
            }
            if values.len() < 100 {
                break;
            }
        }
        // Fetch discussions in repository-wide pages, rather than one request
        // per historical issue. Include human comments even when title/body
        // alone would look unrelated; exclude only our own marked comments.
        let positions: HashMap<_, _> = result
            .iter()
            .enumerate()
            .map(|(i, item)| (item.number, i))
            .collect();
        for page in 1.. {
            let value = api(&format!("repos/{repo}/issues/comments?sort=created&direction=asc&per_page=100&page={page}"), "GET", None).await?;
            let comments = value
                .as_array()
                .context("Expected repository issue comments")?;
            for comment in comments {
                let number = comment["issue_url"]
                    .as_str()
                    .and_then(|url| url.rsplit('/').next())
                    .and_then(|n| n.parse::<u64>().ok())
                    .context("Missing comment issue number")?;
                if let Some(&index) = positions.get(&number)
                    && let Some(text) = self.comment_text(comment)
                {
                    result[index].comments.push(text);
                }
            }
            if comments.len() < 100 {
                break;
            }
        }
        Ok(result)
    }

    pub async fn issue(&self, repo: &str, number: u64) -> Result<Item> {
        let mut result = item(&api(&format!("repos/{repo}/issues/{number}"), "GET", None).await?)?;
        result.comments = self
            .comments(repo, number)
            .await?
            .into_iter()
            .filter_map(|c| self.comment_text(&c))
            .collect();
        // Re-read to detect edits during pagination.
        let after = api(&format!("repos/{repo}/issues/{number}"), "GET", None).await?;
        ensure!(
            after["updated_at"] == result.updated_at,
            "Issue changed while collecting evidence"
        );
        Ok(result)
    }

    fn comment_text(&self, comment: &Value) -> Option<String> {
        let body = comment["body"].as_str().unwrap_or("");
        if comment["user"]["login"] == self.login && body.starts_with(MARKER) {
            return None;
        }
        Some(format!(
            "{} (GitHub association: {}): {}",
            comment["user"]["login"].as_str().unwrap_or("unknown"),
            comment["author_association"].as_str().unwrap_or("NONE"),
            body
        ))
    }

    async fn comments(&self, repo: &str, number: u64) -> Result<Vec<Value>> {
        pages(
            &format!("repos/{repo}/issues/{number}/comments?sort=created"),
            2000,
        )
        .await
    }

    pub async fn pr_diff(&self, repo: &str, number: u64) -> Result<String> {
        let before = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(before["state"] == "open", "Candidate PR is no longer open");
        let diff = command(
            Command::new("gh").args([
                "pr",
                "diff",
                &number.to_string(),
                "--repo",
                repo,
                "--color",
                "never",
            ]),
            80 * 1024,
        )
        .await?;
        let after = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(
            before["head"]["sha"] == after["head"]["sha"] && after["state"] == "open",
            "Candidate PR changed during collection"
        );
        Ok(diff)
    }

    pub async fn mark(
        &self,
        project: &Project,
        number: u64,
        labels: &[String],
        body: &str,
    ) -> Result<()> {
        let repo = &project.repo;
        let endpoint = format!("repos/{repo}/issues/{number}");
        ensure!(
            api(&endpoint, "GET", None).await?["state"] == "open",
            "Issue was closed before marking"
        );
        // Create only missing configured labels, preserving existing project colors/descriptions.
        let existing = pages(&format!("repos/{repo}/labels?"), 2000).await?;
        for label in labels {
            if !existing.iter().any(|v| {
                v["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(label))
            }) {
                api(
                    &format!("repos/{repo}/labels"),
                    "POST",
                    Some(json!({"name": label, "color": "c5def5"})),
                )
                .await?;
            }
        }
        // Delete only labels in our configured namespace; retain human labels.
        let current = api(&endpoint, "GET", None).await?;
        let managed = project.managed_labels();
        for label in current["labels"]
            .as_array()
            .context("Missing issue labels")?
        {
            let name = label["name"].as_str().context("Missing label name")?;
            if managed.iter().any(|l| l.eq_ignore_ascii_case(name))
                && !labels.iter().any(|l| l.eq_ignore_ascii_case(name))
            {
                api(
                    &format!("{endpoint}/labels/{}", encode_segment(name)),
                    "DELETE",
                    None,
                )
                .await?;
            }
        }
        if !labels.is_empty() {
            api(
                &format!("{endpoint}/labels"),
                "POST",
                Some(json!({"labels": labels})),
            )
            .await?;
        }
        let text = format!("{MARKER}\n{body}");
        let comments = self.comments(repo, number).await?;
        if let Some(comment) = comments.iter().find(|c| {
            c["user"]["login"] == self.login
                && c["body"].as_str().is_some_and(|b| b.starts_with(MARKER))
        }) {
            if comment["body"] != text {
                let id = comment["id"].as_u64().context("Missing comment id")?;
                api(
                    &format!("repos/{repo}/issues/comments/{id}"),
                    "PATCH",
                    Some(json!({"body": text})),
                )
                .await?;
            }
        } else {
            api(
                &format!("{endpoint}/comments"),
                "POST",
                Some(json!({"body": text})),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn branch_pr(&self, repo: &str, branch: &str) -> Result<Option<(String, bool)>> {
        let prs = pages(
            &format!(
                "repos/{repo}/pulls?state=all&head={}:{}",
                repo.split('/').next().context("Missing owner")?,
                branch
            ),
            1000,
        )
        .await?;
        prs.first()
            .map(|pr| Ok((string(pr, "html_url")?, pr["state"] == "open")))
            .transpose()
    }

    pub async fn open_pr(
        &self,
        repo: &str,
        branch: &str,
        base: &str,
        number: u64,
        summary: &str,
    ) -> Result<String> {
        if let Some((url, open)) = self.branch_pr(repo, branch).await? {
            ensure!(
                open,
                "A prior bot PR was closed; maintainer decision required"
            );
            return Ok(url);
        }
        // Intentionally no closing keyword: merging must never automatically close the issue.
        let pr = api(&format!("repos/{repo}/pulls"), "POST", Some(json!({
            "title": format!("Address issue #{number}"), "head": branch, "base": base, "draft": true,
            "body": format!("Related to #{number}.\n\n{summary}\n\nGenerated by Fiach; maintainer review is required.")
        }))).await?;
        string(&pr, "html_url")
    }
}

pub(super) async fn api(endpoint: &str, method: &str, body: Option<Value>) -> Result<Value> {
    let mut command = Command::new("gh");
    command.args(["api", "--method", method, endpoint]);
    let output = if let Some(body) = body {
        // JSON goes through stdin, never shell interpolation or process arguments.
        use std::process::Stdio;
        command
            .args(["--input", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        child
            .stdin
            .take()
            .context("Missing gh stdin")?
            .write_all(&serde_json::to_vec(&body)?)
            .await?;
        let output =
            tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await??;
        ensure!(
            output.status.success(),
            "GitHub {method} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    } else {
        command_bytes(&mut command, LIMIT).await?
    };
    if output.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&output)?)
    }
}

async fn pages(endpoint: &str, max: usize) -> Result<Vec<Value>> {
    let mut result = vec![];
    for page in 1.. {
        let value = api(&format!("{endpoint}&per_page=100&page={page}"), "GET", None).await?;
        let values = value.as_array().context("Expected GitHub list")?;
        result.extend(values.iter().cloned());
        ensure!(
            result.len() <= max,
            "GitHub inventory exceeds configured limit ({max}); refusing incomplete triage"
        );
        if values.len() < 100 {
            return Ok(result);
        }
    }
    unreachable!()
}

fn item(v: &Value) -> Result<Item> {
    Ok(Item {
        number: v["number"].as_u64().context("Missing issue number")?,
        title: string(v, "title")?,
        body: v["body"].as_str().unwrap_or("").to_owned(),
        open: string(v, "state")? == "open",
        is_pr: v.get("pull_request").is_some(),
        updated_at: string(v, "updated_at")?,
        comments: vec![],
    })
}
fn string(v: &Value, key: &str) -> Result<String> {
    Ok(v[key]
        .as_str()
        .with_context(|| format!("Missing {key}"))?
        .to_owned())
}
fn encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
pub(super) async fn command_bytes(command: &mut Command, limit: usize) -> Result<Vec<u8>> {
    let output = output_limited(
        command,
        "running issue workflow command",
        Duration::from_secs(300),
        limit,
    )
    .await?;
    ensure!(
        !output.stdout_truncated,
        "Issue command output exceeded limit"
    );
    ensure!(
        output.status.success(),
        "Issue command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
pub(super) async fn command(command: &mut Command, limit: usize) -> Result<String> {
    Ok(String::from_utf8(command_bytes(command, limit).await?)?)
}
pub(super) async fn git(path: &Path, args: &[&str]) -> Result<String> {
    command(
        Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args),
        LIMIT,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn label_names_are_single_url_segments() {
        assert_eq!(encode_segment("area:db / core"), "area%3Adb%20%2F%20core");
    }
}
