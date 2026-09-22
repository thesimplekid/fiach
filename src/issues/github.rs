use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex};

use crate::process::output_limited;

use super::{Item, config::Project};

pub(super) const MARKER: &str = "<!-- fiach-issue-triage -->";
const LIMIT: usize = 8 * 1024 * 1024;
pub(super) const PR_DIFF_LIMIT: usize = 80 * 1024;

// Shared by issue API calls, including publication rechecks and worker helpers.
static RATE_LIMIT_UNTIL: AtomicU64 = AtomicU64::new(0);
static SECONDARY_BACKOFF: AtomicU64 = AtomicU64::new(60);

#[derive(Clone)]
struct CachedDiff {
    head: String,
    base: String,
    diff: Option<String>,
}

pub(super) struct Github {
    pub login: String,
    candidates: Mutex<HashMap<(String, u64), Item>>,
    diffs: Mutex<HashMap<(String, u64), CachedDiff>>,
}

impl Github {
    pub async fn new() -> Result<Self> {
        let user = api("user", "GET", None).await?;
        Ok(Self {
            login: string(&user, "login")?,
            candidates: Mutex::new(HashMap::new()),
            diffs: Mutex::new(HashMap::new()),
        })
    }

    pub async fn inventory(&self, repo: &str) -> Result<Vec<Item>> {
        // Each inventory (including pre-publication rechecks) starts a fresh evidence pass.
        self.candidates.lock().await.clear();
        self.diffs.lock().await.clear();
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

    /// Share detailed candidates within one inventory pass; target reads remain fresh.
    pub async fn candidate(&self, repo: &str, number: u64) -> Result<Item> {
        let key = (repo.to_owned(), number);
        if let Some(item) = self.candidates.lock().await.get(&key).cloned() {
            return Ok(item);
        }
        let item = self.issue(repo, number).await?;
        let mut cache = self.candidates.lock().await;
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, item.clone());
        Ok(item)
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

    /// None means the diff exceeded the capture limit; no partial evidence is returned.
    pub async fn pr_diff(&self, repo: &str, number: u64) -> Result<Option<String>> {
        let before = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(before["state"] == "open", "Candidate PR is no longer open");
        let key = (repo.to_owned(), number);
        let head = string(&before["head"], "sha")?;
        let base = string(&before["base"], "sha")?;
        if let Some(cached) = self.diffs.lock().await.get(&key).cloned()
            && cached.head == head
            && cached.base == base
        {
            return Ok(cached.diff);
        }
        check_rate_limit()?;
        let output = output_limited(
            Command::new("gh").args([
                "pr",
                "diff",
                &number.to_string(),
                "--repo",
                repo,
                "--color",
                "never",
            ]),
            "fetching candidate PR diff",
            Duration::from_secs(300),
            PR_DIFF_LIMIT,
        )
        .await
        .with_context(|| format!("Fetching diff for {repo}#{number}"))?;
        let diff = if output.stdout_truncated {
            tracing::warn!(
                repo,
                candidate_pr = number,
                limit_bytes = PR_DIFF_LIMIT,
                "Candidate PR diff exceeds limit; coverage unresolved"
            );
            None
        } else {
            if !output.status.success() {
                observe_rate_limit(
                    &RateHeaders::default(),
                    &String::from_utf8_lossy(&output.stderr),
                );
            }
            ensure!(
                output.status.success(),
                "Fetching diff for {repo}#{number} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some(String::from_utf8(output.stdout)?)
        };
        let after = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(
            after["head"]["sha"] == head
                && after["base"]["sha"] == base
                && after["state"] == "open",
            "Candidate PR changed during collection"
        );
        let mut cache = self.diffs.lock().await;
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(
            key,
            CachedDiff {
                head,
                base,
                diff: diff.clone(),
            },
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
    check_rate_limit()?;
    let mut command = Command::new("gh");
    command.args(["api", "--method", method, endpoint, "--include"]);
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
        api_output(
            endpoint,
            method,
            output.status.success(),
            &output.stdout,
            &output.stderr,
        )?
    } else {
        let output = output_limited(
            &mut command,
            "calling GitHub issue API",
            Duration::from_secs(300),
            LIMIT,
        )
        .await
        .with_context(|| format!("GitHub {method} {endpoint}"))?;
        ensure!(
            !output.stdout_truncated,
            "GitHub {method} {endpoint} output exceeded limit ({LIMIT} bytes)"
        );
        api_output(
            endpoint,
            method,
            output.status.success(),
            &output.stdout,
            &output.stderr,
        )?
    };
    if output.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&output)?)
    }
}

#[derive(Default)]
struct RateHeaders {
    status: u16,
    remaining: Option<u64>,
    reset: Option<u64>,
    retry_after: Option<u64>,
}

