//! Stable song identity.
//!
//! A [`SongId`] is an opaque number allocated by the scanner from a persistent
//! counter, not a hash of the file path. That makes identity independent of
//! where a file lives: moving a file can be reconciled by its tag fingerprint
//! and keep its stats and playlist membership.
//!
//! Every cross-reference in the app (`Library`, `Playlist` song lists and
//! cover picks, cover cache keys) goes through this id rather than passing
//! owned `SongMetadata` copies around.
//!
//! A newtype (rather than a bare `u64`) so a playlist id, counter or hash
//! can't be passed where a song id belongs. The serde wire shape is unchanged
//! (`#[serde(transparent)]` keeps it a plain number).

use serde::{Deserialize, Deserializer, Serialize};

/// Stable song identity, allocated by the library and persisted alongside the
/// user data so playlists and stats survive cache invalidations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SongId(pub u64);

/// A number in a value position, a numeric string when the id is a map key
/// passing through `serde(flatten)`'s content buffer.
#[derive(Deserialize)]
#[serde(untagged)]
enum IdRepr {
    Number(u64),
    Text(String),
}

impl<'de> Deserialize<'de> for SongId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match IdRepr::deserialize(deserializer)? {
            IdRepr::Number(id) => Self(id),
            IdRepr::Text(text) => {
                Self(text.parse().map_err(|_| serde::de::Error::custom("invalid song id"))?)
            }
        })
    }
}

impl SongId {
    /// The underlying number, for hashing, display and persistence edges.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for SongId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_serde_transparent() {
        let id = SongId(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: SongId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn distinct_ids_stay_distinct() {
        assert_ne!(SongId(1), SongId(2));
        assert_eq!(SongId(7).get(), 7);
    }
}
