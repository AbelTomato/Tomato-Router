//! Codex user configuration synchronization.
//!
//! Only the fields owned by Tomato Router are changed. `toml_edit` keeps the
//! rest of the document, including comments and unrelated Codex settings.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};
use url::Url;

use crate::Config;
use crate::fsutil::{atomic_write, next_backup_path};

pub const DEFAULT_PROVIDER_ID: &str = "tomato";
pub const DEFAULT_ENV_KEY: &str = "TOMATO_ROUTER_KEY";

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub codex_config: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub provider_id: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub dry_run: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            codex_config: None,
            codex_home: None,
            provider_id: DEFAULT_PROVIDER_ID.to_string(),
            model: None,
            base_url: None,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub path: PathBuf,
    pub model: String,
    pub provider_id: String,
    pub base_url: String,
    pub changed: bool,
    pub backup: Option<PathBuf>,
    pub content: String,
}

/// Build a Codex configuration update without writing it.
pub fn plan_sync(config: &Config, options: &SyncOptions) -> Result<SyncPlan> {
    validate_provider_id(&options.provider_id)?;
    let path = resolve_config_path(options)?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| default_model(config));
    if model.trim().is_empty() {
        bail!("Codex model must not be empty");
    }
    let base_url = options
        .base_url
        .as_deref()
        .map(normalize_base_url)
        .transpose()?
        .unwrap_or_else(|| gateway_base_url(&config.server.listen));

    let original = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read Codex config `{}`", path.display()));
        }
    };
    let content = render_update(&original, &options.provider_id, &model, &base_url)?;
    let changed = content != original;

    let backup = if changed && !original.is_empty() {
        Some(next_backup_path(&path))
    } else {
        None
    };

    Ok(SyncPlan {
        path,
        model,
        provider_id: options.provider_id.clone(),
        base_url,
        changed,
        backup,
        content,
    })
}

/// Apply a planned update, returning the final plan including its backup path.
pub fn sync(config: &Config, options: &SyncOptions) -> Result<SyncPlan> {
    let plan = plan_sync(config, options)?;
    if options.dry_run || !plan.changed {
        return Ok(plan);
    }

    if let Some(parent) = plan
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }

    if let Some(backup) = &plan.backup {
        fs::copy(&plan.path, backup).with_context(|| {
            format!(
                "failed to back up Codex config `{}` to `{}`",
                plan.path.display(),
                backup.display()
            )
        })?;
    }

    atomic_write(&plan.path, plan.content.as_bytes())
        .with_context(|| format!("failed to update Codex config `{}`", plan.path.display()))?;
    Ok(plan)
}

fn render_update(original: &str, provider_id: &str, model: &str, base_url: &str) -> Result<String> {
    let mut document = if original.trim().is_empty() {
        DocumentMut::new()
    } else {
        original
            .parse::<DocumentMut>()
            .context("failed to parse Codex config TOML")?
    };

    document["model"] = value(model);
    document["model_provider"] = value(provider_id);

    let providers = document
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_mut()
        .context("Codex `model_providers` must be a TOML table")?;
    let provider = providers
        .entry(provider_id)
        .or_insert(Item::Table(Table::new()));
    let provider = provider
        .as_table_mut()
        .with_context(|| format!("Codex provider `{provider_id}` must be a TOML table"))?;

    provider["name"] = value("Tomato Router");
    provider["base_url"] = value(base_url);
    provider["env_key"] = value(DEFAULT_ENV_KEY);
    provider["wire_api"] = value("responses");

    Ok(document.to_string())
}

fn resolve_config_path(options: &SyncOptions) -> Result<PathBuf> {
    if let Some(path) = &options.codex_config {
        return Ok(path.clone());
    }
    if let Some(home) = &options.codex_home {
        return Ok(home.join("config.toml"));
    }
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.trim().is_empty()
    {
        return Ok(PathBuf::from(home).join("config.toml"));
    }
    let home = default_home_dir()
        .context("cannot find the user home directory; set CODEX_HOME or pass --codex-config")?;
    Ok(home.join(".codex").join("config.toml"))
}

fn default_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    }
}

