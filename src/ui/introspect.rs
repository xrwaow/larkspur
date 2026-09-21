//! Text introspection of a [`LayoutPlan`].
//!
//! Renders the resolved workspace layout as an ASCII box diagram, plus a
//! legend and an explicit border/adjacency list, so the layout can be
//! *read* — by a person in a terminal, or by an LLM in a test failure —
//! without launching a GPU window.
//!
//! Everything here is pure geometry over `LayoutPlan`; it needs no `App`
//! and no `Entity`, which is why `tests/layout.rs` can assert on it.
//!
//! ## What the diagram is and isn't
//!
//! It's a *schematic*: `Px` sizes are scaled into a fixed character grid
//! and the 8px inter-section gaps are omitted, so the picture shows
//! structure and relative arrangement, not pixel-exact geometry. `Fit`
//! bands are drawn at a nominal size (or a known module's declared size
//! from [`ModuleSizes`]), because a view's intrinsic size isn't knowable
//! without rendering it. The legend and border list carry the precise
//! declarations.

use std::collections::HashMap;

use super::container::{Child, Dock, Edge, Layout, LayoutPlan, Size};

/// Intrinsic sizes, in px, for modules whose view has a fixed size. Used
/// to size `Fit` children that are known modules (e.g. the 240px cover)
/// so the diagram doesn't render them as a sliver.
pub type ModuleSizes = HashMap<&'static str, f32>;

/// Nominal main-axis size, in px, for a `Fit` child we have no size for.
const NOMINAL_FIT_PX: f32 = 64.0;

/// The schematic window the diagram is drawn in, and its character grid.
const WINDOW_W: f32 = 1920.0;
const WINDOW_H: f32 = 1080.0;
const GRID_COLS: usize = 96;
const GRID_ROWS: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One leaf of the resolved layout: a module (or filler) occupying a
/// rectangle of the window.
#[derive(Clone, Debug)]
pub struct Region {
    /// Module id, or empty for a filler/gap.
    pub label: String,
    pub rect: Rect,
}

/// A pair of regions sharing an edge (or a region touching a window edge).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Border {
    pub a: String,
    pub b: String,
    /// Which side of `a` touches `b`.
    pub side: &'static str,
}

/// One dock, as declared (before resolution).
#[derive(Clone, Debug)]
pub struct DockInfo {
    pub edge: Edge,
    pub z: i32,
    pub size: String,
    pub modules: Vec<String>,
}

/// The structured form of the layout: what `describe` renders as text.
///
/// Tests should assert against this rather than parsing the diagram string.
#[derive(Clone, Debug)]
pub struct LayoutReport {
    pub docks: Vec<DockInfo>,
    pub center_modules: Vec<String>,
    pub regions: Vec<Region>,
    pub borders: Vec<Border>,
}

/// Resolve `plan` and return its structured report.
pub fn report(plan: &LayoutPlan, sizes: &ModuleSizes) -> LayoutReport {
    let mut regions = Vec::new();
    let window = Rect { x: 0.0, y: 0.0, w: WINDOW_W, h: WINDOW_H };
    resolve(plan, window, sizes, &mut regions);

    let docks = sorted_docks(plan)
        .into_iter()
        .map(|dock| DockInfo {
            edge: dock.edge,
            z: dock.z,
            size: size_label(dock.child.size()),
            modules: module_ids(dock.child.layout()),
        })
        .collect();

    LayoutReport {
        docks,
        center_modules: module_ids(&plan.center),
        borders: borders(&regions),
        regions,
    }
}

