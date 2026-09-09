use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::disclose::DiscloseConfig;
use crate::execution::{ReviewExecutor, ReviewOutcome};
use crate::finalizer::{FinalizationSpec, ReviewFinalizer};
use crate::github::{DiscoveryRequest, GhCli, GitHub, PullRequestSummary};
use crate::process::{CommandExt as _, LONG_COMMAND_TIMEOUT};
use crate::review::{CompletedReview, ReviewExecution, ReviewParams};
use crate::scheduler::{
    ExecutionStatus, ReviewRequest, ReviewTarget, SchedulerHandle, SubmitError,
};

const VETH_SUBNET_BASE_OCTETS: (u8, u8) = (10, 64);
const VETH_SUBNET_MIN_INDEX: u8 = 1;
const VETH_SUBNET_MAX_INDEX: u8 = 254;
const VETH_DNS_PRIMARY: &str = "1.1.1.1";
const VETH_DNS_SECONDARY: &str = "9.9.9.9";

static ACTIVE_VETH_SUBNETS: OnceLock<Mutex<HashSet<u8>>> = OnceLock::new();

#[derive(Clone)]
pub enum DaemonWork {
    Manual {
        repo: String,
        pr_number: u64,
        persona: Option<String>,
    },
    Poll {
        repo: String,
        job: ReviewJob,
        honor_mention_trigger: bool,
    },
}

pub struct DaemonParams {
    pub repos: String,
    pub interval: u64,
    pub provider: String,
    pub model: String,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub dedupe_existing_comments: bool,
    pub skill: Option<String>,
    pub personas: Vec<crate::persona::PersonaSource>,
    pub review_lanes: Vec<String>,
    pub review_lane_prompts: HashMap<String, String>,
    pub max_review_lanes: usize,
    pub max_turns: u32,
    pub timeout_mins: u64,
    pub db_path: PathBuf,
    pub max_retries: u32,
    pub retry_delay_secs: u64,
    pub out_dir: Option<PathBuf>,
    pub disclose_config: DiscloseConfig,
    pub buzz_config: Option<crate::config::BuzzConfig>,
    pub verify_findings: bool,
    pub context_groups: std::collections::HashMap<String, crate::config::ContextGroup>,
    pub pr_states: Vec<String>,
    pub skip_prs: Vec<String>,
    pub allowed_author_associations: Vec<String>,
    pub max_workers: usize,
    pub drafts: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    pub updated_within_days: u32,
    pub filter_by_updated: bool,
    pub trigger_mention: Option<String>,
    pub allowed_mention_users: Vec<String>,
    pub pr_limit: u32,
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

type PullRequest = PullRequestSummary;

#[derive(Clone)]
pub struct ReviewJob {
    pr: PullRequest,
    persona: crate::persona::PersonaSource,
    review_kind: String,
}

#[derive(Default)]
struct PendingPollReviews {
    requests: VecDeque<ReviewRequest<DaemonWork>>,
}

impl PendingPollReviews {
    fn refresh(&mut self, discoveries: Vec<ReviewRequest<DaemonWork>>) {
        let mut order = VecDeque::new();
        let mut latest = HashMap::new();
        for request in discoveries {
            let target = request.target.clone();
            if latest.insert(target.clone(), request).is_none() {
                order.push_back(target);
            }
        }

        // Keep deferred targets ahead of repeat discoveries, using fresh payloads.
        // Targets that no longer match discovery are dropped.
        self.requests = self
            .requests
            .drain(..)
            .map(|request| request.target)
            .chain(order)
            .filter_map(|target| latest.remove(&target))
            .collect();
    }

    async fn submit(
        &mut self,
        scheduler: &SchedulerHandle<DaemonWork>,
        cancel: &CancellationToken,
    ) -> Result<(), SubmitError> {
        while let Some(request) = self.requests.front() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SubmitError::Closed),
                result = scheduler.submit(request.clone()) => { result?; }
            }
            self.requests.pop_front();
        }
        Ok(())
    }

    async fn run(
        &mut self,
        scheduler: &SchedulerHandle<DaemonWork>,
        cancel: &CancellationToken,
        mut discoveries: tokio::sync::watch::Receiver<Vec<ReviewRequest<DaemonWork>>>,
    ) -> Result<(), SubmitError> {
        self.refresh(discoveries.borrow_and_update().clone());
        loop {
            match self.submit(scheduler, cancel).await {
                Ok(()) | Err(SubmitError::Full) => {}
                Err(SubmitError::Closed) => return Err(SubmitError::Closed),
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SubmitError::Closed),
                result = discoveries.changed() => {
                    if result.is_err() {
                        return Ok(());
                    }
                    self.refresh(discoveries.borrow_and_update().clone());
                }
                _ = scheduler.capacity_changed(), if !self.requests.is_empty() => {}
            }
        }
    }
}

pub fn manual_request(
    repo: String,
    pr_number: u64,
    persona: Option<String>,
) -> ReviewRequest<DaemonWork> {
    let review_kind = persona
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(
            |value| match crate::persona::PersonaSource::from_str(value) {
                Ok(source) => source.review_kind(),
                Err(never) => match never {},
            },
        )
        .unwrap_or_else(|| crate::state::DEFAULT_REVIEW_KIND.to_string());
    ReviewRequest {
        target: ReviewTarget {
            repository: repo.clone(),
            pr_number,
            commit_hash: "pending".to_string(),
            review_kind,
        },
        payload: DaemonWork::Manual {
            repo,
            pr_number,
            persona,
        },
    }
}

pub fn start_review_scheduler(
    params: std::sync::Arc<DaemonParams>,
    cancel_token: CancellationToken,
) -> SchedulerHandle<DaemonWork> {
    let workers = params.max_workers;
    let handler = std::sync::Arc::new(
        move |work: DaemonWork,
              cancel: CancellationToken|
              -> BoxFuture<'static, Result<ExecutionStatus>> {
            let params = std::sync::Arc::clone(&params);
            Box::pin(async move {
                let status = match work {
                    DaemonWork::Manual {
                        repo,
                        pr_number,
                        persona,
                    } => {
                        trigger_manual_review(&params, repo, pr_number, persona, cancel).await?;
                        PrProcessStatus::Reviewed
                    }
                    DaemonWork::Poll {
                        repo,
                        job,
                        honor_mention_trigger,
                    } => {
                        process_daemon_job(&params, &repo, &job, cancel, honor_mention_trigger)
                            .await?
                    }
                };
                Ok(match status {
                    PrProcessStatus::Reviewed => ExecutionStatus::Completed,
                    PrProcessStatus::Skipped => ExecutionStatus::Skipped,
                    PrProcessStatus::Failed => anyhow::bail!("review execution failed"),
                })
            })
        },
    );
    crate::scheduler::start(workers, cancel_token, handler)
}

struct SandboxReviewExecutor<'a> {
    daemon: &'a DaemonParams,
}

impl ReviewExecutor for SandboxReviewExecutor<'_> {
    fn execute(
        &self,
        spec: crate::execution::ReviewSpec,
        cancel: CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ReviewOutcome>>> + Send + '_>,
    > {
        Box::pin(async move {
            run_sandboxed_review(self.daemon, &spec.params, cancel)
                .await
                .map(Some)
        })
    }
}

fn is_allowed_author_association(association: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(association))
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

async fn populate_author_associations(repo: &str, prs: &mut [PullRequest]) -> Result<()> {
    let numbers: Vec<_> = prs.iter().map(|pr| pr.number).collect();
    let associations = fetch_author_associations(repo, &numbers).await?;

    for pr in prs {
        if let Some(association) = associations.get(&pr.number) {
            pr.author_association.clone_from(association);
        } else {
            tracing::warn!(
                repo = %repo,
                pr = pr.number,
                "Author association was missing from GraphQL response"
            );
            pr.author_association = "UNKNOWN".to_string();
        }
    }

    Ok(())
}

fn is_veth_network(params: &DaemonParams) -> bool {
    params.sandbox_rootfs.is_some() && effective_sandbox_network(params) == "veth"
}

fn effective_sandbox_network(params: &DaemonParams) -> &str {
    params.sandbox_network.as_deref().unwrap_or("veth")
}

fn validate_sandbox_network_capacity(params: &DaemonParams) -> Result<()> {
    if is_veth_network(params) {
        validate_veth_worker_capacity(params.max_workers)?;
    }

    Ok(())
}

fn validate_veth_worker_capacity(max_workers: usize) -> Result<()> {
    if max_workers == 0 || max_workers > usize::from(VETH_SUBNET_MAX_INDEX) {
        anyhow::bail!(
            "sandbox.networkMode = \"veth\" requires max_workers between 1 and {}; got {}",
            VETH_SUBNET_MAX_INDEX,
            max_workers
        );
    }

    Ok(())
}

#[derive(Debug)]
struct SandboxVethReservation {
    index: u8,
}

impl SandboxVethReservation {
    fn reserve(machine_name: &str) -> Result<Self> {
        let active_subnets = ACTIVE_VETH_SUBNETS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut active_subnets = active_subnets
            .lock()
            .map_err(|_| anyhow::anyhow!("sandbox veth subnet allocator mutex was poisoned"))?;

        Ok(Self {
            index: reserve_sandbox_veth_index(machine_name, &mut active_subnets)?,
        })
    }

    fn host_gateway(&self) -> String {
        format!(
            "{}.{}.{}.1",
            VETH_SUBNET_BASE_OCTETS.0, VETH_SUBNET_BASE_OCTETS.1, self.index
        )
    }

