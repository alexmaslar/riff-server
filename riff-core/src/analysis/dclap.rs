use anyhow::Result;
use ndarray::Array2;
use std::path::Path;
use std::sync::Mutex;
use tracing::info;

// Model files embedded in binary — zero runtime downloads
static DCLAP_AUDIO_MODEL: &[u8] = include_bytes!("../../models/dclap/model_epoch_36.onnx");
static DCLAP_AUDIO_DATA: &[u8] = include_bytes!("../../models/dclap/model_epoch_36.onnx.data");
static DCLAP_TEXT_MODEL: &[u8] = include_bytes!("../../models/dclap/clap_text_model.onnx");
static DCLAP_TOKENIZER: &[u8] = include_bytes!("../../models/dclap/tokenizer.json");

// Audio preprocessing constants (matching DCLAP's Python reference)
const SAMPLE_RATE: u32 = 44100;
const SEGMENT_DURATION: f32 = 10.0;
const SEGMENT_HOP: f32 = 5.0; // 50% overlap
const N_MELS: usize = 64;
const MEL_NORM_OFFSET: f32 = 42.6;
const MEL_NORM_SCALE: f32 = 25.9;
const EMBEDDING_DIM: usize = 512;

// Text tokenizer constants
const MAX_TOKEN_LENGTH: usize = 77;

pub struct DclapModel {
    audio_session: Mutex<ort::session::Session>,
    text_session: Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
}

impl DclapModel {
    /// Load DCLAP ONNX models from embedded bytes.
    /// Writes the external data file to a temp directory since ONNX Runtime
    /// needs file-system access for external tensor data.
    pub fn load() -> Result<Self> {
        // Write external data file to temp dir so ONNX Runtime can find it
        let data_dir = std::env::temp_dir().join("riff-dclap");
        std::fs::create_dir_all(&data_dir)?;

        let model_path = data_dir.join("model_epoch_36.onnx");
        let data_path = data_dir.join("model_epoch_36.onnx.data");

        // Only write if not already present or size changed
        let needs_write = !model_path.exists()
            || std::fs::metadata(&model_path)
                .map(|m| m.len() as usize != DCLAP_AUDIO_MODEL.len())
                .unwrap_or(true);

        if needs_write {
            std::fs::write(&model_path, DCLAP_AUDIO_MODEL)?;
            std::fs::write(&data_path, DCLAP_AUDIO_DATA)?;
        }

        let audio_session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create audio session builder: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("failed to set intra threads: {e}"))?
            .commit_from_file(&model_path)
            .map_err(|e| anyhow::anyhow!("failed to load DCLAP audio model: {e}"))?;

