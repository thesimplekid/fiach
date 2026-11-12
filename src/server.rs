use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::state::RedbStateStore;

#[derive(Clone)]
pub struct AppState {
    pub state_store: Arc<RedbStateStore>,
    pub scheduler: crate::scheduler::SchedulerHandle<crate::daemon::DaemonWork>,
    pub provider_circuit: crate::broker::CircuitBreaker,
    pub server_token: Option<String>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    /// Complete GitHub owner/name or Forgejo HTTPS target. Legacy owner/repo
    /// fields remain accepted when repository is omitted.
    pub repository: Option<String>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
    pub pr: u64,
    pub persona: Option<String>,
}

#[derive(Deserialize)]
pub struct TriggerReviewRequest {
    /// Complete GitHub owner/name or Forgejo HTTPS target. Legacy owner/repo
    /// fields remain accepted when repository is omitted.
    pub repository: Option<String>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
    pub pr: u64,
    pub persona: Option<String>,
}

fn repository_target(repository: Option<&str>, owner: &str, repo: &str) -> Result<String> {
    let target = match repository {
        Some(target) => {
            anyhow::ensure!(
                owner.is_empty() && repo.is_empty(),
                "Use repository or owner/repo, not both"
            );
            target.to_owned()
        }
        None => format!("{owner}/{repo}"),
    };
    crate::forge::Repository::parse(&target)?;
    Ok(target)
}

fn review_kind_from_query(persona: Option<&str>) -> String {
    let Some(persona) = persona.map(str::trim).filter(|persona| !persona.is_empty()) else {
        return crate::state::DEFAULT_REVIEW_KIND.to_string();
    };

    if persona.starts_with("builtin:") || persona.contains('/') || persona.ends_with(".md") {
        match crate::persona::PersonaSource::from_str(persona) {
            Ok(source) => source.review_kind(),
            Err(never) => match never {},
        }
    } else {
        persona.to_string()
    }
}

fn request_authorized(headers: &HeaderMap, server_token: Option<&str>) -> bool {
    let Some(server_token) = server_token else {
        return true;
    };

    if let Some(token) = headers
        .get("x-fiach-token")
        .and_then(|value| value.to_str().ok())
        && token == server_token
    {
        return true;
    }

    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == server_token)
}

