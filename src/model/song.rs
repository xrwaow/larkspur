use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use serde::{Deserialize, Serialize};

use super::identity::{generate_song_id, SongId};
use super::lyrics::{load_sidecar, looks_like_lrc, parse_lrc, Lyrics};

/// What's *declared about the file*, loaded once via `lofty`.
///
/// Static and per-file: title, artists, album, track position, duration,
/// nominal bitrate, embedded lyrics, and cover state. The live playback
/// telemetry (moving bitrate, elapsed time) is a separate concern with a
/// different lifetime and lives in the UI's playback state instead.
///
/// Serialize/Deserialize is derived so the scan cache can round-trip a song
/// whole rather than mirroring every field. Decoded pixels aren't part of the
/// metadata — the cover cache owns those — only *whether* art exists is.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// The full release date — year, month, day — when the tag carries one
    /// (`2003-05-01`). What the library's chronological ordering uses: songs
    /// of the same year order by month/day rather than alphabetically.
    #[serde(default)]
    pub release_date: Option<(u16, u8, u8)>,
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
    /// Whether the file declares embedded art, i.e. whether decoding it is
    /// worth attempting at all.
    pub has_art: bool,
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
            release_date: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            has_art: false,
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
                release_date: None,
                nominal_bitrate: None,
                lyrics: sidecar.unwrap_or(Lyrics::None),
                duration,
                has_art: false,
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
        let (year, release_date) = read_date(tag);

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

        // Decoding/downscaling happens off this call — this just records
        // that art is available and awaiting async load.
        let has_art = tag.pictures().first().is_some();

        Ok(Self {
            id,
            path: path.to_path_buf(),
            song_name,
            artists,
            album_name,
            album_artist,
            track_position,
            year,
            release_date,
            nominal_bitrate,
            lyrics,
            duration,
            has_art,
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

}

/// The date a tag declares — the year, plus the month and day when the value
/// carries them — trying the three keys that carry one.
///
/// `Year` is the Vorbis-comment key (and ID3v2.3 `TYER`); `RecordingDate`
/// (`TDRC`) and `ReleaseDate` (`TDRL`) are ISO-ish dates like `2003-05-01`,
/// which is why only the leading numbers are read: `2003-05-01 12:00` still
/// yields May 1st, and a bare `2003` yields the year alone.
fn read_date(tag: &Tag) -> (Option<u16>, Option<(u16, u8, u8)>) {
    [ItemKey::Year, ItemKey::RecordingDate, ItemKey::ReleaseDate]
        .iter()
        .find_map(|key| tag.get_string(key).map(parse_date))
        .unwrap_or((None, None))
}

/// Parse a date-ish tag value into `(year, (year, month, day))`.
///
/// The leading number is the year; a `-`/`/`-separated month and day follow
/// when present. A compact `20030501` (no separators) is split by position.
/// Anything that doesn't fit degrades: a value with a month but no day keeps
/// the year and drops the rest.
fn parse_date(s: &str) -> (Option<u16>, Option<(u16, u8, u8)>) {
    let trimmed = s.trim();
    let mut numbers: Vec<u32> = trimmed
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();

    // `20030501`: one unseparated run of eight digits is year/month/day.
    if numbers.len() == 1 && trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let n = numbers[0];
        numbers = vec![n / 10_000, (n / 100) % 100, n % 100];
    }

    let year = numbers.first().copied().filter(|y| (1000..=9999).contains(y)).map(|y| y as u16);
    let month = numbers.get(1).copied().filter(|m| (1..=12).contains(m)).map(|m| m as u8);
    let day = numbers.get(2).copied().filter(|d| (1..=31).contains(d)).map(|d| d as u8);
    let date = match (year, month, day) {
        (Some(y), Some(m), Some(d)) => Some((y, m, d)),
        _ => None,
    };
    (year, date)
}

#[cfg(test)]
mod tests {
    use super::parse_date;

    #[test]
    fn parse_date_reads_the_year_and_the_month_day_when_present() {
        assert_eq!(parse_date("2003"), (Some(2003), None));
        assert_eq!(parse_date("2003-05-01"), (Some(2003), Some((2003, 5, 1))));
        assert_eq!(parse_date("2003/5/1"), (Some(2003), Some((2003, 5, 1))));
        // A timestamp after the date is ignored.
        assert_eq!(parse_date("2003-05-01 12:00"), (Some(2003), Some((2003, 5, 1))));
        // A compact `YYYYMMDD` has no separators to split on.
        assert_eq!(parse_date("20030501"), (Some(2003), Some((2003, 5, 1))));
        // A month without a day keeps the year only.
        assert_eq!(parse_date("2003-05"), (Some(2003), None));
        // Out-of-range or non-date values degrade to nothing.
        assert_eq!(parse_date("13"), (None, None));
        assert_eq!(parse_date("2003-13-01"), (Some(2003), None));
        assert_eq!(parse_date(""), (None, None));
    }
}
