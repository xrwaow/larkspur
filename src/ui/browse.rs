//! The library browse view — every album as a headed group, with its tracks
//! underneath.
//!
//! This is the container from the mock. It's the "easy search": no query, no
//! parsing — it calls [`albums::all_sections`] and draws the result through
//! GPUI's virtualized [`list`], so a multi-thousand-song library only ever draws
//! the rows near the viewport (plus a little overdraw) instead of all of them.
//! The advanced search container renders the very same grouping from a parsed
//! query, and opening a playlist renders one section of it.
//!
//! Selection, keyboard navigation, paging, and the highlight fade all live in
//! [`RowList`]; this view only says that activating a row plays the album from
//! it.

use gpui::{
    div, list, prelude::*, px, ClickEvent, Context, Entity, FocusHandle, KeyDownEvent,
    MouseDownEvent, Render, ScrollWheelEvent, Subscription, Window,
};

use crate::model::{InputAction, PlaylistId, SongId};
use crate::ui::albums::{self, RowActions};
use crate::ui::animation::Animator;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::input::action_for_key;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::row_list::{RowAction, RowList};
use crate::ui::state::PlaybackState;
use crate::ui::widgets::{scroll_area, wheel_pixels};

pub struct BrowseView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    themed: Themed,
    rows: RowList,
    /// When set, the library is scoped to this artist's discography.
    artist_filter: Option<String>,
    /// The library revision the rows were last built from, so a highlight-only
    /// notify (a tab switch) doesn't rebuild them.
    seen_revision: u64,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_playback: Subscription,
    _observe_covers: Subscription,
    _observe_animator: Subscription,
}

impl BrowseView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, state, cx| {
            let revision = state.read(cx).revision();
            if this.seen_revision != revision {
                this.seen_revision = revision;
                this.rows.mark_dirty();
            }
            cx.notify();
        });
        let observe_playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        let observe_covers = cx.observe(&covers, |_this, _state, cx| cx.notify());
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            if this.rows.tick(animator.read(cx).dt()) {
                cx.notify();
            }
        });
        let themed = Themed::new(&config, cx);
        Self {
            library,
            playback,
            covers,
            themed,
            rows: RowList::new(),
            artist_filter: None,
            seen_revision: 0,
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_playback: observe_playback,
            _observe_covers: observe_covers,
            _observe_animator: observe_animator,
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
            self.rows.mark_dirty();
            cx.notify();
        }
    }

    fn dispatch(&mut self, action: InputAction, cx: &mut Context<Self>) {
        match self.rows.handle(action) {
            RowAction::Play(playlist, index) => self.play(playlist, index, cx),
            RowAction::Handled => cx.notify(),
            RowAction::Ignored => {}
        }
    }

    fn play(&mut self, playlist: PlaylistId, index: usize, cx: &mut Context<Self>) {
        let playback = self.playback.clone();
        self.library.update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
    }

    /// Snapshot the library into sections, scoped to the artist filter when one
    /// is set.
    fn build_sections(&self, cx: &Context<Self>) -> Vec<albums::AlbumSection> {
        let mut sections = albums::all_sections(self.library.read(cx).library());
        if let Some(artist) = &self.artist_filter {
            sections.retain(|section| section_matches_artist(section, artist));
            // A discography reads newest release first, undated albums last.
            sort_by_year(&mut sections);
        }
        sections
    }
}

impl Container for BrowseView {
    fn container_id() -> &'static str {
        "browse"
    }
}

/// How the browse view reacts to a row click: play the album from there, or
/// extend/toggle the selection with a modifier.
struct BrowseRows;

impl RowActions<BrowseView> for BrowseRows {
    fn activate(
        &self,
        view: &mut BrowseView,
        item_ix: usize,
        playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<BrowseView>,
    ) {
        // A modifier extends or toggles the selection; a plain click plays.
        if view.rows.click(item_ix, event.modifiers()) {
            cx.notify();
            return;
        }
        view.play(playlist, index, cx);
    }

    fn context(
        &self,
        view: &mut BrowseView,
        item_ix: usize,
        _song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<BrowseView>,
    ) {
        let songs = view.rows.context_songs(item_ix);
        let request = SongMenuRequest { songs, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }

    fn hover(
        &self,
        view: &mut BrowseView,
        item_ix: usize,
        hovered: bool,
        cx: &mut Context<BrowseView>,
    ) {
        if view.rows.set_hover(item_ix, hovered) {
            cx.notify();
        }
    }
}

impl Render for BrowseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        let fresh = self.rows.is_dirty().then(|| self.build_sections(cx));
        self.rows.sync(theme, window, fresh);

        let sections = self.rows.sections();
        let items = self.rows.items();
        let title_cols = self.rows.title_cols();
        let list_state = self.rows.list_state().clone();

        let current = self.playback.read(cx).metadata().id;
        let live_bitrate = self.playback.read(cx).live_bitrate();
        let actions: std::rc::Rc<dyn RowActions<BrowseView>> = std::rc::Rc::new(BrowseRows);

        let songs: usize = sections.iter().map(|section| section.tracks.len()).sum();
        // An artist tab titles itself with the artist; the library tab is the
        // fallback. This is the header text, not the tab strip's (see
        // `TabsView::tab_title`) — they agree because both read the filter.
        let title = self.artist_filter.clone().unwrap_or_else(|| "Library".to_string());

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if let Some(action) = action_for_key(&event.keystroke) {
                    this.dispatch(action, cx);
                }
            }))
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
            let highlight = self.rows.highlight();
            let render = cx.processor({
                let sections = sections.clone();
                let items = items.clone();
                let title_cols = title_cols.clone();
                let actions = actions.clone();
                move |_this, ix: usize, _window: &mut Window, cx: &mut Context<BrowseView>| {
                    albums::render_item(
                        theme,
                        ix,
                        &sections,
                        &items,
                        &title_cols,
                        &covers,
                        &library,
                        current,
                        live_bitrate,
                        None,
                        &highlight,
                        &actions,
                        cx,
                    )
                }
            });
            root = root.child(scroll_area(
                list(list_state, render).size_full().into_any_element(),
                cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                    if this.rows.wheel(wheel_pixels(event)) {
                        cx.notify();
                    }
                    cx.stop_propagation();
                }),
            ));
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

/// Sort sections newest release first, with undated albums below the dated ones.
fn sort_by_year(sections: &mut [albums::AlbumSection]) {
    sections.sort_by(|a, b| match (a.year.parse::<u16>().ok(), b.year.parse::<u16>().ok()) {
        (Some(year_a), Some(year_b)) => year_b.cmp(&year_a).then_with(|| a.album.cmp(&b.album)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.album.cmp(&b.album),
    });
}

#[cfg(test)]
mod tests {
    use super::{section_matches_artist, sort_by_year};
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

    fn dated(album: &str, year: &str) -> AlbumSection {
        let mut section = section("A", &["A"]);
        section.album = album.to_string();
        section.year = year.to_string();
        section
    }

    #[test]
    fn discography_sorts_newest_first_with_undated_last() {
        let mut sections = vec![
            dated("Old", "1999"),
            dated("Undated", ""),
            dated("New", "2020"),
            dated("Middle", "2010"),
        ];
        sort_by_year(&mut sections);
        let order: Vec<&str> = sections.iter().map(|s| s.album.as_str()).collect();
        assert_eq!(order, vec!["New", "Middle", "Old", "Undated"]);
    }
}
