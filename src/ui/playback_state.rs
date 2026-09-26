//! The shared, observable playback state.
//!
//! Exactly one `Entity<PlaybackState>` exists per window. Views that care
//! about playback hold a clone of it and **observe** it, re-rendering only
//! when it notifies — rather than each running its own poll loop. Adding a
//! new playback-reading module (bitrate meter, lyrics, queue) is therefore
//! a subscription, not another timer.
//!
//! The state also owns the ticker: a single background loop advances the
//! queue when a track drains and notifies on real change. It runs at ~60 Hz
//! while playing and idles at ~4 Hz otherwise, so a paused or finished
//! player costs almost nothing.
//!
//! The waveform is generated **progressively**, not in one whole-file pass:
//! a background thread decodes the track from the start in chunks, one after
//! another, and publishes each partial waveform into a shared slot the ticker
//! picks up. The transport's bars therefore fill in as the chunks land. The
//! visualizer is separate and genuinely real-time — it transforms the samples
//! the audio thread is playing (see [`PlaybackState::sample_tap`]).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::Context;

use crate::analysis::{TrackAnalyzer, Waveform};
use crate::audio::{PlaybackController, RepeatMode, SampleTap, SongStatus};
use crate::bitrate::BitrateProfile;
use crate::model::{InputAction, SongMetadata};
use crate::ui::animation::SLOW_FPS;

/// How many bars a waveform is reduced to.
const WAVEFORM_BUCKETS: usize = 240;

/// Media-time width of one live-bitrate bucket, and how often the displayed
/// value refreshes.
const BITRATE_BUCKET: Duration = Duration::from_secs(1);

/// Media time decoded per analyzer step — one whole chunk at a time, not a
/// trickle. The chunks are generated back to back, so the waveform fills in as
/// fast as the decoder can read it.
const WAVEFORM_CHUNK: Duration = Duration::from_secs(256);

/// How far the rendered position may run ahead of rodio's last sample while
/// smoothing between its updates.
const MAX_EXTRAPOLATION: Duration = Duration::from_millis(256);

/// How often the state samples the player. Fast enough to notice a track
/// draining promptly; the *notify* rate is separate — the transport re-renders
/// on the slow clock ([`SLOW_FPS`]), not every sample.
const TICK: Duration = Duration::from_millis(128);

/// The waveform chunks + bitrate the background analyzer publishes, shared
/// with the UI through an `Arc<Mutex<..>>` rather than a channel.
///
/// The analyzer owns a Symphonia format reader, which isn't `Send`, so it runs
/// on its own thread and can't hand results back through an executor task. It
/// pushes each decoded chunk onto `chunks` and never waits; the transport view
/// pops them one at a time and animates each in, so the display is decoupled
/// from the decode.
struct AnalysisSlot {
    /// Chunks produced so far, oldest first.
    chunks: VecDeque<Waveform>,
    bitrate: BitrateProfile,
    /// Bumped on every publish, so a tick only notifies on real change.
    revision: u64,
}

impl AnalysisSlot {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            bitrate: BitrateProfile::empty(BITRATE_BUCKET),
            revision: 0,
        }
    }
}

pub struct PlaybackState {
    controller: PlaybackController,
    /// Shared with the background analyzer thread.
    analysis: Arc<Mutex<AnalysisSlot>>,
    /// The revision the UI last rendered, so a tick only notifies on change.
    analysis_revision: u64,
    /// Bumped on every track change; the analyzer thread stops once it no
    /// longer matches the value it started with.
    generation: Arc<AtomicU64>,
    /// Last values we rendered, so a tick only notifies on real change.
    /// The position is quantized to the slow clock, so the transport re-renders
    /// at [`SLOW_FPS`] rather than every sample.
    last_coarse: i64,
    last_status: SongStatus,
    /// The live bitrate currently shown, refreshed every [`BITRATE_BUCKET`].
    live_bitrate_bps: Option<u32>,
    /// Media time of the last live-bitrate refresh.
    last_bitrate_at: Duration,
    /// The position the UI renders — rodio's position, smoothed with wall
    /// clock time between rodio's (coarser) updates so the waveform and the
    /// clock advance smoothly and in lockstep.
    display_position: Duration,
    /// Wall-clock anchor for that smoothing: when we last sampled rodio, and
    /// what it reported.
    anchor: Option<(Instant, Duration)>,
    /// Whether the queue panel is swapped in over the tabs. Lives here (not in
    /// a view) because both the transport's toggle button and the tab
    /// container that hosts the panel observe this state.
    queue_open: bool,
}

