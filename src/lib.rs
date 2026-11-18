//! Tomato Router: a local, strict-priority OpenAI Responses gateway.
//!
//! The public surface is the Responses API. Requests can be served either by
//! providers that natively speak Responses (`wire_api = "responses"`), by
//! Chat Completions providers through the translation adapter
//! (`wire_api = "chat"`), or by Anthropic Messages providers through the native
//! Claude adapter (`wire_api = "messages"`).

pub mod codex;
pub mod config;
pub mod error;
pub mod fsutil;
pub mod gateway;
pub mod metrics;
pub mod protocol;
pub mod provider;
pub mod router;
pub mod server;
pub mod state;
pub mod tui;
pub mod upstream;

pub use config::Config;
pub use server::build_app;
pub use state::AppState;
