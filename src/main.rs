mod config;
mod daemon;
mod disclose;
mod persona;
mod review;
mod state;
mod workspace;

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};

use self::config::FiachConfig;
use self::disclose::ReportMode;

/// Fiach — Autonomous AI-powered PR reviewer using goose.
#[derive(Parser, Debug)]
#[command(name = "fiach", version, about)]
struct Cli {
    /// Path to a TOML configuration file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a security review for a single PR
    Review {
        /// GitHub repository to review (e.g., "org/repo")
        #[arg(long)]
        repo: String,

        /// PR number to review
        #[arg(long)]
        pr: u64,

        /// OpenRouter model to use
        #[arg(long)]
        model: Option<String>,

        /// Path to write the security report. If not provided, defaults to
        /// "./reports/PR{pr_number}_{commit_hash}.md" in the current working directory.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Explicitly instruct the agent to use a specific skill.
        #[arg(long)]
        with_skill: Option<String>,

        /// Path to the persona prompt file (e.g. ./custom.md) or a builtin (builtin:security, builtin:code-quality).
        #[arg(long)]
        persona: Option<String>,

        /// Maximum number of turns for the agent (prevents runaway costs)
        #[arg(long)]
        max_turns: Option<u32>,

        /// Timeout in minutes for the entire review session
        #[arg(long)]
        timeout_mins: Option<u64>,

        /// Path to the redb state database
        #[arg(long)]
        db_path: Option<PathBuf>,

        /// Force a review even if the commit has already been reviewed
        #[arg(long)]
        force: bool,

        /// Maximum number of retries for LLM provider failures
        #[arg(long)]
        max_retries: Option<u32>,

        /// Initial delay in seconds before retrying an LLM failure
        #[arg(long)]
        retry_delay_secs: Option<u64>,

        /// Mode for reporting findings
        #[arg(long)]
        report_mode: Option<String>,

        /// Sync repository for SyncPr mode (e.g., kelbie/security-audits)
        #[arg(long)]
        sync_repo: Option<String>,

        /// Notify even if no vulnerabilities are found
        #[arg(long)]
        notify_on_empty: Option<bool>,

        /// Maximum budget in USD for this review
        #[arg(long)]
        max_cost: Option<f64>,

        /// Override input token price per 1M tokens (USD)
        #[arg(long)]
        input_price: Option<f64>,

        /// Override output token price per 1M tokens (USD)
        #[arg(long)]
        output_price: Option<f64>,

        /// Internal: skip DB state checks and persistence for sandbox child reviews
        #[arg(long, hide = true)]
        sandbox_child: bool,

        /// Internal: write structured sandbox review result to JSON
        #[arg(long, hide = true)]
        result_json: Option<PathBuf>,
    },
    /// Run as a daemon that polls for open PRs
    Daemon {
        /// Comma-separated list of GitHub repositories to monitor (e.g., "org/repo1,org/repo2")
        #[arg(long)]
        repos: Option<String>,

        /// Interval in seconds between polling cycles
        #[arg(long)]
        interval: Option<u64>,

        /// OpenRouter model to use
        #[arg(long)]
        model: Option<String>,

        /// Explicitly instruct the agent to use a specific skill.
        #[arg(long)]
        with_skill: Option<String>,

        /// Path to the persona prompt file (e.g. ./custom.md) or a builtin (builtin:security, builtin:code-quality).
        #[arg(long)]
        persona: Option<String>,

        /// Maximum number of turns for the agent
        #[arg(long)]
        max_turns: Option<u32>,

        /// Timeout in minutes for each review session
        #[arg(long)]
        timeout_mins: Option<u64>,

        /// Path to the redb state database
        #[arg(long)]
        db_path: Option<PathBuf>,

        /// Maximum number of retries for LLM provider failures
        #[arg(long)]
        max_retries: Option<u32>,

        /// Initial delay in seconds before retrying an LLM failure
        #[arg(long)]
        retry_delay_secs: Option<u64>,

        /// Directory to store reports (defaults to "./reports" in current dir if not provided)
        #[arg(long)]
        out_dir: Option<PathBuf>,

        /// Mode for reporting findings
        #[arg(long)]
        report_mode: Option<String>,

        /// Sync repository for SyncPr mode (e.g., kelbie/security-audits)
        #[arg(long)]
        sync_repo: Option<String>,

        /// Notify even if no vulnerabilities are found
        #[arg(long)]
        notify_on_empty: Option<bool>,

        /// Maximum budget in USD for each review
        #[arg(long)]
        max_cost: Option<f64>,

        /// State of PRs to review (open, merged, closed, or all)
        #[arg(long)]
        pr_state: Option<String>,

        /// Comma-separated list of PRs to skip (e.g., "123,org/repo#456")
        #[arg(long)]
        skip_prs: Option<String>,

        /// Whether to fetch drafts (true), only ready PRs (false), or both (omitted). Default is false.
        #[arg(long)]
        drafts: Option<bool>,

        /// Override input token price per 1M tokens (USD)
        #[arg(long)]
        input_price: Option<f64>,

        /// Override output token price per 1M tokens (USD)
        #[arg(long)]
        output_price: Option<f64>,

        /// Rootfs path for sandboxed execution via systemd-nspawn
        #[arg(long)]
        sandbox_rootfs: Option<PathBuf>,

        /// Network mode for sandbox (e.g. host, private, veth)
        #[arg(long)]
        sandbox_network: Option<String>,

        /// Extra arguments to pass to systemd-nspawn
        #[arg(long)]
        sandbox_extra_args: Option<Vec<String>>,
    },
    /// List history of reviewed PRs
    History {
        /// Path to the redb state database
        #[arg(long)]
        db_path: Option<PathBuf>,

        /// Filter by GitHub repository (e.g., "org/repo")
        #[arg(long)]
        repo: Option<String>,

        /// Output in JSON format instead of a table
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (non-fatal if missing)
    let _ = dotenvy::dotenv();

    // Initialize tracing (respects RUST_LOG env var, defaults to info)
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("fiach=info,goose=warn,rmcp=warn,sacp=warn,reqwest=warn,hyper=warn")
        }))
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Load config
    let config = match FiachConfig::load(cli.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("Failed to load config file: {}", e);
            FiachConfig::default()
        }
    };

    let cancel_token = CancellationToken::new();
    let cloned_token = cancel_token.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        tracing::warn!("Ctrl-C received, shutting down...");
        cloned_token.cancel();
    });

    match cli.command {
        Commands::Review {
            repo,
            pr,
            model,
            output,
            with_skill,
            persona,
            max_turns,
            timeout_mins,
            db_path,
            force,
            max_retries,
            retry_delay_secs,
            report_mode,
            sync_repo,
            notify_on_empty,
            max_cost,
            input_price,
            output_price,
            sandbox_child,
            result_json,
        } => {
            let rev_cfg = config.review.unwrap_or_default();

            let model = model
                .or(rev_cfg.model)
                .unwrap_or_else(|| "google/gemini-3.1-pro-preview".to_string());
            let persona_str = persona
                .or(rev_cfg.persona)
                .unwrap_or_else(|| "builtin:security".to_string());
            let persona = persona::PersonaSource::from_str(&persona_str).unwrap();
            let report_mode_str = report_mode
                .or(rev_cfg.report_mode)
                .unwrap_or_else(|| "local".to_string());
            let report_mode = ReportMode::from_str(&report_mode_str).unwrap_or_default();

            tracing::info!(
                repo = %repo,
                pr = pr,
                model = %model,
                output = ?output.clone().or_else(|| rev_cfg.output.clone()),
                with_skill = ?with_skill.clone().or_else(|| rev_cfg.with_skill.clone()),
                persona = ?persona,
                "Starting single PR review"
            );

            let params = review::ReviewParams {
                repo,
                pr_number: pr,
                model,
                output: output.or(rev_cfg.output),
                skill: with_skill.or(rev_cfg.with_skill),
                persona,
                max_turns: max_turns.or(rev_cfg.max_turns).unwrap_or(60),
                timeout_mins: timeout_mins.or(rev_cfg.timeout_mins).unwrap_or(30),
                db_path: db_path
                    .or(rev_cfg.db_path)
                    .unwrap_or_else(|| PathBuf::from("fiach.redb")),
                force: force || rev_cfg.force.unwrap_or(false),
                max_retries: max_retries.or(rev_cfg.max_retries).unwrap_or(3),
                retry_delay_secs: retry_delay_secs.or(rev_cfg.retry_delay_secs).unwrap_or(10),
                disclose_config: disclose::DiscloseConfig {
                    mode: report_mode,
                    sync_repo: sync_repo.or(rev_cfg.sync_repo),
                    notify_on_empty: notify_on_empty.or(rev_cfg.notify_on_empty).unwrap_or(false),
                },
                context_groups: config.context_groups,
                max_cost_usd: max_cost.or(rev_cfg.max_cost_usd),
                input_price_per_m: input_price.or(rev_cfg.input_price_per_m),
                output_price_per_m: output_price.or(rev_cfg.output_price_per_m),
                is_rereview: false, // In direct CLI review, we usually don't track is_rereview exactly like daemon
                execution: review::ReviewExecution {
                    skip_state_check: sandbox_child,
                    persist_side_effects: !sandbox_child,
                    result_json,
                },
            };

            let _ = review::run_review(params, cancel_token).await?;
            Ok(())
        }
        Commands::Daemon {
            repos,
            interval,
            model,
            with_skill,
            persona,
            max_turns,
            timeout_mins,
            db_path,
            max_retries,
            retry_delay_secs,
            out_dir,
            report_mode,
            sync_repo,
            notify_on_empty,
            max_cost,
            pr_state,
            skip_prs,
            drafts,
            input_price,
            output_price,
            sandbox_rootfs,
            sandbox_network,
            sandbox_extra_args,
        } => {
            let daemon_cfg = config.daemon.unwrap_or_default();

            let repos_str = repos
                .or_else(|| daemon_cfg.repos.map(|r| r.join(",")))
                .unwrap_or_else(|| "".to_string());
            if repos_str.is_empty() {
                anyhow::bail!(
                    "No repositories specified. Provide them via --repos or config file."
                );
            }

            let model = model
                .or(daemon_cfg.model)
                .unwrap_or_else(|| "google/gemini-3.1-pro-preview".to_string());
            let persona_str = persona
                .or(daemon_cfg.persona)
                .unwrap_or_else(|| "builtin:security".to_string());
            let persona = persona::PersonaSource::from_str(&persona_str).unwrap();
            let report_mode_str = report_mode
                .or(daemon_cfg.report_mode)
                .unwrap_or_else(|| "local".to_string());
            let report_mode = ReportMode::from_str(&report_mode_str).unwrap_or_default();

            let interval_secs = interval.or(daemon_cfg.interval).unwrap_or(300);
            let pr_states = pr_state
                .map(|s| {
                    s.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .or_else(|| daemon_cfg.pr_state.as_ref().map(|ps| ps.to_vec()))
                .unwrap_or_else(|| vec!["open".to_string()]);

            let mut skip_prs_list = daemon_cfg.skip_prs.unwrap_or_default();
            if let Some(s) = skip_prs {
                skip_prs_list.extend(s.split(',').map(|s| s.trim().to_string()));
            }

            tracing::info!(
                repos = %repos_str,
                interval_secs = interval_secs,
                model = %model,
                persona = ?persona,
                pr_states = ?pr_states,
                skip_prs = ?skip_prs_list,
                "Starting fiach daemon"
            );

            let params = daemon::DaemonParams {
                repos: repos_str,
                interval: interval_secs,
                model,
                skill: with_skill.or(daemon_cfg.with_skill),
                persona,
                max_turns: max_turns.or(daemon_cfg.max_turns).unwrap_or(60),
                timeout_mins: timeout_mins.or(daemon_cfg.timeout_mins).unwrap_or(30),
                db_path: db_path
                    .or(daemon_cfg.db_path)
                    .unwrap_or_else(|| PathBuf::from("fiach.redb")),
                max_retries: max_retries.or(daemon_cfg.max_retries).unwrap_or(3),
                retry_delay_secs: retry_delay_secs
                    .or(daemon_cfg.retry_delay_secs)
                    .unwrap_or(10),
                out_dir: out_dir
                    .or(daemon_cfg.out_dir)
                    .or_else(|| Some(PathBuf::from("reports"))),
                disclose_config: disclose::DiscloseConfig {
                    mode: report_mode,
                    sync_repo: sync_repo.or(daemon_cfg.sync_repo),
                    notify_on_empty: notify_on_empty
                        .or(daemon_cfg.notify_on_empty)
                        .unwrap_or(false),
                },
                context_groups: config.context_groups,
                pr_states,
                skip_prs: skip_prs_list,
                drafts: drafts.or(daemon_cfg.drafts).or(Some(false)), // Default to false
                max_cost_usd: max_cost.or(daemon_cfg.max_cost_usd),
                input_price_per_m: input_price.or(daemon_cfg.input_price_per_m),
                output_price_per_m: output_price.or(daemon_cfg.output_price_per_m),
                sandbox_rootfs: sandbox_rootfs.or(daemon_cfg.sandbox_rootfs),
                sandbox_network: sandbox_network.or(daemon_cfg.sandbox_network),
                sandbox_extra_args: sandbox_extra_args.or(daemon_cfg.sandbox_extra_args),
            };

            daemon::run_daemon(params, cancel_token).await
        }
        Commands::History {
            db_path,
            repo,
            json,
        } => {
            let db_path = db_path.unwrap_or_else(|| PathBuf::from("fiach.redb"));
            let reviews = state::list_reviews(&db_path)?;

            let filtered_reviews: Vec<_> = if let Some(r) = repo {
                reviews
                    .into_iter()
                    .filter(|(repo_name, _, _)| repo_name == &r)
                    .collect()
            } else {
                reviews
            };

            if json {
                println!("{}", serde_json::to_string_pretty(&filtered_reviews)?);
            } else {
                println!(
                    "{:<20} | {:<5} | {:<10} | {:<10} | {:<10} | {:<8} | {:<10} | Cost",
                    "Repository", "PR", "Commit", "Type", "Status", "Findings", "Severity"
                );
                println!(
                    "{:-<20}-+-{:-<5}-+-{:-<10}-+-{:-<10}-+-{:-<10}-+-{:-<8}-+-{:-<10}-+-{:-<7}",
                    "", "", "", "", "", "", "", ""
                );

                for (repo, pr, meta) in filtered_reviews {
                    let cost_str = match meta.cost_usd {
                        Some(c) => format!("${:.3}", c),
                        None => "-".to_string(),
                    };
                    let commit_short = if meta.commit_hash.len() > 7 {
                        &meta.commit_hash[..7]
                    } else {
                        &meta.commit_hash
                    };
                    let type_str = if meta.is_rereview { "Re-Review" } else { "New" };
                    println!(
                        "{:<20} | {:<5} | {:<10} | {:<10} | {:<10} | {:<8} | {:<10} | {}",
                        repo,
                        pr,
                        commit_short,
                        type_str,
                        meta.status,
                        meta.findings_count,
                        meta.severity,
                        cost_str
                    );
                }
            }

            Ok(())
        }
    }
}
