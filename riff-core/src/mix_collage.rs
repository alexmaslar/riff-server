use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::warn;

const COVER_SIZE: u32 = 1024;

// ─── Cache Directory ────────────────────────────────────────────────────────

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

// ─── Color Helpers ──────────────────────────────────────────────────────────

fn darken(color: Rgba<u8>, factor: f32) -> Rgba<u8> {
    Rgba([
        (color.0[0] as f32 * factor) as u8,
        (color.0[1] as f32 * factor) as u8,
        (color.0[2] as f32 * factor) as u8,
        color.0[3],
    ])
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s == 0.0 {
        let v = (l * 255.0) as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue_to_rgb = |t: f32| -> f32 {
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (
        (hue_to_rgb(h + 1.0 / 3.0) * 255.0) as u8,
        (hue_to_rgb(h) * 255.0) as u8,
        (hue_to_rgb(h - 1.0 / 3.0) * 255.0) as u8,
    )
}

// ─── Image Effects ──────────────────────────────────────────────────────────

/// Extract dominant color by averaging the center 50% of the image.
fn extract_dominant_color(img: &RgbaImage) -> Rgba<u8> {
    let (w, h) = img.dimensions();
    let x0 = w / 4;
    let y0 = h / 4;
    let x1 = 3 * w / 4;
    let y1 = 3 * h / 4;

    let mut r_sum = 0u64;
    let mut g_sum = 0u64;
    let mut b_sum = 0u64;
    let mut count = 0u64;

    for y in y0..y1 {
        for x in x0..x1 {
            let p = img.get_pixel(x, y);
            r_sum += p.0[0] as u64;
            g_sum += p.0[1] as u64;
            b_sum += p.0[2] as u64;
            count += 1;
        }
    }

    if count == 0 {
        return Rgba([60, 60, 60, 255]);
    }

    Rgba([
        (r_sum / count) as u8,
        (g_sum / count) as u8,
        (b_sum / count) as u8,
        255,
    ])
}

/// Apply circular mask with 2px anti-aliased edge fade.
fn apply_circular_mask(img: &mut RgbaImage) {
    let (w, h) = img.dimensions();
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let radius = cx.min(cy);
    let fade = 2.0;

    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist > radius {
                img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            } else if dist > radius - fade {
                let alpha = ((radius - dist) / fade).clamp(0.0, 1.0);
                let p = img.get_pixel(x, y);
                img.put_pixel(x, y, Rgba([p.0[0], p.0[1], p.0[2], (p.0[3] as f32 * alpha) as u8]));
            }
        }
    }
}

// ─── Color Palettes ─────────────────────────────────────────────────────────

/// Map genre name to gradient colors. Falls back to a hash-based HSL pair.
fn genre_gradient_colors(genre: &str) -> (Rgba<u8>, Rgba<u8>) {
    let lower = genre.to_lowercase();
    match lower.as_str() {
        "rock" => (Rgba([180, 40, 30, 255]), Rgba([60, 15, 10, 255])),
        "metal" | "heavy metal" => (Rgba([80, 80, 80, 255]), Rgba([20, 20, 20, 255])),
        "punk" | "punk rock" => (Rgba([200, 50, 80, 255]), Rgba([50, 10, 25, 255])),
        "jazz" => (Rgba([40, 60, 140, 255]), Rgba([15, 20, 50, 255])),
        "blues" => (Rgba([30, 50, 160, 255]), Rgba([10, 15, 60, 255])),
        "classical" => (Rgba([160, 130, 80, 255]), Rgba([40, 30, 20, 255])),
        "electronic" | "edm" => (Rgba([0, 180, 200, 255]), Rgba([10, 30, 60, 255])),
        "hip hop" | "hip-hop" | "rap" => (Rgba([200, 120, 20, 255]), Rgba([50, 25, 5, 255])),
        "r&b" | "rnb" | "soul" => (Rgba([140, 40, 120, 255]), Rgba([40, 10, 35, 255])),
        "pop" => (Rgba([200, 60, 150, 255]), Rgba([60, 15, 45, 255])),
        "country" => (Rgba([180, 140, 60, 255]), Rgba([50, 35, 15, 255])),
        "folk" => (Rgba([120, 140, 70, 255]), Rgba([30, 40, 20, 255])),
        "reggae" => (Rgba([40, 160, 60, 255]), Rgba([15, 40, 20, 255])),
        "latin" | "latin pop" => (Rgba([200, 80, 40, 255]), Rgba([60, 20, 10, 255])),
        "ambient" => (Rgba([60, 80, 120, 255]), Rgba([15, 20, 40, 255])),
        _ => {
            // Hash-based fallback: deterministic gradient from genre name
            let mut hasher = DefaultHasher::new();
            lower.hash(&mut hasher);
            let h = hasher.finish();
            let hue = (h % 360) as f32 / 360.0;
            let (r1, g1, b1) = hsl_to_rgb(hue, 0.6, 0.45);
            let (r2, g2, b2) = hsl_to_rgb(hue, 0.5, 0.15);
            (Rgba([r1, g1, b1, 255]), Rgba([r2, g2, b2, 255]))
        }
    }
}

