use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex};

use crate::process::{LimitedOutput, output_limited};

use super::{Item, config::Project};

pub(super) const MARKER: &str = "<!-- fiach-issue-triage -->";
const LIMIT: usize = 8 * 1024 * 1024;
// Bound capture/cache memory independently of Jev's token-based context limit.
pub(super) const PR_DIFF_LIMIT: usize = 512 * 1024;

// Shared by issue API calls, including publication rechecks and worker helpers.
static RATE_LIMIT_UNTIL: AtomicU64 = AtomicU64::new(0);
static SECONDARY_BACKOFF: AtomicU64 = AtomicU64::new(60);

#[derive(Clone)]
struct CachedDiff {
    head: String,
    base: String,
    diff: Option<String>,
}

/// Small bounded LRU; touching one PR must not evict the other cached diffs.
#[derive(Default)]
struct DiffCache {
    entries: VecDeque<((String, u64), CachedDiff)>,
}

impl DiffCache {
    fn get(&mut self, key: &(String, u64), head: &str, base: &str) -> Option<Option<String>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == key)?;
        let entry = self.entries.remove(index)?;
        if entry.1.head != head || entry.1.base != base {
            return None;
        }
        let diff = entry.1.diff.clone();
        self.entries.push_back(entry);
        Some(diff)
    }

    fn insert(&mut self, key: (String, u64), diff: CachedDiff) {
        self.entries.retain(|(cached_key, _)| cached_key != &key);
        if self.entries.len() >= 128 {
            self.entries.pop_front();
        }
        self.entries.push_back((key, diff));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

pub(super) struct Github {
    pub login: String,
    candidates: Mutex<HashMap<(String, u64), Item>>,
    diffs: Mutex<DiffCache>,
}

impl Github {
    pub async fn new() -> Result<Self> {
        let user = api("user", "GET", None).await?;
        Ok(Self {
            login: string(&user, "login")?,
            candidates: Mutex::new(HashMap::new()),
            diffs: Mutex::new(DiffCache::default()),
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

    /// Existing references are only a comment deduplication signal, not fix evidence.
    pub async fn linked_work(
        &self,
        repo: &str,
        issue: &Item,
        candidates: &[u64],
    ) -> Result<HashSet<u64>> {
        let mut linked = HashSet::new();
        for number in candidates {
            if std::iter::once(&issue.title)
                .chain(std::iter::once(&issue.body))
                .chain(issue.comments.iter())
                .any(|text| mentions_work(text, repo, *number))
            {
                linked.insert(*number);
            }
        }
        for event in pages(
            &format!("repos/{repo}/issues/{}/timeline", issue.number),
            2000,
        )
        .await?
        {
            if matches!(
                event["event"].as_str(),
                Some("cross-referenced" | "connected")
            ) {
                for url in [
                    event["source"]["issue"]["html_url"].as_str(),
                    event["subject"]["url"].as_str(),
                ]
                .into_iter()
                .flatten()
                {
                    for prefix in [
                        format!("https://github.com/{repo}/pull/"),
                        format!("https://github.com/{repo}/issues/"),
                        format!("https://api.github.com/repos/{repo}/pulls/"),
                        format!("https://api.github.com/repos/{repo}/issues/"),
                    ] {
                        if let Some(number) = url
                            .strip_prefix(&prefix)
                            .and_then(|tail| tail.parse::<u64>().ok())
                        {
                            linked.insert(number);
                        }
                    }
                }
            }
        }
        Ok(linked)
    }

    /// None means the diff exceeded the local or GitHub limit; no partial evidence is returned.
    pub async fn pr_diff(&self, repo: &str, number: u64) -> Result<Option<String>> {
        let before = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(before["state"] == "open", "Candidate PR is no longer open");
        let key = (repo.to_owned(), number);
        let head = string(&before["head"], "sha")?;
        let base = string(&before["base"], "sha")?;
        if let Some(diff) = self.diffs.lock().await.get(&key, &head, &base) {
            return Ok(diff);
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
        let diff = candidate_diff(repo, number, output)?;
        let after = api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?;
        ensure!(
            after["head"]["sha"] == head
                && after["base"]["sha"] == base
                && after["state"] == "open",
            "Candidate PR changed during collection"
        );
        let mut cache = self.diffs.lock().await;
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
        body: Option<&str>,
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
        let comments = self.comments(repo, number).await?;
        let Some(body) = body else {
            // Remove only this account's marked triage comments when labels suffice.
            for comment in comments.iter().filter(|c| {
                c["user"]["login"] == self.login
                    && c["body"].as_str().is_some_and(|b| b.starts_with(MARKER))
            }) {
                let id = comment["id"].as_u64().context("Missing comment id")?;
                api(
                    &format!("repos/{repo}/issues/comments/{id}"),
                    "DELETE",
                    None,
                )
                .await?;
            }
            return Ok(());
        };
        let text = format!("{MARKER}\n{body}");
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

fn mentions_work(text: &str, repo: &str, number: u64) -> bool {
    [
        format!("#{number}"),
        format!("{repo}#{number}"),
        format!("https://github.com/{repo}/pull/{number}"),
        format!("https://github.com/{repo}/issues/{number}"),
    ]
    .iter()
    .any(|reference| {
        text.match_indices(reference).any(|(offset, _)| {
            let before = text[..offset].chars().next_back();
            let after = text[offset + reference.len()..].chars().next();
            before.is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '/' | '_' | '-' | '#' | '='))
                && after.is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-'))
        })
    })
}

async fn pages(endpoint: &str, max: usize) -> Result<Vec<Value>> {
    let mut result = vec![];
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    for page in 1.. {
        let value = api(
            &format!("{endpoint}{separator}per_page=100&page={page}"),
            "GET",
            None,
        )
        .await?;
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

/// Interpret only explicit size rejections as missing evidence. Other command
/// failures must still abort collection rather than being cached as oversized PRs.
fn candidate_diff(repo: &str, number: u64, output: LimitedOutput) -> Result<Option<String>> {
    if output.stdout_truncated {
        tracing::warn!(
            repo,
            candidate_pr = number,
            limit_bytes = PR_DIFF_LIMIT,
            "Candidate PR diff exceeds limit; coverage unresolved"
        );
        return Ok(None);
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("HTTP 406:")
            && (stderr.contains("the diff exceeded the maximum number of lines")
                || stderr
                    .lines()
                    .any(|line| line.trim() == "PullRequest.diff too_large"))
        {
            tracing::warn!(
                repo,
                candidate_pr = number,
                "Candidate PR diff exceeds GitHub's limit; coverage unresolved"
            );
            return Ok(None);
        }
        observe_rate_limit(&RateHeaders::default(), &stderr);
        anyhow::bail!("Fetching diff for {repo}#{number} failed: {stderr}");
    }
    Ok(Some(String::from_utf8(output.stdout)?))
}

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn references_require_exact_numbers_and_repository() {
        for text in [
            "PR: #2504",
            "See owner/repo#2504.",
            "[PR](https://github.com/owner/repo/pull/2504)",
            "https://github.com/owner/repo/issues/2504#issuecomment-1",
        ] {
            assert!(mentions_work(text, "owner/repo", 2504), "{text}");
        }
        for text in [
            "#25040",
            "other/repo#2504",
            "https://github.com/other/repo/pull/2504",
            "word#2504",
            "#2504abc",
        ] {
            assert!(!mentions_work(text, "owner/repo", 2504), "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_cache_evicts_only_the_least_recently_used_entry() {
        let mut cache = DiffCache::default();
        let key = |number| ("owner/repo".to_owned(), number);
        let diff = |text: Option<&str>| CachedDiff {
            head: "head".into(),
            base: "base".into(),
            diff: text.map(str::to_owned),
        };
        for number in 0..128 {
            cache.insert(key(number), diff(Some("complete diff")));
        }
        assert!(cache.get(&key(0), "head", "base").is_some());
        cache.insert(key(128), diff(None));
        assert!(cache.get(&key(1), "head", "base").is_none());
        for number in (2..128).chain([0]) {
            assert_eq!(
                cache.get(&key(number), "head", "base"),
                Some(Some("complete diff".into()))
            );
        }
        assert_eq!(cache.get(&key(128), "head", "base"), Some(None));
        // Updating an existing entry must neither duplicate it nor evict a peer.
        cache.insert(key(128), diff(Some("replacement")));
        assert_eq!(cache.entries.len(), 128);
        assert!(cache.get(&key(2), "head", "base").is_some());
        assert_eq!(
            cache.get(&key(128), "head", "base"),
            Some(Some("replacement".into()))
        );
        cache.clear();
        assert!(cache.get(&key(128), "head", "base").is_none());
    }

    #[test]
    fn diff_cache_invalidates_either_revision_and_separates_repositories() {
        let mut cache = DiffCache::default();
        let key = ("owner/repo".to_owned(), 1);
        for (head, base) in [("new-head", "base"), ("head", "new-base")] {
            cache.insert(
                key.clone(),
                CachedDiff {
                    head: "head".into(),
                    base: "base".into(),
                    diff: None,
                },
            );
            assert!(
                cache
                    .get(&("other/repo".to_owned(), 1), "head", "base")
                    .is_none()
            );
            assert!(cache.get(&key, head, base).is_none());
            assert!(cache.get(&key, "head", "base").is_none());
        }
    }

    #[cfg(unix)]
    mod diffs {
        use std::{os::unix::process::ExitStatusExt, process::ExitStatus};

        use super::*;

        fn output(success: bool, stdout: &[u8], stderr: &str, truncated: bool) -> LimitedOutput {
            LimitedOutput {
                status: ExitStatus::from_raw(if success { 0 } else { 256 }),
                stdout: stdout.to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                stdout_truncated: truncated,
            }
        }

        #[test]
        fn github_size_rejection_is_unresolved_evidence() {
            let stderr = "could not find pull request diff: HTTP 406: Sorry, the diff exceeded the maximum number of lines (20000) (https://api.github.com/repos/cashubtc/cdk/pulls/2479)\nPullRequest.diff too_large";
            for message in [
                stderr,
                "HTTP 406: Sorry, the diff exceeded the maximum number of lines (20000)",
                "HTTP 406: Not Acceptable\nPullRequest.diff too_large",
            ] {
                assert_eq!(
                    candidate_diff(
                        "cashubtc/cdk",
                        2479,
                        output(false, b"partial", message, false)
                    )
                    .unwrap(),
                    None
                );
            }
        }

        #[test]
        fn unrelated_failures_remain_errors() {
            for stderr in [
                "HTTP 406: Not Acceptable",
                "HTTP 401: Bad credentials",
                "HTTP 403: Resource not accessible by integration",
                "HTTP 502: Bad Gateway",
                "could not find pull request",
                "connection reset by peer",
                "PullRequest.diff too_large",
            ] {
                let error =
                    candidate_diff("owner/repo", 1, output(false, b"", stderr, false)).unwrap_err();
                assert!(error.to_string().contains(stderr));
                assert!(error.to_string().contains("owner/repo#1"));
            }
        }

        #[test]
        fn local_truncation_discards_partial_diff_even_when_process_was_killed() {
            for success in [true, false] {
                assert_eq!(
                    candidate_diff("owner/repo", 1, output(success, b"partial", "", true)).unwrap(),
                    None
                );
            }
        }

        #[test]
        fn complete_diff_is_preserved_and_invalid_utf8_rejected() {
            let diff = b"diff --git a/file b/file\n+HTTP 406: PullRequest.diff too_large";
            assert_eq!(
                candidate_diff("owner/repo", 1, output(true, diff, "", false)).unwrap(),
                Some(String::from_utf8(diff.to_vec()).unwrap())
            );
            assert!(candidate_diff("owner/repo", 1, output(true, &[0xff], "", false)).is_err());
        }
    }

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
