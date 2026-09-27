//! The playlist container — one open playlist as a tab.
//!
//! Draws a playlist the way the library draws an album: a cover header
//! (artist, title, year) over its track rows, so opening a playlist reads like
//! the browse list instead of a different kind of table. A one-song playlist
//! collapses to a single row with its cover on the left, and a long playlist is
//! virtualized through GPUI's [`list`].
//!
//! One view exists per open playlist tab, so its list keeps its own selection —
//! two tabs keep two independent cursors. Right-clicking a track opens the
//! shared song menu (go to artist / go to playlist, plus remove-from-playlist
//! in a custom one); a right-click on a multi-row selection offers the bulk
//! operations. Renaming and deleting the playlist itself live in the left rail.
//!
//! A custom playlist also carries an "Add songs" bar pinned to the bottom, just
//! above the transport: type in it and matching library songs pull up above it,
//! full width; clicking one adds it.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, Render, SharedString, Window,
};

use crate::model::search;
use crate::model::{PlaylistId, SongId};
use crate::ui::album_list::{
    self, AlbumList, AlbumListSubs, AlbumListView, Play,
};
use crate::ui::albums::{self, RowActions};
use crate::ui::animation::Animator;
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::playback_state::PlaybackState;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::widgets::{empty_hint, section_header};

/// How many "add songs" candidates to render at once.
const ADD_LIST_CAP: usize = 200;

pub struct PlaylistView {
    themed: Themed,
    list: AlbumListView,
    /// The playlist this tab shows.
    playlist: PlaylistId,
    is_custom: bool,
    /// The add-songs query box.
    add_query: TextField,
    /// Add-songs candidates, recomputed when the query or library changes.
    add_results: Vec<(SongId, String, String)>,
    add_stale: bool,
    /// Focus for the add-songs box, so it only takes keys once clicked.
    add_focus: FocusHandle,
    focus_handle: FocusHandle,
    _observe: AlbumListSubs,
}

impl PlaylistView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        playlist: PlaylistId,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe =
            album_list::observe(&library, Some(&playback), &covers, &animator, cx);
        // The playlist tab draws the library's rows, so it shares the browse
        // list's typography — one size, so the tabs can't drift apart.
        let themed = BrowseView::themed(&config, cx);
        let is_custom = is_custom(&library, playlist, cx);
        Self {
            themed,
            list: AlbumListView::new(library, playback, covers, animator, Play::Playlist),
            playlist,
            is_custom,
            add_query: TextField::default(),
            add_results: Vec::new(),
            add_stale: false,
            add_focus: cx.focus_handle(),
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// Exposed so the tab container can hand this view keyboard focus.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Recompute the add-songs candidates from the current query. An empty
    /// query pulls nothing up — the list appears as you type.
    fn recompute_add(&mut self, cx: &mut Context<Self>) {
        let results = if self.add_query.is_empty() {
            Vec::new()
        } else {
            let state = self.list.library().read(cx);
            let library = state.library();
            let present: HashSet<SongId> = library
                .playlist(self.playlist)
                .map(|p| p.song_ids.iter().copied().collect())
                .unwrap_or_default();
            let (query, _) = search::parse(&self.add_query.value);
            let mut results: Vec<(SongId, String, String)> = search::matching_songs(library, &query)
                .into_iter()
                .filter(|id| !present.contains(id))
                .filter_map(|id| {
                    library.get(id).map(|song| (id, song.display_title(), song.artists.join(", ")))
                })
                .collect();
            results.truncate(ADD_LIST_CAP);
            results
        };
        self.add_results = results;
    }
}

/// Whether `playlist` is a user-owned custom one.
fn is_custom(library: &Entity<LibraryState>, playlist: PlaylistId, cx: &gpui::App) -> bool {
    library.read(cx).library().playlist(playlist).is_some_and(|p| p.is_custom())
}

impl Container for PlaylistView {
    fn container_id() -> &'static str {
        "playlist"
    }
}

impl Focusable for PlaylistView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AlbumList for PlaylistView {
    fn list(&self) -> &AlbumListView {
        &self.list
    }

    fn list_mut(&mut self) -> &mut AlbumListView {
        &mut self.list
    }

