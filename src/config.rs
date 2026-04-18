use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

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

#[derive(Debug, Deserialize, Default)]
pub struct DaemonConfig {
    pub repos: Option<Vec<String>>,
    pub interval: Option<u64>,
    pub model: Option<String>,
    pub with_skill: Option<String>,
    pub persona: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout_mins: Option<u64>,
    pub db_path: Option<PathBuf>,
    pub max_retries: Option<u32>,
    pub retry_delay_secs: Option<u64>,
    pub out_dir: Option<PathBuf>,
    pub report_mode: Option<String>,
    pub sync_repo: Option<String>,
    pub notify_on_empty: Option<bool>,
    pub pr_state: Option<MultiString>,
    pub skip_prs: Option<Vec<String>>,
    pub drafts: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
    // Sandbox options
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReviewConfig {
    pub model: Option<String>,
    pub output: Option<PathBuf>,
    pub with_skill: Option<String>,
    pub persona: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout_mins: Option<u64>,
    pub db_path: Option<PathBuf>,
    pub force: Option<bool>,
    pub max_retries: Option<u32>,
    pub retry_delay_secs: Option<u64>,
    pub report_mode: Option<String>,
    pub sync_repo: Option<String>,
    pub notify_on_empty: Option<bool>,
    pub max_cost_usd: Option<f64>,
    pub input_price_per_m: Option<f64>,
    pub output_price_per_m: Option<f64>,
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
