//! Forge selection. Bare owner/name targets remain GitHub; HTTPS repository
//! URLs select Forgejo. The complete target is also the persistent state key.

use std::{future::Future, path::Path, pin::Pin};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{forgejo::Forgejo, github::GhCli};

pub type ForgeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

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

/// An inline review comment. Replies point at the thread's first comment.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: u64,
    pub body: String,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
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

pub trait Forge: Send + Sync {
    fn discover<'a>(
        &'a self,
        request: DiscoveryRequest<'a>,
    ) -> ForgeFuture<'a, Vec<PullRequestSummary>>;
    fn pr_context<'a>(&'a self, repository: &'a str, pr: u64) -> ForgeFuture<'a, Value>;
    fn discussion<'a>(&'a self, repository: &'a str, pr: u64) -> ForgeFuture<'a, Value>;
    fn react<'a>(&'a self, repository: &'a str, pr: u64, reaction: &'a str) -> ForgeFuture<'a, ()>;
    fn publish_review<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        payload: &'a Value,
    ) -> ForgeFuture<'a, Value>;
    fn post_comment<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        body: &'a str,
    ) -> ForgeFuture<'a, String>;
    /// Inline review comments of a PR. A forge that cannot reply in review
    /// threads returns none.
    fn review_comments<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
    ) -> ForgeFuture<'a, Vec<ReviewComment>>;
    fn reply_to_review_comment<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        comment: u64,
        body: &'a str,
    ) -> ForgeFuture<'a, ()>;
    fn clone_workspace<'a>(
        &'a self,
        repository: &'a str,
        pr: u64,
        destination: &'a Path,
    ) -> ForgeFuture<'a, ()>;
    fn create_sync_pr<'a>(&'a self, request: SyncPullRequest<'a>) -> ForgeFuture<'a, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// None selects GitHub. Forgejo hosts include an optional port.
    pub host: Option<String>,
    pub slug: String,
}

impl Repository {
    pub fn parse(target: &str) -> Result<Self> {
        let (host, slug) = if target.starts_with("https://") {
            let url = reqwest::Url::parse(target).context("Invalid repository URL")?;
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "Repository URLs must not contain credentials, queries, or fragments"
            );
            let host = url.host_str().context("Repository URL needs a host")?;
            ensure!(
                host != "github.com",
                "Use owner/name for GitHub repositories"
            );
            let authority = match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            let slug = url.path().trim_start_matches('/').to_owned();
            // Reject URL normalization (including dot segments), aliases and trailing slashes.
            ensure!(
                target == format!("https://{authority}/{slug}"),
                "Use a canonical HTTPS repository URL"
            );
            (Some(authority), slug)
        } else {
            (None, target.to_owned())
        };
        let parts: Vec<_> = slug.split('/').collect();
        ensure!(
            parts.len() == 2 && parts.iter().all(|part| valid_component(part)),
            "Repository must be owner/name (GitHub) or https://host/owner/name (Forgejo)"
        );
        ensure!(
            !slug.ends_with(".git"),
            "Use the repository URL without the .git suffix"
        );
        Ok(Self { host, slug })
    }

    pub fn web_url(&self) -> String {
        format!(
            "https://{}/{}",
            self.host.as_deref().unwrap_or("github.com"),
            self.slug
        )
    }

    pub fn pr_url(&self, number: u64) -> String {
        let segment = if self.host.is_some() { "pulls" } else { "pull" };
        format!("{}/{segment}/{number}", self.web_url())
    }

    /// Safe relative path, including host to keep different forges separate.
    pub fn storage_path(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}/{}", self.slug),
            None => self.slug.clone(),
        }
    }
}

