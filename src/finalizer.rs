use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::disclose::{self, DiscloseConfig, DisclosureTarget};
use crate::execution::ReviewOutcome;
use crate::review::ReviewParams;
use crate::state::{self, ReviewRecord, ReviewStatus};

#[derive(Clone)]
pub struct FinalizationSpec {
    pub repository: String,
    pub pr_number: u64,
    pub review_kind: String,
    pub security_review: bool,
    pub disclose: DiscloseConfig,
    pub buzz: Option<crate::config::BuzzConfig>,
    pub db_path: PathBuf,
    pub trigger_mention_node_id: Option<String>,
}

impl From<&ReviewParams> for FinalizationSpec {
    fn from(params: &ReviewParams) -> Self {
        Self {
            repository: params.repo.clone(),
            pr_number: params.pr_number,
            review_kind: params.review_kind.clone(),
            security_review: params.persona.is_security(),
            disclose: params.disclose_config.clone(),
            buzz: params.buzz_config.clone(),
            db_path: params.db_path.clone(),
            trigger_mention_node_id: params.trigger_mention_node_id.clone(),
        }
    }
}

pub struct ReviewFinalizer;

impl ReviewFinalizer {
    pub async fn finalize(
        &self,
        spec: &FinalizationSpec,
        outcome: ReviewOutcome,
        cancel: CancellationToken,
    ) -> Result<ReviewRecord> {
        let report_path = outcome.completed.report_path.clone();
        if cancel.is_cancelled() {
            anyhow::bail!("Review finalization cancelled");
        }

        let markdown = crate::reporting::render_markdown(
            &spec.repository,
            spec.pr_number,
            &outcome.artifact,
            Some(&outcome.policy),
        );
        std::fs::write(&report_path, markdown).context("Failed to write finalized report")?;
        let structured = crate::review::structured_artifact_path_for_report(&report_path);
        let policy_path = crate::review::disclosure_policy_path_for_report(&report_path);
        std::fs::write(&structured, serde_json::to_vec_pretty(&outcome.artifact)?)
            .context("Failed to write finalized structured artifact")?;
        std::fs::write(&policy_path, serde_json::to_vec_pretty(&outcome.policy)?)
            .context("Failed to write finalized disclosure policy")?;

        let mut metadata = outcome.completed.metadata.clone();
        update_metadata(&mut metadata, &outcome, spec.pr_number);
        metadata.artifacts.markdown = Some(absolute(report_path.clone()));
        metadata.artifacts.structured_json = Some(absolute(structured));
        metadata.artifacts.policy_json = Some(absolute(policy_path));
        metadata.artifacts.sandbox_log = outcome.diagnostics.sandbox_log.map(absolute);

        let target = DisclosureTarget {
            repo: &spec.repository,
            pr_number: spec.pr_number,
            commit_hash: &metadata.commit_hash,
            review_kind: &spec.review_kind,
        };
        let disclosure_scope = disclose::disclosure_scope(&spec.disclose);
        let report_url = match state::begin_disclosure(
            &spec.db_path,
            target.repo,
            target.pr_number,
            target.commit_hash,
            target.review_kind,
            &disclosure_scope,
        )? {
            state::DisclosureClaim::Published(url) => {
                tracing::info!(
                    repo = %target.repo,
                    pr = target.pr_number,
                    review_kind = %target.review_kind,
                    "Disclosure already published; reusing persisted receipt"
                );
                url
            }
            state::DisclosureClaim::Publish { recovering } => {
                let url = disclose::handle_structured_disclosure_idempotent(
                    &report_path,
                    target,
                    &outcome.artifact,
                    &outcome.policy,
                    &spec.disclose,
                    recovering,
                )
                .await?;
                state::finish_disclosure(
                    &spec.db_path,
                    target.repo,
                    target.pr_number,
                    target.commit_hash,
                    target.review_kind,
                    &disclosure_scope,
                    url.as_deref(),
                )
                .context("Failed to persist disclosure receipt")?;
                url
            }
        };
        metadata.report_url.clone_from(&report_url);
        metadata.disclosure_url = report_url;

        if metadata.status == ReviewStatus::None
            && let Some(reaction) = spec.disclose.reactions.no_findings.as_deref()
            && let Err(error) =
                disclose::post_pr_reaction(&spec.repository, spec.pr_number, reaction).await
        {
            tracing::warn!(error = %error, "Failed to post no-findings reaction");
        }
        if let Some(node_id) = spec.trigger_mention_node_id.as_deref()
            && disclose::is_non_actionable_status(metadata.status.as_str())
            && let Some(reaction) = spec.disclose.reactions.no_findings.as_deref()
            && let Err(error) = disclose::finalize_mention_reaction(
                node_id,
                reaction,
                spec.disclose.reactions.review_start.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %error, "Failed to finalize mention reaction");
        }

