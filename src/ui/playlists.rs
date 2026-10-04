//! The playlist list — the left rail.
//!
//! Shows custom playlists, autogen album playlists, the artists derived from
//! them, and a folder view of the synced directories. An artist expands into
//! its discography (the album playlists it appears on), since an artist is a
//! *list* of playlists, not one playlist. Albums, Artists, and Folder View are
//! collapsible groups, all folded shut at launch; the folder view expands
//! directory by directory, straight from the filesystem. The Albums and
//! Artists groups each carry their own filter box, scoped to that group.
//!
//! Custom playlists are edited here: right-clicking one opens a menu to rename
//! or delete it, and renaming happens inline. Right-clicking a folder opens a
//! menu to open it — the full subtree, or just the folder's own songs. The
//! settings button in the header opens the settings overlay in the center.
//!
//! A library produces *thousands* of rows (one per autogen album playlist, plus
//! one per artist), and GPUI re-renders and re-lays-out every visible view on
//! each frame it draws — so the rows are drawn through the virtualized
//! [`list`], exactly like the track lists: only the rows near the viewport are
//! ever built into elements. Without that, every hover, cover decode, or
//! animation frame paid for thousands of rows of layout and the whole app ran
//! at single-digit frames per second.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    canvas, div, list, point, prelude::*, px, AnyElement, App, ClickEvent, Context, ElementId,
    Entity, FocusHandle, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent,
    Pixels, Point, Render, Subscription, Window,
};

use crate::model::{FolderMode, Playlist, PlaylistId};
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::input::action_for_key;
use crate::ui::library_state::{subdirs, LibraryState, Request};
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::marquee::{self, Marquee};
use crate::ui::row_list::OVERDRAW_PX;
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::animation::Animator;
use crate::ui::widgets::{empty_hint, panel_header, search_box, section_header};

/// The most characters a rail label shows before its text fades and slides.
const RAIL_MAX_CHARS: usize = 24;

/// Everything a rail row's marquee label needs, from [`PlaylistsView::label_bits`].
struct LabelBits {
    /// The row's marquee state: the slide offset and the region hover that
    /// drives the row's background wash.
    state: Marquee,
    width: f32,
    chars: usize,
    /// The pointer moved over/away from the whole row — the wash the fade
    /// dissolves into.
    on_region: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
    /// The pointer moved over/away from the label cell's right fade zone —
    /// what starts and releases the slide.
    on_slide: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
}

/// An open right-click menu in the rail: on a custom playlist or a folder.
struct RailMenu {
    position: Point<Pixels>,
    target: MenuTarget,
}

/// What the open rail menu acts on.
enum MenuTarget {
    Playlist(PlaylistId),
    /// A folder in the folder view. `here` — the folder has songs directly in
    /// it; `sub` — its subfolders do. Both can hold, either, or neither, and
    /// the menu offers "Open full folder" / "Open folder" accordingly.
    Folder { path: PathBuf, here: bool, sub: bool },
}

/// Which group a filter box (or a fold) belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RailFilter {
    Albums,
    Artists,
}

/// One filterable group's search box: the field, its focus handle, whether the
/// group is unfolded, and the box's chrome text.
struct GroupFilter {
    field: TextField,
    focus: FocusHandle,
    open: bool,
    placeholder: &'static str,
    hint: Option<&'static str>,
}

impl GroupFilter {
    fn new(placeholder: &'static str, hint: Option<&'static str>, cx: &mut App) -> Self {
        Self { field: TextField::default(), focus: cx.focus_handle(), open: false, placeholder, hint }
    }

    /// The lowercased query — empty matches everything.
    fn query(&self) -> String {
        self.field.value.trim().to_lowercase()
    }

    /// Whether the group's rows show: unfolded, or a filter needs to reveal
    /// what the fold is hiding.
    fn showing(&self) -> bool {
        self.open || !self.field.is_empty()
    }
}

/// The row filter a group's query builds: an empty query matches everything,
/// so one closure serves filtered and unfiltered builds alike.
fn query_matches(query: &str) -> impl Fn(&str) -> bool + '_ {
    move |text: &str| query.is_empty() || text.to_lowercase().contains(query)
}

