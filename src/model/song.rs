use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};

use super::cover::CoverState;
use super::identity::{generate_song_id, SongId};
use super::lyrics::{load_sidecar, looks_like_lrc, parse_lrc, Lyrics};

/// What's *declared about the file*, loaded once via `lofty`.
///
/// Static and per-file: title, artists, album, track position, duration,
/// nominal bitrate, embedded lyrics, and cover state. The live playback
/// telemetry (moving bitrate, elapsed time) lives in
/// [`StreamingInfo`](super::streaming::StreamingInfo) instead — a
/// different concern with a different lifetime.
#[derive(Debug, Clone)]
pub struct SongMetadata {
    pub id: SongId,
    pub path: PathBuf,
    pub song_name: Option<String>,
    pub artists: Vec<String>,
    pub album_name: Option<String>,
    /// The album artist tag (`TPE2`/`aART`), when present. Distinct from
    /// `artists`: a compilation's tracks each credit a different artist, but
    /// share one album artist — which is what keeps the album a single
    /// playlist instead of splitting it per track artist.
    pub album_artist: Option<String>,
    pub track_position: Option<u16>,
    /// Release year, when the tags declare one (`Year`, `RecordingDate`, or
    /// `ReleaseDate`). Drives the browse header's year and date filters.
    pub year: Option<u16>,
    /// Declared/average bitrate from the file's properties, in **bits per
    /// second**.
    ///
    /// `lofty` reports kbps; it's normalized to bps here so it's directly
    /// comparable with the *live* moving bitrate (also bps), which is tracked
    /// separately — this is a static file property, that's per-second
    /// telemetry, especially on VBR Opus/MP3.
    pub nominal_bitrate: Option<u32>,
    pub lyrics: Lyrics,
    pub duration: Duration,
    pub cover: CoverState,
}

impl SongMetadata {
    /// A stand-in for "nothing loaded yet".
    ///
    /// Lets the player exist before a track is chosen (e.g. the app launched
    /// with only a directory to scan), so the UI always has something to
    /// render instead of an `Option` threaded through every view.
    pub fn placeholder() -> Self {
        Self {
            id: 0,
            path: PathBuf::new(),
            song_name: None,
            artists: Vec::new(),
            album_name: None,
            album_artist: None,
            track_position: None,
            year: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            cover: CoverState::Missing,
        }
    }

    /// Read tags and properties for `path`.
    ///
    /// Succeeds for untagged files too — they're still playable songs, just
    /// without a title. An `Err` means the file couldn't be opened or isn't
    /// a container `lofty` understands, not merely "has no tags".
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        // Re-guess from the file's actual bytes: `Probe::open` guesses from
        // the extension, and `.ogg` files may hold Opus (lofty would then
        // fail with "Vorbis: File missing magic signature").
        let tagged_file = Probe::open(path)?.guess_file_type()?.read()?;
        let id = generate_song_id(path)?;
        let properties = tagged_file.properties();
        let duration = properties.duration();
        // lofty reports kbps; store bps so it matches the live bitrate's units.
        let nominal_bitrate = properties.audio_bitrate().map(|kbps| kbps.saturating_mul(1000));

        // Read the `.lrc` sidecar up front so both branches below can use it.
        // A sidecar wins over embedded lyrics: it's the only source that
        // reliably carries timestamps.
        let sidecar = load_sidecar(path);

        let Some(tag) = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())
        else {
            // No tag block at all: still a playable song, just untagged.
            return Ok(Self {
                id,
                path: path.to_path_buf(),
                song_name: None,
                artists: Vec::new(),
                album_name: None,
                album_artist: None,
                track_position: None,
                year: None,
                nominal_bitrate,
                lyrics: sidecar.unwrap_or(Lyrics::None),
                duration,
                cover: CoverState::NotRequested,
            });
        };

        let song_name = tag.title().map(|s| s.into_owned());
        let artists: Vec<String> = tag
            .get_items(&ItemKey::TrackArtist)
            .filter_map(|item| item.value().text().map(String::from))
            .collect();
        let album_name = tag.album().map(|s| s.into_owned());
        let album_artist = tag.get_string(&ItemKey::AlbumArtist).map(|s| s.to_string());
        let track_position = tag.track().map(|n| n as u16);
        let year = read_year(tag);

        let lyrics = sidecar.unwrap_or_else(|| {
            tag.get_string(&ItemKey::Lyrics)
                .map(|s| {
                    if looks_like_lrc(s) {
                        Lyrics::Synced(parse_lrc(s))
                    } else {
                        Lyrics::Plain(s.to_string())
                    }
                })
                .unwrap_or(Lyrics::None)
        });

        let cover = if tag.pictures().first().is_some() {
            // Decoding/downscaling happens off this call — this just
            // records that art is available and awaiting async load.
            CoverState::NotRequested
        } else {
            CoverState::Missing
        };

        Ok(Self {
            id,
            path: path.to_path_buf(),
            song_name,
            artists,
            album_name,
            album_artist,
            track_position,
            year,
            nominal_bitrate,
            lyrics,
            duration,
            cover,
        })
    }

    /// Album title, or a stable "Unknown Album" bucket for untagged files.
    ///
    /// Autogen album playlists key on this, so untagged songs land in a
    /// bucket instead of being dropped from the library view.
    pub fn album(&self) -> &str {
        self.album_name.as_deref().unwrap_or("Unknown Album")
    }

    /// The album artist, falling back to the first track artist.
    ///
    /// A compilation's tracks each credit a different artist, but share one
    /// album artist — using this (not the track artist) to key autogen
    /// playlists keeps the album in one piece.
    pub fn album_artist(&self) -> &str {
        self.album_artist
            .as_deref()
            .or_else(|| self.artists.first().map(String::as_str))
            .unwrap_or("Unknown Artist")
    }

    /// First credited track artist, or "Unknown Artist".
    pub fn primary_artist(&self) -> &str {
        self.artists.first().map(String::as_str).unwrap_or("Unknown Artist")
    }

    /// The title to show for this song: the tag's title, or the file's stem
    /// when it's untagged. Every list that names a song uses this, so a
    /// missing tag degrades to the filename instead of a blank row.
    pub fn display_title(&self) -> String {
        self.song_name.clone().unwrap_or_else(|| {
            self.path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_string())
        })
    }

    /// Whether the file declares embedded art, i.e. whether decoding it is
    /// worth attempting at all.
    pub fn has_art(&self) -> bool {
        !matches!(self.cover, CoverState::Missing)
    }
}

/// The year a tag declares, trying the three keys that carry one.
///
/// `Year` is the Vorbis-comment key (and ID3v2.3 `TYER`); `RecordingDate`
/// (`TDRC`) and `ReleaseDate` (`TDRL`) are ISO-ish dates like `2003-05-01`,
/// which is why [`parse_year`] takes the leading four digits.
fn read_year(tag: &Tag) -> Option<u16> {
    [ItemKey::Year, ItemKey::RecordingDate, ItemKey::ReleaseDate]
        .iter()
        .find_map(|key| tag.get_string(key).and_then(parse_year))
}

/// The leading four digits of a date-ish string, as a year.
fn parse_year(s: &str) -> Option<u16> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).take(4).collect();
    (digits.len() == 4).then(|| digits.parse().ok()).flatten()
}
