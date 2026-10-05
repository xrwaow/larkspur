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
    for id in ["tabs", "playlists", "visualizer", "lyrics", "cover", "playback"] {
        assert!(names.iter().any(|n| n == id), "no region for {id}: {names:?}");
    }
}

/// The playback bar spans between the full-height rails, right down to the
/// window's bottom edge - there is no bar below it anymore.
#[test]
fn the_playback_bar_is_at_the_bottom_between_the_rails() {
    let report = report();
    assert!(touches(&report, "playback", "playlists"), "left rail reaches the bottom");
    assert!(touches(&report, "playback", "cover"), "right rail reaches the bottom");
    assert!(touches(&report, "playback", "window.bottom"), "playback hugs the bottom");
}

#[test]
fn left_rail_spans_the_full_window_height() {
    let report = report();
    assert!(touches(&report, "playlists", "window.top"), "left rail reaches the top");
    assert!(touches(&report, "playlists", "window.bottom"), "left rail reaches the bottom");
}

#[test]
fn right_rail_is_split_into_lyrics_visualizer_and_cover() {
    let report = report();
    // The lyrics panel hugs the top of the rail, the cover the bottom,
    // and the visualizer band sits between them.
    assert!(touches(&report, "lyrics", "window.top"));
    assert!(touches(&report, "lyrics", "visualizer"));
    assert!(touches(&report, "visualizer", "cover"));
    assert!(touches(&report, "visualizer", "window.right"));
    assert!(touches(&report, "cover", "window.bottom"), "cover reaches the bottom of the window");
}

#[test]
fn turning_the_visualizer_off_removes_its_band() {
    let report = report_without_visualizer();
    let names = names(&report);
    assert!(!names.iter().any(|n| n == "visualizer"), "no region for the visualizer: {names:?}");
    // With the band gone, the lyrics panel runs down to the cover.
    assert!(touches(&report, "lyrics", "window.top"));
    assert!(touches(&report, "lyrics", "cover"));
}

#[test]
fn playback_bar_sits_between_the_rails() {
    let report = report();
    // The bar's ends meet the rails...
    assert!(touches(&report, "playback", "playlists"), "bar meets the left rail");
    assert!(touches(&report, "playback", "cover"), "bar meets the right rail");
    // ...and it hugs the window's own bottom edge between the rails.
    assert!(touches(&report, "playback", "window.bottom"));
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
fn cover_is_bottom_right_with_the_visualizer_above_it() {
    let report = report();
    assert!(touches(&report, "cover", "window.bottom"), "cover is at the bottom");
    assert!(touches(&report, "cover", "window.right"), "cover is at the right");
    assert!(touches(&report, "cover", "visualizer"), "visualizer sits above cover");
}

#[test]
fn docks_resolve_outermost_first() {
    let report = report();
    let edges: Vec<String> = report
        .docks
        .iter()
        .map(|dock| format!("{:?}", dock.edge))
        .collect();
    // Higher z hugs the window edge, so the rails are outermost, then the
    // bottom bar.
    assert_eq!(edges, vec!["Right", "Left", "Bottom"]);
}

#[test]
fn describe_app_produces_a_readable_diagram() {
    let text = layout::describe_app();
    for id in ["playlists", "visualizer", "lyrics", "cover", "playback", "tabs"] {
        assert!(text.contains(id), "diagram should mention {id}:\n{text}");
    }
    assert!(text.contains('┌') && text.contains('─') && text.contains('│'), "diagram should be box-drawn:\n{text}");
}
