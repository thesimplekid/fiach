use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    forge::{
        DiscoveryRequest, Forge, ForgeFuture, PullRequestSummary, ReviewComment, SyncPullRequest,
    },
    process::{CommandExt as _, LONG_COMMAND_TIMEOUT},
    reporting,
};

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
        command.kill_on_drop(true);
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

impl Forge for GhCli {
    fn discover<'a>(
        &'a self,
        request: DiscoveryRequest<'a>,
    ) -> ForgeFuture<'a, Vec<PullRequestSummary>> {
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

    fn pr_context<'a>(&'a self, repository: &'a str, pr: u64) -> ForgeFuture<'a, Value> {
        Box::pin(async move {
            self.json(&[
                "pr".to_string(),
                "view".to_string(),
                pr.to_string(),
                "--repo".to_string(),
                repository.to_string(),
                "--json".to_string(),
                "headRefOid,headRefName,baseRefOid,baseRefName,state,mergedAt,title,body"
                    .to_string(),
            ])
            .await
        })
    }

    fn discussion<'a>(&'a self, repository: &'a str, pr: u64) -> ForgeFuture<'a, Value> {
        Box::pin(async move {
            self.json(&[
                "pr".to_string(),
                "view".to_string(),
                pr.to_string(),
                "--repo".to_string(),
                repository.to_string(),
                "--json".to_string(),
                "author,body,createdAt,comments,reviews".to_string(),
            ])
            .await
        })
    }

    fn react<'a>(&'a self, repository: &'a str, pr: u64, reaction: &'a str) -> ForgeFuture<'a, ()> {
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
    ) -> ForgeFuture<'a, Value> {
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

    fn post_comment<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        body: &'a str,
    ) -> ForgeFuture<'a, String> {
        Box::pin(async move {
            let output = self
                .output_with_input(
                    "gh",
                    &[
                        "api".into(),
                        "--method".into(),
                        "POST".into(),
                        format!("repos/{repository}/issues/{pr}/comments"),
                        "--input".into(),
                        "-".into(),
                    ],
                    None,
                    Some(&serde_json::to_vec(&serde_json::json!({"body": body}))?),
                )
                .await?;
            let value: Value = serde_json::from_slice(&output)?;
            Ok(value["html_url"]
                .as_str()
                .context("Missing comment URL")?
                .to_owned())
        })
    }

    fn review_comments<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
    ) -> ForgeFuture<'a, Vec<ReviewComment>> {
        Box::pin(async move {
            // One JSON object per line keeps paginated output parseable.
            let output = self
                .output(
                    "gh",
                    &[
                        "api".into(),
                        "--paginate".into(),
                        format!("repos/{repository}/pulls/{pr}/comments"),
                        "--jq".into(),
                        ".[] | {id, body, in_reply_to_id, author: .user.login, author_association}"
                            .into(),
                    ],
                    None,
                )
                .await?;
            parse_review_comments(&output)
        })
    }

    fn reply_to_review_comment<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        comment: u64,
        body: &'a str,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            self.output_with_input(
                "gh",
                &[
                    "api".into(),
                    "--method".into(),
                    "POST".into(),
                    format!("repos/{repository}/pulls/{pr}/comments/{comment}/replies"),
                    "--input".into(),
                    "-".into(),
                ],
                None,
                Some(&serde_json::to_vec(&serde_json::json!({"body": body}))?),
            )
            .await?;
            Ok(())
        })
    }

    fn clone_workspace<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        destination: &'a Path,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            self.clone_repository(repository, destination).await?;
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

    fn create_sync_pr<'a>(&'a self, request: SyncPullRequest<'a>) -> ForgeFuture<'a, String> {
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

fn parse_review_comments(output: &[u8]) -> Result<Vec<ReviewComment>> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("Failed to parse review comment"))
        .collect()
}

fn split_repo_name(repo: &str) -> Result<(&str, &str)> {
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() => Ok((owner, name)),
        _ => anyhow::bail!("Repository must be in owner/name form: {repo}"),
    }
}

