use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use buzz_client::{BuzzClient, BuzzClientConfig, BuzzIdentity, ClientError, RelayMessage};
use buzz_sdk::ThreadRef;
use nostr::{Alphabet, Event, EventBuilder, EventId, Filter, Kind, SingleLetterTag, Timestamp};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::BuzzConfig;
use crate::reporting::{AcceptedFinding, DisclosurePolicy, PullRequestSummary, ReportingArtifact};
use crate::state::{BuzzThreadState, ReviewRecord};

const DEFAULT_BUZZ_RELAY_URL: &str = "http://localhost:3000";
const MAX_MESSAGE_BYTES: usize = 60 * 1024;
const QUESTION_SUBSCRIPTION_ID: &str = "fiach-review-questions";
const QUESTION_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const QUESTION_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuzzThreadReceipt {
    pub channel_id: String,
    pub root_event_id: String,
    pub finding_event_ids: Vec<String>,
    pub published_finding_keys: Vec<String>,
}

pub async fn run_question_listener(
    config: BuzzConfig,
    db_path: &Path,
    default_provider: &str,
    default_model: &str,
    cancel: CancellationToken,
) -> Result<()> {
    let Some(question_config) = config.questions.as_ref().filter(|config| config.enabled) else {
        return Ok(());
    };
    validate_question_config(question_config)?;

    let client = build_client(&config)?;
    let bot_pubkey = client.public_key();
    let provider = question_config
        .provider
        .as_deref()
        .unwrap_or(default_provider)
        .to_string();
    let model = question_config
        .model
        .as_deref()
        .unwrap_or(default_model)
        .to_string();
    let _ = crate::state::get_or_initialize_buzz_question_cursor(
        db_path,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )?;

    tracing::info!(
        pubkey = %bot_pubkey.to_hex(),
        provider = %provider,
        model = %model,
        "Listening for Buzz review questions"
    );

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let cursor = crate::state::get_or_initialize_buzz_question_cursor(
            db_path,
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )?;
        let filters = question_filters(&config, bot_pubkey, cursor)?;
        let subscription = client.subscribe(QUESTION_SUBSCRIPTION_ID, &filters).await;
        let mut subscription = match subscription {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::warn!(error = %error, "Failed to subscribe to Buzz review questions");
                wait_to_reconnect(&cancel).await;
                continue;
            }
        };

        let reconnect = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = subscription.cancel().await;
                    return Ok(());
                }
                message = subscription.next_event(QUESTION_READ_TIMEOUT) => {
                    match message {
                        Ok(RelayMessage::Event { event, .. }) => {
                            if let Err(error) = handle_question_event(
                                &client,
                                &config,
                                question_config,
                                db_path,
                                &provider,
                                &model,
                                &bot_pubkey.to_hex(),
                                &event,
                            )
                            .await
                            {
                                tracing::warn!(
                                    event_id = %event.id.to_hex(),
                                    error = %error,
                                    "Failed to handle Buzz review question"
                                );
                                break true;
                            }
                        }
                        Ok(RelayMessage::Closed { message, .. }) => {
                            tracing::warn!(message = %message, "Buzz question subscription closed");
                            break true;
                        }
                        Ok(RelayMessage::Notice { message }) => {
                            tracing::warn!(message = %message, "Buzz question relay notice");
                        }
                        Ok(_) | Err(ClientError::Timeout) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "Buzz question subscription failed");
                            break true;
                        }
                    }
                }
            }
        };

        if reconnect {
            wait_to_reconnect(&cancel).await;
        }
    }
}

async fn wait_to_reconnect(cancel: &CancellationToken) {
    tokio::select! {
        _ = tokio::time::sleep(QUESTION_RECONNECT_DELAY) => {}
        _ = cancel.cancelled() => {}
    }
}

fn validate_question_config(config: &crate::config::BuzzQuestionsConfig) -> Result<()> {
    if config.max_question_bytes == 0 {
        bail!("Buzz questions max_question_bytes must be greater than zero");
    }
    if config.timeout_secs == 0 {
        bail!("Buzz questions timeout_secs must be greater than zero");
    }
    for pubkey in &config.allowed_pubkeys {
        let pubkey = pubkey.trim();
        if pubkey.len() != 64
            || !pubkey
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            bail!("Buzz questions allowed_pubkeys must contain 64-character hex public keys");
        }
    }
    Ok(())
}

