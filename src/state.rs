use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const PR_STATE: TableDefinition<&str, &str> = TableDefinition::new("pr_state");
const COMMIT_STATE: TableDefinition<&str, &str> = TableDefinition::new("commit_state");
const SCHEMA_VERSION: TableDefinition<&str, u64> = TableDefinition::new("schema_version");
const CURRENT_SCHEMA_VERSION: u64 = 2;
pub const DEFAULT_REVIEW_KIND: &str = "default";

#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ArtifactPaths {
    #[serde(default)]
    pub markdown: Option<PathBuf>,
    #[serde(default)]
    pub structured_json: Option<PathBuf>,
    #[serde(default)]
    pub policy_json: Option<PathBuf>,
    #[serde(default)]
    pub sandbox_log: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewStatus {
    Queued,
    #[serde(rename = "in_progress", alias = "in-progress")]
    InProgress,
    Skipped,
    Failed,
    None,
    Rejected,
    Unverified,
    AlreadyReported,
    Confirmed,
    MarkdownOnly,
}

impl ReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::None => "none",
            Self::Rejected => "rejected",
            Self::Unverified => "unverified",
            Self::AlreadyReported => "already-reported",
            Self::Confirmed => "confirmed",
            Self::MarkdownOnly => "markdown-only",
        }
    }
}

impl std::fmt::Display for ReviewStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "in_progress" | "in-progress" => Ok(Self::InProgress),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
            "none" => Ok(Self::None),
            "rejected" => Ok(Self::Rejected),
            "unverified" => Ok(Self::Unverified),
            "already-reported" => Ok(Self::AlreadyReported),
            "confirmed" => Ok(Self::Confirmed),
            "markdown-only" => Ok(Self::MarkdownOnly),
            other => anyhow::bail!("unknown review status: {other}"),
        }
    }
}