        if let Some(config) = &spec.buzz {
            match state::get_pr_review(
                &spec.db_path,
                &spec.repository,
                spec.pr_number,
                &spec.review_kind,
            ) {
                Ok(previous) => {
                    metadata.buzz_thread = previous.and_then(|record| record.buzz_thread);
                    match crate::buzz::publish_review_thread(
                        config,
                        spec.security_review,
                        &metadata,
                        &outcome.artifact,
                        &outcome.policy,
                        metadata.buzz_thread.as_ref(),
                    )
                    .await
                    {
                        Ok(Some(receipt)) => {
                            metadata.buzz_thread = Some(state::BuzzThreadState {
                                channel_id: receipt.channel_id,
                                root_event_id: receipt.root_event_id,
                                published_finding_keys: receipt.published_finding_keys,
                            });
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(
                                repo = %spec.repository,
                                pr = spec.pr_number,
                                review_kind = %spec.review_kind,
                                error = %error,
                                "Failed to publish Buzz review thread"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        repo = %spec.repository,
                        pr = spec.pr_number,
                        review_kind = %spec.review_kind,
                        error = %error,
                        "Skipped Buzz publication because existing thread state could not be loaded"
                    );
                }
            }
        }

        state::mark_reviewed(&spec.db_path, &spec.repository, spec.pr_number, &metadata)
            .context("Failed to persist finalized review")?;
        Ok(metadata)
    }
}

fn update_metadata(metadata: &mut ReviewRecord, outcome: &ReviewOutcome, pr_number: u64) {
    let publishable = outcome.artifact.publishable_findings(&outcome.policy);
    let already_reported = outcome.artifact.already_reported_findings(&outcome.policy);
    metadata.findings_count = publishable.len() as u32;
    metadata.status = if outcome.artifact.markdown_only_fallback {
        ReviewStatus::MarkdownOnly
    } else if outcome.artifact.verifier_failed {
        ReviewStatus::Unverified
    } else if !publishable.is_empty() {
        ReviewStatus::Confirmed
    } else if !already_reported.is_empty() {
        ReviewStatus::AlreadyReported
    } else if outcome.artifact.no_findings.is_some() {
        ReviewStatus::None
    } else {
        ReviewStatus::Rejected
    };
    metadata.severity = publishable
        .first()
        .map(|finding| finding.severity.clone())
        .unwrap_or_else(|| "none".to_string());
    metadata.pr_classification = if publishable.is_empty() {
        "none".to_string()
    } else {
        pr_number.to_string()
    };
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_or(path.clone(), |directory| directory.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::execution::ExecutionDiagnostics;
    use crate::reporting::{DisclosurePolicy, NoFindings, PrContext, ReportingArtifact};

    fn clean_outcome(root: &std::path::Path) -> ReviewOutcome {
        let report_path = root.join("report.md");
        let metadata = serde_json::from_value(serde_json::json!({
            "repository": "owner/repo",
            "pr_number": 42,
            "review_kind": "security",
            "commit_hash": "abc123",
            "model": "scripted",
            "timestamp": 1,
            "findings_count": 0,
            "status": "none",
            "severity": "none",
            "pr_classification": "none",
            "retry_count": 0,
            "artifacts": {}
        }))
        .unwrap();
        ReviewOutcome {
            completed: crate::review::CompletedReview {
                metadata,
                should_notify: false,
                report_path,
            },
            artifact: ReportingArtifact {
                no_findings: Some(NoFindings {
                    summary: "No actionable findings in the scripted review.".to_string(),
                    skills_used: vec!["scripted".to_string()],
                }),
                ..ReportingArtifact::default()
            },
            policy: DisclosurePolicy {
                pr_context: PrContext {
                    state: "open".to_string(),
                    merged: false,
                    base_ref_name: "main".to_string(),
                    default_branch: "main".to_string(),
                    base_commit: "base".to_string(),
                    head_commit: "abc123".to_string(),
                },
                diff_anchors: BTreeMap::<String, BTreeSet<u32>>::new(),
            },
            diagnostics: ExecutionDiagnostics {
                sandbox_log: None,
                executor: "scripted".to_string(),
            },
        }
    }

    fn local_spec(root: &std::path::Path) -> FinalizationSpec {
        FinalizationSpec {
            repository: "owner/repo".to_string(),
            pr_number: 42,
            review_kind: "security".to_string(),
            security_review: true,
            disclose: DiscloseConfig {
                mode: disclose::ReportMode::Local,
                sync_repo: None,
                notify_on_empty: false,
                reactions: disclose::ReactionConfig {
                    review_start: None,
                    no_findings: None,
                },
            },
            buzz: None,
            db_path: root.join("state.redb"),
            trigger_mention_node_id: None,
        }
    }

    #[tokio::test]
    async fn scripted_outcome_runs_through_render_journal_and_state() {
        let temp = tempfile::tempdir().unwrap();
        let spec = local_spec(temp.path());
        let outcome = clean_outcome(temp.path());
        let finalizer = ReviewFinalizer;

        let first = finalizer
            .finalize(&spec, outcome.clone(), CancellationToken::new())
            .await
            .unwrap();
        let second = finalizer
            .finalize(&spec, outcome, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(first.status, ReviewStatus::None);
        assert_eq!(first.report_url, second.report_url);
        assert!(first.artifacts.markdown.as_ref().unwrap().exists());
        assert!(first.artifacts.structured_json.as_ref().unwrap().exists());
        assert!(first.artifacts.policy_json.as_ref().unwrap().exists());
        let stored = state::get_pr_review(&spec.db_path, "owner/repo", 42, "security")
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ReviewStatus::None);
        assert_eq!(stored.commit_hash, "abc123");
    }
}
