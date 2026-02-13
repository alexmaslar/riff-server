use std::fs;
use std::path::Path;
use symphonia::core::codecs::CodecType;
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
    pub bit_depth: Option<i32>,
    pub file_size_bytes: i64,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub composer: Option<String>,
    pub language: Option<String>,
    pub bpm: Option<f64>,
    pub musical_key: Option<String>,
    pub mood: Option<String>,
    pub is_compilation: bool,
    pub replay_gain_track_gain: Option<f64>,
    pub replay_gain_track_peak: Option<f64>,
    pub replay_gain_album_gain: Option<f64>,
    pub replay_gain_album_peak: Option<f64>,
    pub musicbrainz_recording_id: Option<String>,
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
    let mut bit_depth: Option<i32> = None;
    let mut duration_seconds: i32 = 0;
    let mut codec_type: Option<CodecType> = None;

    if let Some(track) = format.default_track() {
        let params = &track.codec_params;
        codec_type = Some(params.codec);
        if let Some(sr) = params.sample_rate {
            sample_rate = sr as i32;
        }
        // Only store bit_depth when symphonia reports it (lossy codecs return None)
        bit_depth = params.bits_per_sample.map(|bd| bd as i32);
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

    let audio_format = detect_format(path, codec_type);

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
        composer: None,
        language: None,
        bpm: None,
        musical_key: None,
        mood: None,
        is_compilation: false,
        replay_gain_track_gain: None,
        replay_gain_track_peak: None,
        replay_gain_album_gain: None,
        replay_gain_album_peak: None,
        musicbrainz_recording_id: None,
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
            Some(StandardTagKey::Composer) => {
                meta.composer = Some(tag.value.to_string());
            }
            Some(StandardTagKey::Language) => {
                meta.language = Some(tag.value.to_string());
            }
            Some(StandardTagKey::Bpm) => {
                if let Ok(bpm) = tag.value.to_string().parse::<f64>() {
                    if (0.0..=500.0).contains(&bpm) {
                        meta.bpm = Some(bpm);
                    }
                }
            }
            Some(StandardTagKey::Mood) => {
                meta.mood = Some(tag.value.to_string());
            }
            Some(StandardTagKey::Compilation) => {
                let val = tag.value.to_string().to_lowercase();
                meta.is_compilation = val == "1" || val == "true";
            }
            Some(StandardTagKey::ReplayGainTrackGain) => {
                let s = tag.value.to_string().trim_end_matches(" dB").to_string();
                if let Ok(v) = s.parse::<f64>() {
                    meta.replay_gain_track_gain = Some(v);
                }
            }
            Some(StandardTagKey::ReplayGainTrackPeak) => {
                if let Ok(v) = tag.value.to_string().parse::<f64>() {
                    meta.replay_gain_track_peak = Some(v);
                }
            }
            Some(StandardTagKey::ReplayGainAlbumGain) => {
                let s = tag.value.to_string().trim_end_matches(" dB").to_string();
                if let Ok(v) = s.parse::<f64>() {
                    meta.replay_gain_album_gain = Some(v);
                }
            }
            Some(StandardTagKey::ReplayGainAlbumPeak) => {
                if let Ok(v) = tag.value.to_string().parse::<f64>() {
                    meta.replay_gain_album_peak = Some(v);
                }
            }
            Some(StandardTagKey::MusicBrainzRecordingId) => {
                meta.musicbrainz_recording_id = Some(tag.value.to_string());
            }
            _ => {
                // Handle tags without standard key mappings via raw key string
                let key_lower = tag.key.to_lowercase();
                if key_lower == "initialkey" || key_lower == "key" {
                    meta.musical_key = Some(tag.value.to_string());
                }
            }
        }
    }

    Ok(meta)
}

