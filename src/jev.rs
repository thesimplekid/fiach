//! Shared, optional Jev decisions through the GDK TypeSafe provider.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use goose_providers::{
    api_client::{ApiClient, AuthMethod},
    decision::{DecisionProvider, DecisionQuestion as Question, DecisionRequest, DecisionResponse},
    errors::ProviderError,
    typesafe::TypeSafeProvider,
};
use serde::Serialize;
use serde_json::Value;

// Pin behavior and pricing together: https://docs.typesafe.ai/models (2026-09-18).
pub(crate) const MODEL: &str = "jev-1.13.0";
pub(crate) const INPUT_PRICE_PER_MILLION: f64 = 0.042;
// Resource guard, not an estimate of the model's context window. JSON byte size
// varies with escaping and content. Jev enforces its own token limits (32k for
// state + longest question, 64k overall): https://docs.typesafe.ai/models.
pub(crate) const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const STATE_QUESTION_TOKEN_LIMIT: f64 = 32_000.0;
const TOTAL_TOKEN_LIMIT: f64 = 64_000.0;
// Requests are refused before sending only when they cannot fit even at this
// generous ratio. Borderline evidence still reaches Jev intact; the provider
// decides, and callers' size fallbacks handle a rejection.
const OPTIMISTIC_BYTES_PER_TOKEN: f64 = 5.0;
// Batching uses a calibrated, conservative estimate instead: a smaller batch
// costs a request, never evidence. Headroom covers estimation error.
const TOKEN_LIMIT_MARGIN: f64 = 0.9;
// Code and JSON tokenize densely; start conservative until usage calibrates.
const DEFAULT_BYTES_PER_TOKEN: f64 = 2.5;
const MIN_BYTES_PER_TOKEN: f64 = 1.5;
const MAX_BYTES_PER_TOKEN: f64 = 3.5;
const CALIBRATION_RISE_WEIGHT: f64 = 0.1;

// Shared by all clients for an endpoint, independently of issue fingerprints.
static PROVIDERS: LazyLock<Mutex<HashMap<String, Arc<Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct Provider {
    cooldown: Mutex<Cooldown>,
    request: tokio::sync::Mutex<()>,
    calibration: Mutex<Calibration>,
}

/// Observed serialized bytes per Jev input token for one endpoint.
#[derive(Default)]
struct Calibration {
    bytes_per_token: Option<f64>,
}

impl Calibration {
    fn bytes_per_token(&self) -> f64 {
        self.bytes_per_token.unwrap_or(DEFAULT_BYTES_PER_TOKEN)
    }

    /// Falls immediately and rises slowly, so one prose-heavy request cannot
    /// loosen the preflight for code-heavy ones.
    fn observe(&mut self, bytes_per_token: f64) {
        if !bytes_per_token.is_finite() {
            return;
        }
        let observed = bytes_per_token.clamp(MIN_BYTES_PER_TOKEN, MAX_BYTES_PER_TOKEN);
        let current = self.bytes_per_token();
        self.bytes_per_token = Some(if observed <= current {
            observed
        } else {
            current + CALIBRATION_RISE_WEIGHT * (observed - current)
        });
    }

    fn observe_usage(&mut self, bytes: usize, input_tokens: u64) {
        if input_tokens > 0 {
            self.observe(bytes as f64 / input_tokens as f64);
        }
    }

    /// A size rejection proves at least one limit was exceeded, bounding the
    /// true ratio from above by the larger per-limit ratio.
    fn observe_rejection(&mut self, sizes: &RequestSizes, bytes: usize) {
        let bound = (sizes.state_plus_longest_question() as f64 / STATE_QUESTION_TOKEN_LIMIT)
            .max(bytes as f64 / TOTAL_TOKEN_LIMIT);
        if bound < self.bytes_per_token() {
            self.observe(bound * TOKEN_LIMIT_MARGIN);
        }
    }

