use serde::{Deserialize, Serialize};

/// A plugin entry from the community registry (fetched from GitHub).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemotePluginEntry {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub author: String,
    pub capabilities: Vec<String>,
    pub settings: Vec<serde_json::Value>,
    pub wasm_url: String,
    pub manifest_url: String,
}

pub const REMOTE_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/alexmaslar/riff-plugins/master/catalog.json";

/// Fetch the community plugin catalog from GitHub.
/// Returns an empty vec on any failure (network, parse, etc.).
pub async fn fetch_remote_catalog() -> Vec<RemotePluginEntry> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let resp = match client.get(REMOTE_CATALOG_URL).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!("remote plugin catalog returned HTTP {}", r.status());
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!("failed to fetch remote plugin catalog: {e}");
            return Vec::new();
        }
    };

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("failed to read remote plugin catalog body: {e}");
            return Vec::new();
        }
    };

    match serde_json::from_str::<Vec<RemotePluginEntry>>(&text) {
        Ok(entries) => {
            tracing::info!("fetched remote plugin catalog ({} entries)", entries.len());
            entries
        }
        Err(e) => {
            tracing::warn!("failed to parse remote plugin catalog: {e}");
            Vec::new()
        }
    }
}

impl RemotePluginEntry {
    /// Return the keys of settings whose field_type.type is "secret".
    pub fn secret_keys(&self) -> Vec<String> {
        self.settings
            .iter()
            .filter_map(|s| {
                let ft = s.get("field_type")?;
                let t = ft.get("type")?.as_str()?;
                if t == "secret" {
                    s.get("key")?.as_str().map(|k| k.to_string())
                } else {
                    None
                }
            })
            .collect()
    }
}
