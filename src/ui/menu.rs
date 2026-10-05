//! A tiny context-menu helper.
//!
//! GPUI 0.2 has no context-menu widget, so right-click menus are built from an
//! [`anchored`] box positioned at the click, drawn on top of the view via
//! [`deferred`], and dismissed with `on_mouse_down_out`. The menu occludes the
//! mouse so a click on an item can't also land on the row behind it.

use std::path::{Path, PathBuf};

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, App, ClickEvent, Corner, Entity,
    MouseDownEvent, Point, Pixels, Window,
};

use crate::model::{FolderMode, PlaylistId, SongId};
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;

/// A menu item's click handler.
pub type MenuHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static>;

/// A request to open a song's context menu, raised by a row's right-click and
/// rendered by whichever view owns it.
#[derive(Clone)]
pub struct SongMenuRequest {
    /// The songs the menu acts on: one for a plain right-click, the whole
    /// selection for a multi-row one.
    pub songs: Vec<SongId>,
    /// Where the click landed, in window coordinates.
    pub position: Point<Pixels>,
    /// The playlist tab the menu was raised from, if any — lets the menu offer
    /// "Remove from playlist" when that playlist is a custom one.
    pub playlist: Option<PlaylistId>,
    /// Whether the menu was raised from the queue panel — swaps the playlist
    /// operations for "Remove from queue".
    pub queue: bool,
    /// The playlist a right-clicked icon card represents, if the menu was
    /// raised from one — lets the card's menu offer "Go to {artist}" per
    /// credited artist and "Go to playlist" for the card itself.
    pub card: Option<PlaylistId>,
}

/// A preset menu item: "Add to Queue" for one song.
fn add_to_queue_item(
    library: &Entity<LibraryState>,
    playback: &Entity<PlaybackState>,
    song: SongId,
) -> (String, MenuHandler) {
    let library = library.clone();
    let playback = playback.clone();
    (
        "Add to Queue".to_string(),
        Box::new(move |_event, _window, cx| {
            let entry =
                library.read(cx).library().get(song).map(|song| (song.id, song.path.clone()));
            if let Some(entry) = entry {
                playback.update(cx, |state, cx| state.add_to_queue(vec![entry], cx));
            }
        }),
    )
}

/// A preset menu item: "Go to {artist}", opening the artist's discography tab.
fn go_to_artist_item(library: &Entity<LibraryState>, artist: String) -> (String, MenuHandler) {
    let library = library.clone();
    (
        format!("Go to {artist}"),
        Box::new(move |_event, _window, cx| {
            let artist = artist.clone();
            library.update(cx, |state, cx| state.request(Request::ArtistView(artist), cx));
        }),
    )
}

/// A preset menu item: "Go to playlist", opening the playlist's tab.
fn go_to_playlist_item(library: &Entity<LibraryState>, playlist: PlaylistId) -> (String, MenuHandler) {
    let library = library.clone();
    (
        "Go to playlist".to_string(),
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.request(Request::OpenPlaylist(playlist), cx));
        }),
    )
}

/// A preset menu item: "Go to folder", opening the folder's own view — its
/// songs only, no subfolders.
fn go_to_folder_item(library: &Entity<LibraryState>, dir: PathBuf) -> (String, MenuHandler) {
    let library = library.clone();
    (
        "Go to folder".to_string(),
        Box::new(move |_event, _window, cx| {
            library
                .update(cx, |state, cx| state.request(Request::FolderOpen(dir.clone(), FolderMode::Here), cx));
        }),
    )
}

/// A preset menu item: "Remove from playlist" for one song of a custom one.
fn remove_from_playlist_item(
    library: &Entity<LibraryState>,
    playlist: PlaylistId,
    song: SongId,
) -> (String, MenuHandler) {
    let library = library.clone();
    (
        "Remove from playlist".to_string(),
        Box::new(move |_event, _window, cx| {
            library.update(cx, |state, cx| state.remove_song(playlist, song, cx));
        }),
    )
}

