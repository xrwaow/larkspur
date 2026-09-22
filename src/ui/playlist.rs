//! The playlist container — one open playlist as a tab.
//!
//! Draws a playlist the way the library draws an album: a cover header
//! (artist, title, format line, year) over its track rows, so opening a
//! playlist reads like the browse list instead of a different kind of table.
//! A one-song playlist collapses to a single row with its cover on the left,
//! and a long playlist is virtualized through GPUI's [`list`].
//!
//! One view exists per open playlist tab, so it holds the playlist it shows and
//! its own selection — two tabs keep two independent cursors. Selection,
//! navigation, paging, and the highlight fade come from [`RowList`]; this view
//! only says that a plain click selects and a double click plays. Right-clicking
//! a track opens the shared song menu (go to artist / go to playlist, plus
//! remove-from-playlist in a custom one); a right-click on a multi-row selection
//! offers the bulk operations. Renaming and deleting the playlist itself live in
//! the left rail.
//!
//! A custom playlist also carries an "Add songs" bar pinned to the bottom, just
//! above the transport: type in it and matching library songs pull up above it,
//! full width; clicking one adds it.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle,
    Focusable, KeyDownEvent, MouseDownEvent, Render, ScrollWheelEvent, Subscription, Window,
};

use crate::model::search;
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
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::widgets::{scroll_area, section_header, wheel_pixels};

/// How many "add songs" candidates to render at once.
const ADD_LIST_CAP: usize = 200;

pub struct PlaylistView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    /// The playlist this tab shows.
    playlist: PlaylistId,
    themed: Themed,
    rows: RowList,
    is_custom: bool,
    /// The library revision the rows were last built from.
    seen_revision: u64,
    /// The add-songs query box.
    add_query: TextField,
    /// Add-songs candidates, recomputed when the query or library changes.
    add_results: Vec<(SongId, String, String)>,
    add_stale: bool,
    /// Focus for the add-songs box, so it only takes keys once clicked.
    add_focus: FocusHandle,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_playback: Subscription,
    _observe_covers: Subscription,
    _observe_animator: Subscription,
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
        let observe = cx.observe(&library, |this, state, cx| {
            let revision = state.read(cx).revision();
            if this.seen_revision != revision {
                this.seen_revision = revision;
                this.rows.mark_dirty();
                this.is_custom = is_custom(&state, this.playlist, cx);
                this.add_stale = true;
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
        let is_custom = is_custom(&library, playlist, cx);
        Self {
            library,
            playback,
            covers,
            playlist,
            themed,
            rows: RowList::new(),
            is_custom,
            seen_revision: 0,
            add_query: TextField::default(),
            add_results: Vec::new(),
            add_stale: false,
            add_focus: cx.focus_handle(),
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

    /// Recompute the add-songs candidates from the current query. An empty
    /// query pulls nothing up — the list appears as you type.
    fn recompute_add(&mut self, cx: &mut Context<Self>) {
        let results = if self.add_query.is_empty() {
            Vec::new()
        } else {
            let state = self.library.read(cx);
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

/// How the playlist view reacts to a row: single click selects, double click
/// plays, and a modifier extends or toggles the selection.
struct PlaylistRows;

impl RowActions<PlaylistView> for PlaylistRows {
    fn activate(
        &self,
        view: &mut PlaylistView,
        item_ix: usize,
        playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<PlaylistView>,
    ) {
        // A modifier extends or toggles the selection; a plain click selects.
        if view.rows.click(item_ix, event.modifiers()) {
            cx.notify();
            return;
        }
        if event.click_count() >= 2 {
            view.play(playlist, index, cx);
        } else {
            cx.notify();
        }
    }

    fn context(
        &self,
        view: &mut PlaylistView,
        item_ix: usize,
        _song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<PlaylistView>,
    ) {
        let songs = view.rows.context_songs(item_ix);
        let request = SongMenuRequest { songs, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }

    fn hover(
        &self,
        view: &mut PlaylistView,
        item_ix: usize,
        hovered: bool,
        cx: &mut Context<PlaylistView>,
    ) {
        if view.rows.set_hover(item_ix, hovered) {
            cx.notify();
        }
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
        let fresh = self.rows.is_dirty().then(|| {
            albums::section_for_playlist(self.library.read(cx).library(), playlist)
                .into_iter()
                .collect()
        });
        self.rows.sync(theme, window, fresh);

        let sections = self.rows.sections();
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

        let items = self.rows.items();
        let title_cols = self.rows.title_cols();
        let list_state = self.rows.list_state().clone();

        let current = self.playback.read(cx).metadata().id;
        let live_bitrate = self.playback.read(cx).live_bitrate();
        let actions: Rc<dyn RowActions<PlaylistView>> = Rc::new(PlaylistRows);

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
            .font_family(theme.font);

        if section.tracks.is_empty() {
            // No rows to virtualize: draw the header and the hint directly.
            let highlight = self.rows.highlight();
            let header = albums::render_item(
                theme,
                0,
                &sections,
                &items,
                &title_cols,
                &self.covers,
                &self.library,
                current,
                live_bitrate,
                Some(playlist),
                &highlight,
                &actions,
                cx,
            );
            root = root.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pb_4()
                    .child(header)
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .text_size(px(theme.cell_px()))
                            .text_color(theme.text_faint)
                            .child(if is_custom {
                                "Empty playlist — search below to add songs."
                            } else {
                                "No songs."
                            }),
                    ),
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
                move |_this, ix: usize, _window: &mut Window, cx: &mut Context<PlaylistView>| {
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
                        Some(playlist),
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
                panel = panel.child(
                    div()
                        .w_full()
                        .pb_1()
                        .text_size(px(theme.small_px()))
                        .text_color(theme.text_faint)
                        .child("Type to search the library.")
                        .into_any_element(),
                );
            } else if self.add_results.is_empty() {
                panel = panel.child(
                    div()
                        .w_full()
                        .pb_1()
                        .text_size(px(theme.small_px()))
                        .text_color(theme.text_faint)
                        .child("Nothing to add.")
                        .into_any_element(),
                );
            } else {
                let mut list_el = div()
                    .id("add-results")
                    .w_full()
                    .max_h(px(theme.font_size * 16.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col();
                for (song, title, artist) in self.add_results.clone() {
                    list_el = list_el.child(add_row(
                        theme,
                        song,
                        title,
                        artist,
                        cx.listener(move |this, _event, _window, cx| {
                            this.library.update(cx, |state, cx| state.add_song(selected, song, cx));
                        }),
                    ));
                }
                panel = panel.child(list_el);
            }

            panel = panel.child(
                div()
                    .w_full()
                    .track_focus(&self.add_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
                        if this.add_query.handle_key_with_clipboard(event, clipboard.as_deref()) {
                            this.recompute_add(cx);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(self.add_query.render(theme, "Search the library to add…")),
            );

            root = root.child(panel);
        }

        root
    }
}

fn add_row(
    theme: Theme,
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