pub struct PlaylistsView {
    library: Entity<LibraryState>,
    /// Clicking a playlist opens it as a tab in the center — the rail is how
    /// tabs get created, so it holds the tab container.
    tabs: Entity<TabsView>,
    /// The synced-path list for the folder view lives here.
    config: Entity<ConfigState>,
    themed: Themed,
    /// Whether the Folder View group is expanded (folded shut at launch). The
    /// Albums and Artists groups' folds live in their [`GroupFilter`]s — a
    /// filter overrides the fold, so a search reveals what it's hiding.
    folder_open: bool,
    /// The directories the folder view has expanded, subtree by subtree.
    open_dirs: HashSet<PathBuf>,
    /// The Albums and Artists groups' filter boxes, scoped to their group
    /// (custom playlists and the folder view are never filtered).
    album_filter: GroupFilter,
    artist_filter: GroupFilter,
    /// The artists whose discography is unfolded — several at once, so
    /// expanding one never folds another.
    expanded_artists: HashSet<String>,
    /// The playlist being renamed inline, its edit buffer, and the buffer's
    /// focus handle (focused when the rename starts, so typing lands there).
    renaming: Option<(PlaylistId, TextField, FocusHandle)>,
    /// The open right-click menu, if any.
    menu: Option<RailMenu>,
    focus_handle: FocusHandle,
    /// The flattened rows, rebuilt only when the library, the expansion, or the
    /// rename state changes — never per frame.
    rows: Rc<Vec<RailRow>>,
    list_state: ListState,
    /// Per-row marquee, keyed by the row's element id — the same
    /// fade/slide/spring-back the list rows and tabs use, so a long artist,
    /// album, or folder name slides on hover. The region hover also carries
    /// the row's hover wash, so the fade matches what's painted.
    marquees: HashMap<u64, Marquee>,
    /// Set by anything that changes what the rows show; the next render
    /// rebuilds them.
    dirty: bool,
    _observe: Subscription,
    _observe_animator: Subscription,
}