/// The items a song's context menu shows.
///
/// For a single song: "Go to {artist}" for each credited artist, "Go to
/// playlist" for its album, and (in a custom playlist) "Remove from playlist".
/// For a multi-row selection: "Play", one "Add to {playlist}" per custom
/// playlist, "New playlist from selection", and (in a custom playlist) "Remove
/// from playlist". A menu raised from an icon-grid playlist card also offers
/// "Go to {artist}" for every artist the playlist credits, "Go to playlist"
/// for the card itself. A single song also offers "Go to folder" — the folder
/// view of its own directory, no subfolders.
///
/// The cross-view actions go through [`LibraryState`] as requests — the tab
/// container and the browse view pick them up — so a row in the browse view can
/// open a playlist tab or an artist view without holding a handle to the
/// container that owns it.
pub fn song_menu_items(
    library: &Entity<LibraryState>,
    playback: &Entity<PlaybackState>,
    songs: &[SongId],
    context: Option<PlaylistId>,
    queue: bool,
    playlist_card: Option<PlaylistId>,
    cx: &App,
) -> Vec<(String, MenuHandler)> {
    let mut items: Vec<(String, MenuHandler)> = Vec::new();
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
            items.push((
                "Remove from queue".to_string(),
                Box::new(move |_event, _window, cx| {
                    remove_queued(&playback, &selected, cx);
                }),
            ));
        }

        if let Some(album) = album {
            items.push(go_to_playlist_item(library, album));
        }

        if let Some(folder) = folder {
            items.push(go_to_folder_item(library, folder));
        }

        if let Some(playlist) = context.filter(|_| custom_context) {
            items.push(remove_from_playlist_item(library, playlist, song));
        }

        // "Go to {artist}" closes the menu, after every other option.
        items.extend(artists.into_iter().map(|artist| go_to_artist_item(library, artist)));

        return items;
    }

    // A multi-row selection: bulk operations.
    let count = songs.len();

    {
        let library = library.clone();
        let playback = playback.clone();
        let songs = songs.to_vec();
        items.push((
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
        items.push((
            format!("Add {count} songs to queue"),
            Box::new(move |_event, _window, cx| {
                let entries: Vec<(SongId, PathBuf)> = songs
                    .iter()
                    .filter_map(|id| {
                        library.read(cx).library().get(*id).map(|song| (song.id, song.path.clone()))
                    })
                    .collect();
                playback.update(cx, |state, cx| state.add_to_queue(entries, cx));
            }),
        ));
    }

    if queue {
        let playback = playback.clone();
        let songs = songs.to_vec();
        items.push((
            format!("Remove {count} songs from queue"),
            Box::new(move |_event, _window, cx| {
                remove_queued(&playback, &songs, cx);
            }),
        ));
    }

    // A menu raised from a playlist card: "Go to playlist" and "Go to
    // folder" for the card — the folder its first song lives in; its artists
    // are collected for the "Go to {artist}" items that close every menu.
    let mut card_artists: Option<Vec<String>> = None;
    if let Some(card) = playlist_card {
        let folder = songs
            .first()
            .and_then(|id| library.read(cx).library().get(*id))
            .and_then(|song| song.path.parent().map(Path::to_path_buf));
        if let Some(folder) = folder {
            items.push(go_to_folder_item(library, folder));
        }
        let mut artists: Vec<String> = Vec::new();
        for id in songs {
            let Some(song) = library.read(cx).library().get(*id) else { continue };
            for artist in &song.artists {
                if !artists.contains(artist) {
                    artists.push(artist.clone());
                }
            }
        }
        items.push(go_to_playlist_item(library, card));
        card_artists = Some(artists);
    }

    let custom: Vec<(PlaylistId, String)> = library
        .read(cx)
        .library()
        .custom_playlists()
        .iter()
        .map(|playlist| (playlist.id, playlist.title().to_string()))
        .collect();
    for (id, title) in custom {
        let library = library.clone();
        let songs = songs.to_vec();
        items.push((
            format!("Add {count} songs to {title}"),
            Box::new(move |_event, _window, cx| {
                library.update(cx, |state, cx| state.add_songs(id, songs.clone(), cx));
            }),
        ));
    }

    {
        let library = library.clone();
        let songs = songs.to_vec();
        items.push((
            format!("New playlist from {count} songs"),
            Box::new(move |_event, _window, cx| {
                library.update(cx, |state, cx| state.new_playlist_from(songs.clone(), cx));
            }),
        ));
    }

    if let Some(playlist) = context.filter(|_| custom_context) {
        let library = library.clone();
        let songs = songs.to_vec();
        items.push((
            format!("Remove {count} songs from playlist"),
            Box::new(move |_event, _window, cx| {
                library.update(cx, |state, cx| state.remove_songs(playlist, songs.clone(), cx));
            }),
        ));
    }

    // "Go to {artist}" closes the menu, whichever branch built it.
    if let Some(artists) = card_artists {
        items.extend(artists.into_iter().map(|artist| go_to_artist_item(library, artist)));
    }

    items
}

/// Drop every queue entry whose id is in `songs` — the queue menu's "Remove
/// from queue".
fn remove_queued(playback: &Entity<PlaybackState>, songs: &[SongId], cx: &mut gpui::App) {
    playback.update(cx, |state, cx| state.remove_from_queue(songs.to_vec(), cx));
}

/// Build a context menu at `position` (window coordinates).
///
/// `on_dismiss` fires on a mouse-down outside the menu — the view should clear
/// whatever state is showing it. The menu occludes the mouse, so a click on an
/// item never also reaches the row behind it.
pub fn context_menu(
    theme: Theme,
    position: Point<Pixels>,
    items: Vec<(String, MenuHandler)>,
    on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let buttons = items.into_iter().enumerate().map(|(index, (label, on_click))| {
        div()
            .id(("context-menu-item", index))
            .w_full()
            .px_3()
            .py_1()
            .cursor_pointer()
            .text_size(px(theme.cell_px()))
            .text_color(theme.text)
            .hover(|d| d.bg(theme.row_hover))
            .on_click(on_click)
            .child(label)
            .into_any_element()
    });

    let menu = div()
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
        .on_mouse_down_out(on_dismiss)
        .children(buttons);

    deferred(anchored().position(position).anchor(Corner::TopLeft).child(menu))
        .with_priority(100)
        .into_any_element()
}
