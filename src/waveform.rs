use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

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
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe().format(
        &hint, mss, &FormatOptions::default(), &MetadataOptions::default(),
    )?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no playable track"))?;

    // symphonia 0.5 can demux Opus but has no decoder for it — pull
    // samples from our libopus-backed source instead.
    if track.codec_params.codec == CODEC_TYPE_OPUS {
        let source = crate::opus::OpusSource::new(path)?;
        return Ok(peaks_from(source));
    }

    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())?;

    let mut peaks: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(_)) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
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
