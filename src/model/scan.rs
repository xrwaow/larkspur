//! Directory scanning and mtime-based incremental sync.
//!
//! The library is built from a list of directories. Rather than re-reading
//! every file's tags on each launch, the cache records a per-directory mtime
//! stamp: if nothing in the tree changed, the scan is skipped entirely.
//! Within a changed tree, only files whose mtime moved are re-parsed.
//!
//! Identity is assigned here, not derived from the path. The scanner reuses a
//! path's existing id, recognises a moved file by its tag fingerprint, and
//! allocates a new id otherwise. That map is persisted separately from this
//! disposable metadata cache (see [`SongIdentities`]).
//!
//! What's persisted here is metadata only — never audio (decoded at play time)
//! and never pixels (the cover cache owns those).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::format::AudioFormat;
use super::identity::SongId;
use super::library::Library;
use super::lyrics;
use super::persist::{read_versioned, write_versioned};
use super::song::{SongFingerprint, SongMetadata};
use super::user::SongIdentities;

/// What a sync pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Files parsed (new or changed).
    pub parsed: usize,
    /// Files whose cached metadata was reused untouched.
    pub reused: usize,
    /// Cached songs dropped because the file disappeared.
    pub removed: usize,
    /// Files recognised as a moved copy of a cached song.
    pub moved: usize,
    /// Roots skipped entirely because nothing in the tree changed.
    pub skipped_roots: usize,
    /// Files that couldn't be read (kept out of the library, not fatal).
    pub errors: Vec<(PathBuf, String)>,
}

/// Everything one scan produces. The caller applies it to the live library,
/// which owns the user data and the path→id map.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub songs: HashMap<SongId, Arc<SongMetadata>>,
    pub identities: SongIdentities,
    /// New-to-the-cache songs with their file mtime, so the library can stamp
    /// `added` without touching the filesystem again.
    pub added: Vec<(SongId, SystemTime)>,
    pub report: SyncReport,
}

/// One cached song: the metadata exactly as loaded, plus the one cache-only
/// bit — when the file was last read, which is what an incremental sync
/// compares against.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSong {
    song: Arc<SongMetadata>,
    modified: SystemTime,
}

