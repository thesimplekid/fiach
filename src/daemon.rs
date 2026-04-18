use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use time::{OffsetDateTime, format_description};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::disclose::DiscloseConfig;
use crate::review::{CompletedReview, ReviewExecution, ReviewParams, run_review};

pub struct DaemonParams {
    pub repos: String,
    pub interval: u64,
    pub model: String,
    pub skill: Option<String>,
    pub persona: crate::persona::PersonaSource,
    pub max_turns: u32,
    pub timeout_mins: u64,
    pub db_path: PathBuf,
    pub max_retries: u32,
    pub retry_delay_secs: u64,
    pub out_dir: Option<PathBuf>,
    pub disclose_config: DiscloseConfig,
    pub context_groups: std::collections::HashMap<String, crate::config::ContextGroup>,
    pub pr_states: Vec<String>,
    pub skip_prs: Vec<String>,
    pub drafts: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    title: String,
}

pub async fn run_daemon(params: DaemonParams, cancel_token: CancellationToken) -> Result<()> {
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

            // Look for PRs updated in the last 4 months (120 days)
            let four_months_ago = OffsetDateTime::now_utc() - time::Duration::days(120);
            let format = format_description::parse("[year]-[month]-[day]").unwrap();
            let search_date = four_months_ago.format(&format).unwrap();

            for state in &params.pr_states {
                if cancel_token.is_cancelled() {
                    break;
                }

                let mut search_query = format!("state:{} updated:>={}", state, search_date);
                if let Some(drafts) = params.drafts {
                    search_query.push_str(&format!(" draft:{}", drafts));
                }

                let output = Command::new("gh")
                    .args([
                        "pr",
                        "list",
                        "--repo",
                        repo,
                        "--search",
                        &search_query,
                        "--limit",
                        "1000",
                        "--json",
                        "number,headRefOid,headRefName,title",
                    ])
                    .output()
                    .await;

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

                        for pr in &prs {
                            if cancel_token.is_cancelled() {
                                break;
                            }

                            // Check if PR should be skipped
                            let skip = params.skip_prs.iter().any(|s| {
                                s == &pr.number.to_string()
                                    || s == &format!("{}#{}", repo, pr.number)
                            });

                            if skip {
                                tracing::info!(repo = %repo, pr = pr.number, "Skipping PR as requested");
                                skipped += 1;
                                #[allow(clippy::collapsible_if)]
                                if let Ok(decision) = crate::state::should_review(
                                    &params.db_path,
                                    repo,
                                    pr.number,
                                    &pr.head_ref_oid,
                                    false,
                                ) {
                                    if decision != crate::state::ReviewDecision::Skip {
                                        let meta = crate::state::ReviewMetadata {
                                            commit_hash: pr.head_ref_oid.clone(),
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
                                            time_reviewed: Some(time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()),
                                        };
                                        let _ = crate::state::mark_reviewed(
                                            &params.db_path,
                                            repo,
                                            pr.number,
                                            &meta,
                                        );
                                    }
                                }
                                continue;
                            }

                            // Skip backport PRs as they often fail to checkout
                            if pr.head_ref_name.starts_with("backport-")
                                || pr.title.starts_with("[Backport")
                            {
                                tracing::info!(repo = %repo, pr = pr.number, "Skipping backport PR");
                                skipped += 1;
                                #[allow(clippy::collapsible_if)]
                                if let Ok(decision) = crate::state::should_review(
                                    &params.db_path,
                                    repo,
                                    pr.number,
                                    &pr.head_ref_oid,
                                    false,
                                ) {
                                    if decision != crate::state::ReviewDecision::Skip {
                                        let meta = crate::state::ReviewMetadata {
                                            commit_hash: pr.head_ref_oid.clone(),
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
                                            time_reviewed: Some(time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()),
                                        };
                                        let _ = crate::state::mark_reviewed(
                                            &params.db_path,
                                            repo,
                                            pr.number,
                                            &meta,
                                        );
                                    }
                                }
                                continue;
                            }

                            // Check if already reviewed
                            match crate::state::should_review(
                                &params.db_path,
                                repo,
                                pr.number,
                                &pr.head_ref_oid,
                                false,
                            ) {
                                Ok(crate::state::ReviewDecision::FirstReview)
                                | Ok(crate::state::ReviewDecision::ReReview) => {
                                    let is_rereview = matches!(
                                        crate::state::should_review(
                                            &params.db_path,
                                            repo,
                                            pr.number,
                                            &pr.head_ref_oid,
                                            false
                                        ),
                                        Ok(crate::state::ReviewDecision::ReReview)
                                    );

                                    match crate::state::lock_for_review(&params.db_path, repo, pr.number, &pr.head_ref_oid) {
                                        Ok(true) => {
                                            tracing::debug!(repo = %repo, pr = pr.number, "Successfully locked PR for review");
                                        }
                                        Ok(false) => {
                                            tracing::info!(repo = %repo, pr = pr.number, "PR is currently locked by another process, skipping");
                                            skipped += 1;
                                            continue;
                                        }
                                        Err(e) => {
                                            tracing::error!("Failed to lock PR {} in {}: {}", pr.number, repo, e);
                                            failed += 1;
                                            continue;
                                        }
                                    }

                                    tracing::info!(repo = %repo, pr = pr.number, commit = %pr.head_ref_oid, "New PR or commit needs review");

                                    let safe_repo = repo.replace('/', "_");
                                    let out_file_name = format!(
                                        "{}_PR{}_{}_report.md",
                                        safe_repo,
                                        pr.number,
                                        &pr.head_ref_oid[..7]
                                    );
                                    let output_path =
                                        params.out_dir.as_ref().map(|dir| dir.join(out_file_name));

                                    let review_params = ReviewParams {
                                        repo: repo.clone(),
                                        pr_number: pr.number,
                                        model: params.model.clone(),
                                        output: output_path,
                                        skill: params.skill.clone(),
                                        persona: params.persona.clone(),
                                        max_turns: params.max_turns,
                                        timeout_mins: params.timeout_mins,
                                        db_path: params.db_path.clone(),
                                        force: false,
                                        max_retries: params.max_retries,
                                        retry_delay_secs: params.retry_delay_secs,
                                        disclose_config: params.disclose_config.clone(),
                                        context_groups: params.context_groups.clone(),
                                        max_cost_usd: params.max_cost_usd,
                                        input_price_per_m: params.input_price_per_m,
                                        output_price_per_m: params.output_price_per_m,
                                        is_rereview,
                                        execution: ReviewExecution {
                                            skip_state_check: false,
                                            persist_side_effects: true,
                                            result_json: None,
                                        },
                                    };

                                    let review_result = if params.sandbox_rootfs.is_some() {
                                        run_sandboxed_review(
                                            &params,
                                            &review_params,
                                            cancel_token.clone(),
                                        )
                                        .await
                                    } else {
                                        run_review(review_params, cancel_token.clone())
                                            .await
                                            .map(|_| ())
                                    };

                                    if let Err(e) = review_result {
                                        let meta = crate::state::ReviewMetadata {
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
                                            time_reviewed: Some(time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()),
                                        };
                                        let _ = crate::state::mark_reviewed(
                                            &params.db_path,
                                            repo,
                                            pr.number,
                                            &meta,
                                        );

                                        if cancel_token.is_cancelled() {
                                            return Err(e);
                                        }
                                        tracing::error!(
                                            "Failed to review PR {} in {}: {}",
                                            pr.number,
                                            repo,
                                            e
                                        );
                                        failed += 1;
                                        if crate::review::is_fatal_error(&e) {
                                            tracing::error!(
                                                "Fatal error encountered, stopping daemon"
                                            );
                                            return Err(e);
                                        }
                                    } else {
                                        reviewed += 1;
                                    }
                                }
                                Ok(crate::state::ReviewDecision::Skip) => {
                                    // Already reviewed
                                    skipped += 1;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to check review state for PR {} in {}: {}",
                                        pr.number,
                                        repo,
                                        e
                                    );
                                    failed += 1;
                                }
                            }
                        }

                        tracing::info!(
                            repo = %repo,
                            state = %state,
                            total = prs.len(),
                            reviewed = reviewed,
                            skipped = skipped,
                            failed = failed,
                            "Finished {} PR processing for {}",
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
            _ = cancel_token.cancelled() => {
                tracing::info!("Sleep interrupted, shutting down");
            }
        }
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
        if net != "veth" && net != "private" && net != "host" {
            anyhow::bail!(
                "Invalid sandbox network mode: {}. Must be host, private, or veth.",
                net
            );
        }
    }

    let run_dir = sandbox_run_dir(
        params.out_dir.as_deref(),
        &review_params.repo,
        review_params.pr_number,
    )?;
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("Failed to create sandbox run directory at {}", run_dir.display()))?;
    let report_path = run_dir.join("report.md");
    let result_json = run_dir.join("result.json");
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
            format!("Failed to create sandbox runtime directory at {}", path.display())
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
    cmd.arg(format!("--setenv=XDG_STATE_HOME={}", sandbox_xdg_state_home));
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
    if let Ok(val) = std::env::var("GITHUB_TOKEN") {
        cmd.arg(format!("--setenv=GITHUB_TOKEN={}", val));
    }

    // Default the sandbox CA bundle path so git/gh can verify TLS even when
    // the parent service does not export Nix certificate environment vars.
    let ssl_cert_file = std::env::var("SSL_CERT_FILE")
        .unwrap_or_else(|_| "/etc/ssl/certs/ca-bundle.crt".to_string());
    let nix_ssl_cert_file = std::env::var("NIX_SSL_CERT_FILE")
        .unwrap_or_else(|_| ssl_cert_file.clone());
    cmd.arg(format!("--setenv=SSL_CERT_FILE={}", ssl_cert_file));
    cmd.arg(format!("--setenv=NIX_SSL_CERT_FILE={}", nix_ssl_cert_file));

    // Network mode.  "veth" requires the entrypoint script inside the
    // container to configure host0 -- which needs CAP_NET_ADMIN inside the
    // container's net namespace.  "host" shares the host network and needs
    // no extra capabilities.  "private" gives loopback only.
    #[allow(clippy::collapsible_if)]
    if let Some(net) = &params.sandbox_network {
        if net == "veth" {
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
    cmd.arg("--model").arg(&review_params.model);

    let _ = &review_params.output;
    cmd.arg("--output").arg("/sandbox-output/report.md");
    if let Some(skill) = &review_params.skill {
        cmd.arg("--with-skill").arg(skill);
    }
    cmd.arg("--persona").arg(review_params.persona.to_string());
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
        network = ?params.sandbox_network,
        "Launching sandboxed review"
    );

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
    let report_url = crate::disclose::handle_disclosure(
        &report_path,
        &review_params.repo,
        review_params.pr_number,
        completed.metadata.commit_hash.as_str(),
        completed.should_notify,
        &review_params.disclose_config,
    )
    .await?;

    let mut metadata = completed.metadata;
    metadata.report_url = report_url;
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

fn sandbox_run_dir(base_out_dir: Option<&Path>, repo: &str, pr_number: u64) -> Result<PathBuf> {
    let base_dir = match base_out_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()
            .context("Failed to get current working directory")?
            .join("reports"),
    };
    let safe_repo = repo.replace('/', "_");
    Ok(base_dir
        .join("runs")
        .join(format!("{}_PR{}", safe_repo, pr_number)))
}

fn read_completed_review(path: &Path) -> Result<CompletedReview> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read sandbox result JSON at {}", path.display()))?;
    serde_json::from_slice(&bytes).context("Failed to parse sandbox result JSON")
}
