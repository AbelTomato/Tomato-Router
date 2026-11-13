use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::config::{Config, RouteConfig};
use crate::provider::{Provider, ProviderRegistry};

pub struct ModelRouter {
    providers: Vec<Arc<Provider>>,
    routes: Vec<RouteConfig>,
}

impl ModelRouter {
    pub fn new(config: &Config, registry: &ProviderRegistry) -> Result<Self> {
        for route in &config.routes {
            for provider_id in &route.chain {
                if registry.get(provider_id).is_none() {
                    bail!(
                        "route `{}` references unknown provider `{provider_id}`",
                        route.model
                    );
                }
            }
        }

        Ok(Self {
            providers: registry.all().to_vec(),
            routes: config.routes.clone(),
        })
    }

    pub fn chain_for(&self, model: &str) -> Vec<Arc<Provider>> {
        let route = self
            .routes
            .iter()
            .find(|route| route.model == model)
            .or_else(|| self.routes.iter().find(|route| route.model == "*"));

        match route {
            Some(route) => route
                .chain
                .iter()
                .filter_map(|provider_id| {
                    self.providers
                        .iter()
                        .find(|provider| provider.config.id == *provider_id)
                        .cloned()
                })
                .collect(),
            // No explicit route means the provider declaration order is the
            // strict priority order. This keeps a one-provider config trivial.
            None => self.providers.clone(),
        }
    }

    pub fn model_ids(&self) -> Vec<String> {
        let mut models = BTreeSet::new();
        for route in &self.routes {
            if route.model != "*" {
                models.insert(route.model.clone());
            }
        }
        for provider in &self.providers {
            for model in provider.config.models.keys() {
                models.insert(model.clone());
            }
        }
        if models.is_empty() {
            for provider in &self.providers {
                models.insert(provider.config.id.clone());
            }
        }
        models.into_iter().collect()
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }
}
