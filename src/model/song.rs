use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use serde::{Deserialize, Serialize};

use super::identity::{generate_song_id, SongId};
use super::lyrics::{load_sidecar, looks_like_lrc, parse_lrc, Lyrics};

/// A release date packed as `YYYYMMDD` (e.g. `20230501`), with zeros for
/// unknown components: `20030000` is year-only, `20030500` is year + month.
///
/// One integer field, most-significant-first, so chronological ordering is a
/// plain compare with no month/day tie-breaking — and a year-only tag stays
/// distinguishable from a real January 1st. Decompose with
/// [`year`](Self::year)/[`month`](Self::month)/[`day`](Self::day) at display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReleaseDate(pub u32);

impl ReleaseDate {
    pub fn new(year: u16, month: Option<u8>, day: Option<u8>) -> Self {
        Self(year as u32 * 10_000 + month.unwrap_or(0) as u32 * 100 + day.unwrap_or(0) as u32)
    }

    pub fn year(self) -> u16 {
        (self.0 / 10_000) as u16
    }

    pub fn month(self) -> Option<u8> {
        let month = ((self.0 / 100) % 100) as u8;
        (month > 0).then_some(month)
    }

    pub fn day(self) -> Option<u8> {
        let day = (self.0 % 100) as u8;
        (day > 0).then_some(day)
    }
}

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
    /// The release date, packed as `YYYYMMDD` (`20030501`), when the tags
    /// declare one (`Year`, `RecordingDate`, or `ReleaseDate`); a year-only
    /// tag packs as `20030000`. Drives the browse header's year and the date
    /// filters, and is the library's chronological sort key.
    #[serde(default)]
    pub date: Option<ReleaseDate>,
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
                date: None,
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
        let date = read_date(tag);

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
            date,
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

/// The date a tag declares, packed — trying the three keys that carry one.
///
/// `Year` is the Vorbis-comment key (and ID3v2.3 `TYER`); `RecordingDate`
/// (`TDRC`) and `ReleaseDate` (`TDRL`) are ISO-ish dates like `2003-05-01`,
/// which is why only the leading numbers are read: `2003-05-01 12:00` still
/// yields May 1st, and a bare `2003` yields the year alone.
fn read_date(tag: &Tag) -> Option<ReleaseDate> {
    [ItemKey::Year, ItemKey::RecordingDate, ItemKey::ReleaseDate]
        .iter()
        .find_map(|key| tag.get_string(key).and_then(parse_date))
}

/// Parse a date-ish tag value into a packed [`ReleaseDate`].
///
/// The leading number is the year; a `-`/`/`-separated month and day follow
/// when present. A compact `20030501` (no separators) is split by position.
/// Anything that doesn't fit degrades: a value with a month but no day keeps
/// the year and drops the rest, and a value with no usable year yields `None`.
fn parse_date(s: &str) -> Option<ReleaseDate> {
    let trimmed = s.trim();
    let mut numbers: Vec<u32> = trimmed
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<_>>>()?;

    // `20030501`: one unseparated run of eight digits is year/month/day.
    if numbers.len() == 1 && trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let n = numbers[0];
        numbers = vec![n / 10_000, (n / 100) % 100, n % 100];
    }

    let year = numbers.first().copied().filter(|y| (1000..=9999).contains(y))? as u16;
    let month = numbers.get(1).copied().filter(|m| (1..=12).contains(m)).map(|m| m as u8);
    // A day is only meaningful alongside a month.
    let day = month.and_then(|_| {
        numbers.get(2).copied().filter(|d| (1..=31).contains(d)).map(|d| d as u8)
    });
    Some(ReleaseDate::new(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::{parse_date, ReleaseDate};

    #[test]
    fn parse_date_reads_the_year_and_the_month_day_when_present() {
        let year_only = ReleaseDate::new(2003, None, None);
        let full = ReleaseDate::new(2003, Some(5), Some(1));
        assert_eq!(parse_date("2003"), Some(year_only));
        assert_eq!(parse_date("2003-05-01"), Some(full));
        assert_eq!(parse_date("2003/5/1"), Some(full));
        // A timestamp after the date is ignored.
        assert_eq!(parse_date("2003-05-01 12:00"), Some(full));
        // A compact `YYYYMMDD` has no separators to split on.
        assert_eq!(parse_date("20030501"), Some(full));
        // A month without a day keeps the year and month.
        assert_eq!(parse_date("2003-05"), Some(ReleaseDate::new(2003, Some(5), None)));
        // Out-of-range or non-date values degrade to nothing.
        assert_eq!(parse_date("13"), None);
        assert_eq!(parse_date("2003-13-01"), Some(year_only));
        assert_eq!(parse_date(""), None);
    }

    #[test]
    fn packed_dates_sort_chronologically_and_decompose() {
        let jan = ReleaseDate::new(2003, Some(1), Some(1));
        let may = ReleaseDate::new(2003, Some(5), Some(1));
        let next_year = ReleaseDate::new(2004, Some(1), Some(1));
        assert!(jan < may && may < next_year, "most-significant field first");
        assert_eq!(may.year(), 2003);
        assert_eq!(may.month(), Some(5));
        assert_eq!(may.day(), Some(1));
        let year_only = ReleaseDate::new(2003, None, None);
        assert_eq!((year_only.month(), year_only.day()), (None, None));
        assert!(year_only < jan, "year-only sorts before a real January");
    }
}
