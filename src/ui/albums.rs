//! Album sections — the shared rendering behind the browse, search, and
//! playlist containers.
//!
//! All three draw the same thing: an album headed by its cover, the artist, the
//! album title, a `format | bitrate | tracks | time` line and the year, with its
//! tracks underneath. Only what feeds them differs — browse passes every album,
//! search passes what a query matched, a playlist passes its own songs — so the
//! snapshot, the header, and the track row all live here.
//!
//! Column widths are derived from the theme's font size (see [`Theme`]), so
//! raising a container's font size scales the columns with the text rather than
//! misaligning them.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, AnyElement, App, ClickEvent, Context, ElementId, Entity, MouseButton,
    MouseDownEvent, ObjectFit, Window,
};

use crate::model::search::AlbumGroup;
use crate::model::{Library, PlaylistId, SongId, SongMetadata};
use crate::ui::cover_store::{CoverImage, CoverStore};
use crate::ui::format::{format_bitrate, format_duration, format_secs};
use crate::ui::library_state::LibraryState;
use crate::ui::theme::Theme;

/// Cover thumbnail size in an album header.
pub const THUMB_PX: f32 = 64.0;

/// Cover thumbnail size in a compressed one-track section.
const THUMB_SMALL_PX: f32 = 40.0;

const MIN_TITLE_CHARS: usize = 8;
const MAX_TITLE_CHARS: usize = 70;

/// One track row, snapshotted out of the library.
pub struct TrackRow {
    pub song: SongId,
    pub number: usize,
    pub title: String,
    pub artist: String,
    pub secs: u64,
    /// The file's declared/average bitrate, in bps. `None` when the tags don't
    /// carry one.
    pub nominal_bitrate: Option<u32>,
}

/// One album section — its header data plus its tracks.
pub struct AlbumSection {
    pub playlist: PlaylistId,
    pub artist: String,
    pub album: String,
    /// The `format | bitrate | tracks | time` line.
    pub meta: String,
    pub year: String,
    /// The song whose cover represents the album.
    pub cover_song: Option<SongId>,
    pub tracks: Vec<TrackRow>,
}

/// Snapshot the library into drawable sections.
pub fn all_sections(library: &Library) -> Vec<AlbumSection> {
    sections_from_groups(library, crate::model::search::all_albums(library))
}

/// Snapshot a set of album groups (e.g. search results) into drawable sections.
pub fn sections_from_groups(library: &Library, groups: Vec<AlbumGroup>) -> Vec<AlbumSection> {
    groups.into_iter().filter_map(|group| section(library, group)).collect()
}

/// Snapshot one playlist — the playlist container's entry point.
pub fn section_for_playlist(library: &Library, playlist: PlaylistId) -> Option<AlbumSection> {
    let playlist = library.playlist(playlist)?;
    section(library, AlbumGroup { playlist: playlist.id, songs: playlist.song_ids.clone() })
}

fn section(library: &Library, group: AlbumGroup) -> Option<AlbumSection> {
    let playlist = library.playlist(group.playlist)?;
    let songs: Vec<&SongMetadata> = group.songs.iter().filter_map(|id| library.get(*id)).collect();

    let tracks = songs
        .iter()
        .enumerate()
        .map(|(index, song)| TrackRow {
            song: song.id,
            number: song.track_position.map(usize::from).unwrap_or(index + 1),
            title: song.display_title(),
            artist: song.artists.join(", "),
            secs: song.duration.as_secs(),
            nominal_bitrate: song.nominal_bitrate,
        })
        .collect();

    // An empty playlist (a freshly created custom one) still needs a header —
    // taken from the playlist's own metadata, since there's no first song to
    // borrow artist/year/cover from.
    let custom = playlist.is_custom();
    let (artist, meta, year, cover_song) = match songs.first() {
        Some(first) => {
            let total_secs: u64 = songs.iter().map(|song| song.duration.as_secs()).sum();
            (
                // A custom playlist lists whatever the user put in it: the one
                // shared artist when every track agrees, else "Various Artists".
                // An autogen playlist records its album artist; fall back to the
                // first track's for anything else.
                if custom {
                    custom_artist(&songs)
                } else {
                    playlist
                        .meta
                        .artist
                        .clone()
                        .unwrap_or_else(|| first.album_artist().to_string())
                },
                album_meta(first, songs.len(), total_secs),
                // A custom playlist spans arbitrary tracks, so a single release
                // year would be misleading — only albums carry one.
                if custom {
                    String::new()
                } else {
                    songs
                        .iter()
                        .find_map(|song| song.year)
                        .map(|year| year.to_string())
                        .unwrap_or_default()
                },
                // An explicit cover wins; otherwise the first song's — so a
                // custom playlist borrows its first track's art until custom
                // covers land.
                playlist.meta.cover.or(Some(first.id)),
            )
        }
        None => (
            playlist.meta.artist.clone().unwrap_or_default(),
            "0 Tracks".to_string(),
            String::new(),
            playlist.meta.cover,
        ),
    };

    Some(AlbumSection {
        playlist: group.playlist,
        artist,
        album: playlist.meta.title.clone(),
        meta,
        year,
        cover_song,
        tracks,
    })
}

