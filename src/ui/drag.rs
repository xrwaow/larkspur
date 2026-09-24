//! Grabbing a row and dragging it vertically — the reusable core of
//! drag-to-reorder lists.
//!
//! The tracker records geometry only: which row was grabbed, where the
//! pointer and the row were at the grab, and where the pointer is now. The
//! dragging view decides what that means — which row follows the pointer,
//! which rows part to make room, and what a drop does. A press becomes a drag
//! only after [`DRAG_THRESHOLD_PX`] of travel; below that it's a click, and
//! the row's own activation runs (the view checks [`Drag::is_drag`] before
//! treating the release's click as one).
//!
//! The ghost's motion is a chase spring aimed at the **drop slot** rather
//! than the raw pointer, so it always settles exactly inside a slot: the
//! surrounding rows part a full row for it, and what you see is where the
//! drop lands.

use gpui::{Bounds, Pixels, Point};

use crate::ui::animation::Spring;

/// How far the pointer must travel before a press counts as a drag, in px.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// The ghost's chase spring: stiff and critically damped, so it follows the
/// pointer closely without ever overshooting it. This is what turns the
/// pointer's bursty events into motion at the frame clock's [`SMOOTH_FPS`].
const DRAG_STIFFNESS: f32 = 1000.0;
const DRAG_DAMPING: f32 = 63.2; // 2 * sqrt(1000)

/// How many rows of approach a row's parting takes: it stays put until the
/// drag is this close to the slot boundary where it must be fully displaced,
/// then eases open, completing exactly at the boundary — so wherever the
/// drag comes to rest, the gap around the ghost is a full, unambiguous row.
const PART_WIDTH: f32 = 0.3;

/// A grab: the row, the pointer's y at the grab, and the row's window bounds
/// at that moment (its resting place and height).
#[derive(Clone, Copy)]
struct Grab {
    ix: usize,
    pointer_y: f32,
    row_top: f32,
    row_height: f32,
}

/// What a renderer needs to draw an in-flight drag: the grabbed row, the row
/// it would drop on, the row height, the pointer's travel, and the dragged
/// row's top in window coordinates.
#[derive(Clone, Copy)]
pub struct DragInfo {
    pub grabbed: usize,
    pub target: usize,
    pub row_height: f32,
    /// The pointer's travel since the grab, in px.
    pub dy: f32,
    /// The dragged row's top, in window coordinates.
    pub top: f32,
}

/// One row grab-and-drag, from press to release.
///
/// The tracker keeps the pointer's raw geometry — that's what the drop slot
/// is computed from — plus a chase spring on the *visual* travel, which the
/// view advances from the shared frame clock. The ghost and the parting rows
/// render from the spring, so they glide at [`SMOOTH_FPS`] between the
/// pointer's bursty events instead of jumping with them.
pub struct Drag {
    grab: Option<Grab>,
    current_y: f32,
    /// Whether the active or last grab travelled past the threshold.
    moved: bool,
    /// The visual travel, chasing the raw one.
    visual: Spring,
}

impl Default for Drag {
    fn default() -> Self {
        Self {
            grab: None,
            current_y: 0.0,
            moved: false,
            visual: Spring::with_params(0.0, DRAG_STIFFNESS, DRAG_DAMPING),
        }
    }
}

impl Drag {
    /// A left-button press on row `ix`: `pointer` is the event's position,
    /// `row` the row's window bounds (the list state's `bounds_for_item`).
    pub fn grab(&mut self, ix: usize, pointer: Point<Pixels>, row: Bounds<Pixels>) {
        self.grab = Some(Grab {
            ix,
            pointer_y: f32::from(pointer.y),
            row_top: f32::from(row.origin.y),
            row_height: f32::from(row.size.height),
        });
        self.current_y = f32::from(pointer.y);
        self.moved = false;
        self.visual.set(0.0);
    }

    /// A mouse move while grabbing. Returns whether the drag moved far enough
    /// to re-render.
    ///
    /// The ghost's spring is aimed at the *drop slot*, not the pointer: it
    /// glides into the nearest slot's exact position, so where it rests is
    /// where the drop lands — no half-open gap to guess at.
    pub fn drag(&mut self, y: Pixels, count: usize) -> bool {
        let Some(grab) = self.grab else { return false };
        let y = f32::from(y);
        if (y - self.current_y).abs() < 0.5 {
            return false;
        }
        self.current_y = y;
        let dy = y - grab.pointer_y;
        if dy.abs() > DRAG_THRESHOLD_PX {
            self.moved = true;
        }
        let slot = self.target_of(grab, count);
        // Signed: dragging up aims at a slot above the grab.
        self.visual
            .target((slot as isize - grab.ix as isize) as f32 * grab.row_height);
        self.moved
    }