impl PlaylistsView {
    pub fn new(
        library: Entity<LibraryState>,
        tabs: Entity<TabsView>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |this, _state, cx| {
            this.dirty = true;
            cx.notify();
        });
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            let dt = animator.read(cx).dt();
            let moved = this.marquees.values_mut().any(|marquee| marquee.tick(dt));
            if moved {
                cx.notify();
            }
        });
        let themed = Themed::new(&config, cx);
        Self {
            library,
            tabs,
            config,
            themed,
            folder_open: false,
            open_dirs: HashSet::new(),
            album_filter: GroupFilter::new("Filter albums…", None, cx),
            artist_filter: GroupFilter::new("Filter artists…", None, cx),
            expanded_artists: HashSet::new(),
            renaming: None,
            menu: None,
            focus_handle: cx.focus_handle(),
            rows: Rc::new(Vec::new()),
            list_state: ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX)),
            marquees: HashMap::new(),
            dirty: true,
            _observe: observe,
            _observe_animator: observe_animator,
        }
    }

    /// The filter box of `which`, for reads.
    fn filter(&self, which: RailFilter) -> &GroupFilter {
        match which {
            RailFilter::Albums => &self.album_filter,
            RailFilter::Artists => &self.artist_filter,
        }
    }

    /// The filter box of `which`, for edits.
    fn filter_mut(&mut self, which: RailFilter) -> &mut GroupFilter {
        match which {
            RailFilter::Albums => &mut self.album_filter,
            RailFilter::Artists => &mut self.artist_filter,
        }
    }

    /// The marquee bits for the rail row with `key`: its slide state and the
    /// label's measured metrics, plus the two hover listeners — the row's
    /// (the wash the fade dissolves into) and the label cell's own slide
    /// trigger. One call per row keeps every rail row's marquee wired the
    /// same way, with no per-caller patching.
    fn label_bits(
        &mut self,
        key: u64,
        label: &str,
        advance: f32,
        cx: &mut Context<Self>,
    ) -> LabelBits {
        let chars = label.chars().count().min(RAIL_MAX_CHARS);
        let width = advance * chars.max(1) as f32;
        let travel = marquee::travel_for(label, width, chars);
        let state = *self.marquees.entry(key).or_default();
        let on_region = cx.listener(move |this, hovered: &bool, _window, cx| {
            if let Some(marquee) = this.marquees.get_mut(&key) {
                if marquee.set_region_hovered(*hovered) {
                    cx.notify();
                }
            }
        });
        let on_slide = cx.listener(move |this, hovered: &bool, _window, cx| {
            if let Some(marquee) = this.marquees.get_mut(&key) {
                if marquee.set_hovered(*hovered, travel) {
                    cx.notify();
                }
            }
        });
        LabelBits {
            state,
            width,
            chars,
            on_region: Box::new(on_region),
            on_slide: Box::new(on_slide),
        }
    }

    /// Commit the inline rename, if one is open.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((id, field, _)) = self.renaming.take() {
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
        let state = self.library.read(cx);
        let lib = state.library();
        let row_of = |p: &Playlist| Row { id: p.id, title: p.title().to_string(), count: p.len() };

        let mut rows: Vec<RailRow> = Vec::new();

        rows.push(RailRow::Header("Playlists"));
        rows.push(RailRow::NewPlaylist);
        let custom = lib.custom_playlists();
        if custom.is_empty() {
            rows.push(RailRow::NoCustom);
        }
        for playlist in custom {
            // The row being renamed shows an editable field instead of a label.
            if self.renaming.as_ref().is_some_and(|(id, _, _)| *id == playlist.id) {
                rows.push(RailRow::Rename);
            } else {
                rows.push(RailRow::Custom(row_of(playlist)));
            }
        }

        // Each group has its own filter box, scoped to that group only —
        // custom playlists are few and deliberately always visible, and the
        // folder view ignores filters too. A filter overrides its group's
        // collapse: searching must reveal what the fold is hiding.
        let (album_query, artist_query) = (self.album_filter.query(), self.artist_filter.query());
        let (album_matches, artist_matches) =
            (query_matches(&album_query), query_matches(&artist_query));
        let albums: Vec<Row> = lib
            .playlists()
            .iter()
            .filter(|p| !p.is_custom() && album_matches(p.title()))
            .map(|p| row_of(p))
            .collect();
        rows.push(RailRow::GroupHeader { label: "Albums", open: self.album_filter.showing() });
        if self.album_filter.showing() {
            rows.push(RailRow::FilterBox(RailFilter::Albums));
            if !album_query.is_empty() && albums.is_empty() {
                rows.push(RailRow::NoMatches);
            }
            rows.extend(albums.into_iter().map(RailRow::Album));
        }

        rows.push(RailRow::GroupHeader { label: "Artists", open: self.artist_filter.showing() });
        if self.artist_filter.showing() {
            rows.push(RailRow::FilterBox(RailFilter::Artists));
            let all_artists = lib.artists();
            let artists: Vec<&str> = all_artists
                .iter()
                .map(String::as_str)
                .filter(|name| artist_matches(name))
                .collect();
            if !artist_query.is_empty() && artists.is_empty() {
                rows.push(RailRow::NoMatches);
            }
            for (index, artist) in artists.into_iter().enumerate() {
                let expanded = self.expanded_artists.contains(artist);
                rows.push(RailRow::Artist { index, name: artist.to_string(), expanded });
                if expanded {
                    rows.extend(
                        lib.discography(artist)
                            .into_iter()
                            .filter_map(|id| lib.playlist(id))
                            .filter(|p| artist_matches(p.title()))
                            .map(|p| RailRow::Discography(row_of(p))),
                    );
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
        kind: &RailRow,
        selected: Option<PlaylistId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // One monospace advance at the rail's own font, so every label's
        // marquee column and travel math agree.
        let advance = marquee::char_advance(window, theme, theme.cell_px());
        // Rows without a marquee never touch this — the marquee arms below
        // all have a key.
        let key = row_key(kind).unwrap_or(0);
        match kind {
            RailRow::Header(label) => section_header(theme, label),
            RailRow::GroupHeader { label, open } => {
                let toggled = *label;
                let group = match toggled {
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
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        match toggled {
                            "Albums" => this.album_filter.open = !this.album_filter.open,
                            "Artists" => this.artist_filter.open = !this.artist_filter.open,
                            _ => this.folder_open = !this.folder_open,
                        }
                        this.dirty = true;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_faint)
                            .child(toggled.to_string()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_faint)
                            .child(if *open { "▾" } else { "▸" }),
                    )
                    .into_any_element()
            }
            RailRow::NewPlaylist => {
                let bits = self.label_bits(key, "+  New Playlist", advance, cx);
                rail_row(
                    theme,
                    ("new-playlist", 0u64),
                    key,
                    "+  New Playlist".to_string(),
                    bits,
                    None,
                    false,
                    0,
                    Branch::Plain,
                    Some(cx.listener(|this, _event, window, cx| {
                        let id = this.library.update(cx, |state, cx| state.new_playlist(cx));
                        this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                    })),
                )
            }
            RailRow::NoCustom => empty_hint(theme, "No custom playlists yet", false),
            RailRow::Rename => {
                let (_, field, focus) =
                    self.renaming.as_ref().expect("rename row without a rename");
                div()
                    .pb_1()
                    .child(field.render(theme, "Playlist name", focus, window))
                    .into_any_element()
            }
            RailRow::Custom(row) | RailRow::Album(row) | RailRow::Discography(row) => {
                // All three open the playlist they stand for; a custom one
                // additionally takes a right-click menu, and a discography row
                // hangs off its artist's guide line.
                let id = row.id;
                let (name, depth) = match kind {
                    RailRow::Custom(_) => ("custom", 0),
                    RailRow::Album(_) => ("album", 0),
                    _ => ("discography", 1),
                };
                let bits = self.label_bits(key, &row.title, advance, cx);
                let row_el = rail_row(
                    theme,
                    (name, id.0),
                    key,
                    row.title.clone(),
                    bits,
                    Some(row.count),
                    selected == Some(id),
                    depth,
                    Branch::Plain,
                    Some(cx.listener(move |this, _event, window, cx| {
                        this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                    })),
                );
                if matches!(kind, RailRow::Custom(_)) {
                    right_click_target(
                        ("custom-menu-target", id.0),
                        row_el,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.menu = Some(RailMenu {
                                position: event.position,
                                target: MenuTarget::Playlist(id),
                            });
                            cx.notify();
                        }),
                    )
                } else {
                    row_el
                }
            }
            RailRow::FilterBox(which) => {
                let which = *which;
                let filter = self.filter(which);
                div()
                    .pb_1()
                    .child(search_box(
                        theme,
                        &filter.field,
                        &filter.focus,
                        Some(filter.placeholder),
                        false,
                        filter.hint,
                        Some(Box::new(cx.listener(move |this, _event, _window, cx| {
                            this.filter_mut(which).field.clear();
                            this.dirty = true;
                            cx.notify();
                        }))),
                        None,
                        window,
                    ))
                    .into_any_element()
            }
            RailRow::Artist { index, name, expanded } => {
                let toggled = name.clone();
                let bits = self.label_bits(key, name, advance, cx);
                rail_row(
                    theme,
                    ("artist", *index as u64),
                    key,
                    name.clone(),
                    bits,
                    None,
                    false,
                    0,
                    Branch::Toggle(*expanded),
                    Some(cx.listener(move |this, _event, _window, cx| {
                        // Several artists stay unfolded at once; clicking one
                        // never folds another.
                        if !this.expanded_artists.remove(&toggled) {
                            this.expanded_artists.insert(toggled.clone());
                        }
                        this.dirty = true;
                        cx.notify();
                    })),
                )
            }
            RailRow::Folder { path, depth, expanded, has_children } => {
                // Roots read as the paths they are; everything below them is
                // just its own name.
                let toggled = path.clone();
                let label = match path.file_name() {
                    Some(name) if *depth > 0 => name.to_string_lossy().to_string(),
                    _ => path.display().to_string(),
                };
                let bits = self.label_bits(key, &label, advance, cx);
                let row_el = rail_row(
                    theme,
                    ("folder", path_hash(path)),
                    key,
                    label,
                    bits,
                    None,
                    false,
                    *depth,
                    // A leaf keeps the trunk line running through it; only
                    // the chevron marks what can unfold.
                    if *has_children { Branch::Toggle(*expanded) } else { Branch::Line },
                    Some(cx.listener(move |this, _event, _window, cx| {
                        if !this.open_dirs.remove(&toggled) {
                            this.open_dirs.insert(toggled.clone());
                        }
                        this.dirty = true;
                        cx.notify();
                    })),
                );
                let target = path.clone();
                right_click_target(
                    ("folder-menu", path_hash(path)),
                    row_el,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        // Only a folder with something to show raises a menu:
                        // songs directly in it ("Open folder") and/or in its
                        // subfolders ("Open full folder"). One pass over the
                        // library sorts every song under `target` into one of
                        // the two.
                        let lib = this.library.read(cx).library();
                        let (mut here, mut sub) = (false, false);
                        for song in lib.songs() {
                            if !song.path.starts_with(&target) {
                                continue;
                            }
                            if song.path.parent() == Some(target.as_path()) {
                                here = true;
                            } else {
                                sub = true;
                            }
                            if here && sub {
                                break;
                            }
                        }
                        if here || sub {
                            this.menu = Some(RailMenu {
                                position: event.position,
                                target: MenuTarget::Folder {
                                    path: target.clone(),
                                    here,
                                    sub,
                                },
                            });
                            cx.notify();
                        }
                    }),
                )
            }
            RailRow::NoMatches => empty_hint(theme, "No matches", false),
        }
    }
}

