//! A tiny context-menu helper.
//!
//! GPUI 0.2 has no context-menu widget, so right-click menus are built from an
//! [`anchored`] box positioned at the click, drawn on top of the view via
//! [`deferred`], and dismissed with `on_mouse_down_out`. The menu occludes the
//! mouse so a click on an item can't also land on the row behind it.
//!
//! Menus are a tree: an [`MenuItem`] is either an action or a submenu. A
//! submenu opens beside its parent row when the row is hovered, recursively,
//! so long lists (artists, custom playlists, destinations) stay folded away
//! until asked for.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, App, ClickEvent, Corner, Div, ElementId,
    Entity, MouseDownEvent, Pixels, Point, Window,
};

use crate::model::{FolderMode, PlaylistId, QueueEntry, SongId};
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;

/// A menu item's click handler.
pub type MenuHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static>;

/// One row of a context menu: either a directly clickable action, or a
/// submenu that opens on hover.
pub enum MenuItem {
    Action { label: String, on_click: MenuHandler },
    Submenu { label: String, items: Vec<MenuItem> },
}

impl MenuItem {
    pub fn action(label: impl Into<String>, on_click: MenuHandler) -> Self {
        Self::Action { label: label.into(), on_click }
    }

    pub fn submenu(label: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self::Submenu { label: label.into(), items }
    }
}

/// Where a song menu was raised from. Each variant names exactly the actions
/// that origin can offer, so impossible combinations (queue *and* playlist,
/// a card *and* a row) are unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuOrigin {
    /// A song row (or a multi-row selection); `playlist` is the tab it came
    /// from, when that tab is a playlist.
    Row { playlist: Option<PlaylistId> },
    /// The queue panel.
    Queue,
    /// An icon-grid playlist card.
    Card(PlaylistId),
}

/// A request to open a song's context menu, raised by a row's right-click and
/// rendered by whichever view owns it.
#[derive(Clone)]
pub struct SongMenuRequest {
    /// The songs the menu acts on: one for a plain right-click, the whole
    /// selection for a multi-row one.
    pub songs: Vec<SongId>,
    /// Where the click landed, in window coordinates.
    pub position: Point<Pixels>,
    /// What raised the menu — decides which origin-specific items appear.
    pub origin: MenuOrigin,
}

/// A preset menu item: "Add to Queue" for one song.
fn add_to_queue_item(
    library: &Entity<LibraryState>,
    playback: &Entity<PlaybackState>,
    song: SongId,
) -> MenuItem {
    let library = library.clone();
    let playback = playback.clone();
    MenuItem::action(
        "Add to Queue",
        Box::new(move |_event, _window, cx| {
            let entries = library.read(cx).queue_entries(&[song]);
            if !entries.is_empty() {
                playback.update(cx, |state, cx| state.add_to_queue(entries, cx));
            }
        }),
    )
}

/// A preset menu item that raises a cross-view request on the library.
fn request_item(
    label: impl Into<String>,
    library: &Entity<LibraryState>,
    request: Request,
) -> MenuItem {
    let library = library.clone();
    MenuItem::action(
        label,
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.request(request.clone(), cx));
        }),
    )
}

/// A preset menu item: "Remove from playlist" for one song of a custom one.
fn remove_from_playlist_item(
    library: &Entity<LibraryState>,
    playlist: PlaylistId,
    song: SongId,
) -> MenuItem {
    let library = library.clone();
    MenuItem::action(
        "Remove from playlist",
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.remove_song(playlist, song, cx));
        }),
    )
}

/// One "add to this playlist" action for any number of songs.
fn add_songs_item(
    library: &Entity<LibraryState>,
    playlist: PlaylistId,
    label: impl Into<String>,
    songs: &[SongId],
) -> MenuItem {
    let library = library.clone();
    let songs = songs.to_vec();
    MenuItem::action(
        label,
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.add_songs(playlist, songs.clone(), cx));
        }),
    )
}

/// "New playlist from these songs" — the tail of every add-to-playlist menu.
fn new_playlist_item(
    library: &Entity<LibraryState>,
    songs: &[SongId],
    single: bool,
) -> MenuItem {
    let library = library.clone();
    let songs = songs.to_vec();
    let label = if single {
        "New playlist…".to_string()
    } else {
        format!("New playlist from {} songs", songs.len())
    };
    MenuItem::action(
        label,
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.new_playlist_from(songs.clone(), cx));
        }),
    )
}

