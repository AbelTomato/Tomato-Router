use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::error::{
    AttemptError, ErrorKind, GatewayError, classify_http_status, parse_retry_after,
};
use crate::protocol::sse::{SseEvent, SseParser};
use crate::provider::{Provider, ProviderLease};

/// Codex sends these headers to OpenAI. Relays commonly use them for sticky
/// sessions, prompt-cache sharding and tracing, so forward only this allowlist.
const FORWARD_HEADERS: &[&str] = &[
    "openai-beta",
    "session-id",
    "thread-id",
    "originator",
    "x-codex-turn-metadata",
];

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub first_byte: Duration,
    pub idle: Duration,
}

pub struct ResponsesSession {
    lease: ProviderLease,
    provider_id: String,
    upstream_model: String,
    source: Source,
    idle: Duration,
}

enum Source {
    Json(Value),
    Stream {
        prefix: Vec<u8>,
        upstream: ByteStream,
    },
}

pub async fn start(
    provider: Arc<Provider>,
    lease: ProviderLease,
    request: &Value,
    requested_model: &str,
    client_wants_stream: bool,
    forward: &HeaderMap,
    timeouts: Timeouts,
) -> Result<ResponsesSession, AttemptError> {
    if request.as_object().is_none() {
        lease.failure(ErrorKind::Client);
        return Err(AttemptError::new(
            provider.config.id.clone(),
            ErrorKind::Client,
            "request body must be a JSON object",
        ));
    }

    let upstream_model = provider.config.model_for(requested_model);
    let mut body = request.clone();
    body["model"] = Value::String(upstream_model.clone());
    body["stream"] = Value::Bool(client_wants_stream);

    let mut builder = provider
        .request(Method::POST, "responses")
        .header(
            reqwest::header::ACCEPT,
            if client_wants_stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .json(&body);
    for name in FORWARD_HEADERS {
        if let Some(value) = forward.get(*name)
            && let Ok(value) = value.to_str()
        {
            builder = builder.header(*name, value);
        }
    }

    let response = match timeout(timeouts.first_byte, builder.send()).await {
        Err(_) => {
            lease.failure(ErrorKind::Timeout);
            return Err(AttemptError::new(
                provider.config.id.clone(),
                ErrorKind::Timeout,
                "timed out waiting for upstream response headers",
            ));
        }
        Ok(Err(error)) => {
            let kind = if error.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Network
            };
            lease.failure(kind);
            return Err(AttemptError::new(
                provider.config.id.clone(),
                kind,
                format!("upstream request failed: {error}"),
            ));
        }
        Ok(Ok(response)) => response,
    };

    let status = response.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(response.headers());
        let body = match timeout(timeouts.first_byte, response.text()).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => format!("<failed to read upstream error body: {error}>"),
            Err(_) => "<timed out reading upstream error body>".to_string(),
        };
        let message = upstream_error_message(status.as_u16(), &body);
        let kind = classify_http_status(status, &message);
        lease.failure(kind);
        return Err(AttemptError::new(provider.config.id.clone(), kind, message)
            .with_status(status.as_u16())
            .with_retry_after(retry_after));
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let wants_sse = content_type.contains("text/event-stream")
        || (!content_type.contains("json") && client_wants_stream);

    if wants_sse {
        let mut upstream: ByteStream = Box::pin(response.bytes_stream());
        let mut parser = SseParser::new();
        let mut prefix = Vec::new();
        let deadline = tokio::time::Instant::now() + timeouts.first_byte;
        let mut first_events = Vec::new();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                lease.failure(ErrorKind::Timeout);
                return Err(AttemptError::new(
                    provider.config.id.clone(),
                    ErrorKind::Timeout,
                    "timed out waiting for the first upstream SSE event",
                ));
            }

            match timeout(remaining, upstream.next()).await {
                Err(_) => {
                    lease.failure(ErrorKind::Timeout);
                    return Err(AttemptError::new(
                        provider.config.id.clone(),
                        ErrorKind::Timeout,
                        "timed out waiting for the first upstream SSE event",
                    ));
                }
                Ok(None) => {
                    lease.failure(ErrorKind::Protocol);
                    return Err(AttemptError::new(
                        provider.config.id.clone(),
                        ErrorKind::Protocol,
                        "upstream closed before sending an SSE event",
                    ));
                }
                Ok(Some(Err(error))) => {
                    let kind = if error.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Network
                    };
                    lease.failure(kind);
                    return Err(AttemptError::new(
                        provider.config.id.clone(),
                        kind,
                        format!("upstream stream failed before first event: {error}"),
                    ));
                }
                Ok(Some(Ok(bytes))) => {
                    prefix.extend_from_slice(&bytes);
                    first_events.extend(parser.push(&bytes));
                    if !first_events.is_empty() {
                        break;
                    }
                }
            }
        }

        if let Some((kind, message)) = first_event_failure(&first_events) {
            lease.failure(kind);
            return Err(AttemptError::new(provider.config.id.clone(), kind, message));
        }

        return Ok(ResponsesSession {
            lease,
            provider_id: provider.config.id.clone(),
            upstream_model,
            source: Source::Stream { prefix, upstream },
            idle: timeouts.idle,
        });
    }

    let text = match timeout(timeouts.first_byte, response.text()).await {
        Err(_) => {
            lease.failure(ErrorKind::Timeout);
            return Err(AttemptError::new(
                provider.config.id.clone(),
                ErrorKind::Timeout,
                "timed out reading the upstream JSON response",
            ));
        }
        Ok(Err(error)) => {
            let kind = if error.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Network
            };
            lease.failure(kind);
            return Err(AttemptError::new(
                provider.config.id.clone(),
                kind,
                format!("failed to read upstream response: {error}"),
            ));
        }
        Ok(Ok(text)) => text,
    };

    match serde_json::from_str::<Value>(&text) {
        Ok(value) => {
            if let Some((kind, message)) = json_failure(&value) {
                lease.failure(kind);
                return Err(AttemptError::new(provider.config.id.clone(), kind, message));
            }
            Ok(ResponsesSession {
                lease,
                provider_id: provider.config.id.clone(),
                upstream_model,
                source: Source::Json(value),
                idle: timeouts.idle,
            })
        }
        Err(_) => {
            // Some relays answer a streaming request with SSE but omit the
            // content type. Replay the already-buffered bytes as an SSE stream.
            let mut parser = SseParser::new();
            let mut events = parser.push(text.as_bytes());
            events.extend(parser.finish());
            if events.is_empty() {
                lease.failure(ErrorKind::Protocol);
                return Err(AttemptError::new(
                    provider.config.id.clone(),
                    ErrorKind::Protocol,
                    format!("upstream returned a non-JSON body: {}", preview(&text)),
                ));
            }
            if let Some((kind, message)) = first_event_failure(&events) {
                lease.failure(kind);
                return Err(AttemptError::new(provider.config.id.clone(), kind, message));
            }
            Ok(ResponsesSession {
                lease,
                provider_id: provider.config.id.clone(),
                upstream_model,
                source: Source::Stream {
                    prefix: text.into_bytes(),
                    upstream: Box::pin(futures_util::stream::empty()),
                },
                idle: timeouts.idle,
            })
        }
    }
}

