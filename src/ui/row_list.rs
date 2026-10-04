//! The row-selection machinery the list containers share.
//!
//! Browse, search, and playlist tabs all draw the same virtualized list of
//! album rows with the same selection and navigation behaviour: `↑`/`↓` move a
//! cursor and `enter` plays it, a plain click selects, `shift`+click extends a
//! range and `ctrl`+click toggles one, `pgup`/`pgdn` scroll a page. This owns
//! that state so each view only has to say what *activating* a row means (play
//! the album from here, play this playlist, …).
//!
//! The selection and hover highlights are **crossfades**, not swaps: a row
//! being highlighted fades in while the one it left fades out, over
//! [`HIGHLIGHT_FADE_SECS`]. Each row mid-fade carries its own progress, so
//! moving the pointer down a list doesn't drag every row it passed back to full
//! highlight. Both run on the shared frame clock, and only while something is
//! actually moving.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{px, Bounds, ListAlignment, ListState, Modifiers, Pixels, Window};

use crate::model::{InputAction, PlaylistId, SongId};
use crate::ui::albums::{
    self, AlbumSection, Columns, ListItem, RowSelection, MAX_ARTIST_CHARS, MAX_TITLE_CHARS,
};
use crate::ui::animation::Spring;
use crate::ui::marquee::{self, Marquee};
use crate::ui::theme::Theme;

/// Extra rows rendered above and below the viewport, so scrolling a little
/// doesn't pop rows (or their covers) in. This is a direct multiplier on the
/// per-frame cost: every hover or scroll re-renders the viewport *plus* this
/// band, so it's kept modest rather than generous. `pub(crate)` so the rail's
/// list in `ui::playlists` uses the same overdraw instead of a second copy.
pub(crate) const OVERDRAW_PX: f32 = 512.0;

/// Time constant of the eased scroll. Smaller is snappier.
const SCROLL_EASE_SECS: f32 = 0.01;

/// A scroll closer than this to its target is done.
const SCROLL_EPSILON: f32 = 0.1;

/// The furthest a scroll is animated. Beyond it (a jump to a row far outside the
/// overdraw band, with nothing on screen to measure against) the list snaps.
const SCROLL_MAX_ANIMATED_PX: f32 = 2000.0;

/// How far a burst of wheel events may build up before it's capped, so a fast
/// flick doesn't send the list coasting for a second afterwards.
const SCROLL_MAX_MOMENTUM_PX: f32 = 800.0;

/// Per-row highlight opacities, each easing toward its own target on a spring.
///
/// A single shared value can't do this: with several rows mid-fade each has its
/// own progress, and retargeting one must not rewind the others. A row in
/// motion is tracked here; a settled row is dropped, because its value is
/// implied by the selection (or hover) it belongs to.
#[derive(Default, Clone)]
struct Fades {
    rows: HashMap<usize, Spring>,
}

impl Fades {
    /// The in-flight opacity for `ix`, if it's currently moving.
    fn opacity(&self, ix: usize) -> Option<f32> {
        self.rows.get(&ix).map(Spring::value)
    }

    /// Aim `ix` at `target`. `from` seeds a row that isn't tracked yet — a
    /// settled row is dropped from the map, so its value was implied by the
    /// selection (or hover) rather than animated, and a fade starting from it
    /// has to say where it begins.
    fn aim(&mut self, ix: usize, from: f32, target: f32) {
        match self.rows.get_mut(&ix) {
            // Already in motion: just retarget, keeping the spring's velocity.
            Some(spring) => spring.target(target),
            None => {
                let mut spring = Spring::new(from);
                spring.target(target);
                self.rows.insert(ix, spring);
            }
        }
    }

    /// Advance every fade; returns whether anything moved.
    fn tick(&mut self, dt: f32) -> bool {
        let mut moved = false;
        self.rows.retain(|_, spring| {
            if spring.tick(dt) {
                moved = true;
            }
            // Only rows still in motion need tracking.
            spring.is_animating()
        });
        moved
    }
}

