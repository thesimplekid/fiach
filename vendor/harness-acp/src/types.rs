use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Exact executable and explicitly inherited environment for one task.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    /// Declarative Goose provider JSON files, copied into private task config.
    pub provider_definitions: Vec<Value>,
    pub max_frame_bytes: usize,
    pub max_transcript_bytes: usize,
    pub cancellation_grace: Duration,
}

impl LaunchSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: vec!["acp".into()],
            env: BTreeMap::new(),
            provider_definitions: vec![],
            max_frame_bytes: 4 * 1024 * 1024,
            max_transcript_bytes: 16 * 1024 * 1024,
            cancellation_grace: Duration::from_secs(2),
        }
    }
}

/// One independent conversation. Reusing application data never replays effects.
#[derive(Debug, Clone)]
pub struct Task {
    pub prompt: String,
    pub instructions: String,
    pub provider: String,
    pub model: String,
    pub cwd: PathBuf,
    pub output_schema: Option<Value>,
    /// Explicit Goose recipe extensions, including MCP endpoints and allowlists.
    pub extensions: Vec<Value>,
    pub timeout: Duration,
    /// Maximum autonomous actions per prompt. Must be positive.
    pub max_turns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Completed,
    Cancelled,
    TimedOut,
    /// The agent exhausted its configured autonomous-action budget.
    TurnLimit,
    InvalidOutput,
    ProviderError,
    ProtocolError,
    LaunchError,
}

/// Failure retains the bounded evidence collected before it occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub status: Completion,
    pub text: String,
    pub output: Option<Value>,
    /// Provider/ACP-reported usage, absent when not reported; never assumed zero.
    pub usage: Option<Value>,
    pub transcript: Vec<Value>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

impl Default for RunResult {
    fn default() -> Self {
        Self {
            status: Completion::ProtocolError,
            text: String::new(),
            output: None,
            usage: None,
            transcript: vec![],
            error: None,
            duration_ms: 0,
        }
    }
}