/// Render `plan` as human/LLM-readable text.
pub fn describe(plan: &LayoutPlan, sizes: &ModuleSizes) -> String {
    let report = report(plan, sizes);

    let mut out = String::new();
    out.push_str("Larkspur workspace — resolved layout\n");
    out.push_str(&format!(
        "schematic: {GRID_COLS}x{GRID_ROWS} cells ({}x{} px); gaps omitted; \
         Fit bands nominal\n\n",
        WINDOW_W as u32, WINDOW_H as u32,
    ));

    out.push_str(&render_diagram(&report.regions));
    out.push('\n');

    out.push_str("docks (outermost first — higher z hugs the window edge):\n");
    for dock in &report.docks {
        out.push_str(&format!(
            "  {:<6} z={} size={:<6} modules: {}\n",
            edge_name(dock.edge),
            dock.z,
            dock.size,
            join_or_none(&dock.modules),
        ));
    }
    out.push_str(&format!(
        "  {:<6}        modules: {}\n",
        "center",
        join_or_none(&report.center_modules),
    ));

    out.push_str("\nborders (a ↔ b: which region each side touches):\n");
    for border in &report.borders {
        out.push_str(&format!(
            "  {:<20} ↔ {:<20} ({})\n",
            border.a, border.b, border.side,
        ));
    }

    out
}

// ---------------------------------------------------------------------
// Resolution: LayoutPlan -> regions
// ---------------------------------------------------------------------

fn sorted_docks(plan: &LayoutPlan) -> Vec<&Dock> {
    let mut docks: Vec<&Dock> = plan.docks.iter().collect();
    // Same ordering the renderer uses: descending sort_key = outermost.
    docks.sort_by(|a, b| b.sort_key().cmp(&a.sort_key()));
    docks
}

fn resolve(plan: &LayoutPlan, window: Rect, sizes: &ModuleSizes, out: &mut Vec<Region>) {
    let mut rest = window;
    for dock in sorted_docks(plan) {
        let main = match dock.edge {
            Edge::Left | Edge::Right => rest.w,
            Edge::Top | Edge::Bottom => rest.h,
        };
        let thickness = main_extent(dock.child.size(), dock.child.layout(), main, sizes)
            .clamp(0.0, main);
        let (band, remaining) = split(rest, dock.edge, thickness);
        place(dock.child.layout(), band, sizes, out);
        rest = remaining;
    }
    place(&plan.center, rest, sizes, out);
}

fn split(rect: Rect, edge: Edge, thickness: f32) -> (Rect, Rect) {
    match edge {
        Edge::Left => (
            Rect { x: rect.x, y: rect.y, w: thickness, h: rect.h },
            Rect { x: rect.x + thickness, y: rect.y, w: rect.w - thickness, h: rect.h },
        ),
        Edge::Right => (
            Rect { x: rect.x + rect.w - thickness, y: rect.y, w: thickness, h: rect.h },
            Rect { x: rect.x, y: rect.y, w: rect.w - thickness, h: rect.h },
        ),
        Edge::Top => (
            Rect { x: rect.x, y: rect.y, w: rect.w, h: thickness },
            Rect { x: rect.x, y: rect.y + thickness, w: rect.w, h: rect.h - thickness },
        ),
        Edge::Bottom => (
            Rect { x: rect.x, y: rect.y + rect.h - thickness, w: rect.w, h: thickness },
            Rect { x: rect.x, y: rect.y, w: rect.w, h: rect.h - thickness },
        ),
    }
}

fn place(layout: &Layout, rect: Rect, sizes: &ModuleSizes, out: &mut Vec<Region>) {
    match layout {
        Layout::Module(id) => out.push(Region { label: (*id).to_string(), rect }),
        Layout::Gap => out.push(Region { label: String::new(), rect }),
        Layout::Row(children) => place_axis(children, rect, true, sizes, out),
        Layout::Col(children) => place_axis(children, rect, false, sizes, out),
    }
}

fn place_axis(
    children: &[Child],
    rect: Rect,
    horizontal: bool,
    sizes: &ModuleSizes,
    out: &mut Vec<Region>,
) {
    let main = if horizontal { rect.w } else { rect.h };
    let extents = distribute(children, main, sizes);

    let mut offset = 0.0;
    for (child, extent) in children.iter().zip(extents) {
        let sub = if horizontal {
            Rect { x: rect.x + offset, y: rect.y, w: extent, h: rect.h }
        } else {
            Rect { x: rect.x, y: rect.y + offset, w: rect.w, h: extent }
        };
        place(child.layout(), sub, sizes, out);
        offset += extent;
    }
}

