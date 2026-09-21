/// Throwaway diagnostic: verifies the exact decode + seek chain the app
/// uses (rodio Decoder built from a File, so byte_len is set), without
/// needing a UI. Run: cargo run --bin seek_probe -- <audio-file>
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use rodio::{Decoder, Source};

use larkspur::opus::OpusSource;

/// Same source-selection logic as PlaybackController::track_parts.
fn build_source(path: &Path) -> anyhow::Result<Box<dyn Source<Item = f32> + Send>> {
    match Decoder::try_from(File::open(path)?) {
        Ok(decoder) => Ok(Box::new(decoder)),
        Err(_) => Ok(Box::new(OpusSource::new(path)?)),
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: seek_probe <audio-file>");

    // Exercise the full track_parts path, including tag reading.
    let metadata = larkspur::datatypes::SongMetadata::load(Path::new(&path))?;
    println!(
        "metadata: {:?} / {:?}, duration {:?}",
        metadata.as_ref().map(|m| &m.song_name),
        metadata.as_ref().map(|m| &m.artists),
        metadata.as_ref().map(|m| m.duration),
    );
    let waveform = larkspur::waveform::compute_waveform(Path::new(&path), 120)?;
    println!(
        "waveform: {} bars, peak {:.2}",
        waveform.len(),
        waveform.iter().copied().fold(0.0f32, f32::max)
    );

    let mut source = build_source(Path::new(&path))?;
    println!("total_duration: {:?}", source.total_duration());
    println!("channels: {}, sample rate: {}", source.channels(), source.sample_rate());

    // Consume ~1s of samples so we're mid-stream, like the app at 0:18.
    let mut decoded = 0usize;
    let target = source.sample_rate().get() as usize * source.channels().get() as usize;
    while decoded < target {
        match source.next() {
            Some(_) => decoded += 1,
            None => break,
        }
    }
    println!("decoded {decoded} samples (~1s)");

    for target in [Duration::from_secs(10), Duration::from_secs(20)] {
        match source.try_seek(target) {
            Ok(()) => {
                // Confirm samples still flow after the seek.
                let flowed = (0..1000).any(|_| source.next().is_some());
                println!("try_seek({target:?}) -> Ok, samples flow after seek: {flowed}");
            }
            Err(e) => println!("try_seek({target:?}) -> ERR: {e}"),
        }
    }

    // Seek back to the very start, then past the end.
    for target in [Duration::from_secs(0), Duration::from_secs(179)] {
        match source.try_seek(target) {
            Ok(()) => println!("try_seek({target:?}) -> Ok"),
            Err(e) => println!("try_seek({target:?}) -> ERR: {e}"),
        }
    }

    // Determinism check: two fresh linear decodes must agree exactly.
    let mut a = build_source(Path::new(&path))?;
    let mut b = build_source(Path::new(&path))?;
    let da: Vec<f32> = (0..8192).map(|_| a.next().unwrap_or(0.0)).collect();
    let db: Vec<f32> = (0..8192).map(|_| b.next().unwrap_or(0.0)).collect();
    println!("linear-vs-linear identical: {}", da == db);

    // Accuracy check: samples after seek(10s) must align with the samples
    // at the 10s mark from a plain linear decode. Exact equality is too
    // strict for opus — libopus's overlap-add blends the first packet
    // after a seek with stale decoder state — so compare by RMS error.
    let mut source = build_source(Path::new(&path))?;
    let mut seen = 0usize;
    let frame = source.sample_rate().get() as usize * source.channels().get() as usize;
    let mut linear_window = Vec::new();
    while let Some(s) = source.next() {
        if seen >= frame * 10 && linear_window.len() < 8192 {
            linear_window.push(s);
        }
        if linear_window.len() == 8192 {
            break;
        }
        seen += 1;
    }
    source
        .try_seek(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let seeked_window: Vec<f32> = (0..8192).filter_map(|_| source.next()).collect();
    let n = seeked_window.len().min(linear_window.len());
    let rms = (seeked_window[..n]
        .iter()
        .zip(&linear_window[..n])
        .map(|(a, b)| {
            let d = a - b;
            (d * d) as f64
        })
        .sum::<f64>()
        / n as f64)
        .sqrt();
    println!("seek(10s) vs linear RMS error: {rms:.6} ({} samples)", n);

    Ok(())
}
