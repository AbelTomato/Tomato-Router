use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::config::ProviderConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Function,
    Custom,
}

#[derive(Debug, Clone)]
pub struct ToolRoute {
    pub chat_name: String,
    pub original_name: String,
    pub namespace: Option<String>,
    pub kind: ToolKind,
}

#[derive(Debug, Clone, Default)]
pub struct ToolRoutes {
    by_chat_name: HashMap<String, ToolRoute>,
    by_original: HashMap<String, String>,
}

impl ToolRoutes {
    pub fn resolve(&self, chat_name: &str) -> Option<&ToolRoute> {
        self.by_chat_name.get(chat_name)
    }

    pub fn chat_name_for(&self, original_name: &str, namespace: Option<&str>) -> Option<&str> {
        self.by_original
            .get(&original_key(namespace, original_name))
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.by_chat_name.is_empty()
    }

    fn insert(&mut self, route: ToolRoute) {
        let mut chat_name = route.chat_name.clone();
        let base = chat_name.clone();
        let mut suffix = 2;
        while self.by_chat_name.contains_key(&chat_name) {
            chat_name = format!("{base}__{suffix}");
            suffix += 1;
        }
        let mut route = route;
        route.chat_name = chat_name.clone();
        self.by_original.insert(
            original_key(route.namespace.as_deref(), &route.original_name),
            chat_name.clone(),
        );
        self.by_chat_name.insert(chat_name, route);
    }
}

#[derive(Debug, Clone)]
pub struct ChatBuild {
    pub body: Value,
    pub tool_routes: ToolRoutes,
    pub upstream_model: String,
}

pub fn build_chat_request(request: &Value, provider: &ProviderConfig) -> Result<ChatBuild, String> {
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
            "`previous_response_id` is not supported by the stateless Chat adapter; send the full input history instead"
                .to_string(),
        );
    }

    let mut tools = Vec::new();
    collect_additional_tools(object.get("input"), &mut tools);
    if let Some(top_level) = object.get("tools").and_then(Value::as_array) {
        tools.extend(top_level.iter().cloned());
    }

    let mut tool_routes = ToolRoutes::default();
    let mut chat_tools = Vec::new();
    collect_tools(&tools, None, &mut tool_routes, &mut chat_tools);

    let mut messages = Vec::new();
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        push_message(
            &mut messages,
            "system",
            Some(Value::String(instructions.to_string())),
            None,
            None,
        );
    }

    match object.get("input") {
        Some(Value::String(text)) => {
            push_message(
                &mut messages,
                "user",
                Some(Value::String(text.clone())),
                None,
                None,
            );
        }
        Some(Value::Array(items)) => {
            for item in items {
                translate_input_item(item, &tool_routes, &mut messages);
            }
        }
        Some(Value::Object(_)) => {
            if let Some(input) = object.get("input") {
                translate_input_item(input, &tool_routes, &mut messages);
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("`input` must be a string or an array".to_string()),
    }

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
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(true));
    if !chat_tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(chat_tools));
    }
    if let Some(tool_choice) = map_tool_choice(object.get("tool_choice"), &tool_routes) {
        body.insert("tool_choice".to_string(), tool_choice);
    }
    if provider.compat.supports_parallel_tool_calls
        && let Some(parallel) = object.get("parallel_tool_calls").and_then(Value::as_bool)
    {
        body.insert("parallel_tool_calls".to_string(), Value::Bool(parallel));
    }
    if let Some(max_tokens) = object.get("max_output_tokens").and_then(Value::as_u64) {
        body.insert(provider.compat.max_tokens_field.clone(), json!(max_tokens));
    }
    if provider.compat.supports_reasoning_effort
        && let Some(effort) = object
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
    {
        body.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    if provider.compat.supports_stream_options {
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    Ok(ChatBuild {
        body: Value::Object(body),
        tool_routes,
        upstream_model,
    })
}

fn collect_additional_tools(input: Option<&Value>, out: &mut Vec<Value>) {
    let Some(Value::Array(items)) = input else {
        return;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools")
            && let Some(tools) = item.get("tools").and_then(Value::as_array)
        {
            out.extend(tools.iter().cloned());
        }
    }
}

fn collect_tools(
    tools: &[Value],
    namespace: Option<&str>,
    routes: &mut ToolRoutes,
    out: &mut Vec<Value>,
) {
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    let chat_name = sanitized_chat_name(namespace, name);
                    let mut function = Map::new();
                    function.insert("name".to_string(), Value::String(chat_name.clone()));
                    if let Some(description) = tool.get("description").and_then(Value::as_str) {
                        function.insert(
                            "description".to_string(),
                            Value::String(description.to_string()),
                        );
                    }
                    function.insert(
                        "parameters".to_string(),
                        tool.get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                    );
                    if let Some(strict) = tool.get("strict").cloned() {
                        function.insert("strict".to_string(), strict);
                    }
                    out.push(json!({"type": "function", "function": function}));
                    routes.insert(ToolRoute {
                        chat_name,
                        original_name: name.to_string(),
                        namespace: namespace.map(str::to_string),
                        kind: ToolKind::Function,
                    });
                }
            }
            Some("custom") => {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    let chat_name = sanitized_chat_name(namespace, name);
                    let mut description = tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    description.push_str(
                        "\n\nThis is a freeform tool. Put the complete raw input text in the `input` string field. Do not wrap it in markdown fences unless the raw input itself requires them.",
                    );
                    if let Some(format) = tool.get("format")
                        && let Some(definition) = format.get("definition").and_then(Value::as_str)
                    {
                        let preview: String = definition.chars().take(2_000).collect();
                        description.push_str("\n\nInput format constraint:\n");
                        description.push_str(&preview);
                    }
                    out.push(json!({
                        "type": "function",
                        "function": {
                            "name": chat_name,
                            "description": description,
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "input": {
                                        "type": "string",
                                        "description": "Complete raw tool input."
                                    }
                                },
                                "required": ["input"],
                                "additionalProperties": false
                            }
                        }
                    }));
                    routes.insert(ToolRoute {
                        chat_name,
                        original_name: name.to_string(),
                        namespace: namespace.map(str::to_string),
                        kind: ToolKind::Custom,
                    });
                }
            }
            Some("namespace") => {
                let nested_namespace = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .or(namespace)
                    .map(str::to_string);
                if let Some(nested) = tool.get("tools").and_then(Value::as_array) {
                    collect_tools(nested, nested_namespace.as_deref(), routes, out);
                }
            }
            // Native Responses tools have no Chat Completions equivalent. Dropping them
            // is preferable to sending an invalid tool schema to a relay.
            _ => {}
        }
    }
}