/// The artist line for a custom playlist: the one artist every track credits,
/// or "Various Artists" once they differ.
fn custom_artist(songs: &[&SongMetadata]) -> String {
    let mut credited = songs.iter().map(|song| song.artists.join(", "));
    let first = credited.next().unwrap_or_default();
    if credited.all(|artist| artist == first) {
        first
    } else {
        "Various Artists".to_string()
    }
}

/// The `format | bitrate | N Tracks | Time:…` line under an album title.
fn album_meta(song: &SongMetadata, tracks: usize, total_secs: u64) -> String {
    let format = song
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "audio".to_string());
    let labelled = match song.nominal_bitrate {
        Some(bps) => format!("{format}-{}kbps", (bps as f64 / 1000.0).round() as u32),
        None => format,
    };
    format!("{labelled} | {tracks} Tracks | Time:{}", format_secs(total_secs))
}

/// One drawable row of an album list.
#[derive(Clone, Copy)]
pub enum ListItem {
    /// An album header (a section with more than one track).
    Header(usize),
    /// A one-track section, collapsed into a single row.
    Compact(usize),
    /// Track `index` of section `section`.
    Track { section: usize, index: usize },
}

impl ListItem {
    /// The section this row belongs to.
    pub fn section(self) -> usize {
        match self {
            ListItem::Header(s) | ListItem::Compact(s) => s,
            ListItem::Track { section, .. } => section,
        }
    }
}

/// Flatten sections into drawable rows, preserving album structure.
pub fn flatten(sections: &[AlbumSection]) -> Vec<ListItem> {
    let mut items = Vec::new();
    for (section, data) in sections.iter().enumerate() {
        if data.tracks.is_empty() {
            items.push(ListItem::Header(section));
        } else if data.tracks.len() == 1 {
            items.push(ListItem::Compact(section));
        } else {
            items.push(ListItem::Header(section));
            for index in 0..data.tracks.len() {
                items.push(ListItem::Track { section, index });
            }
        }
    }
    items
}

/// How a list reacts to a row. Implemented per view so the shared renderer
/// doesn't need to know which container it's drawing.
pub trait RowActions<V: Render>: 'static {
    /// A row was clicked (single or double, per the event's `click_count`).
    fn activate(
        &self,
        view: &mut V,
        playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<V>,
    );

    /// A row was right-clicked.
    fn context(
        &self,
        view: &mut V,
        song: SongId,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<V>,
    );
}

