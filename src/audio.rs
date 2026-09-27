use std::collections::VecDeque;
use std::fs::File;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::model::SongId;

/// How playback behaves when a track finishes.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum RepeatMode {
    /// Stop when the queue drains.
    #[default]
    Off,
    /// Restart the current track when it ends.
    Once,
    /// Wrap around to the first track when the queue drains.
    All,
}

impl RepeatMode {
    /// The next mode in the button's cycle: off → once → all → off.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Once,
            Self::Once => Self::All,
            Self::All => Self::Off,
        }
    }
}
use crate::opus::OpusSource;

type TrackSource = Box<dyn Source<Item = f32> + Send>;

/// How many mono samples the tap keeps — comfortably more than one FFT window
/// (~93 ms at 44.1 kHz), so the visualizer always has a full window to
/// transform.
const TAP_CAPACITY: usize = 8192;

/// Interleaved samples buffered before one tap flush, so the audio thread
/// takes the tap's lock once per ~10 ms rather than once per sample.
const TAP_FLUSH: usize = 1024;

/// A bounded ring of the most recent mono samples the audio thread has played.
///
/// This is what makes the visualizer real-time: instead of a precomputed
/// spectrogram indexed by playback position, the visualizer transforms the
/// samples that are actually coming out of the speakers. The audio thread only
/// ever appends here (in batches, under a short lock); the FFT runs on the UI
/// side.
pub struct SampleTap {
    inner: Mutex<TapRing>,
}

struct TapRing {
    /// Mono samples, oldest first, newest last.
    samples: VecDeque<f32>,
    sample_rate: u32,
}

impl SampleTap {
    fn new() -> Self {
        Self {
            inner: Mutex::new(TapRing {
                samples: VecDeque::with_capacity(TAP_CAPACITY),
                sample_rate: 44_100,
            }),
        }
    }

    /// The sample rate of the audio currently being tapped.
    pub fn sample_rate(&self) -> u32 {
        self.inner.lock().unwrap().sample_rate
    }

    /// Copy the most recent `n` mono samples (oldest first) into `out`, reusing
    /// its allocation. Fewer if the tap hasn't filled yet.
    pub fn copy_recent(&self, out: &mut Vec<f32>, n: usize) {
        let ring = self.inner.lock().unwrap();
        let len = ring.samples.len();
        let take = n.min(len);
        out.clear();
        out.extend(ring.samples.iter().skip(len - take).copied());
    }

    fn configure(&self, sample_rate: u32) {
        self.inner.lock().unwrap().sample_rate = sample_rate.max(1);
    }

    fn push_mono(&self, mono: &[f32]) {
        let mut ring = self.inner.lock().unwrap();
        for &sample in mono {
            if ring.samples.len() == TAP_CAPACITY {
                ring.samples.pop_front();
            }
            ring.samples.push_back(sample);
        }
    }

    fn clear(&self) {
        self.inner.lock().unwrap().samples.clear();
    }
}

/// Wraps a playback source, copying the samples it hands out into a
/// [`SampleTap`]. Samples are buffered and downmixed to mono in batches, so the
/// audio thread pays one short lock per [`TAP_FLUSH`] samples, not per sample.
struct TapSource<S> {
    inner: S,
    tap: Arc<SampleTap>,
    channels: usize,
    pending: Vec<f32>,
}

impl<S: Source<Item = f32>> TapSource<S> {
    fn new(inner: S, tap: Arc<SampleTap>) -> Self {
        let channels = inner.channels().get() as usize;
        tap.configure(inner.sample_rate().get());
        Self { inner, tap, channels, pending: Vec::with_capacity(TAP_FLUSH + 8) }
    }

    /// Downmix the buffered interleaved samples to mono and hand them to the
    /// tap.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let channels = self.channels.max(1);
        let mono: Vec<f32> = self
            .pending
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect();
        self.tap.push_mono(&mono);
        self.pending.clear();
    }
}

impl<S: Source<Item = f32>> Iterator for TapSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        self.pending.push(sample);
        if self.pending.len() >= TAP_FLUSH {
            self.flush();
        }
        Some(sample)
    }
}

impl<S: Source<Item = f32>> Source for TapSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        self.inner.channels()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // Drop the buffered tail and the tap's history: after a seek the old
        // samples are from the wrong place in the track.
        self.pending.clear();
        self.tap.clear();
        self.inner.try_seek(pos)
    }
}

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
    queue: Vec<(SongId, PathBuf)>,
    /// Bumped on every change to the queue's contents — the UI's cheap "did
    /// the queue move" check, so the queue view can skip snapshotting
    /// thousands of entries every frame.
    queue_generation: u64,
    current: Option<usize>,
    /// The loaded track's real stream duration, for seek clamping.
    pub duration: Duration,
    /// The live-audio ring the visualizer reads. Shared with every source this
    /// controller appends, so it always reflects whatever is playing.
    tap: Arc<SampleTap>,
    repeat: RepeatMode,
}

