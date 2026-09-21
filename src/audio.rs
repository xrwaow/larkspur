use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::datatypes::SongMetadata;
use crate::opus::OpusSource;
use crate::waveform::compute_waveform;

type TrackSource = Box<dyn Source<Item = f32> + Send>;

pub struct PlaybackController {
    _device_sink: MixerDeviceSink, // must outlive the player or audio stops
    player: Player,
    queue: Vec<PathBuf>,
    current: Option<usize>,
    pub metadata: SongMetadata,
    pub waveform: Vec<f32>,
    pub duration: Duration,
}

impl PlaybackController {
    pub fn new(queue: Vec<PathBuf>) -> anyhow::Result<Self> {
        anyhow::ensure!(!queue.is_empty(), "no songs given");

        let device_sink = DeviceSinkBuilder::open_default_sink()?;
        let player = Player::connect_new(device_sink.mixer());

        let (source, metadata, waveform, duration) = Self::track_parts(&queue[0])?;
        player.append(source);

        Ok(Self {
            _device_sink: device_sink,
            player,
            queue,
            current: Some(0),
            metadata,
            waveform,
            duration,
        })
    }

    /// Decode + tag-read one track. rodio's Decoder covers FLAC/MP3/WAV/
    /// Vorbis; Opus falls through to our symphonia+libopus source because
    /// symphonia 0.5 has no Opus decoder.
    fn track_parts(path: &Path) -> anyhow::Result<(TrackSource, SongMetadata, Vec<f32>, Duration)> {
        let metadata = SongMetadata::load(path)?
            .ok_or_else(|| anyhow::anyhow!("no tag data found in {path:?}"))?;
        let waveform = compute_waveform(path, 120)?;
        let source: TrackSource = match Decoder::try_from(File::open(path)?) {
            Ok(decoder) => Box::new(decoder),
            Err(_) => Box::new(OpusSource::new(path)?),
        };
        // Prefer the decoder's real stream duration over the tag value —
        // tags can drift from the actual audio, and seek clamping should
        // match what's actually seekable.
        let duration = source.total_duration().unwrap_or(metadata.duration);
        Ok((source, metadata, waveform, duration))
    }

    fn load_track(&mut self, index: usize) -> anyhow::Result<()> {
        let parts = Self::track_parts(&self.queue[index])?;
        self.start_track(index, parts);
        Ok(())
    }

    fn start_track(
        &mut self,
        index: usize,
        (source, metadata, waveform, duration): (TrackSource, SongMetadata, Vec<f32>, Duration),
    ) {
        // skip_one (not clear — that blocks until the current track
        // finishes!) tells the audio thread to drop the current source;
        // the new one is queued behind it and starts immediately after.
        if !self.player.empty() {
            self.player.skip_one();
        }
        self.player.append(source);
        self.player.play();
        self.current = Some(index);
        self.metadata = metadata;
        self.waveform = waveform;
        self.duration = duration;
    }

    /// Called periodically by the UI: when the queue has drained (track
    /// finished), move to the next one. Returns true if a new track started.
    pub fn tick_advance(&mut self) -> bool {
        self.player.empty() && self.advance()
    }

    /// Start the next track if there is one.
    fn advance(&mut self) -> bool {
        let next = self.current.map_or(0, |i| i + 1);
        if next >= self.queue.len() {
            return false;
        }
        match Self::track_parts(&self.queue[next]) {
            Ok(parts) => self.start_track(next, parts),
            Err(e) => {
                eprintln!("failed to load {:?}: {e}", self.queue[next]);
                // Drop whatever is playing so we land in the ended state
                // instead of looping on a broken file every tick.
                if !self.player.empty() {
                    self.player.skip_one();
                }
                self.current = Some(next);
            }
        }
        true
    }

    pub fn next_track(&mut self) {
        self.advance();
    }

    pub fn prev_track(&mut self) {
        if self.current.is_some_and(|i| i > 0) {
            if let Err(e) = self.load_track(self.current.unwrap() - 1) {
                eprintln!("failed to load previous track: {e}");
            }
        } else {
            // First track: restart it.
            self.seek(Duration::ZERO);
        }
    }

    /// Prev can always do something unless we're at the very start of the
    /// first track — go back one track, or restart this one.
    pub fn can_prev(&self) -> bool {
        self.current.is_some_and(|i| i > 0) || self.position() > Duration::ZERO
    }

    pub fn can_next(&self) -> bool {
        self.current.is_some_and(|i| i + 1 < self.queue.len())
    }

    /// True once the whole queue has played out — play/next are inert then.
    pub fn ended(&self) -> bool {
        self.player.empty()
    }

    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    pub fn is_playing(&self) -> bool {
        !self.player.is_paused() && !self.player.empty()
    }

    pub fn toggle_play_pause(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    pub fn seek(&self, pos: Duration) {
        // Don't swallow seek failures — a silent failure looks exactly
        // like "the arrow keys / seek bar do nothing".
        if let Err(e) = self.player.try_seek(pos.min(self.duration)) {
            eprintln!("seek to {pos:?} failed: {e}");
        }
    }

    pub fn seek_relative(&self, delta_secs: i64) {
        let current = self.position().as_secs_f64();
        self.seek(Duration::from_secs_f64((current + delta_secs as f64).max(0.0)));
    }
}
