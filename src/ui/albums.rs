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

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, fill, img, point, prelude::*, px, size, AnyElement, App, Bounds, ClickEvent,
    Context, ElementId, Entity, MouseButton, MouseDownEvent, ObjectFit, Pixels, RenderImage, Rgba,
    SharedString, Window,
};

use crate::model::search::SongGroup;
use crate::model::{Library, PlaylistId, ReleaseDate, SongId, SongMetadata};
use crate::ui::cover_store::{CoverImage, CoverStore};
use crate::ui::drag::{self, DragInfo};
use crate::ui::format::{format_bitrate, format_duration, format_secs};
use crate::ui::library_state::LibraryState;
use crate::ui::marquee;
use crate::ui::row_list::{Cell, Highlight};
use crate::ui::theme::Theme;
use crate::ui::widgets::blend;

/// Cover thumbnail size in an album header.
pub const THUMB_PX: f32 = 64.0;

/// Cover size in the icon grid — the album-header thumbnail's size, the same
/// "icon size" the playlist headers use.
pub const ICON_COVER_PX: f32 = THUMB_PX;

/// Padding around one icon card, in px. The icons-per-row a container fits is
/// its width divided by the card width (cover + twice this) plus the gap.
pub const ICON_PAD_PX: f32 = 8.0;

/// The full width of one icon card: cover plus padding on both sides.
pub const ICON_CARD_PX: f32 = ICON_COVER_PX + ICON_PAD_PX * 2.0;

/// Horizontal gap between icon columns, in px. The column count a container
/// fits is derived from this together with [`ICON_CARD_PX`].
pub const ICON_GAP_PX: f32 = 8.0;

/// The most characters a title column shows before its text fades and slides.
pub const MAX_TITLE_CHARS: usize = 48;

/// The most characters an artist column shows before its text fades and slides.
pub const MAX_ARTIST_CHARS: usize = 48;

/// The fixed title/artist column widths, in px.
///
/// Both are sized to a maximum character count rather than to the longest
/// string in a section, so every playlist lays its columns out identically and
/// songs line up across tabs. Text longer than its column fades at the edge and
/// slides on hover (see [`marquee::marquee_text`]).
#[derive(Clone, Copy, Debug)]
pub struct Columns {
    pub title: f32,
    pub artist: f32,
    /// The character counts the widths were sized to — the marquee's travel
    /// math and the equalizer's nudge both derive from them.
    pub title_chars: usize,
    pub artist_chars: usize,
}

impl Columns {
    /// Measure the columns for `theme`'s font. The font is monospace, so one
    /// character's advance times the max count gives the width without shaping
    /// every title.
    pub fn measure(window: &Window, theme: Theme) -> Self {
        Self::sized(window, theme, MAX_TITLE_CHARS, MAX_ARTIST_CHARS)
    }

    /// Columns sized to explicit character counts — a narrow container (the
    /// queue panel) scales its columns to its own width instead of the
    /// tables' fixed 48.
    pub fn sized(window: &Window, theme: Theme, title_chars: usize, artist_chars: usize) -> Self {
        let advance = marquee::char_advance(window, theme, theme.cell_px());
        Self {
            title: advance * title_chars as f32,
            artist: advance * artist_chars as f32,
            title_chars,
            artist_chars,
        }
    }
}

/// One track row, snapshotted out of the library. It holds the shared song
/// metadata rather than copies of its formatted labels: a rebuild is a cheap
/// `Arc` clone, and the few cells that need text format it at render time
/// (only visible rows).
pub struct TrackRow {
    pub song: SongId,
    pub number: usize,
    pub meta: Arc<SongMetadata>,
}

/// Where a section's rows come from. A real playlist, or the play queue —
/// which has no [`PlaylistId`] of its own (there is no fake `PlaylistId(0)`
/// sentinel that a future real playlist could alias).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSource {
    Playlist(PlaylistId),
    Queue,
}

/// One album section — its header data plus its tracks.
pub struct AlbumSection {
    pub source: SectionSource,
    pub artist: String,
    pub album: String,
    /// The `format | bitrate | tracks | time` line.
    pub meta: String,
    /// The section's release date (packed `YYYYMMDD`), when its songs declare
    /// one — what the library's chronological ordering sorts by.
    pub date: Option<ReleaseDate>,
    /// The song whose cover represents the album.
    pub cover_song: Option<SongId>,
    /// Rows carry their own song's cover — the playlists whose tracks can
    /// each have different art (custom and smart playlists). Albums draw one
    /// cover in the header instead.
    pub row_covers: bool,
    pub tracks: Vec<TrackRow>,
}

impl AlbumSection {
    /// The playlist this section draws, when it is one (not the queue).
    pub fn playlist(&self) -> Option<PlaylistId> {
        match self.source {
            SectionSource::Playlist(id) => Some(id),
            SectionSource::Queue => None,
        }
    }
}

/// Snapshot a set of album groups (e.g. search results) into drawable sections.
pub fn sections_from_groups(library: &Library, groups: Vec<SongGroup>) -> Vec<AlbumSection> {
    groups.into_iter().filter_map(|group| section(library, group)).collect()
}

/// Snapshot one playlist — the playlist container's entry point.
pub fn section_for_playlist(library: &Library, playlist: PlaylistId) -> Option<AlbumSection> {
    let playlist = library.playlist(playlist)?;
    section(library, SongGroup { playlist: playlist.id, songs: playlist.song_ids.clone() })
}

