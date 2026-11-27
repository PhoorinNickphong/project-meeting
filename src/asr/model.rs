use super::audio::{self, RATE};
use ort::{session::Session, value::Tensor};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::{fs, io, path::Path, sync::Arc};

const FFT_SIZE: usize = 512;
const WINDOW: usize = 400;
const HOP: usize = 160;
const BANDS: usize = 128;
const WIDTH: usize = 1_024;
const STATE_LEN: usize = 1_280;
const BLANK: usize = 14_988;

pub struct OpenThai {
    encoder: Session,
    decoder: Session,
    tokens: Vec<String>,
    features: Features,
}

fn ort_error<T>(error: ort::Error<T>) -> io::Error {
    io::Error::other(error.to_string())
}

impl OpenThai {
    pub fn open(path: &Path, dll: &Path) -> io::Result<Self> {
        ort::init_from(dll)
            .map_err(ort_error)?
            .with_telemetry(false)
            .commit();
        let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
        let session = |file: &str| {
            Session::builder()
                .map_err(ort_error)?
                .with_intra_threads(threads)
                .map_err(ort_error)?
                .commit_from_file(path.join(file))
                .map_err(ort_error)
        };
        let tokens: Vec<_> = fs::read_to_string(path.join("tokens.txt"))?
            .lines()
            .map(str::to_owned)
            .collect();
        if tokens.len() != BLANK {
            return Err(io::Error::other("จำนวนโทเคนไม่ตรงกับรุ่น OpenThai FP32"));
        }
        Ok(Self {
            encoder: session("encoder.onnx")?,
            decoder: session("decoder_joint.onnx")?,
            tokens,
            features: Features::new(),
        })
    }

    pub fn decode(&mut self, samples: &[i16], source_rate: usize) -> io::Result<String> {
        let mut samples = audio::resample(samples, source_rate);
        let blank_bias = if samples.len() < 2 * RATE { -2.5 } else { -1.0 };
        // Keep the input long enough for the encoder's convolutional subsampling.
        samples.resize(samples.len().max(RATE / 4), 0.0);
        let (mel, frames) = self.features.extract(&samples);
        let encoded = self
            .encoder
            .run(ort::inputs![
                "audio_signal" => Tensor::from_array(([1, BANDS, frames], mel)).map_err(ort_error)?,
                "length" => Tensor::from_array(([1], vec![frames as i64])).map_err(ort_error)?,
                "prompt_index" => Tensor::from_array(([1], vec![32_i64])).map_err(ort_error)?,
            ])
            .map_err(ort_error)?;
        let (_, features) = encoded["encoded"]
            .try_extract_tensor::<f32>()
            .map_err(ort_error)?;
        let (_, lengths) = encoded["encoded_length"]
            .try_extract_tensor::<i64>()
            .map_err(ort_error)?;
        let length = usize::try_from(lengths[0]).map_err(io::Error::other)?;
        if length > features.len() / WIDTH {
            return Err(io::Error::other("จำนวนเฟรมที่โมเดลส่งกลับไม่ถูกต้อง"));
        }

        let mut state_1 = vec![0.0_f32; STATE_LEN];
        let mut state_2 = vec![0.0_f32; STATE_LEN];
        let mut last = BLANK;
        let mut text = String::new();
        for frame in features.as_chunks::<WIDTH>().0.iter().take(length) {
            // Advance the predictor state only when the joint emits a non-blank token.
            for _ in 0..2 {
                let result = self.decoder.run(ort::inputs![
                    "encoder_outputs" => Tensor::from_array(([1, WIDTH, 1], frame.to_vec())).map_err(ort_error)?,
                    "targets" => Tensor::from_array(([1, 1], vec![last as i32])).map_err(ort_error)?,
                    "target_length" => Tensor::from_array(([1], vec![1_i32])).map_err(ort_error)?,
                    "input_states_1" => Tensor::from_array(([2, 1, 640], state_1.clone())).map_err(ort_error)?,
                    "input_states_2" => Tensor::from_array(([2, 1, 640], state_2.clone())).map_err(ort_error)?,
                ]).map_err(ort_error)?;
                let (_, scores) = result["outputs"]
                    .try_extract_tensor::<f32>()
                    .map_err(ort_error)?;
                if scores.len() != BLANK + 1 {
                    return Err(io::Error::other("จำนวนคะแนนจากตัวถอด OpenThai ไม่ถูกต้อง"));
                }
                let mut best = (BLANK, scores[BLANK] + blank_bias);
                for (id, &value) in scores[..BLANK].iter().enumerate() {
                    if value > best.1 {
                        best = (id, value);
                    }
                }
                if best.0 == BLANK {
                    break;
                }
                last = best.0;
                if last != 0 {
                    text.push_str(&self.tokens[last]);
                }
                state_1.copy_from_slice(
                    result["output_states_1"]
                        .try_extract_tensor::<f32>()
                        .map_err(ort_error)?
                        .1,
                );
                state_2.copy_from_slice(
                    result["output_states_2"]
                        .try_extract_tensor::<f32>()
                        .map_err(ort_error)?
                        .1,
                );
            }
        }
        Ok(text.replace('▁', " ").trim().to_owned())
    }
}