/// The "Add to playlist" part of a song menu.
///
/// With one custom playlist the action is direct; with several they fold into
/// an "Add to playlist" submenu. "New playlist" rides along, at the end of
/// that submenu when there is one, so the bulk list stays tidy.
fn add_to_playlist_menu(
    library: &Entity<LibraryState>,
    songs: &[SongId],
    single: bool,
    cx: &App,
) -> Vec<MenuItem> {
    let custom: Vec<(PlaylistId, String)> = library
        .read(cx)
        .library()
        .custom_playlists()
        .iter()
        .map(|playlist| (playlist.id, playlist.title().to_string()))
        .collect();

    match custom.len() {
        0 => vec![new_playlist_item(library, songs, single)],
        1 => {
            let (id, title) = custom.into_iter().next().expect("one playlist");
            vec![
                add_songs_item(library, id, format!("Add to {title}"), songs),
                new_playlist_item(library, songs, single),
            ]
        }
        _ => {
            let mut children: Vec<MenuItem> = custom
                .into_iter()
                .map(|(id, title)| add_songs_item(library, id, title, songs))
                .collect();
            children.push(new_playlist_item(library, songs, single));
            vec![MenuItem::submenu("Add to playlist", children)]
        }
    }
}

/// A destination reachable from a song menu. Kept as data so a lone
/// destination can be shown directly while several fold under "Go to".
enum GoTarget {
    Artist(String),
    Artists(Vec<String>),
    Playlist(PlaylistId),
    Folder(PathBuf),
}

/// Render one destination. At the menu root a lone target keeps its "Go to"
/// wording; inside the "Go to" submenu the shorter name is enough.
fn go_target_item(
    target: GoTarget,
    root: bool,
    library: &Entity<LibraryState>,
) -> MenuItem {
    match target {
        GoTarget::Artist(artist) => {
            let label = if root { format!("Go to {artist}") } else { artist.clone() };
            request_item(label, library, Request::ArtistView(artist))
        }
        GoTarget::Artists(artists) => {
            let children = artists
                .into_iter()
                .map(|artist| {
                    let label = artist.clone();
                    request_item(label, library, Request::ArtistView(artist))
                })
                .collect();
            MenuItem::submenu("Artists", children)
        }
        GoTarget::Playlist(id) => {
            let label = if root { "Go to playlist" } else { "Playlist" };
            request_item(label, library, Request::OpenPlaylist(id))
        }
        GoTarget::Folder(path) => {
            let label = if root { "Go to folder" } else { "Folder" };
            request_item(label, library, Request::FolderOpen(path, FolderMode::Here))
        }
    }
}

/// The "Go to" part of a song menu.
///
/// One destination is shown directly (so a song by one artist has a single
/// "Go to {artist}" row); several fold under a "Go to" submenu, and several
/// credited artists fold again under "Artists".
fn go_to_menu(
    library: &Entity<LibraryState>,
    artists: Vec<String>,
    album: Option<PlaylistId>,
    folder: Option<PathBuf>,
) -> Vec<MenuItem> {
    let mut targets = Vec::new();
    match artists.len() {
        0 => {}
        1 => targets.push(GoTarget::Artist(artists.into_iter().next().expect("one artist"))),
        _ => targets.push(GoTarget::Artists(artists)),
    }
    if let Some(album) = album {
        targets.push(GoTarget::Playlist(album));
    }
    if let Some(folder) = folder {
        targets.push(GoTarget::Folder(folder));
    }

    match targets.len() {
        0 => Vec::new(),
        1 => vec![go_target_item(targets.pop().expect("one target"), true, library)],
        _ => vec![MenuItem::submenu(
            "Go to",
            targets
                .into_iter()
                .map(|target| go_target_item(target, false, library))
                .collect(),
        )],
    }
}

