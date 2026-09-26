//! Complete-inventory screening, bounded investigation, and durable diagnostics.
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
};

use anyhow::{Context, Result};
use goose_providers::decision::{DecisionAnswer, DecisionQuestion, DecisionResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::jev::{self, UsageStats};

use super::{
    Item,
    config::CoverageConfig,
    github::{Github, mentions_work},
    triage::{WorkEvidence, choice, comparison_question, comparison_request, validate_answers},
    workflow::{Store, digest},
};

// Independent of routing policy. Prompts, task kind and full evidence also enter keys.
const ANSWER_SCHEMA: &str = "issue-coverage-v1";
// Large single candidates are sent individually, never truncated to fit a batch.
const BATCH_BYTES: usize = 64 * 1024;
const BATCH_QUESTION: &str = r#"Compare only state.candidates["{candidate}"] with state.issue. Other candidates are independent questions, not evidence for this comparison. {question}"#;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    Screening,
    Investigation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    ModelUncertain,
    LowConfidence,
    RequestTooLarge,
    ProviderContextLimit,
    DiffUnavailable,
    InvestigationLimit,
    InvestigationBudget,
    ScreeningBudget,
    ProviderError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ComparisonRecord {
    pub repo: String,
    pub issue: u64,
    pub candidate: u64,
    pub is_pr: bool,
    pub task_kind: String,
    pub stage: Stage,
    pub evidence_fingerprint: String,
    pub issue_fingerprint: String,
    pub candidate_fingerprint: String,
    pub model: String,
    pub selected: Option<String>,
    pub probability: Option<f64>,
    pub probabilities: Option<BTreeMap<String, f64>>,
    pub threshold: f64,
    pub confidence: Option<f64>,
    pub outcome: String,
    pub reason: Option<Reason>,
    pub cache_hit: bool,
    pub batch_size: usize,
}

#[derive(Default)]
struct Stats {
    candidates: usize,
    requests: usize,
    cache_hits: usize,
    investigations: usize,
    reasons: BTreeMap<Reason, usize>,
}

pub(super) struct Coverage<'a> {
    client: &'a jev::Client,
    store: &'a Store,
    scope: (&'a str, &'a str),
    budget: f64,
    usage: &'a mut UsageStats,
    config: &'a CoverageConfig,
    stats: Stats,
}

struct Pending<'a> {
    candidate: &'a Item,
    request: jev::Request,
    key: String,
}

struct Judgment {
    response: Option<DecisionResponse>,
    reason: Option<Reason>,
    cache_hit: bool,
    batch_size: usize,
}

impl Judgment {
    fn blocked(reason: Reason) -> Self {
        Self {
            response: None,
            reason: Some(reason),
            cache_hit: false,
            batch_size: 0,
        }
    }
}

impl<'a> Coverage<'a> {
    pub fn new(
        client: &'a jev::Client,
        store: &'a Store,
        scope: (&'a str, &'a str),
        budget: f64,
        usage: &'a mut UsageStats,
        config: &'a CoverageConfig,
    ) -> Self {
        Self {
            client,
            store,
            scope,
            budget,
            usage,
            config,
            stats: Stats::default(),
        }
    }

    pub async fn run(
        mut self,
        issue: &Item,
        inventory: &[Item],
        kind: &str,
        github: &Github,
    ) -> Result<WorkEvidence> {
        let initial_cost = self.usage.cost_usd;
        let result = self.compare_inventory(issue, inventory, kind, github).await;
        tracing::info!(
            repo = self.scope.1, issue = issue.number,
            candidates = self.stats.candidates,
            requests = self.stats.requests, cache_hits = self.stats.cache_hits,
            investigations = self.stats.investigations,
            reason_observations = ?self.stats.reasons,
            cost_usd = self.usage.cost_usd - initial_cost,
            unresolved = result.as_ref().ok().map(|work| work.unresolved.len()),
            completed = result.is_ok(),
            "Issue coverage pass complete"
        );
        result
    }

