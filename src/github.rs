use std::{future::Future, path::Path, pin::Pin, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub type GitHubFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequestSummary {
    pub number: u64,
    #[serde(rename = "headRefOid")]
    pub head_ref_oid: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(default, rename = "authorAssociation")]
    pub author_association: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveryRequest<'a> {
    pub repository: &'a str,
    pub state: &'a str,
    pub search: &'a str,
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct SyncPullRequest<'a> {
    pub repository: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

pub trait GitHub: Send + Sync {
    fn discover<'a>(
        &'a self,
        request: DiscoveryRequest<'a>,
    ) -> GitHubFuture<'a, Vec<PullRequestSummary>>;
    fn pr_context<'a>(&'a self, repository: &'a str, pr: u64) -> GitHubFuture<'a, Value>;
    fn discussion<'a>(&'a self, repository: &'a str, pr: u64) -> GitHubFuture<'a, Value>;
    fn react<'a>(&'a self, repository: &'a str, pr: u64, reaction: &'a str)
    -> GitHubFuture<'a, ()>;
    fn publish_review<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        payload: &'a Value,
    ) -> GitHubFuture<'a, Value>;
    fn clone_workspace<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        destination: &'a Path,
    ) -> GitHubFuture<'a, ()>;
    fn create_sync_pr<'a>(&'a self, request: SyncPullRequest<'a>) -> GitHubFuture<'a, String>;
}

#[derive(Debug, Clone)]
pub struct GhCli {
    timeout: Duration,
}

impl Default for GhCli {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl GhCli {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    async fn output(&self, program: &str, args: &[String], cwd: Option<&Path>) -> Result<Vec<u8>> {
        self.output_with_input(program, args, cwd, None).await
    }

    async fn output_with_input(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let mut command = Command::new(program);
        command.args(args);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to start {program}"))?;
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin
                .write_all(input)
                .await
                .with_context(|| format!("Failed to write input to {program}"))?;
        }
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .with_context(|| format!("{program} command timed out"))?
            .with_context(|| format!("Failed to wait for {program}"))?;
        if !output.status.success() {
            let stderr = sanitize_stderr(&String::from_utf8_lossy(&output.stderr));
            anyhow::bail!("{program} command failed: {stderr}");
        }
        Ok(output.stdout)
    }

    async fn json<T: DeserializeOwned>(&self, args: &[String]) -> Result<T> {
        let output = self.output("gh", args, None).await?;
        serde_json::from_slice(&output).context("Failed to parse gh JSON output")
    }
}

