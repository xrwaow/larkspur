//! A tiny context-menu helper.
//!
//! GPUI 0.2 has no context-menu widget, so right-click menus are built from an
//! [`anchored`] box positioned at the click, drawn on top of the view via
//! [`deferred`], and dismissed with `on_mouse_down_out`. The menu occludes the
//! mouse so a click on an item can't also land on the row behind it.

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, App, ClickEvent, Corner, Entity,
    MouseDownEvent, Point, Pixels, Window,
};

use crate::model::{PlaylistId, SongId};
use crate::ui::library_state::LibraryState;
use crate::ui::state::PlaybackState;
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
}

/// The items a song's context menu shows.
///
/// For a single song: "Go to {artist}" for each credited artist, "Go to
/// playlist" for its album, and (in a custom playlist) "Remove from playlist".
/// For a multi-row selection: "Play", one "Add to {playlist}" per custom
/// playlist, "New playlist from selection", and (in a custom playlist) "Remove
/// from playlist".
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
    cx: &App,
) -> Vec<(String, MenuHandler)> {
    let mut items: Vec<(String, MenuHandler)> = Vec::new();
    let custom_context = context
        .and_then(|id| library.read(cx).library().playlist(id))
        .is_some_and(|playlist| playlist.is_custom());

    if songs.len() <= 1 {
        let Some(&song) = songs.first() else { return items };
        let (artists, album) = {
            let state = library.read(cx);
            let lib = state.library();
            (
                lib.get(song).map(|song| song.artists.clone()).unwrap_or_default(),
                lib.album_playlist_of(song),
            )
        };

        for artist in artists {
            let library = library.clone();
            items.push((
                format!("Go to {artist}"),
                Box::new(move |_event, _window, cx| {
                    let artist = artist.clone();
                    library.update(cx, |state, cx| state.request_artist_view(artist, cx));
                }),
            ));
        }

        if let Some(album) = album {
            let library = library.clone();
            items.push((
                "Go to playlist".to_string(),
                Box::new(move |_event, _window, cx| {
                    library.update(cx, |state, cx| state.request_open_playlist(album, cx));
                }),
            ));
        }

        if let Some(playlist) = context.filter(|_| custom_context) {
            let library = library.clone();
            items.push((
                "Remove from playlist".to_string(),
                Box::new(move |_event, _window, cx| {
                    library.update(cx, |state, cx| state.remove_song(playlist, song, cx));
                }),
            ));
        }

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
                library.update(cx, |state, cx| state.play(&songs, 0, &playback, cx));
            }),
        ));
    }

    let custom: Vec<(PlaylistId, String)> = library
        .read(cx)
        .library()
        .custom_playlists()
        .iter()
        .map(|playlist| (playlist.id, playlist.meta.title.clone()))
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

    items
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
