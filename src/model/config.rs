//! Runtime configuration: the theme, per-container font sizes, and the list
//! of directories synced into the library.
//!
//! Persisted as JSON under the user's config directory. Deliberately
//! framework-agnostic (no GPUI, no `Entity`) — the settings panel only reads
//! and mutates plain data; the observable wrapper lives in
//! [`ui::config_state`](crate::ui::config_state).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::input::InputAction;

/// Which palette the UI draws with.
///
/// The `Theme` itself lives in the UI layer; the model only records *which*
/// one, so the choice can be persisted without dragging GPUI types into the
/// schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeKind {
    #[default]
    Dark,
    Light,
}

impl ThemeKind {
    /// Every selectable theme, in the order the settings panel lists them.
    pub const ALL: [ThemeKind; 2] = [ThemeKind::Dark, ThemeKind::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::Dark => "Dark",
            ThemeKind::Light => "Light",
        }
    }
}

/// A container's built-in font size, in px, when config has no override.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// The UI typeface the whole app draws with.
///
/// Like [`ThemeKind`], the model only records *which* family; the `Theme` in the
/// UI layer resolves the actual name. A family the system doesn't have simply
/// falls back to the text system's default, so these needn't all be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FontKind {
    /// The default monospace face — the app's original typeface.
    #[default]
    Tx02,
    /// Inter, a humanist sans.
    Inter,
    /// Noto Sans, a neutral sans.
    NotoSans,
    /// Noto Sans Mono, a second monospace option.
    NotoMono,
    /// Noto Serif, for a bookish look.
    NotoSerif,
}

impl FontKind {
    /// Every selectable family, in the order the settings panel lists them.
    pub const ALL: [FontKind; 5] = [
        FontKind::Tx02,
        FontKind::Inter,
        FontKind::NotoSans,
        FontKind::NotoMono,
        FontKind::NotoSerif,
    ];

    /// The font-family name handed to the text system.
    pub fn family(self) -> &'static str {
        match self {
            FontKind::Tx02 => "TX-02",
            FontKind::Inter => "Inter",
            FontKind::NotoSans => "Noto Sans",
            FontKind::NotoMono => "Noto Sans Mono",
            FontKind::NotoSerif => "Noto Serif",
        }
    }

    /// The label the settings panel shows.
    pub fn label(self) -> &'static str {
        match self {
            FontKind::Tx02 => "TX-02",
            FontKind::Inter => "Inter",
            FontKind::NotoSans => "Noto Sans",
            FontKind::NotoMono => "Noto Mono",
            FontKind::NotoSerif => "Noto Serif",
        }
    }
}

/// The range the settings panel lets a font size move within.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// Runtime configuration, loaded from and saved to disk.
///
/// `keybinds` is a placeholder until the input backend is settled (see
/// [`InputAction`]); the value type is a `String` rather than a specific key
/// type on purpose, and it isn't persisted yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Which palette to draw with.
    pub theme: ThemeKind,
    /// Per-container typeface overrides, keyed by
    /// [`Container::container_id`](crate::ui::container::Container::container_id).
    /// A container absent here draws with [`FontKind::default`].
    #[serde(default)]
    pub fonts: HashMap<String, FontKind>,
    /// Per-container font-size overrides, keyed by
    /// [`Container::container_id`](crate::ui::container::Container::container_id).
    /// A container absent here uses its own built-in default.
    pub font_sizes: HashMap<String, f32>,
    /// Directories synced into the library. This is the source of truth for
    /// scan roots — the command line only seeds it on a first run.
    pub roots: Vec<PathBuf>,
    #[serde(skip)]
    pub keybinds: HashMap<InputAction, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeKind::Dark,
            fonts: HashMap::new(),
            font_sizes: HashMap::new(),
            roots: Vec::new(),
            keybinds: HashMap::new(),
        }
    }
}