impl PlaybackController {
    pub fn new(queue: Vec<(SongId, PathBuf)>) -> anyhow::Result<Self> {
        let device_sink = DeviceSinkBuilder::open_default_sink()?;
        let player = Player::connect_new(device_sink.mixer());

        let mut this = Self {
            _device_sink: device_sink,
            player,
            queue,
            // The constructor's queue counts as change #1, so a queue view
            // built after this always snapshots it on its first render.
            queue_generation: 1,
            current: None,
            duration: Duration::ZERO,
            tap: Arc::new(SampleTap::new()),
            repeat: RepeatMode::Off,
        };

        // An empty queue is valid: the app may launch with only a directory to
        // scan, and a track gets queued once the library is up.
        if !this.queue.is_empty() {
            let parts = Self::track_parts(&this.queue[0].1)?;
            this.start_track(0, parts);
        }
        Ok(this)
    }

    /// Replace the whole queue and start playing at `start`.
    ///
    /// This is how a playlist becomes the play queue: the UI resolves the
    /// playlist's [`SongId`](crate::model::SongId)s to paths and hands them
    /// over — the controller never needs to know what a playlist is.
    pub fn set_queue(&mut self, queue: Vec<(SongId, PathBuf)>, start: usize) -> anyhow::Result<()> {
        anyhow::ensure!(!queue.is_empty(), "empty queue");
        let start = start.min(queue.len() - 1);
        let parts = Self::track_parts(&queue[start].1)?;
        self.queue = queue;
        self.queue_generation += 1;
        self.start_track(start, parts);
        Ok(())
    }

    /// Decode one track. The metadata is *not* read here — the library already
    /// holds it, keyed by the entry's [`SongId`], so the controller only needs
    /// the decoder and its real stream duration.
    fn track_parts(path: &Path) -> anyhow::Result<(TrackSource, Duration)> {
        let source = build_source(path)?;
        // Prefer the decoder's real stream duration over any tag value — tags
        // can drift from the actual audio, and seek clamping should match what
        // is actually seekable.
        let duration = source.total_duration().unwrap_or(Duration::ZERO);
        Ok((source, duration))
    }

    fn load_track(&mut self, index: usize) -> anyhow::Result<()> {
        let parts = Self::track_parts(&self.queue[index].1)?;
        self.start_track(index, parts);
        Ok(())
    }

    fn start_track(&mut self, index: usize, (source, duration): (TrackSource, Duration)) {
        // skip_one (not clear — that blocks until the current track
        // finishes!) tells the audio thread to drop the current source;
        // the new one is queued behind it and starts immediately after.
        if !self.player.empty() {
            self.player.skip_one();
        }
        // A fresh track's samples have nothing to do with the last one's, so
        // start the visualizer's ring clean.
        self.tap.clear();
        self.player.append(TapSource::new(source, self.tap.clone()));
        self.player.play();
        self.current = Some(index);
        self.duration = duration;
    }

