use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const PR_STATE: TableDefinition<&str, &str> = TableDefinition::new("pr_state");
const COMMIT_STATE: TableDefinition<&str, &str> = TableDefinition::new("commit_state");
pub const DEFAULT_REVIEW_KIND: &str = "default";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewMetadata {
    #[serde(default = "default_review_kind")]
    pub review_kind: String,
    pub commit_hash: String,
    pub model: String,
    pub timestamp: i64, // Unix timestamp of when the review completed
    pub findings_count: u32,
    pub status: String,
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
}

fn default_review_kind() -> String {
    DEFAULT_REVIEW_KIND.to_string()
}

#[derive(Debug, PartialEq, Eq)]
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
            let json_str =
                serde_json::to_string(metadata).context("Failed to serialize ReviewMetadata")?;
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

            let metadata = ReviewMetadata {
                review_kind: review_kind.to_string(),
                commit_hash: commit_hash.to_string(),
                model: "daemon".to_string(),
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                findings_count: 0,
                status: "in_progress".to_string(),
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
            };

            let metadata = if let Some(value) = pr_table.get(key.as_str())? {
                let json_str = value.value();
                if let Ok(previous) = serde_json::from_str::<ReviewMetadata>(json_str) {
                    if previous.commit_hash == commit_hash
                        && (previous.status == "failed" || previous.status == "in_progress")
                    {
                        ReviewMetadata {
                            retry_count: previous.retry_count.saturating_add(1),
                            ..metadata
                        }
                    } else {
                        metadata
                    }
                } else {
                    metadata
                }
            } else {
                metadata
            };

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
            review_kind: "security".to_string(),
            commit_hash: commit_hash.to_string(),
            model: "test".to_string(),
            timestamp,
            findings_count: 0,
            status: status.to_string(),
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
}
