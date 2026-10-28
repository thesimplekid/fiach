use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const TERMINAL_JOB_LIMIT: usize = 1_000;
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTarget {
    pub repository: String,
    pub pr_number: u64,
    pub commit_hash: String,
    pub review_kind: String,
}

#[derive(Debug, Clone)]
pub struct ReviewRequest<T> {
    /// Eligible fresh work is queued ahead of failed whole-review retries.
    pub is_retry: bool,
    pub target: ReviewTarget,
    pub payload: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

impl JobStatus {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub job_id: String,
    pub status: JobStatus,
    pub target: ReviewTarget,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    #[serde(default)]
    pub failure: Option<ExecutionFailure>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerStats {
    pub accepting: bool,
    pub queued: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    Completed,
    Skipped,
    Failed(ExecutionFailure),
}

/// Only fixed host categories are exposed; never raw provider/model content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionFailure {
    pub stage: String,
    pub reason: String,
}

impl ExecutionFailure {
    pub fn classify(stage: &str, error: &anyhow::Error) -> Self {
        let text = format!("{error:#}").to_ascii_lowercase();
        let reason = if text.contains("timed out") || text.contains("timeout") {
            "timeout"
        } else if text.contains("delegated lane") || text.contains("finder incomplete") {
            "incomplete_lanes"
        } else if text.contains("provider") {
            "provider_failure"
        } else if text.contains("cancelled") {
            "cancelled"
        } else {
            "unknown"
        };
        let stage = if stage == "execution"
            && (text.contains("review session timed out") || text.contains("finder incomplete"))
        {
            "finder"
        } else {
            stage
        };
        Self {
            stage: stage.into(),
            reason: reason.into(),
        }
    }
}

const HEALTH_WINDOW_SECS: i64 = 15 * 60;
const HEALTH_SAMPLE_LIMIT: usize = 1000;
const QUEUE_AGE_THRESHOLD_SECS: u64 = 10 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct ProcessingHealth {
    pub scope: &'static str,
    pub degraded: bool,
    pub reasons: Vec<&'static str>,
    pub oldest_queued_age_secs: Option<u64>,
    pub window_secs: i64,
    pub sample_limit: usize,
    pub recent_attempts: usize,
    pub recent_failures: usize,
    pub recent_failure_rate: Option<f64>,
    pub consecutive_failures: u64,
    pub last_success_at: Option<i64>,
    pub last_failure_at: Option<i64>,
    pub last_failure: Option<ExecutionFailure>,
}

#[derive(Default)]
struct HealthHistory {
    outcomes: VecDeque<(i64, bool)>,
    consecutive_failures: u64,
    last_success_at: Option<i64>,
    last_failure_at: Option<i64>,
    last_failure: Option<ExecutionFailure>,
}

impl HealthHistory {
    fn record(&mut self, timestamp: i64, status: JobStatus, failure: Option<ExecutionFailure>) {
        if !matches!(status, JobStatus::Completed | JobStatus::Failed) {
            return;
        }
        if self
            .outcomes
            .back()
            .is_some_and(|(at, _)| *at < timestamp - HEALTH_WINDOW_SECS)
        {
            self.consecutive_failures = 0;
        }
        let failed = status == JobStatus::Failed;
        self.outcomes.push_back((timestamp, failed));
        while self.outcomes.len() > HEALTH_SAMPLE_LIMIT
            || self
                .outcomes
                .front()
                .is_some_and(|(at, _)| *at < timestamp - HEALTH_WINDOW_SECS)
        {
            self.outcomes.pop_front();
        }
        if failed {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.last_failure_at = Some(timestamp);
            self.last_failure = failure;
        } else {
            self.consecutive_failures = 0;
            self.last_success_at = Some(timestamp);
        }
    }

