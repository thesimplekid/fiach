//! Host-side model broker.
//!
//! Each sandboxed job gets its own broker with one listener per provider
//! route. The sandbox points its provider host variable (for example
//! `OPENROUTER_HOST`) at the listener and authenticates with a per-job key
//! that is not a real credential. The broker checks that key, the request
//! path, and the requested model, then replaces the credential with the real
//! one from the host environment and forwards the request to a fixed upstream.
//! Responses stream back unchanged while the broker reads token usage from
//! them to enforce the job's cost budget.
//!
//! The sandbox key doubles as a canary: it only exists inside the sandbox, so
//! seeing it (or any real credential) in a request body or URL means the job
//! tried to send a secret to the model. The broker then refuses that request
//! and every later one, and the host treats the job as quarantined.
//!
//! Inside the sandbox, `fiach broker-bridge` exposes the broker's Unix
//! sockets on loopback TCP ports and starts the job with each route's host
//! and key variables set.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Requests carry prompts, tool results, and diffs; 8 MiB comfortably fits a
/// long agent conversation while bounding host memory per request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Non-streaming responses are buffered up to this size to read usage.
const MAX_USAGE_SCAN_BYTES: usize = 8 * 1024 * 1024;
/// Goose's pinned Anthropic API version.
const ANTHROPIC_VERSION: &str = "2023-06-01";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Request headers forwarded upstream. Everything else, including any
/// credential the sandbox sends, is dropped.
const FORWARDED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "anthropic-beta",
    "content-type",
    "http-referer",
    "user-agent",
    "x-title",
];
/// Response headers returned to the sandbox. Content length and encoding are
/// recomputed by the broker's own HTTP stack.
const FORWARDED_RESPONSE_HEADERS: &[&str] =
    &["content-type", "request-id", "retry-after", "x-request-id"];
const FORWARDED_RESPONSE_HEADER_PREFIXES: &[&str] = &["anthropic-ratelimit-", "x-ratelimit-"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Anthropic,
    Google,
    TypeSafe,
}

/// Where a request names its model.
#[derive(Debug, PartialEq, Eq)]
enum ModelLocation {
    /// Listing endpoints do not name a model.
    None,
    /// The JSON body's top-level `model` field.
    Body,
    /// A model named in the URL path.
    Path(String),
}

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::OpenRouter,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::Google,
        Provider::TypeSafe,
    ];

    /// Parses a Goose provider name.
    pub fn from_name(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.name() == name)
            .with_context(|| format!("Provider {name} is not supported by the model broker"))
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Google => "google",
            Provider::TypeSafe => "typesafe",
        }
    }

    /// Environment variable holding the provider credential.
    pub fn key_env(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Google => "GOOGLE_API_KEY",
            Provider::TypeSafe => "TYPESAFE_API_KEY",
        }
    }

    /// Environment variable naming the provider base URL. On the host it
    /// selects the upstream; in the sandbox it points at the broker.
    pub fn host_env(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_HOST",
            Provider::OpenAi => "OPENAI_HOST",
            Provider::Anthropic => "ANTHROPIC_HOST",
            Provider::Google => "GOOGLE_HOST",
            Provider::TypeSafe => "TYPESAFE_HOST",
        }
    }

    pub fn default_upstream(self) -> &'static str {
        match self {
            Provider::OpenRouter => "https://openrouter.ai",
            Provider::OpenAi => "https://api.openai.com",
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Google => "https://generativelanguage.googleapis.com",
            Provider::TypeSafe => "https://api.typesafe.ai",
        }
    }

    /// The exact endpoints Goose and Jev use. Anything else is refused.
    fn endpoint(self, method: &Method, path: &str) -> Option<ModelLocation> {
        let post = method == Method::POST;
        let get = method == Method::GET;
        match (self, path) {
            (Provider::OpenRouter, "/api/v1/chat/completions") if post => Some(ModelLocation::Body),
            (Provider::OpenRouter, "/api/v1/models") if get => Some(ModelLocation::None),
            (Provider::OpenAi, "/v1/chat/completions" | "/v1/responses") if post => {
                Some(ModelLocation::Body)
            }
            (Provider::OpenAi, "/v1/models") if get => Some(ModelLocation::None),
            (Provider::Anthropic, "/v1/messages") if post => Some(ModelLocation::Body),
            (Provider::Anthropic, "/v1/models") if get => Some(ModelLocation::None),
            (Provider::Google, "/v1beta/models") if get => Some(ModelLocation::None),
            (Provider::Google, _) if post => {
                let call = path.strip_prefix("/v1beta/models/")?;
                let (model, action) = call.rsplit_once(':')?;
                (matches!(action, "generateContent" | "streamGenerateContent")
                    && !model.is_empty()
                    && !model.contains('/'))
                .then(|| ModelLocation::Path(model.to_owned()))
            }
            (Provider::TypeSafe, "/v1/systemone") if post => Some(ModelLocation::Body),
            _ => None,
        }
    }

    /// Query parameters forwarded upstream. Google streams with `alt=sse`;
    /// no route accepts a credential in the query string.
    fn allows_query_param(self, name: &str) -> bool {
        self == Provider::Google && name == "alt"
    }

    /// The credential presented by the sandbox, if any.
    fn presented_key(self, headers: &HeaderMap) -> Option<&str> {
        match self {
            Provider::OpenRouter | Provider::OpenAi | Provider::TypeSafe => headers
                .get(header::AUTHORIZATION)?
                .to_str()
                .ok()?
                .strip_prefix("Bearer "),
            Provider::Anthropic => headers.get("x-api-key")?.to_str().ok(),
            Provider::Google => headers.get("x-goog-api-key")?.to_str().ok(),
        }
    }

    fn auth_headers(self, key: &str) -> Result<Vec<(HeaderName, HeaderValue)>> {
        let value = |text: String| {
            let mut value = HeaderValue::from_str(&text).context("Invalid provider credential")?;
            value.set_sensitive(true);
            Ok::<_, anyhow::Error>(value)
        };
        Ok(match self {
            Provider::OpenRouter | Provider::OpenAi | Provider::TypeSafe => {
                vec![(header::AUTHORIZATION, value(format!("Bearer {key}"))?)]
            }
            Provider::Anthropic => vec![
                (HeaderName::from_static("x-api-key"), value(key.to_owned())?),
                (
                    HeaderName::from_static("anthropic-version"),
                    HeaderValue::from_static(ANTHROPIC_VERSION),
                ),
            ],
            Provider::Google => vec![(
                HeaderName::from_static("x-goog-api-key"),
                value(key.to_owned())?,
            )],
        })
    }

    /// OpenRouter can fall back to other models named in the body, which
    /// would bypass model pinning.
    fn check_body(self, body: &Value) -> Result<(), &'static str> {
        if self == Provider::OpenRouter && body.get("models").is_some() {
            return Err("model fallback lists are not allowed");
        }
        Ok(())
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Price per million tokens, used to charge usage against the job budget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    pub input_per_m: f64,
    pub output_per_m: f64,
}