    fn host_cidr(&self) -> String {
        format!("{}/30", self.host_gateway())
    }

    fn guest_cidr(&self) -> String {
        format!(
            "{}.{}.{}.2/30",
            VETH_SUBNET_BASE_OCTETS.0, VETH_SUBNET_BASE_OCTETS.1, self.index
        )
    }
}

impl Drop for SandboxVethReservation {
    fn drop(&mut self) {
        if let Some(active_subnets) = ACTIVE_VETH_SUBNETS.get()
            && let Ok(mut active_subnets) = active_subnets.lock()
        {
            active_subnets.remove(&self.index);
        }
    }
}

fn sandbox_veth_start_index(machine_name: &str) -> u8 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    machine_name.hash(&mut hasher);
    VETH_SUBNET_MIN_INDEX
        + (hasher.finish() % u64::from(VETH_SUBNET_MAX_INDEX - VETH_SUBNET_MIN_INDEX + 1)) as u8
}

fn reserve_sandbox_veth_index(machine_name: &str, active_subnets: &mut HashSet<u8>) -> Result<u8> {
    let start = sandbox_veth_start_index(machine_name);

    for offset in 0..=u16::from(VETH_SUBNET_MAX_INDEX - VETH_SUBNET_MIN_INDEX) {
        let index = VETH_SUBNET_MIN_INDEX
            + ((u16::from(start - VETH_SUBNET_MIN_INDEX) + offset)
                % u16::from(VETH_SUBNET_MAX_INDEX - VETH_SUBNET_MIN_INDEX + 1)) as u8;

        if active_subnets.insert(index) {
            return Ok(index);
        }
    }

    anyhow::bail!(
        "No sandbox veth subnets are available in 10.64.{}.0/30 through 10.64.{}.0/30",
        VETH_SUBNET_MIN_INDEX,
        VETH_SUBNET_MAX_INDEX
    );
}

fn sandbox_host_interface_name(machine_name: &str) -> String {
    format!("ve-{machine_name}")
}

fn review_jobs(
    personas: &[crate::persona::PersonaSource],
    prs: &[PullRequest],
    use_persona_kind: bool,
) -> Vec<ReviewJob> {
    prs.iter()
        .flat_map(|pr| {
            personas.iter().map(move |persona| ReviewJob {
                pr: pr.clone(),
                persona: persona.clone(),
                review_kind: if use_persona_kind {
                    persona.review_kind()
                } else {
                    crate::state::DEFAULT_REVIEW_KIND.to_string()
                },
            })
        })
        .collect()
}

fn output_path_for_review(
    out_dir: Option<&Path>,
    repo: &str,
    pr_number: u64,
    commit_hash: &str,
    review_kind: &str,
) -> Option<PathBuf> {
    let safe_repo = repo.replace('/', "_");
    let hash = &commit_hash[..commit_hash.len().min(7)];
    let out_file_name = if review_kind == crate::state::DEFAULT_REVIEW_KIND {
        format!("{}_PR{}_{}_report.md", safe_repo, pr_number, hash)
    } else {
        format!(
            "{}_PR{}_{}_{}_report.md",
            safe_repo, pr_number, hash, review_kind
        )
    };

    out_dir.map(|dir| dir.join(out_file_name))
}

fn pr_search_query(
    state: &str,
    filter_by_updated: bool,
    updated_within_days: u32,
    drafts: Option<bool>,
    trigger_mention: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    if !state.eq_ignore_ascii_case("all") {
        parts.push(format!("state:{state}"));
    }

    if filter_by_updated {
        let time_ago = OffsetDateTime::now_utc() - time::Duration::days(updated_within_days.into());
        let date = time_ago.date();
        let search_date = format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        );
        parts.push(format!("updated:>={search_date}"));
    }

    if drafts == Some(false) {
        parts.push("draft:false".to_string());
    }

    if let Some(mention) = trigger_mention {
        parts.push(format!("mentions:{}", mention.trim_start_matches('@')));
    }

    parts.join(" ")
}

fn pr_list_state_arg(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "open" | "closed" | "merged" | "all" => state.to_ascii_lowercase(),
        _ => "all".to_string(),
    }
}

const MENTION_PR_VIEW_JSON_FIELDS: &str = "author,body,createdAt,comments,reviews";

#[derive(Debug, Deserialize)]
struct MentionAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct MentionSource {
    #[serde(default)]
    id: Option<String>,
    author: Option<MentionAuthor>,
    #[serde(default, rename = "authorAssociation")]
    author_association: String,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: Option<String>,
    #[serde(default, rename = "submittedAt")]
    submitted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrMentionDetails {
    author: Option<MentionAuthor>,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: Option<String>,
    #[serde(default)]
    comments: Vec<MentionSource>,
    #[serde(default)]
    reviews: Vec<MentionSource>,
}

async fn fetch_pr_mention_details(repo: &str, pr_number: u64) -> Result<PrMentionDetails> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            repo,
            "--json",
            MENTION_PR_VIEW_JSON_FIELDS,
        ])
        .output_bounded("loading pull request mention details")
        .await
        .context("Failed to run gh pr view for mention details")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh pr view failed for {repo}#{pr_number}: {stderr}");
    }

    serde_json::from_slice(&output.stdout).context("Failed to parse PR mention details")
}

fn mention_matches(text: &str, username: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let needle = format!("@{}", username.trim_start_matches('@').to_ascii_lowercase());
    let mut search_from = 0;

    while let Some(found) = text[search_from..].find(&needle) {
        let start = search_from + found;
        let end = start + needle.len();
        // Word boundaries so "@fiach-bot" does not match inside "a@fiach-bot"
        // or "@fiach-bot2" (GitHub usernames allow alphanumerics and hyphens).
        let ok_before = text[..start]
            .chars()
            .next_back()
            .map(|ch| !ch.is_ascii_alphanumeric())
            .unwrap_or(true);
        let ok_after = text[end..]
            .chars()
            .next()
            .map(|ch| !ch.is_ascii_alphanumeric() && ch != '-')
            .unwrap_or(true);

        if ok_before && ok_after {
            return true;
        }
        search_from = end;
    }

    false
}

fn is_allowed_mentioner(
    login: &str,
    association: &str,
    allowed_users: &[String],
    allowed_associations: &[String],
) -> bool {
    if allowed_users.is_empty() {
        is_allowed_author_association(association, allowed_associations)
    } else {
        allowed_users
            .iter()
            .any(|user| user.trim_start_matches('@').eq_ignore_ascii_case(login))
    }
}

fn parse_rfc3339_unix(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|timestamp| timestamp.unix_timestamp())
}

#[derive(Debug, PartialEq, Eq)]
struct ValidMention {
    timestamp: i64,
    /// GraphQL node id of the triggering comment or review, used to react to
    /// it directly. None when the mention is in the PR body, where the
    /// PR-level review-start reaction already provides feedback.
    subject_node_id: Option<String>,
}

/// Newest mention of the trigger account by an allowlisted user in the PR
/// body, a comment, or a review body.
fn latest_valid_mention(
    details: &PrMentionDetails,
    pr_author_association: &str,
    mention_user: &str,
    allowed_users: &[String],
    allowed_associations: &[String],
) -> Option<ValidMention> {
    let mut latest: Option<ValidMention> = None;

    let mut consider = |login: Option<&str>,
                        association: &str,
                        body: &str,
                        timestamp: Option<&str>,
                        node_id: Option<&str>| {
        let Some(login) = login else { return };
        let Some(timestamp) = timestamp.and_then(parse_rfc3339_unix) else {
            return;
        };
        if mention_matches(body, mention_user)
            && is_allowed_mentioner(login, association, allowed_users, allowed_associations)
            && latest
                .as_ref()
                .is_none_or(|current| timestamp > current.timestamp)
        {
            latest = Some(ValidMention {
                timestamp,
                subject_node_id: node_id.map(str::to_string),
            });
        }
    };

    consider(
        details.author.as_ref().map(|author| author.login.as_str()),
        pr_author_association,
        &details.body,
        details.created_at.as_deref(),
        None,
    );

    for source in details.comments.iter().chain(details.reviews.iter()) {
        consider(
            source.author.as_ref().map(|author| author.login.as_str()),
            &source.author_association,
            &source.body,
            source
                .created_at
                .as_deref()
                .or(source.submitted_at.as_deref()),
            source.id.as_deref(),
        );
    }

    latest
}

