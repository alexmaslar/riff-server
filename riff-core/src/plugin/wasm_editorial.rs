use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use super::capabilities::{EditorialProvider, EditorialResult};
use super::wasm_host::WasmPluginInstance;

pub struct WasmEditorialProvider {
    instance: Arc<WasmPluginInstance>,
}

#[derive(Serialize)]
struct AlbumReviewInput {
    title: String,
    artist: String,
    year: Option<i32>,
}

impl WasmEditorialProvider {
    pub fn new(instance: Arc<WasmPluginInstance>) -> Self {
        Self { instance }
    }
}

#[async_trait]
impl EditorialProvider for WasmEditorialProvider {
    fn provider_name(&self) -> &str {
        &self.instance.manifest.id
    }

    fn icon_url(&self) -> Option<&str> {
        self.instance.manifest.icon_url.as_deref()
    }

    async fn get_album_reviews(
        &self,
        title: &str,
        artist: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Option<EditorialResult>> {
        let input = serde_json::to_string(&AlbumReviewInput {
            title: title.to_string(),
            artist: artist.to_string(),
            year,
        })?;
        let mut plugin = self.instance.plugin.lock().await;
        let output = plugin.call::<&str, String>("riff_get_album_reviews", &input)?;
        if output.is_empty() || output == "null" {
            return Ok(None);
        }
        let result: EditorialResult = serde_json::from_str(&output)?;
        if result.reviews.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }
}