/// What a [`RowList`] made of an [`InputAction`].
pub enum RowAction {
    /// The view should play this playlist from this track index.
    Play(PlaylistId, usize),
    /// The list handled it; the view should re-render.
    Handled,
    /// Not this list's action — the view (or a parent) may handle it.
    Ignored,
}

/// A short eased scroll, so moving the selection or paging slides rather than
/// jumping. The remaining distance decays exponentially toward zero, which
/// reads as a fast settle rather than a linear glide.
#[derive(Default)]
struct SmoothScroll {
    /// Pixels still to travel; positive scrolls down.
    remaining: f32,
}

impl SmoothScroll {
    /// Aim at a scroll of `pixels` from where we are now.
    fn queue(&mut self, pixels: f32) {
        self.remaining = pixels;
    }

    /// Add to the target — for a continuous input like a wheel, where several
    /// events arrive in a burst and should build up rather than replace.
    fn accumulate(&mut self, pixels: f32) {
        self.remaining =
            (self.remaining + pixels).clamp(-SCROLL_MAX_MOMENTUM_PX, SCROLL_MAX_MOMENTUM_PX);
    }

    fn cancel(&mut self) {
        self.remaining = 0.0;
    }

    fn is_active(&self) -> bool {
        self.remaining.abs() > SCROLL_EPSILON
    }

    /// The pixels to scroll this frame.
    fn step(&mut self, dt: f32) -> f32 {
        if !self.is_active() {
            self.remaining = 0.0;
            return 0.0;
        }
        let t = 1.0 - (-dt / SCROLL_EASE_SECS).exp();
        let step = self.remaining * t;
        self.remaining -= step;
        step
    }
}

/// Which text cell of a row a hover belongs to. Each slides independently, so
/// hovering one doesn't drag the other along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cell {
    Title,
    Artist,
}

/// A snapshot of the list's highlight state, for the renderer.
///
/// Owned rather than borrowed so a `gpui::list` processor closure can capture
/// it. A row settled at full highlight is implied by the selection (or the
/// hovered index); only rows *in motion* carry an explicit opacity.
#[derive(Clone, Default)]
pub struct Highlight {
    selection: RowSelection,
    selection_fade: Fades,
    hovered: Option<usize>,
    hover_fade: Fades,
    /// The marquee per row cell, for the title/artist cells.
    marquees: HashMap<(usize, Cell), Marquee>,
}

impl Highlight {
    /// The selection highlight opacity for `ix`, if it's selected or fading.
    pub fn selection(&self, ix: usize) -> Option<f32> {
        self.selection_fade
            .opacity(ix)
            .or_else(|| self.selection.contains(ix).then_some(1.0))
    }

    /// The hover highlight opacity for `ix`, if it's hovered or fading.
    pub fn hover(&self, ix: usize) -> Option<f32> {
        self.hover_fade
            .opacity(ix)
            .or_else(|| (self.hovered == Some(ix)).then_some(1.0))
    }

    /// The in-flight marquee offset for `ix`'s `cell`, in px — 0 at rest. A
    /// settled, unhovered cell isn't tracked, so it reads as 0.
    pub fn marquee_offset(&self, ix: usize, cell: Cell) -> f32 {
        self.marquees.get(&(ix, cell)).map_or(0.0, Marquee::offset)
    }
}

