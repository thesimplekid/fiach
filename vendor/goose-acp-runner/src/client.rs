use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::{Completion, Connection, LaunchSpec, Launcher, ProcessLauncher, RunResult, Task};

type Failure = (Completion, String);

/// A live conversation. Follow-up prompts retain the same Goose process and
/// session. Any failed or dropped prompt closes execution; it is never replayed.
pub struct Session {
    client: Client,
    transport: Option<Box<dyn crate::Transport>>,
    task: Task,
}

impl Session {
    // Keep the same evidence-bearing result type as prompt and one-shot calls.
    #[allow(clippy::result_large_err)]
    pub async fn connect(
        spec: &LaunchSpec,
        task: &Task,
        cancel: CancellationToken,
    ) -> Result<Self, RunResult> {
        Self::connect_with_launcher(&ProcessLauncher, spec, task, cancel).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn connect_with_launcher(
        launcher: &dyn Launcher,
        spec: &LaunchSpec,
        task: &Task,
        cancel: CancellationToken,
    ) -> Result<Self, RunResult> {
        let deadline = tokio::time::Instant::now() + task.timeout;
        let connection=tokio::select! {
            biased;
            _=cancel.cancelled()=>Err((Completion::Cancelled,"session cancelled".into())),
            _=tokio::time::sleep_until(deadline)=>Err((Completion::TimedOut,"session deadline exceeded".into())),
            c=launcher.launch(spec,task)=>c.map_err(|e|(Completion::LaunchError,e.to_string())),
        }.map_err(failure_result)?;
        let Connection {
            reader,
            writer,
            mut transport,
        } = connection;
        let mut client = Client {
            reader: BufReader::new(reader),
            pending_frame: Vec::new(),
            writer,
            next_id: 1,
            session: None,
            result: RunResult::default(),
            transcript_bytes: 0,
            spec: spec.clone(),
            calls: HashMap::new(),
            secrets: secrets(spec, task),
        };
        let setup = tokio::select! {
            biased;
            _=cancel.cancelled()=>Err((Completion::Cancelled,"session cancelled".into())),
            _=tokio::time::sleep_until(deadline)=>Err((Completion::TimedOut,"session deadline exceeded".into())),
            r=client.setup(task)=>r,
        };
        if let Err(error) = setup {
            cleanup(transport.as_mut()).await;
            let (status, message) = error;
            let safe = client.scrub(&Value::String(message));
            return Err(failure_result((
                status,
                safe.as_str().unwrap_or("ACP failure").to_owned(),
            )));
        }
        Ok(Self {
            client,
            transport: Some(transport),
            task: task.clone(),
        })
    }

    /// Updates from `observer` include cumulative session usage. Hosts should
    /// record deltas between turns rather than adding cumulative totals twice.
    pub async fn prompt(
        &mut self,
        prompt: &str,
        cancel: CancellationToken,
        observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
    ) -> RunResult {
        let Some(mut transport) = self.transport.take() else {
            return failure_result((
                Completion::ProtocolError,
                "session is closed; refusing replay".into(),
            ));
        };
        let started = Instant::now();
        self.client.result = RunResult::default();
        self.client.transcript_bytes = 0;
        self.client.calls.clear();
        let mut task = self.task.clone();
        task.prompt = prompt.to_owned();
        let outcome = tokio::select! {
            biased;
            _=cancel.cancelled()=>Err((Completion::Cancelled,"task cancelled".into())),
            _=tokio::time::sleep(task.timeout)=>Err((Completion::TimedOut,"task deadline exceeded".into())),
            result=self.client.prompt(&task,observer)=>result,
        };
        match outcome {
            Ok(()) => {
                self.client.result.status = Completion::Completed;
                self.transport = Some(transport);
            }
            Err((status, error)) => {
                self.client.result.status = status;
                self.client.result.error = Some(
                    self.client
                        .scrub(&Value::String(error))
                        .as_str()
                        .unwrap_or("ACP failure")
                        .to_owned(),
                );
                if matches!(status, Completion::Cancelled | Completion::TimedOut) {
                    let id = self.client.session.clone();
                    let grace_period = self.client.spec.cancellation_grace;
                    let grace = async {
                        self.client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}})).await?;
                        loop {
                            let frame = self.client.read().await?;
                            if frame.get("result").is_some() || frame.get("error").is_some() {
                                break;
                            }
                            self.client.handle(frame, observer).await?;
                        }
                        Ok::<_, Failure>(())
                    };
                    let _ = tokio::time::timeout(grace_period, grace).await;
                }
                cleanup(transport.as_mut()).await;
            }
        }
        self.client.result.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        std::mem::take(&mut self.client.result)
    }

    pub async fn close(&mut self) {
        if let Some(mut transport) = self.transport.take() {
            cleanup(transport.as_mut()).await;
        }
    }
}

