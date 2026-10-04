//! The library browse view — every album as a headed group, with its tracks
//! underneath.
//!
//! This is the container from the mock. It's the "easy search": it walks the
//! [`Scope`] it was scoped to and draws the result through GPUI's virtualized
//! [`list`], so a multi-thousand-song library only ever draws the rows near the
//! viewport (plus a little overdraw) instead of all of them. The whole list
//! shell — selection, keyboard navigation, paging, hover fades — lives in
//! [`AlbumList`]; this view only scopes the source (library, one artist's
//! discography, a folder's playlists) and draws its header.
//!
//! The library and artist scopes carry the header's filter box (`ctrl+f`)
//! and shuffle button: the box live-filters the sections through the same
//! query engine the search panel uses — within the scope, so an artist tab
//! filters their discography — and the button plays the scope in random
//! order. Every scope has both: a folder view shuffles and filters the folder's
//! albums and singles as they stand right now.

use std::path::PathBuf;

use gpui::{div, prelude::*, px, Context, Entity, FocusHandle, KeyDownEvent, Render, Window};

use crate::model::{search, FolderMode, InputAction, Scope};
use crate::ui::album_list::{self, AlbumList, AlbumListSubs, AlbumListView, Play};
use crate::ui::albums;
use crate::ui::animation::Animator;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::input::action_for_key;
use crate::ui::library_state::LibraryState;
use crate::ui::playback_state::PlaybackState;
use crate::ui::playback::icon;
use crate::ui::text_field::TextField;
use crate::ui::widgets::{empty_hint, panel_header, search_box};