    fn exceeded_limit(&self, sizes: &RequestSizes, bytes: usize) -> Option<&'static str> {
        exceeded_limit(sizes, bytes, self.bytes_per_token(), TOKEN_LIMIT_MARGIN)
    }
}

/// Returns which Jev token limit the request is estimated to exceed.
fn exceeded_limit(
    sizes: &RequestSizes,
    bytes: usize,
    bytes_per_token: f64,
    margin: f64,
) -> Option<&'static str> {
    let tokens = |bytes: usize| bytes as f64 / bytes_per_token;
    if tokens(sizes.state_plus_longest_question()) > STATE_QUESTION_TOKEN_LIMIT * margin {
        Some("state_plus_longest_question")
    } else if tokens(bytes) > TOTAL_TOKEN_LIMIT * margin {
        Some("total")
    } else {
        None
    }
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
        let delay = Duration::from_secs(60 * 2_u64.pow(self.failures.min(6)))
            .min(Duration::from_secs(3600));
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
    inner: TypeSafeProvider,
    provider: Arc<Provider>,
}

impl Client {
    /// Whether the request is estimated, conservatively, to fit both Jev token
    /// limits. Use for packing batches, not for refusing single requests.
    pub(crate) fn fits(&self, request: &Request) -> Result<bool> {
        Ok(self
            .calibration()
            .exceeded_limit(&request.sizes()?, request.encoded_len()?)
            .is_none())
    }

