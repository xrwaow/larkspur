//! The playlist container — one open playlist as a tab.
//!
//! Draws a playlist the way the library draws an album: a cover header
//! (artist, title, format line, year) over its track rows, so opening a
//! playlist reads like the browse list instead of a different kind of table.
//! A one-song playlist collapses to a single row with its cover on the left,
//! and a long playlist is virtualized through GPUI's [`list`].
//!
//! One view exists per open playlist tab, so it holds the playlist it shows and
//! its own selected row — two tabs keep two independent cursors. `↑`/`↓` move
//! the selection and `enter` plays it; right-clicking a track opens the shared
//! song menu (go to artist / go to playlist, plus remove-from-playlist in a
//! custom one). Renaming and deleting the playlist itself live in the left rail.
//!
//! A custom playlist also carries an "Add songs" bar pinned to the bottom, just
//! above the transport: type in it and matching library songs pull up above it,
//! full width; clicking one adds it.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle,
    Focusable, KeyDownEvent, ListAlignment, ListState, MouseDownEvent, Render, Subscription, Window,
};

use crate::model::search;
use crate::model::{InputAction, PlaylistId, SongId};
use crate::ui::albums::{self, AlbumSection, RowActions};
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::playback::action_for_key;
use crate::ui::state::PlaybackState;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;

/// How many "add songs" candidates to render at once.
const ADD_LIST_CAP: usize = 200;

/// Extra rows rendered above and below the viewport. Roughly ±50 rows.
const OVERDRAW_PX: f32 = 1200.0;

pub struct PlaylistView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    /// The playlist this tab shows.
    playlist: PlaylistId,
    theme: Theme,
    /// The playlist's section (empty when the playlist is gone). Rebuilt when
    /// the library changes, not every frame.
    sections: Rc<Vec<AlbumSection>>,
    is_custom: bool,
    dirty: bool,
    /// The add-songs query box.
    add_query: TextField,
    /// Add-songs candidates, recomputed when the query or library changes.
    add_results: Vec<(SongId, String, String)>,
    add_stale: bool,
    /// Focus for the add-songs box, so it only takes keys once clicked.
    add_focus: FocusHandle,
    /// Row index selected within this playlist (keyboard navigation).
    selected_row: Option<usize>,
    /// Virtualized-list state.
    list_state: ListState,
    /// The row count the list was last reset to.
    item_count: usize,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_playback: Subscription,
    _observe_covers: Subscription,
    _observe_config: Subscription,
}

