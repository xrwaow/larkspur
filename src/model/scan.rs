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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::config::Config;
use super::persist::{read_versioned, write_versioned};
use super::library::Library;
use super::lyrics;
use super::playlist::{Playlist, PlaylistId};
use super::identity::SongId;
use super::song::SongMetadata;
use super::stats::Stats;

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
    song: Arc<SongMetadata>,
    modified: SystemTime,
}

impl CachedSong {
    fn new(song: SongMetadata, modified: SystemTime) -> Self {
        Self { song: Arc::new(song), modified }
    }

    /// The song as a shared handle — an `Arc` bump, not a metadata copy.
    fn to_song(&self) -> Arc<SongMetadata> {
        self.song.clone()
    }
}

/// The on-disk cache body; [`CACHE_VERSION`] lives in the envelope.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    /// `BTreeMap` so a save is byte-for-byte deterministic (stable order).
    dirs: BTreeMap<PathBuf, SystemTime>,
    songs: Vec<CachedSong>,
}

/// The on-disk shape of the user-owned playlists — deliberately a *separate*
/// file from the song cache, because the two have different lifetimes: songs
/// are rebuilt from the filesystem, while playlists are the user's and must
/// survive. Keeping them apart is what lets a playlist edit write a few
/// kilobytes instead of the whole library.
#[derive(Serialize, Deserialize)]
struct PlaylistsFile {
    playlists: Vec<Playlist>,
    next_playlist_id: u64,
}

/// The on-disk shape of the per-user song stats — a separate file, like the
/// playlists, because its lifetime is the user's, not the scan's.
#[derive(Serialize, Deserialize)]
struct StatsFile {
    stats: Stats,
}

/// Bumped whenever the cached shape changes. v2: `nominal_bitrate` moved from
/// kbps (as lofty reports it) to bits per second. v3: songs carry `year` and
/// their lyrics (embedded or from a `.lrc` sidecar). v4: a song is cached
/// whole (`SongMetadata`) instead of as a mirrored field list. v5: songs
/// carry their full release date (month/day), for chronological ordering.
/// v6: `has_art` serializes with the song, the duplicate root list is gone
/// (the config is the source of truth), and autogen playlists derive their
/// meta instead of storing it. v7: the year and release date collapse into one
/// packed `YYYYMMDD` [`ReleaseDate`](super::song::ReleaseDate). v8: a playlist's
/// kind carries its own data (the autogen origin, a custom/temporary title),
/// so `PlaylistMeta` and the parallel `meta`/`origin` fields are gone.
const CACHE_VERSION: u32 = 8;

/// Playlist schema version, independent of [`CACHE_VERSION`]: a song-schema
/// bump must not invalidate the user's playlists.
const PLAYLISTS_VERSION: u32 = 1;

/// Stats schema version, likewise independent of the song cache.
const STATS_VERSION: u32 = 1;

/// Persisted library state: where we scanned, per-directory stamps, and the
/// cached song metadata.
pub struct LibraryCache {
    path: PathBuf,
    /// Per-directory mtime stamps. A change anywhere in the tree shows up
    /// here, so an unchanged tree skips the walk entirely.
    dirs: HashMap<PathBuf, SystemTime>,
    entries: HashMap<SongId, CachedSong>,
}

impl LibraryCache {
    /// Open the cache at `path`, loading it if present.
    ///
    /// A missing or unreadable cache is not an error — it just means a first
    /// scan. A version mismatch is treated the same way rather than risking a
    /// mis-parse.
    pub fn open(path: PathBuf) -> Self {
        let loaded = read_versioned::<CacheFile>(&path, CACHE_VERSION);

        match loaded {
            Some(cache) => Self {
                path,
                dirs: cache.dirs.into_iter().collect(),
                entries: cache.songs.into_iter().map(|s| (s.song.id, s)).collect(),
            },
            None => Self {
                path,
                dirs: HashMap::new(),
                entries: HashMap::new(),
            },
        }
    }

