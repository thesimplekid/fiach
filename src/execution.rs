use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::reporting::{self, DisclosurePolicy, ReportingArtifact};
use crate::review::{CompletedReview, ReviewParams};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionDiagnostics {
    pub sandbox_log: Option<PathBuf>,
    pub executor: String,
}

#[derive(Debug, Clone)]
pub struct ReviewOutcome {
    pub completed: CompletedReview,
    pub artifact: ReportingArtifact,
    pub policy: DisclosurePolicy,
    pub diagnostics: ExecutionDiagnostics,
}

impl ReviewOutcome {
    pub fn load(completed: CompletedReview, diagnostics: ExecutionDiagnostics) -> Result<Self> {
        let structured = completed
            .metadata
            .artifacts
            .structured_json
            .as_ref()
            .context("Review outcome did not record a structured artifact path")?;
        let policy_path = completed
            .metadata
            .artifacts
            .policy_json
            .as_ref()
            .context("Review outcome did not record a disclosure policy path")?;
        let mut artifact: ReportingArtifact =
            serde_json::from_slice(&std::fs::read(structured).with_context(|| {
                format!(
                    "Failed to read structured artifact at {}",
                    structured.display()
                )
            })?)
            .context("Failed to parse structured review artifact")?;
        reporting::validate_artifact(&mut artifact)
            .context("Structured review artifact failed validation")?;
        let policy: DisclosurePolicy =
            serde_json::from_slice(&std::fs::read(policy_path).with_context(|| {
                format!(
                    "Failed to read disclosure policy at {}",
                    policy_path.display()
                )
            })?)
            .context("Failed to parse disclosure policy artifact")?;

        Ok(Self {
            completed,
            artifact,
            policy,
            diagnostics,
        })
    }
}

pub struct ReviewSpec {
    pub params: ReviewParams,
}

pub trait ReviewExecutor: Send + Sync {
    fn execute(
        &self,
        spec: ReviewSpec,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ReviewOutcome>>> + Send + '_>>;
}

pub struct LocalReviewExecutor;

impl ReviewExecutor for LocalReviewExecutor {
    fn execute(
        &self,
        mut spec: ReviewSpec,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ReviewOutcome>>> + Send + '_>> {
        Box::pin(async move {
            spec.params.execution.persist_side_effects = false;
            let completed = crate::review::run_review(spec.params, cancel).await?;
            completed
                .map(|completed| {
                    ReviewOutcome::load(
                        completed,
                        ExecutionDiagnostics {
                            sandbox_log: None,
                            executor: "local".to_string(),
                        },
                    )
                })
                .transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_structured_artifact_cannot_become_an_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("report.md");
        let structured = temp.path().join("report.json");
        let policy = temp.path().join("report.policy.json");
        std::fs::write(&report, "report").unwrap();
        std::fs::write(&structured, b"not-json").unwrap();
        std::fs::write(&policy, b"{}").unwrap();
        let metadata = serde_json::from_value(serde_json::json!({
            "repository": "owner/repo",
            "pr_number": 1,
            "review_kind": "security",
            "commit_hash": "abc123",
            "model": "test",
            "timestamp": 0,
            "findings_count": 0,
            "status": "none",
            "severity": "none",
            "pr_classification": "none",
            "retry_count": 0,
            "artifacts": {
                "markdown": report,
                "structured_json": structured,
                "policy_json": policy
            }
        }))
        .unwrap();
        let completed = CompletedReview {
            metadata,
            should_notify: false,
            report_path: temp.path().join("report.md"),
        };

        let error = ReviewOutcome::load(completed, ExecutionDiagnostics::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("parse structured review artifact")
        );
    }
}
