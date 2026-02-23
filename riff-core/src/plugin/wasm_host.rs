use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use extism::{
    host_fn, Manifest as ExtismManifest, PluginBuilder, UserData, Wasm, PTR,
};

use super::manifest::PluginManifest;
use super::{Capability, HealthStatus, Plugin, PluginContext};

type CacheStore = HashMap<String, (Vec<u8>, Instant)>;

// Host function: read from the shared TTL cache
host_fn!(host_cache_get(user_data: CacheStore; key: String) -> Vec<u8> {
    let cache = user_data.get()?;
    let cache = cache.lock().unwrap();
    match cache.get(&key) {
        Some((value, expires)) if Instant::now() < *expires => Ok(value.clone()),
        _ => Ok(vec![]),
    }
});

// Host function: write to the shared TTL cache with a TTL in seconds
host_fn!(host_cache_set(user_data: CacheStore; key: String, value: Vec<u8>, ttl_secs: u64) {
    let cache = user_data.get()?;
    let mut cache = cache.lock().unwrap();
    cache.insert(key, (value, Instant::now() + Duration::from_secs(ttl_secs)));
    Ok(())
});

pub struct WasmPluginInstance {
    pub(crate) manifest: PluginManifest,
    pub(crate) plugin: tokio::sync::Mutex<extism::Plugin>,
    #[allow(dead_code)]
    config: HashMap<String, serde_json::Value>,
}

impl WasmPluginInstance {
    pub fn load(
        wasm_bytes: &[u8],
        manifest: PluginManifest,
        config: HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Self> {
        let mut extism_manifest =
            ExtismManifest::new([Wasm::data(wasm_bytes.to_vec())])
                .with_timeout(Duration::from_secs(30));

        // Set allowed HTTP hosts from plugin permissions
        if let Some(http) = &manifest.permissions.http {
            extism_manifest = extism_manifest
                .with_allowed_hosts(http.required_hosts.iter().cloned());
        }

        // Pass plugin settings as config keys
        for (key, value) in &config {
            if let Some(s) = value.as_str() {
                extism_manifest = extism_manifest.with_config_key(key, s);
            }
        }

        let cache_store: UserData<CacheStore> = UserData::new(HashMap::new());

        let plugin = PluginBuilder::new(extism_manifest)
            .with_wasi(true)
            .with_function(
                "host_cache_get",
                [PTR],
                [PTR],
                cache_store.clone(),
                host_cache_get,
            )
            .with_function(
                "host_cache_set",
                [PTR, PTR, PTR],
                [],
                cache_store,
                host_cache_set,
            )
            .build()?;

        Ok(Self {
            manifest,
            plugin: tokio::sync::Mutex::new(plugin),
            config,
        })
    }

    pub fn name(&self) -> &str {
        &self.manifest.id
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
}

// Safety: extism::Plugin is !Send, but we wrap it in tokio::sync::Mutex
// and only access it through the mutex, ensuring single-threaded access.
// The WasmPluginInstance is stored in Arc and shared across async tasks.
unsafe impl Send for WasmPluginInstance {}
unsafe impl Sync for WasmPluginInstance {}

#[async_trait]
impl Plugin for WasmPluginInstance {
    fn name(&self) -> &str {
        &self.manifest.id
    }

    fn display_name(&self) -> &str {
        &self.manifest.name
    }

    fn version(&self) -> &str {
        &self.manifest.version
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.manifest
            .capabilities
            .iter()
            .filter_map(|c| match c.as_str() {
                "streaming" => Some(Capability::Streaming),
                "lyrics" => Some(Capability::Lyrics),
                "scrobble" => Some(Capability::Scrobble),
                "metadata" => Some(Capability::Metadata),
                _ => None,
            })
            .collect()
    }

    async fn init(&mut self, _ctx: PluginContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        let mut plugin = self.plugin.lock().await;
        match plugin.call::<&str, String>("riff_health_check", "") {
            Ok(_) => HealthStatus {
                healthy: true,
                message: None,
            },
            Err(e) => HealthStatus {
                healthy: false,
                message: Some(e.to_string()),
            },
        }
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
