//! Fiach's agent loop, assembled from GDK operations.
//!
//! The host owns every tool: they run in this process, see host state directly,
//! and are never resolved from persisted extension configuration. The session
//! lives in memory for one phase; Fiach persists only its own artifacts.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use goose::agents::{mcp_client::McpClientTrait, tool_execution::ToolCallContext};
use goose_agent::{
    events::AgentEvent,
    inference::{InferenceEffect, InferenceRunner, is_empty_response_marker},
    machine::{EffectHandler, MachineSession, SessionLoader, StateMachine, Step},
    operation::{
        ConversationEffect, Emitter, MachineEffect, Operation, OperationResult, applied,
        assistant_turn_count, last_effective_role, messages_since_kickoff, not_applicable, yielded,
    },
    tool::{ToolOperation, ToolProvider},
};
use goose_providers::{
    base::Provider,
    conversation::{
        Conversation, EffectiveRole,
        message::{ErrorContent, Message, MessageContent, MessageErrorKind},
        token_usage::ProviderUsage,
    },
    model::ModelConfig,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, JsonObject, Tool,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{fatal_provider_error, is_fatal_error};

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

pub(crate) struct LoopConfig {
    pub provider: Arc<dyn Provider>,
    pub model_config: ModelConfig,
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn HostTools>>,
    /// Model turns allowed per reply.
    pub max_turns: u32,
    /// Ends a reply before the next model call once it holds, e.g. after an
    /// authoritative submission.
    pub stop_when: Option<StopCondition>,
}

pub(crate) type StopCondition = Arc<dyn Fn() -> bool + Send + Sync>;

/// Provider usage, one entry per model call. Clones share the same log, so
/// several sessions can account into one budget.
#[derive(Clone, Default)]
pub(crate) struct UsageLog(Arc<Mutex<Vec<ProviderUsage>>>);

impl UsageLog {
    pub fn snapshot(&self) -> Vec<ProviderUsage> {
        lock(&self.0).clone()
    }

    fn push(&self, usage: ProviderUsage) {
        lock(&self.0).push(usage);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReplyEnd {
    /// The model ended its turn.
    Finished,
    /// The turn limit stopped the reply before another model call.
    TurnLimit,
    /// The provider failed; the error is the last message.
    ProviderError(ErrorContent),
}

/// One in-memory agent session.
pub(crate) struct AgentLoop {
    id: String,
    provider: Arc<dyn Provider>,
    model_config: ModelConfig,
    system_prompt: String,
    tools: Mutex<Vec<Arc<dyn HostTools>>>,
    max_turns: AtomicU32,
    stop_when: Option<StopCondition>,
    conversation: Mutex<Conversation>,
    usage: UsageLog,
    turn_limit_reached: Arc<AtomicBool>,
    /// Context size of the latest model call; drives proactive compaction.
    context_tokens: Arc<AtomicU64>,
}

impl AgentLoop {
    pub fn new(id: impl Into<String>, config: LoopConfig) -> Self {
        Self {
            id: id.into(),
            provider: config.provider,
            model_config: config.model_config,
            system_prompt: config.system_prompt,
            tools: Mutex::new(config.tools),
            max_turns: AtomicU32::new(config.max_turns),
            stop_when: config.stop_when,
            conversation: Mutex::new(Conversation::empty()),
            usage: UsageLog::default(),
            turn_limit_reached: Arc::new(AtomicBool::new(false)),
            context_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Replaces the tools offered from the next reply on. Earlier tool calls and
    /// results stay in the conversation.
    pub fn set_tools(&self, tools: Vec<Arc<dyn HostTools>>) {
        *lock(&self.tools) = tools;
    }

    /// Lowers the per-reply turn budget; it is never raised.
    pub fn limit_turns(&self, limit: u32) {
        self.max_turns.fetch_min(limit, Ordering::SeqCst);
    }

    /// Records usage into a shared log instead of a private one.
    pub fn sharing_usage(mut self, usage: UsageLog) -> Self {
        self.usage = usage;
        self
    }

    /// Provider usage recorded so far, one entry per model call.
    pub fn usage(&self) -> Vec<ProviderUsage> {
        self.usage.snapshot()
    }

    /// Text of the assistant's final messages, after the last tool result or
    /// user message.
    pub fn final_text(&self) -> String {
        let conversation = lock(&self.conversation);
        let messages = conversation.messages();
        let start = messages
            .iter()
            .rposition(|message| message.role != rmcp::model::Role::Assistant)
            .map_or(0, |index| index + 1);
        messages[start..]
            .iter()
            .filter(|message| message.error_kind().is_none())
            .map(Message::as_concat_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Sends `message` and runs the loop until the model ends its turn, the turn
    /// limit is reached, the provider fails, or `cancel` fires. `on_message`
    /// sees each message as it is produced.
    pub async fn reply(
        &self,
        message: Message,
        cancel: &CancellationToken,
        mut on_message: impl FnMut(&Message),
    ) -> Result<ReplyEnd> {
        self.drop_trailing_error();
        self.turn_limit_reached.store(false, Ordering::SeqCst);
        on_message(&message);
        lock(&self.conversation).push(message.with_generated_id_if_missing());

        let host_tools = lock(&self.tools).clone();
        let tools = host_tools.iter().fold(ToolOperation::new(), |op, tools| {
            op.with_provider(Arc::new(HostToolProvider(tools.clone())))
        });
        let known: HashSet<String> = host_tools
            .iter()
            .flat_map(|tools| tools.tools())
            .map(|tool| tool.name.to_string())
            .collect();
        let mut steps: Vec<Step<'_, LoopSession, LoopEffect>> = vec![
            Step::Operation(Arc::new(SystemPrompt(self.system_prompt(&host_tools)))),
            Step::Operation(Arc::new(tools)),
            Step::Operation(Arc::new(UnknownTools(known))),
        ];
        if let Some(stop_when) = &self.stop_when {
            steps.push(Step::Operation(Arc::new(StopWhen(stop_when.clone()))));
        }
        steps.extend([
            Step::Operation(Arc::new(TurnLimit {
                max_turns: self.max_turns.load(Ordering::SeqCst),
                reached: self.turn_limit_reached.clone(),
            })) as Step<'_, LoopSession, LoopEffect>,
            Step::Operation(Arc::new(EmptyResponse)),
            Step::Operation(Arc::new(Compaction {
                provider: self.provider.clone(),
                model_config: self.model_config.clone(),
                context_tokens: self.context_tokens.clone(),
                reactive_attempted: AtomicBool::new(false),
            })),
            Step::Inference(Arc::new(InferenceRunner::new(
                self.provider.clone(),
                self.model_config.clone(),
            ))),
        ]);
        let machine = StateMachine::new(steps, cancel.clone());

        // Deliver events while the machine runs, not only once it finishes.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = {
            let emit = Emitter::new(tx, cancel.clone());
            let run = machine.run(self, &self.id, &emit);
            tokio::pin!(run);
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    Some(event) = rx.recv() => {
                        if let AgentEvent::Message(message) = event {
                            on_message(&message);
                        }
                    }
                }
            }
        };
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Message(message) = event {
                on_message(&message);
            }
        }
        result?;

        let mut conversation = lock(&self.conversation);
        // After its own retries, the GDK marks a still-empty response instead of
        // failing; it is a transient failure, not a result.
        if conversation.last().is_some_and(is_empty_response_marker) {
            conversation.pop();
            return Ok(ReplyEnd::ProviderError(ErrorContent {
                kind: MessageErrorKind::Other,
                message: "The model returned an empty response".into(),
            }));
        }
        if let Some(error) = conversation
            .last()
            .and_then(|message| message.content.iter().find_map(MessageContent::as_error))
        {
            return Ok(ReplyEnd::ProviderError(error.clone()));
        }
        Ok(if self.turn_limit_reached.load(Ordering::SeqCst) {
            ReplyEnd::TurnLimit
        } else {
            ReplyEnd::Finished
        })
    }

    fn system_prompt(&self, tools: &[Arc<dyn HostTools>]) -> String {
        std::iter::once(self.system_prompt.clone())
            .chain(tools.iter().filter_map(|tools| tools.instructions()))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// A failed model call ends the turn. Remove it so a retry continues from
    /// the last good message.
    fn drop_trailing_error(&self) {
        let mut conversation = lock(&self.conversation);
        if conversation
            .last()
            .is_some_and(|message| message.error_kind().is_some())
        {
            conversation.pop();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub(crate) struct LoopSession {
    id: String,
    conversation: Conversation,
}

impl MachineSession for LoopSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

pub(crate) enum LoopEffect {
    Conversation(ConversationEffect),
    Usage(ProviderUsage),
    /// Billable usage of a compaction call; it does not measure the context.
    CompactionUsage(ProviderUsage),
}

impl MachineEffect for LoopEffect {
    fn ensure_message_ids(&mut self) {
        if let LoopEffect::Conversation(effect) = self {
            effect.ensure_message_ids();
        }
    }
}

impl From<Message> for LoopEffect {
    fn from(message: Message) -> Self {
        LoopEffect::Conversation(ConversationEffect::AppendMessage(message))
    }
}

impl InferenceEffect for LoopEffect {
    fn record_usage(usage: ProviderUsage) -> Self {
        LoopEffect::Usage(usage)
    }
}

#[async_trait::async_trait]
impl SessionLoader<LoopSession> for AgentLoop {
    async fn load(&self, session_id: &str) -> Result<LoopSession> {
        if session_id != self.id {
            bail!("Unknown agent session {session_id}");
        }
        Ok(LoopSession {
            id: self.id.clone(),
            conversation: lock(&self.conversation).clone(),
        })
    }
}

#[async_trait::async_trait]
impl EffectHandler<LoopSession, LoopEffect> for AgentLoop {
    async fn apply_effects(
        &self,
        _session: &LoopSession,
        effects: &mut [LoopEffect],
        _emit: &Emitter,
    ) -> Result<()> {
        for effect in effects.iter_mut() {
            match effect {
                LoopEffect::Usage(usage) => {
                    let tokens = nonnegative(usage.usage.input_tokens)
                        + nonnegative(usage.usage.output_tokens);
                    self.context_tokens.store(tokens, Ordering::SeqCst);
                    self.usage.push(usage.clone());
                }
                LoopEffect::CompactionUsage(usage) => {
                    self.context_tokens.store(0, Ordering::SeqCst);
                    self.usage.push(usage.clone());
                }
                LoopEffect::Conversation(ConversationEffect::AppendMessage(message)) => {
                    lock(&self.conversation).push(message.clone());
                }
                LoopEffect::Conversation(ConversationEffect::ReplaceConversation(conversation)) => {
                    *lock(&self.conversation) = conversation.clone()
                }
                LoopEffect::Conversation(ConversationEffect::SetMessageVisibility {
                    message_id,
                    user_visible,
                    agent_visible,
                }) => {
                    let mut conversation = lock(&self.conversation);
                    if let Some(message) = conversation
                        .messages_mut()
                        .iter_mut()
                        .find(|message| message.id.as_deref() == Some(message_id.as_str()))
                    {
                        message.metadata.user_visible = *user_visible;
                        message.metadata.agent_visible = *agent_visible;
                    }
                }
                LoopEffect::Conversation(ConversationEffect::SetMessageOperationNote {
                    message_id,
                    operation,
                    key,
                    value,
                }) => {
                    let mut conversation = lock(&self.conversation);
                    if let Some(message) = conversation
                        .messages_mut()
                        .iter_mut()
                        .find(|message| message.id.as_deref() == Some(message_id.as_str()))
                    {
                        message
                            .metadata
                            .set_operation_note(operation, key, value.clone());
                    }
                }
                // Only tool approval patches request metadata, and Fiach
                // approves every host tool.
                LoopEffect::Conversation(ConversationEffect::PatchToolRequestMeta { .. }) => {
                    bail!("Unsupported tool request metadata patch")
                }
            }
        }
        Ok(())
    }
}

struct HostToolProvider(Arc<dyn HostTools>);

#[async_trait::async_trait]
impl ToolProvider<LoopSession> for HostToolProvider {
    async fn tools(&self, _session: &LoopSession) -> Result<Vec<Tool>> {
        Ok(self.0.tools())
    }

    async fn call(
        &self,
        _session: &LoopSession,
        _request_id: &str,
        call: CallToolRequestParams,
        emit: &Emitter,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .0
            .call(&call.name, call.arguments, emit.cancel_token().clone())
            .await)
    }
}

struct SystemPrompt(String);

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for SystemPrompt {
    fn name(&self) -> &'static str {
        "system_prompt"
    }

    async fn prompt_parts(
        &self,
        _session: &LoopSession,
        _conversation: &Conversation,
    ) -> Result<Vec<(String, String)>> {
        Ok(vec![("fiach".into(), self.0.clone())])
    }
}

/// Answers calls to tools no host service provides, so the model can correct
/// itself instead of the turn stalling on an unanswered request.
struct UnknownTools(HashSet<String>);

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for UnknownTools {
    fn name(&self) -> &'static str {
        "unknown_tools"
    }

    async fn run(
        &self,
        _session: &LoopSession,
        conversation: &Conversation,
        emit: &Emitter,
    ) -> Result<OperationResult<LoopEffect>> {
        let turn = messages_since_kickoff(conversation)?;
        let answered: HashSet<&str> = turn
            .iter()
            .flat_map(Message::get_tool_response_ids)
            .collect();
        let mut response = Message::user();
        for request in turn
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(MessageContent::as_tool_request)
            .filter(|request| !answered.contains(request.id.as_str()))
        {
            let Ok(call) = &request.tool_call else {
                continue;
            };
            if self.0.contains(call.name.as_ref()) {
                continue;
            }
            response.add_tool_response_with_metadata(
                request.id.clone(),
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Tool {} is not available",
                    call.name
                ))])),
                request.metadata.as_ref(),
            );
        }
        if response.get_tool_response_ids().is_empty() {
            return not_applicable();
        }
        applied([LoopEffect::from(emit.message(response))])
    }
}

