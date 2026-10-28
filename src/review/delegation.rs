use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, stream::FuturesUnordered};
use rmcp::{
    ServiceError,
    model::{CallToolResult, ContentBlock as Content, JsonObject},
};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{
    McpClientTrait, ToolCallContext, call_tool_result_failed, extract_subagent_task_id,
    summon_error_category, summon_task_status,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LaneStatus {
    Pending,
    Running,
    Completed,
    Incomplete,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaneSnapshot {
    pub lane: String,
    pub task_id: Option<String>,
    pub status: LaneStatus,
    pub recovery_attempt: u32,
    pub reason: Option<String>,
    /// Untrusted lane output, never authoritative structured findings.
    pub raw_notes: Option<String>,
}

struct Lane {
    snapshot: LaneSnapshot,
    arguments: JsonObject,
    started: Instant,
    result: Option<CallToolResult>,
    gate: Arc<Mutex<()>>,
}

pub(super) struct Recovery {
    lanes: Mutex<HashMap<String, Lane>>,
    tasks: Mutex<HashMap<String, String>>,
    attempt_timeout: Duration,
}

impl Recovery {
    pub fn new(lanes: &[String], attempt_timeout: Duration) -> Self {
        Self {
            lanes: Mutex::new(
                lanes
                    .iter()
                    .map(|lane| {
                        (
                            lane.clone(),
                            Lane {
                                snapshot: LaneSnapshot {
                                    lane: lane.clone(),
                                    task_id: None,
                                    status: LaneStatus::Pending,
                                    recovery_attempt: 0,
                                    reason: None,
                                    raw_notes: None,
                                },
                                arguments: JsonObject::new(),
                                started: Instant::now(),
                                result: None,
                                gate: Arc::new(Mutex::new(())),
                            },
                        )
                    })
                    .collect(),
            ),
            tasks: Mutex::new(HashMap::new()),
            attempt_timeout,
        }
    }

    pub async fn snapshots(&self) -> Vec<LaneSnapshot> {
        let mut snapshots: Vec<_> = self
            .lanes
            .lock()
            .await
            .values()
            .map(|lane| lane.snapshot.clone())
            .collect();
        snapshots.sort_by(|a, b| a.lane.cmp(&b.lane));
        snapshots
    }

    pub async fn complete(&self) -> bool {
        self.snapshots()
            .await
            .iter()
            .all(|lane| lane.status == LaneStatus::Completed)
    }

    /// Independent of coordinator cooperation: stop a repeatedly failing lane
    /// before the finder deadline, even if the model stops loading its result.
    pub async fn exhausted(&self) -> bool {
        let mut lanes = self.lanes.lock().await;
        for lane in lanes.values_mut() {
            if lane.snapshot.status == LaneStatus::Running
                && lane.started.elapsed() > self.attempt_timeout * 4
            {
                lane.snapshot.status = LaneStatus::Incomplete;
                lane.snapshot.reason = Some("lane_deadline".into());
            }
        }
        lanes
            .values()
            .any(|lane| lane.snapshot.status == LaneStatus::Incomplete)
    }

    async fn incomplete(&self, lane: &str, reason: &str) {
        if let Some(record) = self.lanes.lock().await.get_mut(lane) {
            record.snapshot.status = LaneStatus::Incomplete;
            record.snapshot.reason = Some(reason.into());
        }
    }

    async fn invoke(
        inner: &dyn McpClientTrait,
        ctx: &ToolCallContext,
        name: &str,
        arguments: JsonObject,
        limit: Duration,
        cancel: CancellationToken,
    ) -> CallToolResult {
        match tokio::time::timeout(limit, inner.call_tool(ctx, name, Some(arguments), cancel)).await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => CallToolResult::error(vec![Content::text("transport_error")]),
            Err(_) => CallToolResult::error(vec![Content::text("host_wait_timeout")]),
        }
    }

    async fn cancel_task(inner: &dyn McpClientTrait, ctx: &ToolCallContext, task: &str) {
        // Goose cancels its token, allows five seconds, then aborts the task.
        // Bound the entire call too, including session bookkeeping afterwards.
        let args = serde_json::json!({"source": task, "cancel": true})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let _ = Self::invoke(
            inner,
            ctx,
            "load",
            args,
            Duration::from_secs(7),
            CancellationToken::new(),
        )
        .await;
    }

    pub async fn cleanup(&self, inner: &dyn McpClientTrait, ctx: &ToolCallContext) {
        let tasks: Vec<_> = self
            .lanes
            .lock()
            .await
            .values()
            .filter(|lane| lane.snapshot.status != LaneStatus::Completed)
            .filter_map(|lane| lane.snapshot.task_id.clone())
            .collect();
        let mut pending: FuturesUnordered<_> = tasks
            .iter()
            .map(|task| Self::cancel_task(inner, ctx, task))
            .collect();
        while pending.next().await.is_some() {}
    }

    async fn start(
        &self,
        inner: &dyn McpClientTrait,
        ctx: &ToolCallContext,
        lane: &str,
        cancel: CancellationToken,
    ) -> CallToolResult {
        loop {
            let args = self
                .lanes
                .lock()
                .await
                .get(lane)
                .map(|record| record.arguments.clone())
                .unwrap_or_default();
            let result = Self::invoke(
                inner,
                ctx,
                "delegate",
                args,
                self.attempt_timeout,
                cancel.clone(),
            )
            .await;
            if !call_tool_result_failed(&result)
                && let Some(task) = extract_subagent_task_id(&result)
            {
                self.tasks.lock().await.insert(task.clone(), lane.into());
                if let Some(record) = self.lanes.lock().await.get_mut(lane) {
                    record.snapshot.task_id = Some(task);
                }
                return result;
            }
            let attempts = self
                .lanes
                .lock()
                .await
                .get(lane)
                .map(|record| record.snapshot.recovery_attempt)
                .unwrap_or_default();
            tracing::warn!(
                lane,
                recovery_attempt = attempts,
                error_category = summon_error_category(&result),
                "Delegated lane failed to start"
            );
            if !self.replace(lane).await {
                return failure();
            }
        }
    }

    async fn replace(&self, lane: &str) -> bool {
        let mut lanes = self.lanes.lock().await;
        let Some(record) = lanes.get_mut(lane) else {
            return false;
        };
        if record.snapshot.recovery_attempt >= 1 || record.snapshot.status == LaneStatus::Incomplete
        {
            record.snapshot.status = LaneStatus::Incomplete;
            record.snapshot.reason = Some("recovery_exhausted".into());
            return false;
        }
        record.snapshot.recovery_attempt += 1;
        true
    }

    pub async fn call(
        &self,
        inner: &dyn McpClientTrait,
        ctx: &ToolCallContext,
        name: &str,
        mut arguments: JsonObject,
        cancel: CancellationToken,
    ) -> Result<CallToolResult, ServiceError> {
        if name == "delegate" {
            let lane = arguments
                .get("instructions")
                .and_then(|v| v.as_str())
                .and_then(|text| text.lines().next())
                .and_then(|line| line.strip_prefix("Review lane: "))
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            {
                let mut lanes = self.lanes.lock().await;
                let Some(record) = lanes.get_mut(&lane) else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Begin instructions with Review lane: <configured lane> on its own line",
                    )]));
                };
                if record.snapshot.status != LaneStatus::Pending {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Lane already started; load its existing task. The host owns replacement attempts.",
                    )]));
                }
                arguments.insert("async".into(), true.into());
                record.arguments = arguments;
                record.started = Instant::now();
                record.snapshot.status = LaneStatus::Running;
            }
            return Ok(self.start(inner, ctx, &lane, cancel).await);
        }
        let source = arguments
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let Some(lane) = self.tasks.lock().await.get(source).cloned() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Load requires a task ID created by this review; read skills with inspect",
            )]));
        };
        let gate = self
            .lanes
            .lock()
            .await
            .get(&lane)
            .map(|record| record.gate.clone());
        let Some(gate) = gate else {
            return Ok(failure());
        };
        let _guard = gate.lock().await;
        let mut waits = 0;
        loop {
            let (snapshot, cached) = {
                let lanes = self.lanes.lock().await;
                let Some(record) = lanes.get(&lane) else {
                    return Ok(failure());
                };
                (record.snapshot.clone(), record.result.clone())
            };
            if let Some(result) = cached {
                return Ok(result);
            }
            if snapshot.status == LaneStatus::Incomplete {
                return Ok(failure());
            }
            let Some(task) = snapshot.task_id else {
                return Ok(failure());
            };
            arguments.insert("source".into(), task.clone().into());
            if arguments.get("cancel").and_then(|v| v.as_bool()) == Some(true) {
                Self::cancel_task(inner, ctx, &task).await;
                self.incomplete(&lane, "cancelled").await;
                return Ok(failure());
            }
            let result = Self::invoke(
                inner,
                ctx,
                "load",
                arguments.clone(),
                self.attempt_timeout,
                cancel.clone(),
            )
            .await;
            let status = summon_task_status(&result);
            if status == Some("completed") && !call_tool_result_failed(&result) {
                if let Some(record) = self.lanes.lock().await.get_mut(&lane) {
                    record.snapshot.status = LaneStatus::Completed;
                    record.snapshot.raw_notes = Some(
                        result
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                Content::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                            .chars()
                            .take(16_384)
                            .collect(),
                    );
                    record.result = Some(result.clone());
                }
                return Ok(result);
            }
            if status == Some("running") {
                return Ok(result);
            }
            let reason = summon_error_category(&result);
            tracing::warn!(lane = %lane, task_id = %task, recovery_attempt = snapshot.recovery_attempt, error_category = reason, "Delegated lane load failed");
            let terminal = matches!(status, Some("failed" | "panicked" | "cancelled"));
            waits += 1;
            if !terminal && waits < 2 {
                continue;
            }
            Self::cancel_task(inner, ctx, &task).await;
            if !self.replace(&lane).await {
                return Ok(failure());
            }
            let replacement = self.start(inner, ctx, &lane, cancel.clone()).await;
            if call_tool_result_failed(&replacement) {
                return Ok(failure());
            }
            waits = 0;
        }
    }
}