impl PartialEq<&str> for ReviewStatus {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewRecord {
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub pr_number: u64,
    #[serde(default = "default_review_kind")]
    pub review_kind: String,
    pub commit_hash: String,
    pub model: String,
    pub timestamp: i64, // Unix timestamp of when the review completed
    pub findings_count: u32,
    pub status: ReviewStatus,
    pub severity: String,
    pub pr_classification: String,
    #[serde(default)]
    pub duration_secs: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub report_url: Option<String>,
    #[serde(default)]
    pub is_rereview: bool,
    #[serde(default)]
    pub time_reviewed: Option<String>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub artifacts: ArtifactPaths,
    #[serde(default)]
    pub disclosure_url: Option<String>,
    #[serde(default)]
    pub buzz_thread: Option<BuzzThreadState>,
    #[serde(default)]
    pub failure_stage: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct BuzzThreadState {
    pub channel_id: String,
    pub root_event_id: String,
    #[serde(default)]
    pub published_finding_keys: Vec<String>,
}

/// Compatibility name retained while callers migrate to the domain-oriented record.
pub type ReviewMetadata = ReviewRecord;

fn default_review_kind() -> String {
    DEFAULT_REVIEW_KIND.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Skip,
    FirstReview,
    ReReview,
    RetryFailed,
}

struct ShouldReviewInput<'a> {
    db_path: &'a Path,
    repo: &'a str,
    pr: u64,
    current_hash: &'a str,
    review_kind: &'a str,
    force: bool,
    timeout_mins: u64,
    max_failed_retries: Option<u32>,
}

fn pr_state_key(repo: &str, pr: u64, review_kind: &str) -> String {
    if review_kind == DEFAULT_REVIEW_KIND {
        format!("{}_{}", repo, pr)
    } else {
        format!("{repo}|{pr}|{review_kind}")
    }
}

fn commit_state_key(repo: &str, commit_hash: &str, review_kind: &str) -> String {
    if review_kind == DEFAULT_REVIEW_KIND {
        format!("{}_{}", repo, commit_hash)
    } else {
        format!("{repo}|{commit_hash}|{review_kind}")
    }
}

fn parse_pr_state_key(key: &str) -> Option<(String, u64)> {
    if let Some((repo, rest)) = key.split_once('|')
        && let Some((pr_str, _review_kind)) = rest.split_once('|')
        && let Ok(pr) = pr_str.parse::<u64>()
    {
        return Some((repo.to_string(), pr));
    }

    if let Some((repo, pr_str)) = key.rsplit_once('_')
        && let Ok(pr) = pr_str.parse::<u64>()
    {
        return Some((repo.to_string(), pr));
    }

    None
}

fn with_retries<T, F>(mut action: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let max_retries = 5;
    let mut delay = std::time::Duration::from_millis(50);

    for attempt in 0..max_retries {
        match action() {
            Ok(result) => return Ok(result),
            Err(e) => {
                if attempt == max_retries - 1 {
                    return Err(e).context("Max retries reached for database operation");
                }
                tracing::debug!(
                    "Database operation failed (attempt {}): {}. Retrying in {:?}...",
                    attempt + 1,
                    e,
                    delay
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
        }
    }
    unreachable!()
}

/// Checks if a PR needs to be reviewed based on the stored commit hash.
/// Returns `ReviewDecision::FirstReview` if not reviewed before,
/// `ReviewDecision::ReReview` if reviewed on an older commit,
/// and `ReviewDecision::Skip` if it can be skipped.
pub fn should_review(
    db_path: &Path,
    repo: &str,
    pr: u64,
    current_hash: &str,
    review_kind: &str,
    force: bool,
    timeout_mins: u64,
) -> Result<ReviewDecision> {
    should_review_inner(ShouldReviewInput {
        db_path,
        repo,
        pr,
        current_hash,
        review_kind,
        force,
        timeout_mins,
        max_failed_retries: None,
    })
}

/// Checks if a PR needs review, bounding same-commit failed-review retries.
pub fn should_review_with_retry_limit(
    db_path: &Path,
    repo: &str,
    pr: u64,
    current_hash: &str,
    review_kind: &str,
    timeout_mins: u64,
    max_failed_retries: u32,
) -> Result<ReviewDecision> {
    should_review_inner(ShouldReviewInput {
        db_path,
        repo,
        pr,
        current_hash,
        review_kind,
        force: false,
        timeout_mins,
        max_failed_retries: Some(max_failed_retries),
    })
}

fn should_review_inner(input: ShouldReviewInput<'_>) -> Result<ReviewDecision> {
    let ShouldReviewInput {
        db_path,
        repo,
        pr,
        current_hash,
        review_kind,
        force,
        timeout_mins,
        max_failed_retries,
    } = input;

    if force {
        tracing::debug!("Force flag set, bypassing state check");
        return Ok(ReviewDecision::FirstReview);
    }

    if !db_path.exists() {
        tracing::debug!(
            "Database does not exist at {}, proceeding with review",
            db_path.display()
        );
        return Ok(ReviewDecision::FirstReview);
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open redb database")?;

        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(PR_STATE) {
            Ok(t) => t,
            Err(_) => {
                // Table doesn't exist yet, so no reviews have been recorded
                return Ok(ReviewDecision::FirstReview);
            }
        };

        let key = pr_state_key(repo, pr, review_kind);
        if let Some(value) = table.get(key.as_str())? {
            let json_str = value.value();
            match serde_json::from_str::<ReviewMetadata>(json_str) {
                Ok(metadata) => {
                    if metadata.status == "in_progress" {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        let age_secs = now.saturating_sub(metadata.timestamp);
                        let timeout_secs = (timeout_mins + 10) * 60; // 10 min grace period

                        if age_secs as u64 > timeout_secs {
                            tracing::warn!(
                                repo,
                                pr,
                                age_secs,
                                timeout_secs,
                                "Found stale in_progress lock, proceeding with review"
                            );
                            if metadata.commit_hash == current_hash {
                                return Ok(ReviewDecision::RetryFailed);
                            }
                            // Fall through to commit hash check for newer commits.
                        } else {
                            tracing::debug!(repo, pr, "PR review is already in progress, skipping");
                            return Ok(ReviewDecision::Skip);
                        }
                    }
                    if metadata.commit_hash == current_hash {
                        if metadata.status == "failed" {
                            if let Some(max_failed_retries) = max_failed_retries
                                && metadata.retry_count >= max_failed_retries
                            {
                                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                                let age_secs = now.saturating_sub(metadata.timestamp);
                                let retry_cooldown_secs = (timeout_mins + 10) * 60;

                                if age_secs as u64 <= retry_cooldown_secs {
                                    tracing::warn!(
                                        repo = %repo,
                                        pr = pr,
                                        commit = %current_hash,
                                        review_kind = %review_kind,
                                        retries = metadata.retry_count,
                                        max_retries = max_failed_retries,
                                        retry_cooldown_secs = retry_cooldown_secs,
                                        "Skipping previously failed review during retry cooldown"
                                    );
                                    return Ok(ReviewDecision::Skip);
                                }

                                tracing::warn!(
                                    repo = %repo,
                                    pr = pr,
                                    commit = %current_hash,
                                    review_kind = %review_kind,
                                    retries = metadata.retry_count,
                                    max_retries = max_failed_retries,
                                    age_secs = age_secs,
                                    retry_cooldown_secs = retry_cooldown_secs,
                                    "Retrying previously failed review after retry cooldown"
                                );
                            }
                            tracing::info!(
                                repo = %repo,
                                pr = pr,
                                commit = %current_hash,
                                review_kind = %review_kind,
                                retries = metadata.retry_count,
                                "Retrying previously failed review at the same commit"
                            );
                            return Ok(ReviewDecision::RetryFailed);
                        }

                        tracing::debug!(
                            commit = %current_hash,
                            model = %metadata.model,
                            findings = metadata.findings_count,
                            "PR has already been reviewed at this commit"
                        );
                        return Ok(ReviewDecision::Skip);
                    } else {
                        tracing::debug!(
                            old_commit = %metadata.commit_hash,
                            new_commit = %current_hash,
                            "New commit detected, proceeding with review"
                        );
                        return Ok(ReviewDecision::ReReview);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to deserialize review metadata for {}, proceeding with review: {}",
                        key,
                        e
                    );
                    return Ok(ReviewDecision::FirstReview);
                }
            }
        }

        Ok(ReviewDecision::FirstReview)
    })
}

/// Records the completed review metadata in the database.
pub fn mark_reviewed(db_path: &Path, repo: &str, pr: u64, metadata: &ReviewMetadata) -> Result<()> {
    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open or create redb database")?;

        let write_txn = db.begin_write()?;
        {
            let mut pr_table = write_txn.open_table(PR_STATE)?;
            let pr_key = pr_state_key(repo, pr, &metadata.review_kind);
            let mut metadata = metadata.clone();
            if metadata.buzz_thread.is_none() {
                metadata.buzz_thread = pr_table
                    .get(pr_key.as_str())?
                    .and_then(|value| serde_json::from_str::<ReviewMetadata>(value.value()).ok())
                    .and_then(|previous| previous.buzz_thread);
            }
            let json_str =
                serde_json::to_string(&metadata).context("Failed to serialize ReviewMetadata")?;
            pr_table.insert(pr_key.as_str(), json_str.as_str())?;

            let mut commit_table = write_txn.open_table(COMMIT_STATE)?;
            let commit_key = commit_state_key(repo, &metadata.commit_hash, &metadata.review_kind);
            commit_table.insert(commit_key.as_str(), json_str.as_str())?;
        }
        write_txn.commit()?;

        tracing::debug!("Successfully recorded review metadata in database");
        Ok(())
    })
}

