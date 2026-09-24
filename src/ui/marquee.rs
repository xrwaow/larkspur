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
//! The state is a [`Marquee`] — a [`Slide`] (the offset) plus the hover that
//! drives it. A standalone title (the lyrics panel's) owns one directly, and a
//! list row keeps one per title/artist cell, so the two paths share a type.

use gpui::{div, prelude::*, px, AnyElement, App, ElementId, Rgba, Window};

use crate::ui::animation::Spring;
use crate::ui::theme::Theme;
use crate::ui::widgets::{edge_fade, Side};

/// How fast the text slides on hover, in px per second. Constant, so the pace
/// doesn't depend on how far the text has to travel.
pub const SLIDE_PX_PER_SEC: f32 = 150.0;

/// The snap back's spring: stiff and under-damped, so the text *snaps* back past
/// its start rather than easing home. The slide *out* is not a spring — it runs
/// at a constant speed ([`SLIDE_PX_PER_SEC`]) so a long title doesn't whip past.
const SNAP_BACK_STIFFNESS: f32 = 400.0;
const SNAP_BACK_DAMPING: f32 = 16.0;

/// Below this the slide counts as settled.
const SLIDE_EPSILON: f32 = 0.01;

/// A text cell's horizontal offset, animated at a constant speed on the way out
/// and sprung back on release.
#[derive(Clone, Copy, Debug)]
struct Slide {
    /// The current offset, in px. Negative when slid left.
    offset: f32,
    /// Where the offset is heading: 0 at rest, `-travel` when hovered.
    target: f32,
    /// Whether the release spring is driving the offset.
    releasing: bool,
    spring: Spring,
}

impl Slide {
    fn new() -> Self {
        Self {
            offset: 0.0,
            target: 0.0,
            releasing: false,
            spring: Spring::with_params(0.0, SNAP_BACK_STIFFNESS, SNAP_BACK_DAMPING),
        }
    }

    /// The current offset, in px.
    fn offset(&self) -> f32 {
        self.offset
    }

    fn is_animating(&self) -> bool {
        if self.releasing {
            self.spring.is_animating()
        } else {
            (self.target - self.offset).abs() > SLIDE_EPSILON
        }
    }

    /// Aim at `target` px. A move to zero (the release) springs back; a move out
    /// slides at a constant speed.
    fn aim(&mut self, target: f32) {
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
    fn tick(&mut self, dt: f32) -> bool {
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

/// A [`Slide`] plus the hover state that drives it: a standalone title owns one
/// directly, and a list row keeps one per title/artist cell.
#[derive(Clone, Copy, Debug, Default)]
pub struct Marquee {
    slide: Slide,
    hovered: bool,
}

impl Marquee {
    pub fn new() -> Self {
        Self::default()
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

    /// Whether the slide is still in motion.
    pub fn is_animating(&self) -> bool {
        self.slide.is_animating()
    }

    /// Whether the pointer is currently over the text.
    pub fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// The current offset, in px.
    pub fn offset(&self) -> f32 {
        self.slide.offset()
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
        cell = cell.child(edge_fade(background, advance, Side::Right));
    }
    if offset < -0.5 {
        // Once the text has slid, its start fades in at the left edge.
        cell = cell.child(edge_fade(background, advance, Side::Left));
    }
    cell.into_any_element()
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
