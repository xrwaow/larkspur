//! Small view helpers shared across containers.
//!
//! These were copy-pasted between the rail and the list views; they live here
//! so a change to a section label or a panel header lands everywhere at once.

use gpui::{
    div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, size, AnyElement, App,
    Bounds, FocusHandle, Hsla, MouseDownEvent, MouseButton, Pixels, Rgba, ScrollWheelEvent,
    Window,
};

use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;

/// A section label: an uppercased, faint caption above a group of rows.
pub fn section_header(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_2()
        .pt_3()
        .pb_1()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
        .into_any_element()
}

/// A faint one-line hint — what an empty section shows instead of rows.
/// `full` gives a whole empty panel's roomier padding and cell-sized text;
/// the compact one sits inside a row list.
pub fn empty_hint(theme: Theme, label: &str, full: bool) -> AnyElement {
    div()
        .when(full, |d| d.px_4().py_6())
        .when(!full, |d| d.px_2().py_1())
        .text_size(px(if full { theme.cell_px() } else { theme.small_px() }))
        .text_color(theme.text_faint)
        .child(label.to_string())
        .into_any_element()
}

/// A flex-filling area whose vertical wheel scrolling is handed to `on_wheel`
/// instead of being applied immediately, so the caller can ease it.
///
/// The wheel is caught by a transparent overlay placed *after* the content in
/// the tree. Listeners fire in reverse registration order, and an element's
/// listeners are registered before its children's — so a parent would run
/// *after* the inner `gpui::list`'s own scroll handler, too late to stop it. A
/// later sibling runs first and can. It must not occlude, or it would swallow
/// the rows' hover and clicks; a plain hitbox leaves them reachable.
pub fn scroll_area(
    content: AnyElement,
    on_wheel: impl Fn(&ScrollWheelEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(content)
        .child(div().absolute().inset_0().on_scroll_wheel(on_wheel))
        .into_any_element()
}

/// The vertical pixels a wheel event scrolled, in GPUI's sign convention (a
/// positive value scrolls up). [`RowList::wheel`](crate::ui::row_list::RowList::wheel)
/// takes it as-is and does the one negation, so don't negate it here too.
pub fn wheel_pixels(event: &ScrollWheelEvent) -> f32 {
    f32::from(event.delta.pixel_delta(px(20.0)).y)
}

/// Linear blend from `from` to `to`, for a highlight fading in.
pub fn blend(from: Rgba, to: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: from.a + (to.a - from.a) * t,
    }
}

/// Which edge of a container an [`edge_fade`] sits against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

/// A gradient of `background` over `length` px at one `side` of a container —
/// opaque at the outer edge, transparent toward the inside — so content under it
/// dissolves into the background instead of being cut off.
///
/// Absolutely positioned, so drop it in as a child of a `relative` container. A
/// horizontal fade spans the full height, a vertical one the full width.
pub fn edge_fade(background: Rgba, length: f32, side: Side) -> AnyElement {
    let opaque = Hsla::from(background);
    let clear = opaque.alpha(0.0);
    // Angles run to top/right/bottom/left, so 90° is left→right and 180°
    // top→bottom. The outer edge is the opaque end, the inside the clear one.
    let (from, to, angle) = match side {
        Side::Left => (opaque, clear, 90.0),
        Side::Right => (clear, opaque, 90.0),
        Side::Top => (opaque, clear, 180.0),
        Side::Bottom => (clear, opaque, 180.0),
    };
    let horizontal = matches!(side, Side::Left | Side::Right);
    div()
        .absolute()
        .when(horizontal, |d| d.w(px(length)).h_full().top(px(0.0)))
        .when(!horizontal, |d| d.h(px(length)).w_full().left(px(0.0)).right(px(0.0)))
        .when(matches!(side, Side::Left), |d| d.left(px(0.0)))
        .when(matches!(side, Side::Right), |d| d.right(px(0.0)))
        .when(matches!(side, Side::Top), |d| d.top(px(0.0)))
        .when(matches!(side, Side::Bottom), |d| d.bottom(px(0.0)))
        .bg(linear_gradient(angle, linear_color_stop(from, 0.0), linear_color_stop(to, 1.0)))
        .into_any_element()
}

