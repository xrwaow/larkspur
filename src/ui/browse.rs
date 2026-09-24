//! The library browse view — every album as a headed group, with its tracks
//! underneath.
//!
//! This is the container from the mock. It's the "easy search": no query, no
//! parsing — it walks the [`album_list::Source`] it was scoped to and draws
//! the result through GPUI's virtualized [`list`], so a multi-thousand-song
//! library only ever draws the rows near the viewport (plus a little overdraw)
//! instead of all of them. The whole list shell — selection, keyboard
//! navigation, paging, hover fades — lives in [`AlbumList`]; this view only
//! scopes the source (library, one artist's discography, a folder's
//! playlists) and draws its header.

use gpui::{div, prelude::*, Context, Entity, FocusHandle, KeyDownEvent, Render};

use crate::model::PlaylistId;
use crate::ui::album_list::{self, AlbumList, AlbumListSubs, AlbumListView, Play, Source};
use crate::ui::animation::Animator;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::playback_state::PlaybackState;
use crate::ui::widgets::{empty_hint, panel_header};

pub struct BrowseView {
    themed: Themed,
    list: AlbumListView,
    /// What the sections are built from (the library, an artist, a folder's
    /// playlists).
    source: Source,
    /// A folder scope's tab/header title.
    scope_title: Option<String>,
    focus_handle: FocusHandle,
    _observe: AlbumListSubs,
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
        let observe =
            album_list::observe(&library, Some(&playback), &covers, &animator, cx);
        let themed = Themed::new(&config, cx);
        Self {
            themed,
            list: AlbumListView::new(library, playback, covers, animator, Play::Playlist),
            source: Source::Library,
            scope_title: None,
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// Exposed so the tab container can hand this view keyboard focus.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Scope the library to `artist`'s discography (or clear the scope).
    pub fn set_artist_filter(&mut self, artist: Option<String>, cx: &mut Context<Self>) {
        let source = artist.map_or(Source::Library, Source::Artist);
        if self.source != source {
            self.source = source;
            self.scope_title = None;
            self.list.rows_mut().mark_dirty();
            cx.notify();
        }
    }

    /// Show exactly `playlists` as sections, titled `title` — the temporary
    /// view a played folder opens.
    pub fn set_playlist_scope(
        &mut self,
        title: String,
        playlists: Vec<PlaylistId>,
        cx: &mut Context<Self>,
    ) {
        self.source = Source::Playlists(playlists);
        self.scope_title = Some(title);
        self.list.rows_mut().mark_dirty();
        cx.notify();
    }
}

impl Container for BrowseView {
    fn container_id() -> &'static str {
        "browse"
    }
}

impl AlbumList for BrowseView {
    fn list(&self) -> &AlbumListView {
        &self.list
    }

    fn list_mut(&mut self) -> &mut AlbumListView {
        &mut self.list
    }
}

impl Render for BrowseView {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        let fresh = self.list.rows().is_dirty().then(|| {
            self.source.sections(self.list.library().read(cx).library())
        });
        self.list.rows_mut().sync(theme, window, fresh);
        self.list.anchor_to_playing(cx);

        let sections = self.list.rows().sections();
        let songs: usize = sections.iter().map(|section| section.tracks.len()).sum();
        // A scope titles its tab: a folder view by the folder, an artist tab by
        // the artist; the library tab is the fallback. This is the header text,
        // not the tab strip's (see `TabsView::tab_title`) — they agree because
        // both read the same scope.
        let title = match &self.source {
            Source::Library => "Library".to_string(),
            Source::Artist(artist) => artist.clone(),
            Source::Playlists(_) => self.scope_title.clone().unwrap_or_else(|| "Library".to_string()),
        };

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                album_list::list_key_listener(this, event, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(panel_header(
                theme,
                &title,
                theme.cell_px() + 2.0,
                None,
                Some(&format!("{} albums · {} songs", sections.len(), songs)),
                None,
            ));

        if sections.is_empty() {
            root = root.child(empty_hint(
                theme,
                "Nothing scanned yet — add a music directory in settings.",
                true,
            ));
        } else {
            root = root.child(album_list::render_rows(self, theme, None, cx));
        }

        root
    }
}
