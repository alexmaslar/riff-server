use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use base64::Engine;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Returns paths to (cert.pem, key.pem), generating them on first run.
/// Regenerates the certificate if the local IP has changed (DHCP lease change)
/// so the SAN stays valid for cert pinning.
pub fn ensure_certificate() -> Result<(PathBuf, PathBuf)> {
    let dir = cert_dir()?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        // Validate existing cert is loadable
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        if !cert_pem.is_empty() && !key_pem.is_empty() {
            // Check if local IP changed since last cert generation
            if san_ip_changed(&dir) {
                tracing::info!("local IP changed, regenerating TLS certificate");
                generate_certificate(&cert_path, &key_path)?;
            } else {
                tracing::debug!("using existing TLS certificate");
            }
            return Ok((cert_path, key_path));
        }
    }

    tracing::info!("generating self-signed TLS certificate");
    generate_certificate(&cert_path, &key_path)?;
    Ok((cert_path, key_path))
}

/// Compute SHA-256 fingerprint of the DER-encoded certificate, returned as base64.
pub fn cert_fingerprint(cert_path: &Path) -> Result<String> {
    let pem = std::fs::read_to_string(cert_path).context("reading certificate")?;

    // Extract DER bytes from PEM
    let der = pem_to_der(&pem).context("parsing PEM certificate")?;

    let hash = Sha256::digest(&der);
    Ok(base64::engine::general_purpose::STANDARD.encode(hash))
}

/// Returns true if the local IP differs from the IP stored when the cert was generated.
fn san_ip_changed(cert_dir: &Path) -> bool {
    let ip_file = cert_dir.join("san_ip.txt");
    let stored = std::fs::read_to_string(&ip_file).unwrap_or_default();
    let current = local_ip_address::local_ip()
        .ok()
        .map(|ip| ip.to_string())
        .unwrap_or_default();
    if current.is_empty() {
        return false; // Can't determine IP — don't regenerate
    }
    stored.trim() != current
}

/// Save the current local IP alongside the cert for future change detection.
fn save_san_ip(cert_dir: &Path) {
    if let Ok(ip) = local_ip_address::local_ip() {
        let _ = std::fs::write(cert_dir.join("san_ip.txt"), ip.to_string());
    }
}

fn cert_dir() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("riff")
        .join("certs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn generate_certificate(cert_path: &Path, key_path: &Path) -> Result<()> {
    let mut params = CertificateParams::new(vec!["Riff Music Server".to_string()])
        .context("creating cert params")?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Riff Music Server");
    dn.push(DnType::OrganizationName, "Riff");
    params.distinguished_name = dn;

    // SANs for local access
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::IpAddress(IpAddr::from([127, 0, 0, 1])),
    ];

    // Add local LAN IP if discoverable
    if let Ok(local_ip) = local_ip_address::local_ip() {
        params
            .subject_alt_names
            .push(SanType::IpAddress(local_ip));
    }

    // 10-year validity
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);

    let key_pair = KeyPair::generate().context("generating key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing certificate")?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    // Track the IP used in SANs for future change detection
    if let Some(dir) = cert_path.parent() {
        save_san_ip(dir);
    }

    tracing::info!("TLS certificate saved to {}", cert_path.display());
    Ok(())
}

/// Build a RustlsConfig with an explicit ring CryptoProvider.
/// This avoids the runtime panic when both ring and aws-lc-rs features
/// are enabled via Cargo feature unification.
pub fn build_rustls_config(cert_path: &Path, key_path: &Path) -> Result<RustlsConfig> {
    let cert_pem = std::fs::read(cert_path).context("reading cert PEM")?;
    let key_pem = std::fs::read(key_path).context("reading key PEM")?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<_, _>>()
        .context("parsing certificate PEM")?;

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("parsing private key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in PEM file"))?;

    let mut server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("setting TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("configuring TLS certificate")?;

    // Advertise h2 and http/1.1 via ALPN so clients can negotiate HTTP/2,
    // multiplexing all requests over a single TCP+TLS connection.
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(RustlsConfig::from_config(Arc::new(server_config)))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut in_block = false;
    let mut b64 = String::new();

    for line in pem.lines() {
        if line.starts_with("-----BEGIN") {
            in_block = true;
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if in_block {
            b64.push_str(line.trim());
        }
    }

    base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .context("decoding base64 from PEM")
}
