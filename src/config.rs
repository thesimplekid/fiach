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
    pub port: Option<u16>,
    pub interval: Option<u64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub dedupe_existing_comments: Option<bool>,
    pub dedupe_provider: Option<String>,
    pub dedupe_model: Option<String>,
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
    // Sandbox options
    pub sandbox_rootfs: Option<PathBuf>,
    pub sandbox_network: Option<String>,
    pub sandbox_extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReviewConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    pub dedupe_existing_comments: Option<bool>,
    pub dedupe_provider: Option<String>,
    pub dedupe_model: Option<String>,
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
}
