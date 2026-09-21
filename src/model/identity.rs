use std::hash::Hasher;
use std::path::Path;

use twox_hash::XxHash64;

/// Stable song identity, derived from the canonicalized file path.
///
/// NOTE: intentionally NOT `std::hash::DefaultHasher` (SipHash) — that
/// algorithm is explicitly unstable across Rust versions, which would
/// silently invalidate a persisted library/cover cache on toolchain
/// upgrades. XxHash64 is fast and has a fixed, documented algorithm.
///
/// Every cross-reference in the app (`Library`, `Album`, `StreamingInfo`,
/// cover cache keys) goes through this id rather than passing owned
/// `SongMetadata` copies around, so a tag edit only has to happen once.
pub type SongId = u64;

/// Derive the [`SongId`] for a path.
///
/// The path is canonicalized first, so symlinked or relative paths that
/// point at the same file resolve to the same id. Returns an error if the
/// path doesn't exist (canonicalization needs it to).
pub fn generate_song_id(path: &Path) -> anyhow::Result<SongId> {
    let canonical = path.canonicalize()?;
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(canonical.to_string_lossy().as_bytes());
    Ok(hasher.finish())
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
}