    fn snapshot(&self, timestamp: i64, oldest_queued: Option<i64>) -> ProcessingHealth {
        let recent: Vec<_> = self
            .outcomes
            .iter()
            .filter(|(at, _)| *at >= timestamp - HEALTH_WINDOW_SECS)
            .collect();
        let failures = recent.iter().filter(|(_, failed)| *failed).count();
        let consecutive_failures = if recent.is_empty() {
            0
        } else {
            self.consecutive_failures
        };
        let age = oldest_queued.map(|at| timestamp.saturating_sub(at).max(0) as u64);
        let mut reasons = Vec::new();
        if consecutive_failures >= 3 {
            reasons.push("consecutive_review_failures");
        }
        if recent.len() >= 5 && failures * 2 >= recent.len() {
            reasons.push("high_recent_review_failure_rate");
        }
        if age.is_some_and(|age| age >= QUEUE_AGE_THRESHOLD_SECS) {
            reasons.push("review_queue_age_exceeded");
        }
        ProcessingHealth {
            scope: "reviews",
            degraded: !reasons.is_empty(),
            reasons,
            oldest_queued_age_secs: age,
            window_secs: HEALTH_WINDOW_SECS,
            sample_limit: HEALTH_SAMPLE_LIMIT,
            recent_attempts: recent.len(),
            recent_failures: failures,
            recent_failure_rate: (!recent.is_empty())
                .then(|| failures as f64 / recent.len() as f64),
            consecutive_failures,
            last_success_at: self.last_success_at,
            last_failure_at: self.last_failure_at,
            last_failure: self.last_failure.clone(),
        }
    }
}

type BoxJobFuture = Pin<Box<dyn Future<Output = Result<ExecutionStatus>> + Send>>;
type Handler<T> = Arc<dyn Fn(T, CancellationToken) -> BoxJobFuture + Send + Sync>;

struct Submission<T> {
    request: ReviewRequest<T>,
    response: oneshot::Sender<Result<JobSnapshot, SubmitError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Full,
    Closed,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Full => "review queue is full",
            Self::Closed => "review scheduler is shutting down",
        })
    }
}

#[derive(Clone)]
pub struct SchedulerHandle<T> {
    sender: mpsc::Sender<Submission<T>>,
    jobs: Arc<Mutex<HashMap<String, JobSnapshot>>>,
    capacity: QueueCapacity,
    health: Arc<Mutex<HealthHistory>>,
}

#[derive(Clone)]
struct QueueCapacity {
    limit: usize,
    changed: Arc<Notify>,
}

impl<T: Send + 'static> SchedulerHandle<T> {
    pub async fn submit(&self, request: ReviewRequest<T>) -> Result<JobSnapshot, SubmitError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .try_send(Submission { request, response })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SubmitError::Full,
                mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
            })?;
        receiver.await.map_err(|_| SubmitError::Closed)?
    }

    /// Wake the poller when space is freed or the scheduler closes.
    /// A wakeup does not reserve space; submissions can still return `Full`.
    pub async fn capacity_changed(&self) {
        tokio::select! {
            _ = self.capacity.changed.notified() => {}
            _ = self.sender.closed() => {}
        }
    }

    pub async fn get(&self, job_id: &str) -> Option<JobSnapshot> {
        self.jobs.lock().await.get(job_id).cloned()
    }

    pub async fn processing_health(&self) -> ProcessingHealth {
        let oldest_queued = self
            .jobs
            .lock()
            .await
            .values()
            .filter(|job| job.status == JobStatus::Queued)
            .map(|job| job.queued_at)
            .min();
        self.health.lock().await.snapshot(now(), oldest_queued)
    }

    pub async fn stats(&self) -> SchedulerStats {
        let jobs = self.jobs.lock().await;
        let mut stats = SchedulerStats {
            accepting: !self.sender.is_closed(),
            total: jobs.len(),
            ..SchedulerStats::default()
        };
        for job in jobs.values() {
            match job.status {
                JobStatus::Queued => stats.queued += 1,
                JobStatus::Running => stats.running += 1,
                JobStatus::Completed => stats.completed += 1,
                JobStatus::Failed => stats.failed += 1,
                JobStatus::Skipped => stats.skipped += 1,
                JobStatus::Cancelled => stats.cancelled += 1,
            }
        }
        stats
    }
}

struct Queued<T> {
    id: String,
    request: ReviewRequest<T>,
}

