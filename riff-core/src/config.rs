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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
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

fn default_true() -> bool {
    true
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            discogs: DiscogsConfig::default(),
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
    ServerConfig { port: default_port() }
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
}
