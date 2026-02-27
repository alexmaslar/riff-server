use bliss_audio::decoder::symphonia::SymphoniaDecoder;
use bliss_audio::decoder::Decoder;
use bliss_audio::AnalysisIndex;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

pub struct AnalysisResult {
    pub tracks_analyzed: u32,
    pub tracks_failed: u32,
    pub tracks_skipped: u32,
    pub errors: Vec<String>,
    pub enriched_album_ids: Vec<String>,
}

/// Analyze all pending tracks in the library.
/// Runs bliss-audio for tempo/chroma features and ebur128 for loudness.
pub async fn analyze_library(pool: &SqlitePool) -> anyhow::Result<AnalysisResult> {
    let mut result = AnalysisResult {
        tracks_analyzed: 0,
        tracks_failed: 0,
        tracks_skipped: 0,
        errors: Vec::new(),
        enriched_album_ids: Vec::new(),
    };
    let mut analyzed_track_ids: Vec<String> = Vec::new();

    // Reset any tracks stuck in 'analyzing' from a previous interrupted run
    let reset = sqlx::query(
        "UPDATE tracks SET analysis_status = 'pending' WHERE analysis_status = 'analyzing'",
    )
    .execute(pool)
    .await?;
    if reset.rows_affected() > 0 {
        info!("reset {} tracks from 'analyzing' back to 'pending'", reset.rows_affected());
    }

    let rows: Vec<(String, String, i32, i64)> = sqlx::query_as(
        "SELECT id, file_path, duration_seconds, file_size_bytes FROM tracks WHERE analysis_status = 'pending'",
    )
    .fetch_all(pool)
    .await?;

    let total = rows.len();
    info!("analyzing {} tracks", total);

    // Phase 1: Filter skips and mark analyzable tracks
    let mut tracks_to_analyze = Vec::new();
    for (track_id, file_path, duration, file_size) in rows {
        if duration < 5 || file_size > 500 * 1024 * 1024 {
            sqlx::query("UPDATE tracks SET analysis_status = 'skipped' WHERE id = ?")
                .bind(&track_id)
                .execute(pool)
                .await?;
            result.tracks_skipped += 1;
            continue;
        }

        sqlx::query("UPDATE tracks SET analysis_status = 'analyzing' WHERE id = ?")
            .bind(&track_id)
            .execute(pool)
            .await?;
        tracks_to_analyze.push((track_id, file_path));
    }

    // Phase 2: Analyze in parallel across available CPU cores
    let concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    info!("using {} parallel workers", concurrency);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set = JoinSet::new();

    for (track_id, file_path) in tracks_to_analyze {
        let permit = semaphore.clone().acquire_owned().await
            .expect("semaphore closed unexpectedly");
        let path = file_path.clone();
        join_set.spawn(async move {
            let analysis = tokio::task::spawn_blocking(move || analyze_track(&path)).await;
            drop(permit);
            (track_id, file_path, analysis)
        });
    }

    // Phase 3: Collect results and update DB
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((track_id, file_path, analysis)) => {
                match analysis {
                    Ok(Ok(data)) => {
                        let features_json = serde_json::to_string(&data.bliss_features)?;
                        sqlx::query(
                            "UPDATE tracks SET bpm_analyzed = ?, key_analyzed = ?, loudness_lufs = ?, bliss_features = ?, analysis_status = 'complete', analyzed_at = datetime('now') WHERE id = ?",
                        )
                        .bind(data.bpm)
                        .bind(&data.key)
                        .bind(data.loudness_lufs)
                        .bind(&features_json)
                        .bind(&track_id)
                        .execute(pool)
                        .await?;
                        analyzed_track_ids.push(track_id.clone());
                        result.tracks_analyzed += 1;
                    }
                    Ok(Err(e)) => {
                        warn!("analysis failed for {}: {}", file_path, e);
                        sqlx::query("UPDATE tracks SET analysis_status = 'failed' WHERE id = ?")
                            .bind(&track_id)
                            .execute(pool)
                            .await?;
                        result.errors.push(format!("{}: {}", file_path, e));
                        result.tracks_failed += 1;
                    }
                    Err(e) => {
                        warn!("analysis task panicked for {}: {}", file_path, e);
                        sqlx::query("UPDATE tracks SET analysis_status = 'failed' WHERE id = ?")
                            .bind(&track_id)
                            .execute(pool)
                            .await?;
                        result.errors.push(format!("{}: task panicked", file_path));
                        result.tracks_failed += 1;
                    }
                }
            }
            Err(e) => {
                warn!("analysis join error: {}", e);
            }
        }

        let done = result.tracks_analyzed + result.tracks_failed + result.tracks_skipped;
        if done % 50 == 0 || done as usize == total {
            info!("analysis progress: {}/{} tracks", done, total);
        }
    }

    info!(
        "analysis complete: {} analyzed, {} failed, {} skipped",
        result.tracks_analyzed, result.tracks_failed, result.tracks_skipped
    );

    // Collect unique album IDs from analyzed tracks
    if !analyzed_track_ids.is_empty() {
        let placeholders: String = analyzed_track_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!("SELECT DISTINCT album_id FROM tracks WHERE id IN ({})", placeholders);
        let mut q = sqlx::query_scalar::<_, String>(&query);
        for id in &analyzed_track_ids {
            q = q.bind(id);
        }
        if let Ok(album_ids) = q.fetch_all(pool).await {
            result.enriched_album_ids = album_ids;
        }
    }

    Ok(result)
}