/// Locks a PR for review by marking it as in_progress.
/// Returns true if the lock was acquired, false if it's already in progress by another process.
pub fn lock_for_review(
    db_path: &Path,
    repo: &str,
    pr: u64,
    commit_hash: &str,
    review_kind: &str,
    timeout_mins: u64,
) -> Result<bool> {
    if !db_path.exists() {
        // Proceed and let Database::create handle it
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open or create redb database")?;
        let write_txn = db.begin_write()?;

        {
            let mut pr_table = write_txn.open_table(PR_STATE)?;
            let key = pr_state_key(repo, pr, review_kind);

            if let Some(value) = pr_table.get(key.as_str())? {
                let json_str = value.value();
                #[allow(clippy::collapsible_if)]
                if let Ok(metadata) = serde_json::from_str::<ReviewMetadata>(json_str) {
                    if metadata.status == "in_progress" {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        let age_secs = now - metadata.timestamp;
                        let timeout_secs = (timeout_mins + 10) * 60; // 10 min grace period

                        if age_secs as u64 <= timeout_secs {
                            return Ok(false);
                        } else {
                            tracing::warn!(
                                repo,
                                pr,
                                age_secs,
                                timeout_secs,
                                "Overwriting stale in_progress lock"
                            );
                        }
                    }
                }
            }

            let mut metadata = ReviewMetadata {
                repository: repo.to_string(),
                pr_number: pr,
                review_kind: review_kind.to_string(),
                commit_hash: commit_hash.to_string(),
                model: "daemon".to_string(),
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                findings_count: 0,
                status: ReviewStatus::InProgress,
                severity: "none".to_string(),
                pr_classification: "none".to_string(),
                duration_secs: 0,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: Some(0.0),
                report_url: None,
                is_rereview: false,
                time_reviewed: Some(
                    time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                ),
                retry_count: 0,
                artifacts: ArtifactPaths::default(),
                disclosure_url: None,
                buzz_thread: None,
                failure_stage: None,
            };

            if let Some(value) = pr_table.get(key.as_str())? {
                let json_str = value.value();
                if let Ok(previous) = serde_json::from_str::<ReviewMetadata>(json_str) {
                    metadata.buzz_thread = previous.buzz_thread;
                    if previous.commit_hash == commit_hash
                        && (previous.status == "failed" || previous.status == "in_progress")
                    {
                        metadata.retry_count = previous.retry_count.saturating_add(1);
                    }
                }
            }

            let json_str =
                serde_json::to_string(&metadata).context("Failed to serialize ReviewMetadata")?;
            pr_table.insert(key.as_str(), json_str.as_str())?;
        }

        write_txn.commit()?;
        Ok(true)
    })
}

