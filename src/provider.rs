use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{AuthKind, Config, ProviderConfig};
use crate::error::ErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    Full,
    Timeout,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakerState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

struct BreakerInner {
    state: BreakerState,
    failures: u32,
}

pub struct Provider {
    pub config: ProviderConfig,
    pub client: reqwest::Client,
    api_key: Option<String>,
    semaphore: Arc<Semaphore>,
    waiting: AtomicUsize,
    breaker: Mutex<BreakerInner>,
}

impl Provider {
    pub fn new(config: ProviderConfig, server_connect_timeout_ms: u64) -> Result<Self> {
        let api_key = config
            .resolve_api_key()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("provider `{}` authentication", config.id))?;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(server_connect_timeout_ms))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .user_agent(format!("tomato-router/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .with_context(|| format!("provider `{}`: failed to build HTTP client", config.id))?;

        let semaphore = Arc::new(Semaphore::new(config.max_concurrency));
        Ok(Self {
            config,
            client,
            api_key,
            semaphore,
            waiting: AtomicUsize::new(0),
            breaker: Mutex::new(BreakerInner {
                state: BreakerState::Closed,
                failures: 0,
            }),
        })
    }

    pub fn display_name(&self) -> &str {
        self.config.display_name()
    }

    pub fn endpoint(&self, path: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with(&format!("/{path}")) {
            base.to_string()
        } else {
            format!("{base}/{path}")
        }
    }

    pub fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, self.endpoint(path))
            .query(&self.config.query);

        for (name, value) in &self.config.headers {
            if !name.trim().is_empty() {
                request = request.header(name, value);
            }
        }

        if let Some(auth) = &self.config.auth {
            match auth.kind {
                AuthKind::Bearer => {
                    if let Some(key) = &self.api_key {
                        request = request.bearer_auth(key);
                    }
                }
                AuthKind::Header => {
                    if let Some(key) = &self.api_key {
                        let header = auth.header.as_deref().unwrap_or("x-api-key");
                        let prefix = auth.prefix.as_deref().unwrap_or("");
                        request = request.header(header, format!("{prefix}{key}"));
                    }
                }
                AuthKind::Query => {
                    if let Some(key) = &self.api_key {
                        let param = auth.query_param.as_deref().unwrap_or("api_key");
                        request = request.query(&[(param, key.as_str())]);
                    }
                }
                AuthKind::None => {}
            }
        } else if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        request
    }

    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, QueueError> {
        if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
            return Ok(permit);
        }

        if self.config.max_queue == 0 {
            return Err(QueueError::Full);
        }

        let previous = self.waiting.fetch_add(1, Ordering::SeqCst);
        if previous >= self.config.max_queue {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(QueueError::Full);
        }

        let result = tokio::time::timeout(
            Duration::from_millis(self.config.queue_timeout_ms),
            self.semaphore.clone().acquire_owned(),
        )
        .await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);

        match result {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(QueueError::Closed),
            Err(_) => Err(QueueError::Timeout),
        }
    }

    pub fn queue_depth(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    pub fn inflight(&self) -> usize {
        self.config
            .max_concurrency
            .saturating_sub(self.semaphore.available_permits())
    }

    pub fn breaker_available(&self) -> bool {
        let breaker = self.breaker.lock().expect("breaker mutex poisoned");
        match breaker.state {
            BreakerState::Closed | BreakerState::HalfOpen => true,
            BreakerState::Open { until } => Instant::now() >= until,
        }
    }

    pub fn begin_attempt(&self) -> bool {
        let mut breaker = self.breaker.lock().expect("breaker mutex poisoned");
        match breaker.state {
            BreakerState::Closed | BreakerState::HalfOpen => true,
            BreakerState::Open { until } if Instant::now() >= until => {
                breaker.state = BreakerState::HalfOpen;
                true
            }
            BreakerState::Open { .. } => false,
        }
    }

    pub fn record_success(&self) {
        let mut breaker = self.breaker.lock().expect("breaker mutex poisoned");
        breaker.state = BreakerState::Closed;
        breaker.failures = 0;
    }

    pub fn record_failure(&self, kind: ErrorKind) {
        if !kind.should_trip_breaker() {
            return;
        }
        let mut breaker = self.breaker.lock().expect("breaker mutex poisoned");
        breaker.failures = breaker.failures.saturating_add(1);
        let should_open = matches!(breaker.state, BreakerState::HalfOpen)
            || kind == ErrorKind::Auth
            || breaker.failures >= self.config.failure_threshold;
        if should_open {
            let multiplier = u64::from(kind.cooldown_multiplier()).max(1);
            let cooldown = Duration::from_millis(
                self.config
                    .cooldown_ms
                    .saturating_mul(multiplier)
                    .max(1_000),
            );
            breaker.state = BreakerState::Open {
                until: Instant::now() + cooldown,
            };
            breaker.failures = 0;
        }
    }

    pub fn status(&self) -> Value {
        let breaker = self.breaker.lock().expect("breaker mutex poisoned");
        let (state, cooldown_until) = match breaker.state {
            BreakerState::Closed => ("closed", None),
            BreakerState::HalfOpen => ("half_open", None),
            BreakerState::Open { until } => {
                let remaining = until.saturating_duration_since(Instant::now());
                ("open", Some(remaining.as_millis() as u64))
            }
        };
        json!({
            "id": self.config.id,
            "name": self.display_name(),
            "base_url": self.config.base_url,
            "wire_api": self.config.wire_api.to_string(),
            "max_concurrency": self.config.max_concurrency,
            "max_queue": self.config.max_queue,
            "queue_timeout_ms": self.config.queue_timeout_ms,
            "inflight": self.inflight(),
            "queue_depth": self.queue_depth(),
            "breaker": state,
            "cooldown_ms_remaining": cooldown_until,
            "consecutive_failures": breaker.failures,
        })
    }
}

