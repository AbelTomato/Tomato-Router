use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::Config;
use crate::metrics::Metrics;
use crate::provider::ProviderRegistry;
use crate::router::ModelRouter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<ModelRouter>,
    pub api_keys: Arc<Vec<String>>,
    pub metrics: Arc<Metrics>,
    pub started_at: SystemTime,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self> {
        let api_keys = Arc::new(config.resolved_server_api_keys()?);
        let providers = Arc::new(ProviderRegistry::build(&config)?);
        let router = Arc::new(ModelRouter::new(&config, &providers)?);
        Ok(Self {
            config: Arc::new(config),
            providers,
            router,
            api_keys,
            metrics: Arc::new(Metrics::default()),
            started_at: SystemTime::now(),
        })
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at
            .elapsed()
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    pub fn started_at_unix(&self) -> u64 {
        self.started_at
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }
}