fn question_filters(
    config: &BuzzConfig,
    bot_pubkey: nostr::PublicKey,
    cursor: i64,
) -> Result<Vec<Filter>> {
    // Relay `since` matching may be strict, so overlap one second and rely on
    // persisted event receipts to deduplicate same-timestamp replay.
    let since = Timestamp::from_secs(cursor.max(0).saturating_sub(1) as u64);
    let channels = configured_channels(config);
    if channels.is_empty() {
        bail!("Buzz questions require at least one configured channel");
    }
    channels
        .into_iter()
        .map(|channel| {
            Uuid::parse_str(channel).context("configured Buzz channel is not a UUID")?;
            Ok(Filter::new()
                .kind(Kind::Custom(9))
                .pubkey(bot_pubkey)
                .custom_tag(SingleLetterTag::lowercase(Alphabet::H), channel.to_string())
                .since(since))
        })
        .collect()
}

fn configured_channels(config: &BuzzConfig) -> Vec<&str> {
    let mut channels = Vec::new();
    for channel in [
        config.public_channel.as_deref(),
        config.security_channel.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let channel = channel.trim();
        if !channel.is_empty() && !channels.contains(&channel) {
            channels.push(channel);
        }
    }
    channels
}

#[allow(clippy::too_many_arguments)]
async fn handle_question_event(
    client: &BuzzClient,
    config: &BuzzConfig,
    question_config: &crate::config::BuzzQuestionsConfig,
    db_path: &Path,
    provider: &str,
    model: &str,
    bot_pubkey: &str,
    event: &Event,
) -> Result<()> {
    let event_id = event.id.to_hex();
    let timestamp = event.created_at.as_secs().min(i64::MAX as u64) as i64;
    if crate::state::buzz_question_processed(db_path, &event_id)? {
        return Ok(());
    }

    if event.verify().is_err()
        || event.pubkey.to_hex().eq_ignore_ascii_case(bot_pubkey)
        || !event_mentions(event, bot_pubkey)
    {
        return record_ignored_question(db_path, &event_id, timestamp);
    }

    let Some(channel_id) = event_channel(event) else {
        return record_ignored_question(db_path, &event_id, timestamp);
    };
    if !configured_channels(config).contains(&channel_id) {
        return record_ignored_question(db_path, &event_id, timestamp);
    }
    let Some(root_event_id) = event_thread_root(event) else {
        return record_ignored_question(db_path, &event_id, timestamp);
    };
    let Some(metadata) =
        crate::state::find_review_by_buzz_thread(db_path, channel_id, root_event_id)?
    else {
        return record_ignored_question(db_path, &event_id, timestamp);
    };

    let author = event.pubkey.to_hex();
    if !question_config.allowed_pubkeys.is_empty()
        && !question_config
            .allowed_pubkeys
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(&author))
    {
        tracing::warn!(
            author = %author,
            channel = %channel_id,
            "Ignoring Buzz review question from a non-allowlisted author"
        );
        return record_ignored_question(db_path, &event_id, timestamp);
    }

    let question = event.content.trim();
    let answer = if question.is_empty() {
        "Please include a question when you tag me in this review thread.".to_string()
    } else if question.len() > question_config.max_question_bytes {
        format!(
            "That question is too long. Please keep it under {} bytes.",
            question_config.max_question_bytes
        )
    } else {
        match tokio::time::timeout(
            Duration::from_secs(question_config.timeout_secs),
            crate::review::answer_buzz_review_question(provider, model, &metadata, question),
        )
        .await
        {
            Ok(Ok(answer)) => answer,
            Ok(Err(error)) => {
                tracing::warn!(
                    repo = %metadata.repository,
                    pr = metadata.pr_number,
                    review_kind = %metadata.review_kind,
                    error = %error,
                    "Buzz review question model request failed"
                );
                "I couldn't answer that question from the persisted review evidence. Please try tagging me again.".to_string()
            }
            Err(_) => {
                tracing::warn!(
                    repo = %metadata.repository,
                    pr = metadata.pr_number,
                    review_kind = %metadata.review_kind,
                    timeout_secs = question_config.timeout_secs,
                    "Buzz review question timed out"
                );
                "I timed out while answering that question. Please try tagging me again."
                    .to_string()
            }
        }
    };

    let channel_uuid =
        Uuid::parse_str(channel_id).context("Buzz question channel is not a UUID")?;
    let root_id =
        EventId::from_hex(root_event_id).context("Buzz question root event id is invalid")?;
    let answer = truncate_message(answer);
    let answer_event_id =
        send_thread_reply(client, channel_uuid, &answer, &root_id, &event.id, &author).await?;
    crate::state::record_buzz_question(db_path, &event_id, timestamp, Some(&answer_event_id))?;
    tracing::info!(
        repo = %metadata.repository,
        pr = metadata.pr_number,
        review_kind = %metadata.review_kind,
        question_event_id = %event_id,
        answer_event_id = %answer_event_id,
        "Answered Buzz review question"
    );
    Ok(())
}