fn unauthorized_response() -> axum::response::Response {
    (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
}

pub async fn start_server(port: u16, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/health/reviews", get(review_processing_health))
        .route("/ready", get(readiness_check))
        .route("/metrics", get(metrics))
        .route("/reviews", get(get_reviews))
        .route("/review", get(get_review).post(trigger_review))
        .route("/review/content", get(get_review_content))
        .route("/jobs/{job_id}", get(get_job))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("Starting web server on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Recent processing health plus the provider circuit and durable failures.
#[derive(Serialize)]
struct ReviewHealth {
    #[serde(flatten)]
    processing: crate::scheduler::ProcessingHealth,
    provider_circuit: crate::broker::CircuitSnapshot,
    /// Failed reviews the daemon will not retry or recover on its own.
    reviews_needing_intervention: usize,
    /// Failed reviews by failure class; `unknown` predates classification.
    failed_reviews_by_class: BTreeMap<&'static str, usize>,
    /// Earliest whole-review retry still in the future.
    next_review_retry_at: Option<i64>,
}

#[derive(Default)]
struct FailedReviewSummary {
    needing_intervention: usize,
    by_class: BTreeMap<&'static str, usize>,
    next_retry_at: Option<i64>,
}

fn summarize_failed_reviews(
    reviews: &[crate::state::ReviewRecord],
    now: i64,
) -> FailedReviewSummary {
    let mut summary = FailedReviewSummary {
        by_class: BTreeMap::from([
            ("provider", 0),
            ("provider_unavailable", 0),
            ("review", 0),
            ("unknown", 0),
        ]),
        ..FailedReviewSummary::default()
    };
    for review in reviews
        .iter()
        .filter(|review| review.status == crate::state::ReviewStatus::Failed)
    {
        let class = review
            .failure_class
            .map_or("unknown", crate::state::FailureClass::as_str);
        *summary.by_class.entry(class).or_default() += 1;
        if review.needs_intervention {
            summary.needing_intervention += 1;
        }
        if let Some(at) = review.retry_not_before.filter(|at| *at > now) {
            summary.next_retry_at = Some(summary.next_retry_at.map_or(at, |next| next.min(at)));
        }
    }
    summary
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn review_health(
    mut processing: crate::scheduler::ProcessingHealth,
    provider_circuit: crate::broker::CircuitSnapshot,
    reviews: &[crate::state::ReviewRecord],
) -> ReviewHealth {
    if provider_circuit.state == crate::broker::CircuitState::Open {
        processing.degraded = true;
        processing.reasons.push("provider_circuit_open");
    }
    let summary = summarize_failed_reviews(reviews, unix_now());
    ReviewHealth {
        processing,
        provider_circuit,
        reviews_needing_intervention: summary.needing_intervention,
        failed_reviews_by_class: summary.by_class,
        next_review_retry_at: summary.next_retry_at,
    }
}

/// Operational degradation is an alert signal, not a process restart signal.
async fn review_processing_health(State(state): State<AppState>) -> impl IntoResponse {
    let reviews = match state.state_store.list().await {
        Ok(reviews) => reviews,
        Err(error) => {
            tracing::error!(error = %error, "Review health could not read review state");
            Vec::new()
        }
    };
    Json(review_health(
        state.scheduler.processing_health().await,
        state.provider_circuit.snapshot(),
        &reviews,
    ))
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    scheduler: crate::scheduler::SchedulerStats,
    reviews: usize,
    review_processing: ReviewHealth,
}

async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    let scheduler = state.scheduler.stats().await;
    if !scheduler.accepting {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Scheduler is not accepting work",
        )
            .into_response();
    }

    match state.state_store.list().await {
        Ok(reviews) => (
            StatusCode::OK,
            Json(ReadinessResponse {
                status: "ready",
                scheduler,
                reviews: reviews.len(),
                review_processing: review_health(
                    state.scheduler.processing_health().await,
                    state.provider_circuit.snapshot(),
                    &reviews,
                ),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "Readiness check could not read review state");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Review state is unavailable",
            )
                .into_response()
        }
    }
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let scheduler = state.scheduler.stats().await;
    match state.state_store.list().await {
        Ok(reviews) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            format!(
                "{}{}{}",
                render_metrics(&scheduler, &reviews),
                render_processing_metrics(&state.scheduler.processing_health().await),
                render_circuit_metrics(&state.provider_circuit.snapshot()),
            ),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "Metrics endpoint could not read review state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Review state is unavailable",
            )
                .into_response()
        }
    }
}

fn render_processing_metrics(health: &crate::scheduler::ProcessingHealth) -> String {
    let mut output = String::new();
    for (name, value) in [
        ("degraded", u64::from(health.degraded)),
        (
            "oldest_queued_age_seconds",
            health.oldest_queued_age_secs.unwrap_or(0),
        ),
        ("recent_attempts", health.recent_attempts as u64),
        ("recent_failures", health.recent_failures as u64),
        ("consecutive_failures", health.consecutive_failures),
        (
            "last_success_timestamp_seconds",
            health.last_success_at.unwrap_or(0).max(0) as u64,
        ),
    ] {
        let _ = writeln!(
            output,
            "# TYPE fiach_review_processing_{name} gauge\nfiach_review_processing_{name} {value}"
        );
    }
    if let Some(rate) = health.recent_failure_rate {
        let _ = writeln!(
            output,
            "# TYPE fiach_review_processing_recent_failure_rate gauge\nfiach_review_processing_recent_failure_rate {rate}"
        );
    }
    if let Some(failure) = &health.last_failure {
        // Labels are fixed host categories, never provider or model text.
        let _ = writeln!(
            output,
            "# HELP fiach_review_processing_last_failure The most recent failed review attempt.\n\
             # TYPE fiach_review_processing_last_failure gauge\n\
             fiach_review_processing_last_failure{{stage=\"{}\",reason=\"{}\",class=\"{}\",needs_intervention=\"{}\"}} 1",
            failure.stage,
            failure.reason,
            failure.class.as_str(),
            failure.needs_intervention,
        );
        if let Some(at) = failure.retry_not_before {
            let _ = writeln!(
                output,
                "# TYPE fiach_review_processing_last_failure_retry_not_before_timestamp_seconds gauge\n\
                 fiach_review_processing_last_failure_retry_not_before_timestamp_seconds {at}"
            );
        }
    }
    output
}

