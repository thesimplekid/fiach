use std::{collections::HashSet, path::PathBuf};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssueConfig {
    pub repos: Vec<Project>,
    pub interval_secs: u64,
    pub state_path: PathBuf,
    pub scratch_dir: PathBuf,
    pub publish: bool,
    pub auto_fix: bool,
    /// Maximum changed issues handled per polling pass; never an evidence limit.
    pub max_items: usize,
    pub max_jev_cost_usd: f64,
    pub jev_base_url: String,
    pub worker: Option<WorkerConfig>,
}

impl Default for IssueConfig {
    fn default() -> Self {
        Self {
            repos: vec![],
            interval_secs: 300,
            state_path: "issues.redb".into(),
            scratch_dir: "/data/rust/tmp".into(),
            publish: false,
            auto_fix: true,
            max_items: 1000,
            max_jev_cost_usd: 0.25,
            jev_base_url: "https://api.typesafe.ai".into(),
            worker: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub repo: String,
    #[serde(default)]
    pub areas: Vec<Area>,
    #[serde(default)]
    pub labels: Labels,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Area {
    pub label: String,
    pub description: String,
    #[serde(default)]
    pub paths: Vec<String>,
    /// If false, issues touching this area always require a maintainer.
    #[serde(default)]
    pub auto_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Labels {
    pub bug: String,
    pub feature: String,
    pub documentation: String,
    pub question: String,
    pub duplicate: String,
    pub addressed: String,
    pub needs_info: String,
    pub needs_decision: String,
    pub ready: String,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            bug: "bug".into(),
            feature: "enhancement".into(),
            documentation: "documentation".into(),
            question: "question".into(),
            duplicate: "duplicate".into(),
            addressed: "already-being-addressed".into(),
            needs_info: "needs-info".into(),
            needs_decision: "needs-decision".into(),
            ready: "ready-for-agent".into(),
        }
    }
}

impl Project {
    pub fn managed_labels(&self) -> Vec<String> {
        let l = &self.labels;
        [
            &l.bug,
            &l.feature,
            &l.documentation,
            &l.question,
            &l.duplicate,
            &l.addressed,
            &l.needs_info,
            &l.needs_decision,
            &l.ready,
        ]
        .into_iter()
        .cloned()
        .chain(self.areas.iter().map(|a| a.label.clone()))
        .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub rootfs: PathBuf,
    pub provider: String,
    pub model: String,
    pub verifier_provider: Option<String>,
    pub verifier_model: Option<String>,
    /// veth uses Fiach's isolated subnet allocator; host is an explicit compatibility mode.
    #[serde(default = "network")]
    pub network: String,
    #[serde(default = "turns")]
    pub max_turns: u32,
    #[serde(default = "timeout")]
    pub timeout_secs: u64,
}
fn network() -> String {
    "veth".into()
}
fn turns() -> u32 {
    60
}
fn timeout() -> u64 {
    1800
}

pub(super) fn valid_repo(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|s| {
            !s.is_empty()
                && !s.starts_with('.')
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

impl IssueConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.repos.is_empty(), "issues.repos must not be empty");
        ensure!(
            self.interval_secs > 0 && self.max_items > 0,
            "Issue polling limits must be positive"
        );
        ensure!(
            self.max_jev_cost_usd.is_finite() && self.max_jev_cost_usd > 0.0,
            "Invalid Jev budget"
        );
        ensure!(
            self.scratch_dir.is_absolute(),
            "Issue scratch_dir must be absolute"
        );
        let mut repos = HashSet::new();
        for project in &self.repos {
            ensure!(
                valid_repo(&project.repo) && repos.insert(project.repo.to_lowercase()),
                "Invalid or duplicate repository"
            );
            let labels = project.managed_labels();
            let unique: HashSet<_> = labels.iter().map(|s| s.to_lowercase()).collect();
            ensure!(
                unique.len() == labels.len(),
                "Managed issue labels must be unique"
            );
            ensure!(
                labels.iter().all(|s| !s.trim().is_empty()
                    && s.len() <= 50
                    && !s.chars().any(char::is_control)),
                "Invalid issue label"
            );
            for area in &project.areas {
                ensure!(
                    area.paths.iter().all(|p| !p.is_empty()
                        && !p.starts_with(['/', ':'])
                        && !p.contains('\0')
                        && !p.split('/').any(|part| matches!(part, "." | ".."))),
                    "Area paths must be repository-relative Git glob patterns"
                );
            }
            ensure!(
                project
                    .areas
                    .iter()
                    .all(|a| !a.description.trim().is_empty()),
                "Areas require descriptions"
            );
        }
        if let Some(w) = &self.worker {
            ensure!(
                matches!(w.network.as_str(), "veth" | "host"),
                "Worker network must be veth or host"
            );
            ensure!(
                w.rootfs.is_absolute() && w.rootfs.is_dir(),
                "Issue worker rootfs must be an existing absolute directory"
            );
            ensure!(
                w.max_turns > 0 && w.timeout_secs > 0,
                "Worker limits must be positive"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_loads_and_keeps_github_writes_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, include_str!("../../example.issues.toml")).unwrap();
        let config = crate::config::FiachConfig::load(Some(&path))
            .unwrap()
            .issues
            .unwrap();
        config.validate().unwrap();
        assert!(!config.publish);
        assert_eq!(config.repos[0].areas.len(), 5);
        assert!(!config.repos[0].areas[0].auto_fix);
        assert!(config.repos[0].areas[2].auto_fix);
    }

    #[test]
    fn rejects_ambiguous_labels_and_repository_paths() {
        let mut config = IssueConfig {
            repos: vec![Project {
                repo: "owner/repo".into(),
                areas: vec![],
                labels: Labels::default(),
            }],
            ..Default::default()
        };
        config.validate().unwrap();
        config.repos[0].labels.bug = "DUPLICATE".into();
        assert!(config.validate().is_err());
        for repo in [
            "https://github.com/owner/repo",
            "-owner/repo",
            "owner/../repo",
            "owner/repo?state=all",
            "owner/repo\n",
        ] {
            assert!(!valid_repo(repo), "{repo}");
        }
    }
}