    async fn compare_inventory(
        &mut self,
        issue: &Item,
        inventory: &[Item],
        kind: &str,
        github: &Github,
    ) -> Result<WorkEvidence> {
        let mut candidates: Vec<_> = inventory
            .iter()
            .filter(|c| c.number != issue.number && (!c.is_pr || c.open))
            .collect();
        // Priority changes order only; no candidate is discarded by a heuristic.
        self.stats.candidates = candidates.len();
        let paths = path_terms(issue);
        candidates.sort_by_cached_key(|candidate| {
            (
                !explicit_reference(issue, candidate, self.scope.1),
                Reverse(paths.intersection(&path_terms(candidate)).count()),
                candidate.number,
            )
        });
        let mut screened = BTreeMap::new();
        let mut batch = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            if index % 25 == 0 {
                tokio::task::yield_now().await;
                tracing::info!(
                    repo = self.scope.1,
                    issue = issue.number,
                    compared = index,
                    candidate = candidate.number,
                    "Screening issue coverage"
                );
            }
            let Some(request) = comparison_request(issue, candidate, None, kind, false)? else {
                let outcome = self.record(
                    issue,
                    candidate,
                    kind,
                    Stage::Screening,
                    None,
                    Judgment::blocked(Reason::RequestTooLarge),
                )?;
                screened.insert(candidate.number, outcome);
                continue;
            };
            let key = self.key(&request)?;
            if let Some(response) = self.store.answer(&key)? {
                validate_answers(&response, &request.questions)?;
                self.stats.cache_hits += 1;
                let outcome = self.record(
                    issue,
                    candidate,
                    kind,
                    Stage::Screening,
                    Some(&key),
                    Judgment {
                        response: Some(response),
                        reason: None,
                        cache_hit: true,
                        batch_size: 0,
                    },
                )?;
                screened.insert(candidate.number, outcome);
                continue;
            }
            batch.push(Pending {
                candidate,
                request,
                key,
            });
            if batch.len() > 1 && batch_request(&batch)?.encoded_len()? > BATCH_BYTES {
                let last = batch.pop().context("Nonempty coverage batch")?;
                self.screen_batch(issue, kind, std::mem::take(&mut batch), &mut screened)
                    .await?;
                batch.push(last);
            }
            if batch.len() >= self.config.batch_size {
                self.screen_batch(issue, kind, std::mem::take(&mut batch), &mut screened)
                    .await?;
            }
        }
        if !batch.is_empty() {
            self.screen_batch(issue, kind, batch, &mut screened).await?;
        }

