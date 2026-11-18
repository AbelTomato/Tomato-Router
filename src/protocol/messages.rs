use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};

use crate::config::{ProviderConfig, ThinkingMode};
use crate::protocol::tools::{
    ToolRoutes, UpstreamTool, collect_additional_tools, collect_tools, output_to_string,
    resolve_upstream_name,
};

/// Version of the `encrypted_content` payload used to round-trip Anthropic
/// thinking blocks through Codex's opaque reasoning item.
const REASONING_VERSION: u64 = 1;

#[derive(Debug, Clone)]
pub struct MessagesBuild {
    pub body: Value,
    pub tool_routes: ToolRoutes,
    pub upstream_model: String,
}

pub fn build_messages_request(
    request: &Value,
    provider: &ProviderConfig,
) -> Result<MessagesBuild, String> {
    let object = request
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let requested_model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| "missing `model`".to_string())?;
    let upstream_model = provider.model_for(requested_model);

    if object.get("previous_response_id").is_some() {
        return Err(
            "`previous_response_id` is not supported by the stateless Messages adapter; send the full input history instead"
                .to_string(),
        );
    }
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return Err("the Messages adapter currently requires `stream: true`".to_string());
    }

    let mut tools = Vec::new();
    collect_additional_tools(object.get("input"), &mut tools);
    if let Some(top_level) = object.get("tools").and_then(Value::as_array) {
        tools.extend(top_level.iter().cloned());
    }
    let mut tool_routes = ToolRoutes::default();
    let mut normalized_tools = Vec::new();
    collect_tools(&tools, None, &mut tool_routes, &mut normalized_tools);

    let mut system_parts = Vec::new();
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        system_parts.push(instructions.to_string());
    }

    let mut messages = Vec::new();
    let mut pending_thinking = Vec::new();
    match object.get("input") {
        Some(Value::String(text)) => {
            if !text.trim().is_empty() {
                push_message_blocks(&mut messages, "user", vec![text_block(text)]);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                translate_input_item(
                    item,
                    &tool_routes,
                    &mut system_parts,
                    &mut messages,
                    &mut pending_thinking,
                );
            }
        }
        Some(Value::Object(_)) => {
            if let Some(input) = object.get("input") {
                translate_input_item(
                    input,
                    &tool_routes,
                    &mut system_parts,
                    &mut messages,
                    &mut pending_thinking,
                );
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("`input` must be a string or an array".to_string()),
    }

    // A trailing reasoning item has no assistant turn to attach to. Dropping it
    // is safer than sending an empty assistant message to Anthropic.
    pending_thinking.clear();

    if messages.is_empty() {
        return Err("request contains no user/assistant messages".to_string());
    }

    let mut body = Map::new();
    if let Some(extra) = provider
        .compat
        .extra_body
        .as_ref()
        .and_then(Value::as_object)
    {
        body.extend(extra.clone());
    }

    body.insert("model".to_string(), Value::String(upstream_model.clone()));
    if !system_parts.is_empty() {
        body.insert(
            "system".to_string(),
            Value::String(system_parts.join("\n\n")),
        );
    }
    body.insert("messages".to_string(), Value::Array(messages));

    let max_tokens = object
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(provider.compat.default_max_tokens);
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), Value::Bool(true));

    let messages_tools: Vec<Value> = normalized_tools.iter().map(messages_tool).collect();
    let has_tools = !messages_tools.is_empty();
    if has_tools {
        body.insert("tools".to_string(), Value::Array(messages_tools));
    }

    let mut tool_choice = map_tool_choice(object.get("tool_choice"), &tool_routes);
    if has_tools && object.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
        match &mut tool_choice {
            Some(Value::Object(choice)) => {
                choice.insert("disable_parallel_tool_use".to_string(), Value::Bool(true));
            }
            _ => {
                tool_choice = Some(json!({
                    "type": "auto",
                    "disable_parallel_tool_use": true
                }));
            }
        }
    }
    if let Some(tool_choice) = tool_choice {
        body.insert("tool_choice".to_string(), tool_choice);
    }

    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    let thinking = resolve_thinking(provider.compat.thinking, effort);
    let thinking_enabled = thinking.is_some();

    // Anthropic rejects temperature/top_p overrides while thinking is enabled.
    if !thinking_enabled {
        if let Some(temperature) = object.get("temperature").and_then(Value::as_f64) {
            body.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = object.get("top_p").and_then(Value::as_f64) {
            body.insert("top_p".to_string(), json!(top_p));
        }
    }

    if let Some(stop) = object.get("stop") {
        match stop {
            Value::String(text) if !text.is_empty() => {
                body.insert("stop_sequences".to_string(), json!([text]));
            }
            Value::Array(items) if !items.is_empty() => {
                body.insert("stop_sequences".to_string(), Value::Array(items.clone()));
            }
            _ => {}
        }
    }

    if let Some(user_id) = request.pointer("/metadata/user_id").and_then(Value::as_str)
        && !user_id.is_empty()
    {
        body.insert("metadata".to_string(), json!({"user_id": user_id}));
    }

    if thinking.is_some() {
        body.insert("thinking".to_string(), json!({"type": "adaptive"}));
        if let Some(effort) = thinking.flatten() {
            body.insert("output_config".to_string(), json!({"effort": effort}));
        }
    }

    Ok(MessagesBuild {
        body: Value::Object(body),
        tool_routes,
        upstream_model,
    })
}

