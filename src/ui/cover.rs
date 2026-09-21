use gpui::{
    div, img, prelude::*, px, rgb, AnyElement, Context, Entity, ObjectFit, Render, Subscription,
    Window,
};

use crate::model::{CoverState, SongStatus};
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::cover_store::{CoverImage, CoverStore};
use crate::ui::state::PlaybackState;
use crate::ui::theme::Theme;

/// The now-playing cover square.
///
/// Shows the cover of whatever track the shared playback state is on — playing
/// or paused — dimmed while paused. Reads both the playback state (which track)
/// and the cover store (its decoded art), observing each so it re-renders when
/// the track changes or a decode lands.
pub struct CoverView {
    state: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    theme: Theme,
    _observe: Subscription,
    _observe_covers: Subscription,
    _observe_config: Subscription,
}

impl CoverView {
    /// The cover square's fixed size, in px. Shared with `ui::layout` so the
    /// text diagram can size the cover region accurately.
    pub const SIZE: f32 = 240.0;

    pub fn new(
        state: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Request the cover whenever the track changes. `request` is a no-op
        // once an id is known, so this is cheap even at the ticker's rate.
        let observe = cx.observe(&state, |this, state, cx| {
            this.request_current(&state, cx);
            cx.notify();
        });
        let observe_covers = cx.observe(&covers, |_this, _covers, cx| cx.notify());
        let observe_config = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());

        let mut this = Self {
            state: state.clone(),
            covers,
            theme,
            _observe: observe,
            _observe_covers: observe_covers,
            _observe_config: observe_config,
        };
        // A track may already be loaded before we start observing.
        this.request_current(&state, cx);
        this
    }

    /// Ask the store for the current track's cover, if it isn't known yet.
    fn request_current(&mut self, state: &Entity<PlaybackState>, cx: &mut Context<Self>) {
        let (id, path, has_art) = {
            let state = state.read(cx);
            let song = state.metadata();
            (song.id, song.path.clone(), !matches!(song.cover, CoverState::Missing))
        };
        // `SongMetadata::placeholder` is id 0 with an empty path — nothing to
        // decode until a track is actually chosen.
        if id == 0 || path.as_os_str().is_empty() {
            return;
        }
        self.covers.update(cx, |store, cx| store.request(id, path, has_art, cx));
    }
}

impl Container for CoverView {
    fn container_id() -> &'static str {
        "cover"
    }
}

impl Render for CoverView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.state.read(cx).metadata().id;
        let paused = self.state.read(cx).status() == SongStatus::Paused;
        let cover = self.covers.read(cx).cover(id);

        let art: AnyElement = match cover {
            Some(CoverImage::Ready(image)) => img(image)
                .size_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            Some(CoverImage::Loading) => placeholder(self.theme, "Loading…"),
            // Nothing playing, no art, or no art on this file.
            Some(CoverImage::Missing) | None => placeholder(self.theme, "♪"),
        };

        div()
            .size(px(Self::SIZE))
            .bg(rgb(0x202020))
            .overflow_hidden()
            .when(paused, |d| d.opacity(0.45))
            .child(art)
    }
}

/// The stand-in shown while there's no art to draw.
fn placeholder(theme: Theme, label: &str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .font_family(theme.font)
        .text_color(rgb(0x555555))
        .child(label.to_string())
        .into_any_element()
}
