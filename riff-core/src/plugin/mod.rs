pub mod capabilities;
pub mod catalog;
pub mod events;
pub mod manifest;
pub mod registry;
pub mod wasm_host;
pub mod wasm_loader;
pub mod wasm_editorial;
pub mod wasm_streaming;

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Streaming,
    Lyrics,
    Scrobble,
    Metadata,
    Editorial,
}

pub struct PluginContext {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub config: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub healthy: bool,
    pub message: Option<String>,
}

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn display_name(&self) -> &str;
    fn version(&self) -> &str;
    fn capabilities(&self) -> Vec<Capability>;
    fn icon_url(&self) -> Option<&str> { None }
    async fn init(&mut self, ctx: PluginContext) -> anyhow::Result<()>;
    async fn health_check(&self) -> HealthStatus;
    async fn shutdown(&self) -> anyhow::Result<()>;
}
