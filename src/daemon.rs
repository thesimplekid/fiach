use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{StreamExt, stream};
use serde::Deserialize;
use time::{OffsetDateTime, format_description};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::disclose::DiscloseConfig;
use crate::review::{CompletedReview, ReviewExecution, ReviewParams, run_review};

pub enum DaemonMessage {
    TriggerReview {
        repo: String,
        pr_number: u64,
        persona: Option<String>,
    },
}

pub struct DaemonParams {
    pub repos: String,
    pub interval: u64,
    pub provider: String,
    pub model: String,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub skill: Option<String>,
    pub personas: Vec<crate::persona::PersonaSource>,
    pub max_turns: u32,
    pub timeout_mins: u64,
    pub db_path: PathBuf,
    pub max_retries: u32,
    pub retry_delay_secs: u64,
    pub out_dir: Option<PathBuf>,
    pub disclose_config: DiscloseConfig,
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
    pub pr_limit: u32,
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
struct PullRequest {
    number: u64,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "authorAssociation")]
    author_association: String,
    title: String,
}

#[derive(Clone)]
struct ReviewJob {
    pr: PullRequest,
    persona: crate::persona::PersonaSource,
    review_kind: String,
}

fn is_allowed_author_association(association: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(association))
}

fn worker_concurrency(max_workers: usize, job_count: usize) -> usize {
    if job_count == 0 {
        1
    } else if max_workers == 0 {
        job_count
    } else {
        max_workers.min(job_count)
    }
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
) -> String {
    let mut parts = Vec::new();

    if !state.eq_ignore_ascii_case("all") {
        parts.push(format!("state:{state}"));
    }

    if filter_by_updated {
        let time_ago = OffsetDateTime::now_utc() - time::Duration::days(updated_within_days.into());
        let format = format_description::parse("[year]-[month]-[day]").unwrap();
        let search_date = time_ago.format(&format).unwrap();
        parts.push(format!("updated:>={search_date}"));
    }

    if let Some(drafts) = drafts {
        parts.push(format!("draft:{drafts}"));
    }

    parts.join(" ")
}