impl ResponsesSession {
    pub async fn into_response(self, client_wants_stream: bool) -> Response {
        let Self {
            lease,
            provider_id,
            upstream_model,
            source,
            idle,
        } = self;

        let mut response = if client_wants_stream {
            match source {
                Source::Json(value) => synthesize_sse(value),
                Source::Stream { prefix, upstream } => {
                    passthrough_sse(prefix, upstream, lease, idle)
                }
            }
        } else {
            match source {
                Source::Json(value) => axum::Json(value).into_response(),
                Source::Stream { prefix, upstream } => {
                    collect_json(prefix, upstream, lease, idle).await
                }
            }
        };

        insert_header(&mut response, "x-tomato-provider", &provider_id);
        insert_header(&mut response, "x-tomato-upstream-model", &upstream_model);
        response
    }
}

fn passthrough_sse(
    prefix: Vec<u8>,
    mut upstream: ByteStream,
    lease: ProviderLease,
    idle: Duration,
) -> Response {
    let event_stream = stream! {
        let mut parser = SseParser::new();
        let mut relay = RelayState::default();

        for event in relay.process(parser.push(&prefix)) {
            yield Ok::<Event, Infallible>(event);
        }

        while relay.terminal.is_none() {
            match timeout(idle, upstream.next()).await {
                Err(_) => {
                    relay.failure_kind = ErrorKind::Timeout;
                    yield Ok(error_event("upstream stream idle timeout", "upstream_timeout"));
                    relay.terminal = Some(false);
                }
                Ok(None) => {
                    for event in relay.process(parser.finish()) {
                        yield Ok(event);
                    }
                    if relay.terminal.is_none() {
                        relay.failure_kind = ErrorKind::Protocol;
                        yield Ok(error_event(
                            "upstream stream ended before response.completed",
                            "upstream_protocol_error",
                        ));
                        relay.terminal = Some(false);
                    }
                }
                Ok(Some(Err(error))) => {
                    relay.failure_kind = if error.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Network
                    };
                    yield Ok(error_event(
                        &format!("upstream stream failed: {error}"),
                        relay.failure_kind.as_str(),
                    ));
                    relay.terminal = Some(false);
                }
                Ok(Some(Ok(bytes))) => {
                    for event in relay.process(parser.push(&bytes)) {
                        yield Ok(event);
                    }
                }
            }
        }

        if relay.terminal == Some(true) {
            lease.success();
        } else {
            lease.failure(relay.failure_kind);
        }
    };

    let sse = Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keepalive"),
    );
    sse.into_response()
}

