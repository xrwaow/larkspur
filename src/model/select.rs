//! Selection — one way to ask the library for songs: a scope, a query, and
//! an order.
//!
//! The browse view, search, an artist's discography, and a played folder are
//! all the same question with different parameters, so they share this one
//! entry point rather than each walking the library their own way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::identity::SongId;
use super::library::Library;
use super::playlist::PlaylistId;
use super::search::{Query, SongGroup};
use super::song::SongMetadata;
use super::view::FolderMode;

/// What a [`Selection`] draws its songs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole library, grouped into its autogen album playlists.
    Library,
    /// One artist's discography — their album playlists, via the library's
    /// artist index.
    Artist(String),
    /// The albums (and singles) under one directory — the folder view a played
    /// folder opens. Resolved live against the library, so a rescan updates the
    /// view instead of stranding it.
    Folder { dir: PathBuf, mode: FolderMode },
}

/// How a [`Selection`]'s album groups are sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// Alphabetically by album title — the browse view's order.
    #[default]
    AlbumTitle,
    /// Newest release first, undated albums last — the discography order.
    NewestFirst,
    /// The order the scope itself yields: a folder keeps its directory walk
    /// (directories depth-first, albums by title within each), so re-sorting
    /// would undo it.
    TrackOrder,
}

/// One way to ask the library for songs: a scope, a query, and an order.
///
/// Both parts are borrowed: a selection is always consumed by a single
/// [`Library::select`] call, never stored, so cloning the query on every
/// keystroke would be pure waste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection<'a> {
    pub scope: &'a Scope,
    /// Filters the songs; the default (empty) query matches everything.
    pub query: &'a Query,
    pub order: Order,
}

impl Library {
    /// The songs `sel` picks out, grouped by album playlist. Groups with no
    /// matching songs are left out. The songs come back as shared handles so
    /// callers don't have to resolve them again.
    pub fn select(&self, sel: &Selection<'_>) -> Vec<SongGroup> {
        let mut ids: Vec<PlaylistId> = match sel.scope {
            Scope::Library => self
                .playlists()
                .into_iter()
                .filter(|p| p.is_auto())
                .map(|p| p.id)
                .collect(),
            Scope::Artist(artist) => self.discography(artist),
            Scope::Folder { dir, mode } => self.albums_under(dir, *mode),
        };
        // A folder scope arrives already ordered — directories depth-first,
        // albums by title within each — and re-sorting would undo that walk,
        // so its order is fixed whatever `sel.order` asks.
        if !matches!(sel.scope, Scope::Folder { .. }) {
            match sel.order {
                Order::AlbumTitle => ids.sort_by(|a, b| self.title_of(*a).cmp(&self.title_of(*b))),
                Order::NewestFirst => self.sort_by_year(&mut ids),
                Order::TrackOrder => {}
            }
        }
        let matcher = sel.query.matcher();
        ids.into_iter()
            .filter_map(|id| {
                let playlist = self.playlist(id)?;
                let songs: Vec<Arc<SongMetadata>> = playlist
                    .song_ids
                    .iter()
                    .filter_map(|song| self.get_arc(*song))
                    .filter(|song| matcher.matches(song))
                    .collect();
                (!songs.is_empty()).then_some(SongGroup { playlist: id, songs })
            })
            .collect()
    }

