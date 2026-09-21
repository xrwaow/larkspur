use std::env;
use std::fs::File;
use std::io::BufReader;
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStream, Sink};
use larkspur::datatypes::{SongMetadata, StreamingInfo, SongStatus, Config, Lyrics};

use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Tracks a rolling live bitrate from decoded packet sizes, the same
/// way the real playback module will.
struct BitrateTracker {
    window_bytes: usize,
    window_start: Instant,
    window_len: Duration,
}

impl BitrateTracker {
    fn new() -> Self {
        Self {
            window_bytes: 0,
            window_start: Instant::now(),
            window_len: Duration::from_secs(1),
        }
    }

    /// Feed a packet's byte length; returns Some(bps) when a full
    /// window has elapsed and the estimate refreshes.
    fn feed(&mut self, bytes: usize) -> Option<u32> {
        self.window_bytes += bytes;
        let elapsed = self.window_start.elapsed();
        if elapsed >= self.window_len {
            let bps = (self.window_bytes as f64 * 8.0 / elapsed.as_secs_f64()) as u32;
            self.window_bytes = 0;
            self.window_start = Instant::now();
            Some(bps)
        } else {
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    let path = env::args()
        .nth(1)
        .expect("usage: test_playback <path-to-song>");
    let path = std::path::PathBuf::from(path);

    anyhow::ensure!(
        Config::is_supported(&path),
        "unsupported file extension: {:?}",
        path.extension()
    );

    // --- 1. Schema: load tag metadata ---
    let metadata = SongMetadata::load(&path)?
        .ok_or_else(|| anyhow::anyhow!("no tag data found in {path:?}"))?;

    println!("=== Metadata (via lofty / SongMetadata schema) ===");
    println!("id:       {:016x}", metadata.id);
    println!("title:    {:?}", metadata.song_name);
    println!("artists:  {:?}", metadata.artists);
    println!("album:    {:?}", metadata.album_name);
    println!("nominal bitrate: {:?} kbps", metadata.nominal_bitrate);
    println!("duration: {:?}", metadata.duration);
    println!("lyrics:   {:?}", match &metadata.lyrics {
        Lyrics::None => "none".to_string(),
        Lyrics::Plain(s) => format!("plain ({} chars)", s.len()),
        Lyrics::Synced(lines) => format!("synced ({} lines)", lines.len()),
    });

    // --- 2. StreamingInfo: simulate the live state transition ---
    let mut streaming = StreamingInfo::default();
    streaming.current_song = Some(metadata.id);
    streaming.status = SongStatus::Playing;

    // --- 3. Symphonia: decode loop with live bitrate tracking ---
    let file = File::open(&path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
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
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no playable track found"))?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())?;

    let mut tracker = BitrateTracker::new();
    let mut packet_count = 0u32;

    println!("\n=== Decoding (headless, computing live bitrate) ===");
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(_)) => break, // EOF
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }

        if let Some(bps) = tracker.feed(packet.data.len()) {
            streaming.live_bitrate_bps = Some(bps);
            println!("live bitrate: {:.1} kbps", bps as f64 / 1000.0);
        }

        // Decode to validate the packet actually decodes cleanly —
        // real playback would push these samples to the audio sink.
        match decoder.decode(&packet) {
            Ok(_) => packet_count += 1,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // skip corrupt packet
            Err(e) => return Err(e.into()),
        }
    }
    println!("decoded {packet_count} packets successfully");

    // --- 4. rodio: actually play it (headless = no GUI, just audio out) ---
    println!("\n=== Playing via rodio ===");
    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;
    let source = Decoder::new(BufReader::new(File::open(&path)?))?;
    sink.append(source);
    sink.sleep_until_end();

    streaming.status = SongStatus::Paused;
    println!("\nplayback finished, status = {:?}", streaming.status);

    Ok(())
}
