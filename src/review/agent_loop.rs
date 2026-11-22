//! ACP conversations and host-owned tool interfaces.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use goose::agents::{platform_extensions::PlatformExtension, tool_execution::ToolCallContext};
use goose_providers::conversation::{message::Message, token_usage::ProviderUsage};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use tokio_util::sync::CancellationToken;

/// Tools served by the host process.
#[async_trait::async_trait]
pub(crate) trait HostTools: Send + Sync {
    fn tools(&self) -> Vec<Tool>;

    /// Guidance appended to the system prompt.
    fn instructions(&self) -> Option<String> {
        None
    }

    async fn call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        cancel: CancellationToken,
    ) -> CallToolResult;
}

/// Provider usage, one cumulative entry per ACP session. Clones share the log, so
/// several sessions can account into one budget.
#[derive(Clone, Default)]
pub(crate) struct UsageLog(Arc<Mutex<Vec<ProviderUsage>>>);

impl UsageLog {
    pub fn snapshot(&self) -> Vec<ProviderUsage> {
        lock(&self.0).clone()
    }

    /// Reserve a cumulative ACP session entry. Updating it avoids counting
    /// repeated notifications or a late provider charge more than once.
    pub(super) fn reserve(&self, model: &str) -> usize {
        let mut entries = lock(&self.0);
        let index = entries.len();
        entries.push(ProviderUsage::new(model.to_owned(), Default::default()));
        index
    }

    pub(super) fn update(&self, index: usize, usage: ProviderUsage) {
        if let Some(entry) = lock(&self.0).get_mut(index) {
            *entry = usage;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReplyEnd {
    /// The model ended its turn.
    Finished,
    /// The turn limit stopped the reply before another model call.
    TurnLimit,
}

pub(crate) struct LoopConfig {
    pub provider: String,
    pub model: String,
    pub command: PathBuf,
    pub timeout: Duration,
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn HostTools>>,
    pub max_turns: u32,
}

pub(crate) struct AgentLoop {
    tools: Arc<Mutex<Vec<Arc<dyn HostTools>>>>,
    acp: super::acp_loop::AcpLoop,
    usage: UsageLog,
    last_text: Mutex<String>,
}
impl AgentLoop {
    pub fn new(_id: impl Into<String>, config: LoopConfig) -> Self {
        let mut instructions = config.system_prompt;
        for tool in &config.tools {
            if let Some(guidance) = tool.instructions() {
                instructions.push('\n');
                instructions.push_str(&guidance);
            }
        }
        let tools = Arc::new(Mutex::new(config.tools));
        let acp = super::acp_loop::AcpLoop::new(
            &config.command,
            &config.provider,
            &config.model,
            instructions,
            tools.clone(),
            config.max_turns,
            config.timeout,
        );
        Self {
            tools,
            acp,
            usage: UsageLog::default(),
            last_text: Mutex::new(String::new()),
        }
    }
    pub fn set_tools(&self, tools: Vec<Arc<dyn HostTools>>) {
        *lock(&self.tools) = tools;
    }
    pub fn usage(&self) -> Vec<ProviderUsage> {
        self.usage.snapshot()
    }
    pub fn final_text(&self) -> String {
        lock(&self.last_text).clone()
    }
    pub async fn reply(
        &self,
        message: Message,
        cancel: &CancellationToken,
        mut on_message: impl FnMut(&Message),
    ) -> Result<ReplyEnd> {
        on_message(&message);
        let text = message.as_concat_text();
        let execution = self.acp.reply(&text, cancel, &self.usage);
        tokio::pin!(execution);
        let mut budget_tick = tokio::time::interval(Duration::from_millis(100));
        let result = loop {
            tokio::select! {
                result = &mut execution => break result?,
                _ = budget_tick.tick() => on_message(&Message::assistant()),
            }
        };
        *lock(&self.last_text) = result.text.clone();
        on_message(&Message::assistant().with_text(result.text));
        match result.status {
            goose_acp_runner::Completion::Completed | goose_acp_runner::Completion::Cancelled => {
                Ok(ReplyEnd::Finished)
            }
            goose_acp_runner::Completion::TurnLimit => Ok(ReplyEnd::TurnLimit),
            _ => {
                let failure = anyhow::anyhow!(
                    "ACP session ended and cannot be replayed: {}",
                    result
                        .error
                        .unwrap_or_else(|| format!("{:?}", result.status))
                );
                if super::is_fatal_error(&failure) {
                    Err(super::fatal_provider_error(failure, "ACP review"))
                } else {
                    Err(failure)
                }
            }
        }
    }
}
pub(crate) fn executable(command: Option<&Path>) -> PathBuf {
    command.unwrap_or_else(|| Path::new("goose")).to_path_buf()
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Serves an in-process Goose platform extension, such as the developer tools, as
/// host tools rooted in one working directory.
pub(crate) struct PlatformTools {
    client: Arc<dyn PlatformExtension>,
    tools: Vec<Tool>,
    instructions: Option<String>,
    session_id: String,
    working_dir: PathBuf,
}

impl PlatformTools {
    pub async fn new(
        client: Arc<dyn PlatformExtension>,
        session_id: &str,
        working_dir: PathBuf,
    ) -> Result<Self> {
        let tools = client
            .list_tools(session_id)
            .await
            .map_err(|error| anyhow::anyhow!("Listing tools failed: {error}"))?;
        Ok(Self {
            instructions: client.get_instructions(session_id, &working_dir).await,
            client,
            tools,
            session_id: session_id.to_owned(),
            working_dir,
        })
    }
}

#[async_trait::async_trait]
impl HostTools for PlatformTools {
    fn tools(&self) -> Vec<Tool> {
        self.tools.clone()
    }

    fn instructions(&self) -> Option<String> {
        self.instructions.clone()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        cancel: CancellationToken,
    ) -> CallToolResult {
        let context = ToolCallContext::new(
            self.session_id.clone(),
            Some(self.working_dir.clone()),
            None,
        );
        self.client
            .call_tool(&context, name, arguments, cancel)
            .await
            .unwrap_or_else(|error| {
                CallToolResult::error(vec![ContentBlock::text(format!("Tool failed: {error}"))])
            })
    }
}

/// Retry budget shared by every reply within one agent phase.
pub(crate) struct LoopRetry {
    phase: &'static str,
    max_retries: u32,
    retries: u32,
    delay_secs: u64,
}

impl LoopRetry {
    pub fn new(phase: &'static str, max_retries: u32, delay_secs: u64) -> Self {
        Self {
            phase,
            max_retries,
            retries: 0,
            delay_secs,
        }
    }

    pub fn retries(&self) -> u32 {
        self.retries
    }

    pub fn exhausted(&self) -> bool {
        self.retries >= self.max_retries
    }

    pub async fn backoff(&mut self) {
        tracing::info!(
            phase = self.phase,
            attempt = self.retries + 1,
            max_retries = self.max_retries,
            delay_secs = self.delay_secs,
            "Retrying agent reply"
        );
        tokio::time::sleep(Duration::from_secs(self.delay_secs)).await;
        self.retries += 1;
        self.delay_secs *= 2;
    }
}