impl GitHub for GhCli {
    fn discover<'a>(
        &'a self,
        request: DiscoveryRequest<'a>,
    ) -> GitHubFuture<'a, Vec<PullRequestSummary>> {
        Box::pin(async move {
            let mut args = vec![
                "pr".to_string(),
                "list".to_string(),
                "--repo".to_string(),
                request.repository.to_string(),
                "--state".to_string(),
                request.state.to_string(),
            ];
            if !request.search.is_empty() {
                args.extend(["--search".to_string(), request.search.to_string()]);
            }
            args.extend([
                "--limit".to_string(),
                request.limit.to_string(),
                "--json".to_string(),
                "number,headRefOid,headRefName,title".to_string(),
            ]);
            self.json(&args).await
        })
    }

    fn pr_context<'a>(&'a self, repository: &'a str, pr: u64) -> GitHubFuture<'a, Value> {
        Box::pin(async move {
            self.json(&[
                "pr".to_string(),
                "view".to_string(),
                pr.to_string(),
                "--repo".to_string(),
                repository.to_string(),
                "--json".to_string(),
                "headRefOid,headRefName,baseRefOid,baseRefName,state,mergedAt,title".to_string(),
            ])
            .await
        })
    }

    fn discussion<'a>(&'a self, repository: &'a str, pr: u64) -> GitHubFuture<'a, Value> {
        Box::pin(async move {
            self.json(&[
                "pr".to_string(),
                "view".to_string(),
                pr.to_string(),
                "--repo".to_string(),
                repository.to_string(),
                "--json".to_string(),
                "body,comments,reviews".to_string(),
            ])
            .await
        })
    }

    fn react<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        reaction: &'a str,
    ) -> GitHubFuture<'a, ()> {
        Box::pin(async move {
            self.output(
                "gh",
                &[
                    "api".to_string(),
                    "--method".to_string(),
                    "POST".to_string(),
                    format!("repos/{repository}/issues/{pr}/reactions"),
                    "-f".to_string(),
                    format!("content={reaction}"),
                ],
                None,
            )
            .await?;
            Ok(())
        })
    }

    fn publish_review<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        payload: &'a Value,
    ) -> GitHubFuture<'a, Value> {
        Box::pin(async move {
            let output = self
                .output_with_input(
                    "gh",
                    &[
                        "api".to_string(),
                        "--method".to_string(),
                        "POST".to_string(),
                        format!("repos/{repository}/pulls/{pr}/reviews"),
                        "--input".to_string(),
                        "-".to_string(),
                    ],
                    None,
                    Some(&serde_json::to_vec(payload)?),
                )
                .await?;
            serde_json::from_slice(&output).context("Failed to parse published review response")
        })
    }

    fn clone_workspace<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        destination: &'a Path,
    ) -> GitHubFuture<'a, ()> {
        Box::pin(async move {
            let destination_text = destination.to_string_lossy().to_string();
            self.output(
                "gh",
                &[
                    "repo".to_string(),
                    "clone".to_string(),
                    repository.to_string(),
                    destination_text,
                    "--".to_string(),
                    "--quiet".to_string(),
                ],
                None,
            )
            .await?;
            self.output(
                "gh",
                &[
                    "pr".to_string(),
                    "checkout".to_string(),
                    pr.to_string(),
                    "--detach".to_string(),
                ],
                Some(destination),
            )
            .await?;
            Ok(())
        })
    }

    fn create_sync_pr<'a>(&'a self, request: SyncPullRequest<'a>) -> GitHubFuture<'a, String> {
        Box::pin(async move {
            let output = self
                .output(
                    "gh",
                    &[
                        "pr".to_string(),
                        "create".to_string(),
                        "--repo".to_string(),
                        request.repository.to_string(),
                        "--head".to_string(),
                        request.head.to_string(),
                        "--base".to_string(),
                        request.base.to_string(),
                        "--title".to_string(),
                        request.title.to_string(),
                        "--body".to_string(),
                        request.body.to_string(),
                    ],
                    None,
                )
                .await?;
            Ok(String::from_utf8_lossy(&output).trim().to_string())
        })
    }
}

fn sanitize_stderr(stderr: &str) -> String {
    stderr
        .replace(['\n', '\r'], " ")
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_is_single_line_and_bounded() {
        let input = format!("secret\n{}", "x".repeat(600));
        let sanitized = sanitize_stderr(&input);
        assert!(!sanitized.contains('\n'));
        assert_eq!(sanitized.chars().count(), 500);
    }

    #[test]
    fn adapter_timeout_is_configurable() {
        let adapter = GhCli::with_timeout(Duration::from_secs(3));
        assert_eq!(adapter.timeout, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn adapter_captures_stdout() {
        let adapter = GhCli::with_timeout(Duration::from_secs(3));
        let output = adapter
            .output(
                "sh",
                &["-c".to_string(), "printf 'captured output'".to_string()],
                None,
            )
            .await
            .unwrap();

        assert_eq!(output, b"captured output");
    }

    #[tokio::test]
    async fn adapter_reports_captured_stderr() {
        let adapter = GhCli::with_timeout(Duration::from_secs(3));
        let error = adapter
            .output(
                "sh",
                &[
                    "-c".to_string(),
                    "printf 'captured error' >&2; exit 1".to_string(),
                ],
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "sh command failed: captured error");
    }
}
