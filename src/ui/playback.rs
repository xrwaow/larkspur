use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, point, prelude::*, px, AnyElement, Bounds, ClickEvent, Context, Entity,
    MouseButton, MouseDownEvent, PathBuilder, Pixels, Render, Rgba, Subscription, Window,
};

use crate::audio::RepeatMode;
use crate::model::config::WaveformStyle;
use crate::model::InputAction;
use crate::ui::animation::Animator;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::format::format_duration;
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;
use crate::ui::widgets;

/// How long one chunk's bars take to animate in. Short, because the analyzer
/// produces chunks faster than this — the queue drains at this pace.
const CHUNK_ANIM_SECS: f32 = 0.04;

pub struct PlaybackView {
    /// Shared playback state. This view reads it and dispatches actions
    /// into it; it never owns the controller.
    state: Entity<PlaybackState>,
    config: Entity<ConfigState>,
    themed: Themed,
    // Filled in by the seek-bar's canvas each time it paints, so
    // on_mouse_down can turn a window-relative click into a
    // bar-relative fraction without guessing at layout position.
    seek_bar_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The waveform heights currently drawn (0.0–1.0), one per bucket. Chunks
    /// from the analyzer animate into these in sequence.
    bars: Vec<f32>,
    /// The heights the current chunk animation started from.
    from: Vec<f32>,
    /// The heights the current chunk animation is heading to.
    target: Vec<f32>,
    /// Progress of the current chunk animation, 0.0–1.0.
    anim: f32,
    /// Whether a chunk animation is in flight.
    animating: bool,
    /// Whether the track generation the bars belong to, so a track change resets them.
    generation: u64,
    /// Hover state for the prev/next buttons — hover swaps the glyph's color,
    /// which the style-refinement hover can't do, so it's tracked here.
    prev_hovered: bool,
    next_hovered: bool,
    /// Hover state for the repeat toggle (same reason as prev/next).
    repeat_hovered: bool,
    /// Hover state for the queue toggle (same reason as prev/next).
    queue_hovered: bool,
    _observe: Subscription,
    _observe_animator: Subscription,
}

impl PlaybackView {
    pub fn new(
        state: Entity<PlaybackState>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-render whenever the shared state changes. The state owns the
        // ticker, so this view no longer polls for position updates.
        let observe = cx.observe(&state, |_this, _state, cx| cx.notify());
        // The waveform's chunk animations run on the shared frame clock.
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            if this.tick(animator.read(cx).dt(), cx) {
                cx.notify();
            }
        });
        let themed = Themed::new(&config, cx);
        Self {
            state,
            config,
            themed,
            seek_bar_bounds: Rc::new(Cell::new(Bounds::default())),
            bars: Vec::new(),
            from: Vec::new(),
            target: Vec::new(),
            anim: 0.0,
            animating: false,
            generation: 0,
            prev_hovered: false,
            next_hovered: false,
            repeat_hovered: false,
            queue_hovered: false,
            _observe: observe,
            _observe_animator: observe_animator,
        }
    }

    /// Advance the waveform's chunk animation: pop the next chunk when the
    /// current one has settled, and ease the bars toward it. Returns whether
    /// anything moved enough to need a render.
    ///
    /// The analyzer never waits for this — it keeps pushing chunks, and they
    /// queue up here, so the bars animate in one chunk after another.
    fn tick(&mut self, dt: f32, cx: &mut Context<Self>) -> bool {
        let generation = self.state.read(cx).waveform_generation();
        if generation != self.generation {
            // A new track: drop the old bars so the new ones animate in clean.
            self.generation = generation;
            self.bars.clear();
            self.from.clear();
            self.target.clear();
            self.anim = 0.0;
            self.animating = false;
            return true;
        }

        if !self.animating {
            let Some(chunk) = self.state.read(cx).take_chunk() else {
                return false;
            };
            self.target = chunk.normalized_all();
            if self.bars.len() != self.target.len() {
                self.bars = vec![0.0; self.target.len()];
            }
            self.from = self.bars.clone();
            self.anim = 0.0;
            self.animating = true;
        }

        self.anim = (self.anim + dt / CHUNK_ANIM_SECS).min(1.0);
        let t = ease_out_cubic(self.anim);
        for (i, bar) in self.bars.iter_mut().enumerate() {
            *bar = self.from[i] + (self.target[i] - self.from[i]) * t;
        }
        if self.anim >= 1.0 {
            self.animating = false;
        }
        true
    }

    /// Route a decoupled [`InputAction`] into the shared state, re-rendering
    /// only if it changed something.
    fn dispatch(&mut self, action: InputAction, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if state.apply(action) {
                cx.notify();
            }
        });
    }

    fn seek_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.seek_fraction(fraction);
            cx.notify();
        });
    }
}