/// A mention is a one-shot trigger: it only (re)starts a review when it is
/// newer than the last recorded review attempt for this PR and review kind.
fn apply_mention_trigger(
    decision: crate::state::ReviewDecision,
    mention_ts: i64,
    last_review_ts: Option<i64>,
) -> crate::state::ReviewDecision {
    use crate::state::ReviewDecision::{ReReview, Skip};

    let mention_is_fresh = last_review_ts.is_none_or(|last| mention_ts > last);

    match decision {
        // Same commit already reviewed, but re-mentioned since: review again.
        Skip if mention_is_fresh => ReReview,
        // New commit but nobody has re-mentioned since the last review.
        ReReview if !mention_is_fresh => Skip,
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrProcessStatus {
    Reviewed,
    Skipped,
    Failed,
}

pub async fn run_daemon(
    params: std::sync::Arc<DaemonParams>,
    scheduler: SchedulerHandle<DaemonWork>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let repo_list: Vec<String> = params
        .repos
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if repo_list.is_empty() {
        anyhow::bail!("No repositories specified to monitor");
    }

    validate_sandbox_network_capacity(&params)?;

    let question_listener = params
        .buzz_config
        .as_ref()
        .and_then(|config| {
            config
                .questions
                .as_ref()
                .is_some_and(|questions| questions.enabled)
                .then(|| config.clone())
        })
        .map(|config| {
            let db_path = params.db_path.clone();
            let provider = params.provider.clone();
            let model = params.model.clone();
            let cancel = cancel_token.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    crate::buzz::run_question_listener(config, &db_path, &provider, &model, cancel)
                        .await
                {
                    tracing::error!(error = %error, "Buzz question listener stopped");
                }
            })
        });

    // Ensure gh is authenticated
    let gh_auth = Command::new("gh")
        .arg("auth")
        .arg("setup-git")
        .output_bounded("configuring Git authentication")
        .await;
    if let Err(e) = gh_auth {
        tracing::warn!("Failed to run gh auth setup-git: {}", e);
    }

    let sleep_duration = Duration::from_secs(params.interval);
    let github = GhCli::default();
    let (discovery_sender, discoveries) = tokio::sync::watch::channel(Vec::new());
    let submitter_cancel = cancel_token.child_token();
    let _submitter_guard = submitter_cancel.clone().drop_guard();
    let submitter = tokio::spawn(async move {
        PendingPollReviews::default()
            .run(&scheduler, &submitter_cancel, discoveries)
            .await
    });

    loop {
        if cancel_token.is_cancelled() {
            tracing::info!("Daemon shutting down");
            break;
        }

        tracing::debug!("Starting polling cycle");
        let mut discoveries = Vec::new();

        for repo in &repo_list {
            if cancel_token.is_cancelled() {
                break;
            }

            tracing::debug!(repo = %repo, "Checking for open PRs");

            for state in &params.pr_states {
                if cancel_token.is_cancelled() {
                    break;
                }

                let search_query = pr_search_query(
                    state,
                    params.filter_by_updated,
                    params.updated_within_days,
                    params.drafts,
                    params.trigger_mention.as_deref(),
                );

                let list_state = pr_list_state_arg(state);
                match github
                    .discover(DiscoveryRequest {
                        repository: repo,
                        state: &list_state,
                        search: &search_query,
                        limit: params.pr_limit,
                    })
                    .await
                {
                    Ok(mut prs) => {
                        if let Err(e) = populate_author_associations(repo, &mut prs).await {
                            tracing::error!(
                                repo = %repo,
                                state = %state,
                                error = %e,
                                "Failed to fetch PR author associations"
                            );
                            continue;
                        }

                        tracing::info!("Found {} recent {} PRs for {}", prs.len(), state, repo);

                        let jobs = review_jobs(
                            &params.personas,
                            &prs,
                            params.personas.len() > 1 || params.buzz_config.is_some(),
                        );
                        tracing::info!(
                            repo = %repo,
                            state = %state,
                            max_workers = params.max_workers,
                            personas = params.personas.len(),
                            "Collecting discovered review jobs"
                        );
                        for job in jobs {
                            let request = ReviewRequest {
                                target: ReviewTarget {
                                    repository: repo.clone(),
                                    pr_number: job.pr.number,
                                    commit_hash: job.pr.head_ref_oid.clone(),
                                    review_kind: job.review_kind.clone(),
                                },
                                payload: DaemonWork::Poll {
                                    repo: repo.clone(),
                                    job,
                                    honor_mention_trigger: true,
                                },
                            };
                            discoveries.push(request);
                        }
                    }
                    Err(error) => {
                        tracing::error!(repo = %repo, state = %state, error = %error, "Failed to discover PRs");
                    }
                }
            }
        }

        if cancel_token.is_cancelled() {
            break;
        }

        if discovery_sender.send(discoveries).is_err() {
            return Ok(());
        }
        tracing::debug!(
            interval_secs = params.interval,
            "Discovery complete, waiting for the next poll"
        );
        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            _ = cancel_token.cancelled() => {}
            _ = discovery_sender.closed() => return Ok(()),
        }
    }

    drop(discovery_sender);
    let _ = submitter.await;

    if let Some(listener) = question_listener {
        let _ = listener.await;
    }

    Ok(())
}

fn mark_skipped_if_needed(params: &DaemonParams, repo: &str, job: &ReviewJob) {
    #[allow(clippy::collapsible_if)]
    if let Ok(decision) = crate::state::should_review(
        &params.db_path,
        repo,
        job.pr.number,
        &job.pr.head_ref_oid,
        &job.review_kind,
        false,
        params.timeout_mins,
    ) {
        if decision != crate::state::ReviewDecision::Skip {
            let meta = crate::state::ReviewMetadata {
                repository: repo.to_string(),
                pr_number: job.pr.number,
                review_kind: job.review_kind.clone(),
                commit_hash: job.pr.head_ref_oid.clone(),
                model: "daemon".to_string(),
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                findings_count: 0,
                status: crate::state::ReviewStatus::Skipped,
                severity: "none".to_string(),
                pr_classification: "none".to_string(),
                duration_secs: 0,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: Some(0.0),
                report_url: None,
                is_rereview: decision == crate::state::ReviewDecision::ReReview,
                time_reviewed: Some(
                    time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                ),
                retry_count: 0,
                artifacts: crate::state::ArtifactPaths::default(),
                disclosure_url: None,
                buzz_thread: None,
                failure_stage: None,
            };
            let _ = crate::state::mark_reviewed(&params.db_path, repo, job.pr.number, &meta);
        }
    }
}

