//! The library browse view — every album as a headed group, with its tracks
//! underneath.
//!
//! This is the container from the mock. It's the "easy search": no query, no
//! parsing — it calls [`albums::all_sections`] and draws the result through
//! GPUI's virtualized [`list`], so a multi-thousand-song library only ever draws
//! the rows near the viewport (plus a little overdraw) instead of all of them.
//! The advanced search container renders the very same grouping from a parsed
//! query, and opening a playlist renders one section of it.

use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, ClickEvent, Context, Entity, FocusHandle, ListAlignment, ListState,
    MouseDownEvent, Render, Subscription, Window,
};

use crate::model::{PlaylistId, SongId};
use crate::ui::albums::{self, RowActions};
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::state::PlaybackState;
use crate::ui::theme::Theme;

/// Extra rows rendered above and below the viewport, so scrolling a little
/// doesn't pop rows (or their covers) in. Roughly ±50 rows.
const OVERDRAW_PX: f32 = 1200.0;

pub struct BrowseView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    theme: Theme,
    /// The current album sections. Rebuilt when the library changes, not every
    /// frame.
    sections: Rc<Vec<albums::AlbumSection>>,
    /// When set, the library is scoped to this artist's discography.
    artist_filter: Option<String>,
    /// Set when the library changes, so the sections are rebuilt.
    dirty: bool,
    /// Virtualized-list state, owned by the view as GPUI requires.
    list_state: ListState,
    /// The row count the list was last reset to.
    item_count: usize,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_playback: Subscription,
    _observe_covers: Subscription,
    _observe_config: Subscription,
}

impl BrowseView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, _state, cx| {
            this.dirty = true;
            cx.notify();
        });
        let observe_playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        let observe_covers = cx.observe(&covers, |_this, _state, cx| cx.notify());
        let observe_config = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
        Self {
            library,
            playback,
            covers,
            theme,
            sections: Rc::new(Vec::new()),
            artist_filter: None,
            dirty: true,
            list_state: ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX)),
            item_count: 0,
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_playback: observe_playback,
            _observe_covers: observe_covers,
            _observe_config: observe_config,
        }
    }

    /// Exposed so the tab container can hand this view keyboard focus.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Scope the library to `artist`'s discography (or clear the scope).
    pub fn set_artist_filter(&mut self, artist: Option<String>, cx: &mut Context<Self>) {
        if self.artist_filter != artist {
            self.artist_filter = artist;
            self.dirty = true;
            cx.notify();
        }
    }
}

impl Container for BrowseView {
    fn container_id() -> &'static str {
        "browse"
    }
}

/// How the browse view reacts to a row click: play the album from there.
struct BrowseRows;

impl RowActions<BrowseView> for BrowseRows {
    fn activate(
        &self,
        view: &mut BrowseView,
        playlist: PlaylistId,
        index: usize,
        _event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<BrowseView>,
    ) {
        let playback = view.playback.clone();
        view.library.update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
    }

    fn context(
        &self,
        view: &mut BrowseView,
        song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<BrowseView>,
    ) {
        let request = SongMenuRequest { song, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }
}

impl Render for BrowseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        if self.dirty {
            self.dirty = false;
            let filter = self.artist_filter.clone();
            let mut sections = {
                let state = self.library.read(cx);
                albums::all_sections(state.library())
            };
            if let Some(artist) = &filter {
                sections.retain(|section| section_matches_artist(section, artist));
            }
            self.sections = Rc::new(sections);
        }
        let sections = self.sections.clone();
        let items = Rc::new(albums::flatten(&sections));
        if self.item_count != items.len() {
            self.list_state.reset(items.len());
            self.item_count = items.len();
        }

        let current = self.playback.read(cx).metadata().id;
        let live_bitrate = self.playback.read(cx).live_bitrate();
        let actions: Rc<dyn RowActions<BrowseView>> = Rc::new(BrowseRows);

        let songs: usize = sections.iter().map(|section| section.tracks.len()).sum();
        // An artist tab titles itself with the artist; the library tab is the
        // fallback. This is the header text, not the tab strip's (see
        // `TabsView::tab_title`) — they agree because both read the filter.
        let title = self.artist_filter.clone().unwrap_or_else(|| "Library".to_string());

        let mut root = div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(
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
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(theme.cell_px() + 2.0))
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(theme.small_px()))
                                    .text_color(theme.text_muted)
                                    .child(format!("{} albums · {} songs", sections.len(), songs)),
                            ),
                    ),
            );

        if sections.is_empty() {
            root = root.child(
                div()
                    .px_4()
                    .py_6()
                    .text_size(px(theme.cell_px()))
                    .text_color(theme.text_faint)
                    .child("Nothing scanned yet — add a music directory in settings."),
            );
        } else {
            let covers = self.covers.clone();
            let library = self.library.clone();
            let render = cx.processor({
                let sections = sections.clone();
                let items = items.clone();
                let actions = actions.clone();
                move |_this, ix: usize, window: &mut Window, cx: &mut Context<BrowseView>| {
                    albums::render_item(
                        theme,
                        ix,
                        &sections,
                        &items,
                        &covers,
                        &library,
                        current,
                        live_bitrate,
                        None,
                        &actions,
                        window,
                        cx,
                    )
                }
            });
            root = root.child(list(self.list_state.clone(), render).flex_1().w_full().min_h_0());
        }

        root
    }
}

/// Whether a section belongs to `artist`'s discography: the album artist
/// matches, or any track credits them (so a compilation still shows).
fn section_matches_artist(section: &albums::AlbumSection, artist: &str) -> bool {
    section.artist == artist
        || section
            .tracks
            .iter()
            .any(|track| track.artist.split(", ").any(|name| name == artist))
}

#[cfg(test)]
mod tests {
    use super::section_matches_artist;
    use crate::model::PlaylistId;
    use crate::ui::albums::{AlbumSection, TrackRow};

    fn section(artist: &str, track_artists: &[&str]) -> AlbumSection {
        AlbumSection {
            playlist: PlaylistId(0),
            artist: artist.to_string(),
            album: String::new(),
            meta: String::new(),
            year: String::new(),
            cover_song: None,
            tracks: track_artists
                .iter()
                .enumerate()
                .map(|(i, a)| TrackRow {
                    song: i as u64,
                    number: i + 1,
                    title: String::new(),
                    artist: a.to_string(),
                    secs: 0,
                    nominal_bitrate: None,
                })
                .collect(),
        }
    }

    #[test]
    fn artist_filter_matches_album_artist_or_any_track() {
        assert!(section_matches_artist(&section("Grimes", &["Grimes"]), "Grimes"));
        // A compilation: album artist differs, but a track credits the artist.
        assert!(section_matches_artist(
            &section("Various Artists", &["Grimes", "Someone"]),
            "Grimes"
        ));
        assert!(!section_matches_artist(&section("Grimes", &["Grimes"]), "Laffey"));
        // A multi-artist track cell is split on commas.
        assert!(section_matches_artist(
            &section("X", &["Chris Lake, Grimes, NPC"]),
            "Grimes"
        ));
    }
}