fn split_response(output: &[u8]) -> Result<(RateHeaders, &[u8])> {
    let (end, separator) = output
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|end| (end, 4))
        .or_else(|| {
            output
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|end| (end, 2))
        })
        .context("GitHub API response missing headers")?;
    let header = std::str::from_utf8(&output[..end])?;
    let mut lines = header.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("GitHub API response missing status")?
        .parse()?;
    let mut result = RateHeaders {
        status,
        ..Default::default()
    };
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "x-ratelimit-remaining" => result.remaining = value.trim().parse().ok(),
                "x-ratelimit-reset" => result.reset = value.trim().parse().ok(),
                "retry-after" => result.retry_after = value.trim().parse().ok(),
                _ => {}
            }
        }
    }
    Ok((result, &output[end + separator..]))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn rate_limit_wait() -> Duration {
    Duration::from_secs(
        RATE_LIMIT_UNTIL
            .load(Ordering::Relaxed)
            .saturating_sub(now_seconds()),
    )
}

fn check_rate_limit() -> Result<()> {
    ensure!(
        rate_limit_wait().is_zero(),
        "GitHub issue requests paused until Unix timestamp {} after rate limiting",
        RATE_LIMIT_UNTIL.load(Ordering::Relaxed)
    );
    Ok(())
}

fn retry_deadline(headers: &RateHeaders, error: &str, now: u64, fallback: u64) -> Option<u64> {
    if headers.remaining == Some(0)
        || headers.status == 429
        || error.to_ascii_lowercase().contains("rate limit")
        || (headers.status == 403 && headers.retry_after.is_some())
    {
        let reset = if headers.remaining == Some(0) {
            headers.reset
        } else {
            None
        };
        let retry = headers
            .retry_after
            .map(|seconds| now.saturating_add(seconds));
        Some(
            reset
                .into_iter()
                .chain(retry)
                .max()
                .filter(|deadline| *deadline > now)
                .unwrap_or_else(|| now.saturating_add(fallback))
                .saturating_add(1),
        )
    } else {
        None
    }
}

fn observe_rate_limit(headers: &RateHeaders, error: &str) {
    let fallback = SECONDARY_BACKOFF.load(Ordering::Relaxed);
    if let Some(until) = retry_deadline(headers, error, now_seconds(), fallback) {
        RATE_LIMIT_UNTIL.fetch_max(until, Ordering::Relaxed);
        SECONDARY_BACKOFF.store(fallback.saturating_mul(2).min(3600), Ordering::Relaxed);
        tracing::warn!(reset_at = until, remaining = ?headers.remaining,
            "GitHub rate limit reached; pausing issue requests");
    } else if (200..300).contains(&headers.status) {
        SECONDARY_BACKOFF.store(60, Ordering::Relaxed);
    }
}

fn api_output(
    endpoint: &str,
    method: &str,
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Vec<u8>> {
    let error = String::from_utf8_lossy(stderr);
    // gh includes response headers on HTTP errors, but local failures may have none.
    if !success && !stdout.starts_with(b"HTTP/") {
        observe_rate_limit(&RateHeaders::default(), &error);
        anyhow::bail!("GitHub {method} {endpoint} failed: {error}");
    }
    let (headers, body) = split_response(stdout)?;
    observe_rate_limit(&headers, &error);
    ensure!(success, "GitHub {method} {endpoint} failed: {error}");
    Ok(body.to_vec())
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
        "Issue command output exceeded limit ({limit} bytes; program {})",
        command.as_std().get_program().to_string_lossy()
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
    fn rate_headers_preserve_body_and_honor_both_reset_and_retry_after() {
        let (headers, body) = split_response(b"HTTP/2.0 403 Forbidden\r\nx-ratelimit-remaining: 0\r\nX-RateLimit-Reset: 200\r\nRetry-After: 150\r\n\r\n{\"message\":\"limited\"}").unwrap();
        assert_eq!(body, br#"{"message":"limited"}"#);
        assert_eq!(retry_deadline(&headers, "", 100, 60), Some(251));
        let (headers, _) = split_response(
            b"HTTP/2.0 200 OK\nX-RateLimit-Remaining: 0\nX-RateLimit-Reset: 200\n\n{}",
        )
        .unwrap();
        assert_eq!(retry_deadline(&headers, "", 100, 60), Some(201));
    }

    #[test]
    fn secondary_limits_back_off_but_permission_errors_do_not() {
        let headers = RateHeaders {
            status: 403,
            remaining: Some(100),
            ..Default::default()
        };
        assert_eq!(
            retry_deadline(&headers, "secondary rate limit", 100, 120),
            Some(221)
        );
        assert_eq!(
            retry_deadline(&headers, "Resource not accessible", 100, 120),
            None
        );
        let headers = RateHeaders {
            status: 429,
            retry_after: Some(300),
            ..Default::default()
        };
        assert_eq!(retry_deadline(&headers, "", 100, 60), Some(401));
        assert!(split_response(b"not an HTTP response").is_err());
    }

    #[test]
    fn label_names_are_single_url_segments() {
        assert_eq!(encode_segment("area:db / core"), "area%3Adb%20%2F%20core");
    }
}
