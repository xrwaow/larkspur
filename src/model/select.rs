//! Selection — one way to ask the library for songs: a scope, a query, and
//! an order.
//!
//! The browse view, search, and an artist's discography are all the same
//! question with different parameters, so they share this one entry point
//! rather than each walking the library their own way.

use super::identity::SongId;
use super::library::Library;
use super::playlist::PlaylistId;
use super::search::{AlbumGroup, Query};

/// What a [`Selection`] draws its songs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole library, grouped into its autogen album playlists.
    Library,
    /// One artist's discography — their album playlists, via the library's
    /// artist index.
    Artist(String),
    /// An explicit list of playlists, each in its own track order.
    Playlists(Vec<PlaylistId>),
}

/// How a [`Selection`]'s album groups are sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// Alphabetically by album title — the browse view's order.
    #[default]
    AlbumTitle,
    /// Newest release first, undated albums last — the discography order.
    NewestFirst,
    /// The order the scope itself yields: a playlist list keeps the order it
    /// was given in.
    TrackOrder,
}

/// One way to ask the library for songs: a scope, a query, and an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub scope: Scope,
    /// Filters the songs; the default (empty) query matches everything.
    pub query: Query,
    pub order: Order,
}

impl Library {
    /// The songs `sel` picks out, grouped by album playlist. Groups with no
    /// matching songs are left out.
    pub fn select(&self, sel: &Selection) -> Vec<AlbumGroup> {
        let mut ids: Vec<PlaylistId> = match &sel.scope {
            Scope::Library => self
                .playlists()
                .into_iter()
                .filter(|p| p.is_auto())
                .map(|p| p.id)
                .collect(),
            Scope::Artist(artist) => self.discography(artist),
            Scope::Playlists(ids) => ids.clone(),
        };
        match sel.order {
            Order::AlbumTitle => ids.sort_by(|a, b| self.title_of(*a).cmp(&self.title_of(*b))),
            Order::NewestFirst => self.sort_by_year(&mut ids),
            Order::TrackOrder => {}
        }
        let matcher = sel.query.matcher();
        ids.into_iter()
            .filter_map(|id| {
                let playlist = self.playlist(id)?;
                let songs: Vec<SongId> = playlist
                    .song_ids
                    .iter()
                    .copied()
                    .filter(|song| self.get(*song).is_some_and(|song| matcher.matches(song)))
                    .collect();
                (!songs.is_empty()).then_some(AlbumGroup { playlist: id, songs })
            })
            .collect()
    }

    /// The songs `sel` picks out, flattened out of their album groups — the
    /// shape a flat "add songs" list wants.
    pub fn select_songs(&self, sel: &Selection) -> Vec<SongId> {
        self.select(sel).into_iter().flat_map(|group| group.songs).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::search::parse;
    use crate::model::{Lyrics, ReleaseDate, SongMetadata};
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
            date: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::from_secs(180),
            has_art: false,
        }
    }

    /// Three albums by two artists, in a known order.
    fn library() -> Library {
        let mut library = Library::default();
        let mut oblivion = song(1, "Oblivion", "Grimes", "Visions", 1);
        oblivion.date = Some(ReleaseDate::new(2012, None, None));
        let mut genesis = song(2, "Genesis", "Grimes", "Visions", 2);
        genesis.date = Some(ReleaseDate::new(2012, None, None));
        let mut kvm = song(3, "Kill V. Maim", "Grimes", "Art Angels", 1);
        kvm.date = Some(ReleaseDate::new(2015, None, None));
        library.insert_song(oblivion);
        library.insert_song(genesis);
        library.insert_song(kvm);
        library.insert_song(song(4, "The Night We Met", "Lord Huron", "Vide Noir", 1));
        library.rebuild_auto();
        library
    }

    fn titles(library: &Library, groups: &[AlbumGroup]) -> Vec<String> {
        groups.iter().map(|g| library.playlist(g.playlist).unwrap().title().to_string()).collect()
    }

    fn selection(scope: Scope, order: Order) -> Selection {
        Selection { scope, query: Query::default(), order }
    }

    #[test]
    fn library_scope_groups_the_whole_library_by_album() {
        let library = library();
        let groups = library.select(&selection(Scope::Library, Order::AlbumTitle));
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Vide Noir", "Visions"]);
        assert_eq!(groups.iter().map(|g| g.songs.len()).sum::<usize>(), 4);
    }

    #[test]
    fn artist_scope_yields_that_artists_discography() {
        let library = library();
        let groups =
            library.select(&selection(Scope::Artist("Grimes".into()), Order::AlbumTitle));
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Visions"]);
        assert!(library.select(&selection(Scope::Artist("Nobody".into()), Order::AlbumTitle)).is_empty());
    }

    #[test]
    fn playlist_scope_keeps_the_given_playlists_in_track_order() {
        let mut library = library();
        let custom = library.create_custom("Faves");
        library.add_song(custom, 2);
        library.add_song(custom, 1);
        let visions = library.album_playlist_of(1).unwrap();
        let art_angels = library.album_playlist_of(3).unwrap();

        let groups = library.select(&selection(
            Scope::Playlists(vec![custom, art_angels, visions]),
            Order::TrackOrder,
        ));
        assert_eq!(titles(&library, &groups), vec!["Faves", "Art Angels", "Visions"]);
        assert_eq!(groups[0].songs, vec![2, 1], "the playlist's own order, not album order");
    }

    #[test]
    fn newest_first_orders_by_release_year() {
        let library = library();
        let groups = library.select(&selection(Scope::Library, Order::NewestFirst));
        assert_eq!(titles(&library, &groups), vec!["Art Angels", "Visions", "Vide Noir"]);
    }

    #[test]
    fn the_query_filters_songs_within_the_scope() {
        let library = library();
        let (query, _) = parse("genesis");
        let groups = library.select(&Selection {
            scope: Scope::Library,
            query,
            order: Order::AlbumTitle,
        });
        assert_eq!(titles(&library, &groups), vec!["Visions"], "albums with no match drop out");
        assert_eq!(groups[0].songs, vec![2]);
    }

    #[test]
    fn select_songs_flattens_the_groups() {
        let library = library();
        let songs = library.select_songs(&selection(Scope::Library, Order::AlbumTitle));
        assert_eq!(songs, vec![3, 4, 1, 2], "album title order, track order within");
    }
}
