//! The visualizer — a spectrum-style band of bars between the lyrics panel
//! and the now-playing cover.
//!
//! The leftmost bar is the bass end, the rightmost the highs: each bar is one
//! of [`BAND_COUNT`](crate::analysis::BAND_COUNT) log-spaced frequency bands.
//! Unlike the transport's waveform, this is **real-time**: it transforms the
//! samples the audio thread is actually playing (through
//! [`SampleTap`](crate::audio::SampleTap)), so the bars follow the music with
//! no precomputation and no dependence on playback position.
//!
//! Each bar rises to its target instantly and falls back exponentially, which
//! is what makes the strip bounce the way spectrum displays do. A slow
//! auto-gain keeps the bars using the full height as the music's level moves.
//! The view observes the shared frame clock ([`SMOOTH_FPS`]) and re-renders
//! only while a bar is actually moving — a paused track settles and then costs
//! nothing.
//!
//! [`SMOOTH_FPS`]: crate::ui::animation::SMOOTH_FPS

use std::sync::Arc;

use gpui::{canvas, div, prelude::*, px, Context, Entity, Render, Subscription, Window};

use crate::analysis::{SpectrumAnalyzer, BAND_COUNT, FFT_SIZE};
use crate::ui::animation::Animator;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::playback_state::PlaybackState;
use crate::ui::widgets;

/// The band's height, in px. Fixed so the layout can reserve it above the
/// lyrics panel.
pub const BAND_HEIGHT: f32 = 64.0;

/// How many bars the strip draws — one per frequency band, bass on the left.
/// The rail is 240 px wide, so this lands just under 3 px per bar — thin, like
/// the classic spectrum displays.
const BAR_COUNT: usize = BAND_COUNT;

/// Exponential fall time constant, in seconds. A bar whose target dropped to
/// zero falls ~63% of the way in this long — slow enough to read as decay,
/// fast enough to keep up with the music.
const FALL_SECS: f32 = 0.22;

/// Below this height a bar counts as settled and stops asking for frames.
const SETTLE: f32 = 0.001;

/// Auto-gain release time constant, in seconds: how slowly the reference peak
/// decays once the music gets quieter. Long enough that the bars don't pump,
/// short enough that a quiet passage still uses the height.
const GAIN_RELEASE_SECS: f32 = 1.5;

/// The smallest reference peak the auto-gain will divide by, so a silent track
/// doesn't amplify its own noise floor into full-height bars.
const GAIN_FLOOR: f32 = 4.0;

pub struct VisualizerView {
    state: Entity<PlaybackState>,
    config: Entity<ConfigState>,
    themed: Themed,
    /// Smoothed bar heights (0.0–1.0), bass on the left.
    bars: Vec<f32>,
    /// The real-time spectrum core, rebuilt if the track's sample rate changes.
    analyzer: Option<SpectrumAnalyzer>,
    analyzer_rate: u32,
    /// Scratch for the tap's most recent samples, reused each frame.
    samples: Vec<f32>,
    /// The running reference peak the bars are scaled against.
    gain_peak: f32,
    _observe_animator: Subscription,
}

impl VisualizerView {
    pub fn new(
        state: Entity<PlaybackState>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            if this.tick(animator.read(cx).dt(), cx) {
                cx.notify();
            }
        });
        Self {
            state,
            config: config.clone(),
            themed: Themed::new(&config, cx),
            bars: vec![0.0; BAR_COUNT],
            analyzer: None,
            analyzer_rate: 0,
            samples: Vec::with_capacity(FFT_SIZE),
            gain_peak: GAIN_FLOOR,
            _observe_animator: observe_animator,
        }
    }

    /// Advance the bars toward the live spectrum. Returns whether anything
    /// moved enough to need a render — once the music stops the bars settle and
    /// the view goes quiet.
    ///
    /// The clock observer runs whether or not the band is in the layout, so the
    /// off state short-circuits here rather than sampling per frame.
    fn tick(&mut self, dt: f32, cx: &mut Context<Self>) -> bool {
        if !self.config.read(cx).visualizer() {
            return false;
        }
        // Paused (or stopped): freeze the bars exactly where they are. The tap
        // holds the last samples, so without this the auto-gain would keep
        // decaying and the bars would drift even though nothing is playing.
        if !self.state.read(cx).is_playing() {
            return false;
        }

        // Transform what the audio thread last played, not a precomputed
        // spectrogram: this is the whole difference from the old visualizer.
        let tap = self.state.read(cx).sample_tap();
        let rate = tap.sample_rate();
        if self.analyzer.is_none() || self.analyzer_rate != rate {
            self.analyzer = Some(SpectrumAnalyzer::new(rate));
            self.analyzer_rate = rate;
        }
        tap.copy_recent(&mut self.samples, FFT_SIZE);
        let column = self
            .analyzer
            .as_mut()
            .map(|analyzer| analyzer.bands(&self.samples));

        // Auto-gain: track the loudest band, decaying slowly, so a quiet
        // passage still uses the height without the bars pumping.
        let peak = column.map_or(0.0, |c| c.iter().copied().fold(0f32, f32::max));
        let decay = (-dt / GAIN_RELEASE_SECS).exp();
        self.gain_peak = (self.gain_peak * decay).max(peak).max(GAIN_FLOOR);
        let inv = 1.0 / self.gain_peak;

        let mut moved = false;
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let target = column.map_or(0.0, |c| (c[i] * inv).sqrt().min(1.0));
            let next = fall_toward(*bar, target, dt);
            if (next - *bar).abs() > SETTLE {
                moved = true;
            }
            *bar = next;
        }
        moved
    }
}

impl Container for VisualizerView {
    fn container_id() -> &'static str {
        "visualizer"
    }
}

impl Render for VisualizerView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        // Snapshot for the paint closure — 128 floats, cloned per frame.
        let bars: Arc<Vec<f32>> = Arc::new(self.bars.clone());

        div()
            .w_full()
            .h(px(BAND_HEIGHT))
            .px_2()
            .py_2()
            .bg(theme.rail_bg)
            .child(
                canvas(
                    |_bounds, _window, _cx| (),
                    move |bounds, _prepaint, window, _cx| {
                        // The bar paint is the shared transport/visualizer
                        // pass — only the rise/fall dynamics are this view's.
                        widgets::paint_bars(window, bounds, &bars, 0.03, |_| theme.accent);
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

/// Chase `target` from `current`: rise instantly, fall exponentially (so the
/// decay is frame-rate independent). This asymmetry is the classic visualizer
/// feel — bars snap up with the music and melt back down.
fn fall_toward(current: f32, target: f32, dt: f32) -> f32 {
    if target >= current || dt <= 0.0 {
        return target;
    }
    let k = (-dt / FALL_SECS).exp();
    target + (current - target) * k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_rise_instantly_and_fall_exponentially() {
        assert_eq!(fall_toward(0.2, 0.8, 1.0 / 60.0), 0.8, "rise is instant");
        let dropped = fall_toward(1.0, 0.0, FALL_SECS);
        assert!((dropped - 1.0f32 / std::f32::consts::E).abs() < 0.01, "{}", dropped);
        assert_eq!(fall_toward(0.5, 0.5, 1.0 / 60.0), 0.5, "at rest stays at rest");
    }
}