    fn library_changed(&mut self, cx: &mut Context<Self>) {
        let library = self.list.library().clone();
        self.is_custom = is_custom(&library, self.playlist, cx);
        self.add_stale = true;
    }
}

impl Render for PlaylistView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        if self.add_stale {
            self.add_stale = false;
            self.recompute_add(cx);
        }

        let playlist = self.playlist;
        let fresh = self.list.rows().is_dirty().then(|| {
            albums::section_for_playlist(self.list.library().read(cx).library(), playlist)
                .into_iter()
                .collect()
        });
        self.list.rows_mut().sync(theme, window, fresh);
        self.list.anchor_to_playing(cx);

        let sections = self.list.rows().sections();
        let is_custom = self.is_custom;
        let Some(section) = sections.first() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.panel_bg)
                .font_family(theme.font)
                .text_color(theme.text_faint)
                .child("This playlist is gone");
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
            .font_family(theme.font);

        if section.tracks.is_empty() {
            // No rows to virtualize: draw the header and the hint directly.
            let items = self.list.rows().items();
            let columns = self.list.rows().columns();
            let highlight = self.list.rows().highlight();
            let actions: Option<Rc<dyn RowActions<Self>>> = None;
            let row = albums::RowContext {
                theme,
                covers: self.list.covers(),
                library: self.list.library(),
                current: self.list.playback().read(cx).current_song(),
                playing: self.list.playback().read(cx).is_playing(),
                eq_phase: 0.0,
                live_bitrate_label: self
                    .list
                    .playback()
                    .read(cx)
                    .live_bitrate()
                    .map(|bps| SharedString::from(crate::ui::format::format_bitrate(bps))),
                context: Some(playlist),
                highlight: &highlight,
                drag: None,
                slim: false,
            };
            let header =
                albums::render_item(&row, 0, &sections, &items, &columns, actions.as_ref(), cx);
            root = root.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pb_4()
                    .child(header)
                    .child(empty_hint(
                        theme,
                        if is_custom {
                            "Empty playlist — search below to add songs."
                        } else {
                            "No songs."
                        },
                        true,
                    )),
            );
        } else {
            root = root.child(album_list::render_rows(self, theme, Some(playlist), cx));
        }

        // A custom playlist's add-songs bar, pinned above the transport.
        if is_custom {
            let selected = self.playlist;
            let mut panel = div()
                .flex()
                .flex_col()
                .gap_1()
                .px_4()
                .pt_2()
                .pb_3()
                .border_t_1()
                .border_color(theme.border)
                .child(section_header(theme, "Add songs"));

            if self.add_query.is_empty() {
                panel = panel.child(empty_hint(theme, "Type to search the library.", false));
            } else if self.add_results.is_empty() {
                panel = panel.child(empty_hint(theme, "Nothing to add.", false));
            } else {
                let mut list_el = div()
                    .id("add-results")
                    .w_full()
                    .max_h(px(theme.font_size * 16.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col();
                for (index, (song, title, artist)) in self.add_results.clone().into_iter().enumerate() {
                    list_el = list_el.child(add_row(
                        theme,
                        index,
                        song,
                        title,
                        artist,
                        cx.listener(move |this, _event, _window, cx| {
                            this.list.library().clone().update(cx, |state, cx| {
                                state.add_song(selected, song, cx)
                            });
                        }),
                    ));
                }
                panel = panel.child(list_el);
            }

            panel = panel.child(
                div()
                    .w_full()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if this.add_query.handle_routed(event, cx) {
                            this.recompute_add(cx);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(self.add_query.render(theme, "Search the library to add…", &self.add_focus, window)),
            );

            root = root.child(panel);
        }

        root
    }
}

fn add_row(
    theme: Theme,
    index: usize,
    song: SongId,
    title: String,
    artist: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(("add", song))
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(theme.row_bg(index))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.cell_px()))
                        .text_color(theme.text)
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.cell_px()))
                        .text_color(theme.text_muted)
                        .child(artist),
                ),
        )
        .child(div().text_size(px(theme.cell_px())).text_color(theme.accent).child("+"))
        .into_any_element()
}
