//! Layout tests, driven by the text-introspection tooling in
//! `larkspur::ui::introspect`.
//!
//! These assert on the *resolved* workspace layout — which panel sits where,
//! and which panels border which — without opening a GPU window. Run them
//! with `cargo test`; to eyeball the exact same picture, run
//! `cargo run --bin layout_dump`.
//!
//! See `tests/README.md` for the tooling and how to extend it.

use larkspur::ui::introspect::{self, LayoutReport};
use larkspur::ui::layout;

fn report() -> LayoutReport {
    introspect::report(&layout::app_layout(true), &layout::module_sizes())
}

fn report_without_visualizer() -> LayoutReport {
    introspect::report(&layout::app_layout(false), &layout::module_sizes())
}

fn names(report: &LayoutReport) -> Vec<String> {
    report.regions.iter().map(|region| region.label.clone()).collect()
}

/// Whether regions `a` and `b` share an edge (in either direction).
fn touches(report: &LayoutReport, a: &str, b: &str) -> bool {
    report
        .borders
        .iter()
        .any(|border| (border.a == a && border.b == b) || (border.a == b && border.b == a))
}

#[test]
fn every_module_in_the_layout_gets_a_region() {
    let report = report();
    let names = names(&report);
    for id in ["tabs", "playlists", "visualizer", "lyrics", "cover", "playback", "view_toggle"] {
        assert!(names.iter().any(|n| n == id), "no region for {id}: {names:?}");
    }
}

/// The rails stop at the view-toggle band — the thin strip docked below
/// everything else — rather than reaching the window's bottom edge.
#[test]
fn the_toggle_band_is_the_bottom_of_the_window() {
    let report = report();
    assert!(touches(&report, "view_toggle", "window.bottom"), "toggle band hugs the bottom");
    assert!(touches(&report, "view_toggle", "playback"), "playback sits directly above it");
    assert!(touches(&report, "view_toggle", "playlists"), "left rail stops at the band");
    assert!(touches(&report, "view_toggle", "cover"), "right rail stops at the band");
}

#[test]
fn left_rail_spans_the_full_window_height() {
    let report = report();
    assert!(touches(&report, "playlists", "window.top"), "left rail reaches the top");
    assert!(touches(&report, "playlists", "view_toggle"), "left rail reaches the toggle band");
}

#[test]
fn right_rail_is_split_into_visualizer_lyrics_and_cover() {
    let report = report();
    // The visualizer band hugs the top of the rail, the cover the bottom,
    // and the lyrics panel sits between them.
    assert!(touches(&report, "visualizer", "window.top"));
    assert!(touches(&report, "visualizer", "lyrics"));
    assert!(touches(&report, "lyrics", "cover"));
    assert!(touches(&report, "cover", "view_toggle"), "cover rides above the toggle band");
}

#[test]
fn turning_the_visualizer_off_removes_its_band() {
    let report = report_without_visualizer();
    let names = names(&report);
    assert!(!names.iter().any(|n| n == "visualizer"), "no region for the visualizer: {names:?}");
    // With the band gone, the lyrics panel is the top of the rail again.
    assert!(touches(&report, "lyrics", "window.top"));
    assert!(touches(&report, "lyrics", "cover"));
}

#[test]
fn playback_bar_sits_between_the_rails() {
    let report = report();
    // The bar's ends meet the rails...
    assert!(touches(&report, "playback", "playlists"), "bar meets the left rail");
    assert!(touches(&report, "playback", "cover"), "bar meets the right rail");
    // ...and it sits above the toggle band, without reaching the window's own
    // bottom edge — the toggle strip is between them.
    assert!(touches(&report, "playback", "view_toggle"));
    assert!(!touches(&report, "playback", "window.bottom"));
    assert!(!touches(&report, "playback", "window.left"));
    assert!(!touches(&report, "playback", "window.right"));
}

#[test]
fn center_is_flanked_by_both_rails_and_the_bar() {
    let report = report();
    assert!(touches(&report, "tabs", "playlists"), "center meets the left rail");
    assert!(touches(&report, "tabs", "lyrics"), "center meets the right rail");
    assert!(touches(&report, "tabs", "playback"), "center sits above the bar");
}

#[test]
fn cover_is_bottom_right_with_the_lyrics_above_it() {
    let report = report();
    assert!(touches(&report, "cover", "view_toggle"), "cover is at the bottom");
    assert!(touches(&report, "cover", "window.right"), "cover is at the right");
    assert!(touches(&report, "lyrics", "cover"), "lyrics sit above cover");
}

#[test]
fn docks_resolve_outermost_first() {
    let report = report();
    let edges: Vec<String> = report
        .docks
        .iter()
        .map(|dock| format!("{:?}", dock.edge))
        .collect();
    // Higher z hugs the window edge, so the toggle band is outermost, then
    // the right rail, then the left rail, then the bottom bar.
    assert_eq!(edges, vec!["Bottom", "Right", "Left", "Bottom"]);
}

#[test]
fn describe_app_produces_a_readable_diagram() {
    let text = layout::describe_app();
    for id in ["playlists", "visualizer", "lyrics", "cover", "playback", "tabs", "view_toggle"] {
        assert!(text.contains(id), "diagram should mention {id}:\n{text}");
    }
    assert!(text.contains('┌') && text.contains('─') && text.contains('│'), "diagram should be box-drawn:\n{text}");
}