/// The items a song's context menu shows.
///
/// For a single song: "Add to Queue", its "Add to playlist" and "Go to"
/// destinations, and (in a custom playlist) "Remove from playlist". For a
/// multi-row selection: "Play", "Add to queue", "Add to playlist", "New
/// playlist", and "Remove from playlist" when the tab is a custom one. A menu
/// raised from an icon-grid playlist card also offers the card's own "Go to"
/// destinations and every artist the playlist credits.
///
/// The cross-view actions go through [`LibraryState`] as requests — the tab
/// container and the browse view pick them up — so a row in the browse view can
/// open a playlist tab or an artist view without holding a handle to the
/// container that owns it.
pub fn song_menu_items(
    library: &Entity<LibraryState>,
    playback: &Entity<PlaybackState>,
    songs: &[SongId],
    origin: &MenuOrigin,
    cx: &App,
) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = Vec::new();
    let context = match *origin {
        MenuOrigin::Row { playlist } => playlist,
        MenuOrigin::Queue | MenuOrigin::Card(_) => None,
    };
    let queue = matches!(origin, MenuOrigin::Queue);
    let playlist_card = match *origin {
        MenuOrigin::Card(id) => Some(id),
        _ => None,
    };
    let custom_context = context
        .and_then(|id| library.read(cx).library().playlist(id))
        .is_some_and(|playlist| playlist.is_custom());

    if songs.len() <= 1 {
        let Some(&song) = songs.first() else { return items };
        let (artists, album, folder) = {
            let lib = library.read(cx).library();
            (
                lib.get(song).map(|song| song.artists.clone()).unwrap_or_default(),
                lib.album_playlist_of(song),
                lib.get(song).and_then(|song| song.path.parent().map(Path::to_path_buf)),
            )
        };

        // The playback action leads: queueing is what a right-click is most
        // often after.
        items.push(add_to_queue_item(library, playback, song));

        if queue {
            // Resolve the paths from the controller's queue rather than the
            // library: files queued from the command line may not be scanned
            // yet, so matching the hashed id against the queue is the only
            // reliable mapping.
            let playback = playback.clone();
            let selected = vec![song];
            items.push(MenuItem::action(
                "Remove from queue",
                Box::new(move |_event, _window, cx| {
                    remove_queued(&playback, &selected, cx);
                }),
            ));
        }

        items.extend(add_to_playlist_menu(library, &[song], true, cx));
        items.extend(go_to_menu(library, artists, album, folder));

        if let Some(playlist) = context.filter(|_| custom_context) {
            items.push(remove_from_playlist_item(library, playlist, song));
        }

        return items;
    }

    // A multi-row selection: bulk operations.
    let count = songs.len();

    {
        let library = library.clone();
        let playback = playback.clone();
        let songs = songs.to_vec();
        items.push(MenuItem::action(
            format!("Play {count} songs"),
            Box::new(move |_event, _window, cx| {
                // Plays the selection and opens it as a temporary playlist tab.
                library.update(cx, |state, cx| {
                    state.play_selection(songs.clone(), &playback, cx)
                });
            }),
        ));
    }

    {
        let library = library.clone();
        let playback = playback.clone();
        let songs = songs.to_vec();
        items.push(MenuItem::action(
            format!("Add {count} songs to queue"),
            Box::new(move |_event, _window, cx| {
                let entries: Vec<QueueEntry> = library.read(cx).queue_entries(&songs);
                playback.update(cx, |state, cx| state.add_to_queue(entries, cx));
            }),
        ));
    }

    if queue {
        let playback = playback.clone();
        let songs = songs.to_vec();
        items.push(MenuItem::action(
            format!("Remove {count} songs from queue"),
            Box::new(move |_event, _window, cx| {
                remove_queued(&playback, &songs, cx);
            }),
        ));
    }

    // A menu raised from a playlist card: the card itself and the folder its
    // first song lives in are extra "Go to" destinations, alongside every
    // artist the playlist credits.
    let mut artists: Vec<String> = Vec::new();
    let mut folder = None;
    let mut album = None;
    if let Some(card) = playlist_card {
        folder = songs
            .first()
            .and_then(|id| library.read(cx).library().get(*id))
            .and_then(|song| song.path.parent().map(Path::to_path_buf));
        for id in songs {
            let Some(song) = library.read(cx).library().get(*id) else { continue };
            for artist in &song.artists {
                if !artists.contains(artist) {
                    artists.push(artist.clone());
                }
            }
        }
        album = Some(card);
    }
    items.extend(go_to_menu(library, artists, album, folder));

    items.extend(add_to_playlist_menu(library, songs, false, cx));

    if let Some(playlist) = context.filter(|_| custom_context) {
        let library = library.clone();
        let songs = songs.to_vec();
        items.push(MenuItem::action(
            format!("Remove {count} songs from playlist"),
            Box::new(move |_event, _window, cx| {
                library.update(cx, |state, cx| state.remove_songs(playlist, songs.clone(), cx));
            }),
        ));
    }

    items
}

/// Drop every queue entry whose id is in `songs` — the queue menu's "Remove
/// from queue".
fn remove_queued(playback: &Entity<PlaybackState>, songs: &[SongId], cx: &mut gpui::App) {
    playback.update(cx, |state, cx| state.remove_from_queue(songs.to_vec(), cx));
}

/// A stable id fragment for a nested menu path.
fn path_key(path: &[usize]) -> String {
    path.iter().map(|index| index.to_string()).collect::<Vec<_>>().join("-")
}

