//! Opus playback for rodio via symphonia's Ogg demuxer + libopus.
//!
//! symphonia 0.5 demuxes Ogg/Opus (exposes the packets, applies pre-skip
//! and end trims via `Packet::trim_start/trim_end`) but ships no Opus
//! decoder, so `rodio::Decoder` can't play Opus files. This source bridges
//! the gap: symphonia reads packets, libopus decodes them to f32.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use opus::{Channels, Decoder as OpusCodec};
use rodio::source::SeekError;
use rodio::Source;
use symphonia::core::codecs::CODEC_TYPE_OPUS;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, SeekedTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use symphonia::default::get_probe;

/// Maximum samples per channel in one Opus packet (120 ms @ 48 kHz).
const MAX_PACKET_FRAMES: usize = 5760;

/// A `MediaSource` over a plain file that reports its real length —
/// symphonia's Ogg seek needs it for the page binary search.
struct SeekableFile {
    inner: File,
    len: Option<u64>,
}

impl MediaSource for SeekableFile {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

impl Read for SeekableFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for SeekableFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

pub struct OpusSource {
    format: Box<dyn FormatReader>,
    codec: OpusCodec,
    track_id: u32,
    channels: usize,
    /// Interleaved samples of the current packet not yet handed out.
    buffer: Vec<f32>,
    offset: usize,
    scratch: Vec<f32>,
    duration: Option<Time>,
}

impl OpusSource {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata().ok().map(|m| m.len());
        let mss = MediaSourceStream::new(
            Box::new(SeekableFile { inner: file, len }) as Box<dyn MediaSource + Send + Sync>,
            Default::default(),
        );

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = get_probe().format(
            &hint,
            mss,
            // Gapless trimming makes the demuxer apply Opus pre-skip/end
            // padding via Packet::trim_start/trim_end, which refill/refine
            // already honor (same behavior as rodio's own decoder).
            &FormatOptions { enable_gapless: true, ..Default::default() },
            &MetadataOptions::default(),
        )?;

        let track = probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec == CODEC_TYPE_OPUS)
            .ok_or_else(|| anyhow::anyhow!("no Opus track in {path:?}"))?;
        let track_id = track.id;

        let params = &track.codec_params;
        let channels = params.channels.map(|c| c.count()).unwrap_or(2);
        let sample_rate = params.sample_rate.unwrap_or(48_000);
        let duration = params.time_base.zip(params.n_frames).map(|(b, f)| b.calc_time(f));

        let codec = OpusCodec::new(
            sample_rate,
            if channels == 1 { Channels::Mono } else { Channels::Stereo },
        )?;

        let mut this = Self {
            format: probed.format,
            codec,
            track_id,
            channels,
            buffer: Vec::new(),
            offset: 0,
            scratch: vec![0.0; MAX_PACKET_FRAMES * channels],
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
            let decoded = match self.codec.decode_float(&packet.data, &mut self.scratch, false) {
                Ok(n) => n,
                // Corrupt packet: skip and try the next one.
                Err(_) => continue,
            };
            // symphonia trims pre-skip/end frames via ts/dur; the decoded
            // packet still contains them, so start after trim_start and
            // emit exactly `dur` frames.
            let start = packet.trim_start as usize * self.channels;
            let frames = packet.dur().min((decoded - packet.trim_start as usize) as u64) as usize;
            let end = start + frames * self.channels;
            self.buffer = self.scratch[start..end.min(self.scratch.len())].to_vec();
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
        std::num::NonZero::new(48_000).unwrap()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration.map(|t| Duration::from_secs_f64(t.seconds as f64 + t.frac))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // Clamp to the stream's end if it's known — symphonia errors on
        // out-of-range timestamps.
        let pos = self
            .total_duration()
            .map(|dur| pos.min(dur))
            .unwrap_or(pos);

        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time: pos.as_secs_f64().into(), track_id: None })
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
            if self.codec.decode_float(&packet.data, &mut self.scratch, false).is_err() {
                continue;
            }

            let ch = self.channels;
            let pkt_start = packet.ts();
            let pkt_dur = packet.dur();
            let skip_frames = required.saturating_sub(pkt_start).min(pkt_dur) as usize;
            let start = packet.trim_start as usize + skip_frames;
            let frames = pkt_dur as usize - skip_frames;
            let end = (start + frames) * ch;
            self.buffer = self.scratch[start * ch..end.min(self.scratch.len())].to_vec();
            self.offset = 0;
            if !self.buffer.is_empty() {
                return Ok(());
            }
            // Seek landed exactly on a packet boundary; try the next one.
        }
    }
}

#[derive(Debug)]
struct SeekFailure(String);

impl std::fmt::Display for SeekFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SeekFailure {}