fn section(library: &Library, group: SongGroup) -> Option<AlbumSection> {
    let playlist = library.playlist(group.playlist)?;
    let songs: Vec<Arc<SongMetadata>> =
        group.songs.iter().filter_map(|id| library.get_arc(*id)).collect();

    // An album's row keeps the song's own track number (its position in the
    // album, falling back to the row order); a custom or smart playlist
    // numbers by position in the playlist — the song's album number would
    // jump around.
    let album_numbering = playlist.is_auto();
    let tracks = songs
        .iter()
        .enumerate()
        .map(|(index, song)| TrackRow {
            song: song.id,
            number: if album_numbering {
                song.track_position.map(usize::from).unwrap_or(index + 1)
            } else {
                index + 1
            },
            meta: song.clone(),
        })
        .collect();

    // An empty playlist (a freshly created custom one) still needs a header —
    // taken from the playlist's own metadata, since there's no first song to
    // borrow artist/date/cover from. Only albums carry a release date; a
    // custom playlist spans arbitrary tracks, so one would mislead.
    let custom = playlist.is_custom();
    let date = if custom { None } else { songs.iter().find_map(|song| song.date) };

    // The meta line describes the playlist as a whole — every song in it, not
    // just the ones a search section lists.
    let all: Vec<Arc<SongMetadata>> =
        playlist.song_ids.iter().filter_map(|id| library.get_arc(*id)).collect();
    let total_secs: u64 = all.iter().map(|song| song.duration.as_secs()).sum();
    let meta = album_meta(all.len(), total_secs);

    let (artist, cover_song) = match songs.first() {
        Some(first) => (
            // A custom playlist lists whatever the user put in it: the one
            // shared artist when every track agrees, else "Various Artists".
            // An album lists every artist credited on its tracks; when more
            // than a handful share the bill, that collapses to a phrase.
            if custom {
                custom_artist(&songs)
            } else {
                album_artists(&songs)
            },
            // An explicit cover wins; otherwise the first song's — so a
            // custom playlist borrows its first track's art until custom
            // covers land.
            playlist.cover().or(Some(first.id)),
        ),
        None => (playlist.artist().unwrap_or_default().to_string(), playlist.cover()),
    };

    Some(AlbumSection {
        source: SectionSource::Playlist(group.playlist),
        artist,
        album: playlist.title().to_string(),
        meta,
        date,
        cover_song,
        // Anything that isn't an autogen album — custom and smart playlists —
        // can hold songs with different covers, so its rows carry their own.
        row_covers: !playlist.is_auto(),
        tracks,
    })
}

/// The artist line for an album: every artist credited on its tracks, deduped
/// and in credit order — or "Several Artists" once more than a handful share
/// the bill, so the line stays a line.
fn album_artists(songs: &[Arc<SongMetadata>]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut artists: Vec<&str> = Vec::new();
    for song in songs {
        for artist in &song.artists {
            if seen.insert(artist.clone()) {
                artists.push(artist);
            }
        }
    }
    if artists.len() > 5 {
        "Several Artists".to_string()
    } else {
        artists.join(", ")
    }
}

/// The artist line for a custom playlist: the one artist every track credits,
/// or "Various Artists" once they differ.
fn custom_artist(songs: &[Arc<SongMetadata>]) -> String {
    let mut credited = songs.iter().map(|song| song.artists.join(", "));
    let first = credited.next().unwrap_or_default();
    if credited.all(|artist| artist == first) {
        first
    } else {
        "Various Artists".to_string()
    }
}

/// The `N Tracks | m:ss` line under an album title.
///
/// The file format and the album-level bitrate are deliberately left out: the
/// bitrate that matters is per *song* (and live while it plays), and it's shown
/// on each row in the same format either way.
fn album_meta(tracks: usize, total_secs: u64) -> String {
    let tracks = if tracks == 1 { "1 Track".to_string() } else { format!("{tracks} Tracks") };
    format!("{tracks} | {}", format_secs(total_secs))
}

/// One drawable row of an album list.
#[derive(Clone, Copy)]
pub enum ListItem {
    /// An album header.
    Header(usize),
    /// A one-track section, collapsed into a single row — the search panel's
    /// rendering of a single.
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

/// How rows are flattened from album sections: with album headers, as a flat
/// list of tracks (the queue), or with one-track sections collapsed into a
/// single compact row (search).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowLayout {
    /// Headers plus track rows — the browse and playlist tables.
    #[default]
    Albums,
    /// Track rows only, no headers — the queue panel.
    Flat,
    /// Like [`Albums`](Self::Albums), but a one-track section collapses into a
    /// single row carrying its cover — the search panel.
    CompactSingles,
}

/// Flatten sections into drawable rows according to `layout`.
pub fn flatten(sections: &[AlbumSection], layout: RowLayout) -> Vec<ListItem> {
    let mut items = Vec::new();
    for (section, data) in sections.iter().enumerate() {
        match layout {
            RowLayout::Flat => {
                for index in 0..data.tracks.len() {
                    items.push(ListItem::Track { section, index });
                }
            }
            _ if data.tracks.is_empty() => items.push(ListItem::Header(section)),
            RowLayout::CompactSingles if data.tracks.len() == 1 => {
                items.push(ListItem::Compact(section));
            }
            _ => {
                items.push(ListItem::Header(section));
                for index in 0..data.tracks.len() {
                    items.push(ListItem::Track { section, index });
                }
            }
        }
    }
    items
}

/// The item indices that hold a track (not an album header), in order.
pub fn selectable_indices(items: &[ListItem]) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| !matches!(item, ListItem::Header(_)))
        .map(|(ix, _)| ix)
        .collect()
}

/// The song a row holds, if it's a track row.
pub fn row_song(items: &[ListItem], sections: &[AlbumSection], ix: usize) -> Option<SongId> {
    match items.get(ix)? {
        ListItem::Track { section, index } => {
            sections.get(*section)?.tracks.get(*index).map(|track| track.song)
        }
        ListItem::Compact(section) => {
            sections.get(*section)?.tracks.first().map(|track| track.song)
        }
        ListItem::Header(_) => None,
    }
}

/// The first row holding `song`, if any — what up/down anchor to when nothing
/// is selected (the playing song's row).
pub fn row_of_song(items: &[ListItem], sections: &[AlbumSection], song: SongId) -> Option<usize> {
    (0..items.len()).find(|&ix| row_song(items, sections, ix) == Some(song))
}

/// The playlist and track index a row plays from, if it's a track row.
pub fn row_target(
    items: &[ListItem],
    sections: &[AlbumSection],
    ix: usize,
) -> Option<(SectionSource, usize)> {
    match items.get(ix)? {
        ListItem::Track { section, index } => Some((sections.get(*section)?.source, *index)),
        ListItem::Compact(section) => Some((sections.get(*section)?.source, 0)),
        ListItem::Header(_) => None,
    }
}

/// The next selectable row after `current` in `direction` (`forward` = down),
/// clamped to the ends. `None` when there are no selectable rows.
pub fn step_selectable(items: &[ListItem], current: Option<usize>, forward: bool) -> Option<usize> {
    let selectable = selectable_indices(items);
    let Some(&first) = selectable.first() else { return None };
    let Some(&last) = selectable.last() else { return None };
    let Some(current) = current else {
        return Some(if forward { first } else { last });
    };
    match selectable.iter().position(|&i| i == current) {
        Some(position) if forward => Some(selectable[(position + 1).min(selectable.len() - 1)]),
        Some(position) => Some(selectable[position.saturating_sub(1)]),
        None => Some(first),
    }
}

