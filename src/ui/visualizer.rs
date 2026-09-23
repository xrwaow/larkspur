//! The visualizer — a spectrum-style band of bars above the lyrics panel.
//!
//! The leftmost bar is the bass end, the rightmost the highs: each bar is one
//! of [`BAND_COUNT`](crate::analysis::BAND_COUNT) log-spaced frequency bands,
//! showing the track's spectrum at the live playback position. As the song
//! plays the bars dance with the music; on a seek they jump to the new
//! position's spectrum.
//!
//! The data is [`TrackAnalysis::spectrogram`](crate::analysis::TrackAnalysis) —
//! FFT magnitudes over media time, computed in the same single decode pass as
//! the transport's waveform and sampled through
//! [`Spectrogram::at`](crate::analysis::Spectrogram), which interpolates
//! between columns so the bars move continuously rather than stepping at the
//! column rate.
//!
//! Each bar rises to its target instantly and falls back exponentially, which
//! is what makes the strip bounce the way spectrum displays do. The view
//! observes the shared frame clock ([`SMOOTH_FPS`]) and re-renders only while
//! a bar is actually moving — a paused track settles and then costs nothing.

use std::sync::Arc;

use gpui::{canvas, div, prelude::*, px, Context, Entity, Render, Subscription, Window};

use crate::analysis::BAND_COUNT;
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

pub struct VisualizerView {
    state: Entity<PlaybackState>,
    config: Entity<ConfigState>,
    themed: Themed,
    /// Smoothed bar heights (0.0–1.0). Index 0 is the leftmost (oldest) bar;
    /// the last one tracks "now".
    bars: Vec<f32>,
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
            _observe_animator: observe_animator,
        }
    }

    /// Advance the bars toward the music at the live position. Returns whether
    /// anything moved enough to need a render — once the music stops (or the
    /// analysis is still loading) the bars settle and the view goes quiet.
    ///
    /// The clock observer runs whether or not the band is in the layout, so
    /// the off state short-circuits here rather than sampling per frame.
    fn tick(&mut self, dt: f32, cx: &mut Context<Self>) -> bool {
        if !self.config.read(cx).visualizer() {
            return false;
        }
        let (position, spectrogram) = {
            let state = self.state.read(cx);
            (state.live_position().as_secs_f32(), state.spectrogram())
        };
        // The spectrum at the live position, interpolated between columns.
        let column = spectrogram.as_deref().and_then(|s| s.at(position));

        let mut moved = false;
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let target = column.map_or(0.0, |column| column[i]);
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
        // Snapshot for the paint closure — 64 floats, cloned per frame.
        let bars: Arc<Vec<f32>> = Arc::new(self.bars.clone());

        div()
            .w_full()
            .h(px(BAND_HEIGHT))
            .px_2()
            .py_2()
            .bg(theme.panel_bg)
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
