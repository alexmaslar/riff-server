use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    #[serde(default)]
    pub library: LibraryConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub metadata: MetadataConfig,
    #[serde(default)]
    pub remote_access: RemoteAccessConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub cors_origins: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub isolated: bool,
    /// Per-library overrides (None = follow global setting)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_enrich: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_summaries: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_ratings: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_recommendations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_bios: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_recommendations: Option<bool>,
    /// Per-library scan interval in seconds (None = use global default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_interval: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_scan_interval")]
    pub scan_interval: u64,
    #[serde(default)]
    pub libraries: Vec<LibraryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_jwt_secret")]
    pub jwt_secret: String,
    #[serde(default = "default_session_duration")]
    pub session_duration: String,
    #[serde(default = "default_admin_username")]
    pub admin_username: String,
    #[serde(default)]
    pub admin_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataConfig {
    #[serde(default, alias = "discogs")]
    pub enrichment: EnrichmentConfig,
    #[serde(default)]
    pub ai: AiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    OpenAi,
    Anthropic,
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ai_provider")]
    pub provider: AiProvider,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fast_model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_true")]
    pub album_summaries: bool,
    #[serde(default = "default_true")]
    pub album_ratings: bool,
    #[serde(default = "default_true")]
    pub album_recommendations: bool,
    #[serde(default = "default_true")]
    pub artist_bios: bool,
    #[serde(default = "default_true")]
    pub artist_recommendations: bool,
    #[serde(default = "default_true")]
    pub playlist_generation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentConfig {
    #[serde(default = "default_true")]
    pub auto_enrich: bool,
    #[serde(default = "default_true")]
    pub download_covers: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteAccessConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Preferred remote access method: upnp | port_forwarding | external_url
    #[serde(default = "default_remote_method")]
    pub method: String,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub cert_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingConfig {
    #[serde(default = "default_remote_bitrate")]
    pub remote_bitrate: u32,
    #[serde(default = "default_remote_format")]
    pub remote_format: String,
    /// Maximum concurrent FFmpeg transcode processes (default: 2).
    /// Prevents OOM on low-power hardware (Raspberry Pi, NAS).
    #[serde(default = "default_max_transcode_processes")]
    pub max_transcode_processes: usize,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            remote_bitrate: default_remote_bitrate(),
            remote_format: default_remote_format(),
            max_transcode_processes: default_max_transcode_processes(),
        }
    }
}

fn default_remote_bitrate() -> u32 {
    256
}

fn default_remote_format() -> String {
    "aac".to_string()
}

fn default_max_transcode_processes() -> usize {
    2
}

fn default_remote_method() -> String {
    "upnp".to_string()
}

fn default_true() -> bool {
    true
}

fn default_ai_provider() -> AiProvider {
    AiProvider::OpenAi
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_ai_provider(),
            api_key: None,
            model: None,
            fast_model: None,
            base_url: None,
            album_summaries: true,
            album_ratings: true,
            album_recommendations: true,
            artist_bios: true,
            artist_recommendations: true,
            playlist_generation: true,
        }
    }
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            enrichment: EnrichmentConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            auto_enrich: true,
            download_covers: true,
        }
    }
}

fn default_server() -> ServerConfig {
    ServerConfig {
        port: default_port(),
        https_port: default_https_port(),
        cors_origins: None,
    }
}

fn default_https_port() -> u16 {
    8443
}

fn default_port() -> u16 {
    8080
}

fn default_scan_interval() -> u64 {
    3600
}

fn default_jwt_secret() -> String {
    use rand::Rng;
    let secret: [u8; 32] = rand::thread_rng().gen();
    hex::encode(secret)
}

fn default_session_duration() -> String {
    "30d".to_string()
}