struct Features {
    fft: Arc<dyn Fft<f32>>,
    window: [f32; WINDOW],
    filters: Vec<Vec<(usize, f32)>>,
}

impl Features {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = std::array::from_fn(|i| {
            0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (WINDOW - 1) as f32).cos()
        });
        // Slaney mel scale and area normalization used by NeMo's filterbank.
        let log_step = 6.4_f32.ln() / 27.0;
        let top_mel = 15.0 + (8.0_f32).ln() / log_step;
        let centers: Vec<_> = (0..BANDS + 2)
            .map(|index| {
                let mel = top_mel * index as f32 / (BANDS + 1) as f32;
                if mel < 15.0 {
                    mel * (200.0 / 3.0)
                } else {
                    1_000.0 * (log_step * (mel - 15.0)).exp()
                }
            })
            .collect();
        let filters = centers
            .windows(3)
            .map(|band| {
                (0..=FFT_SIZE / 2)
                    .filter_map(|bin| {
                        let hz = bin as f32 * RATE as f32 / FFT_SIZE as f32;
                        let rise = (hz - band[0]) / (band[1] - band[0]);
                        let fall = (band[2] - hz) / (band[2] - band[1]);
                        let weight = rise.min(fall) * 2.0 / (band[2] - band[0]);
                        (weight > 0.0).then_some((bin, weight))
                    })
                    .collect()
            })
            .collect();
        Self {
            fft,
            window,
            filters,
        }
    }

    fn extract(&self, samples: &[f32]) -> (Vec<f32>, usize) {
        let frames = samples.len() / HOP;
        let mut mel = vec![0.0; BANDS * frames];
        let mut scratch = vec![Complex32::default(); self.fft.get_inplace_scratch_len()];
        let mut signal = [Complex32::default(); FFT_SIZE];
        let mut power = [0.0; FFT_SIZE / 2 + 1];
        for frame in 0..frames {
            signal.fill(Complex32::default());
            let center = (frame * HOP) as isize;
            for (i, &window) in self.window.iter().enumerate() {
                let index = center + i as isize - (WINDOW / 2) as isize;
                if let Some(&value) = samples.get(index as usize) {
                    let previous = samples.get((index - 1) as usize).copied().unwrap_or(0.0);
                    signal[(FFT_SIZE - WINDOW) / 2 + i].re = (value - 0.97 * previous) * window;
                }
            }
            self.fft.process_with_scratch(&mut signal, &mut scratch);
            for (energy, value) in power.iter_mut().zip(signal) {
                *energy = value.norm_sqr();
            }
            for (band, filter) in self.filters.iter().enumerate() {
                let energy: f32 = filter
                    .iter()
                    .map(|&(bin, weight)| power[bin] * weight)
                    .sum();
                mel[band * frames + frame] = (energy + f32::EPSILON / 2.0).ln();
            }
        }
        (mel, frames)
    }
}
