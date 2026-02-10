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
pub struct LibraryConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_scan_interval")]
    pub scan_interval: u64,
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
    #[serde(default)]
    pub discogs: DiscogsConfig,
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
pub struct DiscogsConfig {
    #[serde(default)]
    pub api_token: Option<String>,
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
            discogs: DiscogsConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

impl Default for DiscogsConfig {
    fn default() -> Self {
        Self {
            api_token: None,
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
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            path: None,
            scan_interval: default_scan_interval(),
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
