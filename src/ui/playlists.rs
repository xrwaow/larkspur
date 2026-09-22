//! The playlist list — the left rail.
//!
//! Shows custom playlists, autogen album playlists, and the artists derived
//! from them. An artist expands into its discography (the album playlists it
//! appears on), since an artist is a *list* of playlists, not one playlist.
//!
//! Custom playlists are edited here: right-clicking one opens a menu to rename
//! or delete it, and renaming happens inline. The settings button in the header
//! opens the settings overlay in the center.
//!
//! A library produces *thousands* of rows (one per autogen album playlist, plus
//! one per artist), and GPUI re-renders and re-lays-out every visible view on
//! each frame it draws — so the rows are drawn through the virtualized
//! [`list`], exactly like the track lists: only the rows near the viewport are
//! ever built into elements. Without that, every hover, cover decode, or
//! animation frame paid for thousands of rows of layout and the whole app ran
//! at single-digit frames per second.

use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, App, ClickEvent, Context, ElementId, Entity,
    FocusHandle, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, Pixels,
    Point, Render, Subscription, Window,
};

use crate::model::PlaylistId;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::widgets::{empty_hint, section_header};

/// How far past the viewport the list renders, so a fast scroll doesn't pop
/// rows in. Modest, since every rendered row is paid for each frame.
const OVERDRAW_PX: f32 = 512.0;

/// An open right-click menu on a custom playlist.
struct PlaylistMenu {
    position: Point<Pixels>,
    playlist: PlaylistId,
}

pub struct PlaylistsView {
    library: Entity<LibraryState>,
    /// Clicking a playlist opens it as a tab in the center — the rail is how
    /// tabs get created, so it holds the tab container.
    tabs: Entity<TabsView>,
    themed: Themed,
    expanded_artist: Option<String>,
    /// The playlist being renamed inline, and its edit buffer.
    renaming: Option<(PlaylistId, TextField)>,
    /// The open right-click menu, if any.
    menu: Option<PlaylistMenu>,
    focus_handle: FocusHandle,
    /// The flattened rows, rebuilt only when the library, the expansion, or the
    /// rename state changes — never per frame.
    rows: Rc<Vec<RailRow>>,
    list_state: ListState,
    /// Set by anything that changes what the rows show; the next render
    /// rebuilds them.
    dirty: bool,
    _observe: Subscription,
}

