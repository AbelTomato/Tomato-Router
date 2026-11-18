use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::stream;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::time::timeout;
use uuid::Uuid;

use crate::error::{AttemptError, ErrorKind, classify_http_status, parse_retry_after};
use crate::protocol::messages::{build_messages_request, encode_reasoning_blocks};
use crate::protocol::sse::{SseEvent, SseParser};
use crate::protocol::tools::{ToolKind, ToolRoute, ToolRoutes, custom_input};
use crate::provider::{Provider, ProviderLease};

pub struct MessagesSession {
    lease: ProviderLease,
    upstream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    pending: VecDeque<SseEvent>,
    response_id: String,
    created_at: i64,
    requested_model: String,
    upstream_model: String,
    tool_routes: ToolRoutes,
    idle_timeout: Duration,
    post_stop_timeout: Duration,
}

pub async fn start(
    provider: Arc<Provider>,
    lease: ProviderLease,
    request: &Value,
    requested_model: &str,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> Result<MessagesSession, AttemptError> {
    let build = match build_messages_request(request, &provider.config) {
        Ok(build) => build,
        Err(message) => {
            lease.failure(ErrorKind::Client);
            return Err(AttemptError::new(
                provider.config.id.clone(),
                ErrorKind::Client,
                message,
            ));
        }
    };

    let mut builder = provider
        .request(Method::POST, messages_path(&provider))
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if !provider
        .config
        .headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("anthropic-version"))
    {
        builder = builder.header("anthropic-version", "2023-06-01");
    }

    let send_result = timeout(first_byte_timeout, builder.json(&build.body).send()).await;

    let response = match send_result {
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
        let body = match timeout(first_byte_timeout, response.text()).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => format!("<failed to read upstream error body: {error}>"),
            Err(_) => "<timed out reading upstream error body>".to_string(),
        };
        let (kind, message) = describe_http_error(status, &body);
        lease.failure(kind);
        return Err(AttemptError::new(provider.config.id.clone(), kind, message)
            .with_status(status.as_u16())
            .with_retry_after(retry_after));
    }

    let mut upstream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(response.bytes_stream());
    let mut parser = SseParser::new();
    let mut pending = VecDeque::new();
    let deadline = tokio::time::Instant::now() + first_byte_timeout;

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
                pending.extend(parser.push(&bytes));
                if !pending.is_empty() {
                    break;
                }
            }
        }
    }

    if let Some(first) = pending.front()
        && let Some((kind, message)) = event_error(first)
    {
        lease.failure(kind);
        return Err(AttemptError::new(provider.config.id.clone(), kind, message));
    }

    Ok(MessagesSession {
        lease,
        upstream,
        parser,
        pending,
        response_id: format!("resp_{}", Uuid::new_v4().simple()),
        created_at: unix_timestamp(),
        requested_model: requested_model.to_string(),
        upstream_model: build.upstream_model,
        tool_routes: build.tool_routes,
        idle_timeout,
        post_stop_timeout: Duration::from_secs(5),
    })
}

