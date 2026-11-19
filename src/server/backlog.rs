use std::collections::BTreeMap;

use super::*;
use crate::{
    scheduler::{JobSnapshot, JobStatus, PollBacklog, ReviewTarget},
    state::{FailureClass, ReviewRecord, ReviewStatus},
};

#[derive(Default, Deserialize)]
pub(super) struct Page {
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Snapshot {
    snapshot_at: i64,
    discovery_at: Option<i64>,
    total: usize,
    next_offset: Option<usize>,
    items: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    target: ReviewTarget,
    in_latest_discovery: Option<bool>,
    source: &'static str,
    phase: &'static str,
    job_id: Option<String>,
    job_status: Option<JobStatus>,
    queued_at: Option<i64>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    failure_stage: Option<&'static str>,
    record: Option<Record>,
}

/// Project only observer-safe fields from the durable record.
#[derive(Serialize)]
struct Record {
    status: ReviewStatus,
    timestamp: i64,
    retry_count: u32,
    retry_not_before: Option<i64>,
    needs_intervention: bool,
    failure_class: Option<FailureClass>,
    failure_stage: Option<&'static str>,
}

fn stage(value: Option<&str>) -> Option<&'static str> {
    value.map(|s| match s {
        "discovery" => "discovery",
        "state" => "state",
        "finder" => "finder",
        "execution" => "execution",
        "finalization" => "finalization",
        "daemon-restarted" => "daemon-restarted",
        _ => "unknown",
    })
}

impl Row {
    fn new(target: ReviewTarget) -> Self {
        Self {
            target,
            in_latest_discovery: None,
            source: "history",
            phase: "recorded",
            job_id: None,
            job_status: None,
            queued_at: None,
            started_at: None,
            finished_at: None,
            failure_stage: None,
            record: None,
        }
    }
}

fn record_phase(record: &ReviewRecord, now: i64) -> &'static str {
    match record.status {
        ReviewStatus::Failed if record.needs_intervention => "needs_intervention",
        ReviewStatus::Failed if record.retry_not_before.is_some_and(|at| at > now) => {
            "retry_cooldown"
        }
        ReviewStatus::Failed => "failed",
        ReviewStatus::Queued | ReviewStatus::InProgress => "recorded",
        ReviewStatus::Skipped => "skipped",
        ReviewStatus::Unverified => "unverified",
        ReviewStatus::MarkdownOnly => "markdown_only",
        _ => "completed",
    }
}

