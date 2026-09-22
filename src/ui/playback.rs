use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnyElement, Bounds, Context, Entity,
    MouseButton, MouseDownEvent, PathBuilder, Pixels, Render, Rgba, Subscription, Window,
};

use crate::model::config::WaveformStyle;
use crate::model::InputAction;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::format::format_duration;
use crate::ui::state::PlaybackState;
use crate::ui::theme::Theme;

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
    _observe: Subscription,
}

impl PlaybackView {
    pub fn new(
        state: Entity<PlaybackState>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-render whenever the shared state changes. The state owns the
        // ticker, so this view no longer polls for position updates.
        let observe = cx.observe(&state, |_this, _state, cx| cx.notify());
        let themed = Themed::new(&config, cx);
        Self {
            state,
            config,
            themed,
            seek_bar_bounds: Rc::new(Cell::new(Bounds::default())),
            _observe: observe,
        }
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

impl Container for PlaybackView {
    fn container_id() -> &'static str {
        "playback"
    }
}

const BAR_WIDTH: f32 = 480.0;
const BAR_HEIGHT: f32 = 40.0;
const BAR_GAP: f32 = 1.0;

impl Render for PlaybackView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let style = self.config.read(cx).waveform();

        // Snapshot the shared state once, then drop the borrow before
        // building elements.
        let (position, duration, can_prev, can_next, ended, playing, peaks) = {
            let state = self.state.read(cx);
            (
                state.position(),
                state.duration(),
                state.can_prev(),
                state.can_next(),
                state.ended(),
                state.is_playing(),
                state.peaks(),
            )
        };

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
                    .child(render_waveform(peaks, style, fraction, theme)),
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

        // The waveform sits on top, the time labels under it, and the transport
        // controls beneath both.
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
                    .justify_between()
                    .items_center()
                    .w(px(BAR_WIDTH))
                    .text_size(px(theme.small_px()))
                    .text_color(theme.text_muted)
                    .child(format_duration(position))
                    // The live bitrate now rides on the playing track's row in
                    // the list views, next to its duration.
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_muted)
                            .child(format_duration(duration)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .items_center()
                    .justify_center()
                    .child(transport_button(
                        "prev",
                        can_prev,
                        PREV_POLYGONS,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.dispatch(InputAction::PrevTrack, cx);
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
                            .bg(theme.text)
                            .when(!ended, |d| d.hover(|d| d.bg(theme.text_muted)))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.dispatch(InputAction::TogglePause, cx);
                            }))
                            .child(div().flex().items_center().child(if playing {
                                icon(13.0, 16.0, PAUSE_POLYGONS, theme.panel_bg).into_any_element()
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
                        NEXT_POLYGONS,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.dispatch(InputAction::NextTrack, cx);
                        }),
                    )),
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
    polygons: &'static [&'static [(f32, f32)]],
    theme: Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .when(enabled, |d| d.cursor_pointer())
        .when(!enabled, |d| d.opacity(0.35))
        .size(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .when(enabled, |d| d.hover(|d| d.bg(theme.row_active)))
        .on_click(on_click)
        .child(icon(15.0, 14.0, polygons, theme.text_muted))
}

/// A fixed-size canvas painting filled polygons given in unit-square
/// coordinates, scaled into the element's bounds.
fn render_waveform(
    peaks: Option<Arc<Vec<f32>>>,
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
                    .bg(theme.waveform_played),
            )
            .into_any_element(),
        WaveformStyle::Bars => match peaks {
            Some(peaks) => waveform_bars(peaks, progress, theme),
            // Still analyzing (or unavailable): leave the bar empty rather
            // than flashing a placeholder shape that then swaps.
            None => div().size_full().into_any_element(),
        },
    }
}

/// Paint the bars in a single canvas pass.
///
/// One element with one paint callback, instead of one `div` per bar — the
/// element-tree cost is constant no matter how many buckets we use, so the
/// bucket count can grow toward per-pixel resolution without a frame cost.
fn waveform_bars(peaks: Arc<Vec<f32>>, progress: f32, theme: Theme) -> AnyElement {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, _prepaint, window, _cx| {
            let count = peaks.len();
            if count == 0 {
                return;
            }
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let slot = width / count as f32;
            let bar_width = (slot - BAR_GAP).max(1.0);
            let origin_x = f32::from(bounds.origin.x);
            let origin_y = f32::from(bounds.origin.y);

            for (i, &peak) in peaks.iter().enumerate() {
                let bar_height = (peak.max(0.04) * height).max(1.0);
                let x = origin_x + i as f32 * slot + (slot - bar_width) / 2.0;
                let y = origin_y + (height - bar_height);
                let bar = Bounds::new(point(px(x), px(y)), size(px(bar_width), px(bar_height)));
                let played = (i as f32 / count as f32) < progress;
                let color = if played { theme.waveform_played } else { theme.waveform };
                window.paint_quad(fill(bar, color));
            }
        },
    )
    .size_full()
    .into_any_element()
}
