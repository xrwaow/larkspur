use gpui::{div, prelude::*, px, rgb, Context, Entity, Render, Subscription, Window};

use crate::model::CoverState;
use crate::ui::container::Container;
use crate::ui::state::PlaybackState;

/// The now-playing cover square.
///
/// Reads the shared playback state (rather than a `bool` passed in at
/// construction) so it can tell "this song has no art" apart from "art is
/// still loading" — and so it updates when the track changes.
pub struct CoverView {
    state: Entity<PlaybackState>,
    _observe: Subscription,
}

impl CoverView {
    /// The cover square's fixed size, in px. Shared with `ui::layout` so the
    /// text diagram can size the cover region accurately.
    pub const SIZE: f32 = 240.0;

    pub fn new(state: Entity<PlaybackState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_this, _state, cx| cx.notify());
        Self { state, _observe: observe }
    }
}

impl Container for CoverView {
    fn container_id() -> &'static str {
        "cover"
    }
}

impl Render for CoverView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = match &self.state.read(cx).metadata().cover {
            CoverState::Missing => "No Cover",
            CoverState::Loading => "Loading…",
            // Real decoding isn't wired up yet — `Ready` and `NotRequested`
            // both fall back to the placeholder glyph.
            CoverState::NotRequested | CoverState::Ready(_) => "♪",
        };

        div()
            .size(px(Self::SIZE))
            .bg(rgb(0x202020))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(0x555555))
            .child(label)
    }
}