pub fn start<T: Send + 'static>(
    max_workers: usize,
    cancel: CancellationToken,
    handler: Handler<T>,
) -> SchedulerHandle<T> {
    let capacity = max_workers.saturating_mul(2).max(16);
    let worker_limit = if max_workers == 0 {
        usize::MAX
    } else {
        max_workers
    };
    let (sender, receiver) = mpsc::channel(capacity);
    let capacity = QueueCapacity {
        limit: capacity,
        changed: Arc::new(Notify::new()),
    };
    let jobs = Arc::new(Mutex::new(HashMap::new()));
    let health = Arc::new(Mutex::new(HealthHistory::default()));
    tokio::spawn(run_scheduler(
        worker_limit,
        capacity.clone(),
        receiver,
        Arc::clone(&jobs),
        health.clone(),
        cancel,
        handler,
    ));
    SchedulerHandle {
        sender,
        jobs,
        capacity,
        health,
    }
}

async fn run_scheduler<T: Send + 'static>(
    max_workers: usize,
    capacity: QueueCapacity,
    mut receiver: mpsc::Receiver<Submission<T>>,
    jobs: Arc<Mutex<HashMap<String, JobSnapshot>>>,
    health: Arc<Mutex<HealthHistory>>,
    cancel: CancellationToken,
    handler: Handler<T>,
) {
    let mut queue: VecDeque<Queued<T>> = VecDeque::new();
    let mut active = HashSet::new();
    let mut by_target = HashMap::<ReviewTarget, String>::new();
    let mut running = JoinSet::new();
    let mut terminal = VecDeque::new();

    loop {
        while running.len() < max_workers
            && let Some(queued) = queue.pop_front()
        {
            capacity.changed.notify_one();
            update_job(&jobs, &queued.id, JobStatus::Running, None).await;
            let id = queued.id;
            let target = queued.request.target;
            let future = handler(queued.request.payload, cancel.child_token());
            active.insert(id.clone());
            running.spawn(async move { (id, target, future.await) });
        }

        if cancel.is_cancelled() {
            receiver.close();
            while let Some(queued) = queue.pop_front() {
                update_job(&jobs, &queued.id, JobStatus::Cancelled, None).await;
                terminal.push_back(queued.id);
            }
            while let Ok(submission) = receiver.try_recv() {
                let _ = submission.response.send(Err(SubmitError::Closed));
            }
            running.abort_all();
            while running.join_next().await.is_some() {}
            for id in active.drain() {
                update_job(&jobs, &id, JobStatus::Cancelled, None).await;
                terminal.push_back(id);
            }
            break;
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            Some(result) = running.join_next(), if !running.is_empty() => {
                if let Ok((id, target, result)) = result {
                    active.remove(&id);
                    by_target.remove(&target);
                    let (status, error, failure) = match result {
                        Ok(ExecutionStatus::Completed) => (JobStatus::Completed, None, None),
                        Ok(ExecutionStatus::Skipped) => (JobStatus::Skipped, None, None),
                        Ok(ExecutionStatus::Failed(failure)) => (JobStatus::Failed, Some(format!("{}: {}", failure.stage, failure.reason)), Some(failure)),
                        Err(error) => {
                            let failure = ExecutionFailure::classify("execution", &error);
                            (JobStatus::Failed, Some(format!("{}: {}", failure.stage, failure.reason)), Some(failure))
                        }
                    };
                    update_job(&jobs, &id, status, error).await;
                    if let Some(job) = jobs.lock().await.get_mut(&id) { job.failure = failure.clone(); }
                    health.lock().await.record(now(), status, failure);
                    terminal.push_back(id);
                    evict_terminal(&jobs, &mut terminal).await;
                }
            }
            Some(submission) = receiver.recv() => {
                // Also wake a submitter rejected by the bounded input channel.
                if receiver.capacity() == 1 {
                    capacity.changed.notify_one();
                }
                if let Some(existing) = by_target.get(&submission.request.target) {
                    let snapshot = jobs.lock().await.get(existing).cloned();
                    let _ = submission.response.send(snapshot.ok_or(SubmitError::Closed));
                    continue;
                }
                if queue.len() >= capacity.limit {
                    let _ = submission.response.send(Err(SubmitError::Full));
                    continue;
                }
                let id = next_job_id();
                let snapshot = JobSnapshot {
                    job_id: id.clone(),
                    status: JobStatus::Queued,
                    target: submission.request.target.clone(),
                    queued_at: now(),
                    started_at: None,
                    finished_at: None,
                    error: None,
                    failure: None,
                };
                jobs.lock().await.insert(id.clone(), snapshot.clone());
                by_target.insert(submission.request.target.clone(), id.clone());
                let queued = Queued { id, request: submission.request };
                if queued.request.is_retry {
                    queue.push_back(queued);
                } else {
                    let position = queue.iter().position(|job| job.request.is_retry).unwrap_or(queue.len());
                    queue.insert(position, queued);
                }
                let _ = submission.response.send(Ok(snapshot));
            }
            else => {
                if running.is_empty() && queue.is_empty() {
                    break;
                }
            }
        }
    }
}