        let text_session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create text session builder: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("failed to set intra threads: {e}"))?
            .commit_from_memory(DCLAP_TEXT_MODEL)
            .map_err(|e| anyhow::anyhow!("failed to load DCLAP text model: {e}"))?;

        let tokenizer = tokenizers::Tokenizer::from_bytes(DCLAP_TOKENIZER)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {}", e))?;

        info!("DCLAP models loaded successfully");

        Ok(Self {
            audio_session: Mutex::new(audio_session),
            text_session: Mutex::new(text_session),
            tokenizer,
        })
    }

    /// Compute a 512-dim audio embedding for a track file. Blocking, runs on CPU.
    pub fn embed_audio(&self, file_path: &str) -> Result<Vec<f32>> {
        let samples = decode_audio_mono(file_path)?;
        if samples.is_empty() {
            anyhow::bail!("no audio samples decoded from {}", file_path);
        }

        let segments = segment_audio(&samples, SAMPLE_RATE);
        if segments.is_empty() {
            anyhow::bail!("no valid segments from {}", file_path);
        }

        let mut all_embeddings: Vec<Vec<f32>> = Vec::new();

        for segment in &segments {
            let mel = compute_log_mel_spectrogram(segment);
            let embedding = self.run_audio_inference(&mel)?;
            all_embeddings.push(embedding);
        }

        // Average all segment embeddings and L2-normalize
        let mut avg = vec![0.0f32; EMBEDDING_DIM];
        for emb in &all_embeddings {
            for (i, &v) in emb.iter().enumerate() {
                avg[i] += v;
            }
        }
        let n = all_embeddings.len() as f32;
        for v in &mut avg {
            *v /= n;
        }

        l2_normalize(&mut avg);
        Ok(avg)
    }

    /// Compute a 512-dim text embedding for a natural language prompt.
    pub fn embed_text(&self, prompt: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(prompt, true)
            .map_err(|e| anyhow::anyhow!("tokenizer encode failed: {}", e))?;

        let mut input_ids = encoding.get_ids().to_vec();
        let mut attention_mask = encoding.get_attention_mask().to_vec();

        // Pad or truncate to MAX_TOKEN_LENGTH
        input_ids.truncate(MAX_TOKEN_LENGTH);
        attention_mask.truncate(MAX_TOKEN_LENGTH);
        while input_ids.len() < MAX_TOKEN_LENGTH {
            input_ids.push(1); // RoBERTa pad token
            attention_mask.push(0);
        }

        let input_ids_i64: Vec<i64> = input_ids.iter().map(|&v| v as i64).collect();
        let attention_i64: Vec<i64> = attention_mask.iter().map(|&v| v as i64).collect();

        let ids_value =
            ort::value::Tensor::from_array(([1usize, MAX_TOKEN_LENGTH], input_ids_i64))
                .map_err(|e| anyhow::anyhow!("failed to create input_ids tensor: {e}"))?;
        let mask_value =
            ort::value::Tensor::from_array(([1usize, MAX_TOKEN_LENGTH], attention_i64))
                .map_err(|e| anyhow::anyhow!("failed to create attention_mask tensor: {e}"))?;

        let mut session = self
            .text_session
            .lock()
            .map_err(|e| anyhow::anyhow!("text session lock poisoned: {e}"))?;
        let outputs = session
            .run(ort::inputs![ids_value, mask_value])
            .map_err(|e| anyhow::anyhow!("DCLAP text inference failed: {e}"))?;

        let output_view = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract text embedding tensor: {e}"))?;

        let mut embedding: Vec<f32> = output_view.1.to_vec();
        embedding.truncate(EMBEDDING_DIM);

        if embedding.len() != EMBEDDING_DIM {
            anyhow::bail!(
                "text embedding dimension mismatch: got {}, expected {}",
                embedding.len(),
                EMBEDDING_DIM
            );
        }

        l2_normalize(&mut embedding);
        Ok(embedding)
    }

    fn run_audio_inference(&self, mel: &Array2<f32>) -> Result<Vec<f32>> {
        // mel shape: (time_frames, N_MELS) → model expects (batch, time, mels)
        let time_frames = mel.nrows();

        let data: Vec<f32> = mel.iter().copied().collect();
        let input_value =
            ort::value::Tensor::from_array(([1usize, time_frames, N_MELS], data))
                .map_err(|e| anyhow::anyhow!("failed to create mel tensor: {e}"))?;

        let mut session = self
            .audio_session
            .lock()
            .map_err(|e| anyhow::anyhow!("audio session lock poisoned: {e}"))?;
        let outputs = session
            .run(ort::inputs![input_value])
            .map_err(|e| anyhow::anyhow!("DCLAP audio inference failed: {e}"))?;

        let output_view = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract audio embedding tensor: {e}"))?;

        let embedding: Vec<f32> = output_view.1.to_vec();

        if embedding.len() < EMBEDDING_DIM {
            anyhow::bail!(
                "audio embedding dimension mismatch: got {}, expected {}",
                embedding.len(),
                EMBEDDING_DIM
            );
        }

        Ok(embedding[..EMBEDDING_DIM].to_vec())
    }
}

/// Cosine similarity between two L2-normalized vectors (just dot product).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Compute mean of multiple embeddings and L2-normalize the result.
pub fn compute_dclap_centroid(vectors: &[&[f32]]) -> Option<Vec<f32>> {
    if vectors.is_empty() {
        return None;
    }
    let dim = vectors[0].len();
    if dim == 0 {
        return None;
    }

    let mut centroid = vec![0.0f32; dim];
    let mut count = 0usize;
    for v in vectors {
        if v.len() == dim {
            for (i, &val) in v.iter().enumerate() {
                centroid[i] += val;
            }
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    for val in &mut centroid {
        *val /= count as f32;
    }
    l2_normalize(&mut centroid);
    Some(centroid)
}

/// Parse a DCLAP embedding BLOB from a database row.
pub fn parse_dclap_embedding(row: &sqlx::sqlite::SqliteRow) -> Option<Vec<f32>> {
    use sqlx::Row;
    let blob: Option<Vec<u8>> = row.try_get("dclap_embedding").ok().flatten();
    blob.and_then(|bytes| {
        if bytes.len() != EMBEDDING_DIM * 4 {
            return None;
        }
        let floats: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        Some(floats)
    })
}

/// Serialize a DCLAP embedding to bytes for BLOB storage.
pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// ─── Audio Preprocessing ─────────────────────────────────────────────────────

/// Decode audio file to mono f32 samples at 44100 Hz using Symphonia.
fn decode_audio_mono(file_path: &str) -> Result<Vec<f32>> {
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

    let source_rate = track
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

    let mut all_samples: Vec<f32> = Vec::new();

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
        let ch = channels as usize;

        // Downmix to mono
        for frame in samples.chunks(ch) {
            let mono: f32 = frame.iter().sum::<f32>() / ch as f32;
            all_samples.push(mono);
        }
    }

    // Resample to 44100 Hz if needed (simple linear interpolation)
    if source_rate != SAMPLE_RATE {
        all_samples = resample_linear(&all_samples, source_rate, SAMPLE_RATE);
    }

    Ok(all_samples)
}

/// Simple linear interpolation resampler.
fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio) as usize;
    let mut output = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f64;

        let sample = if idx + 1 < samples.len() {
            samples[idx] as f64 * (1.0 - frac) + samples[idx + 1] as f64 * frac
        } else if idx < samples.len() {
            samples[idx] as f64
        } else {
            0.0
        };

        output.push(sample as f32);
    }

    output
}