/// The shared state behind a selectable, virtualized album list.
pub struct RowList {
    sections: Rc<Vec<AlbumSection>>,
    items: Rc<Vec<ListItem>>,
    /// The fixed title/artist column widths, measured once per font size.
    columns: Columns,
    list_state: ListState,
    /// The row count the list was last reset to.
    item_count: usize,
    selection: RowSelection,
    /// The keyboard cursor, as an item index.
    cursor: Option<usize>,
    /// Where up/down step from when the cursor is `None` — the playing song's
    /// row, so navigation after a play (or an autoplay advance) picks up
    /// relative to it. Refreshed by the view after each rebuild.
    anchor: Option<usize>,
    /// The selection crossfade, and the hover crossfade.
    selection_fade: Fades,
    hover_fade: Fades,
    /// The marquee, per row cell — a constant-speed slide out, a sprung snap
    /// back.
    marquees: HashMap<(usize, Cell), Marquee>,
    /// The row the pointer is over.
    hovered: Option<usize>,
    /// The eased scroll in flight.
    scroll: SmoothScroll,
    /// Set when the sections need rebuilding from the library.
    dirty: bool,
    /// The font size the columns were measured at, so a font change re-measures.
    measured_at: f32,
    /// Flat mode: sections flatten to track rows only, no album headers — the
    /// queue panel's list, where every entry is a song.
    flat: bool,
    /// The title/artist column character counts a flat list was asked to size
    /// to (its panel's width, not the tables' fixed 48).
    title_chars: usize,
    artist_chars: usize,
    /// Set when the character counts moved since the columns were measured.
    columns_stale: bool,
    /// Slim mode: rows omit the bitrate and duration columns — the queue
    /// panel, whose width is better spent on the text columns.
    slim: bool,
}