        let investigation_budget = self
            .budget
            .min(self.usage.cost_usd + self.config.max_investigation_cost_usd);
        let mut work = WorkEvidence::default();
        for (index, candidate) in candidates.into_iter().enumerate() {
            if index % 25 == 0 {
                tokio::task::yield_now().await;
            }
            let outcome = screened
                .get(&candidate.number)
                .context("Missing coverage screen result")?;
            if outcome == "different" {
                continue;
            }
            if !candidate.is_pr && outcome != "uncertain" {
                work.record(issue, candidate, outcome, false);
                continue;
            }
            // A distinct, focused comparison resolves ambiguity. PR investigation includes
            // the complete bounded diff (including paths); no summary can prove coverage.
            let result = self
                .investigate(issue, candidate, kind, github, investigation_budget)
                .await?;
            work.record(issue, candidate, &result, candidate.is_pr);
        }
        Ok(work)
    }

    async fn screen_batch(
        &mut self,
        issue: &Item,
        kind: &str,
        batch: Vec<Pending<'_>>,
        screened: &mut BTreeMap<u64, String>,
    ) -> Result<()> {
        // Provider context limits may be smaller than the local byte bound. Split
        // without truncation; only a rejected singleton becomes unresolved.
        let mut pending = vec![batch];
        while let Some(mut batch) = pending.pop() {
            let request = batch_request(&batch)?;
            let questions = request.questions.clone();
            let estimate =
                (request.encoded_len()? + 1024) as f64 * jev::INPUT_PRICE_PER_MILLION / 1_000_000.0;
            if self.usage.cost_usd + estimate > self.budget {
                if batch.len() > 1 {
                    let rest = batch.split_off(batch.len() / 2);
                    pending.push(rest);
                    pending.push(batch);
                    continue;
                }
                for item in &batch {
                    self.record(
                        issue,
                        item.candidate,
                        kind,
                        Stage::Screening,
                        Some(&item.key),
                        Judgment::blocked(Reason::ScreeningBudget),
                    )?;
                }
                anyhow::bail!("Insufficient budget for coverage screening request");
            }
            self.stats.requests += 1;
            let result = jev::evaluate(self.client, request, Some(self.budget), self.usage).await;
            let response = match result {
                Ok(response) => {
                    if let Err(error) = validate_answers(&response, &questions) {
                        for item in &batch {
                            self.record(
                                issue,
                                item.candidate,
                                kind,
                                Stage::Screening,
                                Some(&item.key),
                                Judgment::blocked(Reason::ProviderError),
                            )?;
                        }
                        return Err(error).context("Invalid coverage screening response");
                    }
                    response
                }
                Err(error) if jev::is_size_rejection(&error) => {
                    if batch.len() > 1 {
                        let rest = batch.split_off(batch.len() / 2);
                        pending.push(rest);
                        pending.push(batch);
                        continue;
                    }
                    for item in batch {
                        let outcome = self.record(
                            issue,
                            item.candidate,
                            kind,
                            Stage::Screening,
                            Some(&item.key),
                            Judgment::blocked(Reason::ProviderContextLimit),
                        )?;
                        screened.insert(item.candidate.number, outcome);
                    }
                    continue;
                }
                Err(error) => {
                    for item in &batch {
                        self.record(
                            issue,
                            item.candidate,
                            kind,
                            Stage::Screening,
                            Some(&item.key),
                            Judgment::blocked(Reason::ProviderError),
                        )?;
                    }
                    return Err(error).context("Coverage screening request failed");
                }
            };
            for item in &batch {
                let (id, _, _) = comparison_question(item.candidate, None);
                let answer_id = if batch.len() == 1 {
                    id.to_owned()
                } else {
                    format!("candidate_{}", item.candidate.number)
                };
                let answer = response
                    .answers
                    .get(&answer_id)
                    .context("Missing batch answer")?
                    .clone();
                let mut individual = response.clone();
                individual.answers = HashMap::from([(id.to_owned(), answer)]);
                validate_answers(&individual, &item.request.questions)?;
                self.store.save_answer(&item.key, &individual)?;
                let outcome = self.record(
                    issue,
                    item.candidate,
                    kind,
                    Stage::Screening,
                    Some(&item.key),
                    Judgment {
                        response: Some(individual),
                        reason: None,
                        cache_hit: false,
                        batch_size: batch.len(),
                    },
                )?;
                screened.insert(item.candidate.number, outcome);
            }
        }
        Ok(())
    }

    async fn investigate(
        &mut self,
        issue: &Item,
        candidate: &Item,
        kind: &str,
        github: &Github,
        budget: f64,
    ) -> Result<String> {
        if !candidate.is_pr
            && let Some(request) = comparison_request(issue, candidate, None, kind, true)?
        {
            let key = self.key(&request)?;
            if let Some(response) = self.store.answer(&key)? {
                validate_answers(&response, &request.questions)?;
                self.stats.cache_hits += 1;
                return self.record(
                    issue,
                    candidate,
                    kind,
                    Stage::Investigation,
                    Some(&key),
                    Judgment {
                        response: Some(response),
                        reason: None,
                        cache_hit: true,
                        batch_size: 0,
                    },
                );
            }
        }
        if self.stats.investigations >= self.config.max_investigations {
            return self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                None,
                Judgment::blocked(Reason::InvestigationLimit),
            );
        }
        if self.usage.cost_usd >= budget {
            return self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                None,
                Judgment::blocked(Reason::InvestigationBudget),
            );
        }
        self.stats.investigations += 1;
        let diff = if candidate.is_pr {
            match github.pr_diff(self.scope.1, candidate.number).await {
                Ok(Some(diff)) => Some(diff),
                Ok(None) => {
                    return self.record(
                        issue,
                        candidate,
                        kind,
                        Stage::Investigation,
                        None,
                        Judgment::blocked(Reason::DiffUnavailable),
                    );
                }
                Err(error) => {
                    self.record(
                        issue,
                        candidate,
                        kind,
                        Stage::Investigation,
                        None,
                        Judgment::blocked(Reason::DiffUnavailable),
                    )?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let Some(request) = comparison_request(issue, candidate, diff.as_deref(), kind, true)?
        else {
            return self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                None,
                Judgment::blocked(Reason::RequestTooLarge),
            );
        };
        let key = self.key(&request)?;
        if let Some(response) = self.store.answer(&key)? {
            validate_answers(&response, &request.questions)?;
            self.stats.cache_hits += 1;
            return self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                Some(&key),
                Judgment {
                    response: Some(response),
                    reason: None,
                    cache_hit: true,
                    batch_size: 0,
                },
            );
        }
        // Use the same conservative estimate as the provider adapter. Budget blocks
        // are not cached answers, so a later configured budget can resume the work.
        let estimate =
            (request.encoded_len()? + 1024) as f64 * jev::INPUT_PRICE_PER_MILLION / 1_000_000.0;
        if self.usage.cost_usd + estimate > budget {
            return self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                Some(&key),
                Judgment::blocked(Reason::InvestigationBudget),
            );
        }
        let questions = request.questions.clone();
        self.stats.requests += 1;
        match jev::evaluate(self.client, request, Some(budget), self.usage).await {
            Ok(response) => {
                if let Err(error) = validate_answers(&response, &questions) {
                    self.record(
                        issue,
                        candidate,
                        kind,
                        Stage::Investigation,
                        Some(&key),
                        Judgment::blocked(Reason::ProviderError),
                    )?;
                    return Err(error).context("Invalid coverage investigation response");
                }
                self.store.save_answer(&key, &response)?;
                self.record(
                    issue,
                    candidate,
                    kind,
                    Stage::Investigation,
                    Some(&key),
                    Judgment {
                        response: Some(response),
                        reason: None,
                        cache_hit: false,
                        batch_size: 1,
                    },
                )
            }
            Err(error) if jev::is_size_rejection(&error) => self.record(
                issue,
                candidate,
                kind,
                Stage::Investigation,
                Some(&key),
                Judgment::blocked(Reason::ProviderContextLimit),
            ),
            Err(error) => {
                self.record(
                    issue,
                    candidate,
                    kind,
                    Stage::Investigation,
                    Some(&key),
                    Judgment::blocked(Reason::ProviderError),
                )?;
                Err(error).context("Coverage investigation request failed")
            }
        }
    }

    fn key(&self, request: &jev::Request) -> Result<String> {
        let mut questions = serde_json::to_value(&request.questions)?;
        questions.sort_all_objects();
        digest(&(
            ANSWER_SCHEMA,
            BATCH_QUESTION,
            jev::MODEL,
            self.scope,
            &request.state,
            questions,
        ))
    }

    fn record(
        &mut self,
        issue: &Item,
        candidate: &Item,
        kind: &str,
        stage: Stage,
        key: Option<&str>,
        judgment: Judgment,
    ) -> Result<String> {
        let mut record = ComparisonRecord {
            repo: self.scope.1.to_owned(),
            issue: issue.number,
            candidate: candidate.number,
            is_pr: candidate.is_pr,
            task_kind: kind.to_owned(),
            stage,
            evidence_fingerprint: key.map_or_else(
                || {
                    digest(&(
                        ANSWER_SCHEMA,
                        item_fingerprint(issue)?,
                        item_fingerprint(candidate)?,
                        kind,
                        stage,
                    ))
                },
                |key| Ok(key.to_owned()),
            )?,
            issue_fingerprint: item_fingerprint(issue)?,
            candidate_fingerprint: item_fingerprint(candidate)?,
            model: jev::MODEL.to_owned(),
            selected: None,
            probability: None,
            probabilities: None,
            threshold: 0.95,
            confidence: None,
            outcome: "uncertain".to_owned(),
            reason: judgment.reason,
            cache_hit: judgment.cache_hit,
            batch_size: judgment.batch_size,
        };
        if let Some(response) = judgment.response {
            let has_diff = candidate.is_pr && matches!(stage, Stage::Investigation);
            let (id, _, options) = comparison_question(candidate, has_diff.then_some(""));
            record.outcome = choice(&response, id, options, record.threshold)?.to_owned();
            if let Some(DecisionAnswer::Choice {
                choice,
                probabilities,
                confidence,
            }) = response.answers.get(id)
            {
                record.selected = Some(choice.clone());
                record.probability = probabilities.get(choice).copied();
                record.probabilities = Some(
                    probabilities
                        .iter()
                        .map(|(key, value)| (key.clone(), *value))
                        .collect(),
                );
                record.confidence = Some(*confidence);
                record.reason = if choice == "uncertain" {
                    Some(Reason::ModelUncertain)
                } else if record.outcome == "uncertain" {
                    Some(Reason::LowConfidence)
                } else {
                    None
                };
            }
        }
        if let Some(reason) = record.reason {
            *self.stats.reasons.entry(reason).or_default() += 1;
        }
        self.store.save_comparison(&record)?;
        Ok(record.outcome)
    }
}

