//! Forgejo REST adapter. Responses are normalized at the boundary so review
//! policy, staging, and persistent disclosure recovery remain forge-independent.

use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Method, header::HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::Command;

use crate::{
    forge::{
        DiscoveryRequest, ExistingSyncPr, Forge, ForgeFuture, PullRequestSummary, Repository,
        SyncPullRequest, valid_component,
    },
    process::{CommandExt as _, LONG_COMMAND_TIMEOUT},
    reporting::ExistingPrComment,
};

const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const MAX_PAGES: usize = 1_000;
const DISCUSSION_LIMIT: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;

// Deliberately no Debug: token must never appear in logs.
pub struct Forgejo {
    repository: Repository,
    api_root: String,
    http: Client,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    repo: Option<BranchRepository>,
}

#[derive(Debug, Deserialize)]
struct BranchRepository {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    // Required: absence must not turn a merged PR into a publishable open PR.
    merged: bool,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    draft: bool,
    head: Branch,
    base: Branch,
    user: User,
    created_at: String,
    updated_at: String,
    html_url: String,
}

impl PullRequest {
    fn context(&self) -> Result<Value> {
        ensure!(
            matches!(self.state.as_str(), "open" | "closed"),
            "Unknown Forgejo PR lifecycle"
        );
        // merged=true always forbids publication, even if merged_at is absent.
        let state = if self.merged {
            "MERGED"
        } else if self.state == "open" {
            "OPEN"
        } else {
            "CLOSED"
        };
        Ok(json!({
            "number": self.number, "title": self.title, "body": self.body.as_deref().unwrap_or_default(),
            "state": state, "mergedAt": self.merged_at,
            "headRefOid": self.head.sha, "headRefName": self.head.name,
            "baseRefOid": self.base.sha, "baseRefName": self.base.name,
        }))
    }