/// Retrieves review metadata for a specific commit hash.
pub fn get_pr_review(
    db_path: &Path,
    repo: &str,
    pr: u64,
    review_kind: &str,
) -> Result<Option<ReviewMetadata>> {
    if !db_path.exists() {
        return Ok(None);
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open redb database")?;
        let read_txn = db.begin_read()?;

        let table = match read_txn.open_table(PR_STATE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        let key = pr_state_key(repo, pr, review_kind);
        if let Some(value) = table.get(key.as_str())? {
            let json_str = value.value();
            let metadata: ReviewMetadata = serde_json::from_str(json_str)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    })
}

pub fn get_commit_review(
    db_path: &Path,
    repo: &str,
    commit_hash: &str,
    review_kind: &str,
) -> Result<Option<ReviewMetadata>> {
    if !db_path.exists() {
        return Ok(None);
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open redb database")?;
        let read_txn = db.begin_read()?;

        let table = match read_txn.open_table(COMMIT_STATE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        let key = commit_state_key(repo, commit_hash, review_kind);
        if let Some(value) = table.get(key.as_str())? {
            let json_str = value.value();
            let metadata: ReviewMetadata = serde_json::from_str(json_str)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    })
}

/// Retrieves all reviewed PRs from the database.
pub fn list_reviews(db_path: &Path) -> Result<Vec<(String, u64, ReviewMetadata)>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open redb database")?;
        let read_txn = db.begin_read()?;

        let table = match read_txn.open_table(PR_STATE) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };

        let mut reviews = Vec::new();
        for item in table.iter()? {
            let (key_guard, value_guard) = item?;
            let key = key_guard.value();

            if let Some((repo, pr)) = parse_pr_state_key(key) {
                let json_str = value_guard.value();
                if let Ok(metadata) = serde_json::from_str::<ReviewMetadata>(json_str) {
                    reviews.push((repo, pr, metadata));
                }
            }
        }

        // Sort by timestamp descending (newest first)
        reviews.sort_by_key(|b| std::cmp::Reverse(b.2.timestamp));

        Ok(reviews)
    })
}

