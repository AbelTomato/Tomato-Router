use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::stream;
use axum::http::HeaderValue;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::time::timeout;
use uuid::Uuid;

use crate::error::{AttemptError, ErrorKind, classify_http_status, parse_retry_after};
use crate::protocol::chat::{ToolKind, ToolRoute, ToolRoutes, build_chat_request};
use crate::protocol::sse::{SseEvent, SseParser};
use crate::provider::{Provider, ProviderLease};

pub struct ChatSession {
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
    post_finish_timeout: Duration,
}

pub async fn start(
    provider: std::sync::Arc<Provider>,
    lease: ProviderLease,
    request: &Value,
    requested_model: &str,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> Result<ChatSession, AttemptError> {
    let build = match build_chat_request(request, &provider.config) {
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

    let send_result = timeout(
        first_byte_timeout,
        provider
            .request(Method::POST, "chat/completions")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&build.body)
            .send(),
    )
    .await;

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
        let message = upstream_error_message(status.as_u16(), &body);
        let kind = classify_http_status(status, &message);
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
        && let Some(message) = event_error_message(first)
    {
        lease.failure(ErrorKind::Server);
        return Err(AttemptError::new(
            provider.config.id.clone(),
            ErrorKind::Server,
            message,
        ));
    }

    Ok(ChatSession {
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
        post_finish_timeout: Duration::from_secs(5),
    })
}

impl ChatSession {
    pub fn into_response(self) -> Response {
        let ChatSession {
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
            post_finish_timeout,
        } = self;

        let provider_id = lease.provider.config.id.clone();
        let upstream_model_header = upstream_model.clone();

        let event_stream = stream! {
            let mut translator = ChatTranslator::new(
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
                let wait = if translator.finish_reason.is_some() {
                    post_finish_timeout
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
                lease.failure(
                    translator
                        .error_kind
                        .unwrap_or(ErrorKind::Protocol),
                );
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
    item_id: String,
    output_index: u64,
    text: String,
    started: bool,
    done: bool,
}

struct ToolState {
    output_index: u64,
    item_id: String,
    call_id: String,
    chat_name: String,
    arguments: String,
    emitted_arguments: usize,
    route: Option<ToolRoute>,
    started: bool,
    done: bool,
}

struct ChatTranslator {
    response_id: String,
    created_at: i64,
    model: String,
    tool_routes: ToolRoutes,
    next_output_index: u64,
    text: Option<TextState>,
    tools: BTreeMap<u64, ToolState>,
    output: Vec<Value>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    terminal: bool,
    completed: bool,
    error_kind: Option<ErrorKind>,
}

impl ChatTranslator {
    fn new(response_id: String, created_at: i64, model: String, tool_routes: ToolRoutes) -> Self {
        Self {
            response_id,
            created_at,
            model,
            tool_routes,
            next_output_index: 0,
            text: None,
            tools: BTreeMap::new(),
            output: Vec::new(),
            usage: None,
            finish_reason: None,
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
                "response": self.response_object("in_progress", Vec::new(), None)
            }),
        )
    }

    fn process_event(&mut self, event: &SseEvent) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        if event.is_done() {
            return self.finalize();
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
        if let Some(message) = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
        {
            return self.fail(message.to_string(), ErrorKind::Server);
        }
        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            self.usage = Some(usage_of(usage));
        }

        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Vec::new();
        };

        let mut events = Vec::new();
        if let Some(delta) = choice.get("delta") {
            self.process_delta(delta, &mut events);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        events
    }

    fn process_delta(&mut self, delta: &Value, events: &mut Vec<Event>) {
        if let Some(content) = delta.get("content") {
            let text = content_text(content);
            if !text.is_empty() {
                self.push_text(&text, events);
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, tool_call) in tool_calls.iter().enumerate() {
                let index = tool_call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                let id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = tool_call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.push_tool_delta(index, id, name, arguments, events);
            }
        }
    }

    fn push_text(&mut self, delta: &str, events: &mut Vec<Event>) {
        if self.text.is_none() {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let item_id = format!("msg_{}", Uuid::new_v4().simple());
            self.text = Some(TextState {
                item_id: item_id.clone(),
                output_index,
                text: String::new(),
                started: false,
                done: false,
            });

            events.push(event(
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
            ));
            events.push(event(
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
            ));
            if let Some(text) = self.text.as_mut() {
                text.started = true;
            }
        }

        let Some(text) = self.text.as_mut() else {
            return;
        };
        text.text.push_str(delta);
        events.push(event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "delta": delta,
            }),
        ));
    }

    fn push_tool_delta(
        &mut self,
        index: u64,
        id: &str,
        name: &str,
        arguments: &str,
        events: &mut Vec<Event>,
    ) {
        if !self.tools.contains_key(&index) {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            self.tools.insert(
                index,
                ToolState {
                    output_index,
                    item_id: String::new(),
                    call_id: String::new(),
                    chat_name: String::new(),
                    arguments: String::new(),
                    emitted_arguments: 0,
                    route: None,
                    started: false,
                    done: false,
                },
            );
        }

        let Some(state) = self.tools.get_mut(&index) else {
            return;
        };
        if !id.is_empty() {
            state.call_id = id.to_string();
        }
        if !name.is_empty() {
            state.chat_name = name.to_string();
            state.route = self.tool_routes.resolve(name).cloned();
        }
        if !arguments.is_empty() {
            state.arguments.push_str(arguments);
        }

        if !state.started && !state.chat_name.is_empty() {
            self.start_tool(index, events);
        } else if state.started {
            let is_custom = state
                .route
                .as_ref()
                .map(|route| route.kind == ToolKind::Custom)
                .unwrap_or(false);
            if !is_custom
                && !arguments.is_empty()
                && let Some(state) = self.tools.get_mut(&index)
                && state.emitted_arguments < state.arguments.len()
            {
                let start = state.emitted_arguments;
                let delta = state.arguments[start..].to_string();
                state.emitted_arguments = state.arguments.len();
                events.push(event(
                    "response.function_call_arguments.delta",
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": state.item_id,
                        "output_index": state.output_index,
                        "delta": delta,
                    }),
                ));
            }
        }
    }

    fn start_tool(&mut self, index: u64, events: &mut Vec<Event>) {
        let Some(state) = self.tools.get_mut(&index) else {
            return;
        };
        if state.started || state.chat_name.is_empty() {
            return;
        }
        if state.call_id.is_empty() {
            state.call_id = format!("call_{}", Uuid::new_v4().simple());
        }
        let route = state.route.clone();
        let kind = route
            .as_ref()
            .map(|route| route.kind)
            .unwrap_or(ToolKind::Function);
        state.item_id = match kind {
            ToolKind::Function => format!("fc_{}", Uuid::new_v4().simple()),
            ToolKind::Custom => format!("ctc_{}", Uuid::new_v4().simple()),
        };
        state.started = true;

        let (item_type, name, namespace) = match &route {
            Some(route) => (
                if route.kind == ToolKind::Custom {
                    "custom_tool_call"
                } else {
                    "function_call"
                },
                route.original_name.clone(),
                route.namespace.clone(),
            ),
            None => ("function_call", state.chat_name.clone(), None),
        };

        let mut item = json!({
            "id": state.item_id,
            "type": item_type,
            "status": "in_progress",
            "call_id": state.call_id,
            "name": name,
        });
        if let Some(namespace) = namespace {
            item["namespace"] = Value::String(namespace);
        }
        if kind == ToolKind::Custom {
            item["input"] = Value::String(String::new());
        } else {
            item["arguments"] = Value::String(String::new());
        }
        events.push(event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": state.output_index,
                "item": item,
            }),
        ));

        if kind == ToolKind::Function && !state.arguments.is_empty() {
            let delta = state.arguments.clone();
            state.emitted_arguments = state.arguments.len();
            events.push(event(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "delta": delta,
                }),
            ));
        }
    }

    fn finalize(&mut self) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        self.completed = true;
        let mut events = Vec::new();
        self.finish_text(&mut events);

        let mut indices: Vec<u64> = self.tools.keys().copied().collect();
        indices.sort_by_key(|index| {
            self.tools
                .get(index)
                .map(|state| state.output_index)
                .unwrap_or(u64::MAX)
        });
        for index in indices {
            self.finish_tool(index, &mut events);
        }

        let incomplete = self.finish_reason.as_deref() == Some("length");
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.response_object(status, self.output.clone(), None);
        let event_name = if incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };
        events.push(event(
            event_name,
            json!({
                "type": event_name,
                "response": response
            }),
        ));
        events
    }

    fn finish_text(&mut self, events: &mut Vec<Event>) {
        let Some(text) = self.text.as_mut() else {
            return;
        };
        if !text.started || text.done {
            return;
        }
        text.done = true;
        events.push(event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "text": text.text,
            }),
        ));
        events.push(event(
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": text.item_id,
                "output_index": text.output_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": text.text,
                    "annotations": []
                }
            }),
        ));
        let item = json!({
            "id": text.item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text.text,
                "annotations": []
            }]
        });
        events.push(event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": text.output_index,
                "item": item,
            }),
        ));
        self.output.push(item);
    }

    fn finish_tool(&mut self, index: u64, events: &mut Vec<Event>) {
        if !self
            .tools
            .get(&index)
            .map(|state| state.started)
            .unwrap_or(false)
        {
            self.start_tool(index, events);
        }
        let Some(state) = self.tools.get_mut(&index) else {
            return;
        };
        if !state.started || state.done {
            return;
        }
        state.done = true;

        let kind = state
            .route
            .as_ref()
            .map(|route| route.kind)
            .unwrap_or(ToolKind::Function);
        let original_name = state
            .route
            .as_ref()
            .map(|route| route.original_name.clone())
            .unwrap_or_else(|| state.chat_name.clone());
        let namespace = state
            .route
            .as_ref()
            .and_then(|route| route.namespace.clone());

        let item = if kind == ToolKind::Custom {
            let input = custom_input(&state.arguments);
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
            json!({
                "id": state.item_id,
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": state.call_id,
                "name": original_name,
                "input": input,
            })
        } else {
            events.push(event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "arguments": state.arguments,
                }),
            ));
            json!({
                "id": state.item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": state.call_id,
                "name": original_name,
                "arguments": state.arguments,
            })
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
        self.output.push(item);
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
        if self.finish_reason.is_some() || self.usage.is_some() {
            self.finalize()
        } else {
            self.fail(
                "upstream stream ended before a terminal completion event",
                ErrorKind::Protocol,
            )
        }
    }

    fn response_object(&self, status: &str, output: Vec<Value>, error: Option<Value>) -> Value {
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
            "usage": self.usage,
        })
    }
}

fn event(name: &str, value: Value) -> Event {
    Event::default().event(name).data(value.to_string())
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        _ => String::new(),
    }
}

fn usage_of(usage: &Value) -> Value {
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .or_else(|| usage.pointer("/output_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total,
    })
}

fn custom_input(arguments: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(arguments) {
        if let Some(input) = value.get("input").and_then(Value::as_str) {
            return input.to_string();
        }
        if let Some(input) = value.as_str() {
            return input.to_string();
        }
    }
    arguments.to_string()
}

fn event_error_message(event: &SseEvent) -> Option<String> {
    if event.is_done() {
        return None;
    }
    let value: Value = serde_json::from_str(&event.data).ok()?;
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_string)
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
