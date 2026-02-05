use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use std::path::{Path, PathBuf};

/// Select worn texture pair based on play count
fn select_worn_texture(play_count: u32) -> &'static str {
    match play_count {
        0 => "Cover_04",       // Pristine, still wrapped
        1..=9 => "Cover_05",   // Opened, light use
        10..=19 => "Cover_07", // Moderate wear
        20..=29 => "Cover_09", // Well-worn
        _ => "Cover_12",       // Heavily worn (30+)
    }
}

/// Load shadow/light layer assets extracted from PSDs
fn load_layer_assets(base_name: &str) -> Result<(&'static [u8], &'static [u8])> {
    let (shadows, lights) = match base_name {
        "Cover_04" => (
            include_bytes!("../../assets/worn/Cover_04_shadows.png") as &[u8],
            include_bytes!("../../assets/worn/Cover_04_lights.png") as &[u8],
        ),
        "Cover_05" => (
            include_bytes!("../../assets/worn/Cover_05_shadows.png") as &[u8],
            include_bytes!("../../assets/worn/Cover_05_lights.png") as &[u8],
        ),
        "Cover_07" => (
            include_bytes!("../../assets/worn/Cover_07_shadows.png") as &[u8],
            include_bytes!("../../assets/worn/Cover_07_lights.png") as &[u8],
        ),
        "Cover_09" => (
            include_bytes!("../../assets/worn/Cover_09_shadows.png") as &[u8],
            include_bytes!("../../assets/worn/Cover_09_lights.png") as &[u8],
        ),
        "Cover_12" => (
            include_bytes!("../../assets/worn/Cover_12_shadows.png") as &[u8],
            include_bytes!("../../assets/worn/Cover_12_lights.png") as &[u8],
        ),
        _ => anyhow::bail!("Unknown worn texture: {}", base_name),
    };
    Ok((shadows, lights))
}

/// Multiply blend: white texture = no effect, dark texture = darkens base.
/// Formula: base * tex / 255
fn multiply_blend(base: &mut RgbaImage, texture: &RgbaImage, opacity: f32) {
    for (x, y, tex_pixel) in texture.enumerate_pixels() {
        if x < base.width() && y < base.height() {
            let base_pixel = base.get_pixel(x, y);
            let alpha = (tex_pixel[3] as f32 / 255.0) * opacity;

            let blend_channel = |b: u8, t: u8| -> u8 {
                let multiplied = (b as u16 * t as u16 / 255) as f32;
                (b as f32 * (1.0 - alpha) + multiplied * alpha) as u8
            };

            let r = blend_channel(base_pixel[0], tex_pixel[0]);
            let g = blend_channel(base_pixel[1], tex_pixel[1]);
            let b = blend_channel(base_pixel[2], tex_pixel[2]);

            base.put_pixel(x, y, Rgba([r, g, b, base_pixel[3]]));
        }
    }
}

/// Screen blend: black texture = no effect, bright texture = brightens base.
/// Formula: 255 - ((255 - base) * (255 - tex)) / 255
fn screen_blend(base: &mut RgbaImage, texture: &RgbaImage, opacity: f32) {
    for (x, y, tex_pixel) in texture.enumerate_pixels() {
        if x < base.width() && y < base.height() {
            let base_pixel = base.get_pixel(x, y);
            let alpha = (tex_pixel[3] as f32 / 255.0) * opacity;

            let blend_channel = |b: u8, t: u8| -> u8 {
                let screened = 255u16 - (255 - b as u16) * (255 - t as u16) / 255;
                (b as f32 * (1.0 - alpha) + screened as f32 * alpha) as u8
            };

            let r = blend_channel(base_pixel[0], tex_pixel[0]);
            let g = blend_channel(base_pixel[1], tex_pixel[1]);
            let b = blend_channel(base_pixel[2], tex_pixel[2]);

            base.put_pixel(x, y, Rgba([r, g, b, base_pixel[3]]));
        }
    }
}

/// Generate worn effect by compositing shadow + light layers over base image
pub fn generate_worn_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    let texture_name = select_worn_texture(play_count);
    let (shadow_bytes, light_bytes) = load_layer_assets(texture_name)?;

    let shadows = image::load_from_memory(shadow_bytes)
        .context("Failed to load shadow texture")?;
    let lights = image::load_from_memory(light_bytes)
        .context("Failed to load light texture")?;

    let w = base_image.width();
    let h = base_image.height();

    let shadows_resized = shadows.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    let lights_resized = lights.resize_exact(w, h, image::imageops::FilterType::Lanczos3);

    let mut result = base_image.to_rgba8();

    // Apply shadows (multiply) then lights (screen), matching the PSD layer order
    multiply_blend(&mut result, &shadows_resized.to_rgba8(), 1.0);
    screen_blend(&mut result, &lights_resized.to_rgba8(), 1.0);

    Ok(DynamicImage::ImageRgba8(result))
}

/// Generate wrapped sleeve effect
pub fn generate_wrapped_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    generate_worn_effect(base_image, play_count)
}

/// Generate highlights effect (placeholder - can be implemented later)
pub fn generate_highlights_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    generate_worn_effect(base_image, play_count)
}

/// Generate wrapped cover and save as cover_wrapped.jpg in the album's folder.
/// Returns the output path and the generated image.
pub fn generate_and_save_wrapped(
    cover_art_path: &Path,
    play_count: u32,
    _size: u32,
) -> Result<(PathBuf, DynamicImage)> {
    let base = image::open(cover_art_path)
        .context("Failed to open album art")?;

    let image = generate_wrapped_effect(&base, play_count)?;

    let output_path = cover_art_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cover_art_path has no parent directory"))?
        .join("cover_wrapped.jpg");

    // Save as JPEG quality 92
    let file = std::fs::File::create(&output_path)
        .with_context(|| format!("Failed to create {}", output_path.display()))?;
    let mut buf = std::io::BufWriter::new(file);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92);
    image.write_with_encoder(encoder)
        .context("Failed to encode wrapped cover as JPEG")?;

    Ok((output_path, image))
}
