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

use std::path::{Path, PathBuf};

use gpui::Context;

use crate::model::config::{Config, FontKind};
use crate::model::ThemeKind;
use crate::ui::theme::Theme;

pub struct ConfigState {
    config: Config,
    path: PathBuf,
}

impl ConfigState {
    /// Build the state over an already-loaded [`Config`], remembering where to
    /// persist changes.
    pub fn new(config: Config, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self { config, path }
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
        Theme::for_kind(
            self.config.theme,
            self.config.font_for(container, FontKind::default()).family(),
            self.config.font_size_for(container, default_size),
        )
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

    // --- actions -------------------------------------------------------

    pub fn set_theme(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        if self.config.theme != kind {
            self.config.theme = kind;
            self.persist();
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