/// `None` disables thinking. `Some(effort)` enables adaptive thinking, where
/// the inner option is the explicit output effort, if any.
fn resolve_thinking(mode: ThinkingMode, effort: Option<&str>) -> Option<Option<&'static str>> {
    if effort == Some("none") {
        return None;
    }
    let mapped = effort.and_then(map_effort);
    match mode {
        ThinkingMode::Off => None,
        ThinkingMode::Auto | ThinkingMode::Adaptive => Some(mapped),
    }
}

fn map_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" | "ultra" => Some("max"),
        _ => None,
    }
}

fn messages_tool(tool: &UpstreamTool) -> Value {
    let mut object = Map::new();
    object.insert("name".to_string(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        object.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    object.insert("input_schema".to_string(), tool.input_schema.clone());
    Value::Object(object)
}

fn map_tool_choice(value: Option<&Value>, routes: &ToolRoutes) -> Option<Value> {
    match value? {
        Value::String(mode) => match mode.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => Some(json!({"type": "none"})),
            _ => None,
        },
        Value::Object(object) => {
            let name = object.get("name").and_then(Value::as_str)?;
            let namespace = object.get("namespace").and_then(Value::as_str);
            let upstream_name = resolve_upstream_name(routes, name, namespace);
            Some(json!({"type": "tool", "name": upstream_name}))
        }
        _ => None,
    }
}

fn translate_input_item(
    item: &Value,
    routes: &ToolRoutes,
    system_parts: &mut Vec<String>,
    messages: &mut Vec<Value>,
    pending_thinking: &mut Vec<Value>,
) {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    match item_type {
        "message" => match item.get("role").and_then(Value::as_str).unwrap_or("user") {
            "developer" | "system" => {
                let text = content_text(item.get("content"));
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
            }
            "assistant" => {
                let blocks = convert_content_blocks(item.get("content"));
                if blocks.is_empty() {
                    return;
                }
                let message = assistant_message(messages, pending_thinking);
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    content.extend(blocks);
                }
            }
            _ => {
                flush_pending_thinking(messages, pending_thinking);
                let blocks = convert_content_blocks(item.get("content"));
                push_message_blocks(messages, "user", blocks);
            }
        },
        "function_call" => {
            let call_id = call_id_of(item);
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let upstream_name = resolve_upstream_name(routes, name, namespace);
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let block = json!({
                "type": "tool_use",
                "id": call_id,
                "name": upstream_name,
                "input": parse_tool_arguments(arguments),
            });
            append_assistant_block(messages, pending_thinking, block);
        }
        "custom_tool_call" => {
            let call_id = call_id_of(item);
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let upstream_name = resolve_upstream_name(routes, name, namespace);
            let input = item.get("input").and_then(Value::as_str).unwrap_or("");
            let block = json!({
                "type": "tool_use",
                "id": call_id,
                "name": upstream_name,
                "input": {"input": input},
            });
            append_assistant_block(messages, pending_thinking, block);
        }
        "function_call_output" | "custom_tool_call_output" => {
            flush_pending_thinking(messages, pending_thinking);
            let call_id = call_id_of(item);
            let output = output_to_string(item.get("output"));
            let block = json!({
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": output,
            });
            push_message_blocks(messages, "user", vec![block]);
        }
        "reasoning" => {
            if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str)
                && let Some(blocks) = decode_reasoning_blocks(encrypted)
            {
                pending_thinking.extend(blocks);
            }
        }
        "additional_tools" => {}
        _ => {
            tracing::debug!(
                item_type,
                "dropping Responses item that has no native Messages equivalent"
            );
        }
    }
}

