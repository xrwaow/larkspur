use std::{
    collections::HashMap,
    hash::Hasher,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::Picture;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};
use twox_hash::XxHash64;

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// Stable song identity, derived from the canonicalized file path.
///
/// NOTE: intentionally NOT std::hash::DefaultHasher (SipHash) — that
/// algorithm is explicitly unstable across Rust versions, which would
/// silently invalidate a persisted library/cover cache on toolchain
/// upgrades. XxHash64 is fast and has a fixed, documented algorithm.
pub type SongId = u64;

fn generate_song_id(path: &Path) -> anyhow::Result<SongId> {
    let canonical = path.canonicalize()?;
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(canonical.to_string_lossy().as_bytes());
    Ok(hasher.finish())
}

// ---------------------------------------------------------------------
// Playback status
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongStatus {
    NoSelection,
    Playing,
    Paused,
}

// ---------------------------------------------------------------------
// Input handling (placeholder — decoupled from any specific UI/input
// backend so it can be mapped from crossterm::KeyCode *or* GPUI's key
// events later, rather than being tied to a terminal-only type)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputAction {
    Quit,
    NextTrack,
    PrevTrack,
    TogglePause,
    VolumeUp,
    VolumeDown,
    FocusLeftPane,
    FocusRightPane,
}

// ---------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LyricLine {
    pub timestamp: Duration,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum Lyrics {
    None,
    Plain(String),
    Synced(Vec<LyricLine>),
}

/// Detects whether a string contains LRC-style timestamp tags
/// (e.g. `[01:23.45]`) anywhere in it — used because taggers commonly
/// embed full synced lyrics directly in the Lyrics/USLT tag itself,
/// not just in an external .lrc sidecar.
fn looks_like_lrc(s: &str) -> bool {
    let mut i = 0;
    while let Some(start) = s[i..].find('[') {
        let abs_start = i + start;
        if let Some(rel_end) = s[abs_start..].find(']') {
            let tag = &s[abs_start + 1..abs_start + rel_end];
            if parse_timestamp_tag(tag).is_some() {
                return true;
            }
            i = abs_start + rel_end + 1;
        } else {
            break;
        }
    }
    false
}

fn parse_timestamp_tag(tag: &str) -> Option<Duration> {
    let mut parts = tag.splitn(2, ':');
    let min: u64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(Duration::from_secs_f64(min as f64 * 60.0 + sec))
}

/// Parses LRC-format lyrics text into synced lines. Scans for
/// `[...]` tags directly rather than splitting on lines first, since
/// embedded (non-sidecar) LRC content isn't guaranteed to use real
/// newlines as separators between timestamp entries. Metadata tags
/// like `[ti:...]`/`[ar:...]`/`[by:...]` are skipped automatically —
/// they don't parse as `mm:ss.xx` timestamps.
pub fn parse_lrc(contents: &str) -> Vec<LyricLine> {
    let mut tags = Vec::new();
    let mut i = 0;
    while let Some(start) = contents[i..].find('[') {
        let abs_start = i + start;
        let Some(rel_end) = contents[abs_start..].find(']') else { break };
        let abs_end = abs_start + rel_end;
        tags.push((abs_start, abs_end + 1, &contents[abs_start + 1..abs_end]));
        i = abs_end + 1;
    }

    let mut lines = Vec::new();
    for (idx, &(_, end, tag)) in tags.iter().enumerate() {
        let Some(timestamp) = parse_timestamp_tag(tag) else { continue };
        let text_end = tags.get(idx + 1).map(|(s, _, _)| *s).unwrap_or(contents.len());
        let text = contents[end..text_end]
            .trim()
            .trim_matches('/')
            .trim()
            .to_string();
        if !text.is_empty() {
            lines.push(LyricLine { timestamp, text });
        }
    }

    lines.sort_by_key(|l| l.timestamp);
    lines
}

// ---------------------------------------------------------------------
// Cover art
// ---------------------------------------------------------------------

/// Decoded, already-downscaled RGBA pixel buffer ready for GPU upload.
/// Kept deliberately minimal/framework-agnostic — GPUI's own asset
/// system owns the actual texture upload.
#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum CoverState {
    NotRequested,
    Loading,
    Ready(Arc<DecodedImage>),
    Missing,
}

/// Two-tier cover cache: bounded in-memory LRU (decoded pixels) +
/// a persistent on-disk thumbnail cache keyed by SongId/album id.
pub struct CoverCache {
    memory: lru::LruCache<u64, Arc<DecodedImage>>,
    memory_bytes: usize,
    memory_budget_bytes: usize,
    disk_dir: PathBuf,
}

impl CoverCache {
    pub fn new(disk_dir: PathBuf, memory_budget_bytes: usize) -> Self {
        std::fs::create_dir_all(&disk_dir).ok();
        Self {
            // Capacity is a formality here — we evict by byte budget, not count.
            memory: lru::LruCache::unbounded(),
            memory_bytes: 0,
            memory_budget_bytes,
            disk_dir,
        }
    }

    pub fn get(&mut self, id: u64) -> Option<Arc<DecodedImage>> {
        self.memory.get(&id).cloned()
    }