async fn process_daemon_job(
    params: &DaemonParams,
    repo: &str,
    job: &ReviewJob,
    cancel_token: CancellationToken,
    honor_mention_trigger: bool,
) -> Result<PrProcessStatus> {
    if cancel_token.is_cancelled() {
        return Ok(PrProcessStatus::Skipped);
    }

    let pr = &job.pr;
    let skip = params
        .skip_prs
        .iter()
        .any(|s| s == &pr.number.to_string() || s == &format!("{}#{}", repo, pr.number));

    if skip {
        tracing::info!(
            repo = %repo,
            pr = pr.number,
            review_kind = %job.review_kind,
            "Skipping PR as requested"
        );
        mark_skipped_if_needed(params, repo, job);
        return Ok(PrProcessStatus::Skipped);
    }

    let mention_user = if honor_mention_trigger {
        params.trigger_mention.as_deref()
    } else {
        None
    };

    // In mention-trigger mode the mentioner carries the trust, so the PR
    // author gate only applies when no mention trigger is in effect. This
    // lets a maintainer summon a review on e.g. a first-time contributor's PR.
    if mention_user.is_none()
        && !is_allowed_author_association(
            &pr.author_association,
            &params.allowed_author_associations,
        )
    {
        tracing::info!(
            repo = %repo,
            pr = pr.number,
            author_association = %pr.author_association,
            allowed = ?params.allowed_author_associations,
            "Skipping PR because author association is not allowed"
        );
        return Ok(PrProcessStatus::Skipped);
    }

    if pr.head_ref_name.starts_with("backport-") || pr.title.starts_with("[Backport") {
        tracing::info!(
            repo = %repo,
            pr = pr.number,
            review_kind = %job.review_kind,
            "Skipping backport PR"
        );
        mark_skipped_if_needed(params, repo, job);
        return Ok(PrProcessStatus::Skipped);
    }

    let mention = if let Some(mention_user) = mention_user {
        let details = match fetch_pr_mention_details(repo, pr.number).await {
            Ok(details) => details,
            Err(e) => {
                tracing::error!(
                    repo = %repo,
                    pr = pr.number,
                    error = %e,
                    "Failed to fetch PR mention details"
                );
                return Ok(PrProcessStatus::Failed);
            }
        };

        let Some(mention) = latest_valid_mention(
            &details,
            &pr.author_association,
            mention_user,
            &params.allowed_mention_users,
            &params.allowed_author_associations,
        ) else {
            tracing::info!(
                repo = %repo,
                pr = pr.number,
                mention_user = %mention_user,
                "Skipping PR because no allowlisted user has mentioned the trigger account"
            );
            return Ok(PrProcessStatus::Skipped);
        };
        Some(mention)
    } else {
        None
    };
    let mention_ts = mention.as_ref().map(|mention| mention.timestamp);

    let review_decision = crate::state::should_review_with_retry_limit(
        &params.db_path,
        repo,
        pr.number,
        &pr.head_ref_oid,
        &job.review_kind,
        params.timeout_mins,
        params.max_retries,
    );

    let review_decision = match review_decision {
        Ok(decision) => {
            if let Some(mention_ts) = mention_ts {
                let last_review_ts =
                    crate::state::get_pr_review(&params.db_path, repo, pr.number, &job.review_kind)
                        .ok()
                        .flatten()
                        .map(|meta| meta.timestamp);
                let effective = apply_mention_trigger(decision, mention_ts, last_review_ts);
                if effective != decision {
                    tracing::info!(
                        repo = %repo,
                        pr = pr.number,
                        review_kind = %job.review_kind,
                        original = ?decision,
                        effective = ?effective,
                        "Mention trigger adjusted review decision"
                    );
                }
                Ok(effective)
            } else {
                Ok(decision)
            }
        }
        Err(e) => Err(e),
    };

    match review_decision {
        Ok(
            decision @ (crate::state::ReviewDecision::FirstReview
            | crate::state::ReviewDecision::ReReview
            | crate::state::ReviewDecision::RetryFailed),
        ) => {
            let retry_count_for_attempt = if decision == crate::state::ReviewDecision::RetryFailed {
                crate::state::get_pr_review(&params.db_path, repo, pr.number, &job.review_kind)
                    .ok()
                    .flatten()
                    .map(|m| m.retry_count.saturating_add(1))
                    .unwrap_or(1)
            } else {
                0
            };
            let is_rereview = matches!(decision, crate::state::ReviewDecision::ReReview);

            match crate::state::lock_for_review(
                &params.db_path,
                repo,
                pr.number,
                &pr.head_ref_oid,
                &job.review_kind,
                params.timeout_mins,
            ) {
                Ok(true) => {
                    tracing::debug!(
                        repo = %repo,
                        pr = pr.number,
                        review_kind = %job.review_kind,
                        "Successfully locked PR for review"
                    );
                }
                Ok(false) => {
                    tracing::info!(
                        repo = %repo,
                        pr = pr.number,
                        review_kind = %job.review_kind,
                        "PR review is currently locked by another process, skipping"
                    );
                    return Ok(PrProcessStatus::Skipped);
                }
                Err(e) => {
                    tracing::error!("Failed to lock PR {} in {}: {}", pr.number, repo, e);
                    return Ok(PrProcessStatus::Failed);
                }
            }

            match decision {
                crate::state::ReviewDecision::RetryFailed => {
                    tracing::info!(
                        repo = %repo,
                        pr = pr.number,
                        commit = %pr.head_ref_oid,
                        review_kind = %job.review_kind,
                        "Retrying previously failed PR review"
                    );
                }
                _ => {
                    tracing::info!(
                        repo = %repo,
                        pr = pr.number,
                        commit = %pr.head_ref_oid,
                        review_kind = %job.review_kind,
                        "New PR or commit needs review"
                    );
                }
            }

            // Acknowledge the triggering comment so the mentioner can see the
            // review is starting. Mentions in the PR body have no node id and
            // are covered by the PR-level review-start reaction instead.
            if let Some(node_id) = mention
                .as_ref()
                .and_then(|mention| mention.subject_node_id.as_deref())
                && let Some(reaction) = params.disclose_config.reactions.review_start.as_deref()
                && let Err(error) = crate::disclose::post_mention_reaction(node_id, reaction).await
            {
                tracing::warn!(
                    repo = %repo,
                    pr = pr.number,
                    error = %error,
                    "Failed to react to trigger mention comment"
                );
            }

            let output_path = output_path_for_review(
                params.out_dir.as_deref(),
                repo,
                pr.number,
                &pr.head_ref_oid,
                &job.review_kind,
            );

            let review_params = ReviewParams {
                repo: repo.to_string(),
                pr_number: pr.number,
                provider: params.provider.clone(),
                model: params.model.clone(),
                verifier_provider: params.verifier_provider.clone(),
                verifier_model: params.verifier_model.clone(),
                dedupe_existing_comments: params.dedupe_existing_comments,
                output: output_path,
                skill: params.skill.clone(),
                persona: job.persona.clone(),
                review_lanes: params.review_lanes.clone(),
                review_lane_prompts: params.review_lane_prompts.clone(),
                max_review_lanes: params.max_review_lanes,
                max_turns: params.max_turns,
                timeout_mins: params.timeout_mins,
                db_path: params.db_path.clone(),
                review_kind: job.review_kind.clone(),
                force: false,
                max_retries: params.max_retries,
                retry_delay_secs: params.retry_delay_secs,
                disclose_config: params.disclose_config.clone(),
                buzz_config: params.buzz_config.clone(),
                verify_findings: params.verify_findings,
                context_groups: params.context_groups.clone(),
                max_cost_usd: params.max_cost_usd,
                input_price_per_m: params.input_price_per_m,
                output_price_per_m: params.output_price_per_m,
                is_rereview,
                trigger_mention_node_id: mention
                    .as_ref()
                    .and_then(|mention| mention.subject_node_id.clone()),
                execution: ReviewExecution {
                    skip_state_check: true,
                    persist_side_effects: false,
                    result_json: None,
                },
            };
            let finalization = FinalizationSpec::from(&review_params);

            if let Some(reaction) = review_params
                .disclose_config
                .reactions
                .review_start
                .as_deref()
                && let Err(error) = crate::disclose::post_pr_reaction(
                    &review_params.repo,
                    review_params.pr_number,
                    reaction,
                )
                .await
            {
                tracing::warn!(error = %error, "Failed to post review-start reaction");
            }

            let execution_result = if params.sandbox_rootfs.is_some() {
                SandboxReviewExecutor { daemon: params }
                    .execute(
                        crate::execution::ReviewSpec {
                            params: review_params,
                        },
                        cancel_token.clone(),
                    )
                    .await
            } else {
                crate::execution::LocalReviewExecutor
                    .execute(
                        crate::execution::ReviewSpec {
                            params: review_params,
                        },
                        cancel_token.clone(),
                    )
                    .await
            };

            let outcome = if let Err(e) = execution_result {
                let meta = crate::state::ReviewMetadata {
                    repository: repo.to_string(),
                    pr_number: pr.number,
                    review_kind: job.review_kind.clone(),
                    commit_hash: pr.head_ref_oid.clone(),
                    model: "daemon".to_string(),
                    timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                    findings_count: 0,
                    status: crate::state::ReviewStatus::Failed,
                    severity: "none".to_string(),
                    pr_classification: "none".to_string(),
                    duration_secs: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: Some(0.0),
                    report_url: None,
                    is_rereview,
                    time_reviewed: Some(
                        time::OffsetDateTime::now_utc()
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                    ),
                    retry_count: retry_count_for_attempt,
                    artifacts: crate::state::ArtifactPaths::default(),
                    disclosure_url: None,
                    buzz_thread: None,
                    failure_stage: Some("execution".to_string()),
                };
                let _ = crate::state::mark_reviewed(&params.db_path, repo, pr.number, &meta);

                if cancel_token.is_cancelled() {
                    return Err(e);
                }
                tracing::error!("Failed to review PR {} in {}: {}", pr.number, repo, e);
                if !crate::review::is_nonfatal_review_completion_error(&e)
                    && crate::review::is_fatal_error(&e)
                {
                    tracing::error!("Fatal error encountered, stopping daemon");
                    return Err(e);
                }
                return Ok(PrProcessStatus::Failed);
            } else {
                execution_result?
            };

            let Some(outcome) = outcome else {
                return Ok(PrProcessStatus::Skipped);
            };
            let mut failed_finalization = outcome.completed.metadata.clone();
            if let Err(error) = ReviewFinalizer
                .finalize(&finalization, outcome, cancel_token.clone())
                .await
            {
                failed_finalization.status = crate::state::ReviewStatus::Failed;
                failed_finalization.failure_stage = Some("finalization".to_string());
                failed_finalization.retry_count = retry_count_for_attempt;
                let _ = crate::state::mark_reviewed(
                    &params.db_path,
                    repo,
                    pr.number,
                    &failed_finalization,
                );
                tracing::error!(repo = %repo, pr = pr.number, error = %error, "Review finalization failed");
                return Ok(PrProcessStatus::Failed);
            }

            Ok(PrProcessStatus::Reviewed)
        }
        Ok(crate::state::ReviewDecision::Skip) => Ok(PrProcessStatus::Skipped),
        Err(e) => {
            tracing::error!(
                "Failed to check review state for PR {} in {}: {}",
                pr.number,
                repo,
                e
            );
            Ok(PrProcessStatus::Failed)
        }
    }
}

async fn trigger_manual_review(
    params: &DaemonParams,
    repo: String,
    pr_number: u64,
    persona_filter: Option<String>,
    cancel_token: CancellationToken,
) -> Result<()> {
    tracing::info!(repo = %repo, pr = pr_number, "Fetching manual PR details");

    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            &repo,
            "--json",
            "headRefOid,headRefName,title",
        ])
        .output_bounded("loading a manually requested pull request")
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch PR details: {}", stderr);
    }

    #[derive(Deserialize)]
    struct PrDetails {
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        title: String,
    }

    let pr_details: PrDetails =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR details")?;
    let mut associations = fetch_author_associations(&repo, &[pr_number]).await?;
    let author_association = associations
        .remove(&pr_number)
        .with_context(|| format!("Failed to fetch author association for PR #{pr_number}"))?;

    let selected_personas = if let Some(filter) = persona_filter {
        let requested = filter.trim();
        let personas: Vec<_> = params
            .personas
            .iter()
            .filter(|persona| {
                persona.review_kind() == requested || persona.to_string() == requested
            })
            .cloned()
            .collect();

        if personas.is_empty() {
            anyhow::bail!("No configured persona matches '{}'", requested);
        }
        personas
    } else {
        params.personas.clone()
    };

    let jobs = review_jobs(
        &selected_personas,
        &[PullRequest {
            number: pr_number,
            head_ref_oid: pr_details.head_ref_oid,
            head_ref_name: pr_details.head_ref_name,
            author_association,
            title: pr_details.title,
        }],
        params.personas.len() > 1 || params.buzz_config.is_some(),
    );

    // Manual triggers are deliberate, so they bypass the mention gate.
    for job in jobs {
        process_daemon_job(params, &repo, &job, cancel_token.clone(), false).await?;
    }

    Ok(())
}