fn call_id_of(item: &Value) -> &str {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("call_unknown")
}

fn append_assistant_block(
    messages: &mut Vec<Value>,
    pending_thinking: &mut Vec<Value>,
    block: Value,
) {
    let message = assistant_message(messages, pending_thinking);
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        content.push(block);
    }
}

fn assistant_message<'a>(
    messages: &'a mut Vec<Value>,
    pending_thinking: &mut Vec<Value>,
) -> &'a mut Value {
    let is_assistant = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    if !is_assistant {
        messages.push(json!({"role": "assistant", "content": []}));
    }
    let message = messages
        .last_mut()
        .expect("an assistant message is always present");
    if !pending_thinking.is_empty() {
        let blocks = std::mem::take(pending_thinking);
        prepend_blocks(message, blocks);
    }
    message
}

fn prepend_blocks(message: &mut Value, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        content.splice(0..0, blocks);
    }
}

fn flush_pending_thinking(messages: &mut [Value], pending_thinking: &mut Vec<Value>) {
    if pending_thinking.is_empty() {
        return;
    }
    let last_is_assistant = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    if last_is_assistant {
        let blocks = std::mem::take(pending_thinking);
        if let Some(message) = messages.last_mut() {
            prepend_blocks(message, blocks);
        }
    } else {
        pending_thinking.clear();
    }
}

fn push_message_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn convert_content_blocks(content: Option<&Value>) -> Vec<Value> {
    let Some(content) = content else {
        return Vec::new();
    };
    match content {
        Value::String(text) => {
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![text_block(text)]
            }
        }
        Value::Array(parts) => parts.iter().flat_map(convert_content_part).collect(),
        Value::Object(_) => convert_content_part(content),
        _ => Vec::new(),
    }
}

fn convert_content_part(part: &Value) -> Vec<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![text_block(text)])
            .unwrap_or_default(),
        Some("input_image" | "image") => image_block(part).into_iter().collect(),
        Some("input_file") => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![text_block(text)])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn image_block(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| {
            part.get("image_url")
                .and_then(|image| image.get("url"))
                .and_then(Value::as_str)
        })
        .or_else(|| part.get("url").and_then(Value::as_str))?;

    if let Some(rest) = url.strip_prefix("data:") {
        let (media_type, data) = rest.split_once(";base64,")?;
        return Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }));
    }

    Some(json!({
        "type": "image",
        "source": {"type": "url", "url": url}
    }))
}

fn parse_tool_arguments(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(Value::Null) => json!({}),
        Ok(other) => json!({"value": other}),
        Err(_) => {
            tracing::warn!("invalid JSON in function_call arguments; sending an empty object");
            json!({})
        }
    }
}

