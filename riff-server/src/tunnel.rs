use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelStatus {
    pub enabled: bool,
    pub url: Option<String>,
    pub status: String, // "stopped", "starting", "connected", "error"
}

impl Default for TunnelStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            status: "stopped".to_string(),
        }
    }
}

pub struct TunnelManager {
    pub status: Arc<RwLock<TunnelStatus>>,
    child: Arc<RwLock<Option<Child>>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self {
            status: Arc::new(RwLock::new(TunnelStatus::default())),
            child: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self, port: u16) -> Result<()> {
        // Already running?
        {
            let status = self.status.read().await;
            if status.status == "starting" || status.status == "connected" {
                return Ok(());
            }
        }

        let binary_path = ensure_cloudflared().await?;

        {
            let mut status = self.status.write().await;
            status.enabled = true;
            status.status = "starting".to_string();
            status.url = None;
        }

        let mut child = Command::new(&binary_path)
            .args(["tunnel", "--url", &format!("http://localhost:{}", port)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start cloudflared")?;

        let stderr = child.stderr.take().expect("stderr captured");

        // Store child process
        {
            let mut guard = self.child.write().await;
            *guard = Some(child);
        }

        // Spawn task to read stderr and detect tunnel URL
        let status = self.status.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("cloudflared: {}", line);
                // cloudflared prints the URL like: "... https://xxx.trycloudflare.com ..."
                if let Some(url) = extract_tunnel_url(&line) {
                    let mut s = status.write().await;
                    s.url = Some(url.clone());
                    s.status = "connected".to_string();
                    tracing::info!("tunnel connected: {}", url);
                }
            }
            // Process exited
            let mut s = status.write().await;
            if s.status != "stopped" {
                s.status = "error".to_string();
                tracing::warn!("cloudflared process exited unexpectedly");
            }
        });

        Ok(())
    }

    pub async fn stop(&self) {
        {
            let mut guard = self.child.write().await;
            if let Some(mut child) = guard.take() {
                let _ = child.kill().await;
            }
        }
        let mut status = self.status.write().await;
        status.enabled = false;
        status.url = None;
        status.status = "stopped".to_string();
        tracing::info!("tunnel stopped");
    }
}

fn extract_tunnel_url(line: &str) -> Option<String> {
    // Look for https://xxx.trycloudflare.com patterns
    for word in line.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != ':' && c != '/' && c != '.' && c != '-');
        if word.starts_with("https://") && word.contains("trycloudflare.com") {
            return Some(word.to_string());
        }
    }
    None
}

fn cloudflared_dir() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("riff")
        .join("bin");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cloudflared_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

async fn ensure_cloudflared() -> Result<PathBuf> {
    let dir = cloudflared_dir()?;
    let binary_path = dir.join(cloudflared_binary_name());

    // Check if binary already exists and works
    if binary_path.exists() {
        let output = Command::new(&binary_path)
            .arg("--version")
            .output()
            .await;
        if output.is_ok() {
            return Ok(binary_path);
        }
    }

    tracing::info!("downloading cloudflared...");

    let url = download_url()?;
    let response = reqwest::get(&url)
        .await
        .context("failed to download cloudflared")?;

    if !response.status().is_success() {
        anyhow::bail!("cloudflared download failed: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    // On macOS, the download is a .tgz archive
    if url.ends_with(".tgz") {
        extract_tgz(&bytes, &binary_path)?;
    } else {
        std::fs::write(&binary_path, &bytes)?;
    }

    // Set executable permission on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))?;
    }

    tracing::info!("cloudflared installed to {}", binary_path.display());
    Ok(binary_path)
}

fn download_url() -> Result<String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);

    let url = match (os, arch) {
        ("macos", "aarch64") => {
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-arm64.tgz"
        }
        ("macos", "x86_64") => {
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-amd64.tgz"
        }
        ("linux", "x86_64") => {
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        }
        ("linux", "aarch64") => {
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64"
        }
        _ => anyhow::bail!("unsupported platform: {os}/{arch}"),
    };

    Ok(url.to_string())
}

fn extract_tgz(data: &[u8], binary_path: &PathBuf) -> Result<()> {
    use std::io::Read;

    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.file_name().and_then(|n| n.to_str()) == Some("cloudflared") {
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents)?;
            std::fs::write(binary_path, &contents)?;
            return Ok(());
        }
    }

    anyhow::bail!("cloudflared binary not found in archive")
}
