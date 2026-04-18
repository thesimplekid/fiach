use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const PR_STATE: TableDefinition<&str, &str> = TableDefinition::new("pr_state");
const COMMIT_STATE: TableDefinition<&str, &str> = TableDefinition::new("commit_state");

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewMetadata {
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
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Skip,
    FirstReview,
    ReReview,
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
    force: bool,
) -> Result<ReviewDecision> {
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

        let key = format!("{}_{}", repo, pr);
        if let Some(value) = table.get(key.as_str())? {
            let json_str = value.value();
            match serde_json::from_str::<ReviewMetadata>(json_str) {
                Ok(metadata) => {
                    if metadata.status == "in_progress" {
                        tracing::debug!(
                            repo, pr,
                            "PR review is already in progress, skipping"
                        );
                        return Ok(ReviewDecision::Skip);
                    }
                    if metadata.commit_hash == current_hash {
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
            let pr_key = format!("{}_{}", repo, pr);
            let json_str =
                serde_json::to_string(metadata).context("Failed to serialize ReviewMetadata")?;
            pr_table.insert(pr_key.as_str(), json_str.as_str())?;

            let mut commit_table = write_txn.open_table(COMMIT_STATE)?;
            let commit_key = format!("{}_{}", repo, metadata.commit_hash);
            commit_table.insert(commit_key.as_str(), json_str.as_str())?;
        }
        write_txn.commit()?;

        tracing::debug!("Successfully recorded review metadata in database");
        Ok(())
    })
}

/// Locks a PR for review by marking it as in_progress.
/// Returns true if the lock was acquired, false if it's already in progress by another process.
pub fn lock_for_review(db_path: &Path, repo: &str, pr: u64, commit_hash: &str) -> Result<bool> {
    if !db_path.exists() {
        // Proceed and let Database::create handle it
    }

    with_retries(|| {
        let db = Database::create(db_path).context("Failed to open or create redb database")?;
        let write_txn = db.begin_write()?;
        
        {
            let mut pr_table = write_txn.open_table(PR_STATE)?;
            let key = format!("{}_{}", repo, pr);
            
            if let Some(value) = pr_table.get(key.as_str())? {
                let json_str = value.value();
                if let Ok(metadata) = serde_json::from_str::<ReviewMetadata>(json_str) {
                    if metadata.status == "in_progress" {
                        return Ok(false);
                    }
                }
            }
            
            let metadata = ReviewMetadata {
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
                time_reviewed: Some(time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()),
            };
            
            let json_str = serde_json::to_string(&metadata).context("Failed to serialize ReviewMetadata")?;
            pr_table.insert(key.as_str(), json_str.as_str())?;
        }
        
        write_txn.commit()?;
        Ok(true)
    })
}

/// Retrieves review metadata for a specific commit hash.
pub fn get_commit_review(
    db_path: &Path,
    repo: &str,
    commit_hash: &str,
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

        let key = format!("{}_{}", repo, commit_hash);
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

            // Key format is "{repo}_{pr}"
            #[allow(clippy::collapsible_if)]
            if let Some((repo, pr_str)) = key.rsplit_once('_') {
                if let Ok(pr) = pr_str.parse::<u64>() {
                    let json_str = value_guard.value();
                    if let Ok(metadata) = serde_json::from_str::<ReviewMetadata>(json_str) {
                        reviews.push((repo.to_string(), pr, metadata));
                    }
                }
            }
        }

        // Sort by timestamp descending (newest first)
        reviews.sort_by(|a, b| b.2.timestamp.cmp(&a.2.timestamp));

        Ok(reviews)
    })
}
