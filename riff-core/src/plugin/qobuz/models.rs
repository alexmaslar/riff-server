use serde::{Deserialize, Deserializer, de};

// --- Search response ---

#[derive(Debug, Deserialize)]
pub struct QobuzSearchResponse {
    pub albums: Option<QobuzPage<QobuzAlbum>>,
    pub tracks: Option<QobuzPage<QobuzTrack>>,
    pub artists: Option<QobuzPage<QobuzArtist>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzPage<T: Clone> {
    pub items: Vec<T>,
}

impl<T: Clone> Default for QobuzPage<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
        }
    }
}

// --- Core types ---

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzAlbum {
    pub id: StringOrU64,
    pub title: String,
    pub artist: Option<QobuzArtist>,
    pub image: Option<QobuzImage>,
    pub tracks_count: Option<u32>,
    pub release_date_original: Option<String>,
    pub duration: Option<u32>,
    pub genre: Option<QobuzGenre>,
    pub label: Option<QobuzLabel>,
    /// Some album detail responses embed tracks directly
    pub tracks: Option<QobuzPage<QobuzTrack>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzArtist {
    pub id: Option<u64>,
    pub name: QobuzName,
    pub image: Option<QobuzImage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzTrack {
    pub id: u64,
    pub title: String,
    pub duration: Option<u32>,
    pub track_number: Option<u32>,
    pub performer: Option<QobuzArtist>,
    pub album: Option<QobuzAlbumRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzAlbumRef {
    pub id: StringOrU64,
    pub title: Option<String>,
    pub image: Option<QobuzImage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzImage {
    pub small: Option<String>,
    pub thumbnail: Option<String>,
    pub large: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzGenre {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QobuzLabel {
    pub name: Option<String>,
}

// --- Artist detail response ---

#[derive(Debug, Deserialize)]
pub struct QobuzArtistResponse {
    #[serde(flatten)]
    pub artist: QobuzArtist,
    pub albums: Option<QobuzPage<QobuzAlbum>>,
}

// --- Download response ---

#[derive(Debug, Deserialize)]
pub struct QobuzDownloadResponse {
    pub url: Option<String>,
}

// --- Custom types for API quirks ---

/// Qobuz album IDs can be either strings or integers depending on the endpoint.
#[derive(Debug, Clone)]
pub struct StringOrU64(pub String);

impl<'de> Deserialize<'de> for StringOrU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = StringOrU64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or integer")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<StringOrU64, E> {
                Ok(StringOrU64(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<StringOrU64, E> {
                Ok(StringOrU64(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<StringOrU64, E> {
                Ok(StringOrU64(v.to_string()))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<StringOrU64, E> {
                Ok(StringOrU64(v.to_string()))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<StringOrU64, E> {
                Ok(StringOrU64((v as u64).to_string()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Qobuz artist name can be either a plain string or `{ "display": "Name" }`.
#[derive(Debug, Clone)]
pub struct QobuzName(pub String);

impl<'de> Deserialize<'de> for QobuzName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = QobuzName;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or { display: \"...\" } object")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<QobuzName, E> {
                Ok(QobuzName(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<QobuzName, E> {
                Ok(QobuzName(v))
            }

            fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<QobuzName, M::Error> {
                let mut display = None;
                while let Some(key) = map.next_key::<String>()? {
                    let val: String = map.next_value()?;
                    if key == "display" {
                        display = Some(val);
                    }
                }
                Ok(QobuzName(display.unwrap_or_else(|| "Unknown".to_string())))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
