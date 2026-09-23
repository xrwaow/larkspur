//! Small view helpers shared across containers.
//!
//! These were copy-pasted between the rail and the list views; they live here
//! so a change to a section label or a panel header lands everywhere at once.

use gpui::{
    div, fill, point, prelude::*, px, size, AnyElement, Bounds, Pixels, Rgba, ScrollWheelEvent,
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

/// A panel's top row: a title on the left (optionally with a leading button
/// and a secondary line beneath it), a faint hint on the right.
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
                ),
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

/// A rounded search box: the magnifier, the field's editable text, and an
/// optional trailing button (a clear or help toggle). `accented` highlights
/// the border — the search overlay marks its pinned-open cheat sheet with it.
pub fn search_box(
    theme: Theme,
    field: &TextField,
    placeholder: Option<&str>,
    accented: bool,
    trailing: Option<AnyElement>,
) -> AnyElement {
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
        .child(
            div()
                .flex_none()
                .text_size(px(theme.cell_px()))
                .text_color(theme.accent)
                .child("⌕"),
        )
        .child(field.render_text(theme, placeholder));
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
