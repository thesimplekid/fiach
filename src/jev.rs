//! Shared, optional Jev transport through the standalone Jev SDK.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use jev_sdk::{Question, RetryPolicy, SystemOneResponse, TypeSafeClient};
use serde::Serialize;
use serde_json::Value;

// Pin behavior and pricing together: https://docs.typesafe.ai/models (2026-09-18).
pub(crate) const MODEL: &str = "jev-1.13.0";
pub(crate) const INPUT_PRICE_PER_MILLION: f64 = 0.042;
// Resource guard, not an estimate of the model's context window. JSON byte size
// varies with escaping and content. Jev enforces its own token limits (32k for
// state + longest question, 64k overall): https://docs.typesafe.ai/models.
pub(crate) const MAX_REQUEST_BYTES: usize = 1024 * 1024;

// Shared by all clients for an endpoint, independently of issue fingerprints.
static PROVIDERS: LazyLock<Mutex<HashMap<String, Arc<Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct Provider {
    cooldown: Mutex<Cooldown>,
    request: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Cooldown {
    failures: u32,
    until: Option<Instant>,
}

impl Cooldown {
    fn remaining(&self, now: Instant) -> Duration {
        self.until
            .map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
    }

    fn overload(&mut self, now: Instant) -> Duration {
        let policy = RetryPolicy {
            initial_backoff: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
            ..RetryPolicy::default()
        };
        let delay = policy.backoff_for(self.failures);
        self.failures = self.failures.saturating_add(1);
        self.until = Some(now + delay);
        delay
    }
}

fn provider(base_url: &str) -> Arc<Provider> {
    PROVIDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(base_url.trim_end_matches('/').to_owned())
        .or_default()
        .clone()
}

pub(crate) fn cooldown_wait(base_url: &str) -> Duration {
    provider(base_url)
        .cooldown
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remaining(Instant::now())
}

pub(crate) struct Client {
    inner: TypeSafeClient,
    provider: Arc<Provider>,
}

#[derive(Default)]
pub(crate) struct UsageStats {
    pub peak_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Serialize)]
pub(crate) struct Request {
    pub state: Value,
    pub questions: HashMap<String, Question>,
}

impl Request {
    pub fn encoded_len(&self) -> Result<usize> {
        Ok(serde_json::to_vec(&serde_json::json!({
            "model": MODEL, "state": self.state, "questions": self.questions,
        }))?
        .len())
    }
}

/// Only explicit provider size/context failures may become unresolved evidence.
/// Preserve authentication, overload and unrelated validation failures as errors.
pub(crate) fn is_size_rejection(error: &anyhow::Error) -> bool {
    let Some(jev_sdk::Error::Api(error)) = error.downcast_ref::<jev_sdk::Error>() else {
        return false;
    };
    match error.status.as_u16() {
        413 => true,
        400 | 422 => {
            let message = error.message.to_ascii_lowercase();
            let code = error
                .body
                .as_ref()
                .and_then(|body| body.pointer("/detail/error_type"))
                .and_then(Value::as_str);
            code == Some("context_length_exceeded")
                || ((message.contains("context") || message.contains("token"))
                    && (message.contains("exceed") || message.contains("too long")))
        }
        _ => false,
    }
}

pub(crate) fn client_from_env() -> Result<Option<Client>> {
    let Some(key) = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        return Ok(None);
    };
    // Explicit endpoint/model preserve the same behavior in host and sandbox.
    client(&key, "https://api.typesafe.ai").map(Some)
}

pub(crate) fn client(key: &str, base_url: &str) -> Result<Client> {
    let inner = TypeSafeClient::builder()
        .api_key(key)
        .base_url(base_url)
        .model(MODEL)
        .timeout(Duration::from_secs(10))
        // A timed-out request may still be billable. Let each caller fall back.
        .retry(RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        })
        .build()?;
    Ok(Client {
        inner,
        provider: provider(base_url),
    })
}

