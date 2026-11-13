use std::time::Duration;

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::config::WireApi;
use crate::error::{AttemptError, ErrorKind, GatewayError};
use crate::provider::{ProviderLease, QueueError};
use crate::state::AppState;
use crate::upstream::{chat, responses};

/// Executes one Responses request against the strict-priority provider chain.
///
/// The chain is walked from the highest priority provider downwards. A provider
/// is skipped when its circuit breaker is open, when its local queue rejects the
/// request, or when it fails before anything has been committed to the client.
pub async fn execute(
    state: &AppState,
    request: Value,
    requested_model: &str,
    client_wants_stream: bool,
    headers: &HeaderMap,
) -> Result<Response, GatewayError> {
    if request.get("previous_response_id").is_some() {
        return Err(GatewayError::bad_request(
            "`previous_response_id` is not supported because it cannot be replayed against a \
             different provider; send the full input history with `store: false` instead",
        ));
    }

    let chain = state.router.chain_for(requested_model);
    if chain.is_empty() {
        return Err(GatewayError::bad_request(format!(
            "no provider is configured for model `{requested_model}`"
        )));
    }

    let first_byte_timeout = Duration::from_millis(state.config.server.first_byte_timeout_ms);
    let idle_timeout = Duration::from_millis(state.config.server.stream_idle_timeout_ms);
    let timeouts = responses::Timeouts {
        first_byte: first_byte_timeout,
        idle: idle_timeout,
    };

    let mut attempts = Vec::new();
    let mut chat_json_skips = 0usize;

    for provider in chain {
        let provider_id = provider.config.id.clone();

        if !provider.begin_attempt() {
            attempts.push(AttemptError::new(
                provider_id.clone(),
                ErrorKind::Unavailable,
                "circuit breaker is open",
            ));
            continue;
        }

        let permit = match provider.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                let (kind, message) = match error {
                    QueueError::Full => (
                        ErrorKind::QueueFull,
                        "concurrency slots are busy and max_queue = 0".to_string(),
                    ),
                    QueueError::Timeout => (
                        ErrorKind::QueueTimeout,
                        format!(
                            "queue wait exceeded {} ms",
                            provider.config.queue_timeout_ms
                        ),
                    ),
                    QueueError::Closed => (
                        ErrorKind::Unavailable,
                        "provider queue is closed".to_string(),
                    ),
                };
                tracing::warn!(
                    provider = %provider_id,
                    kind = kind.as_str(),
                    %message,
                    "provider admission rejected request; trying next provider"
                );
                attempts.push(AttemptError::new(provider_id, kind, message));
                continue;
            }
        };

        let lease = ProviderLease::new(provider.clone(), permit);

        match provider.config.wire_api {
            WireApi::Responses => {
                let started = responses::start(
                    provider.clone(),
                    lease,
                    &request,
                    requested_model,
                    client_wants_stream,
                    headers,
                    timeouts,
                )
                .await;

                match started {
                    Ok(session) => {
                        tracing::info!(
                            provider = %provider_id,
                            model = requested_model,
                            stream = client_wants_stream,
                            "request committed to provider"
                        );
                        return Ok(session.into_response(client_wants_stream).await);
                    }
                    Err(error) => {
                        if !error.kind.should_failover() {
                            return Err(GatewayError::from_attempt(error));
                        }
                        tracing::warn!(
                            provider = %provider_id,
                            kind = error.kind.as_str(),
                            message = %error.message,
                            "provider failed before commit; trying next provider"
                        );
                        attempts.push(error);
                    }
                }
            }
            WireApi::Chat => {
                if !client_wants_stream {
                    // The Chat adapter currently only emits streaming Responses
                    // events. A later provider with a native Responses API can
                    // still serve this request.
                    chat_json_skips += 1;
                    attempts.push(AttemptError::new(
                        provider_id,
                        ErrorKind::Client,
                        "the Chat adapter currently requires `stream: true`; use a provider with \
                         `wire_api = \"responses\"` for non-streaming requests",
                    ));
                    continue;
                }

                let started = chat::start(
                    provider.clone(),
                    lease,
                    &request,
                    requested_model,
                    first_byte_timeout,
                    idle_timeout,
                )
                .await;

                match started {
                    Ok(session) => {
                        tracing::info!(
                            provider = %provider_id,
                            model = requested_model,
                            "request committed to Chat adapter"
                        );
                        return Ok(session.into_response());
                    }
                    Err(error) => {
                        if !error.kind.should_failover() {
                            return Err(GatewayError::from_attempt(error));
                        }
                        tracing::warn!(
                            provider = %provider_id,
                            kind = error.kind.as_str(),
                            message = %error.message,
                            "Chat provider failed before commit; trying next provider"
                        );
                        attempts.push(error);
                    }
                }
            }
        }
    }

    if chat_json_skips > 0 && chat_json_skips == attempts.len() {
        return Err(GatewayError::bad_request(
            "non-streaming requests require a provider with `wire_api = \"responses\"`",
        ));
    }

    Err(GatewayError::all_failed(attempts))
}
