//! Issue classification, duplicate marking, and independently verified draft fixes.
pub mod config;
mod coverage;
mod github;
mod iterations;
mod triage;
mod worker;
mod workflow;

pub(crate) use iterations::fix_command as is_pr_fix_command;
pub use worker::run_child;
pub use workflow::{inspect_coverage, run, run_with_reviews};

use serde::{Deserialize, Serialize};

/// Trusted requests from allowlisted users, identified by GitHub ids. Each new id
/// authorizes at most one fix attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(super) struct Request {
    /// Latest comment that mentions the configured bot.
    pub mention: Option<u64>,
    /// Latest event that applied the configured `ready` label.
    pub label: Option<u64>,
}

impl Request {
    pub(super) fn is_some(&self) -> bool {
        self.mention.is_some() || self.label.is_some()
    }
}

/// The branch for an issue's PR. Each retry after a closed PR uses a new branch,
/// because publication never overwrites an existing branch.
pub(super) fn issue_branch(issue: u64, retry: usize) -> String {
    if retry == 0 {
        format!("fiach/issue-{issue}")
    } else {
        format!("fiach/issue-{issue}-r{}", retry + 1)
    }
}

pub(super) fn is_issue_branch(branch: &str, issue: u64) -> bool {
    let Some(rest) = branch.strip_prefix(&format!("fiach/issue-{issue}")) else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix("-r")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| n >= 2 && rest == format!("-r{n}"))
}

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
    /// Tests and justified mutation exclusions, without production behavior changes.
    TestMaintenance,
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
    fn retry_branches_are_distinct_and_recognized() {
        assert_eq!(issue_branch(7, 0), "fiach/issue-7");
        assert_eq!(issue_branch(7, 1), "fiach/issue-7-r2");
        for retry in 0..3 {
            assert!(is_issue_branch(&issue_branch(7, retry), 7));
            assert!(!is_issue_branch(&issue_branch(7, retry), 70));
        }
        for branch in [
            "fiach/issue-70",
            "fiach/issue-7-r1",
            "fiach/issue-7-r02",
            "fiach/issue-7-rx",
            "fiach/issue-7-",
            "other/issue-7",
        ] {
            assert!(!is_issue_branch(branch, 7), "{branch}");
        }
    }

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
        decision.fix_kind = FixKind::TestMaintenance;
        assert!(decision.ready_for_execution());
        decision.fix_kind = FixKind::Bug;
        decision.unresolved.push(42);
        assert!(!decision.ready_for_execution());
    }
}
