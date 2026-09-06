use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct FiachConfig {
    pub daemon: Option<DaemonConfig>,
    pub review: Option<ReviewConfig>,
    #[serde(default)]
    pub context_groups: HashMap<String, ContextGroup>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextGroup {
    pub repos: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum MultiString {
    Single(String),
    List(Vec<String>),
}

impl MultiString {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::Single(s) => s
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Self::List(l) => l.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct DaemonConfig {
    pub repos: Option<Vec<String>>,
    pub port: Option<u16>,
    pub interval: Option<u64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub dedupe_existing_comments: Option<bool>,
    pub with_skill: Option<String>,
    pub persona: Option<MultiString>,
    pub personas: Option<MultiString>,
    pub review_lanes: Option<MultiString>,
    #[serde(default)]
    pub review_lane_prompts: HashMap<String, String>,
    pub max_review_lanes: Option<usize>,
    pub max_turns: Option<u32>,
    pub timeout_mins: Option<u64>,
    pub db_path: Option<PathBuf>,
    pub max_retries: Option<u32>,
    pub retry_delay_secs: Option<u64>,
    pub out_dir: Option<PathBuf>,
    pub report_mode: Option<String>,
    pub sync_repo: Option<String>,
    pub notify_on_empty: Option<bool>,
    pub review_start_reaction: Option<String>,
    pub no_findings_reaction: Option<String>,
    pub verify_findings: Option<bool>,
    pub pr_state: Option<MultiString>,
    pub skip_prs: Option<Vec<String>>,
    pub allowed_author_associations: Option<Vec<String>>,
    pub max_workers: Option<usize>,
    pub drafts: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    pub updated_within_days: Option<u32>,
    pub filter_by_updated: Option<bool>,
    pub trigger_mention: Option<String>,
    pub allowed_mention_users: Option<Vec<String>>,
    pub pr_limit: Option<u32>,
    pub buzz: Option<BuzzConfig>,
    // Sandbox options
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ReviewConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub dedupe_existing_comments: Option<bool>,
    pub output: Option<PathBuf>,
    pub with_skill: Option<String>,
    pub persona: Option<MultiString>,
    pub personas: Option<MultiString>,
    pub review_lanes: Option<MultiString>,
    #[serde(default)]
    pub review_lane_prompts: HashMap<String, String>,
    pub max_review_lanes: Option<usize>,
    pub max_turns: Option<u32>,
    pub timeout_mins: Option<u64>,
    pub db_path: Option<PathBuf>,
    pub force: Option<bool>,
    pub max_retries: Option<u32>,
    pub retry_delay_secs: Option<u64>,
    pub report_mode: Option<String>,
    pub sync_repo: Option<String>,
    pub notify_on_empty: Option<bool>,
    pub review_start_reaction: Option<String>,
    pub no_findings_reaction: Option<String>,
    pub verify_findings: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    pub buzz: Option<BuzzConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct BuzzConfig {
    /// Buzz relay URL. When omitted, `BUZZ_RELAY_URL` or the local relay
    /// default is used.
    pub relay_url: Option<String>,
    /// Channel UUID for non-security PR summary threads.
    pub public_channel: Option<String>,
    /// Private channel UUID for verified security finding threads.
    pub security_channel: Option<String>,
    /// Environment variable containing the Buzz reviewer private key.
    #[serde(default = "default_buzz_private_key_env")]
    pub private_key_env: String,
    /// Optional environment variable containing a NIP-OA auth tag.
    pub auth_tag_env: Option<String>,
    /// Optional inbound review-question listener. It uses the same Buzz
    /// identity as outbound summaries and findings.
    pub questions: Option<BuzzQuestionsConfig>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct BuzzQuestionsConfig {
    /// Whether Fiach should answer cryptographically tagged questions in
    /// review threads.
    #[serde(default)]
    pub enabled: bool,
    /// Optional provider override. Defaults to the daemon finder provider.
    pub provider: Option<String>,
    /// Optional model override. Defaults to the daemon finder model.
    pub model: Option<String>,
    /// Buzz author public keys allowed to ask questions. An empty list allows
    /// any member who can post in the configured channel.
    #[serde(default)]
    pub allowed_pubkeys: Vec<String>,
    /// Maximum accepted UTF-8 question size.
    #[serde(default = "default_buzz_question_bytes")]
    pub max_question_bytes: usize,
    /// Maximum duration of one model request.
    #[serde(default = "default_buzz_question_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_buzz_private_key_env() -> String {
    "FIACH_BUZZ_PRIVATE_KEY".to_string()
}

fn default_buzz_question_bytes() -> usize {
    4 * 1024
}

fn default_buzz_question_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub provider: String,
    pub model: String,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub max_turns: u32,
    pub timeout_mins: u64,
    pub verify_findings: bool,
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclosureConfig {
    pub mode: String,
    pub sync_repo: Option<String>,
    pub notify_on_empty: bool,
    pub review_start_reaction: Option<String>,
    pub no_findings_reaction: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub max_workers: usize,
    pub queue_capacity: usize,
    pub terminal_job_limit: usize,
}

impl SchedulerConfig {
    pub fn new(max_workers: usize) -> Self {
        Self {
            max_workers,
            queue_capacity: max_workers.saturating_mul(2).max(16),
            terminal_job_limit: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub rootfs: Option<PathBuf>,
    pub network: String,
    pub extra_args: Vec<String>,
}

impl SandboxConfig {
    pub fn new(
        rootfs: Option<PathBuf>,
        network: Option<String>,
        extra_args: Option<Vec<String>>,
    ) -> Result<Self> {
        let network = network.unwrap_or_else(|| {
            if rootfs.is_some() {
                "veth".to_string()
            } else {
                "host".to_string()
            }
        });
        if !matches!(network.as_str(), "host" | "bridge" | "private" | "veth") {
            anyhow::bail!("sandbox network must be host, bridge, private, or veth");
        }
        Ok(Self {
            rootfs,
            network,
            extra_args: extra_args.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    pub database: PathBuf,
    pub reports: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    pub agent: AgentConfig,
    pub retry: RetryPolicy,
    pub disclosure: DisclosureConfig,
    pub scheduler: SchedulerConfig,
    pub sandbox: SandboxConfig,
    pub storage: StorageConfig,
}

impl RuntimeConfig {
    pub fn resolve_daemon(raw: &DaemonConfig) -> Result<Self> {
        let max_workers = raw.max_workers.unwrap_or(1);
        let sandbox = SandboxConfig::new(
            raw.sandbox_rootfs.clone(),
            raw.sandbox_network.clone(),
            raw.sandbox_extra_args.clone(),
        )?;
        if sandbox.rootfs.is_some()
            && sandbox.network == "veth"
            && (max_workers == 0 || max_workers > 254)
        {
            anyhow::bail!("veth sandboxing requires max_workers between 1 and 254");
        }
        Ok(Self {
            agent: AgentConfig {
                provider: raw
                    .provider
                    .clone()
                    .unwrap_or_else(|| "openrouter".to_string()),
                model: raw
                    .model
                    .clone()
                    .unwrap_or_else(|| "google/gemini-3.1-pro-preview".to_string()),
                verifier_provider: raw.verifier_provider.clone(),
                verifier_model: raw.verifier_model.clone(),
                max_turns: raw.max_turns.unwrap_or(60),
                timeout_mins: raw.timeout_mins.unwrap_or(30),
                verify_findings: raw.verify_findings.unwrap_or(true),
                max_cost_usd: raw.max_cost_usd,
            },
            retry: RetryPolicy {
                max_retries: raw.max_retries.unwrap_or(3),
                initial_delay_secs: raw.retry_delay_secs.unwrap_or(10),
            },
            disclosure: DisclosureConfig {
                mode: raw
                    .report_mode
                    .clone()
                    .unwrap_or_else(|| "local".to_string()),
                sync_repo: raw.sync_repo.clone(),
                notify_on_empty: raw.notify_on_empty.unwrap_or(false),
                review_start_reaction: raw.review_start_reaction.clone(),
                no_findings_reaction: raw.no_findings_reaction.clone(),
            },
            scheduler: SchedulerConfig::new(max_workers),
            sandbox,
            storage: StorageConfig {
                database: raw
                    .db_path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("fiach.redb")),
                reports: raw
                    .out_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("reports")),
            },
        })
    }
}

impl FiachConfig {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut builder = config::Config::builder();
        if let Some(p) = path {
            builder = builder.add_source(config::File::from(p));
        } else {
            // Also look for a default fiach.toml in current dir
            let default_path = std::env::current_dir()?.join("fiach.toml");
            if default_path.exists() {
                builder = builder.add_source(config::File::from(default_path));
            }
        }

        let config = builder.build()?;
        let fiach_config: FiachConfig = config.try_deserialize()?;
        Ok(fiach_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_loads_custom_review_lane_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fiach.toml");
        std::fs::write(
            &path,
            r#"
[review]
review_lanes = ["security", "cashu-mint"]

[review.review_lane_prompts]
cashu-mint = "Focus on mint quote idempotency."
"#,
        )
        .unwrap();

        let config = FiachConfig::load(Some(&path)).unwrap();
        let review = config.review.unwrap();

        assert_eq!(
            review.review_lanes.unwrap().to_vec(),
            vec!["security", "cashu-mint"]
        );
        assert_eq!(
            review
                .review_lane_prompts
                .get("cashu-mint")
                .map(String::as_str),
            Some("Focus on mint quote idempotency.")
        );
    }

    #[test]
    fn config_loads_separate_public_and_security_buzz_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fiach.toml");
        std::fs::write(
            &path,
            r#"
[daemon.buzz]
relay_url = "https://buzz.example.com"
public_channel = "00000000-0000-0000-0000-000000000001"
security_channel = "00000000-0000-0000-0000-000000000002"

[daemon.buzz.questions]
enabled = true
provider = "openrouter"
model = "openai/gpt-5-mini"
allowed_pubkeys = ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
"#,
        )
        .unwrap();

        let config = FiachConfig::load(Some(&path)).unwrap();
        let buzz = config.daemon.unwrap().buzz.unwrap();
        assert_eq!(
            buzz.public_channel.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
        assert_eq!(
            buzz.security_channel.as_deref(),
            Some("00000000-0000-0000-0000-000000000002")
        );
        assert_eq!(buzz.private_key_env, "FIACH_BUZZ_PRIVATE_KEY");
        let questions = buzz.questions.unwrap();
        assert!(questions.enabled);
        assert_eq!(questions.provider.as_deref(), Some("openrouter"));
        assert_eq!(questions.model.as_deref(), Some("openai/gpt-5-mini"));
        assert_eq!(questions.max_question_bytes, 4 * 1024);
        assert_eq!(questions.timeout_secs, 120);
        assert_eq!(questions.allowed_pubkeys.len(), 1);
    }

    #[test]
    fn runtime_groups_apply_defaults_once() {
        let runtime = RuntimeConfig::resolve_daemon(&DaemonConfig::default()).unwrap();
        assert_eq!(runtime.agent.provider, "openrouter");
        assert_eq!(runtime.retry.max_retries, 3);
        assert_eq!(runtime.scheduler.queue_capacity, 16);
        assert_eq!(runtime.storage.database, PathBuf::from("fiach.redb"));
        assert_eq!(runtime.sandbox.network, "host");
    }

    #[test]
    fn sandboxed_runtime_defaults_to_veth_networking() {
        let raw = DaemonConfig {
            sandbox_rootfs: Some(PathBuf::from("/sandbox")),
            ..DaemonConfig::default()
        };
        let runtime = RuntimeConfig::resolve_daemon(&raw).unwrap();
        assert_eq!(runtime.sandbox.network, "veth");
    }

    #[test]
    fn runtime_groups_reject_invalid_veth_worker_limit() {
        let raw = DaemonConfig {
            max_workers: Some(0),
            sandbox_rootfs: Some(PathBuf::from("/sandbox")),
            sandbox_network: Some("veth".to_string()),
            ..DaemonConfig::default()
        };
        assert!(RuntimeConfig::resolve_daemon(&raw).is_err());
    }
}