impl CachedSong {
    fn new(song: SongMetadata, modified: SystemTime) -> Self {
        Self { song: Arc::new(song), modified }
    }

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

/// Bumped whenever the cached song shape changes.
///
/// v9: identity is assigned and persisted separately, `AudioFormat` is stored
/// with the song, and playlists/stats/identity moved to the user store.
const CACHE_VERSION: u32 = 9;

/// Persisted library state: where we scanned, per-directory stamps, and the
/// cached song metadata.
pub struct LibraryCache {
    path: PathBuf,
    dirs: HashMap<PathBuf, SystemTime>,
    entries: HashMap<SongId, CachedSong>,
}

impl LibraryCache {
    /// Open the cache at `path`, loading it if present.
    pub fn open(path: PathBuf) -> Self {
        let loaded = read_versioned::<CacheFile>(&path, CACHE_VERSION);
        match loaded {
            Some(cache) => Self {
                path,
                dirs: cache.dirs.into_iter().collect(),
                entries: cache.songs.into_iter().map(|s| (s.song.id, s)).collect(),
            },
            None => Self { path, dirs: HashMap::new(), entries: HashMap::new() },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Populate `library`'s songs from the cache without touching the
    /// filesystem — the instant-start path before a sync.
    pub fn install(&self, library: &mut Library) {
        let songs = self.entries.values().map(|entry| (entry.song.id, entry.to_song())).collect();
        library.replace_songs(songs);
    }

    /// Bring `library` up to date with `roots` on disk, reusing cached
    /// metadata for anything unchanged.
    ///
    /// `identities` is the durable path→id map. The returned map is the new
    /// one, with entries for disappeared paths removed and moved files
    /// re-pointed at their preserved id.
    pub fn sync(
        &mut self,
        roots: &[PathBuf],
        force: bool,
        identities: &SongIdentities,
    ) -> ScanResult {
        let mut report = SyncReport::default();
        let roots = normalize_roots(roots);

        // One walk for the whole root set: directories and candidate files.
        let mut current_dirs: HashMap<PathBuf, SystemTime> = HashMap::new();
        let mut file_set: HashSet<PathBuf> = HashSet::new();
        for root in &roots {
            let (dirs, files) = collect_tree(root);
            current_dirs.extend(dirs);
            file_set.extend(files);
        }
        let mut files: Vec<PathBuf> = file_set.into_iter().collect();
        files.sort();
        let current_paths: HashSet<PathBuf> = files.iter().cloned().collect();

        // The fast-path report is diagnostic only: the walk already happened,
        // but unchanged roots still skip per-file reparsing below via mtimes.
        for root in &roots {
            let current: Vec<(&PathBuf, &SystemTime)> =
                current_dirs.iter().filter(|(dir, _)| dir.starts_with(root)).collect();
            let known = self.dirs.keys().filter(|dir| dir.starts_with(root)).count();
            let unchanged = !force
                && !current.is_empty()
                && current.len() == known
                && current.iter().all(|(dir, stamp)| self.dirs.get(*dir) == Some(*stamp));
            if unchanged {
                report.skipped_roots += 1;
            }
        }

        // Cached entries by path, and fingerprints of songs that vanished, so
        // a moved file can reclaim its previous id.
        let cached_by_path: HashMap<PathBuf, (SongId, SystemTime)> = self
            .entries
            .iter()
            .map(|(id, entry)| (entry.song.path.clone(), (*id, entry.modified)))
            .collect();
        let mut missing_fingerprints: HashMap<SongFingerprint, Vec<SongId>> = HashMap::new();
        for (id, entry) in &self.entries {
            if !current_paths.contains(&entry.song.path) {
                missing_fingerprints.entry(entry.song.fingerprint()).or_default().push(*id);
            }
        }
        // Only unambiguous fingerprints are safe to reclaim.
        let unique_missing: HashMap<SongFingerprint, SongId> = missing_fingerprints
            .into_iter()
            .filter_map(|(fingerprint, ids)| (ids.len() == 1).then_some((fingerprint, ids[0])))
            .collect();

        let mut next_id = identities.next_id;
        for id in self.entries.keys().chain(identities.paths.values()) {
            next_id = next_id.max(id.0.saturating_add(1));
        }

        let mut new_entries: HashMap<SongId, CachedSong> = HashMap::new();
        let mut new_paths: BTreeMap<PathBuf, SongId> = BTreeMap::new();
        let mut added: Vec<(SongId, SystemTime)> = Vec::new();
        let mut used_ids: HashSet<SongId> = HashSet::new();

        for file in files {
            let cached = cached_by_path.get(&file).copied();
            let mtime = source_stamp(&file);

            // Unchanged file: reuse the cached metadata untouched.
            if !force {
                if let Some((id, cached_mtime)) = cached {
                    if Some(cached_mtime) == mtime {
                        if let Some(entry) = self.entries.get(&id) {
                            report.reused += 1;
                            new_entries.insert(id, entry.clone());
                            new_paths.insert(file, id);
                            used_ids.insert(id);
                            continue;
                        }
                    }
                }
            }

            match SongMetadata::read(&file) {
                Ok(mut song) => {
                    let fingerprint = song.fingerprint();
                    let id = if let Some((id, _)) = cached {
                        id
                    } else if let Some(id) = identities.paths.get(&file).copied() {
                        id
                    } else if let Some(id) =
                        unique_missing.get(&fingerprint).copied().filter(|id| !used_ids.contains(id))
                    {
                        report.moved += 1;
                        id
                    } else {
                        let id = SongId(next_id);
                        next_id = next_id.saturating_add(1);
                        id
                    };
                    song.id = id;
                    if !used_ids.contains(&id) && !new_entries.contains_key(&id) {
                        added.push((id, mtime.unwrap_or(UNIX_EPOCH)));
                    }
                    new_entries.insert(id, CachedSong::new(song, mtime.unwrap_or(UNIX_EPOCH)));
                    new_paths.insert(file, id);
                    used_ids.insert(id);
                    report.parsed += 1;
                }
                Err(error) => {
                    // A transient read failure must not drop a cached song.
                    if let Some((id, _)) = cached {
                        if let Some(entry) = self.entries.get(&id) {
                            new_entries.insert(id, entry.clone());
                            new_paths.insert(file.clone(), id);
                        }
                    }
                    report.errors.push((file, error.to_string()));
                }
            }
        }

        report.removed = self.entries.len().saturating_sub(new_entries.len());
        let songs: HashMap<SongId, Arc<SongMetadata>> =
            new_entries.iter().map(|(id, entry)| (*id, entry.to_song())).collect();
        self.entries = new_entries;
        self.dirs = current_dirs;

        ScanResult {
            songs,
            identities: SongIdentities { paths: new_paths, next_id },
            added,
            report,
        }
    }

    /// Replace one cached song and refresh its modification stamp.
    ///
    /// Used when a single entry changes outside a scan — accepting downloaded
    /// lyrics embeds them in the audio file, whose new mtime is what the next
    /// incremental sync compares against. Without the refreshed stamp that
    /// sync would re-parse the file it already has.
    pub fn update_song(&mut self, song: Arc<SongMetadata>) {
        let modified = source_stamp(&song.path).unwrap_or_else(SystemTime::now);
        self.entries.insert(song.id, CachedSong { song, modified });
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

    /// Forget cached songs and directory stamps that no longer fall under any
    /// of `roots`.
    pub fn retain_roots(&mut self, roots: &[PathBuf]) {
        let normalized = normalize_roots(roots);
        let covered = |path: &Path| normalized.iter().any(|root| path.starts_with(root));
        self.entries.retain(|_, entry| covered(&entry.song.path));
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
        if !result.iter().any(|parent| path.starts_with(parent)) {
            result.push(path);
        }
    }
    result
}

/// Recursively walk `root` once, returning every directory's mtime and every
/// supported audio file.
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
                let target = path.canonicalize().unwrap_or(path);
                if target.is_dir() {
                    stack.push(target);
                } else if AudioFormat::from_path(&target).is_some() {
                    files.push(target);
                }
            } else if file_type.is_dir() {
                stack.push(path);
            } else if AudioFormat::from_path(&path).is_some() {
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
/// editing a sidecar re-parses the song instead of leaving stale lyrics.
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
    use crate::model::{Lyrics, ReleaseDate, SongStats, UserStore};
    use std::time::Duration;

    fn sync_into(
        cache: &mut LibraryCache,
        library: &mut Library,
        roots: &[PathBuf],
        force: bool,
    ) -> SyncReport {
        let identities = library.identities();
        let result = cache.sync(roots, force, &identities);
        let report = result.report.clone();
        library.adopt_scan(result.songs, result.identities, result.added);
        report
    }

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
        let mut files: Vec<String> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
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
        let first = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false);
        assert_eq!(first.skipped_roots, 0);
        assert_eq!(first.errors.len(), 1, "the fake file can't be parsed");
        let second = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false);
        assert_eq!(second.skipped_roots, 1);
    }

