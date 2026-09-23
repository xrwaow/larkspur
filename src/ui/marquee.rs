//! The marquee: a text cell that fades, slides, and snaps back.
//!
//! A title or artist longer than its column is clipped rather than ellipsised:
//! the overflow fades out over one character at the right edge, and on hover the
//! text slides left so the rest can be read. On release it snaps back — a stiff,
//! under-damped spring, so it overshoots the start and settles, which reads as
//! the text being *thrown* back into place.
//!
//! The slide **out** is deliberately not a spring: a spring's speed scales with
//! the distance it has to cover, so a long title would whip past while a short
//! one crawled. Instead the offset moves at a constant [`SLIDE_PX_PER_SEC`], so
//! every title reads at the same pace. Only the snap back is sprung.
//!
//! The state is a [`Slide`] (the offset), driven either by a [`Marquee`] — a
//! standalone title, like the lyrics panel's — or by a list row, whose hover the
//! [`RowList`](crate::ui::row_list::RowList) already tracks.

use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, AnyElement, App, ElementId, Hsla,
    Rgba, Window,
};

use crate::ui::animation::Spring;
use crate::ui::theme::Theme;

/// How fast the text slides on hover, in px per second. Constant, so the pace
/// doesn't depend on how far the text has to travel.
pub const SLIDE_PX_PER_SEC: f32 = 150.0;

/// Below this the slide counts as settled.
const SLIDE_EPSILON: f32 = 0.01;

/// A text cell's horizontal offset, animated at a constant speed on the way out
/// and sprung back on release.
#[derive(Clone, Copy, Debug)]
pub struct Slide {
    /// The current offset, in px. Negative when slid left.
    offset: f32,
    /// Where the offset is heading: 0 at rest, `-travel` when hovered.
    target: f32,
    /// Whether the release spring is driving the offset.
    releasing: bool,
    spring: Spring,
}

impl Slide {
    pub fn new() -> Self {
        Self { offset: 0.0, target: 0.0, releasing: false, spring: Spring::bouncy(0.0) }
    }

    /// The current offset, in px.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    pub fn is_animating(&self) -> bool {
        if self.releasing {
            self.spring.is_animating()
        } else {
            (self.target - self.offset).abs() > SLIDE_EPSILON
        }
    }

    /// Aim at `target` px. A move to zero (the release) springs back; a move out
    /// slides at a constant speed.
    pub fn aim(&mut self, target: f32) {
        self.target = target;
        if target == 0.0 && self.offset != 0.0 {
            // Start the release once. A repeated exit event must not reset the
            // spring's velocity, or the snap back would stutter.
            if !self.releasing {
                self.releasing = true;
                self.spring.set(self.offset);
                self.spring.target(0.0);
            }
        } else {
            self.releasing = false;
        }
    }

    /// Advance the slide. Returns whether it moved.
    pub fn tick(&mut self, dt: f32) -> bool {
        if self.releasing {
            let moved = self.spring.tick(dt);
            self.offset = self.spring.value();
            if !self.spring.is_animating() {
                self.offset = 0.0;
                self.releasing = false;
            }
            moved
        } else {
            let delta = self.target - self.offset;
            if delta.abs() <= SLIDE_EPSILON {
                self.offset = self.target;
                return false;
            }
            let step = SLIDE_PX_PER_SEC * dt;
            if delta.abs() <= step {
                self.offset = self.target;
            } else {
                self.offset += step * delta.signum();
            }
            true
        }
    }
}

impl Default for Slide {
    fn default() -> Self {
        Self::new()
    }
}

/// A standalone marquee: a [`Slide`] plus the hover state that drives it. A list
/// row doesn't need this — its hover is already tracked by the list.
#[derive(Clone, Copy, Debug)]
pub struct Marquee {
    slide: Slide,
    hovered: bool,
}

impl Marquee {
    pub fn new() -> Self {
        Self { slide: Slide::new(), hovered: false }
    }

    /// Note whether the pointer is over the text, and how far the current text
    /// overflows its column (`travel`, in px). Returns whether the target
    /// changed, so a caller can skip a pointless re-render.
    pub fn set_hovered(&mut self, hovered: bool, travel: f32) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        self.slide.aim(if hovered { -travel } else { 0.0 });
        true
    }

    /// Advance the slide. Returns whether it moved, so the view only re-renders
    /// while something is animating.
    pub fn tick(&mut self, dt: f32) -> bool {
        self.slide.tick(dt)
    }

    /// The current offset, in px.
    pub fn offset(&self) -> f32 {
        self.slide.offset()
    }
}

impl Default for Marquee {
    fn default() -> Self {
        Self::new()
    }
}

/// One character's advance in `theme`'s font, in px. The font is monospace, so
/// this is what turns a character count into a column width.
pub fn char_advance(window: &Window, theme: Theme) -> f32 {
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&gpui::font(theme.font));
    text_system
        .ch_advance(font_id, px(theme.cell_px()))
        .map(f32::from)
        .unwrap_or(theme.cell_px() * 0.6)
}

