use anyhow::{Context, Result};
use igd_next::aio::tokio::Tokio;
use igd_next::aio::Gateway;
use igd_next::PortMappingProtocol;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

const LEASE_DURATION: u32 = 3600; // 1 hour
const RENEWAL_INTERVAL: u64 = 1800; // 30 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAccessStatus {
    pub enabled: bool,
    pub method: String,
    pub status: String,
    pub public_address: Option<String>,
    pub external_url: Option<String>,
    pub cert_fingerprint: Option<String>,
    pub error_message: Option<String>,
    pub https_port: u16,
    pub local_ip: Option<String>,
}

impl Default for RemoteAccessStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            method: "none".to_string(),
            status: "stopped".to_string(),
            public_address: None,
            external_url: None,
            cert_fingerprint: None,
            error_message: None,
            https_port: 8443,
            local_ip: None,
        }
    }
}

pub struct RemoteAccessManager {
    pub status: Arc<RwLock<RemoteAccessStatus>>,
    renewal_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    https_port: u16,
}

impl RemoteAccessManager {
    pub fn new(https_port: u16) -> Self {
        Self {
            status: Arc::new(RwLock::new(RemoteAccessStatus {
                https_port,
                ..Default::default()
            })),
            renewal_handle: Arc::new(RwLock::new(None)),
            https_port,
        }
    }

    /// Set the certificate fingerprint (called once on startup).
    pub async fn set_cert_fingerprint(&self, fingerprint: String) {
        let mut status = self.status.write().await;
        status.cert_fingerprint = Some(fingerprint);
    }

    /// Start remote access. Supported methods:
    /// - "manual": use external_url directly
    /// - "upnp" (default): UPnP port forwarding
    pub async fn start(
        &self,
        external_url: Option<String>,
        preferred_method: &str,
    ) -> Result<()> {
        {
            let status = self.status.read().await;
            if status.status == "active" {
                return Ok(());
            }
        }

        // Manual URL mode — skip everything
        if let Some(url) = external_url {
            let mut status = self.status.write().await;
            status.enabled = true;
            status.method = "manual".to_string();
            status.status = "active".to_string();
            status.external_url = Some(url.clone());
            status.public_address = Some(url);
            status.error_message = None;
            tracing::info!(
                "remote access using manual URL: {}",
                status.public_address.as_ref().unwrap()
            );
            return Ok(());
        }

        match preferred_method {
            "upnp" | _ => self.start_upnp().await,
        }
    }