/// Render one row — what `gpui::list` calls for each visible (and overdraw)
/// item, so a huge library only ever draws the rows near the viewport.
#[allow(clippy::too_many_arguments)]
pub fn render_item<V: Render + 'static>(
    theme: Theme,
    ix: usize,
    sections: &[AlbumSection],
    items: &[ListItem],
    covers: &Entity<CoverStore>,
    library: &Entity<LibraryState>,
    current: SongId,
    live_bitrate: Option<u32>,
    context: Option<PlaylistId>,
    actions: &Rc<dyn RowActions<V>>,
    window: &Window,
    cx: &mut Context<V>,
) -> AnyElement {
    let item = items[ix];
    let section = &sections[item.section()];
    let playlist = section.playlist;

    match item {
        ListItem::Header(_) => {
            request_cover(covers, library, section.cover_song, cx);
            let cover = covers.read(cx).cover_of(section.cover_song);
            album_header(theme, section, cover)
        }
        ListItem::Compact(_) => {
            request_cover(covers, library, section.cover_song, cx);
            let cover = covers.read(cx).cover_of(section.cover_song);
            let track = &section.tracks[0];
            let song = track.song;
            let playing = current == song;
            let bitrate = row_bitrate(playing, track.nominal_bitrate, live_bitrate);
            let on_click = {
                let actions = actions.clone();
                cx.listener(move |view, event: &ClickEvent, window, cx| {
                    actions.activate(view, playlist, 0, event, window, cx)
                })
            };
            let on_right_click = {
                let actions = actions.clone();
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    actions.context(view, song, context, event, window, cx)
                })
            };
            compact_row(theme, track, cover, playing, false, bitrate, on_click, on_right_click)
        }
        ListItem::Track { index, .. } => {
            let track = &section.tracks[index];
            let song = track.song;
            let playing = current == song;
            let bitrate = row_bitrate(playing, track.nominal_bitrate, live_bitrate);
            let title_col = title_column_width(window, theme, &section.tracks);
            let on_click = {
                let actions = actions.clone();
                cx.listener(move |view, event: &ClickEvent, window, cx| {
                    actions.activate(view, playlist, index, event, window, cx)
                })
            };
            let on_right_click = {
                let actions = actions.clone();
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    actions.context(view, song, context, event, window, cx)
                })
            };
            track_row(
                theme,
                track,
                index,
                playing,
                false,
                title_col,
                bitrate,
                on_click,
                on_right_click,
            )
        }
    }
}

/// Ask the store for a section's cover, if it isn't already known. Called while
/// rendering a header, so only the covers actually on screen get requested.
fn request_cover<V: Render + 'static>(
    covers: &Entity<CoverStore>,
    library: &Entity<LibraryState>,
    song: Option<SongId>,
    cx: &mut Context<V>,
) {
    let Some(song) = song else { return };
    if covers.read(cx).cover(song).is_some() {
        return;
    }
    let found = library.read(cx).library().get(song).map(|song| (song.path.clone(), song.has_art()));
    if let Some((path, has_art)) = found {
        covers.update(cx, |store, cx| store.request(song, path, has_art, cx));
    }
}

/// Width of the title column: the longest title in the section, in pixels.
///
/// The font is monospace, so measuring one character's advance and multiplying
/// by the character count gives the column width without shaping every title.
/// This is what keeps the `•` and the artist name starting at the same x on
/// every row.
pub fn title_column_width(window: &Window, theme: Theme, tracks: &[TrackRow]) -> f32 {
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&gpui::font(theme.font));
    let advance = text_system
        .ch_advance(font_id, px(theme.cell_px()))
        .map(f32::from)
        .unwrap_or(theme.cell_px() * 0.6);

    let chars = tracks
        .iter()
        .map(|track| track.title.chars().count())
        .max()
        .unwrap_or(MIN_TITLE_CHARS)
        .clamp(MIN_TITLE_CHARS, MAX_TITLE_CHARS);

    chars as f32 * advance
}

/// An album's header: thumbnail, artist, title, meta line, year.
pub fn album_header(theme: Theme, section: &AlbumSection, cover: Option<CoverImage>) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .px_4()
        .pt_4()
        .pb_2()
        .child(thumbnail(theme, cover, THUMB_PX))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.small_px()))
                        .text_color(theme.text_muted)
                        .child(section.artist.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.cell_px() + 2.0))
                        .text_color(theme.text)
                        .child(section.album.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.small_px()))
                        .text_color(theme.text_faint)
                        .child(section.meta.clone()),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(section.year.clone()),
        )
        .into_any_element()
}

