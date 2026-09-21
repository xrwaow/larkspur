use std::collections::HashMap;

use gpui::{div, prelude::*, px, relative, rgb, AnyElement, AnyView, Context, Entity, Render, Window};

/// Base "class" for a UI module/panel. A Container is just a Render
/// impl with a stable identity, so the workspace can hold a
/// heterogeneous set of modules and place them into the layout by id
/// without knowing their concrete types.
pub trait Container: Render + Sized {
    fn container_id() -> &'static str;
}

/// Main-axis size of a layout child: width in a [`Layout::Row`],
/// height in a [`Layout::Col`] (or the band thickness of a [`Dock`]).
/// The cross axis always fills unless the content is fixed-size.
#[derive(Clone, Copy, Debug)]
pub enum Size {
    /// Grow to take the remaining space (`flex: 1`).
    Fill,
    /// Shrink to the content's natural size.
    Fit,
    /// Fixed pixels.
    Px(f32),
    /// Fraction of the parent's main-axis size (0.0–1.0).
    Fraction(f32),
}

/// A declarative layout subtree. Modules plug in by container id; rows
/// and columns nest arbitrarily.
#[derive(Debug)]
pub enum Layout {
    /// Slot filled by the module registered under this container id.
    Module(&'static str),
    /// Empty filler — takes up space, renders nothing.
    Gap,
    /// Horizontal stack of children.
    Row(Vec<Child>),
    /// Vertical stack of children.
    Col(Vec<Child>),
}

/// A [`Layout`] plus its main-axis size within the parent Row/Col.
#[derive(Debug)]
pub struct Child {
    layout: Layout,
    size: Size,
}

impl Child {
    pub fn new(layout: Layout) -> Self {
        Self { layout, size: Size::Fit }
    }

    pub fn fill(mut self) -> Self {
        self.size = Size::Fill;
        self
    }

    pub fn fit(mut self) -> Self {
        self.size = Size::Fit;
        self
    }

    pub fn px(mut self, v: f32) -> Self {
        self.size = Size::Px(v);
        self
    }

    pub fn fraction(mut self, v: f32) -> Self {
        self.size = Size::Fraction(v);
        self
    }

    /// The main-axis size policy of this child.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The layout subtree this child places.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

/// `Layout::Row` shorthand.
pub fn row(children: Vec<Child>) -> Child {
    Child::new(Layout::Row(children))
}

/// `Layout::Col` shorthand.
pub fn col(children: Vec<Child>) -> Child {
    Child::new(Layout::Col(children))
}

/// Slot for the module with the given container id.
pub fn module(id: &'static str) -> Child {
    Child::new(Layout::Module(id))
}

/// Empty filler.
pub fn gap() -> Child {
    Child::new(Layout::Gap)
}

/// The window edge a [`Dock`] attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Cross-axis placement of a dock's content within its band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

/// A module band docked to one window edge.
///
/// Docks are applied highest `z` first: higher z hugs the window edge
/// and wins contested corners; lower z docks fit into what remains.
/// Default z is 1 for everything, but side docks (Left/Right) carry a
/// −1 handicap against Top/Bottom docks — their "z=0 on the top/bottom"
/// — so by default bars span the full width and side panels span
/// between them. Give a side dock `z(2)` to let it keep its full height
/// and push the bars beside it instead.
#[derive(Debug)]
pub struct Dock {
    pub child: Child,
    pub edge: Edge,
    pub z: i32,
    pub align: Align,
}

impl Dock {
    pub fn top(child: Child) -> Self {
        Self { child, edge: Edge::Top, z: 1, align: Align::Start }
    }

    pub fn bottom(child: Child) -> Self {
        Self { child, edge: Edge::Bottom, z: 1, align: Align::Start }
    }

    pub fn left(child: Child) -> Self {
        Self { child, edge: Edge::Left, z: 1, align: Align::Start }
    }

    pub fn right(child: Child) -> Self {
        Self { child, edge: Edge::Right, z: 1, align: Align::Start }
    }