fn pr_list_state_arg(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "open" | "closed" | "merged" | "all" => state.to_ascii_lowercase(),
        _ => "all".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrProcessStatus {
    Reviewed,
    Skipped,
    Failed,
}

pub async fn run_daemon(
    params: DaemonParams,
    mut rx: mpsc::Receiver<DaemonMessage>,
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

    // Ensure gh is authenticated
    let gh_auth = Command::new("gh")
        .arg("auth")
        .arg("setup-git")
        .output()
        .await;
    if let Err(e) = gh_auth {
        tracing::warn!("Failed to run gh auth setup-git: {}", e);
    }

    let sleep_duration = Duration::from_secs(params.interval);

    loop {
        if cancel_token.is_cancelled() {
            tracing::info!("Daemon shutting down");
            break;
        }

        tracing::debug!("Starting polling cycle");

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
                );

                let list_state = pr_list_state_arg(state);
                let mut command = Command::new("gh");
                command.args(["pr", "list", "--repo", repo, "--state", &list_state]);
                if !search_query.is_empty() {
                    command.args(["--search", &search_query]);
                }
                command.args([
                    "--limit",
                    &params.pr_limit.to_string(),
                    "--json",
                    "number,headRefOid,headRefName,authorAssociation,title",
                ]);

                let output = command.output().await;

                match output {
                    Ok(out) if out.status.success() => {
                        let prs: Vec<PullRequest> = match serde_json::from_slice(&out.stdout) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!(
                                    "Failed to parse gh output for {} (state {}): {}",
                                    repo,
                                    state,
                                    e
                                );
                                continue;
                            }
                        };

                        tracing::info!("Found {} recent {} PRs for {}", prs.len(), state, repo);

                        let mut reviewed = 0;
                        let mut skipped = 0;
                        let mut failed = 0;

                        let jobs = review_jobs(&params.personas, &prs, params.personas.len() > 1);
                        let concurrency = worker_concurrency(params.max_workers, jobs.len());
                        tracing::info!(
                            repo = %repo,
                            state = %state,
                            max_workers = params.max_workers,
                            concurrency = concurrency,
                            personas = params.personas.len(),
                            "Processing review jobs with worker limit"
                        );

                        let mut outcomes = stream::iter(jobs.iter())
                            .map(|job| process_daemon_job(&params, repo, job, cancel_token.clone()))
                            .buffer_unordered(concurrency);

                        while let Some(outcome) = outcomes.next().await {
                            match outcome? {
                                PrProcessStatus::Reviewed => reviewed += 1,
                                PrProcessStatus::Skipped => skipped += 1,
                                PrProcessStatus::Failed => failed += 1,
                            }

                            if cancel_token.is_cancelled() {
                                break;
                            }
                        }

                        tracing::info!(
                            repo = %repo,
                            state = %state,
                            total = jobs.len(),
                            reviewed = reviewed,
                            skipped = skipped,
                            failed = failed,
                            "Finished {} review job processing for {}",
                            state,
                            repo
                        );
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        tracing::error!(
                            "gh cli failed for repo {} (state {}): {}",
                            repo,
                            state,
                            stderr
                        );
                    }
                    Err(e) => {
                        tracing::error!("Failed to execute gh cli: {}", e);
                    }
                }
            }
        }

        if cancel_token.is_cancelled() {
            break;
        }

        tracing::debug!(
            "Polling cycle complete, sleeping for {} seconds",
            params.interval
        );
        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            Some(msg) = rx.recv() => {
                match msg {
                    DaemonMessage::TriggerReview {
                        repo,
                        pr_number,
                        persona,
                    } => {
                        tracing::info!("Received trigger to review {}/{}", repo, pr_number);
                        if let Err(e) =
                            trigger_manual_review(&params, repo, pr_number, persona, cancel_token.clone()).await
                        {
                            tracing::error!("Failed to manually trigger review: {}", e);
                        }
                    }
                }
            }
            _ = cancel_token.cancelled() => {
                tracing::info!("Sleep interrupted, shutting down");
            }
        }
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
                review_kind: job.review_kind.clone(),
                commit_hash: job.pr.head_ref_oid.clone(),
                model: "daemon".to_string(),
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                findings_count: 0,
                status: "skipped".to_string(),
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

    if !is_allowed_author_association(&pr.author_association, &params.allowed_author_associations) {
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

    match crate::state::should_review(
        &params.db_path,
        repo,
        pr.number,
        &pr.head_ref_oid,
        &job.review_kind,
        false,
        params.timeout_mins,
    ) {
        Ok(crate::state::ReviewDecision::FirstReview)
        | Ok(crate::state::ReviewDecision::ReReview)
        | Ok(crate::state::ReviewDecision::RetryFailed) => {
            let decision = crate::state::should_review(
                &params.db_path,
                repo,
                pr.number,
                &pr.head_ref_oid,
                &job.review_kind,
                false,
                params.timeout_mins,
            )?;
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
                output: output_path,
                skill: params.skill.clone(),
                persona: job.persona.clone(),
                max_turns: params.max_turns,
                timeout_mins: params.timeout_mins,
                db_path: params.db_path.clone(),
                review_kind: job.review_kind.clone(),
                force: false,
                max_retries: params.max_retries,
                retry_delay_secs: params.retry_delay_secs,
                disclose_config: params.disclose_config.clone(),
                verify_findings: params.verify_findings,
                context_groups: params.context_groups.clone(),
                max_cost_usd: params.max_cost_usd,
                input_price_per_m: params.input_price_per_m,
                output_price_per_m: params.output_price_per_m,
                is_rereview,
                execution: ReviewExecution {
                    skip_state_check: true,
                    persist_side_effects: true,
                    result_json: None,
                },
            };

            let review_result = if params.sandbox_rootfs.is_some() {
                run_sandboxed_review(params, &review_params, cancel_token.clone()).await
            } else {
                run_review(review_params, cancel_token.clone())
                    .await
                    .map(|_| ())
            };

            if let Err(e) = review_result {
                let retry_count =
                    crate::state::get_pr_review(&params.db_path, repo, pr.number, &job.review_kind)
                        .ok()
                        .flatten()
                        .map(|m| m.retry_count)
                        .unwrap_or(0);

                let meta = crate::state::ReviewMetadata {
                    review_kind: job.review_kind.clone(),
                    commit_hash: pr.head_ref_oid.clone(),
                    model: "daemon".to_string(),
                    timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                    findings_count: 0,
                    status: "failed".to_string(),
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
                    retry_count,
                };
                let _ = crate::state::mark_reviewed(&params.db_path, repo, pr.number, &meta);

                if cancel_token.is_cancelled() {
                    return Err(e);
                }
                tracing::error!("Failed to review PR {} in {}: {}", pr.number, repo, e);
                if crate::review::is_fatal_error(&e) {
                    tracing::error!("Fatal error encountered, stopping daemon");
                    return Err(e);
                }
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
            "headRefOid,headRefName,authorAssociation,title",
        ])
        .output()
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
        #[serde(rename = "authorAssociation")]
        author_association: String,
        title: String,
    }

    let pr_details: PrDetails =
        serde_json::from_slice(&output.stdout).context("Failed to parse PR details")?;

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
            author_association: pr_details.author_association,
            title: pr_details.title,
        }],
        params.personas.len() > 1,
    );

    let concurrency = worker_concurrency(params.max_workers, jobs.len());
    let mut outcomes = stream::iter(jobs.iter())
        .map(|job| process_daemon_job(params, &repo, job, cancel_token.clone()))
        .buffer_unordered(concurrency);

    while let Some(outcome) = outcomes.next().await {
        outcome?;
    }

    Ok(())
}