/// Fast at the start, easing to a stop — the standard "settle" curve.
fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

impl Container for PlaybackView {
    fn container_id() -> &'static str {
        "playback"
    }
}

pub(crate) const BAR_WIDTH: f32 = 480.0;
const BAR_HEIGHT: f32 = 40.0;

impl Render for PlaybackView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let style = self.config.read(cx).waveform();

        // Snapshot the shared state once, then drop the borrow before
        // building elements.
        let (position, duration, can_prev, can_next, ended, playing, repeat) = {
            let state = self.state.read(cx);
            (
                state.position(),
                state.duration(),
                state.can_prev(),
                state.can_next(),
                state.ended(),
                state.is_playing(),
                state.repeat_mode(),
            )
        };
        let queue_open = self.state.read(cx).queue_open();
        // Snapshot the animated bars for the paint closure.
        let bars: Arc<Vec<f32>> = Arc::new(self.bars.clone());

        let fraction = if duration.as_secs_f32() > 0.0 {
            position.as_secs_f32() / duration.as_secs_f32()
        } else {
            0.0
        };

        let bounds_cell = self.seek_bar_bounds.clone();
        let bounds_for_paint = bounds_cell.clone();

        let seek_bar = div()
            .id("seek-bar")
            .w(px(BAR_WIDTH))
            .h(px(BAR_HEIGHT))
            .relative()
            .overflow_hidden()
            .child(
                // canvas gives us the element's real on-screen bounds
                // every time it paints — that's what on_mouse_down below
                // reads to compute a click-relative fraction, instead of
                // assuming window coords == local coords.
                canvas(
                    move |bounds, _window, _cx| {
                        bounds_for_paint.set(bounds);
                        bounds
                    },
                    |_bounds, _prepaint, _window, _cx| {},
                )
                .size_full(),
            )
            .child(
                // Absolutely positioned so the waveform paints inside the
                // bar instead of stacking below it.
                div()
                    .absolute()
                    .inset_0()
                    .child(render_waveform(bars, style, fraction, theme)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    // Use the *actual* painted width, not BAR_WIDTH: if the
                    // bar ever becomes responsive the two diverge and clicks
                    // would land in the wrong place.
                    let bounds = bounds_cell.get();
                    let width = f32::from(bounds.size.width);
                    if width <= 0.0 {
                        return;
                    }
                    let local_x: f32 = f32::from(event.position.x - bounds.origin.x);
                    this.seek_fraction(local_x / width, cx);
                }),
            );

        // The waveform sits on top; below it a single row with the current
        // time, the transport controls, and the end time.
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .p_4()
            .w_full()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(seek_bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w(px(BAR_WIDTH))
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_muted)
                            .child(format_duration(position)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .items_center()
                            .child(
                                // The repeat toggle, to the left of "prev":
                                // clicks cycle off → repeat-one → repeat-all.
                                // Lit while any repeat mode is active, and the
                                // glyph gains a "1" in repeat-one.
                                div()
                                    .id("repeat-toggle")
                                    .cursor_pointer()
                                    .size(px(36.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                                        this.repeat_hovered = *hovered;
                                        cx.notify();
                                    }))
                                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.state.update(cx, |state, cx| state.cycle_repeat(cx));
                                    }))
                                    .child(icon(
                                        15.0,
                                        14.0,
                                        if repeat == RepeatMode::Once {
                                            REPEAT_ONCE_POLYGONS
                                        } else {
                                            REPEAT_POLYGONS
                                        },
                                        if repeat != RepeatMode::Off || self.repeat_hovered {
                                            theme.accent
                                        } else {
                                            theme.text_muted
                                        },
                                    )),
                            )
                            .child(transport_button(
                                "prev",
                                can_prev,
                                self.prev_hovered,
                                PREV_POLYGONS,
                                theme,
                                cx.listener(|this, _event, _window, cx| {
                                    this.dispatch(InputAction::PrevTrack, cx);
                                }),
                                cx.listener(|this, hovered: &bool, _window, cx| {
                                    this.prev_hovered = *hovered;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                div()
                                    .id("play-pause")
                                    .when(!ended, |d| d.cursor_pointer())
                                    .when(ended, |d| d.opacity(0.4))
                                    .rounded_full()
                                    .size(px(48.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme.text_muted)
                                    .when(!ended, |d| d.hover(|d| d.bg(theme.accent)))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.dispatch(InputAction::TogglePause, cx);
                                    }))
                                    .child(div().flex().items_center().child(if playing {
                                        icon(13.0, 16.0, PAUSE_POLYGONS, theme.panel_bg)
                                            .into_any_element()
                                    } else {
                                        // Nudge the triangle right so it reads as
                                        // optically centered in the circle.
                                        div()
                                            .pl(px(2.0))
                                            .child(icon(14.0, 16.0, PLAY_POLYGONS, theme.panel_bg))
                                            .into_any_element()
                                    })),
                            )
                            .child(transport_button(
                                "next",
                                can_next,
                                self.next_hovered,
                                NEXT_POLYGONS,
                                theme,
                                cx.listener(|this, _event, _window, cx| {
                                    this.dispatch(InputAction::NextTrack, cx);
                                }),
                                cx.listener(|this, hovered: &bool, _window, cx| {
                                    this.next_hovered = *hovered;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                // The queue toggle, to the right of "next": it
                                // swaps the queue panel in over the tabs, so
                                // it stays lit while the panel is open.
                                div()
                                    .id("queue-toggle")
                                    .cursor_pointer()
                                    .size(px(36.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                                        this.queue_hovered = *hovered;
                                        cx.notify();
                                    }))
                                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.state.update(cx, |state, cx| state.toggle_queue(cx));
                                    }))
                                    .child(icon(
                                        15.0,
                                        14.0,
                                        QUEUE_POLYGONS,
                                        if queue_open || self.queue_hovered {
                                            theme.accent
                                        } else {
                                            theme.text_muted
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_muted)
                            .child(format_duration(duration)),
                    ),
            )
    }
}

