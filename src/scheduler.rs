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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Completed,
    Skipped,
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
    tokio::spawn(run_scheduler(
        worker_limit,
        capacity.clone(),
        receiver,
        Arc::clone(&jobs),
        cancel,
        handler,
    ));
    SchedulerHandle {
        sender,
        jobs,
        capacity,
    }
}

async fn run_scheduler<T: Send + 'static>(
    max_workers: usize,
    capacity: QueueCapacity,
    mut receiver: mpsc::Receiver<Submission<T>>,
    jobs: Arc<Mutex<HashMap<String, JobSnapshot>>>,
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
                    let (status, error) = match result {
                        Ok(ExecutionStatus::Completed) => (JobStatus::Completed, None),
                        Ok(ExecutionStatus::Skipped) => (JobStatus::Skipped, None),
                        Err(error) => (JobStatus::Failed, Some(sanitize_error(&error))),
                    };
                    update_job(&jobs, &id, status, error).await;
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
                };
                jobs.lock().await.insert(id.clone(), snapshot.clone());
                by_target.insert(submission.request.target.clone(), id.clone());
                queue.push_back(Queued { id, request: submission.request });
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

fn sanitize_error(error: &anyhow::Error) -> String {
    let text = error.to_string().replace(['\n', '\r'], " ");
    text.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(pr: u64) -> ReviewRequest<u64> {
        ReviewRequest {
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
}