    /// Start UPnP port forwarding.
    async fn start_upnp(&self) -> Result<()> {
        {
            let mut status = self.status.write().await;
            status.enabled = true;
            status.method = "upnp".to_string();
            status.status = "starting".to_string();
            status.error_message = None;
        }

        match self.setup_upnp().await {
            Ok(public_address) => {
                let mut status = self.status.write().await;
                status.status = "active".to_string();
                status.public_address = Some(public_address.clone());
                status.error_message = None;
                tracing::info!("UPnP remote access active: {}", public_address);

                // Start lease renewal loop
                self.start_renewal_loop().await;
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e}");
                let mut status = self.status.write().await;
                status.status = "error".to_string();
                status.error_message = Some(msg.clone());
                tracing::warn!("UPnP setup failed: {msg}");
                Err(e)
            }
        }
    }

    /// Stop remote access (UPnP or manual).
    pub async fn stop(&self) {
        // Cancel renewal loop
        {
            let mut handle = self.renewal_handle.write().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // Try to remove the UPnP port mapping
        let current_method = {
            let s = self.status.read().await;
            s.method.clone()
        };
        if current_method == "upnp" {
            if let Ok(gateway) = discover_gateway().await {
                let _ = gateway
                    .remove_port(PortMappingProtocol::TCP, self.https_port)
                    .await;
                tracing::info!("UPnP port mapping removed");
            }
        }

        let mut status = self.status.write().await;
        status.enabled = false;
        status.status = "stopped".to_string();
        status.public_address = None;
        status.external_url = None;
        status.error_message = None;
        // Keep cert_fingerprint — it doesn't change
        tracing::info!("remote access stopped");
    }

    async fn setup_upnp(&self) -> Result<String> {
        let gateway = discover_gateway().await?;

        let external_ip = gateway
            .get_external_ip()
            .await
            .context("getting external IP from gateway")?;

        // Check for CGNAT (100.64.0.0/10)
        if let IpAddr::V4(ipv4) = external_ip {
            let octets = ipv4.octets();
            if octets[0] == 100 && (octets[1] & 0xC0) == 64 {
                tracing::warn!(
                    "CGNAT detected (external IP {}). UPnP port forwarding may not work for remote access.",
                    external_ip
                );
            }
        }

        let local_ip = local_ip_address::local_ip().context("getting local IP")?;
        let local_addr: SocketAddr = SocketAddr::new(local_ip, self.https_port);

        gateway
            .add_port(
                PortMappingProtocol::TCP,
                self.https_port,
                local_addr,
                LEASE_DURATION,
                "Riff Music Server",
            )
            .await
            .context("adding UPnP port mapping")?;

        tracing::info!(
            "UPnP port mapping: external {}:{} -> {}",
            external_ip,
            self.https_port,
            local_addr
        );

        Ok(format!("https://{}:{}", external_ip, self.https_port))
    }

    async fn start_renewal_loop(&self) {
        let mut handle = self.renewal_handle.write().await;
        if let Some(h) = handle.take() {
            h.abort();
        }

        let status = self.status.clone();
        let https_port = self.https_port;
        let h = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(RENEWAL_INTERVAL)).await;

                let mut success = false;
                for attempt in 1..=3u32 {
                    match renew_mapping(https_port).await {
                        Ok(new_address) => {
                            let mut s = status.write().await;
                            let old_address = s.public_address.clone();
                            if old_address.as_ref() != Some(&new_address) {
                                tracing::info!("external address changed: {}", new_address);
                                s.public_address = Some(new_address);
                            }
                            s.status = "active".to_string();
                            s.error_message = None;
                            success = true;
                            break;
                        }
                        Err(e) => {
                            if attempt < 3 {
                                tracing::warn!(
                                    "UPnP renewal attempt {}/3 failed: {e}, retrying in 10s",
                                    attempt
                                );
                                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            } else {
                                tracing::warn!(
                                    "UPnP lease renewal failed after 3 attempts: {e}"
                                );
                                // Set error message but keep public_address — mapping may still be valid
                                let mut s = status.write().await;
                                s.error_message =
                                    Some(format!("renewal failed: {e}"));
                            }
                        }
                    }
                }
                let _ = success;
            }
        });

        *handle = Some(h);
    }
}

async fn discover_gateway() -> Result<Gateway<Tokio>> {
    igd_next::aio::tokio::search_gateway(igd_next::SearchOptions::default())
        .await
        .context("discovering UPnP gateway (is UPnP enabled on your router?)")
}

async fn renew_mapping(https_port: u16) -> Result<String> {
    let gateway = discover_gateway().await?;

    let external_ip = gateway
        .get_external_ip()
        .await
        .context("getting external IP")?;

    let local_ip = local_ip_address::local_ip().context("getting local IP")?;
    let local_addr: SocketAddr = SocketAddr::new(local_ip, https_port);

    // Try add_port first (works on most routers for renewal)
    if gateway
        .add_port(
            PortMappingProtocol::TCP,
            https_port,
            local_addr,
            LEASE_DURATION,
            "Riff Music Server",
        )
        .await
        .is_err()
    {
        // Some routers reject duplicate mappings — remove first, then re-add
        let _ = gateway
            .remove_port(PortMappingProtocol::TCP, https_port)
            .await;
        gateway
            .add_port(
                PortMappingProtocol::TCP,
                https_port,
                local_addr,
                LEASE_DURATION,
                "Riff Music Server",
            )
            .await
            .context("re-adding UPnP port mapping after remove")?;
    }

    Ok(format!("https://{}:{}", external_ip, https_port))
}