/// Ends the reply once the host's stop condition holds.
struct StopWhen(StopCondition);

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for StopWhen {
    fn name(&self) -> &'static str {
        "stop_when"
    }

    async fn run(
        &self,
        _session: &LoopSession,
        _conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<LoopEffect>> {
        if (self.0)() {
            yielded()
        } else {
            not_applicable()
        }
    }
}

/// Serves an in-process Goose MCP client, such as the developer tools, as
/// host tools rooted in one working directory.
pub(crate) struct McpTools {
    client: Arc<dyn McpClientTrait>,
    tools: Vec<Tool>,
    instructions: Option<String>,
    session_id: String,
    working_dir: PathBuf,
}

impl McpTools {
    pub async fn new(
        client: Arc<dyn McpClientTrait>,
        session_id: &str,
        working_dir: PathBuf,
    ) -> Result<Self> {
        let tools = client
            .list_tools(session_id, None, CancellationToken::new())
            .await
            .map_err(|error| anyhow::anyhow!("Listing tools failed: {error}"))?
            .tools;
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
impl HostTools for McpTools {
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

/// Ends the reply when the model's response stayed empty after the GDK's own
/// retries. Without this, inference would run again immediately.
struct EmptyResponse;

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for EmptyResponse {
    fn name(&self) -> &'static str {
        "empty_response"
    }

    async fn run(
        &self,
        _session: &LoopSession,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<LoopEffect>> {
        if conversation.last().is_some_and(is_empty_response_marker) {
            yielded()
        } else {
            not_applicable()
        }
    }
}

/// Summarizes the conversation before a model call once the previous call used
/// most of the model's context window, or once after a context overflow. The
/// summary replaces the history the model sees; the originals stay recorded.
struct Compaction {
    provider: Arc<dyn Provider>,
    model_config: ModelConfig,
    context_tokens: Arc<AtomicU64>,
    reactive_attempted: AtomicBool,
}

impl Compaction {
    async fn over_threshold(&self) -> bool {
        let tokens = self.context_tokens.load(Ordering::SeqCst);
        if tokens == 0 {
            return false;
        }
        let limit = self
            .provider
            .get_context_limit(&self.model_config.model_name, None)
            .await;
        limit > 0
            && tokens as f64 > limit as f64 * goose_context_management::DEFAULT_COMPACTION_THRESHOLD
    }
}

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for Compaction {
    fn name(&self) -> &'static str {
        "compaction"
    }

    async fn run(
        &self,
        session: &LoopSession,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<LoopEffect>> {
        let turn = messages_since_kickoff(conversation)?;
        let overflowed = conversation.last().and_then(Message::error_kind)
            == Some(MessageErrorKind::ContextLengthExceeded);
        let conversation = if overflowed {
            // One reactive attempt per reply; a second overflow is final.
            if self.reactive_attempted.swap(true, Ordering::SeqCst) {
                return not_applicable();
            }
            let mut messages = conversation.messages().clone();
            messages.pop();
            Conversation::new_unvalidated(messages)
        } else {
            let awaiting_model = matches!(
                last_effective_role(turn)?,
                EffectiveRole::User | EffectiveRole::Tool
            );
            if !awaiting_model || !self.over_threshold().await {
                return not_applicable();
            }
            conversation.clone()
        };
        tracing::info!(
            session_id = %session.id,
            overflowed,
            context_tokens = self.context_tokens.load(Ordering::SeqCst),
            "Compacting agent conversation"
        );
        match goose::context_mgmt::compact_messages(
            self.provider.as_ref(),
            &self.model_config,
            &session.id,
            &conversation,
            false,
        )
        .await
        {
            Ok(result) => applied([
                LoopEffect::Conversation(ConversationEffect::ReplaceConversation(
                    result.conversation,
                )),
                LoopEffect::CompactionUsage(result.usage),
            ]),
            Err(error) => {
                // Continue uncompacted; an overflow then ends the reply as a
                // provider error.
                tracing::warn!(session_id = %session.id, error = %error, "Compaction failed");
                not_applicable()
            }
        }
    }
}

fn nonnegative(tokens: Option<i32>) -> u64 {
    tokens.unwrap_or(0).max(0) as u64
}

/// Stops the reply before a model call once the turn has used its budget.
struct TurnLimit {
    max_turns: u32,
    reached: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Operation<LoopSession, LoopEffect> for TurnLimit {
    fn name(&self) -> &'static str {
        "turn_limit"
    }

    async fn run(
        &self,
        _session: &LoopSession,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<LoopEffect>> {
        let turn = messages_since_kickoff(conversation)?;
        let awaiting_model = matches!(
            last_effective_role(turn)?,
            EffectiveRole::User | EffectiveRole::Tool
        );
        if !awaiting_model || assistant_turn_count(turn) < self.max_turns {
            return not_applicable();
        }
        tracing::warn!(max_turns = self.max_turns, "Agent reached the turn limit");
        self.reached.store(true, Ordering::SeqCst);
        yielded()
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

    /// Fails on permanent provider errors and once the budget is spent;
    /// otherwise returns the error for the follow-up prompt.
    pub fn provider_error(&self, error: &ErrorContent) -> Result<anyhow::Error> {
        let failure = anyhow::anyhow!("{}", error.message);
        let permanent = matches!(
            error.kind,
            MessageErrorKind::Authentication
                | MessageErrorKind::CreditsExhausted
                | MessageErrorKind::ContextLengthExceeded
        );
        if permanent || is_fatal_error(&failure) {
            return Err(fatal_provider_error(failure, self.phase));
        }
        if self.exhausted() {
            return Err(failure.context(format!(
                "{} provider failed after {} retries",
                self.phase, self.retries
            )));
        }
        Ok(failure)
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

/// A provider that replays scripted responses, for driving the loop in tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use futures::stream;
    use goose_providers::{
        base::{MessageStream, Provider},
        conversation::{
            message::Message,
            token_usage::{ProviderUsage, Usage},
        },
        errors::ProviderError,
        model::ModelConfig,
    };
    use rmcp::model::Tool;

    use super::lock;

    /// Replays one scripted response per model call.
    pub(crate) struct Scripted {
        responses: Mutex<std::collections::VecDeque<Result<Message, ProviderError>>>,
        pub calls: Mutex<Vec<(String, Vec<String>)>>,
        /// Input tokens reported for every call, if any.
        input_tokens: Option<i32>,
    }

    impl Scripted {
        pub fn new(responses: Vec<Result<Message, ProviderError>>) -> Arc<Self> {
            Self::reporting(responses, None)
        }

        pub fn reporting(
            responses: Vec<Result<Message, ProviderError>>,
            input_tokens: Option<i32>,
        ) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
                input_tokens,
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn get_name(&self) -> &str {
            "scripted"
        }

        async fn stream(
            &self,
            _model_config: &ModelConfig,
            system: &str,
            _messages: &[Message],
            tools: &[Tool],
        ) -> Result<MessageStream, ProviderError> {
            lock(&self.calls).push((
                system.to_owned(),
                tools.iter().map(|tool| tool.name.to_string()).collect(),
            ));
            let next = lock(&self.responses)
                .pop_front()
                .unwrap_or_else(|| Ok(Message::assistant().with_text("done")));
            let message = next?;
            let usage = self.input_tokens.map(|tokens| {
                ProviderUsage::new(
                    "scripted".into(),
                    Usage::new(Some(tokens), Some(10), Some(tokens + 10)),
                )
            });
            Ok(Box::pin(stream::iter([Ok((Some(message), usage))])))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::Scripted, *};

    use goose_providers::conversation::message::MessageContent;
    use goose_providers::errors::ProviderError;
    use rmcp::model::Role;

    struct Echo(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl HostTools for Echo {
        fn tools(&self) -> Vec<Tool> {
            vec![Tool::new(
                "echo",
                "Echo text",
                Arc::new(serde_json::Map::new()),
            )]
        }

        fn instructions(&self) -> Option<String> {
            Some("Use echo.".into())
        }

        async fn call(
            &self,
            name: &str,
            arguments: Option<JsonObject>,
            _cancel: CancellationToken,
        ) -> CallToolResult {
            lock(&self.0).push(format!("{name}:{}", serde_json::json!(arguments)));
            CallToolResult::success(vec![ContentBlock::text("echoed")])
        }
    }

    fn tool_call(id: &str, name: &str) -> Message {
        Message::assistant().with_tool_request(
            id,
            Ok(CallToolRequestParams::new(name.to_owned()).with_arguments(
                serde_json::json!({"text": "hi"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
        )
    }

    fn agent(provider: Arc<Scripted>, tools: Arc<Echo>, max_turns: u32) -> AgentLoop {
        AgentLoop::new(
            "test",
            LoopConfig {
                provider,
                model_config: ModelConfig::new("scripted"),
                system_prompt: "Policy.".into(),
                tools: vec![tools],
                max_turns,
                stop_when: None,
            },
        )
    }

    fn tool_results(agent: &AgentLoop) -> Vec<String> {
        lock(&agent.conversation)
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(MessageContent::as_tool_response)
            .map(|response| {
                let result = response.tool_result.as_ref().unwrap();
                format!(
                    "{}:{}",
                    result.is_error.unwrap_or(false),
                    result.content[0].as_text().unwrap().text
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn runs_host_tools_until_the_model_finishes() {
        let provider = Scripted::new(vec![
            Ok(tool_call("call-1", "echo")),
            Ok(Message::assistant().with_text("done")),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools.clone(), 10);
        let mut seen = Vec::new();

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |message| seen.push(message.role.clone()),
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(*lock(&tools.0), [r#"echo:{"text":"hi"}"#]);
        assert_eq!(tool_results(&agent), ["false:echoed"]);
        let calls = lock(&provider.calls);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "Policy.\n\nUse echo.");
        assert_eq!(calls[0].1, ["echo"]);
        assert_eq!(
            seen,
            [Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
    }

    #[tokio::test]
    async fn answers_unknown_tools_with_an_error() {
        let provider = Scripted::new(vec![Ok(tool_call("call-1", "shell"))]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools.clone(), 10);

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::Finished);
        assert!(lock(&tools.0).is_empty());
        assert_eq!(tool_results(&agent), ["true:Tool shell is not available"]);
        assert_eq!(lock(&provider.calls).len(), 2);
    }

    #[tokio::test]
    async fn stops_before_a_model_call_past_the_turn_limit() {
        let provider = Scripted::new(vec![
            Ok(tool_call("call-1", "echo")),
            Ok(tool_call("call-2", "echo")),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools, 1);

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::TurnLimit);
        assert_eq!(lock(&provider.calls).len(), 1);
        assert_eq!(tool_results(&agent), ["false:echoed"]);

        // A follow-up starts a new turn with a fresh budget.
        let end = agent
            .reply(
                Message::user().with_text("report"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(end, ReplyEnd::TurnLimit);
        assert_eq!(lock(&provider.calls).len(), 2);
    }

    #[tokio::test]
    async fn provider_errors_end_the_reply_and_are_dropped_before_a_retry() {
        let provider = Scripted::new(vec![
            Err(ProviderError::RateLimitExceeded {
                details: "slow down".into(),
                retry_delay: None,
            }),
            Ok(Message::assistant().with_text("done")),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools, 10);

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        let ReplyEnd::ProviderError(error) = end else {
            panic!("expected a provider error, got {end:?}");
        };
        let retry = LoopRetry::new("test", 1, 0);
        assert!(retry.provider_error(&error).is_ok());

        let end = agent
            .reply(
                Message::user().with_text("continue"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(end, ReplyEnd::Finished);
        assert!(
            lock(&agent.conversation)
                .iter()
                .all(|message| message.error_kind().is_none())
        );
    }

    #[tokio::test]
    async fn stop_condition_ends_the_reply_before_another_model_call() {
        let provider = Scripted::new(vec![
            Ok(tool_call("call-1", "echo")),
            Err(ProviderError::RequestFailed("must not be reached".into())),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let echoes = tools.clone();
        let agent = AgentLoop::new(
            "test",
            LoopConfig {
                provider: provider.clone(),
                model_config: ModelConfig::new("scripted"),
                system_prompt: "Policy.".into(),
                tools: vec![tools],
                max_turns: 10,
                stop_when: Some(Arc::new(move || !lock(&echoes.0).is_empty())),
            },
        );

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(lock(&provider.calls).len(), 1);
        assert_eq!(tool_results(&agent), ["false:echoed"]);
    }

    #[tokio::test]
    async fn swapped_tools_and_lowered_turns_apply_to_later_replies() {
        let provider = Scripted::new(vec![
            Ok(tool_call("call-1", "echo")),
            Ok(Message::assistant().with_text("done")),
            Ok(tool_call("call-2", "echo")),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools.clone(), 10);
        agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        agent.set_tools(Vec::new());
        agent.limit_turns(1);
        agent.limit_turns(5);
        let end = agent
            .reply(
                Message::user().with_text("report"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::TurnLimit);
        assert_eq!(lock(&tools.0).len(), 1);
        assert_eq!(
            tool_results(&agent),
            ["false:echoed", "true:Tool echo is not available"]
        );
        let calls = lock(&provider.calls);
        assert_eq!(calls[2], ("Policy.".to_owned(), Vec::<String>::new()));
    }

    #[tokio::test]
    async fn empty_responses_are_retryable_failures_not_results() {
        // The GDK retries an empty response three times within one call.
        let blank = || Ok(Message::assistant().with_text("  "));
        let provider = Scripted::new(vec![blank(), Ok(Message::assistant().with_text("result"))]);
        let recovered = agent(provider, Arc::new(Echo(Mutex::new(Vec::new()))), 10);
        let end = recovered
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(recovered.final_text(), "result");

        // A response that stays empty is a transient failure, not a result.
        let provider = Scripted::new(vec![
            blank(),
            blank(),
            blank(),
            blank(),
            Ok(Message::assistant().with_text("result")),
        ]);
        let agent = agent(provider, Arc::new(Echo(Mutex::new(Vec::new()))), 10);
        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        let ReplyEnd::ProviderError(error) = &end else {
            panic!("expected a provider error, got {end:?}");
        };
        assert!(LoopRetry::new("test", 1, 0).provider_error(error).is_ok());
        assert_eq!(agent.final_text(), "");

        let end = agent
            .reply(
                Message::user().with_text("again"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(agent.final_text(), "result");
    }

    fn agent_visible_text(agent: &AgentLoop) -> Vec<String> {
        lock(&agent.conversation)
            .iter()
            .filter(|message| message.is_agent_visible())
            .map(Message::as_concat_text)
            .collect()
    }

    #[tokio::test]
    async fn a_full_context_is_compacted_before_the_next_model_call() {
        let provider = Scripted::reporting(
            vec![
                Ok(tool_call("call-1", "echo")),
                Ok(Message::assistant().with_text("SUMMARY OF EARLIER WORK")),
                Ok(Message::assistant().with_text("done")),
            ],
            Some(10_000_000),
        );
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let agent = agent(provider.clone(), tools.clone(), 10);

        let end = agent
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(lock(&tools.0).len(), 1);
        assert_eq!(lock(&provider.calls).len(), 3);
        let visible = agent_visible_text(&agent).join("\n");
        assert!(visible.contains("SUMMARY OF EARLIER WORK"), "{visible}");
        assert!(visible.contains("go"));
        assert!(!visible.contains("echoed"));
        assert_eq!(agent.final_text(), "done");
        // The compaction call is billed like any other.
        assert_eq!(agent.usage().len(), 3);
    }

    #[tokio::test]
    async fn a_context_overflow_is_compacted_and_retried_once() {
        let provider = Scripted::new(vec![
            Err(ProviderError::ContextLengthExceeded("too long".into())),
            Ok(Message::assistant().with_text("SUMMARY")),
            Ok(Message::assistant().with_text("done")),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let recovered = agent(provider.clone(), tools, 10);

        let end = recovered
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(end, ReplyEnd::Finished);
        assert_eq!(recovered.final_text(), "done");

        let provider = Scripted::new(vec![
            Err(ProviderError::ContextLengthExceeded("too long".into())),
            Ok(Message::assistant().with_text("SUMMARY")),
            Err(ProviderError::ContextLengthExceeded(
                "still too long".into(),
            )),
        ]);
        let tools = Arc::new(Echo(Mutex::new(Vec::new())));
        let repeated = agent(provider.clone(), tools, 10);
        let end = repeated
            .reply(
                Message::user().with_text("go"),
                &CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
        let ReplyEnd::ProviderError(error) = end else {
            panic!("expected a final overflow, got {end:?}");
        };
        assert_eq!(error.kind, MessageErrorKind::ContextLengthExceeded);
        assert_eq!(lock(&provider.calls).len(), 3);
    }

    #[test]
    fn permanent_provider_errors_are_fatal_and_budget_is_bounded() {
        let retry = LoopRetry::new("verifier", 0, 0);
        let auth = ErrorContent {
            kind: MessageErrorKind::Authentication,
            message: "bad key".into(),
        };
        let error = retry.provider_error(&auth).unwrap_err();
        assert!(super::super::is_fatal_provider_error(&error));

        let transient = ErrorContent {
            kind: MessageErrorKind::Other,
            message: "connection reset".into(),
        };
        let error = retry.provider_error(&transient).unwrap_err();
        assert!(!super::super::is_fatal_provider_error(&error));
        assert!(format!("{error:#}").contains("after 0 retries"));
    }
}