impl PlaylistsView {
    pub fn new(
        library: Entity<LibraryState>,
        tabs: Entity<TabsView>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, _state, cx| {
            this.dirty = true;
            cx.notify();
        });
        let themed = Themed::new(&config, cx);
        Self {
            library,
            tabs,
            themed,
            expanded_artist: None,
            renaming: None,
            menu: None,
            focus_handle: cx.focus_handle(),
            rows: Rc::new(Vec::new()),
            list_state: ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX)),
            dirty: true,
            _observe: observe,
        }
    }

    /// Commit the inline rename, if one is open.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((id, field)) = self.renaming.take() {
            let title = field.value.trim().to_string();
            if !title.is_empty() {
                self.library.update(cx, |state, cx| state.rename_playlist(id, title, cx));
            }
        }
        self.dirty = true;
        cx.notify();
    }

    /// Cancel the inline rename.
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        self.dirty = true;
        cx.notify();
    }

    /// Flatten the rail into rows. Runs only when the underlying data changed,
    /// never per frame — the row *data* is cheap, but rebuilding it (and its
    /// elements) every frame is what made the rail dominate frame time.
    fn build_rows(&self, cx: &Context<Self>) -> Vec<RailRow> {
        let (custom, albums, artists, discography) = {
            let state = self.library.read(cx);
            let lib = state.library();

            let custom: Vec<Row> = lib
                .custom_playlists()
                .iter()
                .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                .collect();
            let albums: Vec<Row> = lib
                .playlists()
                .iter()
                .filter(|p| !p.is_custom())
                .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                .collect();
            let artists = lib.artists();

            let discography: Vec<Row> = self
                .expanded_artist
                .as_ref()
                .map(|artist| {
                    lib.discography(artist)
                        .into_iter()
                        .filter_map(|id| lib.playlist(id))
                        .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                        .collect()
                })
                .unwrap_or_default();

            (custom, albums, artists, discography)
        };

        let mut rows: Vec<RailRow> = Vec::new();

        rows.push(RailRow::Header("Playlists"));
        rows.push(RailRow::NewPlaylist);
        if custom.is_empty() {
            rows.push(RailRow::NoCustom);
        }
        for row in custom {
            // The row being renamed shows an editable field instead of a label.
            if self.renaming.as_ref().is_some_and(|(renaming, _)| *renaming == row.id) {
                rows.push(RailRow::Rename);
            } else {
                rows.push(RailRow::Custom(row));
            }
        }

        rows.push(RailRow::Header("Albums"));
        for row in albums {
            rows.push(RailRow::Album(row));
        }

        rows.push(RailRow::Header("Artists"));
        for (index, artist) in artists.into_iter().enumerate() {
            let expanded = self.expanded_artist.as_deref() == Some(artist.as_str());
            rows.push(RailRow::Artist { index, name: artist, expanded });
            if expanded {
                for row in &discography {
                    rows.push(RailRow::Discography(row.clone()));
                }
            }
        }

        rows
    }

    /// Render one row of the flattened rail. Called by the virtualized list
    /// only for rows near the viewport.
    fn render_row(
        &mut self,
        theme: Theme,
        row: &RailRow,
        selected: Option<PlaylistId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Every row carries the list's item spacing, matching the gap the old
        // unvirtualized column had between children.
        let row = match row {
            RailRow::Header(label) => section_header(theme, label),
            RailRow::NewPlaylist => nav_row(
                theme,
                ("new-playlist", 0u64),
                "+  New Playlist".to_string(),
                None,
                false,
                false,
                cx.listener(|this, _event, window, cx| {
                    let id = this.library.update(cx, |state, cx| state.new_playlist(cx));
                    this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                }),
            ),
            RailRow::NoCustom => empty_hint(theme, "No custom playlists yet"),
            RailRow::Rename => {
                let field = &self.renaming.as_ref().expect("rename row without a rename").1;
                rename_row(theme, field)
            }
            RailRow::Custom(row) => {
                let id = row.id;
                let row_el = nav_row(
                    theme,
                    ("custom", id.0),
                    row.title.clone(),
                    Some(row.count),
                    selected == Some(id),
                    false,
                    cx.listener(move |this, _event, window, cx| {
                        this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                    }),
                );
                div()
                    .id(("custom-menu-target", id.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.menu =
                                Some(PlaylistMenu { position: event.position, playlist: id });
                            cx.notify();
                        }),
                    )
                    .child(row_el)
                    .into_any_element()
            }
            RailRow::Album(row) => {
                let id = row.id;
                nav_row(
                    theme,
                    ("album", id.0),
                    row.title.clone(),
                    Some(row.count),
                    selected == Some(id),
                    false,
                    cx.listener(move |this, _event, window, cx| {
                        this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                    }),
                )
            }
            RailRow::Artist { index, name, expanded } => {
                let expanded = *expanded;
                let label = if expanded { format!("▾  {name}") } else { format!("▸  {name}") };
                let toggled = name.clone();
                nav_row(
                    theme,
                    ("artist", *index as u64),
                    label,
                    None,
                    false,
                    false,
                    cx.listener(move |this, _event, _window, cx| {
                        this.expanded_artist =
                            if this.expanded_artist.as_deref() == Some(toggled.as_str()) {
                                None
                            } else {
                                Some(toggled.clone())
                            };
                        this.dirty = true;
                        cx.notify();
                    }),
                )
            }
            RailRow::Discography(row) => {
                let id = row.id;
                nav_row(
                    theme,
                    ("discography", id.0),
                    row.title.clone(),
                    Some(row.count),
                    selected == Some(id),
                    true,
                    cx.listener(move |this, _event, window, cx| {
                        this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                    }),
                )
            }
        };
        div().pb_1().child(row).into_any_element()
    }
}

impl Container for PlaylistsView {
    fn container_id() -> &'static str {
        "playlists"
    }
}

/// A playlist row, snapshotted out of the library so the borrow is dropped
/// before elements are built.
#[derive(Clone)]
struct Row {
    id: PlaylistId,
    title: String,
    count: usize,
}

