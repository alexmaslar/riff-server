use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};

pub fn start_discovery(port: u16) -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;

    let hostname = gethostname();
    let service_name = format!("Riff on {}", hostname);
    let host = format!("{}.local.", hostname);

    let properties = [("version", env!("CARGO_PKG_VERSION"))];

    let service = ServiceInfo::new(
        "_riff._tcp.local.",
        &service_name,
        &host,
        "", // auto-detect IP
        port,
        properties.as_slice(),
    )?;

    mdns.register(service)?;
    tracing::info!("mDNS: advertising as '{}' on port {}", service_name, port);

    Ok(mdns)
}

fn gethostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "riff-server".to_string())
}
