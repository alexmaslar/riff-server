use anyhow::{Context, Result};
use base64::Engine;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Returns paths to (cert.pem, key.pem), generating them on first run.
pub fn ensure_certificate() -> Result<(PathBuf, PathBuf)> {
    let dir = cert_dir()?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        // Validate existing cert is loadable
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        if !cert_pem.is_empty() && !key_pem.is_empty() {
            tracing::debug!("using existing TLS certificate");
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

    tracing::info!("TLS certificate saved to {}", cert_path.display());
    Ok(())
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
