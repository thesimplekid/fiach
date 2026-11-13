//! Per-PR ledger of the findings Fiach reported, so later reviews recheck
//! them and the review verdict reflects what is still open. One ledger is
//! shared by every review kind of a PR, because each kind's verdict is
//! submitted by the same forge account and replaces the earlier one.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::reporting::{AffectedLocation, DisclosurePolicy, PriorFinding, ReportingArtifact};

/// Reviews remembered as applied; older ones can only be re-applied by a
/// forced re-review of a commit far back in the PR's history.
const MAX_APPLIED_REVIEWS: usize = 200;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerEntry {
    pub id: String,
    pub review_kind: String,
    pub title: String,
    pub severity: String,
    #[serde(default)]
    pub affected_locations: Vec<AffectedLocation>,
    pub body: String,
    pub first_commit: String,
    pub status: EntryStatus,
    #[serde(default)]
    pub resolved_commit: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    /// The review artifact's finding ID, which links the entry to the inline
    /// comments posted for it.
    #[serde(default)]
    pub source_finding_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AppliedReview {
    review_kind: String,
    commit: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrLedger {
    #[serde(default)]
    pub entries: Vec<LedgerEntry>,
    /// Reviews whose outcome is already recorded, so a recovered
    /// finalization does not add the same findings twice.
    #[serde(default)]
    applied: Vec<AppliedReview>,
}

/// What the review of one commit changed in the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerChange {
    pub added: Vec<LedgerEntry>,
    pub resolved: Vec<LedgerEntry>,
    /// Entries of every review kind still open after this review.
    pub open: Vec<LedgerEntry>,
}

impl PrLedger {
    fn parse(value: Option<&str>) -> Result<Self> {
        value
            .map(|value| serde_json::from_str(value).context("Invalid findings ledger"))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn load(db_path: &Path, repo: &str, pr: u64) -> Result<Self> {
        Self::parse(crate::state::get_finding_ledger(db_path, repo, pr)?.as_deref())
    }

    /// Open findings of one review kind for a new review to recheck.
    pub fn open_findings(&self, review_kind: &str) -> Vec<PriorFinding> {
        self.entries
            .iter()
            .filter(|entry| entry.status == EntryStatus::Open && entry.review_kind == review_kind)
            .map(|entry| PriorFinding {
                id: entry.id.clone(),
                title: entry.title.clone(),
                severity: entry.severity.clone(),
                affected_locations: entry.affected_locations.clone(),
                body: entry.body.clone(),
                first_reported_commit: entry.first_commit.clone(),
            })
            .collect()
    }

    fn applied(&self, review_kind: &str, commit: &str) -> bool {
        self.applied
            .iter()
            .any(|applied| applied.review_kind == review_kind && applied.commit == commit)
    }

    /// Records the `review_kind` review of `commit`: verified resolutions
    /// close entries and publishable findings open new ones. Incomplete
    /// reviews change nothing. Applying the same review again returns the
    /// recorded change.
    pub fn apply(
        &mut self,
        review_kind: &str,
        commit: &str,
        artifact: &ReportingArtifact,
        policy: &DisclosurePolicy,
    ) -> LedgerChange {
        let complete = !artifact.is_incomplete() && !artifact.markdown_only_fallback;
        if complete && !self.applied(review_kind, commit) {
            for (prior, status) in artifact.resolved_prior_findings() {
                if let Some(entry) = self.entries.iter_mut().find(|entry| {
                    entry.id == prior.id
                        && entry.review_kind == review_kind
                        && entry.status == EntryStatus::Open
                }) {
                    entry.status = EntryStatus::Resolved;
                    entry.resolved_commit = Some(commit.to_string());
                    entry.resolution = Some(status.rationale.trim().to_string());
                }
            }
            for finding in artifact.publishable_findings(policy) {
                let affected_locations = finding
                    .inline_comments
                    .iter()
                    .map(|comment| AffectedLocation {
                        path: comment.path.clone(),
                        start_line: Some(comment.line),
                        end_line: None,
                    })
                    .chain(finding.additional_locations.iter().cloned())
                    .chain(finding.unanchored_locations.iter().cloned())
                    .collect();
                self.entries.push(LedgerEntry {
                    id: format!("P-{}", self.entries.len() + 1),
                    review_kind: review_kind.to_string(),
                    title: finding.title,
                    severity: finding.severity,
                    affected_locations,
                    body: finding.body,
                    first_commit: commit.to_string(),
                    status: EntryStatus::Open,
                    resolved_commit: None,
                    resolution: None,
                    source_finding_id: Some(finding.finding_id),
                });
            }
            self.applied.push(AppliedReview {
                review_kind: review_kind.to_string(),
                commit: commit.to_string(),
            });
            let excess = self.applied.len().saturating_sub(MAX_APPLIED_REVIEWS);
            self.applied.drain(..excess);
        }
        self.change_for(review_kind, commit, complete)
    }

    fn change_for(&self, review_kind: &str, commit: &str, complete: bool) -> LedgerChange {
        let recorded = complete && self.applied(review_kind, commit);
        let select = |predicate: &dyn Fn(&LedgerEntry) -> bool| {
            self.entries
                .iter()
                .filter(|entry| predicate(entry))
                .cloned()
                .collect::<Vec<_>>()
        };
        let this_review = |entry: &LedgerEntry| recorded && entry.review_kind == review_kind;
        LedgerChange {
            added: select(&|entry| this_review(entry) && entry.first_commit == commit),
            resolved: select(&|entry| {
                this_review(entry) && entry.resolved_commit.as_deref() == Some(commit)
            }),
            open: select(&|entry| entry.status == EntryStatus::Open),
        }
    }
}

/// Records a finished review in the PR's ledger in one transaction, so
/// concurrent review kinds of the same PR cannot lose each other's updates.
pub fn record_review(
    db_path: &Path,
    repo: &str,
    pr: u64,
    review_kind: &str,
    commit: &str,
    artifact: &ReportingArtifact,
    policy: &DisclosurePolicy,
) -> Result<LedgerChange> {
    crate::state::update_finding_ledger(db_path, repo, pr, |current| {
        let mut ledger = PrLedger::parse(current)?;
        let change = ledger.apply(review_kind, commit, artifact, policy);
        Ok((serde_json::to_string(&ledger)?, change))
    })
}

/// Open findings of one review kind, or none when the ledger is unreadable.
pub fn load_open_findings(
    db_path: &Path,
    repo: &str,
    pr: u64,
    review_kind: &str,
) -> Vec<PriorFinding> {
    match PrLedger::load(db_path, repo, pr) {
        Ok(ledger) => ledger.open_findings(review_kind),
        Err(error) => {
            tracing::warn!(
                repo = %repo,
                pr,
                review_kind = %review_kind,
                error = %error,
                "Could not load findings ledger; earlier findings will not be rechecked"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::reporting::{
        CommandTranscript, Finding, FindingInput, InspectionRecord, PrContext, PriorFindingStatus,
        TranscriptEvidence, Verdict,
    };

    fn policy() -> DisclosurePolicy {
        DisclosurePolicy {
            pr_context: PrContext {
                state: "open".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: "head".to_string(),
            },
            diff_anchors: BTreeMap::from([("src/lib.rs".to_string(), BTreeSet::from([10]))]),
        }
    }

    fn location(path: &str, line: u32) -> AffectedLocation {
        AffectedLocation {
            path: path.to_string(),
            start_line: Some(line),
            end_line: None,
        }
    }

    fn transcript() -> CommandTranscript {
        CommandTranscript {
            evidence: TranscriptEvidence::Inspection,
            command: "inspect read src/lib.rs".to_string(),
            branch_or_commit: "head".to_string(),
            key_output: "code".to_string(),
            interpretation: "established".to_string(),
        }
    }

    fn reviewed(title: &str) -> ReportingArtifact {
        let finding = Finding::from_input(
            0,
            FindingInput {
                title: title.to_string(),
                severity: "high".to_string(),
                confidence: "high".to_string(),
                affected_locations: vec![location("src/lib.rs", 10), location("src/other.rs", 3)],
                evidence: "evidence".to_string(),
                skills_used: Vec::new(),
                body_markdown: "body".to_string(),
            },
        )
        .unwrap();
        ReportingArtifact {
            findings: vec![finding],
            verdicts: vec![Verdict {
                finding_id: "F-1".to_string(),
                confirmed: true,
                introduced_by_pr: true,
                present_on_pr_branch: true,
                present_on_base: false,
                present_on_default_branch: false,
                disclosure_decision: "disclose".to_string(),
                title_override: None,
                severity_override: None,
                impact_override: None,
                final_comment_body: None,
                affected_locations: Vec::new(),
                command_transcripts: vec![transcript()],
                rationale: "confirmed".to_string(),
            }],
            ..ReportingArtifact::default()
        }
    }

    fn recheck(ledger: &PrLedger, resolved: bool, inspected: &str) -> ReportingArtifact {
        let prior_findings = ledger.open_findings("security");
        ReportingArtifact {
            no_findings: Some(crate::reporting::NoFindings {
                summary: "nothing new".to_string(),
                skills_used: Vec::new(),
            }),
            prior_statuses: prior_findings
                .iter()
                .map(|prior| PriorFindingStatus {
                    prior_finding_id: prior.id.clone(),
                    resolved,
                    affected_locations: Vec::new(),
                    command_transcripts: vec![transcript()],
                    rationale: "A guard now rejects it.".to_string(),
                })
                .collect(),
            prior_findings,
            verifier_inspections: Some(vec![InspectionRecord {
                operation: "read".to_string(),
                path: inspected.to_string(),
            }]),
            ..ReportingArtifact::default()
        }
    }

    #[test]
    fn findings_stay_open_until_the_verifier_establishes_a_fix_by_inspection() {
        let mut ledger = PrLedger::default();
        let change = ledger.apply("security", "c1", &reviewed("Lost update"), &policy());
        assert_eq!(change.added.len(), 1);
        assert_eq!(change.open.len(), 1);
        let entry = &change.added[0];
        assert_eq!(entry.id, "P-1");
        assert_eq!(
            entry.affected_locations,
            vec![location("src/lib.rs", 10), location("src/other.rs", 3)]
        );
        let prior = ledger.open_findings("security");
        assert_eq!(prior[0].first_reported_commit, "c1");

        // Claimed fixed, but the verifier never inspected a cited file.
        let change = ledger.apply(
            "security",
            "c2",
            &recheck(&ledger, true, "src/unrelated.rs"),
            &policy(),
        );
        assert!(change.resolved.is_empty());
        assert_eq!(change.open.len(), 1);

        // Inspected and still reachable.
        let change = ledger.apply(
            "security",
            "c3",
            &recheck(&ledger, false, "src/lib.rs"),
            &policy(),
        );
        assert!(change.resolved.is_empty());

        let change = ledger.apply(
            "security",
            "c4",
            &recheck(&ledger, true, "src/lib.rs"),
            &policy(),
        );
        assert_eq!(change.resolved.len(), 1);
        assert!(change.open.is_empty());
        assert_eq!(change.resolved[0].resolved_commit.as_deref(), Some("c4"));
        assert_eq!(
            change.resolved[0].resolution.as_deref(),
            Some("A guard now rejects it.")
        );
        assert!(ledger.open_findings("security").is_empty());
    }

    #[test]
    fn reapplying_a_commit_returns_the_recorded_change_without_duplicates() {
        let mut ledger = PrLedger::default();
        let artifact = reviewed("Lost update");
        let first = ledger.apply("security", "c1", &artifact, &policy());
        let again = ledger.apply("security", "c1", &artifact, &policy());
        assert_eq!(first, again);
        assert_eq!(ledger.entries.len(), 1);

        let next = ledger.apply("security", "c2", &reviewed("Second bug"), &policy());
        assert_eq!(next.added.len(), 1);
        assert_eq!(next.added[0].id, "P-2");
        assert_eq!(next.open.len(), 2);
    }

    #[test]
    fn incomplete_reviews_leave_the_ledger_unchanged() {
        let mut ledger = PrLedger::default();
        ledger.apply("security", "c1", &reviewed("Lost update"), &policy());
        let before = ledger.clone();

        let mut incomplete = recheck(&ledger, true, "src/lib.rs");
        incomplete.verifier_failed = true;
        let change = ledger.apply("security", "c2", &incomplete, &policy());
        assert!(change.added.is_empty() && change.resolved.is_empty());
        assert_eq!(change.open.len(), 1);
        assert_eq!(ledger, before);

        // A later complete review of the same commit still applies.
        let change = ledger.apply(
            "security",
            "c2",
            &recheck(&ledger, true, "src/lib.rs"),
            &policy(),
        );
        assert_eq!(change.resolved.len(), 1);
    }

    #[test]
    fn review_kinds_share_one_ledger_per_pr() {
        let mut ledger = PrLedger::default();
        ledger.apply("security", "c1", &reviewed("Key leak"), &policy());
        let change = ledger.apply("pr-review", "c1", &reviewed("Lost update"), &policy());
        assert_eq!(change.added.len(), 1);
        assert_eq!(change.added[0].id, "P-2");
        // Every open finding counts toward the verdict.
        assert_eq!(change.open.len(), 2);
        // Each kind rechecks only its own findings.
        assert_eq!(ledger.open_findings("security")[0].id, "P-1");
        assert_eq!(ledger.open_findings("pr-review")[0].id, "P-2");

        let mut recheck = recheck(&ledger, true, "src/lib.rs");
        recheck.prior_findings = ledger.open_findings("pr-review");
        recheck.prior_statuses[0].prior_finding_id = "P-2".to_string();
        let change = ledger.apply("pr-review", "c2", &recheck, &policy());
        assert_eq!(change.resolved.len(), 1);
        assert_eq!(change.open.len(), 1);
        assert_eq!(change.open[0].review_kind, "security");
    }

    #[test]
    fn ledgers_persist_per_pr() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("state.redb");
        assert!(load_open_findings(&db, "owner/repo", 1, "security").is_empty());

        let artifact = reviewed("Lost update");
        let first =
            record_review(&db, "owner/repo", 1, "security", "c1", &artifact, &policy()).unwrap();
        let again =
            record_review(&db, "owner/repo", 1, "security", "c1", &artifact, &policy()).unwrap();
        assert_eq!(first, again);

        let ledger = PrLedger::load(&db, "owner/repo", 1).unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(
            load_open_findings(&db, "owner/repo", 1, "security").len(),
            1
        );
        assert!(load_open_findings(&db, "owner/repo", 1, "pr-review").is_empty());
        assert!(load_open_findings(&db, "owner/repo", 2, "security").is_empty());
        assert!(load_open_findings(&db, "owner/repository", 1, "security").is_empty());
    }
}