/// A panel's top row: a title on the left (optionally with a leading button
/// and a secondary line beneath it), optional header tools filling the middle
/// (a filter box, action buttons), and a faint hint on the right.
///
/// `title_px` carries the deliberate center/rail split: the center panels
/// headline at `cell_px() + 2.0`, the rail bands stay at `cell_px()`.
#[allow(clippy::too_many_arguments)]
pub fn panel_header(
    theme: Theme,
    title: &str,
    title_px: f32,
    leading: Option<AnyElement>,
    subtitle: Option<&str>,
    hint: Option<(&str, Rgba)>,
    tools: Option<AnyElement>,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3()
        .child(
            div()
                .flex()
                .items_center()
                .flex_1()
                .min_w_0()
                .gap_2()
                .when_some(leading, |d, leading| d.child(leading))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(title_px))
                                .text_color(theme.text)
                                .child(title.to_string()),
                        )
                        .when_some(subtitle, |d, subtitle| {
                            d.child(
                                div()
                                    .text_size(px(theme.small_px()))
                                    .text_color(theme.text_muted)
                                    .child(subtitle.to_string()),
                            )
                        }),
                )
                .when_some(tools, |d, tools| d.child(tools)),
        )
        .when_some(hint, |d, (hint, color)| {
            d.child(
                div()
                    .flex_none()
                    .text_size(px(theme.small_px()))
                    .text_color(color)
                    .child(hint.to_string()),
            )
        })
        .into_any_element()
}

/// A rounded search box: the magnifier, the field's editable text, a dimmed
/// `hint` while it's empty and unfocused, and — once there's text — either a
/// `×` clear button (`on_clear`) or a custom `trailing` element. `accented`
/// highlights the border — the search overlay marks its pinned-open cheat
/// sheet with it.
///
/// `focus` is the handle that owns the field: it's tracked on the chrome, so
/// clicking anywhere in the box focuses the field (and typing lands in it),
/// while clicking anywhere else leaves it blurred.
#[allow(clippy::too_many_arguments)]
pub fn search_box(
    theme: Theme,
    field: &TextField,
    focus: &FocusHandle,
    placeholder: Option<&str>,
    accented: bool,
    hint: Option<&'static str>,
    on_clear: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>>,
    trailing: Option<AnyElement>,
    window: &Window,
) -> AnyElement {
    let trailing = match on_clear {
        Some(on_clear) if !field.is_empty() => Some(
            div()
                .flex_none()
                .px_1()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .hover(|d| d.bg(theme.row_hover).text_color(theme.text))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    on_clear(event, window, cx)
                })
                .child("×")
                .into_any_element(),
        ),
        _ => trailing,
    };
    let mut box_el = div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme.row_odd)
        .border_1()
        .border_color(if accented { theme.accent } else { theme.border })
        .track_focus(focus)
        .child(field.render_text(theme, placeholder, focus, window));
    if hint.is_some() && field.is_empty() && !focus.is_focused(window) {
        box_el = box_el.child(
            div()
                .flex_none()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(hint.unwrap().to_string()),
        );
    }
    if let Some(trailing) = trailing {
        box_el = box_el.child(trailing);
    }
    box_el.into_any_element()
}

/// Gap between painted bars, in px.
pub const BAR_GAP: f32 = 1.0;

/// Paint a strip of bottom-aligned bars in one pass — the transport's
/// waveform and the visualizer's spectrum share this geometry. `values` are
/// 0.0–1.0 heights spread evenly across `bounds`, with [`BAR_GAP`] between
/// them; `floor` keeps a tiny baseline visible instead of a flat empty strip;
/// `color` picks per bar (the waveform's played/unplayed split).
///
/// One element with one paint callback, instead of one `div` per bar — the
/// element-tree cost is constant no matter how many buckets we use, so the
/// bucket count can grow toward per-pixel resolution without a frame cost.
pub fn paint_bars(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    values: &[f32],
    floor: f32,
    color: impl Fn(usize) -> Rgba,
) {
    let count = values.len();
    if count == 0 {
        return;
    }
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let slot = width / count as f32;
    let bar_width = (slot - BAR_GAP).max(1.0);
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);

    for (i, &value) in values.iter().enumerate() {
        let bar_height = (value.max(floor) * height).max(1.0);
        let x = origin_x + i as f32 * slot + (slot - bar_width) / 2.0;
        let y = origin_y + (height - bar_height);
        let bar = Bounds::new(point(px(x), px(y)), size(px(bar_width), px(bar_height)));
        window.paint_quad(fill(bar, color(i)));
    }
}