async fn wait_for_link(interface: &str) -> Result<()> {
    for _ in 1..=100 {
        let output = Command::new("ip")
            .args(["link", "show", "dev", interface])
            .output_bounded("inspecting a sandbox veth interface")
            .await
            .with_context(|| format!("Failed to inspect sandbox veth interface {interface}"))?;

        if output.status.success() {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!("Timed out waiting for sandbox veth interface {interface}");
}

async fn run_ip_command(args: &[&str]) -> Result<()> {
    let output = Command::new("ip")
        .args(args)
        .output_bounded("configuring sandbox networking")
        .await
        .with_context(|| format!("Failed to run ip {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ip {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(())
}

async fn configure_sandbox_veth_host(
    machine_name: &str,
    network: &SandboxVethReservation,
) -> Result<()> {
    let interface = sandbox_host_interface_name(machine_name);
    let host_cidr = network.host_cidr();

    wait_for_link(&interface).await?;
    run_ip_command(&["addr", "replace", &host_cidr, "dev", &interface]).await?;
    run_ip_command(&["link", "set", "dev", &interface, "up"]).await?;

    Ok(())
}

fn validate_sandbox_review_token<'a>(
    review_token: &'a str,
    host_token: Option<&str>,
) -> Result<&'a str> {
    if review_token.trim().is_empty() {
        anyhow::bail!("FIACH_REVIEW_GITHUB_TOKEN must not be empty");
    }
    if host_token == Some(review_token) {
        anyhow::bail!("FIACH_REVIEW_GITHUB_TOKEN must be distinct from the host GITHUB_TOKEN");
    }

    Ok(review_token)
}

async fn run_sandboxed_review(
    params: &DaemonParams,
    review_params: &ReviewParams,
    cancel_token: CancellationToken,
) -> Result<ReviewOutcome> {
    let rootfs = params
        .sandbox_rootfs
        .as_ref()
        .context("Sandbox rootfs is required for sandboxed review execution")?;
    let network_mode = effective_sandbox_network(params);

    if !rootfs.exists() || !rootfs.is_dir() {
        anyhow::bail!(
            "Sandbox rootfs does not exist or is not a directory: {}",
            rootfs.display()
        );
    }

    #[allow(clippy::collapsible_if)]
    if !matches!(network_mode, "bridge" | "veth" | "private" | "host") {
        anyhow::bail!(
            "Invalid sandbox network mode: {}. Must be host, bridge, private, or veth.",
            network_mode
        );
    }

    let run_dir = sandbox_run_dir(
        params.out_dir.as_deref(),
        &review_params.repo,
        review_params.pr_number,
        &review_params.review_kind,
    )?;
    std::fs::create_dir_all(&run_dir).with_context(|| {
        format!(
            "Failed to create sandbox run directory at {}",
            run_dir.display()
        )
    })?;
    let report_path = run_dir.join("report.md");
    let result_json = run_dir.join("result.json");
    let nspawn_log = run_dir.join("nspawn.log");
    let runtime_rootfs = prepare_runtime_rootfs(rootfs, &run_dir).await?;
    let _runtime_rootfs_guard = RuntimeRootfsGuard::new(runtime_rootfs.clone());
    let sandbox_home = "/tmp";
    let sandbox_xdg_state_home = "/tmp/.local/state";

    for dir in [
        sandbox_xdg_state_home,
        "/tmp/.local/state/goose",
        "/tmp/.local/state/goose/logs",
        "/root/.local/state",
        "/root/.local/state/goose",
        "/root/.local/state/goose/logs",
    ] {
        let path = runtime_rootfs.join(dir.trim_start_matches('/'));
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "Failed to create sandbox runtime directory at {}",
                path.display()
            )
        })?;
    }

    let mut cmd = Command::new("systemd-nspawn");
    let machine_name = sandbox_machine_name_for_run(
        &review_params.repo,
        review_params.pr_number,
        &review_params.review_kind,
    );
    let veth_network = if network_mode == "veth" {
        Some(SandboxVethReservation::reserve(&machine_name)?)
    } else {
        None
    };
    cmd.arg(format!("--machine={machine_name}"));
    cmd.arg(format!("--directory={}", runtime_rootfs.display()));
    // --private-users=no: DynamicUser provides a transient UID without subuid/subgid
    // mappings, so nspawn's default --private-users=pick fails.
    // --keep-unit: prevent nspawn from registering a new transient scope with systemd,
    // which requires privileges a service unit doesn't have.
    cmd.arg("--private-users=no");
    cmd.arg("--keep-unit");
    cmd.arg("--settings=no");
    cmd.arg("--register=no");
    cmd.arg("--no-new-privileges=yes");
    // Set PATH inside the sandbox so the child fiach process can find git, gh, etc.
    // in /bin (populated by the Nix-built rootfs's pathsToLink = [ "/bin" ... ]).
    cmd.arg("--setenv=PATH=/bin");
    cmd.arg(format!("--setenv=HOME={}", sandbox_home));
    cmd.arg(format!(
        "--setenv=XDG_STATE_HOME={}",
        sandbox_xdg_state_home
    ));
    cmd.arg(format!(
        "--bind={}:{}",
        run_dir.display(),
        "/sandbox-output"
    ));

    if let Some(network) = &veth_network {
        cmd.arg(format!(
            "--setenv=FIACH_SANDBOX_HOST_GATEWAY={}",
            network.host_gateway()
        ));
        cmd.arg(format!(
            "--setenv=FIACH_SANDBOX_GUEST_CIDR={}",
            network.guest_cidr()
        ));
        cmd.arg(format!(
            "--setenv=FIACH_SANDBOX_DNS_PRIMARY={}",
            VETH_DNS_PRIMARY
        ));
        cmd.arg(format!(
            "--setenv=FIACH_SANDBOX_DNS_SECONDARY={}",
            VETH_DNS_SECONDARY
        ));
    }

    // Bind mount /nix/store read-only so the Nix-built rootfs symlinks resolve correctly
    let nix_store = std::path::Path::new("/nix/store");
    if nix_store.exists() {
        cmd.arg("--bind-ro=/nix/store");
    }

    // Provider credentials are required by Goose inside the review sandbox.
    if let Ok(val) = std::env::var("OPENROUTER_API_KEY") {
        cmd.arg(format!("--setenv=OPENROUTER_API_KEY={}", val));
    }
    if let Ok(val) = std::env::var("OPENAI_API_KEY") {
        cmd.arg(format!("--setenv=OPENAI_API_KEY={}", val));
    }
    if let Ok(val) = std::env::var("ANTHROPIC_API_KEY") {
        cmd.arg(format!("--setenv=ANTHROPIC_API_KEY={}", val));
    }
    if let Ok(val) = std::env::var("GOOGLE_API_KEY") {
        cmd.arg(format!("--setenv=GOOGLE_API_KEY={}", val));
    }
    // Never expose the host disclosure token to the model-controlled review
    // process. This token must be separately provisioned with read-only access
    // sufficient for cloning repositories and reading pull-request metadata.
    let review_github_token = std::env::var("FIACH_REVIEW_GITHUB_TOKEN").context(
        "Sandboxed reviews require FIACH_REVIEW_GITHUB_TOKEN; configure a read-only GitHub token distinct from GITHUB_TOKEN",
    )?;
    let host_github_token = std::env::var("GITHUB_TOKEN").ok();
    let review_github_token =
        validate_sandbox_review_token(&review_github_token, host_github_token.as_deref())?;
    cmd.arg(format!("--setenv=GITHUB_TOKEN={review_github_token}"));
    if let Ok(val) = std::env::var("RUST_LOG") {
        cmd.arg(format!("--setenv=RUST_LOG={}", val));
    }

    // Default the sandbox CA bundle path so git/gh can verify TLS even when
    // the parent service does not export Nix certificate environment vars.
    let ssl_cert_file = std::env::var("SSL_CERT_FILE")
        .unwrap_or_else(|_| "/etc/ssl/certs/ca-bundle.crt".to_string());
    let nix_ssl_cert_file =
        std::env::var("NIX_SSL_CERT_FILE").unwrap_or_else(|_| ssl_cert_file.clone());
    cmd.arg(format!("--setenv=SSL_CERT_FILE={}", ssl_cert_file));
    cmd.arg(format!("--setenv=NIX_SSL_CERT_FILE={}", nix_ssl_cert_file));

    // Network mode.  "bridge" attaches to the host's br-nspawn network.
    // "veth" requires the entrypoint script inside the
    // container to configure host0 -- which needs CAP_NET_ADMIN inside the
    // container's net namespace.  "host" shares the host network and needs
    // no extra capabilities.  "private" gives loopback only.
    match network_mode {
        "bridge" => {
            cmd.arg("--network-bridge=br-nspawn");
            cmd.arg("--capability=CAP_NET_ADMIN"); // Required for dhcpcd to set IP/routes
        }
        "veth" => {
            cmd.arg("--network-veth");
            cmd.arg("--capability=CAP_NET_ADMIN");
        }
        "private" => {
            cmd.arg("--private-network");
        }
        "host" => {}
        other => anyhow::bail!("Unsupported sandbox network mode: {other}"),
    }

    if let Some(extra_args) = &params.sandbox_extra_args {
        for arg in extra_args {
            cmd.arg(arg);
        }
    }

    // Command to run inside the sandbox.  The entrypoint script (provided by
    // the Nix module) configures the container's network interface for veth
    // mode, then execs `/bin/fiach` with the supplied arguments.
    cmd.arg("/bin/fiach-sandbox-entrypoint");
    cmd.arg("review");
    cmd.arg("--repo").arg(&review_params.repo);
    cmd.arg("--pr").arg(review_params.pr_number.to_string());
    cmd.arg("--provider").arg(&review_params.provider);
    cmd.arg("--model").arg(&review_params.model);
    if let Some(provider) = &review_params.verifier_provider {
        cmd.arg("--verifier-provider").arg(provider);
    }
    if let Some(model) = &review_params.verifier_model {
        cmd.arg("--verifier-model").arg(model);
    }
    cmd.arg("--dedupe-existing-comments")
        .arg(review_params.dedupe_existing_comments.to_string());

    let _ = &review_params.output;
    cmd.arg("--output").arg("/sandbox-output/report.md");
    if let Some(skill) = &review_params.skill {
        cmd.arg("--with-skill").arg(skill);
    }
    cmd.arg("--persona").arg(review_params.persona.to_string());
    if !review_params.review_lanes.is_empty() {
        cmd.arg("--review-lanes")
            .arg(review_params.review_lanes.join(","));
    }
    if !review_params.review_lane_prompts.is_empty() {
        cmd.arg("--review-lane-prompts-json").arg(
            serde_json::to_string(&review_params.review_lane_prompts)
                .context("Failed to serialize review lane prompts for sandbox child")?,
        );
    }
    cmd.arg("--max-review-lanes")
        .arg(review_params.max_review_lanes.to_string());
    cmd.arg("--review-kind").arg(&review_params.review_kind);
    cmd.arg("--max-turns")
        .arg(review_params.max_turns.to_string());
    cmd.arg("--timeout-mins")
        .arg(review_params.timeout_mins.to_string());
    cmd.arg("--db-path").arg(&review_params.db_path);
    cmd.arg("--sandbox-child");
    cmd.arg("--result-json").arg("/sandbox-output/result.json");

    if review_params.force {
        cmd.arg("--force");
    }

    cmd.arg("--max-retries")
        .arg(review_params.max_retries.to_string());
    cmd.arg("--retry-delay-secs")
        .arg(review_params.retry_delay_secs.to_string());
    cmd.arg("--report-mode")
        .arg(review_params.disclose_config.mode.to_string());
    cmd.arg("--verify-findings")
        .arg(review_params.verify_findings.to_string());

    if let Some(sync) = &review_params.disclose_config.sync_repo {
        cmd.arg("--sync-repo").arg(sync);
    }
    if review_params.disclose_config.notify_on_empty {
        cmd.arg("--notify-on-empty").arg("true");
    }
    if let Some(cost) = review_params.max_cost_usd {
        cmd.arg("--max-cost").arg(cost.to_string());
    }
    if let Some(p) = review_params.input_price_per_m {
        cmd.arg("--input-price").arg(p.to_string());
    }
    if let Some(p) = review_params.output_price_per_m {
        cmd.arg("--output-price").arg(p.to_string());
    }

    tracing::info!(
        repo = %review_params.repo,
        pr = %review_params.pr_number,
        rootfs = %runtime_rootfs.display(),
        machine = %machine_name,
        veth_subnet = veth_network.as_ref().map(|network| network.index),
        log = %nspawn_log.display(),
        network = network_mode,
        "Launching sandboxed review"
    );

    let log_file = std::fs::File::create(&nspawn_log)
        .with_context(|| format!("Failed to create sandbox log at {}", nspawn_log.display()))?;
    cmd.stdout(Stdio::from(
        log_file
            .try_clone()
            .context("Failed to clone sandbox log file")?,
    ));
    cmd.stderr(Stdio::from(log_file));

    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("Failed to spawn systemd-nspawn")?;

    if let Some(network) = &veth_network
        && let Err(error) = configure_sandbox_veth_host(&machine_name, network).await
    {
        let _ = child.kill().await;
        anyhow::bail!(
            "Failed to configure sandbox veth host link for machine {} using subnet 10.64.{}.0/30: {}",
            machine_name,
            network.index,
            error
        );
    }

    // The child now includes coordinator duplicate adjudication as well as finding
    // and verification. Allow each enabled phase its configured timeout plus setup.
    let phase_count = 1
        + u64::from(review_params.verify_findings)
        + u64::from(review_params.verify_findings && review_params.dedupe_existing_comments);
    let timeout_duration = std::time::Duration::from_secs(
        review_params
            .timeout_mins
            .saturating_mul(60)
            .saturating_mul(phase_count)
            .saturating_add(300),
    );

    tokio::select! {
        status_res = tokio::time::timeout(timeout_duration, child.wait()) => {
            match status_res {
                Ok(Ok(status)) => {
                    if !status.success() {
                        let log_tail = tail_file(&nspawn_log, 40)
                            .unwrap_or_else(|e| format!("failed to read sandbox log: {e}"));
                        anyhow::bail!(
                            "Sandboxed review failed with status: {}; log: {}; recent output:\n{}",
                            status,
                            nspawn_log.display(),
                            log_tail
                        );
                    }
                }
                Ok(Err(e)) => {
                    anyhow::bail!("Sandboxed review child wait error: {}", e);
                }
                Err(_) => {
                    tracing::warn!(
                        repo = %review_params.repo,
                        pr = review_params.pr_number,
                        timeout_secs = timeout_duration.as_secs(),
                        "Sandboxed review exceeded hard timeout, killing process"
                    );
                    let _ = child.kill().await;
                    anyhow::bail!("Sandboxed review timed out");
                }
            }
        }
        _ = cancel_token.cancelled() => {
            tracing::info!("Cancellation requested, killing sandbox...");
            let _ = child.kill().await;
            anyhow::bail!("Sandboxed review cancelled");
        }
    }

    let mut completed = read_completed_review(&result_json)?;
    let structured_path = crate::review::structured_artifact_path_for_report(&report_path);
    let policy_path = crate::review::disclosure_policy_path_for_report(&report_path);
    completed.report_path.clone_from(&report_path);
    completed.metadata.artifacts = crate::state::ArtifactPaths {
        markdown: Some(report_path),
        structured_json: Some(structured_path),
        policy_json: Some(policy_path),
        sandbox_log: Some(nspawn_log.clone()),
    };
    ReviewOutcome::load(
        completed,
        crate::execution::ExecutionDiagnostics {
            sandbox_log: Some(nspawn_log),
            executor: "sandbox".to_string(),
        },
    )
}

