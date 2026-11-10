//! Fiach's agent loop, assembled from GDK operations.
//!
//! The host owns every tool: they run in this process, see host state directly,
//! and are never resolved from persisted extension configuration. The session
//! lives in memory for one phase; Fiach persists only its own artifacts.

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use goose::{
    conversation::{
        Conversation, EffectiveRole,
        message::{ErrorContent, Message, MessageContent, MessageErrorKind},
        token_usage::ProviderUsage,
    },
    providers::base::Provider,
};
use goose_agent::{
    events::AgentEvent,
    inference::{InferenceEffect, InferenceRunner},
    machine::{EffectHandler, MachineSession, SessionLoader, StateMachine, Step},
    operation::{
        ConversationEffect, Emitter, MachineEffect, Operation, OperationResult, applied,
        assistant_turn_count, last_effective_role, messages_since_kickoff, not_applicable, yielded,
    },
    tool::{ToolOperation, ToolProvider},
};
use goose_providers::model::ModelConfig;
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
    config: LoopConfig,
    conversation: Mutex<Conversation>,
    usage: Mutex<Vec<ProviderUsage>>,
    turn_limit_reached: Arc<AtomicBool>,
}

impl AgentLoop {
    pub fn new(id: impl Into<String>, config: LoopConfig) -> Self {
        Self {
            id: id.into(),
            config,
            conversation: Mutex::new(Conversation::empty()),
            usage: Mutex::new(Vec::new()),
            turn_limit_reached: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Provider usage recorded so far, one entry per model call.
    pub fn usage(&self) -> Vec<ProviderUsage> {
        lock(&self.usage).clone()
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

        let tools = self
            .config
            .tools
            .iter()
            .fold(ToolOperation::new(), |op, tools| {
                op.with_provider(Arc::new(HostToolProvider(tools.clone())))
            });
        let known: HashSet<String> = self
            .config
            .tools
            .iter()
            .flat_map(|tools| tools.tools())
            .map(|tool| tool.name.to_string())
            .collect();
        let machine = StateMachine::new(
            vec![
                Step::Operation(Arc::new(SystemPrompt(self.system_prompt()))),
                Step::Operation(Arc::new(tools)),
                Step::Operation(Arc::new(UnknownTools(known))),
                Step::Operation(Arc::new(TurnLimit {
                    max_turns: self.config.max_turns,
                    reached: self.turn_limit_reached.clone(),
                })),
                Step::Inference(Arc::new(InferenceRunner::new(
                    self.config.provider.clone(),
                    self.config.model_config.clone(),
                ))),
            ],
            cancel.clone(),
        );

        // The emitter applies backpressure, so drain events while the machine runs.
        let (tx, mut rx) = mpsc::channel(64);
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

        let conversation = lock(&self.conversation);
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

    fn system_prompt(&self) -> String {
        std::iter::once(self.config.system_prompt.clone())
            .chain(
                self.config
                    .tools
                    .iter()
                    .filter_map(|tools| tools.instructions()),
            )
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
                LoopEffect::Usage(usage) => lock(&self.usage).push(usage.clone()),
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
        applied([LoopEffect::from(emit.message(response).await)])
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use futures::stream;
    use goose::{conversation::message::MessageContent, providers::base::MessageStream};
    use goose_providers::errors::ProviderError;
    use rmcp::model::Role;

    /// Replays one scripted response per model call.
    struct Scripted {
        responses: Mutex<std::collections::VecDeque<Result<Message, ProviderError>>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl Scripted {
        fn new(responses: Vec<Result<Message, ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
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
            Ok(Box::pin(stream::iter([Ok((Some(message), None))])))
        }
    }

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
