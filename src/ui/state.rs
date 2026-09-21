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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::Context;

use crate::analysis::{analyze_track, TrackAnalysis};
use crate::audio::PlaybackController;
use crate::model::{InputAction, SongMetadata, SongStatus};

/// How many bars a waveform is reduced to.
const WAVEFORM_BUCKETS: usize = 240;

/// Media-time width of one live-bitrate bucket, and how often the displayed
/// value refreshes.
const BITRATE_BUCKET: Duration = Duration::from_secs(1);

/// How far the rendered position may run ahead of rodio's last sample while
/// smoothing between its updates.
const MAX_EXTRAPOLATION: Duration = Duration::from_millis(200);

/// Tick interval while audio is playing (≈60 fps progress).
const PLAYING_TICK: Duration = Duration::from_millis(16);
/// Tick interval while idle — just fast enough to notice external changes.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// The current track's waveform peaks + live-bitrate curve, computed off the
/// UI thread in a single pass when the track changes.
#[derive(Clone)]
pub enum TrackAnalysisState {
    Loading,
    Ready(Arc<TrackAnalysis>),
    Unavailable,
}

impl TrackAnalysisState {
    /// The waveform peaks, if ready. Cheap: clones an `Arc`.
    pub fn peaks(&self) -> Option<Arc<Vec<f32>>> {
        match self {
            TrackAnalysisState::Ready(analysis) => Some(analysis.peaks.clone()),
            TrackAnalysisState::Loading | TrackAnalysisState::Unavailable => None,
        }
    }

    /// The live bitrate at `position`, if the profile is ready.
    pub fn bitrate_at(&self, position: Duration) -> Option<u32> {
        match self {
            TrackAnalysisState::Ready(analysis) => analysis.bitrate.at(position),
            TrackAnalysisState::Loading | TrackAnalysisState::Unavailable => None,
        }
    }
}

pub struct PlaybackState {
    controller: PlaybackController,
    analysis: TrackAnalysisState,
    /// Bumped on every track change so a late-arriving analysis for a track
    /// we've already moved past is discarded instead of clobbering the
    /// current one.
    generation: u64,
    /// Last values we rendered, so a tick only notifies on real change.
    last_position: Duration,
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
}

impl PlaybackState {
    pub fn new(controller: PlaybackController, cx: &mut Context<Self>) -> Self {
        let status = controller.status();
        let position = controller.position();
        let mut this = Self {
            controller,
            analysis: TrackAnalysisState::Loading,
            generation: 0,
            last_position: Duration::ZERO,
            last_status: status,
            live_bitrate_bps: None,
            last_bitrate_at: Duration::ZERO,
            display_position: position,
            anchor: None,
        };
        this.reload_track(cx);
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

    /// The current track's waveform peaks, if the analysis is ready.
    pub fn peaks(&self) -> Option<Arc<Vec<f32>>> {
        self.analysis.peaks()
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

    /// Replace the play queue with `paths`, starting at `start`.
    ///
    /// Used to turn a playlist into the queue: the caller resolves the
    /// playlist's songs to paths, so this stays playlist-agnostic. Re-loads
    /// the waveform for whatever track ends up current.
    pub fn play_paths(&mut self, paths: Vec<PathBuf>, start: usize, cx: &mut Context<Self>) -> bool {
        if paths.is_empty() {
            return false;
        }
        match self.controller.set_queue(paths, start) {
            Ok(()) => {
                self.reload_track(cx);
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
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;

        if self.controller.tick_advance() {
            self.reload_track(cx);
            changed = true;
        }

        let position = self.display_position_now();
        self.display_position = position;
        if position != self.last_position {
            self.last_position = position;
            changed = true;
        }

        // Refresh the live bitrate every BITRATE_BUCKET of media time (and
        // immediately after a seek, where the position jumps backwards).
        if self.controller.is_playing()
            && position.abs_diff(self.last_bitrate_at) >= BITRATE_BUCKET
        {
            self.last_bitrate_at = position;
            let next = self.analysis.bitrate_at(position);
            if next != self.live_bitrate_bps {
                self.live_bitrate_bps = next;
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
                // Smooth while playing; cheap while idle.
                let interval = match this.read_with(cx, |state, _| state.is_playing()) {
                    Ok(true) => PLAYING_TICK,
                    Ok(false) => IDLE_TICK,
                    Err(_) => break,
                };
                cx.background_executor().timer(interval).await;

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

    /// Kick off off-thread analysis for the current track: waveform peaks and
    /// a live-bitrate profile, from a single demux+decode pass. Reports back
    /// into this state, guarded by `generation` so a stale result is discarded.
    fn reload_track(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.analysis = TrackAnalysisState::Loading;
        self.live_bitrate_bps = None;
        self.last_bitrate_at = Duration::ZERO;
        self.anchor = None;
        self.display_position = self.controller.position();

        let Some(path) = self.controller.current_path().map(PathBuf::from) else {
            self.analysis = TrackAnalysisState::Unavailable;
            return;
        };

        cx.spawn(async move |this, cx| {
            let compute = cx.background_executor().spawn(async move {
                analyze_track(&path, WAVEFORM_BUCKETS, BITRATE_BUCKET)
            });
            let analysis = compute.await;

            this.update(cx, |state, cx| {
                // Discard if the track changed while we were analyzing.
                if state.generation == generation {
                    state.analysis = match analysis {
                        Ok(analysis) => TrackAnalysisState::Ready(Arc::new(analysis)),
                        Err(_) => TrackAnalysisState::Unavailable,
                    };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}