struct RuntimeRootfsGuard {
    path: PathBuf,
}

impl RuntimeRootfsGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for RuntimeRootfsGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            tracing::debug!(path = %self.path.display(), "Removing sandbox runtime rootfs");
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "Failed to remove sandbox runtime rootfs"
                );
            }
        }
    }
}

async fn prepare_runtime_rootfs(source_rootfs: &Path, run_dir: &Path) -> Result<PathBuf> {
    let runtime_rootfs = run_dir.join("rootfs");

    if runtime_rootfs.exists() {
        tokio::fs::remove_dir_all(&runtime_rootfs)
            .await
            .with_context(|| {
                format!(
                    "Failed to remove stale sandbox runtime rootfs at {}",
                    runtime_rootfs.display()
                )
            })?;
    }

    tracing::debug!(
        source = %source_rootfs.display(),
        destination = %runtime_rootfs.display(),
        "Materializing writable sandbox rootfs"
    );

    let output = Command::new("cp")
        .args(["-a"])
        .arg(source_rootfs)
        .arg(&runtime_rootfs)
        .output_with_timeout("materializing the sandbox rootfs", LONG_COMMAND_TIMEOUT)
        .await
        .context("Failed to spawn rootfs copy command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to copy sandbox rootfs: {stderr}");
    }

    for dir in ["tmp", "run", "var/tmp"] {
        let path = runtime_rootfs.join(dir);
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "Failed to create runtime rootfs directory at {}",
                path.display()
            )
        })?;
    }

    Ok(runtime_rootfs)
}

fn sandbox_run_dir(
    base_out_dir: Option<&Path>,
    repo: &str,
    pr_number: u64,
    review_kind: &str,
) -> Result<PathBuf> {
    let base_dir = match base_out_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()
            .context("Failed to get current working directory")?
            .join("reports"),
    };
    let safe_repo = repo.replace('/', "_");
    let run_name = if review_kind == crate::state::DEFAULT_REVIEW_KIND {
        format!("{}_PR{}", safe_repo, pr_number)
    } else {
        format!("{}_PR{}_{}", safe_repo, pr_number, review_kind)
    };
    Ok(base_dir.join("runs").join(run_name))
}

fn sandbox_machine_name_for_run(repo: &str, pr_number: u64, review_kind: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
        ^ u128::from(std::process::id());

    sandbox_machine_name(repo, pr_number, review_kind, nonce)
}

fn sandbox_machine_name(repo: &str, pr_number: u64, review_kind: &str, nonce: u128) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut hasher);
    pr_number.hash(&mut hasher);
    review_kind.hash(&mut hasher);
    nonce.hash(&mut hasher);

    format!("fiach-{}", base36_suffix(hasher.finish(), 6))
}

fn base36_suffix(mut value: u64, width: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut chars = vec!['0'; width];

    for index in (0..width).rev() {
        chars[index] = ALPHABET[(value % 36) as usize] as char;
        value /= 36;
    }

    chars.into_iter().collect()
}

fn read_completed_review(path: &Path) -> Result<CompletedReview> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read sandbox result JSON at {}", path.display()))?;
    serde_json::from_slice(&bytes).context("Failed to parse sandbox result JSON")
}