impl Container for PlaylistsView {
    fn container_id() -> &'static str {
        "playlists"
    }
}

/// A playlist row, snapshotted out of the library so the borrow is dropped
/// before elements are built.
#[derive(Clone, PartialEq)]
struct Row {
    id: PlaylistId,
    title: String,
    count: usize,
}

/// One flattened row of the rail — the unit the virtualized list renders.
/// `PartialEq` drives the minimal splice: only the stretch of rows that
/// actually changed is handed to the list, so the scroll position survives
/// rebuilds happening elsewhere.
#[derive(PartialEq)]
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
    /// One of the rail's filter boxes, rendered under its group's header.
    FilterBox(RailFilter),
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
            let new_rows = self.build_rows(cx);
            // Splice only the stretch that actually changed. gpui resets a
            // scroll top that falls inside the spliced range, so replacing the
            // whole list would fling the rail to the top every time a group
            // folded or a filter ran; a minimal splice leaves the viewport
            // alone, and gpui shifts it by the delta for changes above it.
            let (old, new) = (self.rows.as_ref(), new_rows.as_slice());
            let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
            let mut tail = old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count();
            tail = tail.min(old.len() - head).min(new.len() - head);
            let (old_end, new_end) = (old.len() - tail, new.len() - tail);
            if old[head..old_end] != new[head..new_end] {
                self.list_state.splice(head..old_end, new_end - head);
            }
            self.rows = Rc::new(new_rows);
            // Marquee state for rows that are gone is dropped with them.
            let keys: HashSet<u64> = self.rows.iter().filter_map(row_key).collect();
            self.marquees.retain(|key, _| keys.contains(key));
        }

        let rows = self.rows.clone();
        let render = cx.processor(move |this, ix, window: &mut Window, cx: &mut Context<Self>| {
            this.render_row(theme, &rows[ix], selected, window, cx)
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
                    if let Some((_, field, _)) = &mut this.renaming {
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
                // Each filter box only takes keys while it's focused — selected
                // by clicking it or by `ctrl+f`. `escape` clears the focused
                // filter before anything else; an empty one just blurs.
                let focused = [RailFilter::Albums, RailFilter::Artists]
                    .into_iter()
                    .find(|which| this.filter(*which).focus.is_focused(window));
                if event.keystroke.key == "escape" {
                    if let Some(which) = focused {
                        let filter = this.filter_mut(which);
                        if !filter.field.is_empty() {
                            filter.field.clear();
                            this.dirty = true;
                        } else {
                            window.focus(&this.focus_handle);
                        }
                        cx.notify();
                        return;
                    }
                }
                let mut consumed = false;
                if let Some(which) = focused {
                    consumed = this.filter_mut(which).field.handle_routed(event, cx);
                    if consumed {
                        cx.stop_propagation();
                        this.dirty = true;
                        cx.notify();
                    }
                }
                // Anything the field doesn't edit — `space`, the command
                // chords — goes to the same hub a center tab uses: `space`
                // pauses, `ctrl+shift+f` opens search, and `ctrl+f` comes right
                // back to this rail's album filter.
                if !consumed {
                    if let Some(action) = action_for_key(&event.keystroke) {
                        this.tabs.update(cx, |tabs, cx| tabs.dispatch(action, window, cx));
                    }
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
                Some(
                    div()
                        .id("open-settings")
                        .flex_none()
                        .px_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(theme.cell_px()))
                        .text_color(theme.text_muted)
                        .hover(|d| d.bg(theme.row_hover).text_color(theme.text))
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.tabs.update(cx, |tabs, cx| tabs.open_settings(window, cx));
                        }))
                        .child("⚙")
                        .into_any_element(),
                ),
                None,
                if scanning { Some(("scanning…", theme.text_muted)) } else { None },
                None,
            ))
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
                        .map(|p| p.title().to_string())
                        .unwrap_or_default();

                    let rename: MenuHandler = Box::new(cx.listener(move |this, _event, window, cx| {
                        this.menu = None;
                        let field = TextField::new(title.clone());
                        let focus = cx.focus_handle();
                        this.renaming = Some((playlist, field, focus.clone()));
                        this.dirty = true;
                        window.focus(&focus);
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
                MenuTarget::Folder { path, here, sub } => {
                    // The rail holds no playback handle, so opening a folder
                    // goes through the tab container as a request. "Open full
                    // folder" scopes the tab to the folder and its subfolders;
                    // "Open folder" only the folder's own songs. Nothing is
                    // played — the view opens on the library's place, and the
                    // header's shuffle button starts playback from there.
                    let (path, here, sub) = (path.clone(), *here, *sub);
                    let full_path = path.clone();
                    let open_full: MenuHandler = Box::new(cx.listener(
                        move |this, _event, _window, cx| {
                            this.menu = None;
                            this.library.update(cx, |state, cx| {
                                state.request(
                                    Request::FolderOpen(full_path.clone(), FolderMode::Full),
                                    cx,
                                )
                            });
                        },
                    ));
                    let open_here: MenuHandler = Box::new(cx.listener(
                        move |this, _event, _window, cx| {
                            this.menu = None;
                            this.library.update(cx, |state, cx| {
                                state.request(
                                    Request::FolderOpen(path.clone(), FolderMode::Here),
                                    cx,
                                )
                            });
                        },
                    ));
                    let mut items: Vec<(String, MenuHandler)> = Vec::new();
                    if sub {
                        items.push(("Open full folder".to_string(), open_full));
                    }
                    if here {
                        items.push(("Open folder".to_string(), open_here));
                    }
                    root = root.child(context_menu(theme, position, items, dismiss));
                }
            }
        }

        root
    }
}

