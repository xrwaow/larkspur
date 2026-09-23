//! Opus playback for rodio via Symphonia's Ogg demuxer + libopus.
//!
//! Symphonia 0.5 demuxes Ogg/Opus (exposes the packets, applies pre-skip
//! and end trims via `Packet::trim_start/trim_end`) but ships no Opus
//! decoder, so `rodio::Decoder` can't play Opus files. This source bridges
//! the gap: Symphonia reads packets, libopus decodes them to f32.

use std::path::Path;
use std::time::Duration;

use rodio::source::SeekError;
use rodio::Source;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, SeekedTo};
use symphonia::core::units::Time;

use crate::decode::{open_track, PacketDecoder, OPUS_SAMPLE_RATE};

pub struct OpusSource {
    format: Box<dyn FormatReader>,
    decoder: PacketDecoder,
    track_id: u32,
    channels: usize,
    /// Interleaved samples of the current packet not yet handed out.
    buffer: Vec<f32>,
    offset: usize,
    duration: Option<Time>,
}

impl OpusSource {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        let opened = open_track(path, true)?;
        let channels = opened.channels();
        let decoder = PacketDecoder::new(&opened.codec_params, channels)?;
        anyhow::ensure!(decoder.is_opus(), "no Opus track in {path:?}");

        let duration = opened
            .codec_params
            .time_base
            .zip(opened.codec_params.n_frames)
            .map(|(b, f)| b.calc_time(f));

        let mut this = Self {
            format: opened.format,
            decoder,
            track_id: opened.track_id,
            channels,
            buffer: Vec::new(),
            offset: 0,
            duration,
        };
        this.refill();
        Ok(this)
    }

    /// Decode the next packet into `buffer`; false at end of stream.
    fn refill(&mut self) -> bool {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(_) => return false,
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let Some(decoded) = self.decoder.decode(&packet) else { continue };
            // Symphonia trims pre-skip/end frames via ts/dur; the decoded
            // packet still contains them, so start after trim_start and
            // emit exactly `dur` frames. saturating_sub guards against a
            // pathological trim_start > decoded rather than underflowing.
            let frames_total = decoded.len() / self.channels;
            let trim_start = packet.trim_start as usize;
            let available = frames_total.saturating_sub(trim_start);
            let frames = (packet.dur() as usize).min(available);
            self.buffer = slice_frames(decoded, trim_start, 0, frames, self.channels);
            self.offset = 0;
            return true;
        }
    }

    fn seek_error(e: impl std::fmt::Display) -> SeekError {
        SeekError::Other(std::sync::Arc::new(SeekFailure(e.to_string())))
    }
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.offset >= self.buffer.len() && !self.refill() {
            return None;
        }
        let sample = self.buffer[self.offset];
        self.offset += 1;
        Some(sample)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len() - self.offset)
    }

    fn channels(&self) -> std::num::NonZero<u16> {
        std::num::NonZero::new(self.channels as u16).unwrap()
    }

    fn sample_rate(&self) -> std::num::NonZero<u32> {
        std::num::NonZero::new(OPUS_SAMPLE_RATE).unwrap()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration.map(|t| Duration::from_secs_f64(t.seconds as f64 + t.frac))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // Clamp to the stream's end if it's known — Symphonia errors on
        // out-of-range timestamps.
        let pos = self
            .total_duration()
            .map(|dur| pos.min(dur))
            .unwrap_or(pos);

        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time { time: pos.as_secs_f64().into(), track_id: None },
            )
            .map_err(Self::seek_error)?;
        self.refine_after_seek(seeked)?;
        Ok(())
    }
}

impl OpusSource {
    /// After a seek, decode packets until one covers the requested
    /// timestamp, dropping the head of it so playback resumes exactly
    /// at the sought position.
    fn refine_after_seek(&mut self, seeked: SeekedTo) -> Result<(), SeekError> {
        let required = seeked.required_ts;
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                // Seeking to (or past) the very end: land on an empty
                // buffer; playback ends immediately and the queue advances.
                Err(SymphoniaError::IoError(_)) => {
                    self.buffer.clear();
                    self.offset = 0;
                    return Ok(());
                }
                Err(e) => return Err(Self::seek_error(format!("seek failed: {e}"))),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let Some(decoded) = self.decoder.decode(&packet) else { continue };

            let pkt_start = packet.ts();
            let pkt_dur = packet.dur();
            let skip_frames = required.saturating_sub(pkt_start).min(pkt_dur) as usize;
            let frames = pkt_dur as usize - skip_frames;
            self.buffer = slice_frames(
                decoded,
                packet.trim_start as usize,
                skip_frames,
                frames,
                self.channels,
            );
            self.offset = 0;
            if !self.buffer.is_empty() {
                return Ok(());
            }
            // Seek landed exactly on a packet boundary; try the next one.
        }
    }
}

/// Slice `frames`-worth of samples starting at frame `trim_start + skip`,
/// where each frame is `channels` interleaved samples. Both the gapless
/// pre-skip/trim in `refill` and the exact-position refinement after a seek
/// need this same indexed window out of a decoded packet; out-of-range
/// indices yield an empty slice rather than panicking.
fn slice_frames(
    decoded: &[f32],
    trim_start: usize,
    skip: usize,
    frames: usize,
    channels: usize,
) -> Vec<f32> {
    let start = (trim_start + skip) * channels;
    let end = (trim_start + skip + frames) * channels;
    decoded.get(start..end).unwrap_or(&[]).to_vec()
}

#[derive(Debug)]
struct SeekFailure(String);

impl std::fmt::Display for SeekFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SeekFailure {}