#[derive(Clone)]
pub struct RedbStateStore {
    database: Arc<Database>,
    db_path: PathBuf,
}

impl RedbStateStore {
    pub fn open(db_path: impl Into<PathBuf>, reports_dir: Option<&Path>) -> Result<Self> {
        let db_path = db_path.into();
        let existed = db_path.exists();
        let database =
            Arc::new(Database::create(&db_path).with_context(|| {
                format!("Failed to open state database at {}", db_path.display())
            })?);
        let store = Self { database, db_path };
        store.migrate_if_needed(existed, reports_dir)?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.db_path
    }

    fn migrate_if_needed(&self, existed: bool, reports_dir: Option<&Path>) -> Result<()> {
        let current_version = {
            let read = self.database.begin_read()?;
            match read.open_table(SCHEMA_VERSION) {
                Ok(table) => table.get("schema")?.map(|value| value.value()),
                Err(_) => None,
            }
        };
        if current_version == Some(CURRENT_SCHEMA_VERSION) {
            return Ok(());
        }
        if let Some(version) = current_version
            && version > CURRENT_SCHEMA_VERSION
        {
            anyhow::bail!(
                "State database schema {version} is newer than supported schema {CURRENT_SCHEMA_VERSION}"
            );
        }

        if existed {
            let backup = migration_backup_path(&self.db_path);
            std::fs::copy(&self.db_path, &backup).with_context(|| {
                format!(
                    "Failed to back up state database to {} before migration",
                    backup.display()
                )
            })?;
        }

        let write = self.database.begin_write()?;
        migrate_table(&write, PR_STATE, reports_dir, true)?;
        migrate_table(&write, COMMIT_STATE, reports_dir, false)?;
        {
            let mut schema = write.open_table(SCHEMA_VERSION)?;
            schema.insert("schema", CURRENT_SCHEMA_VERSION)?;
        }
        write
            .commit()
            .context("Failed to commit state database migration")
    }

    pub async fn get(
        &self,
        repository: &str,
        pr_number: u64,
        review_kind: &str,
    ) -> Result<Option<ReviewRecord>> {
        let database = Arc::clone(&self.database);
        let key = pr_state_key(repository, pr_number, review_kind);
        tokio::task::spawn_blocking(move || read_record(&database, PR_STATE, &key))
            .await
            .context("State read task failed")?
    }