fn render_circuit_metrics(circuit: &crate::broker::CircuitSnapshot) -> String {
    let mut output = String::from(
        "# HELP fiach_provider_circuit_state Provider circuit breaker state (1 for the current state).\n\
         # TYPE fiach_provider_circuit_state gauge\n",
    );
    for state in [
        crate::broker::CircuitState::Closed,
        crate::broker::CircuitState::Open,
        crate::broker::CircuitState::Disabled,
    ] {
        let _ = writeln!(
            output,
            "fiach_provider_circuit_state{{state=\"{}\"}} {}",
            state.as_str(),
            u8::from(circuit.state == state)
        );
    }
    for (name, kind, value) in [
        (
            "consecutive_failures",
            "gauge",
            u64::from(circuit.consecutive_failures),
        ),
        (
            "failure_threshold",
            "gauge",
            u64::from(circuit.failure_threshold),
        ),
        (
            "opened_timestamp_seconds",
            "gauge",
            circuit.opened_at.unwrap_or(0).max(0) as u64,
        ),
        (
            "last_probe_timestamp_seconds",
            "gauge",
            circuit.last_probe_at.unwrap_or(0).max(0) as u64,
        ),
        ("opens_total", "counter", circuit.opens_total),
        ("probes_total", "counter", circuit.probes_total),
    ] {
        let _ = writeln!(
            output,
            "# TYPE fiach_provider_circuit_{name} {kind}\nfiach_provider_circuit_{name} {value}"
        );
    }
    output
}

fn render_metrics(
    scheduler: &crate::scheduler::SchedulerStats,
    reviews: &[crate::state::ReviewRecord],
) -> String {
    let mut output = String::from(
        "# HELP fiach_scheduler_accepting Whether the scheduler accepts new work.\n\
         # TYPE fiach_scheduler_accepting gauge\n",
    );
    let _ = writeln!(
        output,
        "fiach_scheduler_accepting {}",
        usize::from(scheduler.accepting)
    );
    output.push_str(
        "# HELP fiach_scheduler_jobs Current in-memory jobs by status.\n\
         # TYPE fiach_scheduler_jobs gauge\n",
    );
    for (status, value) in [
        ("queued", scheduler.queued),
        ("running", scheduler.running),
        ("completed", scheduler.completed),
        ("failed", scheduler.failed),
        ("skipped", scheduler.skipped),
        ("cancelled", scheduler.cancelled),
    ] {
        let _ = writeln!(
            output,
            "fiach_scheduler_jobs{{status=\"{status}\"}} {value}"
        );
    }

    let mut review_counts = BTreeMap::from([
        ("already-reported", 0usize),
        ("confirmed", 0),
        ("failed", 0),
        ("in_progress", 0),
        ("markdown-only", 0),
        ("none", 0),
        ("queued", 0),
        ("rejected", 0),
        ("skipped", 0),
        ("unverified", 0),
    ]);
    let mut recorded_cost = 0.0;
    for review in reviews {
        *review_counts.entry(review.status.as_str()).or_default() += 1;
        recorded_cost += review.cost_usd.unwrap_or(0.0);
    }
    output.push_str(
        "# HELP fiach_reviews Current durable review records by status.\n\
         # TYPE fiach_reviews gauge\n",
    );
    for (status, value) in review_counts {
        let _ = writeln!(output, "fiach_reviews{{status=\"{status}\"}} {value}");
    }
    output.push_str(
        "# HELP fiach_recorded_review_cost_usd Sum of cost recorded on current durable reviews.\n\
         # TYPE fiach_recorded_review_cost_usd gauge\n",
    );
    let _ = writeln!(output, "fiach_recorded_review_cost_usd {recorded_cost:.6}");

    let failed = summarize_failed_reviews(reviews, unix_now());
    output.push_str(
        "# HELP fiach_failed_reviews Current durable failed reviews by failure class.\n\
         # TYPE fiach_failed_reviews gauge\n",
    );
    for (class, value) in &failed.by_class {
        let _ = writeln!(output, "fiach_failed_reviews{{class=\"{class}\"}} {value}");
    }
    output.push_str(
        "# HELP fiach_reviews_needing_intervention Failed reviews the daemon will not recover on its own.\n\
         # TYPE fiach_reviews_needing_intervention gauge\n",
    );
    let _ = writeln!(
        output,
        "fiach_reviews_needing_intervention {}",
        failed.needing_intervention
    );
    if let Some(at) = failed.next_retry_at {
        output.push_str(
            "# HELP fiach_review_next_retry_timestamp_seconds Earliest whole-review retry still in the future.\n\
             # TYPE fiach_review_next_retry_timestamp_seconds gauge\n",
        );
        let _ = writeln!(output, "fiach_review_next_retry_timestamp_seconds {at}");
    }
    output
}