    /// Called periodically by the UI: when the queue has drained (track
    /// finished), move to the next one. Returns true if a new track started.
    pub fn tick_advance(&mut self) -> bool {
        if !self.player.empty() {
            return false;
        }
        // Repeat-one grants exactly one extra play of the finished track,
        // then drops back to off — a failed reload falls through to the
        // normal advance so a broken file can't wedge playback in an
        // endless restart loop.
        if self.repeat == RepeatMode::Once {
            if let Some(i) = self.current {
                if let Ok(parts) = Self::track_parts(&self.queue[i].1) {
                    self.repeat = RepeatMode::Off;
                    self.start_track(i, parts);
                    return true;
                }
            }
        }
        self.advance()
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    /// Cycle the repeat mode for the transport's repeat button.
    pub fn cycle_repeat(&mut self) -> RepeatMode {
        self.repeat = self.repeat.next();
        self.repeat
    }

    /// Start the next track if there is one. Returns whether a new track
    /// actually started (a failed load returns `false`, not a phantom
    /// success).
    fn advance(&mut self) -> bool {
        let mut next = self.current.map_or(0, |i| i + 1);
        // Repeat-all wraps the queue instead of draining it.
        if next >= self.queue.len() && self.repeat == RepeatMode::All && !self.queue.is_empty() {
            next = 0;
        }
        if next >= self.queue.len() {
            return false;
        }
        match Self::track_parts(&self.queue[next].1) {
            Ok(parts) => {
                self.start_track(next, parts);
                true
            }
            Err(e) => {
                eprintln!("failed to load {:?}: {e}", self.queue[next].1);
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
            || (self.repeat == RepeatMode::All && !self.queue.is_empty())
    }

    /// True once the whole queue has played out — play/next are inert then.
    pub fn ended(&self) -> bool {
        self.player.empty()
    }

    /// The play queue and the index of the loaded track, for the UI's queue
    /// view. Read-only: the queue is replaced wholesale via [`set_queue`].
    pub fn queue(&self) -> (&[(SongId, PathBuf)], Option<usize>) {
        (&self.queue, self.current)
    }

    /// The queue's generation — bumped on every change to its contents.
    pub fn queue_generation(&self) -> u64 {
        self.queue_generation
    }

    /// Start playing the queue entry at `index` — how the UI's queue view
    /// jumps to a row.
    pub fn play_index(&mut self, index: usize) -> anyhow::Result<()> {
        anyhow::ensure!(index < self.queue.len(), "queue index {index} out of range");
        self.load_track(index)
    }

    /// Append `paths` to the end of the play queue — the song menu's "Add to
    /// Queue". Returns whether a track was started: with nothing loaded and
    /// nothing playing (an empty queue) the first appended one begins
    /// playing; a drained queue picks the appended tracks up on the next
    /// tick, like any track that finishes.
    pub fn add_to_queue(&mut self, entries: Vec<(SongId, PathBuf)>) -> anyhow::Result<bool> {
        anyhow::ensure!(!entries.is_empty(), "no tracks to queue");
        let first = self.queue.len();
        self.queue.extend(entries);
        self.queue_generation += 1;
        if self.current.is_none() && self.player.empty() {
            let parts = Self::track_parts(&self.queue[first].1)?;
            self.start_track(first, parts);
            return Ok(true);
        }
        Ok(false)
    }

    /// Drop every entry matching `paths` from the play queue — the song
    /// menu's "Remove from queue". Returns whether the loaded track changed
    /// (a new one started, or the player stopped), so the caller can reload
    /// the waveform.
    ///
    /// Removing the loaded track plays whatever took its slot (the next
    /// surviving entry); with nothing after it the queue lands in its
    /// played-out state on the last surviving entry, and an emptied queue
    /// stops playback.
    pub fn remove_from_queue(&mut self, ids: &[SongId]) -> bool {
        let before = self.queue.len();
        let Some(current) = self.current else {
            self.queue.retain(|(id, _)| !ids.contains(id));
            self.queue_generation += (self.queue.len() != before) as u64;
            return false;
        };
        let loaded_removed = ids.contains(&self.queue[current].0);
        let removed_before = self.queue[..current].iter().filter(|(id, _)| ids.contains(id)).count();
        self.queue.retain(|(id, _)| !ids.contains(id));
        if self.queue.len() == before {
            return false;
        }
        self.queue_generation += 1;
        if loaded_removed {
            if current < self.queue.len() {
                // The next surviving entry took the removed slot: play it.
                match Self::track_parts(&self.queue[current].1) {
                    Ok(parts) => self.start_track(current, parts),
                    Err(e) => {
                        eprintln!("failed to load {:?}: {e}", self.queue[current].1);
                        if !self.player.empty() {
                            self.player.skip_one();
                        }
                        self.current = Some(current);
                    }
                }
            } else {
                // Nothing after it: stop and land in the played-out state on
                // the last surviving entry — or forget the track entirely
                // when the queue emptied.
                if !self.player.empty() {
                    self.player.skip_one();
                }
                self.clear_tap();
                self.current =
                    if self.queue.is_empty() { None } else { Some(self.queue.len() - 1) };
            }
            true
        } else {
            // The loaded track survived: re-point at its shifted slot.
            self.current = Some(current - removed_before);
            false
        }
    }

    /// Move the queue entry at `from` to `to` — the queue panel's
    /// drag-to-reorder. The loaded track follows its entry to the new slot.
    pub fn move_entry(&mut self, from: usize, to: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            from < self.queue.len() && to < self.queue.len(),
            "queue index out of range"
        );
        if from == to {
            return Ok(());
        }
        let entry = self.queue.remove(from);
        self.queue.insert(to, entry);
        self.queue_generation += 1;
        // Re-point the loaded track at its entry's new slot; entries between
        // the two slots shift by one to make room.
        if let Some(current) = self.current {
            self.current = Some(if current == from {
                to
            } else if from < current && to >= current {
                current - 1
            } else if from > current && to <= current {
                current + 1
            } else {
                current
            });
        }
        Ok(())
    }

    /// Drop every queue entry — the queue panel's clear button. The loaded
    /// track keeps playing; the queue just won't advance past it, and
    /// anything appended afterwards starts from its first entry.
    pub fn clear_queue(&mut self) {
        self.queue_generation += (!self.queue.is_empty()) as u64;
        self.queue.clear();
        self.current = None;
    }

    /// Path of the currently-loaded track, for the UI to load the waveform
    /// (and later, cover art) from without touching the decoder.
    pub fn current_path(&self) -> Option<&Path> {
        self.current.and_then(|i| self.queue.get(i)).map(|(_, path)| path.as_path())
    }

    /// The loaded track's [`SongId`], if any — the UI resolves its metadata
    /// from the library with this.
    pub fn current_id(&self) -> Option<SongId> {
        self.current.and_then(|i| self.queue.get(i)).map(|(id, _)| *id)
    }

    /// The live-audio ring the visualizer transforms. Cheap: clones an `Arc`.
    pub fn sample_tap(&self) -> Arc<SampleTap> {
        self.tap.clone()
    }

    /// Drop the tap's history — used when the queue drains so the visualizer
    /// doesn't freeze on the last track's spectrum.
    pub fn clear_tap(&self) {
        self.tap.clear();
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
        // The tap's history is from the old position; drop it so the visualizer
        // doesn't briefly show the pre-seek spectrum.
        self.tap.clear();
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