    /// The file this cache is persisted to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Populate `library`'s songs from the cache without touching the
    /// filesystem — the instant-start path before (or instead of) a sync.
    /// Playlists come from [`load_playlists`](Self::load_playlists), since they
    /// live in their own file.
    pub fn install(&self, library: &mut Library) {
        let songs = self.entries.values().map(|entry| (entry.song.id, entry.to_song())).collect();
        library.replace_songs(songs);
    }

    /// Bring `library` up to date with `roots` on disk, reusing cached
    /// metadata for anything unchanged. Returns what changed.
    ///
    /// `force` skips the fast path, re-reading every file — the escape hatch
    /// for in-place tag edits, which change a file's mtime but not its
    /// directory's, so the fast path can't see them.
    ///
    /// Note the "fast path" still **walks every directory** to compare mtimes;
    /// it only skips re-parsing the audio files. On a launch where nothing
    /// changed, the per-directory walk remains the hot part; `force` is the
    /// explicit re-read, not the only traversal.
    ///
    /// `stats` is seeded with `added` stamps for songs the library gains: on
    /// the very first run (empty stats file) from the file's mtime, later
    /// from "now" — a copy preserves the old mtime, which would bury a newly
    /// added song at the bottom of "Recently Added".
    pub fn sync(
        &mut self,
        roots: &[PathBuf],
        force: bool,
        stats: &mut Stats,
    ) -> (HashMap<SongId, Arc<SongMetadata>>, SyncReport) {
        let mut report = SyncReport::default();
        // Entries are keyed by SongId; a path→id view keeps the per-file walk
        // a hash lookup instead of a linear scan.
        let mut by_path: HashMap<PathBuf, SongId> =
            self.entries.iter().map(|(id, e)| (e.song.path.clone(), *id)).collect();

        for root in normalize_roots(roots) {
            let (current, files) = collect_tree(&root);

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
            for file in files {
                seen.insert(file.clone());
                let mtime = source_stamp(&file);
                let cached_id = by_path.get(&file).copied();
                if let (Some(id), Some(m)) = (cached_id, mtime) {
                    if self.entries.get(&id).is_some_and(|cached| cached.modified == m) {
                        report.reused += 1;
                        continue;
                    }
                }
                match SongMetadata::load_canonical(&file) {
                    Ok(song) => {
                        let id = song.id;
                        if let Some(old) = cached_id {
                            self.entries.remove(&old);
                            by_path.remove(&file);
                        }
                        self.entries
                            .insert(id, CachedSong::new(song, mtime.unwrap_or(UNIX_EPOCH)));
                        by_path.insert(file.clone(), id);
                        report.parsed += 1;
                    }
                    Err(e) => report.errors.push((file.clone(), e.to_string())),
                }
            }

            // Anything cached under this root that we didn't see is gone.
            let before = self.entries.len();
            self.entries
                .retain(|_, entry| !entry.song.path.starts_with(&root) || seen.contains(&entry.song.path));
            report.removed += before - self.entries.len();

            // Refresh this root's directory stamps.
            self.dirs.retain(|d, _| !d.starts_with(&root));
            self.dirs.extend(current);
        }

        // Hand the caller the current song set rather than mutating a library:
        // the live library owns the custom playlists and id counter, so
        // applying the songs in place keeps both intact (and keeps album ids
        // from colliding with custom ids).
        let songs: HashMap<SongId, Arc<SongMetadata>> =
            self.entries.values().map(|e| (e.song.id, e.to_song())).collect();
        seed_added(songs.values().map(AsRef::as_ref), stats);

        (songs, report)
    }

