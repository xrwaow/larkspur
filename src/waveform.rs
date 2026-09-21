use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymphoniaError;

use crate::decode::open_track;

/// Compute `buckets` normalized peaks (0.0–1.0) for the whole file.
pub fn compute_waveform(path: &Path, buckets: usize) -> anyhow::Result<Vec<f32>> {
    let peaks = collect_peaks(path)?;

    if peaks.is_empty() {
        return Ok(vec![0.0; buckets]);
    }

    let chunk_size = ((peaks.len() as f32 / buckets as f32).ceil() as usize).max(1);
    let bucketed: Vec<f32> = peaks
        .chunks(chunk_size)
        .map(|c| c.iter().copied().fold(0f32, f32::max))
        .collect();

    let max = bucketed.iter().copied().fold(0f32, f32::max).max(0.0001);
    Ok(bucketed
        .into_iter()
        .map(|v| v / max)
        .chain(std::iter::repeat(0.0))
        .take(buckets)
        .collect())
}

/// Per-512-sample interleaved peaks of the whole file.
fn collect_peaks(path: &Path) -> anyhow::Result<Vec<f32>> {
    let opened = open_track(path, false)?;

    // Symphonia 0.5 can demux Opus but has no decoder for it — pull
    // samples from our libopus-backed source instead.
    if opened.codec_params.codec == CODEC_TYPE_OPUS {
        return Ok(peaks_from(crate::opus::OpusSource::new(path)?));
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&opened.codec_params, &DecoderOptions::default())?;
    let track_id = opened.track_id;
    let mut format = opened.format;

    let mut peaks: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };

        if sample_buf.is_none() {
            sample_buf = Some(SampleBuffer::new(decoded.capacity() as u64, *decoded.spec()));
        }
        let buf = sample_buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);

        push_peaks(buf.samples(), &mut peaks);
    }

    Ok(peaks)
}

fn push_peaks(samples: &[f32], peaks: &mut Vec<f32>) {
    for chunk in samples.chunks(512) {
        peaks.push(chunk.iter().fold(0f32, |m, &s| m.max(s.abs())));
    }
}

fn peaks_from(samples: impl Iterator<Item = f32>) -> Vec<f32> {
    let mut peaks = Vec::new();
    let mut chunk = Vec::with_capacity(512);
    for sample in samples {
        chunk.push(sample);
        if chunk.len() == 512 {
            push_peaks(&chunk, &mut peaks);
            chunk.clear();
        }
    }
    push_peaks(&chunk, &mut peaks);
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_normalized_and_padded() {
        // Four peaks of increasing magnitude, bucketed down to 2.
        let peaks = [0.25, 0.5, 0.75, 1.0];
        let chunk = ((peaks.len() as f32 / 2.0).ceil() as usize).max(1);
        let bucketed: Vec<f32> = peaks
            .chunks(chunk)
            .map(|c| c.iter().copied().fold(0f32, f32::max))
            .collect();
        let max = bucketed.iter().copied().fold(0f32, f32::max).max(0.0001);
        let normalized: Vec<f32> = bucketed.into_iter().map(|v| v / max).collect();
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[1], 1.0, "loudest bucket normalizes to 1.0");
        assert!(normalized[0] < normalized[1]);
    }

    #[test]
    fn peaks_group_every_512_samples() {
        let samples = vec![1.0f32; 1025];
        let mut peaks = Vec::new();
        push_peaks(&samples, &mut peaks);
        assert_eq!(peaks.len(), 3, "1025 samples -> 512, 512, 1");
        assert!(peaks.iter().all(|&p| p == 1.0));
    }
}