fn translate_input_item(item: &Value, routes: &ToolRoutes, messages: &mut Vec<Value>) {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    match item_type {
        "message" => {
            let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                "developer" => "system",
                "system" => "system",
                "assistant" => "assistant",
                _ => "user",
            };
            let content = convert_content(item.get("content"), role);
            push_message(messages, role, content, None, None);
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let chat_name = routes
                .chat_name_for(name, namespace)
                .map(str::to_string)
                .unwrap_or_else(|| sanitized_chat_name(namespace, name));
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            push_tool_call(messages, call_id, &chat_name, arguments);
        }
        "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let namespace = item.get("namespace").and_then(Value::as_str);
            let chat_name = routes
                .chat_name_for(name, namespace)
                .map(str::to_string)
                .unwrap_or_else(|| sanitized_chat_name(namespace, name));
            let input = item.get("input").and_then(Value::as_str).unwrap_or("");
            let arguments = json!({ "input": input }).to_string();
            push_tool_call(messages, call_id, &chat_name, &arguments);
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let output = output_to_string(item.get("output"));
            push_message(
                messages,
                "tool",
                Some(Value::String(output)),
                None,
                Some(call_id.to_string()),
            );
        }
        "reasoning" => {
            // Chat Completions has no portable reasoning item. Dropping encrypted
            // reasoning is intentional; the visible conversation remains complete.
        }
        "additional_tools" => {
            // Merged into the top-level tools list by build_chat_request.
        }
        _ => {}
    }
}

fn push_tool_call(messages: &mut Vec<Value>, call_id: &str, name: &str, arguments: &str) {
    let tool_call = json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    });

    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("assistant")
    {
        if last.get("tool_calls").is_none() {
            last["tool_calls"] = Value::Array(Vec::new());
        }
        if let Some(calls) = last.get_mut("tool_calls").and_then(Value::as_array_mut) {
            calls.push(tool_call);
            return;
        }
    }

    messages.push(json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": [tool_call],
    }));
}

