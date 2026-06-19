use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use rmcp::model::{JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewPhase {
    Finder,
    Verifier,
    Dedupe,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportingArtifact {
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub no_findings: Option<NoFindings>,
    #[serde(default)]
    pub verdicts: Vec<Verdict>,
    #[serde(default)]
    pub duplicate_decisions: Vec<DuplicateDecision>,
    #[serde(default)]
    pub verifier_failed: bool,
    #[serde(default)]
    pub markdown_only_fallback: bool,
}

impl ReportingArtifact {
    pub fn finder_complete(&self) -> bool {
        self.no_findings.is_some() || !self.findings.is_empty()
    }

    pub fn verifier_complete(&self) -> bool {
        !self.findings.is_empty() && self.verdicts.len() >= self.findings.len()
    }

    pub fn accepted_findings(&self, policy: &DisclosurePolicy) -> Vec<AcceptedFinding> {
        self.accepted_findings_with_policy(policy, true)
    }

    pub fn publishable_findings(&self, policy: &DisclosurePolicy) -> Vec<AcceptedFinding> {
        self.accepted_findings(policy)
            .into_iter()
            .filter(|finding| !self.is_already_reported(&finding.finding_id))
            .collect()
    }

    pub fn already_reported_findings(&self, policy: &DisclosurePolicy) -> Vec<AcceptedFinding> {
        self.accepted_findings(policy)
            .into_iter()
            .filter(|finding| self.is_already_reported(&finding.finding_id))
            .collect()
    }

    pub fn is_already_reported(&self, finding_id: &str) -> bool {
        self.duplicate_decisions
            .iter()
            .any(|decision| decision.suppresses_finding(finding_id))
    }

    pub fn duplicate_decision_for(&self, finding_id: &str) -> Option<&DuplicateDecision> {
        self.duplicate_decisions
            .iter()
            .find(|decision| decision.finding_id == finding_id)
    }

    pub fn confirmed_findings_including_non_pr(
        &self,
        policy: &DisclosurePolicy,
    ) -> Vec<AcceptedFinding> {
        self.findings_with_policy(policy, false, false)
    }

    fn accepted_findings_with_policy(
        &self,
        policy: &DisclosurePolicy,
        require_introduced_by_pr: bool,
    ) -> Vec<AcceptedFinding> {
        self.findings_with_policy(policy, require_introduced_by_pr, true)
    }

    fn findings_with_policy(
        &self,
        policy: &DisclosurePolicy,
        require_introduced_by_pr: bool,
        require_disclosure_decision: bool,
    ) -> Vec<AcceptedFinding> {
        self.findings
            .iter()
            .filter_map(|finding| {
                let verdict = self
                    .verdicts
                    .iter()
                    .find(|verdict| verdict.finding_id == finding.id)?;
                AcceptedFinding::from_parts(
                    finding,
                    verdict,
                    policy,
                    require_introduced_by_pr,
                    require_disclosure_decision,
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AffectedLocation {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FindingInput {
    pub title: String,
    pub severity: String,
    pub confidence: String,
    #[serde(default)]
    pub affected_locations: Vec<AffectedLocation>,
    pub evidence: String,
    #[serde(default)]
    pub skills_used: Vec<String>,
    pub body_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub confidence: String,
    #[serde(default)]
    pub affected_locations: Vec<AffectedLocation>,
    pub evidence: String,
    #[serde(default)]
    pub skills_used: Vec<String>,
    pub body_markdown: String,
}

impl Finding {
    pub fn from_input(index: usize, input: FindingInput) -> Result<Self> {
        validate_required("title", &input.title)?;
        validate_required("severity", &input.severity)?;
        validate_required("confidence", &input.confidence)?;
        validate_required("evidence", &input.evidence)?;
        validate_required("body_markdown", &input.body_markdown)?;

        for location in &input.affected_locations {
            validate_required("affected_locations.path", &location.path)?;
            if let (Some(start), Some(end)) = (location.start_line, location.end_line)
                && end < start
            {
                bail!("affected_locations.end_line must be >= start_line");
            }
        }

        Ok(Self {
            id: format!("F-{}", index + 1),
            title: input.title,
            severity: normalize_scalar(input.severity),
            confidence: normalize_scalar(input.confidence),
            affected_locations: input.affected_locations,
            evidence: input.evidence,
            skills_used: normalize_skills(input.skills_used),
            body_markdown: input.body_markdown,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoFindings {
    pub summary: String,
    #[serde(default)]
    pub skills_used: Vec<String>,
}

impl NoFindings {
    pub fn validate(&mut self) -> Result<()> {
        validate_required("summary", &self.summary)?;
        self.skills_used = normalize_skills(std::mem::take(&mut self.skills_used));
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandTranscript {
    pub command: String,
    pub branch_or_commit: String,
    pub key_output: String,
    pub interpretation: String,
}

impl CommandTranscript {
    fn validate(&self) -> Result<()> {
        validate_required("command_transcripts.command", &self.command)?;
        validate_required(
            "command_transcripts.branch_or_commit",
            &self.branch_or_commit,
        )?;
        validate_required("command_transcripts.key_output", &self.key_output)?;
        validate_required("command_transcripts.interpretation", &self.interpretation)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Verdict {
    pub finding_id: String,
    pub confirmed: bool,
    pub introduced_by_pr: bool,
    pub present_on_pr_branch: bool,
    pub present_on_base: bool,
    pub present_on_default_branch: bool,
    pub disclosure_decision: String,
    #[serde(default)]
    pub title_override: Option<String>,
    #[serde(default)]
    pub severity_override: Option<String>,
    #[serde(default)]
    pub impact_override: Option<String>,
    #[serde(default)]
    pub final_comment_body: Option<String>,
    #[serde(default)]
    pub affected_locations: Vec<AffectedLocation>,
    #[serde(default)]
    pub command_transcripts: Vec<CommandTranscript>,
    pub rationale: String,
}

impl Verdict {
    pub fn validate(&mut self, finding_ids: &BTreeSet<String>) -> Result<()> {
        validate_required("finding_id", &self.finding_id)?;
        if !finding_ids.contains(&self.finding_id) {
            bail!(
                "verdict references unknown finding_id `{}`",
                self.finding_id
            );
        }
        validate_required("disclosure_decision", &self.disclosure_decision)?;
        validate_required("rationale", &self.rationale)?;

        if let Some(title) = &self.title_override {
            validate_required("title_override", title)?;
        }
        if let Some(severity) = &mut self.severity_override {
            *severity = normalize_scalar(std::mem::take(severity));
        }
        for transcript in &self.command_transcripts {
            transcript.validate()?;
        }
        for location in &self.affected_locations {
            validate_required("affected_locations.path", &location.path)?;
        }
        Ok(())
    }

    pub fn wants_disclosure(&self) -> bool {
        matches!(
            self.disclosure_decision.to_ascii_lowercase().as_str(),
            "disclose" | "pr-comment" | "comment" | "inline"
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DuplicateDecision {
    pub finding_id: String,
    pub already_reported: bool,
    #[serde(default)]
    pub matching_comment_ids: Vec<u64>,
    pub confidence: String,
    pub rationale: String,
}

impl DuplicateDecision {
    pub fn validate(&mut self, finding_ids: &BTreeSet<String>) -> Result<()> {
        validate_required("finding_id", &self.finding_id)?;
        if !finding_ids.contains(&self.finding_id) {
            bail!(
                "duplicate decision references unknown finding_id `{}`",
                self.finding_id
            );
        }
        validate_required("confidence", &self.confidence)?;
        self.confidence = normalize_scalar(std::mem::take(&mut self.confidence));
        validate_required("rationale", &self.rationale)
    }

    pub fn suppresses_finding(&self, finding_id: &str) -> bool {
        self.finding_id == finding_id
            && self.already_reported
            && !self.matching_comment_ids.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExistingPrComment {
    pub id: u64,
    pub kind: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

impl ExistingPrComment {
    pub fn from_issue_comment(value: &Value) -> Option<Self> {
        Self::from_value(value, "top-level", None, None)
    }

    pub fn from_inline_comment(value: &Value) -> Option<Self> {
        let path = value
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let line = value
            .get("line")
            .or_else(|| value.get("original_line"))
            .and_then(|value| value.as_u64());
        Self::from_value(value, "inline", path, line)
    }

    pub fn from_review(value: &Value) -> Option<Self> {
        Self::from_value(value, "review", None, None)
    }

    fn from_value(
        value: &Value,
        kind: &str,
        path: Option<String>,
        line: Option<u64>,
    ) -> Option<Self> {
        let id = value.get("id")?.as_u64()?;
        let body = value
            .get("body")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if body.is_empty() {
            return None;
        }
        let author = value
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let url = value
            .get("html_url")
            .and_then(|value| value.as_str())
            .map(str::to_string);

        Some(Self {
            id,
            kind: kind.to_string(),
            author,
            body,
            path,
            line,
            url,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrContext {
    pub state: String,
    pub merged: bool,
    pub base_ref_name: String,
    pub default_branch: String,
    pub base_commit: String,
    pub head_commit: String,
}

impl PrContext {
    pub fn comments_allowed(&self) -> bool {
        self.state.eq_ignore_ascii_case("open") && !self.merged
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisclosurePolicy {
    pub pr_context: PrContext,
    pub diff_anchors: BTreeMap<String, BTreeSet<u32>>,
}

impl DisclosurePolicy {
    pub fn valid_anchor(&self, location: &AffectedLocation) -> bool {
        let Some(line) = location.start_line else {
            return false;
        };
        self.diff_anchors
            .get(&location.path)
            .is_some_and(|lines| lines.contains(&line))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedFinding {
    pub finding_id: String,
    pub title: String,
    pub severity: String,
    pub impact: Option<String>,
    pub body: String,
    pub inline_comments: Vec<InlineComment>,
    #[serde(default)]
    pub additional_locations: Vec<AffectedLocation>,
    pub unanchored_locations: Vec<AffectedLocation>,
    pub verdict: Verdict,
}

impl AcceptedFinding {
    fn from_parts(
        finding: &Finding,
        verdict: &Verdict,
        policy: &DisclosurePolicy,
        require_introduced_by_pr: bool,
        require_disclosure_decision: bool,
    ) -> Option<Self> {
        if !verdict.confirmed
            || (require_introduced_by_pr && !verdict.introduced_by_pr)
            || (require_disclosure_decision && !verdict.wants_disclosure())
            || verdict.command_transcripts.is_empty()
        {
            return None;
        }

        let body = verdict
            .final_comment_body
            .clone()
            .unwrap_or_else(|| finding.body_markdown.clone());
        if body.trim().is_empty() {
            return None;
        }

        let locations = if verdict.affected_locations.is_empty() {
            finding.affected_locations.clone()
        } else {
            verdict.affected_locations.clone()
        };

        let mut inline_comments = Vec::new();
        let mut additional_locations = Vec::new();
        let mut unanchored_locations = Vec::new();
        let mut seen_locations = BTreeSet::new();
        for location in locations {
            let location_key = (
                location.path.clone(),
                location.start_line,
                location.end_line,
            );
            if !seen_locations.insert(location_key) {
                continue;
            }

            if policy.valid_anchor(&location) {
                if inline_comments.is_empty() {
                    inline_comments.push(InlineComment {
                        path: location.path.clone(),
                        line: location.start_line.unwrap_or_default(),
                        body: body.clone(),
                    });
                } else {
                    additional_locations.push(location);
                }
            } else {
                unanchored_locations.push(location);
            }
        }

        Some(Self {
            finding_id: finding.id.clone(),
            title: verdict
                .title_override
                .clone()
                .unwrap_or_else(|| finding.title.clone()),
            severity: verdict
                .severity_override
                .clone()
                .unwrap_or_else(|| finding.severity.clone()),
            impact: verdict.impact_override.clone(),
            body,
            inline_comments,
            additional_locations,
            unanchored_locations,
            verdict: verdict.clone(),
        })
    }
}

pub fn reporting_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "submit_finding".to_string(),
            "Submit one candidate finding from the finder pass. Call this once per candidate."
                .to_string(),
            object_schema(
                vec![
                    ("title", string_schema("Short finding title")),
                    (
                        "severity",
                        string_schema("Severity such as low, medium, high, or critical"),
                    ),
                    (
                        "confidence",
                        string_schema("Confidence in the candidate finding"),
                    ),
                    ("affected_locations", array_schema(location_schema())),
                    (
                        "evidence",
                        string_schema("Specific evidence supporting the finding"),
                    ),
                    ("skills_used", array_schema(string_schema("Skill name"))),
                    (
                        "body_markdown",
                        string_schema("Markdown body for the finding"),
                    ),
                ],
                vec![
                    "title",
                    "severity",
                    "confidence",
                    "evidence",
                    "body_markdown",
                ],
            ),
        ),
        Tool::new(
            "submit_no_findings".to_string(),
            "Submit a structured no-findings result when the finder found no candidates."
                .to_string(),
            object_schema(
                vec![
                    ("summary", string_schema("Brief review summary")),
                    ("skills_used", array_schema(string_schema("Skill name"))),
                ],
                vec!["summary"],
            ),
        ),
        Tool::new(
            "submit_verdict".to_string(),
            "Submit the verifier verdict for exactly one candidate finding.".to_string(),
            object_schema(
                vec![
                    (
                        "finding_id",
                        string_schema("Candidate finding id, for example F-1"),
                    ),
                    ("confirmed", bool_schema("Whether the finding is confirmed")),
                    (
                        "introduced_by_pr",
                        bool_schema("Whether the PR introduced the issue"),
                    ),
                    (
                        "present_on_pr_branch",
                        bool_schema("Whether the issue is present on the PR branch"),
                    ),
                    (
                        "present_on_base",
                        bool_schema("Whether the issue is present at the base commit"),
                    ),
                    (
                        "present_on_default_branch",
                        bool_schema("Whether the issue is present on the current default branch"),
                    ),
                    ("disclosure_decision", string_schema("disclose or suppress")),
                    ("title_override", string_schema("Optional final title")),
                    (
                        "severity_override",
                        string_schema("Optional final severity"),
                    ),
                    (
                        "impact_override",
                        string_schema("Optional final impact statement"),
                    ),
                    (
                        "final_comment_body",
                        string_schema("Optional final Markdown comment body"),
                    ),
                    ("affected_locations", array_schema(location_schema())),
                    (
                        "command_transcripts",
                        array_schema(command_transcript_schema()),
                    ),
                    ("rationale", string_schema("Verifier rationale")),
                ],
                vec![
                    "finding_id",
                    "confirmed",
                    "introduced_by_pr",
                    "present_on_pr_branch",
                    "present_on_base",
                    "present_on_default_branch",
                    "disclosure_decision",
                    "rationale",
                ],
            ),
        ),
        Tool::new(
            "submit_duplicate_decision".to_string(),
            "Submit whether one verified finding was already reported in existing PR discussion."
                .to_string(),
            object_schema(
                vec![
                    (
                        "finding_id",
                        string_schema("Verified finding id, for example F-1"),
                    ),
                    (
                        "already_reported",
                        bool_schema(
                            "Whether an existing PR comment already reports the same root issue",
                        ),
                    ),
                    (
                        "matching_comment_ids",
                        array_schema(number_schema("GitHub comment or review id that matched")),
                    ),
                    (
                        "confidence",
                        string_schema("Confidence in the duplicate decision"),
                    ),
                    (
                        "rationale",
                        string_schema("Brief rationale citing why the comments do or do not match"),
                    ),
                ],
                vec![
                    "finding_id",
                    "already_reported",
                    "matching_comment_ids",
                    "confidence",
                    "rationale",
                ],
            ),
        ),
    ]
}

pub fn parse_diff_anchors(diff: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut anchors: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    let mut new_line: Option<u32> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = Some(path.to_string());
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_file = None;
            continue;
        }
        if line.starts_with("@@") {
            new_line = parse_hunk_new_start(line);
            continue;
        }
        let Some(path) = &current_file else {
            continue;
        };
        let Some(line_no) = new_line else {
            continue;
        };

        if line.starts_with('+') && !line.starts_with("+++") {
            anchors.entry(path.clone()).or_default().insert(line_no);
            new_line = Some(line_no + 1);
        } else if !line.starts_with('-') {
            new_line = Some(line_no + 1);
        }
    }

    anchors
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let marker = line.split_whitespace().find(|part| part.starts_with('+'))?;
    let start = marker
        .trim_start_matches('+')
        .split(',')
        .next()
        .unwrap_or_default();
    start.parse().ok()
}

pub fn render_markdown(
    repo: &str,
    pr_number: u64,
    artifact: &ReportingArtifact,
    policy: Option<&DisclosurePolicy>,
) -> String {
    let accepted = policy
        .map(|policy| artifact.publishable_findings(policy))
        .unwrap_or_default();
    let already_reported = policy
        .map(|policy| artifact.already_reported_findings(policy))
        .unwrap_or_default();
    let status = if artifact.verifier_failed {
        "unverified"
    } else if !accepted.is_empty() {
        "confirmed"
    } else if !already_reported.is_empty() {
        "already-reported"
    } else if artifact.no_findings.is_some() {
        "none"
    } else {
        "rejected"
    };
    let severity = accepted
        .iter()
        .map(|finding| finding.severity.as_str())
        .max_by_key(|severity| severity_rank(severity))
        .unwrap_or("none");
    let skills = collect_skills(artifact);
    let findings_count = accepted.len();
    let notify = findings_count > 0 && !artifact.verifier_failed;

    let mut out = format!(
        r#"---
title: "{title}"
notify: {notify}
status: {status}
severity: {severity}
target: {repo}
pr: {pr_number}
skills_used: [{skills}]
findings_count: {findings_count}
---

"#,
        title = markdown_title(artifact, &accepted, &already_reported),
        skills = skills
            .iter()
            .map(|skill| format!("\"{}\"", skill.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", "),
    );

    if let Some(no_findings) = &artifact.no_findings {
        out.push_str("## Summary\n");
        out.push_str(no_findings.summary.trim());
        out.push('\n');
        return out;
    }

    if artifact.verifier_failed {
        out.push_str("## Summary\nVerifier failed or did not submit complete structured verdicts. No PR disclosure was allowed.\n\n");
    }

    if accepted.is_empty() {
        out.push_str(
            "## Summary\nNo new verified PR-introduced findings were approved for disclosure.\n\n",
        );
    } else {
        out.push_str(
            "## Summary\nVerified PR-introduced findings were approved by the verifier.\n\n",
        );
        for finding in &accepted {
            out.push_str(&format!(
                "## {} ({})\n\n{}\n\n",
                finding.title,
                finding.severity,
                finding.body.trim()
            ));
            out.push_str("### Verifier Evidence\n");
            out.push_str(finding.verdict.rationale.trim());
            out.push('\n');
            for transcript in &finding.verdict.command_transcripts {
                out.push_str(&format!(
                    "\n- `{}` at `{}`: {}\n",
                    transcript.command, transcript.branch_or_commit, transcript.interpretation
                ));
            }
            if !finding.additional_locations.is_empty() {
                out.push_str("\n### Additional Locations\n");
                for location in &finding.additional_locations {
                    out.push_str(&format!(
                        "- {}:{}\n",
                        location.path,
                        location.start_line.unwrap_or_default()
                    ));
                }
            }
            if !finding.unanchored_locations.is_empty() {
                out.push_str("\n### Unanchored Locations\n");
                for location in &finding.unanchored_locations {
                    out.push_str(&format!(
                        "- {}:{}\n",
                        location.path,
                        location.start_line.unwrap_or_default()
                    ));
                }
            }
            out.push('\n');
        }
    }

    if !already_reported.is_empty() {
        out.push_str("## Already Reported\n");
        for finding in &already_reported {
            let matches = artifact
                .duplicate_decision_for(&finding.finding_id)
                .map(|decision| {
                    decision
                        .matching_comment_ids
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let rationale = artifact
                .duplicate_decision_for(&finding.finding_id)
                .map(|decision| decision.rationale.trim())
                .unwrap_or("duplicate decision did not include a rationale");
            out.push_str(&format!(
                "- {} ({}): matched existing comment ids [{}]. {}\n",
                finding.title, finding.finding_id, matches, rationale
            ));
        }
        out.push('\n');
    }

    let rejected: Vec<_> = artifact
        .verdicts
        .iter()
        .filter(|verdict| {
            !verdict.confirmed
                || !verdict.introduced_by_pr
                || !verdict.wants_disclosure()
                || verdict.command_transcripts.is_empty()
        })
        .collect();
    if !rejected.is_empty() {
        out.push_str("## Suppressed Candidates\n");
        for verdict in rejected {
            out.push_str(&format!(
                "- {}: {}\n",
                verdict.finding_id, verdict.rationale
            ));
        }
    }

    out
}

pub fn review_summary_body(accepted: &[AcceptedFinding]) -> String {
    if accepted.is_empty() {
        return "Verified review completed. No inline findings were anchorable.".to_string();
    }

    let mut body = String::from("Verified findings approved for disclosure:\n\n");
    for finding in accepted {
        body.push_str(&format!(
            "- **{}** ({}) - {}\n",
            finding.title,
            finding.severity,
            finding.impact.as_deref().unwrap_or("see inline comment")
        ));
        if !finding.additional_locations.is_empty() {
            body.push_str("  Additional locations included in summary:\n");
            for location in &finding.additional_locations {
                body.push_str(&format!(
                    "  - {}:{}\n",
                    location.path,
                    location.start_line.unwrap_or_default()
                ));
            }
        }
        if !finding.unanchored_locations.is_empty() {
            body.push_str("  Unanchored locations included in summary:\n");
            for location in &finding.unanchored_locations {
                body.push_str(&format!(
                    "  - {}:{}\n",
                    location.path,
                    location.start_line.unwrap_or_default()
                ));
            }
        }
    }
    body
}

pub fn validate_artifact(artifact: &mut ReportingArtifact) -> Result<()> {
    let mut ids = BTreeSet::new();
    for finding in &artifact.findings {
        if !ids.insert(finding.id.clone()) {
            bail!("duplicate finding id `{}`", finding.id);
        }
    }
    for no_findings in artifact.no_findings.iter_mut() {
        no_findings.validate()?;
    }
    for verdict in &mut artifact.verdicts {
        verdict.validate(&ids)?;
    }
    for decision in &mut artifact.duplicate_decisions {
        decision.validate(&ids)?;
    }
    Ok(())
}

fn object_schema(properties: Vec<(&str, JsonObject)>, required: Vec<&str>) -> JsonObject {
    let mut props = serde_json::Map::new();
    for (name, schema) in properties {
        props.insert(name.to_string(), Value::Object(schema));
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false
    })
    .as_object()
    .unwrap()
    .clone()
}

fn location_schema() -> JsonObject {
    object_schema(
        vec![
            ("path", string_schema("Repository-relative file path")),
            ("start_line", number_schema("Start line on the PR branch")),
            ("end_line", number_schema("End line on the PR branch")),
        ],
        vec!["path"],
    )
}

fn command_transcript_schema() -> JsonObject {
    object_schema(
        vec![
            ("command", string_schema("Command that was run")),
            ("branch_or_commit", string_schema("Branch or commit tested")),
            ("key_output", string_schema("Relevant output excerpt")),
            ("interpretation", string_schema("What this output proves")),
        ],
        vec![
            "command",
            "branch_or_commit",
            "key_output",
            "interpretation",
        ],
    )
}

fn string_schema(description: &str) -> JsonObject {
    json!({"type": "string", "description": description})
        .as_object()
        .unwrap()
        .clone()
}

fn bool_schema(description: &str) -> JsonObject {
    json!({"type": "boolean", "description": description})
        .as_object()
        .unwrap()
        .clone()
}

fn number_schema(description: &str) -> JsonObject {
    json!({"type": "integer", "minimum": 1, "description": description})
        .as_object()
        .unwrap()
        .clone()
}

fn array_schema(items: JsonObject) -> JsonObject {
    json!({"type": "array", "items": items})
        .as_object()
        .unwrap()
        .clone()
}

fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn normalize_scalar(value: String) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_skills(skills: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<_> = skills
        .into_iter()
        .map(|skill| skill.trim().to_string())
        .filter(|skill| !skill.is_empty())
        .collect();
    if normalized.is_empty() {
        normalized.push("none".to_string());
    }
    normalized.sort();
    normalized.dedup();
    normalized
}

fn collect_skills(artifact: &ReportingArtifact) -> Vec<String> {
    let mut skills = Vec::new();
    for finding in &artifact.findings {
        skills.extend(finding.skills_used.clone());
    }
    if let Some(no_findings) = &artifact.no_findings {
        skills.extend(no_findings.skills_used.clone());
    }
    for verdict in &artifact.verdicts {
        if verdict.confirmed {
            skills.push("verifier".to_string());
        }
    }
    normalize_skills(skills)
}

fn markdown_title(
    artifact: &ReportingArtifact,
    accepted: &[AcceptedFinding],
    already_reported: &[AcceptedFinding],
) -> String {
    if artifact.verifier_failed {
        return "Unverified review".to_string();
    }
    if let Some(finding) = accepted.first() {
        return finding.title.replace('"', "\\\"");
    }
    if !already_reported.is_empty() {
        return "Verified findings already reported".to_string();
    }
    if artifact.no_findings.is_some() {
        return "No verified findings".to_string();
    }
    "No verified PR-introduced findings".to_string()
}

fn severity_rank(severity: &str) -> u8 {
    match severity.to_ascii_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DisclosurePolicy {
        DisclosurePolicy {
            pr_context: PrContext {
                state: "OPEN".to_string(),
                merged: false,
                base_ref_name: "main".to_string(),
                default_branch: "main".to_string(),
                base_commit: "base".to_string(),
                head_commit: "head".to_string(),
            },
            diff_anchors: BTreeMap::from([(
                "src/lib.rs".to_string(),
                BTreeSet::from([10_u32, 11_u32]),
            )]),
        }
    }

    fn accepted_artifact(count: usize) -> ReportingArtifact {
        let findings = (0..count)
            .map(|index| {
                Finding::from_input(
                    index,
                    FindingInput {
                        title: format!("Bug {}", index + 1),
                        severity: "high".to_string(),
                        confidence: "high".to_string(),
                        affected_locations: vec![AffectedLocation {
                            path: "src/lib.rs".to_string(),
                            start_line: Some(10 + index as u32),
                            end_line: None,
                        }],
                        evidence: "evidence".to_string(),
                        skills_used: Vec::new(),
                        body_markdown: format!("body {}", index + 1),
                    },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let verdicts = (0..count)
            .map(|index| Verdict {
                finding_id: format!("F-{}", index + 1),
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
                command_transcripts: vec![CommandTranscript {
                    command: "cargo test".to_string(),
                    branch_or_commit: "head".to_string(),
                    key_output: "failed".to_string(),
                    interpretation: "reproduced".to_string(),
                }],
                rationale: "confirmed".to_string(),
            })
            .collect();

        ReportingArtifact {
            findings,
            verdicts,
            ..Default::default()
        }
    }

    #[test]
    fn parses_added_lines_from_diff() {
        let diff = r#"diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -8,2 +8,4 @@
 old
+new one
 context
+new two
"#;

        let anchors = parse_diff_anchors(diff);

        assert!(anchors["src/lib.rs"].contains(&9));
        assert!(anchors["src/lib.rs"].contains(&11));
    }

    #[test]
    fn accepted_finding_requires_transcript_and_valid_policy() {
        let finding = Finding::from_input(
            0,
            FindingInput {
                title: "Bug".to_string(),
                severity: "High".to_string(),
                confidence: "high".to_string(),
                affected_locations: vec![AffectedLocation {
                    path: "src/lib.rs".to_string(),
                    start_line: Some(10),
                    end_line: None,
                }],
                evidence: "evidence".to_string(),
                skills_used: vec!["rust".to_string()],
                body_markdown: "body".to_string(),
            },
        )
        .unwrap();
        let mut artifact = ReportingArtifact {
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
                command_transcripts: vec![CommandTranscript {
                    command: "cargo test".to_string(),
                    branch_or_commit: "head".to_string(),
                    key_output: "failed".to_string(),
                    interpretation: "reproduced".to_string(),
                }],
                rationale: "confirmed".to_string(),
            }],
            ..Default::default()
        };

        validate_artifact(&mut artifact).unwrap();
        let accepted = artifact.accepted_findings(&policy());

        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].inline_comments[0].line, 10);
    }

    #[test]
    fn accepted_finding_posts_one_inline_comment_per_finding() {
        let finding = Finding::from_input(
            0,
            FindingInput {
                title: "Bug".to_string(),
                severity: "High".to_string(),
                confidence: "high".to_string(),
                affected_locations: vec![
                    AffectedLocation {
                        path: "src/lib.rs".to_string(),
                        start_line: Some(10),
                        end_line: None,
                    },
                    AffectedLocation {
                        path: "src/lib.rs".to_string(),
                        start_line: Some(11),
                        end_line: None,
                    },
                    AffectedLocation {
                        path: "src/lib.rs".to_string(),
                        start_line: Some(11),
                        end_line: None,
                    },
                    AffectedLocation {
                        path: "src/lib.rs".to_string(),
                        start_line: Some(12),
                        end_line: None,
                    },
                ],
                evidence: "evidence".to_string(),
                skills_used: vec!["rust".to_string()],
                body_markdown: "body".to_string(),
            },
        )
        .unwrap();
        let mut artifact = ReportingArtifact {
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
                command_transcripts: vec![CommandTranscript {
                    command: "cargo test".to_string(),
                    branch_or_commit: "head".to_string(),
                    key_output: "failed".to_string(),
                    interpretation: "reproduced".to_string(),
                }],
                rationale: "confirmed".to_string(),
            }],
            ..Default::default()
        };

        validate_artifact(&mut artifact).unwrap();
        let accepted = artifact.accepted_findings(&policy());

        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].inline_comments.len(), 1);
        assert_eq!(accepted[0].inline_comments[0].line, 10);
        assert_eq!(accepted[0].additional_locations.len(), 1);
        assert_eq!(accepted[0].additional_locations[0].start_line, Some(11));
        assert_eq!(accepted[0].unanchored_locations.len(), 1);
        assert_eq!(accepted[0].unanchored_locations[0].start_line, Some(12));

        let summary = review_summary_body(&accepted);
        assert!(summary.contains("Additional locations included in summary"));
        assert!(summary.contains("src/lib.rs:11"));
        assert!(summary.contains("Unanchored locations included in summary"));
        assert!(summary.contains("src/lib.rs:12"));
    }

    #[test]
    fn missing_command_transcript_suppresses_disclosure() {
        let finding = Finding::from_input(
            0,
            FindingInput {
                title: "Bug".to_string(),
                severity: "high".to_string(),
                confidence: "high".to_string(),
                affected_locations: Vec::new(),
                evidence: "evidence".to_string(),
                skills_used: Vec::new(),
                body_markdown: "body".to_string(),
            },
        )
        .unwrap();
        let artifact = ReportingArtifact {
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
                final_comment_body: Some("body".to_string()),
                affected_locations: Vec::new(),
                command_transcripts: Vec::new(),
                rationale: "confirmed".to_string(),
            }],
            ..Default::default()
        };

        assert!(artifact.accepted_findings(&policy()).is_empty());
    }

    #[test]
    fn duplicate_decision_with_match_suppresses_publishable_finding() {
        let mut artifact = accepted_artifact(2);
        artifact.duplicate_decisions.push(DuplicateDecision {
            finding_id: "F-1".to_string(),
            already_reported: true,
            matching_comment_ids: vec![101],
            confidence: "high".to_string(),
            rationale: "same issue already reported".to_string(),
        });

        validate_artifact(&mut artifact).unwrap();

        let publishable = artifact.publishable_findings(&policy());
        let already_reported = artifact.already_reported_findings(&policy());
        assert_eq!(publishable.len(), 1);
        assert_eq!(publishable[0].finding_id, "F-2");
        assert_eq!(already_reported.len(), 1);
        assert_eq!(already_reported[0].finding_id, "F-1");
    }

    #[test]
    fn duplicate_decision_without_matching_ids_does_not_suppress() {
        let mut artifact = accepted_artifact(1);
        artifact.duplicate_decisions.push(DuplicateDecision {
            finding_id: "F-1".to_string(),
            already_reported: true,
            matching_comment_ids: Vec::new(),
            confidence: "high".to_string(),
            rationale: "claimed duplicate without concrete comment".to_string(),
        });

        validate_artifact(&mut artifact).unwrap();

        assert_eq!(artifact.publishable_findings(&policy()).len(), 1);
        assert!(artifact.already_reported_findings(&policy()).is_empty());
    }

    #[test]
    fn render_markdown_includes_already_reported_section() {
        let mut artifact = accepted_artifact(1);
        artifact.duplicate_decisions.push(DuplicateDecision {
            finding_id: "F-1".to_string(),
            already_reported: true,
            matching_comment_ids: vec![42],
            confidence: "high".to_string(),
            rationale: "existing review comment reports the same root issue".to_string(),
        });
        validate_artifact(&mut artifact).unwrap();

        let markdown = render_markdown("owner/repo", 7, &artifact, Some(&policy()));

        assert!(markdown.contains("status: already-reported"));
        assert!(markdown.contains("## Already Reported"));
        assert!(markdown.contains("matched existing comment ids [42]"));
        assert!(!markdown.contains("## Suppressed Candidates\n- F-1"));
    }

    #[test]
    fn normalizes_existing_pr_comments_from_github_shapes() {
        let issue = json!({
            "id": 1,
            "body": "top-level body",
            "html_url": "https://github.test/comment/1",
            "user": { "login": "alice" }
        });
        let inline = json!({
            "id": 2,
            "body": "inline body",
            "path": "src/lib.rs",
            "line": 10,
            "html_url": "https://github.test/comment/2",
            "user": { "login": "bob" }
        });
        let review = json!({
            "id": 3,
            "body": "review body",
            "html_url": "https://github.test/review/3",
            "user": { "login": "carol" }
        });

        let issue = ExistingPrComment::from_issue_comment(&issue).unwrap();
        let inline = ExistingPrComment::from_inline_comment(&inline).unwrap();
        let review = ExistingPrComment::from_review(&review).unwrap();

        assert_eq!(issue.kind, "top-level");
        assert_eq!(issue.author.as_deref(), Some("alice"));
        assert_eq!(inline.kind, "inline");
        assert_eq!(inline.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(inline.line, Some(10));
        assert_eq!(review.kind, "review");
        assert_eq!(review.body, "review body");
    }
}