impl MessagesSession {
    pub fn into_response(self) -> Response {
        let MessagesSession {
            lease,
            mut upstream,
            mut parser,
            mut pending,
            response_id,
            created_at,
            requested_model,
            upstream_model,
            tool_routes,
            idle_timeout,
            post_stop_timeout,
        } = self;

        let provider_id = lease.provider.config.id.clone();
        let upstream_model_header = upstream_model.clone();

        let event_stream = stream! {
            let mut translator = MessagesTranslator::new(
                response_id,
                created_at,
                requested_model,
                tool_routes,
            );

            yield Ok::<Event, Infallible>(translator.created_event());

            for event in pending.drain(..) {
                for output in translator.process_event(&event) {
                    yield Ok(output);
                }
                if translator.is_terminal() {
                    break;
                }
            }

            while !translator.is_terminal() {
                let wait = if translator.stop_reason.is_some() {
                    post_stop_timeout
                } else {
                    idle_timeout
                };

                match timeout(wait, upstream.next()).await {
                    Err(_) => {
                        for output in translator.fail(
                            "upstream stream idle timeout",
                            ErrorKind::Timeout,
                        ) {
                            yield Ok(output);
                        }
                        break;
                    }
                    Ok(None) => {
                        for event in parser.finish() {
                            for output in translator.process_event(&event) {
                                yield Ok(output);
                            }
                        }
                        for output in translator.finish_or_fail() {
                            yield Ok(output);
                        }
                        break;
                    }
                    Ok(Some(Err(error))) => {
                        for output in translator.fail(
                            format!("upstream stream failed: {error}"),
                            if error.is_timeout() {
                                ErrorKind::Timeout
                            } else {
                                ErrorKind::Network
                            },
                        ) {
                            yield Ok(output);
                        }
                        break;
                    }
                    Ok(Some(Ok(bytes))) => {
                        for event in parser.push(&bytes) {
                            for output in translator.process_event(&event) {
                                yield Ok(output);
                            }
                            if translator.is_terminal() {
                                break;
                            }
                        }
                    }
                }
            }

            if translator.completed {
                lease.success();
            } else {
                lease.failure(translator.error_kind.unwrap_or(ErrorKind::Protocol));
            }
        };

        let sse = Sse::new(event_stream).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .text("keepalive"),
        );
        let mut response = sse.into_response();
        insert_header(&mut response, "x-tomato-provider", &provider_id);
        insert_header(
            &mut response,
            "x-tomato-upstream-model",
            &upstream_model_header,
        );
        response
    }
}

struct TextState {
    output_index: u64,
    item_id: String,
    text: String,
}

struct ThinkingState {
    output_index: u64,
    item_id: String,
    text: String,
    signature: String,
    redacted: Option<String>,
}

struct ToolState {
    output_index: u64,
    item_id: String,
    call_id: String,
    upstream_name: String,
    arguments: String,
    route: Option<ToolRoute>,
}

enum BlockState {
    Text(TextState),
    Thinking(ThinkingState),
    Tool(ToolState),
    Ignored,
}

#[derive(Debug, Clone, Copy, Default)]
struct AnthropicUsage {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
}

impl AnthropicUsage {
    fn apply(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input = value;
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation = value;
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output = value;
        }
    }

    fn to_responses(self) -> Value {
        let input = self.input + self.cache_creation + self.cache_read;
        json!({
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": self.cache_read},
            "output_tokens": self.output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input + self.output,
        })
    }
}

struct MessagesTranslator {
    response_id: String,
    created_at: i64,
    model: String,
    tool_routes: ToolRoutes,
    next_output_index: u64,
    blocks: BTreeMap<u64, BlockState>,
    output: Vec<(u64, Value)>,
    usage: AnthropicUsage,
    usage_seen: bool,
    stop_reason: Option<String>,
    terminal: bool,
    completed: bool,
    error_kind: Option<ErrorKind>,
}

impl MessagesTranslator {
    fn new(response_id: String, created_at: i64, model: String, tool_routes: ToolRoutes) -> Self {
        Self {
            response_id,
            created_at,
            model,
            tool_routes,
            next_output_index: 0,
            blocks: BTreeMap::new(),
            output: Vec::new(),
            usage: AnthropicUsage::default(),
            usage_seen: false,
            stop_reason: None,
            terminal: false,
            completed: false,
            error_kind: None,
        }
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn created_event(&self) -> Event {
        event(
            "response.created",
            json!({
                "type": "response.created",
                "response": self.response_object("in_progress", Vec::new(), None, None)
            }),
        )
    }

    fn process_event(&mut self, event: &SseEvent) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        if event.is_done() {
            return self.finish_or_fail();
        }
        let value: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(error) => {
                return self.fail(
                    format!("invalid JSON in upstream SSE event: {error}"),
                    ErrorKind::Protocol,
                );
            }
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or("");

        match event_type {
            "message_start" => self.message_start(&value),
            "content_block_start" => self.content_block_start(&value),
            "content_block_delta" => self.content_block_delta(&value),
            "content_block_stop" => self.content_block_stop(&value),
            "message_delta" => self.message_delta(&value),
            "message_stop" => self.finalize(),
            "ping" => Vec::new(),
            "error" => {
                let (kind, message) = describe_anthropic_error(&value, None);
                self.fail(message, kind)
            }
            _ => {
                if value.get("error").is_some() {
                    let (kind, message) = describe_anthropic_error(&value, None);
                    self.fail(message, kind)
                } else {
                    tracing::debug!(event = event_type, "ignoring unknown Messages SSE event");
                    Vec::new()
                }
            }
        }
    }

