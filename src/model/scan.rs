//! Directory scanning and mtime-based incremental sync.
//!
//! The library is built from a list of directories. Rather than re-reading
//! every file's tags on each launch, the cache records a per-directory mtime
//! stamp: if nothing in the tree changed, the scan is skipped entirely.
//! Within a changed tree, only files whose mtime moved are re-parsed.
//!
//! What's persisted is metadata only — never audio (decoded at play time) and
//! never pixels (the cover cache owns those). That keeps the cache small and
//! makes "reopen the app" cheap even for a large library.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::config::Config;
use super::library::Library;
use super::lyrics;
use super::playlist::Playlist;
use super::song::SongMetadata;

/// What a sync pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Files parsed (new or changed).
    pub parsed: usize,
    /// Files whose cached metadata was reused untouched.
    pub reused: usize,
    /// Cached songs dropped because the file disappeared.
    pub removed: usize,
    /// Roots skipped entirely because nothing in the tree changed.
    pub skipped_roots: usize,
    /// Files that couldn't be read (kept out of the library, not fatal).
    pub errors: Vec<(PathBuf, String)>,
}

/// One cached song: the metadata exactly as loaded, plus the one cache-only
/// bit — when the file was last read, which is what an incremental sync
/// compares against.
///
/// Holding a whole [`SongMetadata`] rather than mirroring its fields means the
/// mapping can't drift when the schema grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSong {
    song: SongMetadata,
    modified: SystemTime,
}

impl CachedSong {
    fn from_song(song: &SongMetadata, modified: SystemTime) -> Self {
        Self { song: song.clone(), modified }
    }

    fn to_song(&self) -> SongMetadata {
        self.song.clone()
    }
}

/// The on-disk cache format. Versioned so a schema change can migrate instead
/// of silently mis-parsing an old file.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    dirs: HashMap<PathBuf, SystemTime>,
    songs: Vec<CachedSong>,
    playlists: Vec<Playlist>,
    next_playlist_id: u64,
}

/// Bumped whenever the cached shape changes. v2: `nominal_bitrate` moved from
/// kbps (as lofty reports it) to bits per second. v3: songs carry `year` and
/// their lyrics (embedded or from a `.lrc` sidecar). v4: a song is cached
/// whole (`SongMetadata`) instead of as a mirrored field list. v5: songs
/// carry their full release date (month/day), for chronological ordering.
/// v6: `has_art` serializes with the song, the duplicate root list is gone
/// (the config is the source of truth), and autogen playlists derive their
/// meta instead of storing it.
const CACHE_VERSION: u32 = 6;

/// Persisted library state: where we scanned, per-directory stamps, and the
/// cached song metadata.
pub struct LibraryCache {
    path: PathBuf,
    /// Per-directory mtime stamps. A change anywhere in the tree shows up
    /// here, so an unchanged tree skips the walk entirely.
    dirs: HashMap<PathBuf, SystemTime>,
    entries: HashMap<PathBuf, CachedSong>,
    playlists: Vec<Playlist>,
    next_playlist_id: u64,
}

impl LibraryCache {
    /// Open the cache at `path`, loading it if present.
    ///
    /// A missing or unreadable cache is not an error — it just means a first
    /// scan. A version mismatch is treated the same way rather than risking a
    /// mis-parse.
    pub fn open(path: PathBuf) -> Self {
        let loaded = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
            .filter(|cache| cache.version == CACHE_VERSION);

        match loaded {
            Some(cache) => Self {
                path,
                dirs: cache.dirs,
                entries: cache.songs.into_iter().map(|s| (s.song.path.clone(), s)).collect(),
                playlists: cache.playlists,
                next_playlist_id: cache.next_playlist_id,
            },
            None => Self {
                path,
                dirs: HashMap::new(),
                entries: HashMap::new(),
                playlists: Vec::new(),
                next_playlist_id: 0,
            },
        }
    }

    /// The file this cache is persisted to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Populate `library` from the cache without touching the filesystem —
    /// the instant-start path before (or instead of) a sync.
    pub fn install(&self, library: &mut Library) {
        let songs = self.entries.values().map(|entry| (entry.song.id, entry.to_song())).collect();
        let playlists = self.playlists.iter().map(|p| (p.id, p.clone())).collect();
        library.install(songs, playlists, self.next_playlist_id);
    }

