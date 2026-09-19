//! Shared, optional Jev transport through the standalone Jev SDK.
use std::{collections::HashMap, time::Duration};

use anyhow::{Result, bail};
use jev_sdk::{Question, RetryPolicy, SystemOneResponse, TypeSafeClient};
use serde::Serialize;
use serde_json::Value;

// Pin behavior and pricing together: https://docs.typesafe.ai/models (2026-09-18).
pub(crate) const MODEL: &str = "jev-1.13.0";
pub(crate) const INPUT_PRICE_PER_MILLION: f64 = 0.042;
// Byte bound, not a tokenizer: the API enforces its separate token limits.
pub(crate) const MAX_REQUEST_BYTES: usize = 96 * 1024;

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

pub(crate) fn client_from_env() -> Result<Option<TypeSafeClient>> {
    let Some(key) = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        return Ok(None);
    };
    // Explicit endpoint/model preserve the same behavior in host and sandbox.
    client(&key, "https://api.typesafe.ai").map(Some)
}

pub(crate) fn client(key: &str, base_url: &str) -> Result<TypeSafeClient> {
    Ok(TypeSafeClient::builder()
        .api_key(key)
        .base_url(base_url)
        .model(MODEL)
        .timeout(Duration::from_secs(10))
        // A timed-out request may still be billable. Let each caller fall back.
        .retry(RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        })
        .build()?)
}

pub(crate) async fn evaluate(
    client: &TypeSafeClient,
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
    let response = client.system_one(request.state, request.questions).await?;
    usage.peak_input_tokens = usage.peak_input_tokens.max(response.usage.input_tokens);
    usage.output_tokens += response.usage.output_tokens;
    usage.total_tokens += response.usage.input_tokens + response.usage.output_tokens;
    usage.cost_usd += response.usage.input_tokens as f64 * INPUT_PRICE_PER_MILLION / 1_000_000.0;
    if response.model != MODEL {
        bail!("Unexpected Jev response model");
    }
    Ok(response)
}
