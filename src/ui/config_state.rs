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
use std::time::Duration;

use gpui::{Context, Entity, Rgba, Subscription};

use crate::model::config::{
    Config, DynamicBase, FontKind, TextureMode, WaveformStyle, WindowPlacement,
};
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
    /// Whether a debounced save of the window placement is already scheduled —
    /// resizing fires many bounds events, and each must not hit the disk.
    window_save_pending: bool,
    /// Bumped whenever anything that feeds a [`Theme`] changes. [`Themed`]
    /// skips its recompute + notify while this is unchanged, so changing a
    /// scan root doesn't re-theme every view.
    appearance_revision: u64,
}

impl ConfigState {
    /// Build the state over an already-loaded [`Config`], remembering where to
    /// persist changes.
    pub fn new(config: Config, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self { config, path, dynamic_accent: None, window_save_pending: false, appearance_revision: 0 }
    }

    // --- read-only accessors: what views render from -------------------

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn theme_kind(&self) -> ThemeKind {
        self.config.theme
    }

    /// What the dynamic theme builds on — only meaningful while the theme is
    /// [`ThemeKind::Dynamic`].
    pub fn dynamic_base(&self) -> DynamicBase {
        self.config.dynamic_base
    }

    /// Bumped whenever the theme, dynamic base, accent, typeface or font size
    /// changes. [`Themed`] uses it to skip needless re-renders on unrelated
    /// config changes (scan roots, visualizer, window placement).
    pub fn appearance_revision(&self) -> u64 {
        self.appearance_revision
    }

    /// The base the lyrics panel's procedural texture should be drawn on.
    ///
    /// It follows the effective palette rather than just the dynamic-base
    /// setting: the static Dark theme gets the black texture and Light the
    /// white one, while Dynamic uses its configured black/white base.
    pub fn texture_base(&self) -> DynamicBase {
        match self.config.theme {
            ThemeKind::Light => DynamicBase::White,
            ThemeKind::Dark => DynamicBase::Black,
            ThemeKind::Dynamic => self.config.dynamic_base,
        }
    }

    /// The theme for `container`, at its configured (or built-in) font size and
    /// typeface.
    pub fn theme_for(&self, container: &str, default_size: f32) -> Theme {
        let font = self.config.font_for(container, FontKind::default()).family();
        let size = self.config.font_size_for(container, default_size);
        match self.config.theme {
            // The dynamic palette is derived from the current cover; until one
            // has been analysed it falls back to the base palette (black or
            // white, per the setting).
            ThemeKind::Dynamic => match self.dynamic_accent {
                Some(accent) => Theme::dynamic(accent, self.config.dynamic_base, font, size),
                None => Theme::for_kind(self.config.dynamic_base.into(), font, size),
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

    /// Whether the spectrum-bar band above the lyrics is shown.
    pub fn visualizer(&self) -> bool {
        self.config.visualizer
    }

    /// When the lyrics panel draws its procedural texture.
    pub fn lyrics_texture(&self) -> TextureMode {
        self.config.lyrics_texture
    }

    /// Whether the left library rail is collapsed to its header button.
    pub fn sidebar_minimized(&self) -> bool {
        self.config.sidebar_minimized
    }

    // --- actions -------------------------------------------------------

    pub fn set_theme(&mut self, kind: ThemeKind, cx: &mut Context<Self>) {
        if self.config.theme != kind {
            self.config.theme = kind;
            self.appearance_revision += 1;
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

    /// Show or hide the visualizer band above the lyrics. The workspace
    /// observes this and adds/removes the container from the layout.
    pub fn set_visualizer(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.config.visualizer != on {
            self.config.visualizer = on;
            self.persist();
            cx.notify();
        }
    }

    /// When the lyrics panel draws its procedural cover-art texture.
    pub fn set_lyrics_texture(&mut self, mode: TextureMode, cx: &mut Context<Self>) {
        if self.config.lyrics_texture != mode {
            self.config.lyrics_texture = mode;
            self.persist();
            cx.notify();
        }
    }

    /// Collapse or expand the left library rail. The workspace observes this
    /// and slides the rail to its new width.
    pub fn set_sidebar_minimized(&mut self, minimized: bool, cx: &mut Context<Self>) {
        if self.config.sidebar_minimized != minimized {
            self.config.sidebar_minimized = minimized;
            self.persist();
            cx.notify();
        }
    }

    /// Set the accent the dynamic theme derives from. A no-op when unchanged, so
    /// a per-frame caller can't churn the whole UI.
    pub fn set_dynamic_accent(&mut self, accent: Option<Rgba>, cx: &mut Context<Self>) {
        if self.dynamic_accent != accent {
            self.dynamic_accent = accent;
            self.appearance_revision += 1;
            cx.notify();
        }
    }

    /// Set what the dynamic theme builds on. Only read while the theme is
    /// [`ThemeKind::Dynamic`], but persisted either way.
    pub fn set_dynamic_base(&mut self, base: DynamicBase, cx: &mut Context<Self>) {
        if self.config.dynamic_base != base {
            self.config.dynamic_base = base;
            self.appearance_revision += 1;
            self.persist();
            cx.notify();
        }
    }

    /// Set (or clear, with `None`) a container's typeface override.
    pub fn set_font(&mut self, container: &str, font: Option<FontKind>, cx: &mut Context<Self>) {
        if self.config.font_override(container) == font {
            return;
        }
        self.config.set_font(container, font);
        self.appearance_revision += 1;
        self.persist();
        cx.notify();
    }

    pub fn set_font_size(&mut self, container: &str, size: f32, cx: &mut Context<Self>) {
        if self.config.font_size_override(container) == Some(size) {
            return;
        }
        self.config.set_font_size(container, Some(size));
        self.appearance_revision += 1;
        self.persist();
        cx.notify();
    }

    /// Clear a container's override, restoring its built-in default.
    pub fn reset_font_size(&mut self, container: &str, cx: &mut Context<Self>) {
        if self.config.font_size_override(container).is_some() {
            self.config.set_font_size(container, None);
            self.appearance_revision += 1;
            self.persist();
            cx.notify();
        }
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

    /// Record where the window is. The in-memory config updates immediately;
    /// the disk write is debounced, since a resize drag fires many events.
    pub fn set_window_placement(&mut self, placement: WindowPlacement, cx: &mut Context<Self>) {
        if !self.config.set_window(placement) {
            return;
        }
        if self.window_save_pending {
            return;
        }
        self.window_save_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            this.update(cx, |state, _cx| {
                state.window_save_pending = false;
                state.persist();
            })
            .ok();
        })
        .detach();
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
        Self::with_container(C::container_id(), C::default_font_size(), config, cx)
    }

    /// Resolve for an explicit container, rather than the calling view's own —
    /// for views that deliberately share another container's typography. The
    /// playlist and search tabs render the library's rows, so they read the
    /// browse list's typeface and size and can't drift from it.
    pub fn with_container<C: 'static>(
        container: &'static str,
        default_size: f32,
        config: &Entity<ConfigState>,
        cx: &mut Context<C>,
    ) -> Self {
        let theme = Rc::new(Cell::new(config.read(cx).theme_for(container, default_size)));
        let cell = theme.clone();
        let last_revision = Rc::new(Cell::new(config.read(cx).appearance_revision()));
        let seen = last_revision.clone();
        let config = config.clone();
        let _config = cx.observe(&config, move |_this, config, cx| {
            let state = config.read(cx);
            let revision = state.appearance_revision();
            if seen.get() == revision {
                return;
            }
            seen.set(revision);
            cell.set(state.theme_for(container, default_size));
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
