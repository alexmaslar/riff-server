use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use std::path::{Path, PathBuf};

/// Get covers cache directory, creating it if needed
pub fn get_cache_dir() -> Result<PathBuf> {
    let data_dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find data directory"))?
        .join("riff");

    let cache_dir = data_dir.join("cache").join("covers");
    std::fs::create_dir_all(&cache_dir)
        .context("Failed to create cache directory")?;

    Ok(cache_dir)
}

/// Generate a dark gradient fallback cell for empty collage slots.
fn generate_fallback_cell(size: u32) -> RgbaImage {
    let dark_bg = Rgba([18, 18, 18, 255]);
    let mut cell = RgbaImage::from_pixel(size, size, dark_bg);

    // Vertical gradient from slightly lighter to dark background
    for y in 0..size {
        let t = y as f32 / size as f32;
        let brightness = ((1.0 - t) * 30.0) as u8;
        for x in 0..size {
            cell.put_pixel(x, y, Rgba([18 + brightness, 18 + brightness, 18 + brightness, 255]));
        }
    }
    cell
}

/// Generate a 2x2 album art collage for a daily mix cover.
/// 1024x1024 canvas with 2px dark gap between quadrants.
/// Covers that fail to open are skipped; empty slots get a dark gradient fill.
pub fn generate_mix_collage(cover_paths: &[&Path]) -> Result<DynamicImage> {
    let size = 1024u32;
    let cell = 510u32; // (1024 - 2px gap) / 2 = 511, but 510 + 2 + 510 + 2 = 1024
    let gap = 2u32;
    let dark_bg = Rgba([18, 18, 18, 255]);

    let mut canvas = RgbaImage::from_pixel(size, size, dark_bg);

    // Grid positions: top-left, top-right, bottom-left, bottom-right
    let positions: [(u32, u32); 4] = [
        (0, 0),
        (cell + gap, 0),
        (0, cell + gap),
        (cell + gap, cell + gap),
    ];

    // Track which slots have a real cover placed
    let mut filled = [false; 4];

    for (i, path) in cover_paths.iter().take(4).enumerate() {
        if let Ok(img) = image::open(path) {
            let resized = img.resize_exact(cell, cell, image::imageops::FilterType::Lanczos3);
            let (px, py) = positions[i];
            image::imageops::overlay(&mut canvas, &resized.to_rgba8(), px as i64, py as i64);
            filled[i] = true;
        }
    }

    // Fill empty slots (failed opens or fewer than 4 paths) with dark gradient
    for (i, &has_cover) in filled.iter().enumerate() {
        if !has_cover {
            let fallback = generate_fallback_cell(cell);
            let (px, py) = positions[i];
            image::imageops::overlay(&mut canvas, &fallback, px as i64, py as i64);
        }
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}
