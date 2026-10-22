//! Audio container formats the decoder stack supports.
//!
//! Parsed once at scan time from the extension and stored on
//! [`SongMetadata`](super::song::SongMetadata), so search filters and support
//! checks compare an enum instead of lowercasing a path per song.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A supported audio container/codec family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Flac,
    Mp3,
    Opus,
    Ogg,
    M4a,
    M4b,
}

impl AudioFormat {
    pub const ALL: [AudioFormat; 6] = [
        AudioFormat::Flac,
        AudioFormat::Mp3,
        AudioFormat::Opus,
        AudioFormat::Ogg,
        AudioFormat::M4a,
        AudioFormat::M4b,
    ];

    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "flac" => Some(Self::Flac),
            "mp3" => Some(Self::Mp3),
            "opus" => Some(Self::Opus),
            "ogg" => Some(Self::Ogg),
            "m4a" => Some(Self::M4a),
            "m4b" => Some(Self::M4b),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|extension| extension.to_str())
            .and_then(Self::from_extension)
    }

    fn bit(self) -> u8 {
        match self {
            Self::Flac => 1 << 0,
            Self::Mp3 => 1 << 1,
            Self::Opus => 1 << 2,
            Self::Ogg => 1 << 3,
            Self::M4a => 1 << 4,
            Self::M4b => 1 << 5,
        }
    }
}

/// A set of accepted formats, stored as bits so a filter stays a `Copy` value
/// rather than a `Vec` allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatSet(u8);

impl FormatSet {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn insert(&mut self, format: AudioFormat) {
        self.0 |= format.bit();
    }

    pub fn contains(self, format: AudioFormat) -> bool {
        self.0 & format.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_extensions_case_insensitively() {
        assert_eq!(AudioFormat::from_path(Path::new("a.FLAC")), Some(AudioFormat::Flac));
        assert_eq!(AudioFormat::from_path(Path::new("a.m4b")), Some(AudioFormat::M4b));
        assert_eq!(AudioFormat::from_path(Path::new("a.wav")), None);
        assert_eq!(AudioFormat::from_path(Path::new("noext")), None);
    }

    #[test]
    fn format_set_is_a_bitset() {
        let mut set = FormatSet::empty();
        set.insert(AudioFormat::Flac);
        set.insert(AudioFormat::Mp3);
        assert!(set.contains(AudioFormat::Flac));
        assert!(set.contains(AudioFormat::Mp3));
        assert!(!set.contains(AudioFormat::Ogg));
    }
}