fn build_author_associations_query(numbers: &[u64]) -> String {
    let mut query = String::from(
        "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {",
    );

    for number in numbers {
        query.push_str(&format!(
            " pr{number}: pullRequest(number: {number}) {{ authorAssociation }}"
        ));
    }

    query.push_str(" } }");
    query
}

#[derive(Deserialize)]
struct AuthorAssociationsResponse {
    data: AuthorAssociationsData,
}

#[derive(Deserialize)]
struct AuthorAssociationsData {
    repository: HashMap<String, Option<AuthorAssociationPullRequest>>,
}

#[derive(Deserialize)]
struct AuthorAssociationPullRequest {
    #[serde(rename = "authorAssociation")]
    author_association: String,
}

async fn fetch_author_associations(repo: &str, numbers: &[u64]) -> Result<HashMap<u64, String>> {
    let (owner, name) = split_repo_name(repo)?;
    let mut associations = HashMap::new();

    for chunk in numbers.chunks(50) {
        let query = build_author_associations_query(chunk);
        let output = Command::new("gh")
            .args(["api", "graphql", "-f", &format!("owner={owner}")])
            .args(["-f", &format!("name={name}")])
            .args(["-f", &format!("query={query}")])
            .output_bounded("loading pull request author associations")
            .await
            .context("Failed to run gh api graphql")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api graphql failed for {repo}: {stderr}");
        }

        let response: AuthorAssociationsResponse = serde_json::from_slice(&output.stdout)
            .context("Failed to parse author association GraphQL response")?;

        for (alias, pr) in response.data.repository {
            let Some(number) = alias
                .strip_prefix("pr")
                .and_then(|number| number.parse::<u64>().ok())
            else {
                continue;
            };

            if let Some(pr) = pr {
                associations.insert(number, pr.author_association);
            }
        }
    }

    Ok(associations)
}

async fn fetch_existing_pr_comments(
    repo: &str,
    pr_number: u64,
) -> Result<Vec<reporting::ExistingPrComment>> {
    let mut comments = Vec::new();
    let issue_endpoint = format!("repos/{repo}/issues/{pr_number}/comments");
    for value in gh_api_paginated_array(&issue_endpoint).await? {
        if let Some(comment) = reporting::ExistingPrComment::from_issue_comment(&value) {
            comments.push(comment);
        }
    }

    let inline_endpoint = format!("repos/{repo}/pulls/{pr_number}/comments");
    for value in gh_api_paginated_array(&inline_endpoint).await? {
        if let Some(comment) = reporting::ExistingPrComment::from_inline_comment(&value) {
            comments.push(comment);
        }
    }

    let reviews_endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews");
    for value in gh_api_paginated_array(&reviews_endpoint).await? {
        if let Some(comment) = reporting::ExistingPrComment::from_review(&value) {
            comments.push(comment);
        }
    }

    Ok(comments)
}

async fn gh_api_paginated_array(endpoint: &str) -> Result<Vec<serde_json::Value>> {
    let output = tokio::process::Command::new("gh")
        .args(["api", "--paginate", "--slurp", endpoint])
        .output_bounded("loading pull request discussion")
        .await
        .with_context(|| format!("Failed to run `gh api` for {endpoint}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api failed for {endpoint}: {stderr}");
    }

    let pages: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("Failed to parse paginated gh response")?;
    let mut values = Vec::new();
    for page in pages {
        match page {
            serde_json::Value::Array(items) => values.extend(items),
            other => values.push(other),
        }
    }
    Ok(values)
}

fn sanitize_stderr(stderr: &str) -> String {
    stderr
        .replace(['\n', '\r'], " ")
        .chars()
        .take(500)
        .collect()
}

