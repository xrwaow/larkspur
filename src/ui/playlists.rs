//! The playlist list — the left rail.
//!
//! Shows custom playlists, autogen album playlists, the artists derived from
//! them, and a folder view of the synced directories. An artist expands into
//! its discography (the album playlists it appears on), since an artist is a
//! *list* of playlists, not one playlist. Albums, Artists, and Folder View are
//! collapsible groups, all folded shut at launch; the folder view expands
//! directory by directory, straight from the filesystem.
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

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, list, prelude::*, px, AnyElement, App, ClickEvent, Context, ElementId, Entity,
    FocusHandle, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, Pixels,
    Point, Render, Subscription, Window,
};

use crate::model::PlaylistId;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::library_state::{subdirs, LibraryState};
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::row_list::OVERDRAW_PX;
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::widgets::{empty_hint, panel_header, search_box, section_header};

/// An open right-click menu in the rail: on a custom playlist or a folder.
struct RailMenu {
    position: Point<Pixels>,
    target: MenuTarget,
}

/// What the open rail menu acts on.
enum MenuTarget {
    Playlist(PlaylistId),
    Folder(PathBuf),
}

pub struct PlaylistsView {
    library: Entity<LibraryState>,
    /// Clicking a playlist opens it as a tab in the center — the rail is how
    /// tabs get created, so it holds the tab container.
    tabs: Entity<TabsView>,
    /// The synced-path list for the folder view lives here.
    config: Entity<ConfigState>,
    themed: Themed,
    expanded_artist: Option<String>,
    /// Whether the Albums, Artists, and Folder View groups are expanded. All
    /// three are dropdowns, folded shut at launch; a filter overrides Albums
    /// and Artists (a search must be able to reveal matches in a collapsed
    /// group).
    albums_open: bool,
    artists_open: bool,
    folder_open: bool,
    /// The directories the folder view has expanded, subtree by subtree.
    open_dirs: HashSet<PathBuf>,
    /// The text filtering the Albums and Artists groups (custom playlists and
    /// the folder view are never filtered).
    filter: TextField,
    /// The playlist being renamed inline, and its edit buffer.
    renaming: Option<(PlaylistId, TextField)>,
    /// The open right-click menu, if any.
    menu: Option<RailMenu>,
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
            config,
            themed,
            expanded_artist: None,
            albums_open: false,
            artists_open: false,
            folder_open: false,
            open_dirs: HashSet::new(),
            filter: TextField::default(),
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
                .map(|p| Row { id: p.id, title: p.meta().title, count: p.len() })
                .collect();
            let albums: Vec<Row> = lib
                .playlists()
                .iter()
                .filter(|p| !p.is_custom())
                .map(|p| Row { id: p.id, title: p.meta().title, count: p.len() })
                .collect();
            let artists = lib.artists();

            let discography: Vec<Row> = self
                .expanded_artist
                .as_ref()
                .map(|artist| {
                    lib.discography(artist)
                        .into_iter()
                        .filter_map(|id| lib.playlist(id))
                        .map(|p| Row { id: p.id, title: p.meta().title, count: p.len() })
                        .collect()
                })
                .unwrap_or_default();