fn failure() -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(
        "Delegated lane incomplete: bounded recovery exhausted or cancelled. Never submit a clean review.",
    )]);
    result.meta = serde_json::from_value(serde_json::json!({"task_status": "failed"})).ok();
    result
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use rmcp::model::{InitializeResult, ListToolsResult};

    use super::*;

    struct Scripted {
        results: Mutex<VecDeque<CallToolResult>>,
        calls: Mutex<Vec<(String, JsonObject)>>,
    }

    #[async_trait::async_trait]
    impl McpClientTrait for Scripted {
        async fn list_tools(
            &self,
            _: &str,
            _: Option<String>,
            _: CancellationToken,
        ) -> Result<ListToolsResult, ServiceError> {
            Ok(ListToolsResult::default())
        }
        async fn call_tool(
            &self,
            _: &ToolCallContext,
            name: &str,
            args: Option<JsonObject>,
            _: CancellationToken,
        ) -> Result<CallToolResult, ServiceError> {
            self.calls
                .lock()
                .await
                .push((name.into(), args.unwrap_or_default()));
            Ok(self
                .results
                .lock()
                .await
                .pop_front()
                .expect("unexpected call"))
        }
        fn get_info(&self) -> Option<&InitializeResult> {
            None
        }
    }

    fn result(task: &str, status: &str) -> CallToolResult {
        let mut result = CallToolResult::success(vec![Content::text("lane notes")]);
        result.meta = Some(
            serde_json::from_value(
                serde_json::json!({"subagent_session_id": task, "task_status": status}),
            )
            .unwrap(),
        );
        result
    }
    fn setup(results: Vec<CallToolResult>) -> (Recovery, Scripted, ToolCallContext) {
        (
            Recovery::new(
                &["persona".into(), "security".into()],
                Duration::from_millis(50),
            ),
            Scripted {
                results: Mutex::new(results.into()),
                calls: Mutex::new(vec![]),
            },
            ToolCallContext::new("test".into(), None, None),
        )
    }
    async fn delegate(
        recovery: &Recovery,
        inner: &Scripted,
        ctx: &ToolCallContext,
        lane: &str,
    ) -> CallToolResult {
        recovery.call(inner, ctx, "delegate", serde_json::json!({"instructions": format!("Review lane: {lane}\nInspect the diff")}).as_object().unwrap().clone(), CancellationToken::new()).await.unwrap()
    }
    async fn load(
        recovery: &Recovery,
        inner: &Scripted,
        ctx: &ToolCallContext,
        task: &str,
    ) -> CallToolResult {
        recovery
            .call(
                inner,
                ctx,
                "load",
                serde_json::json!({"source": task})
                    .as_object()
                    .unwrap()
                    .clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn transient_wait_does_not_replace_and_completed_results_are_cached() {
        let (recovery, inner, ctx) = setup(vec![
            result("one", "running"),
            CallToolResult::error(vec![Content::text("still running after waiting")]),
            result("one", "completed"),
        ]);
        delegate(&recovery, &inner, &ctx, "persona").await;
        assert_eq!(
            summon_task_status(&load(&recovery, &inner, &ctx, "one").await),
            Some("completed")
        );
        load(&recovery, &inner, &ctx, "one").await;
        assert_eq!(inner.calls.lock().await.len(), 3);
        assert_eq!(recovery.snapshots().await[0].recovery_attempt, 0);
        assert!(!recovery.complete().await); // security was never run
    }

    #[tokio::test]
    async fn terminal_failure_cancels_and_replacement_preserves_other_lane() {
        let (recovery, inner, ctx) = setup(vec![
            result("other", "running"),
            result("other", "completed"),
            result("one", "running"),
            result("one", "failed"),
            result("one", "cancelled"),
            result("two", "running"),
            result("two", "completed"),
        ]);
        delegate(&recovery, &inner, &ctx, "security").await;
        load(&recovery, &inner, &ctx, "other").await;
        delegate(&recovery, &inner, &ctx, "persona").await;
        assert_eq!(
            summon_task_status(&load(&recovery, &inner, &ctx, "one").await),
            Some("completed")
        );
        assert!(recovery.complete().await);
        assert!(!recovery.exhausted().await);
        let calls = inner.calls.lock().await;
        assert_eq!(calls[4].1.get("cancel"), Some(&true.into()));
        assert_eq!(calls[6].1.get("source"), Some(&"two".into()));
    }

    #[tokio::test]
    async fn repeated_failures_exhaust_one_replacement_and_cannot_restart() {
        let error = || CallToolResult::error(vec![Content::text("unknown failure")]);
        let (recovery, inner, ctx) = setup(vec![
            result("one", "running"),
            error(),
            error(),
            result("one", "cancelled"),
            result("two", "running"),
            error(),
            error(),
            result("two", "cancelled"),
        ]);
        delegate(&recovery, &inner, &ctx, "persona").await;
        assert!(call_tool_result_failed(
            &load(&recovery, &inner, &ctx, "one").await
        ));
        assert!(recovery.exhausted().await);
        assert!(!recovery.complete().await);
        assert!(call_tool_result_failed(
            &delegate(&recovery, &inner, &ctx, "persona").await
        ));
        assert_eq!(inner.calls.lock().await.len(), 8);
    }

    #[tokio::test]
    async fn explicit_cancellation_is_incomplete() {
        let (recovery, inner, ctx) =
            setup(vec![result("one", "running"), result("one", "cancelled")]);
        delegate(&recovery, &inner, &ctx, "persona").await;
        let output = recovery
            .call(
                &inner,
                &ctx,
                "load",
                serde_json::json!({"source": "one", "cancel": true})
                    .as_object()
                    .unwrap()
                    .clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(call_tool_result_failed(&output));
        assert_eq!(
            recovery.snapshots().await[0].reason.as_deref(),
            Some("cancelled")
        );
    }

    #[tokio::test]
    async fn watchdog_bounds_uncollected_tasks() {
        let (recovery, inner, ctx) = setup(vec![result("one", "running")]);
        delegate(&recovery, &inner, &ctx, "persona").await;
        recovery
            .lanes
            .lock()
            .await
            .get_mut("persona")
            .unwrap()
            .started = Instant::now() - Duration::from_secs(1);
        assert!(recovery.exhausted().await);
        assert_eq!(
            recovery.snapshots().await[0].reason.as_deref(),
            Some("lane_deadline")
        );
    }
}