async fn run_sandboxed_review(
    params: &DaemonParams,
    review_params: &ReviewParams,
    cancel_token: CancellationToken,
) -> Result<()> {
    let rootfs = params.sandbox_rootfs.as_ref().unwrap();

    if !rootfs.exists() || !rootfs.is_dir() {
        anyhow::bail!(
            "Sandbox rootfs does not exist or is not a directory: {}",
            rootfs.display()
        );
    }

    #[allow(clippy::collapsible_if)]
    if let Some(net) = &params.sandbox_network {
        if net != "bridge" && net != "veth" && net != "private" && net != "host" {
            anyhow::bail!(
                "Invalid sandbox network mode: {}. Must be host, bridge, private, or veth.",
                net
            );
        }
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
    cmd.arg(format!("--directory={}", runtime_rootfs.display()));
    // --private-users=no: DynamicUser provides a transient UID without subuid/subgid
    // mappings, so nspawn's default --private-users=pick fails.
    // --keep-unit: prevent nspawn from registering a new transient scope with systemd,
    // which requires privileges a service unit doesn't have.
    cmd.arg("--private-users=no");
    cmd.arg("--keep-unit");
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

    // Bind mount /nix/store read-only so the Nix-built rootfs symlinks resolve correctly
    let nix_store = std::path::Path::new("/nix/store");
    if nix_store.exists() {
        cmd.arg("--bind-ro=/nix/store");
    }

    // Ensure API keys are forwarded securely
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
    if let Ok(val) = std::env::var("GITHUB_TOKEN") {
        cmd.arg(format!("--setenv=GITHUB_TOKEN={}", val));
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
    #[allow(clippy::collapsible_if)]
    if let Some(net) = &params.sandbox_network {
        if net == "bridge" {
            cmd.arg("--network-bridge=br-nspawn");
            cmd.arg("--capability=CAP_NET_ADMIN"); // Required for dhcpcd to set IP/routes
        } else if net == "veth" {
            cmd.arg("--network-veth");
            cmd.arg("--capability=CAP_NET_ADMIN");
        } else if net == "private" {
            cmd.arg("--private-network");
        } else if net != "host" {
            tracing::warn!("Unknown network mode {}, defaulting to host", net);
        }
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

    let _ = &review_params.output;
    cmd.arg("--output").arg("/sandbox-output/report.md");
    if let Some(skill) = &review_params.skill {
        cmd.arg("--with-skill").arg(skill);
    }
    cmd.arg("--persona").arg(review_params.persona.to_string());
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
        log = %nspawn_log.display(),
        network = ?params.sandbox_network,
        "Launching sandboxed review"
    );

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
        tracing::warn!(
            repo = %review_params.repo,
            pr = review_params.pr_number,
            error = %error,
            "Failed to post review start reaction"
        );
    }

    let log_file = std::fs::File::create(&nspawn_log)
        .with_context(|| format!("Failed to create sandbox log at {}", nspawn_log.display()))?;
    cmd.stdout(Stdio::from(
        log_file
            .try_clone()
            .context("Failed to clone sandbox log file")?,
    ));
    cmd.stderr(Stdio::from(log_file));

    let mut child = cmd.spawn().context("Failed to spawn systemd-nspawn")?;

    let timeout_duration = std::time::Duration::from_secs(review_params.timeout_mins * 60 + 300);

    tokio::select! {
        status_res = tokio::time::timeout(timeout_duration, child.wait()) => {
            match status_res {
                Ok(Ok(status)) => {
                    if !status.success() {
                        anyhow::bail!("Sandboxed review failed with status: {}", status);
                    }
                }
                Ok(Err(e)) => {
                    anyhow::bail!("Sandboxed review child wait error: {}", e);
                }
                Err(_) => {
                    tracing::warn!(
                        repo = %review_params.repo,
                        pr = review_params.pr_number,
                        "Sandboxed review exceeded hard timeout of {} minutes, killing process",
                        review_params.timeout_mins + 5
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

    let completed = read_completed_review(&result_json)?;
    let structured_path = crate::review::structured_artifact_path_for_report(&report_path);
    let policy_path = crate::review::disclosure_policy_path_for_report(&report_path);
    let report_url = match (
        read_json_file::<crate::reporting::ReportingArtifact>(&structured_path),
        read_json_file::<crate::reporting::DisclosurePolicy>(&policy_path),
    ) {
        (Ok(artifact), Ok(policy)) => {
            crate::disclose::handle_structured_disclosure(
                &report_path,
                crate::disclose::DisclosureTarget {
                    repo: &review_params.repo,
                    pr_number: review_params.pr_number,
                    commit_hash: completed.metadata.commit_hash.as_str(),
                    review_kind: &review_params.review_kind,
                },
                &artifact,
                &policy,
                &review_params.disclose_config,
            )
            .await?
        }
        _ => {
            tracing::warn!(
                report = %report_path.display(),
                "Sandbox child did not emit structured disclosure artifacts; suppressing PR comments"
            );
            crate::disclose::handle_disclosure(
                &report_path,
                crate::disclose::DisclosureTarget {
                    repo: &review_params.repo,
                    pr_number: review_params.pr_number,
                    commit_hash: completed.metadata.commit_hash.as_str(),
                    review_kind: &review_params.review_kind,
                },
                false,
                &review_params.disclose_config,
            )
            .await?
        }
    };

    let mut metadata = completed.metadata;
    metadata.report_url = report_url;
    if metadata.status == "none"
        && let Some(reaction) = review_params
            .disclose_config
            .reactions
            .no_findings
            .as_deref()
        && let Err(error) = crate::disclose::post_pr_reaction(
            &review_params.repo,
            review_params.pr_number,
            reaction,
        )
        .await
    {
        tracing::warn!(
            repo = %review_params.repo,
            pr = review_params.pr_number,
            error = %error,
            "Failed to post no-findings reaction"
        );
    }
    crate::state::mark_reviewed(
        &params.db_path,
        &review_params.repo,
        review_params.pr_number,
        &metadata,
    )?;

    Ok(())
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
        .output()
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

fn read_completed_review(path: &Path) -> Result<CompletedReview> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read sandbox result JSON at {}", path.display()))?;
    serde_json::from_slice(&bytes).context("Failed to parse sandbox result JSON")
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read JSON file at {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("Failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_association_filter_is_case_insensitive() {
        let allowed = vec!["COLLABORATOR".to_string(), "MEMBER".to_string()];

        assert!(is_allowed_author_association("collaborator", &allowed));
        assert!(is_allowed_author_association("MEMBER", &allowed));
        assert!(!is_allowed_author_association("FIRST_TIMER", &allowed));
    }

    #[test]
    fn worker_concurrency_caps_jobs_and_allows_unlimited() {
        assert_eq!(worker_concurrency(1, 10), 1);
        assert_eq!(worker_concurrency(3, 10), 3);
        assert_eq!(worker_concurrency(20, 10), 10);
        assert_eq!(worker_concurrency(0, 10), 10);
        assert_eq!(worker_concurrency(0, 0), 1);
    }

    #[test]
    fn pr_search_query_can_omit_updated_filter() {
        assert_eq!(pr_search_query("open", false, 120, None), "state:open");
        assert_eq!(
            pr_search_query("open", false, 120, Some(false)),
            "state:open draft:false"
        );
    }

    #[test]
    fn pr_search_query_includes_updated_filter_when_configured() {
        let query = pr_search_query("open", true, 120, Some(false));

        assert!(query.starts_with("state:open updated:>="));
        assert!(query.ends_with(" draft:false"));
    }

    #[test]
    fn pr_search_query_omits_state_filter_for_all() {
        assert_eq!(pr_search_query("all", false, 120, None), "");
        assert_eq!(
            pr_search_query("all", false, 120, Some(false)),
            "draft:false"
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
}
