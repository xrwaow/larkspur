//! The advanced search container.
//!
//! Not a tab: it's an overlay the tab container swaps in over the active tab
//! (`ctrl+shift+f`), so a search is a detour from whatever you were reading
//! rather than another thing to keep track of. Results are grouped by album
//! using the same sections the browse and playlist containers draw, drawn
//! through GPUI's virtualized [`list`].
//!
//! All matching lives in [`crate::model::search`]; this view owns the text being
//! typed (a [`TextField`], with caret, selection, editing chords, and clipboard
//! paste) and the last query it ran. Selection and navigation come from
//! [`RowList`]. `enter` submits a changed query and otherwise plays the selected
//! result.

use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, MouseDownEvent, Render, ScrollWheelEvent, Subscription, Window,
};

use crate::model::search::{self, Query};
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
use crate::ui::widgets::{panel_header, scroll_area, wheel_pixels};

pub struct SearchView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    themed: Themed,
    rows: RowList,
    /// The query box.
    input: TextField,
    /// The last query that was submitted, and what it warned about.
    query: Query,
    /// The text of the last submitted query, so `enter` can tell "run this" from
    /// "play the selected result".
    submitted: String,
    /// The library revision the results were last built from.
    seen_revision: u64,
    warnings: Vec<String>,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_covers: Subscription,
    _observe_animator: Subscription,
}

impl SearchView {
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
            input: TextField::default(),
            query: Query::default(),
            submitted: String::new(),
            seen_revision: 0,
            warnings: Vec::new(),
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_covers: observe_covers,
            _observe_animator: observe_animator,
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
        self.submitted = self.input.value.clone();
        self.warnings = warnings;
        self.rows.mark_dirty();
        cx.notify();
    }

    /// Build the result sections from the last submitted query.
    fn build_sections(&self, cx: &Context<Self>) -> Vec<albums::AlbumSection> {
        if self.input.is_empty() {
            // An empty box isn't a query for "everything" — that's the browse
            // container's job.
            Vec::new()
        } else {
            let state = self.library.read(cx);
            albums::sections_from_groups(
                state.library(),
                search::search(state.library(), &self.query),
            )
        }
    }

    fn dispatch(&mut self, action: InputAction, cx: &mut Context<Self>) {
        match self.rows.handle(action) {
            RowAction::Play(playlist, index) => {
                let playback = self.playback.clone();
                self.library
                    .update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
            }
            RowAction::Handled => cx.notify(),
            RowAction::Ignored => {}
        }
    }

    fn summary(&self, sections: &[albums::AlbumSection]) -> String {
        if self.input.is_empty() {
            "Type a query and press enter".to_string()
        } else if let Some(warning) = self.warnings.first() {
            warning.clone()
        } else {
            let songs: usize = sections.iter().map(|s| s.tracks.len()).sum();
            format!("{} songs in {} albums", songs, sections.len())
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

/// How the search view reacts to a row click: play the album from there, or
/// extend/toggle the selection with a modifier.
struct SearchRows;

impl RowActions<SearchView> for SearchRows {
    fn activate(
        &self,
        view: &mut SearchView,
        item_ix: usize,
        playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<SearchView>,
    ) {
        if view.rows.click(item_ix, event.modifiers()) {
            cx.notify();
            return;
        }
        let playback = view.playback.clone();
        view.library.update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx));
    }

    fn context(
        &self,
        view: &mut SearchView,
        item_ix: usize,
        _song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<SearchView>,
    ) {
        let songs = view.rows.context_songs(item_ix);
        let request = SongMenuRequest { songs, position: event.position, playlist: context };
        view.library.update(cx, |state, cx| state.request_song_menu(request, cx));
    }

    fn hover(
        &self,
        view: &mut SearchView,
        item_ix: usize,
        hovered: bool,
        cx: &mut Context<SearchView>,
    ) {
        if view.rows.set_hover(item_ix, hovered) {
            cx.notify();
        }
    }
}

impl Render for SearchView {
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
        let actions: Rc<dyn RowActions<SearchView>> = Rc::new(SearchRows);

        let summary = self.summary(&sections);
        let hint_color = if self.warnings.is_empty() { theme.text_muted } else { theme.accent };

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                // The box owns plain typing and editing keys, including the
                // selection chords and paste. `escape` and unhandled command
                // chords (`ctrl+tab`, `ctrl+shift+f`, …) bubble to the tab
                // container.
                if event.keystroke.key == "escape" {
                    return;
                }
                // Navigation keys move the result selection rather than the
                // caret, so the box never sees them.
                if let Some(action) = action_for_key(&event.keystroke) {
                    if matches!(
                        action,
                        InputAction::SelectNext
                            | InputAction::SelectPrev
                            | InputAction::PageUp
                            | InputAction::PageDown
                    ) {
                        this.dispatch(action, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                if event.keystroke.key == "enter" {
                    // A changed query runs; an unchanged one plays the selection.
                    if this.input.value != this.submitted {
                        this.submit(cx);
                    } else {
                        this.dispatch(InputAction::Activate, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
                if this.input.handle_key_with_clipboard(event, clipboard.as_deref()) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(panel_header(theme, "Search", &summary, hint_color));

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
            let highlight = self.rows.highlight();
            let render = cx.processor({
                let sections = sections.clone();
                let items = items.clone();
                let title_cols = title_cols.clone();
                let actions = actions.clone();
                move |_this, ix: usize, _window: &mut Window, cx: &mut Context<SearchView>| {
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

        root.child(div().px_4().pb_3().child(search_box(theme, &self.input)))
    }
}

/// The query box: a magnifier, the editable text, and a hint.
fn search_box(theme: crate::ui::theme::Theme, input: &TextField) -> AnyElement {
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
