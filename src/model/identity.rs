use std::hash::Hasher;
use std::path::Path;

use serde::{Deserialize, Serialize};
use twox_hash::XxHash64;

/// Stable song identity, derived from the canonicalized file path.
///
/// NOTE: intentionally NOT `std::hash::DefaultHasher` (SipHash) — that
/// algorithm is explicitly unstable across Rust versions, which would
/// silently invalidate a persisted library/cover cache on toolchain
/// upgrades. XxHash64 is fast and has a fixed, documented algorithm.
///
/// Every cross-reference in the app (`Library`, `Playlist` song lists and
/// cover picks, cover cache keys) goes through this id rather than passing
/// owned `SongMetadata` copies around, so a tag edit only has to happen once.
///
/// A newtype (rather than a bare `u64`) so a playlist id, counter or hash
/// can't be passed where a song id belongs. The serde wire shape is unchanged
/// (`#[serde(transparent)]` keeps it a plain number).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SongId(pub u64);

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

/// Derive the [`SongId`] for a path.
///
/// The path is canonicalized first, so symlinked or relative paths that
/// point at the same file resolve to the same id. Returns an error if the
/// path doesn't exist (canonicalization needs it to).
pub fn generate_song_id(path: &Path) -> anyhow::Result<SongId> {
    let canonical = path.canonicalize()?;
    Ok(song_id_for_canonical(&canonical))
}

/// Derive the [`SongId`] for a path that is **already canonical**, skipping the
/// per-file `canonicalize` syscall. Callers must have resolved symlinks first
/// — the scanner canonicalizes symlinked directories during its walk, so its
/// file paths are canonical by construction.
pub fn song_id_for_canonical(path: &Path) -> SongId {
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(path.to_string_lossy().as_bytes());
    SongId(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_dir() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn stable_across_calls() {
        let a = generate_song_id(manifest_dir()).unwrap();
        let b = generate_song_id(manifest_dir()).unwrap();
        assert_eq!(a, b, "same path must always hash to the same id");
    }

    #[test]
    fn distinct_paths_produce_distinct_ids() {
        let root = generate_song_id(manifest_dir()).unwrap();
        let src = generate_song_id(&manifest_dir().join("src")).unwrap();
        assert_ne!(root, src);
    }

    #[test]
    fn missing_path_is_an_error() {
        assert!(generate_song_id(Path::new("/no/such/path/larkspur")).is_err());
    }

    #[test]
    fn canonical_id_matches_the_canonicalizing_one() {
        let canonical = manifest_dir().canonicalize().unwrap();
        assert_eq!(song_id_for_canonical(&canonical), generate_song_id(manifest_dir()).unwrap());
    }

    #[test]
    fn id_is_serde_transparent() {
        let id = SongId(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: SongId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
