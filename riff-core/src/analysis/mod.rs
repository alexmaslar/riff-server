pub mod dclap;

use bliss_audio::decoder::symphonia::SymphoniaDecoder;
use bliss_audio::decoder::Decoder;
use bliss_audio::AnalysisIndex;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

use self::dclap::DclapModel;

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
        info!(count = reset.rows_affected(), "reset tracks from 'analyzing' back to 'pending'");
    }

    let rows: Vec<(String, String, i32, i64)> = sqlx::query_as(
        "SELECT id, file_path, duration_seconds, file_size_bytes FROM tracks WHERE analysis_status = 'pending'",
    )
    .fetch_all(pool)
    .await?;

    let total = rows.len();
    info!(count = total, "analyzing tracks");

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
    info!(workers = concurrency, "parallel analysis workers");
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
                        warn!(path = %file_path, error = %e, "analysis failed");
                        sqlx::query("UPDATE tracks SET analysis_status = 'failed' WHERE id = ?")
                            .bind(&track_id)
                            .execute(pool)
                            .await?;
                        result.errors.push(format!("{}: {}", file_path, e));
                        result.tracks_failed += 1;
                    }
                    Err(e) => {
                        warn!(path = %file_path, error = %e, "analysis task panicked");
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
                warn!(error = %e, "analysis join error");
            }
        }

        let done = result.tracks_analyzed + result.tracks_failed + result.tracks_skipped;
        if done % 50 == 0 || done as usize == total {
            info!(done, total, "analysis progress");
        }
    }

    info!(
        analyzed = result.tracks_analyzed,
        failed = result.tracks_failed,
        skipped = result.tracks_skipped,
        "bliss analysis complete",
    );

    // Phase 4: DCLAP embedding pass — sequential through model for newly analyzed tracks
    // plus backfill for existing tracks missing embeddings
    let dclap_model = match DclapModel::load() {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            warn!(error = %e, "DCLAP model not available, skipping neural embeddings");
            None
        }
    };

    if let Some(model) = &dclap_model {
        // Newly analyzed tracks
        let tracks_for_dclap: Vec<(String, String)> = if analyzed_track_ids.is_empty() {
            Vec::new()
        } else {
            let placeholders: String = analyzed_track_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                "SELECT id, file_path FROM tracks WHERE id IN ({}) AND dclap_embedding IS NULL",
                placeholders
            );
            let mut q = sqlx::query_as::<_, (String, String)>(&query);
            for id in &analyzed_track_ids {
                q = q.bind(id);
            }
            q.fetch_all(pool).await.unwrap_or_default()
        };

        // Backfill: existing analyzed tracks without DCLAP embeddings
        let backfill_tracks: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, file_path FROM tracks WHERE analysis_status = 'complete' AND dclap_embedding IS NULL",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let mut dclap_ids: Vec<(String, String)> = tracks_for_dclap;
        // Add backfill tracks not already in the list
        let existing_ids: std::collections::HashSet<String> = dclap_ids.iter().map(|(id, _)| id.clone()).collect();
        for (id, path) in backfill_tracks {
            if !existing_ids.contains(&id) {
                dclap_ids.push((id, path));
            }
        }

        if !dclap_ids.is_empty() {
            let dclap_total = dclap_ids.len();
            info!(count = dclap_total, "computing DCLAP embeddings");
            let mut dclap_ok = 0u32;
            let mut dclap_fail = 0u32;

            // Pipeline: decode audio ahead while ONNX inference runs
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String, Result<(Vec<ndarray::Array2<f32>>, u128, u128), String>)>(3);

            let producer = tokio::spawn(async move {
                for (track_id, file_path) in dclap_ids {
                    let path = file_path.clone();
                    let mels_result = tokio::task::spawn_blocking(move || {
                        let t0 = std::time::Instant::now();
                        let samples = dclap::decode_audio_mono(&path)?;
                        let decode_ms = t0.elapsed().as_millis();

                        if samples.is_empty() {
                            anyhow::bail!("no audio samples decoded from {}", path);
                        }
                        let segments = dclap::segment_audio(&samples, 44100);
                        if segments.is_empty() {
                            anyhow::bail!("no valid segments from {}", path);
                        }

                        let t1 = std::time::Instant::now();
                        let mels: Vec<ndarray::Array2<f32>> = segments.iter().map(|s| dclap::compute_log_mel_spectrogram(s)).collect();
                        let mel_ms = t1.elapsed().as_millis();

                        Ok((mels, decode_ms, mel_ms))
                    }).await;

                    let result = match mels_result {
                        Ok(Ok(data)) => Ok(data),
                        Ok(Err(e)) => Err(format!("{e}")),
                        Err(e) => Err(format!("task panicked: {e}")),
                    };

                    if tx.send((track_id, file_path, result)).await.is_err() {
                        break;
                    }
                }
            });

            while let Some((track_id, file_path, mels_result)) = rx.recv().await {
                match mels_result {
                    Ok((mels, decode_ms, mel_ms)) => {
                        let model = Arc::clone(model);
                        let seg_count = mels.len();
                        let t2 = std::time::Instant::now();
                        let embedding = tokio::task::spawn_blocking(move || {
                            model.embed_audio_from_mels(&mels)
                        }).await;
                        let onnx_ms = t2.elapsed().as_millis();

                        let fname = Path::new(&file_path).file_name().unwrap_or_default().to_string_lossy().to_string();
                        info!(
                            "DCLAP timing: decode={}ms mel={}ms onnx={}ms segments={} | {}",
                            decode_ms, mel_ms, onnx_ms, seg_count, fname
                        );

                        match embedding {
                            Ok(Ok(emb)) => {
                                let bytes = dclap::embedding_to_bytes(&emb);
                                if let Err(e) = sqlx::query("UPDATE tracks SET dclap_embedding = ? WHERE id = ?")
                                    .bind(&bytes)
                                    .bind(&track_id)
                                    .execute(pool)
                                    .await
                                {
                                    warn!(track_id, error = %e, "failed to store DCLAP embedding");
                                    dclap_fail += 1;
                                } else {
                                    dclap_ok += 1;
                                }
                            }
                            Ok(Err(e)) => {
                                warn!(path = %file_path, error = %e, "DCLAP embedding failed");
                                dclap_fail += 1;
                            }
                            Err(e) => {
                                warn!(path = %file_path, error = %e, "DCLAP task panicked");
                                dclap_fail += 1;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(path = %file_path, error = %e, "DCLAP mel computation failed");
                        dclap_fail += 1;
                    }
                }

                let done = dclap_ok + dclap_fail;
                if done % 50 == 0 || done as usize == dclap_total {
                    info!(done, total = dclap_total, "DCLAP progress");
                }
            }

            producer.await.ok();
            info!(ok = dclap_ok, failed = dclap_fail, "DCLAP embeddings complete");
        }
    }

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
