use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::model::SongMetadata;
use crate::opus::OpusSource;

type TrackSource = Box<dyn Source<Item = f32> + Send>;

/// Build the decode source for a track: rodio's `Decoder` covers
/// FLAC/MP3/WAV/Vorbis; Opus falls through to our symphonia+libopus source
/// (symphonia 0.5 ships no Opus decoder). Shared by the playback controller
/// and the dev `probe` binary's seek diagnostics.
pub fn build_source(path: &Path) -> anyhow::Result<TrackSource> {
    Ok(match Decoder::try_from(File::open(path)?) {
        Ok(decoder) => Box::new(decoder),
        Err(_) => Box::new(OpusSource::new(path)?),
    })
}

/// Coarse playback status for display and state projection.
///
/// Distinct from the live playback state the UI owns: this is the small,
/// display-oriented slice (`playing` / `paused` / `ended` / nothing selected)
/// that views match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongStatus {
    /// Nothing has been selected yet.
    NoSelection,
    Playing,
    Paused,
    /// The queue has played out — play/next are inert.
    Ended,
}

/// Owns the rodio player and device sink, the play queue, and the current
/// track's static metadata.
///
/// Deliberately does *not* own the waveform: that's loaded off-thread by
/// the UI layer (see the UI's `PlaybackState`), so a track change never
/// blocks on a full-file decode here.
pub struct PlaybackController {
    _device_sink: MixerDeviceSink, // must outlive the player or audio stops
    player: Player,
    queue: Vec<PathBuf>,
    current: Option<usize>,
    pub metadata: SongMetadata,
    pub duration: Duration,
}

impl PlaybackController {
    pub fn new(queue: Vec<PathBuf>) -> anyhow::Result<Self> {
        let device_sink = DeviceSinkBuilder::open_default_sink()?;
        let player = Player::connect_new(device_sink.mixer());

        let mut this = Self {
            _device_sink: device_sink,
            player,
            queue,
            current: None,
            metadata: SongMetadata::placeholder(),
            duration: Duration::ZERO,
        };

        // An empty queue is valid: the app may launch with only a directory to
        // scan, and a track gets queued once the library is up.
        if !this.queue.is_empty() {
            let parts = Self::track_parts(&this.queue[0])?;
            this.start_track(0, parts);
        }
        Ok(this)
    }

    /// Replace the whole queue and start playing at `start`.
    ///
    /// This is how a playlist becomes the play queue: the UI resolves the
    /// playlist's [`SongId`](crate::model::SongId)s to paths and hands them
    /// over — the controller never needs to know what a playlist is.
    pub fn set_queue(&mut self, queue: Vec<PathBuf>, start: usize) -> anyhow::Result<()> {
        anyhow::ensure!(!queue.is_empty(), "empty queue");
        let start = start.min(queue.len() - 1);
        let parts = Self::track_parts(&queue[start])?;
        self.queue = queue;
        self.start_track(start, parts);
        Ok(())
    }

    /// Decode + tag-read one track.
    fn track_parts(path: &Path) -> anyhow::Result<(TrackSource, SongMetadata, Duration)> {
        let metadata = SongMetadata::load(path)?;
        let source = build_source(path)?;
        // Prefer the decoder's real stream duration over the tag value —
        // tags can drift from the actual audio, and seek clamping should
        // match what's actually seekable.
        let duration = source.total_duration().unwrap_or(metadata.duration);
        Ok((source, metadata, duration))
    }

    fn load_track(&mut self, index: usize) -> anyhow::Result<()> {
        let parts = Self::track_parts(&self.queue[index])?;
        self.start_track(index, parts);
        Ok(())
    }

    fn start_track(
        &mut self,
        index: usize,
        (source, metadata, duration): (TrackSource, SongMetadata, Duration),
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
        self.duration = duration;
    }

    /// Called periodically by the UI: when the queue has drained (track
    /// finished), move to the next one. Returns true if a new track started.
    pub fn tick_advance(&mut self) -> bool {
        self.player.empty() && self.advance()
    }

    /// Start the next track if there is one. Returns whether a new track
    /// actually started (a failed load returns `false`, not a phantom
    /// success).
    fn advance(&mut self) -> bool {
        let next = self.current.map_or(0, |i| i + 1);
        if next >= self.queue.len() {
            return false;
        }
        match Self::track_parts(&self.queue[next]) {
            Ok(parts) => {
                self.start_track(next, parts);
                true
            }
            Err(e) => {
                eprintln!("failed to load {:?}: {e}", self.queue[next]);
                // Drop whatever is playing so we land in the ended state
                // instead of looping on a broken file every tick.
                if !self.player.empty() {
                    self.player.skip_one();
                }
                self.current = Some(next);
                false
            }
        }
    }

    pub fn next_track(&mut self) {
        self.advance();
    }

    pub fn prev_track(&mut self) {
        match self.current {
            Some(i) if i > 0 => {
                if let Err(e) = self.load_track(i - 1) {
                    eprintln!("failed to load previous track: {e}");
                }
            }
            // First track (or nothing loaded): restart it.
            _ => self.seek(Duration::ZERO),
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

    /// Path of the currently-loaded track, for the UI to load the waveform
    /// (and later, cover art) from without touching the decoder.
    pub fn current_path(&self) -> Option<&Path> {
        self.current
            .and_then(|i| self.queue.get(i))
            .map(PathBuf::as_path)
    }

    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    pub fn is_playing(&self) -> bool {
        !self.player.is_paused() && !self.player.empty()
    }

    /// Coarse playback status for display / state projection.
    pub fn status(&self) -> SongStatus {
        if self.current.is_none() {
            SongStatus::NoSelection
        } else if self.ended() {
            SongStatus::Ended
        } else if self.player.is_paused() {
            SongStatus::Paused
        } else {
            SongStatus::Playing
        }
    }

    pub fn toggle_play_pause(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    pub fn seek(&mut self, pos: Duration) {
        // A drained queue has no source left to seek into — try_seek would just
        // fail and the click would do nothing. Reload the current track at the
        // requested position instead, so clicking the bar after the song ends
        // plays from there without going through prev first.
        if self.player.empty() {
            let Some(current) = self.current else { return };
            if let Err(e) = self.load_track(current) {
                eprintln!("failed to reload {:?} for seek: {e}", self.queue[current]);
                return;
            }
        }
        // Don't swallow seek failures — a silent failure looks exactly
        // like "the arrow keys / seek bar do nothing".
        if let Err(e) = self.player.try_seek(pos.min(self.duration)) {
            eprintln!("seek to {pos:?} failed: {e}");
        }
    }

    pub fn seek_relative(&mut self, delta_secs: i64) {
        let current = self.position().as_secs_f64();
        self.seek(Duration::from_secs_f64((current + delta_secs as f64).max(0.0)));
    }
}
