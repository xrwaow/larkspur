use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    canvas, div, point, prelude::*, px, relative, rgb, Bounds, Context, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, PathBuilder, Pixels, Render, Rgba, Window,
};

use crate::audio::PlaybackController;
use crate::ui::container::Container;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WaveformStyle {
    FlatLine,
    Bars,
}

pub struct PlaybackView {
    controller: PlaybackController,
    style: WaveformStyle,
    focus_handle: FocusHandle,
    // Filled in by the seek-bar's canvas each time it paints, so
    // on_mouse_down can turn a window-relative click into a
    // bar-relative fraction without guessing at layout position.
    seek_bar_bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl PlaybackView {
    pub fn new(controller: PlaybackController, cx: &mut Context<Self>) -> Self {
        // Drives the position display and auto-advances the queue when a
        // track finishes. Stops when the view is dropped.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if this
                    .update(cx, |this, cx| {
                        this.controller.tick_advance();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            controller,
            style: WaveformStyle::Bars,
            focus_handle: cx.focus_handle(),
            seek_bar_bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }

    /// Exposed so main.rs can grab OS/window keyboard focus onto this
    /// view right after creating it — without this, on_key_down never
    /// fires because nothing is focused by default.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn toggle_style(&mut self, cx: &mut Context<Self>) {
        self.style = match self.style {
            WaveformStyle::FlatLine => WaveformStyle::Bars,
            WaveformStyle::Bars => WaveformStyle::FlatLine,
        };
        cx.notify();
    }

    fn seek_to_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = self.controller.duration.mul_f32(fraction.clamp(0.0, 1.0));
        self.controller.seek(target);
        cx.notify();
    }

    fn seek_relative(&mut self, delta_secs: i64, cx: &mut Context<Self>) {
        self.controller.seek_relative(delta_secs);
        cx.notify();
    }
}

impl Container for PlaybackView {
    fn container_id() -> &'static str {
        "playback"
    }
}

impl Focusable for PlaybackView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const BAR_WIDTH: f32 = 480.0;
const BAR_HEIGHT: f32 = 40.0;

impl Render for PlaybackView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let position = self.controller.position();
        let duration = self.controller.duration;
        let fraction = if duration.as_secs_f32() > 0.0 {
            position.as_secs_f32() / duration.as_secs_f32()
        } else {
            0.0
        };

        let can_prev = self.controller.can_prev();
        let can_next = self.controller.can_next();
        let ended = self.controller.ended();
        let playing = self.controller.is_playing();

        let bounds_cell = self.seek_bar_bounds.clone();
        let bounds_for_paint = bounds_cell.clone();

        let seek_bar = div()
            .id("seek-bar")
            .w(px(BAR_WIDTH))
            .h(px(BAR_HEIGHT))
            .relative()
            .overflow_hidden()
            .bg(rgb(0x181818))
            .child(
                // canvas gives us the element's real on-screen bounds
                // every time it paints — that's what on_mouse_down
                // below reads to compute a click-relative fraction,
                // instead of assuming window coords == local coords.
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
                    .child(render_waveform(&self.controller.waveform, self.style, fraction)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    let bounds = bounds_cell.get();
                    let local_x: f32 = f32::from(event.position.x - bounds.origin.x);
                    this.seek_to_fraction(local_x / BAR_WIDTH, cx);
                }),
            );

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                match event.keystroke.key.as_str() {
                    "left" => this.seek_relative(-5, cx),
                    "right" => this.seek_relative(5, cx),
                    // No toggling once the queue has played out.
                    "space" if !this.controller.ended() => {
                        this.controller.toggle_play_pause();
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .p_4()
            .w_full()
            .bg(rgb(0x141414))
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
                        cx.listener(|this, _event, _window, cx| {
                            this.controller.prev_track();
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
                            .bg(rgb(0xffffff))
                            .when(!ended, |d| d.hover(|d| d.bg(rgb(0xdddddd))))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if !this.controller.ended() {
                                    this.controller.toggle_play_pause();
                                    cx.notify();
                                }
                            }))
                            .child(div().flex().items_center().child(if playing {
                                icon(13.0, 16.0, PAUSE_POLYGONS, rgb(0x101010)).into_any_element()
                            } else {
                                // Nudge the triangle right so it reads as
                                // optically centered in the circle.
                                div()
                                    .pl(px(2.0))
                                    .child(icon(14.0, 16.0, PLAY_POLYGONS, rgb(0x101010)))
                                    .into_any_element()
                            })),
                    )
                    .child(transport_button(
                        "next",
                        can_next,
                        NEXT_POLYGONS,
                        cx.listener(|this, _event, _window, cx| {
                            this.controller.next_track();
                            cx.notify();
                        }),
                    ))
                    .child(
                        div()
                            .id("style-toggle")
                            .cursor_pointer()
                            .ml_4()
                            .text_xs()
                            .text_color(rgb(0x707070))
                            .hover(|d| d.text_color(rgb(0xaaaaaa)))
                            .on_click(cx.listener(|this, _event, _window, cx| this.toggle_style(cx)))
                            .child(match self.style {
                                WaveformStyle::Bars => "bars",
                                WaveformStyle::FlatLine => "line",
                            }),
                    ),
            )
            .child(seek_bar)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .w(px(BAR_WIDTH))
                    .text_xs()
                    .text_color(rgb(0x707070))
                    .child(format_duration(position))
                    .child(format_duration(duration)),
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
        .when(enabled, |d| d.hover(|d| d.bg(rgb(0x222222))))
        .on_click(on_click)
        .child(icon(15.0, 14.0, polygons, rgb(0xcccccc)))
}

fn format_duration(d: Duration) -> String {
    format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

fn render_waveform(peaks: &[f32], style: WaveformStyle, progress: f32) -> impl IntoElement {
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
                    .bg(rgb(0x404040)),
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
                    .bg(rgb(0xffffff)),
            )
            .into_any_element(),
        WaveformStyle::Bars => div()
            .flex()
            .items_end()
            .gap(px(1.0))
            .h_full()
            .children(peaks.iter().enumerate().map(|(i, &peak)| {
                let played = (i as f32 / peaks.len() as f32) < progress;
                div()
                    .w(px(3.0))
                    .h(relative(peak.max(0.04)))
                    .bg(if played { rgb(0xffffff) } else { rgb(0x404040) })
            }))
            .into_any_element(),
    }
}