/// Unit-square polygon sets for the transport icons, painted via
/// `PathBuilder` — gpui has no per-side border colors, so the old
/// CSS-border triangle trick just rendered solid squares.
const PLAY_POLYGONS: &[&[(f32, f32)]] = &[&[(0.0, 0.0), (0.0, 1.0), (1.0, 0.5)]];

const PAUSE_POLYGONS: &[&[(f32, f32)]] = &[
    &[(0.0, 0.0), (0.35, 0.0), (0.35, 1.0), (0.0, 1.0)],
    &[(0.65, 0.0), (1.0, 0.0), (1.0, 1.0), (0.65, 1.0)],
];

const NEXT_POLYGONS: &[&[(f32, f32)]] = &[
    &[(0.0, 0.0), (0.0, 1.0), (0.68, 0.5)],
    &[(0.8, 0.0), (1.0, 0.0), (1.0, 1.0), (0.8, 1.0)],
];

const PREV_POLYGONS: &[&[(f32, f32)]] = &[
    &[(1.0, 0.0), (1.0, 1.0), (0.32, 0.5)],
    &[(0.0, 0.0), (0.2, 0.0), (0.2, 1.0), (0.0, 1.0)],
];

/// The repeat glyph: two curved arrows chasing each other around a rounded
/// loop — the top one flows right, the bottom one flows left. Each arrow is a
/// straight stub, a quarter-arc (outer/inner radius around a shared center),
/// a straight run, and a triangular head.
const REPEAT_POLYGONS: &[&[(f32, f32)]] = &[
    // Top arrow: rises on the left, curves over the top, heads right.
    &[(0.10, 0.60), (0.26, 0.60), (0.26, 0.36), (0.10, 0.36)],
    &[
        (0.10, 0.36), (0.124, 0.238), (0.194, 0.134), (0.297, 0.065), (0.42, 0.04),
        (0.42, 0.20), (0.359, 0.212), (0.307, 0.247), (0.256, 0.299), (0.26, 0.36),
    ],
    &[(0.42, 0.04), (0.70, 0.04), (0.70, 0.20), (0.42, 0.20)],
    &[(0.66, 0.00), (0.94, 0.12), (0.66, 0.24)],
    // Bottom arrow: falls on the right, curves under the bottom, heads left.
    &[(0.74, 0.40), (0.90, 0.40), (0.90, 0.64), (0.74, 0.64)],
    &[
        (0.90, 0.64), (0.876, 0.762), (0.806, 0.866), (0.703, 0.935), (0.58, 0.96),
        (0.58, 0.80), (0.641, 0.788), (0.693, 0.753), (0.744, 0.701), (0.74, 0.64),
    ],
    &[(0.30, 0.80), (0.58, 0.80), (0.58, 0.96), (0.30, 0.96)],
    &[(0.34, 0.76), (0.06, 0.88), (0.34, 1.00)],
];