/// A one-track section, compressed into a single row with its cover on the
/// left instead of a header plus one line.
pub fn compact_row(
    theme: Theme,
    track: &TrackRow,
    cover: Option<CoverImage>,
    playing: bool,
    active: bool,
    bitrate: Option<u32>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(("album-single", track.song))
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_2()
        .cursor_pointer()
        .bg(row_background(theme, playing, active, 0))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .on_mouse_down(MouseButton::Right, on_right_click)
        .child(thumbnail(theme, cover, THUMB_SMALL_PX))
        .child(title_and_artist(theme, track, playing, true))
        .child(bitrate_cell(theme, bitrate))
        .child(duration_cell(theme, track))
        .into_any_element()
}

/// One track row: number, `Title • Artist`, bitrate, duration. The playing row
/// is drawn in the accent colour.
#[allow(clippy::too_many_arguments)]
pub fn track_row(
    theme: Theme,
    track: &TrackRow,
    index: usize,
    playing: bool,
    active: bool,
    title_col: f32,
    bitrate: Option<u32>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let id: ElementId = ("album-track", track.song).into();

    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_1()
        .cursor_pointer()
        .bg(row_background(theme, playing, active, index))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .on_mouse_down(MouseButton::Right, on_right_click)
        .child(
            div()
                .w(px(theme.num_col()))
                .flex_none()
                .text_right()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(format!("{:02}.", track.number)),
        )
        // A fixed title column, so the `•` and the artist start at the same x
        // on every row of this album.
        .child(
            div()
                .w(px(title_col))
                .flex_none()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(if playing { theme.accent } else { theme.text })
                .child(track.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_faint)
                .child("•"),
        )
        .child(title_and_artist(theme, track, playing, false))
        // Always reserve the action column, so rows keep identical children and
        // the numeric columns stay put.
        .child(div().w(px(theme.action_col())).flex_none())
        .child(bitrate_cell(theme, bitrate))
        .child(duration_cell(theme, track))
        .into_any_element()
}

/// The artist cell. In a compressed row the title rides along with it (there's
/// no separate column), otherwise the title is already drawn separately.
fn title_and_artist(
    theme: Theme,
    track: &TrackRow,
    playing: bool,
    include_title: bool,
) -> AnyElement {
    let mut cell = div().flex_1().min_w_0().flex().items_center().gap_2();
    if include_title {
        cell = cell.child(
            div()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(if playing { theme.accent } else { theme.text })
                .child(track.title.clone()),
        );
        cell = cell.child(
            div()
                .flex_none()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_faint)
                .child("•"),
        );
    }
    cell.child(
        div()
            .truncate()
            .text_size(px(theme.cell_px()))
            .text_color(theme.text_muted)
            .child(track.artist.clone()),
    )
    .into_any_element()
}

/// The row background: playing wins, then the keyboard selection, then the
/// zebra stripe.
fn row_background(theme: Theme, playing: bool, active: bool, index: usize) -> gpui::Rgba {
    if playing {
        theme.row_playing
    } else if active {
        theme.row_active
    } else {
        theme.row_bg(index)
    }
}

/// The declared/current bitrate, right-aligned just left of the duration.
/// Empty when nothing is known.
fn bitrate_cell(theme: Theme, bitrate: Option<u32>) -> AnyElement {
    div()
        .w(px(theme.bitrate_col()))
        .flex_none()
        .text_right()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(bitrate.map(format_bitrate).unwrap_or_default())
        .into_any_element()
}

fn duration_cell(theme: Theme, track: &TrackRow) -> AnyElement {
    div()
        .w(px(theme.length_col()))
        .flex_none()
        .text_right()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(format_duration(Duration::from_secs(track.secs)))
        .into_any_element()
}

/// The bitrate a row shows: the live number while it's playing, its declared
/// bitrate otherwise.
pub fn row_bitrate(playing: bool, nominal: Option<u32>, live: Option<u32>) -> Option<u32> {
    if playing {
        live.or(nominal)
    } else {
        nominal
    }
}

