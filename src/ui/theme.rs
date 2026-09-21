//! The UI palette — colors and fonts in one place.
//!
//! Views read a [`Theme`] instead of hardcoding hex values, so a theme is just
//! another constructor (`Theme::dark()`, `Theme::light()`) and the `Copy` struct
//! can be swapped per-view without touching any view code.
//!
//! The theme also carries the font size, because each view builds its own copy:
//! the palette is shared (chosen by [`ThemeKind`]), but the font size is
//! per-container, so two views can render the same colors at different sizes.

use gpui::{rgb, Rgba};

use crate::model::config::ThemeKind;
use crate::model::config::{FontKind, DEFAULT_FONT_SIZE};

#[derive(Clone, Copy)]
pub struct Theme {
    /// The UI typeface. TX-02 is monospace, which is what lets the number,
    /// bitrate, and duration columns line up.
    pub font: &'static str,
    /// Base text size for this container, in px. Column widths and the
    /// `small`/`line` sizes all derive from it, so raising it scales the whole
    /// container rather than just the glyphs.
    pub font_size: f32,

    /// The window backdrop behind the panels.
    pub window_bg: Rgba,
    /// The center panel.
    pub panel_bg: Rgba,
    /// The left rail.
    pub rail_bg: Rgba,
    /// The table's header band.
    pub header_bg: Rgba,

    /// Alternating row backgrounds — the light/dark zebra stripes.
    pub row_even: Rgba,
    pub row_odd: Rgba,
    pub row_hover: Rgba,
    /// A selected/active row in the rail.
    pub row_active: Rgba,
    /// The row currently playing.
    pub row_playing: Rgba,

    pub text: Rgba,
    pub text_muted: Rgba,
    pub text_faint: Rgba,
    /// Emphasis: the playing track, the "+" affordance.
    pub accent: Rgba,
    /// Selected text in a text field.
    pub selection: Rgba,
    /// Hairlines and subtle dividers.
    pub border: Rgba,
    /// Waveform bars: the not-yet-played portion.
    pub waveform: Rgba,
    /// Waveform bars: the played portion.
    pub waveform_played: Rgba,
}

impl Theme {
    /// The theme for `kind`, drawing with `font` at `font_size`.
    pub fn for_kind(kind: ThemeKind, font: &'static str, font_size: f32) -> Self {
        let mut theme = match kind {
            ThemeKind::Dark => Self::dark_palette(),
            ThemeKind::Light => Self::light_palette(),
        };
        theme.font = font;
        theme.font_size = font_size;
        theme
    }

    pub fn dark() -> Self {
        Self::for_kind(ThemeKind::Dark, FontKind::default().family(), DEFAULT_FONT_SIZE)
    }

    pub fn light() -> Self {
        Self::for_kind(ThemeKind::Light, FontKind::default().family(), DEFAULT_FONT_SIZE)
    }

    fn dark_palette() -> Self {
        Self {
            font: "TX-02",
            font_size: DEFAULT_FONT_SIZE,
            window_bg: rgb(0x000000),
            panel_bg: rgb(0x141414),
            rail_bg: rgb(0x101010),
            header_bg: rgb(0x1b1b1b),
            row_even: rgb(0x141414),
            row_odd: rgb(0x191919),
            row_hover: rgb(0x232323),
            row_active: rgb(0x2b2b2b),
            row_playing: rgb(0x1c2a1c),
            text: rgb(0xe0e0e0),
            text_muted: rgb(0x808080),
            text_faint: rgb(0x606060),
            accent: rgb(0x8fe08f),
            selection: rgb(0x2a4a6a),
            border: rgb(0x242424),
            waveform: rgb(0x404040),
            waveform_played: rgb(0xffffff),
        }
    }

    /// The same structure as [`dark`](Self::dark), inverted: light surfaces
    /// with dark text, and a deeper green accent so it stays readable.
    fn light_palette() -> Self {
        Self {
            font: "TX-02",
            font_size: DEFAULT_FONT_SIZE,
            window_bg: rgb(0xf4f4f4),
            panel_bg: rgb(0xfafafa),
            rail_bg: rgb(0xeeeeee),
            header_bg: rgb(0xe4e4e4),
            row_even: rgb(0xfafafa),
            row_odd: rgb(0xf1f1f1),
            row_hover: rgb(0xe2e2e2),
            row_active: rgb(0xd6d6d6),
            row_playing: rgb(0xdcecdc),
            text: rgb(0x1a1a1a),
            text_muted: rgb(0x5a5a5a),
            text_faint: rgb(0x8a8a8a),
            accent: rgb(0x1f7a1f),
            selection: rgb(0xbfd8ff),
            border: rgb(0xd0d0d0),
            waveform: rgb(0xb8b8b8),
            waveform_played: rgb(0x303030),
        }
    }

    /// The row background for index `i` — the zebra stripe.
    pub fn row_bg(&self, i: usize) -> Rgba {
        if i % 2 == 0 {
            self.row_even
        } else {
            self.row_odd
        }
    }

    // --- sizes derived from `font_size` ---------------------------------
    //
    // Views use these instead of their own px constants, so a container's font
    // size scales its columns along with its glyphs.

    /// Body text / cell text.
    pub fn cell_px(&self) -> f32 {
        self.font_size
    }

    /// Secondary text: artists, meta lines, hints.
    pub fn small_px(&self) -> f32 {
        (self.font_size - 2.0).max(6.0)
    }

    /// The track-number column.
    pub fn num_col(&self) -> f32 {
        self.font_size * 2.9
    }

    /// The duration column.
    pub fn length_col(&self) -> f32 {
        self.font_size * 4.6
    }

    /// The bitrate column — wide enough for `999 kb/s`.
    pub fn bitrate_col(&self) -> f32 {
        self.font_size * 5.4
    }

    /// The trailing action column (the `×` on a playlist row).
    pub fn action_col(&self) -> f32 {
        self.font_size * 2.0
    }

    /// One lyric slot's height, so blank padding lines up with real ones.
    pub fn line_height(&self) -> f32 {
        self.font_size * 1.7
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
