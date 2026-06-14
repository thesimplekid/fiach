use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use goose::agents::{Agent, AgentEvent, ExtensionConfig, SessionConfig};
use goose::config::GooseMode;
use goose::conversation::message::{Message, MessageContent};
use goose::providers::canonical::maybe_get_canonical_model;
use goose::providers::create_with_named_model;
use goose::session::session_manager::SessionType;
use rmcp::model::{CallToolResult, Content, Role};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::disclose;
use crate::reporting::{self, ReportingArtifact, ReviewPhase};
use crate::state;
use crate::workspace;

const SANDBOX_SKILLS_DIR: &str = "/etc/fiach/skills";

fn resolve_skills_dir() -> Result<Option<PathBuf>> {
    let current_dir = std::env::current_dir().context("Failed to get current directory")?;
    let workspace_skills_dir = current_dir.join(".agents").join("skills");
    if workspace_skills_dir.is_dir() {
        return Ok(Some(workspace_skills_dir));
    }

    let packaged_skills_dir = PathBuf::from(SANDBOX_SKILLS_DIR);
    if packaged_skills_dir.is_dir() {
        return Ok(Some(packaged_skills_dir));
    }

    Ok(None)
}

fn list_available_skills(skills_dir: Option<&std::path::Path>) -> Vec<String> {
    let Some(skills_dir) = skills_dir else {
        return Vec::new();
    };

    let mut available = Vec::new();
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && entry.path().join("SKILL.md").exists()
                && let Ok(name) = entry.file_name().into_string()
            {
                available.push(name);
            }
        }
    }

    available
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletedReview {
    pub metadata: state::ReviewMetadata,
    pub should_notify: bool,
    pub report_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewExecution {
    pub skip_state_check: bool,
    pub persist_side_effects: bool,
    pub result_json: Option<PathBuf>,
}

/// Parameters for a single PR review.
pub struct ReviewParams {
    /// GitHub repository (e.g., "org/repo")
    pub repo: String,
    /// PR number to review
    pub pr_number: u64,
    /// Model identifier for the configured provider.
    pub model: String,
    /// Goose provider name (e.g., openrouter, anthropic, openai, google).
    pub provider: String,
    /// Optional model override for the verifier pass.
    pub verifier_model: Option<String>,
    /// Optional provider override for the verifier pass.
    pub verifier_provider: Option<String>,
    /// Optional path to write the final report. If None, it will be generated
    /// in the current working directory as "PR{pr}_{hash}.md" after the
    /// workspace is prepared.
    pub output: Option<PathBuf>,
    /// Optional domain skill name or path (e.g., "my-skill" or "./skills/my-skill")
    pub skill: Option<String>,
    /// Path to the persona file or builtin
    pub persona: crate::persona::PersonaSource,
    /// Maximum number of turns for the agent
    pub max_turns: u32,
    /// Timeout in minutes for the session
    pub timeout_mins: u64,
    /// Path to the redb database
    pub db_path: PathBuf,
    /// Persona-specific review identity used for report paths and state keys.
    pub review_kind: String,
    /// Force a review even if it was already done
    pub force: bool,
    /// Maximum number of retries for LLM provider failures
    pub max_retries: u32,
    /// Initial delay in seconds before retrying an LLM failure
    pub retry_delay_secs: u64,
    /// Configuration for disclosing the report
    pub disclose_config: disclose::DiscloseConfig,
    /// Run a second verifier pass before disclosure when findings would notify.
    pub verify_findings: bool,
    pub context_groups: std::collections::HashMap<String, crate::config::ContextGroup>,
    /// Maximum budget in USD for this review
    pub max_cost_usd: Option<f64>,
    /// Override input token price per 1M tokens (USD)
    pub input_price_per_m: Option<f64>,
    /// Override output token price per 1M tokens (USD)
    pub output_price_per_m: Option<f64>,
    pub is_rereview: bool,
    pub execution: ReviewExecution,
}

type SharedReportingArtifact = Arc<Mutex<ReportingArtifact>>;

/// Run a review of a GitHub PR using the goose agent.
///
/// This function:
/// 1. Prepares a workspace (clone repo, checkout PR) — no agent turns wasted on setup
/// 2. Creates an OpenRouter LLM provider
/// 3. Initializes a goose Agent with a hidden session rooted in the workspace
/// 4. Appends the selected persona to the system prompt (extras mode)
/// 5. Loads the `developer` Platform extension (in-process, no subprocess)
/// 6. Sends the review request and streams the agent's response to stdout
pub async fn run_review(
    params: ReviewParams,
    cancel_token: CancellationToken,
) -> Result<Option<CompletedReview>> {
    let start_time = Instant::now();
    let mut peak_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut total_processed_tokens = 0u64; // For informational logging
    let skills_dir = resolve_skills_dir()?;
    let reporting_artifact = Arc::new(Mutex::new(ReportingArtifact::default()));

    if let Some(skill_name) = &params.skill {
        let skill_path = skills_dir
            .as_ref()
            .map(|dir| dir.join(skill_name).join("SKILL.md"))
            .unwrap_or_else(|| {
                PathBuf::from(".agents")
                    .join("skills")
                    .join(skill_name)
                    .join("SKILL.md")
            });
        if !skill_path.exists() {
            let available = list_available_skills(skills_dir.as_deref());
            if available.is_empty() {
                bail!(
                    "Skill '{}' not found at {}. No skills available in workspace or packaged skills directories.",
                    skill_name,
                    skill_path.display()
                );
            } else {
                bail!(
                    "Skill '{}' not found at {}. Available skills: {}",
                    skill_name,
                    skill_path.display(),
                    available.join(", ")
                );
            }
        }
    }

    // 1. Prepare workspace: clone repo and checkout PR branch
    //    The agent starts already inside the checked-out PR branch,
    //    matching the ctf-pr-reviewer pattern.
    let context_group = params.context_groups.get(&params.repo);
    let workspace = workspace::prepare(&params.repo, params.pr_number, None, context_group).await?;

    if !params.execution.skip_state_check {
        let decision = state::should_review(
            &params.db_path,
            &params.repo,
            params.pr_number,
            &workspace.commit_hash,
            &params.review_kind,
            params.force,
            params.timeout_mins,
        )?;

        if decision == state::ReviewDecision::Skip {
            workspace.cleanup().await?;
            return Ok(None);
        }
    }

    let report_path = match params.output {
        Some(path) => {
            if path.is_absolute() {
                path.to_str()
                    .context("Output path must be valid UTF-8")?
                    .to_string()
            } else {
                std::env::current_dir()
                    .context("Failed to get current working directory")?
                    .join(path)
                    .to_str()
                    .context("Output path must be valid UTF-8")?
                    .to_string()
            }
        }
        None => {
            let hash = &workspace.commit_hash[..workspace.commit_hash.len().min(7)];
            let reports_dir = std::env::current_dir()
                .context("Failed to get current working directory")?
                .join("reports");
            let file_name = if params.review_kind == state::DEFAULT_REVIEW_KIND {
                format!("PR{}_{}.md", params.pr_number, hash)
            } else {
                format!("PR{}_{}_{}.md", params.pr_number, hash, params.review_kind)
            };
            reports_dir
                .join(file_name)
                .to_str()
                .context("Output path must be valid UTF-8")?
                .to_string()
        }
    };

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(&report_path).parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create report directory at {}", parent.display())
        })?;
    }

    tracing::info!(
        repo = %params.repo,
        pr = params.pr_number,
        model = %params.model,
        output = %report_path,
        commit = %workspace.commit_hash,
        "Starting review"
    );

    if params.execution.persist_side_effects
        && let Some(reaction) = params.disclose_config.reactions.review_start.as_deref()
        && let Err(error) =
            disclose::post_pr_reaction(&params.repo, params.pr_number, reaction).await
    {
        tracing::warn!(
            repo = %params.repo,
            pr = params.pr_number,
            error = %error,
            "Failed to post review start reaction"
        );
    }

    // 2. Create the configured LLM provider
    let provider = create_with_named_model(&params.provider, &params.model, Vec::new())
        .await
        .with_context(|| format!("Failed to create {} provider", params.provider))?;

    // 3. Create the agent and a hidden session rooted in the workspace
    let agent = Agent::new();

    let session = agent
        .config
        .session_manager
        .create_session(
            workspace.path.clone(),
            params.persona.session_name().to_string(),
            SessionType::Hidden,
            GooseMode::Auto,
        )
        .await
        .context("Failed to create agent session")?;

    // 4. Set provider on the session
    agent
        .update_provider(provider.clone(), &session.id)
        .await
        .context("Failed to update provider")?;

    // 5. If we don't have an explicit skill, try to discover one using a fast LLM call
    let mut actual_skill = params.skill.clone();

    if actual_skill.is_none() {
        tracing::debug!("No explicit skill provided. Attempting dynamic skill discovery...");

        let pr_info_output = tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &params.pr_number.to_string(),
                "--json",
                "title,body",
                "--repo",
                &params.repo,
            ])
            .output()
            .await;

        match pr_info_output {
            Ok(output) if output.status.success() => {
                let pr_json: serde_json::Value =
                    serde_json::from_slice(&output.stdout).unwrap_or_default();
                let title = pr_json.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let body = pr_json.get("body").and_then(|v| v.as_str()).unwrap_or("");

                let available_skills = list_available_skills(skills_dir.as_deref());

                if !available_skills.is_empty() {
                    let prompt = format!(
                        "You are an expert code reviewer configuring an autonomous agent.\n\
                        Your task is to select the MOST RELEVANT specialized domain skill for the following Pull Request, based on its title and description.\n\n\
                        Available skills: {}\n\n\
                        PR Title: {}\n\
                        PR Body: {}\n\n\
                        Reply with ONLY the exact name of the relevant skill from the list above. If none of the skills seem specifically relevant, reply with 'none'. Do not output any other text or reasoning.",
                        available_skills.join(", "),
                        title,
                        body
                    );

                    let discovery_message =
                        goose::conversation::message::Message::user().with_text(&prompt);

                    // Call the LLM to pick the skill
                    let model_config = provider.get_model_config();
                    match provider
                        .complete(
                            &model_config,
                            "skill-discovery",
                            "You are an expert system orchestrator.",
                            &[discovery_message],
                            &[],
                        )
                        .await
                    {
                        Ok((response, usage)) => {
                            let input = usage.usage.input_tokens.unwrap_or(0) as u64;
                            let output = usage.usage.output_tokens.unwrap_or(0) as u64;
                            peak_input_tokens = peak_input_tokens.max(input);
                            total_output_tokens += output;
                            total_processed_tokens += input + output;

                            let response_text = response.as_concat_text().trim().to_lowercase();
                            let mut selected = None;

                            for skill in &available_skills {
                                if response_text.contains(&skill.to_lowercase()) {
                                    selected = Some(skill.clone());
                                    break;
                                }
                            }

                            if let Some(s) = selected {
                                tracing::info!("LLM automatically selected skill: {}", s);
                                actual_skill = Some(s);
                            } else {
                                tracing::info!(
                                    "LLM determined no specific skill is needed (response: '{}')",
                                    response_text
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Skill discovery LLM call failed: {}. Proceeding without a specialized skill.",
                                e
                            );
                        }
                    }
                } else {
                    tracing::info!(skills_dir = ?skills_dir, "No skills available in workspace or packaged skills directories");
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("Failed to fetch PR info for skill discovery: {}", stderr);
            }
            Err(e) => {
                tracing::warn!("Failed to execute gh command for skill discovery: {}", e);
            }
        }
    }

    // 6. Append custom persona to system prompt (extras mode — preserves tool instructions)
    let raw_persona = params.persona.load_content()?;

    let skill_hint = match &actual_skill {
        Some(name) => format!(
            "You have been instructed to use the `{name}` domain skill for this review. \
             Make sure to load it, apply its domain knowledge, and list it in the \
             `skills_used` field when calling `submit_finding` or `submit_no_findings`."
        ),
        None => "No domain skill was specified for this review. Use `skills_used: [\"none\"]` \
                 when calling `submit_finding` or `submit_no_findings` unless you independently loaded a skill."
            .to_string(),
    };

    let persona_prompt = raw_persona
        .replace("{repo}", &params.repo)
        .replace("{pr_number}", &params.pr_number.to_string())
        .replace("{base_branch}", &workspace.base_commit)
        .replace("{report_path}", &report_path)
        .replace("{skill_hint}", &skill_hint);

    agent
        .extend_system_prompt("custom_persona".to_string(), persona_prompt)
        .await;

    tracing::info!(
        "Custom persona loaded from {:?} (extras mode)",
        params.persona
    );

    // 6. Load developer extension in-process via Platform config
    let developer_ext = ExtensionConfig::Platform {
        name: "developer".to_string(),
        description: "Write and edit files, and execute shell commands".to_string(),
        display_name: Some("Developer".to_string()),
        bundled: None,
        available_tools: Vec::new(),
    };
    agent
        .add_extension(developer_ext, &session.id)
        .await
        .context("Failed to load developer extension")?;

    tracing::debug!("Developer extension loaded (in-process)");

    add_reporting_extension(&agent, &session.id)
        .await
        .context("Failed to load fiach-reporting extension")?;

    tracing::debug!("fiach-reporting extension loaded (frontend in-process)");

    // Log available extensions
    for ext in agent.list_extensions().await {
        tracing::debug!(extension = %ext, "Extension available");
    }

    // Get the list of commits in this PR to check for stacked diffs
    let mut diff_base = workspace.base_commit.clone();
    let mut prev_review_context = String::new();

    let commits_output = tokio::process::Command::new("git")
        .args([
            "log",
            "--reverse",
            "--format=%H",
            &format!("{}..HEAD", workspace.base_commit),
        ])
        .current_dir(&workspace.path)
        .output()
        .await;

    if let Ok(output) = commits_output {
        #[allow(clippy::collapsible_if)]
        if output.status.success() {
            let commits_str = String::from_utf8_lossy(&output.stdout);
            let commits: Vec<&str> = commits_str
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect();

            // Iterate backwards to find the most recent reviewed commit
            for commit in commits.iter().rev() {
                if let Ok(Some(metadata)) = state::get_commit_review(
                    &params.db_path,
                    &params.repo,
                    commit,
                    &params.review_kind,
                ) {
                    tracing::info!(
                        commit = commit,
                        "Found previously reviewed commit in PR history"
                    );

                    diff_base = commit.to_string();

                    let report_link = match metadata.report_url {
                        Some(url) => format!("(Previous report: {})", url),
                        None => String::new(),
                    };

                    prev_review_context = format!(
                        "\n\nNOTE: Commits up to `{}` have already been reviewed {}. You are reviewing ONLY the new commits added since then. `BASE_BRANCH` has been set to `{}` for your diffs.",
                        commit, report_link, diff_base
                    );
                    break;
                }
            }
        }
    }

    // 7. Construct the user message — agent is already in the checked-out PR workspace
    let review_target = params.persona.review_target();
    let candidate_kind = params.persona.candidate_kind();
    let methodology_hint = params.persona.methodology_hint();
    let user_message_text = match &actual_skill {
        Some(skill_name) => format!(
            "Review PR #{pr_number} in {repo} for {review_target}. \
             The current working directory is a clone of the repository with the PR branch already checked out. \
             Focus ONLY on the changes in this PR. Do NOT run tests, builds, compilers, interpreters, scratch programs, or ad hoc reproduction code. \
             Start by reading `.pr_diff.txt`, which contains the complete patch for this review scope. \
             Verify that any finding's root cause is in `.pr_diff.txt`. For large per-file inspection, use `git diff {diff_base}...HEAD --name-only` and `BASE_BRANCH={diff_base} ./safe_diff.sh <single_file_path>`, \
             {methodology_hint}, \
             and submit each candidate finding with `submit_finding`. If you find no {candidate_kind}, call `submit_no_findings`. Do not stop before using one of these reporting tools. The host will write the final Markdown report to {report_path}.{prev_review_context}\n\n\
             IMPORTANT: Use the `{skill_name}` skill to complete this review. Use the load tool to load it if you haven't already.",
            pr_number = params.pr_number,
            repo = params.repo,
            review_target = review_target,
            diff_base = diff_base,
            methodology_hint = methodology_hint,
            candidate_kind = candidate_kind,
            report_path = report_path,
            prev_review_context = prev_review_context,
            skill_name = skill_name,
        ),
        None => format!(
            "Review PR #{pr_number} in {repo} for {review_target}. \
             The current working directory is a clone of the repository with the PR branch already checked out. \
             Focus ONLY on the changes in this PR. Do NOT run tests, builds, compilers, interpreters, scratch programs, or ad hoc reproduction code. \
             Start by reading `.pr_diff.txt`, which contains the complete patch for this review scope. \
             Verify that any finding's root cause is in `.pr_diff.txt`. For large per-file inspection, use `git diff {diff_base}...HEAD --name-only` and `BASE_BRANCH={diff_base} ./safe_diff.sh <single_file_path>`, \
             {methodology_hint}, \
             and submit each candidate finding with `submit_finding`. If you find no {candidate_kind}, call `submit_no_findings`. Do not stop before using one of these reporting tools. The host will write the final Markdown report to {report_path}.{prev_review_context}",
            pr_number = params.pr_number,
            repo = params.repo,
            review_target = review_target,
            diff_base = diff_base,
            methodology_hint = methodology_hint,
            candidate_kind = candidate_kind,
            report_path = report_path,
            prev_review_context = prev_review_context,
        ),
    };

    let user_message = Message::user().with_text(&user_message_text);

    let session_config = SessionConfig {
        id: session.id,
        schedule_id: None,
        max_turns: Some(params.max_turns),
        retry_config: None,
    };

    // 8. Stream the agent's response with a timeout
    tracing::info!(
        max_turns = params.max_turns,
        timeout_mins = params.timeout_mins,
        "Sending review request to agent..."
    );

    let review_future = async {
        let mut retries = 0;
        let mut delay = params.retry_delay_secs;
        let mut accumulated_turn_count = 0;
        let mut budget_exceeded = false;
        let mut last_assistant_text: Option<String> = None;

        let mut stream = loop {
            let user_message_clone = user_message.clone();
            let session_config_clone = session_config.clone();

            match agent
                .reply(user_message_clone, session_config_clone, None)
                .await
            {
                Ok(s) => break s,
                Err(e) => {
                    if is_fatal_error(&e) {
                        return Err(e).context("Fatal provider error");
                    }
                    if retries >= params.max_retries {
                        return Err(anyhow::anyhow!(
                            "Failed to start agent reply stream after {} retries: {}",
                            retries,
                            e
                        ));
                    }
                    tracing::info!(
                        "Failed to start agent reply stream (attempt {}/{}): {}. Retrying in {}s...",
                        retries + 1,
                        params.max_retries,
                        e,
                        delay
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    retries += 1;
                    delay *= 2; // exponential backoff
                }
            }
        };

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::warn!("Review cancelled by user (Ctrl+C)");
                    bail!("Review cancelled by user");
                }
                event_opt = stream.next() => {
                    match event_opt {
                        Some(Ok(AgentEvent::Message(message))) => {
                            handle_reporting_tool_requests(
                                &agent,
                                &message,
                                ReviewPhase::Finder,
                                reporting_artifact.clone(),
                            )
                            .await?;

                            // Log each message to trace for debugging
                            if let Ok(json) = serde_json::to_string_pretty(&message) {
                                tracing::trace!(message = %json, "Agent message");
                            }

                            // Detect turns (LLM responses)
                            if message.role == Role::Assistant {
                                accumulated_turn_count += 1;
                                let text = message.as_concat_text();
                                if !text.trim().is_empty() {
                                    last_assistant_text = Some(text);
                                }

                                 // Check usage and budget every 5 turns
                                 if (accumulated_turn_count % 5 == 0 || accumulated_turn_count == 1)
                                     && let Ok(session) = agent
                                         .config
                                         .session_manager
                                         .get_session(&session_config.id, false)
                                         .await
                                 {
                                     let current_input =
                                         session.accumulated_input_tokens.unwrap_or(0).max(0) as u64;

                                        let current_output = session.accumulated_output_tokens.unwrap_or(0).max(0) as u64;

                                        // Heuristic: peak input in a session is roughly the history size of the last turn.
                                        peak_input_tokens = peak_input_tokens.max((2 * current_input) / (accumulated_turn_count as u64 + 1));

                                        let current_cost = estimate_cost(
                                            &params.provider,
                                            &params.model,
                                            peak_input_tokens,
                                            current_output + total_output_tokens, // Include discovery output
                                            params.input_price_per_m,
                                            params.output_price_per_m
                                        ).unwrap_or(0.0);

                                        tracing::info!(
                                            turn = accumulated_turn_count,
                                            max_turns = params.max_turns,
                                            cost = %format!("${:.2}", current_cost),
                                            "Review in progress..."
                                        );

                                         // Budget check
                                         if let Some(max_cost) = params.max_cost_usd
                                             && current_cost > max_cost
                                             && !budget_exceeded
                                         {
                                             tracing::warn!(
                                                 cost = %format!("${:.2}", current_cost),
                                                 max = %format!("${:.2}", max_cost),
                                                 "Budget exceeded! Requesting immediate report..."
                                             );
                                             budget_exceeded = true;

                                             let budget_nudge = "BUDGET EXCEEDED! Stop analyzing and submit your current result now. Use `submit_finding` for each candidate, or `submit_no_findings` if there are no candidates. Do not do anything else.".to_string();
                                             let follow_up_message = Message::user().with_text(&budget_nudge);

                                             tracing::info!("Nudging agent to finalize report due to budget...");

                                             let mut s_opt = None;
                                             let mut last_err = None;
                                             while retries <= params.max_retries {
                                                 match agent
                                                     .reply(
                                                         follow_up_message.clone(),
                                                         session_config.clone(),
                                                         None,
                                                     )
                                                     .await
                                                 {
                                                     Ok(s) => {
                                                         s_opt = Some(s);
                                                         break;
                                                     }
                                                     Err(e) => {
                                                         tracing::error!(
                                                             "Failed to send budget nudge: {}, retrying...",
                                                             e
                                                         );
                                                         last_err = Some(e);
                                                         retries += 1;
                                                         tokio::time::sleep(Duration::from_secs(
                                                             delay,
                                                         ))
                                                         .await;
                                                         delay *= 2;
                                                     }
                                                 }
                                             }
                                             match s_opt {
                                                 Some(s) => {
                                                     stream = s;
                                                     continue;
                                                 }
                                                 None => {
                                                     if let Some(err) = last_err {
                                                         tracing::warn!("Failed to restart stream for budget nudge after retries. Last error: {}", err);
                                                     } else {
                                                         tracing::warn!("Failed to restart stream for budget nudge after retries.");
                                                     }
                                                     return Ok(accumulated_turn_count);
                                                 }
                                             }
                                         }
                                     }
                                 } else {
                                 tracing::debug!(
                                     turn = accumulated_turn_count,
                                     max_turns = params.max_turns,
                                     "Agent turn completed"
                                 );
                             }

                             if accumulated_turn_count >= params.max_turns {
                                return Ok(accumulated_turn_count);
                            }
                        }
                        Some(Ok(_)) => {
                            // Other event types (e.g., tool calls) — skip for now
                        }
                        Some(Err(e)) => {
                            if is_fatal_error(&e) {
                                return Err(e).context("Fatal error during agent stream");
                            }
                            tracing::error!("Agent stream error: {e}");

                            if retries >= params.max_retries {
                                return Err(anyhow::anyhow!("Stream failed after {} retries: {}", retries, e));
                            }

                            let follow_up_text = match last_assistant_text.as_deref() {
                                Some(text) if !text.trim().is_empty() => format!(
                                    "The connection was interrupted due to an error: {e}. Continue from where you left off. If you are done reviewing, call `submit_finding` for each candidate or `submit_no_findings` if there are no candidates. Do not stop without using a reporting tool. The host will write the final report to {report_path}. Your last visible message was:

{text}"
                                ),
                                _ => format!("The connection was interrupted due to an error: {e}. Continue the review.")
                            };

                            tracing::info!(
                                turns = accumulated_turn_count,
                                attempt = retries + 1,
                                max_retries = params.max_retries,
                                "Stream interrupted; retrying with a follow-up prompt"
                            );

                            let follow_up_message = Message::user().with_text(&follow_up_text);

                            retries += 1;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            delay *= 2;

                            let mut s_opt = None;
                            let mut last_err = None;
                            while retries <= params.max_retries {
                                match agent.reply(follow_up_message.clone(), session_config.clone(), None).await {
                                    Ok(s) => {
                                        s_opt = Some(s);
                                        break;
                                    }
                                    Err(start_err) => {
                                        if is_fatal_error(&start_err) {
                                            return Err(start_err).context("Fatal error restarting stream");
                                        }
                                        tracing::error!("Failed to restart stream after interruption: {}, retrying...", start_err);
                                        last_err = Some(start_err);
                                        retries += 1;
                                        tokio::time::sleep(Duration::from_secs(delay)).await;
                                        delay *= 2;
                                    }
                                }
                            }
                            match s_opt {
                                Some(s) => { stream = s; continue; },
                                None => {
                                    if let Some(err) = last_err {
                                        return Err(anyhow::anyhow!("Reached max retries while trying to restart stream. Last error: {}", err));
                                    } else {
                                        return Err(anyhow::anyhow!("Reached max retries while trying to restart stream"));
                                    }
                                }
                            }
                        }
                        None => {
                            if reporting_artifact.lock().await.finder_complete() {
                                return Ok(accumulated_turn_count); // Stream finished successfully
                            }

                            if accumulated_turn_count >= params.max_turns || (budget_exceeded && retries > 0) {
                                return Ok(accumulated_turn_count);
                            }

                            if retries >= params.max_retries {
                                tracing::warn!(
                                    turns = accumulated_turn_count,
                                    "Agent stopped prematurely and reached retry limit without writing report"
                                );
                                return Ok(accumulated_turn_count);
                            }

                            let follow_up_text = if budget_exceeded {
                                "BUDGET EXCEEDED! Stop analyzing and submit your current result now. Use `submit_finding` for each candidate, or `submit_no_findings` if there are no candidates. Do not do anything else.".to_string()
                            } else {
                                match last_assistant_text.as_deref() {
                                    Some(text) if !text.trim().is_empty() => format!(
                                        "You stopped before submitting a structured review result. Continue from where you left off. If you are done reviewing, call `submit_finding` for each candidate or `submit_no_findings` if there are no candidates. Do not stop without using a reporting tool. Your last visible message was:

{text}"
                                    ),
                                    _ => "You stopped before submitting a structured review result. Continue the review and call `submit_finding` for each candidate or `submit_no_findings` if there are no candidates. Do not stop without using a reporting tool.".to_string(),
                                }
                            };

                            tracing::info!(
                                turns = accumulated_turn_count,
                                attempt = retries + 1,
                                max_retries = params.max_retries,
                                "Agent stream ended before structured findings were submitted; retrying with a follow-up prompt"
                            );

                            let follow_up_message = Message::user().with_text(&follow_up_text);

                            retries += 1;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            delay *= 2;

                            let mut s_opt = None;
                            let mut last_err = None;
                            while retries <= params.max_retries {
                                match agent.reply(follow_up_message.clone(), session_config.clone(), None).await {
                                    Ok(s) => {
                                        s_opt = Some(s);
                                        break;
                                    }
                                    Err(start_err) => {
                                        if is_fatal_error(&start_err) {
                                            return Err(start_err).context("Fatal error after premature stop");
                                        }
                                        tracing::error!("Failed to restart agent after premature stop: {}, retrying...", start_err);
                                        last_err = Some(start_err);
                                        retries += 1;
                                        tokio::time::sleep(Duration::from_secs(delay)).await;
                                        delay *= 2;
                                    }
                                }
                            }
                            match s_opt {
                                Some(s) => { stream = s; continue; },
                                None => {
                                    if let Some(err) = last_err {
                                        return Err(anyhow::anyhow!("Reached max retries while trying to restart stream. Last error: {}", err));
                                    } else {
                                        return Err(anyhow::anyhow!("Reached max retries while trying to restart stream"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    let mut turn_count =
        match timeout(Duration::from_secs(params.timeout_mins * 60), review_future).await {
            Ok(result) => result?,
            Err(_) => {
                tracing::warn!(
                    timeout_mins = params.timeout_mins,
                    "Review session timed out"
                );
                bail!(
                    "Review session timed out after {} minutes",
                    params.timeout_mins
                );
            }
        };

    // 9. Collect final session metrics before cleaning up
    if let Ok(session) = agent
        .config
        .session_manager
        .get_session(&session_config.id, false)
        .await
    {
        let input = session.accumulated_input_tokens.unwrap_or(0).max(0) as u64;
        let output = session.accumulated_output_tokens.unwrap_or(0).max(0) as u64;

        // For a growing conversation, peak ≈ (2 * sum) / (count + 1)
        peak_input_tokens = peak_input_tokens.max((2 * input) / (turn_count as u64 + 1));
        total_output_tokens += output;
        total_processed_tokens += input + output;
    }

    let duration_secs = start_time.elapsed().as_secs();
    // 10. Build structured artifact, verify candidates, and render Markdown.
    let report_file = std::path::Path::new(&report_path);
    let mut artifact = reporting_artifact.lock().await.clone();

    if !artifact.finder_complete() && report_file.exists() {
        tracing::warn!(
            path = %report_path,
            "Agent produced Markdown without structured findings; allowing local artifact only"
        );
        artifact.markdown_only_fallback = true;
    }

    if artifact.finder_complete() || artifact.markdown_only_fallback {
        if params.verify_findings && !artifact.findings.is_empty() {
            tracing::info!(
                findings = artifact.findings.len(),
                "Running structured verifier pass before disclosure"
            );

            let verifier_result = run_verification_pass(VerificationParams {
                artifact: reporting_artifact.clone(),
                workspace_path: &workspace.path,
                repo: &params.repo,
                pr_number: params.pr_number,
                pr_context: &workspace.pr_context,
                diff_base: &diff_base,
                provider_name: params
                    .verifier_provider
                    .as_deref()
                    .unwrap_or(&params.provider),
                model: params.verifier_model.as_deref().unwrap_or(&params.model),
                max_retries: params.max_retries,
                retry_delay_secs: params.retry_delay_secs,
                timeout_mins: params.timeout_mins,
                max_turns: params.max_turns,
                cancel_token: cancel_token.clone(),
            })
            .await?;
            turn_count += verifier_result.turns;
            peak_input_tokens = peak_input_tokens.max(verifier_result.peak_input_tokens);
            total_output_tokens += verifier_result.output_tokens;
            total_processed_tokens += verifier_result.total_tokens;
            artifact = reporting_artifact.lock().await.clone();

            if !artifact.verifier_complete() {
                tracing::warn!(
                    "Verifier did not submit verdicts for all findings; disclosure disabled"
                );
                artifact.verifier_failed = true;
            }
        } else if !artifact.findings.is_empty() {
            tracing::warn!("Verifier pass disabled; structured findings will not be disclosed");
            artifact.verifier_failed = true;
        }

        let total_tokens = total_processed_tokens;
        let cost_usd = estimate_cost(
            &params.provider,
            &params.model,
            peak_input_tokens,
            total_output_tokens,
            params.input_price_per_m,
            params.output_price_per_m,
        );

        reporting::validate_artifact(&mut artifact)?;
        *reporting_artifact.lock().await = artifact.clone();

        let diff_content = std::fs::read_to_string(workspace.path.join(".pr_diff.txt"))
            .unwrap_or_else(|_| String::new());
        let policy = reporting::DisclosurePolicy {
            pr_context: workspace.pr_context.clone(),
            diff_anchors: reporting::parse_diff_anchors(&diff_content),
        };

        let report_content =
            reporting::render_markdown(&params.repo, params.pr_number, &artifact, Some(&policy));
        fs::write(report_file, report_content).context("Failed to write rendered report")?;

        let structured_path = structured_artifact_path(report_file);
        fs::write(&structured_path, serde_json::to_vec_pretty(&artifact)?)
            .context("Failed to write structured review artifact")?;
        let policy_path = disclosure_policy_path(report_file);
        fs::write(&policy_path, serde_json::to_vec_pretty(&policy)?)
            .context("Failed to write disclosure policy artifact")?;

        let limit_reached = turn_count >= params.max_turns;
        let accepted_findings = if artifact.markdown_only_fallback {
            Vec::new()
        } else {
            artifact.accepted_findings(&policy)
        };
        let findings_count = accepted_findings.len() as u32;
        let should_notify = findings_count > 0 && !artifact.verifier_failed;
        let status = if artifact.markdown_only_fallback {
            "markdown-only".to_string()
        } else if artifact.verifier_failed {
            "unverified".to_string()
        } else if findings_count > 0 {
            "confirmed".to_string()
        } else if artifact.no_findings.is_some() {
            "none".to_string()
        } else {
            "rejected".to_string()
        };
        let severity = accepted_findings
            .iter()
            .map(|finding| finding.severity.clone())
            .next()
            .unwrap_or_else(|| "none".to_string());
        let pr_classification = if findings_count > 0 {
            params.pr_number.to_string()
        } else {
            "none".to_string()
        };

        tracing::info!(
            path = %report_path,
            structured = %structured_path.display(),
            policy = %policy_path.display(),
            turns = turn_count,
            limit_reached = limit_reached,
            should_notify = should_notify,
            findings_count = findings_count,
            status = %status,
            severity = %severity,
            pr = %pr_classification,
            duration = %format!("{}s", duration_secs),
            tokens = %format!("in:{} peak:{} out:{} total:{}", total_processed_tokens, peak_input_tokens, total_output_tokens, total_tokens),
            cost = %cost_usd.map(|c| format!("${:.4}", c)).unwrap_or_else(|| "unknown".to_string()),
            "Review complete"
        );

        if !should_notify {
            tracing::info!(
                "No actionable findings that require notification were found in this PR"
            );
        }

        let mut completed = CompletedReview {
            metadata: state::ReviewMetadata {
                review_kind: params.review_kind.clone(),
                commit_hash: workspace.commit_hash.clone(),
                model: params.model.clone(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                findings_count,
                status,
                severity,
                pr_classification,
                duration_secs,
                input_tokens: peak_input_tokens,
                output_tokens: total_output_tokens,
                total_tokens: total_processed_tokens,
                cost_usd,
                report_url: None,
                is_rereview: params.is_rereview,
                time_reviewed: Some(
                    time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                ),
                retry_count: state::get_pr_review(
                    &params.db_path,
                    &params.repo,
                    params.pr_number,
                    &params.review_kind,
                )
                .ok()
                .flatten()
                .map(|m| m.retry_count)
                .unwrap_or(0),
            },
            should_notify,
            report_path: report_file.to_path_buf(),
        };

        if params.execution.persist_side_effects {
            let report_url = if artifact.markdown_only_fallback {
                crate::disclose::handle_disclosure(
                    report_file,
                    &params.repo,
                    params.pr_number,
                    workspace.commit_hash.as_str(),
                    false,
                    &params.disclose_config,
                )
                .await?
            } else {
                crate::disclose::handle_structured_disclosure(
                    report_file,
                    &params.repo,
                    params.pr_number,
                    workspace.commit_hash.as_str(),
                    &artifact,
                    &policy,
                    &params.disclose_config,
                )
                .await?
            };
            completed.metadata.report_url = report_url;

            if completed.metadata.status == "none"
                && let Some(reaction) = params.disclose_config.reactions.no_findings.as_deref()
                && let Err(error) =
                    disclose::post_pr_reaction(&params.repo, params.pr_number, reaction).await
            {
                tracing::warn!(
                    repo = %params.repo,
                    pr = params.pr_number,
                    error = %error,
                    "Failed to post no-findings reaction"
                );
            }

            if let Err(e) = state::mark_reviewed(
                &params.db_path,
                &params.repo,
                params.pr_number,
                &completed.metadata,
            ) {
                tracing::warn!("Failed to record review state in database: {}", e);
            }
        }

        if let Some(result_json) = &params.execution.result_json {
            if let Some(parent) = result_json.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "Failed to create review result directory at {}",
                        parent.display()
                    )
                })?;
            }
            fs::write(result_json, serde_json::to_vec_pretty(&completed)?)
                .context("Failed to write sandbox review result")?;
        }

        workspace.cleanup().await?;

        if cancel_token.is_cancelled() {
            bail!("Review cancelled");
        }

        return Ok(Some(completed));
    } else {
        let limit_reached = turn_count >= params.max_turns;
        if limit_reached {
            tracing::warn!(
                turns = turn_count,
                max_turns = params.max_turns,
                "Agent reached the turn limit without submitting structured findings"
            );
        } else {
            tracing::warn!(
                turns = turn_count,
                "Agent finished without submitting structured findings"
            );
        }
    }

    // 11. Clean up workspace
    workspace.cleanup().await?;

    if cancel_token.is_cancelled() {
        bail!("Review cancelled");
    }

    Ok(None)
}

#[cfg(test)]
fn report_would_notify(content: &str) -> bool {
    let notify = parse_frontmatter_field(content, "notify")
        .unwrap_or_else(|| "false".to_string())
        .eq_ignore_ascii_case("true");
    let findings_count = parse_frontmatter_field(content, "findings_count")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let status = parse_frontmatter_field(content, "status").unwrap_or_default();

    notify || (findings_count > 0 && status != "none")
}

async fn add_reporting_extension(agent: &Agent, session_id: &str) -> Result<()> {
    let reporting_ext = ExtensionConfig::Frontend {
        name: "fiach-reporting".to_string(),
        description: "Structured Fiach review reporting tools".to_string(),
        tools: reporting::reporting_tools(),
        instructions: Some(
            "Use these tools to submit structured review results to Fiach. Finder passes call `submit_finding` once per candidate or `submit_no_findings`. Verifier passes call `submit_verdict` once per candidate finding. These tools do not post to GitHub."
                .to_string(),
        ),
        bundled: Some(true),
        available_tools: Vec::new(),
    };

    agent
        .add_extension(reporting_ext, session_id)
        .await
        .context("Failed to add frontend reporting extension")?;
    Ok(())
}

async fn handle_reporting_tool_requests(
    agent: &Agent,
    message: &Message,
    phase: ReviewPhase,
    artifact: SharedReportingArtifact,
) -> Result<()> {
    for content in &message.content {
        let MessageContent::FrontendToolRequest(request) = content else {
            continue;
        };
        let Ok(tool_call) = &request.tool_call else {
            continue;
        };

        let result = match tool_call.name.as_ref() {
            "submit_finding" if phase == ReviewPhase::Finder => {
                match parse_tool_arguments::<reporting::FindingInput>(tool_call.arguments.clone()) {
                    Ok(input) => {
                        let mut guard = artifact.lock().await;
                        if guard.no_findings.is_some() {
                            CallToolResult::error(vec![Content::text(
                                "cannot submit findings after submit_no_findings",
                            )])
                        } else {
                            match reporting::Finding::from_input(guard.findings.len(), input) {
                                Ok(finding) => {
                                    let id = finding.id.clone();
                                    guard.findings.push(finding);
                                    CallToolResult::success(vec![Content::text(format!(
                                        "accepted finding {id}"
                                    ))])
                                }
                                Err(error) => {
                                    CallToolResult::error(vec![Content::text(error.to_string())])
                                }
                            }
                        }
                    }
                    Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
                }
            }
            "submit_no_findings" if phase == ReviewPhase::Finder => {
                match parse_tool_arguments::<reporting::NoFindings>(tool_call.arguments.clone()) {
                    Ok(mut no_findings) => {
                        if let Err(error) = no_findings.validate() {
                            CallToolResult::error(vec![Content::text(error.to_string())])
                        } else {
                            let mut guard = artifact.lock().await;
                            if !guard.findings.is_empty() {
                                CallToolResult::error(vec![Content::text(
                                    "cannot submit no-findings after submit_finding",
                                )])
                            } else {
                                guard.no_findings = Some(no_findings);
                                CallToolResult::success(vec![Content::text(
                                    "accepted no-findings result",
                                )])
                            }
                        }
                    }
                    Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
                }
            }
            "submit_verdict" if phase == ReviewPhase::Verifier => {
                match parse_tool_arguments::<reporting::Verdict>(tool_call.arguments.clone()) {
                    Ok(mut verdict) => {
                        let mut guard = artifact.lock().await;
                        let ids = guard
                            .findings
                            .iter()
                            .map(|finding| finding.id.clone())
                            .collect();
                        match verdict.validate(&ids) {
                            Ok(()) => {
                                guard
                                    .verdicts
                                    .retain(|existing| existing.finding_id != verdict.finding_id);
                                let id = verdict.finding_id.clone();
                                guard.verdicts.push(verdict);
                                CallToolResult::success(vec![Content::text(format!(
                                    "accepted verdict for {id}"
                                ))])
                            }
                            Err(error) => {
                                CallToolResult::error(vec![Content::text(error.to_string())])
                            }
                        }
                    }
                    Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
                }
            }
            "submit_finding" | "submit_no_findings" | "submit_verdict" => {
                CallToolResult::error(vec![Content::text(format!(
                    "tool `{}` is not valid during the {:?} phase",
                    tool_call.name, phase
                ))])
            }
            _ => continue,
        };

        agent
            .handle_tool_result(request.id.clone(), Ok(result))
            .await;
    }

    Ok(())
}

fn parse_tool_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<rmcp::model::JsonObject>,
) -> Result<T> {
    let value = arguments
        .map(serde_json::Value::Object)
        .context("missing tool arguments")?;
    serde_json::from_value(value).context("invalid tool arguments")
}

fn structured_artifact_path(report_file: &std::path::Path) -> PathBuf {
    let mut path = report_file.to_path_buf();
    path.set_extension("structured.json");
    path
}

pub fn structured_artifact_path_for_report(report_file: &std::path::Path) -> PathBuf {
    structured_artifact_path(report_file)
}

fn disclosure_policy_path(report_file: &std::path::Path) -> PathBuf {
    let mut path = report_file.to_path_buf();
    path.set_extension("policy.json");
    path
}

pub fn disclosure_policy_path_for_report(report_file: &std::path::Path) -> PathBuf {
    disclosure_policy_path(report_file)
}

#[derive(Debug, Default)]
struct VerificationStats {
    turns: u32,
    peak_input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

struct VerificationParams<'a> {
    artifact: SharedReportingArtifact,
    workspace_path: &'a std::path::Path,
    repo: &'a str,
    pr_number: u64,
    pr_context: &'a reporting::PrContext,
    diff_base: &'a str,
    provider_name: &'a str,
    model: &'a str,
    max_retries: u32,
    retry_delay_secs: u64,
    timeout_mins: u64,
    max_turns: u32,
    cancel_token: CancellationToken,
}

async fn run_verification_pass(params: VerificationParams<'_>) -> Result<VerificationStats> {
    let provider = create_with_named_model(params.provider_name, params.model, Vec::new())
        .await
        .with_context(|| {
            format!(
                "Failed to create verifier {} provider",
                params.provider_name
            )
        })?;
    let agent = Agent::new();
    let session = agent
        .config
        .session_manager
        .create_session(
            params.workspace_path.to_path_buf(),
            "review-verifier".to_string(),
            SessionType::Hidden,
            GooseMode::Auto,
        )
        .await
        .context("Failed to create verifier session")?;
    agent
        .update_provider(provider, &session.id)
        .await
        .context("Failed to update verifier provider")?;

    let developer_ext = ExtensionConfig::Platform {
        name: "developer".to_string(),
        description: "Write and edit files, and execute shell commands".to_string(),
        display_name: Some("Developer".to_string()),
        bundled: None,
        available_tools: Vec::new(),
    };
    agent
        .add_extension(developer_ext, &session.id)
        .await
        .context("Failed to load developer extension for verifier")?;
    add_reporting_extension(&agent, &session.id).await?;

    let candidates = serde_json::to_string_pretty(&params.artifact.lock().await.findings)?;
    let pr_context = serde_json::to_string_pretty(params.pr_context)?;
    let verifier_prompt = format!(
        "VERIFIER PHASE for PR #{pr_number} in {repo}.\n\
         Candidate findings are below as JSON. Review all candidates in one pass and call `submit_verdict` exactly once for each finding_id.\n\
         Candidate findings:\n{candidates}\n\n\
         PR context:\n{pr_context}\n\n\
         The current diff base for shell checks is `{diff_base}`. The complete patch is in `.pr_diff.txt`.\n\
         You may run bounded reproduction commands through the developer shell when needed. For every finding that you mark as discloseable, include command transcript evidence comparing the PR branch against base and/or default branch context.\n\
         Mark `confirmed` true only for a real issue, not a subjective nit. Mark `introduced_by_pr` true only when the PR diff introduced the root cause. Use `disclosure_decision: \"disclose\"` only when the finding is confirmed, PR-introduced, actionable for PR review, and supported by command transcript evidence.\n\
         Do not post comments, create PRs, or modify files. Only submit structured verdicts.",
        pr_number = params.pr_number,
        repo = params.repo,
        diff_base = params.diff_base,
    );

    agent
        .extend_system_prompt(
            "verifier_policy".to_string(),
            "You are a strict adjudicator. Suppress uncertain, theoretical, pre-existing, or unverified findings. Models never disclose directly; only the host may disclose after policy checks.".to_string(),
        )
        .await;

    let session_config = SessionConfig {
        id: session.id,
        schedule_id: None,
        max_turns: Some(params.max_turns),
        retry_config: None,
    };

    let verifier_future = async {
        let mut retries = 0;
        let mut delay = params.retry_delay_secs;
        let mut turns = 0;
        let mut stream = loop {
            match agent
                .reply(
                    Message::user().with_text(&verifier_prompt),
                    session_config.clone(),
                    None,
                )
                .await
            {
                Ok(stream) => break stream,
                Err(e) => {
                    if is_fatal_error(&e) {
                        return Err(e).context("Fatal provider error during verifier pass");
                    }
                    if retries >= params.max_retries {
                        return Err(anyhow::anyhow!(
                            "Failed to start verifier pass after {} retries: {}",
                            retries,
                            e
                        ));
                    }
                    tracing::info!(
                        "Failed to start verifier pass (attempt {}/{}): {}. Retrying in {}s...",
                        retries + 1,
                        params.max_retries,
                        e,
                        delay
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    retries += 1;
                    delay *= 2;
                }
            }
        };

        loop {
            tokio::select! {
                _ = params.cancel_token.cancelled() => {
                    bail!("Verifier pass cancelled by user");
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(AgentEvent::Message(message))) => {
                            handle_reporting_tool_requests(
                                &agent,
                                &message,
                                ReviewPhase::Verifier,
                                params.artifact.clone(),
                            )
                            .await?;

                            if message.role == Role::Assistant {
                                turns += 1;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            if is_fatal_error(&e) {
                                return Err(e).context("Fatal provider error during verifier stream");
                            }
                            if retries >= params.max_retries {
                                return Err(anyhow::anyhow!(
                                    "Verifier stream failed after {} retries: {}",
                                    retries,
                                    e
                                ));
                            }
                            let retry_prompt = format!(
                                "The verifier stream was interrupted due to this error: {e}. Continue verification and call `submit_verdict` exactly once for every candidate before stopping."
                            );
                            retries += 1;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            delay *= 2;
                            stream = agent
                                .reply(
                                    Message::user().with_text(&retry_prompt),
                                    session_config.clone(),
                                    None,
                                )
                                .await
                                .context("Failed to restart verifier stream")?;
                        }
                        None => {
                            if params.artifact.lock().await.verifier_complete() {
                                return Ok(turns);
                            }
                            if retries >= params.max_retries {
                                return Ok(turns);
                            }
                            let retry_prompt = "You stopped before submitting verdicts for all candidate findings. Continue verification and call `submit_verdict` exactly once for every remaining candidate.".to_string();
                            retries += 1;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            delay *= 2;
                            stream = agent
                                .reply(
                                    Message::user().with_text(&retry_prompt),
                                    session_config.clone(),
                                    None,
                                )
                                .await
                                .context("Failed to restart verifier stream")?;
                        }
                    }
                }
            }
        }
    };

    let turns = timeout(
        Duration::from_secs(params.timeout_mins * 60),
        verifier_future,
    )
    .await
    .context("Verifier pass timed out")??;

    let mut stats = VerificationStats {
        turns,
        ..Default::default()
    };
    if let Ok(session) = agent
        .config
        .session_manager
        .get_session(&session_config.id, false)
        .await
    {
        let input = session.accumulated_input_tokens.unwrap_or(0).max(0) as u64;
        let output = session.accumulated_output_tokens.unwrap_or(0).max(0) as u64;
        stats.peak_input_tokens = if turns == 0 {
            input
        } else {
            (2 * input) / (turns as u64 + 1)
        };
        stats.output_tokens = output;
        stats.total_tokens = input + output;
    }

    Ok(stats)
}

/// Extract a value from the YAML frontmatter of a report.
///
/// Looks for lines between `---` delimiters matching `key: value`.
/// Returns the trimmed value string, or `None` if the key is not found.
#[cfg(test)]
fn parse_frontmatter_field(content: &str, key: &str) -> Option<String> {
    let mut in_frontmatter = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if in_frontmatter {
                // End of frontmatter — stop searching
                return None;
            }
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter
            && let Some(rest) = trimmed.strip_prefix(key)
            && let Some(value) = rest.strip_prefix(':')
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Estimate the cost in USD based on token usage and model pricing.
fn estimate_cost(
    provider: &str,
    model_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    input_override: Option<f64>,
    output_override: Option<f64>,
) -> Option<f64> {
    // OpenRouter model IDs are often in the format "upstream-provider/model".
    let (canonical_provider, model) = if provider == "openrouter" {
        if let Some((p, m)) = model_id.split_once('/') {
            (p, m)
        } else {
            (provider, model_id)
        }
    } else {
        (provider, model_id)
    };

    let (input_cost_per_m, output_cost_per_m) =
        if let (Some(i), Some(o)) = (input_override, output_override) {
            (i, o)
        } else {
            let canonical = maybe_get_canonical_model(canonical_provider, model)?;
            (
                input_override.unwrap_or(canonical.cost.input?),
                output_override.unwrap_or(canonical.cost.output?),
            )
        };

    let input_cost = (input_tokens as f64 / 1_000_000.0) * input_cost_per_m;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_cost_per_m;

    Some(input_cost + output_cost)
}

/// Returns true if the error is a non-transient failure that should not be retried.
pub fn is_fatal_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("credits exhausted")
        || msg.contains("payment required")
        || msg.contains("402")
        || msg.contains("insufficient credits")
        || msg.contains("limit exceeded")
        || msg.contains("quota exceeded")
        || msg.contains("unauthorized")
        || msg.contains("401")
        || msg.contains("forbidden")
        || msg.contains("403")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_REPORT_WITH_FINDINGS: &str = r#"---
title: "Token bypass via unchecked signature"
status: confirmed
severity: high
target: owner/repo
pr: 1835
skills_used: ["rust-security"]
findings_count: 1
---

## Summary
A bypass was found.
"#;

    const SAMPLE_REPORT_NO_FINDINGS: &str = r#"---
title: "No vulnerabilities found"
status: none
severity: none
target: owner/repo
pr: none
skills_used: ["none"]
findings_count: 0
---

## Summary
Reviewed the PR and found no vulnerabilities.
"#;

    #[test]
    fn test_parse_frontmatter_status_confirmed() {
        let status = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "status");
        assert_eq!(status.as_deref(), Some("confirmed"));
    }

    #[test]
    fn test_parse_frontmatter_severity() {
        let severity = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "severity");
        assert_eq!(severity.as_deref(), Some("high"));
    }

    #[test]
    fn test_parse_frontmatter_skills_used() {
        let skills = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "skills_used");
        assert_eq!(skills.as_deref(), Some(r#"["rust-security"]"#));
    }

    #[test]
    fn test_parse_frontmatter_pr() {
        let pr = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "pr");
        assert_eq!(pr.as_deref(), Some("1835"));
        let pr_none = parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "pr");
        assert_eq!(pr_none.as_deref(), Some("none"));
    }

    #[test]
    fn test_parse_frontmatter_findings_count() {
        let count = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "findings_count");
        assert_eq!(count.as_deref(), Some("1"));
    }

    #[test]
    fn test_parse_frontmatter_no_findings() {
        let status = parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "status");
        assert_eq!(status.as_deref(), Some("none"));

        let count = parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "findings_count");
        assert_eq!(count.as_deref(), Some("0"));
    }

    #[test]
    fn test_parse_frontmatter_missing_key() {
        let result = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just a plain markdown file.\n\nNo frontmatter here.";
        let result = parse_frontmatter_field(content, "status");
        assert!(result.is_none());
    }

    #[test]
    fn test_should_notify_logic() {
        // With findings
        let notify_str = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "notify")
            .unwrap_or_else(|| "false".to_string());
        let count: u32 = parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "findings_count")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let status =
            parse_frontmatter_field(SAMPLE_REPORT_WITH_FINDINGS, "status").unwrap_or_default();
        assert!(notify_str.to_lowercase() == "true" || (count > 0 && status != "none"));

        // Without findings
        let notify_str = parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "notify")
            .unwrap_or_else(|| "false".to_string());
        let count: u32 = parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "findings_count")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let status =
            parse_frontmatter_field(SAMPLE_REPORT_NO_FINDINGS, "status").unwrap_or_default();
        assert!(!(notify_str.to_lowercase() == "true" || (count > 0 && status != "none")));
    }

    #[test]
    fn test_report_would_notify_for_findings() {
        assert!(report_would_notify(SAMPLE_REPORT_WITH_FINDINGS));
    }

    #[test]
    fn test_report_would_notify_for_explicit_notify() {
        let report = r#"---
title: "No vulnerabilities found"
notify: true
status: none
severity: none
target: owner/repo
pr: none
skills_used: ["none"]
findings_count: 0
---

## Summary
Reviewed the PR and found no vulnerabilities.
"#;

        assert!(report_would_notify(report));
    }

    #[test]
    fn test_report_would_notify_false_for_empty_report() {
        assert!(!report_would_notify(SAMPLE_REPORT_NO_FINDINGS));
    }

    #[test]
    fn test_report_path_resolution() {
        // We'll test the logic here by mimicking it
        let current_dir = std::env::current_dir().unwrap();
        let pr_number = 123;
        let commit_hash = "abcdef1234567890";

        // Case 1: No output path provided (should use reports/ in current_dir)
        let output: Option<PathBuf> = None;
        let report_path = match output {
            Some(path) => {
                if path.is_absolute() {
                    path.to_str().unwrap().to_string()
                } else {
                    current_dir.join(path).to_str().unwrap().to_string()
                }
            }
            None => {
                let hash = &commit_hash[..commit_hash.len().min(7)];
                current_dir
                    .join("reports")
                    .join(format!("PR{}_{}.md", pr_number, hash))
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };
        assert_eq!(
            report_path,
            current_dir
                .join("reports")
                .join("PR123_abcdef1.md")
                .to_str()
                .unwrap()
                .to_string()
        );

        // Case 2: Relative output path provided (should use current_dir)
        let output: Option<PathBuf> = Some(PathBuf::from("my_report.md"));
        let report_path = match output {
            Some(path) => {
                if path.is_absolute() {
                    path.to_str().unwrap().to_string()
                } else {
                    current_dir.join(path).to_str().unwrap().to_string()
                }
            }
            None => {
                let hash = &commit_hash[..commit_hash.len().min(7)];
                current_dir
                    .join("reports")
                    .join(format!("PR{}_{}.md", pr_number, hash))
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };
        assert_eq!(
            report_path,
            current_dir
                .join("my_report.md")
                .to_str()
                .unwrap()
                .to_string()
        );

        // Case 3: Absolute output path provided (should use as-is)
        let absolute_path = current_dir.join("absolute_dir").join("my_report.md");
        let output: Option<PathBuf> = Some(absolute_path.clone());
        let report_path = match output {
            Some(path) => {
                if path.is_absolute() {
                    path.to_str().unwrap().to_string()
                } else {
                    current_dir.join(path).to_str().unwrap().to_string()
                }
            }
            None => {
                let hash = &commit_hash[..commit_hash.len().min(7)];
                current_dir
                    .join("reports")
                    .join(format!("PR{}_{}.md", pr_number, hash))
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };
        assert_eq!(report_path, absolute_path.to_str().unwrap().to_string());
    }
}