    pub fn insert(&mut self, id: u64, image: Arc<DecodedImage>) {
        let size = image.rgba.len();
        self.memory.put(id, image);
        self.memory_bytes += size;

        while self.memory_bytes > self.memory_budget_bytes {
            match self.memory.pop_lru() {
                Some((_, evicted)) => self.memory_bytes -= evicted.rgba.len(),
                None => break,
            }
        }
    }

    pub fn disk_path(&self, id: u64) -> PathBuf {
        self.disk_dir.join(format!("{id:016x}.jpg"))
    }

    /// Placeholder for the folder.jpg/cover.png fallback lookup —
    /// lofty only reads embedded pictures, so external cover files
    /// need this separate, album-level path.
    pub fn find_external_cover(album_dir: &Path) -> Option<PathBuf> {
        const CANDIDATES: &[&str] = &["cover.jpg", "cover.png", "folder.jpg", "folder.png"];
        CANDIDATES
            .iter()
            .map(|name| album_dir.join(name))
            .find(|p| p.exists())
    }
}

// ---------------------------------------------------------------------
// Song metadata (static, per-file — NOT live playback telemetry)
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SongMetadata {
    pub id: SongId,
    pub path: PathBuf,
    pub song_name: Option<String>,
    pub artists: Vec<String>,
    pub album_name: Option<String>,
    pub track_position: Option<u16>,
    /// Declared/average bitrate from the file's properties.
    /// The *live* moving bitrate during playback is tracked separately
    /// in StreamingInfo — these are different concerns with different
    /// lifetimes (static file property vs. per-second telemetry).
    pub nominal_bitrate: Option<u32>,
    pub lyrics: Lyrics,
    pub duration: Duration,
    pub cover: CoverState,
}

impl SongMetadata {
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        // Re-guess from the file's actual bytes: Probe::open guesses from
        // the extension, and .ogg files may hold Opus (lofty would then
        // fail with "Vorbis: File missing magic signature").
        let tagged_file = Probe::open(path)?.guess_file_type()?.read()?;
        let id = generate_song_id(path)?;
        let properties = tagged_file.properties();
        let duration = properties.duration();
        let nominal_bitrate = properties.audio_bitrate();

        let Some(tag) = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())
        else {
            // No tag block at all: still a playable song, just untagged.
            return Ok(Some(Self {
                id,
                path: path.to_path_buf(),
                song_name: None,
                artists: Vec::new(),
                album_name: None,
                track_position: None,
                nominal_bitrate,
                lyrics: Lyrics::None,
                duration,
                cover: CoverState::NotRequested,
            }));
        };

        let song_name = tag.title().map(|s| s.into_owned());
        let artists: Vec<String> = tag
            .get_items(&ItemKey::TrackArtist)
            .filter_map(|item| item.value().text().map(String::from))
            .collect();
        let album_name = tag.album().map(|s| s.into_owned());
        let track_position = tag.track().map(|n| n as u16);

        let lyrics = tag
            .get_string(&ItemKey::Lyrics)
            .map(|s| {
                if looks_like_lrc(s) {
                    Lyrics::Synced(parse_lrc(s))
                } else {
                    Lyrics::Plain(s.to_string())
                }
            })
            .unwrap_or(Lyrics::None);

        let cover = if tag.pictures().first().is_some() {
            // Decoding/downscaling happens off this call — this just
            // records that art is available and awaiting async load.
            CoverState::NotRequested
        } else {
            CoverState::Missing
        };

        Ok(Some(Self {
            id,
            path: path.to_path_buf(),
            song_name,
            artists,
            album_name,
            track_position,
            nominal_bitrate,
            lyrics,
            duration,
            cover,
        }))
    }

    pub fn embedded_picture<'a>(tag: &'a lofty::tag::Tag) -> Option<&'a Picture> {
        tag.pictures().first()
    }
}

// ---------------------------------------------------------------------
// Library — single source of truth; everything else references by id
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Album {
    pub name: Option<String>,
    pub total_songs: Option<u16>,
    pub song_ids: Vec<SongId>,
}

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

// ---------------------------------------------------------------------
// Live playback state — updated every frame/packet, not per-file
// ---------------------------------------------------------------------

pub struct StreamingInfo {
    pub status: SongStatus,
    /// Reference into Library::songs, not an owned copy — avoids the
    /// metadata drifting out of sync with the library's copy.
    pub current_song: Option<SongId>,
    pub elapsed: Duration,
    /// Rolling live bitrate, computed from decoded packet sizes.
    pub live_bitrate_bps: Option<u32>,
}

impl Default for StreamingInfo {
    fn default() -> Self {
        Self {
            status: SongStatus::NoSelection,
            current_song: None,
            elapsed: Duration::ZERO,
            live_bitrate_bps: None,
        }
    }
}

// ---------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------

pub struct Config {
    pub keybinds: HashMap<InputAction, String>, // placeholder value type until input backend is chosen
}

impl Config {
    pub const SONG_EXTENSIONS: &'static [&'static str] = &["flac", "mp3", "opus", "ogg"];

    pub fn is_supported(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| Self::SONG_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}