    /// Advance the ghost's chase spring — the view's frame-clock observer
    /// calls this every [`SMOOTH_FPS`] frame. Returns whether it moved, so
    /// the view re-renders only while the drag is in motion.
    pub fn tick(&mut self, dt: f32) -> bool {
        self.visual.tick(dt)
    }

    /// The button was released: the drag is over. Returns the grabbed row and
    /// the row it would drop on — equal indices mean "no move".
    pub fn release(&mut self, count: usize) -> Option<(usize, usize)> {
        let grabbed = self.grab.take()?;
        self.visual.set(0.0);
        Some((grabbed.ix, self.target_of(grabbed, count)))
    }

    /// Forget the drag without a drop — e.g. the list changed under it.
    pub fn cancel(&mut self) {
        self.grab = None;
        self.visual.set(0.0);
    }

    /// The grabbed row, while the button is down.
    pub fn grabbed(&self) -> Option<usize> {
        self.grab.map(|grab| grab.ix)
    }

    /// Whether the button is down.
    pub fn is_active(&self) -> bool {
        self.grab.is_some()
    }

    /// Whether the active or last grab turned into a real drag — a view
    /// checks this before treating the release's click as activation.
    pub fn is_drag(&self) -> bool {
        self.moved
    }

    /// The drag to render, once it's a real one: rows part to make room and
    /// the view draws the grabbed row at [`DragInfo::top`]. The travel is the
    /// chase spring's value — the smoothed one the ghost renders from — while
    /// the drop slot comes from the pointer's raw position.
    pub fn info(&self, count: usize) -> Option<DragInfo> {
        let grab = self.grab?;
        if !self.moved {
            return None;
        }
        let dy = self.visual.value();
        Some(DragInfo {
            grabbed: grab.ix,
            target: self.target_of(grab, count),
            row_height: grab.row_height,
            dy,
            top: grab.row_top + dy,
        })
    }

    /// The row the dragged row would land on: the grab index shifted by the
    /// pointer's travel, in row heights, clamped to the list.
    fn target_of(&self, grab: Grab, count: usize) -> usize {
        let dy = self.current_y - grab.pointer_y;
        let shift = (dy / grab.row_height.max(1.0)).round() as isize;
        (grab.ix as isize + shift).clamp(0, count.max(1) as isize - 1) as usize
    }
}

/// How the row at `ix` moves for an in-flight `drag`: the grabbed row hides
/// (the view draws it following the pointer), and each row it has approached
/// parts open — staying put until the drag is [`PART_WIDTH`] rows from the
/// boundary where its displacement completes, then easing open so the gap is
/// a full row exactly when the drag reaches it. The ghost settles into that
/// gap, so what you see is where the drop lands.
pub fn row_shift(ix: usize, drag: &DragInfo) -> Option<f32> {
    if ix == drag.grabbed {
        return None;
    }
    let row_height = drag.row_height.max(1.0);
    // The ghost's travel in row heights — the spring-smoothed one it renders
    // from, which settles exactly on slot centres.
    let travelled = drag.dy / row_height;
    let shift = if ix > drag.grabbed {
        // Below the grab: parts upward, completing when the ghost reaches the
        // boundary just above this row's slot.
        let d = (ix - drag.grabbed) as f32;
        -row_height * part((d - 0.5) - travelled)
    } else {
        // Above the grab: parts downward, completing when the ghost reaches
        // the boundary just below this row's slot.
        let d = (drag.grabbed - ix) as f32;
        row_height * part(travelled + d - 0.5)
    };
    Some(shift)
}

/// How far a row has parted when the drag is `to_go` rows from the boundary
/// where its displacement completes: 0 until [`PART_WIDTH`] remains, then a
/// smoothstep to 1 exactly at the boundary.
fn part(to_go: f32) -> f32 {
    let t = (1.0 - to_go / PART_WIDTH).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
