//! Trusted, bounded revisions of PRs whose creation the host journaled.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    FixKind,
    config::{IssueConfig, Project},
    github::{self, Github, git},
    worker::{self, Feedback, Revision},
    workflow::{Store, digest},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct State {
    pub pr: u64,
    issue: u64,
    branch: String,
    base_branch: String,
    head: String,
    author: String,
    fix_kind: FixKind,
    checks: Vec<Vec<String>>,
    attempts: u32,
    last_command: u64,
    pending: Option<PendingPush>,
    pub review_pending: Option<String>,
    notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingPush {
    commit: String,
    check: Vec<String>,
}

impl State {
    pub(super) fn new(
        pr: u64,
        issue: u64,
        branch: &str,
        base_branch: &str,
        head: &str,
        author: &str,
        validation: (FixKind, Vec<String>),
    ) -> Self {
        Self {
            pr,
            issue,
            branch: branch.into(),
            base_branch: base_branch.into(),
            head: head.into(),
            author: author.into(),
            fix_kind: validation.0,
            checks: vec![validation.1],
            attempts: 0,
            last_command: 0,
            pending: None,
            review_pending: Some(head.into()),
            notice: None,
        }
    }

    fn reconcile(&mut self, head: &str) -> Result<()> {
        if let Some(pending) = self.pending.take() {
            if head == pending.commit {
                self.head = pending.commit;
                if !self.checks.contains(&pending.check) {
                    self.checks.push(pending.check);
                }
                self.review_pending = Some(self.head.clone());
                self.notice = Some(format!(
                    "Verified update pushed at `{}`. A fresh review is pending in the Fiach daemon.",
                    self.head
                ));
            } else {
                ensure!(
                    head == self.head,
                    "PR branch changed outside Fiach; refusing to overwrite it"
                );
                self.notice = Some(
                    "The verified update was not published. Inspect the previous attempt and post a new trusted fix command to retry.".into()
                );
            }
        }
        ensure!(
            head == self.head,
            "PR branch changed outside Fiach; maintainer intervention required"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PrRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    repo: Repository,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct Repository {
    full_name: String,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct Author {
    login: String,
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PullRequest {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    merged: bool,
    user: Author,
    head: PrRef,
    base: PrRef,
}

impl PullRequest {
    fn validate(&self, repo: &str, state: &State) -> Result<()> {
        ensure!(
            self.number == state.pr && self.state == "open" && !self.merged,
            "PR is no longer open"
        );
        ensure!(
            self.head.repo.full_name.eq_ignore_ascii_case(repo)
                && self.base.repo.full_name.eq_ignore_ascii_case(repo)
                && self.head.name == state.branch
                && self.base.name == state.base_branch
                && self.user.login.eq_ignore_ascii_case(&state.author),
            "PR ownership or target changed; refusing iteration"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Comment {
    id: u64,
    user: Author,
    body: String,
}

pub(crate) fn fix_command(body: &str, mention: &str) -> bool {
    let mut words = body.split_whitespace();
    words.next().is_some_and(|word| {
        word.eq_ignore_ascii_case(&format!("@{}", mention.trim_start_matches('@')))
    }) && words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("fix"))
}

fn latest_command<'a>(
    comments: &'a [Comment],
    config: &IssueConfig,
    bot: &str,
) -> Option<&'a Comment> {
    let mention = config.trigger_mention.as_deref()?;
    comments
        .iter()
        .filter(|comment| {
            !comment.user.login.eq_ignore_ascii_case(bot)
                && config
                    .allowed_mention_users
                    .iter()
                    .any(|user| user.eq_ignore_ascii_case(&comment.user.login))
                && fix_command(&comment.body, mention)
        })
        .max_by_key(|comment| comment.id)
}

async fn pull_request(repo: &str, number: u64) -> Result<PullRequest> {
    serde_json::from_value(github::api(&format!("repos/{repo}/pulls/{number}"), "GET", None).await?)
        .context("Invalid PR metadata")
}

async fn comments(github: &Github, repo: &str, number: u64) -> Result<Vec<Comment>> {
    github
        .comments(repo, number)
        .await?
        .into_iter()
        .map(|value| serde_json::from_value(value).context("Invalid PR comment"))
        .collect()
}

async fn feedback(repo: &str, pr: &PullRequest, comments: &[Comment]) -> Result<Vec<Feedback>> {
    let mut result: Vec<_> = comments
        .iter()
        .filter(|comment| !comment.body.starts_with("<!-- fiach-pr-iteration:"))
        .map(|comment| Feedback {
            author: comment.user.login.clone(),
            body: comment.body.clone(),
            path: None,
            line: None,
        })
        .collect();
    for suffix in ["reviews", "comments"] {
        for value in
            github::pages(&format!("repos/{repo}/pulls/{}/{suffix}", pr.number), 2000).await?
        {
            // Do not feed dismissed, pending or outdated review findings into a new revision.
            if value["commit_id"] != pr.head.sha
                || matches!(value["state"].as_str(), Some("DISMISSED" | "PENDING"))
                || (suffix == "comments" && value["line"].is_null())
            {
                continue;
            }
            let Some(body) = value["body"]
                .as_str()
                .filter(|body| !body.trim().is_empty())
            else {
                continue;
            };
            result.push(Feedback {
                author: value["user"]["login"]
                    .as_str()
                    .context("Missing review author")?
                    .into(),
                body: body.into(),
                path: value["path"].as_str().map(str::to_owned),
                line: value["line"].as_u64(),
            });
        }
    }
    Ok(result)
}

pub(super) async fn process(
    config: &IssueConfig,
    project: &Project,
    issue: u64,
    github: &Github,
    store: &Store,
    cancel: &CancellationToken,
    reviews: Option<&crate::daemon::GeneratedReviewQueue>,
) -> Result<bool> {
    let Some(policy) = &config.iterations else {
        return Ok(false);
    };
    if !config.publish {
        return Ok(false);
    }
    let key = format!("{}#{issue}", project.repo);
    let Some(mut state) = store.iteration(&key)? else {
        return Ok(false);
    };
    ensure!(
        state.issue == issue && state.branch == format!("fiach/issue-{issue}"),
        "Invalid recorded PR identity"
    );
    let pr = pull_request(&project.repo, state.pr).await?;
    // Closed or merged PRs receive neither comments nor branch writes.
    if pr.state != "open" || pr.merged {
        return Ok(false);
    }
    pr.validate(&project.repo, &state)?;
    state.reconcile(&pr.head.sha)?;
    store.save_iteration(&key, &state)?;
    publish_notice(github, &project.repo, &mut state).await?;
    store.save_iteration(&key, &state)?;

    if state.review_pending.as_deref() == Some(pr.head.sha.as_str())
        && let Some(reviews) = reviews
        && reviews
            .submit(
                &project.repo,
                &crate::github::PullRequestSummary {
                    number: pr.number,
                    title: pr.title.clone(),
                    head_ref_oid: pr.head.sha.clone(),
                    head_ref_name: pr.head.name.clone(),
                    author_association: String::new(),
                },
            )
            .await?
    {
        state.review_pending = None;
        store.save_iteration(&key, &state)?;
    }

    if !config.auto_fix || config.worker.is_none() || cancel.is_cancelled() {
        return Ok(false);
    }
    let discussion = comments(github, &project.repo, state.pr).await?;
    let Some(command) = latest_command(&discussion, config, &github.login) else {
        return Ok(false);
    };
    if command.id <= state.last_command {
        return Ok(false);
    }
    state.last_command = command.id;
    if state.attempts >= policy.max_attempts {
        state.notice = Some(format!(
            "The limit of {} PR fix attempts has been reached. A maintainer must take over.",
            policy.max_attempts
        ));
        store.save_iteration(&key, &state)?;
        publish_notice(github, &project.repo, &mut state).await?;
        store.save_iteration(&key, &state)?;
        return Ok(true);
    }
    // Save the attempt before starting an agent, so a crash cannot repeat a paid coding run.
    state.attempts += 1;
    store.save_iteration(&key, &state)?;
    let context = Iteration {
        config,
        project,
        github,
        store,
        key: &key,
        cancel,
    };
    if let Err(error) = context.attempt(&mut state, &pr, command, &discussion).await {
        tracing::warn!(repo = %project.repo, pr = state.pr, command = command.id, error = %format!("{error:#}"), "PR iteration stopped");
        state.notice = Some("The PR fix attempt stopped before completion. Inspect the local iteration artifacts and logs before posting a new fix command. Any interrupted push will be reconciled before another attempt.".into());
        store.save_iteration(&key, &state)?;
        // Recheck lifecycle before any status comment, including failure reports.
        let fresh = pull_request(&project.repo, state.pr).await?;
        if fresh.state == "open" && !fresh.merged {
            publish_notice(github, &project.repo, &mut state).await?;
            store.save_iteration(&key, &state)?;
        }
        return Err(error);
    }
    publish_notice(github, &project.repo, &mut state).await?;
    store.save_iteration(&key, &state)?;
    Ok(true)
}

struct Iteration<'a> {
    config: &'a IssueConfig,
    project: &'a Project,
    github: &'a Github,
    store: &'a Store,
    key: &'a str,
    cancel: &'a CancellationToken,
}

impl Iteration<'_> {
    async fn attempt(
        &self,
        state: &mut State,
        pr: &PullRequest,
        command: &Comment,
        discussion: &[Comment],
    ) -> Result<()> {
        let repo = &self.project.repo;
        let issue = self.github.issue(repo, state.issue).await?;
        ensure!(
            issue.open && !issue.is_pr,
            "Original issue is no longer open"
        );
        let review_feedback = feedback(repo, pr, discussion).await?;
        let revision = Revision {
            pr_number: state.pr,
            branch: state.branch.clone(),
            head: state.head.clone(),
            feedback: review_feedback,
            previous_checks: state.checks.clone(),
        };
        let fix = worker::fix_at(
            self.config
                .worker
                .as_ref()
                .context("Missing issue worker")?,
            &issue,
            self.project,
            state.fix_kind,
            &self.config.scratch_dir,
            self.cancel,
            Some(&revision),
        )
        .await?;
        let artifacts = self
            .config
            .state_path
            .with_extension("artifacts")
            .join(repo.replace('/', "_"))
            .join(state.issue.to_string())
            .join(format!("iteration-{}", command.id));
        tokio::fs::create_dir_all(&artifacts).await?;
        tokio::fs::write(artifacts.join("patch.diff"), &fix.patch).await?;
        tokio::fs::write(
            artifacts.join("report.json"),
            serde_json::to_vec_pretty(&fix.report)?,
        )
        .await?;
        if fix.report.status != "candidate" {
            state.notice = Some("The coding agent could not prepare a verified revision. A maintainer must inspect the iteration report before retrying.".into());
            return Ok(());
        }
        ensure!(
            !self.cancel.is_cancelled(),
            "Iteration cancelled before publication"
        );
        let fresh = pull_request(repo, state.pr).await?;
        fresh.validate(repo, state)?;
        ensure!(
            &fresh == pr,
            "PR head, base, or lifecycle changed during iteration"
        );
        let fresh_issue = self.github.issue(repo, state.issue).await?;
        ensure!(
            fresh_issue == issue,
            "Original issue changed during iteration"
        );
        let fresh_comments = comments(self.github, repo, state.pr).await?;
        let fresh_command = latest_command(&fresh_comments, self.config, &self.github.login)
            .context("Trusted fix command was removed")?;
        ensure!(
            digest(fresh_command)? == digest(command)?,
            "Trusted fix command changed during iteration"
        );
        ensure!(
            digest(&feedback(repo, &fresh, &fresh_comments).await?)? == digest(&revision.feedback)?,
            "PR feedback changed during iteration"
        );
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
                &format!("Address review feedback on PR #{}", state.pr),
            ],
        )
        .await?;
        let commit = git(fix.checkout.path(), &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let parent = git(fix.checkout.path(), &["rev-parse", "HEAD^"]).await?;
        ensure!(
            parent.trim() == state.head,
            "Revision is not a direct child of the recorded PR head"
        );
        state.pending = Some(PendingPush {
            commit: commit.clone(),
            check: fix.report.reproduction.clone(),
        });
        self.store.save_iteration(self.key, state)?;
        // The patch is a direct child; the lease prevents overwriting even a concurrent fast-forward.
        git(
            fix.checkout.path(),
            &[
                "push",
                &format!(
                    "--force-with-lease=refs/heads/{}:{}",
                    state.branch, state.head
                ),
                "origin",
                &format!("{commit}:refs/heads/{}", state.branch),
            ],
        )
        .await?;
        state.reconcile(&commit)?;
        self.store.save_iteration(self.key, state)?;
        Ok(())
    }
}

async fn publish_notice(github: &Github, repo: &str, state: &mut State) -> Result<()> {
    let Some(message) = &state.notice else {
        return Ok(());
    };
    let fresh = pull_request(repo, state.pr).await?;
    fresh.validate(repo, state)?;
    let marker = format!(
        "<!-- fiach-pr-iteration:issue={}:command={} -->",
        state.issue, state.last_command
    );
    let body = format!("{marker}\n{message}");
    let comments = github.comments(repo, state.pr).await?;
    if let Some(existing) = comments.iter().find(|comment| {
        comment["user"]["login"]
            .as_str()
            .is_some_and(|author| author.eq_ignore_ascii_case(&github.login))
            && comment["body"]
                .as_str()
                .is_some_and(|text| text.starts_with(&marker))
    }) {
        if existing["body"] != body {
            let id = existing["id"]
                .as_u64()
                .context("Missing status comment ID")?;
            github::api(
                &format!("repos/{repo}/issues/comments/{id}"),
                "PATCH",
                Some(json!({"body": body})),
            )
            .await?;
        }
    } else {
        github::api(
            &format!("repos/{repo}/issues/{}/comments", state.pr),
            "POST",
            Some(json!({"body": body})),
        )
        .await?;
    }
    state.notice = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(
            9,
            1,
            "fiach/issue-1",
            "main",
            "old",
            "bot",
            (FixKind::Bug, vec!["test".into()]),
        )
    }

    #[test]
    fn commands_require_a_direct_mention_and_exact_verb() {
        assert!(fix_command("@fiach-bot fix", "fiach-bot"));
        assert!(fix_command(
            " @FIACH-BOT FIX please address the findings",
            "@fiach-bot"
        ));
        for text in [
            "please @fiach-bot fix",
            "> @fiach-bot fix",
            "```\n@fiach-bot fix",
            "@fiach-bot fixes",
            "@fiach-bot2 fix",
            "@fiach-bot review",
        ] {
            assert!(!fix_command(text, "fiach-bot"), "{text}");
        }
    }

    #[test]
    fn interrupted_push_reconciles_without_overwriting_other_commits() {
        let mut state = state();
        state.pending = Some(PendingPush {
            commit: "new".into(),
            check: vec!["new-test".into()],
        });
        let mut failed = state.clone();
        failed.reconcile("old").unwrap();
        assert_eq!(failed.head, "old");
        assert!(failed.pending.is_none());
        assert!(state.clone().reconcile("somebody-elses-commit").is_err());
        state.reconcile("new").unwrap();
        assert_eq!(state.head, "new");
        assert_eq!(state.review_pending.as_deref(), Some("new"));
        assert_eq!(state.checks.len(), 2);
        state.reconcile("new").unwrap();
        assert_eq!(state.checks.len(), 2);
    }

    #[test]
    fn service_account_cannot_trigger_itself_even_if_allowlisted() {
        let config = IssueConfig {
            trigger_mention: Some("bot".into()),
            allowed_mention_users: vec!["maintainer".into(), "bot".into()],
            ..Default::default()
        };
        let comments = vec![
            Comment {
                id: 1,
                user: Author {
                    login: "Maintainer".into(),
                },
                body: "@bot fix".into(),
            },
            Comment {
                id: 2,
                user: Author {
                    login: "bot".into(),
                },
                body: "@bot fix".into(),
            },
            Comment {
                id: 3,
                user: Author {
                    login: "outsider".into(),
                },
                body: "@bot fix".into(),
            },
        ];
        assert_eq!(latest_command(&comments, &config, "bot").unwrap().id, 1);
    }
}