/// Repeat-one: the loop mark plus a "1" in the ring's center.
const REPEAT_ONCE_POLYGONS: &[&[(f32, f32)]] = &[
    &[(0.10, 0.60), (0.26, 0.60), (0.26, 0.36), (0.10, 0.36)],
    &[
        (0.10, 0.36), (0.124, 0.238), (0.194, 0.134), (0.297, 0.065), (0.42, 0.04),
        (0.42, 0.20), (0.359, 0.212), (0.307, 0.247), (0.256, 0.299), (0.26, 0.36),
    ],
    &[(0.42, 0.04), (0.70, 0.04), (0.70, 0.20), (0.42, 0.20)],
    &[(0.66, 0.00), (0.94, 0.12), (0.66, 0.24)],
    &[(0.74, 0.40), (0.90, 0.40), (0.90, 0.64), (0.74, 0.64)],
    &[
        (0.90, 0.64), (0.876, 0.762), (0.806, 0.866), (0.703, 0.935), (0.58, 0.96),
        (0.58, 0.80), (0.641, 0.788), (0.693, 0.753), (0.744, 0.701), (0.74, 0.64),
    ],
    &[(0.30, 0.80), (0.58, 0.80), (0.58, 0.96), (0.30, 0.96)],
    &[(0.34, 0.76), (0.06, 0.88), (0.34, 1.00)],
    // The "1": a stem with a diagonal serif at its top left.
    &[(0.46, 0.38), (0.54, 0.38), (0.54, 0.66), (0.46, 0.66)],
    &[(0.38, 0.48), (0.46, 0.38), (0.46, 0.48)],
];

/// The queue toggle's glyph: three horizontal bars, the last one shorter —
/// the standard "up next" list mark.
const QUEUE_POLYGONS: &[&[(f32, f32)]] = &[
    &[(0.0, 0.0), (1.0, 0.0), (1.0, 0.16), (0.0, 0.16)],
    &[(0.0, 0.42), (1.0, 0.42), (1.0, 0.58), (0.0, 0.58)],
    &[(0.0, 0.84), (0.62, 0.84), (0.62, 1.0), (0.0, 1.0)],
];

