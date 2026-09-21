//! The advanced search container.
//!
//! Not a tab: it's an overlay the tab container swaps in over the active tab
//! (`ctrl+shift+f`), so a search is a detour from whatever you were reading
//! rather than another thing to keep track of. Results are grouped by album
//! using the same sections the browse and playlist containers draw, drawn
//! through GPUI's virtualized [`list`].
//!
//! All matching lives in [`crate::model::search`]; this view only owns the text
//! being typed (a [`TextField`], with caret, selection, and editing chords) and
//! the last query it ran.

use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, ListAlignment, ListState, MouseDownEvent, Render, Subscription, Window,
};

use crate::model::search::{self, Query};
use crate::model::{PlaylistId, SongId};
use crate::ui::albums::{self, AlbumSection, RowActions};
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::state::PlaybackState;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;

/// Extra rows rendered above and below the viewport. Roughly ±50 rows.
const OVERDRAW_PX: f32 = 1200.0;

pub struct SearchView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    theme: Theme,
    /// The query box.
    input: TextField,
    /// The last query that was submitted, and what it warned about.
    query: Query,
    warnings: Vec<String>,
    /// The current result sections. Rebuilt on submit, not every frame.
    sections: Rc<Vec<AlbumSection>>,
    /// Set when the library changes, so results are re-run against it.
    stale: bool,
    /// Virtualized-list state.
    list_state: ListState,
    /// The row count the list was last reset to.
    item_count: usize,
    /// Set when the result set changed, so the list re-measures.
    list_dirty: bool,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_covers: Subscription,
    _observe_config: Subscription,
}

impl SearchView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, _state, cx| {
            this.stale = true;
            cx.notify();
        });
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
            input: TextField::default(),
            query: Query::default(),
            warnings: Vec::new(),
            sections: Rc::new(Vec::new()),
            stale: false,
            list_state: ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX)),
            item_count: 0,
            list_dirty: false,
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_covers: observe_covers,
            _observe_config: observe_config,
        }
    }

    /// The focus handle the tab container hands focus to when it opens search.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Parse and run the current input.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let (query, warnings) = search::parse(&self.input.value);
        self.query = query;
        self.warnings = warnings;
        self.list_dirty = true;
        self.recompute(cx);
        cx.notify();
    }

    /// Re-run the last query against the library. Does not notify — callers
    /// decide, so this is safe to call from `render`.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        self.sections = Rc::new(if self.input.is_empty() {
            // An empty box isn't a query for "everything" — that's the browse
            // container's job.
            Vec::new()
        } else {
            let state = self.library.read(cx);
            albums::sections_from_groups(
                state.library(),
                search::search(state.library(), &self.query),
            )
        });
    }

    fn summary(&self) -> String {
        if self.input.is_empty() {
            "Type a query and press enter".to_string()
        } else if let Some(warning) = self.warnings.first() {
            warning.clone()
        } else {
            let songs: usize = self.sections.iter().map(|s| s.tracks.len()).sum();
            format!("{} songs in {} albums", songs, self.sections.len())
        }
    }
}

impl Container for SearchView {
    fn container_id() -> &'static str {
        "search"
    }
}

impl Focusable for SearchView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// How the search view reacts to a row click: play the album from there.
struct SearchRows;

impl RowActions<SearchView> for SearchRows {
    fn activate(
        &self,
        view: &mut SearchView,
        playlist: PlaylistId,
        index: usize,
        _event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<SearchView>,
    ) {
        let playback = view.playback.clone();
        view.library.update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
    }

    fn context(
        &self,
        view: &mut SearchView,
        song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<SearchView>,
    ) {
        let request = SongMenuRequest { song, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }
}

impl Render for SearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        if self.stale {
            self.stale = false;
            self.list_dirty = true;
            self.recompute(cx);
        }

        let sections = self.sections.clone();
        let items = Rc::new(albums::flatten(&sections));
        if self.item_count != items.len() || self.list_dirty {
            self.list_state.reset(items.len());
            self.item_count = items.len();
            self.list_dirty = false;
        }

        let current = self.playback.read(cx).metadata().id;
        let live_bitrate = self.playback.read(cx).live_bitrate();
        let actions: Rc<dyn RowActions<SearchView>> = Rc::new(SearchRows);

        let summary = self.summary();

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                // The box owns plain typing and editing keys, including the
                // selection chords. `escape` and unhandled command chords
                // (`ctrl+tab`, `ctrl+shift+f`, …) bubble to the tab container.
                if event.keystroke.key == "escape" {
                    return;
                }
                if event.keystroke.key == "enter" {
                    this.submit(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.input.handle_key(event) {
                    cx.stop_propagation();
                    cx.notify();
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
                            .text_size(px(theme.cell_px() + 2.0))
                            .text_color(theme.text)
                            .child("Search"),
                    )
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(if self.warnings.is_empty() {
                                theme.text_muted
                            } else {
                                theme.accent
                            })
                            .child(summary),
                    ),
            );

        if sections.is_empty() {
            root = root.child(
                div()
                    .flex_1()
                    .px_4()
                    .py_6()
                    .text_size(px(theme.cell_px()))
                    .text_color(theme.text_faint)
                    .child(if self.input.is_empty() {
                        "Type a query and press enter."
                    } else {
                        "Nothing matched."
                    }),
            );
        } else {
            let covers = self.covers.clone();
            let library = self.library.clone();
            let render = cx.processor({
                let sections = sections.clone();
                let items = items.clone();
                let actions = actions.clone();
                move |_this, ix: usize, window: &mut Window, cx: &mut Context<SearchView>| {
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

        root.child(div().px_4().pb_3().child(search_box(theme, &self.input)))
    }
}

/// The query box: a magnifier, the editable text, and a hint.
fn search_box(theme: Theme, input: &TextField) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(theme.row_odd)
        .child(
            div()
                .flex_none()
                .text_size(px(theme.cell_px()))
                .text_color(theme.accent)
                .child("⌕"),
        )
        .child(input.render_text(theme, Some("Search the library…")))
        .child(
            div()
                .flex_none()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child("esc"),
        )
        .into_any_element()
}
