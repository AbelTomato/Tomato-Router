use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub require_auth: bool,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_first_byte_timeout_ms")]
    pub first_byte_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default)]
    pub log_request_bodies: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            require_auth: false,
            api_keys: Vec::new(),
            max_body_bytes: default_max_body_bytes(),
            connect_timeout_ms: default_connect_timeout_ms(),
            first_byte_timeout_ms: default_first_byte_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            log_request_bodies: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub wire_api: WireApi,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    /// Client-facing model name -> upstream model name.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default)]
    pub max_queue: usize,
    #[serde(default = "default_queue_timeout_ms")]
    pub queue_timeout_ms: u64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_retry_after_wait_ms")]
    pub retry_after_wait_ms: u64,
    #[serde(default)]
    pub compat: CompatConfig,
}

impl ProviderConfig {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub fn model_for(&self, requested: &str) -> String {
        self.models
            .get(requested)
            .cloned()
            .unwrap_or_else(|| requested.to_string())
    }

    pub fn has_auth(&self) -> bool {
        self.auth
            .as_ref()
            .map(|auth| auth.kind != AuthKind::None)
            .unwrap_or(true)
    }

    pub fn resolve_api_key(&self) -> Result<Option<String>, String> {
        if !self.has_auth() {
            return Ok(None);
        }

        if let Some(value) = &self.api_key {
            return expand_env(value).map(Some);
        }
        if let Some(name) = &self.api_key_env {
            return match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(Some(value)),
                _ => Err(format!(
                    "provider `{}` requires environment variable `{name}`, but it is not set",
                    self.id
                )),
            };
        }

