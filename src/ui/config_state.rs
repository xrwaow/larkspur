//! The shared, observable configuration.
//!
//! Exactly one `Entity<ConfigState>` exists per window. It owns the persisted
//! [`Config`] and is **observed** by every view that draws with the theme or a
//! per-container font size — so changing the theme or a font size re-renders
//! those views live, without a restart.
//!
//! Scan roots also live here (the synced-path list), which is why
//! [`LibraryState`](super::library_state::LibraryState) observes this entity
//! too: adding or removing a root triggers a rescan.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{Context, Entity, Rgba, Subscription};

use crate::model::config::{Config, FontKind, WaveformStyle};
use crate::model::ThemeKind;
use crate::ui::container::Container;
use crate::ui::theme::Theme;

pub struct ConfigState {
    config: Config,
    path: PathBuf,
    /// The accent the dynamic theme is built from — the dominant colour of the
    /// current cover. `None` until a cover has been analysed, so the dynamic
    /// theme falls back to the default palette at launch.
    dynamic_accent: Option<Rgba>,
}

impl ConfigState {
    /// Build the state over an already-loaded [`Config`], remembering where to
    /// persist changes.
    pub fn new(config: Config, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self { config, path, dynamic_accent: None }
    }

    // --- read-only accessors: what views render from -------------------

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn theme_kind(&self) -> ThemeKind {
        self.config.theme
    }

    /// The theme for `container`, at its configured (or built-in) font size and
    /// typeface.
    pub fn theme_for(&self, container: &str, default_size: f32) -> Theme {
        let font = self.config.font_for(container, FontKind::default()).family();
        let size = self.config.font_size_for(container, default_size);
        match self.config.theme {
            // The dynamic palette is derived from the current cover; until one
            // has been analysed it falls back to the default dark palette.
            ThemeKind::Dynamic => match self.dynamic_accent {
                Some(accent) => Theme::dynamic(accent, font, size),
                None => Theme::for_kind(ThemeKind::Dark, font, size),
            },
            kind => Theme::for_kind(kind, font, size),
        }
    }

    /// The effective typeface for `container`.
    pub fn font_for(&self, container: &str, default: FontKind) -> FontKind {
        self.config.font_for(container, default)
    }

    /// The effective font size for `container`.
    pub fn font_size(&self, container: &str, default: f32) -> f32 {
        self.config.font_size_for(container, default)
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.config.roots
    }

    /// How the transport bar draws the waveform.
    pub fn waveform(&self) -> WaveformStyle {
        self.config.waveform
    }

    // --- actions -------------------------------------------------------

    pub fn set_theme(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        if self.config.theme != kind {
            self.config.theme = kind;
            self.persist();
            cx.notify();
        }
    }

    /// Set the waveform style the transport bar draws with.
    pub fn set_waveform(&mut self, style: WaveformStyle, cx: &mut Context<Self>) {
        if self.config.waveform != style {
            self.config.waveform = style;
            self.persist();
            cx.notify();
        }
    }

    /// Set the accent the dynamic theme derives from. A no-op when unchanged, so
    /// a per-frame caller can't churn the whole UI.
    pub fn set_dynamic_accent(&mut self, accent: Option<Rgba>, cx: &mut Context<Self>) {
        if self.dynamic_accent != accent {
            self.dynamic_accent = accent;
            cx.notify();
        }
    }

    /// Set (or clear, with `None`) a container's typeface override.
    pub fn set_font(&mut self, container: &str, font: Option<FontKind>, cx: &mut Context<Self>) {
        self.config.set_font(container, font);
        self.persist();
        cx.notify();
    }

    pub fn set_font_size(&mut self, container: &str, size: f32, cx: &mut Context<Self>) {
        self.config.set_font_size(container, Some(size));
        self.persist();
        cx.notify();
    }

    /// Clear a container's override, restoring its built-in default.
    pub fn reset_font_size(&mut self, container: &str, cx: &mut Context<Self>) {
        self.config.set_font_size(container, None);
        self.persist();
        cx.notify();
    }

    /// Add a synced path. Returns whether it was actually added.
    pub fn add_root(&mut self, path: PathBuf, cx: &mut Context<Self>) -> bool {
        if self.config.add_root(path) {
            self.persist();
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Remove a synced path. Returns whether it was present.
    pub fn remove_root(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        if self.config.remove_root(path) {
            self.persist();
            cx.notify();
            true
        } else {
            false
        }
    }

    // --- internals -----------------------------------------------------

    fn persist(&mut self) {
        if let Err(e) = self.config.save(&self.path) {
            eprintln!("failed to save config: {e}");
        }
    }
}

/// A view's live theme: the current palette plus the config subscription that
/// keeps it current.
///
/// Every container draws with a [`Theme`] and re-renders when the config
/// changes it — the same two lines in every view. A view holds a `Themed` and
/// reads [`theme`](Self::theme), which removes the boilerplate and, more
/// usefully, makes it impossible for a new view to forget the subscription.
pub struct Themed {
    theme: Rc<Cell<Theme>>,
    _config: Subscription,
}

impl Themed {
    pub fn new<C: Container>(config: &Entity<ConfigState>, cx: &mut Context<C>) -> Self {
        let theme = Rc::new(Cell::new(current::<C>(config, cx)));
        let cell = theme.clone();
        let config = config.clone();
        let _config = cx.observe(&config, move |_this, config, cx| {
            cell.set(config.read(cx).theme_for(C::container_id(), C::default_font_size()));
            cx.notify();
        });
        Self { theme, _config }
    }

    /// The container's current palette, at its configured font size and
    /// typeface.
    pub fn theme(&self) -> Theme {
        self.theme.get()
    }
}

fn current<C: Container>(config: &Entity<ConfigState>, cx: &Context<C>) -> Theme {
    config.read(cx).theme_for(C::container_id(), C::default_font_size())
}