/// Split `main` px among `children` by their size policy. `Fill` children
/// share whatever the fixed ones leave.
fn distribute(children: &[Child], main: f32, sizes: &ModuleSizes) -> Vec<f32> {
    let mut extents: Vec<f32> = children
        .iter()
        .map(|child| match child.size() {
            Size::Fill => 0.0,
            size => main_extent(size, child.layout(), main, sizes),
        })
        .collect();

    let fill_count = children
        .iter()
        .filter(|child| matches!(child.size(), Size::Fill))
        .count();
    if fill_count > 0 {
        let used: f32 = extents.iter().sum();
        let per = ((main - used) / fill_count as f32).max(0.0);
        for (extent, child) in extents.iter_mut().zip(children) {
            if matches!(child.size(), Size::Fill) {
                *extent = per;
            }
        }
    }

    extents
}

fn main_extent(size: Size, layout: &Layout, main: f32, sizes: &ModuleSizes) -> f32 {
    match size {
        Size::Px(v) => v,
        Size::Fraction(f) => f * main,
        // A `Fill` child only ever gets a real extent via `distribute`;
        // when it's the whole band (a dock), half is a sane stand-in.
        Size::Fill => main * 0.5,
        Size::Fit => match layout {
            Layout::Module(id) => sizes.get(id).copied().unwrap_or(NOMINAL_FIT_PX),
            _ => NOMINAL_FIT_PX,
        },
    }
}

// ---------------------------------------------------------------------
// Borders / adjacency
// ---------------------------------------------------------------------

fn borders(regions: &[Region]) -> Vec<Border> {
    let mut out = Vec::new();

    for (i, region) in regions.iter().enumerate() {
        let (x0, y0, x1, y1) = cell_bounds(region.rect);
        let name = display_name(region);

        if x0 == 0 {
            out.push(Border { a: name.clone(), b: "window.left".into(), side: "left" });
        }
        if x1 >= GRID_COLS - 1 {
            out.push(Border { a: name.clone(), b: "window.right".into(), side: "right" });
        }
        if y0 == 0 {
            out.push(Border { a: name.clone(), b: "window.top".into(), side: "top" });
        }
        if y1 >= GRID_ROWS - 1 {
            out.push(Border { a: name.clone(), b: "window.bottom".into(), side: "bottom" });
        }

        for other in regions.iter().skip(i + 1) {
            if let Some(side) = adjacency(region, other) {
                out.push(Border { a: name.clone(), b: display_name(other), side });
            }
        }
    }

    out
}

/// Which side of `a` touches `b`, if they share an edge. Adjacency is
/// checked in the character grid, so regions that merely touch within
/// rounding still count.
fn adjacency(a: &Region, b: &Region) -> Option<&'static str> {
    let (ax0, ay0, ax1, ay1) = cell_bounds(a.rect);
    let (bx0, by0, bx1, by1) = cell_bounds(b.rect);

    let overlaps_x = ax0.max(bx0) <= ax1.min(bx1);
    let overlaps_y = ay0.max(by0) <= ay1.min(by1);

    if overlaps_x {
        if ay1 == by0 {
            return Some("bottom");
        }
        if by1 == ay0 {
            return Some("top");
        }
    }
    if overlaps_y {
        if ax1 == bx0 {
            return Some("right");
        }
        if bx1 == ax0 {
            return Some("left");
        }
    }
    None
}

fn display_name(region: &Region) -> String {
    if region.label.is_empty() {
        "(gap)".to_string()
    } else {
        region.label.clone()
    }
}

// ---------------------------------------------------------------------
// Diagram rendering
// ---------------------------------------------------------------------

/// Inclusive cell bounds `(x0, y0, x1, y1)` of a region in the character
/// grid. Adjacent regions share their border cell (`a.x1 == b.x0`), which is
/// what makes a shared edge render as a single line.
fn cell_bounds(rect: Rect) -> (usize, usize, usize, usize) {
    let scale_x = (GRID_COLS - 1) as f32 / WINDOW_W;
    let scale_y = (GRID_ROWS - 1) as f32 / WINDOW_H;
    let cell = |v: f32, max: usize| (v.round().max(0.0) as usize).min(max);
    (
        cell(rect.x * scale_x, GRID_COLS - 1),
        cell(rect.y * scale_y, GRID_ROWS - 1),
        cell((rect.x + rect.w) * scale_x, GRID_COLS - 1),
        cell((rect.y + rect.h) * scale_y, GRID_ROWS - 1),
    )
}