pub fn encode_reasoning_blocks(blocks: &[Value]) -> String {
    let payload = json!({
        "v": REASONING_VERSION,
        "blocks": blocks,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode_reasoning_blocks(encrypted: &str) -> Option<Vec<Value>> {
    let bytes = URL_SAFE_NO_PAD.decode(encrypted.trim()).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    if payload.get("v").and_then(Value::as_u64) != Some(REASONING_VERSION) {
        return None;
    }
    let blocks = payload.get("blocks")?.as_array()?;
    let mut output = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let Some(thinking) = block.get("thinking").and_then(Value::as_str) else {
                    continue;
                };
                let Some(signature) = block.get("signature").and_then(Value::as_str) else {
                    continue;
                };
                if signature.is_empty() {
                    continue;
                }
                output.push(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                }));
            }
            Some("redacted_thinking") => {
                let Some(data) = block.get("data").and_then(Value::as_str) else {
                    continue;
                };
                output.push(json!({"type": "redacted_thinking", "data": data}));
            }
            _ => {}
        }
    }
    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderConfig {
        toml::from_str(
            r#"
            id = "relay"
            base_url = "https://relay.example"
            api_key = "sk-test"
            wire_api = "messages"
            [models]
            "tomato-claude" = "claude-opus-5-5"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn translates_codex_history_and_tools() {
        let request = json!({
            "model": "tomato-claude",
            "instructions": "You are Codex.",
            "input": [
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"Be concise."}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"edit a file"}]},
                {"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"a.txt"},
                {"type":"custom_tool_call","call_id":"call_2","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"},
                {"type":"custom_tool_call_output","call_id":"call_2","output":"done"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"finished"}]}
            ],
            "tools": [
                {"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}},
                {"type":"custom","name":"apply_patch","description":"Apply a patch","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}},
                {"type":"web_search"}
            ],
            "tool_choice": "required",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "xhigh"},
            "max_output_tokens": 4096,
            "stream": true
        });

        let build = build_messages_request(&request, &provider()).unwrap();
        let body = build.body.as_object().unwrap();
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["system"], "You are Codex.\n\nBe concise.");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["output_config"], json!({"effort":"max"}));
        assert_eq!(body["tool_choice"]["type"], "any");
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][1]["input_schema"]["required"][0], "input");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["name"], "exec_command");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["content"], "a.txt");
        assert_eq!(messages[3]["content"][0]["name"], "apply_patch");
        assert_eq!(
            messages[3]["content"][0]["input"]["input"],
            "*** Begin Patch\n*** End Patch"
        );
        assert_eq!(messages[4]["content"][0]["type"], "tool_result");
        assert_eq!(messages[5]["content"][0]["text"], "finished");
    }

    #[test]
    fn round_trips_thinking_through_encrypted_content() {
        let encrypted = encode_reasoning_blocks(&[json!({
            "type": "thinking",
            "thinking": "inspect the repository",
            "signature": "sig-123"
        })]);
        assert!(encrypted.contains('_') || !encrypted.contains('+'));

        let request = json!({
            "model": "tomato-claude",
            "input": [
                {"type":"message","role":"user","content":"go"},
                {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":encrypted},
                {"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}
            ],
            "tools": [{"type":"function","name":"exec_command","parameters":{"type":"object"}}],
            "stream": true
        });

        let build = build_messages_request(&request, &provider()).unwrap();
        let messages = build.body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][0]["signature"], "sig-123");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn drops_foreign_or_signatureless_reasoning() {
        assert!(decode_reasoning_blocks("not-base64!").is_none());
        let encrypted = encode_reasoning_blocks(&[json!({
            "type": "thinking",
            "thinking": "unsigned"
        })]);
        assert!(decode_reasoning_blocks(&encrypted).is_none());
    }

    #[test]
    fn defaults_max_tokens_and_disables_thinking_for_none() {
        let request = json!({
            "model": "tomato-claude",
            "input": "hello",
            "reasoning": {"effort": "none"},
            "stream": true
        });
        let build = build_messages_request(&request, &provider()).unwrap();
        let body = build.body.as_object().unwrap();
        assert_eq!(body["max_tokens"], 16_384);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn rejects_non_streaming_and_previous_response_id() {
        let mut request = json!({"model":"tomato-claude","input":"hi","stream":false});
        assert!(build_messages_request(&request, &provider()).is_err());
        request["stream"] = Value::Bool(true);
        request["previous_response_id"] = json!("resp_1");
        assert!(build_messages_request(&request, &provider()).is_err());
    }

    #[test]
    fn keeps_image_data_uris_as_base64_sources() {
        let request = json!({
            "model": "tomato-claude",
            "input": [{"type":"message","role":"user","content":[
                {"type":"input_text","text":"what is this?"},
                {"type":"input_image","image_url":"data:image/png;base64,AAAA"}
            ]}],
            "stream": true
        });
        let build = build_messages_request(&request, &provider()).unwrap();
        let image = &build.body["messages"][0]["content"][1];
        assert_eq!(image["source"]["type"], "base64");
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(image["source"]["data"], "AAAA");
    }
}