impl PlaybackState {
    pub fn new(controller: PlaybackController, cx: &mut Context<Self>) -> Self {
        let status = controller.status();
        let position = controller.position();
        let mut this = Self {
            controller,
            analysis: Arc::new(Mutex::new(AnalysisSlot::new())),
            analysis_revision: 0,
            generation: Arc::new(AtomicU64::new(0)),
            last_coarse: -1,
            last_status: status,
            live_bitrate_bps: None,
            last_bitrate_at: Duration::ZERO,
            display_position: position,
            anchor: None,
            queue_open: false,
        };
        this.reload_track();
        this.start_ticker(cx);
        this
    }

    // --- read-only accessors: what views render from -------------------

    pub fn metadata(&self) -> &SongMetadata {
        &self.controller.metadata
    }

    pub fn duration(&self) -> Duration {
        self.controller.duration
    }

    pub fn position(&self) -> Duration {
        self.display_position
    }

    /// The position projected to *this instant*, rather than to the last tick.
    ///
    /// [`position`](Self::position) is only refreshed when the ticker runs, so
    /// it can be up to a tick stale. A consumer on its own clock — the lyrics
    /// panel timing a scroll to land on a line's timestamp — needs finer
    /// resolution than that.
    pub fn live_position(&self) -> Duration {
        if !self.controller.is_playing() {
            return self.display_position;
        }
        match self.anchor {
            Some((at, base)) => {
                let elapsed =
                    Instant::now().saturating_duration_since(at).min(MAX_EXTRAPOLATION);
                base + elapsed
            }
            None => self.display_position,
        }
    }

    /// The next waveform chunk to animate in, if the analyzer has produced
    /// one. The transport view pops these in order.
    pub fn take_chunk(&self) -> Option<Waveform> {
        self.analysis.lock().unwrap().chunks.pop_front()
    }

    /// A counter bumped on every track change, so a view can reset its
    /// progressive-waveform animation when the track changes.
    pub fn waveform_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The live-audio ring the visualizer transforms. Cheap: clones an `Arc`.
    pub fn sample_tap(&self) -> Arc<SampleTap> {
        self.controller.sample_tap()
    }

    /// The current track's live bitrate in bits per second, refreshed every
    /// [`BITRATE_BUCKET`] of media time while playing. `None` when nothing is
    /// playing or the analysis isn't ready yet.
    pub fn live_bitrate(&self) -> Option<u32> {
        self.live_bitrate_bps
    }

    pub fn status(&self) -> SongStatus {
        self.controller.status()
    }

    pub fn is_playing(&self) -> bool {
        self.controller.is_playing()
    }

    pub fn ended(&self) -> bool {
        self.controller.ended()
    }

    pub fn can_prev(&self) -> bool {
        self.controller.can_prev()
    }

    pub fn can_next(&self) -> bool {
        self.controller.can_next()
    }

    /// The current repeat mode, for the transport's repeat button.
    pub fn repeat_mode(&self) -> RepeatMode {
        self.controller.repeat_mode()
    }