fn push_message(
    messages: &mut Vec<Value>,
    role: &str,
    content: Option<Value>,
    tool_calls: Option<Vec<Value>>,
    tool_call_id: Option<String>,
) {
    if role == "system"
        && let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("system")
    {
        let merged = merge_system_content(last.get("content"), content);
        last["content"] = merged;
        return;
    }

    let mut message = Map::new();
    message.insert("role".to_string(), Value::String(role.to_string()));
    if let Some(content) = content {
        message.insert("content".to_string(), content);
    }
    if let Some(tool_calls) = tool_calls {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    if let Some(tool_call_id) = tool_call_id {
        message.insert("tool_call_id".to_string(), Value::String(tool_call_id));
    }
    messages.push(Value::Object(message));
}

fn merge_system_content(existing: Option<&Value>, incoming: Option<Value>) -> Value {
    let left = existing.map(content_text).unwrap_or_default();
    let right = incoming.as_ref().map(content_text).unwrap_or_default();
    match (left.is_empty(), right.is_empty()) {
        (true, true) => Value::String(String::new()),
        (true, false) => Value::String(right),
        (false, true) => Value::String(left),
        (false, false) => Value::String(format!("{left}\n\n{right}")),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn convert_content(content: Option<&Value>, role: &str) -> Option<Value> {
    let content = content?;
    match content {
        Value::String(text) => Some(Value::String(text.clone())),
        Value::Array(parts) => {
            let mut output_parts = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            output_parts.push(json!({"type":"text","text":text}));
                        }
                    }
                    Some("input_image") if role != "assistant" => {
                        if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                            let mut image_url = json!({"url": url});
                            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                                image_url["detail"] = Value::String(detail.to_string());
                            }
                            output_parts.push(json!({
                                "type": "image_url",
                                "image_url": image_url
                            }));
                        } else if let Some(image_url) = part.get("image_url")
                            && image_url.is_object()
                        {
                            output_parts.push(json!({
                                "type": "image_url",
                                "image_url": image_url
                            }));
                        }
                    }
                    Some("input_file") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            output_parts.push(json!({"type":"text","text":text}));
                        }
                    }
                    _ => {}
                }
            }

            if output_parts.is_empty() {
                None
            } else if output_parts.len() == 1
                && output_parts[0].get("type").and_then(Value::as_str) == Some("text")
            {
                output_parts[0].get("text").cloned()
            } else {
                Some(Value::Array(output_parts))
            }
        }
        Value::Object(_) => convert_content(Some(&Value::Array(vec![content.clone()])), role),
        _ => None,
    }
}

fn output_to_string(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn map_tool_choice(value: Option<&Value>, routes: &ToolRoutes) -> Option<Value> {
    let value = value?;
    match value {
        Value::String(mode) => Some(Value::String(mode.clone())),
        Value::Object(object) => {
            let name = object.get("name").and_then(Value::as_str)?;
            let namespace = object.get("namespace").and_then(Value::as_str);
            let chat_name = routes
                .chat_name_for(name, namespace)
                .map(str::to_string)
                .unwrap_or_else(|| sanitized_chat_name(namespace, name));
            Some(json!({
                "type": "function",
                "function": { "name": chat_name }
            }))
        }
        _ => None,
    }
}

fn original_key(namespace: Option<&str>, name: &str) -> String {
    format!("{}\u{0}{name}", namespace.unwrap_or(""))
}

fn sanitized_chat_name(namespace: Option<&str>, name: &str) -> String {
    let combined = match namespace {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}__{name}"),
        _ => name.to_string(),
    };
    sanitize_tool_name(&combined)
}

fn sanitize_tool_name(name: &str) -> String {
    let mut output = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        output.push_str("tool");
    }
    if output.len() > 64 {
        let hash = fnv1a(name);
        output.truncate(55);
        output.push('_');
        output.push_str(&format!("{hash:08x}"));
    }
    output
}

fn fnv1a(value: &str) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn provider() -> ProviderConfig {
        toml::from_str(
            r#"
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"
            [models]
            "tomato-codex" = "deepseek-chat"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn translates_codex_like_history_and_custom_tool() {
        let request = json!({
            "model": "tomato-codex",
            "input": [
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"You are Codex."}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"edit a file"}]},
                {"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"a.txt"},
                {"type":"custom_tool_call","call_id":"call_2","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"},
                {"type":"custom_tool_call_output","call_id":"call_2","output":"done"}
            ],
            "tools": [
                {"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}},
                {"type":"custom","name":"apply_patch","description":"Apply a patch","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}
            ],
            "stream": true
        });

        let build = build_chat_request(&request, &provider()).unwrap();
        let body = build.body.as_object().unwrap();
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["messages"].as_array().unwrap().len(), 6);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "exec_command"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][4]["role"], "assistant");
        assert_eq!(
            body["messages"][4]["tool_calls"][0]["function"]["name"],
            "apply_patch"
        );
        assert_eq!(body["messages"][5]["role"], "tool");
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["tools"][1]["function"]["parameters"]["required"][0],
            "input"
        );
    }

    #[test]
    fn flattens_namespaced_tools() {
        let request = json!({
            "model": "tomato-codex",
            "input": "hi",
            "tools": [
                {
                    "type":"namespace",
                    "name":"functions",
                    "tools":[
                        {"type":"function","name":"wait","parameters":{"type":"object"}}
                    ]
                }
            ]
        });
        let build = build_chat_request(&request, &provider()).unwrap();
        assert_eq!(
            build.body["tools"][0]["function"]["name"],
            "functions__wait"
        );
        assert_eq!(
            build
                .tool_routes
                .resolve("functions__wait")
                .unwrap()
                .namespace
                .as_deref(),
            Some("functions")
        );
    }

    #[test]
    fn merges_additional_tools() {
        let request = json!({
            "model": "tomato-codex",
            "input": [
                {
                    "type":"additional_tools",
                    "tools":[{"type":"function","name":"wait","parameters":{"type":"object"}}]
                },
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]
        });
        let build = build_chat_request(&request, &provider()).unwrap();
        assert_eq!(build.body["tools"][0]["function"]["name"], "wait");
    }
}
