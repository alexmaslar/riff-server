use serde::Serialize;

use super::Capability;

#[derive(Debug, Clone, Serialize)]
pub struct PluginDefinition {
    pub name: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub capabilities: Vec<Capability>,
    pub settings: Vec<SettingField>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingField {
    pub key: &'static str,
    pub label: &'static str,
    pub field_type: FieldType,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_text: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum FieldType {
    String,
    Secret,
    Select { options: Vec<SelectOption> },
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectOption {
    pub value: &'static str,
    pub label: &'static str,
}

impl SettingField {
    pub fn is_secret(&self) -> bool {
        matches!(self.field_type, FieldType::Secret)
    }
}

pub fn plugin_catalog() -> Vec<PluginDefinition> {
    vec![
        PluginDefinition {
            name: "lastfm",
            display_name: "Last.fm",
            description: "Scrobble tracks to Last.fm and enrich metadata with listening stats and tags.",
            capabilities: vec![Capability::Scrobble, Capability::Metadata],
            settings: vec![
                SettingField {
                    key: "api_key",
                    label: "API Key",
                    field_type: FieldType::Secret,
                    required: true,
                    help_text: Some("Create an API account at last.fm/api/account/create"),
                },
                SettingField {
                    key: "api_secret",
                    label: "API Secret",
                    field_type: FieldType::Secret,
                    required: true,
                    help_text: None,
                },
            ],
        },
        PluginDefinition {
            name: "listenbrainz",
            display_name: "ListenBrainz",
            description: "Scrobble tracks to ListenBrainz, the open-source listening history service.",
            capabilities: vec![Capability::Scrobble],
            settings: vec![SettingField {
                key: "token",
                label: "User Token",
                field_type: FieldType::Secret,
                required: true,
                help_text: Some("Find your token at listenbrainz.org/profile"),
            }],
        },
        PluginDefinition {
            name: "genius",
            display_name: "Genius",
            description: "Fetch song lyrics from Genius.",
            capabilities: vec![Capability::Lyrics],
            settings: vec![SettingField {
                key: "api_key",
                label: "API Key",
                field_type: FieldType::Secret,
                required: true,
                help_text: Some("Create an API client at genius.com/api-clients"),
            }],
        },
        PluginDefinition {
            name: "qobuz",
            display_name: "Qobuz",
            description: "Stream hi-res audio from Qobuz via SquidWTF proxy.",
            capabilities: vec![Capability::Streaming],
            settings: vec![
                SettingField {
                    key: "quality",
                    label: "Audio Quality",
                    field_type: FieldType::Select {
                        options: vec![
                            SelectOption { value: "27", label: "Hi-Res (24-bit, 192kHz)" },
                            SelectOption { value: "7", label: "Hi-Res (24-bit, 96kHz)" },
                            SelectOption { value: "6", label: "Lossless (16-bit, 44.1kHz)" },
                            SelectOption { value: "5", label: "High (MP3 320kbps)" },
                        ],
                    },
                    required: false,
                    help_text: Some("Default: Lossless"),
                },
                SettingField {
                    key: "country",
                    label: "Country Code",
                    field_type: FieldType::String,
                    required: false,
                    help_text: Some("Two-letter country code. Default: US"),
                },
            ],
        },
        PluginDefinition {
            name: "tidal",
            display_name: "Tidal",
            description: "Stream lossless audio from Tidal via SquidWTF proxy.",
            capabilities: vec![Capability::Streaming],
            settings: vec![
                SettingField {
                    key: "quality",
                    label: "Audio Quality",
                    field_type: FieldType::Select {
                        options: vec![
                            SelectOption { value: "HI_RES_LOSSLESS", label: "Hi-Res (24-bit, up to 192kHz)" },
                            SelectOption { value: "LOSSLESS", label: "Lossless (16-bit, 44.1kHz)" },
                            SelectOption { value: "HIGH", label: "High (AAC 320kbps)" },
                            SelectOption { value: "LOW", label: "Low (AAC 96kbps)" },
                        ],
                    },
                    required: false,
                    help_text: Some("Default: Lossless"),
                },
                SettingField {
                    key: "instance_timeout",
                    label: "Instance Timeout",
                    field_type: FieldType::Select {
                        options: vec![
                            SelectOption { value: "3", label: "3 seconds" },
                            SelectOption { value: "5", label: "5 seconds" },
                            SelectOption { value: "10", label: "10 seconds" },
                            SelectOption { value: "15", label: "15 seconds" },
                        ],
                    },
                    required: false,
                    help_text: Some("Default: 5 seconds"),
                },
            ],
        },
    ]
}
