use std::fs;
use std::path::Path;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Tag};
use symphonia::core::probe::Hint;

#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub duration_seconds: i32,
    pub format: String,
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub file_size_bytes: i64,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
}

pub fn extract_metadata(path: &Path) -> anyhow::Result<TrackMetadata> {
    let file = fs::File::open(path)?;
    let file_size = file.metadata()?.len() as i64;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let meta_opts = MetadataOptions::default();
    let fmt_opts = FormatOptions::default();

    let mut probed = symphonia::default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts)?;

    let mut format = probed.format;

    // Extract codec params from the default (first) track
    let mut sample_rate: i32 = 44100;
    let mut bit_depth: i32 = 16;
    let mut duration_seconds: i32 = 0;

    if let Some(track) = format.default_track() {
        let params = &track.codec_params;
        if let Some(sr) = params.sample_rate {
            sample_rate = sr as i32;
        }
        if let Some(bd) = params.bits_per_sample {
            bit_depth = bd as i32;
        }
        // Calculate duration from n_frames and sample_rate
        if let (Some(n_frames), Some(sr)) = (params.n_frames, params.sample_rate) {
            if sr > 0 {
                duration_seconds = (n_frames as f64 / sr as f64).round() as i32;
            }
        }
    }

    // Collect all tags from metadata revisions
    let mut tags: Vec<Tag> = Vec::new();

    // Tags from the probe metadata
    if let Some(probe_meta) = probed.metadata.get() {
        if let Some(rev) = probe_meta.current() {
            tags.extend(rev.tags().iter().cloned());
        }
    }

    // Tags from the container metadata
    if let Some(rev) = format.metadata().current() {
        tags.extend(rev.tags().iter().cloned());
    }

    let audio_format = detect_format(path);

    let mut meta = TrackMetadata {
        title: filename_without_ext(path),
        artist: "Unknown Artist".to_string(),
        album: "Unknown Album".to_string(),
        track_number: 1,
        disc_number: 1,
        duration_seconds,
        format: audio_format,
        sample_rate,
        bit_depth,
        file_size_bytes: file_size,
        year: None,
        genre: Vec::new(),
        style: Vec::new(),
    };

    for tag in &tags {
        match tag.std_key {
            Some(StandardTagKey::TrackTitle) => {
                meta.title = tag.value.to_string();
            }
            Some(StandardTagKey::Artist) | Some(StandardTagKey::AlbumArtist) => {
                // Prefer album artist if set, but any artist is better than "Unknown"
                if meta.artist == "Unknown Artist"
                    || tag.std_key == Some(StandardTagKey::AlbumArtist)
                {
                    meta.artist = tag.value.to_string();
                }
            }
            Some(StandardTagKey::Album) => {
                meta.album = tag.value.to_string();
            }
            Some(StandardTagKey::TrackNumber) => {
                if let Ok(n) = tag.value.to_string().split('/').next().unwrap_or("1").parse::<i32>()
                {
                    meta.track_number = n;
                }
            }
            Some(StandardTagKey::DiscNumber) => {
                if let Ok(n) = tag.value.to_string().split('/').next().unwrap_or("1").parse::<i32>()
                {
                    meta.disc_number = n;
                }
            }
            Some(StandardTagKey::Date) | Some(StandardTagKey::OriginalDate) => {
                // Try to parse year from date string (e.g., "2023" or "2023-01-15")
                let date_str = tag.value.to_string();
                if let Ok(y) = date_str.get(..4).unwrap_or("").parse::<i32>() {
                    if meta.year.is_none() || tag.std_key == Some(StandardTagKey::Date) {
                        meta.year = Some(y);
                    }
                }
            }
            Some(StandardTagKey::Genre) => {
                let genre_str = tag.value.to_string();
                for g in genre_str.split(';').map(|s| s.trim().to_string()) {
                    if !g.is_empty() && !meta.genre.contains(&g) {
                        meta.genre.push(g);
                    }
                }
            }
            _ => {}
        }
    }

    // If still unknown, try path-based fallback for artist/album
    if meta.artist == "Unknown Artist" || meta.album == "Unknown Album" {
        if let Some(path_meta) = metadata_from_path(path) {
            if meta.artist == "Unknown Artist" {
                meta.artist = path_meta.artist;
            }
            if meta.album == "Unknown Album" {
                meta.album = path_meta.album;
            }
        }
    }

    Ok(meta)
}

/// Fallback: parse metadata from directory structure
/// Expects: /library/Artist Name/Album Name (Year)/01 - Track Name.flac
pub fn metadata_from_path(path: &Path) -> Option<TrackMetadata> {
    let parent = path.parent()?;
    let grandparent = parent.parent()?;

    let album_dir = parent.file_name()?.to_str()?;
    let artist_name = grandparent.file_name()?.to_str()?.to_string();

    // Parse "Album Name (Year)" or just "Album Name"
    let (album_name, year) = parse_album_dir(album_dir);

    // Parse "01 - Track Name.flac"
    let filename = filename_without_ext(path);
    let title = parse_track_filename(&filename);

    let track_number = filename
        .split(['-', ' ', '.'])
        .next()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(1);

    let file_size = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);

    Some(TrackMetadata {
        title,
        artist: artist_name,
        album: album_name,
        track_number,
        disc_number: 1,
        duration_seconds: 0,
        format: detect_format(path),
        sample_rate: 44100,
        bit_depth: 16,
        file_size_bytes: file_size,
        year,
        genre: Vec::new(),
        style: Vec::new(),
    })
}

fn detect_format(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("flac") => "FLAC".to_string(),
        Some("m4a") => "ALAC".to_string(),
        Some("wav") => "WAV".to_string(),
        Some("aiff") | Some("aif") => "AIFF".to_string(),
        Some(ext) => ext.to_uppercase(),
        None => "UNKNOWN".to_string(),
    }
}

fn filename_without_ext(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string()
}

fn parse_album_dir(dir_name: &str) -> (String, Option<i32>) {
    // Match "Album Name (2023)" pattern
    if let Some(paren_start) = dir_name.rfind('(') {
        if let Some(paren_end) = dir_name.rfind(')') {
            if paren_end > paren_start {
                let year_str = &dir_name[paren_start + 1..paren_end];
                if let Ok(year) = year_str.trim().parse::<i32>() {
                    if (1900..=2100).contains(&year) {
                        let album = dir_name[..paren_start].trim().to_string();
                        return (album, Some(year));
                    }
                }
            }
        }
    }
    (dir_name.to_string(), None)
}

fn parse_track_filename(filename: &str) -> String {
    // Strip leading track number patterns: "01 - ", "01. ", "1 "
    let trimmed = filename.trim();

    // Try "NN - Title" pattern
    if let Some(pos) = trimmed.find(" - ") {
        let prefix = &trimmed[..pos];
        if prefix.trim().chars().all(|c| c.is_ascii_digit()) {
            return trimmed[pos + 3..].trim().to_string();
        }
    }

    // Try "NN. Title" pattern
    if let Some(pos) = trimmed.find(". ") {
        let prefix = &trimmed[..pos];
        if prefix.trim().chars().all(|c| c.is_ascii_digit()) {
            return trimmed[pos + 2..].trim().to_string();
        }
    }

    trimmed.to_string()
}