impl RowList {
    pub fn new() -> Self {
        Self {
            sections: Rc::new(Vec::new()),
            items: Rc::new(Vec::new()),
            columns: Columns {
                title: 0.0,
                artist: 0.0,
                title_chars: MAX_TITLE_CHARS,
                artist_chars: MAX_ARTIST_CHARS,
            },
            list_state: ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX)),
            item_count: 0,
            selection: RowSelection::default(),
            cursor: None,
            anchor: None,
            selection_fade: Fades::default(),
            hover_fade: Fades::default(),
            marquees: HashMap::new(),
            hovered: None,
            scroll: SmoothScroll::default(),
            dirty: true,
            measured_at: 0.0,
            flat: false,
            title_chars: MAX_TITLE_CHARS,
            artist_chars: MAX_ARTIST_CHARS,
            columns_stale: false,
            slim: false,
        }
    }

    /// Mark the sections stale, so the next [`sync`](Self::sync) rebuilds them.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Flatten to track rows only — no album headers. The queue panel's list.
    pub fn set_flat(&mut self) {
        self.flat = true;
        self.mark_dirty();
    }

    /// Size a flat list's columns to explicit character counts — its
    /// container's width rather than the tables' fixed 48. Takes effect on
    /// the next [`sync`](Self::sync).
    pub fn set_columns_chars(&mut self, title: usize, artist: usize) {
        if !self.columns_stale && self.title_chars == title && self.artist_chars == artist {
            return;
        }
        self.title_chars = title;
        self.artist_chars = artist;
        self.columns_stale = true;
    }

    /// Omit the bitrate and duration columns — the queue panel's rows.
    pub fn set_slim(&mut self) {
        self.slim = true;
    }

    /// Whether rows omit the bitrate and duration columns.
    pub fn slim(&self) -> bool {
        self.slim
    }

    /// Rebuild the rows from `sections` when given, and re-measure the columns
    /// when the font size moved. Passing `None` skips the rebuild — the view
    /// only computes fresh sections when [`is_dirty`](Self::is_dirty).
    pub fn sync(
        &mut self,
        theme: Theme,
        window: &Window,
        sections: Option<Vec<AlbumSection>>,
    ) {
        if let Some(sections) = sections {
            self.dirty = false;
            self.sections = Rc::new(sections);
            self.selection.clear();
            self.selection_fade = Fades::default();
            // Row indices shift on a rebuild, so drop the per-row marquee state
            // rather than let a stale slide land on the wrong row.
            self.marquees.clear();
            self.hovered = None;
            // A fresh list opens with nothing selected — the playing row's own
            // highlight is the only thing lit until the user navigates.
            self.cursor = None;
            self.rebuild_rows(theme, window);
        } else if self.columns_stale || (self.measured_at - theme.font_size).abs() > 0.01 {
            self.measure_columns(theme, window);
            self.columns_stale = false;
        }
    }

    fn rebuild_rows(&mut self, theme: Theme, window: &Window) {
        let items = if self.flat {
            albums::flatten_tracks(&self.sections)
        } else {
            albums::flatten(&self.sections)
        };
        if self.item_count != items.len() {
            self.list_state.reset(items.len());
            self.item_count = items.len();
        }
        self.items = Rc::new(items);
        self.measure_columns(theme, window);
    }

    fn measure_columns(&mut self, theme: Theme, window: &Window) {
        self.columns = if self.flat {
            Columns::sized(window, theme, self.title_chars, self.artist_chars)
        } else {
            Columns::measure(window, theme)
        };
        self.measured_at = theme.font_size;
    }

    // --- accessors the renderer needs ----------------------------------

    pub fn sections(&self) -> Rc<Vec<AlbumSection>> {
        self.sections.clone()
    }

    pub fn items(&self) -> Rc<Vec<ListItem>> {
        self.items.clone()
    }

    pub fn columns(&self) -> Columns {
        self.columns
    }

    pub fn list_state(&self) -> &ListState {
        &self.list_state
    }

    /// The window-space bounds of row `ix`, if it's been laid out — the
    /// drag-to-reorder grab needs the row's resting place and height.
    pub fn item_bounds(&self, ix: usize) -> Option<Bounds<Pixels>> {
        self.list_state.bounds_for_item(ix)
    }

    /// A snapshot of the highlight state for the renderer.
    pub fn highlight(&self) -> Highlight {
        Highlight {
            selection: self.selection.clone(),
            selection_fade: self.selection_fade.clone(),
            hovered: self.hovered,
            hover_fade: self.hover_fade.clone(),
            marquees: self.marquees.clone(),
        }
    }

    // --- interaction ---------------------------------------------------

    /// Point the no-cursor anchor at `ix` — the playing song's row, per the
    /// view's playback state. Called after [`sync`](Self::sync), once the row
    /// indices are current.
    pub fn set_anchor(&mut self, ix: Option<usize>) {
        self.anchor = ix;
    }

    /// Drop the selection and the cursor — how a play leaves the list. The
    /// playing row carries the highlight from here, and up/down re-anchor
    /// from it, so an autoplay advance needs no bookkeeping of its own.
    pub fn clear_selection(&mut self) {
        if self.selection.is_empty() && self.cursor.is_none() {
            return;
        }
        let previous = std::mem::take(&mut self.selection);
        self.cursor = None;
        self.restart_selection_fade(&previous);
    }

    /// Apply a row click. Returns `true` when it was a selection gesture
    /// (`shift`/`ctrl`), which the view shouldn't also read as "play this".
    pub fn click(&mut self, item_ix: usize, modifiers: Modifiers) -> bool {
        let previous = self.selection.clone();
        let selection_gesture = if modifiers.shift {
            self.selection.extend_to(item_ix, &self.items);
            true
        } else if modifiers.control || modifiers.platform {
            self.selection.toggle(item_ix);
            true
        } else {
            self.cursor = Some(item_ix);
            self.selection.set_single(item_ix);
            false
        };
        if selection_gesture {
            self.cursor = Some(item_ix);
        }
        self.restart_selection_fade(&previous);
        selection_gesture
    }

    /// Ensure the right-clicked row is part of the selection, then return the
    /// songs the resulting menu should act on.
    pub fn context_songs(&mut self, item_ix: usize) -> Vec<SongId> {
        if !self.selection.contains(item_ix) {
            let previous = self.selection.clone();
            self.cursor = Some(item_ix);
            self.selection.set_single(item_ix);
            self.restart_selection_fade(&previous);
        }
        self.selection.songs(&self.items, &self.sections)
    }

    /// Note that `item_ix`'s hover state changed. Returns whether anything the
    /// renderer draws changed.
    pub fn set_hover(&mut self, item_ix: usize, hovered: bool) -> bool {
        if hovered {
            if self.hovered == Some(item_ix) {
                return false;
            }
            if let Some(previous) = self.hovered.replace(item_ix) {
                self.hover_fade.aim(previous, 1.0, 0.0);
            }
            self.hover_fade.aim(item_ix, 0.0, 1.0);
        } else {
            let was_hovered = self.hovered == Some(item_ix);
            if was_hovered {
                self.hovered = None;
            }
            // GPUI reports an exit for every row the pointer leaves, including
            // rows that were never highlighted; only react to a real one.
            if !was_hovered && self.hover_fade.opacity(item_ix).is_none() {
                return false;
            }
            self.hover_fade.aim(item_ix, 1.0, 0.0);
        }
        true
    }

    /// Note that `item_ix`'s `cell` hover state changed. Returns whether
    /// anything the renderer draws changed. Each cell slides on its own, so
    /// hovering the title doesn't drag the artist along.
    pub fn set_cell_hover(&mut self, item_ix: usize, cell: Cell, hovered: bool) -> bool {
        if !hovered {
            // As with the row hover, GPUI reports an exit for cells the pointer
            // never entered; only react to a real one.
            let Some(marquee) = self.marquees.get_mut(&(item_ix, cell)) else {
                return false;
            };
            return marquee.set_hovered(false, 0.0);
        }
        let travel = self.cell_travel(item_ix, cell);
        self.marquees.entry((item_ix, cell)).or_default().set_hovered(true, travel)
    }

    /// How far the row at `ix`'s `cell` text overflows its column, in px.
    fn cell_travel(&self, ix: usize, cell: Cell) -> f32 {
        let Some(item) = self.items.get(ix) else { return 0.0 };
        let section = &self.sections[item.section()];
        let track = match item {
            ListItem::Track { index, .. } => section.tracks.get(*index),
            ListItem::Compact(_) => section.tracks.first(),
            ListItem::Header(_) => None,
        };
        let Some(track) = track else { return 0.0 };
        match cell {
            Cell::Title => marquee::travel_for(&track.title, self.columns.title, self.title_chars),
            Cell::Artist => {
                marquee::travel_for(&track.artist, self.columns.artist, self.artist_chars)
            }
        }
    }

    /// Apply a wheel event's vertical delta, in GPUI's sign convention (a
    /// positive `y` scrolls up).
    ///
    /// The list is driven by the eased [`SmoothScroll`] rather than jumping, so
    /// a burst of events accumulates into the target and settles instead of
    /// stepping. The caller stops the event's propagation, or `gpui::list` would
    /// apply it itself and the two would fight.
    pub fn wheel(&mut self, delta_y: f32) -> bool {
        if delta_y == 0.0 {
            return false;
        }
        self.scroll.accumulate(-delta_y);
        true
    }

    /// Handle the list's own navigation actions.
    pub fn handle(&mut self, action: InputAction) -> RowAction {
        match action {
            InputAction::SelectNext => {
                self.move_cursor(true);
                RowAction::Handled
            }
            InputAction::SelectPrev => {
                self.move_cursor(false);
                RowAction::Handled
            }
            InputAction::PageDown => {
                self.page(1.0);
                RowAction::Handled
            }
            InputAction::PageUp => {
                self.page(-1.0);
                RowAction::Handled
            }
            InputAction::Activate => match self.activate_target() {
                Some((playlist, index)) => RowAction::Play(playlist, index),
                None => RowAction::Handled,
            },
            _ => RowAction::Ignored,
        }
    }

    /// Advance the highlight fades, the marquee slides, and the scroll. Returns
    /// whether anything moved, so the view only re-renders while something is
    /// animating.
    pub fn tick(&mut self, dt: f32) -> bool {
        let selection = self.selection_fade.tick(dt);
        let hover = self.hover_fade.tick(dt);
        let marquee = self.tick_marquees(dt);
        let scrolled = self.apply_scroll(dt);
        selection || hover || marquee || scrolled
    }

    /// Advance every marquee, dropping the settled ones. A settled marquee
    /// whose cell is hovered is kept, so the renderer can read its offset
    /// without recomputing the travel.
    fn tick_marquees(&mut self, dt: f32) -> bool {
        let mut moved = false;
        self.marquees.retain(|_, marquee| {
            if marquee.tick(dt) {
                moved = true;
            }
            marquee.is_animating() || marquee.is_hovered()
        });
        moved
    }

    fn apply_scroll(&mut self, dt: f32) -> bool {
        if !self.scroll.is_active() {
            return false;
        }
        let step = self.scroll.step(dt);
        if step != 0.0 {
            self.list_state.scroll_by(px(step));
        }
        true
    }

    fn move_cursor(&mut self, forward: bool) {
        // With no cursor — a fresh list, or one whose play just cleared it —
        // step from the anchor (the playing row) instead of the list's ends.
        let origin = self.cursor.or(self.anchor);
        let Some(next) = albums::step_selectable(&self.items, origin, forward) else { return };
        let previous = self.selection.clone();
        self.cursor = Some(next);
        self.selection.set_single(next);
        self.reveal(next);
        self.restart_selection_fade(&previous);
    }

    /// The list's viewport height, in px — 0 before the first layout, so a
    /// caller can wait to scroll until the panel is actually sized.
    pub fn viewport_height(&self) -> f32 {
        f32::from(self.list_state.viewport_bounds().size.height)
    }

    /// Bring the playing row into view. See [`RowList::reveal`].
    pub fn reveal(&mut self, ix: usize) {
        self.reveal_row(ix);
    }

    /// Scroll the list by a page, eased.
    fn page(&mut self, direction: f32) {
        let height = f32::from(self.list_state.viewport_bounds().size.height);
        if height > 0.0 {
            self.scroll.queue(direction * height);
        }
    }

    /// Bring `ix` into view, sliding rather than jumping.
    ///
    /// The distance is measured from the row's current on-screen bounds, which
    /// only exist for rows inside the overdraw band. A jump to a row outside it
    /// has nothing to measure against, so that one snaps.
    fn reveal_row(&mut self, ix: usize) {
        match self.reveal_delta(ix) {
            Some(delta) if delta.abs() <= SCROLL_MAX_ANIMATED_PX => self.scroll.queue(delta),
            _ => {
                self.scroll.cancel();
                self.list_state.scroll_to_reveal_item(ix);
            }
        }
    }

    /// The pixels to scroll to bring `ix` fully into view, if its bounds are
    /// known. Positive scrolls down.
    fn reveal_delta(&self, ix: usize) -> Option<f32> {
        let bounds = self.list_state.bounds_for_item(ix)?;
        let viewport = self.list_state.viewport_bounds();
        let row_top = f32::from(bounds.origin.y);
        let row_bottom = row_top + f32::from(bounds.size.height);
        let view_top = f32::from(viewport.origin.y);
        let view_bottom = view_top + f32::from(viewport.size.height);
        if row_top < view_top {
            Some(row_top - view_top)
        } else if row_bottom > view_bottom {
            Some(row_bottom - view_bottom)
        } else {
            Some(0.0)
        }
    }

    /// The playlist and track index the cursor's row plays from.
    fn activate_target(&self) -> Option<(PlaylistId, usize)> {
        let cursor = self.cursor?;
        albums::row_target(&self.items, &self.sections, cursor)
    }

    /// Start the crossfade after a selection change: the rows that just left
    /// fade out from full, the rows that just arrived fade in from nothing.
    /// Rows that were already selected are left alone.
    fn restart_selection_fade(&mut self, previous: &RowSelection) {
        for ix in previous.iter() {
            if !self.selection.contains(ix) {
                self.selection_fade.aim(ix, 1.0, 0.0);
            }
        }
        for ix in self.selection.iter() {
            if !previous.contains(ix) {
                self.selection_fade.aim(ix, 0.0, 1.0);
            }
        }
    }
}