    fn message_start(&mut self, value: &Value) -> Vec<Event> {
        if let Some(usage) = value.pointer("/message/usage") {
            self.usage.apply(usage);
            self.usage_seen = true;
        }
        Vec::new()
    }

    fn message_delta(&mut self, value: &Value) -> Vec<Event> {
        if let Some(usage) = value.get("usage") {
            self.usage.apply(usage);
            self.usage_seen = true;
        }
        if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_string());
        }
        Vec::new()
    }

    fn content_block_start(&mut self, value: &Value) -> Vec<Event> {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(block) = value.get("content_block") else {
            return Vec::new();
        };
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
        let output_index = self.next_output_index;
        self.next_output_index += 1;

        match block_type {
            "text" => {
                let item_id = format!("msg_{}", Uuid::new_v4().simple());
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut events = vec![
                    event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": {
                                "id": item_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": []
                            }
                        }),
                    ),
                    event(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": []
                            }
                        }),
                    ),
                ];
                if !text.is_empty() {
                    events.push(event(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "delta": text,
                        }),
                    ));
                }
                self.blocks.insert(
                    index,
                    BlockState::Text(TextState {
                        output_index,
                        item_id,
                        text,
                    }),
                );
                events
            }
            "thinking" => {
                let item_id = format!("rs_{}", Uuid::new_v4().simple());
                let text = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut events = reasoning_started(output_index, &item_id);
                if !text.is_empty() {
                    events.push(reasoning_delta(output_index, &item_id, &text));
                }
                self.blocks.insert(
                    index,
                    BlockState::Thinking(ThinkingState {
                        output_index,
                        item_id,
                        text,
                        signature: String::new(),
                        redacted: None,
                    }),
                );
                events
            }
            "redacted_thinking" => {
                let item_id = format!("rs_{}", Uuid::new_v4().simple());
                let redacted = block
                    .get("data")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let events = reasoning_started(output_index, &item_id);
                self.blocks.insert(
                    index,
                    BlockState::Thinking(ThinkingState {
                        output_index,
                        item_id,
                        text: String::new(),
                        signature: String::new(),
                        redacted,
                    }),
                );
                events
            }
            "tool_use" => {
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let upstream_name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let route = self.tool_routes.resolve(&upstream_name).cloned();
                let kind = route
                    .as_ref()
                    .map(|route| route.kind)
                    .unwrap_or(ToolKind::Function);
                let item_id = match kind {
                    ToolKind::Function => format!("fc_{}", Uuid::new_v4().simple()),
                    ToolKind::Custom => format!("ctc_{}", Uuid::new_v4().simple()),
                };
                let original_name = route
                    .as_ref()
                    .map(|route| route.original_name.clone())
                    .unwrap_or_else(|| upstream_name.clone());
                let mut item = json!({
                    "id": item_id,
                    "type": if kind == ToolKind::Custom {
                        "custom_tool_call"
                    } else {
                        "function_call"
                    },
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": original_name,
                });
                if let Some(namespace) = route.as_ref().and_then(|route| route.namespace.clone()) {
                    item["namespace"] = Value::String(namespace);
                }
                if kind == ToolKind::Custom {
                    item["input"] = Value::String(String::new());
                } else {
                    item["arguments"] = Value::String(String::new());
                }
                let events = vec![event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": item,
                    }),
                )];
                self.blocks.insert(
                    index,
                    BlockState::Tool(ToolState {
                        output_index,
                        item_id,
                        call_id,
                        upstream_name,
                        arguments: String::new(),
                        route,
                    }),
                );
                events
            }
            other => {
                tracing::debug!(block_type = other, "ignoring unsupported Messages block");
                self.blocks.insert(index, BlockState::Ignored);
                Vec::new()
            }
        }
    }

    fn content_block_delta(&mut self, value: &Value) -> Vec<Event> {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(delta) = value.get("delta") else {
            return Vec::new();
        };
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(block) = self.blocks.get_mut(&index) else {
            return Vec::new();
        };

        match block {
            BlockState::Text(state) if delta_type == "text_delta" => {
                let Some(text) = delta.get("text").and_then(Value::as_str) else {
                    return Vec::new();
                };
                state.text.push_str(text);
                vec![event(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": state.item_id,
                        "output_index": state.output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                )]
            }
            BlockState::Thinking(state) if delta_type == "thinking_delta" => {
                let Some(text) = delta.get("thinking").and_then(Value::as_str) else {
                    return Vec::new();
                };
                state.text.push_str(text);
                vec![reasoning_delta(state.output_index, &state.item_id, text)]
            }
            BlockState::Thinking(state) if delta_type == "signature_delta" => {
                if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                    state.signature.push_str(signature);
                }
                Vec::new()
            }
            BlockState::Tool(state) if delta_type == "input_json_delta" => {
                let Some(partial) = delta.get("partial_json").and_then(Value::as_str) else {
                    return Vec::new();
                };
                state.arguments.push_str(partial);
                let is_custom = state
                    .route
                    .as_ref()
                    .map(|route| route.kind == ToolKind::Custom)
                    .unwrap_or(false);
                if is_custom || partial.is_empty() {
                    Vec::new()
                } else {
                    vec![event(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "delta": partial,
                        }),
                    )]
                }
            }
            _ => Vec::new(),
        }
    }

    fn content_block_stop(&mut self, value: &Value) -> Vec<Event> {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(block) = self.blocks.remove(&index) else {
            return Vec::new();
        };

        match block {
            BlockState::Text(state) => {
                let mut events = vec![
                    event(
                        "response.output_text.done",
                        json!({
                            "type": "response.output_text.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "content_index": 0,
                            "text": state.text,
                        }),
                    ),
                    event(
                        "response.content_part.done",
                        json!({
                            "type": "response.content_part.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": state.text,
                                "annotations": []
                            }
                        }),
                    ),
                ];
                let item = json!({
                    "id": state.item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": state.text,
                        "annotations": []
                    }]
                });
                events.push(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": state.output_index,
                        "item": item,
                    }),
                ));
                self.output.push((state.output_index, item));
                events
            }
            BlockState::Thinking(state) => {
                let summary = state.text.clone();
                let block = match &state.redacted {
                    Some(data) => json!({"type": "redacted_thinking", "data": data}),
                    None => json!({
                        "type": "thinking",
                        "thinking": state.text,
                        "signature": state.signature,
                    }),
                };
                let encrypted = encode_reasoning_blocks(&[block]);
                let mut events = vec![
                    event(
                        "response.reasoning_summary_text.done",
                        json!({
                            "type": "response.reasoning_summary_text.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "summary_index": 0,
                            "text": summary,
                        }),
                    ),
                    event(
                        "response.reasoning_summary_part.done",
                        json!({
                            "type": "response.reasoning_summary_part.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "summary_index": 0,
                            "part": {
                                "type": "summary_text",
                                "text": summary
                            }
                        }),
                    ),
                ];
                let item = json!({
                    "id": state.item_id,
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": summary
                    }],
                    "encrypted_content": encrypted,
                });
                events.push(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": state.output_index,
                        "item": item,
                    }),
                ));
                self.output.push((state.output_index, item));
                events
            }
            BlockState::Tool(state) => {
                let kind = state
                    .route
                    .as_ref()
                    .map(|route| route.kind)
                    .unwrap_or(ToolKind::Function);
                let original_name = state
                    .route
                    .as_ref()
                    .map(|route| route.original_name.clone())
                    .unwrap_or_else(|| state.upstream_name.clone());
                let namespace = state
                    .route
                    .as_ref()
                    .and_then(|route| route.namespace.clone());

                let (mut events, item) = if kind == ToolKind::Custom {
                    let input = custom_input(&state.arguments);
                    let mut events = Vec::new();
                    if !input.is_empty() {
                        events.push(event(
                            "response.custom_tool_call_input.delta",
                            json!({
                                "type": "response.custom_tool_call_input.delta",
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "delta": input,
                            }),
                        ));
                    }
                    events.push(event(
                        "response.custom_tool_call_input.done",
                        json!({
                            "type": "response.custom_tool_call_input.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "input": input,
                        }),
                    ));
                    let item = json!({
                        "id": state.item_id,
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": state.call_id,
                        "name": original_name,
                        "input": input,
                    });
                    (events, item)
                } else {
                    let arguments = if state.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        state.arguments.clone()
                    };
                    let events = vec![event(
                        "response.function_call_arguments.done",
                        json!({
                            "type": "response.function_call_arguments.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "arguments": arguments,
                        }),
                    )];
                    let item = json!({
                        "id": state.item_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": state.call_id,
                        "name": original_name,
                        "arguments": arguments,
                    });
                    (events, item)
                };

                let mut item = item;
                if let Some(namespace) = namespace {
                    item["namespace"] = Value::String(namespace);
                }
                events.push(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": state.output_index,
                        "item": item,
                    }),
                ));
                self.output.push((state.output_index, item));
                events
            }
            BlockState::Ignored => Vec::new(),
        }
    }

    fn finalize(&mut self) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        self.completed = true;

        let mut events = Vec::new();
        let open: Vec<u64> = self.blocks.keys().copied().collect();
        for index in open {
            events.extend(self.content_block_stop(&json!({"index": index})));
        }

        let incomplete = self.stop_reason.as_deref() == Some("max_tokens");
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let event_name = if incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };

        let mut output = self.output.clone();
        output.sort_by_key(|(output_index, _)| *output_index);
        let output: Vec<Value> = output.into_iter().map(|(_, item)| item).collect();
        let usage = self.usage_seen.then(|| self.usage.to_responses());
        let response = self.response_object(status, output, None, usage);

        events.push(event(
            event_name,
            json!({
                "type": event_name,
                "response": response
            }),
        ));
        events
    }

    fn fail(&mut self, message: impl Into<String>, kind: ErrorKind) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        self.completed = false;
        self.error_kind = Some(kind);
        let message = message.into();
        let response = self.response_object(
            "failed",
            Vec::new(),
            Some(json!({
                "code": kind.as_str(),
                "message": message,
            })),
            self.usage_seen.then(|| self.usage.to_responses()),
        );
        vec![event(
            "response.failed",
            json!({
                "type": "response.failed",
                "response": response
            }),
        )]
    }

    fn finish_or_fail(&mut self) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        if self.stop_reason.is_some() {
            self.finalize()
        } else {
            self.fail(
                "upstream stream ended before a terminal Messages event",
                ErrorKind::Protocol,
            )
        }
    }

    fn response_object(
        &self,
        status: &str,
        output: Vec<Value>,
        error: Option<Value>,
        usage: Option<Value>,
    ) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": output,
            "error": error,
            "incomplete_details": if status == "incomplete" {
                Some(json!({"reason": "max_output_tokens"}))
            } else {
                None
            },
            "usage": usage,
        })
    }
}