            (custom, albums, artists, discography)
        };

        // The filter narrows the Albums and Artists groups only — custom
        // playlists are few and deliberately always visible. An empty query
        // matches everything, so one code path serves both modes.
        let query = self.filter.value.trim().to_lowercase();
        let filtering = !query.is_empty();
        let matches = |text: &str| text.to_lowercase().contains(&query);
        let albums: Vec<Row> = albums.into_iter().filter(|r| matches(&r.title)).collect();
        let artists: Vec<String> = artists.into_iter().filter(|n| matches(n)).collect();
        let discography: Vec<Row> =
            discography.into_iter().filter(|r| matches(&r.title)).collect();

        // A filter overrides a collapsed group: searching must reveal what a
        // dropdown is hiding.
        let show_albums = filtering || self.albums_open;
        let show_artists = filtering || self.artists_open;

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

        rows.push(RailRow::GroupHeader { label: "Albums", open: show_albums });
        if show_albums {
            if filtering && albums.is_empty() {
                rows.push(RailRow::NoMatches);
            }
            for row in albums {
                rows.push(RailRow::Album(row));
            }
        }

        rows.push(RailRow::GroupHeader { label: "Artists", open: show_artists });
        if show_artists {
            if filtering && artists.is_empty() {
                rows.push(RailRow::NoMatches);
            }
            for (index, artist) in artists.into_iter().enumerate() {
                let expanded = self.expanded_artist.as_deref() == Some(artist.as_str());
                rows.push(RailRow::Artist { index, name: artist, expanded });
                if expanded {
                    for row in &discography {
                        rows.push(RailRow::Discography(row.clone()));
                    }
                }
            }
        }

        // The folder view mirrors the synced directories on disk. It ignores
        // the filter (that one is for albums and artists) and reads the
        // filesystem only for the subtrees actually expanded.
        rows.push(RailRow::GroupHeader { label: "Folder View", open: self.folder_open });
        if self.folder_open {
            for root in self.config.read(cx).roots() {
                push_folder_tree(&mut rows, root, 0, &self.open_dirs);
            }
        }

        rows
    }

    /// Render one row of the flattened rail. Called by the virtualized list
    /// only for rows near the viewport.
    fn render_row(
        &mut self,
        theme: Theme,
        ix: usize,
        row: &RailRow,
        selected: Option<PlaylistId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Every row carries the list's item spacing, matching the gap the old
        // unvirtualized column had between children.
        let row = match row {
            RailRow::Header(label) => section_header(theme, label),
            RailRow::GroupHeader { label, open } => {
                let toggled = *label;
                group_header(
                    theme,
                    *label,
                    *open,
                    cx.listener(move |this, _event, _window, cx| {
                        match toggled {
                            "Albums" => this.albums_open = !this.albums_open,
                            "Artists" => this.artists_open = !this.artists_open,
                            _ => this.folder_open = !this.folder_open,
                        }
                        this.dirty = true;
                        cx.notify();
                    }),
                )
            }
            RailRow::NewPlaylist => nav_row(
                theme,
                ix,
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
            RailRow::NoCustom => empty_hint(theme, "No custom playlists yet", false),
            RailRow::Rename => {
                let field = &self.renaming.as_ref().expect("rename row without a rename").1;
                rename_row(theme, field)
            }
            RailRow::Custom(row) => {
                let id = row.id;
                let row_el = nav_row(
                    theme,
                    ix,
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
                            this.menu = Some(RailMenu {
                                position: event.position,
                                target: MenuTarget::Playlist(id),
                            });
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
                    ix,
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
                    ix,
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
                    ix,
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
            RailRow::Folder { path, depth, expanded, has_children } => {
                // Roots read as the paths they are; everything below them is
                // just its own name.
                let label = match path.file_name() {
                    Some(name) if *depth > 0 => name.to_string_lossy().to_string(),
                    _ => path.display().to_string(),
                };
                let toggled = path.clone();
                let row_el = folder_row(
                    theme,
                    ix,
                    path_hash(path),
                    label,
                    *depth,
                    *expanded,
                    *has_children,
                    cx.listener(move |this, _event, _window, cx| {
                        if !this.open_dirs.remove(&toggled) {
                            this.open_dirs.insert(toggled.clone());
                        }
                        this.dirty = true;
                        cx.notify();
                    }),
                );
                let target = path.clone();
                div()
                    .id(("folder-menu", path_hash(path)))
                    .w_full()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.menu = Some(RailMenu {
                                position: event.position,
                                target: MenuTarget::Folder(target.clone()),
                            });
                            cx.notify();
                        }),
                    )
                    .child(row_el)
                    .into_any_element()
            }
            RailRow::NoMatches => empty_hint(theme, "No matches", false),
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
    /// A collapsible group header (Albums, Artists, Folder View): `open` draws
    /// the arrow.
    GroupHeader { label: &'static str, open: bool },
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
    /// A directory in the folder view: a scan root or one of its subdirectories.
    Folder { path: PathBuf, depth: usize, expanded: bool, has_children: bool },
    /// A filtered group with nothing left in it.
    NoMatches,
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
            this.render_row(theme, ix, &rows[ix], selected, cx)
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
                        if field.handle_routed(event, cx) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }
                    return;
                }
                if event.keystroke.key == "escape" && this.menu.is_some() {
                    this.menu = None;
                    cx.notify();
                    return;
                }
                // Escape clears the filter before it bubbles; everything the
                // field edits (typing, caret moves, paste) is consumed here.
                if event.keystroke.key == "escape" && !this.filter.is_empty() {
                    this.filter.clear();
                    this.dirty = true;
                    cx.notify();
                    return;
                }
                if this.filter.handle_routed(event, cx) {
                    cx.stop_propagation();
                    this.dirty = true;
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .child(panel_header(
                theme,
                "Library",
                theme.cell_px(),
                Some(settings_button(theme, cx.listener(|this, _event, window, cx| {
                    this.tabs.update(cx, |tabs, cx| tabs.open_settings(window, cx));
                }))),
                None,
                if scanning { Some(("scanning…", theme.text_muted)) } else { None },
            ))
            .child(
                div()
                    .px_2()
                    .pb_2()
                    // Clicking the box focuses the rail, so typing lands in the
                    // filter (the field itself owns no focus).
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, window, _cx| window.focus(&this.focus_handle)),
                    )
                    .child(rail_search(
                        theme,
                        &self.filter,
                        cx.listener(|this, _event, _window, cx| {
                            this.filter.clear();
                            this.dirty = true;
                            cx.notify();
                        }),
                    )),
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
            let dismiss = cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                this.menu = None;
                cx.notify();
            });
            match &menu.target {
                MenuTarget::Playlist(playlist) => {
                    let playlist = *playlist;
                    let title = self
                        .library
                        .read(cx)
                        .library()
                        .playlist(playlist)
                        .map(|p| p.meta().title)
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
                    root = root.child(context_menu(
                        theme,
                        position,
                        vec![("Rename".to_string(), rename), ("Delete".to_string(), delete)],
                        dismiss,
                    ));
                }
                MenuTarget::Folder(path) => {
                    // The rail holds no playback handle, so playing a folder
                    // goes through the tab container as a request.
                    let path = path.clone();
                    let play: MenuHandler = Box::new(cx.listener(move |this, _event, _window, cx| {
                        this.menu = None;
                        this.library
                            .update(cx, |state, cx| state.request_folder_play(path.clone(), cx));
                    }));
                    root = root.child(context_menu(
                        theme,
                        position,
                        vec![("Play folder".to_string(), play)],
                        dismiss,
                    ));
                }
            }
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