fn snapshot(
    reviews: Vec<ReviewRecord>,
    poll: PollBacklog,
    mut jobs: Vec<JobSnapshot>,
    now: i64,
    offset: usize,
    limit: usize,
) -> Snapshot {
    let mut rows = BTreeMap::new();
    for review in reviews {
        let target = ReviewTarget {
            repository: review.repository.clone(),
            pr_number: review.pr_number,
            commit_hash: review.commit_hash.clone(),
            review_kind: review.review_kind.clone(),
        };
        let mut row = Row::new(target.clone());
        row.phase = record_phase(&review, now);
        row.record = Some(Record {
            status: review.status,
            timestamp: review.timestamp,
            retry_count: review.retry_count,
            retry_not_before: review.retry_not_before,
            needs_intervention: review.needs_intervention,
            failure_class: review.failure_class,
            failure_stage: stage(review.failure_stage.as_deref()),
        });
        rows.insert(target, row);
    }
    for (target, eligible) in &poll.deferred {
        let row = rows
            .entry(target.clone())
            .or_insert_with(|| Row::new(target.clone()));
        row.source = "poll";
        row.phase = if *eligible {
            "eligible_waiting"
        } else {
            "candidate"
        };
    }
    // The monotonic suffix breaks ties between attempts admitted in the same second.
    jobs.sort_by_key(|job| {
        (
            match job.status {
                JobStatus::Running if job.claimed_target.is_some() => 3,
                JobStatus::Running => 2,
                JobStatus::Queued => 1,
                _ => 0,
            },
            job.job_id
                .rsplit('-')
                .next()
                .and_then(|s| u64::from_str_radix(s, 16).ok()),
        )
    });
    for job in jobs.into_iter().rev() {
        let claimed = job.claimed_target.is_some();
        let target = job.claimed_target.unwrap_or(job.target);
        let row = rows
            .entry(target.clone())
            .or_insert_with(|| Row::new(target));
        if row.job_id.is_some() || (row.source == "poll" && job.status.terminal()) {
            continue;
        }
        row.job_id = Some(job.job_id);
        row.job_status = Some(job.status);
        row.queued_at = Some(job.queued_at);
        row.started_at = job.started_at;
        row.finished_at = job.finished_at;
        row.failure_stage = stage(job.failure.as_ref().map(|f| f.stage.as_str()));
        match job.status {
            JobStatus::Queued | JobStatus::Running => {
                row.source = "scheduler";
                row.phase = if claimed {
                    "running"
                } else if job.poll_eligible.is_some() && !poll.candidates.contains_key(&row.target)
                {
                    "retired"
                } else if job.status == JobStatus::Queued
                    && job.poll_eligible.is_some()
                    && poll.candidates.get(&row.target) == Some(&true)
                {
                    "eligible_waiting"
                } else {
                    "candidate"
                };
            }
            JobStatus::Failed
                if row.record.as_ref().is_none_or(|r| {
                    r.timestamp < job.queued_at
                        || (r.status != ReviewStatus::Failed
                            && Some(r.timestamp) <= job.finished_at)
                }) =>
            {
                row.phase = "failed";
            }
            _ if row.record.is_none() => {
                row.phase = match job.status {
                    JobStatus::Skipped => "skipped",
                    JobStatus::Cancelled => "cancelled",
                    JobStatus::Failed => "failed",
                    _ => "finished",
                };
            }
            _ => {}
        }
    }
    let total = rows.len();
    let end = offset.saturating_add(limit);
    Snapshot {
        snapshot_at: now,
        discovery_at: poll.refreshed_at,
        total,
        next_offset: (end < total).then_some(end),
        items: rows
            .into_values()
            .skip(offset)
            .take(limit)
            .map(|mut row| {
                row.in_latest_discovery = poll
                    .refreshed_at
                    .map(|_| poll.observed.contains(&row.target));
                row
            })
            .collect(),
    }
}

