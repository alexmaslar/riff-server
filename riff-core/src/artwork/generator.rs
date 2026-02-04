use anyhow::{Context, Result};
use image::DynamicImage;
use std::path::Path;
use tokio::task;

use super::effects;

/// Effect type for album art
#[derive(Debug, Clone, Copy)]
pub enum EffectType {
    Vinyl { with_hole: bool },
    Wrapped,
    Highlights,
    None,
}

impl EffectType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "vinyl" => Some(Self::Vinyl { with_hole: false }),
            "wrapped" => Some(Self::Wrapped),
            "highlights" => Some(Self::Highlights),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub fn to_string(&self) -> &'static str {
        match self {
            Self::Vinyl { .. } => "vinyl",
            Self::Wrapped => "wrapped",
            Self::Highlights => "highlights",
            Self::None => "none",
        }
    }
}

/// Generate album art effect
/// CPU-bound operation, runs in blocking task pool
pub async fn generate_effect(
    source_path: &Path,
    effect: EffectType,
    size: u32,
    play_count: u32,
    with_hole: bool,
) -> Result<DynamicImage> {
    let source_path = source_path.to_owned();

    task::spawn_blocking(move || {
        match effect {
            EffectType::Vinyl { .. } => {
                effects::generate_vinyl_effect(&source_path, size, with_hole, play_count)
            }
            EffectType::Wrapped => {
                let base = image::open(&source_path)
                    .context("Failed to open album art")?
                    .resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                effects::generate_wrapped_effect(&base, play_count)
            }
            EffectType::Highlights => {
                let base = image::open(&source_path)
                    .context("Failed to open album art")?
                    .resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                effects::generate_highlights_effect(&base, play_count)
            }
            EffectType::None => {
                let img = image::open(&source_path)
                    .context("Failed to open album art")?
                    .resize_exact(size, size, image::imageops::FilterType::Lanczos3);
                Ok(img)
            }
        }
    })
    .await
    .context("Effect generation task panicked")?
}