struct TrackAnalysis {
    bpm: Option<f64>,
    key: Option<String>,
    loudness_lufs: Option<f64>,
    bliss_features: Vec<f64>,
}

/// Run bliss-audio and ebur128 analysis on a single track (blocking).
fn analyze_track(file_path: &str) -> anyhow::Result<TrackAnalysis> {
    // Run bliss-audio analysis using Symphonia decoder
    let song = SymphoniaDecoder::song_from_path(file_path)
        .map_err(|e| anyhow::anyhow!("bliss analysis failed: {}", e))?;

    let features: Vec<f64> = song.analysis.as_vec().iter().map(|&v| v as f64).collect();

    // Extract BPM from bliss tempo feature
    let bpm = features
        .get(AnalysisIndex::Tempo as usize)
        .copied()
        .filter(|&b| b > 0.0 && b <= 500.0);

    // Estimate key from chroma features using Krumhansl-Schmuckler
    let key = estimate_key_from_chroma(&features);

    // Measure loudness via ebur128
    let loudness_lufs = measure_lufs(file_path).ok();

    Ok(TrackAnalysis {
        bpm,
        key,
        loudness_lufs,
        bliss_features: features,
    })
}

/// Estimate musical key from bliss chroma features using the Krumhansl-Schmuckler algorithm.
fn estimate_key_from_chroma(features: &[f64]) -> Option<String> {
    let chroma_start = AnalysisIndex::Chroma1 as usize;
    if features.len() < chroma_start + 12 {
        return None;
    }
    let chroma: Vec<f64> = features[chroma_start..chroma_start + 12].to_vec();

    // Krumhansl-Schmuckler key profiles
    let major_profile: [f64; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];
    let minor_profile: [f64; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];

    let key_names = [
        "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
    ];

    let mut best_key = String::new();
    let mut best_corr = f64::NEG_INFINITY;

    for rotation in 0..12 {
        let rotated: Vec<f64> = (0..12).map(|i| chroma[(i + rotation) % 12]).collect();

        let major_corr = pearson_correlation(&rotated, &major_profile);
        if major_corr > best_corr {
            best_corr = major_corr;
            best_key = key_names[rotation].to_string();
        }

        let minor_corr = pearson_correlation(&rotated, &minor_profile);
        if minor_corr > best_corr {
            best_corr = minor_corr;
            best_key = format!("{}m", key_names[rotation]);
        }
    }

    if best_corr < 0.5 {
        return None;
    }

    Some(best_key)
}

fn pearson_correlation(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean_x: f64 = x.iter().sum::<f64>() / n;
    let mean_y: f64 = y.iter().sum::<f64>() / n;

    let mut num = 0.0;
    let mut den_x = 0.0;
    let mut den_y = 0.0;

    for i in 0..x.len() {
        let dx = x[i] - mean_x;
        let dy = y[i] - mean_y;
        num += dx * dy;
        den_x += dx * dx;
        den_y += dy * dy;
    }

    let denom = (den_x * den_y).sqrt();
    if denom < f64::EPSILON {
        return 0.0;
    }
    num / denom
}

/// Measure integrated loudness in LUFS using ebur128.
/// Decodes the audio file with Symphonia and feeds samples to the ebur128 crate.
fn measure_lufs(file_path: &str) -> anyhow::Result<f64> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(file_path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow::anyhow!("no default track"))?;

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow::anyhow!("no sample rate"))?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(2);

    let track_id = track.id;

    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut ebu = ebur128::EbuR128::new(channels as u32, sample_rate, ebur128::Mode::I)?;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let spec = *decoded.spec();
        let duration = decoded.capacity();

        let mut sample_buf = SampleBuffer::<f32>::new(duration as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);

        let samples = sample_buf.samples();
        if !samples.is_empty() {
            ebu.add_frames_f32(samples)?;
        }
    }

    let loudness = ebu.loudness_global()?;
    Ok(loudness)
}
