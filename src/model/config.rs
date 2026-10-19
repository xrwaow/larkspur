//! Runtime configuration: the theme, per-container font sizes, and the list
//! of directories synced into the library.
//!
//! Persisted as JSON under the user's config directory. Deliberately
//! framework-agnostic (no GPUI, no `Entity`) — the settings panel only reads
//! and mutates plain data; the observable wrapper lives in
//! [`ui::config_state`](crate::ui::config_state).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// A palette derived from the current track's cover art. Falls back to
    /// [`Dark`](Self::Dark) until a cover has been analysed.
    Dynamic,
}

impl ThemeKind {
    /// Every selectable theme, in the order the settings panel lists them.
    pub const ALL: [ThemeKind; 3] = [ThemeKind::Dark, ThemeKind::Light, ThemeKind::Dynamic];

    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::Dark => "Dark",
            ThemeKind::Light => "Light",
            ThemeKind::Dynamic => "Dynamic",
        }
    }
}

/// The surface the dynamic theme builds on: a black (dark) or white (light)
/// base. Only consulted while [`ThemeKind::Dynamic`] is selected — it decides
/// both the palette structure and what a black & white cover renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DynamicBase {
    /// Dark surfaces with light text — the original dynamic look.
    #[default]
    Black,
    /// Light surfaces with dark text, tinted by the same cover hue.
    White,
}

impl DynamicBase {
    /// Every selectable base, in the order the settings panel lists them.
    pub const ALL: [DynamicBase; 2] = [DynamicBase::Black, DynamicBase::White];

    pub fn label(self) -> &'static str {
        match self {
            DynamicBase::Black => "Black",
            DynamicBase::White => "White",
        }
    }
}

impl From<DynamicBase> for ThemeKind {
    fn from(base: DynamicBase) -> Self {
        match base {
            DynamicBase::Black => ThemeKind::Dark,
            DynamicBase::White => ThemeKind::Light,
        }
    }
}

/// How the transport bar draws the waveform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WaveformStyle {
    /// Amplitude bars.
    #[default]
    Bars,
    /// A single flat progress line.
    FlatLine,
}

impl WaveformStyle {
    /// Every selectable style, in the order the settings panel lists them.
    pub const ALL: [WaveformStyle; 2] = [WaveformStyle::Bars, WaveformStyle::FlatLine];

    pub fn label(self) -> &'static str {
        match self {
            WaveformStyle::Bars => "Bars",
            WaveformStyle::FlatLine => "Line",
        }
    }
}

/// When the lyrics panel draws its procedural cover-art texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextureMode {
    /// Never draw it; no-lyrics tracks get the plain empty hint.
    Off,
    /// Draw it only when the track has no lyrics.
    #[default]
    NoLyrics,
    /// Draw it behind lyrics too, dimmed so the text stays readable.
    Always,
}

impl TextureMode {
    /// Every selectable mode, in the order the settings panel lists them.
    pub const ALL: [TextureMode; 3] =
        [TextureMode::Off, TextureMode::NoLyrics, TextureMode::Always];
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

/// The window's last on-screen placement, persisted so the next launch reopens
/// exactly where the user left it. `None` until the first save — a fresh
/// install opens at half the monitor's size, centered.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The serde default for [`Config::visualizer`] — the field postdates the
/// first configs, and an older file should gain the visualizer rather than
/// silently lose it.
fn default_visualizer() -> bool {
    true
}

/// One container's presentation overrides. Absent fields fall back to the
/// container's built-in defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerStyle {
    pub font: Option<FontKind>,
    pub size: Option<f32>,
}

impl ContainerStyle {
    fn is_empty(self) -> bool {
        self.font.is_none() && self.size.is_none()
    }
}

/// Runtime configuration, loaded from and saved to disk.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    /// Which palette to draw with.
    pub theme: ThemeKind,
    /// What the dynamic theme builds on — only read while `theme` is
    /// [`ThemeKind::Dynamic`].
    pub dynamic_base: DynamicBase,
    /// Per-container typeface and font-size overrides, keyed by
    /// [`Container::container_id`](crate::ui::container::Container::container_id).
    /// A container absent here draws with its own defaults.
    pub styles: BTreeMap<String, ContainerStyle>,
    /// Directories synced into the library. This is the source of truth for
    /// scan roots — the command line only seeds it on a first run.
    pub roots: Vec<PathBuf>,
    /// How the transport bar draws the waveform.
    pub waveform: WaveformStyle,
    /// Whether the spectrum-bar band above the lyrics is shown.
    pub visualizer: bool,
    /// When the lyrics panel draws its procedural cover-art texture.
    pub lyrics_texture: TextureMode,
    /// Whether the left library rail is collapsed to its header button.
    pub sidebar_minimized: bool,
    /// Where the window was when the app last ran, so it reopens the same
    /// size and position.
    pub window: Option<WindowPlacement>,
}

