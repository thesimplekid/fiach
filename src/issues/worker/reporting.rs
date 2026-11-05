use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, ensure};
use goose::agents::{mcp_client::McpClientTrait, tool_execution::ToolCallContext};
use rmcp::{
    ServiceError,
    model::{
        CallToolResult, ContentBlock as Content, InitializeResult, JsonObject, ListToolsResult,
        Tool,
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{FixKind, Report};

pub(super) struct ReportingClient {
    pub verify: bool,
    pub kind: FixKind,
    pub output: PathBuf,
    pub report: Arc<Mutex<Option<Report>>>,
}

impl ReportingClient {
    async fn submit(&self, name: &str, arguments: Option<JsonObject>) -> Result<()> {
        let report: Report = serde_json::from_value(serde_json::Value::Object(
            arguments.context("Missing report arguments")?,
        ))
        .context("Invalid report fields")?;
        validate(name, &report, self.verify, self.kind)?;
        let mut accepted = self.report.lock().await;
        if let Some(previous) = accepted.as_ref() {
            ensure!(
                previous == &report,
                "A different report was already accepted; submission is final"
            );
            return Ok(());
        }
        // Persist before acknowledging so a subsequent transport failure cannot lose it.
        tokio::fs::write(
            self.output.join("report.json"),
            serde_json::to_vec(&report)?,
        )
        .await?;
        *accepted = Some(report);
        Ok(())
    }
}

fn validate(name: &str, report: &Report, verify: bool, kind: FixKind) -> Result<()> {
    ensure!(
        !report.summary.trim().is_empty(),
        "summary must explain the result or precise blocker"
    );
    let blocked = matches!(
        report.status.as_str(),
        "needs_info" | "needs_decision" | "needs_review"
    );
    match (verify, name) {
        (false, "submit_candidate") => {
            ensure!(
                kind != FixKind::Investigation,
                "Investigation has no patch validation policy; use submit_blocked"
            );
            ensure!(
                report.status == "candidate" && !report.approved,
                "Candidate requires status=candidate and approved=false"
            );
            ensure!(
                !report.reproduction.is_empty() && !report.reproduction[0].trim().is_empty(),
                "reproduction must contain a program and its arguments"
            );
            match kind {
                FixKind::Bug => ensure!(
                    !report.test_files.is_empty(),
                    "Bug candidate requires regression test_files"
                ),
                FixKind::Maintenance => ensure!(
                    report.test_files.is_empty(),
                    "Maintenance requires empty test_files"
                ),
                FixKind::Investigation => anyhow::bail!("Investigation cannot submit a candidate"),
            }
            ensure!(
                report.test_files.iter().all(|p| !p.trim().is_empty()),
                "test_files cannot contain empty paths"
            );
        }
        (false, "submit_blocked") => ensure!(
            blocked && !report.approved,
            "Blocked result requires needs_info, needs_decision, or needs_review and approved=false"
        ),
        (true, "submit_verdict") => {
            ensure!(
                (report.status == "verified" && report.approved) || (blocked && !report.approved),
                "Verdict requires verified/approved=true or a blocked status/approved=false"
            );
            ensure!(
                report.test_files.is_empty() && report.reproduction.is_empty(),
                "Verifier must use empty test_files and reproduction"
            );
        }
        _ => anyhow::bail!("Tool {name} is not available in this worker phase"),
    }
    Ok(())
}

#[async_trait::async_trait]
impl McpClientTrait for ReportingClient {
    async fn list_tools(
        &self,
        _: &str,
        _: Option<String>,
        _: CancellationToken,
    ) -> std::result::Result<ListToolsResult, ServiceError> {
        let names = if self.verify {
            vec!["submit_verdict"]
        } else {
            vec!["submit_candidate", "submit_blocked"]
        };
        let schema = serde_json::json!({
            "type": "object", "additionalProperties": false,
            "required": ["status", "summary", "test_files", "reproduction", "approved"],
            "properties": {
                "status": {"type":"string", "enum":["candidate", "verified", "needs_info", "needs_decision", "needs_review"]},
                "summary": {"type":"string", "minLength":1},
                "test_files": {"type":"array", "items":{"type":"string"}},
                "reproduction": {"type":"array", "items":{"type":"string"}},
                "approved": {"type":"boolean"}
            }
        });
        let schema = schema.as_object().cloned().unwrap_or_default();
        Ok(ListToolsResult::with_all_items(names.into_iter().map(|name| Tool::new(name.to_owned(), "Submit the final worker report. Correct any validation error and resubmit. An accepted report is final; stop work. This does not publish a PR.".to_owned(), schema.clone())).collect()))
    }

    async fn call_tool(
        &self,
        _: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _: CancellationToken,
    ) -> std::result::Result<CallToolResult, ServiceError> {
        Ok(match self.submit(name, arguments).await {
            Ok(()) => CallToolResult::success(vec![Content::text(
                "Report accepted. Stop work; the host handles validation and publication.",
            )]),
            Err(error) => CallToolResult::error(vec![Content::text(format!("{error:#}"))]),
        })
    }
    fn get_info(&self) -> Option<&InitializeResult> {
        None
    }
    fn get_instructions(&self) -> Option<String> {
        Some("Finish by calling a reporting tool. Plain text or JSON in an assistant message is not a submission. Correct tool validation errors before stopping.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            status: "candidate".into(),
            summary: "Fix with regression coverage".into(),
            test_files: vec!["tests/regression.rs".into()],
            reproduction: vec!["cargo".into(), "test".into()],
            approved: false,
        }
    }

    #[test]
    fn validates_phase_status_and_validation_policy() {
        let candidate = report();
        assert!(validate("submit_candidate", &candidate, false, FixKind::Bug).is_ok());
        assert!(validate("submit_candidate", &candidate, true, FixKind::Bug).is_err());
        assert!(
            validate(
                "submit_candidate",
                &candidate,
                false,
                FixKind::Investigation
            )
            .is_err()
        );
        assert!(validate("submit_candidate", &candidate, false, FixKind::Maintenance).is_err());
        let mut maintenance = candidate.clone();
        maintenance.test_files.clear();
        assert!(
            validate(
                "submit_candidate",
                &maintenance,
                false,
                FixKind::Maintenance
            )
            .is_ok()
        );
        assert!(validate("submit_candidate", &maintenance, false, FixKind::Bug).is_err());
        maintenance.reproduction.clear();
        assert!(
            validate(
                "submit_candidate",
                &maintenance,
                false,
                FixKind::Maintenance
            )
            .is_err()
        );
        let mut verdict = maintenance;
        verdict.status = "verified".into();
        assert!(validate("submit_verdict", &verdict, true, FixKind::Bug).is_err());
        verdict.approved = true;
        assert!(validate("submit_verdict", &verdict, true, FixKind::Bug).is_ok());
        verdict.status = "needs_review".into();
        assert!(validate("submit_verdict", &verdict, true, FixKind::Bug).is_err());
        verdict.approved = false;
        assert!(validate("submit_verdict", &verdict, true, FixKind::Bug).is_ok());
        assert!(validate("submit_blocked", &verdict, false, FixKind::Investigation).is_ok());
        verdict.summary = " ".into();
        assert!(validate("submit_blocked", &verdict, false, FixKind::Bug).is_err());
    }

    #[tokio::test]
    async fn invalid_submission_can_be_corrected_and_accepted_result_is_durable() {
        let output = tempfile::tempdir().unwrap();
        let client = ReportingClient {
            verify: false,
            kind: FixKind::Bug,
            output: output.path().into(),
            report: Arc::new(Mutex::new(None)),
        };
        let mut args = serde_json::to_value(report())
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        args.remove("summary");
        let error = client
            .submit("submit_candidate", Some(args.clone()))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("summary"));
        assert!(client.report.lock().await.is_none());
        assert!(!output.path().join("report.json").exists());
        args.insert("summary".into(), "Fixed and tested".into());
        args.insert("unexpected".into(), true.into());
        assert!(
            client
                .submit("submit_candidate", Some(args.clone()))
                .await
                .is_err()
        );
        args.remove("unexpected");
        client
            .submit("submit_candidate", Some(args.clone()))
            .await
            .unwrap();
        let accepted: Report =
            serde_json::from_slice(&std::fs::read(output.path().join("report.json")).unwrap())
                .unwrap();
        assert_eq!(accepted.summary, "Fixed and tested");
        // A lost acknowledgement can safely be retried, but cannot replace the result.
        client
            .submit("submit_candidate", Some(args.clone()))
            .await
            .unwrap();
        args.insert("summary".into(), "Changed my mind".into());
        assert!(client.submit("submit_candidate", Some(args)).await.is_err());
        assert_eq!(*client.report.lock().await, Some(accepted));
    }

    #[tokio::test]
    async fn failed_persistence_does_not_accept_report() {
        let output = tempfile::tempdir().unwrap();
        let client = ReportingClient {
            verify: false,
            kind: FixKind::Bug,
            output: output.path().join("missing"),
            report: Arc::new(Mutex::new(None)),
        };
        let args = serde_json::to_value(report())
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        assert!(client.submit("submit_candidate", Some(args)).await.is_err());
        assert!(client.report.lock().await.is_none());
    }
}