/// One row of the rail — the single builder behind every group, plain
/// (playlists, albums) or tree (artists, the folder view). Tree rows draw one
/// guide column per nesting level, and the expand chevron is a painted
/// triangle sitting on its own guide line, so a subtree reads as one
/// continuous line from the arrow down through its children. No zebra: rows
/// sit transparent on the rail and light up on hover/selection. Labels overflow
/// as a marquee (fade at the right edge, slide on hover) instead of truncating;
/// `bits` carries that cell's slide and the row's hover, and the fade
/// dissolves into the same `wash` the row paints itself with.
#[allow(clippy::too_many_arguments)]
fn rail_row(
    theme: Theme,
    id: impl Into<ElementId>,
    key: u64,
    label: String,
    bits: LabelBits,
    count: Option<usize>,
    active: bool,
    depth: usize,
    branch: Branch,
    on_click: Option<impl Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
) -> AnyElement {
    // The chevron slot is one guide column wide on every row — empty on rows
    // that can't expand — so labels line up down the rail, and a parent's
    // arrow sits exactly on the line its children hang from.
    let guide = theme.cell_px() * 1.1;
    let slot = match branch {
        Branch::Plain => div().flex_none().w(px(guide)),
        Branch::Line => div().flex_none().w(px(guide)).border_l_1().border_color(theme.border),
        Branch::Toggle(expanded) => div()
            .flex_none()
            .relative()
            .w(px(guide))
            .border_l_1()
            .border_color(theme.border)
            .child(expand_triangle(theme, expanded)),
    };
    // The fade dissolves the text into whatever the row's background is, so
    // both come from the same `wash`: the plain rail, or the hover wash while
    // the pointer is over the row.
    let background = marquee::wash(theme.rail_bg, theme.row_hover, bits.state.is_region_hovered());
    div()
        .id(id.into())
        .flex()
        .w_full()
        .pl_2()
        .pr_2()
        .rounded_md()
        .cursor_pointer()
        .bg(background)
        .on_hover(bits.on_region)
        .when_some(on_click, |d, click| d.on_click(click))
        .children((0..depth).map(|_| {
            div()
                .flex_none()
                .w(px(guide))
                .border_l_1()
                .border_color(theme.border)
        }))
        .child(slot)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .justify_between()
                .py_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(marquee::marquee_text(
                            theme,
                            ("rail-label", key),
                            &label,
                            bits.width,
                            bits.chars,
                            theme.cell_px(),
                            bits.state.offset(),
                            if active { theme.text } else { theme.text_muted },
                            background,
                            bits.on_slide,
                        )),
                )
                .when_some(count, |d, count| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_faint)
                            .child(count.to_string()),
                    )
                }),
        )
        .into_any_element()
}