/// The on-disk shape, accepting the legacy split `fonts` + `font_sizes` maps
/// and folding them into [`Config::styles`].
#[derive(Deserialize)]
struct ConfigWire {
    #[serde(default)]
    theme: ThemeKind,
    #[serde(default)]
    dynamic_base: DynamicBase,
    #[serde(default)]
    styles: BTreeMap<String, ContainerStyle>,
    #[serde(default)]
    fonts: BTreeMap<String, FontKind>,
    #[serde(default)]
    font_sizes: BTreeMap<String, f32>,
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default)]
    waveform: WaveformStyle,
    #[serde(default = "default_visualizer")]
    visualizer: bool,
    #[serde(default)]
    lyrics_texture: TextureMode,
    #[serde(default)]
    sidebar_minimized: bool,
    #[serde(default)]
    window: Option<WindowPlacement>,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ConfigWire::deserialize(deserializer)?;
        let mut styles = wire.styles;
        // A pre-D5 config kept fonts and sizes in two parallel maps; fold them
        // in. New keys win if both shapes somehow coexist.
        for (container, font) in wire.fonts {
            styles.entry(container).or_default().font = Some(font);
        }
        for (container, size) in wire.font_sizes {
            styles.entry(container).or_default().size = Some(size);
        }
        Ok(Self {
            theme: wire.theme,
            dynamic_base: wire.dynamic_base,
            styles,
            roots: wire.roots,
            waveform: wire.waveform,
            visualizer: wire.visualizer,
            lyrics_texture: wire.lyrics_texture,
            sidebar_minimized: wire.sidebar_minimized,
            window: wire.window,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeKind::Dark,
            dynamic_base: DynamicBase::default(),
            styles: BTreeMap::new(),
            roots: Vec::new(),
            waveform: WaveformStyle::default(),
            visualizer: default_visualizer(),
            lyrics_texture: TextureMode::default(),
            sidebar_minimized: false,
            window: None,
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
        super::persist::read_json(path).unwrap_or_default()
    }

    /// Persist to `path`, writing via a temp file + rename so a crash mid-write
    /// can't leave a truncated config behind.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        super::persist::write_json_atomic(path, self, true)
    }

    /// The effective font size for `container`: its override when set, else
    /// the container's own `default`. Clamped to the settable range.
    pub fn font_size_for(&self, container: &str, default: f32) -> f32 {
        self.styles
            .get(container)
            .and_then(|style| style.size)
            .unwrap_or(default)
            .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// The container's font-size override, if it has one (for change checks).
    pub fn font_size_override(&self, container: &str) -> Option<f32> {
        self.styles.get(container).and_then(|style| style.size)
    }

    /// Set (or clear, with `None`) a container's font-size override.
    pub fn set_font_size(&mut self, container: &str, size: Option<f32>) {
        self.update_style(container, |style| {
            style.size = size.map(|s| s.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
        });
    }

    /// The effective typeface for `container`: its override when set, else
    /// `default`.
    pub fn font_for(&self, container: &str, default: FontKind) -> FontKind {
        self.styles.get(container).and_then(|style| style.font).unwrap_or(default)
    }

    /// The container's typeface override, if it has one (for change checks).
    pub fn font_override(&self, container: &str) -> Option<FontKind> {
        self.styles.get(container).and_then(|style| style.font)
    }

    /// Set (or clear, with `None`) a container's typeface override.
    pub fn set_font(&mut self, container: &str, font: Option<FontKind>) {
        self.update_style(container, |style| style.font = font);
    }

    /// Apply `change` to a container's style, dropping the entry when nothing
    /// is overridden any more so the saved file stays minimal.
    fn update_style(&mut self, container: &str, change: impl FnOnce(&mut ContainerStyle)) {
        let style = self.styles.entry(container.to_string()).or_default();
        change(style);
        if style.is_empty() {
            self.styles.remove(container);
        }
    }

    /// Record where the window was. Returns whether anything changed, so a
    /// caller can skip redundant work.
    pub fn set_window(&mut self, placement: WindowPlacement) -> bool {
        if self.window == Some(placement) {
            return false;
        }
        self.window = Some(placement);
        true
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
    fn legacy_split_font_maps_merge_into_styles() {
        let json = r#"{"theme":"Dark","fonts":{"browse":"Inter"},"font_sizes":{"browse":18.0,"lyrics":16.0},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.font_for("browse", FontKind::Tx02), FontKind::Inter);
        assert_eq!(config.font_size_for("browse", 14.0), 18.0);
        assert_eq!(config.font_size_for("lyrics", 14.0), 16.0);
        // Saving writes the new single-map shape, not the legacy fields.
        let out = serde_json::to_value(&config).unwrap();
        assert!(out.get("styles").is_some());
        assert!(out.get("fonts").is_none());
        assert!(out.get("font_sizes").is_none());
    }

    #[test]
    fn clearing_both_overrides_drops_the_style_entry() {
        let mut config = Config::default();
        config.set_font("browse", Some(FontKind::Inter));
        config.set_font_size("browse", Some(18.0));
        assert_eq!(config.styles.len(), 1);
        config.set_font("browse", None);
        assert_eq!(config.styles.len(), 1, "size override remains");
        config.set_font_size("browse", None);
        assert!(config.styles.is_empty(), "empty overrides are dropped");
    }

    #[test]
    fn round_trips_through_json() {
        let mut config = Config::default();
        config.theme = ThemeKind::Light;
        config.dynamic_base = DynamicBase::White;
        config.set_font("browse", Some(FontKind::Inter));
        config.set_font_size("lyrics", Some(16.0));
        config.add_root(PathBuf::from("/music"));

        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.theme, ThemeKind::Light);
        assert_eq!(back.dynamic_base, DynamicBase::White);
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

    #[test]
    fn a_config_without_a_dynamic_base_defaults_to_black() {
        // An older config predates the `dynamic_base` field.
        let json = r#"{"theme":"Dynamic","font_sizes":{},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.dynamic_base, DynamicBase::Black);
    }

    #[test]
    fn waveform_defaults_to_bars_and_round_trips() {
        assert_eq!(Config::default().waveform, WaveformStyle::Bars);
        let mut config = Config::default();
        config.waveform = WaveformStyle::FlatLine;
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.waveform, WaveformStyle::FlatLine);
    }

    #[test]
    fn a_config_without_a_waveform_defaults_to_bars() {
        // An older config predates the `waveform` field.
        let json = r#"{"theme":"Dark","font_sizes":{},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.waveform, WaveformStyle::Bars);
    }

    #[test]
    fn visualizer_defaults_on_and_round_trips() {
        assert!(Config::default().visualizer);
        let mut config = Config::default();
        config.visualizer = false;
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert!(!back.visualizer);
    }

    #[test]
    fn a_config_without_a_visualizer_field_defaults_to_on() {
        // An older config predates the `visualizer` field.
        let json = r#"{"theme":"Dark","font_sizes":{},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.visualizer);
    }

    #[test]
    fn lyrics_texture_defaults_to_no_lyrics_and_round_trips() {
        assert_eq!(Config::default().lyrics_texture, TextureMode::NoLyrics);

        let mut config = Config::default();
        config.lyrics_texture = TextureMode::Always;
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.lyrics_texture, TextureMode::Always);

        // An older config predates the field; it keeps the previous behavior.
        let json = r#"{"theme":"Dark","font_sizes":{},"roots":[]}"#;
        let old: Config = serde_json::from_str(json).unwrap();
        assert_eq!(old.lyrics_texture, TextureMode::NoLyrics);
    }

    #[test]
    fn sidebar_minimized_defaults_off_and_round_trips() {
        let json = r#"{"theme":"Dark","font_sizes":{},"roots":[]}"#;
        let old: Config = serde_json::from_str(json).unwrap();
        assert!(!old.sidebar_minimized, "old config leaves the sidebar open");

        let mut config = Config::default();
        config.sidebar_minimized = true;
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert!(back.sidebar_minimized);
    }

    #[test]
    fn window_placement_round_trips_and_defaults_to_none() {
        let json = r#"{"theme":"Dark","font_sizes":{},"roots":[]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.window, None, "an older config predates `window`");

        let mut config = Config::default();
        let placement = WindowPlacement { x: 10.0, y: 20.0, width: 800.0, height: 600.0 };
        assert!(config.set_window(placement), "first set changes");
        assert!(!config.set_window(placement), "same placement is a no-op");
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.window, Some(placement));
    }
}