/// Credentials for one provider, kept out of `Debug` output.
#[derive(Clone)]
pub struct Credential(String);

impl Credential {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(<redacted>)")
    }
}

/// One provider the job may use.
#[derive(Clone, Debug)]
pub struct RouteConfig {
    pub provider: Provider,
    pub upstream: String,
    pub credential: Credential,
    /// The per-job key the sandbox must present.
    pub sandbox_key: Credential,
    /// Exact model names the job may request, with their prices when known.
    pub models: BTreeMap<String, Option<Pricing>>,
}

impl RouteConfig {
    /// Builds a route from the host environment: `<PROVIDER>_API_KEY` for the
    /// credential and `<PROVIDER>_HOST` for an optional upstream override.
    pub fn from_env(
        provider: Provider,
        models: impl IntoIterator<Item = (String, Option<Pricing>)>,
    ) -> Result<Self> {
        let credential = std::env::var(provider.key_env())
            .ok()
            .filter(|key| !key.trim().is_empty())
            .with_context(|| format!("{} is not set on the host", provider.key_env()))?;
        let upstream = std::env::var(provider.host_env())
            .ok()
            .filter(|host| !host.trim().is_empty())
            .unwrap_or_else(|| provider.default_upstream().to_owned());
        Ok(Self {
            provider,
            upstream,
            credential: Credential::new(credential),
            sandbox_key: Credential::new(sandbox_key()),
            models: models.into_iter().collect(),
        })
    }
}

/// A random per-job key with no meaning outside this broker.
pub fn sandbox_key() -> String {
    format!(
        "fiach-sandbox-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[derive(Clone, Debug)]
pub enum Listen {
    Tcp(SocketAddr),
    /// A Unix socket created inside this directory, named after the provider.
    Unix(PathBuf),
}

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub routes: Vec<RouteConfig>,
    pub listen: Listen,
    pub max_request_bytes: usize,
    /// Total spend across all routes before further requests are refused.
    pub max_cost_usd: Option<f64>,
    /// JSONL request log; bodies are never written.
    pub log_path: Option<PathBuf>,
    /// Planted secrets beyond the routes' own keys, such as a fake GitHub
    /// token in the sandbox's `gh` config. Seeing one quarantines the job.
    pub canaries: Vec<Canary>,
}

/// A secret whose appearance in a request means the job is leaking it.
#[derive(Clone, Debug)]
pub struct Canary {
    /// Names the secret in reports without revealing it.
    pub label: String,
    pub value: Credential,
}

/// Why the broker stopped serving a job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Quarantine {
    pub provider: Provider,
    pub path: String,
    /// Which secret appeared, by label.
    pub canary: String,
}

/// Where a running route accepts connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Clone, Debug)]
pub struct RouteEndpoint {
    pub provider: Provider,
    pub endpoint: Endpoint,
    pub sandbox_key: Credential,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Usage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Default)]
struct Ledger {
    usage: Usage,
    unpriced_requests: u64,
}

struct Shared {
    ledger: Mutex<Ledger>,
    max_cost_usd: Option<f64>,
    log: Option<Mutex<std::fs::File>>,
    canaries: Vec<Canary>,
    quarantine: Mutex<Option<Quarantine>>,
}

impl Shared {
    fn quarantined(&self) -> bool {
        self.quarantine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Returns the label of the first canary found in the request.
    fn find_canary(&self, uri: &Uri, body: &[u8]) -> Option<&str> {
        let body = String::from_utf8_lossy(body);
        let uri = uri.to_string();
        self.canaries
            .iter()
            .find(|canary| {
                let value = canary.value.expose();
                !value.is_empty() && (body.contains(value) || uri.contains(value))
            })
            .map(|canary| canary.label.as_str())
    }

    fn quarantine(&self, quarantine: Quarantine) {
        let mut current = self.quarantine.lock().unwrap_or_else(|e| e.into_inner());
        if current.is_none() {
            tracing::error!(
                provider = %quarantine.provider,
                path = %quarantine.path,
                canary = %quarantine.canary,
                "Model broker saw a secret in a sandbox request; quarantining job"
            );
            *current = Some(quarantine);
        }
    }

    fn budget_exhausted(&self) -> bool {
        let ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        self.max_cost_usd
            .is_some_and(|max| ledger.usage.cost_usd >= max)
    }

    fn charge(&self, pricing: Option<Pricing>, input_tokens: u64, output_tokens: u64) -> f64 {
        let cost = pricing.map_or(0.0, |pricing| {
            (input_tokens as f64 * pricing.input_per_m
                + output_tokens as f64 * pricing.output_per_m)
                / 1_000_000.0
        });
        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        ledger.usage.requests += 1;
        ledger.usage.input_tokens += input_tokens;
        ledger.usage.output_tokens += output_tokens;
        ledger.usage.cost_usd += cost;
        if pricing.is_none() && input_tokens + output_tokens > 0 {
            ledger.unpriced_requests += 1;
        }
        cost
    }

    fn log(&self, record: &LogRecord<'_>) {
        let Some(log) = &self.log else {
            return;
        };
        let Ok(mut line) = serde_json::to_vec(record) else {
            return;
        };
        line.push(b'\n');
        let mut file = log.lock().unwrap_or_else(|e| e.into_inner());
        let _ = file.write_all(&line);
    }
}

struct RouteState {
    provider: Provider,
    upstream: String,
    credential: Credential,
    sandbox_key: Credential,
    models: BTreeMap<String, Option<Pricing>>,
    max_request_bytes: usize,
    client: reqwest::Client,
    shared: Arc<Shared>,
}

#[derive(Serialize)]
struct LogRecord<'a> {
    unix_ms: u128,
    provider: Provider,
    method: &'a str,
    path: &'a str,
    model: Option<&'a str>,
    status: u16,
    outcome: &'a str,
    request_bytes: usize,
    response_bytes: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    duration_ms: u128,
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

/// What the broker observed for a job, returned when it shuts down.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BrokerReport {
    pub usage: Usage,
    /// Requests with usage for models without pricing; not charged.
    pub unpriced_requests: u64,
    /// Set when the job tried to send a secret; its results must not be
    /// disclosed.
    pub quarantine: Option<Quarantine>,
}