fn default_model(config: &Config) -> String {
    config
        .routes
        .iter()
        .find(|route| route.model != "*")
        .map(|route| route.model.clone())
        .or_else(|| {
            config
                .providers
                .iter()
                .flat_map(|provider| provider.models.keys())
                .next()
                .cloned()
        })
        .unwrap_or_else(|| "gpt-5.5".to_string())
}

fn gateway_base_url(listen: &str) -> String {
    let listen = listen.trim();
    let (host, port) = listen
        .rsplit_once(':')
        .map(|(host, port)| (host.trim_matches(['[', ']']), port))
        .unwrap_or((listen, "8787"));
    let host = match host {
        "0.0.0.0" | "::" | "" => "127.0.0.1",
        host => host,
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("http://{host}:{port}/v1")
}

fn normalize_base_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).with_context(|| format!("invalid Codex base URL `{raw}`"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Codex base URL must use http or https");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("Codex base URL must not contain a query or fragment");
    }
    if url.path() == "/" || url.path().is_empty() {
        url.set_path("/v1");
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn validate_provider_id(provider_id: &str) -> Result<()> {
    if provider_id.is_empty()
        || provider_id == "."
        || provider_id == ".."
        || provider_id.contains(['/', '\\'])
        || provider_id.chars().any(char::is_control)
    {
        bail!(
            "Codex provider id must be a non-empty TOML key without `/`, `\\`, or control characters"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config() -> Config {
        toml::from_str(
            r#"
            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"

            [providers.models]
            "gpt-test" = "upstream-test"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn updates_only_codex_owned_fields_and_preserves_comments() {
        let _config = config();
        let input = r#"# Keep this comment
model = "old"
model_reasoning_effort = "high"
model_provider = "old-provider"

[model_providers.tomato]
name = "Old name"
base_url = "http://old/v1"
env_key = "OLD_KEY"
wire_api = "chat"
requires_openai_auth = true

[features]
foo = true
"#;
        let output =
            render_update(input, "tomato", "gpt-test", "http://127.0.0.1:8787/v1").unwrap();
        assert!(output.contains("# Keep this comment"));
        assert!(output.contains("model_reasoning_effort = \"high\""));
        assert!(output.contains("requires_openai_auth = true"));
        assert!(output.contains("foo = true"));
        assert!(output.contains("model = \"gpt-test\""));
        assert!(output.contains("wire_api = \"responses\""));
    }

    #[test]
    fn creates_provider_table_and_is_idempotent() {
        let first =
            render_update("# user\n", "tomato", "gpt-test", "http://127.0.0.1:8787/v1").unwrap();
        let second =
            render_update(&first, "tomato", "gpt-test", "http://127.0.0.1:8787/v1").unwrap();
        assert_eq!(first, second);
        let parsed = first.parse::<DocumentMut>().unwrap();
        assert_eq!(parsed["model"].as_str(), Some("gpt-test"));
        assert_eq!(parsed["model_provider"].as_str(), Some("tomato"));
    }

    #[test]
    fn derives_safe_endpoint_from_listen_address() {
        assert_eq!(
            gateway_base_url("127.0.0.1:8787"),
            "http://127.0.0.1:8787/v1"
        );
        assert_eq!(gateway_base_url("0.0.0.0:9000"), "http://127.0.0.1:9000/v1");
        assert_eq!(gateway_base_url("[::1]:8787"), "http://[::1]:8787/v1");
    }

    #[test]
    fn explicit_config_path_wins_over_home_options() {
        let options = SyncOptions {
            codex_config: Some(PathBuf::from("explicit-config.toml")),
            codex_home: Some(PathBuf::from("ignored-home")),
            ..Default::default()
        };
        assert_eq!(
            resolve_config_path(&options).unwrap(),
            PathBuf::from("explicit-config.toml")
        );
    }

    #[test]
    fn writes_backup_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!(
            "tomato-router-codex-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        let original = "# preserve\nmodel = \"old\"\n[features]\nfoo = true\n";
        fs::write(&path, original).unwrap();

        let options = SyncOptions {
            codex_config: Some(path.clone()),
            model: Some("gpt-test".to_string()),
            ..Default::default()
        };
        let first = sync(&config(), &options).unwrap();
        let backup = first.backup.clone().unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
        assert!(fs::read_to_string(&path).unwrap().contains("gpt-test"));

        let second = sync(&config(), &options).unwrap();
        assert!(!second.changed);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }
}
