use std::sync::Arc;

use super::capabilities::{EditorialProvider, LyricsProvider, MetadataProvider, ScrobbleProvider};

pub struct PluginRegistry {
    lyrics: Vec<Arc<dyn LyricsProvider>>,
    scrobble: Vec<Arc<dyn ScrobbleProvider>>,
    metadata: Vec<Arc<dyn MetadataProvider>>,
    editorial: Vec<Arc<dyn EditorialProvider>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            lyrics: Vec::new(),
            scrobble: Vec::new(),
            metadata: Vec::new(),
            editorial: Vec::new(),
        }
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

    pub fn register_editorial(&mut self, provider: Arc<dyn EditorialProvider>) {
        self.editorial.push(provider);
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

    pub fn editorial_providers(&self) -> &[Arc<dyn EditorialProvider>] {
        &self.editorial
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}