pub(crate) async fn evaluate(
    client: &Client,
    request: Request,
    budget: Option<f64>,
    usage: &mut UsageStats,
) -> Result<SystemOneResponse> {
    let bytes = request.encoded_len()?;
    if bytes > MAX_REQUEST_BYTES {
        bail!("Jev request is {bytes} bytes; limit is {MAX_REQUEST_BYTES} bytes");
    }
    let estimated_cost = (bytes + 1024) as f64 * INPUT_PRICE_PER_MILLION / 1_000_000.0;
    if budget.is_some_and(|max| usage.cost_usd + estimated_cost > max) {
        bail!("Insufficient budget for Jev request");
    }
    // Serialize requests per provider so concurrent callers cannot bypass a new cooldown.
    let _request = client.provider.request.lock().await;
    let remaining = client
        .provider
        .cooldown
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remaining(Instant::now());
    if !remaining.is_zero() {
        bail!(
            "Jev provider cooling down for {:.0} seconds",
            remaining.as_secs_f64().ceil()
        );
    }
    let response = match client
        .inner
        .system_one(request.state, request.questions)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if matches!(
                error.status().map(|status| status.as_u16()),
                Some(429 | 503 | 529)
            ) {
                // SDK 0.1.0 discards response headers, so Retry-After is unavailable here.
                let delay = client
                    .provider
                    .cooldown
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .overload(Instant::now());
                tracing::warn!(status = ?error.status(), cooldown_secs = delay.as_secs(),
                    "Jev overloaded; pausing provider requests");
            }
            return Err(error.into());
        }
    };
    usage.peak_input_tokens = usage.peak_input_tokens.max(response.usage.input_tokens);
    usage.output_tokens += response.usage.output_tokens;
    usage.total_tokens += response.usage.input_tokens + response.usage.output_tokens;
    usage.cost_usd += response.usage.input_tokens as f64 * INPUT_PRICE_PER_MILLION / 1_000_000.0;
    if response.model != MODEL {
        bail!("Unexpected Jev response model");
    }
    *client
        .provider
        .cooldown
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Cooldown::default();
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_size_rejections_do_not_hide_other_errors() {
        for (status, message, code, expected) in [
            (413, "Request body too large", "", true),
            (422, "State plus question exceeds token limit", "", true),
            (400, "Maximum context length exceeded", "", true),
            (422, "Input is too long", "context_length_exceeded", true),
            (422, "Missing questions", "validation_error", false),
            (400, "Invalid token parameter", "", false),
            (401, "Token limit exceeded", "", false),
            (429, "Token rate limit exceeded", "", false),
            (529, "Context length exceeded", "", false),
        ] {
            let error = jev_sdk::Error::Api(jev_sdk::ApiError {
                status: StatusCode::from_u16(status).unwrap(),
                message: message.into(),
                body: Some(json!({"detail": {"error_type": code}})),
            });
            let error = anyhow::Error::new(error).context("Comparing candidate");
            assert_eq!(is_size_rejection(&error), expected, "{status}: {message}");
        }
        assert!(!is_size_rejection(&anyhow::anyhow!(
            "context length exceeded"
        )));
    }

    #[test]
    fn cooldown_grows_across_expirations_and_caps_at_one_hour() {
        let mut cooldown = Cooldown::default();
        let mut now = Instant::now();
        assert!(cooldown.remaining(now).is_zero());
        for minimum in [60, 120, 240, 480, 960, 1920, 3600, 3600] {
            let delay = cooldown.overload(now);
            assert!(delay >= Duration::from_secs(minimum));
            assert!(delay <= Duration::from_secs(minimum).mul_f64(1.25));
            assert!(delay <= Duration::from_secs(3600));
            assert_eq!(cooldown.remaining(now), delay);
            now += delay;
            assert!(cooldown.remaining(now).is_zero());
        }
    }

    #[tokio::test]
    async fn overload_blocks_other_clients_and_recovers_after_expiry() {
        let status = Arc::new(AtomicU16::new(529));
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_status = status.clone();
        let handler_calls = calls.clone();
        let app = Router::new().route("/v1/systemone", post(move || {
            let status = handler_status.clone();
            let calls = handler_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                (StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap(),
                 Json(json!({"model": MODEL, "answers": {}, "usage": {"input_tokens": 10, "output_tokens": 0}})))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let first = client("test", &endpoint).unwrap();
        let second = client("test", &format!("{endpoint}/")).unwrap();
        let request = || Request {
            state: json!({"issue": "changed evidence"}),
            questions: HashMap::from([(
                "q".into(),
                Question::from(jev_sdk::Noul::new("Is this a bug?")),
            )]),
        };
        let mut usage = UsageStats::default();
        for code in [529, 429, 503] {
            status.store(code, Ordering::SeqCst);
            let before = calls.load(Ordering::SeqCst);
            let error = evaluate(&first, request(), None, &mut usage)
                .await
                .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<jev_sdk::Error>()
                    .unwrap()
                    .status()
                    .unwrap()
                    .as_u16(),
                code
            );
            assert!(!cooldown_wait(&endpoint).is_zero());
            let error = evaluate(&second, request(), None, &mut usage)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("cooling down"));
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            first.provider.cooldown.lock().unwrap().until = Some(Instant::now());
        }
        assert_eq!(first.provider.cooldown.lock().unwrap().failures, 3);
        status.store(200, Ordering::SeqCst);
        evaluate(&second, request(), None, &mut usage)
            .await
            .unwrap();
        assert!(cooldown_wait(&endpoint).is_zero());
        assert_eq!(first.provider.cooldown.lock().unwrap().failures, 0);
        assert_eq!(usage.total_tokens, 10);
        status.store(401, Ordering::SeqCst);
        assert!(evaluate(&first, request(), None, &mut usage).await.is_err());
        assert!(cooldown_wait(&endpoint).is_zero());
        task.abort();
    }
}
