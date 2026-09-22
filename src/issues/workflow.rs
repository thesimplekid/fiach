use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Result, ensure};
use nostr::hashes::{Hash, sha256};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{
    Decision, Item, Route,
    config::{IssueConfig, Project},
    github::{self, Github, git},
    triage::{Triage, route_label},
    worker,
};

const RECORDS: TableDefinition<&str, &str> = TableDefinition::new("issue_workflow_v1");
const ANSWERS: TableDefinition<&str, &str> = TableDefinition::new("issue_jev_answers_v1");

#[derive(Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    retry: Option<Retry>,
    fingerprint: String,
    decision: Option<Decision>,
    attempted: Option<String>,
    pr: Option<String>,
    pending: Option<Publication>,
}
#[derive(Serialize, Deserialize)]
struct Retry {
    fingerprint: String,
    failures: u32,
    not_before: u64,
}
#[derive(Serialize, Deserialize)]
struct Publication {
    branch: String,
    commit: String,
    base_branch: String,
    base_sha: String,
    issue_key: String,
    summary: String,
    // Old journals lack host-enforced path policy and cannot authorize publication.
    #[serde(default)]
    area_policy: Option<String>,
}
pub(super) struct Store(Database);
impl Store {
    fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            tx.open_table(RECORDS)?;
            tx.open_table(ANSWERS)?;
        }
        tx.commit()?;
        Ok(Self(db))
    }
    fn get(&self, key: &str) -> Result<Record> {
        let tx = self.0.begin_read()?;
        let table = tx.open_table(RECORDS)?;
        let value = table.get(key)?;
        value
            .map(|v| serde_json::from_str(v.value()).map_err(Into::into))
            .unwrap_or_else(|| Ok(Record::default()))
    }
    fn put(&self, key: &str, record: &Record) -> Result<()> {
        let value = serde_json::to_string(record)?;
        let tx = self.0.begin_write()?;
        {
            tx.open_table(RECORDS)?.insert(key, value.as_str())?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(super) fn answer(&self, key: &str) -> Result<Option<jev_sdk::SystemOneResponse>> {
        let tx = self.0.begin_read()?;
        let table = tx.open_table(ANSWERS)?;
        table
            .get(key)?
            .map(|v| serde_json::from_str(v.value()).map_err(Into::into))
            .transpose()
    }
    pub(super) fn save_answer(&self, key: &str, answer: &jev_sdk::SystemOneResponse) -> Result<()> {
        let value = serde_json::to_string(answer)?;
        let tx = self.0.begin_write()?;
        {
            tx.open_table(ANSWERS)?.insert(key, value.as_str())?;
        }
        tx.commit()?;
        Ok(())
    }
}

pub async fn run(
    config: IssueConfig,
    watch: bool,
    only: Option<u64>,
    cancel: CancellationToken,
) -> Result<()> {
    config.validate()?;
    if only.is_some() {
        ensure!(
            config.repos.len() == 1,
            "--issue requires exactly one configured repository"
        );
    }
    tracing::info!(
        repos = ?config.repos.iter().map(|project| &project.repo).collect::<Vec<_>>(),
        interval_secs = config.interval_secs,
        publish = config.publish,
        auto_fix = config.auto_fix,
        worker_configured = config.worker.is_some(),
        watch,
        issue = ?only,
        "Starting issue workflow"
    );
    tokio::fs::create_dir_all(&config.scratch_dir).await?;
    // redb's exclusive file lock also prevents concurrent issue publishers using this state file.
    let store = Store::open(&config.state_path)?;
    let github = loop {
        match Github::new().await {
            Ok(github) => break github,
            Err(error) if watch && !github::rate_limit_wait().is_zero() => {
                tracing::warn!(error = %format!("{error:#}"), "Waiting for GitHub quota before starting issue workflow");
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(github::rate_limit_wait()) => {}
                }
            }
            Err(error) => return Err(error),
        }
    };
    let mut cursors: HashMap<String, u64> = HashMap::new();
    loop {
        let started = std::time::Instant::now();
        tracing::info!("Starting issue polling cycle");
        let mut failures = 0;
        for project in &config.repos {
            if !github::rate_limit_wait().is_zero()
                || !crate::jev::cooldown_wait(&config.jev_base_url).is_zero()
            {
                failures = failures.max(1);
                break;
            }
            tracing::info!(repo = %project.repo, "Collecting issue and PR inventory");
            let inventory = match github.inventory(&project.repo).await {
                Ok(items) => items,
                Err(error) => {
                    tracing::error!(repo = %project.repo, error = %format!("{error:#}"), "Issue inventory failed");
                    failures += 1;
                    continue;
                }
            };
            let cursor = cursors.get(&project.repo).copied().unwrap_or(0);
            let targets = inventory
                .iter()
                .filter(|i| i.open && !i.is_pr && only.is_none_or(|n| n == i.number));
            let targets: Vec<_> = targets
                .clone()
                .filter(|i| i.number > cursor)
                .chain(targets.filter(|i| i.number <= cursor))
                .collect();
            tracing::info!(
                repo = %project.repo,
                inventory_items = inventory.len(),
                eligible_issues = targets.len(),
                max_items = config.max_items,
                "Issue inventory collected"
            );
            let mut processed = 0;
            let mut cached_or_closed = 0;
            for item in targets {
                if !github::rate_limit_wait().is_zero()
                    || !crate::jev::cooldown_wait(&config.jev_base_url).is_zero()
                {
                    failures = failures.max(1);
                    break;
                }
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let worked = match process(
                    &config,
                    project,
                    item.number,
                    &inventory,
                    &github,
                    &store,
                    &cancel,
                )
                .await
                {
                    Ok(worked) => worked,
                    Err(error) => {
                        tracing::error!(repo = %project.repo, issue = item.number, error = %format!("{error:#}"), "Issue workflow failed; no fix authorized");
                        failures += 1;
                        true
                    }
                };
                cursors.insert(project.repo.clone(), item.number);
                if worked {
                    processed += 1;
                } else {
                    cached_or_closed += 1;
                }
                if processed >= config.max_items {
                    break;
                }
            }
            tracing::info!(repo = %project.repo, processed, cached_or_closed, "Issue repository pass complete");
        }
        tracing::info!(
            failures,
            elapsed_secs = started.elapsed().as_secs(),
            "Issue polling cycle complete"
        );
        if !watch {
            ensure!(failures == 0, "{failures} issue workflow operations failed");
            return Ok(());
        }
        let jev_cooldown = crate::jev::cooldown_wait(&config.jev_base_url);
        let cooldown = github::rate_limit_wait().max(jev_cooldown);
        let wait = if cooldown.is_zero() {
            Duration::from_secs(config.interval_secs)
        } else {
            cooldown
        };
        tracing::info!(
            interval_secs = wait.as_secs(),
            rate_limited = !github::rate_limit_wait().is_zero(),
            jev_cooldown_secs = jev_cooldown.as_secs(),
            "Waiting for next issue poll"
        );
        tokio::select! { _ = cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(wait) => {} }
    }
}