    /// The autogen album playlists under `dir`, in folder order: directories
    /// depth-first — the same order the rail's folder tree walks, since both
    /// sort paths lexicographically — and within a directory by album title.
    /// [`FolderMode::Full`] takes every directory under `dir` too;
    /// [`FolderMode::Here`] only the albums rooted directly in `dir`.
    ///
    /// An album playlist's songs all live in one directory — the directory is
    /// part of its [`AlbumOrigin`](super::playlist::AlbumOrigin) key — so
    /// a covered album is covered whole: no partial albums, and the groups a
    /// folder scope yields are the playlists as-is.
    pub fn albums_under(&self, dir: &Path, mode: FolderMode) -> Vec<PlaylistId> {
        let mut by_dir: BTreeMap<&Path, Vec<(&str, PlaylistId)>> = BTreeMap::new();
        for playlist in self.playlists() {
            if let Some(album) = playlist.origin() {
                let covered = match mode {
                    FolderMode::Full => album.dir.starts_with(dir),
                    FolderMode::Here => album.dir == dir,
                };
                if covered {
                    by_dir
                        .entry(album.dir.as_path())
                        .or_default()
                        .push((album.name.as_str(), playlist.id));
                }
            }
        }
        by_dir
            .into_iter()
            .flat_map(|(_, mut albums)| {
                albums.sort_unstable();
                albums.into_iter().map(|(_, id)| id)
            })
            .collect()
    }

