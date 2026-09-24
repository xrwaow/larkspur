//! The advanced search container.
//!
//! Not a tab: it's an overlay the tab container swaps in over the active tab
//! (`ctrl+shift+f`), so a search is a detour from whatever you were reading
//! rather than another thing to keep track of. Results are grouped by album
//! using the same sections the browse and playlist containers draw, drawn
//! through GPUI's virtualized [`list`] — the shell comes from [`album_list`];
//! this view owns the query box and what a submitted query means.
//!
//! All matching lives in [`crate::model::search`]; this view owns the text being
//! typed (a [`TextField`], with caret, selection, editing chords, and clipboard
//! paste) and the last query it ran. `enter` submits a changed query and
//! otherwise plays the selected result.

use gpui::{
    div, prelude::*, px, AnyElement, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, Render, Window,
};

use crate::model::search::{self, Query};
use crate::model::InputAction;
use crate::ui::album_list::{AlbumList, AlbumListSubs, AlbumListView, Play};
use crate::ui::albums;
use crate::ui::animation::Animator;
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::input::action_for_key;
use crate::ui::library_state::LibraryState;
use crate::ui::playback_state::PlaybackState;
use crate::ui::text_field::TextField;
use crate::ui::widgets::{empty_hint, panel_header, search_box};

pub struct SearchView {
    themed: Themed,
    list: AlbumListView,
    /// The query box.
    input: TextField,
    /// The last query that was submitted, and what it warned about.
    query: Query,
    /// The text of the last submitted query, so `enter` can tell "run this" from
    /// "play the selected result".
    submitted: String,
    /// Whether the query-syntax cheat sheet is pinned open (the "?" button).
    /// It also shows on its own while the box is empty, as the greeting.
    help_open: bool,
    warnings: Vec<String>,
    focus_handle: FocusHandle,
    /// No playback observer — the overlay re-renders with focus changes
    /// anyway; it reads playback while it renders.
    _observe: AlbumListSubs,
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
        let observe = crate::ui::album_list::observe(&library, None, &covers, &animator, cx);
        // The search overlay draws the library's rows, so it shares the browse
        // list's typography — one size, so the tabs can't drift apart.
        let themed =
            Themed::with_container(BrowseView::container_id(), BrowseView::default_font_size(), &config, cx);
        Self {
            themed,
            list: AlbumListView::new(library, playback, covers, animator, Play::Result),
            input: TextField::default(),
            query: Query::default(),
            submitted: String::new(),
            help_open: false,
            warnings: Vec::new(),
            focus_handle: cx.focus_handle(),
            _observe: observe,
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
        self.list.rows_mut().mark_dirty();
        cx.notify();
    }

    /// Build the result sections from the last submitted query.
    fn build_sections(&self, cx: &Context<Self>) -> Vec<albums::AlbumSection> {
        if self.input.is_empty() {
            // An empty box isn't a query for "everything" — that's the browse
            // container's job.
            Vec::new()
        } else {
            let state = self.list.library().read(cx);
            albums::sections_from_groups(
                state.library(),
                search::search(state.library(), &self.query),
            )
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

impl AlbumList for SearchView {
    fn list(&self) -> &AlbumListView {
        &self.list
    }

    fn list_mut(&mut self) -> &mut AlbumListView {
        &mut self.list
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        let fresh = self.list.rows().is_dirty().then(|| self.build_sections(cx));
        self.list.rows_mut().sync(theme, window, fresh);
        self.list.anchor_to_playing(cx);

        let sections = self.list.rows().sections();

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
                        this.list.dispatch(action, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                if event.keystroke.key == "enter" {
                    // A changed query runs; an unchanged one plays the selection.
                    if this.input.value != this.submitted {
                        this.submit(cx);
                    } else {
                        this.list.dispatch(InputAction::Activate, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.input.handle_routed(event, cx) {
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
                "Search",
                theme.cell_px() + 2.0,
                None,
                None,
                Some((&summary, hint_color)),
            ));

        if sections.is_empty() {
            root = root.child(div().flex_1().child(empty_hint(
                theme,
                if self.input.is_empty() {
                    "Type a query and press enter."
                } else {
                    "Nothing matched."
                },
                true,
            )));
        } else {
            root = root.child(crate::ui::album_list::render_rows(self, theme, None, cx));
        }

        root.child(
            div()
                .px_4()
                .pb_3()
                .flex()
                .flex_col()
                .gap_2()
                .when(
                    self.input.is_empty() || self.help_open,
                    |d| d.child(syntax_help(theme)),
                )
                .child(search_box(
                    theme,
                    &self.input,
                    Some("artist:sinatra year:>=2000"),
                    self.help_open,
                    Some(
                        div()
                            .id("search-help")
                            .cursor_pointer()
                            .text_color(if self.help_open { theme.accent } else { theme.text_faint })
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.help_open = !this.help_open;
                                cx.notify();
                            }))
                            .child("?")
                            .into_any_element(),
                    ),
                )),
        )
    }
}

/// The query-syntax cheat sheet.
fn syntax_help(theme: crate::ui::theme::Theme) -> AnyElement {
    let examples: &[(&str, &str)] = &[
        ("nancy sinatra", "every word, anywhere"),
        ("\"the blue moon\"", "phrase"),
        ("artist:beck title:earth", "scope a term"),
        ("from:lyrics type:fuzzy hvnly", "search lyrics, subsequence"),
        ("year:1997-2003 dur:>4:00", "numeric ranges"),
        ("[title]{love the way} [ext]{flac}", "brackets quote term text"),
    ];
    let mut panel = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .py_2()
        .rounded_md()
        .bg(theme.row_odd)
        .text_size(px(theme.small_px()));
    for (query, meaning) in examples {
        panel = panel.child(
            div()
                .flex()
                .items_baseline()
                .gap_3()
                .child(
                    div()
                        .w(px(theme.font_size * 30.0))
                        .flex_none()
                        .text_color(theme.accent)
                        .child(query.to_string()),
                )
                .child(div().text_color(theme.text_muted).child(meaning.to_string())),
        );
    }
    panel.into_any_element()
}