/// How far `text` overflows a column of `width` px sized for `max_chars`
/// characters, in px. Zero when it fits.
pub fn travel_for(text: &str, width: f32, max_chars: usize) -> f32 {
    let advance = width / max_chars.max(1) as f32;
    ((text.chars().count() as f32) - max_chars as f32).max(0.0) * advance
}

/// Render `text` in a fixed-width cell of `width` px, sized for `max_chars`
/// characters: clipped, faded at the edges when it overflows, and drawn at
/// `offset` px (the slide).
///
/// `background` is the colour behind the cell — the fade is a gradient of it, so
/// the text appears to dissolve into the row rather than into a coloured band.
/// `on_hover` is wired to the cell itself, so each cell slides only while the
/// pointer is over *it*.
#[allow(clippy::too_many_arguments)]
pub fn marquee_text(
    theme: Theme,
    id: impl Into<ElementId>,
    text: &str,
    width: f32,
    max_chars: usize,
    offset: f32,
    color: Rgba,
    background: Rgba,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let advance = width / max_chars.max(1) as f32;
    let travel = travel_for(text, width, max_chars);

    let mut cell = div()
        .id(id)
        .w(px(width))
        .flex_none()
        .overflow_hidden()
        .relative()
        .on_hover(on_hover)
        .child(
            // `relative` (not `absolute`) so the text still gives the cell its
            // height; `left` slides it and the cell's `overflow_hidden` clips it.
            div()
                .relative()
                .left(px(offset))
                .whitespace_nowrap()
                .text_size(px(theme.cell_px()))
                .text_color(color)
                .child(text.to_string()),
        );

    if travel > 0.5 {
        // The overflow fades out over one character at the right edge.
        cell = cell.child(edge_fade(background, advance, Edge::Right));
    }
    if offset < -0.5 {
        // Once the text has slid, its start fades in at the left edge.
        cell = cell.child(edge_fade(background, advance, Edge::Left));
    }
    cell.into_any_element()
}

/// Which edge of a cell an [`edge_fade`] sits on.
#[derive(Clone, Copy)]
enum Edge {
    Left,
    Right,
}

/// A one-character gradient of `background`, transparent at the inner edge and
/// opaque at the outer one, so the text under it dissolves into the row.
fn edge_fade(background: Rgba, width: f32, edge: Edge) -> AnyElement {
    let opaque = Hsla::from(background);
    let clear = opaque.alpha(0.0);
    // 90° runs left→right, so the right edge fades from clear to the background
    // and the left edge the other way round.
    let (from, to) = match edge {
        Edge::Right => (clear, opaque),
        Edge::Left => (opaque, clear),
    };
    div()
        .absolute()
        .top(px(0.0))
        .h_full()
        .w(px(width))
        .when(matches!(edge, Edge::Right), |d| d.right(px(0.0)))
        .when(matches!(edge, Edge::Left), |d| d.left(px(0.0)))
        .bg(linear_gradient(90.0, linear_color_stop(from, 0.0), linear_color_stop(to, 1.0)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_has_no_travel() {
        // 10 chars in a 20-char column: nothing to slide.
        assert_eq!(travel_for("short", 200.0, 20), 0.0);
    }

    #[test]
    fn travel_is_the_overflow_in_px() {
        // 30 chars in a 20-char, 200px column: 10 chars * 10px each.
        assert_eq!(travel_for(&"x".repeat(30), 200.0, 20), 100.0);
    }

    #[test]
    fn the_slide_out_runs_at_a_constant_speed() {
        // Two slides of different lengths cover the same px per second, so the
        // pace doesn't depend on the distance.
        let mut short = Slide::new();
        short.aim(-50.0);
        let mut long = Slide::new();
        long.aim(-500.0);

        let dt = 1.0 / 60.0;
        short.tick(dt);
        long.tick(dt);
        assert!((short.offset() - long.offset()).abs() < 0.001, "same step regardless of travel");
        assert!((short.offset() + SLIDE_PX_PER_SEC * dt).abs() < 0.001);
    }

    #[test]
    fn the_slide_out_stops_exactly_on_its_target() {
        // No overshoot at the end: it lands on the target and stays.
        let mut slide = Slide::new();
        slide.aim(-100.0);
        for _ in 0..600 {
            if !slide.tick(1.0 / 60.0) {
                break;
            }
        }
        assert_eq!(slide.offset(), -100.0);
        assert!(!slide.is_animating());
    }

    #[test]
    fn the_release_springs_back_past_the_start() {
        // The snap back overshoots zero (pushing the text right) before settling.
        let mut slide = Slide::new();
        slide.aim(-100.0);
        for _ in 0..600 {
            if !slide.tick(1.0 / 60.0) {
                break;
            }
        }
        slide.aim(0.0);
        let mut peak: f32 = 0.0;
        for _ in 0..600 {
            if !slide.tick(1.0 / 60.0) {
                break;
            }
            peak = peak.max(slide.offset());
        }
        assert!(peak > 1.0, "it overshoots the start, peaking at {peak}");
        assert_eq!(slide.offset(), 0.0, "then settles at rest");
    }
}
