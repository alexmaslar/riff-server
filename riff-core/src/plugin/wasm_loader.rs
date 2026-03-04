use std::path::Path;
use std::time::Duration;

use super::manifest::PluginManifest;

pub struct LoadedWasmPlugin {
    pub manifest: PluginManifest,
    pub wasm_bytes: Vec<u8>,
}

/// Download a WASM plugin from URLs and save to the plugins directory.
pub async fn download_wasm_plugin(
    plugin_id: &str,
    wasm_url: &str,
    manifest_url: &str,
    plugin_dir: &Path,
    client: &reqwest::Client,
) -> anyhow::Result<()> {
    let dest_dir = plugin_dir.join(plugin_id);
    std::fs::create_dir_all(&dest_dir)?;

    // Download plugin.wasm
    let wasm_resp = client
        .get(wasm_url)
        .timeout(Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()?;
    let wasm_bytes = wasm_resp.bytes().await?;
    std::fs::write(dest_dir.join("plugin.wasm"), &wasm_bytes)?;

    // Download manifest.json
    let manifest_resp = client
        .get(manifest_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?;
    let manifest_bytes = manifest_resp.bytes().await?;
    // Validate manifest before writing
    let _: PluginManifest = serde_json::from_slice(&manifest_bytes)?;
    std::fs::write(dest_dir.join("manifest.json"), &manifest_bytes)?;

    Ok(())
}

/// Remove a WASM plugin from the plugins directory.
pub fn remove_wasm_plugin(plugin_id: &str, plugin_dir: &Path) -> anyhow::Result<()> {
    let dest_dir = plugin_dir.join(plugin_id);
    if dest_dir.exists() {
        std::fs::remove_dir_all(&dest_dir)?;
    }
    Ok(())
}

pub fn scan_plugin_directory(path: &Path) -> anyhow::Result<Vec<LoadedWasmPlugin>> {
    let mut plugins = Vec::new();

    if !path.exists() {
        return Ok(plugins);
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let sub_path = entry.path();
        if !sub_path.is_dir() {
            continue;
        }

        let manifest_path = sub_path.join("manifest.json");
        let wasm_path = sub_path.join("plugin.wasm");

        if !manifest_path.exists() || !wasm_path.exists() {
            continue;
        }

        let manifest_str = std::fs::read_to_string(&manifest_path)?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_str)?;
        let wasm_bytes = std::fs::read(&wasm_path)?;

        tracing::info!(
            "found wasm plugin: {} v{} at {}",
            manifest.id,
            manifest.version,
            sub_path.display()
        );

        plugins.push(LoadedWasmPlugin {
            manifest,
            wasm_bytes,
        });
    }

    Ok(plugins)
}
