//! User-owned state: playlists, stats, album ids, and the path→song-id map.
//!
//! These have a different lifetime from the scanned song cache: the cache is
//! disposable and rebuilt from files, while this data must survive cache
//! invalidation. Keeping it out of `LibraryCache` is also what lets a playlist
//! edit or a play count write a small file instead of the whole library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::SongId;
use super::persist::{read_versioned, write_versioned};
use super::playlist::{AlbumOrigin, Playlist, PlaylistId};
use super::stats::Stats;

const USER_VERSION: u32 = 1;
const IDENTITIES_VERSION: u32 = 1;

/// Persisted user state: what the user owns, as opposed to what a scan
/// discovers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserData {
    /// Custom playlists only. Autogen albums are rebuilt from tags, and smart
    /// views from stats.
    pub playlists: Vec<Playlist>,
    pub next_playlist_id: u64,
    /// Stable album id per origin. Kept so an album keeps its id across
    /// rescans and launches without hashing its title. Stored as pairs rather
    /// than a map: JSON object keys must be strings, and an [`AlbumOrigin`] is
    /// a structured key.
    #[serde(default)]
    pub album_ids: Vec<(AlbumOrigin, PlaylistId)>,
    #[serde(default)]
    pub stats: Stats,
}

/// The persistent path→song-id map and its allocator.
///
/// It lives separately from both the song cache and the user-content file:
/// the cache can be invalidated without losing identity, and a play count
/// does not rewrite the map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SongIdentities {
    pub paths: BTreeMap<PathBuf, SongId>,
    pub next_id: u64,
}

/// Reads and writes the user-owned files beside the library cache.
pub struct UserStore {
    user_path: PathBuf,
    identities_path: PathBuf,
}

impl UserStore {
    /// `library_path` is the song cache path; the user files are siblings.
    pub fn open(library_path: &Path) -> Self {
        Self {
            user_path: library_path.with_file_name("user.json"),
            identities_path: library_path.with_file_name("identities.json"),
        }
    }

    pub fn load_user(&self) -> UserData {
        read_versioned(&self.user_path, USER_VERSION).unwrap_or_default()
    }

    pub fn save_user(&self, user: &UserData) -> anyhow::Result<()> {
        write_versioned(&self.user_path, user, USER_VERSION, true)
    }

    pub fn load_identities(&self) -> SongIdentities {
        read_versioned(&self.identities_path, IDENTITIES_VERSION).unwrap_or_default()
    }

    pub fn save_identities(&self, identities: &SongIdentities) -> anyhow::Result<()> {
        write_versioned(&self.identities_path, identities, IDENTITIES_VERSION, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AlbumOrigin, PlaylistKind};

    #[test]
    fn user_data_round_trips_through_the_store() {
        let dir = std::env::temp_dir().join("larkspur-user-test");
        let _ = std::fs::remove_dir_all(&dir);
        let store = UserStore::open(&dir.join("library.json"));
        let mut user = UserData::default();
        user.next_playlist_id = 3;
        user.playlists.push(Playlist {
            id: PlaylistId(1),
            kind: PlaylistKind::Custom { title: "Faves".into() },
            song_ids: vec![SongId(7)],
        });
        user.stats.record_play(SongId(7), std::time::SystemTime::UNIX_EPOCH);
        user.album_ids.push((
            AlbumOrigin::new("Visions", "Grimes", PathBuf::from("/music/visions")),
            PlaylistId(2),
        ));
        store.save_user(&user).unwrap();

        let loaded = store.load_user();
        assert_eq!(loaded.next_playlist_id, 3);
        assert_eq!(loaded.playlists, user.playlists);
        assert_eq!(loaded.album_ids, user.album_ids);
        assert_eq!(loaded.stats.get(SongId(7)).plays, 1);
        assert_eq!(
            loaded.stats.get(SongId(7)).last_played,
            Some(std::time::SystemTime::UNIX_EPOCH)
        );

        let identities = SongIdentities {
            paths: BTreeMap::from([(PathBuf::from("/music/a.flac"), SongId(9))]),
            next_id: 10,
        };
        store.save_identities(&identities).unwrap();
        let loaded = store.load_identities();
        assert_eq!(loaded.paths.get(Path::new("/music/a.flac")), Some(&SongId(9)));
        assert_eq!(loaded.next_id, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
