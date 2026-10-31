//! The playing row's equalizer, as a retained view.
//!
//! The equalizer is the one thing on an album list that moves whenever audio
//! plays, and it used to be the reason a playing list repainted at full frame
//! rate: its phase came from the clock at the *list's* render time, so the
//! whole list had to redraw to advance three bars. As its own [`Entity`] the
//! arithmetic changes: the view observes the clock itself, repaints only its
//! own tiny subtree (a few painted quads inside the number column), and the
//! list around it stays still.
//!
//! The parent doesn't drive it at all — it just drops the entity into the
//! playing row's element tree. When nothing is playing the stepper settles and
//! the clock parks; a play/pause flip arrives through the playback observer,
//! which repaints the rest profile once and lets the clock go.

use std::time::{Duration, Instant};

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity,
    IntoElement, Pixels, Render, Rgba, Subscription, Window,
};

use crate::ui::animation::{animate, Advance, Animator};
use crate::ui::config_state::ConfigState;
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;

/// How long since the view's last paint before its stepper stops asking for
/// frames. A mounted, visible equalizer repaints every few milliseconds, so
/// this only trips for an entity nobody is drawing — the equalizer of a
/// background tab (each album list owns one, and they all tick while audio
/// plays). Painting resumes on the next real render, which re-arms the clock
/// from [`EqBars::render`].
const PAINT_LIVENESS: Duration = Duration::from_millis(250);

/// How many bars the equalizer draws.
const EQ_BARS: usize = 3;

/// Width of one equalizer bar, and the gap between them, in px.
const EQ_BAR_PX: f32 = 2.0;
const EQ_GAP_PX: f32 = 2.0;

/// The playing row's equalizer: a few small bars in the accent colour, dancing
/// while the track plays and resting when it's paused. It takes the number
/// column's place, so the row still lines up. One per album list, dropped into
/// whichever row is the playing one.
pub struct EqBars {
    animator: Entity<Animator>,
    playback: Entity<PlaybackState>,
    config: Entity<ConfigState>,
    /// When this view last rendered — the paint-liveness signal the stepper
    /// reads. `None` until the first paint (a never-rendered tab animates
    /// nothing).
    last_paint: Option<Instant>,
    /// The container whose typography the bars size to. Every album list
    /// shares the browse list's typography (see the views' `Themed`
    /// resolution), so they all pass the same pair.
    container: &'static str,
    default_size: f32,
    _anim: Subscription,
    _playback: Subscription,
}

impl EqBars {
    /// Build the view and wire it to the clock and the playback state. The
    /// entity must still be `request`ed a first frame when it starts
    /// rendering — [`Self::render`] does that, since a row can become the
    /// playing one at any moment.
    pub fn new(
        animator: Entity<Animator>,
        playback: Entity<PlaybackState>,
        config: Entity<ConfigState>,
        container: &'static str,
        default_size: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        // Dance at the clock's full cadence while audio plays; the repaint is
        // this view's alone, so full frame rate costs a few quads. Unless the
        // view isn't being painted at all (a background tab): go idle there,
        // rather than ticking a render that never happens.
        let _anim = animate(&animator, cx, |this, _dt, cx| {
            if !this.playback.read(cx).is_playing() {
                return Advance::IDLE;
            }
            match this.last_paint {
                Some(last) if last.elapsed() < PAINT_LIVENESS => Advance::MOVED,
                _ => Advance::IDLE,
            }
        });
        // Play/pause changes what the bars draw (motion vs rest profile). The
        // observer repaints once per flip; a playing state's own 1 Hz ticks
        // re-render this three-bar view, which is noise.
        let _playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        Self {
            animator,
            playback,
            config,
            container,
            default_size,
            last_paint: None,
            _anim,
            _playback,
        }
    }

    /// The theme the parent list renders with — resolved the same way its
    /// `Themed` does, so a font-size or palette change moves the bars with
    /// everything else.
    fn theme(&self, cx: &App) -> Theme {
        self.config.read(cx).theme_for(self.container, self.default_size)
    }
}

impl Render for EqBars {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Record the paint, both as the liveness signal and because a render
        // here is the only thing that can re-arm the clock after a stall —
        // the playing row may have just scrolled into view, or the tab may
        // just have come to the front.
        self.last_paint = Some(Instant::now());
        let theme = self.theme(cx);
        let phase = self.animator.read(cx).elapsed();
        let playing = self.playback.read(cx).is_playing();
        if playing {
            // Nothing else asks the clock for frames on this view's behalf.
            self.animator.read(cx).request();
        }
        eq_icon(theme, phase, playing)
    }
}

/// The equalizer's bars at `phase` seconds, in the number column's place.
fn eq_icon(theme: Theme, phase: f32, playing: bool) -> AnyElement {
    let height = theme.font_size * 0.95;
    let levels = eq_levels(phase, playing);
    let width = EQ_BARS as f32 * EQ_BAR_PX + (EQ_BARS - 1) as f32 * EQ_GAP_PX;
    div()
        .w(px(theme.number_col()))
        .flex_none()
        .h(px(height))
        .flex()
        // Start on the same vertical line as the numbers it replaces.
        .justify_start()
        .child(
            canvas(
                |_bounds, _window, _cx| (),
                move |bounds, _prepaint, window, _cx| {
                    paint_eq(window, bounds, &levels, theme.accent);
                },
            )
            .w(px(width))
            .h_full(),
        )
        .into_any_element()
}

/// The bar heights (0..1) at `phase` seconds. Each bar runs at its own speed
/// and offset so the three never move in lockstep; a paused track rests at a
/// short, even profile.
fn eq_levels(phase: f32, playing: bool) -> [f32; EQ_BARS] {
    if !playing {
        return [0.30, 0.55, 0.40];
    }
    let mut levels = [0.0; EQ_BARS];
    for (i, level) in levels.iter_mut().enumerate() {
        let speed = 5.0 + i as f32 * 1.7;
        let offset = i as f32 * 1.9;
        let wave = (phase * speed + offset).sin().abs();
        *level = 0.22 + 0.78 * wave;
    }
    levels
}

/// Paint the equalizer's bars, bottom-aligned in `bounds`.
fn paint_eq(window: &mut Window, bounds: Bounds<Pixels>, levels: &[f32; EQ_BARS], color: Rgba) {
    let height = f32::from(bounds.size.height);
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    for (i, level) in levels.iter().enumerate() {
        let bar_height = (level * height).max(2.0);
        let x = origin_x + i as f32 * (EQ_BAR_PX + EQ_GAP_PX);
        let y = origin_y + (height - bar_height);
        let bar = Bounds::new(point(px(x), px(y)), size(px(EQ_BAR_PX), px(bar_height)));
        window.paint_quad(fill(bar, color));
    }
}