fn thumbnail(theme: Theme, cover: Option<CoverImage>, size: f32) -> AnyElement {
    match cover {
        Some(CoverImage::Ready(image)) => img(image)
            .size(px(size))
            .flex_none()
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        _ => div()
            .size(px(size))
            .flex_none()
            .bg(theme.row_odd)
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.text_faint)
            .child("♪")
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CoverState, Lyrics, PlaylistId};
    use std::path::PathBuf;
    use std::time::Duration;

    fn song(id: SongId, title: &str, artist: &str, album: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: PathBuf::from(format!("/music/{title}.flac")),
            song_name: Some(title.to_string()),
            artists: vec![artist.to_string()],
            album_name: Some(album.to_string()),
            album_artist: None,
            track_position: None,
            year: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            cover: CoverState::Missing,
        }
    }

    #[test]
    fn an_empty_custom_playlist_still_gets_a_section() {
        // A freshly created playlist has no songs — it must still render a
        // header rather than "This playlist is gone".
        let mut library = Library::default();
        let id = library.create_custom("Fresh");

        let section = section_for_playlist(&library, id).expect("empty playlist renders");
        assert_eq!(section.album, "Fresh");
        assert!(section.tracks.is_empty());
        assert_eq!(section.cover_song, None);
    }

    #[test]
    fn a_missing_playlist_has_no_section() {
        let library = Library::default();
        assert!(section_for_playlist(&library, PlaylistId(999)).is_none());
    }

    #[test]
    fn a_custom_playlist_borrows_its_first_songs_cover() {
        let mut library = Library::default();
        let id = library.create_custom("Faves");
        library.insert_song(song(7, "Seven", "Grimes", "Visions"));
        library.add_song(id, 7);

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.cover_song, Some(7), "cover comes from the first song");
        assert_eq!(section.album, "Faves", "the playlist title, not the album tag");
        assert_eq!(section.tracks.len(), 1);
    }

    #[test]
    fn a_custom_playlist_with_mixed_artists_reads_various_artists() {
        let mut library = Library::default();
        let id = library.create_custom("Mix");
        library.insert_song(song(1, "One", "Grimes", "Visions"));
        library.insert_song(song(2, "Two", "Laffey", "Depths"));
        library.add_song(id, 1);
        library.add_song(id, 2);

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.artist, "Various Artists");
        assert_eq!(section.year, "", "a custom playlist carries no release year");
    }

    #[test]
    fn a_custom_playlist_of_one_artist_keeps_that_artist() {
        let mut library = Library::default();
        let id = library.create_custom("Grimes");
        library.insert_song(song(1, "One", "Grimes", "Visions"));
        library.insert_song(song(2, "Two", "Grimes", "Visions"));
        library.add_song(id, 1);
        library.add_song(id, 2);

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.artist, "Grimes");
    }

    #[test]
    fn row_bitrate_prefers_live_while_playing() {
        assert_eq!(row_bitrate(true, Some(320_000), Some(256_000)), Some(256_000));
        assert_eq!(row_bitrate(true, Some(320_000), None), Some(320_000));
        assert_eq!(row_bitrate(false, Some(320_000), Some(256_000)), Some(320_000));
    }

    fn section_with_tracks(count: usize) -> AlbumSection {
        AlbumSection {
            playlist: PlaylistId(0),
            artist: String::new(),
            album: String::new(),
            meta: String::new(),
            year: String::new(),
            cover_song: None,
            tracks: (0..count)
                .map(|i| TrackRow {
                    song: i as SongId,
                    number: i + 1,
                    title: String::new(),
                    artist: String::new(),
                    secs: 0,
                    nominal_bitrate: None,
                })
                .collect(),
        }
    }

    #[test]
    fn flatten_preserves_album_structure() {
        let sections =
            vec![section_with_tracks(2), section_with_tracks(0), section_with_tracks(1)];
        let items = flatten(&sections);

        // A 2-track album is a header plus two rows; an empty one is just a
        // header; a one-track album collapses to a single compact row.
        assert_eq!(items.len(), 5);
        assert!(matches!(items[0], ListItem::Header(0)));
        assert!(matches!(items[1], ListItem::Track { section: 0, index: 0 }));
        assert!(matches!(items[2], ListItem::Track { section: 0, index: 1 }));
        assert!(matches!(items[3], ListItem::Header(1)));
        assert!(matches!(items[4], ListItem::Compact(2)));
        assert_eq!(items[4].section(), 2);
    }
}
