use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
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
    headers: HeaderMap,
    Query(query): Query<ReviewQuery>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    let repo_full = format!("{}/{}", query.owner, query.repo);
    let review_kind = review_kind_from_query(query.persona.as_deref());
    match get_pr_review(&state.db_path, &repo_full, query.pr, &review_kind) {
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
    let metadata = match get_pr_review(&state.db_path, &repo_full, query.pr, &review_kind) {
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

    let file_name = if metadata.review_kind == crate::state::DEFAULT_REVIEW_KIND {
        format!("{}_PR{}_{}_report.md", safe_repo, query.pr, hash_short)
    } else {
        format!(
            "{}_PR{}_{}_{}_report.md",
            safe_repo, query.pr, hash_short, metadata.review_kind
        )
    };
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
    headers: HeaderMap,
    Json(payload): Json<TriggerReviewRequest>,
) -> impl IntoResponse {
    if !request_authorized(&headers, state.server_token.as_deref()) {
        return unauthorized_response();
    }

    let repo_full = format!("{}/{}", payload.owner, payload.repo);
    let msg = crate::daemon::DaemonMessage::TriggerReview {
        repo: repo_full.clone(),
        pr_number: payload.pr,
        persona: payload.persona.clone(),
    };

    match state.daemon_tx.send(msg).await {
        Ok(_) => {
            tracing::info!("Triggered review for {}/{}", repo_full, payload.pr);
            (
                StatusCode::ACCEPTED,
                format!("Review triggered for {}/{}", repo_full, payload.pr),
            )
                .into_response()
        }
        Err(_) => {
            tracing::error!("Failed to send TriggerReview message to daemon");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Daemon is not reachable".to_string(),
            )
                .into_response()
        }
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