/// A set of selected rows in a flattened album list.
///
/// Rows are identified by their index in the flattened [`ListItem`] list, so a
/// range is just a span of item indices. Headers aren't selectable;
/// [`selectable_indices`] is what a range walks.
///
/// The selection remembers the order rows were picked in: queueing and playing
/// a multi-row selection follow that pick order, not the list's order.
#[derive(Default, Clone)]
pub struct RowSelection {
    selected: BTreeSet<usize>,
    /// The selected rows in the order they were picked. A shift-range has no
    /// per-row pick order, so it lands here in list order.
    order: Vec<usize>,
    /// Where a shift-click range extends from — the last row clicked without a
    /// modifier.
    anchor: Option<usize>,
}

impl RowSelection {
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.selected.contains(&ix)
    }

    /// The lowest selected item index, if any.
    pub fn first(&self) -> Option<usize> {
        self.selected.iter().next().copied()
    }

    /// Every selected item index, in list order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.selected.iter().copied()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.order.clear();
        self.anchor = None;
    }

    /// Select exactly `ix` and make it the anchor.
    pub fn set_single(&mut self, ix: usize) {
        self.selected.clear();
        self.order.clear();
        self.selected.insert(ix);
        self.order.push(ix);
        self.anchor = Some(ix);
    }

    /// Add or remove `ix` (ctrl-click), making it the anchor.
    pub fn toggle(&mut self, ix: usize) {
        if !self.selected.remove(&ix) {
            self.selected.insert(ix);
            self.order.push(ix);
        } else {
            self.order.retain(|&picked| picked != ix);
        }
        self.anchor = Some(ix);
    }

    /// Extend the selection from the anchor to `ix` (shift-click), selecting
    /// every selectable row in between.
    pub fn extend_to(&mut self, ix: usize, items: &[ListItem]) {
        let Some(anchor) = self.anchor else {
            return self.set_single(ix);
        };
        let selectable = selectable_indices(items);
        let (Some(from), Some(to)) = (
            selectable.iter().position(|&i| i == anchor),
            selectable.iter().position(|&i| i == ix),
        ) else {
            return self.set_single(ix);
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        let range = &selectable[lo..=hi];
        self.selected = range.iter().copied().collect();
        self.order = range.to_vec();
    }

    /// The songs the selection covers, in the order the rows were picked.
    pub fn songs(&self, items: &[ListItem], sections: &[AlbumSection]) -> Vec<SongId> {
        self.order
            .iter()
            .filter_map(|&ix| row_song(items, sections, ix))
            .collect()
    }
}

/// How a list reacts to a row. Implemented per view so the shared renderer
/// doesn't need to know which container it's drawing.
pub trait RowActions<V: Render>: 'static {
    /// A row was clicked (single or double, per the event's `click_count`).
    /// `item_ix` is the row's index in the flattened list; `index` is its track
    /// index within `playlist`.
    fn activate(
        &self,
        view: &mut V,
        item_ix: usize,
        source: SectionSource,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<V>,
    );

    /// A row (or, in icon view, an album cover) was right-clicked. `songs` is
    /// what the menu acts on — the clicked row's song, or the album's songs —
    /// and `playlist` is the playlist the row or card belongs to.
    fn context(
        &self,
        view: &mut V,
        item_ix: usize,
        songs: Vec<SongId>,
        source: SectionSource,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<V>,
    );

    /// A row's hover state changed.
    fn hover(&self, view: &mut V, item_ix: usize, hovered: bool, cx: &mut Context<V>);

    /// A row's title or artist cell hover state changed, so its marquee slides
    /// on its own.
    fn cell_hover(
        &self,
        _view: &mut V,
        _item_ix: usize,
        _cell: Cell,
        _hovered: bool,
        _cx: &mut Context<V>,
    ) {
    }

    /// A left-button press on a row — the grab that starts a drag-to-reorder.
    /// Most lists don't reorder; the default ignores it.
    fn press(
        &self,
        _view: &mut V,
        _item_ix: usize,
        _event: &MouseDownEvent,
        _window: &mut Window,
        _cx: &mut Context<V>,
    ) {
    }
}

/// The read-only backdrop every row renderer shares: where a row's looks
/// come from (theme, covers, playback status) and what state it wears
/// (selection, hover, highlight fade).
pub struct RowContext<'a> {
    pub theme: Theme,
    pub covers: &'a Entity<CoverStore>,
    pub library: &'a Entity<LibraryState>,
    /// The currently-playing song, if it is one.
    pub current: Option<SongId>,
    /// Whether audio is actually playing, so the playing row's equalizer only
    /// animates while it should.
    pub playing: bool,
    /// The shared clock's elapsed seconds, driving the equalizer's motion.
    pub eq_phase: f32,
    /// The playing song's live bitrate label, preformatted once per frame —
    /// shown on its row instead of the declared one.
    pub live_bitrate_label: Option<SharedString>,
    /// The playlist the row's menu offers as a destination (the open one).
    pub context: Option<PlaylistId>,
    pub highlight: &'a Highlight,
    /// The in-flight drag-to-reorder, if any: the grabbed row hides (the view
    /// draws it following the pointer) and the rows between it and the target
    /// shift one row to make room.
    pub drag: Option<DragInfo>,
    /// Slim rows: no bitrate or duration columns (the queue panel).
    pub slim: bool,
}

/// How one row paints: the colours that make up its background and the
/// bitrate cell's contents.
struct RowPaint {
    theme: Theme,
    /// This row is the current track.
    playing: bool,
    /// Audio is actually playing (the equalizer animates only then).
    audio_playing: bool,
    eq_phase: f32,
    selection: Option<f32>,
    hover: Option<f32>,
    /// The title cell's marquee offset, in px.
    title_offset: f32,
    /// The artist cell's marquee offset, in px — each slides on its own hover.
    artist_offset: f32,
    bitrate: SharedString,
    /// This row's vertical shift for an in-flight drag, in px.
    translate: f32,
    /// This row is the dragged one — it hides while the view draws it
    /// following the pointer.
    hidden: bool,
    /// Slim rows: no bitrate or duration columns.
    slim: bool,
}

/// The six row listeners, bundled so a row takes one argument instead of six
/// near-identical closures.
struct RowHandlers {
    on_click: Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
    on_right_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App)>,
    on_title_hover: Box<dyn Fn(&bool, &mut Window, &mut App)>,
    on_artist_hover: Box<dyn Fn(&bool, &mut Window, &mut App)>,
    on_press: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