    fn calibration(&self) -> std::sync::MutexGuard<'_, Calibration> {
        self.provider
            .calibration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
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

pub(crate) fn choice_question(
    instructions: impl Into<String>,
    criteria: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> Question {
    Question::Choice {
        instructions: instructions.into(),
        criteria: criteria
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect(),
    }
}

impl Request {
    pub fn encoded_len(&self) -> Result<usize> {
        Ok(serde_json::to_vec(&serde_json::json!({
            "model": MODEL, "state": self.state, "questions": self.questions,
        }))?
        .len())
    }

    /// Serialized sizes of the parts each Jev token limit applies to.
    fn sizes(&self) -> Result<RequestSizes> {
        let state_bytes = serde_json::to_vec(&self.state)?.len();
        let mut longest_question = None;
        let mut longest_question_bytes = 0;
        let mut questions_bytes = 0;
        for (key, question) in &self.questions {
            let bytes = serde_json::to_vec(question)?.len();
            questions_bytes += bytes;
            if bytes > longest_question_bytes {
                longest_question_bytes = bytes;
                longest_question = Some(key.clone());
            }
        }
        Ok(RequestSizes {
            state_bytes,
            longest_question,
            longest_question_bytes,
            questions_bytes,
        })
    }
}

/// Jev limits state plus the longest question to 32k tokens and the whole
/// request to 64k. Bytes are not token counts, but comparing these parts shows
/// which limit a rejected request is closest to.
struct RequestSizes {
    state_bytes: usize,
    longest_question: Option<String>,
    longest_question_bytes: usize,
    questions_bytes: usize,
}

impl RequestSizes {
    fn state_plus_longest_question(&self) -> usize {
        self.state_bytes + self.longest_question_bytes
    }
}

/// Extracts the provider's `error_type` code without logging free-form error
/// text, which may echo request content.
fn provider_error_type(message: &str) -> Option<&str> {
    let start = message.find("\"error_type\"")? + "\"error_type\"".len();
    let rest = message[start..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let code = &rest[..rest.find('"')?];
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(code)
}

/// Only explicit provider size/context failures may become unresolved evidence.
/// Preserve authentication, overload and unrelated validation failures as errors.
pub(crate) fn is_size_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ProviderError>()
        .is_some_and(is_provider_size_rejection)
}

fn is_provider_size_rejection(error: &ProviderError) -> bool {
    match error {
        ProviderError::ContextLengthExceeded(_) => true,
        // GDK maps some TypeSafe 400/422 bodies to RequestFailed and keeps
        // the HTTP status in these prefixes. Do not classify unrelated errors.
        ProviderError::RequestFailed(message)
            if message.starts_with("Bad request (400):")
                || message.starts_with("Request failed with status 422 ") =>
        {
            let message = message.to_ascii_lowercase();
            (message.contains("context") || message.contains("token"))
                && (message.contains("exceed") || message.contains("too long"))
        }
        _ => false,
    }
}

fn overload_retry_delay(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimitExceeded { retry_delay, .. } => {
            Some(retry_delay.unwrap_or(Duration::ZERO))
        }
        ProviderError::ServerError(message)
            if message.starts_with("Server error (503 ")
                || message.starts_with("Server error (529 ") =>
        {
            Some(Duration::ZERO)
        }
        _ => None,
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
    // The decision provider sends once; a timed-out request may be billable.
    let inner = TypeSafeProvider::new(ApiClient::with_timeout_and_tls(
        base_url.trim_end_matches('/').to_owned(),
        AuthMethod::BearerToken(key.to_owned()),
        Duration::from_secs(10),
        None,
    )?);
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
) -> Result<DecisionResponse> {
    let bytes = request.encoded_len()?;
    if bytes > MAX_REQUEST_BYTES {
        bail!("Jev request is {bytes} bytes; limit is {MAX_REQUEST_BYTES} bytes");
    }
    let question_count = request.questions.len();
    let sizes = request.sizes()?;
    let bytes_per_token = client.calibration().bytes_per_token();
    if let Some(limit) = exceeded_limit(&sizes, bytes, OPTIMISTIC_BYTES_PER_TOKEN, 1.0) {
        log_request_sizes(
            &sizes,
            bytes,
            question_count,
            limit,
            "",
            OPTIMISTIC_BYTES_PER_TOKEN,
            "Decision request cannot fit Jev token limits; not sent",
        );
        // Same error kind as a provider rejection, so callers' size fallbacks apply.
        return Err(ProviderError::ContextLengthExceeded(format!(
            "Jev request cannot fit the {limit} token limit"
        ))
        .into());
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
        .create_decision(&DecisionRequest {
            model: MODEL.to_owned(),
            state: request.state,
            questions: request.questions,
        })
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let message = error.to_string();
            log_request_sizes(
                &sizes,
                bytes,
                question_count,
                crate::request_diagnostics::failure_kind(&message),
                provider_error_type(&message).unwrap_or(""),
                bytes_per_token,
                "Decision request failed",
            );
            if is_provider_size_rejection(&error) {
                client.calibration().observe_rejection(&sizes, bytes);
            }
            if let Some(retry_after) = overload_retry_delay(&error) {
                let mut cooldown = client
                    .provider
                    .cooldown
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let delay = cooldown.overload(now).max(retry_after);
                cooldown.until = now.checked_add(delay).or(cooldown.until);
                tracing::warn!(
                    cooldown_secs = delay.as_secs(),
                    "Jev overloaded; pausing provider requests"
                );
            }
            return Err(error.into());
        }
    };
    let input_tokens = response
        .usage
        .input_tokens
        .context("Jev response omitted input token usage")?;
    let output_tokens = response
        .usage
        .output_tokens
        .context("Jev response omitted output token usage")?;
    client.calibration().observe_usage(bytes, input_tokens);
    usage.peak_input_tokens = usage.peak_input_tokens.max(input_tokens);
    usage.output_tokens += output_tokens;
    usage.total_tokens += input_tokens + output_tokens;
    usage.cost_usd += input_tokens as f64 * INPUT_PRICE_PER_MILLION / 1_000_000.0;
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

fn log_request_sizes(
    sizes: &RequestSizes,
    bytes: usize,
    question_count: usize,
    failure_kind: &str,
    provider_error_type: &str,
    bytes_per_token: f64,
    message: &str,
) {
    tracing::warn!(
        provider = "typesafe",
        model = MODEL,
        requested_output_tokens = "not_sent",
        input_bytes = bytes,
        input_size_kind = "serialized_payload_bytes",
        state_bytes = sizes.state_bytes,
        longest_question = sizes.longest_question.as_deref().unwrap_or(""),
        longest_question_bytes = sizes.longest_question_bytes,
        state_plus_longest_question_bytes = sizes.state_plus_longest_question(),
        questions_bytes = sizes.questions_bytes,
        question_count,
        bytes_per_token,
        provider_error_type,
        failure_kind,
        "{message}"
    );
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
            let error = goose_providers::http_status::map_http_error_to_provider_error(
                StatusCode::from_u16(status).unwrap(),
                Some(json!({"detail": {"error_type": code, "message": message}})),
                "http://localhost/v1/systemone",
            );
            let error = anyhow::Error::new(error).context("Comparing candidate");
            assert_eq!(is_size_rejection(&error), expected, "{status}: {message}");
        }
        assert!(!is_size_rejection(&anyhow::anyhow!(
            "context length exceeded"
        )));
    }