fn item_fingerprint(item: &Item) -> Result<String> {
    // Match the normalized request evidence; bot timestamps do not change issues.
    let mut item = item.clone();
    if !item.is_pr {
        item.updated_at.clear();
    }
    digest(&item)
}

// Scheduling hint only: path tokens can never exclude a candidate or prove a match.
fn path_terms(item: &Item) -> HashSet<&str> {
    std::iter::once(&item.title)
        .chain(std::iter::once(&item.body))
        .chain(item.comments.iter())
        .flat_map(|text| text.split(|c: char| c.is_whitespace() || "`\"'()[],".contains(c)))
        .filter(|word| word.contains('/') && !word.contains("://"))
        .filter_map(|word| word.split(':').next())
        .collect()
}

fn explicit_reference(issue: &Item, candidate: &Item, repo: &str) -> bool {
    [(issue, candidate.number), (candidate, issue.number)]
        .into_iter()
        .any(|(item, number)| {
            std::iter::once(&item.title)
                .chain(std::iter::once(&item.body))
                .chain(item.comments.iter())
                .any(|text| mentions_work(text, repo, number))
        })
}

fn batch_request(batch: &[Pending<'_>]) -> Result<jev::Request> {
    let first = batch.first().context("Empty coverage batch")?;
    if batch.len() == 1 {
        return Ok(jev::Request {
            state: first.request.state.clone(),
            questions: first.request.questions.clone(),
        });
    }
    let mut candidates = BTreeMap::new();
    let mut questions = HashMap::new();
    for item in batch {
        let (_, question) = item
            .request
            .questions
            .iter()
            .next()
            .context("Missing screen question")?;
        let mut question = question.clone();
        if let DecisionQuestion::Choice { instructions, .. } = &mut question {
            *instructions = BATCH_QUESTION
                .replace("{candidate}", &item.candidate.number.to_string())
                .replace("{question}", instructions);
        }
        candidates.insert(
            item.candidate.number.to_string(),
            item.request.state["candidate"].clone(),
        );
        questions.insert(format!("candidate_{}", item.candidate.number), question);
    }
    Ok(jev::Request {
        state: json!({"issue": first.request.state["issue"], "task_kind": first.request.state["task_kind"], "candidates": candidates, "investigate": false}),
        questions,
    })
}
