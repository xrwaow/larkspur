//! The app's workspace layout, defined once.
//!
//! Both the real UI (`ui/main.rs`) and the layout-introspection tooling
//! (`layout_dump`, `tests/layout.rs`) build from [`app_layout`], so the
//! text diagram can never drift from what the app actually renders.

use super::container::{col, module, Dock, LayoutPlan};
use super::cover::CoverView;
use super::introspect::{self, ModuleSizes};

/// The window's dock/center arrangement.
///
/// Playback bar across the bottom, the playlist list docked left, and the
/// right rail carrying the lyrics panel above the cover square. The center is
/// the tab container, which holds one active container per tab — browse,
/// search, or an opened playlist. The rails carry z(2), so they hug their
/// window edges at full height and the bottom bar (z=1) fits *between* them —
/// keeping it centered in the window instead of shifted left by the cover.
pub fn app_layout() -> LayoutPlan {
    let mut plan = LayoutPlan::new(module("tabs"));
    plan.dock(Dock::bottom(module("playback")));
    plan.dock(
        Dock::right(
            col(vec![
                module("lyrics").fill(),
                module("cover"),
            ])
            .px(240.0),
        )
        .z(2),
    );
    plan.dock(Dock::left(module("playlists").px(240.0)).z(2));
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
    introspect::describe(&app_layout(), &module_sizes())
}