pub(crate) fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn pr_url(repository: &str, number: u64) -> String {
    Repository::parse(repository)
        .map(|repo| repo.pr_url(number))
        .unwrap_or_else(|_| format!("{repository}#{number}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExistingSyncPr {
    pub number: u64,
    pub url: String,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: String,
}

/// Additional host operations live here rather than leaking forge-specific
/// commands into the review, staging, and disclosure workflows.
pub enum Client {
    GitHub(GhCli),
    Forgejo(Forgejo),
}

pub fn client(repository: &str) -> Result<Client> {
    let repo = Repository::parse(repository)?;
    Ok(match repo.host {
        Some(_) => Client::Forgejo(Forgejo::new(repo)?),
        None => Client::GitHub(GhCli::default()),
    })
}

impl Client {
    pub fn adapter(&self) -> &dyn Forge {
        match self {
            Self::GitHub(client) => client,
            Self::Forgejo(client) => client,
        }
    }

    pub async fn clone_repository(&self, repo: &str, destination: &Path) -> Result<()> {
        match self {
            Self::GitHub(client) => client.clone_repository(repo, destination).await,
            Self::Forgejo(client) => client.clone_repository(destination).await,
        }
    }

    pub async fn repository_context(&self, repo: &str) -> Result<Value> {
        match self {
            Self::GitHub(client) => client.repository_context(repo).await,
            Self::Forgejo(client) => client.repository_context().await,
        }
    }

    pub async fn post_comment(&self, repo: &str, pr: u64, body: &str) -> Result<String> {
        match self {
            Self::GitHub(client) => client.post_comment(repo, pr, body).await,
            Self::Forgejo(client) => client.post_comment(pr, body).await,
        }
    }

    pub async fn author_associations(
        &self,
        repo: &str,
        numbers: &[u64],
    ) -> Result<std::collections::HashMap<u64, String>> {
        match self {
            Self::GitHub(client) => client.author_associations(repo, numbers).await,
            Self::Forgejo(client) => client.author_associations(numbers).await,
        }
    }

    pub async fn existing_comments(
        &self,
        repo: &str,
        pr: u64,
    ) -> Result<Vec<crate::reporting::ExistingPrComment>> {
        match self {
            Self::GitHub(client) => client.existing_comments(repo, pr).await,
            Self::Forgejo(client) => client.existing_comments(pr).await,
        }
    }

    pub async fn find_sync_pr(
        &self,
        repo: &str,
        head: &str,
        base: &str,
    ) -> Result<Option<ExistingSyncPr>> {
        match self {
            Self::GitHub(client) => client.find_sync_pr(repo, head, base).await,
            Self::Forgejo(client) => client.find_sync_pr(head, base).await,
        }
    }

    /// Network Git operations receive ephemeral authentication only on the host.
    /// No token or credential helper is written into the staged checkout.
    pub fn git_command(&self) -> tokio::process::Command {
        match self {
            Self::GitHub(_) => tokio::process::Command::new("git"),
            Self::Forgejo(client) => client.git_command(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_identity_keeps_forges_separate() {
        assert!(Repository::parse("owner/.github").is_ok());
        let github = Repository::parse("owner/repo").unwrap();
        let forgejo = Repository::parse("https://git.cashu.dev/owner/repo").unwrap();
        assert_eq!(github.pr_url(42), "https://github.com/owner/repo/pull/42");
        assert_eq!(
            forgejo.pr_url(42),
            "https://git.cashu.dev/owner/repo/pulls/42"
        );
        assert_eq!(forgejo.storage_path(), "git.cashu.dev/owner/repo");
        assert_ne!(github.storage_path(), forgejo.storage_path());
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_repository_targets() {
        for target in [
            "",
            "owner",
            "../repo",
            "a/b/c",
            "-owner/repo",
            "o/r?x",
            "http://host/o/r",
            "https://token@host/o/r",
            "https://host/o/r?token=x",
            "https://host/o/r#x",
            "https://host/o/../r",
            "https://host/o/%72",
            "https://host/o/r/",
            "https://host/o/r.git",
            "https://github.com/o/r",
            "https://HOST/o/r",
        ] {
            assert!(Repository::parse(target).is_err(), "accepted {target}");
        }
    }
}
