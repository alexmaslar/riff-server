use std::fs;
use std::path::{Path, PathBuf};
use symphonia::core::codecs::CodecType;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Tag};
use symphonia::core::probe::Hint;

/// How albums are organized on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryConvention {
    /// `Artist - Album/Artist - Album - NN Title.ext`
    Flat,
    /// `Artist/Album/NN - Title.ext`
    Nested,
}

/// Sample a library directory to determine its naming convention.
///
/// Looks at direct children of the library root. If the majority of
/// directories use `Artist - Album` style names (containing ` - `),
/// returns `Flat`. Otherwise returns `Nested`. Defaults to `Flat`
/// when the library is empty.
pub fn detect_library_convention(library_path: &Path) -> LibraryConvention {
    let entries = match fs::read_dir(library_path) {
        Ok(entries) => entries,
        Err(_) => return LibraryConvention::Flat,
    };

    let mut flat_count = 0u32;
    let mut nested_count = 0u32;

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.contains(" - ") {
            flat_count += 1;
        } else {
            nested_count += 1;
        }
    }

    if flat_count >= nested_count {
        LibraryConvention::Flat
    } else {
        LibraryConvention::Nested
    }
}

/// Build the album directory path for a given convention.
pub fn build_album_dir(
    library_path: &Path,
    artist: &str,
    album: &str,
    convention: LibraryConvention,
) -> PathBuf {
    let artist = sanitize_filename(artist);
    let album = sanitize_filename(album);
    match convention {
        LibraryConvention::Flat => library_path.join(format!("{artist} - {album}")),
        LibraryConvention::Nested => library_path.join(&artist).join(&album),
    }
}

/// Build a track filename for a given convention.
pub fn build_track_filename(
    artist: &str,
    album: &str,
    track_number: i32,
    title: &str,
    ext: &str,
    convention: LibraryConvention,
) -> String {
    let artist = sanitize_filename(artist);
    let album = sanitize_filename(album);
    let title = sanitize_filename(title);
    match convention {
        LibraryConvention::Flat => {
            format!("{artist} - {album} - {track_number:02} {title}.{ext}")
        }
        LibraryConvention::Nested => {
            format!("{track_number:02} - {title}.{ext}")
        }
    }
}

/// Replace filesystem-unsafe characters with underscores.
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// Write standard Vorbis Comment tags into a FLAC file.
///
/// Called after downloading a track so the scanner picks up proper metadata
/// instead of falling back to filename parsing.
///
/// Parses the FLAC metadata block chain directly, ignoring premature
/// `last-metadata-block` flags to handle files corrupted by lofty 0.22.
/// Replaces the VORBIS_COMMENT block and rewrites the file with a correct
/// block chain.
pub fn write_flac_tags(
    path: &Path,
    artist: &str,
    album: &str,
    title: &str,
    track_number: u32,
    disc_number: u32,
    year: Option<i32>,
) -> anyhow::Result<()> {
    let data = fs::read(path)?;
    anyhow::ensure!(data.len() >= 8 && &data[..4] == b"fLaC", "not a FLAC file");

    let (mut blocks, audio_offset) = parse_flac_metadata_blocks(&data)?;

    // Remove existing VORBIS_COMMENT (type 4) and PADDING (type 1)
    blocks.retain(|(btype, _)| *btype != 4 && *btype != 1);

    // Build and append new VORBIS_COMMENT
    let mut comments = vec![
        format!("TITLE={title}"),
        format!("ARTIST={artist}"),
        format!("ALBUM={album}"),
        format!("TRACKNUMBER={track_number}"),
        format!("DISCNUMBER={disc_number}"),
    ];
    if let Some(y) = year {
        comments.push(format!("YEAR={y}"));
    }
    blocks.push((4, encode_vorbis_comment(&comments)));

    let out = rebuild_flac(&blocks, &data[audio_offset..]);
    fs::write(path, &out)?;
    Ok(())
}