    pub fn z(mut self, z: i32) -> Self {
        self.z = z;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    /// Docks higher z first (outermost = hugs the window edge). Side
    /// docks are handicapped by 1 so top/bottom bars win contested
    /// corners by default.
    ///
    /// `pub(crate)` so the text introspection in `ui::introspect` resolves
    /// the layout in exactly the order the renderer does.
    pub(crate) fn sort_key(&self) -> (i32, i32) {
        let side = matches!(self.edge, Edge::Left | Edge::Right);
        (self.z - if side { 1 } else { 0 }, self.z)
    }
}

/// The root child's size is irrelevant — the workspace fills the window —
/// so a `row(...)`/`col(...)` shorthand converts straight into a Layout.
impl From<Child> for Layout {
    fn from(child: Child) -> Self {
        child.layout
    }
}

/// Gap between sections, so flush panels don't visually merge.
const SECTION_GAP: f32 = 8.0;

/// The pure layout description: docks plus a center, and nothing else.
///
/// Holding the layout as plain data (no `Entity`s, no `App`) is what lets
/// `ui::introspect` — and the tests in `tests/layout.rs` — resolve and
/// assert on the layout without opening a window.
#[derive(Debug)]
pub struct LayoutPlan {
    pub docks: Vec<Dock>,
    pub center: Layout,
}

impl LayoutPlan {
    pub fn new(center: impl Into<Layout>) -> Self {
        Self { docks: Vec::new(), center: center.into() }
    }

    /// Add an edge-docked band.
    pub fn dock(&mut self, dock: Dock) {
        self.docks.push(dock);
    }
}

/// Holds the layout plan and the module set, and renders them: edge-docked
/// bands first (highest z outermost), with the center layout in whatever
/// remains.
pub struct Workspace {
    plan: LayoutPlan,
    modules: HashMap<&'static str, AnyView>,
}

impl Workspace {
    pub fn new(center: impl Into<Layout>) -> Self {
        Self { plan: LayoutPlan::new(center), modules: HashMap::new() }
    }

    /// Build a workspace from a pre-built [`LayoutPlan`] (see
    /// `ui::layout::app_layout`), so the app and the layout-introspection
    /// tools share one definition of the layout.
    pub fn from_plan(plan: LayoutPlan) -> Self {
        Self { plan, modules: HashMap::new() }
    }

    /// Add an edge-docked band.
    pub fn dock(&mut self, dock: Dock) {
        self.plan.dock(dock);
    }

    /// The layout plan this workspace renders — the input to
    /// `ui::introspect`.
    pub fn plan(&self) -> &LayoutPlan {
        &self.plan
    }

    /// Register a module under its container id, ready to be placed by
    /// the docks/center.
    pub fn push<C: Container>(&mut self, entity: Entity<C>) {
        self.modules.insert(C::container_id(), entity.into());
    }