/// One flattened row of the rail — the unit the virtualized list renders.
enum RailRow {
    Header(&'static str),
    NewPlaylist,
    /// The "no custom playlists" hint.
    NoCustom,
    Custom(Row),
    /// The inline rename field, shown in place of the custom row being renamed.
    Rename,
    Album(Row),
    Artist { index: usize, name: String, expanded: bool },
    /// An album within an expanded artist's discography (rendered indented).
    Discography(Row),
}

impl Render for PlaylistsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let scanning = self.library.read(cx).is_scanning();
        let selected = self.library.read(cx).selected();

        if self.dirty {
            self.dirty = false;
            self.rows = Rc::new(self.build_rows(cx));
            // Update the row count without resetting the scroll position —
            // expanding an artist shouldn't fling the rail back to the top.
            let count = self.rows.len();
            let old = self.list_state.item_count();
            if old != count {
                self.list_state.splice(0..old, count);
            }
        }

        let rows = self.rows.clone();
        let render = cx.processor(move |this, ix, _window: &mut Window, cx: &mut Context<Self>| {
            this.render_row(theme, &rows[ix], selected, cx)
        });

        let mut root = div()
            .relative()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.renaming.is_some() {
                    match event.keystroke.key.as_str() {
                        "enter" => {
                            this.commit_rename(cx);
                            this.tabs.update(cx, |tabs, cx| tabs.refocus(window, cx));
                            return;
                        }
                        "escape" => {
                            this.cancel_rename(cx);
                            this.tabs.update(cx, |tabs, cx| tabs.refocus(window, cx));
                            return;
                        }
                        _ => {}
                    }
                    if let Some((_, field)) = &mut this.renaming {
                        let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
                        if field.handle_key_with_clipboard(event, clipboard.as_deref()) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }
                    return;
                }
                if event.keystroke.key == "escape" && this.menu.is_some() {
                    this.menu = None;
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(settings_button(theme, cx.listener(|this, _event, window, cx| {
                                this.tabs.update(cx, |tabs, cx| tabs.open_settings(window, cx));
                            })))
                            .child(
                                div()
                                    .text_size(px(theme.cell_px()))
                                    .text_color(theme.text)
                                    .child("Library"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_muted)
                            .child(if scanning { "scanning…" } else { "" }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px_2()
                    .child(list(self.list_state.clone(), render).size_full()),
            );

        if let Some(menu) = &self.menu {
            let position = menu.position;
            let playlist = menu.playlist;
            let title = self
                .library
                .read(cx)
                .library()
                .playlist(playlist)
                .map(|p| p.meta.title.clone())
                .unwrap_or_default();

            let rename: MenuHandler = Box::new(cx.listener(move |this, _event, window, cx| {
                this.menu = None;
                this.renaming = Some((playlist, TextField::new(title.clone())));
                this.dirty = true;
                window.focus(&this.focus_handle);
                cx.notify();
            }));
            let delete: MenuHandler = Box::new(cx.listener(move |this, _event, _window, cx| {
                this.menu = None;
                this.library.update(cx, |state, cx| state.delete_playlist(playlist, cx));
            }));
            let dismiss = cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                this.menu = None;
                cx.notify();
            });
            root = root.child(context_menu(
                theme,
                position,
                vec![("Rename".to_string(), rename), ("Delete".to_string(), delete)],
                dismiss,
            ));
        }

        root
    }
}

fn settings_button(
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id("open-settings")
        .flex_none()
        .px_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.cell_px()))
        .text_color(theme.text_muted)
        .hover(|d| d.bg(theme.row_hover).text_color(theme.text))
        .on_click(on_click)
        .child("⚙")
        .into_any_element()
}

/// The editable row shown while renaming.
fn rename_row(theme: Theme, field: &TextField) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .child(field.render(theme, "Playlist name"))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn nav_row(
    theme: Theme,
    id: impl Into<ElementId>,
    label: String,
    count: Option<usize>,
    active: bool,
    indent: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .when(indent, |d| d.pl_5())
        .when(active, |d| d.bg(theme.row_active))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(if active { theme.text } else { theme.text_muted })
                .child(label),
        )
        .child(
            div()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(count.map(|c| c.to_string()).unwrap_or_default()),
        )
        .into_any_element()
}