fn record_ignored_question(db_path: &Path, event_id: &str, timestamp: i64) -> Result<()> {
    crate::state::record_buzz_question(db_path, event_id, timestamp, None)
}

fn event_mentions(event: &Event, pubkey: &str) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("p")
            && parts
                .get(1)
                .is_some_and(|value| value.eq_ignore_ascii_case(pubkey))
    })
}

fn event_channel(event: &Event) -> Option<&str> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some("h"))
            .then(|| parts.get(1).map(String::as_str))
            .flatten()
    })
}

fn event_thread_root(event: &Event) -> Option<&str> {
    let mut reply = None;
    let mut event_tags = 0;
    for tag in event.tags.iter() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("e") {
            continue;
        }
        event_tags += 1;
        let event_id = parts.get(1).map(String::as_str)?;
        match parts.get(3).map(String::as_str) {
            Some("root") => return Some(event_id),
            Some("reply") => reply = Some(event_id),
            _ => {}
        }
    }
    (event_tags == 1).then_some(reply).flatten()
}

pub async fn publish_review_thread(
    config: &BuzzConfig,
    security_review: bool,
    metadata: &ReviewRecord,
    artifact: &ReportingArtifact,
    policy: &DisclosurePolicy,
    existing_thread: Option<&BuzzThreadState>,
) -> Result<Option<BuzzThreadReceipt>> {
    let findings = if artifact.verifier_failed || artifact.markdown_only_fallback {
        Vec::new()
    } else {
        artifact.publishable_findings(policy)
    };
    let channel_id = channel_for(config, security_review)?;
    let channel_uuid =
        Uuid::parse_str(channel_id).context("configured Buzz channel is not a UUID")?;
    let reusable_thread = reusable_thread(existing_thread, channel_id);
    let mut published_finding_keys = reusable_thread
        .map(|thread| {
            thread
                .published_finding_keys
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let new_findings = new_findings(findings, &published_finding_keys);

    if security_review && new_findings.is_empty() {
        tracing::info!(
            repo = %metadata.repository,
            pr = metadata.pr_number,
            "No new verified security findings to publish to the private Buzz channel"
        );
        return Ok(None);
    }
    if reusable_thread.is_some() && new_findings.is_empty() {
        tracing::info!(
            repo = %metadata.repository,
            pr = metadata.pr_number,
            "No new Buzz findings to append to the existing review thread"
        );
        return Ok(None);
    }

    let client = build_client(config)?;
    let (root_event_id, root_id) = match reusable_thread {
        Some(thread) => {
            let root_id = EventId::from_hex(&thread.root_event_id)
                .context("stored Buzz root event id is invalid")?;
            (thread.root_event_id.clone(), root_id)
        }
        None => {
            let summary = artifact
                .pr_summary
                .as_ref()
                .context("Buzz delivery requires the `summary` review lane")?;
            let root_content = render_pr_summary(metadata, summary);
            let root_event_id = send_message(&client, channel_uuid, &root_content, None).await?;
            let root_id = EventId::from_hex(&root_event_id)
                .context("Buzz returned an invalid root event id")?;
            (root_event_id, root_id)
        }
    };

    let mut finding_event_ids = Vec::with_capacity(new_findings.len());
    for (finding, key) in new_findings {
        let content = render_finding(metadata, &finding);
        let event_id = send_message(&client, channel_uuid, &content, Some(&root_id)).await?;
        finding_event_ids.push(event_id);
        published_finding_keys.insert(key);
    }

    Ok(Some(BuzzThreadReceipt {
        channel_id: channel_id.to_string(),
        root_event_id,
        finding_event_ids,
        published_finding_keys: published_finding_keys.into_iter().collect(),
    }))
}

fn reusable_thread<'a>(
    existing_thread: Option<&'a BuzzThreadState>,
    channel_id: &str,
) -> Option<&'a BuzzThreadState> {
    existing_thread.filter(|thread| thread.channel_id == channel_id)
}