    #[test]
    fn force_rescans_an_otherwise_unchanged_tree() {
        let tmp = TempDir::new("force");
        tmp.touch("track.flac");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();
        sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], false);
        let forced = sync_into(&mut cache, &mut library, &[tmp.path().to_path_buf()], true);
        assert_eq!(forced.skipped_roots, 0);
        assert_eq!(forced.errors.len(), 1);
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
        cache.entries.insert(
            SongId(42),
            CachedSong::new(library.get(SongId(42)).unwrap().clone(), UNIX_EPOCH),
        );
        cache.save().unwrap();

        let mut reloaded = Library::default();
        LibraryCache::open(cache_path).install(&mut reloaded);
        assert_eq!(reloaded.song_count(), 1);
        assert_eq!(reloaded.get(SongId(42)).unwrap().album(), "Visions");
        assert_eq!(
            reloaded.get(SongId(42)).unwrap().lyrics,
            Lyrics::Plain("a line".into())
        );
    }

    #[test]
    fn retain_roots_only_drops_what_no_remaining_root_covers() {
        let tmp = TempDir::new("retain");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let nested_dir = a.join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::create_dir_all(&b).unwrap();
        let nested = nested_dir.join("x.flac");
        let other = b.join("y.flac");

        let seed = |cache: &mut LibraryCache| {
            cache.entries.insert(SongId(1), CachedSong::new(song(SongId(1), &nested), UNIX_EPOCH));
            cache.entries.insert(SongId(2), CachedSong::new(song(SongId(2), &other), UNIX_EPOCH));
            cache.dirs.insert(a.clone(), UNIX_EPOCH);
            cache.dirs.insert(nested_dir.clone(), UNIX_EPOCH);
            cache.dirs.insert(b.clone(), UNIX_EPOCH);
        };

        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[a.clone()]);
        assert!(cache.entries.contains_key(&SongId(1)));
        assert!(!cache.entries.contains_key(&SongId(2)));

        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        seed(&mut cache);
        cache.retain_roots(&[b.clone()]);
        assert!(!cache.entries.contains_key(&SongId(1)));
        assert!(cache.entries.contains_key(&SongId(2)));
    }

    #[test]
    fn version_mismatch_is_treated_as_empty() {
        let tmp = TempDir::new("version");
        let cache_path = tmp.path().join("cache.json");
        fs::write(
            &cache_path,
            r#"{"version":999,"dirs":{},"songs":[],"playlists":[],"next_playlist_id":5}"#,
        )
        .unwrap();
        let mut library = Library::default();
        LibraryCache::open(cache_path).install(&mut library);
        assert_eq!(library.song_count(), 0);
        assert!(library.playlists().is_empty());
    }

    #[test]
    fn missing_stats_file_is_empty_not_an_error() {
        let tmp = TempDir::new("stats-missing");
        let store = UserStore::open(&tmp.path().join("cache.json"));
        assert_eq!(store.load_user().stats.get(SongId(1)), SongStats::default());
    }

    #[test]
    fn cached_songs_rebuild_autogen_playlists() {
        let tmp = TempDir::new("autogen");
        let mut cache = LibraryCache::open(tmp.path().join("cache.json"));
        let mut library = Library::default();
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
        let mut installed = Library::default();
        cache.install(&mut installed);
        installed.rebuild_auto();
        let auto = installed.playlists();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].title(), "Visions");
        assert_eq!(auto[0].song_ids, vec![SongId(7)]);
    }
}