/// Render one row — what `gpui::list` calls for each visible (and overdraw)
/// item, so a huge library only ever draws the rows near the viewport.
/// `actions` is absent for a header-only render (e.g. an empty playlist),
/// which no one can interact with anyway.
pub fn render_item<V: Render + 'static>(
    context: &RowContext,
    ix: usize,
    sections: &[AlbumSection],
    items: &[ListItem],
    columns: &Columns,
    actions: Option<&Rc<dyn RowActions<V>>>,
    cx: &mut Context<V>,
) -> AnyElement {
    let item = items[ix];
    let section = &sections[item.section()];
    let source = section.source;
    let playlist_context = context.context;

    // A header has no title/artist cells or row actions to wire; draw it and go.
    let Some((track, index)) = (match item {
        ListItem::Header(_) => None,
        ListItem::Compact(_) => Some((&section.tracks[0], 0)),
        ListItem::Track { index, .. } => Some((&section.tracks[index], index)),
    }) else {
        let cover = header_cover(context, section, cx);
        return album_header(context.theme, section, cover);
    };

    let song = track.song;
    let playing = context.current == Some(song);
    // An in-flight drag: the grabbed row hides (the view draws it following
    // the pointer), the rows between it and the target shift to make room.
    let (translate, hidden) = match context.drag.and_then(|drag| drag::row_shift(ix, &drag)) {
        Some(shift) => (shift, false),
        None => (0.0, context.drag.is_some()),
    };
    let paint = RowPaint {
        theme: context.theme,
        playing,
        audio_playing: context.playing,
        eq_phase: context.eq_phase,
        selection: context.highlight.selection(ix),
        hover: context.highlight.hover(ix),
        title_offset: context.highlight.marquee_offset(ix, Cell::Title),
        artist_offset: context.highlight.marquee_offset(ix, Cell::Artist),
        bitrate: row_bitrate(
            playing,
            &SharedString::from(
                track.meta.nominal_bitrate.map(format_bitrate).unwrap_or_default(),
            ),
            context.live_bitrate_label.as_ref(),
        ),
        translate,
        hidden,
        slim: context.slim,
    };

    // The five listeners a track row wires up — the same for a compact row
    // (`index` is its section's only track) and a multi-track one.
    let actions = actions.expect("a row is interactive");
    let (a1, actions2) = (actions.clone(), actions.clone());
    let (actions3, actions4) = (actions.clone(), actions.clone());
    let (actions5, actions6) = (actions.clone(), actions.clone());
    let handlers = RowHandlers {
        on_click: Box::new(cx.listener(move |view, event: &ClickEvent, window, cx| {
            a1.activate(view, ix, source, index, event, window, cx)
        })),
        on_right_click: Box::new(cx.listener(move |view, event: &MouseDownEvent, window, cx| {
            actions2.context(view, ix, vec![song], source, playlist_context, event, window, cx)
        })),
        on_hover: Box::new(cx.listener(move |view, hovered: &bool, _window, cx| {
            actions3.hover(view, ix, *hovered, cx)
        })),
        on_title_hover: Box::new(cx.listener(move |view, hovered: &bool, _window, cx| {
            actions4.cell_hover(view, ix, Cell::Title, *hovered, cx)
        })),
        on_artist_hover: Box::new(cx.listener(move |view, hovered: &bool, _window, cx| {
            actions5.cell_hover(view, ix, Cell::Artist, *hovered, cx)
        })),
        on_press: Box::new(cx.listener(move |view, event: &MouseDownEvent, window, cx| {
            actions6.press(view, ix, event, window, cx)
        })),
    };

    let year_label = section.date.map(|date| date.year().to_string());
    if matches!(item, ListItem::Compact(_)) {
        // A compact row carries its section's cover on the left.
        request_cover(context.covers, context.library, section.cover_song, cx);
        let cover = context.covers.read(cx).cover_of(section.cover_song);
        track_row(
            &paint,
            ix,
            track,
            index,
            columns,
            cover,
            Some(year_label.unwrap_or_default()),
            handlers,
        )
    } else {
        // A custom or smart playlist's row carries its own song's cover; a
        // track of an album doesn't (the header holds the cover).
        let row_cover = (section.row_covers && !context.slim).then(|| {
            request_cover(context.covers, context.library, Some(song), cx);
            context.covers.read(cx).cover_of(Some(song))
        }).flatten();
        track_row(&paint, ix, track, index, columns, row_cover, None, handlers)
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
    let found = library.read(cx).library().get(song).map(|song| (song.path.clone(), song.has_art));
    if let Some((path, has_art)) = found {
        covers.update(cx, |store, cx| store.request(song, path, has_art, cx));
    }
}

/// The cover an album header draws: a single cover for an album, or — for a
/// custom or smart playlist — a 2×2 mosaic of its first four songs' covers,
/// with the default placeholder in any slot left over.
fn header_cover<V: Render + 'static>(
    context: &RowContext,
    section: &AlbumSection,
    cx: &mut Context<V>,
) -> AnyElement {
    if !section.row_covers {
        request_cover(context.covers, context.library, section.cover_song, cx);
        let cover = context.covers.read(cx).cover_of(section.cover_song);
        return thumbnail(context.theme, cover, THUMB_PX);
    }
    let mut cells: Vec<Option<SongId>> =
        section.tracks.iter().take(4).map(|track| Some(track.song)).collect();
    cells.resize(4, None);
    for song in cells.iter().flatten() {
        request_cover(context.covers, context.library, Some(*song), cx);
    }
    let covers: Vec<Option<CoverImage>> = cells
        .into_iter()
        .map(|song| context.covers.read(cx).cover_of(song))
        .collect();
    cover_mosaic(context.theme, &covers, THUMB_PX)
}

/// Slice two covers along the square's anti-diagonal: `a` fills the upper-left
/// triangle, `b` the lower-right one. The result is a new GPU image, so it can
/// be handed straight to `img()`; callers should cache it by the pair's ids
/// rather than recomposing every frame.
pub(crate) fn compose_diagonal(
    a: &Arc<RenderImage>,
    b: &Arc<RenderImage>,
) -> Option<Arc<RenderImage>> {
    let a_size = a.size(0);
    let b_size = b.size(0);
    let w = usize::try_from(a_size.width.0).ok()?;
    let h = usize::try_from(a_size.height.0).ok()?;
    let bw = usize::try_from(b_size.width.0).ok()?;
    let bh = usize::try_from(b_size.height.0).ok()?;
    if w == 0 || h == 0 || bw == 0 || bh == 0 {
        return None;
    }
    let a_bytes = a.as_bytes(0)?;
    let b_bytes = b.as_bytes(0)?;
    if a_bytes.len() < w * h * 4 || b_bytes.len() < bw * bh * 4 {
        return None;
    }

    // RenderImage pixels are BGRA; copying them straight through keeps the
    // channels as both sources and the GPU already expect them.
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let source = if x * h + y * w < w * h {
                // Above the anti-diagonal: `a`.
                let i = (y * w + x) * 4;
                &a_bytes[i..i + 4]
            } else {
                // Below it: `b`, sampled into the output grid.
                let sx = x * bw / w;
                let sy = y * bh / h;
                let i = (sy * bw + sx) * 4;
                &b_bytes[i..i + 4]
            };
            let o = (y * w + x) * 4;
            out[o..o + 4].copy_from_slice(source);
        }
    }

    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, out)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// A 2×2 mosaic of covers at `size`, empty slots showing the default
