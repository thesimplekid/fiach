use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use goose::agents::{Agent, AgentEvent, ExtensionConfig, SessionConfig};
use goose::config::GooseMode;
use goose::conversation::message::Message;
use goose::providers::canonical::maybe_get_canonical_model;
use goose::providers::create_with_named_model;
use goose::session::session_manager::SessionType;
use rmcp::model::Role;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::disclose;
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
    /// OpenRouter model identifier (e.g., "anthropic/claude-sonnet-4")
    pub model: String,
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
    /// Force a review even if it was already done
    pub force: bool,
    /// Maximum number of retries for LLM provider failures
    pub max_retries: u32,
    /// Initial delay in seconds before retrying an LLM failure
    pub retry_delay_secs: u64,
    /// Configuration for disclosing the report
    pub disclose_config: disclose::DiscloseConfig,
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

/// Run a review of a GitHub PR using the goose agent.
///
/// This function:
/// 1. Prepares a workspace (clone repo, checkout PR) — no agent turns wasted on setup
/// 2. Creates an OpenRouter LLM provider
/// 3. Initializes a goose Agent with a hidden session rooted in the workspace
/// 4. Appends the CTF security persona to the system prompt (extras mode)
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
            reports_dir
                .join(format!("PR{}_{}.md", params.pr_number, hash))
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

    // 2. Create the OpenRouter provider
    let provider = create_with_named_model("openrouter", &params.model, Vec::new())
        .await
        .context("Failed to create OpenRouter provider")?;

    // 3. Create the agent and a hidden session rooted in the workspace
    let agent = Agent::new();

    let session = agent
        .config
        .session_manager
        .create_session(
            workspace.path.clone(),
            "security-review".to_string(),
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
             `skills_used` frontmatter field of your report."
        ),
        None => "No domain skill was specified for this review. Record `skills_used: [\"none\"]` \
                 in your report frontmatter unless you independently loaded a skill."
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
                if let Ok(Some(metadata)) =
                    state::get_commit_review(&params.db_path, &params.repo, commit)
                {
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
    let user_message_text = match &params.skill {
        Some(skill_name) => format!(
            "Review PR #{pr_number} in {repo} for security vulnerabilities introduced by this PR. \
             The current working directory is a clone of the repository with the PR branch already checked out. \
             Focus ONLY on the changes in this PR. Do NOT run tests, builds, compilers, interpreters, scratch programs, or ad hoc reproduction code. \
             Analyze the diff using `git diff {diff_base}...HEAD --name-only` and `BASE_BRANCH={diff_base} ./safe_diff.sh <single_file_path>`, \
             follow the CTF methodology in your instructions, \
             and write findings to {report_path}.{prev_review_context}\n\n\
             IMPORTANT: Use the `{skill_name}` skill to complete this review. Use the load tool to load it if you haven't already.",
            pr_number = params.pr_number,
            repo = params.repo,
            diff_base = diff_base,
            report_path = report_path,
            prev_review_context = prev_review_context,
            skill_name = skill_name,
        ),
        None => format!(
            "Review PR #{pr_number} in {repo} for security vulnerabilities introduced by this PR. \
             The current working directory is a clone of the repository with the PR branch already checked out. \
             Focus ONLY on the changes in this PR. Do NOT run tests, builds, compilers, interpreters, scratch programs, or ad hoc reproduction code. \
             Analyze the diff using `git diff {diff_base}...HEAD --name-only` and `BASE_BRANCH={diff_base} ./safe_diff.sh <single_file_path>`, \
             follow the CTF methodology in your instructions, \
             and write findings to {report_path}.{prev_review_context}",
            pr_number = params.pr_number,
            repo = params.repo,
            diff_base = diff_base,
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

                                             let budget_nudge = format!("BUDGET EXCEEDED! Stop analyzing and write your final report to {} NOW. Use the `write` tool immediately. Do not do anything else.", report_path);
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
                                    "The connection was interrupted due to an error: {e}. Continue from where you left off. If you are done reviewing, use the `write` tool now to create the report file at {report_path}. Your last visible message was:

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
                            let report_file = std::path::Path::new(&report_path);
                            if report_file.exists() {
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
                                format!("BUDGET EXCEEDED! Stop analyzing and write your final report to {} NOW. Use the `write` tool immediately. Do not do anything else.", report_path)
                            } else {
                                match last_assistant_text.as_deref() {
                                    Some(text) if !text.trim().is_empty() => format!(
                                        "You stopped before writing the required report file at {report_path}.                                          Continue from where you left off. If you are done reviewing, use the `write` tool now to create the report file.                                          Do not stop without writing it. Your last visible message was:

{text}"
                                    ),
                                    _ => format!(
                                        "You stopped before writing the required report file at {report_path}.                                          Continue the review and use the `write` tool to create the report file now.                                          Do not stop without writing it."
                                    ),
                                }
                            };

                            tracing::info!(
                                turns = accumulated_turn_count,
                                attempt = retries + 1,
                                max_retries = params.max_retries,
                                path = %report_path,
                                "Agent stream ended before report was written; retrying with a follow-up prompt"
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
    let turn_count =
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
    let total_tokens = total_processed_tokens;
    let cost_usd = estimate_cost(
        &params.model,
        peak_input_tokens,
        total_output_tokens,
        params.input_price_per_m,
        params.output_price_per_m,
    );

    // 10. Check if the report was written and parse its frontmatter for summary
    let report_file = std::path::Path::new(&report_path);
    let limit_reached = turn_count >= params.max_turns;

    if report_file.exists() {
        let report_content = std::fs::read_to_string(report_file)
            .context("Report file exists but could not be read")?;

        let findings_count = parse_frontmatter_field(&report_content, "findings_count")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let status = parse_frontmatter_field(&report_content, "status")
            .unwrap_or_else(|| "unknown".to_string());
        let severity = parse_frontmatter_field(&report_content, "severity")
            .unwrap_or_else(|| "unknown".to_string());
        let notify_str = parse_frontmatter_field(&report_content, "notify")
            .unwrap_or_else(|| "false".to_string());
        let skills_used = parse_frontmatter_field(&report_content, "skills_used")
            .unwrap_or_else(|| "unknown".to_string());
        let pr_classification =
            parse_frontmatter_field(&report_content, "pr").unwrap_or_else(|| "unknown".to_string());

        let should_notify =
            notify_str.to_lowercase() == "true" || (findings_count > 0 && status != "none");

        tracing::info!(
            path = %report_path,
            turns = turn_count,
            limit_reached = limit_reached,
            should_notify = should_notify,
            findings_count = findings_count,
            status = %status,
            severity = %severity,
            pr = %pr_classification,
            skills_used = %skills_used,
            duration = %format!("{}s", duration_secs),
            tokens = %format!("in:{} peak:{} out:{} total:{}", total_processed_tokens, peak_input_tokens, total_output_tokens, total_tokens),
            cost = %cost_usd.map(|c| format!("${:.4}", c)).unwrap_or_else(|| "unknown".to_string()),
            "Review complete — report written"
        );

        if !should_notify {
            tracing::info!(
                "No actionable findings that require notification were found in this PR"
            );
        }

        if let Some(requested_skill) = &params.skill
            && !skills_used.contains(requested_skill)
        {
            tracing::warn!(
                requested = %requested_skill,
                reported = %skills_used,
                "Agent was instructed to use a skill, but the report frontmatter does not list it."
            );
        }

        let mut completed = CompletedReview {
            metadata: state::ReviewMetadata {
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
            },
            should_notify,
            report_path: report_file.to_path_buf(),
        };

        if params.execution.persist_side_effects {
            let report_url = crate::disclose::handle_disclosure(
                report_file,
                &params.repo,
                params.pr_number,
                workspace.commit_hash.as_str(),
                should_notify,
                &params.disclose_config,
            )
            .await?;
            completed.metadata.report_url = report_url;

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
        // The agent was explicitly instructed to always write a report, so a
        // missing report means something went wrong.
        if limit_reached {
            tracing::warn!(
                turns = turn_count,
                max_turns = params.max_turns,
                "Agent reached the turn limit WITHOUT writing a report"
            );
        } else {
            tracing::warn!(
                turns = turn_count,
                "Agent finished but FAILED to write a report — it was instructed to always produce one"
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

/// Extract a value from the YAML frontmatter of a report.
///
/// Looks for lines between `---` delimiters matching `key: value`.
/// Returns the trimmed value string, or `None` if the key is not found.
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
    model_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    input_override: Option<f64>,
    output_override: Option<f64>,
) -> Option<f64> {
    // OpenRouter model IDs are often in the format "provider/model"
    let (provider, model) = if let Some((p, m)) = model_id.split_once('/') {
        (p, m)
    } else {
        ("openrouter", model_id)
    };

    let (input_cost_per_m, output_cost_per_m) =
        if let (Some(i), Some(o)) = (input_override, output_override) {
            (i, o)
        } else {
            let canonical = maybe_get_canonical_model(provider, model)?;
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