/// Map decade to warm/era-specific gradient colors.
fn decade_gradient_colors(decade: i32) -> (Rgba<u8>, Rgba<u8>) {
    match decade {
        1950 => (Rgba([180, 150, 100, 255]), Rgba([60, 40, 25, 255])),
        1960 => (Rgba([200, 130, 60, 255]), Rgba([50, 30, 15, 255])),
        1970 => (Rgba([180, 100, 40, 255]), Rgba([50, 25, 10, 255])),
        1980 => (Rgba([200, 60, 150, 255]), Rgba([50, 15, 40, 255])),
        1990 => (Rgba([80, 140, 180, 255]), Rgba([20, 35, 50, 255])),
        2000 => (Rgba([160, 60, 60, 255]), Rgba([45, 15, 15, 255])),
        2010 => (Rgba([60, 120, 180, 255]), Rgba([15, 30, 50, 255])),
        2020 => (Rgba([140, 80, 200, 255]), Rgba([35, 20, 55, 255])),
        _ => (Rgba([160, 120, 80, 255]), Rgba([40, 30, 20, 255])),
    }
}

// ─── Legacy 2×2 Collage ─────────────────────────────────────────────────────

/// Generate a dark gradient fallback cell for empty collage slots.
fn generate_fallback_cell(size: u32) -> RgbaImage {
    let dark_bg = Rgba([18, 18, 18, 255]);
    let mut cell = RgbaImage::from_pixel(size, size, dark_bg);

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
    let size = COVER_SIZE;
    let cell = 510u32;
    let gap = 2u32;
    let dark_bg = Rgba([18, 18, 18, 255]);

    let mut canvas = RgbaImage::from_pixel(size, size, dark_bg);

    let positions: [(u32, u32); 4] = [
        (0, 0),
        (cell + gap, 0),
        (0, cell + gap),
        (cell + gap, cell + gap),
    ];

    let mut filled = [false; 4];

    for (i, path) in cover_paths.iter().take(4).enumerate() {
        if let Ok(img) = image::open(path) {
            let resized = img.resize_exact(cell, cell, image::imageops::FilterType::Lanczos3);
            let (px, py) = positions[i];
            image::imageops::overlay(&mut canvas, &resized.to_rgba8(), px as i64, py as i64);
            filled[i] = true;
        }
    }

    for (i, &has_cover) in filled.iter().enumerate() {
        if !has_cover {
            let fallback = generate_fallback_cell(cell);
            let (px, py) = positions[i];
            image::imageops::overlay(&mut canvas, &fallback, px as i64, py as i64);
        }
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

// ─── Per-Type Cover Generators ──────────────────────────────────────────────

/// Artist mix: hero image on solid dominant-color background.
/// Fallback: 2×2 collage if no artist image.
pub fn generate_artist_mix_cover(
    artist_image: Option<&RgbaImage>,
    album_cover_paths: &[&Path],
) -> Result<DynamicImage> {
    let size = COVER_SIZE;

    match artist_image {
        Some(img) => {
            let dominant = extract_dominant_color(img);
            let bg = darken(dominant, 0.7);
            let mut canvas = RgbaImage::from_pixel(size, size, bg);

            // Resize artist image to 640×640, centered at (192, 192)
            let hero = image::imageops::resize(img, 640, 640, image::imageops::FilterType::Lanczos3);
            image::imageops::overlay(&mut canvas, &hero, 192, 192);

            Ok(DynamicImage::ImageRgba8(canvas))
        }
        None => {
            let collage = generate_mix_collage(album_cover_paths)?;
            Ok(collage)
        }
    }
}

/// Genre mix: 5 circular album art portraits scattered on solid genre color.
pub fn generate_genre_mix_cover(genre: &str, cover_images: &[RgbaImage]) -> Result<DynamicImage> {
    let size = COVER_SIZE;
    let (bg_color, _) = genre_gradient_colors(genre);
    let mut canvas = RgbaImage::from_pixel(size, size, bg_color);

    // Circle layout: (diameter, center_x, center_y)
    let circles: [(u32, i64, i64); 5] = [
        (400, 300, 350),
        (400, 724, 400),
        (280, 512, 700),
        (220, 150, 680),
        (220, 870, 200),
    ];

    for (i, &(diam, cx, cy)) in circles.iter().enumerate() {
        let x = cx - (diam as i64 / 2);
        let y = cy - (diam as i64 / 2);

        if let Some(img) = cover_images.get(i) {
            let resized = image::imageops::resize(img, diam, diam, image::imageops::FilterType::Lanczos3);
            let mut circle = resized;
            apply_circular_mask(&mut circle);
            image::imageops::overlay(&mut canvas, &circle, x, y);
        } else {
            // Solid-color circle fallback
            let fill = darken(bg_color, 0.5);
            let mut solid = RgbaImage::from_pixel(diam, diam, fill);
            apply_circular_mask(&mut solid);
            image::imageops::overlay(&mut canvas, &solid, x, y);
        }
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

/// Deep cuts: circular artist image on solid dark background.
/// Fallback: first album cover as circle, or solid dark purple.
pub fn generate_deep_cuts_cover(
    artist_image: Option<&RgbaImage>,
    album_cover_paths: &[&Path],
) -> Result<DynamicImage> {
    let size = COVER_SIZE;
    let bg = Rgba([25, 12, 35, 255]);
    let mut canvas = RgbaImage::from_pixel(size, size, bg);

    // Try artist image, then first album cover
    let source = if let Some(img) = artist_image {
        Some(img.clone())
    } else {
        album_cover_paths
            .first()
            .and_then(|p| image::open(p).ok())
            .map(|img| img.to_rgba8())
    };

    if let Some(img) = source {
        let diam = 640u32;
        let resized = image::imageops::resize(&img, diam, diam, image::imageops::FilterType::Lanczos3);
        let mut circle = resized;
        apply_circular_mask(&mut circle);
        let x = (size - diam) as i64 / 2;
        let y = (size - diam) as i64 / 2;
        image::imageops::overlay(&mut canvas, &circle, x, y);
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

/// Decade mix: 4 circular album art portraits in 2×2 layout on solid warm color.
pub fn generate_decade_mix_cover(decade: i32, cover_images: &[RgbaImage]) -> Result<DynamicImage> {
    let size = COVER_SIZE;
    let (bg_color, _) = decade_gradient_colors(decade);
    let mut canvas = RgbaImage::from_pixel(size, size, bg_color);

    // Triangular layout: two smaller behind, one large in front (drawn last)
    // Draw order matters — back circles first, then large on top
    let layout: [(u32, i64, i64); 3] = [
        (300, 270, 490),  // left (behind)
        (300, 754, 490),  // right (behind)
        (420, 512, 370),  // center-top (in front)
    ];

    let draw_circle = |canvas: &mut RgbaImage, idx: usize, diam: u32, cx: i64, cy: i64| {
        let x = cx - (diam as i64 / 2);
        let y = cy - (diam as i64 / 2);

        if let Some(img) = cover_images.get(idx) {
            let resized = image::imageops::resize(img, diam, diam, image::imageops::FilterType::Lanczos3);
            let mut circle = resized;
            apply_circular_mask(&mut circle);
            image::imageops::overlay(canvas, &circle, x, y);
        } else {
            let fill = darken(bg_color, 0.4);
            let mut solid = RgbaImage::from_pixel(diam, diam, fill);
            apply_circular_mask(&mut solid);
            image::imageops::overlay(canvas, &solid, x, y);
        }
    };

    // Index mapping: cover_images[0] → large front, [1] → left, [2] → right
    draw_circle(&mut canvas, 1, layout[0].0, layout[0].1, layout[0].2);
    draw_circle(&mut canvas, 2, layout[1].0, layout[1].1, layout[1].2);
    draw_circle(&mut canvas, 0, layout[2].0, layout[2].1, layout[2].2);

    Ok(DynamicImage::ImageRgba8(canvas))
}

// ─── Artist Image Download ──────────────────────────────────────────────────

/// Download an artist image with local caching (7-day TTL).
/// Returns None on any error (network, decode, etc.) — callers should fall back.
pub async fn download_artist_image(artist_id: &str, image_url: &str) -> Result<Option<DynamicImage>> {
    let cache_dir = get_cache_dir()?;
    let cache_path = cache_dir.join(format!("artist_{}.jpg", artist_id));

    // Check cache with 7-day TTL
    if cache_path.exists() {
        if let Ok(meta) = std::fs::metadata(&cache_path) {
            if let Ok(modified) = meta.modified() {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                if age.as_secs() < 7 * 24 * 3600 {
                    match image::open(&cache_path) {
                        Ok(img) => return Ok(Some(img)),
                        Err(e) => warn!("cached artist image corrupt for {artist_id}: {e}"),
                    }
                }
            }
        }
    }

    // Download with timeout
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = match client.get(image_url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("failed to download artist image for {artist_id}: {e}");
            return Ok(None);
        }
    };

    if !resp.status().is_success() {
        warn!("artist image download returned {} for {artist_id}", resp.status());
        return Ok(None);
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!("failed to read artist image bytes for {artist_id}: {e}");
            return Ok(None);
        }
    };

    // Decode and cache
    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(e) => {
            warn!("failed to decode artist image for {artist_id}: {e}");
            return Ok(None);
        }
    };

    // Save to cache (best-effort)
    if let Err(e) = img.save(&cache_path) {
        warn!("failed to cache artist image for {artist_id}: {e}");
    }

    Ok(Some(img))
}
