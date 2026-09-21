use std::collections::HashMap;

use super::identity::SongId;
use super::song::SongMetadata;

/// An album as a set of references — it holds [`SongId`]s, not owned
/// [`SongMetadata`], so a tag edit only has to happen in one place.
#[derive(Debug, Clone)]
pub struct Album {
    pub name: Option<String>,
    pub total_songs: Option<u16>,
    pub song_ids: Vec<SongId>,
}

/// The single source of truth for static library state.
///
/// Albums, playlists, and the play queue all reference songs by
/// [`SongId`] rather than owning duplicate `SongMetadata` structs — this
/// avoids the classic "edited the tag in one place, three other copies are
/// now stale" bug.
#[derive(Default)]
pub struct Library {
    pub songs: HashMap<SongId, SongMetadata>,
    pub albums: HashMap<u64, Album>,
}

impl Library {
    pub fn insert_song(&mut self, song: SongMetadata) -> SongId {
        let id = song.id;
        self.songs.insert(id, song);
        id
    }

    pub fn get(&self, id: SongId) -> Option<&SongMetadata> {
        self.songs.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CoverState, Lyrics};
    use std::path::PathBuf;
    use std::time::Duration;

    fn song(id: SongId, name: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: PathBuf::from("/music/x.flac"),
            song_name: Some(name.to_string()),
            artists: Vec::new(),
            album_name: None,
            track_position: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            cover: CoverState::Missing,
        }
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let mut lib = Library::default();
        let id = lib.insert_song(song(7, "Seven"));
        assert_eq!(id, 7);
        assert_eq!(lib.get(7).unwrap().song_name.as_deref(), Some("Seven"));
        assert!(lib.get(8).is_none());
    }

    #[test]
    fn reinserting_same_id_replaces() {
        let mut lib = Library::default();
        lib.insert_song(song(1, "Old"));
        lib.insert_song(song(1, "New"));
        assert_eq!(lib.songs.len(), 1);
        assert_eq!(lib.get(1).unwrap().song_name.as_deref(), Some("New"));
    }
}
