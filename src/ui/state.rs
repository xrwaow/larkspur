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
use std::time::Duration;

use gpui::Context;

use crate::audio::PlaybackController;
use crate::model::{InputAction, SongMetadata, SongStatus};
use crate::waveform::compute_waveform;

/// How many bars a waveform is reduced to.
const WAVEFORM_BUCKETS: usize = 120;

/// Tick interval while audio is playing (≈60 fps progress).
const PLAYING_TICK: Duration = Duration::from_millis(16);
/// Tick interval while idle — just fast enough to notice external changes.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// Waveform for the current track, computed off the UI thread.
#[derive(Clone)]
pub enum Waveform {
    Loading,
    Ready(Arc<Vec<f32>>),
    Unavailable,
}

impl Waveform {
    /// The loaded peaks, if ready. Cheap to call: clones an `Arc`.
    pub fn peaks(&self) -> Option<Arc<Vec<f32>>> {
        match self {
            Waveform::Ready(peaks) => Some(peaks.clone()),
            Waveform::Loading | Waveform::Unavailable => None,
        }
    }
}

pub struct PlaybackState {
    controller: PlaybackController,
    waveform: Waveform,
    /// Bumped on every track change so a late-arriving waveform decode for
    /// a track we've already moved past is discarded instead of clobbering
    /// the current one.
    generation: u64,
    /// Last values we rendered, so a tick only notifies on real change.
    last_position: Duration,
    last_status: SongStatus,
}

impl PlaybackState {
    pub fn new(controller: PlaybackController, cx: &mut Context<Self>) -> Self {
        let status = controller.status();
        let mut this = Self {
            controller,
            waveform: Waveform::Loading,
            generation: 0,
            last_position: Duration::ZERO,
            last_status: status,
        };
        this.reload_waveform(cx);
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
        self.controller.position()
    }

    pub fn waveform(&self) -> &Waveform {
        &self.waveform
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

    pub fn seek_relative(&mut self, secs: i64) {
        self.controller.seek_relative(secs);
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
            self.reload_waveform(cx);
            changed = true;
        }

        let position = self.controller.position();
        if position != self.last_position {
            self.last_position = position;
            changed = true;
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

    /// Kick off an off-thread waveform decode for the current track.
    fn reload_waveform(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.waveform = Waveform::Loading;

        let Some(path) = self.controller.current_path().map(PathBuf::from) else {
            self.waveform = Waveform::Unavailable;
            return;
        };

        cx.spawn(async move |this, cx| {
            let decode = cx
                .background_executor()
                .spawn(async move { compute_waveform(&path, WAVEFORM_BUCKETS) });
            let peaks = decode.await;

            this.update(cx, |state, cx| {
                // Discard if the track changed while we were decoding.
                if state.generation == generation {
                    state.waveform = match peaks {
                        Ok(peaks) => Waveform::Ready(Arc::new(peaks)),
                        Err(_) => Waveform::Unavailable,
                    };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}