type HoverHandler = Rc<dyn Fn(&Option<Vec<usize>>, &mut Window, &mut App)>;
type DismissHandler = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// Build one menu panel: a column of rows, with any open submenu attached to
/// whichever side of its parent row has room. Recurses for nested submenus.
fn menu_panel(
    theme: Theme,
    items: Vec<MenuItem>,
    path: &[usize],
    open: &[usize],
    open_left: bool,
    on_hover: &HoverHandler,
    on_dismiss: &DismissHandler,
) -> Div {
    let prefix = path.to_vec();
    let entries = items.into_iter().enumerate().map(|(index, item)| {
        let mut item_path = prefix.clone();
        item_path.push(index);
        let id = ElementId::Name(
            format!("context-menu-{}", path_key(&item_path)).into(),
        );

        match item {
            MenuItem::Action { label, on_click } => {
                let hover = on_hover.clone();
                // Hovering a direct action closes any submenu opened from
                // this panel, but leaves the panel itself open.
                let parent_path = if prefix.is_empty() { None } else { Some(prefix.clone()) };
                div()
                    .id(id)
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_size(px(theme.cell_px()))
                    .text_color(theme.text)
                    .hover(|d| d.bg(theme.row_hover))
                    .on_hover(move |hovered, window, cx| {
                        // Moving onto an action abandons any open submenu.
                        if *hovered {
                            hover(&parent_path, window, cx);
                        }
                    })
                    .on_click(on_click)
                    .child(label)
                    .into_any_element()
            }
            MenuItem::Submenu { label, items } => {
                let hover = on_hover.clone();
                let sub_path = item_path.clone();
                // A submenu stays open while any deeper submenu is open: the
                // open path is the full chain, not just the leaf.
                let is_open = open.starts_with(&item_path);
                let row = div()
                    .id(id)
                    .relative()
                    .px_3()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_size(px(theme.cell_px()))
                    .text_color(theme.text)
                    // A left-opening submenu puts its chevron on the left, so
                    // keep the label beside it instead of pushing it across.
                    .when(open_left, |d| d.justify_start())
                    .when(!open_left, |d| d.justify_between())
                    .hover(|d| d.bg(theme.row_hover))
                    .on_hover(move |hovered, window, cx| {
                        if *hovered {
                            hover(&Some(sub_path.clone()), window, cx);
                        }
                    });

                let chevron = if open_left { "‹" } else { "›" };
                let row = if open_left {
                    row.child(
                        div()
                            .flex_none()
                            .text_color(theme.text_faint)
                            .child(chevron),
                    )
                    .child(label)
                } else {
                    row.child(label).child(
                        div()
                            .flex_none()
                            .text_color(theme.text_faint)
                            .child(chevron),
                    )
                };

                if is_open {
                    let panel =
                        menu_panel(theme, items, &item_path, open, open_left, on_hover, on_dismiss)
                            .absolute()
                            .top_0();
                    row.child(if open_left {
                        // Cover the parent's edge instead of drawing a
                        // second border beside it.
                        panel.right_full().mr(px(-1.0))
                    } else {
                        panel.left_full().ml(px(-1.0))
                    })
                } else {
                    row
                }
                .into_any_element()
            }
        }
    });

    // Keep the menu alive while the pointer is inside an open submenu: only
    // the deepest open panel dismisses on an outside click.
    let deeper = open.starts_with(&prefix) && open.len() > prefix.len();
    let dismiss = on_dismiss.clone();
    div()
        .flex()
        .flex_col()
        .min_w(px(theme.font_size * 9.0))
        .py_1()
        .rounded_md()
        .bg(theme.row_active)
        .border_1()
        .border_color(theme.border)
        // Block the mouse from the rows behind the menu: their hitboxes overlap
        // it, and without this a click on an item also plays the song under it.
        .occlude()
        .on_mouse_down_out(move |event, window, cx| {
            if !deeper {
                dismiss(event, window, cx);
            }
        })
        .children(entries)
}

/// Build a context menu at `position` (window coordinates).
///
/// `open` is the path of the submenu currently expanded (empty for none);
/// `on_hover` is called with the path the pointer moved onto, or `None` when
/// it moves onto a directly clickable item. The view owning the menu stores
/// that path and re-renders. `on_dismiss` fires on a mouse-down outside the
/// menu — the view should clear whatever state is showing it. The menu
/// occludes the mouse, so a click on an item never also reaches the row
/// behind it.
pub fn context_menu(
    theme: Theme,
    position: Point<Pixels>,
    window: &Window,
    items: Vec<MenuItem>,
    open: &[usize],
    on_hover: impl Fn(&Option<Vec<usize>>, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let on_hover: HoverHandler = Rc::new(on_hover);
    let on_dismiss: DismissHandler = Rc::new(on_dismiss);
    let open = open.to_vec();

    // Submenus need room for themselves and their descendants. When the click
    // is close enough to the right edge that a chain of panels would overflow,
    // open the menu and every submenu to the left instead. The reserve scales
    // with the UI font but never eats more than half a narrow window.
    let viewport_width = window.viewport_size().width;
    let reserve = px((theme.font_size * 30.0).min(f32::from(viewport_width) * 0.5));
    let open_left = position.x > viewport_width - reserve;
    let anchor = if open_left { Corner::TopRight } else { Corner::TopLeft };

    let menu = menu_panel(theme, items, &[], &open, open_left, &on_hover, &on_dismiss);

    deferred(anchored().position(position).anchor(anchor).child(menu))
        .with_priority(100)
        .into_any_element()
}