fn reasoning_started(output_index: u64, item_id: &str) -> Vec<Event> {
    vec![
        event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "reasoning",
                    "summary": []
                }
            }),
        ),
        event(
            "response.reasoning_summary_part.added",
            json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": ""
                }
            }),
        ),
    ]
}

fn reasoning_delta(output_index: u64, item_id: &str, delta: &str) -> Event {
    event(
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "summary_index": 0,
            "delta": delta,
        }),
    )
}

fn messages_path(provider: &Provider) -> &'static str {
    let base = provider.config.base_url.trim_end_matches('/');
    if base.ends_with("/v1") || base.ends_with("/v1/messages") || base.ends_with("/messages") {
        "messages"
    } else {
        "v1/messages"
    }
}

fn event(name: &str, value: Value) -> Event {
    Event::default().event(name).data(value.to_string())
}

fn event_error(event: &SseEvent) -> Option<(ErrorKind, String)> {
    if event.is_done() {
        return None;
    }
    let value: Value = serde_json::from_str(&event.data).ok()?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .or(event.event.as_deref());
    if event_type == Some("error") || value.get("error").is_some() {
        Some(describe_anthropic_error(&value, None))
    } else {
        None
    }
}

fn describe_anthropic_error(value: &Value, status: Option<StatusCode>) -> (ErrorKind, String) {
    let error = value.get("error");
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("upstream returned an error");
    let kind = match error_type {
        "authentication_error" | "permission_error" => ErrorKind::Auth,
        "not_found_error" => ErrorKind::NotFound,
        "rate_limit_error" => ErrorKind::RateLimit,
        "overloaded_error" | "api_error" => ErrorKind::Server,
        "timeout_error" => ErrorKind::Timeout,
        "invalid_request_error" | "request_too_large" => {
            classify_http_status(status.unwrap_or(StatusCode::BAD_REQUEST), message)
        }
        _ => status
            .filter(|status| status.is_client_error())
            .map(|status| classify_http_status(status, message))
            .unwrap_or(ErrorKind::Server),
    };
    (kind, message.to_string())
}