async fn collect_json(
    prefix: Vec<u8>,
    mut upstream: ByteStream,
    lease: ProviderLease,
    idle: Duration,
) -> Response {
    let provider_id = lease.provider.config.id.clone();
    let mut parser = SseParser::new();
    let mut final_response: Option<Value> = None;
    let mut failure: Option<(ErrorKind, String)> = None;

    let process = |events: Vec<SseEvent>,
                   final_response: &mut Option<Value>,
                   failure: &mut Option<(ErrorKind, String)>| {
        for event in events {
            if event.is_done() || final_response.is_some() || failure.is_some() {
                continue;
            }
            match event.event_type().as_deref() {
                Some("response.completed" | "response.incomplete") => {
                    if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                        *final_response = Some(value.get("response").cloned().unwrap_or(value));
                    } else {
                        *failure = Some((
                            ErrorKind::Protocol,
                            "invalid JSON in terminal response event".to_string(),
                        ));
                    }
                }
                Some("response.failed" | "error") => {
                    *failure = Some((event_failure_kind(&event), event_failure_message(&event)));
                }
                _ => {}
            }
        }
    };

    process(parser.push(&prefix), &mut final_response, &mut failure);

    while final_response.is_none() && failure.is_none() {
        match timeout(idle, upstream.next()).await {
            Err(_) => {
                failure = Some((
                    ErrorKind::Timeout,
                    "upstream stream idle timeout".to_string(),
                ));
            }
            Ok(None) => {
                process(parser.finish(), &mut final_response, &mut failure);
                if final_response.is_none() && failure.is_none() {
                    failure = Some((
                        ErrorKind::Protocol,
                        "upstream stream ended before response.completed".to_string(),
                    ));
                }
            }
            Ok(Some(Err(error))) => {
                failure = Some((
                    if error.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Network
                    },
                    format!("upstream stream failed: {error}"),
                ));
            }
            Ok(Some(Ok(bytes))) => {
                process(parser.push(&bytes), &mut final_response, &mut failure);
            }
        }
    }

    match final_response {
        Some(value) => {
            lease.success();
            axum::Json(value).into_response()
        }
        None => {
            let (kind, message) = failure.unwrap_or((
                ErrorKind::Protocol,
                "upstream stream ended without a terminal event".to_string(),
            ));
            lease.failure(kind);
            GatewayError::upstream_failed(AttemptError::new(provider_id, kind, message))
                .into_response()
        }
    }
}

struct RelayState {
    terminal: Option<bool>,
    failure_kind: ErrorKind,
}

impl Default for RelayState {
    fn default() -> Self {
        Self {
            terminal: None,
            failure_kind: ErrorKind::Protocol,
        }
    }
}

impl RelayState {
    fn process(&mut self, events: Vec<SseEvent>) -> Vec<Event> {
        let mut output = Vec::new();
        for event in events {
            if event.is_done() {
                continue;
            }
            let event_type = event.event_type();
            match event_type.as_deref() {
                Some("response.completed" | "response.incomplete") => {
                    self.terminal = Some(true);
                }
                Some("response.failed" | "error") => {
                    self.terminal = Some(false);
                    self.failure_kind = event_failure_kind(&event);
                }
                _ => {}
            }
            if let Some(event) = to_sse_event(&event) {
                output.push(event);
            }
            if self.terminal.is_some() {
                break;
            }
        }
        output
    }
}