fn failure_result((status, error): Failure) -> RunResult {
    RunResult {
        status,
        error: Some(error),
        ..RunResult::default()
    }
}

pub async fn run(spec: &LaunchSpec, task: &Task, cancel: CancellationToken) -> RunResult {
    run_inner(&ProcessLauncher, spec, task, cancel, None).await
}

/// Observe sanitized updates while running. The callback may cancel the supplied
/// token to enforce an application policy; it must not block the protocol loop.
pub async fn run_observed(
    spec: &LaunchSpec,
    task: &Task,
    cancel: CancellationToken,
    observer: &(dyn Fn(&Value) + Send + Sync),
) -> RunResult {
    run_inner(&ProcessLauncher, spec, task, cancel, Some(observer)).await
}

/// Execute exactly one task. Transport failure is terminal and never replayed.
pub async fn run_with_launcher(
    launcher: &dyn Launcher,
    spec: &LaunchSpec,
    task: &Task,
    cancel: CancellationToken,
) -> RunResult {
    run_inner(launcher, spec, task, cancel, None).await
}

async fn run_inner(
    launcher: &dyn Launcher,
    spec: &LaunchSpec,
    task: &Task,
    cancel: CancellationToken,
    observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
) -> RunResult {
    let started = Instant::now();
    let mut result = RunResult::default();
    let deadline = tokio::time::Instant::now() + task.timeout;
    let launched = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err((Completion::Cancelled, "task cancelled before launch".into())),
        _ = tokio::time::sleep_until(deadline) => Err((Completion::TimedOut, "task deadline exceeded".into())),
        connection = launcher.launch(spec,task) => connection.map_err(|e| (Completion::LaunchError,e.to_string())),
    };
    let connection = match launched {
        Ok(c) => c,
        Err((status, error)) => {
            result.status = status;
            result.error = Some(error);
            return result;
        }
    };
    let Connection {
        reader,
        writer,
        mut transport,
    } = connection;
    let mut client = Client {
        reader: BufReader::new(reader),
        pending_frame: Vec::new(),
        writer,
        next_id: 1,
        session: None,
        result,
        transcript_bytes: 0,
        spec: spec.clone(),
        calls: HashMap::new(),
        secrets: secrets(spec, task),
    };
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err((Completion::Cancelled,"task cancelled".into())),
        _ = tokio::time::sleep_until(deadline) => Err((Completion::TimedOut,"task deadline exceeded".into())),
        outcome = client.execute(task, observer) => outcome,
    };
    match outcome {
        Ok(()) => client.result.status = Completion::Completed,
        Err((status, error)) => {
            client.result.status = status;
            client.result.error = Some(
                client
                    .scrub(&Value::String(error))
                    .as_str()
                    .unwrap_or("ACP failure")
                    .to_owned(),
            );
            if matches!(status, Completion::Cancelled | Completion::TimedOut)
                && let Some(session) = client.session.clone()
            {
                let grace = async {
                    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}})).await?;
                    loop {
                        let frame = client.read().await?;
                        if frame.get("result").is_some() || frame.get("error").is_some() {
                            break;
                        }
                        client.handle(frame, observer).await?;
                    }
                    Ok::<_, Failure>(())
                };
                let _ = tokio::time::timeout(spec.cancellation_grace, grace).await;
            }
        }
    }
    cleanup(transport.as_mut()).await;
    client.result.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    client.result
}