fn new_findings(
    findings: Vec<AcceptedFinding>,
    published_finding_keys: &BTreeSet<String>,
) -> Vec<(AcceptedFinding, String)> {
    findings
        .into_iter()
        .filter_map(|finding| {
            let key = finding_key(&finding);
            (!published_finding_keys.contains(&key)).then_some((finding, key))
        })
        .collect()
}

fn finding_key(finding: &AcceptedFinding) -> String {
    let mut paths = finding
        .inline_comments
        .iter()
        .map(|comment| comment.path.trim().to_ascii_lowercase())
        .chain(
            finding
                .additional_locations
                .iter()
                .chain(finding.unanchored_locations.iter())
                .map(|location| location.path.trim().to_ascii_lowercase()),
        )
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    format!(
        "{}\n{}\n{}",
        normalize_finding_text(&finding.title),
        normalize_finding_text(&finding.body),
        paths.join("\n")
    )
}

fn normalize_finding_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn channel_for(config: &BuzzConfig, security: bool) -> Result<&str> {
    let channel = if security {
        config.security_channel.as_deref().with_context(
            || "security review has findings but no private Buzz security_channel is configured",
        )?
    } else {
        config
            .public_channel
            .as_deref()
            .context("no public Buzz channel is configured")?
    };
    let channel = channel.trim();
    if channel.is_empty() {
        bail!("configured Buzz channel must not be empty");
    }
    Ok(channel)
}

fn render_pr_summary(metadata: &ReviewRecord, summary: &PullRequestSummary) -> String {
    let mut content = format!(
        "**{}#{}**\n\n{}",
        metadata.repository,
        metadata.pr_number,
        summary.overview.trim()
    );
    if !summary.key_changes.is_empty() {
        content.push_str("\n\n**Key changes**\n");
        for change in &summary.key_changes {
            content.push_str(&format!("- {}\n", change.trim()));
        }
    }
    if !summary.affected_areas.is_empty() {
        content.push_str("\n**Affected areas:** ");
        content.push_str(&summary.affected_areas.join(", "));
    }
    if let Some(testing) = &summary.testing {
        content.push_str("\n\n**Testing:** ");
        content.push_str(testing.trim());
    }
    content.push_str(&format!(
        "\n\n[`{commit}`](https://github.com/{repo}/pull/{pr}) · [View pull request](https://github.com/{repo}/pull/{pr})",
        commit = abbreviated_commit(&metadata.commit_hash),
        repo = metadata.repository,
        pr = metadata.pr_number,
    ));
    truncate_message(content)
}

fn render_finding(metadata: &ReviewRecord, finding: &AcceptedFinding) -> String {
    let mut content = format!(
        "**{} — {}**\n\n{}",
        finding.severity.to_ascii_uppercase(),
        finding.title.trim(),
        finding.body.trim()
    );

    let mut locations = Vec::new();
    for comment in &finding.inline_comments {
        locations.push(format!("`{}:{}`", comment.path, comment.line));
    }
    for location in finding
        .additional_locations
        .iter()
        .chain(finding.unanchored_locations.iter())
    {
        locations.push(match location.start_line {
            Some(line) => format!("`{}:{line}`", location.path),
            None => format!("`{}`", location.path),
        });
    }
    locations.sort();
    locations.dedup();
    if !locations.is_empty() {
        content.push_str("\n\n**Locations:** ");
        content.push_str(&locations.join(", "));
    }

    content.push_str(&format!(
        "\n\n[View pull request](https://github.com/{repo}/pull/{pr})",
        repo = metadata.repository,
        pr = metadata.pr_number,
    ));
    truncate_message(content)
}