    /// The songs `sel` picks out, flattened out of their album groups — the
    /// shape a flat "add songs" list wants.
    pub fn select_songs(&self, sel: &Selection<'_>) -> Vec<SongId> {
        self.select(sel)
            .into_iter()
            .flat_map(|group| group.songs)
            .map(|song| song.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::search::parse;
    use crate::model::{ReleaseDate, SongMetadata};
    use std::time::Duration;

    fn song(id: SongId, title: &str, artist: &str, album: &str, track: u16) -> SongMetadata {
        SongMetadata {
            id,
            path: format!("/music/{album}/{title}.flac").into(),
            song_name: Some(title.into()),
            artists: vec![artist.into()],
            album_name: Some(album.into()),
            album_artist: Some(artist.into()),
            track_position: Some(track),
            duration: Duration::from_secs(180),
            ..Default::default()
        }
    }

    /// Three albums by two artists, in a known order.
    fn library() -> Library {
        let mut library = Library::default();
        let mut oblivion = song(SongId(1), "Oblivion", "Grimes", "Visions", 1);
        oblivion.date = Some(ReleaseDate::new(2012, None, None));
        let mut genesis = song(SongId(2), "Genesis", "Grimes", "Visions", 2);
        genesis.date = Some(ReleaseDate::new(2012, None, None));
        let mut kvm = song(SongId(3), "Kill V. Maim", "Grimes", "Art Angels", 1);
        kvm.date = Some(ReleaseDate::new(2015, None, None));
        library.insert_song(oblivion);
        library.insert_song(genesis);
        library.insert_song(kvm);
        library.insert_song(song(SongId(4), "The Night We Met", "Lord Huron", "Vide Noir", 1));
        library.rebuild_auto();
        library
    }

    fn titles(library: &Library, groups: &[SongGroup]) -> Vec<String> {
        groups.iter().map(|g| library.playlist(g.playlist).unwrap().title().to_string()).collect()
    }

    fn select(library: &Library, scope: Scope, order: Order) -> Vec<SongGroup> {
        library.select(&Selection { scope: &scope, query: &Query::default(), order })
    }

    #[test]
    fn library_scope_groups_the_whole_library_by_album() {
        let library = library();
        let groups = select(&library, Scope::Library, Order::AlbumTitle);
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Vide Noir", "Visions"]);
        assert_eq!(groups.iter().map(|g| g.songs.len()).sum::<usize>(), 4);
    }

    #[test]
    fn artist_scope_yields_that_artists_discography() {
        let library = library();
        let groups =
            select(&library, Scope::Artist("Grimes".into()), Order::AlbumTitle);
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Visions"]);
        assert!(select(&library, Scope::Artist("Nobody".into()), Order::AlbumTitle).is_empty());
    }

    /// A tagged song living in a specific directory — the folder tests need
    /// control over the paths, which `song`'s `/music/{album}/` shape fixes.
    fn song_in(id: SongId, dir: &str, title: &str, artist: &str, album: &str, track: u16) -> SongMetadata {
        SongMetadata {
            id,
            path: format!("{dir}/{title}.flac").into(),
            song_name: Some(title.into()),
            artists: vec![artist.into()],
            album_name: Some(album.into()),
            album_artist: Some(artist.into()),
            track_position: Some(track),
            duration: Duration::from_secs(180),
            ..Default::default()
        }
    }

    /// Albums nested in folders: the two-track `Alpha` directly in `/music/A`,
    /// the one-track `Alpha Live` in its `live` subfolder, and `Beta` beside
    /// them in `/music/B`.
    fn folder_library() -> Library {
        let mut library = Library::default();
        library.insert_song(song_in(SongId(1), "/music/A", "One", "X", "Alpha", 1));
        library.insert_song(song_in(SongId(2), "/music/A", "Two", "X", "Alpha", 2));
        library.insert_song(song_in(SongId(3), "/music/A/live", "Live", "X", "Alpha Live", 1));
        library.insert_song(song_in(SongId(4), "/music/B", "Beta Song", "Y", "Beta", 1));
        library.rebuild_auto();
        library
    }

    #[test]
    fn folder_scope_flattens_subfolders_in_walk_order() {
        let library = folder_library();
        let groups = select(
            &library,
            Scope::Folder { dir: "/music".into(), mode: FolderMode::Full },
            Order::TrackOrder,
        );
        assert_eq!(titles(&library, &groups), vec!["Alpha", "Alpha Live", "Beta"]);
        assert_eq!(
            groups[0].songs.iter().map(|song| song.id).collect::<Vec<_>>(),
            vec![SongId(1), SongId(2)],
            "tracks in album order"
        );
    }

    #[test]
    fn folder_here_mode_takes_only_the_folders_own_albums() {
        let library = folder_library();
        let groups = select(
            &library,
            Scope::Folder { dir: "/music/A".into(), mode: FolderMode::Here },
            Order::TrackOrder,
        );
        assert_eq!(titles(&library, &groups), vec!["Alpha"], "the subfolder's album stays out");
        let full = select(
            &library,
            Scope::Folder { dir: "/music/A".into(), mode: FolderMode::Full },
            Order::TrackOrder,
        );
        assert_eq!(titles(&library, &full), vec!["Alpha", "Alpha Live"]);
    }

    #[test]
    fn albums_in_one_directory_order_by_title() {
        let mut library = Library::default();
        library.insert_song(song_in(SongId(1), "/music/A", "Zed", "X", "Zebra", 1));
        library.insert_song(song_in(SongId(2), "/music/A", "Apple", "Y", "Apricot", 1));
        library.rebuild_auto();
        let groups = select(
            &library,
            Scope::Folder { dir: "/music/A".into(), mode: FolderMode::Full },
            Order::TrackOrder,
        );
        assert_eq!(titles(&library, &groups), vec!["Apricot", "Zebra"]);
    }

    #[test]
    fn newest_first_orders_by_release_year() {
        let library = library();
        let groups = select(&library, Scope::Library, Order::NewestFirst);
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Visions", "Vide Noir"]);
    }

    #[test]
    fn the_query_filters_songs_within_the_scope() {
        let library = library();
        let (query, _) = parse("genesis");
        let scope = Scope::Library;
        let groups = library.select(&Selection {
            scope: &scope,
            query: &query,
            order: Order::AlbumTitle,
        });
        assert_eq!(titles(&library, &groups), vec!["Visions"], "albums with no match drop out");
        assert_eq!(groups[0].songs.iter().map(|song| song.id).collect::<Vec<_>>(), vec![SongId(2)]);
    }

    #[test]
    fn select_songs_flattens_the_groups() {
        let library = library();
        let songs = library.select_songs(&Selection {
            scope: &Scope::Library,
            query: &Query::default(),
            order: Order::AlbumTitle,
        });
        assert_eq!(songs, vec![SongId(3), SongId(4), SongId(1), SongId(2)], "album title order, track order within");
    }
}
