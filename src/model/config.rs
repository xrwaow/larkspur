use std::collections::HashMap;
use std::path::Path;

use super::input::InputAction;

/// Runtime configuration.
///
/// `keybinds` is a placeholder until the input backend is settled (see
/// [`InputAction`]); the value type is a `String` rather than a specific
/// key type on purpose.
pub struct Config {
    pub keybinds: HashMap<InputAction, String>,
}

impl Config {
    /// File extensions the decoder stack can play. Opus and OGG both go
    /// through the Ogg demuxer; `.ogg` may hold either Vorbis or Opus.
    pub const SONG_EXTENSIONS: &'static [&'static str] = &["flac", "mp3", "opus", "ogg"];

    /// Whether `path` looks like a playable audio file, by extension.
    /// Case-insensitive.
    pub fn is_supported(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| Self::SONG_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_known_extensions_case_insensitively() {
        for name in ["a.flac", "b.MP3", "c.Opus", "d.ogg"] {
            assert!(Config::is_supported(Path::new(name)), "{name} should be supported");
        }
    }

    #[test]
    fn rejects_unknown_and_extensionless() {
        for name in ["a.wav", "b.txt", "noextension", "c.flac.bak"] {
            assert!(!Config::is_supported(Path::new(name)), "{name} should be rejected");
        }
    }
}
