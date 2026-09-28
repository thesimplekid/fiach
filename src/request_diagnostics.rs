//! Metadata-only diagnostics at the provider payload boundary. Never persist
//! request/response bodies, provider error text, headers, or credentials.
use std::error::Error;

use anyhow::Result;
use goose_providers::request_log::{self, RequestLogHandle, RequestLogger};
use serde_json::Value;

pub fn install() -> Result<()> {
    request_log::install_logger(MetadataLogger)?;
    Ok(())
}

struct MetadataLogger;
struct MetadataHandle {
    span: tracing::Span,
}

type LogResult<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

impl RequestLogger for MetadataLogger {
    fn start(&self) -> LogResult<Box<dyn RequestLogHandle>> {
        Ok(Box::new(MetadataHandle {
            span: tracing::Span::current(),
        }))
    }
}

impl RequestLogHandle for MetadataHandle {
    fn write(&mut self, line: &str) -> LogResult<()> {
        // Parsing failure must neither leak the line nor fail an LLM request.
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Ok(());
        };
        let span = self.span.clone();
        let _entered = span.enter();
        if let Some(input) = value.get("input") {
            let model = input.get("model").and_then(Value::as_str).or_else(|| {
                value
                    .pointer("/model_config/model_name")
                    .and_then(Value::as_str)
            });
            let output = output_limit(input);
            let input_bytes = serde_json::to_vec(input).map_or(0, |bytes| bytes.len());
            // Byte count is the serialized payload, including tools and settings;
            // it is deliberately not reported as a tokenizer estimate.
            self.span = tracing::info_span!(
                "provider_request",
                model,
                requested_output_tokens = output,
                output_limit_sent = output.is_some(),
                input_bytes,
                input_size_kind = "serialized_payload_bytes"
            );
            tracing::info!(parent: &self.span, "Provider request started");
        } else if let Some(error) = value.get("error").and_then(Value::as_str) {
            tracing::warn!(
                failure_kind = failure_kind(error),
                "Provider request failed"
            );
        }
        Ok(())
    }
}

fn output_limit(input: &Value) -> Option<u64> {
    [
        "/max_completion_tokens",
        "/max_tokens",
        "/max_output_tokens",
        "/generationConfig/maxOutputTokens",
    ]
    .into_iter()
    .find_map(|key| input.pointer(key).and_then(Value::as_u64))
}

/// Conservative labels: ambiguous token-limit errors stay ambiguous.
pub(crate) fn failure_kind(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("context length")
        || message.contains("context_length")
        || message.contains("input too long")
    {
        "input_context_limit"
    } else if (message.contains("max_tokens")
        || message.contains("max_output_tokens")
        || message.contains("max_completion_tokens"))
        && (message.contains("output")
            || message.contains("must be")
            || message.contains("at most"))
    {
        "output_limit"
    } else if message.contains("token") && (message.contains("exceed") || message.contains("limit"))
    {
        "token_limit_unspecified"
    } else if is_bad_request(&message) {
        "invalid_request"
    } else {
        "provider_error"
    }
}

pub(crate) fn is_bad_request(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "http 400",
        "status 400",
        "status: 400",
        "(400)",
        "http 422",
        "status 422",
        "status: 422",
        "(422)",
    ]
    .iter()
    .any(|marker| {
        message.match_indices(marker).any(|(offset, _)| {
            message
                .as_bytes()
                .get(offset + marker.len())
                .is_none_or(|next| !next.is_ascii_digit())
        })
    })
}

pub(crate) fn permanent_rejection(error: &anyhow::Error) -> bool {
    use goose_providers::errors::ProviderError;
    if let Some(provider_error) = error.downcast_ref::<ProviderError>() {
        return match provider_error {
            ProviderError::InvalidValue(_) | ProviderError::ContextLengthExceeded(_) => true,
            ProviderError::RequestFailed(message) => is_bad_request(message),
            _ => false,
        };
    }
    error
        .chain()
        .any(|cause| is_bad_request(&cause.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_metadata_and_correlation_without_body_or_credentials() {
        use std::{
            io::Write,
            sync::{Arc, Mutex},
        };
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || Writer(sink.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "review",
                provider = "openrouter",
                pr = 42,
                stage = "verifier"
            );
            let mut handle = span.in_scope(|| MetadataLogger.start().unwrap());
            handle
                .write(
                    &serde_json::json!({
                        "model_config": {"request_headers": {"Authorization": "secret-key"}},
                        "input": {"model": "test-model", "max_tokens": 8192,
                            "messages": [{"content": "private-prompt"}]}
                    })
                    .to_string(),
                )
                .unwrap();
            handle
                .write(r#"{"error":"Bad request (400): private-prompt secret-key"}"#)
                .unwrap();
            handle
                .write(r#"{"data":{"content":"private-response"}}"#)
                .unwrap();
        });
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        for field in [
            "openrouter",
            "test-model",
            "pr=42",
            "verifier",
            "requested_output_tokens=8192",
            "input_bytes=",
            "invalid_request",
        ] {
            assert!(output.contains(field), "missing {field}: {output}");
        }
        for secret in [
            "private-prompt",
            "secret-key",
            "private-response",
            "Authorization",
        ] {
            assert!(!output.contains(secret));
        }
    }

    #[test]
    fn distinguish_explicit_limits_without_guessing() {
        assert_eq!(
            failure_kind("400 max_tokens must be at most 8192"),
            "output_limit"
        );
        assert_eq!(
            failure_kind("maximum context length exceeded"),
            "input_context_limit"
        );
        assert_eq!(
            failure_kind("max_tokens_exceeded"),
            "token_limit_unspecified"
        );
        assert_eq!(
            failure_kind("Bad request (400): invalid schema"),
            "invalid_request"
        );
        assert_eq!(
            output_limit(&serde_json::json!({"max_completion_tokens": 16384})),
            Some(16384)
        );
        assert_eq!(output_limit(&serde_json::json!({})), None);
    }

    #[test]
    fn wrapped_bad_requests_are_terminal_but_transient_errors_are_not() {
        let error = anyhow::anyhow!("Bad request (400): invalid parameter").context("screening");
        assert!(permanent_rejection(&error));
        let rate_limit = goose_providers::errors::ProviderError::RateLimitExceeded {
            details: "HTTP 429 after an earlier HTTP 400".into(),
            retry_delay: None,
        };
        assert!(!permanent_rejection(&anyhow::Error::new(rate_limit)));
        for message in ["HTTP 429", "HTTP 503", "connection reset", "status 40012"] {
            assert!(!permanent_rejection(&anyhow::anyhow!(message)));
        }
    }
}