    /// Persist the cached song metadata and directory stamps to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        let mut songs: Vec<CachedSong> = self.entries.values().cloned().collect();
        songs.sort_by_key(|entry| entry.song.id);
        let file = CacheFile {
            dirs: self.dirs.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            songs,
        };
        write_versioned(&self.path, &file, CACHE_VERSION, false)
    }

    /// The file the user-owned playlists are persisted to — a sibling of the
    /// song cache.
    fn playlists_path(&self) -> PathBuf {
        self.path.with_file_name("playlists.json")
    }

    /// Load the user-owned playlists and the id counter, or empty on a first
    /// run / version mismatch.
    pub fn load_playlists(&self) -> (HashMap<PlaylistId, Playlist>, u64) {
        let loaded = read_versioned::<PlaylistsFile>(&self.playlists_path(), PLAYLISTS_VERSION);
        match loaded {
            Some(file) => (
                file.playlists.into_iter().map(|p| (p.id, p)).collect(),
                file.next_playlist_id,
            ),
            None => (HashMap::new(), 0),
        }
    }

    /// Persist the user-owned playlists and the id counter. Small — this is
    /// what a playlist edit writes, instead of the whole song library.
    pub fn save_playlists(&self, library: &Library) -> anyhow::Result<()> {
        let file = PlaylistsFile {
            playlists: library.custom_playlists().into_iter().cloned().collect(),
            next_playlist_id: library.next_playlist_id(),
        };
        write_versioned(&self.playlists_path(), &file, PLAYLISTS_VERSION, true)
    }

    /// Persist the per-user song stats. Tiny — a few bytes per song the user
    /// has actually played.
    pub fn save_stats(&self, stats: &Stats) -> anyhow::Result<()> {
        let file = StatsFile { stats: stats.clone() };
        write_versioned(&self.stats_path(), &file, STATS_VERSION, false)
    }

    /// The file the per-user song stats are persisted to — a sibling of the
    /// song cache, like the playlists file.
    fn stats_path(&self) -> PathBuf {
        self.path.with_file_name("stats.json")
    }

    /// Load the per-user song stats, or empty on a first run / version
    /// mismatch. Never an error: stats are user state a broken file must not
    /// take down the library with.
    pub fn load_stats(&self) -> Stats {
        read_versioned::<StatsFile>(&self.stats_path(), STATS_VERSION).map(|file| file.stats)
            .unwrap_or_default()
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
        self.entries.retain(|_, entry| covered(&entry.song.path));
        self.dirs.retain(|dir, _| covered(dir));
    }
}

/// Stamp `added` for every song `stats` doesn't know yet.
///
/// On the very first run (no stats at all) the song file's mtime is the best
/// guess at when it entered the library. Later arrivals get "now": a copy
/// carries the original's mtime, so trusting it there would sort a newly
/// added song as if it had always been present.
fn seed_added<'a>(songs: impl IntoIterator<Item = &'a SongMetadata>, stats: &mut Stats) {
    let first_run = stats.is_empty();
    for song in songs {
        if stats.contains(song.id) {
            continue;
        }
        let stamp = if first_run {
            fs::metadata(&song.path).ok().and_then(|m| m.modified().ok()).unwrap_or_else(SystemTime::now)
        } else {
            SystemTime::now()
        };
        stats.ensure_added(song.id, stamp);
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

/// Recursively walk `root` once, returning every directory's mtime and every
/// supported audio file. A single traversal (rather than one for directories
/// and another for files) halves the `read_dir` calls on a changed root.
fn collect_tree(root: &Path) -> (HashMap<PathBuf, SystemTime>, Vec<PathBuf>) {
    let mut dirs = HashMap::new();
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Some(mtime) = modified(&dir) {
            dirs.insert(dir.clone(), mtime);
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                // Resolve once per link so every file path the walk yields is
                // canonical — `SongMetadata::load_canonical` then skips a
                // `canonicalize` syscall per file.
                let target = path.canonicalize().unwrap_or(path);
                if target.is_dir() {
                    stack.push(target);
                } else if Config::is_supported(&target) {
                    files.push(target);
                }
            } else if file_type.is_dir() {
                stack.push(path);
            } else if Config::is_supported(&path) {
                files.push(path);
            }
        }
    }
    (dirs, files)
}