    pub async fn list(&self) -> Result<Vec<ReviewRecord>> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database.begin_read()?;
            let table = match read.open_table(PR_STATE) {
                Ok(table) => table,
                Err(_) => return Ok(Vec::new()),
            };
            let mut records = Vec::new();
            for item in table.iter()? {
                let (_, value) = item?;
                records.push(serde_json::from_str::<ReviewRecord>(value.value())?);
            }
            records.sort_by_key(|record| std::cmp::Reverse(record.timestamp));
            Ok(records)
        })
        .await
        .context("State list task failed")?
    }

    pub async fn try_claim(&self, mut record: ReviewRecord, timeout_mins: u64) -> Result<bool> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write()?;
            let key = pr_state_key(&record.repository, record.pr_number, &record.review_kind);
            {
                let mut table = write.open_table(PR_STATE)?;
                if let Some(value) = table.get(key.as_str())? {
                    let previous: ReviewRecord = serde_json::from_str(value.value())?;
                    record.buzz_thread = previous.buzz_thread.clone();
                    if previous.status == ReviewStatus::InProgress {
                        let age = time::OffsetDateTime::now_utc()
                            .unix_timestamp()
                            .saturating_sub(previous.timestamp)
                            as u64;
                        if age <= (timeout_mins + 10) * 60 {
                            return Ok(false);
                        }
                    }
                    if previous.commit_hash == record.commit_hash
                        && matches!(
                            previous.status,
                            ReviewStatus::Failed | ReviewStatus::InProgress
                        )
                    {
                        record.retry_count = previous.retry_count.saturating_add(1);
                    }
                }
                record.status = ReviewStatus::InProgress;
                record.timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
                let json = serde_json::to_string(&record)?;
                table.insert(key.as_str(), json.as_str())?;
            }
            write.commit()?;
            Ok(true)
        })
        .await
        .context("State claim task failed")?
    }

    pub async fn complete(&self, record: ReviewRecord) -> Result<()> {
        self.write_terminal(record).await
    }

    pub async fn fail(&self, mut record: ReviewRecord, stage: &str) -> Result<()> {
        record.status = ReviewStatus::Failed;
        record.failure_stage = Some(stage.to_string());
        self.write_terminal(record).await
    }

    async fn write_terminal(&self, mut record: ReviewRecord) -> Result<()> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write()?;
            {
                let mut prs = write.open_table(PR_STATE)?;
                let key = pr_state_key(&record.repository, record.pr_number, &record.review_kind);
                if record.buzz_thread.is_none() {
                    record.buzz_thread = prs
                        .get(key.as_str())?
                        .and_then(|value| serde_json::from_str::<ReviewRecord>(value.value()).ok())
                        .and_then(|previous| previous.buzz_thread);
                }
                let json = serde_json::to_string(&record)?;
                prs.insert(key.as_str(), json.as_str())?;
                let mut commits = write.open_table(COMMIT_STATE)?;
                let key =
                    commit_state_key(&record.repository, &record.commit_hash, &record.review_kind);
                commits.insert(key.as_str(), json.as_str())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
        .context("State write task failed")?
    }
}

fn migration_backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".v1.backup");
    PathBuf::from(backup)
}

fn read_record(
    database: &Database,
    table: TableDefinition<&str, &str>,
    key: &str,
) -> Result<Option<ReviewRecord>> {
    let read = database.begin_read()?;
    let table = match read.open_table(table) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    Ok(table
        .get(key)?
        .map(|value| serde_json::from_str(value.value()))
        .transpose()?)
}

fn migrate_table(
    write: &redb::WriteTransaction,
    definition: TableDefinition<&str, &str>,
    reports_dir: Option<&Path>,
    pr_table: bool,
) -> Result<()> {
    let mut table = write.open_table(definition)?;
    let mut migrated = Vec::new();
    for item in table.iter()? {
        let (key, value) = item?;
        let mut record: ReviewRecord = serde_json::from_str(value.value())
            .with_context(|| format!("Failed to migrate state record {}", key.value()))?;
        if pr_table && let Some((repository, pr_number)) = parse_pr_state_key(key.value()) {
            record.repository = repository;
            record.pr_number = pr_number;
        }
        if record.artifacts.markdown.is_none()
            && let Some(root) = reports_dir
        {
            record.artifacts = discover_legacy_artifacts(root, &record);
        }
        migrated.push((key.value().to_string(), serde_json::to_string(&record)?));
    }
    for (key, value) in migrated {
        table.insert(key.as_str(), value.as_str())?;
    }
    Ok(())
}

fn discover_legacy_artifacts(root: &Path, record: &ReviewRecord) -> ArtifactPaths {
    let mut paths = ArtifactPaths::default();
    let repo = record.repository.replace('/', "_");
    let hash = record.commit_hash.chars().take(7).collect::<String>();
    let legacy_stem = if record.review_kind == DEFAULT_REVIEW_KIND {
        format!("{repo}_PR{}_{}_report", record.pr_number, hash)
    } else {
        format!(
            "{repo}_PR{}_{}_{}_report",
            record.pr_number, hash, record.review_kind
        )
    };
    let run_prefix = format!("{repo}_PR{}_{}", record.pr_number, record.review_kind);
    for candidate in walk_files(root) {
        let name = candidate
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let in_matching_run = candidate
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&run_prefix));
        if name == format!("{legacy_stem}.md") || (in_matching_run && name == "report.md") {
            paths.markdown = Some(to_absolute(candidate));
        } else if name == format!("{legacy_stem}.json")
            || (in_matching_run && name == "report.json")
        {
            paths.structured_json = Some(to_absolute(candidate));
        } else if name == format!("{legacy_stem}.policy.json")
            || (in_matching_run && name == "report.policy.json")
        {
            paths.policy_json = Some(to_absolute(candidate));
        } else if in_matching_run && name == "nspawn.log" {
            paths.sandbox_log = Some(to_absolute(candidate));
        }
    }
    paths
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

