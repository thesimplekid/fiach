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
    pub server_token: Option<String>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub owner: String,
    pub repo: String,
    pub pr: u64,
    pub persona: Option<String>,
}

#[derive(Deserialize)]
pub struct TriggerReviewRequest {
    pub owner: String,
    pub repo: String,
    pub pr: u64,
    pub persona: Option<String>,
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

    let repo_full = format!("{}/{}", query.owner, query.repo);
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

    let repo_full = format!("{}/{}", query.owner, query.repo);
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

    let repo_full = format!("{}/{}", payload.owner, payload.repo);
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
}