/// Fallback: parse metadata from directory structure
/// Supports:
///   /library/Artist - Album Name (Year)/01 - Track Name.flac
///   /library/Artist - Album Name (Year)/CD1/01 - Track Name.flac
///   /library/Artist Name/Album Name (Year)/01 - Track Name.flac
pub fn metadata_from_path(path: &Path, library_root: &Path) -> Option<TrackMetadata> {
    let parent = path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;

    // Handle disc subfolders (CD1, CD2, Disc 1, etc.) — go up one level
    let (album_dir, disc_number) = if let Some(disc_num) = parse_disc_folder(parent_name) {
        (parent.parent()?, disc_num)
    } else {
        (parent, 1)
    };

    let album_dir_name = album_dir.file_name()?.to_str()?;
    let is_direct_child = album_dir.parent().map_or(true, |gp| gp == library_root);

    let (artist_name, album_name, year) = if is_direct_child {
        // Flat structure: try "Artist - Album (Year)" pattern
        if let Some(parsed) = parse_artist_album_dir(album_dir_name) {
            parsed
        } else {
            let (album, year) = parse_album_dir(album_dir_name);
            ("Unknown Artist".to_string(), album, year)
        }
    } else {
        // Two-level: /Artist/Album/track.flac
        let grandparent = album_dir.parent()?;
        let artist = grandparent.file_name()?.to_str()?.to_string();
        let (album, year) = parse_album_dir(album_dir_name);
        (artist, album, year)
    };

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
        disc_number,
        duration_seconds: 0,
        format: detect_format(path, None),
        sample_rate: 44100,
        bit_depth: Some(16),
        file_size_bytes: file_size,
        year,
        genre: Vec::new(),
        style: Vec::new(),
        composer: None,
        language: None,
        bpm: None,
        musical_key: None,
        mood: None,
        is_compilation: false,
        replay_gain_track_gain: None,
        replay_gain_track_peak: None,
        replay_gain_album_gain: None,
        replay_gain_album_peak: None,
        musicbrainz_recording_id: None,
    })
}

/// Parse disc subfolder names like "CD1", "CD 1", "Disc 1", "Disc1"
fn parse_disc_folder(name: &str) -> Option<i32> {
    let lower = name.to_lowercase();
    if lower.starts_with("cd") {
        return lower[2..].trim().parse::<i32>().ok();
    }
    if lower.starts_with("disc") {
        return lower[4..].trim().parse::<i32>().ok();
    }
    None
}