/// Repair a corrupted FLAC file in-place.
///
/// Rebuilds the metadata block chain with correct `last-metadata-block` flags,
/// preserving all existing tags and metadata blocks (except PADDING).
pub fn repair_flac(path: &Path) -> anyhow::Result<()> {
    let data = fs::read(path)?;
    anyhow::ensure!(data.len() >= 8 && &data[..4] == b"fLaC", "not a FLAC file");

    let (mut blocks, audio_offset) = parse_flac_metadata_blocks(&data)?;
    blocks.retain(|(btype, _)| *btype != 1); // drop PADDING

    let out = rebuild_flac(&blocks, &data[audio_offset..]);
    fs::write(path, &out)?;
    Ok(())
}

/// Parse FLAC metadata blocks and locate the audio frame boundary.
///
/// Handles files corrupted by lofty 0.22 where the `last-metadata-block` flag
/// is set prematurely, orphaning subsequent blocks. Parses blocks up to the
/// first `last=1` flag, then scans for the FLAC frame sync (`0xFFF8`/`0xFFF9`)
/// to find where audio actually begins. Any valid metadata blocks in the
/// orphaned region between the two are also recovered.
///
/// Returns `(blocks, audio_offset)` where each block is `(type, data)` and
/// `audio_offset` is the byte position where audio frames begin.
fn parse_flac_metadata_blocks(data: &[u8]) -> anyhow::Result<(Vec<(u8, Vec<u8>)>, usize)> {
    let mut pos = 4; // skip fLaC magic
    let mut blocks = Vec::new();

    // Phase 1: parse blocks until we see last=1 or hit an invalid header
    loop {
        if pos + 4 > data.len() {
            anyhow::bail!("unexpected EOF in metadata block chain");
        }

        let is_last = (data[pos] >> 7) & 1 == 1;
        let block_type = data[pos] & 0x7F;

        if block_type > 6 {
            break;
        }

        let length =
            u32::from_be_bytes([0, data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;

        if pos + 4 + length > data.len() {
            break;
        }

        blocks.push((block_type, data[pos + 4..pos + 4 + length].to_vec()));
        pos += 4 + length;

        if is_last {
            break;
        }
    }

    // Phase 2: find the actual audio frame boundary by scanning for the
    // FLAC frame sync code (0xFFF8 or 0xFFF9). In a well-formed file this
    // is exactly at `pos`; in a corrupted file there may be orphaned blocks
    // or garbage between `pos` and the first audio frame.
    let audio_offset = find_frame_sync(data, pos)
        .ok_or_else(|| anyhow::anyhow!("no FLAC frame sync found after metadata"))?;

    // Phase 3: try to recover valid metadata blocks in the orphaned region
    let mut orphan_pos = pos;
    while orphan_pos + 4 <= audio_offset {
        let block_type = data[orphan_pos] & 0x7F;
        if block_type > 6 {
            break;
        }

        let length =
            u32::from_be_bytes([0, data[orphan_pos + 1], data[orphan_pos + 2], data[orphan_pos + 3]])
                as usize;

        // Block must fit within the orphaned region (before audio)
        if orphan_pos + 4 + length > audio_offset {
            break;
        }

        blocks.push((block_type, data[orphan_pos + 4..orphan_pos + 4 + length].to_vec()));
        orphan_pos += 4 + length;
    }

    Ok((blocks, audio_offset))
}

/// Scan for the first FLAC frame sync code (`0xFFF8` or `0xFFF9`) starting
/// at `from`. Returns the byte offset if found.
fn find_frame_sync(data: &[u8], from: usize) -> Option<usize> {
    data[from..]
        .windows(2)
        .position(|w| w[0] == 0xFF && (w[1] == 0xF8 || w[1] == 0xF9))
        .map(|pos| from + pos)
}

/// Encode a Vorbis Comment block body from a list of `KEY=value` strings.
fn encode_vorbis_comment(comments: &[String]) -> Vec<u8> {
    let vendor = b"riff";
    let mut buf = Vec::new();
    buf.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    buf.extend_from_slice(vendor);
    buf.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        let bytes = c.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    buf
}

/// Reassemble a FLAC file from metadata blocks and audio frame data.
fn rebuild_flac(blocks: &[(u8, Vec<u8>)], audio_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + blocks.iter().map(|(_, d)| 4 + d.len()).sum::<usize>() + audio_data.len());
    out.extend_from_slice(b"fLaC");
    for (i, (btype, bdata)) in blocks.iter().enumerate() {
        let is_last = i == blocks.len() - 1;
        out.push(if is_last { 0x80 | btype } else { *btype });
        let len = bdata.len() as u32;
        out.extend_from_slice(&len.to_be_bytes()[1..4]);
        out.extend_from_slice(bdata);
    }
    out.extend_from_slice(audio_data);
    out
}

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
    pub isrc: Option<String>,
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
        isrc: None,
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
                for g in genre_str.split(';').map(|s| crate::db::title_case_genre(s.trim())) {
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
            Some(StandardTagKey::IdentIsrc) => {
                meta.isrc = Some(tag.value.to_string());
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
        isrc: None,
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

    // -- sanitize_filename --

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("Hello/World"), "Hello_World");
        assert_eq!(sanitize_filename("a:b*c?d"), "a_b_c_d");
        assert_eq!(sanitize_filename("Normal Name"), "Normal Name");
    }

    // -- detect_library_convention --

    #[test]
    fn test_detect_convention_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_library_convention(dir.path()), LibraryConvention::Flat);
    }

    #[test]
    fn test_detect_convention_flat() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("Artist - Album One")).unwrap();
        fs::create_dir(dir.path().join("Artist - Album Two")).unwrap();
        fs::create_dir(dir.path().join("Radiohead")).unwrap(); // one nested outlier
        assert_eq!(detect_library_convention(dir.path()), LibraryConvention::Flat);
    }

    #[test]
    fn test_detect_convention_nested() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("Radiohead")).unwrap();
        fs::create_dir(dir.path().join("Pink Floyd")).unwrap();
        fs::create_dir(dir.path().join("Bad Bunny")).unwrap();
        assert_eq!(detect_library_convention(dir.path()), LibraryConvention::Nested);
    }

    #[test]
    fn test_detect_convention_nonexistent_path() {
        assert_eq!(
            detect_library_convention(Path::new("/nonexistent/path")),
            LibraryConvention::Flat
        );
    }

    // -- build_album_dir --

    #[test]
    fn test_build_album_dir_flat() {
        let dir = build_album_dir(
            Path::new("/music"),
            "Radiohead",
            "OK Computer",
            LibraryConvention::Flat,
        );
        assert_eq!(dir, PathBuf::from("/music/Radiohead - OK Computer"));
    }

    #[test]
    fn test_build_album_dir_nested() {
        let dir = build_album_dir(
            Path::new("/music"),
            "Radiohead",
            "OK Computer",
            LibraryConvention::Nested,
        );
        assert_eq!(dir, PathBuf::from("/music/Radiohead/OK Computer"));
    }

    #[test]
    fn test_build_album_dir_sanitizes() {
        let dir = build_album_dir(
            Path::new("/music"),
            "AC/DC",
            "Back In Black",
            LibraryConvention::Flat,
        );
        assert_eq!(dir, PathBuf::from("/music/AC_DC - Back In Black"));
    }

    // -- build_track_filename --

    #[test]
    fn test_build_track_filename_flat() {
        let name = build_track_filename("Radiohead", "OK Computer", 1, "Airbag", "flac", LibraryConvention::Flat);
        assert_eq!(name, "Radiohead - OK Computer - 01 Airbag.flac");
    }

    #[test]
    fn test_build_track_filename_nested() {
        let name = build_track_filename("Radiohead", "OK Computer", 3, "Subterranean Homesick Alien", "flac", LibraryConvention::Nested);
        assert_eq!(name, "03 - Subterranean Homesick Alien.flac");
    }

    #[test]
    fn test_build_track_filename_sanitizes() {
        let name = build_track_filename("Test", "Album", 1, "What?", "flac", LibraryConvention::Flat);
        assert_eq!(name, "Test - Album - 01 What_.flac");
    }

    // -- FLAC tag writing --

    /// Build a minimal valid FLAC file: fLaC + STREAMINFO + one silent audio frame.
    fn make_minimal_flac() -> Vec<u8> {
        // STREAMINFO: 34 bytes (min/max block=4096, min/max frame=0,
        // sample_rate=44100, channels=1(mono), bps=16, total_samples=4096, md5=0)
        let streaminfo: [u8; 34] = [
            0x10, 0x00, 0x10, 0x00, // min/max block size = 4096
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // min/max frame size = 0
            0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0x10, 0x00,
            // ^^^^^ sample_rate=44100, channels=1(0), bps=16(15)
            // total_samples = 4096
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // MD5 = all zeros
        ];

        // Minimal FLAC frame: sync + header + subframe + footer
        // Use a constant silent frame: fff8 6918 00 00(silence) + CRC
        let audio_frame: [u8; 11] = [
            0xFF, 0xF8, 0x69, 0x18, // sync(14b) + blocking(1b=fixed) + blocksize(4b) + samplerate(4b) + channel(4b) + bps(3b) + reserved(1b)
            0x00, // frame number = 0 (UTF-8 coded)
            0xA8, // blocksize-1 uncommon = read 8-bit -> 0xA8 ... simplified
            0x00, // constant subframe (SUBFRAME_CONSTANT, all zeros)
            0x00, 0x00, // padding to byte align
            0xDB, 0xFC, // CRC-16 (placeholder)
        ];

        let mut data = Vec::new();
        data.extend_from_slice(b"fLaC");
        // STREAMINFO block header: last=1, type=0, length=34
        data.push(0x80);
        data.extend_from_slice(&34u32.to_be_bytes()[1..4]);
        data.extend_from_slice(&streaminfo);
        // Audio frame
        data.extend_from_slice(&audio_frame);
        data
    }

    #[test]
    fn test_write_flac_tags_creates_vorbis_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.flac");
        fs::write(&path, make_minimal_flac()).unwrap();

        write_flac_tags(&path, "Artist", "Album", "Title", 3, 1, Some(2025)).unwrap();

        let data = fs::read(&path).unwrap();
        assert_eq!(&data[..4], b"fLaC");

        // Parse blocks
        let (blocks, _) = parse_flac_metadata_blocks(&data).unwrap();
        assert!(blocks.iter().any(|(t, _)| *t == 0), "STREAMINFO present");
        assert!(blocks.iter().any(|(t, _)| *t == 4), "VORBIS_COMMENT present");

        // Verify tag content
        let vc_data = &blocks.iter().find(|(t, _)| *t == 4).unwrap().1;
        let vc_str = String::from_utf8_lossy(vc_data);
        assert!(vc_str.contains("TITLE=Title"));
        assert!(vc_str.contains("ARTIST=Artist"));
        assert!(vc_str.contains("ALBUM=Album"));
        assert!(vc_str.contains("TRACKNUMBER=3"));
        assert!(vc_str.contains("YEAR=2025"));
    }

    #[test]
    fn test_write_flac_tags_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.flac");
        fs::write(&path, make_minimal_flac()).unwrap();

        // Write once
        write_flac_tags(&path, "A1", "B1", "T1", 1, 1, None).unwrap();
        // Write again with different values
        write_flac_tags(&path, "A2", "B2", "T2", 5, 2, Some(2020)).unwrap();

        let data = fs::read(&path).unwrap();
        let (blocks, _) = parse_flac_metadata_blocks(&data).unwrap();

        // Only one VORBIS_COMMENT block
        let vc_count = blocks.iter().filter(|(t, _)| *t == 4).count();
        assert_eq!(vc_count, 1);

        let vc_data = &blocks.iter().find(|(t, _)| *t == 4).unwrap().1;
        let vc_str = String::from_utf8_lossy(vc_data);
        assert!(vc_str.contains("ARTIST=A2"));
        assert!(vc_str.contains("ALBUM=B2"));
        assert!(!vc_str.contains("ARTIST=A1"));
    }

    #[test]
    fn test_write_flac_tags_correct_last_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.flac");
        fs::write(&path, make_minimal_flac()).unwrap();

        write_flac_tags(&path, "A", "B", "T", 1, 1, None).unwrap();

        let data = fs::read(&path).unwrap();
        // Walk block headers and verify only the last has the last flag
        let mut pos = 4usize;
        let mut last_flags = Vec::new();
        while pos + 4 <= data.len() {
            let is_last = (data[pos] >> 7) & 1 == 1;
            let block_type = data[pos] & 0x7F;
            if block_type > 6 {
                break;
            }
            let length = u32::from_be_bytes([0, data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
            last_flags.push(is_last);
            pos += 4 + length;
            if is_last {
                break;
            }
        }
        // All blocks except the last should have last=false
        assert!(last_flags.len() >= 2);
        for flag in &last_flags[..last_flags.len() - 1] {
            assert!(!flag, "non-final block should not have last flag");
        }
        assert!(last_flags.last().unwrap(), "final block should have last flag");
    }

    #[test]
    fn test_repair_flac_fixes_premature_last_flag() {
        // Build a FLAC with premature last=1 on STREAMINFO, followed by PADDING then audio
        let streaminfo = [0u8; 34];
        let padding = [0u8; 64];

        let mut corrupted = Vec::new();
        corrupted.extend_from_slice(b"fLaC");
        // STREAMINFO: last=1 (premature!), type=0, length=34
        corrupted.push(0x80);
        corrupted.extend_from_slice(&34u32.to_be_bytes()[1..4]);
        corrupted.extend_from_slice(&streaminfo);
        // PADDING: last=1, type=1, length=64 (orphaned)
        corrupted.push(0x81);
        corrupted.extend_from_slice(&64u32.to_be_bytes()[1..4]);
        corrupted.extend_from_slice(&padding);
        // Audio frame sync
        corrupted.extend_from_slice(&[0xFF, 0xF8, 0x00, 0x00]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.flac");
        fs::write(&path, &corrupted).unwrap();

        repair_flac(&path).unwrap();

        let data = fs::read(&path).unwrap();
        assert_eq!(&data[..4], b"fLaC");

        let (blocks, audio_offset) = parse_flac_metadata_blocks(&data).unwrap();
        // PADDING should be stripped
        assert!(blocks.iter().all(|(t, _)| *t != 1));
        // STREAMINFO should be present
        assert_eq!(blocks[0].0, 0);
        // Audio should follow immediately
        assert_eq!(data[audio_offset], 0xFF);
        assert_eq!(data[audio_offset + 1], 0xF8);
    }

    #[test]
    fn test_encode_vorbis_comment() {
        let comments = vec!["TITLE=Test".to_string(), "ARTIST=Me".to_string()];
        let data = encode_vorbis_comment(&comments);

        // Vendor: "riff" (4 bytes LE length + 4 bytes string)
        assert_eq!(&data[0..4], &4u32.to_le_bytes());
        assert_eq!(&data[4..8], b"riff");
        // 2 comments
        assert_eq!(&data[8..12], &2u32.to_le_bytes());
        // First comment: "TITLE=Test" (10 bytes)
        assert_eq!(&data[12..16], &10u32.to_le_bytes());
        assert_eq!(&data[16..26], b"TITLE=Test");
    }
}