async fn process(
    config: &IssueConfig,
    project: &Project,
    number: u64,
    inventory: &[Item],
    github: &Github,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<bool> {
    let key = format!("{}#{number}", project.repo);
    let mut record = store.get(&key)?;
    if config.publish
        && let Some(pending) = &record.pending
    {
        // Publication was journaled before pushing. Never blindly create a second PR.
        if let Some((url, _)) = github.branch_pr(&project.repo, &pending.branch).await? {
            record.pr = Some(url);
        } else if config.auto_fix && config.worker.is_some() {
            ensure!(
                pending.area_policy.as_deref() == Some(digest(&project.areas)?.as_str()),
                "Pending fix was not verified under the current area policy; maintainer intervention required"
            );
            let reference = github::api(
                &format!("repos/{}/git/ref/heads/{}", project.repo, pending.branch),
                "GET",
                None,
            )
            .await?;
            ensure!(
                reference["object"]["sha"] == pending.commit,
                "Pending branch is absent or changed; maintainer intervention required"
            );
            let fresh = github.issue(&project.repo, number).await?;
            ensure!(
                fresh.open && content_key(&fresh)? == pending.issue_key,
                "Issue changed since interrupted publication"
            );
            let current_base = github::api(
                &format!("repos/{}/commits/{}", project.repo, pending.base_branch),
                "GET",
                None,
            )
            .await?;
            ensure!(
                current_base["sha"] == pending.base_sha,
                "Default branch changed since interrupted publication"
            );
            let inventory = github.inventory(&project.repo).await?;
            let mut recheck = Triage::new(
                config.max_jev_cost_usd,
                &config.jev_base_url,
                store,
                &project.repo,
            )?;
            ensure!(
                recheck
                    .classify(project, &fresh, &inventory, github)
                    .await?
                    .route
                    == Route::Ready,
                "Issue no longer eligible for interrupted publication"
            );
            record.pr = Some(
                github
                    .open_pr(
                        &project.repo,
                        &pending.branch,
                        &pending.base_branch,
                        number,
                        &pending.summary,
                    )
                    .await?,
            );
        }
        if record.pr.is_some() {
            record.pending = None;
            store.put(&key, &record)?;
        }
    }
    let issue = github.issue(&project.repo, number).await?;
    if !issue.open || issue.is_pr {
        return Ok(false);
    }
    let fingerprint = digest(&(
        fingerprint(project, &issue, inventory, config.publish, config.auto_fix)?,
        &config.worker,
        config.max_jev_cost_usd,
        &config.jev_base_url,
        super::triage::CACHE_VERSION,
        crate::jev::MODEL,
    ))?;
    if record.fingerprint == fingerprint {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    if let Some(retry) = &record.retry
        && retry.fingerprint == fingerprint
        && now < retry.not_before
    {
        tracing::info!(repo = %project.repo, issue = number, retry_at = retry.not_before, "Issue retry deferred");
        return Ok(false);
    }
    tracing::info!(repo = %project.repo, issue = number, "Triaging issue");
    let mut triage = Triage::new(
        config.max_jev_cost_usd,
        &config.jev_base_url,
        store,
        &project.repo,
    )?;
    let mut decision = match triage.classify(project, &issue, inventory, github).await {
        Ok(decision) => decision,
        Err(error) => {
            let failures = record
                .retry
                .as_ref()
                .filter(|r| r.fingerprint == fingerprint)
                .map_or(1, |r| r.failures.saturating_add(1));
            let delay = (60_u64 * (1_u64 << failures.saturating_sub(1).min(6))).min(3600);
            let failed_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            record.retry = Some(Retry {
                fingerprint,
                failures,
                not_before: failed_at + delay,
            });
            store.put(&key, &record)?;
            return Err(error);
        }
    };
    record.retry = None;
    // Persist model judgments before any side effect. The fingerprint is committed only after marking succeeds.
    record.decision = Some(decision.clone());
    store.put(&key, &record)?;
    if config.publish
        && config.auto_fix
        && decision.route == Route::Ready
        && record.pr.is_none()
        && record.pending.is_none()
    {
        let attempt_key = content_key(&issue)?;
        if record.attempted.as_deref() == Some(&attempt_key) {
            reroute(
                project,
                &mut decision,
                Route::NeedsDecision,
                "A fix was already attempted for this issue content. A maintainer must inspect the previous run before another attempt.",
            );
        } else if let Some(worker_config) = &config.worker {
            record.attempted = Some(attempt_key);
            store.put(&key, &record)?;
            match worker::fix(worker_config, &issue, project, &config.scratch_dir, cancel).await {
                Ok(fix) if fix.report.status != "candidate" => {
                    let route = if fix.report.status == "needs_info" {
                        Route::NeedsInfo
                    } else {
                        Route::NeedsDecision
                    };
                    reroute(project, &mut decision, route, &fix.report.summary);
                }
                Ok(fix) => {
                    let fresh = github.issue(&project.repo, number).await?;
                    ensure!(
                        fresh.open && content_key(&fresh)? == content_key(&issue)?,
                        "Issue changed during fix; withholding publication"
                    );
                    let inventory = github.inventory(&project.repo).await?;
                    let mut recheck = Triage::new(
                        config.max_jev_cost_usd,
                        &config.jev_base_url,
                        store,
                        &project.repo,
                    )?;
                    decision = recheck
                        .classify(project, &fresh, &inventory, github)
                        .await?;
                    if decision.route == Route::Ready {
                        decision
                            .labels
                            .retain(|label| !project.areas.iter().any(|a| &a.label == label));
                        decision.labels.extend(fix.area_labels.clone());
                        let base = git(
                            fix.checkout.path(),
                            &["ls-remote", "origin", &format!("refs/heads/{}", fix.branch)],
                        )
                        .await?;
                        ensure!(
                            base.split_whitespace().next() == Some(&fix.base),
                            "Default branch changed; withholding stale fix"
                        );
                        let branch = format!("fiach/issue-{number}");
                        if let Some((url, _)) = github.branch_pr(&project.repo, &branch).await? {
                            record.pr = Some(url);
                        } else {
                            git(fix.checkout.path(), &["add", "--all"]).await?;
                            git(
                                fix.checkout.path(),
                                &[
                                    "-c",
                                    "user.name=Fiach",
                                    "-c",
                                    "user.email=fiach@localhost",
                                    "-c",
                                    "commit.gpgsign=false",
                                    "commit",
                                    "-m",
                                    &format!("Address issue #{number}"),
                                ],
                            )
                            .await?;
                            let commit = git(fix.checkout.path(), &["rev-parse", "HEAD"])
                                .await?
                                .trim()
                                .to_owned();
                            let summary = "The isolated coding agent prepared this patch. The host ran the same regression command against the original code with the new test (failed) and the complete patch (passed). An independent verifier approved the fix. Full evidence is retained in the local issue artifacts.".to_owned();
                            record.pending = Some(Publication {
                                branch: branch.clone(),
                                commit: commit.clone(),
                                base_branch: fix.branch.clone(),
                                base_sha: fix.base.clone(),
                                issue_key: content_key(&issue)?,
                                summary: summary.clone(),
                                area_policy: Some(digest(&project.areas)?),
                            });
                            store.put(&key, &record)?;
                            // Empty expected value makes publication create-only; never overwrite anyone's branch.
                            git(
                                fix.checkout.path(),
                                &[
                                    "push",
                                    &format!("--force-with-lease=refs/heads/{branch}:"),
                                    "origin",
                                    &format!("{commit}:refs/heads/{branch}"),
                                ],
                            )
                            .await?;
                            record.pr = Some(
                                github
                                    .open_pr(&project.repo, &branch, &fix.branch, number, &summary)
                                    .await?,
                            );
                            record.pending = None;
                            store.put(&key, &record)?;
                        }
                        reroute(
                            project,
                            &mut decision,
                            Route::Addressed,
                            "A verified draft fix is ready for maintainer review.",
                        );
                    }
                    // Keep reviewable host artifacts independently of temporary workspaces.
                    let artifacts = config
                        .state_path
                        .with_extension("artifacts")
                        .join(project.repo.replace('/', "_"))
                        .join(number.to_string());
                    tokio::fs::create_dir_all(&artifacts).await?;
                    tokio::fs::write(artifacts.join("patch.diff"), &fix.patch).await?;
                    tokio::fs::write(
                        artifacts.join("report.json"),
                        serde_json::to_vec_pretty(&fix.report)?,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(repo = %project.repo, issue = number, %error, "Automatic fix stopped");
                    reroute(
                        project,
                        &mut decision,
                        Route::NeedsDecision,
                        "The automatic investigation or independent verification did not complete successfully. A maintainer must inspect the worker results.",
                    );
                }
            }
        }
    }
    if record.pending.is_some() && (!config.auto_fix || config.worker.is_none()) {
        reroute(
            project,
            &mut decision,
            Route::NeedsDecision,
            "Pending fix publication is paused because automatic fixes or the worker are disabled.",
        );
    }
    if record.pr.is_some() {
        let open = github
            .branch_pr(&project.repo, &format!("fiach/issue-{number}"))
            .await?
            .is_some_and(|(_, open)| open);
        reroute(
            project,
            &mut decision,
            if open {
                Route::Addressed
            } else {
                Route::NeedsDecision
            },
            if open {
                "A Fiach draft PR already exists for this issue. Further changes require maintainer review."
            } else {
                "The previous Fiach PR is no longer open. A maintainer must decide how to proceed."
            },
        );
    }
    let mut body = decision.explanation.clone();
    if !decision.matches.is_empty() {
        body.push_str(&format!("\n\nMatching work: {}.", links(&decision.matches)));
    }
    if !decision.related.is_empty() {
        body.push_str(&format!("\n\nRelated work: {}.", links(&decision.related)));
    }
    if let Some(url) = &record.pr {
        body.push_str(&format!("\n\nDraft PR: {url}"));
    }
    if config.publish {
        let fresh = github.issue(&project.repo, number).await?;
        ensure!(
            fresh.open && content_key(&fresh)? == content_key(&issue)?,
            "Issue changed before marking"
        );
        github
            .mark(project, number, &decision.labels, &body)
            .await?;
    }
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"repo":project.repo,"issue":number,"published":config.publish,"decision":decision,"pr":record.pr})
        )?
    );
    record.fingerprint = fingerprint;
    record.decision = Some(decision);
    store.put(&key, &record)?;
    tracing::info!(
        repo = %project.repo,
        issue = number,
        route = ?record.decision.as_ref().map(|decision| &decision.route),
        published = config.publish,
        "Issue triage complete"
    );
    Ok(true)
}