/// A path's mtime, used for both directory stamps and file staleness.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// The stamp a cached entry is invalidated by.
///
/// It's the audio file's mtime, or the `.lrc` sidecar's when that's newer — so
/// editing a sidecar re-parses the song instead of leaving stale lyrics in the
/// cache. The sidecar lives in the same directory, so its own directory-mtime
/// bump would re-walk the tree anyway; this is what makes the *file* re-read.
fn source_stamp(path: &Path) -> Option<SystemTime> {
    let audio = modified(path);
    let sidecar = modified(&lyrics::sidecar_path(path));
    match (audio, sidecar) {
        (Some(a), Some(s)) => Some(a.max(s)),
        (Some(a), None) => Some(a),
        (None, s) => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lyrics, ReleaseDate, SongId, SongStats};
    use std::time::Duration;

    /// Apply a sync the way the app does: replace the song set in place so the
    /// library keeps its playlists and id counter, then refresh derived data.
    fn sync_into(
        cache: &mut LibraryCache,
        library: &mut Library,
        roots: &[PathBuf],
        force: bool,
        stats: &mut Stats,
    ) -> SyncReport {
        let (songs, report) = cache.sync(roots, force, stats);
        library.replace_songs(songs);
        library.prune();
        library.rebuild_auto();
        report
    }

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

    /// A minimal song for cache tests.
    fn song(id: SongId, path: &Path) -> SongMetadata {
        SongMetadata { id, path: path.to_path_buf(), song_name: Some("X".into()), ..Default::default() }
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

        let (_, files) = collect_tree(tmp.path());
        let mut files: Vec<String> = files
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
        let (dirs, _) = collect_tree(tmp.path());
        assert_eq!(dirs.len(), 3, "root, a, and a/b");
    }

    #[test]
    fn unchanged_tree_is_skipped_on_the_next_sync() {
        let tmp = TempDir::new("skip");
        tmp.touch("album/track.flac");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();

        let first = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false, &mut Stats::default());
        assert_eq!(first.skipped_roots, 0);
        assert_eq!(first.errors.len(), 1, "the fake file can't be parsed");

        let second = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false, &mut Stats::default());
        assert_eq!(second.skipped_roots, 1, "nothing changed, so nothing walked");
    }

    #[test]
    fn force_rescans_an_otherwise_unchanged_tree() {
        let tmp = TempDir::new("force");
        tmp.touch("track.flac");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();

        sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false, &mut Stats::default());
        let forced = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], true, &mut Stats::default());
        assert_eq!(forced.skipped_roots, 0);
        assert_eq!(forced.errors.len(), 1, "re-read, and failed again");
    }

    #[test]
    fn cache_round_trips_songs() {
        let tmp = TempDir::new("roundtrip");
        let cache_path = tmp.path().join("cache.json");

        let mut library = Library::default();
        library.insert_song(SongMetadata {
            id: SongId(42),
            path: tmp.path().join("x.flac"),
            song_name: Some("X".into()),
            artists: vec!["Grimes".into()],
            album_name: Some("Visions".into()),
            track_position: Some(3),
            date: Some(ReleaseDate::new(2012, Some(1), Some(1))),
            nominal_bitrate: Some(320_000),
            lyrics: Lyrics::Plain("a line".into()),
            duration: Duration::from_secs(200),
            has_art: true,
            ..Default::default()
        });

        let mut cache = LibraryCache::open(cache_path.clone());
        // Seed the per-file entry the way a real scan would.
        cache.entries.insert(
            SongId(42),
            CachedSong::new(library.get(SongId(42)).unwrap().clone(), UNIX_EPOCH),
        );
        cache.save().unwrap();

        let mut reloaded = Library::default();
        LibraryCache::open(cache_path).install(&mut reloaded);
        assert_eq!(reloaded.song_count(), 1);
        assert_eq!(reloaded.get(SongId(42)).unwrap().album(), "Visions");
        let song = reloaded.get(SongId(42)).unwrap();
        assert_eq!(
            song.date,
            Some(ReleaseDate::new(2012, Some(1), Some(1))),
            "the release date survives the cache"
        );
        assert_eq!(
            song.lyrics,
            Lyrics::Plain("a line".into()),
            "lyrics are cached with the rest of the metadata"
        );
    }

    #[test]
    fn playlists_round_trip_in_their_own_file() {
        let tmp = TempDir::new("playlists");
        let cache_path = tmp.path().join("cache.json");

        let mut library = Library::default();
        library.insert_song(song(SongId(42), &tmp.path().join("x.flac")));
        let custom = library.create_custom("Faves");
        library.add_song(custom, SongId(42));

        let cache = LibraryCache::open(cache_path.clone());
        cache.save_playlists(&library).unwrap();

        // The playlists file carries the playlists; the song cache is separate.
        let (playlists, next_playlist_id) = LibraryCache::open(cache_path).load_playlists();
        let mut reloaded = Library::default();
        reloaded.replace_playlists(playlists, next_playlist_id);
        assert_eq!(reloaded.custom_playlists().len(), 1);
        assert_eq!(reloaded.custom_playlists()[0].title(), "Faves");
        assert_eq!(reloaded.songs_of(custom).len(), 0, "songs live in the other file");
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
                ..Default::default()
            };
            cache.entries.insert(SongId(1), CachedSong::new(song(SongId(1), &nested), UNIX_EPOCH));
            cache.entries.insert(SongId(2), CachedSong::new(song(SongId(2), &other), UNIX_EPOCH));
            cache.dirs.insert(a.clone(), UNIX_EPOCH);
            cache.dirs.insert(nested_dir.clone(), UNIX_EPOCH);
            cache.dirs.insert(b.clone(), UNIX_EPOCH);
        };

        // Removing `b` leaves `a`'s subtree — nested directory and all — alone.
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[a.clone()]);
        assert!(cache.entries.contains_key(&SongId(1)), "a/nested is still covered by a");
        assert!(!cache.entries.contains_key(&SongId(2)), "b's song is dropped");
        assert!(cache.dirs.contains_key(&nested_dir), "a's directory stamps survive");

        // Removing `a` instead drops the nested contents, leaving only `b`'s.
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[b.clone()]);
        assert!(!cache.entries.contains_key(&SongId(1)), "a/nested is gone with a");
        assert!(cache.entries.contains_key(&SongId(2)), "b's song remains");
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
    fn stats_round_trip_in_their_own_file() {
        let tmp = TempDir::new("stats");
        let cache_path = tmp.path().join("cache.json");
        let cache = LibraryCache::open(cache_path.clone());

        let mut stats = Stats::default();
        stats.record_play(SongId(42), UNIX_EPOCH);
        stats.ensure_added(SongId(42), UNIX_EPOCH);
        cache.save_stats(&stats).unwrap();

        let loaded = LibraryCache::open(cache_path).load_stats();
        assert_eq!(loaded.get(SongId(42)).plays, 1);
        assert_eq!(loaded.get(SongId(42)).added, Some(UNIX_EPOCH));
        assert_eq!(loaded.get(SongId(7)), SongStats::default(), "unknown songs stay default");
    }

    #[test]
    fn playlists_survive_a_song_cache_version_bump() {
        // A song-schema bump only invalidates the song cache; the user's
        // playlists live in their own file with their own version.
        let tmp = TempDir::new("playlists-independent");
        let cache_path = tmp.path().join("cache.json");
        let cache = LibraryCache::open(cache_path.clone());
        let mut library = Library::default();
        library.insert_song(song(SongId(42), &tmp.path().join("x.flac")));
        let id = library.create_custom("Faves");
        library.add_song(id, SongId(42));
        cache.save_playlists(&library).unwrap();

        // Simulate an old song cache: its version no longer matches.
        fs::write(&cache_path, r#"{"version":1,"dirs":{},"songs":[]}"#).unwrap();
        let reopened = LibraryCache::open(cache_path);
        let (playlists, _) = reopened.load_playlists();
        assert_eq!(playlists.len(), 1, "a cache bump leaves playlists alone");
    }

    #[test]
    fn playlists_version_mismatch_is_treated_as_empty() {
        let tmp = TempDir::new("playlists-version");
        let cache = LibraryCache::open(tmp.path().join("cache.json"));
        fs::write(
            cache.playlists_path(),
            r#"{"version":999,"playlists":[],"next_playlist_id":5}"#,
        )
        .unwrap();
        assert!(cache.load_playlists().0.is_empty());
    }

    #[test]
    fn saving_twice_produces_identical_bytes() {
        let tmp = TempDir::new("deterministic");
        let cache_path = tmp.path().join("cache.json");
        let cache = LibraryCache::open(cache_path);
        let mut library = Library::default();
        library.insert_song(song(SongId(2), &tmp.path().join("b.flac")));
        library.insert_song(song(SongId(1), &tmp.path().join("a.flac")));
        library.rebuild_auto();
        cache.save_playlists(&library).unwrap();
        let first = fs::read(cache.playlists_path()).unwrap();
        cache.save_playlists(&library).unwrap();
        let second = fs::read(cache.playlists_path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn stats_version_mismatch_is_treated_as_empty() {
        let tmp = TempDir::new("stats-version");
        let path = tmp.path().join("stats.json");
        fs::write(&path, r#"{"version":999,"stats":{}}"#).unwrap();
        assert!(LibraryCache::open(tmp.path().join("cache.json")).load_stats().is_empty());
    }

    #[test]
    fn missing_stats_file_is_empty_not_an_error() {
        let tmp = TempDir::new("stats-missing");
        assert!(LibraryCache::open(tmp.path().join("cache.json")).load_stats().is_empty());
    }

    #[test]
    fn sync_seeds_added_from_mtime_on_first_run_and_now_later() {
        let tmp = TempDir::new("seed");
        let path = tmp.touch("track.flac");
        let mtime = source_stamp(&path).unwrap();
        let mut library = Library::default();
        library.insert_song(song(SongId(1), &path));

        // First run: the stats are empty, so the file's mtime is the best
        // guess at when the song entered the library.
        let mut stats = Stats::default();
        seed_added(library.songs(), &mut stats);
        assert_eq!(stats.get(SongId(1)).added, Some(mtime));

        // A song that arrives when stats already exist is stamped "now", not
        // with its (old) mtime — otherwise a copied file would look ancient.
        let mut stats = Stats::default();
        stats.record_play(SongId(999), UNIX_EPOCH); // non-empty => not the first run
        seed_added(library.songs(), &mut stats);
        assert!(stats.get(SongId(1)).added.is_some_and(|t| t > mtime), "later arrivals stamp now");
    }

    #[test]
    fn sync_rebuilds_autogen_playlists() {
        let tmp = TempDir::new("autogen");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();
        // Seed a cached song by hand (a real scan needs real audio).
        library.insert_song(SongMetadata {
            id: SongId(7),
            path: tmp.path().join("y.flac"),
            song_name: Some("Y".into()),
            artists: vec!["Grimes".into()],
            album_name: Some("Visions".into()),
            track_position: Some(1),
            ..Default::default()
        });
        cache.entries.insert(
            SongId(7),
            CachedSong::new(library.get(SongId(7)).unwrap().clone(), UNIX_EPOCH),
        );

        // A sync with no roots still rebuilds from whatever is cached.
        sync_into(&mut cache, &mut library, &[], false, &mut Stats::default());
        let auto = library.playlists();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].title(), "Visions");
        assert_eq!(auto[0].song_ids, vec![SongId(7)]);
    }
}