/// The row's own guide column: a plain row draws nothing, a tree row keeps
/// the line running through it (a leaf folder too — the trunk stays
/// connected), and an expandable one carries the chevron on that line.
enum Branch {
    Plain,
    Line,
    Toggle(bool),
}

/// The expand chevron: a small triangle painted over its guide line — right
/// when folded, down when open — instead of a text glyph, so it stays glued
/// to the line whatever the font does.
fn expand_triangle(theme: Theme, expanded: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let half = theme.font_size * 0.15;
            // Centered on the slot's left edge — the guide line's x — so the
            // line runs straight through the triangle.
            let x = f32::from(bounds.origin.x);
            let y = f32::from(bounds.origin.y + bounds.size.height / 2.0);
            let mut path = gpui::Path::new(point(px(x - half), px(y - half)));
            if expanded {
                path.line_to(point(px(x + half), px(y - half)));
                path.line_to(point(px(x), px(y + half)));
            } else {
                path.line_to(point(px(x - half), px(y + half)));
                path.line_to(point(px(x + half), px(y)));
            }
            window.paint_path(path, theme.text_faint);
        },
    )
    .absolute()
    .inset_0()
}

/// A row wrapped so a right-click on it can raise a menu.
fn right_click_target(
    id: impl Into<ElementId>,
    row: AnyElement,
    on_menu: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .w_full()
        .on_mouse_down(MouseButton::Right, on_menu)
        .child(row)
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

/// A stable marquee key for a rail row: the row's identity hashed with its
/// kind, so ids from different namespaces (playlist ids, artist indexes,
/// folder paths) can never collide. Rows without a marquee have none — they
/// only show in the hover set, never in the map.
fn row_key(kind: &RailRow) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match kind {
        RailRow::NewPlaylist => "new".hash(&mut hasher),
        RailRow::Custom(row) => {
            "custom".hash(&mut hasher);
            row.id.hash(&mut hasher);
        }
        RailRow::Album(row) => {
            "album".hash(&mut hasher);
            row.id.hash(&mut hasher);
        }
        RailRow::Discography(row) => {
            "discography".hash(&mut hasher);
            row.id.hash(&mut hasher);
        }
        RailRow::Artist { index, .. } => {
            "artist".hash(&mut hasher);
            index.hash(&mut hasher);
        }
        RailRow::Folder { path, .. } => {
            "folder".hash(&mut hasher);
            path.hash(&mut hasher);
        }
        _ => return None,
    }
    Some(hasher.finish())
}

/// A stable element id for a directory row.
fn path_hash(path: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}