impl PlaylistView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        playlist: PlaylistId,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, _state, cx| {
            this.dirty = true;
            this.add_stale = true;
            cx.notify();
        });
        let observe_playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        let observe_covers = cx.observe(&covers, |_this, _state, cx| cx.notify());
        let observe_config = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
        // Start on the first row, so `enter` plays the playlist immediately
        // instead of needing an arrow press first.
        let rows = library.read(cx).library().songs_of(playlist).len();
        Self {
            library,
            playback,
            covers,
            playlist,
            theme,
            sections: Rc::new(Vec::new()),
            is_custom: false,
            dirty: true,
            add_query: TextField::default(),
            add_results: Vec::new(),
            add_stale: false,
            add_focus: cx.focus_handle(),
            selected_row: (rows > 0).then_some(0),
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

    /// How many rows this playlist has.
    fn row_count(&self, cx: &Context<Self>) -> usize {
        self.library.read(cx).library().songs_of(self.playlist).len()
    }

    fn select_row(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        if self.selected_row != row {
            self.selected_row = row;
            cx.notify();
        }
    }

    /// Move the selection down a row, clamped to the last.
    fn select_next(&mut self, cx: &mut Context<Self>) {
        let next = step_down(self.selected_row, self.row_count(cx));
        self.select_row(next, cx);
    }

    /// Move the selection up a row, clamped to the first.
    fn select_prev(&mut self, cx: &mut Context<Self>) {
        let previous = step_up(self.selected_row, self.row_count(cx));
        self.select_row(previous, cx);
    }

    /// Play this playlist from the selected row.
    fn activate(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row {
            let playback = self.playback.clone();
            let playlist = self.playlist;
            self.library
                .update(cx, |state, cx| state.play_playlist(playlist, row, &playback, cx));
        }
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

    /// Handle the actions this view owns. Transport keys belong to the tab
    /// container (it forwards them to playback), so a playlist tab and a
    /// browse tab respond to space and the arrows the same way.
    fn dispatch(&mut self, action: InputAction, cx: &mut Context<Self>) {
        match action {
            InputAction::SelectNext => self.select_next(cx),
            InputAction::SelectPrev => self.select_prev(cx),
            InputAction::Activate => self.activate(cx),
            _ => {}
        }
    }
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
/// plays.
struct PlaylistRows;

impl RowActions<PlaylistView> for PlaylistRows {
    fn activate(
        &self,
        view: &mut PlaylistView,
        playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<PlaylistView>,
    ) {
        if event.click_count() >= 2 {
            let playback = view.playback.clone();
            view.library
                .update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
        } else {
            view.select_row(Some(index), cx);
        }
    }

    fn context(
        &self,
        view: &mut PlaylistView,
        song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<PlaylistView>,
    ) {
        let request = SongMenuRequest { song, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }
}

impl Render for PlaylistView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        if self.dirty {
            self.dirty = false;
            let state = self.library.read(cx);
            let library = state.library();
            self.is_custom =
                library.playlist(self.playlist).map(|p| p.is_custom()).unwrap_or(false);
            self.sections = Rc::new(
                albums::section_for_playlist(library, self.playlist).into_iter().collect(),
            );
        }
        if self.add_stale {
            self.add_stale = false;
            self.recompute_add(cx);
        }

        let sections = self.sections.clone();
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

        let items = Rc::new(albums::flatten(&sections));
        if self.item_count != items.len() {
            self.list_state.reset(items.len());
            self.item_count = items.len();
        }

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
            let header = albums::render_item(
                theme,
                0,
                &sections,
                &items,
                &self.covers,
                &self.library,
                current,
                live_bitrate,
                Some(self.playlist),
                &actions,
                window,
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
            let playlist = self.playlist;
            let render = cx.processor({
                let sections = sections.clone();
                let items = items.clone();
                let actions = actions.clone();
                move |_this, ix: usize, window: &mut Window, cx: &mut Context<PlaylistView>| {
                    albums::render_item(
                        theme,
                        ix,
                        &sections,
                        &items,
                        &covers,
                        &library,
                        current,
                        live_bitrate,
                        Some(playlist),
                        &actions,
                        window,
                        cx,
                    )
                }
            });
            root = root.child(list(self.list_state.clone(), render).flex_1().w_full().min_h_0());
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
                        if this.add_query.handle_key(event) {
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

fn section_header(theme: Theme, label: &str) -> AnyElement {
    div()
        .pb_1()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
        .into_any_element()
}

/// The row to select when moving down: the next one, clamped to the last.
/// `None` when the playlist is empty.
fn step_down(current: Option<usize>, count: usize) -> Option<usize> {
    let last = count.checked_sub(1)?;
    Some(current.map_or(0, |i| (i + 1).min(last)))
}

/// The row to select when moving up: the previous one, clamped to the first.
fn step_up(current: Option<usize>, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(current.map_or(0, |i| i.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::{step_down, step_up};

    #[test]
    fn stepping_down_walks_and_clamps() {
        assert_eq!(step_down(None, 3), Some(0), "nothing selected -> first row");
        assert_eq!(step_down(Some(0), 3), Some(1));
        assert_eq!(step_down(Some(2), 3), Some(2), "clamped at the last row");
        assert_eq!(step_down(Some(0), 0), None, "empty playlist");
    }

    #[test]
    fn stepping_up_walks_and_clamps() {
        assert_eq!(step_up(None, 3), Some(0));
        assert_eq!(step_up(Some(2), 3), Some(1));
        assert_eq!(step_up(Some(0), 3), Some(0), "clamped at the first row");
        assert_eq!(step_up(Some(0), 0), None, "empty playlist");
    }
}