fn to_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_or(path.clone(), |directory| directory.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(commit_hash: &str, status: &str, retry_count: u32) -> ReviewMetadata {
        metadata_with_timestamp(
            commit_hash,
            status,
            retry_count,
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )
    }

    fn metadata_with_timestamp(
        commit_hash: &str,
        status: &str,
        retry_count: u32,
        timestamp: i64,
    ) -> ReviewMetadata {
        ReviewMetadata {
            repository: "owner/repo".to_string(),
            pr_number: 42,
            review_kind: "security".to_string(),
            commit_hash: commit_hash.to_string(),
            model: "test".to_string(),
            timestamp,
            findings_count: 0,
            status: status.parse().unwrap_or(ReviewStatus::Failed),
            severity: "none".to_string(),
            pr_classification: "none".to_string(),
            duration_secs: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: Some(0.0),
            report_url: None,
            is_rereview: false,
            time_reviewed: None,
            retry_count,
            artifacts: ArtifactPaths::default(),
            disclosure_url: None,
            buzz_thread: None,
            failure_stage: None,
        }
    }

    #[test]
    fn default_review_kind_uses_legacy_keys() {
        assert_eq!(
            pr_state_key("owner/repo", 42, DEFAULT_REVIEW_KIND),
            "owner/repo_42"
        );
        assert_eq!(
            commit_state_key("owner/repo", "abcdef", DEFAULT_REVIEW_KIND),
            "owner/repo_abcdef"
        );
    }

    #[test]
    fn persona_review_kind_uses_scoped_keys() {
        assert_eq!(
            pr_state_key("owner/repo", 42, "security"),
            "owner/repo|42|security"
        );
        assert_eq!(
            commit_state_key("owner/repo", "abcdef", "security"),
            "owner/repo|abcdef|security"
        );
    }

    #[test]
    fn list_key_parser_reads_legacy_and_scoped_pr_keys() {
        assert_eq!(
            parse_pr_state_key("owner/repo_42"),
            Some(("owner/repo".to_string(), 42))
        );
        assert_eq!(
            parse_pr_state_key("owner/repo|42|security"),
            Some(("owner/repo".to_string(), 42))
        );
    }

    #[test]
    fn failed_review_retries_until_limit() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(&db_path, repo, pr, &metadata(commit, "failed", 2)).unwrap();

        assert_eq!(
            should_review_with_retry_limit(&db_path, repo, pr, commit, review_kind, 30, 3).unwrap(),
            ReviewDecision::RetryFailed
        );
        assert_eq!(
            should_review_with_retry_limit(&db_path, repo, pr, commit, review_kind, 30, 2).unwrap(),
            ReviewDecision::Skip
        );
    }

    #[test]
    fn failed_review_retries_after_retry_limit_cooldown() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(
            &db_path,
            repo,
            pr,
            &metadata_with_timestamp(commit, "failed", 3, 0),
        )
        .unwrap();

        assert_eq!(
            should_review_with_retry_limit(&db_path, repo, pr, commit, review_kind, 30, 3).unwrap(),
            ReviewDecision::RetryFailed
        );
    }

    #[test]
    fn stale_in_progress_review_retries_same_commit() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(
            &db_path,
            repo,
            pr,
            &metadata_with_timestamp(commit, "in_progress", 0, 0),
        )
        .unwrap();

        assert_eq!(
            should_review_with_retry_limit(&db_path, repo, pr, commit, review_kind, 30, 3).unwrap(),
            ReviewDecision::RetryFailed
        );
    }

    #[test]
    fn fresh_in_progress_review_skips_same_commit() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(&db_path, repo, pr, &metadata(commit, "in_progress", 0)).unwrap();

        assert_eq!(
            should_review_with_retry_limit(&db_path, repo, pr, commit, review_kind, 30, 3).unwrap(),
            ReviewDecision::Skip
        );
    }

    #[test]
    fn rereview_preserves_buzz_thread_delivery_state() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let review_kind = "security";
        let mut first = metadata("old-commit", "confirmed", 0);
        first.buzz_thread = Some(BuzzThreadState {
            channel_id: "private".to_string(),
            root_event_id: "root-event".to_string(),
            published_finding_keys: vec!["finding-key".to_string()],
        });
        mark_reviewed(&db_path, repo, pr, &first).unwrap();

        assert!(lock_for_review(&db_path, repo, pr, "new-commit", review_kind, 30).unwrap());
        let locked = get_pr_review(&db_path, repo, pr, review_kind)
            .unwrap()
            .unwrap();
        assert_eq!(locked.buzz_thread, first.buzz_thread);

        let completed = metadata("new-commit", "confirmed", 0);
        mark_reviewed(&db_path, repo, pr, &completed).unwrap();
        let stored = get_pr_review(&db_path, repo, pr, review_kind)
            .unwrap()
            .unwrap();
        assert_eq!(stored.buzz_thread, first.buzz_thread);
    }

    #[test]
    fn retry_lock_increments_failed_review_retry_count() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(&db_path, repo, pr, &metadata(commit, "failed", 0)).unwrap();

        assert!(lock_for_review(&db_path, repo, pr, commit, review_kind, 30).unwrap());

        let locked = get_pr_review(&db_path, repo, pr, review_kind)
            .unwrap()
            .unwrap();
        assert_eq!(locked.status, "in_progress");
        assert_eq!(locked.retry_count, 1);
    }

    #[test]
    fn retry_lock_increments_stale_in_progress_retry_count() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let repo = "owner/repo";
        let pr = 42;
        let commit = "abcdef";
        let review_kind = "security";

        mark_reviewed(
            &db_path,
            repo,
            pr,
            &metadata_with_timestamp(commit, "in_progress", 1, 0),
        )
        .unwrap();

        assert!(lock_for_review(&db_path, repo, pr, commit, review_kind, 30).unwrap());

        let locked = get_pr_review(&db_path, repo, pr, review_kind)
            .unwrap()
            .unwrap();
        assert_eq!(locked.status, "in_progress");
        assert_eq!(locked.retry_count, 2);
    }

    #[tokio::test]
    async fn atomic_competing_claims_have_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let store = RedbStateStore::open(temp.path().join("state.redb"), None).unwrap();
        let mut record = metadata("abc123", "queued", 0);
        record.repository = "owner/repo".to_string();
        record.pr_number = 7;

        let (first, second) = tokio::join!(
            store.try_claim(record.clone(), 30),
            store.try_claim(record, 30)
        );
        assert_ne!(first.unwrap(), second.unwrap());
        assert_eq!(
            store
                .get("owner/repo", 7, "security")
                .await
                .unwrap()
                .unwrap()
                .status,
            ReviewStatus::InProgress
        );
    }

    #[test]
    fn migration_backs_up_and_discovers_legacy_report() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("state.redb");
        let reports = temp.path().join("reports");
        std::fs::create_dir(&reports).unwrap();
        let report = reports.join("owner_repo_PR42_abc1234_security_report.md");
        std::fs::write(&report, "report").unwrap();
        let legacy = metadata("abc1234", "confirmed", 0);
        mark_reviewed(&db_path, "owner/repo", 42, &legacy).unwrap();

        let store = RedbStateStore::open(&db_path, Some(&reports)).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let migrated = runtime
            .block_on(store.get("owner/repo", 42, "security"))
            .unwrap()
            .unwrap();

        assert_eq!(migrated.repository, "owner/repo");
        assert_eq!(migrated.pr_number, 42);
        assert_eq!(migrated.artifacts.markdown, Some(report));
        assert!(migration_backup_path(&db_path).exists());
    }
}
