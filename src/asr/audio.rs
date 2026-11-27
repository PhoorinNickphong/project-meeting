use crate::storage::Meeting;
use std::{fs::File, io, path::Path};

pub const RATE: usize = 16_000;

pub fn read_segments(
    path: &Path,
    meeting: &Meeting,
    mut segment: impl FnMut(u64, u64, Option<u32>, &[i16]) -> io::Result<()>,
    mut progress: impl FnMut(u64),
) -> io::Result<()> {
    let mut wav = hound::WavReader::open(path).map_err(io::Error::other)?;
    let spec = wav.spec();
    if spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
        || spec.sample_rate != meeting.sample_rate
        || wav.duration() as u64 != meeting.frames
    {
        return Err(io::Error::other(
            "รูปแบบ WAV หรือจำนวนเฟรมเสียงไม่ตรงกับข้อมูลการประชุม",
        ));
    }
    let rate = spec.sample_rate as usize;
    let block_size = rate / 50;
    let padding = rate / 10;
    let mut block = Vec::with_capacity(block_size);
    let mut clip = Vec::with_capacity(rate * 24);
    let mut cursor = 0;
    let mut speaker = None;
    let mut samples = wav.samples::<i16>();

    for (end, next_speaker) in meeting
        .changes
        .iter()
        .map(|change| (change.frame, change.speaker))
        .chain(std::iter::once((meeting.frames, None)))
    {
        if end < cursor || end > meeting.frames {
            return Err(io::Error::other("จุดเปลี่ยนผู้พูดอยู่นอกลำดับของ WAV"));
        }
        let mut trailing = 0;
        let mut has_voice = false;
        while cursor < end {
            let len = (end - cursor).min(block_size as u64) as usize;
            read_block(&mut samples, &mut block, len)?;
            cursor += len as u64;
            let energy: f64 = block.iter().map(|&value| f64::from(value).powi(2)).sum();
            // A pause is measured on the original WAV, before resampling or mel extraction.
            if energy >= len as f64 * 100.0_f64.powi(2) {
                trailing = 0;
                has_voice = true;
            } else {
                trailing += len;
            }
            clip.extend_from_slice(&block);

            if has_voice && (trailing >= rate * 3 / 5 || clip.len() >= rate * 24 || cursor == end) {
                let start = cursor - clip.len() as u64;
                let kept = clip.len() - trailing.saturating_sub(padding);
                segment(start, start + kept as u64, speaker, &clip[..kept])?;
                clip.clear();
                trailing = 0;
                has_voice = false;
            } else if !has_voice && clip.len() > padding {
                clip.drain(..clip.len() - padding);
            }
            progress(cursor);
        }
        clip.clear();
        speaker = next_speaker;
    }
    Ok(())
}

fn read_block(
    samples: &mut hound::WavSamples<'_, std::io::BufReader<File>, i16>,
    block: &mut Vec<i16>,
    len: usize,
) -> io::Result<()> {
    block.clear();
    for _ in 0..len {
        let sample = samples
            .next()
            .ok_or_else(|| io::Error::other("ข้อมูลเสียงใน WAV ไม่ครบ"))?;
        block.push(sample.map_err(io::Error::other)?);
    }
    Ok(())
}

pub fn resample(samples: &[i16], source_rate: usize) -> Vec<f32> {
    let ratio = source_rate as f64 / RATE as f64;
    if source_rate == RATE {
        return samples
            .iter()
            .map(|&sample| f32::from(sample) / 32_768.0)
            .collect();
    }
    let cutoff = 0.94 / ratio.max(1.0);
    let radius = (16.0 * ratio.max(1.0)).ceil() as isize;
    let len = (samples.len() * RATE).div_ceil(source_rate);
    (0..len)
        .map(|index| {
            let position = index as f64 * ratio;
            let center = position.floor() as isize;
            let mut value = 0.0;
            let mut weight = 0.0;
            for i in center - radius + 1..=center + radius {
                let distance = position - i as f64;
                let phase = std::f64::consts::PI * distance;
                let sinc = if phase.abs() < 1e-9 {
                    cutoff
                } else {
                    (cutoff * phase).sin() / phase
                };
                let coefficient = sinc * (0.5 + 0.5 * (phase / radius as f64).cos());
                let sample = samples[i.clamp(0, samples.len() as isize - 1) as usize];
                value += f64::from(sample) * coefficient;
                weight += coefficient;
            }
            (value / (32_768.0 * weight)) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Change, random_id};

    #[test]
    fn chunks_respect_exact_speaker_changes_and_split_long_silence() {
        let rate = 48_000;
        let turn = 21_001;
        let total = 85_773;
        let meeting = Meeting {
            id: random_id().unwrap(),
            title: "Test".into(),
            speakers: vec!["A".into(), "B".into()],
            sample_rate: rate,
            created_at: 0,
            frames: total,
            finished: true,
            changes: vec![Change {
                frame: turn,
                speaker: Some(2),
            }],
            markers: Vec::new(),
            incident: None,
        };
        let path = std::env::temp_dir().join(format!("meeting-asr-{}.wav", meeting.id));
        let mut wav = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for i in 0..total {
            let sample: i16 = if (28_681..67_081).contains(&i) {
                0
            } else {
                1000
            };
            wav.write_sample(sample).unwrap();
        }
        wav.finalize().unwrap();
        let mut segments = Vec::new();
        let mut progress = 0;
        let result = read_segments(
            &path,
            &meeting,
            |start, end, speaker, samples| {
                assert_eq!(end - start, samples.len() as u64);
                segments.push((start, end, speaker));
                Ok(())
            },
            |frame| progress = frame,
        );
        std::fs::remove_file(path).unwrap();
        result.unwrap();
        assert_eq!(
            segments,
            [
                (0, turn, None),
                (turn, 33_481, Some(2)),
                (62_281, total, Some(2))
            ]
        );
        assert_eq!(progress, total);
    }

    #[test]
    fn resampling_48_khz_preserves_level_and_expected_duration() {
        let output = resample(&[8192; 480], 48_000);
        assert_eq!(output.len(), 160);
        assert!(output.iter().all(|sample| (sample - 0.25).abs() < 1e-5));
    }
}