    #[tokio::test]
    async fn gdk_transport_preserves_context_and_validation_failures() {
        for (status, body, oversized) in [
            (400, json!({"error_type": "max_tokens_exceeded"}), true),
            (
                422,
                json!({"detail": {"error_type": "context_length_exceeded"}}),
                true,
            ),
            (413, json!({"message": "Request too large"}), true),
            (422, json!({"detail": "Missing questions"}), false),
            (401, json!({"error_type": "max_tokens_exceeded"}), false),
        ] {
            let app = Router::new().route(
                "/v1/systemone",
                post(move || {
                    let body = body.clone();
                    async move { (StatusCode::from_u16(status).unwrap(), Json(body)) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = client("test", &endpoint).unwrap();
            let error = evaluate(
                &client,
                Request {
                    state: json!({}),
                    questions: HashMap::new(),
                },
                None,
                &mut UsageStats::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(is_size_rejection(&error), oversized, "{status}: {error}");
            assert!(cooldown_wait(&endpoint).is_zero());
            task.abort();
        }
    }

    #[test]
    fn request_sizes_split_state_and_longest_question() {
        let request = Request {
            state: json!({"issue": "x".repeat(100)}),
            questions: HashMap::from([
                ("short".into(), choice_question("a", [("yes", "y")])),
                (
                    "long".into(),
                    choice_question("b".repeat(50), [("yes", "y")]),
                ),
            ]),
        };
        let sizes = request.sizes().unwrap();
        assert_eq!(
            sizes.state_bytes,
            serde_json::to_vec(&request.state).unwrap().len()
        );
        assert_eq!(sizes.longest_question.as_deref(), Some("long"));
        assert!(sizes.longest_question_bytes > 50);
        assert!(sizes.questions_bytes > sizes.longest_question_bytes);
        assert!(sizes.state_bytes + sizes.questions_bytes <= request.encoded_len().unwrap());
    }

    fn sizes(state_bytes: usize, longest_question_bytes: usize) -> RequestSizes {
        RequestSizes {
            state_bytes,
            longest_question: Some("q".into()),
            longest_question_bytes,
            questions_bytes: longest_question_bytes,
        }
    }

    #[test]
    fn preflight_names_the_limit_a_request_would_exceed() {
        let calibration = Calibration::default();
        // 2.5 bytes/token with a 90% margin: 72,000 bytes for state + question.
        assert_eq!(
            calibration.exceeded_limit(&sizes(60_000, 10_000), 70_000),
            None
        );
        assert_eq!(
            calibration.exceeded_limit(&sizes(70_000, 3_000), 73_000),
            Some("state_plus_longest_question")
        );
        // Many small questions: under 32k per question, over 64k overall.
        assert_eq!(
            calibration.exceeded_limit(&sizes(20_000, 1_000), 150_000),
            Some("total")
        );
    }

    #[test]
    fn calibration_falls_fast_rises_slowly_and_stays_bounded() {
        let mut calibration = Calibration::default();
        calibration.observe_usage(2_000, 1_000);
        assert_eq!(calibration.bytes_per_token(), 2.0);
        calibration.observe_usage(4_000, 1_000);
        assert!((calibration.bytes_per_token() - 2.15).abs() < 1e-9);
        calibration.observe_usage(100, 1_000);
        assert_eq!(calibration.bytes_per_token(), MIN_BYTES_PER_TOKEN);
        calibration.observe_usage(100, 0);
        assert_eq!(calibration.bytes_per_token(), MIN_BYTES_PER_TOKEN);

        let mut calibration = Calibration::default();
        for _ in 0..200 {
            calibration.observe_usage(10_000, 1_000);
        }
        assert!(calibration.bytes_per_token() <= MAX_BYTES_PER_TOKEN);
    }

    #[test]
    fn size_rejection_tightens_calibration_below_the_rejected_ratio() {
        let mut calibration = Calibration::default();
        // Rejected at 64,000 bytes of state: true ratio is below 2.0 bytes/token.
        let rejected = sizes(63_000, 1_000);
        calibration.observe_rejection(&rejected, 64_000);
        assert!(calibration.bytes_per_token() < 2.0);
        assert!(calibration.exceeded_limit(&rejected, 64_000).is_some());
    }

    #[tokio::test]
    async fn oversized_requests_fail_locally_as_size_rejections() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = Router::new().route(
            "/v1/systemone",
            post(move || {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = client("test", &endpoint).unwrap();
        let request = Request {
            state: json!({"diff": "x".repeat(170_000)}),
            questions: HashMap::from([("q".into(), choice_question("a", [("yes", "y")]))]),
        };
        assert!(request.encoded_len().unwrap() < MAX_REQUEST_BYTES);
        assert!(!client.fits(&request).unwrap());

        let error = evaluate(&client, request, None, &mut UsageStats::default())
            .await
            .unwrap_err();
        assert!(is_size_rejection(&error), "{error}");
        assert!(crate::request_diagnostics::permanent_rejection(&error));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }

    #[test]
    fn provider_error_type_is_a_bare_code_only() {
        assert_eq!(
            provider_error_type(r#"Bad request (400): {"error_type":"max_tokens_exceeded"}"#),
            Some("max_tokens_exceeded")
        );
        assert_eq!(
            provider_error_type(r#"{"detail": {"error_type": "context_length_exceeded"}}"#),
            Some("context_length_exceeded")
        );
        for message in [
            "Bad request (400): max tokens exceeded",
            r#"{"error_type":"private prompt text"}"#,
            r#"{"error_type":""}"#,
        ] {
            assert_eq!(provider_error_type(message), None, "{message}");
        }
    }

    #[tokio::test]
    async fn missing_usage_cannot_bypass_budget_accounting() {
        for usage in [
            json!({}),
            json!({"input_tokens": 10}),
            json!({"output_tokens": 0}),
        ] {
            let app = Router::new().route(
                "/v1/systemone",
                post(move || {
                    let usage = usage.clone();
                    async move { Json(json!({"model": MODEL, "answers": {}, "usage": usage})) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = client("test", &endpoint).unwrap();
            let error = evaluate(
                &client,
                Request {
                    state: json!({}),
                    questions: HashMap::new(),
                },
                Some(1.0),
                &mut UsageStats::default(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("token usage"), "{error}");
            task.abort();
        }
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
                 [("retry-after", "300")],
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
                Question::Noul {
                    instructions: "Is this a bug?".into(),
                    criteria: None,
                },
            )]),
        };
        let mut usage = UsageStats::default();
        for code in [529, 429, 503] {
            status.store(code, Ordering::SeqCst);
            let before = calls.load(Ordering::SeqCst);
            let error = evaluate(&first, request(), None, &mut usage)
                .await
                .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<ProviderError>().unwrap(),
                ProviderError::RateLimitExceeded { .. } | ProviderError::ServerError(_)
            ));
            assert!(!cooldown_wait(&endpoint).is_zero());
            if code == 429 {
                assert!(cooldown_wait(&endpoint) > Duration::from_secs(299));
            }
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
