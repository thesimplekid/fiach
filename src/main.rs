mod config;
mod daemon;
mod disclose;
mod persona;
mod reporting;
mod review;
mod server;
mod state;
mod workspace;

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};

use self::config::{FiachConfig, MultiString};
use self::disclose::ReportMode;

fn parse_report_mode(value: &str) -> Result<ReportMode> {
    ReportMode::from_str(value).map_err(|error| anyhow::anyhow!(error))
}

fn parse_persona_sources(values: Vec<String>) -> Vec<persona::PersonaSource> {
    values
        .into_iter()
        .map(|value| match persona::PersonaSource::from_str(&value) {
            Ok(source) => source,
            Err(never) => match never {},
        })
        .collect()
}

fn resolve_personas(
    cli_persona: Option<String>,
    cfg_persona: Option<MultiString>,
    cfg_personas: Option<MultiString>,
) -> Vec<persona::PersonaSource> {
    let values = cli_persona
        .map(MultiString::Single)
        .or(cfg_personas)
        .or(cfg_persona)
        .map(|personas| personas.to_vec())
        .unwrap_or_else(|| vec!["builtin:security".to_string()]);

    parse_persona_sources(values)
}

fn review_kind_for(persona: &persona::PersonaSource, use_persona_kind: bool) -> String {
    if use_persona_kind {
        persona.review_kind()
    } else {
        state::DEFAULT_REVIEW_KIND.to_string()
    }
}

