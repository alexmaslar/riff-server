use anyhow::Result;
use std::process::{Child, Command};

/// Handle that keeps the mDNS advertisement alive. Drops the child process on drop.
pub struct DiscoveryHandle {
    child: Child,
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Advertise this server via Bonjour/mDNS using the system's dns-sd command.
/// On macOS this talks to mDNSResponder (the only reliable path).
/// On Linux, dns-sd is provided by avahi-compat or mdnsd.
pub fn start_discovery(port: u16) -> Result<DiscoveryHandle> {
    let hostname = gethostname();
    let service_name = format!("Riff on {}", hostname);

    let child = Command::new("dns-sd")
        .args([
            "-R",
            &service_name,
            "_riff._tcp",
            "local.",
            &port.to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    tracing::info!("mDNS: advertising as '{}' on port {}", service_name, port);

    Ok(DiscoveryHandle { child })
}

fn gethostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "riff-server".to_string())
}