async fn get_reviews(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    match state.state_store.list().await {
        Ok(reviews) => (StatusCode::OK, Json(reviews)).into_response(),
        Err(e) => {
            tracing::error!("Failed to list reviews: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list reviews: {}", e),
            )
                .into_response()
        }
    }
}

async fn get_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ReviewQuery>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    let repo_full = match repository_target(query.repository.as_deref(), &query.owner, &query.repo)
    {
        Ok(repo) => repo,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let review_kind = review_kind_from_query(query.persona.as_deref());
    match state
        .state_store
        .get(&repo_full, query.pr, &review_kind)
        .await
    {
        Ok(Some(metadata)) => (StatusCode::OK, Json(metadata)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "Review not found").into_response(),
        Err(e) => {
            tracing::error!("Failed to get review: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get review: {}", e),
            )
                .into_response()
        }
    }
}

async fn get_review_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ReviewQuery>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    let repo_full = match repository_target(query.repository.as_deref(), &query.owner, &query.repo)
    {
        Ok(repo) => repo,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let review_kind = review_kind_from_query(query.persona.as_deref());
    let metadata = match state
        .state_store
        .get(&repo_full, query.pr, &review_kind)
        .await
    {
        Ok(Some(m)) => m,
        Ok(None) => return (StatusCode::NOT_FOUND, "Review not found").into_response(),
        Err(e) => {
            tracing::error!("Failed to get review metadata: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get review: {}", e),
            )
                .into_response();
        }
    };

    let Some(file_path) = metadata.artifacts.markdown else {
        return (
            StatusCode::NOT_FOUND,
            "Review has no recorded Markdown artifact",
        )
            .into_response();
    };

    match tokio::fs::read_to_string(&file_path).await {
        Ok(content) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to read report file {:?}: {}", file_path, e);
            (StatusCode::NOT_FOUND, "Report file not found on disk").into_response()
        }
    }
}

async fn trigger_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TriggerReviewRequest>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    let repo_full =
        match repository_target(payload.repository.as_deref(), &payload.owner, &payload.repo) {
            Ok(repo) => repo,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
    let request =
        crate::daemon::manual_request(repo_full.clone(), payload.pr, payload.persona.clone());

    match state.scheduler.submit(request).await {
        Ok(job) => {
            tracing::info!("Triggered review for {}/{}", repo_full, payload.pr);
            (
                StatusCode::ACCEPTED,
                Json(AcceptedJob {
                    job_id: job.job_id,
                    status: job.status,
                }),
            )
                .into_response()
        }
        Err(crate::scheduler::SubmitError::Full) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Review queue is full".to_string(),
        )
            .into_response(),
        Err(crate::scheduler::SubmitError::Closed) => {
            tracing::error!("Review scheduler is not reachable");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Review scheduler is not reachable".to_string(),
            )
                .into_response()
        }
    }
}

