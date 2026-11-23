//! Persistent ACP conversations for review phases with host-owned tools.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use goose_acp_runner::{
    RunResult, Session, Task,
    mcp::{McpBridge, ToolHandler},
};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use tokio_util::sync::CancellationToken;

use super::{
    agent_loop::{HostTools, UsageLog, launch_spec},
    lanes::AcpUsage,
};

type Tools = Arc<Mutex<Vec<Arc<dyn HostTools>>>>;

pub(super) struct AcpLoop {
    command: PathBuf,
    provider: String,
    model: String,
    instructions: String,
    tools: Tools,
    max_turns: u32,
    timeout: Duration,
    state: tokio::sync::Mutex<Option<Running>>,
}

struct Running {
    session: Session,
    _bridge: McpBridge,
    _scratch: tempfile::TempDir,
    usage_slot: usize,
    observed: Mutex<AcpUsage>,
}

impl AcpLoop {
    pub fn new(
        command: &Path,
        provider: &str,
        model: &str,
        instructions: String,
        tools: Tools,
        max_turns: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            command: command.to_path_buf(),
            provider: provider.into(),
            model: model.into(),
            instructions,
            tools,
            max_turns,
            timeout,
            state: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn reply(
        &self,
        prompt: &str,
        cancel: &CancellationToken,
        usage: &UsageLog,
    ) -> Result<RunResult> {
        let mut state = self.state.lock().await;
        let mut running = if let Some(running) = state.take() {
            running
        } else {
            let bridge = McpBridge::start(Arc::new(CurrentTools(self.tools.clone())))
                .await
                .map_err(|error| anyhow::anyhow!("Starting review MCP bridge: {error}"))?;
            let scratch = tempfile::Builder::new()
                .prefix("fiach-acp-review-")
                .tempdir()?;
            let launch = launch_spec(&self.command, &self.provider)?;
            let task = Task {
                prompt: String::new(),
                instructions: self.instructions.clone(),
                provider: self.provider.clone(),
                model: self.model.clone(),
                cwd: scratch.path().to_path_buf(),
                output_schema: None,
                extensions: vec![bridge.extension("review")],
                timeout: self.timeout,
                max_turns: self.max_turns,
            };
            let session = match Session::connect(&launch, &task, cancel.child_token()).await {
                Ok(session) => session,
                Err(result) => {
                    return Ok(result);
                }
            };
            Running {
                session,
                _bridge: bridge,
                _scratch: scratch,
                usage_slot: usage.reserve(&self.model),
                observed: Mutex::new(AcpUsage::default()),
            }
        };
        let Running {
            session,
            observed,
            usage_slot,
            ..
        } = &mut running;
        let observed: &Mutex<AcpUsage> = observed;
        let observe = |update: &serde_json::Value| {
            let mut observed = observed.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(current) = observed.observe(update, &self.model) {
                usage.update(*usage_slot, current);
            }
        };
        let result = session
            .prompt(prompt, cancel.child_token(), Some(&observe))
            .await;
        if let Some(current) = &result.usage {
            observe(current);
        }
        if result.status == goose_acp_runner::Completion::Completed {
            *state = Some(running);
        }
        Ok(result)
    }
}

/// The MCP catalog is fixed at connection time, but dispatch always checks the
/// current tool set. A stale advertised tool cannot survive phase narrowing.
struct CurrentTools(Tools);

#[async_trait::async_trait]
impl ToolHandler for CurrentTools {
    fn tools(&self) -> Vec<Tool> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .flat_map(|tools| tools.tools())
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        cancel: CancellationToken,
    ) -> CallToolResult {
        let handler = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|handler| handler.tools().iter().any(|tool| tool.name == name))
            .cloned();
        match handler {
            Some(handler) => handler.call(name, arguments, cancel).await,
            None => CallToolResult::error(vec![ContentBlock::text(
                "Tool is no longer permitted for this phase",
            )]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl HostTools for Probe {
        fn tools(&self) -> Vec<Tool> {
            vec![Tool::new(
                "inspect",
                "read",
                rmcp::object!({"type":"object"}),
            )]
        }
        async fn call(
            &self,
            _: &str,
            _: Option<JsonObject>,
            _: CancellationToken,
        ) -> CallToolResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            CallToolResult::success(vec![ContentBlock::text("inspected")])
        }
    }

    #[tokio::test]
    async fn narrowing_revokes_an_already_advertised_tool_at_dispatch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry: Tools = Arc::new(Mutex::new(vec![Arc::new(Probe(calls.clone()))]));
        let tools = CurrentTools(registry.clone());
        assert_eq!(tools.tools().len(), 1);
        let first = tools.call("inspect", None, CancellationToken::new()).await;
        assert_ne!(first.is_error, Some(true));
        registry.lock().unwrap().clear();
        let denied = tools.call("inspect", None, CancellationToken::new()).await;
        assert_eq!(denied.is_error, Some(true));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