pub struct Broker {
    endpoints: Vec<RouteEndpoint>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Broker {
    pub async fn start(config: BrokerConfig) -> Result<Self> {
        ensure!(
            !config.routes.is_empty(),
            "Model broker needs at least one route"
        );
        let mut providers = BTreeSet::new();
        for route in &config.routes {
            ensure!(
                providers.insert(route.provider),
                "Duplicate model broker route for {}",
                route.provider
            );
            ensure!(
                !route.models.is_empty(),
                "Model broker route for {} allows no models",
                route.provider
            );
        }
        let log = match &config.log_path {
            Some(path) => Some(Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("Opening broker log {}", path.display()))?,
            )),
            None => None,
        };
        let mut canaries = config.canaries;
        for route in &config.routes {
            canaries.push(Canary {
                label: format!("{} sandbox key", route.provider),
                value: route.sandbox_key.clone(),
            });
            canaries.push(Canary {
                label: format!("{} credential", route.provider),
                value: route.credential.clone(),
            });
        }
        let shared = Arc::new(Shared {
            ledger: Mutex::new(Ledger::default()),
            max_cost_usd: config.max_cost_usd,
            log,
            canaries,
            quarantine: Mutex::new(None),
        });
        let cancel = CancellationToken::new();
        let mut endpoints = Vec::new();
        let mut tasks = Vec::new();
        for route in config.routes {
            let client = upstream_client(&route.upstream).await?;
            let state = Arc::new(RouteState {
                provider: route.provider,
                upstream: route.upstream.trim_end_matches('/').to_owned(),
                credential: route.credential,
                sandbox_key: route.sandbox_key.clone(),
                models: route.models,
                max_request_bytes: config.max_request_bytes,
                client,
                shared: shared.clone(),
            });
            let app = Router::new().fallback(forward).with_state(state);
            let shutdown = cancel.clone().cancelled_owned();
            let endpoint = match &config.listen {
                Listen::Tcp(addr) => {
                    let listener = tokio::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("Binding broker listener on {addr}"))?;
                    let local = listener.local_addr()?;
                    tasks.push(tokio::spawn(async move {
                        let _ = axum::serve(listener, app)
                            .with_graceful_shutdown(shutdown)
                            .await;
                    }));
                    Endpoint::Tcp(local)
                }
                Listen::Unix(dir) => {
                    let path = dir.join(format!("{}.sock", route.provider));
                    let listener = bind_unix(&path)?;
                    tasks.push(tokio::spawn(async move {
                        let _ = axum::serve(listener, app)
                            .with_graceful_shutdown(shutdown)
                            .await;
                    }));
                    Endpoint::Unix(path)
                }
            };
            endpoints.push(RouteEndpoint {
                provider: route.provider,
                endpoint,
                sandbox_key: route.sandbox_key,
            });
        }
        Ok(Self {
            endpoints,
            shared,
            cancel,
            tasks,
        })
    }

    pub fn endpoints(&self) -> &[RouteEndpoint] {
        &self.endpoints
    }

    /// Environment for a client that reaches the routes over TCP directly.
    pub fn sandbox_env(&self) -> Vec<(&'static str, String)> {
        self.endpoints
            .iter()
            .filter_map(|route| match &route.endpoint {
                Endpoint::Tcp(addr) => Some([
                    (route.provider.host_env(), format!("http://{addr}")),
                    (
                        route.provider.key_env(),
                        route.sandbox_key.expose().to_owned(),
                    ),
                ]),
                Endpoint::Unix(_) => None,
            })
            .flatten()
            .collect()
    }

    /// Writes the manifest `fiach broker-bridge` reads inside the sandbox.
    /// Socket paths are relative to the manifest's directory, so the host
    /// can bind that directory anywhere in the sandbox.
    pub fn write_manifest(&self, path: &Path) -> Result<()> {
        let mut routes = Vec::new();
        for route in &self.endpoints {
            let Endpoint::Unix(socket) = &route.endpoint else {
                bail!("Bridge manifests need Unix socket routes");
            };
            let socket = socket
                .file_name()
                .context("Broker socket has no file name")?
                .to_string_lossy()
                .into_owned();
            routes.push(BridgeRoute {
                provider: route.provider,
                socket,
                sandbox_key: route.sandbox_key.expose().to_owned(),
            });
        }
        std::fs::write(path, serde_json::to_vec(&BridgeManifest { routes })?)
            .with_context(|| format!("Writing broker manifest {}", path.display()))
    }

    pub fn quarantine(&self) -> Option<Quarantine> {
        self.shared
            .quarantine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn usage(&self) -> Usage {
        self.shared
            .ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .usage
            .clone()
    }

    /// Stops accepting requests and returns what the job used. In-flight
    /// responses are cut off when the listeners stop.
    pub async fn shutdown(mut self) -> BrokerReport {
        self.cancel.cancel();
        for task in std::mem::take(&mut self.tasks) {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        let quarantine = self.quarantine();
        let ledger = self.shared.ledger.lock().unwrap_or_else(|e| e.into_inner());
        BrokerReport {
            usage: ledger.usage.clone(),
            unpriced_requests: ledger.unpriced_requests,
            quarantine,
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Removing stale socket {}", path.display()));
        }
    }
    tokio::net::UnixListener::bind(path)
        .with_context(|| format!("Binding broker socket {}", path.display()))
}

/// An HTTP client for one upstream. Redirects are never followed, and the
/// upstream host is resolved once so a later DNS change cannot move it.
async fn upstream_client(upstream: &str) -> Result<reqwest::Client> {
    let url = reqwest::Url::parse(upstream)
        .with_context(|| format!("Invalid model broker upstream {upstream}"))?;
    let host = url
        .host_str()
        .with_context(|| format!("Model broker upstream {upstream} has no host"))?
        .to_owned();
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .ok();
    let loopback = ip.map_or(host == "localhost", |ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        scheme => bail!("Model broker upstream {upstream} must use https, not {scheme}"),
    }
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.query().is_none(),
        "Model broker upstream {upstream} must not carry credentials or a query"
    );
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT);
    if ip.is_none() {
        let port = url
            .port_or_known_default()
            .context("Model broker upstream has no port")?;
        let addr = tokio::net::lookup_host((host.as_str(), port))
            .await
            .with_context(|| format!("Resolving model broker upstream {host}"))?
            .next()
            .with_context(|| format!("Model broker upstream {host} has no addresses"))?;
        builder = builder.resolve(&host, addr);
    }
    builder.build().context("Building model broker HTTP client")
}

fn deny(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "message": format!("fiach broker: {message}") } });
    (status, axum::Json(body)).into_response()
}

