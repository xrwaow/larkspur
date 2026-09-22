//! Small view helpers shared across containers.
//!
//! These were copy-pasted between the rail and the list views; they live here
//! so a change to a section label or a panel header lands everywhere at once.

use gpui::{div, prelude::*, px, AnyElement, Rgba, ScrollWheelEvent, Window};

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
pub fn empty_hint(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .text_size(px(theme.small_px()))
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
/// A panel's top row: a title on the left, a hint on the right.
pub fn panel_header(theme: Theme, title: &str, hint: &str, hint_color: Rgba) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3()
        .child(
            div()
                .text_size(px(theme.cell_px() + 2.0))
                .text_color(theme.text)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(theme.small_px()))
                .text_color(hint_color)
                .child(hint.to_string()),
        )
        .into_any_element()
}