    /// Bring `library` up to date with `roots` on disk, reusing cached
    /// metadata for anything unchanged. Returns what changed.
    ///
    /// `force` skips the fast path, re-reading every file — the escape hatch
    /// for in-place tag edits, which change a file's mtime but not its
    /// directory's, so the fast path can't see them.
    pub fn sync(&mut self, library: &mut Library, roots: &[PathBuf], force: bool) -> SyncReport {
        let mut report = SyncReport::default();

        for root in normalize_roots(roots) {
            let current = collect_dirs(&root);

            // Fast path: nothing in the tree changed, so every cached song
            // under this root is still valid — skip the walk entirely.
            let known = self.dirs.keys().filter(|d| d.starts_with(&root)).count();
            let unchanged = !force
                && !current.is_empty()
                && current.len() == known
                && current.iter().all(|(d, t)| self.dirs.get(d) == Some(t));

            if unchanged {
                report.skipped_roots += 1;
                report.reused += self
                    .entries
                    .values()
                    .filter(|e| e.song.path.starts_with(&root))
                    .count();
                continue;
            }

            let mut seen: HashSet<PathBuf> = HashSet::new();
            for file in collect_files(&root) {
                seen.insert(file.clone());
                let mtime = source_stamp(&file);
                if let (Some(cached), Some(m)) = (self.entries.get(&file), mtime) {
                    if cached.modified == m {
                        report.reused += 1;
                        continue;
                    }
                }
                match SongMetadata::load(&file) {
                    Ok(song) => {
                        self.entries.insert(
                            file.clone(),
                            CachedSong::from_song(&song, mtime.unwrap_or(UNIX_EPOCH)),
                        );
                        report.parsed += 1;
                    }
                    Err(e) => report.errors.push((file.clone(), e.to_string())),
                }
            }

            // Anything cached under this root that we didn't see is gone.
            let before = self.entries.len();
            self.entries
                .retain(|path, _| !path.starts_with(&root) || seen.contains(path));
            report.removed += before - self.entries.len();

            // Refresh this root's directory stamps.
            self.dirs.retain(|d, _| !d.starts_with(&root));
            self.dirs.extend(current);
        }

        // Rebuild the library's song map from the (now current) cache, then
        // drop dangling references and refresh autogen playlists. Custom
        // playlists the library already holds are left alone.
        library.replace_songs(self.entries.values().map(|e| (e.song.id, e.to_song())).collect());
        library.prune();
        library.rebuild_auto();

        report
    }

    /// Persist the library (its playlists and id counter, plus the cached
    /// song metadata) to disk.
    pub fn save(&mut self, library: &Library) -> anyhow::Result<()> {
        self.playlists = library.playlists().into_iter().cloned().collect();
        self.next_playlist_id = library.next_playlist_id();

        let file = CacheFile {
            version: CACHE_VERSION,
            dirs: self.dirs.clone(),
            songs: self.entries.values().cloned().collect(),
            playlists: self.playlists.clone(),
            next_playlist_id: self.next_playlist_id,
        };

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&file)?;
        // Write to a temp file and rename, so a crash mid-write can't leave a
        // truncated cache that fails to parse on the next launch.
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Forget cached songs and directory stamps that no longer fall under any
    /// of `roots`.
    ///
    /// Used when the synced-path list changes. Roots may nest, so an entry is
    /// only dropped when *no* remaining root covers it: removing `b` while
    /// `a/b` is still synced leaves `a/b`'s contents alone, but removing `a`
    /// drops them, since nothing else reaches them.
    pub fn retain_roots(&mut self, roots: &[PathBuf]) {
        let normalized = normalize_roots(roots);
        let covered = |path: &Path| normalized.iter().any(|root| path.starts_with(root));
        self.entries.retain(|path, _| covered(path));
        self.dirs.retain(|dir, _| covered(dir));
    }
}

/// Canonicalize, dedupe, and drop roots nested inside another — the parent's
/// walk already covers them, so scanning both would double-read every file.
fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canon: Vec<PathBuf> = roots.iter().filter_map(|p| p.canonicalize().ok()).collect();
    canon.sort();
    canon.dedup();

    let mut result: Vec<PathBuf> = Vec::new();
    for path in canon {
        // After sorting, a parent always precedes its children, so checking
        // the kept set is enough to catch nesting.
        if !result.iter().any(|parent| path.starts_with(parent)) {
            result.push(path);
        }
    }
    result
}