#[derive(Serialize)]
struct AcceptedJob {
    job_id: String,
    status: crate::scheduler::JobStatus,
}

async fn get_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    match state.scheduler.get(&job_id).await {
        Some(job) => (StatusCode::OK, Json(job)).into_response(),
        None => (StatusCode::NOT_FOUND, "Job not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_targets_accept_legacy_and_forgejo_requests() {
        assert_eq!(
            repository_target(None, "owner", "repo").unwrap(),
            "owner/repo"
        );
        let target = "https://git.cashu.dev/owner/repo";
        assert_eq!(repository_target(Some(target), "", "").unwrap(), target);
        assert!(repository_target(Some(target), "other", "repo").is_err());
        assert!(repository_target(Some("https://user:secret@host/owner/repo"), "", "").is_err());
        let request: TriggerReviewRequest =
            serde_json::from_value(serde_json::json!({"repository": target, "pr": 42})).unwrap();
        assert_eq!(request.repository.as_deref(), Some(target));
        assert!(request.owner.is_empty());
    }

    #[test]
    fn review_kind_query_accepts_raw_kind_and_builtin_persona() {
        assert_eq!(review_kind_from_query(Some("pr-review")), "pr-review");
        assert_eq!(
            review_kind_from_query(Some("builtin:pr-review")),
            "pr-review"
        );
    }

    #[test]
    fn review_kind_query_defaults_when_missing() {
        assert_eq!(
            review_kind_from_query(None),
            crate::state::DEFAULT_REVIEW_KIND
        );
        assert_eq!(
            review_kind_from_query(Some(" ")),
            crate::state::DEFAULT_REVIEW_KIND
        );
    }

    #[test]
    fn authorization_accepts_bearer_or_token_header_when_configured() {
        let mut headers = HeaderMap::new();
        assert!(request_authorized(&headers, None));
        assert!(!request_authorized(&headers, Some("secret")));

        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(request_authorized(&headers, Some("secret")));

        headers.clear();
        headers.insert("x-fiach-token", "secret".parse().unwrap());
        assert!(request_authorized(&headers, Some("secret")));
    }

    fn failed_review(
        pr: u64,
        class: Option<&str>,
        needs_intervention: bool,
        retry_not_before: Option<i64>,
    ) -> crate::state::ReviewRecord {
        serde_json::from_value(serde_json::json!({
            "repository": "owner/repo",
            "pr_number": pr,
            "commit_hash": "abcdef",
            "model": "m",
            "timestamp": 0,
            "findings_count": 0,
            "status": "failed",
            "severity": "none",
            "pr_classification": "none",
            "failure_class": class,
            "needs_intervention": needs_intervention,
            "retry_not_before": retry_not_before,
        }))
        .unwrap()
    }

    fn idle_processing() -> crate::scheduler::ProcessingHealth {
        crate::scheduler::ProcessingHealth {
            scope: "reviews",
            degraded: false,
            reasons: Vec::new(),
            oldest_queued_age_secs: None,
            window_secs: 900,
            sample_limit: 1000,
            recent_attempts: 0,
            recent_failures: 0,
            recent_failure_rate: None,
            consecutive_failures: 0,
            last_success_at: None,
            last_failure_at: None,
            last_failure: None,
        }
    }

    #[test]
    fn review_health_reports_circuit_and_failure_classes() {
        let circuit = crate::broker::CircuitBreaker::new(
            [crate::broker::Provider::OpenRouter],
            1,
            std::time::Duration::from_secs(60),
        );
        let future = unix_now() + 3600;
        let reviews = vec![
            failed_review(1, Some("provider_unavailable"), false, Some(future + 60)),
            failed_review(2, Some("review"), true, None),
            failed_review(3, None, false, Some(future)),
            failed_review(4, Some("provider"), false, Some(1)),
        ];

        let closed = review_health(idle_processing(), circuit.snapshot(), &reviews);
        assert!(!closed.processing.degraded);
        assert_eq!(closed.reviews_needing_intervention, 1);
        assert_eq!(closed.failed_reviews_by_class["provider_unavailable"], 1);
        assert_eq!(closed.failed_reviews_by_class["review"], 1);
        assert_eq!(closed.failed_reviews_by_class["provider"], 1);
        assert_eq!(closed.failed_reviews_by_class["unknown"], 1);
        assert_eq!(
            closed.next_review_retry_at,
            Some(future),
            "past retries are ignored"
        );

        circuit.record_failure("server_error");
        let open = review_health(idle_processing(), circuit.snapshot(), &reviews);
        assert!(open.processing.degraded);
        assert_eq!(open.processing.reasons, ["provider_circuit_open"]);
        let json = serde_json::to_value(&open).unwrap();
        assert_eq!(json["provider_circuit"]["state"], "open");
        assert_eq!(json["scope"], "reviews", "processing fields stay top-level");
        assert_eq!(json["degraded"], true);
    }

    #[test]
    fn metrics_render_circuit_and_failure_details() {
        let circuit = crate::broker::CircuitBreaker::new(
            [crate::broker::Provider::OpenRouter],
            2,
            std::time::Duration::from_secs(60),
        );
        circuit.record_failure("rate_limited");
        let metrics = render_circuit_metrics(&circuit.snapshot());
        assert!(metrics.contains("fiach_provider_circuit_state{state=\"closed\"} 1"));
        assert!(metrics.contains("fiach_provider_circuit_state{state=\"open\"} 0"));
        assert!(metrics.contains("fiach_provider_circuit_consecutive_failures 1"));
        assert!(metrics.contains("fiach_provider_circuit_failure_threshold 2"));

        let mut processing = idle_processing();
        processing.last_failure = Some(crate::scheduler::ExecutionFailure {
            stage: "execution".into(),
            reason: "provider_unavailable".into(),
            class: crate::state::FailureClass::ProviderUnavailable,
            needs_intervention: false,
            retry_not_before: Some(1234),
        });
        let metrics = render_processing_metrics(&processing);
        assert!(metrics.contains(
            "fiach_review_processing_last_failure{stage=\"execution\",reason=\"provider_unavailable\",class=\"provider_unavailable\",needs_intervention=\"false\"} 1"
        ));
        assert!(metrics.contains(
            "fiach_review_processing_last_failure_retry_not_before_timestamp_seconds 1234"
        ));

        let reviews = vec![failed_review(1, Some("review"), true, None)];
        let metrics = render_metrics(&crate::scheduler::SchedulerStats::default(), &reviews);
        assert!(metrics.contains("fiach_failed_reviews{class=\"review\"} 1"));
        assert!(metrics.contains("fiach_failed_reviews{class=\"unknown\"} 0"));
        assert!(metrics.contains("fiach_reviews_needing_intervention 1"));
        assert!(!metrics.contains("fiach_review_next_retry_timestamp_seconds"));
    }

    #[test]
    fn metrics_render_scheduler_and_cost_gauges() {
        let scheduler = crate::scheduler::SchedulerStats {
            accepting: true,
            queued: 2,
            running: 1,
            ..crate::scheduler::SchedulerStats::default()
        };

        let metrics = render_metrics(&scheduler, &[]);

        assert!(metrics.contains("fiach_scheduler_accepting 1"));
        assert!(metrics.contains("fiach_scheduler_jobs{status=\"queued\"} 2"));
        assert!(metrics.contains("fiach_scheduler_jobs{status=\"running\"} 1"));
        assert!(metrics.contains("fiach_recorded_review_cost_usd 0.000000"));
    }
}