impl Default for RowList {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the fades to rest.
    fn settle(highlight: &mut Highlight) {
        for _ in 0..90 {
            highlight.selection_fade.tick(1.0 / 60.0);
            highlight.hover_fade.tick(1.0 / 60.0);
        }
    }

    #[test]
    fn a_settled_row_is_implied_by_the_selection() {
        let mut highlight = Highlight::default();
        highlight.selection.set_single(3);
        highlight.selection_fade.aim(3, 0.0, 1.0);
        settle(&mut highlight);

        // A settled row leaves the map — its opacity comes from the selection.
        assert!(highlight.selection_fade.opacity(3).is_none());
        assert_eq!(highlight.selection(3), Some(1.0));
        assert_eq!(highlight.selection(9), None);
    }

    #[test]
    fn moving_off_rows_does_not_disturb_a_third() {
        // The bug this guards: sweeping the pointer down a list must not drag
        // every row it passed back to full highlight.
        let mut highlight = Highlight::default();
        for ix in [1, 2, 3] {
            highlight.selection.toggle(ix);
            highlight.selection_fade.aim(ix, 0.0, 1.0);
        }
        settle(&mut highlight);

        // Leave 1 and 2; 3 stays selected.
        highlight.selection.toggle(1);
        highlight.selection.toggle(2);
        highlight.selection_fade.aim(1, 1.0, 0.0);
        highlight.selection_fade.aim(2, 1.0, 0.0);
        highlight.selection_fade.tick(0.02);

        assert!(highlight.selection(1).unwrap() < 1.0, "row 1 is fading out");
        assert!(highlight.selection(2).unwrap() < 1.0, "row 2 is fading out");
        assert_eq!(highlight.selection(3), Some(1.0), "row 3 is untouched");
    }