impl Config {
    /// File extensions the decoder stack can play. Opus and OGG both go
    /// through the Ogg demuxer; `.ogg` may hold either Vorbis or Opus.
    /// `.m4a`/`.m4b` are MP4 containers (AAC/ALAC), covered by symphonia's
    /// `isomp4`/`aac` support that rodio's `symphonia-all` feature pulls in.
    pub const SONG_EXTENSIONS: &'static [&'static str] =
        &["flac", "mp3", "opus", "ogg", "m4a", "m4b"];

    /// Whether `path` looks like a playable audio file, by extension.
    /// Case-insensitive.
    pub fn is_supported(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| Self::SONG_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// Load the config at `path`, falling back to defaults when it's missing
    /// or unreadable. A broken config is not fatal — it just means defaults.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist to `path`, writing via a temp file + rename so a crash mid-write
    /// can't leave a truncated config behind.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The effective font size for `container`: its override when set, else
    /// the container's own `default`. Clamped to the settable range.
    pub fn font_size_for(&self, container: &str, default: f32) -> f32 {
        self.font_sizes
            .get(container)
            .copied()
            .unwrap_or(default)
            .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// Set (or clear, with `None`) a container's font-size override.
    pub fn set_font_size(&mut self, container: &str, size: Option<f32>) {
        match size {
            Some(size) => {
                self.font_sizes
                    .insert(container.to_string(), size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
            }
            None => {
                self.font_sizes.remove(container);
            }
        }
    }

    /// The effective typeface for `container`: its override when set, else
    /// `default`.
    pub fn font_for(&self, container: &str, default: FontKind) -> FontKind {
        self.fonts.get(container).copied().unwrap_or(default)
    }

    /// Set (or clear, with `None`) a container's typeface override.
    pub fn set_font(&mut self, container: &str, font: Option<FontKind>) {
        match font {
            Some(font) => {
                self.fonts.insert(container.to_string(), font);
            }
            None => {
                self.fonts.remove(container);
            }
        }
    }

    /// Add a scan root, canonicalized so the same directory can't be listed
    /// twice under different spellings. Returns whether it was added.
    pub fn add_root(&mut self, path: PathBuf) -> bool {
        let path = path.canonicalize().unwrap_or(path);
        if self.roots.iter().any(|r| r == &path) {
            return false;
        }
        self.roots.push(path);
        true
    }

    /// Remove a scan root. Returns whether it was present.
    pub fn remove_root(&mut self, path: &Path) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| r != path);
        self.roots.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_known_extensions_case_insensitively() {
        for name in ["a.flac", "b.MP3", "c.Opus", "d.ogg", "e.m4a", "f.M4B"] {
            assert!(Config::is_supported(Path::new(name)), "{name} should be supported");
        }
    }

    #[test]
    fn rejects_unknown_and_extensionless() {
        for name in ["a.wav", "b.txt", "noextension", "c.flac.bak"] {
            assert!(!Config::is_supported(Path::new(name)), "{name} should be rejected");
        }
    }

    #[test]
    fn font_size_falls_back_to_the_container_default() {
        let mut config = Config::default();
        assert_eq!(config.font_size_for("browse", 13.0), 13.0, "no override -> container default");
        config.set_font_size("browse", Some(18.0));
        assert_eq!(config.font_size_for("browse", 13.0), 18.0);
        config.set_font_size("browse", None);
        assert_eq!(config.font_size_for("browse", 13.0), 13.0, "clearing restores the default");
    }

    #[test]
    fn font_size_is_clamped() {
        let mut config = Config::default();
        config.set_font_size("browse", Some(1000.0));
        assert_eq!(config.font_size_for("browse", 13.0), MAX_FONT_SIZE);
        config.set_font_size("browse", Some(0.0));
        assert_eq!(config.font_size_for("browse", 13.0), MIN_FONT_SIZE);
    }

    #[test]
    fn roots_dedupe_on_add_and_remove_by_path() {
        let mut config = Config::default();
        assert!(config.add_root(PathBuf::from("/music/a")));
        assert!(!config.add_root(PathBuf::from("/music/a")), "duplicate is ignored");
        assert!(config.remove_root(Path::new("/music/a")));
        assert!(!config.remove_root(Path::new("/music/a")), "already gone");
    }

    #[test]
    fn round_trips_through_json() {
        let mut config = Config::default();
        config.theme = ThemeKind::Light;
        config.set_font("browse", Some(FontKind::Inter));
        config.set_font_size("lyrics", Some(16.0));
        config.add_root(PathBuf::from("/music"));

        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.theme, ThemeKind::Light);
        assert_eq!(back.font_for("browse", FontKind::Tx02), FontKind::Inter);
        assert_eq!(back.font_size_for("lyrics", 14.0), 16.0);
        assert_eq!(back.roots, vec![PathBuf::from("/music")]);
    }

    #[test]
    fn font_falls_back_to_the_default_and_can_be_cleared() {
        let mut config = Config::default();
        assert_eq!(config.font_for("browse", FontKind::Tx02), FontKind::Tx02);
        config.set_font("browse", Some(FontKind::NotoMono));
        assert_eq!(config.font_for("browse", FontKind::Tx02), FontKind::NotoMono);
        config.set_font("browse", None);
        assert_eq!(config.font_for("browse", FontKind::Tx02), FontKind::Tx02);
    }

    #[test]
    fn a_config_without_fonts_falls_back_to_the_default() {
        // An older config predates the `fonts` field; loading it must keep the
        // rest of the settings and default the typeface.
        let json = r#"{"theme":"Light","font_sizes":{"lyrics":16.0},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.theme, ThemeKind::Light);
        assert_eq!(config.font_for("lyrics", FontKind::Tx02), FontKind::Tx02);
        assert_eq!(config.font_size_for("lyrics", 14.0), 16.0);
    }
}
