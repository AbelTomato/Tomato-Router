//! Configuration draft, validation report and file persistence for the TUI.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{Config, is_loopback_listen};
use crate::fsutil::{atomic_write, backup_file};

const EXAMPLE_CONFIG: &str = include_str!("../../config/tomato.example.toml");

pub struct LoadedDraft {
    pub path: PathBuf,
    pub draft: Option<Draft>,
    pub error: Option<String>,
    pub missing: bool,
}

pub struct Draft {
    pub path: PathBuf,
    pub config: Config,
    pub saved_config: Config,
}

#[derive(Debug)]
pub struct SaveOutcome {
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub summary: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub can_save: bool,
    pub can_start: bool,
}

impl LoadedDraft {
    pub fn load(path: &Path) -> Self {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    path: path.to_path_buf(),
                    draft: None,
                    error: Some(format!("config `{}` does not exist", path.display())),
                    missing: true,
                };
            }
            Err(error) => {
                return Self {
                    path: path.to_path_buf(),
                    draft: None,
                    error: Some(format!("failed to read `{}`: {error}", path.display())),
                    missing: false,
                };
            }
        };

        match toml::from_str::<Config>(&raw) {
            Ok(config) => Self {
                path: path.to_path_buf(),
                draft: Some(Draft {
                    path: path.to_path_buf(),
                    saved_config: config.clone(),
                    config,
                }),
                error: None,
                missing: false,
            },
            Err(error) => Self {
                path: path.to_path_buf(),
                draft: None,
                error: Some(format!("failed to parse `{}`: {error}", path.display())),
                missing: false,
            },
        }
    }
}

impl Draft {
    pub fn is_dirty(&self) -> bool {
        self.config != self.saved_config
    }

    pub fn report(&self) -> Report {
        let mut report = Report {
            summary: self.config.summary_lines(),
            can_save: true,
            can_start: true,
            ..Report::default()
        };

        if let Err(error) = self.config.validate_structure() {
            report.errors.push(error.to_string());
            report.can_save = false;
            report.can_start = false;
        }

        for (id, ok, detail) in self.config.provider_env_status() {
            if !ok {
                report
                    .warnings
                    .push(format!("provider `{id}` cannot start yet: {detail}"));
                report.can_start = false;
            }
        }

        if let Err(error) = self.config.resolved_server_api_keys() {
            report
                .warnings
                .push(format!("server.api_keys cannot be resolved: {error}"));
            report.can_start = false;
        }

        if !self.config.server.require_auth && !is_loopback_listen(&self.config.server.listen) {
            report.warnings.push(
                "server.listen is not loopback while require_auth = false; this is unsafe"
                    .to_string(),
            );
        }
        if self.config.server.log_request_bodies {
            report.warnings.push(
                "log_request_bodies = true: prompts and tool output will appear in the log view"
                    .to_string(),
            );
        }
        for provider in &self.config.providers {
            if provider.api_key.is_some() && provider.api_key_env.is_some() {
                report.warnings.push(format!(
                    "provider `{}` sets both api_key and api_key_env; api_key wins",
                    provider.id
                ));
            }
        }
        if self.config.routes.is_empty() {
            report.warnings.push(
                "no routes are configured; provider declaration order is the default chain"
                    .to_string(),
            );
        }

        report
    }

    pub fn save(&mut self) -> Result<SaveOutcome> {
        self.config.validate_structure()?;
        let text = self.config.to_toml_pretty()?;
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create `{}`", parent.display()))?;
        }
        let backup = backup_file(&self.path)?;
        atomic_write(&self.path, text.as_bytes())
            .with_context(|| format!("failed to save `{}`", self.path.display()))?;
        self.saved_config = self.config.clone();
        Ok(SaveOutcome { backup })
    }
}

pub fn create_from_example(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("config `{}` already exists", path.display());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create `{}`", path.display()))?;
    std::io::Write::write_all(&mut file, EXAMPLE_CONFIG.as_bytes())
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(())
}

pub fn parse_comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn format_comma_list(items: &[String]) -> String {
    items.join(", ")
}

pub fn parse_model_map(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for entry in raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((client, upstream)) = entry.split_once('=') else {
            return Err(format!(
                "model mapping `{entry}` must use the form client=upstream"
            ));
        };
        let client = client.trim();
        let upstream = upstream.trim();
        if client.is_empty() || upstream.is_empty() {
            return Err(format!("model mapping `{entry}` has an empty side"));
        }
        if map
            .insert(client.to_string(), upstream.to_string())
            .is_some()
        {
            return Err(format!("duplicate model mapping for `{client}`"));
        }
    }
    Ok(map)
}

pub fn format_model_map(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(client, upstream)| format!("{client}={upstream}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_map() {
        let map = parse_model_map("a=one, b=two").unwrap();
        assert_eq!(map.get("a").map(String::as_str), Some("one"));
        assert_eq!(format_model_map(&map), "a=one, b=two");
        assert!(parse_model_map("bad").is_err());
        assert!(parse_model_map("a=one, a=two").is_err());
    }

    #[test]
    fn save_round_trips_config_and_creates_backup() {
        let path = std::env::temp_dir().join(format!(
            "tomato-tui-draft-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"
            "#,
        )
        .unwrap();
        config.validate_structure().unwrap();
        let mut draft = Draft {
            path: path.clone(),
            saved_config: config.clone(),
            config,
        };
        let first = draft.save().unwrap();
        assert!(first.backup.is_none());
        assert!(!draft.is_dirty());

        draft.config.providers[0].name = Some("Renamed".to_string());
        let second = draft.save().unwrap();
        assert!(second.backup.is_some());
        let reloaded = LoadedDraft::load(&path);
        assert_eq!(
            reloaded.draft.unwrap().config.providers[0].name.as_deref(),
            Some("Renamed")
        );

        let _ = fs::remove_file(&path);
        if let Some(backup) = second.backup {
            let _ = fs::remove_file(backup);
        }
    }
}
