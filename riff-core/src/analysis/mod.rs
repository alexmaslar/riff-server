use rustfft::{FftPlanner, num_complex::Complex};
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
/// Extracts BPM, key (via STFT + chroma), and loudness (ebur128).
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
                        sqlx::query(
                            "UPDATE tracks SET bpm_analyzed = ?, key_analyzed = ?, loudness_lufs = ?, bliss_features = NULL, analysis_status = 'complete', analyzed_at = datetime('now') WHERE id = ?",
                        )
                        .bind(data.bpm)
                        .bind(&data.key)
                        .bind(data.loudness_lufs)
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
        "audio analysis complete",
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
}

/// Analyze a single track: BPM (spectral flux autocorrelation), key (chroma), loudness (ebur128).
fn analyze_track(file_path: &str) -> anyhow::Result<TrackAnalysis> {
    let (samples, sample_rate, channels) = decode_audio(file_path)?;

    // Mix to mono
    let mono: Vec<f32> = if channels > 1 {
        samples
            .chunks(channels as usize)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        samples
    };

    // Compute STFT
    let fft_size = 4096;
    let hop_size = 512;
    let spectral_frames = compute_stft(&mono, fft_size, hop_size);

    // Extract chroma from spectral frames
    let chroma = extract_chroma(&spectral_frames, sample_rate, fft_size);

    // Estimate key from averaged chroma
    let key = estimate_key_from_chroma(&chroma);

    // Detect BPM via spectral flux onset strength + autocorrelation
    let bpm = detect_bpm(&spectral_frames, sample_rate as f64, hop_size);

    // Measure loudness via ebur128
    let loudness_lufs = measure_lufs(file_path).ok();

    Ok(TrackAnalysis {
        bpm,
        key,
        loudness_lufs,
    })
}

// ─── Audio Decoding ──────────────────────────────────────────────────────────

/// Decode audio file to interleaved f32 samples using symphonia.
fn decode_audio(file_path: &str) -> anyhow::Result<(Vec<f32>, u32, u16)> {
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
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let track_id = track.id;

    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut all_samples = Vec::new();

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
        all_samples.extend_from_slice(sample_buf.samples());
    }

    Ok((all_samples, sample_rate, channels))
}

// ─── STFT ────────────────────────────────────────────────────────────────────

fn compute_stft(mono: &[f32], fft_size: usize, hop_size: usize) -> Vec<Vec<f64>> {
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(fft_size);

    let half = fft_size / 2 + 1;
    let mut frames = Vec::new();

    // Precompute Hann window
    let window: Vec<f64> = (0..fft_size)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / fft_size as f64).cos()))
        .collect();

    let mut pos = 0;
    while pos + fft_size <= mono.len() {
        let mut buffer: Vec<Complex<f64>> = mono[pos..pos + fft_size]
            .iter()
            .enumerate()
            .map(|(i, &s)| Complex::new(s as f64 * window[i], 0.0))
            .collect();

        fft.process(&mut buffer);

        let magnitudes: Vec<f64> = buffer[..half]
            .iter()
            .map(|c| c.norm())
            .collect();

        frames.push(magnitudes);
        pos += hop_size;
    }

    frames
}

// ─── Chroma Extraction ───────────────────────────────────────────────────────

fn extract_chroma(spectral_frames: &[Vec<f64>], sample_rate: u32, fft_size: usize) -> Vec<f64> {
    let mut chroma = [0.0f64; 12];

    for frame in spectral_frames {
        for (bin, &mag) in frame.iter().enumerate().skip(1) {
            let freq = bin as f64 * sample_rate as f64 / fft_size as f64;
            if freq < 20.0 || freq > 5000.0 {
                continue;
            }
            // Map frequency to pitch class (0 = C, 1 = C#, ..., 11 = B)
            let midi = 12.0 * (freq / 440.0).log2() + 69.0;
            let pitch_class = ((midi.round() as i32 % 12) + 12) % 12;
            chroma[pitch_class as usize] += mag * mag; // energy
        }
    }

    // Normalize
    let max = chroma.iter().copied().fold(0.0f64, f64::max);
    if max > 0.0 {
        for c in &mut chroma {
            *c /= max;
        }
    }

    chroma.to_vec()
}

// ─── Key Detection ───────────────────────────────────────────────────────────

/// Estimate musical key from chroma vector using the Krumhansl-Schmuckler algorithm.
fn estimate_key_from_chroma(chroma: &[f64]) -> Option<String> {
    if chroma.len() < 12 {
        return None;
    }

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

// ─── BPM Detection ───────────────────────────────────────────────────────────

/// Detect BPM via spectral flux onset strength → autocorrelation → peak picking.
fn detect_bpm(spectral_frames: &[Vec<f64>], sample_rate: f64, hop_size: usize) -> Option<f64> {
    if spectral_frames.len() < 2 {
        return None;
    }

    let mut onset_strength: Vec<f64> = Vec::with_capacity(spectral_frames.len() - 1);
    for i in 1..spectral_frames.len() {
        let flux: f64 = spectral_frames[i]
            .iter()
            .zip(spectral_frames[i - 1].iter())
            .map(|(&curr, &prev)| (curr - prev).max(0.0))
            .sum();
        onset_strength.push(flux);
    }

    if onset_strength.is_empty() {
        return None;
    }

    // Normalize onset strength
    let max_onset = onset_strength.iter().copied().fold(0.0f64, f64::max);
    if max_onset > 0.0 {
        for v in &mut onset_strength {
            *v /= max_onset;
        }
    }

    // Autocorrelation over BPM range 60-200
    let frames_per_sec = sample_rate / hop_size as f64;
    let min_lag = (frames_per_sec * 60.0 / 200.0) as usize; // 200 BPM
    let max_lag = (frames_per_sec * 60.0 / 60.0) as usize;  // 60 BPM
    let max_lag = max_lag.min(onset_strength.len() - 1);

    if min_lag >= max_lag {
        return None;
    }

    let mut best_lag = min_lag;
    let mut best_corr = f64::NEG_INFINITY;

    for lag in min_lag..=max_lag {
        let mut corr = 0.0;
        let n = onset_strength.len() - lag;
        for i in 0..n {
            corr += onset_strength[i] * onset_strength[i + lag];
        }
        corr /= n as f64;

        if corr > best_corr {
            best_corr = corr;
            best_lag = lag;
        }
    }

    let bpm = 60.0 * frames_per_sec / best_lag as f64;

    // Validate BPM range
    if bpm >= 60.0 && bpm <= 200.0 {
        Some((bpm * 10.0).round() / 10.0)
    } else {
        None
    }
}

// ─── Loudness Measurement ────────────────────────────────────────────────────

/// Measure integrated loudness in LUFS using ebur128.
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