struct Client {
    reader: BufReader<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>>,
    pending_frame: Vec<u8>,
    writer: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    next_id: u64,
    session: Option<String>,
    result: RunResult,
    transcript_bytes: usize,
    spec: LaunchSpec,
    calls: HashMap<String, (String, Value)>,
    secrets: Vec<String>,
}

impl Client {
    async fn send(&mut self, value: Value) -> Result<(), Failure> {
        let mut bytes = serde_json::to_vec(&value).map_err(protocol)?;
        if bytes.len() > self.spec.max_frame_bytes {
            return Err((
                Completion::ProtocolError,
                "outgoing ACP frame exceeds limit".into(),
            ));
        }
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.map_err(protocol)?;
        self.writer.flush().await.map_err(protocol)
    }

    async fn read(&mut self) -> Result<Value, Failure> {
        loop {
            let available = self.reader.fill_buf().await.map_err(protocol)?;
            if available.is_empty() {
                return Err((
                    Completion::ProtocolError,
                    "ACP connection closed before completion".into(),
                ));
            }
            let n = available
                .iter()
                .position(|b| *b == b'\n')
                .map_or(available.len(), |i| i + 1);
            if self.pending_frame.len() + n > self.spec.max_frame_bytes {
                return Err((
                    Completion::ProtocolError,
                    "incoming ACP frame exceeds limit".into(),
                ));
            }
            self.pending_frame.extend_from_slice(&available[..n]);
            self.reader.consume(n);
            if self.pending_frame.last() == Some(&b'\n') {
                break;
            }
        }
        let bytes = std::mem::take(&mut self.pending_frame);
        let value: Value = serde_json::from_slice(&bytes).map_err(protocol)?;
        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err((Completion::ProtocolError, "invalid JSON-RPC version".into()));
        }
        Ok(value)
    }

    async fn rpc(
        &mut self,
        method: &str,
        params: Value,
        observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
    ) -> Result<Value, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let frame = self.read().await?;
            if frame.get("method").is_some() {
                self.handle(frame, observer).await?;
                continue;
            }
            if frame.get("id").and_then(Value::as_u64) != Some(id) {
                return Err((
                    Completion::ProtocolError,
                    "unexpected ACP response ID".into(),
                ));
            }
            if let Some(error) = frame.get("error") {
                return Err((rpc_error_status(error), error.to_string()));
            }
            return frame.get("result").cloned().ok_or_else(|| {
                (
                    Completion::ProtocolError,
                    "response has neither result nor error".into(),
                )
            });
        }
    }

    async fn execute(
        &mut self,
        task: &Task,
        observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
    ) -> Result<(), Failure> {
        self.setup(task).await?;
        self.prompt(task, observer).await
    }

    async fn setup(&mut self, task: &Task) -> Result<(), Failure> {
        let init=self.rpc("initialize",json!({"protocolVersion":1,"clientInfo":{"name":"goose-acp-runner","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"_meta":{"goose":{"customNotifications":true}}}}),None).await?;
        if init["protocolVersion"] != 1 {
            return Err((
                Completion::ProtocolError,
                "unsupported ACP protocol version".into(),
            ));
        }
        let mut recipe = json!({"version":"1.0.0","title":"Harness task","description":"Application-owned bounded task","instructions":task.instructions,"extensions":task.extensions,"settings":{"goose_provider":task.provider,"goose_model":task.model,"max_turns":task.max_turns}});
        if let Some(schema) = &task.output_schema {
            recipe["response"] = json!({"json_schema":schema});
        }
        let encoded = self
            .rpc(
                "_goose/unstable/recipes/encode",
                json!({"recipe":recipe}),
                None,
            )
            .await?;
        let deeplink = encoded["deeplink"].as_str().ok_or_else(|| {
            (
                Completion::ProtocolError,
                "recipe encoding returned no deeplink".into(),
            )
        })?;
        let session = self
            .rpc(
                "session/new",
                json!({"cwd":task.cwd,"mcpServers":[],"_meta":{"recipeDeeplink":deeplink}}),
                None,
            )
            .await?;
        self.session = Some(
            session["sessionId"]
                .as_str()
                .ok_or_else(|| {
                    (
                        Completion::ProtocolError,
                        "session/new returned no sessionId".into(),
                    )
                })?
                .to_owned(),
        );
        Ok(())
    }

    async fn prompt(
        &mut self,
        task: &Task,
        observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
    ) -> Result<(), Failure> {
        let validator = task
            .output_schema
            .as_ref()
            .map(jsonschema::validator_for)
            .transpose()
            .map_err(|e| {
                (
                    Completion::InvalidOutput,
                    format!("invalid output schema: {e}"),
                )
            })?;
        let response = self
            .rpc(
                "session/prompt",
                json!({"sessionId":self.session,"prompt":[{"type":"text","text":task.prompt}]}),
                observer,
            )
            .await?;
        if let Some(usage) = response.get("usage").filter(|u| !u.is_null()) {
            self.result.usage.get_or_insert_with(|| json!({}))["promptUsage"] = self.scrub(usage);
        }
        match response["stopReason"].as_str() {
            Some("end_turn") => {}
            Some("max_turn_requests") => {
                return Err((Completion::TurnLimit, "agent reached its turn limit".into()));
            }
            Some("cancelled") => {
                return Err((Completion::Cancelled, "agent cancelled turn".into()));
            }
            reason => {
                return Err((
                    Completion::ProviderError,
                    format!("agent stopped without completion: {reason:?}"),
                ));
            }
        }
        if let Some(validator) = validator {
            let output = self.result.output.as_ref().ok_or_else(|| {
                (
                    Completion::InvalidOutput,
                    "turn ended without accepted final_output".into(),
                )
            })?;
            if !validator.is_valid(output) {
                return Err((
                    Completion::InvalidOutput,
                    "accepted output failed host schema validation".into(),
                ));
            }
            self.result.text = output.to_string();
        } else if self.result.text.trim().is_empty() && self.result.output.is_none() {
            return Err((
                Completion::InvalidOutput,
                "turn ended without final answer".into(),
            ));
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        frame: Value,
        observer: Option<&(dyn Fn(&Value) + Send + Sync)>,
    ) -> Result<(), Failure> {
        let method = frame["method"].as_str().unwrap_or("");
        if let Some(id) = frame.get("id") {
            let response = if method == "session/request_permission"
                || method == "session/requestPermission"
            {
                json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"cancelled"}}})
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"client capability is disabled"}})
            };
            self.send(response).await?;
            return Ok(());
        }
        if method != "session/update" && method != "_goose/unstable/session/update" {
            return Ok(());
        }
        let params = &frame["params"];
        if params["sessionId"].as_str() != self.session.as_deref() {
            return Err((
                Completion::ProtocolError,
                "update belongs to another session".into(),
            ));
        }
        let update = &params["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        if kind == "agent_thought_chunk" {
            return Ok(());
        }
        // Keep only visible task evidence; never arbitrary provider metadata.
        if !matches!(
            kind,
            "agent_message_chunk" | "tool_call" | "tool_call_update" | "usage_update"
        ) {
            return Ok(());
        }
        let sanitized = self.scrub(update);
        let size = sanitized.to_string().len();
        if self.transcript_bytes.saturating_add(size) > self.spec.max_transcript_bytes {
            return Err((
                Completion::ProtocolError,
                "ACP transcript exceeds limit".into(),
            ));
        }
        self.transcript_bytes += size;
        self.result.transcript.push(sanitized.clone());
        if let Some(observer) = observer {
            observer(&sanitized);
        }
        if kind == "agent_message_chunk"
            && sanitized["content"]["type"] == "text"
            && let Some(text) = sanitized["content"]["text"].as_str()
        {
            self.result.text.push_str(text);
        }
        if kind == "usage_update" {
            let usage = self.result.usage.get_or_insert_with(|| json!({}));
            if let Some(fields) = sanitized.as_object() {
                for (k, v) in fields {
                    usage[k] = v.clone();
                }
            }
        }
        if matches!(kind, "tool_call" | "tool_call_update") {
            let id = update["toolCallId"].as_str().ok_or_else(|| {
                (
                    Completion::ProtocolError,
                    "tool update missing call ID".into(),
                )
            })?;
            let name = update
                .pointer("/_meta/goose/toolCall/toolName")
                .and_then(Value::as_str)
                .or_else(|| update.pointer("/_meta/toolName").and_then(Value::as_str));
            if let Some(name) = name {
                self.calls.insert(
                    id.to_owned(),
                    (
                        name.to_owned(),
                        update.get("rawInput").cloned().unwrap_or(Value::Null),
                    ),
                );
            } else if let Some(input) = update.get("rawInput")
                && let Some(call) = self.calls.get_mut(id)
            {
                call.1 = input.clone();
            }
            if update["status"] == "completed"
                && let Some((name, args)) = self.calls.get(id)
                && name == "final_output"
            {
                self.result.output = Some(args.clone());
            }
        }
        Ok(())
    }

    fn scrub(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(
                self.secrets
                    .iter()
                    .fold(text.clone(), |s, secret| s.replace(secret, "[REDACTED]")),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.scrub(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| {
                        !matches!(
                            k.as_str(),
                            "reasoning"
                                | "thinking"
                                | "signature"
                                | "authorization"
                                | "headers"
                                | "envs"
                                | "env"
                        )
                    })
                    .map(|(k, v)| (k.clone(), self.scrub(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn protocol(error: impl std::fmt::Display) -> Failure {
    (Completion::ProtocolError, error.to_string())
}

fn secrets(spec: &LaunchSpec, task: &Task) -> Vec<String> {
    let declared: Vec<&str> = spec
        .provider_definitions
        .iter()
        .filter_map(|v| v["api_key_env"].as_str())
        .collect();
    let mut secrets: Vec<String> = spec
        .env
        .iter()
        .filter(|(k, _)| {
            k.to_str().is_some_and(|name| {
                let upper = name.to_ascii_uppercase();
                declared.contains(&name)
                    || [
                        "KEY",
                        "TOKEN",
                        "SECRET",
                        "PASSWORD",
                        "CREDENTIAL",
                        "AUTHORIZATION",
                    ]
                    .iter()
                    .any(|word| upper.contains(word))
            })
        })
        .filter_map(|(_, v)| v.to_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    for extension in &task.extensions {
        for field in ["headers", "envs"] {
            if let Some(values) = extension[field].as_object() {
                secrets.extend(
                    values
                        .values()
                        .filter_map(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                );
            }
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    secrets
}

async fn cleanup(transport: &mut dyn crate::Transport) {
    let _ = tokio::time::timeout(Duration::from_secs(2), transport.shutdown()).await;
}

// Goose bf074d6 reports action exhaustion as a generic turn_failed RPC error.
// Match its exact envelope and message rather than classifying arbitrary model
// text or other failures containing "limit" as exhausted task policy.
fn rpc_error_status(error: &Value) -> Completion {
    const MAX_TURNS_MESSAGE: &str = "I've reached the maximum number of actions I can do without user input. Would you like me to continue?";
    if error["code"] == -32603
        && error["data"]["reason"] == "turn_failed"
        && error["message"] == MAX_TURNS_MESSAGE
    {
        Completion::TurnLimit
    } else {
        Completion::ProviderError
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn turn_limit_requires_the_pinned_goose_error_contract() {
        let mut error = json!({"code":-32603,"message":"I've reached the maximum number of actions I can do without user input. Would you like me to continue?","data":{"reason":"turn_failed"}});
        assert_eq!(rpc_error_status(&error), Completion::TurnLimit);
        error["data"]["reason"] = json!("provider_error");
        assert_eq!(rpc_error_status(&error), Completion::ProviderError);
        error["data"]["reason"] = json!("turn_failed");
        error["message"] = json!("Provider rate limit exceeded");
        assert_eq!(rpc_error_status(&error), Completion::ProviderError);
    }
}