fn describe_http_error(status: StatusCode, body: &str) -> (ErrorKind, String) {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.get("error").is_some()
    {
        return describe_anthropic_error(&value, Some(status));
    }

    let message = if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| upstream_error_message(status.as_u16(), trimmed))
    } else {
        upstream_error_message(status.as_u16(), trimmed)
    };
    (classify_http_status(status, &message), message)
}

fn upstream_error_message(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    let preview: String = trimmed.chars().take(2_000).collect();
    if preview.is_empty() {
        format!("upstream returned HTTP {status}")
    } else {
        format!("upstream returned HTTP {status}: {preview}")
    }
}

fn insert_header(response: &mut Response, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tools::collect_tools;

    fn routes() -> ToolRoutes {
        let tools = vec![
            json!({"type":"function","name":"exec_command","parameters":{"type":"object"}}),
            json!({"type":"custom","name":"apply_patch"}),
        ];
        let mut routes = ToolRoutes::default();
        let mut out = Vec::new();
        collect_tools(&tools, None, &mut routes, &mut out);
        routes
    }

    fn translator() -> MessagesTranslator {
        MessagesTranslator::new(
            "resp_test".to_string(),
            0,
            "claude-opus-5-5".to_string(),
            routes(),
        )
    }

    fn send(translator: &mut MessagesTranslator, value: Value) -> Vec<Event> {
        translator.process_event(&SseEvent {
            event: value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string),
            data: value.to_string(),
            id: None,
        })
    }

    fn debug_events(events: &[Event]) -> String {
        events
            .iter()
            .map(|event| format!("{event:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn output(translator: &MessagesTranslator) -> Vec<Value> {
        let mut output = translator.output.clone();
        output.sort_by_key(|(index, _)| *index);
        output.into_iter().map(|(_, item)| item).collect()
    }

    #[test]
    fn translates_thinking_text_and_tool_use() {
        let mut translator = translator();

        send(
            &mut translator,
            json!({
                "type":"message_start",
                "message":{"id":"msg_1","model":"claude-opus-5-5","usage":{
                    "input_tokens":10,"output_tokens":0,
                    "cache_creation_input_tokens":5,"cache_read_input_tokens":100
                }}
            }),
        );
        send(
            &mut translator,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"inspect the repo"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_stop","index":0}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"checking"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_stop","index":1}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_start","index":2,"content_block":{
                "type":"tool_use","id":"tooluse_1","name":"exec_command","input":{}
            }}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_stop","index":2}),
        );
        send(
            &mut translator,
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
        );
        let events = send(&mut translator, json!({"type":"message_stop"}));

        assert!(translator.completed);
        let output = output(&translator);
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "inspect the repo");
        let blocks = crate::protocol::messages::decode_reasoning_blocks(
            output[0]["encrypted_content"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "inspect the repo");
        assert_eq!(blocks[0]["signature"], "sig-abc");

        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "checking");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["name"], "exec_command");
        assert_eq!(output[2]["arguments"], "{\"cmd\":\"ls\"}");

        assert_eq!(translator.usage.input, 10);
        assert_eq!(translator.usage.cache_creation, 5);
        assert_eq!(translator.usage.cache_read, 100);
        assert_eq!(translator.usage.output, 42);
        let usage = translator.usage.to_responses();
        assert_eq!(usage["input_tokens"], 115);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 100);
        assert_eq!(usage["output_tokens"], 42);

        let debug = debug_events(&events);
        assert!(debug.contains("response.completed"), "{debug}");
    }

    #[test]
    fn restores_custom_tool_input() {
        let mut translator = translator();
        send(
            &mut translator,
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_start","index":0,"content_block":{
                "type":"tool_use","id":"tooluse_2","name":"apply_patch","input":{}
            }}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":0,"delta":{
                "type":"input_json_delta","partial_json":"{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
            }}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_stop","index":0}),
        );
        send(
            &mut translator,
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        );
        send(&mut translator, json!({"type":"message_stop"}));

        let output = output(&translator);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "custom_tool_call");
        assert_eq!(output[0]["name"], "apply_patch");
        assert_eq!(output[0]["input"], "*** Begin Patch\n*** End Patch");
    }

    #[test]
    fn reports_max_tokens_as_incomplete() {
        let mut translator = translator();
        send(
            &mut translator,
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cut"}}),
        );
        send(
            &mut translator,
            json!({"type":"content_block_stop","index":0}),
        );
        send(
            &mut translator,
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":2}}),
        );
        let events = send(&mut translator, json!({"type":"message_stop"}));

        assert!(translator.completed);
        let debug = debug_events(&events);
        assert!(debug.contains("response.incomplete"));
        assert!(debug.contains("max_output_tokens"));
    }

    #[test]
    fn maps_error_events_to_failed_responses() {
        let mut translator = translator();
        let events = send(
            &mut translator,
            json!({
                "type":"error",
                "error":{"type":"rate_limit_error","message":"slow down"}
            }),
        );

        assert!(translator.is_terminal());
        assert!(!translator.completed);
        assert_eq!(translator.error_kind, Some(ErrorKind::RateLimit));
        let debug = debug_events(&events);
        assert!(debug.contains("response.failed"));
        assert!(debug.contains("rate_limit"));
    }

    #[test]
    fn fails_when_stream_ends_without_message_stop() {
        let mut translator = translator();
        send(
            &mut translator,
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        );
        let events = translator.finish_or_fail();
        assert!(!translator.completed);
        assert_eq!(translator.error_kind, Some(ErrorKind::Protocol));
        assert!(debug_events(&events).contains("response.failed"));
    }
}