/// Split audio into segments of SEGMENT_DURATION seconds with SEGMENT_HOP overlap.
fn segment_audio(samples: &[f32], sample_rate: u32) -> Vec<Vec<f32>> {
    let seg_len = (SEGMENT_DURATION * sample_rate as f32) as usize;
    let hop_len = (SEGMENT_HOP * sample_rate as f32) as usize;

    if samples.len() < seg_len / 2 {
        // Too short — use the whole thing padded
        let mut padded = samples.to_vec();
        padded.resize(seg_len, 0.0);
        return vec![padded];
    }

    let mut segments = Vec::new();
    let mut start = 0;

    while start + seg_len <= samples.len() {
        segments.push(samples[start..start + seg_len].to_vec());
        start += hop_len;
    }

    // Handle the last partial segment if > 50% of segment length
    if start < samples.len() && samples.len() - start > seg_len / 2 {
        let mut last = samples[start..].to_vec();
        last.resize(seg_len, 0.0);
        segments.push(last);
    }

    // If we got nothing (very short audio), use padded full audio
    if segments.is_empty() {
        let mut padded = samples.to_vec();
        padded.resize(seg_len, 0.0);
        segments.push(padded);
    }

    segments
}

/// Compute log-mel spectrogram for a segment using mel_spec's Fbank.
/// Returns Array2<f32> of shape (time_frames, N_MELS), normalized for DCLAP.
fn compute_log_mel_spectrogram(samples: &[f32]) -> Array2<f32> {
    use mel_spec::fbank::{Fbank, FbankConfig};

    let config = FbankConfig {
        sample_rate: SAMPLE_RATE as f64,
        num_mel_bins: N_MELS,
        frame_length_ms: 1000.0 * 1024.0 / SAMPLE_RATE as f64, // ~23.2ms for 1024 FFT at 44100
        frame_shift_ms: 1000.0 * 480.0 / SAMPLE_RATE as f64,   // ~10.9ms for hop 480 at 44100
        use_log_fbank: false, // We'll apply our own normalization
        apply_cmn: false,
        ..Default::default()
    };

    let fbank = Fbank::new(config);
    let features = fbank.compute(samples);

    // Apply DCLAP-specific normalization: (log_mel + offset) / scale
    let normalized = features.mapv(|v| {
        let log_val = (v.max(1e-10)).ln();
        (log_val + MEL_NORM_OFFSET) / MEL_NORM_SCALE
    });

    normalized
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_compute_dclap_centroid() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let vecs: Vec<&[f32]> = vec![&a, &b];
        let centroid = compute_dclap_centroid(&vecs).unwrap();
        assert_eq!(centroid.len(), 3);
        let expected = 1.0 / 2.0f32.sqrt();
        assert!((centroid[0] - expected).abs() < 1e-5);
        assert!((centroid[1] - expected).abs() < 1e-5);
        assert!(centroid[2].abs() < 1e-5);
    }

    #[test]
    fn test_compute_dclap_centroid_empty() {
        let vecs: Vec<&[f32]> = vec![];
        assert!(compute_dclap_centroid(&vecs).is_none());
    }

    #[test]
    fn test_embedding_roundtrip() {
        let emb = vec![1.0f32, -0.5, 0.25, 0.0];
        let bytes = embedding_to_bytes(&emb);
        assert_eq!(bytes.len(), 16);

        let restored: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        assert_eq!(emb, restored);
    }

    #[test]
    fn test_l2_normalize() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-5);
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn test_segment_audio_short() {
        let samples = vec![0.0f32; 22050]; // 0.5s at 44100
        let segments = segment_audio(&samples, 44100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].len(), (SEGMENT_DURATION * 44100.0) as usize);
    }

    #[test]
    fn test_segment_audio_normal() {
        let samples = vec![0.0f32; 44100 * 30]; // 30s
        let segments = segment_audio(&samples, 44100);
        // 30s with 10s segments, 5s hop: starts at 0, 5, 10, 15, 20 = 5 segments
        assert_eq!(segments.len(), 5);
    }

    #[test]
    fn test_resample_same_rate() {
        let samples = vec![1.0f32, 2.0, 3.0];
        let resampled = resample_linear(&samples, 44100, 44100);
        assert_eq!(resampled, samples);
    }
}