fn detect_format(path: &Path, codec: Option<CodecType>) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    match ext.as_deref() {
        Some("flac") => "FLAC".to_string(),
        Some("m4a") => {
            // Use codec ID to distinguish ALAC from AAC in M4A containers
            match codec {
                Some(symphonia::core::codecs::CODEC_TYPE_ALAC) => "ALAC".to_string(),
                Some(symphonia::core::codecs::CODEC_TYPE_AAC) => "AAC".to_string(),
                _ => "ALAC".to_string(), // default assumption for m4a without codec info
            }
        }
        Some("wav") => "WAV".to_string(),
        Some("aiff") | Some("aif") => "AIFF".to_string(),
        Some("mp3") => "MP3".to_string(),
        Some("ogg") | Some("oga") => "OGG".to_string(),
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

/// Parse flat "Artist - Album (Year)" folder name
fn parse_artist_album_dir(dir_name: &str) -> Option<(String, String, Option<i32>)> {
    let sep_pos = dir_name.find(" - ")?;
    let artist = dir_name[..sep_pos].trim().to_string();
    let album_part = dir_name[sep_pos + 3..].trim();

    if artist.is_empty() || album_part.is_empty() {
        return None;
    }

    let (album, year) = parse_album_dir(album_part);
    Some((artist, album, year))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -- detect_format --

    #[test]
    fn test_detect_format_flac() {
        assert_eq!(detect_format(Path::new("song.flac"), None), "FLAC");
    }

    #[test]
    fn test_detect_format_m4a_default() {
        assert_eq!(detect_format(Path::new("song.m4a"), None), "ALAC");
    }

    #[test]
    fn test_detect_format_m4a_alac_codec() {
        assert_eq!(
            detect_format(Path::new("song.m4a"), Some(symphonia::core::codecs::CODEC_TYPE_ALAC)),
            "ALAC"
        );
    }

    #[test]
    fn test_detect_format_m4a_aac_codec() {
        assert_eq!(
            detect_format(Path::new("song.m4a"), Some(symphonia::core::codecs::CODEC_TYPE_AAC)),
            "AAC"
        );
    }

    #[test]
    fn test_detect_format_wav() {
        assert_eq!(detect_format(Path::new("song.wav"), None), "WAV");
    }

    #[test]
    fn test_detect_format_aiff() {
        assert_eq!(detect_format(Path::new("song.aiff"), None), "AIFF");
        assert_eq!(detect_format(Path::new("song.aif"), None), "AIFF");
    }

    #[test]
    fn test_detect_format_mp3() {
        assert_eq!(detect_format(Path::new("song.mp3"), None), "MP3");
    }

    #[test]
    fn test_detect_format_ogg() {
        assert_eq!(detect_format(Path::new("song.ogg"), None), "OGG");
        assert_eq!(detect_format(Path::new("song.oga"), None), "OGG");
    }

    #[test]
    fn test_detect_format_case_insensitive() {
        assert_eq!(detect_format(Path::new("song.FLAC"), None), "FLAC");
        assert_eq!(detect_format(Path::new("song.Wav"), None), "WAV");
    }

    #[test]
    fn test_detect_format_no_extension() {
        assert_eq!(detect_format(Path::new("song"), None), "UNKNOWN");
    }

    // -- filename_without_ext --

    #[test]
    fn test_filename_without_ext() {
        assert_eq!(filename_without_ext(Path::new("/music/01 - Song.flac")), "01 - Song");
        assert_eq!(filename_without_ext(Path::new("track.wav")), "track");
    }

    #[test]
    fn test_filename_without_ext_no_ext() {
        assert_eq!(filename_without_ext(Path::new("noext")), "noext");
    }

    // -- parse_track_filename --

    #[test]
    fn test_parse_track_filename_dash_pattern() {
        assert_eq!(parse_track_filename("01 - Paranoid Android"), "Paranoid Android");
        assert_eq!(parse_track_filename("2 - Lucky"), "Lucky");
        assert_eq!(parse_track_filename("12 - The Tourist"), "The Tourist");
    }

    #[test]
    fn test_parse_track_filename_dot_pattern() {
        assert_eq!(parse_track_filename("01. Paranoid Android"), "Paranoid Android");
        assert_eq!(parse_track_filename("3. Subterranean Homesick Alien"), "Subterranean Homesick Alien");
    }

    #[test]
    fn test_parse_track_filename_no_number_prefix() {
        assert_eq!(parse_track_filename("Paranoid Android"), "Paranoid Android");
        assert_eq!(parse_track_filename("Song Title"), "Song Title");
    }

    #[test]
    fn test_parse_track_filename_artist_dash_title() {
        // "Artist - Title" should NOT strip because "Artist" is not all digits
        assert_eq!(parse_track_filename("Radiohead - Creep"), "Radiohead - Creep");
    }

    // -- parse_disc_folder --

    #[test]
    fn test_parse_disc_folder_cd() {
        assert_eq!(parse_disc_folder("CD1"), Some(1));
        assert_eq!(parse_disc_folder("CD 2"), Some(2));
        assert_eq!(parse_disc_folder("cd3"), Some(3));
    }

    #[test]
    fn test_parse_disc_folder_disc() {
        assert_eq!(parse_disc_folder("Disc 1"), Some(1));
        assert_eq!(parse_disc_folder("Disc1"), Some(1));
        assert_eq!(parse_disc_folder("disc 2"), Some(2));
    }

    #[test]
    fn test_parse_disc_folder_non_disc() {
        assert_eq!(parse_disc_folder("Extras"), None);
        assert_eq!(parse_disc_folder("Bonus"), None);
        assert_eq!(parse_disc_folder(""), None);
    }

    // -- parse_album_dir --

    #[test]
    fn test_parse_album_dir_with_year() {
        let (album, year) = parse_album_dir("OK Computer (1997)");
        assert_eq!(album, "OK Computer");
        assert_eq!(year, Some(1997));
    }

    #[test]
    fn test_parse_album_dir_without_year() {
        let (album, year) = parse_album_dir("OK Computer");
        assert_eq!(album, "OK Computer");
        assert_eq!(year, None);
    }

    #[test]
    fn test_parse_album_dir_invalid_year() {
        let (album, year) = parse_album_dir("Album (abc)");
        assert_eq!(album, "Album (abc)");
        assert_eq!(year, None);
    }

    #[test]
    fn test_parse_album_dir_year_out_of_range() {
        let (album, year) = parse_album_dir("Album (1800)");
        assert_eq!(album, "Album (1800)");
        assert_eq!(year, None);
    }

    // -- parse_artist_album_dir --

    #[test]
    fn test_parse_artist_album_dir() {
        let result = parse_artist_album_dir("Radiohead - OK Computer (1997)");
        assert!(result.is_some());
        let (artist, album, year) = result.unwrap();
        assert_eq!(artist, "Radiohead");
        assert_eq!(album, "OK Computer");
        assert_eq!(year, Some(1997));
    }

    #[test]
    fn test_parse_artist_album_dir_no_year() {
        let result = parse_artist_album_dir("Radiohead - OK Computer");
        assert!(result.is_some());
        let (artist, album, year) = result.unwrap();
        assert_eq!(artist, "Radiohead");
        assert_eq!(album, "OK Computer");
        assert_eq!(year, None);
    }

    #[test]
    fn test_parse_artist_album_dir_no_separator() {
        assert!(parse_artist_album_dir("Just An Album Name").is_none());
    }

    #[test]
    fn test_parse_artist_album_dir_empty_parts() {
        assert!(parse_artist_album_dir(" - Album").is_none());
        assert!(parse_artist_album_dir("Artist - ").is_none());
    }

    // -- metadata_from_path --

    #[test]
    fn test_metadata_from_path_flat_structure() {
        let library = PathBuf::from("/music");
        let path = PathBuf::from("/music/Radiohead - OK Computer (1997)/01 - Airbag.flac");

        let meta = metadata_from_path(&path, &library).unwrap();
        assert_eq!(meta.artist, "Radiohead");
        assert_eq!(meta.album, "OK Computer");
        assert_eq!(meta.year, Some(1997));
        assert_eq!(meta.title, "Airbag");
        assert_eq!(meta.track_number, 1);
        assert_eq!(meta.format, "FLAC");
    }

    #[test]
    fn test_metadata_from_path_two_level_structure() {
        let library = PathBuf::from("/music");
        let path = PathBuf::from("/music/Radiohead/OK Computer (1997)/02 - Paranoid Android.flac");

        let meta = metadata_from_path(&path, &library).unwrap();
        assert_eq!(meta.artist, "Radiohead");
        assert_eq!(meta.album, "OK Computer");
        assert_eq!(meta.year, Some(1997));
        assert_eq!(meta.title, "Paranoid Android");
        assert_eq!(meta.track_number, 2);
    }

    #[test]
    fn test_metadata_from_path_with_disc_subfolder() {
        let library = PathBuf::from("/music");
        let path = PathBuf::from("/music/Radiohead/OK Computer (1997)/CD1/01 - Airbag.flac");

        let meta = metadata_from_path(&path, &library).unwrap();
        assert_eq!(meta.artist, "Radiohead");
        assert_eq!(meta.album, "OK Computer");
        assert_eq!(meta.disc_number, 1);
    }

    #[test]
    fn test_metadata_from_path_disc2() {
        let library = PathBuf::from("/music");
        let path = PathBuf::from("/music/Artist/Album (2020)/Disc 2/05 - Track.flac");

        let meta = metadata_from_path(&path, &library).unwrap();
        assert_eq!(meta.disc_number, 2);
        assert_eq!(meta.track_number, 5);
    }
}