fn abbreviated_commit(commit: &str) -> &str {
    commit.get(..commit.len().min(12)).unwrap_or(commit)
}

fn truncate_message(mut content: String) -> String {
    if content.len() <= MAX_MESSAGE_BYTES {
        return content;
    }
    let mut end = MAX_MESSAGE_BYTES.saturating_sub(32);
    while !content.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    content.truncate(end);
    content.push_str("\n\n… truncated by Fiach");
    content
}

fn build_client(config: &BuzzConfig) -> Result<BuzzClient> {
    let relay_url = config
        .relay_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("BUZZ_RELAY_URL")
                .ok()
                .filter(|url| !url.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_BUZZ_RELAY_URL.to_string());
    let private_key = required_env(&config.private_key_env)?;
    let auth_tag = optional_auth_tag(config);
    let identity = BuzzIdentity::parse(&private_key, auth_tag.as_deref())
        .context("Failed to initialize Buzz identity")?;
    BuzzClient::new(BuzzClientConfig::new(relay_url), identity)
        .context("Failed to initialize Buzz client")
}

fn required_env(source: &str) -> Result<String> {
    let source = source.trim();
    if source.is_empty() {
        bail!("Buzz private key source environment variable must not be empty");
    }
    std::env::var(source)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| {
            format!("required Buzz credential environment variable `{source}` is unset")
        })
}