pub(super) async fn get_backlog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> axum::response::Response {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }
    let limit = page.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return (StatusCode::BAD_REQUEST, "limit must be between 1 and 500").into_response();
    }
    match state.state_store.list().await {
        Ok(records) => {
            let (poll, jobs) = state.scheduler.backlog().await;
            Json(snapshot(
                records,
                poll,
                jobs,
                unix_now(),
                page.offset.unwrap_or(0),
                limit,
            ))
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Could not read review backlog");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Review state is unavailable",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: ReviewStatus) -> ReviewRecord {
        serde_json::from_value(serde_json::json!({
            "repository":"acme/repo", "pr_number":7, "commit_hash":"old",
            "review_kind":"security", "model":"secret-model", "timestamp":5,
            "findings_count":0, "status":status, "severity":"none", "pr_classification":"none",
            "failure_stage":"raw credential text", "retry_count":1, "retry_not_before":100
        }))
        .unwrap()
    }

    fn target(head: &str) -> ReviewTarget {
        ReviewTarget {
            repository: "acme/repo".into(),
            pr_number: 7,
            commit_hash: head.into(),
            review_kind: "security".into(),
        }
    }

    fn job(status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            job_id: "a-1".into(),
            status,
            target: target("old"),
            poll_eligible: Some(true),
            claimed_target: None,
            queued_at: 6,
            started_at: None,
            finished_at: None,
            error: Some("raw credential text".into()),
            failure: None,
        }
    }

    #[test]
    fn reports_one_row_through_admission_claim_outcome_and_restart() {
        let old = target("old");
        let mut poll = PollBacklog::default();
        poll.observed.insert(old.clone());
        poll.candidates.insert(old.clone(), true);
        poll.deferred.insert(old.clone(), true);
        poll.refreshed_at = Some(7);
        let page = |poll, jobs, status| snapshot(vec![record(status)], poll, jobs, 10, 0, 3);
        let deferred = page(poll.clone(), vec![], ReviewStatus::Failed);
        assert_eq!(deferred.items[0].source, "poll");
        assert_eq!(deferred.items[0].phase, "eligible_waiting");
        let queued = page(
            poll.clone(),
            vec![job(JobStatus::Queued)],
            ReviewStatus::Failed,
        );
        assert_eq!(queued.total, 1);
        assert_eq!(queued.items[0].source, "scheduler");
        assert_eq!(queued.items[0].phase, "eligible_waiting");
        poll.deferred.clear();
        let mut checking = job(JobStatus::Running);
        checking.poll_eligible = None;
        assert_eq!(
            page(poll.clone(), vec![checking.clone()], ReviewStatus::Failed).items[0].phase,
            "candidate"
        );
        checking.claimed_target = Some(old);
        checking.target = target("pending");
        let running = page(poll.clone(), vec![checking], ReviewStatus::InProgress);
        assert_eq!(running.total, 1);
        assert_eq!(running.items[0].phase, "running");

        for (status, phase) in [
            (ReviewStatus::Unverified, "unverified"),
            (ReviewStatus::Confirmed, "completed"),
        ] {
            let finished = page(poll.clone(), vec![job(JobStatus::Completed)], status);
            assert_eq!(finished.items[0].phase, phase);
            assert_eq!(finished.items[0].source, "history");
        }
        for status in [ReviewStatus::InProgress, ReviewStatus::Queued] {
            let restart = page(PollBacklog::default(), vec![], status);
            assert_eq!(restart.items[0].phase, "recorded");
            assert!(restart.discovery_at.is_none());
            assert!(restart.items[0].job_id.is_none());
        }
    }

    #[test]
    fn separates_new_heads_and_retry_history_and_bounds_pages() {
        let new = target("new");
        let mut poll = PollBacklog {
            refreshed_at: Some(10),
            ..Default::default()
        };
        poll.observed.insert(new.clone());
        poll.candidates.insert(new.clone(), true);
        poll.deferred.insert(new, true);
        let page = |offset, record| snapshot(vec![record], poll.clone(), vec![], 10, offset, 1);
        let first = page(0, record(ReviewStatus::Failed));
        assert_eq!(first.next_offset, Some(1));
        assert_eq!(first.items[0].target.commit_hash, "new");
        assert_eq!(first.items[0].in_latest_discovery, Some(true));
        let second = page(1, record(ReviewStatus::Failed));
        assert_eq!(second.next_offset, None);
        assert_eq!(second.items[0].phase, "retry_cooldown");
        assert_eq!(second.items[0].in_latest_discovery, Some(false));
        assert!(
            !serde_json::to_string(&second)
                .unwrap()
                .contains("raw credential")
        );
        let mut failed = record(ReviewStatus::Failed);
        failed.needs_intervention = true;
        assert_eq!(page(1, failed).items[0].phase, "needs_intervention");
        let retired = snapshot(vec![], poll, vec![job(JobStatus::Queued)], 10, 0, 2);
        assert_eq!(retired.items[1].phase, "retired");
    }

    #[tokio::test]
    async fn backlog_requires_server_token_and_enforces_page_size() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let scheduler = crate::scheduler::start(
            1,
            cancel.clone(),
            Arc::new(|_, _| Box::pin(async { Ok(crate::scheduler::ExecutionStatus::Skipped) })),
        );
        let state = AppState {
            state_store: Arc::new(RedbStateStore::open(dir.path().join("db"), None).unwrap()),
            scheduler,
            provider_circuit: crate::broker::CircuitBreaker::disabled(),
            server_token: Some("observer".into()),
        };
        let get = |headers, limit| {
            get_backlog(
                State(state.clone()),
                headers,
                Query(Page {
                    offset: None,
                    limit,
                }),
            )
        };
        assert_eq!(
            get(HeaderMap::new(), None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer observer".parse().unwrap());
        assert_eq!(get(headers.clone(), None).await.status(), StatusCode::OK);
        assert_eq!(
            get(headers, Some(501)).await.status(),
            StatusCode::BAD_REQUEST
        );
        cancel.cancel();
    }
}