/// A fixed-size canvas painting filled polygons given in unit-square
/// coordinates, scaled into the element's bounds.
fn icon(
    w: f32,
    h: f32,
    polygons: &'static [&'static [(f32, f32)]],
    color: Rgba,
) -> impl IntoElement {
    canvas(
        |bounds, _window, _cx| bounds,
        move |bounds, _prepaint, window, _cx| {
            for polygon in polygons {
                let mut builder = PathBuilder::fill();
                builder.add_polygon(
                    &polygon
                        .iter()
                        .map(|(x, y)| {
                            point(
                                bounds.origin.x + px(x * f32::from(bounds.size.width)),
                                bounds.origin.y + px(y * f32::from(bounds.size.height)),
                            )
                        })
                        .collect::<Vec<_>>(),
                    true,
                );
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .w(px(w))
    .h(px(h))
}

fn transport_button(
    id: &'static str,
    enabled: bool,
    hovered: bool,
    polygons: &'static [&'static [(f32, f32)]],
    theme: Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_hover: impl Fn(&bool, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .when(enabled, |d| d.cursor_pointer())
        .size(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .on_hover(on_hover)
        .on_click(on_click)
        // Three states, ordered dimmest to brightest in both themes:
        // unusable dims to faint text, usable sits one step above it, and
        // hover jumps to the waveform's bright played color. (The waveform's
        // unplayed color can't be the usable state — in the dark palette it's
        // darker than faint text, so a usable button would read as more
        // muted than a disabled one.)
        .child(icon(
            15.0,
            14.0,
            polygons,
            if !enabled {
                theme.text_faint
            } else if hovered {
                theme.accent
            } else {
                theme.text_muted
            },
        ))
}

/// A fixed-size canvas painting filled polygons given in unit-square
/// coordinates, scaled into the element's bounds.
fn render_waveform(
    bars: Arc<Vec<f32>>,
    style: WaveformStyle,
    progress: f32,
    theme: Theme,
) -> AnyElement {
    match style {
        WaveformStyle::FlatLine => div()
            .w_full()
            .h_full()
            .relative()
            .child(
                // Line centered vertically in the bar (BAR_HEIGHT = 40).
                div()
                    .absolute()
                    .top(px((BAR_HEIGHT - 2.0) / 2.0))
                    .w_full()
                    .h(px(2.0))
                    .bg(theme.waveform),
            )
            .child(
                // Progress dot centered on the line, fully inside the bar
                // (overflow_hidden would clip a negatively-offset dot, and
                // the dot must not run past the right edge at progress 1.0).
                div()
                    .absolute()
                    .left(px(progress * (BAR_WIDTH - 10.0)))
                    .top(px((BAR_HEIGHT - 10.0) / 2.0))
                    .size(px(10.0))
                    .rounded_full()
                    .bg(theme.accent),
            )
            .into_any_element(),
        WaveformStyle::Bars => waveform_bars(bars, progress, theme),
    }
}

/// Paint the bars through the shared bar-strip pass — only the per-bar color
/// (played/unplayed) and the baseline floor are this view's own.
///
/// `bars` are already normalized (0.0–1.0) and already animated: the view
/// eases each chunk in as it arrives, so this is a plain paint.
fn waveform_bars(bars: Arc<Vec<f32>>, progress: f32, theme: Theme) -> AnyElement {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, _prepaint, window, _cx| {
            let count = bars.len();
            widgets::paint_bars(window, bounds, &bars, 0.04, |i| {
                if (i as f32 / count.max(1) as f32) < progress {
                    theme.accent
                } else {
                    theme.waveform
                }
            });
        },
    )
    .size_full()
    .into_any_element()
}
