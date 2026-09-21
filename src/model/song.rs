use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};

use super::cover::CoverState;
use super::identity::{generate_song_id, SongId};
use super::lyrics::{looks_like_lrc, parse_lrc, Lyrics};

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
    pub track_position: Option<u16>,
    /// Declared/average bitrate from the file's properties.
    ///
    /// The *live* moving bitrate during playback is tracked separately in
    /// [`StreamingInfo`](super::streaming::StreamingInfo) — this is a
    /// static file property, that's per-second telemetry, especially on
    /// VBR Opus/MP3.
    pub nominal_bitrate: Option<u32>,
    pub lyrics: Lyrics,
    pub duration: Duration,
    pub cover: CoverState,
}

impl SongMetadata {
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
        let nominal_bitrate = properties.audio_bitrate();

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
                track_position: None,
                nominal_bitrate,
                lyrics: Lyrics::None,
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

        Ok(Self {
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
        })
    }
}