    fn summary(self) -> PullRequestSummary {
        PullRequestSummary {
            number: self.number,
            head_ref_oid: self.head.sha,
            head_ref_name: self.head.name,
            title: self.title,
            author_association: String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: u64,
    #[serde(default)]
    body: Option<String>,
    user: Option<User>,
    html_url: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    position: Option<u64>,
}

impl Comment {
    fn existing(&self, kind: &str) -> Option<ExistingPrComment> {
        let body = self.body.as_deref().unwrap_or_default().trim();
        if body.is_empty() {
            return None;
        }
        Some(ExistingPrComment {
            id: self.id,
            kind: kind.to_owned(),
            author: self.user.as_ref().map(|u| u.login.clone()),
            body: body.to_owned(),
            path: self.path.clone(),
            line: self.position.filter(|line| *line > 0),
            url: Some(self.html_url.clone()),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MentionSubject {
    pub repository: String,
    pub comment: u64,
}

impl MentionSubject {
    fn encode(&self) -> Result<String> {
        Ok(format!("forgejo:{}", serde_json::to_string(self)?))
    }

    pub fn decode(value: &str) -> Result<Option<Self>> {
        value
            .strip_prefix("forgejo:")
            .map(|value| serde_json::from_str(value).context("Invalid Forgejo mention subject"))
            .transpose()
    }
}

fn scoped_token(
    host: &str,
    configured_host: Option<&str>,
    token: Option<String>,
) -> Result<Option<String>> {
    let token = token.filter(|token| !token.is_empty());
    if token.is_some() {
        let configured = configured_host
            .context("FORGEJO_TOKEN requires FORGEJO_HOST to bind the credential to its host")?;
        ensure!(
            configured == host,
            "FORGEJO_HOST does not match the repository host"
        );
    }
    Ok(token)
}

impl Forgejo {
    pub fn new(repository: Repository) -> Result<Self> {
        let host = repository
            .host
            .as_deref()
            .context("Forgejo repository needs a host")?;
        let token = scoped_token(
            host,
            std::env::var("FORGEJO_HOST").ok().as_deref(),
            std::env::var("FORGEJO_TOKEN").ok(),
        )?;
        Self::connect(repository, token)
    }

    fn connect(repository: Repository, token: Option<String>) -> Result<Self> {
        let host = repository
            .host
            .as_deref()
            .context("Forgejo repository needs a host")?;
        let api_root = format!("https://{host}/api/v1/repos/{}", repository.slug);
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("fiach/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Building Forgejo HTTP client")?;
        Ok(Self {
            repository,
            api_root,
            http,
            token,
        })
    }

    fn check_repository(&self, repository: &str) -> Result<()> {
        ensure!(
            Repository::parse(repository)? == self.repository,
            "Forgejo adapter repository mismatch"
        );
        Ok(())
    }

    async fn api<T: DeserializeOwned>(
        &self,
        method: Method,
        suffix: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let bytes = self.request(method, suffix, body).await?;
        serde_json::from_slice(&bytes).context("Invalid Forgejo API response")
    }

    async fn request(&self, method: Method, suffix: &str, body: Option<Value>) -> Result<Vec<u8>> {
        if method != Method::GET {
            ensure!(
                self.token.is_some(),
                "Forgejo writes require FORGEJO_HOST and FORGEJO_TOKEN"
            );
        }
        let mut request = self
            .http
            .request(method.clone(), format!("{}{suffix}", self.api_root));
        if let Some(token) = &self.token {
            let mut header = HeaderValue::from_str(&format!("token {token}"))
                .context("Invalid Forgejo credential")?;
            header.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, header);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.context("Forgejo request failed")?;
        // Do not include response bodies in errors: a server may echo credentials.
        ensure!(
            response.status().is_success(),
            "Forgejo {method} {suffix} returned HTTP {}",
            response.status()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= RESPONSE_LIMIT as u64),
            "Forgejo response exceeded size limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("Reading Forgejo response")? {
            ensure!(
                bytes.len() + chunk.len() <= RESPONSE_LIMIT,
                "Forgejo response exceeded size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn page<T: DeserializeOwned>(
        &self,
        suffix: &str,
        page: usize,
    ) -> Result<(Vec<T>, usize)> {
        let separator = if suffix.contains('?') { '&' } else { '?' };
        let bytes = self
            .request(
                Method::GET,
                &format!("{suffix}{separator}limit={PAGE_SIZE}&page={page}"),
                None,
            )
            .await?;
        let values = serde_json::from_slice(&bytes).context("Invalid Forgejo page")?;
        Ok((values, bytes.len()))
    }

    async fn pages<T: DeserializeOwned>(&self, suffix: &str) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut total_bytes = 0;
        for page in 1..=MAX_PAGES {
            let (values, bytes) = self.page(suffix, page).await?;
            total_bytes += bytes;
            ensure!(
                total_bytes <= DISCUSSION_LIMIT,
                "Forgejo paginated evidence exceeded size limit"
            );
            // Do not assume the server honored our requested page size.
            if values.is_empty() {
                return Ok(items);
            }
            items.extend(values);
        }
        bail!("Forgejo pagination exceeded {MAX_PAGES} pages; refusing incomplete evidence")
    }

    async fn pull(&self, pr: u64) -> Result<PullRequest> {
        self.api(Method::GET, &format!("/pulls/{pr}"), None).await
    }

    pub async fn repository_context(&self) -> Result<Value> {
        #[derive(Deserialize)]
        struct Context {
            default_branch: String,
        }
        let context: Context = self.api(Method::GET, "", None).await?;
        ensure!(
            !context.default_branch.is_empty(),
            "Missing Forgejo default branch"
        );
        Ok(json!({"defaultBranchRef": {"name": context.default_branch}}))
    }

    pub fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        command.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(token) = &self.token {
            // Git's per-command configuration, not a persisted helper. The fixed
            // helper reads the secret from its environment, never from argv.
            const HELPER: &str = r#"!f() { if [ "$1" = get ]; then printf '%s\n' username=fiach "password=$FIACH_FORGEJO_GIT_TOKEN"; fi; }; f"#;
            command
                .env("FIACH_FORGEJO_GIT_TOKEN", token)
                .env("GIT_CONFIG_COUNT", "4")
                .env("GIT_CONFIG_KEY_0", "credential.helper")
                .env("GIT_CONFIG_VALUE_0", "")
                .env(
                    "GIT_CONFIG_KEY_1",
                    format!("credential.{}.helper", self.repository.web_url()),
                )
                .env("GIT_CONFIG_VALUE_1", HELPER)
                .env("GIT_CONFIG_KEY_2", "credential.useHttpPath")
                .env("GIT_CONFIG_VALUE_2", "true")
                .env("GIT_CONFIG_KEY_3", "http.followRedirects")
                .env("GIT_CONFIG_VALUE_3", "false");
        }
        command
    }

    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<()> {
        let mut command = self.git_command();
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command
            .output_with_timeout("running Forgejo Git transport", LONG_COMMAND_TIMEOUT)
            .await?;
        ensure!(
            output.status.success(),
            "Forgejo Git transport failed ({})",
            output.status
        );
        Ok(())
    }

    pub async fn clone_repository(&self, destination: &Path) -> Result<()> {
        self.git(
            &[
                "clone",
                "--quiet",
                "--",
                &self.repository.web_url(),
                destination
                    .to_str()
                    .context("Non-UTF-8 clone destination")?,
            ],
            None,
        )
        .await
    }

    async fn association(&self, login: &str) -> Result<String> {
        ensure!(valid_component(login), "Invalid Forgejo login");
        let owner = self.repository.slug.split('/').next().unwrap_or_default();
        if login.eq_ignore_ascii_case(owner) {
            return Ok("OWNER".to_owned());
        }
        #[derive(Deserialize)]
        struct Permission {
            permission: String,
        }
        let result: Result<Permission> = self
            .api(
                Method::GET,
                &format!("/collaborators/{login}/permission"),
                None,
            )
            .await;
        Ok(match result {
            Ok(permission) => match permission.permission.as_str() {
                "admin" | "write" | "owner" => "COLLABORATOR",
                "read" | "none" => "NONE",
                _ => "UNKNOWN",
            },
            Err(error) => {
                tracing::warn!(user = login, error = %error, "Could not establish Forgejo author permissions; failing closed");
                "UNKNOWN"
            }
        }.to_owned())
    }

    pub async fn author_associations(&self, numbers: &[u64]) -> Result<HashMap<u64, String>> {
        let mut result = HashMap::new();
        let mut users: HashMap<String, String> = HashMap::new();
        for number in numbers {
            let pr = self.pull(*number).await?;
            let association = if let Some(value) = users.get(&pr.user.login) {
                value.clone()
            } else {
                let value = self.association(&pr.user.login).await?;
                users.insert(pr.user.login, value.clone());
                value
            };
            result.insert(*number, association);
        }
        Ok(result)
    }

    async fn normalized_comment(
        &self,
        comment: &Comment,
        is_review: bool,
        users: &mut HashMap<String, String>,
    ) -> Result<Value> {
        let association = if let Some(user) = &comment.user {
            if let Some(value) = users.get(&user.login) {
                value.clone()
            } else {
                let value = self.association(&user.login).await?;
                users.insert(user.login.clone(), value.clone());
                value
            }
        } else {
            "UNKNOWN".to_owned()
        };
        let subject = if is_review {
            None
        } else {
            Some(
                MentionSubject {
                    repository: self.repository.web_url(),
                    comment: comment.id,
                }
                .encode()?,
            )
        };
        Ok(json!({
            "id": subject, "body": comment.body.as_deref().unwrap_or_default(), "url": comment.html_url,
            "author": comment.user.as_ref().map(|user| json!({"login": user.login})),
            "authorAssociation": association, "createdAt": comment.created_at, "submittedAt": comment.submitted_at,
        }))
    }

    async fn has_mention(&self, pr: &PullRequest, mention: &str) -> Result<bool> {
        if crate::daemon::mention_matches(pr.body.as_deref().unwrap_or_default(), mention) {
            return Ok(true);
        }
        for suffix in [
            format!("/issues/{}/comments", pr.number),
            format!("/pulls/{}/reviews", pr.number),
        ] {
            for comment in self.pages::<Comment>(&suffix).await? {
                if crate::daemon::mention_matches(
                    comment.body.as_deref().unwrap_or_default(),
                    mention,
                ) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub async fn existing_comments(&self, pr: u64) -> Result<Vec<ExistingPrComment>> {
        let mut result = self
            .pages::<Comment>(&format!("/issues/{pr}/comments"))
            .await?
            .into_iter()
            .filter_map(|comment| comment.existing("top-level"))
            .collect::<Vec<_>>();
        for review in self
            .pages::<Comment>(&format!("/pulls/{pr}/reviews"))
            .await?
        {
            if let Some(comment) = review.existing("review") {
                result.push(comment);
            }
            // This endpoint is unpaginated in Forgejo's API schema.
            let inline: Vec<Comment> = self
                .api(
                    Method::GET,
                    &format!("/pulls/{pr}/reviews/{}/comments", review.id),
                    None,
                )
                .await?;
            result.extend(
                inline
                    .into_iter()
                    .filter_map(|comment| comment.existing("inline")),
            );
        }
        Ok(result)
    }

    pub async fn post_comment(&self, pr: u64, body: &str) -> Result<String> {
        let value: Comment = self
            .api(
                Method::POST,
                &format!("/issues/{pr}/comments"),
                Some(json!({"body": body})),
            )
            .await?;
        Ok(value.html_url)
    }

    pub async fn react_to_comment(&self, comment: u64, content: &str, remove: bool) -> Result<()> {
        self.request(
            if remove { Method::DELETE } else { Method::POST },
            &format!("/issues/comments/{comment}/reactions"),
            Some(json!({"content": content})),
        )
        .await?;
        Ok(())
    }

    pub async fn find_sync_pr(&self, head: &str, base: &str) -> Result<Option<ExistingSyncPr>> {
        let prs: Vec<Option<PullRequest>> =
            self.pages("/pulls?state=open&sort=recentupdate").await?;
        let mut matches: Vec<_> = prs
            .into_iter()
            .flatten()
            .filter(|pr| {
                pr.head.name == head
                    && pr.head.repo.as_ref().is_some_and(|repo| {
                        repo.full_name.eq_ignore_ascii_case(&self.repository.slug)
                    })
            })
            .collect();
        matches.sort_by_key(|pr| pr.base.name != base);
        if let Some(pr) = matches.into_iter().next() {
            if pr.base.name != base {
                self.request(
                    Method::PATCH,
                    &format!("/pulls/{}", pr.number),
                    Some(json!({"base": base})),
                )
                .await?;
            }
            return Ok(Some(ExistingSyncPr {
                number: pr.number,
                url: pr.html_url,
                base_ref_name: base.to_owned(),
            }));
        }
        Ok(None)
    }
}

#[derive(Default)]
struct DiscoveryFilter {
    updated_since: Option<OffsetDateTime>,
    exclude_drafts: bool,
    mention: Option<String>,
}

impl DiscoveryFilter {
    fn parse(search: &str) -> Result<Self> {
        let mut filter = Self::default();
        for term in search.split_whitespace() {
            if let Some(date) = term.strip_prefix("updated:>=") {
                filter.updated_since = Some(
                    OffsetDateTime::parse(&format!("{date}T00:00:00Z"), &Rfc3339)
                        .context("Invalid discovery cutoff")?,
                );
            } else if term == "draft:false" {
                filter.exclude_drafts = true;
            } else if let Some(mention) = term.strip_prefix("mentions:") {
                ensure!(valid_component(mention), "Invalid Forgejo mention filter");
                filter.mention = Some(mention.to_owned());
            } else if term.starts_with("state:") {
                // State is handled below. Mention authorization is applied by
                // the daemon against the complete discussion, not API search.
            } else {
                bail!("Unsupported Forgejo discovery filter: {term}");
            }
        }
        Ok(filter)
    }

    fn matches(&self, pr: &PullRequest, state: &str) -> Result<bool> {
        let lifecycle = match state {
            "open" => pr.state == "open" && !pr.merged,
            "closed" => pr.state == "closed" && !pr.merged,
            "merged" => pr.merged,
            "all" => true,
            _ => bail!("Unsupported Forgejo PR state: {state}"),
        };
        let title = pr.title.to_ascii_lowercase();
        let draft = pr.draft
            || title.starts_with("wip:")
            || title.starts_with("[wip]")
            || title.starts_with("draft:");
        let recent = match self.updated_since {
            Some(cutoff) => {
                OffsetDateTime::parse(&pr.updated_at, &Rfc3339)
                    .context("Invalid Forgejo updated_at")?
                    >= cutoff
            }
            None => true,
        };
        Ok(lifecycle && recent && !(self.exclude_drafts && draft))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewPayload {
    commit_id: String,
    event: String,
    body: String,
    comments: Vec<ReviewComment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewComment {
    path: String,
    line: u32,
    side: String,
    body: String,
}

fn review_payload(value: &Value) -> Result<Value> {
    let payload: ReviewPayload =
        serde_json::from_value(value.clone()).context("Invalid review payload")?;
    // Forgejo names the approval state APPROVED rather than APPROVE.
    let event = match payload.event.as_str() {
        "COMMENT" => "COMMENT",
        "APPROVE" => "APPROVED",
        "REQUEST_CHANGES" => "REQUEST_CHANGES",
        other => bail!("Unsupported review event {other}"),
    };
    let mut comments = Vec::new();
    for comment in payload.comments {
        ensure!(
            comment.line > 0 && comment.side == "RIGHT",
            "Invalid Forgejo inline anchor"
        );
        comments.push(json!({"path": comment.path, "body": comment.body, "new_position": comment.line, "old_position": 0}));
    }
    Ok(
        json!({"commit_id": payload.commit_id, "event": event, "body": payload.body, "comments": comments}),
    )
}

impl Forge for Forgejo {
    fn discover<'a>(
        &'a self,
        request: DiscoveryRequest<'a>,
    ) -> ForgeFuture<'a, Vec<PullRequestSummary>> {
        Box::pin(async move {
            self.check_repository(request.repository)?;
            let filter = DiscoveryFilter::parse(request.search)?;
            let state = if request.state == "merged" {
                "closed"
            } else {
                request.state
            };
            ensure!(
                matches!(state, "open" | "closed" | "all"),
                "Invalid Forgejo discovery state"
            );
            let mut result = Vec::new();
            if request.limit == 0 {
                return Ok(result);
            }
            for page in 1..=MAX_PAGES {
                let (values, _): (Vec<Option<PullRequest>>, _) = self
                    .page(&format!("/pulls?state={state}&sort=recentupdate"), page)
                    .await?;
                if values.is_empty() {
                    return Ok(result);
                }
                for pr in values.into_iter().flatten() {
                    if filter.matches(&pr, request.state)? {
                        if let Some(mention) = &filter.mention
                            && !self.has_mention(&pr, mention).await?
                        {
                            continue;
                        }
                        result.push(pr.summary());
                        if result.len() >= request.limit as usize {
                            return Ok(result);
                        }
                    }
                }
            }
            bail!("Forgejo discovery exceeded pagination limit")
        })
    }

    fn pr_context<'a>(&'a self, repo: &'a str, pr: u64) -> ForgeFuture<'a, Value> {
        Box::pin(async move {
            self.check_repository(repo)?;
            self.pull(pr).await?.context()
        })
    }

    fn discussion<'a>(&'a self, repo: &'a str, pr: u64) -> ForgeFuture<'a, Value> {
        Box::pin(async move {
            self.check_repository(repo)?;
            let pull = self.pull(pr).await?;
            let mut users = HashMap::new();
            let mut comments = Vec::new();
            for comment in self
                .pages::<Comment>(&format!("/issues/{pr}/comments"))
                .await?
            {
                comments.push(self.normalized_comment(&comment, false, &mut users).await?);
            }
            let mut reviews = Vec::new();
            for review in self
                .pages::<Comment>(&format!("/pulls/{pr}/reviews"))
                .await?
            {
                reviews.push(self.normalized_comment(&review, true, &mut users).await?);
            }
            Ok(
                json!({"body": pull.body.unwrap_or_default(), "author": {"login": pull.user.login}, "createdAt": pull.created_at, "comments": comments, "reviews": reviews}),
            )
        })
    }

    fn react<'a>(&'a self, repo: &'a str, pr: u64, reaction: &'a str) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            self.check_repository(repo)?;
            self.request(
                Method::POST,
                &format!("/issues/{pr}/reactions"),
                Some(json!({"content": reaction})),
            )
            .await?;
            Ok(())
        })
    }

    fn publish_review<'a>(
        &'a self,
        repo: &'a str,
        pr: u64,
        payload: &'a Value,
    ) -> ForgeFuture<'a, Value> {
        Box::pin(async move {
            self.check_repository(repo)?;
            self.api(
                Method::POST,
                &format!("/pulls/{pr}/reviews"),
                Some(review_payload(payload)?),
            )
            .await
        })
    }

    fn post_comment<'a>(
        &'a self,
        repo: &'a str,
        pr: u64,
        body: &'a str,
    ) -> ForgeFuture<'a, String> {
        Box::pin(async move {
            self.check_repository(repo)?;
            self.post_comment(pr, body).await
        })
    }

    fn review_comments<'a>(
        &'a self,
        repo: &'a str,
        _pr: u64,
    ) -> ForgeFuture<'a, Vec<crate::forge::ReviewComment>> {
        // The Forgejo API cannot reply in a review thread, so there are no
        // threads to answer.
        Box::pin(async move {
            self.check_repository(repo)?;
            Ok(Vec::new())
        })
    }

    fn reply_to_review_comment<'a>(
        &'a self,
        _repo: &'a str,
        _pr: u64,
        _comment: u64,
        _body: &'a str,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move { bail!("Forgejo does not support review thread replies") })
    }

    fn clone_workspace<'a>(
        &'a self,
        repo: &'a str,
        pr: u64,
        destination: &'a Path,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            self.check_repository(repo)?;
            self.clone_repository(destination).await?;
            self.git(
                &[
                    "fetch",
                    "--no-tags",
                    "origin",
                    &format!("refs/pull/{pr}/head"),
                ],
                Some(destination),
            )
            .await?;
            self.git(&["checkout", "--detach", "FETCH_HEAD"], Some(destination))
                .await
        })
    }

    fn create_sync_pr<'a>(&'a self, request: SyncPullRequest<'a>) -> ForgeFuture<'a, String> {
        Box::pin(async move {
            self.check_repository(request.repository)?;
            #[derive(Deserialize)]
            struct Created {
                html_url: String,
            }
            let created: Created = self.api(Method::POST, "/pulls", Some(json!({"head": request.head, "base": request.base, "title": request.title, "body": request.body}))).await?;
            Ok(created.html_url)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        process::Stdio,
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::State,
        http::{Request, Response, StatusCode},
        routing::any,
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};

    use super::*;

    const REPO: &str = "https://forge.test/owner/repo";

    fn pull(number: u64) -> Value {
        json!({
            "number": number, "title": "Change", "body": "PR body", "state": "open", "merged": false,
            "merged_at": null, "draft": false,
            "head": {"ref": "topic", "sha": "head-sha", "repo": {"full_name": "owner/repo"}},
            "base": {"ref": "main", "sha": "base-sha", "repo": {"full_name": "owner/repo"}},
            "user": {"login": "owner"}, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z",
            "html_url": format!("{REPO}/pulls/{number}"),
        })
    }

    fn comment(id: u64) -> Value {
        json!({"id": id, "body": "@bot please review", "user": {"login": "owner"},
            "html_url": format!("{REPO}/pulls/1#issuecomment-{id}"), "created_at": "2026-10-02T00:00:00Z"})
    }

    struct Reply {
        method: Method,
        path: String,
        status: StatusCode,
        response: Value,
        body: Option<Value>,
    }

    impl Reply {
        fn get(path: &str, response: Value) -> Self {
            Self {
                method: Method::GET,
                path: path.into(),
                status: StatusCode::OK,
                response,
                body: None,
            }
        }
        fn write(method: Method, path: &str, body: Value, response: Value) -> Self {
            Self {
                method,
                path: path.into(),
                status: StatusCode::OK,
                response,
                body: Some(body),
            }
        }
    }

    struct Mock {
        replies: Arc<Mutex<VecDeque<Reply>>>,
        task: JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Mock {
        async fn start(replies: Vec<Reply>) -> (Self, Forgejo) {
            async fn handler(
                State(replies): State<Arc<Mutex<VecDeque<Reply>>>>,
                request: Request<Body>,
            ) -> Response<Body> {
                let reply = replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request");
                assert_eq!(request.method(), reply.method);
                assert_eq!(
                    request.uri().path_and_query().unwrap().as_str(),
                    format!("/api/v1/repos/owner/repo{}", reply.path)
                );
                assert_eq!(request.headers()["authorization"], "token test-secret");
                let bytes = to_bytes(request.into_body(), RESPONSE_LIMIT).await.unwrap();
                if let Some(body) = reply.body {
                    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
                } else {
                    assert!(bytes.is_empty());
                }
                Response::builder()
                    .status(reply.status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&reply.response).unwrap()))
                    .unwrap()
            }
            let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = Router::new()
                .fallback(any(handler))
                .with_state(replies.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let client = Forgejo {
                repository: Repository::parse(REPO).unwrap(),
                api_root: format!("http://{address}/api/v1/repos/owner/repo"),
                http: Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
                token: Some("test-secret".into()),
            };
            (Self { replies, task }, client)
        }

        fn done(&self) {
            assert!(
                self.replies.lock().unwrap().is_empty(),
                "missing HTTP calls"
            );
        }
    }

    #[test]
    fn credentials_require_an_explicit_matching_host() {
        assert_eq!(
            scoped_token("forge.test", Some("forge.test"), Some("secret".into()))
                .unwrap()
                .as_deref(),
            Some("secret")
        );
        for host in [None, Some("other.test"), Some("https://forge.test")] {
            let error = scoped_token("forge.test", host, Some("secret".into())).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
        assert!(scoped_token("forge.test", None, None).unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "read-only smoke test against the public git.cashu.dev instance"]
    async fn public_git_cashu_smoke() {
        let repo = "https://git.cashu.dev/thesimplekid/fiach";
        let client = Forgejo::connect(Repository::parse(repo).unwrap(), None).unwrap();
        let prs = client
            .discover(DiscoveryRequest {
                repository: repo,
                state: "open",
                search: "draft:false",
                limit: 1,
            })
            .await
            .unwrap();
        let pr = prs
            .first()
            .expect("public repository needs an open PR for this smoke test");
        let context = client.pr_context(repo, pr.number).await.unwrap();
        assert_eq!(context["state"], "OPEN");
        let repository = client.repository_context().await.unwrap();
        assert!(repository["defaultBranchRef"]["name"].as_str().is_some());
        client.discussion(repo, pr.number).await.unwrap();
        client.existing_comments(pr.number).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("repo");
        client
            .clone_workspace(repo, pr.number, &checkout)
            .await
            .unwrap();
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&checkout)
            .output()
            .await
            .unwrap();
        assert!(head.status.success());
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            pr.head_ref_oid
        );
        let config = tokio::fs::read_to_string(checkout.join(".git/config"))
            .await
            .unwrap();
        assert!(!config.contains("credential"));
    }

    #[tokio::test]
    async fn discovery_paginates_past_filtered_results_and_server_page_cap() {
        let mut draft = pull(1);
        draft["draft"] = json!(true);
        let mut closed = pull(2);
        closed["state"] = json!("closed");
        let mut merged = pull(3);
        merged["state"] = json!("closed");
        merged["merged"] = json!(true);
        let (mock, client) = Mock::start(vec![
            Reply::get(
                "/pulls?state=closed&sort=recentupdate&limit=50&page=1",
                json!([draft]),
            ),
            Reply::get(
                "/pulls?state=closed&sort=recentupdate&limit=50&page=2",
                json!([closed]),
            ),
            Reply::get(
                "/pulls?state=closed&sort=recentupdate&limit=50&page=3",
                json!([merged]),
            ),
        ])
        .await;
        let prs = client
            .discover(DiscoveryRequest {
                repository: REPO,
                state: "merged",
                search: "state:merged draft:false updated:>=2026-09-01",
                limit: 1,
            })
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 3);
        assert_eq!(prs[0].head_ref_oid, "head-sha");
        mock.done();
    }

    #[tokio::test]
    async fn discovery_applies_mentions_before_the_result_limit() {
        let mut mentioned = pull(2);
        mentioned["body"] = json!("@bot please review");
        let (mock, client) = Mock::start(vec![
            Reply::get(
                "/pulls?state=open&sort=recentupdate&limit=50&page=1",
                json!([pull(1), mentioned]),
            ),
            Reply::get("/issues/1/comments?limit=50&page=1", json!([])),
            Reply::get("/pulls/1/reviews?limit=50&page=1", json!([])),
        ])
        .await;
        let prs = client
            .discover(DiscoveryRequest {
                repository: REPO,
                state: "open",
                search: "mentions:bot",
                limit: 1,
            })
            .await
            .unwrap();
        assert_eq!(prs[0].number, 2);
        mock.done();
    }

    #[test]
    fn lifecycle_is_fail_closed_and_preserves_head_and_base() {
        let mut value = pull(1);
        let context = serde_json::from_value::<PullRequest>(value.clone())
            .unwrap()
            .context()
            .unwrap();
        assert_eq!(context["state"], "OPEN");
        assert_eq!(context["headRefOid"], "head-sha");
        assert_eq!(context["baseRefOid"], "base-sha");
        assert!(context["mergedAt"].is_null());
        value["merged"] = json!(true);
        assert_eq!(
            serde_json::from_value::<PullRequest>(value.clone())
                .unwrap()
                .context()
                .unwrap()["state"],
            "MERGED"
        );
        value.as_object_mut().unwrap().remove("merged");
        assert!(serde_json::from_value::<PullRequest>(value.clone()).is_err());
        value["merged"] = json!(false);
        value["state"] = json!("unknown");
        assert!(
            serde_json::from_value::<PullRequest>(value)
                .unwrap()
                .context()
                .is_err()
        );
    }

    #[test]
    fn cutoff_drafts_and_unmerged_closed_pulls_are_distinct() {
        let filter = DiscoveryFilter::parse("updated:>=2026-09-01 draft:false").unwrap();
        let mut value = pull(1);
        value["updated_at"] = json!("2026-08-31T23:59:59Z");
        assert!(
            !filter
                .matches(&serde_json::from_value(value.clone()).unwrap(), "open")
                .unwrap()
        );
        value["updated_at"] = json!("2026-09-01T00:00:00Z");
        value["title"] = json!("WIP: change");
        assert!(
            !filter
                .matches(&serde_json::from_value(value.clone()).unwrap(), "open")
                .unwrap()
        );
        value["title"] = json!("Change");
        value["state"] = json!("closed");
        let pr = serde_json::from_value(value).unwrap();
        assert!(filter.matches(&pr, "closed").unwrap());
        assert!(!filter.matches(&pr, "merged").unwrap());
        assert!(DiscoveryFilter::parse("unsupported:filter").is_err());
    }

    #[tokio::test]
    async fn review_uses_forgejo_line_coordinates_and_reviewed_commit() {
        let (mock, client) = Mock::start(vec![Reply::write(Method::POST, "/pulls/1/reviews",
            json!({"commit_id": "reviewed", "event": "COMMENT", "body": "summary marker", "comments": [{"path": "src/lib.rs", "body": "finding", "new_position": 42, "old_position": 0}]}),
            json!({"html_url": "published"}),
        )]).await;
        let payload = json!({"commit_id": "reviewed", "event": "COMMENT", "body": "summary marker", "comments": [{"path": "src/lib.rs", "body": "finding", "line": 42, "side": "RIGHT"}]});
        assert_eq!(
            client.publish_review(REPO, 1, &payload).await.unwrap()["html_url"],
            "published"
        );
        let mut invalid = payload;
        invalid["comments"][0]["side"] = json!("LEFT");
        assert!(client.publish_review(REPO, 1, &invalid).await.is_err());
        mock.done();
    }

    #[test]
    fn review_events_use_forgejo_state_names() {
        let payload = |event: &str| json!({"commit_id": "reviewed", "event": event, "body": "summary", "comments": []});
        for (event, forgejo) in [
            ("COMMENT", "COMMENT"),
            ("APPROVE", "APPROVED"),
            ("REQUEST_CHANGES", "REQUEST_CHANGES"),
        ] {
            assert_eq!(review_payload(&payload(event)).unwrap()["event"], forgejo);
        }
        assert!(review_payload(&payload("PENDING")).is_err());
    }

    #[tokio::test]
    async fn discussion_normalizes_mentions_and_recovery_urls() {
        let mut review = comment(20);
        review["submitted_at"] = json!("2026-10-03T00:00:00Z");
        let (mock, client) = Mock::start(vec![
            Reply::get("/pulls/1", pull(1)),
            Reply::get("/issues/1/comments?limit=50&page=1", json!([comment(10)])),
            Reply::get("/issues/1/comments?limit=50&page=2", json!([])),
            Reply::get("/pulls/1/reviews?limit=50&page=1", json!([review])),
            Reply::get("/pulls/1/reviews?limit=50&page=2", json!([])),
        ])
        .await;
        let discussion = client.discussion(REPO, 1).await.unwrap();
        assert_eq!(discussion["comments"][0]["authorAssociation"], "OWNER");
        let subject = MentionSubject::decode(discussion["comments"][0]["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(subject.repository, REPO);
        assert_eq!(subject.comment, 10);
        assert_eq!(
            discussion["reviews"][0]["url"],
            format!("{REPO}/pulls/1#issuecomment-20")
        );
        assert!(discussion["reviews"][0]["id"].is_null());
        mock.done();
    }

    #[tokio::test]
    async fn existing_discussion_includes_inline_review_comments() {
        let mut inline = comment(30);
        inline["path"] = json!("src/lib.rs");
        inline["position"] = json!(42);
        let (mock, client) = Mock::start(vec![
            Reply::get("/issues/1/comments?limit=50&page=1", json!([comment(10)])),
            Reply::get("/issues/1/comments?limit=50&page=2", json!([])),
            Reply::get("/pulls/1/reviews?limit=50&page=1", json!([comment(20)])),
            Reply::get("/pulls/1/reviews?limit=50&page=2", json!([])),
            Reply::get("/pulls/1/reviews/20/comments", json!([inline])),
        ])
        .await;
        let comments = client.existing_comments(1).await.unwrap();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[2].kind, "inline");
        assert_eq!(comments[2].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(comments[2].line, Some(42));
        mock.done();
    }

    #[tokio::test]
    async fn permission_failures_never_become_trusted_associations() {
        let mut forbidden = Reply::get(
            "/collaborators/outsider/permission",
            json!({"message": "forbidden"}),
        );
        forbidden.status = StatusCode::FORBIDDEN;
        let (mock, client) = Mock::start(vec![
            forbidden,
            Reply::get(
                "/collaborators/writer/permission",
                json!({"permission": "write"}),
            ),
            Reply::get(
                "/collaborators/reader/permission",
                json!({"permission": "read"}),
            ),
        ])
        .await;
        assert_eq!(client.association("outsider").await.unwrap(), "UNKNOWN");
        assert_eq!(client.association("writer").await.unwrap(), "COLLABORATOR");
        assert_eq!(client.association("reader").await.unwrap(), "NONE");
        mock.done();
    }

    #[tokio::test]
    async fn sync_lookup_ignores_forks_and_retargets_own_report_branch() {
        let mut fork = pull(1);
        fork["head"]["repo"]["full_name"] = json!("outsider/repo");
        let (mock, client) = Mock::start(vec![
            Reply::get(
                "/pulls?state=open&sort=recentupdate&limit=50&page=1",
                json!([fork, pull(2)]),
            ),
            Reply::get(
                "/pulls?state=open&sort=recentupdate&limit=50&page=2",
                json!([]),
            ),
            Reply::write(
                Method::PATCH,
                "/pulls/2",
                json!({"base": "stable"}),
                json!({}),
            ),
        ])
        .await;
        let pr = client
            .find_sync_pr("topic", "stable")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pr.number, 2);
        assert_eq!(pr.base_ref_name, "stable");
        mock.done();
    }

    #[tokio::test]
    async fn comment_reaction_removal_accepts_empty_response() {
        let mut reply = Reply::write(
            Method::DELETE,
            "/issues/comments/10/reactions",
            json!({"content": "eyes"}),
            Value::Null,
        );
        reply.status = StatusCode::NO_CONTENT;
        let (mock, client) = Mock::start(vec![reply]).await;
        client.react_to_comment(10, "eyes", true).await.unwrap();
        mock.done();
    }

    #[tokio::test]
    async fn error_body_is_not_exposed_and_anonymous_writes_are_rejected() {
        let mut reply = Reply::get("/pulls/1", json!({"echo": "test-secret"}));
        reply.status = StatusCode::UNAUTHORIZED;
        let (mock, mut client) = Mock::start(vec![reply]).await;
        let error = client.pr_context(REPO, 1).await.unwrap_err().to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("test-secret"));
        client.token = None;
        assert!(
            client
                .react(REPO, 1, "eyes")
                .await
                .unwrap_err()
                .to_string()
                .contains("require")
        );
        mock.done();
    }

    #[tokio::test]
    async fn git_credentials_are_ephemeral_and_scoped_to_the_repository() {
        let (mock, client) = Mock::start(vec![]).await;
        for (host, path, permitted) in [
            ("forge.test", "owner/repo", true),
            ("other.test", "owner/repo", false),
            ("forge.test", "owner/other", false),
        ] {
            let mut cmd = client.git_command();
            cmd.args(["credential", "fill"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_ASKPASS")
                .env_remove("SSH_ASKPASS")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            assert!(!format!("{:?}", cmd.as_std().get_args()).contains("test-secret"));
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("protocol=https\nhost={host}\npath={path}\n\n").as_bytes())
                .await
                .unwrap();
            let output = child.wait_with_output().await.unwrap();
            assert_eq!(output.status.success(), permitted);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).contains("password=test-secret"),
                permitted
            );
        }
        mock.done();
    }
}