fn default_admin_username() -> String {
    "admin".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            library: LibraryConfig::default(),
            auth: AuthConfig::default(),
            metadata: MetadataConfig::default(),
            remote_access: RemoteAccessConfig::default(),
            streaming: StreamingConfig::default(),
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            path: None,
            scan_interval: default_scan_interval(),
            libraries: Vec::new(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: default_jwt_secret(),
            session_duration: default_session_duration(),
            admin_username: default_admin_username(),
            admin_password: None,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let config_path = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?
            .join("riff")
            .join("config.yaml");

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            let config: Config = serde_yaml_ng::from_str(&contents)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    pub fn load_from_str(yaml: &str) -> anyhow::Result<Self> {
        let config: Config = serde_yaml_ng::from_str(yaml)?;
        Ok(config)
    }

    /// Return the effective list of libraries.
    /// If `libraries` is non-empty, return it.
    /// Else if `path` is Some, return a single entry with name "Music" and that path.
    /// Else return empty vec.
    pub fn resolved_libraries(&self) -> Vec<LibraryEntry> {
        if !self.library.libraries.is_empty() {
            return self.library.libraries.clone();
        }
        if let Some(ref path) = self.library.path {
            return vec![LibraryEntry {
                name: "Music".to_string(),
                path: path.clone(),
                isolated: false,
                auto_enrich: None,
                album_summaries: None,
                album_ratings: None,
                album_recommendations: None,
                artist_bios: None,
                artist_recommendations: None,
                scan_interval: None,
            }];
        }
        Vec::new()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?
            .join("riff");

        std::fs::create_dir_all(&config_dir)?;
        let yaml = serde_yaml_ng::to_string(self)?;
        std::fs::write(config_dir.join("config.yaml"), yaml)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.https_port, 8443);
        assert!(config.server.cors_origins.is_none());
        assert!(config.library.path.is_none());
        assert_eq!(config.library.scan_interval, 3600);
        assert_eq!(config.auth.admin_username, "admin");
        assert!(config.auth.admin_password.is_none());
        assert_eq!(config.auth.session_duration, "30d");
        assert!(!config.auth.jwt_secret.is_empty());
    }

    #[test]
    fn test_default_streaming_config() {
        let config = StreamingConfig::default();
        assert_eq!(config.remote_bitrate, 256);
        assert_eq!(config.remote_format, "aac");
        assert_eq!(config.max_transcode_processes, 2);
    }

    #[test]
    fn test_default_ai_config() {
        let config = AiConfig::default();
        assert!(!config.enabled);
        assert!(config.api_key.is_none());
        assert!(config.album_summaries);
        assert!(config.album_ratings);
        assert!(config.album_recommendations);
        assert!(config.artist_bios);
        assert!(config.artist_recommendations);
        assert!(config.playlist_generation);
    }

    #[test]
    fn test_default_enrichment_config() {
        let config = EnrichmentConfig::default();
        assert!(config.auto_enrich);
        assert!(config.download_covers);
    }

    #[test]
    fn test_default_remote_access_config() {
        let config = RemoteAccessConfig::default();
        assert!(!config.enabled);
        assert!(config.external_url.is_none());
    }

    #[test]
    fn test_remote_access_serde_default_method() {
        // When remote_access section exists but method is omitted,
        // serde uses the default_remote_method function
        let yaml = "remote_access:\n  enabled: false\n";
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.remote_access.method, "upnp");
    }

    #[test]
    fn test_load_from_yaml_string() {
        let yaml = r#"
server:
  port: 9090
  https_port: 9443
library:
  path: /music
  scan_interval: 7200
auth:
  admin_username: testadmin
  session_duration: 7d
"#;
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.server.https_port, 9443);
        assert_eq!(config.library.path.as_deref(), Some("/music"));
        assert_eq!(config.library.scan_interval, 7200);
        assert_eq!(config.auth.admin_username, "testadmin");
        assert_eq!(config.auth.session_duration, "7d");
    }

    #[test]
    fn test_load_empty_yaml_uses_defaults() {
        let config = Config::load_from_str("{}").unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.library.scan_interval, 3600);
        assert_eq!(config.auth.admin_username, "admin");
    }

    #[test]
    fn test_load_partial_yaml() {
        let yaml = r#"
server:
  port: 3000
"#;
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.server.port, 3000);
        // Other fields should use defaults
        assert_eq!(config.server.https_port, 8443);
        assert_eq!(config.library.scan_interval, 3600);
    }

    #[test]
    fn test_ai_provider_deserialize() {
        let yaml = r#"
metadata:
  ai:
    enabled: true
    provider: anthropic
"#;
        let config = Config::load_from_str(yaml).unwrap();
        assert!(config.metadata.ai.enabled);
        assert!(matches!(config.metadata.ai.provider, AiProvider::Anthropic));
    }

    #[test]
    fn test_streaming_config_custom() {
        let yaml = r#"
streaming:
  remote_bitrate: 128
  remote_format: opus
  max_transcode_processes: 4
"#;
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.streaming.remote_bitrate, 128);
        assert_eq!(config.streaming.remote_format, "opus");
        assert_eq!(config.streaming.max_transcode_processes, 4);
    }

    #[test]
    fn test_config_serialize_roundtrip() {
        let config = Config::default();
        let yaml = serde_yaml_ng::to_string(&config).unwrap();
        let roundtripped = Config::load_from_str(&yaml).unwrap();
        assert_eq!(config.server.port, roundtripped.server.port);
        assert_eq!(config.server.https_port, roundtripped.server.https_port);
        assert_eq!(config.library.scan_interval, roundtripped.library.scan_interval);
        assert_eq!(config.streaming.remote_bitrate, roundtripped.streaming.remote_bitrate);
    }
}
