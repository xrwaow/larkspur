use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One timestamped line of synced lyrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub timestamp: Duration,
    pub text: String,
}

/// Lyrics for a song, in whatever form we managed to find them.
///
/// Embedded tags (ID3 USLT / Vorbis comments) essentially never carry
/// timestamps, so `Synced` usually comes from a `.lrc` sidecar — but some
/// taggers *do* embed LRC-formatted text, which is why [`looks_like_lrc`]
/// sniffs the tag contents rather than trusting the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lyrics {
    None,
    /// Unsynced text, shown as a block.
    Plain(String),
    /// Timestamped lines, shown highlighted against playback position.
    Synced(Vec<LyricLine>),
}

impl Lyrics {
    pub fn is_none(&self) -> bool {
        matches!(self, Lyrics::None)
    }

    /// The full text, however it's stored — used by the search engine and by
    /// the plain rendering path. Borrowed for `Plain`, joined for `Synced`.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        match self {
            Lyrics::None => None,
            Lyrics::Plain(s) => Some(Cow::Borrowed(s)),
            Lyrics::Synced(lines) => {
                if lines.is_empty() {
                    return None;
                }
                let mut joined = String::new();
                for (i, line) in lines.iter().enumerate() {
                    if i > 0 {
                        joined.push('\n');
                    }
                    joined.push_str(&line.text);
                }
                Some(Cow::Owned(joined))
            }
        }
    }
}

/// The `.lrc` sidecar path for an audio file: same directory, same stem.
pub fn sidecar_path(audio: &Path) -> PathBuf {
    audio.with_extension("lrc")
}

/// Load synced lyrics from a `.lrc` sidecar next to `audio`, if one is there.
///
/// A sidecar wins over embedded lyrics: it's the only source that reliably
/// carries timestamps, so when both exist the synced one is the better answer.
/// Sidecars are read at scan time and cached with the rest of the metadata.
pub fn load_sidecar(audio: &Path) -> Option<Lyrics> {
    let text = std::fs::read_to_string(sidecar_path(audio)).ok()?;
    let lines = parse_lrc(&text);
    (!lines.is_empty()).then_some(Lyrics::Synced(lines))
}

/// Detects whether a string contains LRC-style timestamp tags
/// (e.g. `[01:23.45]`) anywhere in it — used because taggers commonly
/// embed full synced lyrics directly in the Lyrics/USLT tag itself,
/// not just in an external `.lrc` sidecar.
pub(crate) fn looks_like_lrc(s: &str) -> bool {
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

/// Parse a `mm:ss.xx` (or `mm:ss`) timestamp tag. Returns `None` for
/// non-timestamp tags like `[ti:...]`/`[ar:...]`, which is what lets the
/// scanner skip metadata without special-casing it.
pub(crate) fn parse_timestamp_tag(tag: &str) -> Option<Duration> {
    let mut parts = tag.splitn(2, ':');
    let min: u64 = parts.next()?.trim().parse().ok()?;
    let sec: f64 = parts.next()?.trim().parse().ok()?;
    Some(Duration::from_secs_f64(min as f64 * 60.0 + sec))
}

/// Parses LRC-format lyrics text into synced lines.
///
/// Scans for `[...]` tags directly rather than splitting on lines first,
/// since embedded (non-sidecar) LRC content isn't guaranteed to use real
/// newlines as separators between timestamp entries. Metadata tags like
/// `[ti:...]`/`[ar:...]` are skipped automatically — they don't parse as
/// `mm:ss.xx` timestamps.
///
/// A run of consecutive tags on the *same line* shares the text that
/// follows the run, per the LRC convention: `[00:01][00:10]chorus`
/// yields the same line at both timestamps. (The naive "text between this
/// tag and the next" approach silently drops every timestamp but the
/// last.) A newline between tags ends the run, so a standalone blank
/// `[00:01]` line stays a gap rather than borrowing the next line's text.
pub fn parse_lrc(contents: &str) -> Vec<LyricLine> {
    // (start, end, tag_body) for every `[...]` in the input.
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
    let mut i = 0;
    while i < tags.len() {
        // Extend the run while the next tag is on the same line with
        // nothing but spaces between them. A newline (or any real text)
        // ends the run.
        let mut j = i;
        while j + 1 < tags.len() {
            let between = &contents[tags[j].1..tags[j + 1].0];
            if between.contains('\n') || !between.trim().is_empty() {
                break;
            }
            j += 1;
        }

        let text_end = tags.get(j + 1).map(|(s, _, _)| *s).unwrap_or(contents.len());
        let text = contents[tags[j].1..text_end]
            .trim()
            .trim_matches('/')
            .trim();
        if !text.is_empty() {
            for (_, _, tag) in &tags[i..=j] {
                if let Some(timestamp) = parse_timestamp_tag(tag) {
                    lines.push(LyricLine { timestamp, text: text.to_string() });
                }
            }
        }
        i = j + 1;
    }

    lines.sort_by_key(|l| l.timestamp);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(min: u64, sec: f64) -> Duration {
        Duration::from_secs_f64(min as f64 * 60.0 + sec)
    }

    #[test]
    fn parses_basic_lrc() {
        let lines = parse_lrc("[00:01.00]first\n[00:02.50]second");
        assert_eq!(
            lines,
            vec![
                LyricLine { timestamp: at(0, 1.0), text: "first".into() },
                LyricLine { timestamp: at(0, 2.5), text: "second".into() },
            ]
        );
    }

    #[test]
    fn skips_metadata_tags() {
        let lines = parse_lrc("[ti:Title]\n[ar:Artist]\n[00:05.00]hello");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[0].timestamp, at(0, 5.0));
    }

    #[test]
    fn run_of_timestamps_shares_the_following_text() {
        let lines = parse_lrc("[00:01.00][00:10.00]chorus");
        assert_eq!(lines.len(), 2, "both timestamps should yield a line");
        assert!(lines.iter().all(|l| l.text == "chorus"));
        assert_eq!(lines[0].timestamp, at(0, 1.0));
        assert_eq!(lines[1].timestamp, at(0, 10.0));
    }

    #[test]
    fn output_is_sorted_by_timestamp() {
        let lines = parse_lrc("[00:10.00]later\n[00:01.00]earlier");
        assert_eq!(lines[0].text, "earlier");
        assert_eq!(lines[1].text, "later");
    }

    #[test]
    fn empty_lines_are_dropped() {
        let lines = parse_lrc("[00:01.00]\n[00:02.00]real");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "real");
    }

    #[test]
    fn detects_lrc_vs_plain() {
        assert!(looks_like_lrc("[00:01.00]hi"));
        assert!(looks_like_lrc("intro\n[01:02.5]hi"));
        assert!(!looks_like_lrc("just some plain lyrics"));
        assert!(!looks_like_lrc("[ti:not a timestamp]"));
    }

    #[test]
    fn accepts_short_and_long_timestamps() {
        assert_eq!(parse_timestamp_tag("00:01"), Some(at(0, 1.0)));
        assert_eq!(parse_timestamp_tag("01:02.50"), Some(at(1, 2.5)));
        assert_eq!(parse_timestamp_tag("ti:Title"), None);
    }
}
