use anyhow::Result;
use ndarray::Array2;
use std::path::Path;
use std::sync::Mutex;
use tracing::info;

// Model files embedded in binary — zero runtime downloads
static DCLAP_AUDIO_MODEL: &[u8] = include_bytes!("../../models/dclap/model_epoch_36.onnx");
static DCLAP_TEXT_MODEL: &[u8] = include_bytes!("../../models/dclap/clap_text_model.onnx");
static DCLAP_TOKENIZER: &[u8] = include_bytes!("../../models/dclap/tokenizer.json");

// Audio preprocessing constants (matching DCLAP's Python reference)
const SAMPLE_RATE: u32 = 44100;
const SEGMENT_DURATION: f32 = 10.0;
const SEGMENT_HOP: f32 = 5.0; // 5s overlap — every second covered by at least one 10s window
const N_MELS: usize = 128;
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
    /// Load DCLAP ONNX models from embedded bytes (in-memory, no temp files).
    pub fn load() -> Result<Self> {
        let audio_session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create audio session builder: {e}"))?
            .with_intra_threads(4)
            .map_err(|e| anyhow::anyhow!("failed to set intra threads: {e}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("failed to set optimization level: {e}"))?
            .commit_from_memory(DCLAP_AUDIO_MODEL)
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
        let t0 = std::time::Instant::now();
        let samples = decode_audio_mono(file_path)?;
        let decode_ms = t0.elapsed().as_millis();

        if samples.is_empty() {
            anyhow::bail!("no audio samples decoded from {}", file_path);
        }

        let segments = segment_audio(&samples, SAMPLE_RATE);
        if segments.is_empty() {
            anyhow::bail!("no valid segments from {}", file_path);
        }

        let t1 = std::time::Instant::now();
        let mels: Vec<Array2<f32>> = segments.iter().map(|s| compute_log_mel_spectrogram(s)).collect();
        let mel_ms = t1.elapsed().as_millis();

        let t2 = std::time::Instant::now();
        let result = self.embed_audio_from_mels(&mels);
        let onnx_ms = t2.elapsed().as_millis();

        info!(
            "DCLAP timing: decode={}ms mel={}ms onnx={}ms segments={} | {}",
            decode_ms, mel_ms, onnx_ms, mels.len(),
            Path::new(file_path).file_name().unwrap_or_default().to_string_lossy()
        );

        result
    }

    /// Compute embedding from pre-computed mel spectrograms (batched inference).
    pub fn embed_audio_from_mels(&self, mels: &[Array2<f32>]) -> Result<Vec<f32>> {
        if mels.is_empty() {
            anyhow::bail!("no mel spectrograms provided");
        }

        let all_embeddings = self.run_audio_inference_batch(mels)?;

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

    /// Decode audio and compute mel spectrograms (CPU-only, no ONNX).
    pub fn compute_mels(file_path: &str) -> Result<Vec<Array2<f32>>> {
        let samples = decode_audio_mono(file_path)?;
        if samples.is_empty() {
            anyhow::bail!("no audio samples decoded from {}", file_path);
        }

        let segments = segment_audio(&samples, SAMPLE_RATE);
        if segments.is_empty() {
            anyhow::bail!("no valid segments from {}", file_path);
        }

        Ok(segments.iter().map(|s| compute_log_mel_spectrogram(s)).collect())
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

    fn run_audio_inference_batch(&self, mels: &[Array2<f32>]) -> Result<Vec<Vec<f32>>> {
        let mut session = self
            .audio_session
            .lock()
            .map_err(|e| anyhow::anyhow!("audio session lock poisoned: {e}"))?;

        let mut embeddings = Vec::with_capacity(mels.len());

        // Model has fixed batch=1, so iterate but hold the lock for the whole batch
        for mel in mels {
            let time_frames = mel.nrows();
            let transposed = mel.t();
            let data: Vec<f32> = transposed.iter().copied().collect();

            let input_value =
                ort::value::Tensor::from_array(([1usize, 1usize, N_MELS, time_frames], data))
                    .map_err(|e| anyhow::anyhow!("failed to create mel tensor: {e}"))?;

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
            embeddings.push(embedding[..EMBEDDING_DIM].to_vec());
        }

        Ok(embeddings)
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
pub fn decode_audio_mono(file_path: &str) -> Result<Vec<f32>> {
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
pub fn segment_audio(samples: &[f32], sample_rate: u32) -> Vec<Vec<f32>> {
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

// ── vDSP-accelerated mel spectrogram ─────────────────────────────────────────

const FFT_SIZE: usize = 1024;
const HOP_SIZE: usize = 480;

mod accelerate {
    //! Raw FFI bindings to Apple's Accelerate framework (vDSP).
    use std::os::raw::c_int;

    /// Split-complex representation for vDSP FFT.
    #[repr(C)]
    pub struct DSPSplitComplex {
        pub realp: *mut f32,
        pub imagp: *mut f32,
    }

    /// Opaque FFT setup handle.
    pub enum OpaqueFFTSetup {}
    pub type FFTSetup = *mut OpaqueFFTSetup;

    /// FFT direction constants.
    pub const FFT_FORWARD: c_int = 1;

    /// FFT radix constants.
    pub const FFT_RADIX2: c_int = 0;

    extern "C" {
        /// Create an FFT setup for a given log2 size.
        pub fn vDSP_create_fftsetup(log2n: u32, radix: c_int) -> FFTSetup;

        /// Destroy an FFT setup.
        pub fn vDSP_destroy_fftsetup(setup: FFTSetup);

        /// In-place real-to-complex FFT (interleaved → split-complex).
        pub fn vDSP_fft_zrip(
            setup: FFTSetup,
            c: *mut DSPSplitComplex,
            stride: i32,
            log2n: u32,
            direction: c_int,
        );

        /// Convert interleaved real data to split-complex format.
        pub fn vDSP_ctoz(
            c: *const f32, // interleaved pairs
            ic: i32,
            z: *mut DSPSplitComplex,
            iz: i32,
            n: u32,
        );

        /// Element-wise multiply-add: C = A * B (complex magnitudes squared).
        /// We use vDSP_zvmags for |z|^2 = re^2 + im^2.
        pub fn vDSP_zvmags(
            a: *const DSPSplitComplex,
            ia: i32,
            c: *mut f32,
            ic: i32,
            n: u32,
        );

        /// Vector-scalar multiply: C[i] = A[i] * B.
        pub fn vDSP_vsmul(
            a: *const f32,
            ia: i32,
            b: *const f32,
            c: *mut f32,
            ic: i32,
            n: u32,
        );

        /// Matrix multiply: C = A * B.
        pub fn vDSP_mmul(
            a: *const f32,
            ia: i32,
            b: *const f32,
            ib: i32,
            c: *mut f32,
            ic: i32,
            m: u32, // rows of A / rows of C
            n: u32, // cols of B / cols of C
            p: u32, // cols of A / rows of B
        );
    }
}

/// Lazily-initialized mel filterbank matrix and FFT setup.
struct MelState {
    /// Mel filterbank: shape (n_fft_bins, n_mels) stored column-major for vDSP_mmul.
    /// Actually stored as (n_fft_bins × n_mels) row-major where each row is one FFT bin.
    filterbank: Vec<f32>,
    n_fft_bins: usize,
    fft_setup: accelerate::FFTSetup,
    log2n: u32,
    hann_window: Vec<f32>,
}

unsafe impl Send for MelState {}
unsafe impl Sync for MelState {}

impl Drop for MelState {
    fn drop(&mut self) {
        unsafe {
            accelerate::vDSP_destroy_fftsetup(self.fft_setup);
        }
    }
}

impl MelState {
    fn new() -> Self {
        let n_fft_bins = FFT_SIZE / 2 + 1; // 513 bins
        let log2n = (FFT_SIZE as f32).log2() as u32; // 10

        // Create FFT setup
        let fft_setup = unsafe {
            accelerate::vDSP_create_fftsetup(log2n, accelerate::FFT_RADIX2)
        };
        assert!(!fft_setup.is_null(), "failed to create vDSP FFT setup");

        // Pre-compute Hann window
        let hann_window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
            })
            .collect();

        // Build mel filterbank matrix (n_fft_bins × N_MELS)
        let filterbank = build_mel_filterbank(SAMPLE_RATE, FFT_SIZE, N_MELS);

        MelState {
            filterbank,
            n_fft_bins,
            fft_setup,
            log2n,
            hann_window,
        }
    }
}

/// Get or initialize the global mel state.
fn mel_state() -> &'static MelState {
    use std::sync::OnceLock;
    static STATE: OnceLock<MelState> = OnceLock::new();
    STATE.get_or_init(MelState::new)
}

/// Convert frequency in Hz to mel scale.
fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

/// Convert mel scale to frequency in Hz.
fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0f32.powf(mel / 2595.0) - 1.0)
}

/// Build a mel filterbank matrix of shape (n_fft_bins × n_mels).
fn build_mel_filterbank(sample_rate: u32, fft_size: usize, n_mels: usize) -> Vec<f32> {
    let n_fft_bins = fft_size / 2 + 1;
    let sr = sample_rate as f32;

    let mel_low = hz_to_mel(0.0);
    let mel_high = hz_to_mel(sr / 2.0);

    // n_mels + 2 equally spaced points in mel space
    let mel_points: Vec<f32> = (0..n_mels + 2)
        .map(|i| mel_to_hz(mel_low + (mel_high - mel_low) * i as f32 / (n_mels + 1) as f32))
        .collect();

    // Convert to FFT bin indices
    let bin_points: Vec<f32> = mel_points
        .iter()
        .map(|&hz| hz * fft_size as f32 / sr)
        .collect();

    let mut filterbank = vec![0.0f32; n_fft_bins * n_mels];

    for m in 0..n_mels {
        let f_left = bin_points[m];
        let f_center = bin_points[m + 1];
        let f_right = bin_points[m + 2];

        for k in 0..n_fft_bins {
            let kf = k as f32;
            let weight = if kf >= f_left && kf <= f_center && f_center > f_left {
                (kf - f_left) / (f_center - f_left)
            } else if kf > f_center && kf <= f_right && f_right > f_center {
                (f_right - kf) / (f_right - f_center)
            } else {
                0.0
            };
            filterbank[k * n_mels + m] = weight;
        }
    }

    filterbank
}

/// Compute log-mel spectrogram using Apple's vDSP (Accelerate framework).
/// Returns Array2<f32> of shape (time_frames, N_MELS), normalized for DCLAP.
pub fn compute_log_mel_spectrogram(samples: &[f32]) -> Array2<f32> {
    let state = mel_state();
    let n_frames = if samples.len() >= FFT_SIZE {
        (samples.len() - FFT_SIZE) / HOP_SIZE + 1
    } else {
        0
    };

    if n_frames == 0 {
        return Array2::zeros((1, N_MELS));
    }

    let half_n = FFT_SIZE / 2;

    // Buffers reused per frame
    let mut windowed = vec![0.0f32; FFT_SIZE];
    let mut real = vec![0.0f32; half_n];
    let mut imag = vec![0.0f32; half_n];
    let mut power_spectrum = vec![0.0f32; state.n_fft_bins];
    let mut mel_row = vec![0.0f32; N_MELS];

    let mut output = Vec::with_capacity(n_frames * N_MELS);

    for frame in 0..n_frames {
        let offset = frame * HOP_SIZE;
        let frame_samples = &samples[offset..offset + FFT_SIZE];

        // Apply Hann window
        for i in 0..FFT_SIZE {
            windowed[i] = frame_samples[i] * state.hann_window[i];
        }

        // Pack into split-complex for vDSP_fft_zrip
        // vDSP_ctoz converts interleaved pairs to split complex
        let mut split = accelerate::DSPSplitComplex {
            realp: real.as_mut_ptr(),
            imagp: imag.as_mut_ptr(),
        };

        unsafe {
            accelerate::vDSP_ctoz(
                windowed.as_ptr(),
                2, // stride of 2 (interleaved real pairs)
                &mut split,
                1,
                half_n as u32,
            );

            // In-place FFT
            accelerate::vDSP_fft_zrip(
                state.fft_setup,
                &mut split,
                1,
                state.log2n,
                accelerate::FFT_FORWARD,
            );

            // Compute power spectrum |X(k)|^2
            // DC component: real[0] contains DC, imag[0] contains Nyquist (vDSP packing)
            power_spectrum[0] = real[0] * real[0]; // DC
            power_spectrum[half_n] = imag[0] * imag[0]; // Nyquist

            // Bins 1..half_n-1
            accelerate::vDSP_zvmags(&split, 1, power_spectrum[1..].as_mut_ptr(), 1, (half_n - 1) as u32);
            // vDSP_zvmags wrote bins 1..half_n-1, but we offset by 1, so it's correct.

            // Scale by 1/FFT_SIZE^2 for proper normalization
            let scale = 1.0f32 / (FFT_SIZE as f32 * FFT_SIZE as f32);
            accelerate::vDSP_vsmul(
                power_spectrum.as_ptr(),
                1,
                &scale,
                power_spectrum.as_mut_ptr(),
                1,
                state.n_fft_bins as u32,
            );

            // Mel filterbank: mel_row = power_spectrum (1×513) × filterbank (513×128) → (1×128)
            accelerate::vDSP_mmul(
                power_spectrum.as_ptr(),
                1,
                state.filterbank.as_ptr(),
                1,
                mel_row.as_mut_ptr(),
                1,
                1,                        // m: 1 row
                N_MELS as u32,            // n: 128 cols output
                state.n_fft_bins as u32,  // p: 513 cols input
            );
        }

        // Apply DCLAP normalization: (ln(max(mel, 1e-10)) + offset) / scale
        for &v in &mel_row {
            let log_val = v.max(1e-10).ln();
            output.push((log_val + MEL_NORM_OFFSET) / MEL_NORM_SCALE);
        }
    }

    Array2::from_shape_vec((n_frames, N_MELS), output).unwrap()
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