fn optional_auth_tag(config: &BuzzConfig) -> Option<String> {
    let source = config.auth_tag_env.as_deref().unwrap_or("BUZZ_AUTH_TAG");
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    std::env::var(source)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn build_message_event(
    client: &BuzzClient,
    channel_id: Uuid,
    content: &str,
    reply_to: Option<&EventId>,
) -> Result<Event> {
    let builder = build_message_builder(channel_id, content, reply_to)?;
    client
        .sign_event(builder)
        .context("Failed to sign Buzz channel message")
}

fn build_message_builder(
    channel_id: Uuid,
    content: &str,
    reply_to: Option<&EventId>,
) -> Result<EventBuilder> {
    let thread_ref = reply_to.map(|event_id| ThreadRef {
        root_event_id: event_id.to_owned(),
        parent_event_id: event_id.to_owned(),
    });
    build_thread_message_builder(channel_id, content, thread_ref.as_ref(), &[])
}

fn build_thread_message_builder(
    channel_id: Uuid,
    content: &str,
    thread_ref: Option<&ThreadRef>,
    mentions: &[&str],
) -> Result<EventBuilder> {
    buzz_sdk::build_message(channel_id, content, thread_ref, mentions, false, &[])
        .context("Failed to build Buzz channel message")
}

async fn send_message(
    client: &BuzzClient,
    channel_id: Uuid,
    content: &str,
    reply_to: Option<&EventId>,
) -> Result<String> {
    let event = build_message_event(client, channel_id, content, reply_to)?;
    let response = client
        .submit_event(event)
        .await
        .context("Failed to submit Buzz channel message")?;
    if !response.accepted {
        bail!("Buzz relay rejected message: {}", response.message);
    }
    let event_id =
        EventId::from_hex(&response.event_id).context("Buzz returned an invalid event id")?;
    Ok(event_id.to_hex())
}

async fn send_thread_reply(
    client: &BuzzClient,
    channel_id: Uuid,
    content: &str,
    root_event_id: &EventId,
    parent_event_id: &EventId,
    mention: &str,
) -> Result<String> {
    let thread_ref = ThreadRef {
        root_event_id: root_event_id.to_owned(),
        parent_event_id: parent_event_id.to_owned(),
    };
    let builder = build_thread_message_builder(channel_id, content, Some(&thread_ref), &[mention])?;
    let event = client
        .sign_event(builder)
        .context("Failed to sign Buzz question answer")?;
    let response = client
        .submit_event(event)
        .await
        .context("Failed to submit Buzz question answer")?;
    if !response.accepted {
        bail!("Buzz relay rejected question answer: {}", response.message);
    }
    EventId::from_hex(&response.event_id).context("Buzz returned an invalid answer event id")?;
    Ok(response.event_id)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::reporting::{AffectedLocation, CommandTranscript, Finding, InlineComment, Verdict};
    use crate::state::{ArtifactPaths, ReviewStatus};

    fn metadata() -> ReviewRecord {
        ReviewRecord {
            repository: "block/buzz".to_string(),
            pr_number: 42,
            review_kind: "pr-review".to_string(),
            commit_hash: "abcdef1234567890".to_string(),
            model: "test".to_string(),
            timestamp: 0,
            findings_count: 1,
            status: ReviewStatus::Confirmed,
            severity: "high".to_string(),
            pr_classification: "42".to_string(),
            duration_secs: 1,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
            report_url: None,
            is_rereview: false,
            time_reviewed: None,
            retry_count: 0,
            artifacts: ArtifactPaths::default(),
            disclosure_url: None,
            buzz_thread: None,
            failure_stage: None,
        }
    }

    fn accepted_finding(title: &str, body: &str, path: &str, line: u32) -> AcceptedFinding {
        AcceptedFinding {
            finding_id: "F-1".to_string(),
            title: title.to_string(),
            severity: "high".to_string(),
            impact: None,
            body: body.to_string(),
            inline_comments: vec![InlineComment {
                path: path.to_string(),
                line,
                body: body.to_string(),
            }],
            additional_locations: Vec::new(),
            unanchored_locations: Vec::new(),
            verdict: Verdict {
                finding_id: "F-1".to_string(),
                confirmed: true,
                introduced_by_pr: true,
                present_on_pr_branch: true,
                present_on_base: false,
                present_on_default_branch: false,
                disclosure_decision: "disclose".to_string(),
                title_override: None,
                severity_override: None,
                impact_override: None,
                final_comment_body: None,
                affected_locations: Vec::new(),
                command_transcripts: Vec::new(),
                rationale: "confirmed".to_string(),
            },
        }
    }

    #[test]
    fn summary_message_describes_the_pr_not_the_review() {
        let summary = PullRequestSummary {
            overview: "Adds bounded retries to relay publishing.".to_string(),
            key_changes: vec!["Preserves event ids across retries.".to_string()],
            affected_areas: vec!["relay transport".to_string()],
            testing: Some("Adds transient failure coverage.".to_string()),
        };
        let rendered = render_pr_summary(&metadata(), &summary);
        assert!(rendered.contains("Adds bounded retries"));
        assert!(rendered.contains("Preserves event ids"));
        assert!(!rendered.contains("verified finding"));
        assert!(!rendered.contains("review completed"));
    }

    #[test]
    fn security_routes_only_to_private_channel() {
        let config = BuzzConfig {
            relay_url: None,
            public_channel: Some("public".to_string()),
            security_channel: Some("private".to_string()),
            private_key_env: "FIACH_BUZZ_PRIVATE_KEY".to_string(),
            auth_tag_env: None,
            questions: None,
        };
        assert_eq!(channel_for(&config, false).unwrap(), "public");
        assert_eq!(channel_for(&config, true).unwrap(), "private");
    }

    #[test]
    fn direct_finding_reply_references_the_summary_root() {
        let channel_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let root_id = EventId::from_hex(&"a".repeat(64)).unwrap();
        let keys = nostr::Keys::parse(&"1".repeat(64)).unwrap();

        let event = build_message_builder(channel_id, "Finding", Some(&root_id))
            .unwrap()
            .sign_with_keys(&keys)
            .unwrap();
        let tags = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();

        assert!(tags.contains(&vec!["h".to_string(), channel_id.to_string()]));
        assert!(tags.contains(&vec![
            "e".to_string(),
            root_id.to_hex(),
            String::new(),
            "reply".to_string(),
        ]));
    }

    #[test]
    fn tagged_nested_reply_resolves_to_the_review_root() {
        let channel_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let root_id = EventId::from_hex(&"a".repeat(64)).unwrap();
        let parent_id = EventId::from_hex(&"b".repeat(64)).unwrap();
        let bot_keys = nostr::Keys::parse(&"1".repeat(64)).unwrap();
        let author_keys = nostr::Keys::parse(&"2".repeat(64)).unwrap();
        let bot_pubkey = bot_keys.public_key().to_hex();
        let thread_ref = ThreadRef {
            root_event_id: root_id,
            parent_event_id: parent_id,
        };

        let event = build_thread_message_builder(
            channel_id,
            "@Fiach why is this high severity?",
            Some(&thread_ref),
            &[&bot_pubkey],
        )
        .unwrap()
        .sign_with_keys(&author_keys)
        .unwrap();

        let expected_channel = channel_id.to_string();
        let expected_root = root_id.to_hex();
        assert!(event_mentions(&event, &bot_pubkey));
        assert_eq!(event_channel(&event), Some(expected_channel.as_str()));
        assert_eq!(event_thread_root(&event), Some(expected_root.as_str()));
    }

    #[test]
    fn unthreaded_tag_is_not_a_review_question() {
        let channel_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let bot_keys = nostr::Keys::parse(&"1".repeat(64)).unwrap();
        let author_keys = nostr::Keys::parse(&"2".repeat(64)).unwrap();
        let bot_pubkey = bot_keys.public_key().to_hex();

        let event =
            build_thread_message_builder(channel_id, "@Fiach review this", None, &[&bot_pubkey])
                .unwrap()
                .sign_with_keys(&author_keys)
                .unwrap();

        assert!(event_mentions(&event, &bot_pubkey));
        assert!(event_thread_root(&event).is_none());
    }

    #[test]
    fn repeated_review_reuses_thread_only_in_the_same_channel() {
        let thread = BuzzThreadState {
            channel_id: "public".to_string(),
            root_event_id: "root".to_string(),
            published_finding_keys: Vec::new(),
        };

        assert_eq!(
            reusable_thread(Some(&thread), "public").map(|state| state.root_event_id.as_str()),
            Some("root")
        );
        assert!(reusable_thread(Some(&thread), "different").is_none());
    }

    #[test]
    fn repeated_review_selects_only_findings_not_already_delivered() {
        let original = accepted_finding(
            "Retry duplicates writes",
            "The retry path loses its idempotency key.",
            "src/relay.rs",
            10,
        );
        let repeated = accepted_finding(
            "  RETRY   duplicates writes ",
            "The retry path loses its   idempotency key.",
            "SRC/RELAY.RS",
            40,
        );
        let new = accepted_finding(
            "Retry duplicates writes",
            "A second retry path drops the signed event.",
            "src/relay.rs",
            50,
        );
        let published = BTreeSet::from([finding_key(&original)]);

        assert!(new_findings(vec![repeated], &published).is_empty());
        let selected = new_findings(vec![new], &published);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn finding_message_includes_locations() {
        let finding = Finding {
            id: "F-1".to_string(),
            title: "Retry duplicates writes".to_string(),
            severity: "high".to_string(),
            confidence: "high".to_string(),
            affected_locations: vec![AffectedLocation {
                path: "src/relay.rs".to_string(),
                start_line: Some(10),
                end_line: None,
            }],
            evidence: "evidence".to_string(),
            skills_used: vec!["none".to_string()],
            body_markdown: "The retry path loses its idempotency key.".to_string(),
        };
        let verdict = Verdict {
            finding_id: "F-1".to_string(),
            confirmed: true,
            introduced_by_pr: true,
            present_on_pr_branch: true,
            present_on_base: false,
            present_on_default_branch: false,
            disclosure_decision: "disclose".to_string(),
            title_override: None,
            severity_override: None,
            impact_override: None,
            final_comment_body: None,
            affected_locations: Vec::new(),
            command_transcripts: vec![CommandTranscript {
                command: "git diff".to_string(),
                branch_or_commit: "HEAD".to_string(),
                key_output: "output".to_string(),
                interpretation: "proof".to_string(),
            }],
            rationale: "confirmed".to_string(),
        };
        let artifact = ReportingArtifact {
            pr_summary: None,
            findings: vec![finding],
            verdicts: vec![verdict],
            ..ReportingArtifact::default()
        };
        let policy = DisclosurePolicy {
            pr_context: crate::reporting::PrContext {
                state: "OPEN".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: "head".to_string(),
            },
            diff_anchors: BTreeMap::from([("src/relay.rs".to_string(), BTreeSet::from([10]))]),
        };
        let accepted = artifact.publishable_findings(&policy);
        let rendered = render_finding(&metadata(), &accepted[0]);
        assert!(rendered.contains("`src/relay.rs:10`"));
        assert!(rendered.contains("Retry duplicates writes"));
    }
}