fn render_diagram(regions: &[Region]) -> String {
    // Mark every cell that sits on a region border. Adjacent regions share
    // their border cell, so a shared edge becomes one line and the junctions
    // (├ ┼ ┬ ...) fall out of the neighbour check below.
    let mut line = vec![vec![false; GRID_COLS]; GRID_ROWS];

    for region in regions {
        let (x0, y0, x1, y1) = cell_bounds(region.rect);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for c in x0..=x1 {
            line[y0][c] = true;
            line[y1][c] = true;
        }
        for r in y0..=y1 {
            line[r][x0] = true;
            line[r][x1] = true;
        }
    }

    let mut canvas: Vec<Vec<char>> = (0..GRID_ROWS)
        .map(|r| {
            (0..GRID_COLS)
                .map(|c| {
                    if !line[r][c] {
                        return ' ';
                    }
                    let up = r > 0 && line[r - 1][c];
                    let down = r + 1 < GRID_ROWS && line[r + 1][c];
                    let left = c > 0 && line[r][c - 1];
                    let right = c + 1 < GRID_COLS && line[r][c + 1];
                    box_char(up, down, left, right)
                })
                .collect()
        })
        .collect();

    for region in regions {
        let (x0, y0, x1, y1) = cell_bounds(region.rect);
        if region.label.is_empty() || x1 < x0 + 2 || y1 < y0 + 2 {
            continue;
        }
        let inner_w = x1 - x0 - 1;
        let inner_h = y1 - y0 - 1;
        let lines = wrap_label(&region.label, inner_w, inner_h);
        let start_y = y0 + 1 + inner_h.saturating_sub(lines.len()) / 2;

        for (li, text) in lines.iter().enumerate() {
            let ty = start_y + li;
            if ty >= y1 {
                break;
            }
            let len = text.chars().count().min(inner_w);
            let tx = x0 + 1 + (inner_w - len) / 2;
            for (i, ch) in text.chars().take(inner_w).enumerate() {
                canvas[ty][tx + i] = ch;
            }
        }
    }

    let mut out = String::new();
    for row in canvas {
        out.extend(row);
        out.push('\n');
    }
    out
}

/// Wrap a module id into at most `max_lines` lines of `width` cells,
/// preferring to break on `-` so `empty-above-cover` becomes
/// `empty / above / cover` rather than a hard-sliced `empty-abov / e-cover`.
fn wrap_label(label: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for segment in label.split('-') {
        let candidate = if current.is_empty() {
            segment.to_string()
        } else {
            format!("{current}-{segment}")
        };
        if candidate.chars().count() <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = segment.to_string();
        }
        if lines.len() == max_lines {
            break;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }

    lines.truncate(max_lines);
    lines
}

fn box_char(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (false, false, false, false) => ' ',
        (true, false, false, false) => '╵',
        (false, true, false, false) => '╷',
        (false, false, true, false) => '╴',
        (false, false, false, true) => '╶',
        (true, true, false, false) => '│',
        (false, false, true, true) => '─',
        (true, false, true, false) => '┘',
        (true, false, false, true) => '└',
        (false, true, true, false) => '┐',
        (false, true, false, true) => '┌',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, true, true, true) => '┼',
    }
}

// ---------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------

fn module_ids(layout: &Layout) -> Vec<String> {
    let mut ids = Vec::new();
    collect_modules(layout, &mut ids);
    ids
}

fn collect_modules(layout: &Layout, ids: &mut Vec<String>) {
    match layout {
        Layout::Module(id) => ids.push((*id).to_string()),
        Layout::Gap => {}
        Layout::Row(children) | Layout::Col(children) => {
            for child in children {
                collect_modules(child.layout(), ids);
            }
        }
    }
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Top => "top",
        Edge::Bottom => "bottom",
        Edge::Left => "left",
        Edge::Right => "right",
    }
}