    /// Register a module under an explicit id rather than its type's
    /// [`Container::container_id`]. Needed for filler panels, where one
    /// placeholder type has to reserve several sections of the layout.
    pub fn push_as<C: Container>(&mut self, id: &'static str, entity: Entity<C>) {
        self.modules.insert(id, entity.into());
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x000000))
            .child(render_dock_stack(&self.plan.docks, &self.plan.center, &self.modules))
    }
}

/// Nests the docks (highest z outermost) around the center layout.
fn render_dock_stack(
    docks: &[Dock],
    center: &Layout,
    modules: &HashMap<&'static str, AnyView>,
) -> AnyElement {
    let mut sorted: Vec<&Dock> = docks.iter().collect();
    sorted.sort_by(|a, b| b.sort_key().cmp(&a.sort_key()));

    // Innermost: the center layout.
    let mut element = render_layout(center, modules);

    // Wrap docks from innermost (lowest z) to outermost (highest z).
    for dock in sorted.into_iter().rev() {
        let is_tb = matches!(dock.edge, Edge::Top | Edge::Bottom);

        // The remaining space after this dock's band.
        let rest = div()
            .overflow_hidden()
            .map(|d| if is_tb { d.w_full() } else { d.h_full() })
            .flex_1()
            .child(element);

        let band = render_band(dock, modules);

        // Left/Top docks hug the leading edge; Right/Bottom the trailing.
        let container = match dock.edge {
            Edge::Left | Edge::Right => div().flex().flex_row().gap(px(SECTION_GAP)).size_full(),
            Edge::Top | Edge::Bottom => div().flex().flex_col().gap(px(SECTION_GAP)).size_full(),
        };
        element = match dock.edge {
            Edge::Left | Edge::Top => container.child(band).child(rest),
            Edge::Right | Edge::Bottom => container.child(rest).child(band),
        }
        .into_any_element();
    }
    element
}

/// A dock's band: sized along the dock edge, full across, with the
/// content justified toward the docked edge and cross-aligned.
fn render_band(dock: &Dock, modules: &HashMap<&'static str, AnyView>) -> AnyElement {
    let is_tb = matches!(dock.edge, Edge::Top | Edge::Bottom);

    let d = div()
        .overflow_hidden()
        .map(|d| if is_tb { d.w_full() } else { d.h_full() })
        .map(|d| match dock.child.size {
            Size::Fill => d.flex_1(),
            Size::Fit => d,
            Size::Px(v) => {
                if is_tb {
                    d.h(px(v))
                } else {
                    d.w(px(v))
                }
            }
            Size::Fraction(f) => {
                if is_tb {
                    d.h(relative(f))
                } else {
                    d.w(relative(f))
                }
            }
        })
        .flex()
        // Hug the docked edge along the main axis...
        .map(|d| match dock.edge {
            Edge::Left => d.justify_start(),
            Edge::Right => d.justify_end(),
            Edge::Top => d.justify_start(),
            Edge::Bottom => d.justify_end(),
        })
        // ...and place along the cross axis.
        .map(|d| match dock.align {
            Align::Start => d.items_start(),
            Align::Center => d.items_center(),
            Align::End => d.items_end(),
        });

    d.child(render_layout(&dock.child.layout, modules))
        .into_any_element()
}

fn render_layout(layout: &Layout, modules: &HashMap<&'static str, AnyView>) -> AnyElement {
    match layout {
        // The view is the leaf — no wrapper. A wrapper with size_full
        // would resolve percentages against auto-height parents and
        // knock fixed-size modules out of position.
        Layout::Module(id) => modules
            .get(id)
            .cloned()
            .map(|view| view.into_any_element())
            .unwrap_or_else(|| div().into_any_element()),
        Layout::Gap => div().into_any_element(),
        Layout::Row(children) => {
            let mut d = div().flex().flex_row().gap(px(SECTION_GAP)).size_full();
            for child in children {
                d = d.child(render_child(child, true, modules));
            }
            d.into_any_element()
        }
        Layout::Col(children) => {
            let mut d = div().flex().flex_col().gap(px(SECTION_GAP)).size_full();
            for child in children {
                d = d.child(render_child(child, false, modules));
            }
            d.into_any_element()
        }
    }
}

fn render_child(
    child: &Child,
    is_row: bool,
    modules: &HashMap<&'static str, AnyView>,
) -> AnyElement {
    let d = div()
        .overflow_hidden()
        .map(|d| if is_row { d.h_full() } else { d.w_full() })
        .map(|d| match child.size {
            Size::Fill => d.flex_1(),
            Size::Fit => d,
            Size::Px(v) => {
                if is_row {
                    d.w(px(v))
                } else {
                    d.h(px(v))
                }
            }
            Size::Fraction(f) => {
                if is_row {
                    d.w(relative(f))
                } else {
                    d.h(relative(f))
                }
            }
        });
    d.child(render_layout(&child.layout, modules)).into_any_element()
}

/// A do-nothing placeholder panel, useful for reserving a section of
/// the layout before its real module exists.
pub struct EmptyView;

impl Container for EmptyView {
    fn container_id() -> &'static str {
        "empty"
    }
}

impl Render for EmptyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0x141414))
    }
}
