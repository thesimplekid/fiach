//! Issue classification, duplicate marking, and independently verified draft fixes.
pub mod config;
mod coverage;
mod github;
mod triage;
mod worker;
mod workflow;

pub use worker::run_child;
pub use workflow::{inspect_coverage, run};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub is_pr: bool,
    pub updated_at: String,
    pub comments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Duplicate,
    Addressed,
    NeedsInfo,
    NeedsDecision,
    NeedsReview,
    Ready,
}

/// Host-selected validation policy; the coder cannot downgrade a bug to maintenance.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixKind {
    #[default]
    Bug,
    Maintenance,
    /// Actionable work without a supported automatic validation policy.
    Investigation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    #[serde(default)]
    pub fix_kind: FixKind,
    pub route: Route,
    /// Triage eligibility only; global permission and a configured worker are also required.
    #[serde(default)]
    pub auto_fix_eligible: bool,
    /// Execution evidence gap; does not change task readiness.
    #[serde(default)]
    pub area_uncertain: bool,
    pub labels: Vec<String>,
    pub matches: Vec<u64>,
    pub related: Vec<u64>,
    /// Internal evidence gaps; never render these as related work.
    #[serde(default)]
    pub unresolved: Vec<u64>,
    /// Concrete worker guidance suitable for publication, when available.
    #[serde(default)]
    pub guidance: Option<String>,
    pub explanation: String,
}

impl Decision {
    pub(super) fn ready_for_execution(&self) -> bool {
        self.route == Route::Ready
            && self.auto_fix_eligible
            && !self.area_uncertain
            && self.fix_kind != FixKind::Investigation
            && self.unresolved.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_ready_decisions_cannot_authorize_execution() {
        let mut decision: Decision = serde_json::from_value(serde_json::json!({
            "route": "ready", "fix_kind": "maintenance", "labels": [],
            "matches": [], "related": [], "explanation": "Old ready judgment"
        }))
        .unwrap();
        assert!(!decision.ready_for_execution());
        decision.auto_fix_eligible = true;
        assert!(decision.ready_for_execution());
        decision.area_uncertain = true;
        assert!(!decision.ready_for_execution());
        decision.area_uncertain = false;
        decision.fix_kind = FixKind::Investigation;
        assert!(!decision.ready_for_execution());
        decision.fix_kind = FixKind::Bug;
        decision.unresolved.push(42);
        assert!(!decision.ready_for_execution());
    }
}
