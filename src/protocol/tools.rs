use std::collections::HashMap;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Function,
    Custom,
}

#[derive(Debug, Clone)]
pub struct ToolRoute {
    /// Tool name sent to the upstream protocol.
    pub upstream_name: String,
    /// Tool name Codex originally used.
    pub original_name: String,
    pub namespace: Option<String>,
    pub kind: ToolKind,
}

#[derive(Debug, Clone, Default)]
pub struct ToolRoutes {
    by_upstream_name: HashMap<String, ToolRoute>,
    by_original: HashMap<String, String>,
}

impl ToolRoutes {
    pub fn resolve(&self, upstream_name: &str) -> Option<&ToolRoute> {
        self.by_upstream_name.get(upstream_name)
    }

    pub fn upstream_name_for(&self, original_name: &str, namespace: Option<&str>) -> Option<&str> {
        self.by_original
            .get(&original_key(namespace, original_name))
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.by_upstream_name.is_empty()
    }

    fn insert(&mut self, route: ToolRoute) {
        let mut upstream_name = route.upstream_name.clone();
        let base = upstream_name.clone();
        let mut suffix = 2;
        while self.by_upstream_name.contains_key(&upstream_name) {
            upstream_name = format!("{base}__{suffix}");
            suffix += 1;
        }
        let mut route = route;
        route.upstream_name = upstream_name.clone();
        self.by_original.insert(
            original_key(route.namespace.as_deref(), &route.original_name),
            upstream_name.clone(),
        );
        self.by_upstream_name.insert(upstream_name, route);
    }
}

/// Protocol-neutral tool definition produced from a Responses tool declaration.
#[derive(Debug, Clone)]
pub struct UpstreamTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub strict: Option<Value>,
    pub kind: ToolKind,
}

pub fn collect_additional_tools(input: Option<&Value>, out: &mut Vec<Value>) {
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

pub fn collect_tools(
    tools: &[Value],
    namespace: Option<&str>,
    routes: &mut ToolRoutes,
    out: &mut Vec<UpstreamTool>,
) {
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                if let Some(original_name) = tool.get("name").and_then(Value::as_str) {
                    let name = upstream_name_for(namespace, original_name);
                    out.push(UpstreamTool {
                        name: name.clone(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        input_schema: tool
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                        strict: tool.get("strict").cloned(),
                        kind: ToolKind::Function,
                    });
                    routes.insert(ToolRoute {
                        upstream_name: name,
                        original_name: original_name.to_string(),
                        namespace: namespace.map(str::to_string),
                        kind: ToolKind::Function,
                    });
                }
            }
            Some("custom") => {
                if let Some(original_name) = tool.get("name").and_then(Value::as_str) {
                    let name = upstream_name_for(namespace, original_name);
                    out.push(UpstreamTool {
                        name: name.clone(),
                        description: Some(custom_tool_description(tool)),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "input": {
                                    "type": "string",
                                    "description": "Complete raw tool input."
                                }
                            },
                            "required": ["input"],
                            "additionalProperties": false
                        }),
                        strict: None,
                        kind: ToolKind::Custom,
                    });
                    routes.insert(ToolRoute {
                        upstream_name: name,
                        original_name: original_name.to_string(),
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
            // Native Responses tools have no portable upstream equivalent. Dropping
            // them is preferable to sending an invalid tool schema to a relay.
            _ => {}
        }
    }
}

fn custom_tool_description(tool: &Value) -> String {
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
    description
}

pub fn upstream_name_for(namespace: Option<&str>, name: &str) -> String {
    let combined = match namespace {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}__{name}"),
        _ => name.to_string(),
    };
    sanitize_tool_name(&combined)
}

pub fn resolve_upstream_name(routes: &ToolRoutes, name: &str, namespace: Option<&str>) -> String {
    routes
        .upstream_name_for(name, namespace)
        .map(str::to_string)
        .unwrap_or_else(|| upstream_name_for(namespace, name))
}

/// Recover the raw freeform input from a wrapped custom tool argument object.
pub fn custom_input(arguments: &str) -> String {
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

pub fn output_to_string(output: Option<&Value>) -> String {
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

fn original_key(namespace: Option<&str>, name: &str) -> String {
    format!("{}\u{0}{name}", namespace.unwrap_or(""))
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

    #[test]
    fn flattens_namespaces_and_deduplicates_names() {
        let tools = vec![
            json!({"type":"function","name":"wait","parameters":{"type":"object"}}),
            json!({"type":"namespace","name":"functions","tools":[
                {"type":"function","name":"wait","parameters":{"type":"object"}}
            ]}),
        ];
        let mut routes = ToolRoutes::default();
        let mut out = Vec::new();
        collect_tools(&tools, None, &mut routes, &mut out);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "wait");
        assert_eq!(out[1].name, "functions__wait");
        assert_eq!(
            routes.upstream_name_for("wait", Some("functions")),
            Some("functions__wait")
        );
    }

    #[test]
    fn wraps_custom_tools_with_a_single_input_field() {
        let tools = vec![json!({
            "type":"custom",
            "name":"apply_patch",
            "description":"Apply a patch",
            "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}
        })];
        let mut routes = ToolRoutes::default();
        let mut out = Vec::new();
        collect_tools(&tools, None, &mut routes, &mut out);

        assert_eq!(out[0].kind, ToolKind::Custom);
        assert_eq!(out[0].input_schema["required"][0], "input");
        assert!(out[0].description.as_deref().unwrap().contains("start:"));
    }

    #[test]
    fn extracts_custom_input_and_tool_output() {
        assert_eq!(
            custom_input(r#"{"input":"*** Begin Patch"}"#),
            "*** Begin Patch"
        );
        assert_eq!(custom_input("plain"), "plain");
        assert_eq!(
            output_to_string(Some(
                &json!([{"type":"input_text","text":"a"},{"text":"b"}])
            )),
            "a\nb"
        );
    }
}
