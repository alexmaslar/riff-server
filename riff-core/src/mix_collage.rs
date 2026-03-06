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

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    (a as f32 + (b as f32 - a as f32) * t) as u8
}

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

/// Apply a color tint over the image.
fn apply_tint(img: &mut RgbaImage, tint: Rgba<u8>, strength: f32) {
    for p in img.pixels_mut() {
        p.0[0] = lerp(p.0[0], tint.0[0], strength);
        p.0[1] = lerp(p.0[1], tint.0[1], strength);
        p.0[2] = lerp(p.0[2], tint.0[2], strength);
    }
}

/// Apply vignette effect — darken edges based on distance from center.
fn apply_vignette(img: &mut RgbaImage, strength: f32) {
    let (w, h) = img.dimensions();
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let max_dist = (cx * cx + cy * cy).sqrt();

    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt() / max_dist;
            let factor = 1.0 - (dist * strength).clamp(0.0, 1.0);
            let p = img.get_pixel(x, y);
            img.put_pixel(
                x,
                y,
                Rgba([
                    (p.0[0] as f32 * factor) as u8,
                    (p.0[1] as f32 * factor) as u8,
                    (p.0[2] as f32 * factor) as u8,
                    p.0[3],
                ]),
            );
        }
    }
}

// ─── Background Generators ──────────────────────────────────────────────────

fn generate_vertical_gradient(size: u32, top: Rgba<u8>, bottom: Rgba<u8>) -> RgbaImage {
    let mut img = RgbaImage::new(size, size);
    for y in 0..size {
        let t = y as f32 / (size - 1) as f32;
        let r = lerp(top.0[0], bottom.0[0], t);
        let g = lerp(top.0[1], bottom.0[1], t);
        let b = lerp(top.0[2], bottom.0[2], t);
        for x in 0..size {
            img.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    img
}

fn generate_radial_gradient(size: u32, center: Rgba<u8>, edge: Rgba<u8>) -> RgbaImage {
    let mut img = RgbaImage::new(size, size);
    let cx = size as f32 / 2.0;
    let cy = size as f32 / 2.0;
    let max_dist = (cx * cx + cy * cy).sqrt();

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let t = (dx * dx + dy * dy).sqrt() / max_dist;
            let r = lerp(center.0[0], edge.0[0], t);
            let g = lerp(center.0[1], edge.0[1], t);
            let b = lerp(center.0[2], edge.0[2], t);
            img.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    img
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

/// Artist mix: hero image on radial gradient from dominant color.
/// Fallback: 2×2 collage on gradient if no artist image.
pub fn generate_artist_mix_cover(
    artist_image: Option<&RgbaImage>,
    album_cover_paths: &[&Path],
) -> Result<DynamicImage> {
    let size = COVER_SIZE;

    match artist_image {
        Some(img) => {
            let dominant = extract_dominant_color(img);
            let edge = darken(dominant, 0.3);
            let mut canvas = generate_radial_gradient(size, dominant, edge);

            // Resize artist image to 640×640, centered at (192, 192)
            let hero = image::imageops::resize(img, 640, 640, image::imageops::FilterType::Lanczos3);
            image::imageops::overlay(&mut canvas, &hero, 192, 192);

            Ok(DynamicImage::ImageRgba8(canvas))
        }
        None => {
            // Fallback: collage on a dark gradient
            let collage = generate_mix_collage(album_cover_paths)?;
            Ok(collage)
        }
    }
}

/// Genre mix: 5 circular album art portraits scattered on vertical gradient.
pub fn generate_genre_mix_cover(genre: &str, cover_images: &[RgbaImage]) -> Result<DynamicImage> {
    let size = COVER_SIZE;
    let (top, bottom) = genre_gradient_colors(genre);
    let mut canvas = generate_vertical_gradient(size, top, bottom);

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
            let fill = darken(top, 0.5);
            let mut solid = RgbaImage::from_pixel(diam, diam, fill);
            apply_circular_mask(&mut solid);
            image::imageops::overlay(&mut canvas, &solid, x, y);
        }
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

/// Deep cuts: moody full-bleed artist image with purple tint, vignette, darken.
/// Fallback: first album cover with same treatment, or dark purple→black gradient.
pub fn generate_deep_cuts_cover(
    artist_image: Option<&RgbaImage>,
    album_cover_paths: &[&Path],
) -> Result<DynamicImage> {
    let size = COVER_SIZE;

    // Try artist image, then first album cover, then gradient fallback
    let base_image = if let Some(img) = artist_image {
        Some(img.clone())
    } else {
        // Try first album cover
        album_cover_paths
            .first()
            .and_then(|p| image::open(p).ok())
            .map(|img| img.to_rgba8())
    };

    let mut canvas = match base_image {
        Some(img) => {
            image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3)
        }
        None => {
            // Dark purple→black gradient fallback
            generate_vertical_gradient(
                size,
                Rgba([60, 20, 80, 255]),
                Rgba([10, 5, 15, 255]),
            )
        }
    };

    // Apply effects: purple tint, vignette, darken
    apply_tint(&mut canvas, Rgba([120, 40, 160, 255]), 0.35);
    apply_vignette(&mut canvas, 1.5);
    // Overall darken ×0.6
    for p in canvas.pixels_mut() {
        p.0[0] = (p.0[0] as f32 * 0.6) as u8;
        p.0[1] = (p.0[1] as f32 * 0.6) as u8;
        p.0[2] = (p.0[2] as f32 * 0.6) as u8;
    }

    Ok(DynamicImage::ImageRgba8(canvas))
}

/// Decade mix: 4 circular album art portraits in 2×2 layout on warm gradient.
pub fn generate_decade_mix_cover(decade: i32, cover_images: &[RgbaImage]) -> Result<DynamicImage> {
    let size = COVER_SIZE;
    let (top, bottom) = decade_gradient_colors(decade);
    let mut canvas = generate_vertical_gradient(size, top, bottom);

    // 2×2 balanced layout: (diameter, center_x, center_y)
    let circles: [(u32, i64, i64); 4] = [
        (340, 280, 300),
        (340, 744, 300),
        (340, 280, 724),
        (340, 744, 724),
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
            // Solid warm-tone circle fallback
            let fill = darken(top, 0.4);
            let mut solid = RgbaImage::from_pixel(diam, diam, fill);
            apply_circular_mask(&mut solid);
            image::imageops::overlay(&mut canvas, &solid, x, y);
        }
    }

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