    /// Cycle off → once → all — the transport's repeat button.
    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.controller.cycle_repeat();
        cx.notify();
    }

    /// Whether the queue panel is swapped in over the tabs.
    pub fn queue_open(&self) -> bool {
        self.queue_open
    }

    /// The play queue and the index of the loaded entry, for the queue view.
    pub fn queue(&self) -> (&[PathBuf], Option<usize>) {
        self.controller.queue()
    }

    /// The queue's generation — bumped on every change to its contents. The
    /// queue view's cheap per-frame check, so it only snapshots the paths
    /// (and re-sums the total time) when the queue actually moved.
    pub fn queue_generation(&self) -> u64 {
        self.controller.queue_generation()
    }

    // --- actions -------------------------------------------------------

    pub fn toggle_play_pause(&mut self) {
        self.controller.toggle_play_pause();
    }

    pub fn next_track(&mut self) {
        self.controller.next_track();
    }

    pub fn prev_track(&mut self) {
        self.controller.prev_track();
    }

    pub fn seek_fraction(&mut self, fraction: f32) {
        let target = self.controller.duration.mul_f32(fraction.clamp(0.0, 1.0));
        self.controller.seek(target);
    }

    /// Seek to an absolute position. Used by the lyrics panel, where clicking
    /// a line jumps to that line's timestamp.
    pub fn seek_to(&mut self, position: Duration) {
        self.controller.seek(position);
    }

    pub fn seek_relative(&mut self, secs: i64) {
        self.controller.seek_relative(secs);
    }

    /// Show or hide the queue panel.
    pub fn set_queue_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.queue_open != open {
            self.queue_open = open;
            cx.notify();
        }
    }

    /// Toggle the queue panel — the transport's queue button.
    pub fn toggle_queue(&mut self, cx: &mut Context<Self>) {
        self.set_queue_open(!self.queue_open, cx);
    }

    /// Jump to the queue entry at `index` — a click in the queue view.
    pub fn play_queue_index(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.controller.play_index(index) {
            Ok(()) => {
                self.reload_track();
                cx.notify();
            }
            Err(e) => eprintln!("failed to jump to queue entry {index}: {e}"),
        }
    }

    /// Append `paths` to the play queue — the song menu's "Add to Queue".
    pub fn add_to_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        match self.controller.add_to_queue(paths) {
            Ok(started) => {
                // Only a freshly started track needs a waveform reload;
                // appending behind a playing one leaves it untouched.
                if started {
                    self.reload_track();
                }
                cx.notify();
            }
            Err(e) => eprintln!("failed to add to queue: {e}"),
        }
    }

    /// Drop every queue entry matching `paths` — the song menu's "Remove from
    /// queue". Reloads the waveform when the loaded track changed (a new one
    /// started, or playback stopped).
    pub fn remove_from_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        if self.controller.remove_from_queue(&paths) {
            self.reload_track();
        }
        cx.notify();
    }

    /// Drop every queue entry — the queue panel's clear button. The loaded
    /// track keeps playing.
    pub fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.controller.clear_queue();
        cx.notify();
    }

    /// Move the queue entry at `from` to `to` — the queue panel's
    /// drag-to-reorder. The loaded track follows its entry, so no waveform
    /// reload is needed.
    pub fn move_queue_entry(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        match self.controller.move_entry(from, to) {
            Ok(()) => cx.notify(),
            Err(e) => eprintln!("failed to reorder queue: {e}"),
        }
    }

    /// Replace the play queue with `paths`, starting at `start`.
    ///
    /// Used to turn a playlist into the queue: the caller resolves the
    /// playlist's songs to paths, so this stays playlist-agnostic. Re-loads
    /// the waveform for whatever track ends up current.
    pub fn play_paths(&mut self, paths: Vec<PathBuf>, start: usize, _cx: &mut Context<Self>) -> bool {
        if paths.is_empty() {
            return false;
        }
        match self.controller.set_queue(paths, start) {
            Ok(()) => {
                self.reload_track();
                true
            }
            Err(e) => {
                eprintln!("failed to start playlist: {e}");
                false
            }
        }
    }

    /// Apply a backend-agnostic [`InputAction`].
    ///
    /// Returns whether it did anything, so the caller can skip a pointless
    /// re-render. Actions that don't apply (e.g. play/pause once the queue
    /// has ended) report `false` rather than silently toggling.
    pub fn apply(&mut self, action: InputAction) -> bool {
        match action {
            InputAction::TogglePause => {
                if self.controller.ended() {
                    false
                } else {
                    self.controller.toggle_play_pause();
                    true
                }
            }
            InputAction::NextTrack => {
                let could = self.controller.can_next();
                self.controller.next_track();
                could
            }
            InputAction::PrevTrack => {
                let could = self.controller.can_prev();
                self.controller.prev_track();
                could
            }
            InputAction::SeekForward => {
                self.controller.seek_relative(5);
                true
            }
            InputAction::SeekBackward => {
                self.controller.seek_relative(-5);
                true
            }
            // Volume / focus / quit are not wired to playback yet.
            _ => false,
        }
    }

    // --- internals -----------------------------------------------------

    /// Advance the queue if it drained, and report whether anything the UI
    /// renders has changed since the last tick.
    fn tick(&mut self, _cx: &mut Context<Self>) -> bool {
        let mut changed = false;

        if self.controller.tick_advance() {
            self.reload_track();
            changed = true;
        }

        let position = self.display_position_now();
        self.display_position = position;

        // The queue has drained: drop the tap so the visualizer doesn't freeze
        // on the last track's spectrum.
        if self.controller.ended() {
            self.controller.clear_tap();
        }

        // Report the position only once per slow tick, so a playing track
        // re-renders the transport at `SLOW_FPS` instead of every sample.
        let coarse = (position.as_secs_f32() * SLOW_FPS) as i64;
        if coarse != self.last_coarse {
            self.last_coarse = coarse;
            changed = true;
        }

        // Pick up whatever the analyzer has published since the last tick.
        let (revision, bitrate_at) = {
            let slot = self.analysis.lock().unwrap();
            (slot.revision, slot.bitrate.at(position))
        };
        if revision != self.analysis_revision {
            self.analysis_revision = revision;
            changed = true;
        }

        // Refresh the live bitrate every BITRATE_BUCKET of media time (and
        // immediately after a seek, where the position jumps backwards).
        if self.controller.is_playing()
            && position.abs_diff(self.last_bitrate_at) >= BITRATE_BUCKET
        {
            self.last_bitrate_at = position;
            if bitrate_at != self.live_bitrate_bps {
                self.live_bitrate_bps = bitrate_at;
                changed = true;
            }
        }

        let status = self.controller.status();
        if status != self.last_status {
            self.last_status = status;
            changed = true;
        }

        changed
    }

    fn start_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;

                if this
                    .update(cx, |state, cx| {
                        if state.tick(cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// The position to render.
    ///
    /// rodio's position can arrive in coarse steps (one per decoded buffer),
    /// which makes the waveform jump while the clock looks smooth. Extrapolate
    /// it with wall-clock time between samples so both advance together at the
    /// ticker's rate. The extrapolation is bounded, so a stalled player can't
    /// run the display away from the audio.
    fn display_position_now(&mut self) -> Duration {
        let raw = self.controller.position();
        if !self.controller.is_playing() {
            self.anchor = None;
            return raw;
        }

        let now = Instant::now();
        match self.anchor {
            Some((at, base)) if raw == base => {
                let elapsed = now.saturating_duration_since(at).min(MAX_EXTRAPOLATION);
                base + elapsed
            }
            _ => {
                self.anchor = Some((now, raw));
                raw
            }
        }
    }

    /// Start generating the current track's waveform from the start.
    ///
    /// A background thread decodes the track in chunks, one after another,
    /// publishing each partial waveform into the shared slot as it lands. The
    /// previous track's thread is stopped by bumping the generation counter,
    /// and the slot is replaced so a late write from the old thread can't
    /// clobber the new track's bars.
    fn reload_track(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.analysis = Arc::new(Mutex::new(AnalysisSlot::new()));
        self.analysis_revision = 0;
        self.live_bitrate_bps = None;
        self.last_bitrate_at = Duration::ZERO;
        self.last_coarse = -1;
        self.anchor = None;
        self.display_position = self.controller.position();

        let Some(path) = self.controller.current_path().map(PathBuf::from) else {
            return;
        };
        let duration = self.controller.duration;
        let analysis = self.analysis.clone();
        let generation_flag = self.generation.clone();

        // The analyzer owns a Symphonia format reader, which isn't `Send`, so it
        // can't be moved onto the executor's pool after construction. It runs on
        // its own thread instead, created and used entirely inside this closure,
        // and publishes into the shared slot.
        std::thread::spawn(move || {
            let mut analyzer =
                match TrackAnalyzer::new(&path, WAVEFORM_BUCKETS, BITRATE_BUCKET, Some(duration)) {
                    Ok(analyzer) => analyzer,
                    // Nothing readable: leave the slot empty rather than
                    // flashing a placeholder.
                    Err(_) => return,
                };
            loop {
                if generation_flag.load(Ordering::SeqCst) != generation {
                    break;
                }
                // Decode the next chunk and publish it. No waiting on the
                // playhead: the chunks are generated back to back, so the bars
                // fill in as fast as the decoder can read them.
                if analyzer.advance(WAVEFORM_CHUNK.as_secs_f64()).is_err() {
                    break;
                }
                let mut slot = analysis.lock().unwrap();
                slot.chunks.push_back(analyzer.waveform());
                slot.bitrate = analyzer.bitrate();
                slot.revision += 1;
                if analyzer.finished() {
                    break;
                }
            }
        });
    }
}