async fn forward(
    State(route): State<Arc<RouteState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let started = Instant::now();
    let path = uri.path().to_owned();
    let mut record = PendingRecord {
        route: route.clone(),
        method: method.to_string(),
        path: path.clone(),
        model: None,
        request_bytes: 0,
        started,
    };
    let Some(location) = route.provider.endpoint(&method, &path) else {
        return record.finish_denied(StatusCode::FORBIDDEN, "endpoint not allowed");
    };
    let presented = route.provider.presented_key(&headers);
    if presented != Some(route.sandbox_key.expose()) {
        return record.finish_denied(StatusCode::UNAUTHORIZED, "invalid sandbox key");
    }
    if route.shared.quarantined() {
        return record.finish_denied(StatusCode::FORBIDDEN, "job quarantined");
    }
    if route.shared.budget_exhausted() {
        return record.finish_denied(StatusCode::FORBIDDEN, "job cost budget exhausted");
    }
    let body = match axum::body::to_bytes(body, route.max_request_bytes).await {
        Ok(body) => body,
        Err(_) => return record.finish_denied(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
    };
    record.request_bytes = body.len();
    if let Some(canary) = route.shared.find_canary(&uri, &body) {
        route.shared.quarantine(Quarantine {
            provider: route.provider,
            path: path.clone(),
            canary: canary.to_owned(),
        });
        return record.finish_denied(StatusCode::FORBIDDEN, "job quarantined");
    }
    let model = match location {
        ModelLocation::None => None,
        ModelLocation::Path(model) => Some(model),
        ModelLocation::Body => {
            let Ok(json) = serde_json::from_slice::<Value>(&body) else {
                return record.finish_denied(StatusCode::BAD_REQUEST, "request body is not JSON");
            };
            if let Err(message) = route.provider.check_body(&json) {
                return record.finish_denied(StatusCode::FORBIDDEN, message);
            }
            match json.get("model").and_then(Value::as_str) {
                Some(model) => Some(model.to_owned()),
                None => {
                    return record.finish_denied(StatusCode::BAD_REQUEST, "request names no model");
                }
            }
        }
    };
    if let Some(model) = &model {
        record.model = Some(model.clone());
        if !route.models.contains_key(model) {
            return record.finish_denied(StatusCode::FORBIDDEN, "model not allowed for this job");
        }
    }

    let Ok(mut target) = reqwest::Url::parse(&format!("{}{}", route.upstream, path)) else {
        return record.finish_denied(StatusCode::BAD_REQUEST, "invalid request path");
    };
    let query: Vec<(String, String)> = match uri.query() {
        Some(query) => match reqwest::Url::parse(&format!("http://query/?{query}")) {
            Ok(parsed) => parsed
                .query_pairs()
                .filter(|(name, _)| route.provider.allows_query_param(name))
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect(),
            Err(_) => return record.finish_denied(StatusCode::BAD_REQUEST, "invalid query"),
        },
        None => Vec::new(),
    };
    if !query.is_empty() {
        target.query_pairs_mut().extend_pairs(&query);
    }
    let mut upstream_headers = reqwest::header::HeaderMap::new();
    for name in FORWARDED_REQUEST_HEADERS {
        if let Some(value) = headers.get(*name) {
            upstream_headers.insert(HeaderName::from_static(name), value.clone());
        }
    }
    let auth = match route.provider.auth_headers(route.credential.expose()) {
        Ok(auth) => auth,
        Err(_) => return record.finish_denied(StatusCode::BAD_GATEWAY, "invalid host credential"),
    };
    for (name, value) in auth {
        upstream_headers.insert(name, value);
    }
    let response = match route
        .client
        .request(method.clone(), target)
        .headers(upstream_headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(provider = %route.provider, error = %error, "Model broker upstream request failed");
            return record.finish_denied(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };

    let status = response.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in response.headers() {
        let name_str = name.as_str();
        if FORWARDED_RESPONSE_HEADERS.contains(&name_str)
            || FORWARDED_RESPONSE_HEADER_PREFIXES
                .iter()
                .any(|prefix| name_str.starts_with(prefix))
        {
            builder = builder.header(name.clone(), value.clone());
        }
    }
    let sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let stream = ResponseStream {
        response,
        scan: UsageScan::new(sse),
        record: Some(record),
        status: status.as_u16(),
    };
    let body = Body::from_stream(futures::stream::unfold(stream, |mut stream| async move {
        stream.record.as_ref()?;
        match stream.response.chunk().await {
            Ok(Some(chunk)) => {
                stream.scan.feed(&chunk);
                Some((Ok::<Bytes, std::io::Error>(chunk), stream))
            }
            Ok(None) => {
                stream.finish("ok");
                None
            }
            Err(error) => {
                stream.finish("upstream_error");
                Some((Err(std::io::Error::other(error)), stream))
            }
        }
    }));
    builder
        .body(body)
        .unwrap_or_else(|_| deny(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}

struct PendingRecord {
    route: Arc<RouteState>,
    method: String,
    path: String,
    model: Option<String>,
    request_bytes: usize,
    started: Instant,
}

impl PendingRecord {
    fn pricing(&self) -> Option<Pricing> {
        self.model
            .as_ref()
            .and_then(|model| self.route.models.get(model).copied().flatten())
    }

    fn finish_denied(self, status: StatusCode, message: &str) -> Response {
        tracing::warn!(
            provider = %self.route.provider,
            path = %self.path,
            model = ?self.model,
            status = status.as_u16(),
            reason = message,
            "Model broker refused request"
        );
        self.write(status.as_u16(), message, 0, 0, 0, 0.0);
        deny(status, message)
    }

    fn write(
        &self,
        status: u16,
        outcome: &str,
        response_bytes: u64,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    ) {
        self.route.shared.log(&LogRecord {
            unix_ms: unix_ms(),
            provider: self.route.provider,
            method: &self.method,
            path: &self.path,
            model: self.model.as_deref(),
            status,
            outcome,
            request_bytes: self.request_bytes,
            response_bytes,
            input_tokens,
            output_tokens,
            cost_usd,
            duration_ms: self.started.elapsed().as_millis(),
        });
    }
}

/// An upstream response being streamed to the sandbox. Usage is charged when
/// the stream ends, or when it is dropped because the sandbox disconnected.
struct ResponseStream {
    response: reqwest::Response,
    scan: UsageScan,
    record: Option<PendingRecord>,
    status: u16,
}

impl ResponseStream {
    fn finish(&mut self, outcome: &str) {
        let Some(record) = self.record.take() else {
            return;
        };
        let (input, output) = self.scan.finish();
        let cost = record.route.shared.charge(record.pricing(), input, output);
        record.write(self.status, outcome, self.scan.bytes, input, output, cost);
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.finish("client_disconnected");
    }
}

/// Reads token usage from provider responses without changing them.
///
/// The providers report usage in different places, and streaming responses
/// repeat or accumulate it across events, so the scan keeps the largest value
/// seen for input and output separately:
/// - OpenAI-style chat: `usage.prompt_tokens` / `usage.completion_tokens`,
///   in the final event when streaming;
/// - OpenAI Responses: `response.usage.input_tokens` / `output_tokens`;
/// - Anthropic: `message.usage` in `message_start`, cumulative `usage` in
///   `message_delta`, including cache tokens;
/// - Google: cumulative `usageMetadata` in each chunk;
/// - TypeSafe: `usage.input_tokens` / `output_tokens`.
struct UsageScan {
    sse: bool,
    buffer: Vec<u8>,
    overflow: bool,
    bytes: u64,
    input: u64,
    output: u64,
}

impl UsageScan {
    fn new(sse: bool) -> Self {
        Self {
            sse,
            buffer: Vec::new(),
            overflow: false,
            bytes: 0,
            input: 0,
            output: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        if self.overflow {
            return;
        }
        if self.buffer.len() + chunk.len() > MAX_USAGE_SCAN_BYTES {
            self.overflow = true;
            self.buffer.clear();
            return;
        }
        self.buffer.extend_from_slice(chunk);
        if self.sse {
            while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                self.scan_sse_line(&line);
            }
        }
    }

    fn scan_sse_line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else {
            return;
        };
        if let Some(data) = line.trim_end().strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim_start())
        {
            self.observe(&value);
        }
    }

    fn finish(&mut self) -> (u64, u64) {
        let buffer = std::mem::take(&mut self.buffer);
        if self.sse {
            self.scan_sse_line(&buffer);
        } else if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
            self.observe(&value);
        }
        (self.input, self.output)
    }

    fn observe(&mut self, value: &Value) {
        for usage in [
            value.get("usage"),
            value.pointer("/message/usage"),
            value.pointer("/response/usage"),
            value.get("usageMetadata"),
        ]
        .into_iter()
        .flatten()
        {
            let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
            let input = field("prompt_tokens")
                + field("input_tokens")
                + field("cache_creation_input_tokens")
                + field("cache_read_input_tokens")
                + field("promptTokenCount");
            let output = field("completion_tokens")
                + field("output_tokens")
                + field("candidatesTokenCount")
                + field("thoughtsTokenCount");
            self.input = self.input.max(input);
            self.output = self.output.max(output);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BridgeManifest {
    routes: Vec<BridgeRoute>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BridgeRoute {
    provider: Provider,
    /// File name of the route's socket next to the manifest.
    socket: String,
    sandbox_key: String,
}

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Loopback listeners inside the sandbox that relay to the broker sockets.
pub struct Bridge {
    env: Vec<(&'static str, String)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Bridge {
    pub async fn start(manifest_path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(manifest_path)
            .with_context(|| format!("Reading broker manifest {}", manifest_path.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES,
            "Invalid broker manifest {}",
            manifest_path.display()
        );
        let manifest: BridgeManifest = serde_json::from_slice(&std::fs::read(manifest_path)?)
            .context("Parsing broker manifest")?;
        let dir = manifest_path
            .parent()
            .context("Broker manifest has no directory")?;
        let mut env = Vec::new();
        let mut tasks = Vec::new();
        for route in manifest.routes {
            ensure!(
                !route.socket.is_empty()
                    && !route.socket.contains('/')
                    && route.socket != "."
                    && route.socket != "..",
                "Invalid broker socket name {}",
                route.socket
            );
            let socket = dir.join(&route.socket);
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("Binding broker bridge listener")?;
            let addr = listener.local_addr()?;
            tasks.push(tokio::spawn(relay(listener, socket)));
            env.push((route.provider.host_env(), format!("http://{addr}")));
            env.push((route.provider.key_env(), route.sandbox_key));
        }
        Ok(Self { env, tasks })
    }

    pub fn env(&self) -> &[(&'static str, String)] {
        &self.env
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn relay(listener: tokio::net::TcpListener, socket: PathBuf) {
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let socket = socket.clone();
        tokio::spawn(async move {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Err(error) => {
                    tracing::warn!(socket = %socket.display(), error = %error, "Broker bridge could not reach the broker");
                }
            }
        });
    }
}

/// Runs `command` with the bridge's provider variables and returns its
/// exit status. Provider variables already in the environment are replaced.
pub async fn run_bridge(
    manifest_path: &Path,
    command: &[String],
    cancel: CancellationToken,
) -> Result<std::process::ExitStatus> {
    let (program, args) = command
        .split_first()
        .context("broker-bridge needs a command to run")?;
    let bridge = Bridge::start(manifest_path).await?;
    let mut child = tokio::process::Command::new(program);
    child.args(args).kill_on_drop(true);
    for provider in Provider::ALL {
        child.env_remove(provider.key_env());
        child.env_remove(provider.host_env());
    }
    child.envs(bridge.env().iter().map(|(name, value)| (*name, value)));
    let mut child = child
        .spawn()
        .with_context(|| format!("Starting {program} under the broker bridge"))?;
    tokio::select! {
        status = child.wait() => Ok(status?),
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            bail!("broker-bridge cancelled")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        query: Option<String>,
        headers: HeaderMap,
        body: Bytes,
    }

    type Responder = fn(&str) -> Response;

    #[derive(Clone, Default)]
    struct Mock {
        seen: Arc<Mutex<Vec<Seen>>>,
        hits: Arc<AtomicUsize>,
    }

    /// A local upstream that records requests and answers with a canned
    /// response for each provider shape.
    async fn mock_upstream(response: Responder) -> (String, Mock) {
        let mock = Mock::default();
        let state = (mock.clone(), response);
        let app = Router::new()
            .fallback(
                |State((mock, response)): State<(Mock, Responder)>,
                 uri: Uri,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    mock.hits.fetch_add(1, Ordering::SeqCst);
                    mock.seen.lock().unwrap().push(Seen {
                        path: uri.path().to_owned(),
                        query: uri.query().map(str::to_owned),
                        headers,
                        body,
                    });
                    response(uri.path())
                },
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), mock)
    }

    fn json_response(body: Value) -> Response {
        (StatusCode::OK, axum::Json(body)).into_response()
    }

    fn sse_response(events: &[Value]) -> Response {
        let body: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
            .collect();
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    fn route(provider: Provider, upstream: &str, model: &str) -> RouteConfig {
        RouteConfig {
            provider,
            upstream: upstream.to_owned(),
            credential: Credential::new(format!("real-{provider}-key")),
            sandbox_key: Credential::new(sandbox_key()),
            models: BTreeMap::from([(
                model.to_owned(),
                Some(Pricing {
                    input_per_m: 1.0,
                    output_per_m: 2.0,
                }),
            )]),
        }
    }

    async fn start(routes: Vec<RouteConfig>, max_cost_usd: Option<f64>) -> Broker {
        Broker::start(BrokerConfig {
            routes,
            listen: Listen::Tcp("127.0.0.1:0".parse().unwrap()),
            max_request_bytes: 64 * 1024,
            max_cost_usd,
            log_path: None,
            canaries: Vec::new(),
        })
        .await
        .unwrap()
    }

    fn base(broker: &Broker) -> String {
        match &broker.endpoints()[0].endpoint {
            Endpoint::Tcp(addr) => format!("http://{addr}"),
            Endpoint::Unix(path) => panic!("unexpected unix endpoint {}", path.display()),
        }
    }

    fn key(broker: &Broker) -> String {
        broker.endpoints()[0].sandbox_key.expose().to_owned()
    }

    fn client_auth(
        provider: Provider,
        request: reqwest::RequestBuilder,
        key: &str,
    ) -> reqwest::RequestBuilder {
        match provider {
            Provider::OpenRouter | Provider::OpenAi | Provider::TypeSafe => {
                request.bearer_auth(key)
            }
            Provider::Anthropic => request.header("x-api-key", key),
            Provider::Google => request.header("x-goog-api-key", key),
        }
    }

    struct Case {
        provider: Provider,
        model: &'static str,
        path: String,
        body: Value,
        response: Responder,
        input: u64,
        output: u64,
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                provider: Provider::OpenRouter,
                model: "openai/gpt-5",
                path: "/api/v1/chat/completions".into(),
                body: serde_json::json!({ "model": "openai/gpt-5", "stream": true }),
                response: |_| {
                    sse_response(&[
                        serde_json::json!({ "choices": [{ "delta": { "content": "hi" } }] }),
                        serde_json::json!({ "choices": [], "usage": { "prompt_tokens": 1000, "completion_tokens": 200 } }),
                    ])
                },
                input: 1000,
                output: 200,
            },
            Case {
                provider: Provider::OpenAi,
                model: "gpt-5",
                path: "/v1/responses".into(),
                body: serde_json::json!({ "model": "gpt-5", "stream": true }),
                response: |_| {
                    sse_response(&[
                        serde_json::json!({ "type": "response.output_text.delta", "delta": "hi" }),
                        serde_json::json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 300, "output_tokens": 40 } } }),
                    ])
                },
                input: 300,
                output: 40,
            },
            Case {
                provider: Provider::Anthropic,
                model: "claude-opus-5-5",
                path: "/v1/messages".into(),
                body: serde_json::json!({ "model": "claude-opus-5-5", "stream": true }),
                response: |_| {
                    sse_response(&[
                        serde_json::json!({ "type": "message_start", "message": { "usage": { "input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1 } } }),
                        serde_json::json!({ "type": "message_delta", "usage": { "output_tokens": 25 } }),
                    ])
                },
                input: 100,
                output: 25,
            },
            Case {
                provider: Provider::Google,
                model: "gemini-3-pro",
                path: "/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse&key=leak".into(),
                body: serde_json::json!({ "contents": [] }),
                response: |_| {
                    sse_response(&[
                        serde_json::json!({ "usageMetadata": { "promptTokenCount": 50, "candidatesTokenCount": 5 } }),
                        serde_json::json!({ "usageMetadata": { "promptTokenCount": 50, "candidatesTokenCount": 12, "thoughtsTokenCount": 8 } }),
                    ])
                },
                input: 50,
                output: 20,
            },
            Case {
                provider: Provider::TypeSafe,
                model: "jev-1.13.0",
                path: "/v1/systemone".into(),
                body: serde_json::json!({ "model": "jev-1.13.0", "state": "s", "questions": {} }),
                response: |_| {
                    json_response(serde_json::json!({
                        "model": "jev-1.13.0",
                        "answers": {},
                        "usage": { "input_tokens": 70, "output_tokens": 3 }
                    }))
                },
                input: 70,
                output: 3,
            },
        ]
    }

    #[tokio::test]
    async fn forwards_each_provider_with_host_credentials() {
        for case in cases() {
            let (upstream, mock) = mock_upstream(case.response).await;
            let broker = start(vec![route(case.provider, &upstream, case.model)], None).await;
            let sandbox_key = key(&broker);
            let request = reqwest::Client::new()
                .post(format!("{}{}", base(&broker), case.path))
                .header("cookie", "session=1")
                .json(&case.body);
            let response = client_auth(case.provider, request, &sandbox_key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{}", case.provider);
            let text = response.text().await.unwrap();
            assert!(!text.is_empty());

            let seen = mock.seen.lock().unwrap()[0].clone();
            let real = format!("real-{}-key", case.provider);
            let (auth_header, expected) = match case.provider {
                Provider::OpenRouter | Provider::OpenAi | Provider::TypeSafe => {
                    ("authorization", format!("Bearer {real}"))
                }
                Provider::Anthropic => ("x-api-key", real.clone()),
                Provider::Google => ("x-goog-api-key", real.clone()),
            };
            assert_eq!(seen.headers.get(auth_header).unwrap(), expected.as_str());
            assert!(seen.headers.get("cookie").is_none());
            if case.provider == Provider::Anthropic {
                assert_eq!(
                    seen.headers.get("anthropic-version").unwrap(),
                    ANTHROPIC_VERSION
                );
            }
            let forwarded = format!("{:?}{:?}{:?}", seen.headers, seen.query, seen.body);
            assert!(!forwarded.contains(&sandbox_key), "{}", case.provider);
            assert!(!forwarded.contains("leak"), "{}", case.provider);
            if case.provider == Provider::Google {
                assert_eq!(seen.query.as_deref(), Some("alt=sse"));
            }
            assert_eq!(seen.path, case.path.split('?').next().unwrap());

            let report = broker.shutdown().await;
            assert_eq!(report.usage.requests, 1, "{}", case.provider);
            assert_eq!(report.usage.input_tokens, case.input, "{}", case.provider);
            assert_eq!(report.usage.output_tokens, case.output, "{}", case.provider);
            let expected_cost = (case.input as f64 + 2.0 * case.output as f64) / 1_000_000.0;
            assert!((report.usage.cost_usd - expected_cost).abs() < 1e-12);
        }
    }

    #[tokio::test]
    async fn rejects_wrong_model_for_each_provider() {
        for case in cases() {
            let (upstream, mock) = mock_upstream(case.response).await;
            let broker = start(vec![route(case.provider, &upstream, "other-model")], None).await;
            let request = reqwest::Client::new()
                .post(format!("{}{}", base(&broker), case.path))
                .json(&case.body);
            let response = client_auth(case.provider, request, &key(&broker))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{}", case.provider);
            assert_eq!(mock.hits.load(Ordering::SeqCst), 0, "{}", case.provider);
        }
    }

    #[tokio::test]
    async fn rejects_missing_or_real_credentials() {
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        let broker = start(vec![route(case.provider, &upstream, case.model)], None).await;
        for presented in [None, Some("real-openrouter-key"), Some("guess")] {
            let mut request = reqwest::Client::new()
                .post(format!("{}{}", base(&broker), case.path))
                .json(&case.body);
            if let Some(presented) = presented {
                request = request.bearer_auth(presented);
            }
            assert_eq!(request.send().await.unwrap().status(), 401);
        }
        assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_unlisted_endpoints_and_methods() {
        let (upstream, mock) = mock_upstream(|_| json_response(serde_json::json!({}))).await;
        let broker = start(
            vec![route(Provider::OpenRouter, &upstream, "openai/gpt-5")],
            None,
        )
        .await;
        let client = reqwest::Client::new();
        let base = base(&broker);
        let key = key(&broker);
        for (method, path) in [
            (Method::POST, "/api/v1/auth/keys"),
            (Method::GET, "/api/v1/chat/completions"),
            (Method::POST, "/api/alpha/decisions"),
            (Method::GET, "/api/v1/credits"),
        ] {
            let response = client
                .request(method.clone(), format!("{base}{path}"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{method} {path}");
        }
        let models = client
            .get(format!("{base}/api/v1/models"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap();
        assert_eq!(models.status(), 200);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_openrouter_model_fallbacks() {
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        let broker = start(vec![route(case.provider, &upstream, case.model)], None).await;
        let response = reqwest::Client::new()
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&serde_json::json!({ "model": case.model, "models": ["anthropic/claude-opus-5-5"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_oversized_bodies() {
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        let broker = start(vec![route(case.provider, &upstream, case.model)], None).await;
        let padding = "x".repeat(128 * 1024);
        let response = reqwest::Client::new()
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&serde_json::json!({ "model": case.model, "padding": padding }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refuses_requests_once_budget_is_spent() {
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        // One response costs 0.0014 USD at the test pricing.
        let broker = start(
            vec![route(case.provider, &upstream, case.model)],
            Some(0.002),
        )
        .await;
        let client = reqwest::Client::new();
        let mut statuses = Vec::new();
        for _ in 0..3 {
            let response = client
                .post(format!("{}{}", base(&broker), case.path))
                .bearer_auth(key(&broker))
                .json(&case.body)
                .send()
                .await
                .unwrap();
            statuses.push(response.status().as_u16());
            let _ = response.bytes().await.unwrap();
        }
        assert_eq!(statuses, [200, 200, 403]);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
        let report = broker.shutdown().await;
        assert_eq!(report.usage.requests, 2);
    }

    #[tokio::test]
    async fn charges_each_model_at_its_own_price() {
        let case = &cases()[0];
        let (upstream, _mock) = mock_upstream(case.response).await;
        let mut config = route(case.provider, &upstream, case.model);
        config.models.insert(
            "anthropic/claude-opus-5-5".into(),
            Some(Pricing {
                input_per_m: 10.0,
                output_per_m: 0.0,
            }),
        );
        config.models.insert("unpriced/model".into(), None);
        let broker = start(vec![config], None).await;
        let client = reqwest::Client::new();
        for model in [case.model, "anthropic/claude-opus-5-5", "unpriced/model"] {
            let response = client
                .post(format!("{}{}", base(&broker), case.path))
                .bearer_auth(key(&broker))
                .json(&serde_json::json!({ "model": model }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{model}");
            let _ = response.bytes().await.unwrap();
        }
        let report = broker.shutdown().await;
        // 1000 input and 200 output tokens per response: 0.0014 + 0.01 + 0.
        assert!((report.usage.cost_usd - 0.0114).abs() < 1e-12);
        assert_eq!(report.usage.requests, 3);
        assert_eq!(report.unpriced_requests, 1);
    }

    #[tokio::test]
    async fn does_not_follow_redirects() {
        let (upstream, mock) = mock_upstream(|_| {
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header(header::LOCATION, "http://127.0.0.1:9/steal")
                .body(Body::empty())
                .unwrap()
        })
        .await;
        let broker = start(
            vec![route(Provider::OpenRouter, &upstream, "openai/gpt-5")],
            None,
        )
        .await;
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("{}/api/v1/models", base(&broker)))
            .bearer_auth(key(&broker))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 307);
        assert!(response.headers().get(header::LOCATION).is_none());
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn logs_metadata_without_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("broker.jsonl");
        let case = &cases()[0];
        let (upstream, _mock) = mock_upstream(case.response).await;
        let broker = Broker::start(BrokerConfig {
            routes: vec![route(case.provider, &upstream, case.model)],
            listen: Listen::Tcp("127.0.0.1:0".parse().unwrap()),
            max_request_bytes: 64 * 1024,
            max_cost_usd: None,
            log_path: Some(log_path.clone()),
            canaries: Vec::new(),
        })
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&serde_json::json!({ "model": case.model, "messages": ["secret prompt"] }))
            .send()
            .await
            .unwrap();
        let _ = response.bytes().await.unwrap();
        let denied = client
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&serde_json::json!({ "model": "other" }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
        broker.shutdown().await;

        let log = std::fs::read_to_string(&log_path).unwrap();
        let records: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["outcome"], "ok");
        assert_eq!(records[0]["model"], case.model);
        assert_eq!(records[0]["input_tokens"], 1000);
        assert_eq!(records[1]["outcome"], "model not allowed for this job");
        assert_eq!(records[1]["status"], 403);
        assert!(!log.contains("secret prompt"));
        assert!(!log.contains("real-openrouter-key"));
    }

    #[tokio::test]
    async fn serves_routes_on_unix_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let case = &cases()[4];
        let (upstream, _mock) = mock_upstream(case.response).await;
        let broker = Broker::start(BrokerConfig {
            routes: vec![route(case.provider, &upstream, case.model)],
            listen: Listen::Unix(dir.path().to_owned()),
            max_request_bytes: 64 * 1024,
            max_cost_usd: None,
            log_path: None,
            canaries: Vec::new(),
        })
        .await
        .unwrap();
        let Endpoint::Unix(path) = &broker.endpoints()[0].endpoint else {
            panic!("expected a unix endpoint");
        };
        assert_eq!(path, &dir.path().join("typesafe.sock"));
        let body = serde_json::to_vec(&case.body).unwrap();
        let request = format!(
            "POST /v1/systemone HTTP/1.1\r\nhost: broker\r\nauthorization: Bearer {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            key(&broker),
            body.len()
        );
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let report = broker.shutdown().await;
        assert_eq!(report.usage.input_tokens, 70);
    }

    #[tokio::test]
    async fn canary_in_request_quarantines_the_job() {
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        let broker = Broker::start(BrokerConfig {
            routes: vec![route(case.provider, &upstream, case.model)],
            listen: Listen::Tcp("127.0.0.1:0".parse().unwrap()),
            max_request_bytes: 64 * 1024,
            max_cost_usd: None,
            log_path: None,
            canaries: vec![Canary {
                label: "gh hosts token".into(),
                value: Credential::new("ghp_canaryvalue0123456789"),
            }],
        })
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let leaked = format!("the key is {}", key(&broker));
        let response = client
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&serde_json::json!({ "model": case.model, "messages": [leaked] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        let clean = client
            .post(format!("{}{}", base(&broker), case.path))
            .bearer_auth(key(&broker))
            .json(&case.body)
            .send()
            .await
            .unwrap();
        assert_eq!(clean.status(), 403);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
        let report = broker.shutdown().await;
        assert_eq!(
            report.quarantine,
            Some(Quarantine {
                provider: Provider::OpenRouter,
                path: case.path.clone(),
                canary: "openrouter sandbox key".into(),
            })
        );
    }

    #[tokio::test]
    async fn real_credentials_and_planted_canaries_quarantine() {
        for (leak, label) in [
            ("real-google-key", "google credential"),
            ("ghp_canaryvalue0123456789", "gh hosts token"),
        ] {
            let case = &cases()[3];
            let (upstream, mock) = mock_upstream(case.response).await;
            let broker = Broker::start(BrokerConfig {
                routes: vec![route(case.provider, &upstream, case.model)],
                listen: Listen::Tcp("127.0.0.1:0".parse().unwrap()),
                max_request_bytes: 64 * 1024,
                max_cost_usd: None,
                log_path: None,
                canaries: vec![Canary {
                    label: "gh hosts token".into(),
                    value: Credential::new("ghp_canaryvalue0123456789"),
                }],
            })
            .await
            .unwrap();
            // Leaks are caught in the query string as well as the body.
            let response = reqwest::Client::new()
                .post(format!(
                    "{}/v1beta/models/{}:generateContent?alt={leak}",
                    base(&broker),
                    case.model
                ))
                .header("x-goog-api-key", key(&broker))
                .json(&case.body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{label}");
            assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
            let report = broker.shutdown().await;
            assert_eq!(report.quarantine.unwrap().canary, label);
        }
    }

    async fn unix_broker(dir: &Path, upstream: &str) -> Broker {
        Broker::start(BrokerConfig {
            routes: vec![route(Provider::OpenRouter, upstream, "openai/gpt-5")],
            listen: Listen::Unix(dir.to_owned()),
            max_request_bytes: 64 * 1024,
            max_cost_usd: None,
            log_path: None,
            canaries: Vec::new(),
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn bridge_relays_loopback_requests_to_broker_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let case = &cases()[0];
        let (upstream, mock) = mock_upstream(case.response).await;
        let broker = unix_broker(dir.path(), &upstream).await;
        let manifest = dir.path().join("manifest.json");
        broker.write_manifest(&manifest).unwrap();

        let bridge = Bridge::start(&manifest).await.unwrap();
        let env: std::collections::HashMap<_, _> = bridge.env().iter().cloned().collect();
        assert_eq!(env["OPENROUTER_API_KEY"], key(&broker));
        let host = &env["OPENROUTER_HOST"];
        assert!(host.starts_with("http://127.0.0.1:"), "{host}");
        let response = reqwest::Client::new()
            .post(format!("{host}{}", case.path))
            .bearer_auth(&env["OPENROUTER_API_KEY"])
            .json(&case.body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let _ = response.bytes().await.unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
        assert_eq!(broker.shutdown().await.usage.input_tokens, 1000);
    }

    #[tokio::test]
    async fn run_bridge_sets_route_env_and_returns_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let broker = unix_broker(dir.path(), "http://127.0.0.1:9").await;
        let manifest = dir.path().join("manifest.json");
        broker.write_manifest(&manifest).unwrap();
        let script = format!(
            r#"case "$OPENROUTER_HOST" in http://127.0.0.1:*) ;; *) exit 3;; esac
            test "$OPENROUTER_API_KEY" = "{}" || exit 4
            test -z "$OPENAI_HOST" || exit 5
            exit 7"#,
            key(&broker)
        );
        let status = run_bridge(
            &manifest,
            &["sh".into(), "-c".into(), script],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[tokio::test]
    async fn bridge_rejects_socket_paths_outside_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        for socket in ["../broker.sock", "/run/broker.sock", "..", ""] {
            let manifest = dir.path().join("manifest.json");
            std::fs::write(
                &manifest,
                serde_json::to_vec(&serde_json::json!({
                    "routes": [{ "provider": "openrouter", "socket": socket, "sandbox_key": "k" }]
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(Bridge::start(&manifest).await.is_err(), "{socket}");
        }
    }

    #[test]
    fn google_endpoint_requires_a_plain_model_name() {
        let post = Method::POST;
        assert_eq!(
            Provider::Google.endpoint(&post, "/v1beta/models/gemini-3:generateContent"),
            Some(ModelLocation::Path("gemini-3".into()))
        );
        for path in [
            "/v1beta/models/:generateContent",
            "/v1beta/models/a/b:generateContent",
            "/v1beta/models/gemini-3:countTokens",
            "/v1beta/tunedModels/x:generateContent",
        ] {
            assert_eq!(Provider::Google.endpoint(&post, path), None, "{path}");
        }
    }

    #[test]
    fn provider_names_round_trip() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_name(provider.name()).unwrap(), provider);
        }
        assert!(Provider::from_name("ollama").is_err());
    }

    #[tokio::test]
    async fn rejects_plain_http_and_credentialed_upstreams() {
        for upstream in [
            "http://openrouter.ai",
            "https://user:pass@openrouter.ai",
            "https://openrouter.ai/?key=x",
            "ftp://127.0.0.1",
        ] {
            assert!(upstream_client(upstream).await.is_err(), "{upstream}");
        }
    }

    #[test]
    fn credential_debug_is_redacted() {
        let route = route(Provider::OpenAi, "https://api.openai.com", "gpt-5");
        let debug = format!("{route:?}");
        assert!(!debug.contains("real-openai-key"));
        assert!(!debug.contains(route.sandbox_key.expose()));
    }
}