        Err(format!(
            "provider `{}` has authentication enabled but no `api_key` or `api_key_env`",
            self.id
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    #[default]
    Chat,
    Responses,
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chat => f.write_str("chat"),
            Self::Responses => f.write_str("responses"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(rename = "type", default)]
    pub kind: AuthKind,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub query_param: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    #[default]
    Bearer,
    Header,
    Query,
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CompatConfig {
    /// `max_tokens` is the widest-compatibility default for relay services.
    #[serde(default = "default_max_tokens_field")]
    pub max_tokens_field: String,
    #[serde(default = "default_true")]
    pub supports_stream_options: bool,
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    #[serde(default = "default_true")]
    pub supports_parallel_tool_calls: bool,
    /// Optional provider-specific reasoning delta field, e.g. `reasoning_content`.
    #[serde(default)]
    pub reasoning_field: Option<String>,
    /// Extra top-level JSON merged into upstream Chat requests.
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            max_tokens_field: default_max_tokens_field(),
            supports_stream_options: true,
            supports_reasoning_effort: false,
            supports_parallel_tool_calls: true,
            reasoning_field: None,
            extra_body: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteConfig {
    pub model: String,
    pub chain: Vec<String>,
}

impl Config {
    /// Read and parse a configuration file without running semantic validation.
    ///
    /// The TUI uses this so a configuration with missing environment variables
    /// can still be opened and repaired interactively.
    pub fn parse(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config `{}`", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("failed to parse config `{}`", path.display()))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let config = Self::parse(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Full validation used by the CLI and by service startup.
    pub fn validate(&self) -> Result<()> {
        self.validate_structure()?;
        self.validate_runtime()
    }

    /// Validate shape, references and numeric limits without touching the
    /// process environment.
    pub fn validate_structure(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("config must define at least one provider");
        }
        if self.providers.len() > 1 {
            let mut ids = std::collections::HashSet::new();
            for provider in &self.providers {
                if !ids.insert(provider.id.as_str()) {
                    bail!("duplicate provider id `{}`", provider.id);
                }
            }
        }

        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                bail!("provider id must not be empty");
            }
            if provider.max_concurrency == 0 {
                bail!("provider `{}`: max_concurrency must be > 0", provider.id);
            }
            if provider.failure_threshold == 0 {
                bail!("provider `{}`: failure_threshold must be > 0", provider.id);
            }
            let url = Url::parse(&provider.base_url)
                .with_context(|| format!("provider `{}`: invalid base_url", provider.id))?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!(
                    "provider `{}`: base_url must use http or https, got `{}`",
                    provider.id,
                    url.scheme()
                );
            }
            if url.query().is_some() || url.fragment().is_some() {
                bail!(
                    "provider `{}`: put query params in `query`, not in base_url",
                    provider.id
                );
            }
            if let Some(extra) = &provider.compat.extra_body
                && !extra.is_object()
            {
                bail!(
                    "provider `{}`: compat.extra_body must be a JSON object",
                    provider.id
                );
            }
        }

        if self.server.listen.trim().is_empty() {
            bail!("server.listen must not be empty");
        }
        if self.server.max_body_bytes == 0 {
            bail!("server.max_body_bytes must be > 0");
        }
        if self.server.first_byte_timeout_ms == 0 {
            bail!("server.first_byte_timeout_ms must be > 0");
        }
        if self.server.stream_idle_timeout_ms == 0 {
            bail!("server.stream_idle_timeout_ms must be > 0");
        }
        if self.server.require_auth && self.server.api_keys.is_empty() {
            bail!("server.require_auth is true but server.api_keys is empty");
        }
        for key in &self.server.api_keys {
            if key.trim().is_empty() {
                bail!("server.api_keys must not contain empty keys");
            }
        }

        let ids: std::collections::HashSet<&str> =
            self.providers.iter().map(|p| p.id.as_str()).collect();
        let mut route_models = std::collections::HashSet::new();
        for route in &self.routes {
            if route.model.trim().is_empty() {
                bail!("route model must not be empty");
            }
            if !route_models.insert(route.model.as_str()) {
                bail!("duplicate route for model `{}`", route.model);
            }
            if route.chain.is_empty() {
                bail!("route `{}`: chain must not be empty", route.model);
            }
            for provider_id in &route.chain {
                if !ids.contains(provider_id.as_str()) {
                    bail!(
                        "route `{}` references unknown provider `{provider_id}`",
                        route.model
                    );
                }
            }
        }

        Ok(())
    }

    /// Validate that configured secrets can be resolved. This reads the process
    /// environment and is therefore separate from structural validation.
    pub fn validate_runtime(&self) -> Result<()> {
        for (id, ok, detail) in self.provider_env_status() {
            if !ok {
                bail!("provider `{id}`: {detail}");
            }
        }
        self.resolved_server_api_keys()?;
        Ok(())
    }

    pub fn resolved_server_api_keys(&self) -> Result<Vec<String>> {
        self.server
            .api_keys
            .iter()
            .map(|key| expand_env(key).map_err(anyhow::Error::msg))
            .collect()
    }

    pub fn provider_env_status(&self) -> Vec<(String, bool, String)> {
        self.providers
            .iter()
            .map(|provider| {
                if !provider.has_auth() {
                    return (provider.id.clone(), true, "auth disabled".to_string());
                }
                if let Some(value) = &provider.api_key {
                    return match expand_env(value) {
                        Ok(_) => (
                            provider.id.clone(),
                            true,
                            if value.contains("${") {
                                "inline environment reference".to_string()
                            } else {
                                "inline api_key".to_string()
                            },
                        ),
                        Err(error) => (provider.id.clone(), false, error),
                    };
                }
                match &provider.api_key_env {
                    Some(name) => {
                        let present = std::env::var(name)
                            .map(|value| !value.is_empty())
                            .unwrap_or(false);
                        (
                            provider.id.clone(),
                            present,
                            if present {
                                format!("{name} is set")
                            } else {
                                format!("{name} is missing")
                            },
                        )
                    }
                    None => (
                        provider.id.clone(),
                        false,
                        "no api_key or api_key_env".to_string(),
                    ),
                }
            })
            .collect()
    }

    /// Human-readable summary shared by `--check` and the TUI.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "config OK: {} provider(s), {} route(s), listen={}",
            self.providers.len(),
            self.routes.len(),
            self.server.listen
        )];
        lines.push(format!(
            "local auth: {}",
            if self.server.require_auth {
                format!("required ({} key(s))", self.server.api_keys.len())
            } else {
                "disabled".to_string()
            }
        ));
        for (id, ok, detail) in self.provider_env_status() {
            lines.push(format!(
                "provider {id}: auth={} ({detail})",
                if ok { "ok" } else { "MISSING" }
            ));
        }
        for route in &self.routes {
            lines.push(format!(
                "route {} -> {}",
                route.model,
                route.chain.join(" -> ")
            ));
        }
        if self.routes.is_empty() {
            let order = self
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>()
                .join(" -> ");
            lines.push(format!("default chain (declaration order): {order}"));
        }
        lines
    }

    /// Serialize the complete configuration. Comments and unknown fields from
    /// the original document are not preserved by this writer.
    pub fn to_toml_pretty(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize config")
    }
}