/// placeholder — the same one a missing single cover shows.
fn cover_mosaic(theme: Theme, covers: &[Option<CoverImage>], size: f32) -> AnyElement {
    let cell = size / 2.0;
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .flex_wrap()
        .children(
            covers
                .iter()
                .take(4)
                .chain(std::iter::repeat(&None))
                .take(4)
                .map(|cover| thumbnail(theme, cover.clone(), cell)),
        )
        .into_any_element()
}

/// An album's header: thumbnail, artist, title with the year right-aligned on
/// the same line (same size and colour as the title), meta line.
pub fn album_header(theme: Theme, section: &AlbumSection, cover: AnyElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .px_4()
        .pt_4()
        .pb_2()
        .child(cover)
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
                        .flex()
                        .items_baseline()
                        .min_w_0()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(theme.cell_px() + 2.0))
                                .text_color(theme.text)
                                .child(section.album.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(theme.cell_px() + 2.0))
                                .text_color(theme.text)
                                .child(
                                section.date.map(|date| date.year().to_string()).unwrap_or_default(),
                            ),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme.small_px()))
                        .text_color(theme.text_faint)
                        .child(section.meta.clone()),
                ),
        )
        .into_any_element()
}

/// One track row. A compact row (a one-track section) is compressed with its
/// section's cover on the left instead of a number column plus a header; a
/// regular row shows the number (or the playing row's equalizer), an optional
/// per-song cover, then title, artist, and the numeric columns. The playing row
/// is drawn in the accent colour.
fn track_row(
    paint: &RowPaint,
    item_ix: usize,
    track: &TrackRow,
    index: usize,
    columns: &Columns,
    cover: Option<CoverImage>,
    compact_year: Option<String>,
    handlers: RowHandlers,
) -> AnyElement {
    let compact = compact_year.is_some();
    let id: ElementId = if compact {
        ("album-single", item_ix).into()
    } else {
        ("album-track", item_ix).into()
    };
    let theme = paint.theme;
    let playing = paint.playing;
    let background = row_background(theme, playing, paint.selection, paint.hover, if compact { 0 } else { index });
    // The song-entry convention: regular rows are square; the selected and
    // currently-playing ones carry the slight rounding.
    let rounded = playing || paint.selection.is_some();

    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py(if compact { px(6.0) } else { px(4.0) })
        .cursor_pointer()
        .bg(background)
        .when(rounded, |d| d.rounded_md())
        .when(paint.hidden, |d| d.opacity(0.0))
        .when(paint.translate != 0.0, |d| d.relative().top(px(paint.translate)))
        .on_hover(handlers.on_hover)
        .on_click(handlers.on_click)
        .on_mouse_down(MouseButton::Right, handlers.on_right_click)
        .on_mouse_down(MouseButton::Left, handlers.on_press)
        .when(compact, |d| d.child(thumbnail(theme, cover.clone(), theme.cover_col())))
        .when(!compact, |d| {
            d.child(if playing {
                eq_icon(theme, paint.eq_phase, paint.audio_playing)
            } else {
                number_cell(theme, track.number)
            })
            .children(cover.map(|cover| thumbnail(theme, Some(cover), theme.cover_col())))
        })
        .child(text_cell(
            theme,
            ("row-title", track.song.get()),
            &track.meta.display_title(),
            columns.title,
            columns.title_chars,
            paint.title_offset,
            if playing { theme.accent } else { theme.text },
            background,
            handlers.on_title_hover,
        ))
        .child(text_cell(
            theme,
            ("row-artist", track.song.get()),
            &track.meta.artists.join(", "),
            columns.artist,
            columns.artist_chars,
            paint.artist_offset,
            theme.text_muted,
            background,
            handlers.on_artist_hover,
        ))
        // Fill the gap so the numeric columns stay right-aligned.
        .child(div().flex_1().min_w_0())
        .when_some(compact_year, |d, year| {
            d.child(right_cell(theme, theme.year_col(), year))
        })
        // Always reserve the action column on regular rows, so rows keep
        // identical children and the numeric columns stay put. Slim rows (the
        // queue) end at the gap — no numeric columns follow.
        .when(!compact && !paint.slim, |d| d.child(div().w(px(theme.action_col())).flex_none()))
        .when(!paint.slim, |d| {
            d.child(right_cell(theme, theme.bitrate_col(), paint.bitrate.clone()))
        })
        .when(!paint.slim, |d| {
            d.child(right_cell(
                theme,
                theme.length_col(),
                format_duration(track.meta.duration),
            ))
        })
        .into_any_element()
}

/// A marquee text cell of fixed width.
#[allow(clippy::too_many_arguments)]
fn text_cell(
    theme: Theme,
    id: impl Into<ElementId>,
    text: &str,
    width: f32,
    chars: usize,
    offset: f32,
    color: Rgba,
    background: Rgba,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    marquee::marquee_text(
        theme,
        id,
        text,
        width,
        chars,
        theme.cell_px(),
        offset,
        color,
        background,
        on_hover,
    )
}

/// The track-number cell. Left-aligned in a column just wide enough for the
/// number, so it starts on the album cover's left edge while the title keeps
/// the same gap after it.
fn number_cell(theme: Theme, number: usize) -> AnyElement {
    div()
        .w(px(theme.number_col()))
        .flex_none()
        .text_left()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(format!("{number:02}."))
        .into_any_element()
}

/// How many bars the equalizer draws.
const EQ_BARS: usize = 3;

/// Width of one equalizer bar, and the gap between them, in px.
const EQ_BAR_PX: f32 = 2.0;
const EQ_GAP_PX: f32 = 2.0;

/// The playing row's equalizer: a few small bars in the accent colour, dancing
/// while the track plays and resting when it's paused. It takes the number
/// column's place, so the row still lines up. Shared with the queue panel's
/// rows, which use the same row anatomy.
pub fn eq_icon(theme: Theme, phase: f32, playing: bool) -> AnyElement {
    let height = theme.font_size * 0.95;
    let levels = eq_levels(phase, playing);
    let width = EQ_BARS as f32 * EQ_BAR_PX + (EQ_BARS - 1) as f32 * EQ_GAP_PX;
    div()
        .w(px(theme.number_col()))
        .flex_none()
        .h(px(height))
        .flex()
        // Start on the same vertical line as the numbers it replaces.
        .justify_start()
        .child(
            canvas(
                |_bounds, _window, _cx| (),
                move |bounds, _prepaint, window, _cx| {
                    paint_eq(window, bounds, &levels, theme.accent);
                },
            )
            .w(px(width))
            .h_full(),
        )
        .into_any_element()
}