fn synthesize_sse(value: Value) -> Response {
    let events = synthesize_events(value);
    let stream = futures_util::stream::iter(events.into_iter().map(Ok::<_, Infallible>));
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .text("keepalive"),
        )
        .into_response()
}

fn synthesize_events(value: Value) -> Vec<Event> {
    if value.get("object").and_then(Value::as_str) != Some("response")
        || value.get("error").is_some()
    {
        return vec![error_event(
            &json_failure_message(&value),
            "upstream_protocol_error",
        )];
    }

    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let terminal_name = match status {
        "failed" => "response.failed",
        "incomplete" => "response.incomplete",
        _ => "response.completed",
    };

    let mut created = value.clone();
    if let Some(object) = created.as_object_mut() {
        object.insert(
            "status".to_string(),
            Value::String("in_progress".to_string()),
        );
        object.insert("output".to_string(), json!([]));
        object.insert("usage".to_string(), Value::Null);
    }

    vec![
        Event::default().event("response.created").data(
            json!({
                "type": "response.created",
                "response": created,
            })
            .to_string(),
        ),
        Event::default().event(terminal_name).data(
            json!({
                "type": terminal_name,
                "response": value,
            })
            .to_string(),
        ),
    ]
}

fn to_sse_event(event: &SseEvent) -> Option<Event> {
    if event.is_done() || event.data.is_empty() {
        return None;
    }
    Some(match event.event_type() {
        Some(name) => Event::default().event(name).data(event.data.clone()),
        None => Event::default().data(event.data.clone()),
    })
}

fn error_event(message: &str, code: &str) -> Event {
    Event::default().event("error").data(
        json!({
            "type": "error",
            "code": code,
            "message": message,
            "param": null,
        })
        .to_string(),
    )
}

fn first_event_failure(events: &[SseEvent]) -> Option<(ErrorKind, String)> {
    for event in events {
        if event.is_done() {
            continue;
        }
        if let Some("response.failed" | "error") = event.event_type().as_deref() {
            return Some((event_failure_kind(event), event_failure_message(event)));
        }
    }
    if !events.is_empty() && events.iter().all(SseEvent::is_done) {
        return Some((
            ErrorKind::Protocol,
            "upstream sent [DONE] before any response event".to_string(),
        ));
    }
    None
}

fn json_failure(value: &Value) -> Option<(ErrorKind, String)> {
    if let Some(error) = value.get("error")
        && !error.is_null()
    {
        let message = json_failure_message(value);
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| error.get("type").and_then(Value::as_str))
            .unwrap_or("");
        return Some((classify_error_code(code), message));
    }
    if value.get("object").and_then(Value::as_str) == Some("response")
        && value.get("status").and_then(Value::as_str) == Some("failed")
    {
        return Some((
            ErrorKind::Server,
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("upstream response failed")
                .to_string(),
        ));
    }
    None
}

fn json_failure_message(value: &Value) -> String {
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("error").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| preview(&value.to_string()))
}

fn event_failure_kind(event: &SseEvent) -> ErrorKind {
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return ErrorKind::Protocol;
    };
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/response/error/code")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    classify_error_code(code)
}

fn event_failure_message(event: &SseEvent) -> String {
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return preview(&event.data);
    };
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/response/error/message")
                .and_then(Value::as_str)
        })
        .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| preview(&event.data))
}

fn classify_error_code(code: &str) -> ErrorKind {
    let lower = code.to_ascii_lowercase();
    if lower.contains("rate_limit") {
        ErrorKind::RateLimit
    } else if lower.contains("invalid_request")
        || lower.contains("context_length")
        || lower.contains("content_policy")
    {
        ErrorKind::Client
    } else if lower.contains("timeout") {
        ErrorKind::Timeout
    } else {
        ErrorKind::Server
    }
}

fn upstream_error_message(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Some(message) = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
    {
        return message.to_string();
    }
    let preview = preview(trimmed);
    if preview.is_empty() {
        format!("upstream returned HTTP {status}")
    } else {
        format!("upstream returned HTTP {status}: {preview}")
    }
}

fn preview(value: &str) -> String {
    value.chars().take(2_000).collect()
}

fn insert_header(response: &mut Response, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}