impl GhCli {
    pub async fn clone_repository(&self, repo: &str, destination: &Path) -> Result<()> {
        Self::with_timeout(LONG_COMMAND_TIMEOUT)
            .output(
                "gh",
                &[
                    "repo".into(),
                    "clone".into(),
                    repo.into(),
                    destination.to_string_lossy().into_owned(),
                ],
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn repository_context(&self, repo: &str) -> Result<Value> {
        self.json(&[
            "repo".into(),
            "view".into(),
            repo.into(),
            "--json".into(),
            "defaultBranchRef".into(),
        ])
        .await
    }

    pub async fn author_associations(
        &self,
        repo: &str,
        numbers: &[u64],
    ) -> Result<HashMap<u64, String>> {
        fetch_author_associations(repo, numbers).await
    }

    pub async fn existing_comments(
        &self,
        repo: &str,
        pr: u64,
    ) -> Result<Vec<reporting::ExistingPrComment>> {
        fetch_existing_pr_comments(repo, pr).await
    }

    pub async fn find_sync_pr(
        &self,
        repo: &str,
        head: &str,
        base: &str,
    ) -> Result<Option<crate::forge::ExistingSyncPr>> {
        let prs: Vec<crate::forge::ExistingSyncPr> = self
            .json(&[
                "pr".into(),
                "list".into(),
                "--repo".into(),
                repo.into(),
                "--head".into(),
                head.into(),
                "--state".into(),
                "open".into(),
                "--json".into(),
                "number,url,baseRefName".into(),
            ])
            .await?;
        if let Some(pr) = prs.iter().find(|pr| pr.base_ref_name == base) {
            return Ok(Some(pr.clone()));
        }
        if let Some(mut pr) = prs.into_iter().next() {
            self.output(
                "gh",
                &[
                    "pr".into(),
                    "edit".into(),
                    pr.number.to_string(),
                    "--repo".into(),
                    repo.into(),
                    "--base".into(),
                    base.into(),
                ],
                None,
            )
            .await?;
            pr.base_ref_name = base.to_owned();
            return Ok(Some(pr));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paginated_review_comments_parse_line_by_line() {
        let output = br#"{"id":1,"body":"root","in_reply_to_id":null,"author":"bot","author_association":"NONE"}
{"id":2,"body":"reply","in_reply_to_id":1,"author":null,"author_association":null}

"#;
        assert_eq!(
            parse_review_comments(output).unwrap(),
            vec![
                ReviewComment {
                    id: 1,
                    body: "root".into(),
                    in_reply_to_id: None,
                    author: Some("bot".into()),
                    author_association: Some("NONE".into()),
                },
                ReviewComment {
                    id: 2,
                    body: "reply".into(),
                    in_reply_to_id: Some(1),
                    author: None,
                    author_association: None,
                },
            ]
        );
        assert!(parse_review_comments(b"not json").is_err());
    }

    #[test]
    fn parse_open_sync_prs_reads_base_branch() {
        let prs = serde_json::from_slice::<Vec<crate::forge::ExistingSyncPr>>(
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
            vec![crate::forge::ExistingSyncPr {
                number: 5,
                url: "https://github.com/thesimplekid/cdk-reviews/pull/5".to_string(),
                base_ref_name: "main".to_string(),
            }]
        );
    }

    #[test]
    fn parse_open_sync_prs_rejects_invalid_json() {
        let result = serde_json::from_slice::<Vec<crate::forge::ExistingSyncPr>>(b"not json");

        assert!(result.is_err());
    }

    #[test]
    fn split_repo_name_requires_owner_and_name() {
        assert_eq!(split_repo_name("owner/repo").unwrap(), ("owner", "repo"));
        assert!(split_repo_name("owner").is_err());
        assert!(split_repo_name("/repo").is_err());
        assert!(split_repo_name("owner/").is_err());
    }

    #[test]
    fn author_associations_query_uses_pr_aliases() {
        let query = build_author_associations_query(&[1, 42]);

        assert!(query.contains("repository(owner: $owner, name: $name)"));
        assert!(query.contains("pr1: pullRequest(number: 1) { authorAssociation }"));
        assert!(query.contains("pr42: pullRequest(number: 42) { authorAssociation }"));
    }

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