/// The bar heights (0..1) at `phase` seconds. Each bar runs at its own speed
/// and offset so the three never move in lockstep; a paused track rests at a
/// short, even profile.
fn eq_levels(phase: f32, playing: bool) -> [f32; EQ_BARS] {
    if !playing {
        return [0.30, 0.55, 0.40];
    }
    let mut levels = [0.0; EQ_BARS];
    for (i, level) in levels.iter_mut().enumerate() {
        let speed = 5.0 + i as f32 * 1.7;
        let offset = i as f32 * 1.9;
        let wave = (phase * speed + offset).sin().abs();
        *level = 0.22 + 0.78 * wave;
    }
    levels
}

/// Paint the equalizer's bars, bottom-aligned in `bounds`.
fn paint_eq(window: &mut Window, bounds: Bounds<Pixels>, levels: &[f32; EQ_BARS], color: Rgba) {
    let height = f32::from(bounds.size.height);
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    for (i, level) in levels.iter().enumerate() {
        let bar_height = (level * height).max(2.0);
        let x = origin_x + i as f32 * (EQ_BAR_PX + EQ_GAP_PX);
        let y = origin_y + (height - bar_height);
        let bar = Bounds::new(point(px(x), px(y)), size(px(EQ_BAR_PX), px(bar_height)));
        window.paint_quad(fill(bar, color));
    }
}

/// The row background: playing wins, then the selection crossfade, then the
/// hover crossfade, then the zebra stripe.
fn row_background(
    theme: Theme,
    playing: bool,
    selection: Option<f32>,
    hover: Option<f32>,
    index: usize,
) -> gpui::Rgba {
    if playing {
        return theme.row_playing;
    }
    let mut background = theme.row_bg(index);
    if let Some(fade) = selection {
        background = blend(background, theme.row_active, fade);
    }
    if let Some(fade) = hover {
        background = blend(background, theme.row_hover, fade);
    }
    background
}

/// A right-aligned numeric label cell of fixed width, the shared body of the
/// bitrate, duration and year columns. Empty when nothing is known.
fn right_cell(theme: Theme, width: f32, label: impl Into<SharedString>) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .text_right()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.into())
        .into_any_element()
}

/// The bitrate label a row shows: the live number while it's playing, its
/// declared label otherwise. A `SharedString` clone is a refcount bump, so a
/// non-playing row costs no allocation.
fn row_bitrate(playing: bool, nominal: &SharedString, live: Option<&SharedString>) -> SharedString {
    if playing {
        live.cloned().unwrap_or_else(|| nominal.clone())
    } else {
        nominal.clone()
    }
}

/// A square cover thumbnail. Rounding is opt-in per surface — see
/// `thumbnail_rounded` for the one that carries the radius on the image
/// element itself (gpui's `overflow_hidden` clips to a rectangular mask that
/// knows nothing of corner radii, so a rounded wrapper shows no rounding).
pub(crate) fn thumbnail(theme: Theme, cover: Option<CoverImage>, size: f32) -> AnyElement {
    thumbnail_sized(theme, cover, size, false)
}

/// The rounded variant: the radius has to sit on the image element itself —
/// gpui's `overflow_hidden` clips to a rectangular mask that knows nothing of
/// corner radii, so a rounded wrapper around a square image shows no rounding.
fn thumbnail_rounded(theme: Theme, cover: Option<CoverImage>, size: f32) -> AnyElement {
    thumbnail_sized(theme, cover, size, true)
}

fn thumbnail_sized(
    theme: Theme,
    cover: Option<CoverImage>,
    size: f32,
    rounded: bool,
) -> AnyElement {
    match cover {
        Some(CoverImage::Ready(image)) => img(image)
            .size(px(size))
            .flex_none()
            .object_fit(ObjectFit::Cover)
            .when(rounded, |d| d.rounded_md())
            .into_any_element(),
        _ => div()
            .size(px(size))
            .flex_none()
            .when(rounded, |d| d.rounded_md())
            .bg(theme.row_odd)
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.text_faint)
            .child("♪")
            .into_any_element(),
    }
}

/// The uniform height of an icon card: cover plus padding. Icon view draws no
/// text — the playing song's information lives in the bottom bar — so every
/// card is the same size and the column packing is exact.
pub fn icon_card_height() -> f32 {
    ICON_PAD_PX * 2.0 + ICON_COVER_PX
}

/// One packed row of the icon grid: the card indices it holds, left to right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IconRow {
    pub cards: Vec<usize>,
}

/// The packed icon grid: rows of cards plus the height they reach.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IconLayout {
    pub rows: Vec<IconRow>,
    pub height: f32,
}

/// Pack `count` uniform cards into rows that fill left to right and wrap to
/// the next row when one is full. The cards-per-row count comes from
/// `viewport_w`; a partial last row keeps whatever is left over.
pub fn pack_rows(count: usize, viewport_w: f32) -> IconLayout {
    let card_h = icon_card_height();
    let per_row =
        (((viewport_w + ICON_GAP_PX) / (ICON_CARD_PX + ICON_GAP_PX)).floor() as usize).max(1);
    let indices: Vec<usize> = (0..count).collect();
    let rows = indices
        .chunks(per_row)
        .map(|chunk| IconRow { cards: chunk.to_vec() })
        .collect();
    IconLayout { rows, height: (count.div_ceil(per_row)) as f32 * card_h }
}

/// How far past the viewport's edges the grid renders cards, in px — the
/// overdraw that makes fast scrolling not pop cards in from nothing.
const ICON_OVERDRAW_PX: f32 = 400.0;