/// Recursively collect every directory under `root` (including `root`), with
/// its mtime.
fn collect_dirs(root: &Path) -> HashMap<PathBuf, SystemTime> {
    let mut dirs = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Some(mtime) = dir_modified(&dir) {
            dirs.insert(dir.clone(), mtime);
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    dirs
}

/// Recursively collect every supported audio file under `root`.
fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if Config::is_supported(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn dir_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn file_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// The stamp a cached entry is invalidated by.
///
/// It's the audio file's mtime, or the `.lrc` sidecar's when that's newer — so
/// editing a sidecar re-parses the song instead of leaving stale lyrics in the
/// cache. The sidecar lives in the same directory, so its own directory-mtime
/// bump would re-walk the tree anyway; this is what makes the *file* re-read.
fn source_stamp(path: &Path) -> Option<SystemTime> {
    let audio = file_modified(path);
    let sidecar = file_modified(&lyrics::sidecar_path(path));
    match (audio, sidecar) {
        (Some(a), Some(s)) => Some(a.max(s)),
        (Some(a), None) => Some(a),
        (None, s) => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lyrics;
    use std::time::Duration;

    /// A unique temp directory per test, cleaned up by the caller's drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("larkspur-scan-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, b"not really audio").unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nested_roots_are_deduped_and_pruned() {
        let tmp = TempDir::new("roots");
        let child = tmp.path().join("child");
        fs::create_dir_all(&child).unwrap();

        let roots = normalize_roots(&[child.clone(), tmp.path().to_path_buf()]);
        assert_eq!(roots, vec![tmp.path().canonicalize().unwrap()]);
    }

    #[test]
    fn collect_files_filters_by_supported_extension() {
        let tmp = TempDir::new("collect");
        tmp.touch("a.flac");
        tmp.touch("sub/b.opus");
        tmp.touch("sub/notes.txt");

        let mut files: Vec<String> = collect_files(tmp.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files, vec!["a.flac", "b.opus"]);
    }

    #[test]
    fn collect_dirs_walks_the_whole_tree() {
        let tmp = TempDir::new("dirs");
        tmp.touch("a/b/c.flac");
        let dirs = collect_dirs(tmp.path());
        assert_eq!(dirs.len(), 3, "root, a, and a/b");
    }

    #[test]
    fn unchanged_tree_is_skipped_on_the_next_sync() {
        let tmp = TempDir::new("skip");
        tmp.touch("album/track.flac");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();

        let first = cache.sync(&mut library, &[tmp.path().to_path_buf()], false);
        assert_eq!(first.skipped_roots, 0);
        assert_eq!(first.errors.len(), 1, "the fake file can't be parsed");

        let second = cache.sync(&mut library, &[tmp.path().to_path_buf()], false);
        assert_eq!(second.skipped_roots, 1, "nothing changed, so nothing walked");
    }

    #[test]
    fn force_rescans_an_otherwise_unchanged_tree() {
        let tmp = TempDir::new("force");
        tmp.touch("track.flac");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();

        cache.sync(&mut library, &[tmp.path().to_path_buf()], false);
        let forced = cache.sync(&mut library, &[tmp.path().to_path_buf()], true);
        assert_eq!(forced.skipped_roots, 0);
        assert_eq!(forced.errors.len(), 1, "re-read, and failed again");
    }

    #[test]
    fn cache_round_trips_playlists_and_songs() {
        let tmp = TempDir::new("roundtrip");
        let cache_path = tmp.path().join("cache.json");

        let mut library = Library::default();
        library.insert_song(SongMetadata {
            id: 42,
            path: tmp.path().join("x.flac"),
            song_name: Some("X".into()),
            artists: vec!["Grimes".into()],
            album_name: Some("Visions".into()),
            album_artist: None,
            track_position: Some(3),
            year: Some(2012),
            release_date: Some((2012, 1, 1)),
            nominal_bitrate: Some(320_000),
            lyrics: Lyrics::Plain("a line".into()),
            duration: Duration::from_secs(200),
            has_art: true,
        });
        let custom = library.create_custom("Faves");
        library.add_song(custom, 42);

        let mut cache = LibraryCache::open(cache_path.clone());
        // Seed the per-file entry the way a real scan would.
        cache.entries.insert(
            tmp.path().join("x.flac"),
            CachedSong::from_song(library.get(42).unwrap(), UNIX_EPOCH),
        );
        cache.save(&library).unwrap();

        let mut reloaded = Library::default();
        LibraryCache::open(cache_path).install(&mut reloaded);
        assert_eq!(reloaded.song_count(), 1);
        assert_eq!(reloaded.get(42).unwrap().album(), "Visions");
        let song = reloaded.get(42).unwrap();
        assert_eq!(song.year, Some(2012), "the year survives the cache");
        assert_eq!(
            song.release_date,
            Some((2012, 1, 1)),
            "the release date survives the cache"
        );
        assert_eq!(
            song.lyrics,
            Lyrics::Plain("a line".into()),
            "lyrics are cached with the rest of the metadata"
        );
        assert_eq!(reloaded.custom_playlists().len(), 1);
        assert_eq!(reloaded.songs_of(custom).len(), 1);
    }

    #[test]
    fn retain_roots_only_drops_what_no_remaining_root_covers() {
        // Two roots, with a subdirectory nested under the first.
        let tmp = TempDir::new("retain");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let nested_dir = a.join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::create_dir_all(&b).unwrap();
        let nested = nested_dir.join("x.flac");
        let other = b.join("y.flac");

        let seed = |cache: &mut LibraryCache| {
            let song = |id, path: &Path| SongMetadata {
                id,
                path: path.to_path_buf(),
                song_name: Some("X".into()),
                artists: Vec::new(),
                album_name: None,
                album_artist: None,
                track_position: None,
                year: None,
                release_date: None,
                nominal_bitrate: None,
                lyrics: Lyrics::None,
                duration: Duration::ZERO,
                has_art: false,
            };
            cache.entries.insert(nested.clone(), CachedSong::from_song(&song(1, &nested), UNIX_EPOCH));
            cache.entries.insert(other.clone(), CachedSong::from_song(&song(2, &other), UNIX_EPOCH));
            cache.dirs.insert(a.clone(), UNIX_EPOCH);
            cache.dirs.insert(nested_dir.clone(), UNIX_EPOCH);
            cache.dirs.insert(b.clone(), UNIX_EPOCH);
        };

        // Removing `b` leaves `a`'s subtree — nested directory and all — alone.
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[a.clone()]);
        assert!(cache.entries.contains_key(&nested), "a/nested is still covered by a");
        assert!(!cache.entries.contains_key(&other), "b's song is dropped");
        assert!(cache.dirs.contains_key(&nested_dir), "a's directory stamps survive");

        // Removing `a` instead drops the nested contents, leaving only `b`'s.
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[b.clone()]);
        assert!(!cache.entries.contains_key(&nested), "a/nested is gone with a");
        assert!(cache.entries.contains_key(&other), "b's song remains");
    }

    #[test]
    fn version_mismatch_is_treated_as_empty() {
        let tmp = TempDir::new("version");
        let cache_path = tmp.path().join("cache.json");
        let stale = r#"{"version":999,"dirs":{},"songs":[],"playlists":[],"next_playlist_id":5}"#;
        fs::write(&cache_path, stale).unwrap();

        let mut library = Library::default();
        LibraryCache::open(cache_path).install(&mut library);
        assert_eq!(library.song_count(), 0);
        assert!(library.playlists().is_empty());
    }

    #[test]
    fn sync_rebuilds_autogen_playlists() {
        let tmp = TempDir::new("autogen");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();
        // Seed a cached song by hand (a real scan needs real audio).
        library.insert_song(SongMetadata {
            id: 7,
            path: tmp.path().join("y.flac"),
            song_name: Some("Y".into()),
            artists: vec!["Grimes".into()],
            album_name: Some("Visions".into()),
            album_artist: None,
            track_position: Some(1),
            year: None,
            release_date: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            has_art: false,
        });
        cache.entries.insert(
            tmp.path().join("y.flac"),
            CachedSong::from_song(library.get(7).unwrap(), UNIX_EPOCH),
        );

        // A sync with no roots still rebuilds from whatever is cached.
        cache.sync(&mut library, &[], false);
        let auto = library.playlists();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].meta().title, "Visions");
        assert_eq!(auto[0].song_ids, vec![7]);
    }
}
