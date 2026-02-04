use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use imageproc::drawing::draw_filled_circle_mut;
use std::path::Path;

/// Select vinyl texture based on play count
fn select_vinyl_texture(play_count: u32) -> &'static str {
    match play_count {
        0..=9 => "Vinyl_Clean.png",
        10..=29 => "Vinyl_Black.png",
        _ => "Vinyl_Dirty.png",
    }
}

/// Select worn texture based on play count
fn select_worn_texture(play_count: u32) -> &'static str {
    match play_count {
        0 => "Cover_01.png",       // Pristine, still wrapped
        1..=9 => "Cover_05.png",   // Opened, light use
        10..=19 => "Cover_07.png", // Moderate wear
        20..=29 => "Cover_09.png", // Well-worn
        _ => "Cover_12.png",       // Heavily worn (30+)
    }
}

/// Load vinyl asset from embedded assets
fn load_vinyl_asset(name: &str) -> Result<&'static [u8]> {
    match name {
        "Vinyl_Clean.png" => Ok(include_bytes!("../../assets/vinyl/Vinyl_Clean.png")),
        "Vinyl_Black.png" => Ok(include_bytes!("../../assets/vinyl/Vinyl_Black.png")),
        "Vinyl_Dirty.png" => Ok(include_bytes!("../../assets/vinyl/Vinyl_Dirty.png")),
        _ => anyhow::bail!("Unknown vinyl texture: {}", name),
    }
}

/// Load worn asset from embedded assets
fn load_worn_asset(name: &str) -> Result<&'static [u8]> {
    match name {
        "Cover_01.png" => Ok(include_bytes!("../../assets/worn/Cover_01.png")),
        "Cover_05.png" => Ok(include_bytes!("../../assets/worn/Cover_05.png")),
        "Cover_07.png" => Ok(include_bytes!("../../assets/worn/Cover_07.png")),
        "Cover_09.png" => Ok(include_bytes!("../../assets/worn/Cover_09.png")),
        "Cover_12.png" => Ok(include_bytes!("../../assets/worn/Cover_12.png")),
        _ => anyhow::bail!("Unknown worn texture: {}", name),
    }
}

/// Apply multiply blend mode for worn textures
fn overlay_multiply(base: &mut RgbaImage, texture: &RgbaImage) {
    for (x, y, pixel) in texture.enumerate_pixels() {
        if x < base.width() && y < base.height() {
            let base_pixel = base.get_pixel(x, y);
            let blended = Rgba([
                ((base_pixel[0] as u16 * pixel[0] as u16) / 255) as u8,
                ((base_pixel[1] as u16 * pixel[1] as u16) / 255) as u8,
                ((base_pixel[2] as u16 * pixel[2] as u16) / 255) as u8,
                base_pixel[3],
            ]);
            base.put_pixel(x, y, blended);
        }
    }
}

/// Generate vinyl disc effect with album art as center label
pub fn generate_vinyl_effect(
    source_path: &Path,
    size: u32,
    with_hole: bool,
    play_count: u32,
) -> Result<DynamicImage> {
    // Load source album art
    let album_art = image::open(source_path)
        .context("Failed to open album art")?;

    // Load appropriate vinyl disc texture based on play count
    let vinyl_texture = select_vinyl_texture(play_count);
    let vinyl_bytes = load_vinyl_asset(vinyl_texture)?;
    let mut vinyl_disc = image::load_from_memory(vinyl_bytes)
        .context("Failed to load vinyl texture")?
        .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
        .to_rgba8();

    // Calculate label size (~40% of disc radius)
    let label_size = (size as f32 * 0.4) as u32;
    let label = album_art.resize_exact(
        label_size,
        label_size,
        image::imageops::FilterType::Lanczos3,
    );

    // Composite album art as center label
    let label_offset = (size - label_size) / 2;
    image::imageops::overlay(
        &mut vinyl_disc,
        &label.to_rgba8(),
        label_offset as i64,
        label_offset as i64,
    );

    // Punch center hole if requested
    if with_hole {
        let hole_radius = (size as f32 * 0.05) as u32;
        draw_filled_circle_mut(
            &mut vinyl_disc,
            ((size / 2) as i32, (size / 2) as i32),
            hole_radius as i32,
            Rgba([0, 0, 0, 0]), // Transparent
        );
    }

    Ok(DynamicImage::ImageRgba8(vinyl_disc))
}

/// Generate worn effect by compositing wear texture over base image
pub fn generate_worn_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    // Select texture based on play count (always applied, even at 0)
    let texture_name = select_worn_texture(play_count);
    let texture_bytes = load_worn_asset(texture_name)?;
    let texture = image::load_from_memory(texture_bytes)
        .context("Failed to load worn texture")?;

    // Resize texture to match base image
    let texture_resized = texture.resize_exact(
        base_image.width(),
        base_image.height(),
        image::imageops::FilterType::Lanczos3,
    );

    // Composite: multiply blend mode
    let mut result = base_image.to_rgba8();
    overlay_multiply(&mut result, &texture_resized.to_rgba8());

    Ok(DynamicImage::ImageRgba8(result))
}

/// Generate wrapped sleeve effect (placeholder - can be implemented later)
pub fn generate_wrapped_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    // Apply worn effect first
    generate_worn_effect(base_image, play_count)
    // TODO: Add plastic grain, highlights, edge reflections, vignette
}

/// Generate highlights effect (placeholder - can be implemented later)
pub fn generate_highlights_effect(
    base_image: &DynamicImage,
    play_count: u32,
) -> Result<DynamicImage> {
    // Apply worn effect first
    generate_worn_effect(base_image, play_count)
    // TODO: Add highlight texture with soft-light blend
}