fn next_job_id() -> String {
    format!(
        "{:x}-{:x}",
        now(),
        NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

async fn update_job(
    jobs: &Mutex<HashMap<String, JobSnapshot>>,
    id: &str,
    status: JobStatus,
    error: Option<String>,
) {
    if let Some(job) = jobs.lock().await.get_mut(id) {
        job.status = status;
        if status == JobStatus::Running {
            job.started_at = Some(now());
        }
        if status.terminal() {
            job.finished_at = Some(now());
        }
        job.error = error;
    }
}

async fn evict_terminal(
    jobs: &Mutex<HashMap<String, JobSnapshot>>,
    terminal: &mut VecDeque<String>,
) {
    if terminal.len() > TERMINAL_JOB_LIMIT
        && let Some(id) = terminal.pop_front()
    {
        jobs.lock().await.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(pr: u64) -> ReviewRequest<u64> {
        ReviewRequest {
            is_retry: false,
            target: ReviewTarget {
                repository: "owner/repo".to_string(),
                pr_number: pr,
                commit_hash: format!("commit-{pr}"),
                review_kind: "security".to_string(),
            },
            payload: pr,
        }
    }

    #[tokio::test]
    async fn deduplicates_active_jobs_and_exposes_lifecycle() {
        let cancel = CancellationToken::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let handler = {
            let gate = Arc::clone(&gate);
            Arc::new(move |_: u64, _: CancellationToken| {
                let gate = Arc::clone(&gate);
                Box::pin(async move {
                    gate.notified().await;
                    Ok(ExecutionStatus::Completed)
                }) as BoxJobFuture
            })
        };
        let scheduler = start(1, cancel, handler);
        let first = scheduler.submit(request(1)).await.unwrap();
        let duplicate = scheduler.submit(request(1)).await.unwrap();
        assert_eq!(first.job_id, duplicate.job_id);

        tokio::task::yield_now().await;
        assert_eq!(
            scheduler.get(&first.job_id).await.unwrap().status,
            JobStatus::Running
        );
        gate.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(
            scheduler.get(&first.job_id).await.unwrap().status,
            JobStatus::Completed
        );
        assert_eq!(
            scheduler.stats().await,
            SchedulerStats {
                accepting: true,
                completed: 1,
                total: 1,
                ..SchedulerStats::default()
            }
        );
    }

    #[tokio::test]
    async fn dispatches_fifo_with_worker_limit() {
        let cancel = CancellationToken::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let handler = {
            let order = Arc::clone(&order);
            Arc::new(move |pr: u64, _: CancellationToken| {
                let order = Arc::clone(&order);
                Box::pin(async move {
                    order.lock().await.push(pr);
                    Ok(ExecutionStatus::Completed)
                }) as BoxJobFuture
            })
        };
        let scheduler = start(1, cancel, handler);
        for pr in 1..=3 {
            scheduler.submit(request(pr)).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(*order.lock().await, vec![1, 2, 3]);
    }
    #[tokio::test]
    async fn fresh_work_runs_before_queued_retries() {
        let cancel = CancellationToken::new();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let handler = Arc::new(move |pr: u64, _: CancellationToken| {
            let sender = sender.clone();
            Box::pin(async move {
                sender.send(pr).unwrap();
                Ok(ExecutionStatus::Completed)
            }) as BoxJobFuture
        });
        // Keep a separate gate reference for releasing the first worker.
        let first_gate = Arc::new(tokio::sync::Notify::new());
        let release = first_gate.clone();
        let handler = Arc::new(move |pr: u64, token: CancellationToken| {
            let handler = handler.clone();
            let first_gate = first_gate.clone();
            Box::pin(async move {
                if pr == 1 {
                    first_gate.notified().await;
                    return Ok(ExecutionStatus::Completed);
                }
                handler(pr, token).await
            }) as BoxJobFuture
        });
        let scheduler = start(1, cancel.clone(), handler);
        scheduler.submit(request(1)).await.unwrap();
        let mut retry = request(2);
        retry.is_retry = true;
        scheduler.submit(retry).await.unwrap();
        scheduler.submit(request(3)).await.unwrap();
        release.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(2)
        );
        cancel.cancel();
    }
    #[test]
    fn processing_health_distinguishes_idle_failure_recovery_and_queue_stalls() {
        let mut history = HealthHistory::default();
        let idle = history.snapshot(1000, None);
        assert!(!idle.degraded);
        assert_eq!(idle.recent_failure_rate, None);
        let failure = ExecutionFailure::classify(
            "execution",
            &anyhow::anyhow!("Review session timed out secret=private"),
        );
        assert_eq!(failure.stage, "finder");
        assert_eq!(failure.reason, "timeout");
        for at in 1000..1003 {
            history.record(at, JobStatus::Failed, Some(failure.clone()));
        }
        history.record(1004, JobStatus::Skipped, None);
        let degraded = history.snapshot(1004, None);
        assert!(degraded.degraded);
        assert_eq!(degraded.recent_attempts, 3);
        assert_eq!(degraded.consecutive_failures, 3);
        assert_eq!(degraded.last_failure, Some(failure.clone()));
        history.record(1005, JobStatus::Completed, None);
        let recovered = history.snapshot(1005, None);
        assert!(!recovered.degraded);
        assert_eq!(recovered.last_success_at, Some(1005));
        assert_eq!(recovered.consecutive_failures, 0);
        assert!(
            history
                .snapshot(1005, Some(400))
                .reasons
                .contains(&"review_queue_age_exceeded")
        );
        assert!(!history.snapshot(2000, None).degraded);
        history.record(2000, JobStatus::Failed, Some(failure));
        assert_eq!(history.snapshot(2000, None).consecutive_failures, 1);
    }

    #[test]
    fn recent_failure_rate_uses_attempts_and_clears_after_sustained_recovery() {
        let mut history = HealthHistory::default();
        for at in 1000..1005 {
            history.record(at, JobStatus::Failed, None);
        }
        for at in 1005..1010 {
            history.record(at, JobStatus::Completed, None);
        }
        let health = history.snapshot(1010, None);
        assert_eq!(health.recent_failure_rate, Some(0.5));
        assert!(health.reasons.contains(&"high_recent_review_failure_rate"));
        history.record(1010, JobStatus::Completed, None);
        assert!(!history.snapshot(1011, None).degraded);
        assert_eq!(history.snapshot(3000, None).recent_attempts, 0);
    }

    #[tokio::test]
    async fn live_scheduler_exposes_degradation_without_changing_acceptance() {
        let cancel = CancellationToken::new();
        let handler = Arc::new(|_: u64, _: CancellationToken| {
            Box::pin(async {
                Ok(ExecutionStatus::Failed(ExecutionFailure {
                    stage: "finder".into(),
                    reason: "timeout".into(),
                }))
            }) as BoxJobFuture
        });
        let scheduler = start(1, cancel.clone(), handler);
        for pr in 1..=3 {
            scheduler.submit(request(pr)).await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while scheduler.stats().await.failed < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(scheduler.stats().await.accepting);
        assert!(scheduler.processing_health().await.degraded);
        assert_eq!(
            scheduler
                .processing_health()
                .await
                .last_failure
                .unwrap()
                .stage,
            "finder"
        );
        cancel.cancel();
    }
}