pub struct ProviderLease {
    pub provider: Arc<Provider>,
    _permit: OwnedSemaphorePermit,
}

impl ProviderLease {
    pub fn new(provider: Arc<Provider>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            provider,
            _permit: permit,
        }
    }

    pub fn success(&self) {
        self.provider.record_success();
    }

    pub fn failure(&self, kind: ErrorKind) {
        self.provider.record_failure(kind);
    }
}

pub struct ProviderRegistry {
    providers: Vec<Arc<Provider>>,
    by_id: HashMap<String, Arc<Provider>>,
}

impl ProviderRegistry {
    pub fn build(config: &Config) -> Result<Self> {
        let mut providers = Vec::with_capacity(config.providers.len());
        let mut by_id = HashMap::with_capacity(config.providers.len());
        for provider_config in &config.providers {
            if provider_config.id.trim().is_empty() {
                bail!("provider id must not be empty");
            }
            let provider = Arc::new(Provider::new(
                provider_config.clone(),
                config.server.connect_timeout_ms,
            )?);
            if by_id
                .insert(provider_config.id.clone(), provider.clone())
                .is_some()
            {
                bail!("duplicate provider id `{}`", provider_config.id);
            }
            providers.push(provider);
        }
        Ok(Self { providers, by_id })
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Provider>> {
        self.by_id.get(id)
    }

    pub fn all(&self) -> &[Arc<Provider>] {
        &self.providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn provider(max_concurrency: usize, max_queue: usize) -> Provider {
        let config: ProviderConfig = toml::from_str(&format!(
            r#"
            id = "test"
            base_url = "http://127.0.0.1:1/v1"
            api_key = "sk-test"
            max_concurrency = {max_concurrency}
            max_queue = {max_queue}
            queue_timeout_ms = 10
            "#
        ))
        .unwrap();
        Provider::new(config, 1_000).unwrap()
    }

    #[tokio::test]
    async fn queue_reports_full_without_waiting_when_max_queue_is_zero() {
        let provider = provider(1, 0);
        let _first = provider.acquire().await.unwrap();
        assert_eq!(provider.acquire().await.unwrap_err(), QueueError::Full);
    }

    #[tokio::test]
    async fn queue_waits_then_times_out() {
        let provider = Arc::new(provider(1, 1));
        let _first = provider.acquire().await.unwrap();
        let second = provider.clone();
        let handle = tokio::spawn(async move { second.acquire().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(handle.await.unwrap().unwrap_err(), QueueError::Timeout);
        assert_eq!(provider.queue_depth(), 0);
    }

    #[test]
    fn auth_failure_opens_breaker_immediately() {
        let config: ProviderConfig = toml::from_str(
            r#"
            id = "test"
            base_url = "http://127.0.0.1:1/v1"
            api_key = "sk-test"
            failure_threshold = 10
            "#,
        )
        .unwrap();
        let provider = Provider::new(config, 1_000).unwrap();
        assert!(provider.begin_attempt());
        provider.record_failure(ErrorKind::Auth);
        assert!(!provider.breaker_available());
    }

    #[test]
    fn breaker_opens_and_recovers_after_cooldown() {
        let mut config: ProviderConfig = toml::from_str(
            r#"
            id = "test"
            base_url = "http://127.0.0.1:1/v1"
            api_key = "sk-test"
            failure_threshold = 1
            cooldown_ms = 10
            "#,
        )
        .unwrap();
        config.failure_threshold = 1;
        let provider = Provider::new(config, 1_000).unwrap();
        assert!(provider.breaker_available());
        assert!(provider.begin_attempt());
        provider.record_failure(ErrorKind::Server);
        assert!(!provider.breaker_available());
        // Cooldowns have a 1 second floor to avoid breaker flapping.
        std::thread::sleep(Duration::from_millis(1_050));
        assert!(provider.breaker_available());
        assert!(provider.begin_attempt());
        provider.record_success();
        assert!(provider.breaker_available());
    }
}