/// The icon grid: every album as a cover — no text under it; the playing
/// song's information lives in the bottom bar. Cards are uniform, packed into
/// rows that fill left to right and wrap to the next row when one is full
/// (see [`pack_rows`]).
///
/// The grid is windowed to the viewport: only the rows whose (pre-computed)
/// position intersects the scroll range render, the first one leading with a
/// spacer of the height scrolled past. That's what keeps a multi-thousand
/// album library at full speed.
pub fn render_icon_grid<V: Render + 'static>(
    context: &RowContext,
    sections: &[AlbumSection],
    layout: &IconLayout,
    scroll_y: f32,
    viewport_h: f32,
    actions: &Rc<dyn RowActions<V>>,
    cx: &mut Context<V>,
) -> AnyElement {
    let card_h = icon_card_height();
    let top = scroll_y - ICON_OVERDRAW_PX;
    let bottom = scroll_y + viewport_h + ICON_OVERDRAW_PX;

    let mut grid = div()
        .flex_none()
        .h(px(layout.height))
        .flex()
        .flex_col()
        .content_start();

    let mut rendered = false;
    for (ix, row) in layout.rows.iter().enumerate() {
        // The visible rows with their y offsets; everything above the window
        // becomes one spacer, and the rows themselves render whole — the
        // scroll container clips whatever pokes past its top.
        let y = ix as f32 * card_h;
        if y >= bottom || y + card_h <= top {
            continue;
        }
        if !rendered && y > 0.0 {
            grid = grid.child(div().w_full().flex_none().h(px(y)));
        }
        rendered = true;
        grid = grid.child(
            div()
                .flex_none()
                .h(px(card_h))
                .flex()
                .flex_row()
                .gap_x(px(ICON_GAP_PX))
                .children(row.cards.iter().map(|&card| {
                    let section = &sections[card];
                    request_cover(context.covers, context.library, section.cover_song, cx);
                    let cover = context.covers.read(cx).cover_of(section.cover_song);
                    icon_card(context, section, cover, actions, cx)
                })),
        );
    }

    // The rows pack to a fixed total width; when the viewport is wider
    // than that, the block centers so the covers sit with equal spacing from
    // the window's left and right edges.
    div()
        .w_full()
        .flex()
        .justify_center()
        .child(grid)
        .into_any_element()
}