    #[test]
    fn a_faded_out_row_stops_being_highlighted() {
        let mut highlight = Highlight::default();
        highlight.selection.set_single(4);
        highlight.selection_fade.aim(4, 0.0, 1.0);
        settle(&mut highlight);

        // Move the selection off it.
        highlight.selection.set_single(5);
        highlight.selection_fade.aim(4, 1.0, 0.0);
        highlight.selection_fade.aim(5, 0.0, 1.0);
        highlight.selection_fade.tick(0.02);
        assert!(highlight.selection(4).unwrap() < 1.0, "the old row is fading");
        assert!(highlight.selection(5).unwrap() < 1.0, "the new row is fading in");

        settle(&mut highlight);
        assert_eq!(highlight.selection(4), None, "the old row is gone");
        assert_eq!(highlight.selection(5), Some(1.0));
    }

    #[test]
    fn hover_reads_from_the_hovered_index_when_settled() {
        let mut highlight = Highlight::default();
        highlight.hovered = Some(7);
        highlight.hover_fade.aim(7, 0.0, 1.0);
        settle(&mut highlight);
        assert_eq!(highlight.hover(7), Some(1.0));
        assert_eq!(highlight.hover(8), None);
    }

    #[test]
    fn a_wheel_burst_builds_momentum_within_a_cap() {
        let mut rows = RowList::new();
        // A positive wheel delta scrolls up, so the remaining distance goes
        // negative; a zero delta does nothing at all.
        assert!(rows.wheel(10.0));
        assert!(rows.scroll.remaining < 0.0);
        assert!(!rows.wheel(0.0));

        // A long burst is capped rather than coasting forever.
        for _ in 0..1000 {
            rows.wheel(-50.0);
        }
        assert!(rows.scroll.remaining <= SCROLL_MAX_MOMENTUM_PX + 0.001);
        assert!(rows.scroll.remaining >= -SCROLL_MAX_MOMENTUM_PX - 0.001);
    }

    #[test]
    fn a_queued_scroll_converges_on_its_target() {
        let mut scroll = SmoothScroll::default();
        scroll.queue(100.0);
        assert!(scroll.is_active());

        let mut travelled = 0.0;
        for _ in 0..500 {
            if !scroll.is_active() {
                break;
            }
            travelled += scroll.step(1.0 / 60.0);
        }
        assert!((travelled - 100.0).abs() < 1.0, "travelled {travelled}");
        assert!(!scroll.is_active(), "it settles");
        assert_eq!(scroll.step(1.0 / 60.0), 0.0, "a settled scroll is inert");
    }

    #[test]
    fn a_scroll_settles_in_well_under_a_second() {
        // It should read as a fast settle, not a glide.
        let mut scroll = SmoothScroll::default();
        scroll.queue(100.0);
        let mut frames = 0;
        while scroll.is_active() && frames < 600 {
            scroll.step(1.0 / 60.0);
            frames += 1;
        }
        assert!(frames < 40, "took {frames} frames");
    }
}