/// A collapsible group header (Albums, Artists, Folder View): the label with
/// its arrow after it, clickable to fold or unfold the group.
fn group_header(
    theme: Theme,
    label: &'static str,
    open: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let group = match label {
        "Albums" => 0u64,
        "Artists" => 1u64,
        _ => 2u64,
    };
    div()
        .id(("group-header", group))
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .pt_2()
        .pb_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(
            div()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(if open { "▾" } else { "▸" }),
        )
        .into_any_element()
}

/// One directory row of the folder view: its name (indented by depth), with
/// the expand arrow after it — only when the directory has subdirectories.
/// Zebra-striped like the rail's other rows.
fn folder_row(
    theme: Theme,
    index: usize,
    id: u64,
    label: String,
    depth: usize,
    expanded: bool,
    has_children: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(("folder", id))
        .flex()
        .items_center()
        .gap_1()
        .pl(px(theme.cell_px() * (2.0 + depth as f32 * 1.6)))
        .pr_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(theme.row_bg(index))
        .when(has_children, |d| d.hover(|d| d.bg(theme.row_hover)))
        .when(has_children, |d| d.on_click(on_click))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(if has_children && expanded { "▾" } else if has_children { "▸" } else { "" }),
        )
        .into_any_element()
}

/// Recursively flatten a directory subtree into rail rows: the directory
/// itself, then — when it's expanded — its subdirectories, depth-first.
fn push_folder_tree(rows: &mut Vec<RailRow>, dir: &Path, depth: usize, open: &HashSet<PathBuf>) {
    let children = subdirs(dir);
    let has_children = !children.is_empty();
    let expanded = has_children && open.contains(dir);
    rows.push(RailRow::Folder {
        path: dir.to_path_buf(),
        depth,
        expanded,
        has_children,
    });
    if expanded {
        for child in &children {
            push_folder_tree(rows, child, depth + 1, open);
        }
    }
}

/// A stable element id for a directory row.
fn path_hash(path: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// The rail's filter box: the shared search-box chrome with— once there's
/// something typed — a clear button at the end.
fn rail_search(
    theme: Theme,
    field: &TextField,
    on_clear: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let trailing = (!field.is_empty()).then(|| {
        div()
            .id("clear-rail-filter")
            .flex_none()
            .px_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(theme.small_px()))
            .text_color(theme.text_faint)
            .hover(|d| d.bg(theme.row_hover).text_color(theme.text))
            .on_click(on_clear)
            .child("×")
            .into_any_element()
    });
    search_box(theme, field, Some("Filter albums & artists…"), false, trailing)
}

/// A rail navigation row: zebra-striped like every row list, with the active
/// row overriding the stripe and hover overriding both.
#[allow(clippy::too_many_arguments)]
fn nav_row(
    theme: Theme,
    index: usize,
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
        .bg(theme.row_bg(index))
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