fn reroute(project: &Project, decision: &mut Decision, route: Route, explanation: &str) {
    decision
        .labels
        .retain(|l| l != route_label(project, &decision.route));
    decision
        .labels
        .push(route_label(project, &route).to_owned());
    decision.route = route;
    decision.explanation = explanation.to_owned();
}
fn links(numbers: &[u64]) -> String {
    numbers
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}
pub(super) fn digest(value: &impl Serialize) -> Result<String> {
    Ok(sha256::Hash::hash(&serde_json::to_vec(value)?).to_string())
}
fn content_key(issue: &Item) -> Result<String> {
    digest(&(
        issue.number,
        &issue.title,
        &issue.body,
        &issue.comments,
        issue.open,
    ))
}
fn fingerprint(
    project: &Project,
    issue: &Item,
    inventory: &[Item],
    publish: bool,
    auto_fix: bool,
) -> Result<String> {
    // Bot labels/comments change issue updated_at. Exclude that timestamp to avoid self-trigger loops.
    // PR update timestamps detect new commits, even on PRs without a linked issue.
    let candidates: Vec<_> = inventory
        .iter()
        .filter(|i| i.number != issue.number && (!i.is_pr || i.open))
        .map(|i| {
            (
                i.number,
                &i.title,
                &i.body,
                i.open,
                i.is_pr,
                &i.comments,
                if i.is_pr { i.updated_at.as_str() } else { "" },
            )
        })
        .collect();
    digest(&(project, content_key(issue)?, candidates, publish, auto_fix))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn item() -> Item {
        Item {
            number: 1,
            title: "bug".into(),
            body: "repro".into(),
            open: true,
            is_pr: false,
            updated_at: "a".into(),
            comments: vec![],
        }
    }
    fn project() -> Project {
        Project {
            repo: "owner/repo".into(),
            areas: vec![],
            labels: Default::default(),
        }
    }
    #[test]
    fn bot_updates_do_not_loop_but_new_unlinked_prs_invalidate_triage() {
        let mut issue = item();
        let old = fingerprint(&project(), &issue, &[], true, true).unwrap();
        issue.updated_at = "b".into();
        assert_eq!(
            old,
            fingerprint(&project(), &issue, &[], true, true).unwrap()
        );
        let mut pr = item();
        pr.number = 2;
        pr.is_pr = true;
        assert_ne!(
            old,
            fingerprint(&project(), &issue, &[pr], true, true).unwrap()
        );
        issue.comments.push("maintainer: intended behavior".into());
        assert_ne!(
            old,
            fingerprint(&project(), &issue, &[], true, true).unwrap()
        );
    }
    #[test]
    fn publication_journal_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut record = Record {
            attempted: Some("issue-content".into()),
            ..Default::default()
        };
        record.pending = Some(Publication {
            branch: "fiach/issue-1".into(),
            commit: "abc".into(),
            base_branch: "main".into(),
            base_sha: "base".into(),
            issue_key: "issue-content".into(),
            summary: "fix".into(),
            area_policy: None,
        });
        {
            let store = Store::open(&path).unwrap();
            store.put("owner/repo#1", &record).unwrap();
        }
        let record = Store::open(&path).unwrap().get("owner/repo#1").unwrap();
        assert_eq!(record.pending.unwrap().commit, "abc");
        assert_eq!(record.attempted.as_deref(), Some("issue-content"));
    }
}