/// One icon card: just the cover — uniform with every other card. Clicking it
/// plays the album (what clicking a header row would do); right-clicking
/// opens the song menu for the album's songs.
fn icon_card<V: Render + 'static>(
    context: &RowContext,
    section: &AlbumSection,
    cover: Option<CoverImage>,
    actions: &Rc<dyn RowActions<V>>,
    cx: &mut Context<V>,
) -> AnyElement {
    let theme = context.theme;
    let source = section.source;
    let songs: Vec<SongId> = section.tracks.iter().map(|track| track.song).collect();

    // The cover plays the album from its first track.
    let on_click = section.tracks.first().map(|_track| {
        let actions = actions.clone();
        cx.listener(move |view, event: &ClickEvent, window, cx| {
            actions.activate(view, 0, source, 0, event, window, cx)
        })
    });
    // Right-click opens the song menu for the album's songs.
    let on_right_click = {
        let actions = actions.clone();
        let songs = songs.clone();
        let source = section.source;
        let playlist_context = context.context;
        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
            actions.context(view, 0, songs.clone(), source, playlist_context, event, window, cx)
        })
    };

    div()
        .id(ElementId::NamedInteger(
            "icon-cover".into(),
            section.tracks.first().map_or(0, |track| track.song.0),
        ))
        .w(px(ICON_CARD_PX))
        .h(px(icon_card_height()))
        .flex_none()
        .flex()
        .items_start()
        .justify_start()
        .p(px(ICON_PAD_PX))
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(theme.row_hover))
        .when_some(on_click, |d, on_click| d.on_click(on_click))
        .when(!songs.is_empty(), |d| {
            d.on_mouse_down(MouseButton::Right, on_right_click)
        })
        // Icon view: singles (one-track sections) get rounded covers;
        // album and playlist covers stay square.
        .child(if section.tracks.len() == 1 {
            thumbnail_rounded(theme, cover, ICON_COVER_PX)
        } else {
            thumbnail(theme, cover, ICON_COVER_PX)
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PlaylistId;
    use std::path::PathBuf;

    fn song(id: SongId, title: &str, artist: &str, album: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: PathBuf::from(format!("/music/{title}.flac")),
            song_name: Some(title.to_string()),
            artists: vec![artist.to_string()],
            album_name: Some(album.to_string()),
            ..Default::default()
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
        library.insert_song(song(SongId(7), "Seven", "Grimes", "Visions"));
        library.add_song(id, SongId(7));

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.cover_song, Some(SongId(7)), "cover comes from the first song");
        assert_eq!(section.album, "Faves", "the playlist title, not the album tag");
        assert_eq!(section.tracks.len(), 1);
    }

    #[test]
    fn a_custom_playlist_with_mixed_artists_reads_various_artists() {
        let mut library = Library::default();
        let id = library.create_custom("Mix");
        library.insert_song(song(SongId(1), "One", "Grimes", "Visions"));
        library.insert_song(song(SongId(2), "Two", "Laffey", "Depths"));
        library.add_song(id, SongId(1));
        library.add_song(id, SongId(2));

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.artist, "Various Artists");
        assert_eq!(section.date, None, "a custom playlist carries no release date");
    }

    #[test]
    fn a_custom_playlist_of_one_artist_keeps_that_artist() {
        let mut library = Library::default();
        let id = library.create_custom("Grimes");
        library.insert_song(song(SongId(1), "One", "Grimes", "Visions"));
        library.insert_song(song(SongId(2), "Two", "Grimes", "Visions"));
        library.add_song(id, SongId(1));
        library.add_song(id, SongId(2));

        let section = section_for_playlist(&library, id).unwrap();
        assert_eq!(section.artist, "Grimes");
    }

    #[test]
    fn row_bitrate_prefers_live_while_playing() {
        let nominal = SharedString::from("320 kb/s");
        let live = SharedString::from("256 kb/s");
        assert_eq!(row_bitrate(true, &nominal, Some(&live)), live);
        assert_eq!(row_bitrate(true, &nominal, None), nominal);
        assert_eq!(row_bitrate(false, &nominal, Some(&live)), nominal);
    }

    fn section_with_tracks(count: usize) -> AlbumSection {
        AlbumSection {
            source: SectionSource::Playlist(PlaylistId(0)),
            artist: String::new(),
            album: String::new(),
            row_covers: false,
            meta: String::new(),
            date: None,
            cover_song: None,
            tracks: (0..count)
                .map(|i| TrackRow {
                    song: SongId(i as u64),
                    number: i + 1,
                    meta: Arc::new(SongMetadata {
                        id: SongId(i as u64),
                        ..Default::default()
                    }),
                })
                .collect(),
        }
    }

    #[test]
    fn flatten_preserves_album_structure() {
        let sections =
            vec![section_with_tracks(2), section_with_tracks(0), section_with_tracks(1)];
        let items = flatten(&sections, RowLayout::Albums);

        // A 2-track album is a header plus two rows; an empty one is just a
        // header; a one-track album — a single — draws like any other album:
        // a header plus its one row.
        assert_eq!(items.len(), 6);
        assert!(matches!(items[0], ListItem::Header(0)));
        assert!(matches!(items[1], ListItem::Track { section: 0, index: 0 }));
        assert!(matches!(items[2], ListItem::Track { section: 0, index: 1 }));
        assert!(matches!(items[3], ListItem::Header(1)));
        assert!(matches!(items[4], ListItem::Header(2)));
        assert!(matches!(items[5], ListItem::Track { section: 2, index: 0 }));
    }

    #[test]
    fn flatten_search_collapses_singles() {
        let sections =
            vec![section_with_tracks(2), section_with_tracks(0), section_with_tracks(1)];
        let items = flatten(&sections, RowLayout::CompactSingles);

        // Search keeps the compact single: the one-track section collapses
        // into a single row carrying its cover.
        assert_eq!(items.len(), 5);
        assert!(matches!(items[0], ListItem::Header(0)));
        assert!(matches!(items[1], ListItem::Track { section: 0, index: 0 }));
        assert!(matches!(items[2], ListItem::Track { section: 0, index: 1 }));
        assert!(matches!(items[3], ListItem::Header(1)));
        assert!(matches!(items[4], ListItem::Compact(2)));
        assert_eq!(items[4].section(), 2);
    }

    #[test]
    fn step_selectable_walks_tracks_and_clamps() {
        let sections = vec![section_with_tracks(2)];
        let items = flatten(&sections, RowLayout::Albums); // Header, Track 0, Track 1
        assert_eq!(step_selectable(&items, None, true), Some(1));
        assert_eq!(step_selectable(&items, Some(1), true), Some(2));
        assert_eq!(step_selectable(&items, Some(2), true), Some(2), "clamped at the last");
        assert_eq!(step_selectable(&items, Some(2), false), Some(1));
        assert_eq!(step_selectable(&items, Some(1), false), Some(1), "clamped at the first");
        assert_eq!(step_selectable(&[], None, true), None, "no rows");
    }

    #[test]
    fn row_of_song_finds_the_playing_row() {
        let sections = vec![section_with_tracks(2), section_with_tracks(1)];
        let items = flatten(&sections, RowLayout::CompactSingles); // Header, T0, T1, Compact
        assert_eq!(row_of_song(&items, &sections, SongId(1)), Some(2));
        assert_eq!(row_of_song(&items, &sections, SongId(0)), Some(1), "first match wins");
        assert_eq!(row_of_song(&items, &sections, SongId(9)), None);
    }

    #[test]
    fn selection_ranges_skip_headers() {
        let sections = vec![section_with_tracks(2), section_with_tracks(2)];
        let items = flatten(&sections, RowLayout::Albums); // Header, T0, T1, Header, T0, T1
        let mut selection = RowSelection::default();
        selection.set_single(1);
        selection.extend_to(4, &items);
        assert_eq!(selection.len(), 3, "rows 1, 2, 4 — the header at 3 is skipped");
        assert!(selection.contains(1) && selection.contains(2) && selection.contains(4));
        assert!(!selection.contains(3));
        assert_eq!(selection.songs(&items, &sections).len(), 3);
    }

    #[test]
    fn selection_toggle_adds_then_removes() {
        let mut selection = RowSelection::default();
        selection.toggle(3);
        assert!(selection.contains(3));
        selection.toggle(3);
        assert!(!selection.contains(3));
        assert!(selection.is_empty());
    }

    #[test]
    fn songs_follow_pick_order_not_list_order() {
        let sections = vec![section_with_tracks(4)];
        let items = flatten(&sections, RowLayout::Albums); // Header, T0, T1, T2, T3
        let mut selection = RowSelection::default();
        selection.toggle(3); // song 2
        selection.toggle(1); // song 0
        selection.toggle(4); // song 3
        assert_eq!(selection.songs(&items, &sections), vec![SongId(2), SongId(0), SongId(3)]);

        // Deselecting a middle pick keeps the rest in pick order.
        selection.toggle(1);
        assert_eq!(selection.songs(&items, &sections), vec![SongId(2), SongId(3)]);
    }

    #[test]
    fn compose_diagonal_slices_a_over_b() {
        let solid = |pixel: [u8; 4]| {
            let buffer = image::RgbaImage::from_pixel(4, 4, image::Rgba(pixel));
            Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
        };
        let a = solid([255, 0, 0, 255]);
        let b = solid([0, 0, 255, 255]);
        let out = compose_diagonal(&a, &b).expect("same-sized images compose");
        let bytes = out.as_bytes(0).unwrap();
        let at = |x: usize, y: usize| &bytes[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(at(0, 0), [255, 0, 0, 255], "upper-left comes from a");
        assert_eq!(at(3, 3), [0, 0, 255, 255], "lower-right comes from b");
    }

    #[test]
    fn pack_rows_fills_rows_left_to_right() {
        // Three uniform cards, a viewport two cards wide: row one holds the
        // first two (left to right), row two the last — never a shared row.
        let layout = pack_rows(3, 2.0 * (ICON_CARD_PX + ICON_GAP_PX));
        let h = icon_card_height();
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.rows[0].cards, vec![0, 1], "row one fills left to right");
        assert_eq!(layout.rows[1].cards, vec![2], "overflow starts the next row");
        assert_eq!(layout.height, 2.0 * h);
    }

    #[test]
    fn pack_rows_keeps_a_partial_last_row() {
        // Seven cards over two per row: 2 + 2 + 2 + 1, the last row a stub.
        let layout = pack_rows(7, 2.0 * (ICON_CARD_PX + ICON_GAP_PX));
        let lens: Vec<usize> = layout.rows.iter().map(|r| r.cards.len()).collect();
        assert_eq!(lens, vec![2, 2, 2, 1]);
        assert_eq!(layout.height, 4.0 * icon_card_height());
    }

    #[test]
    fn pack_rows_keeps_a_single_card() {
        let layout = pack_rows(1, 5000.0);
        assert_eq!(layout.rows.len(), 1);
        assert_eq!(layout.rows[0].cards, vec![0]);
        assert_eq!(layout.height, icon_card_height());
    }

    #[test]
    fn pack_rows_with_no_cards_is_empty() {
        let layout = pack_rows(0, 5000.0);
        assert!(layout.rows.is_empty());
        assert_eq!(layout.height, 0.0);
    }

    #[test]
    fn icon_cards_are_uniform() {
        // No text under the covers, so every card is exactly cover + padding.
        assert_eq!(icon_card_height(), ICON_PAD_PX * 2.0 + ICON_COVER_PX);
    }

    #[test]
    fn a_shift_range_queues_in_list_order() {
        let sections = vec![section_with_tracks(4)];
        let items = flatten(&sections, RowLayout::Albums); // Header, T0, T1, T2, T3
        let mut selection = RowSelection::default();
        selection.set_single(4);
        selection.extend_to(1, &items);
        assert_eq!(selection.songs(&items, &sections), vec![SongId(0), SongId(1), SongId(2), SongId(3)]);
    }
}
