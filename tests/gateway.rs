use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tomato_router::{AppState, Config, build_app};

#[derive(Default)]
struct MessagesCaptured {
    bodies: Vec<Value>,
    api_keys: Vec<Option<String>>,
    versions: Vec<Option<String>>,
}

type MessagesCapture = Arc<Mutex<MessagesCaptured>>;

#[derive(Default)]
struct Captured {
    bodies: Vec<Value>,
    session_ids: Vec<Option<String>>,
}

type Capture = Arc<Mutex<Captured>>;

const MESSAGES_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"mock-messages\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":100}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"inspect first\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-integration\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"checking\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tooluse_1\",\"name\":\"exec_command\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":20}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

async fn messages_mock(
    State(capture): State<MessagesCapture>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let mut captured = capture.lock().unwrap();
    captured.bodies.push(body);
    captured.api_keys.push(
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    captured.versions.push(
        headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    drop(captured);
    ([(header::CONTENT_TYPE, "text/event-stream")], MESSAGES_SSE).into_response()
}

#[tokio::test]
async fn messages_adapter_streams_thinking_and_tool_calls() {
    let capture: MessagesCapture = Arc::default();
    let mock = spawn(
        Router::new()
            .route("/v1/messages", post(messages_mock))
            .with_state(capture.clone()),
    )
    .await;

    let config = parse_config(
        &format!(
            r#"
            [[providers]]
            id = "claude"
            base_url = "{mock}"
            wire_api = "messages"
            api_key = "sk-test"
            max_concurrency = 4

            [providers.auth]
            type = "header"
            header = "x-api-key"
            prefix = ""

            [providers.models]
            "tomato-codex" = "mock-messages"
            "#
        ),
        "",
    );
    let gateway = spawn_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .json(&json!({
            "model": "tomato-codex",
            "stream": true,
            "instructions": "be brief",
            "reasoning": {"effort": "high"},
            "max_output_tokens": 2048,
            "tools": [
                {"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}},
                {"type":"custom","name":"apply_patch","description":"Apply a patch"}
            ],
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text","text":"list files"}]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-tomato-provider"], "claude");
    assert_eq!(
        response.headers()["x-tomato-upstream-model"],
        "mock-messages"
    );

    let body = response.text().await.unwrap();
    assert!(body.contains("response.created"), "{body}");
    assert!(
        body.contains("response.reasoning_summary_text.delta"),
        "{body}"
    );
    assert!(body.contains("response.completed"), "{body}");

    let completed = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed event");
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["type"], "reasoning");
    assert!(output[0]["encrypted_content"].as_str().unwrap().len() > 20);
    assert_eq!(output[0]["summary"][0]["text"], "inspect first");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["name"], "exec_command");
    assert_eq!(output[2]["call_id"], "tooluse_1");
    assert_eq!(output[2]["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 110);
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        100
    );

    let captured = capture.lock().unwrap();
    assert_eq!(captured.api_keys[0].as_deref(), Some("sk-test"));
    assert_eq!(captured.versions[0].as_deref(), Some("2023-06-01"));
    let sent = &captured.bodies[0];
    assert_eq!(sent["model"], "mock-messages");
    assert_eq!(sent["system"], "be brief");
    assert_eq!(sent["max_tokens"], 2048);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["thinking"], json!({"type":"adaptive"}));
    assert_eq!(sent["output_config"], json!({"effort":"high"}));
    assert_eq!(sent["messages"][0]["role"], "user");
    assert_eq!(sent["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(sent["tools"][1]["input_schema"]["required"][0], "input");
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

async fn spawn_gateway(config: Config) -> String {
    let state = AppState::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, build_app(state)).await.unwrap();
    });
    format!("http://{address}")
}

fn parse_config(providers: &str, extra: &str) -> Config {
    let text = format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        require_auth = false
        max_body_bytes = 4194304
        first_byte_timeout_ms = 5000
        stream_idle_timeout_ms = 5000

        {providers}
        {extra}
        "#
    );
    let config: Config = toml::from_str(&text).unwrap();
    config.validate().unwrap();
    config
}

async fn chat_mock(State(capture): State<Capture>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    capture.lock().unwrap().bodies.push(body);
    capture.lock().unwrap().session_ids.push(
        headers
            .get("session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );

    let stream = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
}

async fn lite_chat_mock(State(capture): State<Capture>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    capture.lock().unwrap().bodies.push(body);

    let stream = concat!(
        "data: {\"id\":\"chatcmpl-lite\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_lite_exec\",\"type\":\"function\",\"function\":{\"name\":\"functions__exec\",\"arguments\":\"{\\\"input\\\":\\\"text(1)\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-lite\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
}

const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"mock-responses\",\"output\":[]}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"mock-responses\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"from secondary\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n",
);

async fn responses_mock(
    State(capture): State<Capture>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    capture.lock().unwrap().bodies.push(body);
    capture.lock().unwrap().session_ids.push(
        headers
            .get("session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], RESPONSES_SSE).into_response()
}

async fn responses_json_mock(State(capture): State<Capture>, body: Bytes) -> Json<Value> {
    let body: Value = serde_json::from_slice(&body).unwrap();
    capture.lock().unwrap().bodies.push(body);
    Json(json!({
        "id": "resp_json",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "mock-responses",
        "output": [{
            "id": "msg_json",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "from json", "annotations": []}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
    }))
}

async fn primary_failure() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": {"message": "primary is down", "type": "server_error"}})),
    )
        .into_response()
}

fn provider(id: &str, base_url: &str, wire_api: &str) -> String {
    format!(
        r#"
        [[providers]]
        id = "{id}"
        base_url = "{base_url}"
        wire_api = "{wire_api}"
        api_key = "sk-test"
        max_concurrency = 4

        [providers.models]
        "tomato-codex" = "{id}-upstream"
        "#
    )
}

#[tokio::test]
async fn chat_adapter_streams_responses_events_and_translates_tools() {
    let capture: Capture = Arc::default();
    let mock = spawn(
        Router::new()
            .route("/v1/chat/completions", post(chat_mock))
            .with_state(capture.clone()),
    )
    .await;

    let config = parse_config(
        &format!(
            r#"
            [[providers]]
            id = "chat"
            base_url = "{mock}/v1"
            wire_api = "chat"
            api_key = "sk-test"
            max_concurrency = 4

            [providers.models]
            "tomato-codex" = "mock-chat"
            "#
        ),
        "",
    );
    let gateway = spawn_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .header("session-id", "session-123")
        .json(&json!({
            "model": "tomato-codex",
            "stream": true,
            "instructions": "be brief",
            "input": "hello"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-tomato-provider"], "chat");
    assert_eq!(response.headers()["content-type"], "text/event-stream");

    let body = response.text().await.unwrap();
    assert!(body.contains("response.created"), "{body}");
    assert!(body.contains("Hello!"), "{body}");
    assert!(body.contains("response.completed"), "{body}");

    let completed = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed event");
    assert!(completed["response"]["error"].is_null(), "{completed}");
    assert_eq!(completed["response"]["usage"]["total_tokens"], 7);
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "Hello!"
    );

    let captured = capture.lock().unwrap();
    assert_eq!(captured.bodies.len(), 1);
    assert_eq!(captured.bodies[0]["model"], "mock-chat");
    assert_eq!(captured.bodies[0]["messages"][0]["role"], "system");
    assert_eq!(captured.bodies[0]["messages"][1]["role"], "user");
    assert_eq!(captured.bodies[0]["messages"][1]["content"], "hello");
    assert_eq!(captured.bodies[0]["stream"], true);
    assert_eq!(captured.session_ids[0], None);
}

#[tokio::test]
async fn responses_provider_fails_over_before_commit() {
    let primary_capture: Capture = Arc::default();
    let primary = spawn(
        Router::new()
            .route("/v1/responses", post(primary_failure))
            .with_state(primary_capture.clone()),
    )
    .await;
    let secondary_capture: Capture = Arc::default();
    let secondary = spawn(
        Router::new()
            .route("/v1/responses", post(responses_mock))
            .with_state(secondary_capture.clone()),
    )
    .await;

    let config = parse_config(
        &format!(
            "{}{}",
            provider("primary", &format!("{primary}/v1"), "responses"),
            provider("secondary", &format!("{secondary}/v1"), "responses")
        ),
        "",
    );
    let gateway = spawn_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .header("session-id", "session-abc")
        .json(&json!({
            "model": "tomato-codex",
            "stream": true,
            "input": "hello",
            "store": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-tomato-provider"], "secondary");
    assert_eq!(
        response.headers()["x-tomato-upstream-model"],
        "secondary-upstream"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("from secondary"), "{body}");

    assert_eq!(
        secondary_capture.lock().unwrap().session_ids[0].as_deref(),
        Some("session-abc")
    );
}

#[tokio::test]
async fn responses_provider_supports_non_streaming_json() {
    let capture: Capture = Arc::default();
    let mock = spawn(
        Router::new()
            .route("/v1/responses", post(responses_json_mock))
            .with_state(capture.clone()),
    )
    .await;

    let config = parse_config(
        &provider("responses", &format!("{mock}/v1"), "responses"),
        "",
    );
    let gateway = spawn_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .json(&json!({
            "model": "tomato-codex",
            "input": "hello",
            "store": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-tomato-provider"], "responses");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "from json");
    assert_eq!(capture.lock().unwrap().bodies[0]["stream"], false);
}

#[tokio::test]
async fn local_auth_is_enforced() {
    let capture: Capture = Arc::default();
    let mock = spawn(
        Router::new()
            .route("/v1/chat/completions", post(chat_mock))
            .with_state(capture.clone()),
    )
    .await;

    let config_text = format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        require_auth = true
        api_keys = ["local-test-key"]
        first_byte_timeout_ms = 5000
        stream_idle_timeout_ms = 5000

        [[providers]]
        id = "chat"
        base_url = "{mock}/v1"
        wire_api = "chat"
        api_key = "sk-test"
        "#
    );
    let config: Config = toml::from_str(&config_text).unwrap();
    config.validate().unwrap();
    let gateway = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let unauthorized = client
        .post(format!("{gateway}/v1/responses"))
        .json(&json!({"model": "tomato-codex", "stream": true, "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = client
        .post(format!("{gateway}/v1/responses"))
        .bearer_auth("local-test-key")
        .json(&json!({"model": "tomato-codex", "stream": true, "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
}

#[tokio::test]
async fn chat_adapter_reconstructs_namespaced_custom_exec_call() {
    let capture: Capture = Arc::default();
    let mock = spawn(
        Router::new()
            .route("/v1/chat/completions", post(lite_chat_mock))
            .with_state(capture.clone()),
    )
    .await;

    let config = parse_config(
        &format!(
            r#"
            [[providers]]
            id = "chat"
            base_url = "{mock}/v1"
            wire_api = "chat"
            api_key = "sk-test"
            max_concurrency = 4

            [providers.models]
            "tomato-codex" = "mock-chat"
            "#
        ),
        "",
    );
    let gateway = spawn_gateway(config).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .json(&json!({
            "model": "tomato-codex",
            "stream": true,
            "input": [
                {
                    "type": "additional_tools",
                    "id": "at_test",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "functions",
                        "description": "",
                        "tools": [{
                            "type": "custom",
                            "name": "exec",
                            "description": "Run JavaScript code to orchestrate tool calls"
                        }]
                    }]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "run"}]
                }
            ],
            "store": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();

    {
        let captured = capture.lock().unwrap();
        let tools = captured.bodies[0]["tools"].as_array().unwrap();
        let exec = tools
            .iter()
            .find(|tool| tool["function"]["name"] == "functions__exec")
            .expect("namespaced custom exec should be flattened into a Chat function");
        assert_eq!(exec["type"], "function");
        assert_eq!(
            exec["function"]["parameters"]["properties"]["input"]["type"],
            "string"
        );
    }

    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect();

    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.custom_tool_call_input.delta"),
        "{body}"
    );

    let item = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done"
                && event["item"]["type"] == "custom_tool_call"
        })
        .expect("custom_tool_call output item");
    assert_eq!(item["item"]["name"], "exec");
    assert_eq!(item["item"]["namespace"], "functions");
    assert_eq!(item["item"]["call_id"], "call_lite_exec");
    assert_eq!(item["item"]["input"], "text(1)");
}