fn tail_file(path: &Path, max_lines: usize) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open log file at {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut lines = VecDeque::with_capacity(max_lines);

    for line in reader.lines() {
        let line =
            line.with_context(|| format!("Failed to read log file at {}", path.display()))?;
        if max_lines == 0 {
            continue;
        }
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    if lines.is_empty() {
        Ok("<sandbox log was empty>".to_string())
    } else {
        Ok(lines.into_iter().collect::<Vec<_>>().join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_request(pr: u64) -> ReviewRequest<DaemonWork> {
        let repo = if pr <= 11 {
            "owner/first"
        } else {
            "owner/second"
        }
        .to_string();
        let job = ReviewJob {
            pr: PullRequest {
                number: pr,
                head_ref_oid: format!("head-{pr}"),
                head_ref_name: "feature".to_string(),
                author_association: "MEMBER".to_string(),
                title: format!("PR {pr}"),
            },
            persona: crate::persona::PersonaSource::BuiltinSecurity,
            review_kind: "security".to_string(),
        };
        ReviewRequest {
            target: ReviewTarget {
                repository: repo.clone(),
                pr_number: pr,
                commit_hash: job.pr.head_ref_oid.clone(),
                review_kind: job.review_kind.clone(),
            },
            payload: DaemonWork::Poll {
                repo,
                job,
                honor_mention_trigger: true,
            },
        }
    }

    #[test]
    fn pending_polls_refresh_payloads_and_remove_stale_targets() {
        let mut pending = PendingPollReviews::default();
        pending.refresh(vec![poll_request(2), poll_request(3), poll_request(4)]);
        let mut updated = poll_request(2);
        if let DaemonWork::Poll { job, .. } = &mut updated.payload {
            job.pr.title = "Updated title".to_string();
        }
        let mut new_head = poll_request(4);
        new_head.target.commit_hash = "new-head".to_string();
        if let DaemonWork::Poll { job, .. } = &mut new_head.payload {
            job.pr.head_ref_oid = "new-head".to_string();
        }
        let mut other_persona = poll_request(2);
        other_persona.target.review_kind = "other-persona".to_string();
        if let DaemonWork::Poll { job, .. } = &mut other_persona.payload {
            job.review_kind = "other-persona".to_string();
        }
        pending.refresh(vec![
            poll_request(1),
            updated.clone(),
            new_head.clone(),
            updated,
            other_persona.clone(),
        ]);

        let targets: Vec<_> = pending.requests.iter().map(|r| r.target.clone()).collect();
        assert_eq!(
            targets,
            vec![
                poll_request(2).target,
                poll_request(1).target,
                new_head.target,
                other_persona.target,
            ]
        );
        let DaemonWork::Poll { job, .. } = &pending.requests[0].payload else {
            panic!("expected poll work");
        };
        assert_eq!(job.pr.title, "Updated title");
    }

    #[tokio::test]
    async fn full_queue_resumes_deferred_polls_before_repeating_skipped_prs() {
        let cancel = CancellationToken::new();
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let (processed, mut received) = tokio::sync::mpsc::unbounded_channel();
        let handler = {
            let gate = gate.clone();
            std::sync::Arc::new(move |work: DaemonWork, _: CancellationToken| {
                let gate = gate.clone();
                let processed = processed.clone();
                Box::pin(async move {
                    gate.acquire().await.unwrap().forget();
                    let DaemonWork::Poll { job, .. } = work else {
                        panic!("expected poll work");
                    };
                    processed.send(job.pr.number).unwrap();
                    Ok(ExecutionStatus::Skipped)
                }) as BoxFuture<'static, Result<ExecutionStatus>>
            })
        };
        let scheduler = crate::scheduler::start(1, cancel.clone(), handler);
        let mut pending = PendingPollReviews::default();
        pending.refresh((1..=22).map(poll_request).collect());
        assert_eq!(
            pending.submit(&scheduler, &cancel).await,
            Err(SubmitError::Full)
        );
        // One running job plus the scheduler's sixteen queue slots.
        assert_eq!(pending.requests.front().unwrap().target.pr_number, 18);
        gate.add_permits(17);
        tokio::time::timeout(Duration::from_secs(5), async {
            for expected in 1..=17 {
                assert_eq!(received.recv().await.unwrap(), expected);
            }
            while scheduler.stats().await.skipped != 17 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        pending.refresh((1..=22).map(poll_request).collect());
        assert_eq!(
            pending.submit(&scheduler, &cancel).await,
            Err(SubmitError::Full)
        );
        gate.add_permits(17);
        tokio::time::timeout(Duration::from_secs(5), async {
            for expected in (18..=22).chain(1..=12) {
                assert_eq!(received.recv().await.unwrap(), expected);
            }
            while scheduler.stats().await.skipped != 34 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pending.submit(&scheduler, &cancel).await, Ok(()));
        assert!(pending.requests.is_empty());
        gate.add_permits(5);
        tokio::time::timeout(Duration::from_secs(5), async {
            for expected in 13..=17 {
                assert_eq!(received.recv().await.unwrap(), expected);
            }
        })
        .await
        .unwrap();
        cancel.cancel();
    }

    #[tokio::test]
    async fn poll_submitter_refills_without_another_discovery() {
        let cancel = CancellationToken::new();
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let (processed, mut received) = tokio::sync::mpsc::unbounded_channel();
        let handler = {
            let gate = gate.clone();
            std::sync::Arc::new(move |work: DaemonWork, _: CancellationToken| {
                let gate = gate.clone();
                let processed = processed.clone();
                Box::pin(async move {
                    gate.acquire().await.unwrap().forget();
                    let DaemonWork::Poll { job, .. } = work else {
                        panic!("expected poll work");
                    };
                    processed.send(job.pr.number).unwrap();
                    Ok(ExecutionStatus::Skipped)
                }) as BoxFuture<'static, Result<ExecutionStatus>>
            })
        };
        let scheduler = crate::scheduler::start(1, cancel.clone(), handler);
        let (discovered, discoveries) = tokio::sync::watch::channel(Vec::new());
        let submitter = {
            let scheduler = scheduler.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                PendingPollReviews::default()
                    .run(&scheduler, &cancel, discoveries)
                    .await
            })
        };

        discovered
            .send((1..=22).map(poll_request).collect())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while scheduler.stats().await.queued != 16 {
                tokio::task::yield_now().await;
            }
            // Free one worker at a time. All 22 must run from this one discovery,
            // even though the scheduler can initially accept only 17.
            for expected in 1..=22 {
                gate.add_permits(1);
                assert_eq!(received.recv().await.unwrap(), expected);
            }
            while scheduler.stats().await.skipped != 22 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(scheduler.stats().await.total, 22);

        // New discoveries still wake an idle submitter, and cancellation stops
        // it promptly even when the scheduler is full again.
        discovered
            .send((23..=44).map(poll_request).collect())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while scheduler.stats().await.queued != 16 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), submitter)
                .await
                .unwrap()
                .unwrap(),
            Err(SubmitError::Closed)
        );
    }

    #[tokio::test]
    async fn pending_polls_stop_on_cancellation_or_closed_scheduler() {
        let cancel = CancellationToken::new();
        let handler = std::sync::Arc::new(|_: DaemonWork, _: CancellationToken| {
            Box::pin(async { Ok(ExecutionStatus::Skipped) })
                as BoxFuture<'static, Result<ExecutionStatus>>
        });
        let scheduler = crate::scheduler::start(1, cancel.clone(), handler);
        let mut pending = PendingPollReviews::default();
        pending.refresh(vec![poll_request(1)]);
        cancel.cancel();
        assert_eq!(
            pending.submit(&scheduler, &cancel).await,
            Err(SubmitError::Closed)
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while scheduler.stats().await.accepting {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            pending.submit(&scheduler, &CancellationToken::new()).await,
            Err(SubmitError::Closed)
        );
        assert_eq!(pending.requests.len(), 1);
        assert_eq!(scheduler.stats().await.total, 0);
    }

    #[test]
    fn runtime_rootfs_guard_removes_materialized_root() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_rootfs = temp.path().join("rootfs");
        std::fs::create_dir(&runtime_rootfs).unwrap();

        {
            let _guard = RuntimeRootfsGuard::new(runtime_rootfs.clone());
            assert!(runtime_rootfs.exists());
        }

        assert!(!runtime_rootfs.exists());
    }

    #[test]
    fn author_association_filter_is_case_insensitive() {
        let allowed = vec!["COLLABORATOR".to_string(), "MEMBER".to_string()];

        assert!(is_allowed_author_association("collaborator", &allowed));
        assert!(is_allowed_author_association("MEMBER", &allowed));
        assert!(!is_allowed_author_association("FIRST_TIMER", &allowed));
    }

    #[test]
    fn review_jobs_can_preserve_a_lone_security_persona_kind() {
        let prs = vec![PullRequest {
            number: 7,
            head_ref_oid: "head".to_string(),
            head_ref_name: "security-fix".to_string(),
            author_association: "MEMBER".to_string(),
            title: "Harden token validation".to_string(),
        }];

        let jobs = review_jobs(
            &[crate::persona::PersonaSource::BuiltinSecurity],
            &prs,
            true,
        );

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].review_kind, "security");
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
    fn sandbox_machine_name_is_short_and_veth_safe() {
        let name = sandbox_machine_name("owner/repo", 42, "security", 123);
        let host_interface_name = format!("ve-{name}");

        assert!(name.starts_with("fiach-"));
        assert_eq!(name.len(), 12);
        assert_eq!(host_interface_name.len(), 15);
        assert!(
            name.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        );
    }

    #[test]
    fn sandbox_machine_name_varies_by_nonce() {
        assert_ne!(
            sandbox_machine_name("owner/repo", 42, "security", 123),
            sandbox_machine_name("owner/repo", 42, "security", 124)
        );
    }

    #[test]
    fn sandbox_veth_start_index_is_stable_and_in_pool() {
        let first = sandbox_veth_start_index("fiach-test01");
        let second = sandbox_veth_start_index("fiach-test01");

        assert_eq!(first, second);
        assert!((VETH_SUBNET_MIN_INDEX..=VETH_SUBNET_MAX_INDEX).contains(&first));
    }

    #[test]
    fn sandbox_veth_index_probes_to_avoid_active_collision() {
        let machine_name = "fiach-collide";
        let first = sandbox_veth_start_index(machine_name);
        let mut active = HashSet::from([first]);

        let second = reserve_sandbox_veth_index(machine_name, &mut active).unwrap();

        assert_ne!(first, second);
        assert!(active.contains(&first));
        assert!(active.contains(&second));
        assert!((VETH_SUBNET_MIN_INDEX..=VETH_SUBNET_MAX_INDEX).contains(&second));
    }

    #[test]
    fn sandbox_veth_reservation_formats_host_and_guest_addresses() {
        let reservation = SandboxVethReservation { index: 42 };

        assert_eq!(reservation.host_gateway(), "10.64.42.1");
        assert_eq!(reservation.host_cidr(), "10.64.42.1/30");
        assert_eq!(reservation.guest_cidr(), "10.64.42.2/30");
    }

    #[test]
    fn sandbox_veth_index_reports_pool_exhaustion() {
        let mut active: HashSet<u8> = (VETH_SUBNET_MIN_INDEX..=VETH_SUBNET_MAX_INDEX).collect();

        let error = reserve_sandbox_veth_index("fiach-full", &mut active).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("No sandbox veth subnets are available")
        );
    }

    #[test]
    fn sandbox_veth_worker_capacity_rejects_unbounded_or_too_large() {
        assert!(validate_veth_worker_capacity(1).is_ok());
        assert!(validate_veth_worker_capacity(254).is_ok());
        assert!(validate_veth_worker_capacity(0).is_err());
        assert!(validate_veth_worker_capacity(255).is_err());
    }

    #[test]
    fn tail_file_returns_recent_lines() {
        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("nspawn.log");
        std::fs::write(&log_path, "one\ntwo\nthree\nfour\n").unwrap();

        assert_eq!(tail_file(&log_path, 2).unwrap(), "three\nfour");
    }

    #[test]
    fn pr_search_query_can_omit_updated_filter() {
        assert_eq!(
            pr_search_query("open", false, 120, None, None),
            "state:open"
        );
        assert_eq!(
            pr_search_query("open", false, 120, Some(true), None),
            "state:open"
        );
        assert_eq!(
            pr_search_query("open", false, 120, Some(false), None),
            "state:open draft:false"
        );
    }

    #[test]
    fn pr_search_query_includes_updated_filter_when_configured() {
        let query = pr_search_query("open", true, 120, Some(false), None);

        assert!(query.starts_with("state:open updated:>="));
        assert!(query.ends_with(" draft:false"));
    }

    #[test]
    fn pr_search_query_omits_state_filter_for_all() {
        assert_eq!(pr_search_query("all", false, 120, None, None), "");
        assert_eq!(pr_search_query("all", false, 120, Some(true), None), "");
        assert_eq!(
            pr_search_query("all", false, 120, Some(false), None),
            "draft:false"
        );
    }

    fn mention_source(
        login: &str,
        association: &str,
        body: &str,
        created_at: &str,
    ) -> MentionSource {
        MentionSource {
            id: Some(format!("IC_{login}")),
            author: Some(MentionAuthor {
                login: login.to_string(),
            }),
            author_association: association.to_string(),
            body: body.to_string(),
            created_at: Some(created_at.to_string()),
            submitted_at: None,
        }
    }

    #[test]
    fn mention_matches_respects_word_boundaries() {
        assert!(mention_matches("please review @fiach-bot", "fiach-bot"));
        assert!(mention_matches("@FIACH-BOT take a look", "fiach-bot"));
        assert!(mention_matches("@fiach-bot, thanks", "@fiach-bot"));
        assert!(!mention_matches("mail me at a@fiach-bot", "fiach-bot"));
        assert!(!mention_matches("@fiach-bot2 is someone else", "fiach-bot"));
        assert!(!mention_matches("@fiach-botanic", "fiach-bot"));
        assert!(!mention_matches("no mention here", "fiach-bot"));
    }

    #[test]
    fn mentioner_allowlist_prefers_explicit_users_over_associations() {
        let users = vec!["Lead-Maintainer".to_string()];
        let associations = vec!["MEMBER".to_string()];

        assert!(is_allowed_mentioner(
            "lead-maintainer",
            "NONE",
            &users,
            &associations
        ));
        assert!(!is_allowed_mentioner(
            "drive-by",
            "MEMBER",
            &users,
            &associations
        ));
        assert!(is_allowed_mentioner("anyone", "member", &[], &associations));
        assert!(!is_allowed_mentioner("anyone", "NONE", &[], &associations));
    }

    #[test]
    fn latest_valid_mention_uses_newest_allowlisted_mention() {
        let details = PrMentionDetails {
            author: Some(MentionAuthor {
                login: "new-contributor".to_string(),
            }),
            body: "cc @fiach-bot".to_string(),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            comments: vec![
                mention_source(
                    "drive-by",
                    "NONE",
                    "@fiach-bot review",
                    "2026-01-03T00:00:00Z",
                ),
                mention_source(
                    "maintainer",
                    "MEMBER",
                    "@fiach-bot review",
                    "2026-01-02T00:00:00Z",
                ),
            ],
            reviews: vec![],
        };
        let associations = vec!["MEMBER".to_string()];

        // PR author (NONE) and drive-by commenter are not allowlisted; only the
        // maintainer's mention counts, and it carries the comment node id.
        let mention =
            latest_valid_mention(&details, "NONE", "fiach-bot", &[], &associations).unwrap();
        assert_eq!(
            Some(mention.timestamp),
            parse_rfc3339_unix("2026-01-02T00:00:00Z")
        );
        assert_eq!(mention.subject_node_id.as_deref(), Some("IC_maintainer"));

        let none = latest_valid_mention(&details, "NONE", "other-bot", &[], &associations);
        assert_eq!(none, None);
    }

    #[test]
    fn latest_valid_mention_in_pr_body_has_no_node_id() {
        let details = PrMentionDetails {
            author: Some(MentionAuthor {
                login: "maintainer".to_string(),
            }),
            body: "cc @fiach-bot".to_string(),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            comments: vec![],
            reviews: vec![],
        };
        let associations = vec!["MEMBER".to_string()];

        let mention =
            latest_valid_mention(&details, "MEMBER", "fiach-bot", &[], &associations).unwrap();
        assert_eq!(mention.subject_node_id, None);
    }

    #[test]
    fn mention_trigger_is_one_shot() {
        use crate::state::ReviewDecision::*;

        // Never reviewed: any valid mention triggers.
        assert_eq!(apply_mention_trigger(FirstReview, 100, None), FirstReview);
        // Re-mentioned after a completed same-commit review: run again.
        assert_eq!(apply_mention_trigger(Skip, 200, Some(100)), ReReview);
        // Stale mention on an already-reviewed commit: stay skipped.
        assert_eq!(apply_mention_trigger(Skip, 100, Some(200)), Skip);
        // New commit but no fresh mention: do not auto re-review.
        assert_eq!(apply_mention_trigger(ReReview, 100, Some(200)), Skip);
        // New commit and fresh mention: review.
        assert_eq!(apply_mention_trigger(ReReview, 300, Some(200)), ReReview);
        // Failed attempts keep retrying without a new mention.
        assert_eq!(
            apply_mention_trigger(RetryFailed, 100, Some(200)),
            RetryFailed
        );
    }

    #[test]
    fn pr_search_query_includes_trigger_mention() {
        assert_eq!(
            pr_search_query("open", false, 120, None, Some("fiach-bot")),
            "state:open mentions:fiach-bot"
        );
    }

    #[test]
    fn pr_search_query_strips_leading_at_from_trigger_mention() {
        assert_eq!(
            pr_search_query("all", false, 120, None, Some("@fiach-bot")),
            "mentions:fiach-bot"
        );
    }

    #[test]
    fn pr_list_state_arg_supports_documented_states() {
        assert_eq!(pr_list_state_arg("open"), "open");
        assert_eq!(pr_list_state_arg("closed"), "closed");
        assert_eq!(pr_list_state_arg("merged"), "merged");
        assert_eq!(pr_list_state_arg("all"), "all");
        assert_eq!(pr_list_state_arg("unexpected"), "all");
    }

    #[test]
    fn sandbox_review_token_must_be_nonempty_and_distinct() {
        assert_eq!(
            validate_sandbox_review_token("review-read-only", Some("host-write")).unwrap(),
            "review-read-only"
        );
        assert!(validate_sandbox_review_token("", Some("host-write")).is_err());
        assert!(validate_sandbox_review_token("shared-token", Some("shared-token")).is_err());
    }
}
