use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub capabilities: Vec<String>,
    pub permissions: PluginPermissions,
    #[serde(default)]
    pub settings: Vec<ManifestSettingField>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginPermissions {
    pub http: Option<HttpPermission>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpPermission {
    pub reason: String,
    pub required_hosts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestSettingField {
    pub key: String,
    pub label: String,
    pub field_type: serde_json::Value,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help_text: Option<String>,
}