/// True when `listen` binds a loopback address (or a loopback hostname).
pub fn is_loopback_listen(listen: &str) -> bool {
    let host = listen
        .rsplit_once(':')
        .map(|(host, _)| host.trim_matches(['[', ']']))
        .unwrap_or(listen);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn expand_env(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unterminated environment reference in `{value}`"));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(format!("empty environment reference in `{value}`"));
        }
        let resolved =
            std::env::var(name).map_err(|_| format!("environment variable `{name}` is not set"))?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn default_listen() -> String {
    "127.0.0.1:8787".to_string()
}
fn default_max_body_bytes() -> usize {
    32 * 1024 * 1024
}
fn default_connect_timeout_ms() -> u64 {
    10_000
}
fn default_first_byte_timeout_ms() -> u64 {
    120_000
}
fn default_stream_idle_timeout_ms() -> u64 {
    300_000
}
fn default_max_concurrency() -> usize {
    4
}
fn default_queue_timeout_ms() -> u64 {
    800
}
fn default_cooldown_ms() -> u64 {
    30_000
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_retry_after_wait_ms() -> u64 {
    1_000
}
fn default_max_tokens_field() -> String {
    "max_tokens".to_string()
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.providers[0].wire_api, WireApi::Chat);
        assert_eq!(
            config.providers[0].model_for("tomato-codex"),
            "tomato-codex"
        );
    }

    #[test]
    fn expands_environment_references() {
        unsafe { std::env::set_var("TOMATO_TEST_VALUE", "abc") };
        assert_eq!(expand_env("x-${TOMATO_TEST_VALUE}-y").unwrap(), "x-abc-y");
    }

    #[test]
    fn rejects_unknown_route_provider() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"

            [[routes]]
            model = "tomato-codex"
            chain = ["missing"]
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn structural_validation_does_not_resolve_secrets() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "${TOMATO_TEST_MISSING_KEY_9B3F}"
            "#,
        )
        .unwrap();
        config.validate_structure().unwrap();
        assert!(config.validate_runtime().is_err());
        let status = config.provider_env_status();
        assert!(!status[0].1);
    }

    #[test]
    fn serialized_config_round_trips() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "0.0.0.0:8787"
            require_auth = true
            api_keys = ["${TOMATO_ROUTER_KEY}"]
            log_request_bodies = true

            [[providers]]
            id = "primary"
            name = "Primary"
            base_url = "https://relay.example/v1"
            wire_api = "responses"
            api_key_env = "PRIMARY_KEY"
            max_concurrency = 2
            max_queue = 3
            headers = { "X-Test" = "yes" }

            [providers.models]
            "client-model" = "upstream-model"

            [providers.compat]
            max_tokens_field = "max_completion_tokens"
            supports_stream_options = false
            extra_body = { top_k = 20 }

            [[routes]]
            model = "*"
            chain = ["primary"]
            "#,
        )
        .unwrap();
        config.validate_structure().unwrap();

        let text = config.to_toml_pretty().unwrap();
        let round: Config = toml::from_str(&text).unwrap();
        assert_eq!(config, round);
    }
}