fn size_label(size: Size) -> String {
    match size {
        Size::Fill => "fill".to_string(),
        Size::Fit => "fit".to_string(),
        Size::Px(v) => format!("{v:.0}px"),
        Size::Fraction(f) => format!("{:.0}%", f * 100.0),
    }
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::container::{col, module};

    fn plan() -> LayoutPlan {
        let mut plan = LayoutPlan::new(module("center"));
        plan.dock(Dock::bottom(module("bar")));
        plan.dock(Dock::right(col(vec![module("top_right").fill(), module("cover")]).px(240.0)).z(2));
        plan.dock(Dock::left(module("left").px(240.0)).z(2));
        plan
    }

    fn sizes() -> ModuleSizes {
        ModuleSizes::from([("cover", 240.0), ("bar", 180.0)])
    }

    fn names(report: &LayoutReport) -> Vec<String> {
        report.regions.iter().map(display_name).collect()
    }

    fn touches(report: &LayoutReport, a: &str, b: &str) -> bool {
        report
            .borders
            .iter()
            .any(|bd| (bd.a == a && bd.b == b) || (bd.a == b && bd.b == a))
    }

    #[test]
    fn every_declared_module_gets_a_region() {
        let report = report(&plan(), &sizes());
        let names = names(&report);
        for id in ["center", "bar", "left", "top_right", "cover"] {
            assert!(names.iter().any(|n| n == id), "missing region {id}: {names:?}");
        }
    }

    #[test]
    fn side_rails_span_the_full_height() {
        let report = report(&plan(), &sizes());
        // The left rail is a single panel, so it spans top to bottom.
        assert!(touches(&report, "left", "window.top"));
        assert!(touches(&report, "left", "window.bottom"));
        // The right rail is split: the filler reaches the top, the cover the
        // bottom, and the two meet in the middle.
        assert!(touches(&report, "top_right", "window.top"));
        assert!(touches(&report, "cover", "window.bottom"));
        assert!(touches(&report, "top_right", "cover"));
    }

    #[test]
    fn bottom_bar_sits_between_the_rails() {
        let report = report(&plan(), &sizes());
        assert!(touches(&report, "bar", "left"), "bar's left edge meets the left rail");
        assert!(touches(&report, "bar", "cover"), "bar's right edge meets the right rail");
        assert!(touches(&report, "bar", "window.bottom"));
        // ...and does *not* reach the window's own left/right edges.
        assert!(!touches(&report, "bar", "window.left"));
        assert!(!touches(&report, "bar", "window.right"));
    }

    #[test]
    fn dock_order_is_outermost_first() {
        let report = report(&plan(), &sizes());
        let order: Vec<&str> = report.docks.iter().map(|d| edge_name(d.edge)).collect();
        assert_eq!(order, vec!["right", "left", "bottom"]);
    }

    #[test]
    fn describe_mentions_every_module_and_draws_a_frame() {
        let text = describe(&plan(), &sizes());
        for id in ["center", "bar", "left", "top_right", "cover"] {
            assert!(text.contains(id), "describe() should mention {id}");
        }
        assert!(
            text.contains('┌') && text.contains('─') && text.contains('│'),
            "should draw box art:\n{text}",
        );
    }

    #[test]
    fn long_labels_wrap_on_hyphens() {
        assert_eq!(wrap_label("empty-above-cover", 10, 4), vec!["empty", "above", "cover"]);
        assert_eq!(wrap_label("cover", 10, 4), vec!["cover"]);
        assert_eq!(wrap_label("empty-above-cover", 10, 2), vec!["empty", "above"]);
    }

    #[test]
    fn fill_shares_what_fixed_children_leave() {
        let children = vec![module("a").px(100.0), module("b").fill()];
        let extents = distribute(&children, 400.0, &ModuleSizes::new());
        assert_eq!(extents, vec![100.0, 300.0]);
    }
}
