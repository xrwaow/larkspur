use gpui::{
    div, img, prelude::*, px, AnyElement, Context, Entity, ObjectFit, Render, Rgba,
    Subscription, Window,
};

use crate::model::{SongId, ThemeKind};
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::{CoverImage, CoverStore};
use crate::ui::playback_state::PlaybackState;

/// The now-playing cover square.
///
/// Shows the cover of whatever track the shared playback state is on —
/// playing or paused, at full brightness. Reads both the playback state (which track)
/// and the cover store (its decoded art), observing each so it re-renders when
/// the track changes or a decode lands.
///
/// It also feeds the dynamic theme: when that theme is selected it pushes the
/// current cover's dominant colour (which the cover store derives at decode
/// time) into [`ConfigState`], which every view reads its palette from. The
/// last colour is kept when a new track's art hasn't decoded yet.
pub struct CoverView {
    state: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    config: Entity<ConfigState>,
    themed: Themed,
    /// The last cover pushed to the dynamic theme, so a cover is only pushed
    /// once and the previous colour survives a track change.
    accent: Option<(SongId, Rgba)>,
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
            this.sync_accent(cx);
            cx.notify();
        });
        let observe_covers = cx.observe(&covers, |this, _covers, cx| {
            this.sync_accent(cx);
            cx.notify();
        });
        let observe_config = cx.observe(&config, |this, _config, cx| {
            this.sync_accent(cx);
            cx.notify();
        });
        let themed = Themed::new(&config, cx);

        let mut this = Self {
            state: state.clone(),
            covers,
            config,
            themed,
            accent: None,
            _observe: observe,
            _observe_covers: observe_covers,
            _observe_config: observe_config,
        };
        // A track may already be loaded before we start observing.
        this.request_current(&state, cx);
        this.sync_accent(cx);
        this
    }

    /// Ask the store for the current track's cover, if it isn't known yet.
    fn request_current(&mut self, state: &Entity<PlaybackState>, cx: &mut Context<Self>) {
        let Some(song) = state.read(cx).metadata(cx) else { return };
        self.covers
            .update(cx, |store, cx| store.request_full(song.id, song.path.clone(), song.has_art, cx));
    }

    /// Push the current cover's accent into the config, if the dynamic theme is
    /// selected. A no-op otherwise, so the theme never changes when the user
    /// hasn't asked for it.
    fn sync_accent(&mut self, cx: &mut Context<Self>) {
        if self.config.read(cx).theme_kind() != ThemeKind::Dynamic {
            return;
        }
        let Some(id) = self.state.read(cx).current_song() else { return };
        if self.accent.as_ref().is_some_and(|(cached, _)| *cached == id) {
            return;
        }
        let Some(accent) = self.covers.read(cx).accent(id) else {
            // Art not decoded yet: keep the previous colour rather than
            // flashing back to the default palette.
            return;
        };
        self.accent = Some((id, accent));
        self.config.update(cx, |config, cx| config.set_dynamic_accent(Some(accent), cx));
    }
}

impl Container for CoverView {
    fn container_id() -> &'static str {
        "cover"
    }
}

impl Render for CoverView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let id = self.state.read(cx).current_song();
        let cover = id.and_then(|id| self.covers.read(cx).full(id));

        let art: AnyElement = match cover {
            Some(CoverImage::Ready(image)) => img(image)
                .size_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            Some(CoverImage::Loading) => placeholder(theme, "Loading…"),
            // Nothing playing, no art, or no art on this file.
            Some(CoverImage::Missing) | None => placeholder(theme, "♪"),
        };

        div()
            .size(px(Self::SIZE))
            .bg(theme.row_odd)
            .overflow_hidden()
            .child(art)
    }
}

/// The stand-in shown while there's no art to draw.
fn placeholder(theme: crate::ui::theme::Theme, label: &str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .font_family(theme.font)
        .text_color(theme.text_faint)
        .child(label.to_string())
        .into_any_element()
}
