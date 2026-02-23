use std::sync::Arc;

use super::Plugin;
use super::capabilities::{LyricsProvider, MetadataProvider, ScrobbleProvider, StreamingProvider, StreamingSearchResults};

pub struct PluginRegistry {
    plugins: Vec<Arc<dyn Plugin>>,
    streaming: Vec<Arc<dyn StreamingProvider>>,
    lyrics: Vec<Arc<dyn LyricsProvider>>,
    scrobble: Vec<Arc<dyn ScrobbleProvider>>,
    metadata: Vec<Arc<dyn MetadataProvider>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            streaming: Vec::new(),
            lyrics: Vec::new(),
            scrobble: Vec::new(),
            metadata: Vec::new(),
        }
    }

    // --- Registration ---

    pub fn register_base(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    pub fn register_streaming(&mut self, provider: Arc<dyn StreamingProvider>) {
        self.streaming.push(provider);
    }

    pub fn register_lyrics(&mut self, provider: Arc<dyn LyricsProvider>) {
        self.lyrics.push(provider);
    }

    pub fn register_scrobble(&mut self, provider: Arc<dyn ScrobbleProvider>) {
        self.scrobble.push(provider);
    }

    pub fn register_metadata(&mut self, provider: Arc<dyn MetadataProvider>) {
        self.metadata.push(provider);
    }

    /// Remove all registrations for a plugin by name.
    /// Used for hot-reloading WASM plugins.
    pub fn unregister_by_name(&mut self, name: &str) {
        self.plugins.retain(|p| p.name() != name);
        self.streaming.retain(|p| p.provider_name() != name);
        self.lyrics.retain(|p| p.provider_name() != name);
        self.scrobble.retain(|p| p.provider_name() != name);
        self.metadata.retain(|p| p.provider_name() != name);
    }

    // --- Accessors ---

    pub fn all_plugins(&self) -> &[Arc<dyn Plugin>] {
        &self.plugins
    }

    pub fn streaming_providers(&self) -> &[Arc<dyn StreamingProvider>] {
        &self.streaming
    }

    pub fn lyrics_providers(&self) -> &[Arc<dyn LyricsProvider>] {
        &self.lyrics
    }

    pub fn scrobble_providers(&self) -> &[Arc<dyn ScrobbleProvider>] {
        &self.scrobble
    }

    pub fn metadata_providers(&self) -> &[Arc<dyn MetadataProvider>] {
        &self.metadata
    }

    // --- Convenience ---

    /// Fan out a search query to all streaming providers concurrently,
    /// returning merged results.
    pub async fn search_all_streaming(
        &self,
        query: &str,
        limit: u32,
    ) -> Vec<(String, anyhow::Result<StreamingSearchResults>)> {
        let mut handles = Vec::new();

        for provider in &self.streaming {
            let provider = provider.clone();
            let query = query.to_string();
            handles.push(tokio::spawn(async move {
                let name = provider.provider_name().to_string();
                let result = provider.search(&query, limit).await;
                (name, result)
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    tracing::warn!("streaming search task panicked: {e}");
                }
            }
        }
        results
    }

    /// Shutdown all plugins gracefully.
    pub async fn shutdown_all(&self) {
        for plugin in &self.plugins {
            if let Err(e) = plugin.shutdown().await {
                tracing::warn!("plugin {} shutdown error: {e}", plugin.name());
            }
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Capability, HealthStatus, Plugin, PluginContext};

    // --- Test helpers ---

    struct TestPlugin {
        test_name: String,
    }

    #[async_trait::async_trait]
    impl Plugin for TestPlugin {
        fn name(&self) -> &str {
            &self.test_name
        }
        fn display_name(&self) -> &str {
            &self.test_name
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Scrobble]
        }
        async fn init(&mut self, _ctx: PluginContext) -> anyhow::Result<()> {
            Ok(())
        }
        async fn health_check(&self) -> HealthStatus {
            HealthStatus {
                healthy: true,
                message: None,
            }
        }
        async fn shutdown(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct TestStreamingProvider;

    #[async_trait::async_trait]
    impl StreamingProvider for TestStreamingProvider {
        fn provider_name(&self) -> &str {
            "test-streaming"
        }
        async fn search(
            &self,
            _query: &str,
            _limit: u32,
        ) -> anyhow::Result<StreamingSearchResults> {
            Ok(StreamingSearchResults {
                albums: vec![],
                artists: vec![],
                tracks: vec![],
            })
        }
        async fn get_album(
            &self,
            _id: &str,
        ) -> anyhow::Result<super::super::capabilities::StreamingAlbumDetail> {
            unimplemented!()
        }
        async fn get_artist_albums(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<super::super::capabilities::StreamingAlbum>> {
            unimplemented!()
        }
        async fn download_track(
            &self,
            _id: &str,
            _quality: super::super::capabilities::StreamingQuality,
            _dest: &std::path::Path,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    // --- Tests ---

    #[test]
    fn test_registry_new_is_empty() {
        let registry = PluginRegistry::new();
        assert!(registry.all_plugins().is_empty());
        assert!(registry.streaming_providers().is_empty());
        assert!(registry.lyrics_providers().is_empty());
        assert!(registry.scrobble_providers().is_empty());
        assert!(registry.metadata_providers().is_empty());
    }

    #[test]
    fn test_register_base_plugin() {
        let mut registry = PluginRegistry::new();
        let plugin = Arc::new(TestPlugin {
            test_name: "test".to_string(),
        });
        registry.register_base(plugin);
        assert_eq!(registry.all_plugins().len(), 1);
        assert_eq!(registry.all_plugins()[0].name(), "test");
    }

    #[test]
    fn test_register_streaming_provider() {
        let mut registry = PluginRegistry::new();
        registry.register_streaming(Arc::new(TestStreamingProvider));
        assert_eq!(registry.streaming_providers().len(), 1);
        assert_eq!(registry.streaming_providers()[0].provider_name(), "test-streaming");
    }

    #[test]
    fn test_multi_capability_plugin() {
        let mut registry = PluginRegistry::new();
        let plugin = Arc::new(TestPlugin {
            test_name: "multi".to_string(),
        });
        registry.register_base(plugin);
        registry.register_streaming(Arc::new(TestStreamingProvider));
        assert_eq!(registry.all_plugins().len(), 1);
        assert_eq!(registry.streaming_providers().len(), 1);
    }

    #[tokio::test]
    async fn test_search_all_streaming_empty() {
        let registry = PluginRegistry::new();
        let results = registry.search_all_streaming("test", 10).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_all_streaming_with_provider() {
        let mut registry = PluginRegistry::new();
        registry.register_streaming(Arc::new(TestStreamingProvider));
        let results = registry.search_all_streaming("test", 10).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "test-streaming");
        assert!(results[0].1.is_ok());
    }

    #[test]
    fn test_unregister_by_name() {
        let mut registry = PluginRegistry::new();
        registry.register_base(Arc::new(TestPlugin {
            test_name: "tidal".to_string(),
        }));
        registry.register_base(Arc::new(TestPlugin {
            test_name: "qobuz".to_string(),
        }));
        registry.register_streaming(Arc::new(TestStreamingProvider));
        assert_eq!(registry.all_plugins().len(), 2);

        registry.unregister_by_name("tidal");
        assert_eq!(registry.all_plugins().len(), 1);
        assert_eq!(registry.all_plugins()[0].name(), "qobuz");
        // streaming provider name doesn't match "tidal", so it stays
        assert_eq!(registry.streaming_providers().len(), 1);
    }

    #[tokio::test]
    async fn test_shutdown_all() {
        let mut registry = PluginRegistry::new();
        registry.register_base(Arc::new(TestPlugin {
            test_name: "a".to_string(),
        }));
        registry.register_base(Arc::new(TestPlugin {
            test_name: "b".to_string(),
        }));
        registry.shutdown_all().await;
        // No panic = success
    }
}