pub struct BrowseView {
    themed: Themed,
    list: AlbumListView,
    /// What the sections are built from (the library, an artist, a folder).
    scope: Scope,
    /// The scope's filter box, and its focus handle — `ctrl+f` lands here,
    /// and the box only takes keys while it's focused.
    search: TextField,
    search_focus: FocusHandle,
    focus_handle: FocusHandle,
    /// The shared config — read for the list/icon view mode (the flag itself
    /// is persisted there and changed by the bottom bar's toggle).
    config: Entity<ConfigState>,
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
            scope: Scope::Library,
            search: TextField::default(),
            search_focus: cx.focus_handle(),
            focus_handle: cx.focus_handle(),
            config: config.clone(),
            _observe: observe,
        }
    }

    /// Exposed so the tab container can hand this view keyboard focus.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// The theme for a view that renders the library's rows — the playlist,
    /// search, and queue tabs share the browse list's typography so they can't
    /// drift from it.
    pub fn themed<C: 'static>(config: &Entity<ConfigState>, cx: &mut Context<C>) -> Themed {
        Themed::with_container(Self::container_id(), Self::default_font_size(), config, cx)
    }

    /// Select the library's filter box — what `ctrl+f` lands on.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.search_focus);
        cx.notify();
    }

    /// Scope the library to `artist`'s discography (or clear the scope).
    pub fn set_artist_filter(&mut self, artist: Option<String>, cx: &mut Context<Self>) {
        let scope = artist.map_or(Scope::Library, Scope::Artist);
        if self.scope != scope {
            self.scope = scope;
            self.list.rows_mut().mark_dirty();
            cx.notify();
        }
    }

    /// Scope the view to a folder — the albums and singles under it, flattened
    /// across its subfolders — the view a played folder opens. Resolved live
    /// against the library, so a rescan updates the tab instead of stranding
    /// it; re-playing the same folder in the other mode re-scopes it.
    pub fn set_folder_scope(&mut self, dir: PathBuf, mode: FolderMode, cx: &mut Context<Self>) {
        let scope = Scope::Folder { dir, mode };
        if self.scope != scope {
            self.scope = scope;
            self.list.rows_mut().mark_dirty();
            cx.notify();
        }
    }
    /// Build the sections the list draws: the scope as-is, or — once the
    /// filter box has text — the scope's songs run through the query engine,
    /// grouped by album like the search panel's results.
    fn build_sections(&self, cx: &Context<Self>) -> Vec<albums::AlbumSection> {
        let library = self.list.library().read(cx).library();
        let query = if self.search.is_empty() {
            search::Query::default()
        } else {
            search::parse(&self.search.value).0
        };
        album_list::sections(library, &self.scope, &query)
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

        let fresh = self.list.rows().is_dirty().then(|| self.build_sections(cx));
        self.list.rows_mut().sync(theme, window, fresh);
        self.list.anchor_to_playing(cx);
        // A filter queues the matched songs only; the unfiltered library
        // queues the whole album from the clicked row.
        self.list.set_play(if self.search.is_empty() { Play::Playlist } else { Play::Result });

        let sections = self.list.rows().sections();
        let songs: usize = sections.iter().map(|section| section.tracks.len()).sum();
        // A scope titles its tab: a folder view by the folder, an artist tab by
        // the artist; the library tab is the fallback. This is the header text,
        // not the tab strip's (see `TabsView::tab_title`) — they agree because
        // both read the same scope.
        let title = match &self.scope {
            Scope::Library => "Library".to_string(),
            Scope::Artist(artist) => artist.clone(),
            // A folder view titles itself with the folder, like the tab strip
            // does (see `TabsView::tab_title`).
            Scope::Folder { dir, .. } => dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| dir.display().to_string()),
        };
        // The filter box's placeholder names its scope.
        let placeholder = match &self.scope {
            Scope::Artist(_) => "Filter the discography…",
            Scope::Folder { .. } => "Filter the folder…",
            _ => "Filter the library…",
        };

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Arrows and paging move the list even while the box is
                // focused; the box never sees them.
                if let Some(action) = action_for_key(&event.keystroke) {
                    if matches!(
                        action,
                        InputAction::SelectNext
                            | InputAction::SelectPrev
                            | InputAction::PageUp
                            | InputAction::PageDown
                    ) {
                        this.list.dispatch(action, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                // The filter box takes keys only while it's focused — selected
                // by clicking it or by `ctrl+f`. Blurred, everything bubbles:
                // `space` pauses, arrows scroll. `escape` clears it first.
                let field_focused = this.search_focus.is_focused(window);
                if event.keystroke.key == "escape" {
                    if field_focused && !this.search.is_empty() {
                        this.search.clear();
                        this.list.rows_mut().mark_dirty();
                        cx.notify();
                    }
                    return;
                }
                if event.keystroke.key == "enter" {
                    this.list.dispatch(InputAction::Activate, cx);
                    cx.stop_propagation();
                    return;
                }
                if field_focused && this.search.handle_routed(event, cx) {
                    this.list.rows_mut().mark_dirty();
                    cx.stop_propagation();
                    cx.notify();
                }
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
                // The scope's header tools share the title's line: the filter
                // box and the shuffle button. The library filters and shuffles
                // the whole library; an artist tab, their discography; a
                // folder view, the folder's albums and singles as they stand
                // right now.
                matches!(self.scope, Scope::Library | Scope::Artist(_) | Scope::Folder { .. })
                    .then(|| {
                    // Fixed-width and pushed to the right edge: the albums·songs
                    // readout changes with the filter, and a flexed box would
                    // resize on every keystroke.
                    div()
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .justify_end()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .w(px(theme.cell_px() * 22.0))
                                .child(search_box(
                                    theme,
                                    &self.search,
                                    &self.search_focus,
                                    Some(placeholder),
                                    false,
                                    Some("ctrl+f"),
                                    Some(Box::new(cx.listener(|this, _event, _window, cx| {
                                        this.search.clear();
                                        this.list.rows_mut().mark_dirty();
                                        cx.notify();
                                    }))),
                                    None,
                                    window,
                                )),
                        )
                        .child(
                            div()
                                .id("shuffle")
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|d| d.bg(theme.row_hover))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    let playback = this.list.playback().clone();
                                    let scope = this.scope.clone();
                                    this.list.library().clone().update(cx, |state, cx| {
                                        state.play_shuffled(scope, &playback, cx)
                                    });
                                }))
                                .child(icon(
                                    theme.cell_px() * 1.6,
                                    theme.cell_px() * 1.6,
                                    SHUFFLE_POLYGONS,
                                    theme.text_muted,
                                )),
                        )
                        .into_any_element()
                }),
            ));

        if sections.is_empty() {
            root = root.child(empty_hint(
                theme,
                if self.search.is_empty() {
                    "Nothing scanned yet — add a music directory in settings."
                } else {
                    "Nothing matched."
                },
                true,
            ));
        } else if self.config.read(cx).icon_view() {
            // Icon view: album cards — cover, then its songs — wrapped to the
            // container's width, instead of the headed track-row list.
            root = root.child(album_list::render_icons(self, theme, None, window, cx));
        } else {
            root = root.child(album_list::render_rows(self, theme, None, cx));
        }

        root
    }
}

const SHUFFLE_POLYGONS: &[&[(f32, f32)]] = &[
    // Top-left arm, falling to the bottom-right head.
    &[(0.0, 0.28), (0.35, 0.28), (0.68, 0.64), (0.68, 0.80), (0.35, 0.44), (0.0, 0.44)],
    &[(0.66, 0.50), (0.98, 0.72), (0.66, 0.94)],
    // Bottom-left arm, rising to the top-right head.
    &[(0.0, 0.56), (0.35, 0.56), (0.68, 0.20), (0.68, 0.36), (0.35, 0.72), (0.0, 0.72)],
    &[(0.66, 0.06), (0.98, 0.28), (0.66, 0.50)],
];