fn output_for_persona(
    output: Option<PathBuf>,
    persona: &persona::PersonaSource,
    use_persona_kind: bool,
) -> Option<PathBuf> {
    if !use_persona_kind {
        return output;
    }

    let path = output?;
    let review_kind = persona.review_kind();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("md");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("report");
    let file_name = format!("{stem}.{review_kind}.{extension}");
    Some(path.with_file_name(file_name))
}

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
    /// Run a review for a single PR
    Review {
        /// GitHub repository to review (e.g., "org/repo")
        #[arg(long)]
        repo: String,

        /// PR number to review
        #[arg(long)]
        pr: u64,

        /// Model to use with the selected provider
        #[arg(long)]
        model: Option<String>,

        /// Goose provider to use (openrouter, anthropic, openai, google)
        #[arg(long)]
        provider: Option<String>,

        /// Model to use for the verifier pass. Defaults to --model.
        #[arg(long)]
        verifier_model: Option<String>,

        /// Provider to use for the verifier pass. Defaults to --provider.
        #[arg(long)]
        verifier_provider: Option<String>,

        /// Path to write the review report. If not provided, defaults to
        /// "./reports/PR{pr_number}_{commit_hash}.md" in the current working directory.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Explicitly instruct the agent to use a specific skill.
        #[arg(long)]
        with_skill: Option<String>,

        /// Path to the persona prompt file or builtin. Can be comma-separated.
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

        /// Maximum number of retries for LLM provider failures and failed review attempts
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

        /// Notify even if no findings are found
        #[arg(long)]
        notify_on_empty: Option<bool>,

        /// GitHub reaction to add when review starts (e.g. eyes, rocket)
        #[arg(long)]
        review_start_reaction: Option<String>,

        /// GitHub reaction to add when review completes with no findings
        #[arg(long)]
        no_findings_reaction: Option<String>,

        /// Run a verifier pass before disclosure when findings are present
        #[arg(long)]
        verify_findings: Option<bool>,

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

        /// Internal: persona-scoped state/report identity for sandbox child reviews
        #[arg(long, hide = true)]
        review_kind: Option<String>,
    },
    /// Run as a daemon that polls for open PRs
    Daemon {
        /// Comma-separated list of GitHub repositories to monitor (e.g., "org/repo1,org/repo2")
        #[arg(long)]
        repos: Option<String>,

        /// Interval in seconds between polling cycles
        #[arg(long)]
        interval: Option<u64>,

        /// Model to use with the selected provider
        #[arg(long)]
        model: Option<String>,

        /// Goose provider to use (openrouter, anthropic, openai, google)
        #[arg(long)]
        provider: Option<String>,

        /// Model to use for the verifier pass. Defaults to --model.
        #[arg(long)]
        verifier_model: Option<String>,

        /// Provider to use for the verifier pass. Defaults to --provider.
        #[arg(long)]
        verifier_provider: Option<String>,

        /// Explicitly instruct the agent to use a specific skill.
        #[arg(long)]
        with_skill: Option<String>,

        /// Path to the persona prompt file or builtin. Can be comma-separated.
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

        /// Maximum number of retries for LLM provider failures and failed review attempts
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

        /// Notify even if no findings are found
        #[arg(long)]
        notify_on_empty: Option<bool>,

        /// GitHub reaction to add when review starts (e.g. eyes, rocket)
        #[arg(long)]
        review_start_reaction: Option<String>,

        /// GitHub reaction to add when review completes with no findings
        #[arg(long)]
        no_findings_reaction: Option<String>,

        /// Run a verifier pass before disclosure when findings are present
        #[arg(long)]
        verify_findings: Option<bool>,

        /// Maximum budget in USD for each review
        #[arg(long)]
        max_cost: Option<f64>,

        /// State of PRs to review (open, merged, closed, or all)
        #[arg(long)]
        pr_state: Option<String>,

        /// Comma-separated list of PRs to skip (e.g., "123,org/repo#456")
        #[arg(long)]
        skip_prs: Option<String>,

        /// Comma-separated list of GitHub author associations allowed to trigger reviews
        #[arg(long)]
        allowed_author_associations: Option<String>,

        /// Maximum number of PR reviews to run concurrently per polling query. 0 means unlimited.
        #[arg(long)]
        max_workers: Option<usize>,

        /// Whether to fetch drafts (true), only ready PRs (false), or both (omitted). Default is false.
        #[arg(long)]
        drafts: Option<bool>,

        /// Override input token price per 1M tokens (USD)
        #[arg(long)]
        input_price: Option<f64>,

        /// Override output token price per 1M tokens (USD)
        #[arg(long)]
        output_price: Option<f64>,

        /// Number of days to look back for updated PRs
        #[arg(long)]
        updated_within_days: Option<u32>,

        /// Whether to add an updated:>= filter to PR discovery
        #[arg(long)]
        filter_by_updated: Option<bool>,

        /// Maximum number of PRs to fetch from GitHub
        #[arg(long)]
        pr_limit: Option<u32>,

        /// Rootfs path for sandboxed execution via systemd-nspawn
        #[arg(long)]
        sandbox_rootfs: Option<PathBuf>,

        /// Network mode for sandbox (e.g. host, bridge, private, veth)
        #[arg(long)]
        sandbox_network: Option<String>,

        /// Extra arguments to pass to systemd-nspawn
        #[arg(long)]
        sandbox_extra_args: Option<Vec<String>>,

        /// Port for the interactive web server (defaults to 3000)
        #[arg(long)]
        port: Option<u16>,
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
        .with_ansi(false)
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
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %error, "Failed to listen for Ctrl-C");
            return;
        }
        tracing::warn!("Ctrl-C received, shutting down...");
        cloned_token.cancel();
    });

    match cli.command {
        Commands::Review {
            repo,
            pr,
            model,
            provider,
            verifier_model,
            verifier_provider,
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
            review_start_reaction,
            no_findings_reaction,
            verify_findings,
            max_cost,
            input_price,
            output_price,
            sandbox_child,
            result_json,
            review_kind,
        } => {
            let rev_cfg = config.review.unwrap_or_default();

            let model = model
                .or(rev_cfg.model)
                .unwrap_or_else(|| "google/gemini-3.1-pro-preview".to_string());
            let provider = provider
                .or(rev_cfg.provider)
                .unwrap_or_else(|| "openrouter".to_string());
            let verifier_model = verifier_model.or(rev_cfg.verifier_model);
            let verifier_provider = verifier_provider.or(rev_cfg.verifier_provider);
            let personas = resolve_personas(persona, rev_cfg.persona, rev_cfg.personas);
            let use_persona_kind = personas.len() > 1;
            let report_mode_str = report_mode
                .or(rev_cfg.report_mode)
                .unwrap_or_else(|| "local".to_string());
            let report_mode = parse_report_mode(&report_mode_str)?;
            let output = output.or(rev_cfg.output);
            let skill = with_skill.or(rev_cfg.with_skill);
            let db_path = db_path
                .or(rev_cfg.db_path)
                .unwrap_or_else(|| PathBuf::from("fiach.redb"));

            tracing::info!(
                repo = %repo,
                pr = pr,
                provider = %provider,
                model = %model,
                output = ?output,
                with_skill = ?skill,
                personas = ?personas,
                "Starting single PR review"
            );

            for persona in personas {
                let params = review::ReviewParams {
                    repo: repo.clone(),
                    pr_number: pr,
                    model: model.clone(),
                    provider: provider.clone(),
                    verifier_model: verifier_model.clone(),
                    verifier_provider: verifier_provider.clone(),
                    output: output_for_persona(output.clone(), &persona, use_persona_kind),
                    skill: skill.clone(),
                    persona: persona.clone(),
                    max_turns: max_turns.or(rev_cfg.max_turns).unwrap_or(60),
                    timeout_mins: timeout_mins.or(rev_cfg.timeout_mins).unwrap_or(30),
                    db_path: db_path.clone(),
                    review_kind: review_kind
                        .clone()
                        .unwrap_or_else(|| review_kind_for(&persona, use_persona_kind)),
                    force: force || rev_cfg.force.unwrap_or(false),
                    max_retries: max_retries.or(rev_cfg.max_retries).unwrap_or(3),
                    retry_delay_secs: retry_delay_secs.or(rev_cfg.retry_delay_secs).unwrap_or(10),
                    disclose_config: disclose::DiscloseConfig {
                        mode: report_mode.clone(),
                        sync_repo: sync_repo.clone().or(rev_cfg.sync_repo.clone()),
                        notify_on_empty: notify_on_empty
                            .or(rev_cfg.notify_on_empty)
                            .unwrap_or(false),
                        reactions: disclose::ReactionConfig::with_defaults(
                            review_start_reaction
                                .clone()
                                .or(rev_cfg.review_start_reaction.clone()),
                            no_findings_reaction
                                .clone()
                                .or(rev_cfg.no_findings_reaction.clone()),
                        ),
                    },
                    verify_findings: verify_findings.or(rev_cfg.verify_findings).unwrap_or(true),
                    context_groups: config.context_groups.clone(),
                    max_cost_usd: max_cost.or(rev_cfg.max_cost_usd),
                    input_price_per_m: input_price.or(rev_cfg.input_price_per_m),
                    output_price_per_m: output_price.or(rev_cfg.output_price_per_m),
                    is_rereview: false,
                    execution: review::ReviewExecution {
                        skip_state_check: sandbox_child,
                        persist_side_effects: !sandbox_child,
                        result_json: output_for_persona(
                            result_json.clone(),
                            &persona,
                            use_persona_kind,
                        ),
                    },
                };

                let _ = review::run_review(params, cancel_token.clone()).await?;
            }
            Ok(())
        }
        Commands::Daemon {
            repos,
            interval,
            model,
            provider,
            verifier_model,
            verifier_provider,
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
            review_start_reaction,
            no_findings_reaction,
            verify_findings,
            max_cost,
            pr_state,
            skip_prs,
            allowed_author_associations,
            max_workers,
            drafts,
            input_price,
            output_price,
            updated_within_days,
            filter_by_updated,
            pr_limit,
            sandbox_rootfs,
            sandbox_network,
            sandbox_extra_args,
            port,
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
            let provider = provider
                .or(daemon_cfg.provider)
                .unwrap_or_else(|| "openrouter".to_string());
            let verifier_model = verifier_model.or(daemon_cfg.verifier_model);
            let verifier_provider = verifier_provider.or(daemon_cfg.verifier_provider);
            let personas = resolve_personas(persona, daemon_cfg.persona, daemon_cfg.personas);
            let report_mode_str = report_mode
                .or(daemon_cfg.report_mode)
                .unwrap_or_else(|| "local".to_string());
            let report_mode = parse_report_mode(&report_mode_str)?;

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

            let allowed_author_associations = allowed_author_associations
                .map(|s| {
                    s.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .or(daemon_cfg.allowed_author_associations)
                .unwrap_or_else(|| {
                    vec![
                        "COLLABORATOR".to_string(),
                        "CONTRIBUTOR".to_string(),
                        "MEMBER".to_string(),
                        "OWNER".to_string(),
                    ]
                });
            let max_workers = max_workers.or(daemon_cfg.max_workers).unwrap_or(1);

            tracing::info!(
                repos = %repos_str,
                interval_secs = interval_secs,
                provider = %provider,
                model = %model,
                personas = ?personas,
                pr_states = ?pr_states,
                skip_prs = ?skip_prs_list,
                allowed_author_associations = ?allowed_author_associations,
                max_workers = max_workers,
                "Starting fiach daemon"
            );

            let params = daemon::DaemonParams {
                repos: repos_str,
                interval: interval_secs,
                provider,
                model,
                verifier_provider,
                verifier_model,
                skill: with_skill.or(daemon_cfg.with_skill),
                personas,
                max_turns: max_turns.or(daemon_cfg.max_turns).unwrap_or(60),
                timeout_mins: timeout_mins.or(daemon_cfg.timeout_mins).unwrap_or(30),
                db_path: db_path
                    .clone()
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
                    reactions: disclose::ReactionConfig::with_defaults(
                        review_start_reaction.or(daemon_cfg.review_start_reaction),
                        no_findings_reaction.or(daemon_cfg.no_findings_reaction),
                    ),
                },
                verify_findings: verify_findings
                    .or(daemon_cfg.verify_findings)
                    .unwrap_or(true),
                context_groups: config.context_groups,
                pr_states,
                skip_prs: skip_prs_list,
                allowed_author_associations,
                max_workers,
                drafts: drafts.or(daemon_cfg.drafts).or(Some(false)), // Default to false
                max_cost_usd: max_cost.or(daemon_cfg.max_cost_usd),
                input_price_per_m: input_price.or(daemon_cfg.input_price_per_m),
                output_price_per_m: output_price.or(daemon_cfg.output_price_per_m),
                updated_within_days: updated_within_days
                    .or(daemon_cfg.updated_within_days)
                    .unwrap_or(120),
                filter_by_updated: filter_by_updated
                    .or(daemon_cfg.filter_by_updated)
                    .unwrap_or(true),
                pr_limit: pr_limit.or(daemon_cfg.pr_limit).unwrap_or(1000),
                sandbox_rootfs: sandbox_rootfs.or(daemon_cfg.sandbox_rootfs),
                sandbox_network: sandbox_network.or(daemon_cfg.sandbox_network),
                sandbox_extra_args: sandbox_extra_args.or(daemon_cfg.sandbox_extra_args),
            };

            let port = port.or(daemon_cfg.port).unwrap_or(3000);
            let (tx, rx) = tokio::sync::mpsc::channel(100);
            let app_state = server::AppState {
                db_path: params.db_path.clone(),
                out_dir: params
                    .out_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("reports")),
                daemon_tx: tx,
                server_token: std::env::var("FIACH_SERVER_TOKEN")
                    .ok()
                    .filter(|token| !token.trim().is_empty()),
            };

            tokio::spawn(async move {
                if let Err(e) = server::start_server(port, app_state).await {
                    tracing::error!("Web server error: {}", e);
                }
            });

            daemon::run_daemon(params, rx, cancel_token).await
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
