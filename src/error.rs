use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    NotFound,
    RateLimit,
    ContextLength,
    ContentPolicy,
    Client,
    Server,
    Network,
    Timeout,
    QueueFull,
    QueueTimeout,
    Unavailable,
    Protocol,
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth_error",
            Self::NotFound => "not_found",
            Self::RateLimit => "rate_limit",
            Self::ContextLength => "context_length_exceeded",
            Self::ContentPolicy => "content_policy",
            Self::Client => "invalid_request",
            Self::Server => "upstream_server_error",
            Self::Network => "upstream_network_error",
            Self::Timeout => "upstream_timeout",
            Self::QueueFull => "queue_full",
            Self::QueueTimeout => "queue_timeout",
            Self::Unavailable => "provider_unavailable",
            Self::Protocol => "upstream_protocol_error",
            Self::Internal => "internal_error",
        }
    }

    pub fn cooldown_multiplier(self) -> u32 {
        match self {
            Self::Auth => 10,
            Self::NotFound => 5,
            Self::RateLimit => 2,
            _ => 1,
        }
    }

    /// Client-side request errors are deterministic: sending the same request to
    /// another provider would waste a request and hide the real problem.
    pub fn should_failover(self) -> bool {
        !matches!(
            self,
            Self::Client | Self::ContextLength | Self::ContentPolicy
        )
    }

    /// Local admission control and malformed requests say nothing about provider health.
    pub fn should_trip_breaker(self) -> bool {
        matches!(
            self,
            Self::Auth
                | Self::NotFound
                | Self::RateLimit
                | Self::Server
                | Self::Network
                | Self::Timeout
                | Self::Protocol
                | Self::Unavailable
        )
    }

    pub fn http_status(self) -> StatusCode {
        match self {
            Self::Auth => StatusCode::BAD_GATEWAY,
            Self::NotFound => StatusCode::BAD_GATEWAY,
            Self::RateLimit | Self::QueueFull | Self::QueueTimeout => StatusCode::TOO_MANY_REQUESTS,
            Self::ContextLength | Self::ContentPolicy | Self::Client => StatusCode::BAD_REQUEST,
            Self::Server | Self::Network | Self::Protocol | Self::Internal => {
                StatusCode::BAD_GATEWAY
            }
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttemptError {
    pub provider_id: String,
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl AttemptError {
    pub fn new(
        provider_id: impl Into<String>,
        kind: ErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            kind,
            status: None,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }
}

#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub attempts: Vec<AttemptError>,
    pub retry_after: Option<Duration>,
}

impl GatewayError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
            attempts: Vec::new(),
            retry_after: None,
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_api_key",
            message: "missing or invalid local API key".to_string(),
            attempts: Vec::new(),
            retry_after: None,
        }
    }

    pub fn payload_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: "request body is too large".to_string(),
            attempts: Vec::new(),
            retry_after: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
            attempts: Vec::new(),
            retry_after: None,
        }
    }

    /// A terminal upstream error that must be returned to the client instead of
    /// being retried against the next provider.
    pub fn from_attempt(attempt: AttemptError) -> Self {
        let status = attempt
            .status
            .and_then(|status| StatusCode::from_u16(status).ok())
            .filter(|status| status.is_client_error())
            .unwrap_or_else(|| attempt.kind.http_status());
        Self {
            status,
            code: attempt.kind.as_str(),
            message: format!(
                "provider `{}` rejected the request: {}",
                attempt.provider_id, attempt.message
            ),
            attempts: vec![attempt],
            retry_after: None,
        }
    }

    /// Failure after data has already been committed to the client. There is no
    /// safe failover point anymore, so this is always reported as a bad gateway.
    pub fn upstream_failed(attempt: AttemptError) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: attempt.kind.as_str(),
            message: format!(
                "provider `{}` failed: {}",
                attempt.provider_id, attempt.message
            ),
            attempts: vec![attempt],
            retry_after: None,
        }
    }

    pub fn all_failed(attempts: Vec<AttemptError>) -> Self {
        let has_busy = attempts.iter().any(|attempt| {
            matches!(
                attempt.kind,
                ErrorKind::RateLimit | ErrorKind::QueueFull | ErrorKind::QueueTimeout
            )
        });
        let has_timeout = attempts
            .iter()
            .any(|attempt| attempt.kind == ErrorKind::Timeout);
        let all_unavailable = !attempts.is_empty()
            && attempts
                .iter()
                .all(|attempt| attempt.kind == ErrorKind::Unavailable);
        let status = if has_busy {
            StatusCode::TOO_MANY_REQUESTS
        } else if has_timeout {
            StatusCode::GATEWAY_TIMEOUT
        } else if all_unavailable {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_GATEWAY
        };
        let code = if has_busy {
            "all_providers_busy"
        } else if has_timeout {
            "all_providers_timeout"
        } else if all_unavailable {
            "all_providers_unavailable"
        } else {
            "all_providers_failed"
        };

        let message = if attempts.is_empty() {
            "no provider is available for this model".to_string()
        } else {
            let summary = attempts
                .iter()
                .map(|attempt| format!("{}: {}", attempt.provider_id, attempt.message))
                .collect::<Vec<_>>()
                .join("; ");
            format!("all providers failed for this request ({summary})")
        };

        Self {
            status,
            code,
            message,
            attempts,
            retry_after: Some(Duration::from_secs(1)),
        }
    }

    pub fn attempts_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.attempts
                .iter()
                .map(|attempt| {
                    json!({
                        "provider": attempt.provider_id,
                        "kind": attempt.kind.as_str(),
                        "status": attempt.status,
                        "message": attempt.message,
                    })
                })
                .collect(),
        )
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        tracing::warn!(
            status = %self.status,
            code = self.code,
            attempts = %self.attempts_json(),
            "request failed"
        );

        let body = Json(json!({
            "error": {
                "message": self.message,
                "type": if self.status == StatusCode::UNAUTHORIZED { "authentication_error" } else { "gateway_error" },
                "param": null,
                "code": self.code,
            }
        }));

        let mut response = (self.status, body).into_response();
        if let Some(retry_after) = self.retry_after {
            let seconds = retry_after.as_secs().max(1).to_string();
            if let Ok(value) = HeaderValue::from_str(&seconds) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

pub fn classify_http_status(status: StatusCode, message: &str) -> ErrorKind {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::Auth,
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ErrorKind::Timeout,
        StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimit,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorKind::Client,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("context length")
                || lower.contains("context_length")
                || lower.contains("maximum context")
                || lower.contains("prompt is too long")
            {
                ErrorKind::ContextLength
            } else if lower.contains("content policy")
                || lower.contains("content management")
                || lower.contains("moderation")
            {
                ErrorKind::ContentPolicy
            } else {
                ErrorKind::Client
            }
        }
        status if status.is_server_error() => ErrorKind::Server,
        _ => ErrorKind::Client,
    }
}

pub fn parse_retry_after(headers: &axum::http::HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // RFC 7231 HTTP-date form is uncommon for model gateways; ignore it safely.
    None
}
