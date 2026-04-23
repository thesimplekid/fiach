use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::state::{get_pr_review, list_reviews};

#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
    pub out_dir: PathBuf,
    pub daemon_tx: mpsc::Sender<crate::daemon::DaemonMessage>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub owner: String,
    pub repo: String,
    pub pr: u64,
}

#[derive(Deserialize)]
pub struct TriggerReviewRequest {
    pub owner: String,
    pub repo: String,
    pub pr: u64,
}

pub async fn start_server(port: u16, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/reviews", get(get_reviews))
        .route("/review", get(get_review).post(trigger_review))
        .route("/review/content", get(get_review_content))
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

async fn get_reviews(State(state): State<AppState>) -> impl IntoResponse {
    match list_reviews(&state.db_path) {
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
    Query(query): Query<ReviewQuery>,
) -> impl IntoResponse {
    let repo_full = format!("{}/{}", query.owner, query.repo);
    match get_pr_review(&state.db_path, &repo_full, query.pr) {
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
    Query(query): Query<ReviewQuery>,
) -> impl IntoResponse {
    let repo_full = format!("{}/{}", query.owner, query.repo);
    let metadata = match get_pr_review(&state.db_path, &repo_full, query.pr) {
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

    let safe_repo = repo_full.replace('/', "_");
    let hash_short = if metadata.commit_hash.len() > 7 {
        &metadata.commit_hash[..7]
    } else {
        &metadata.commit_hash
    };

    let file_name = format!("{}_PR{}_{}_report.md", safe_repo, query.pr, hash_short);
    let file_path = state.out_dir.join(file_name);

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
    Json(payload): Json<TriggerReviewRequest>,
) -> impl IntoResponse {
    let repo_full = format!("{}/{}", payload.owner, payload.repo);
    let msg = crate::daemon::DaemonMessage::TriggerReview {
        repo: repo_full.clone(),
        pr_number: payload.pr,
    };

    match state.daemon_tx.send(msg).await {
        Ok(_) => {
            tracing::info!("Triggered review for {}/{}", repo_full, payload.pr);
            (
                StatusCode::ACCEPTED,
                format!("Review triggered for {}/{}", repo_full, payload.pr),
            )
        }
        Err(_) => {
            tracing::error!("Failed to send TriggerReview message to daemon");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Daemon is not reachable".to_string(),
            )
        }
    }
}
