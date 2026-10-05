//! The app's workspace layout, defined once.
//!
//! Both the real UI (`ui/main.rs`) and the layout-introspection tooling
//! (`tests/layout.rs`) build from [`app_layout`], so the text diagram can
//! never drift from what the app actually renders.

use super::container::{col, module, Dock, LayoutPlan};
use super::cover::CoverView;
use super::introspect::{self, ModuleSizes};
use super::visualizer::BAND_HEIGHT;

/// Width of the side rails — the playlists dock left, the lyrics/cover rail
/// right — in px. The queue panel's geometry is derived from it (see
/// `ui::tabs`), so the two can't drift apart.
pub const RAIL_PX: f32 = 240.0;

/// The window's dock/center arrangement.
///
/// Playback bar across the bottom, the playlist list docked left, and the
/// right rail carrying the lyrics panel, the visualizer band (when enabled),
/// and the cover square. The center is the tab container, which holds one
/// active container per tab — browse, search, or an opened playlist. The
/// rails carry z(3), so they hug their window edges for the full height of
/// the window; the playback bar (z=1) fits between the rails — keeping it
/// centered in the window instead of shifted left by the cover.
///
/// `show_visualizer` is the config's on/off switch: the band is only in the
/// layout when it's on, and the workspace re-plans live when the setting
/// flips (see `ui/main.rs`).
pub fn app_layout(show_visualizer: bool) -> LayoutPlan {
    let mut rail = Vec::new();
    rail.push(module("lyrics").fill());
    if show_visualizer {
        rail.push(module("visualizer").px(BAND_HEIGHT));
    }
    rail.push(module("cover"));

    let mut plan = LayoutPlan::new(module("tabs"));
    // The rails run the full window height (z=3), so the transport bar
    // (z=1) fits between them at the bottom.
    plan.dock(Dock::bottom(module("playback")));
    plan.dock(Dock::right(col(rail).px(RAIL_PX)).z(3));
    plan.dock(Dock::left(module("playlists").px(RAIL_PX)).z(3));
    plan
}

/// Intrinsic sizes for the fixed-size modules in [`app_layout`], so the text
/// diagram can size `Fit` bands accurately instead of guessing.
pub fn module_sizes() -> ModuleSizes {
    ModuleSizes::from([
        ("cover", CoverView::SIZE),
        ("playback", PLAYBACK_BAR_PX),
    ])
}

/// Nominal height of the transport bar. It's content-sized in the real
/// layout, so this is an approximation used only for the diagram.
const PLAYBACK_BAR_PX: f32 = 180.0;

/// The current app layout rendered as text — the one call a test (or an
/// LLM) needs to "see" the UI. See `tests/README.md`.
pub fn describe_app() -> String {
    introspect::describe(&app_layout(true), &module_sizes())
}
