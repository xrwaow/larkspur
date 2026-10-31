//! The album-list shell: one [`RowList`] + subscriptions + row renderer shared
//! by the browse, playlist, and search views.
//!
//! The three containers draw the same thing — album sections over a virtualized
//! list — and differ only in what feeds the sections, what a plain click plays,
//! and the chrome around the list (browse's header, the playlist's add-songs
//! bar, search's query box). Everything in between lives here: building
//! sections from a [`Source`], the four observers, the row actions (one
//! modifier-aware `activate`), the scroll area, and the `render_item` wiring.
//!
//! A view plugs in by holding an [`AlbumListView`] and implementing the
//! [`AlbumList`] trait — two accessors and an optional on-library-change hook.

use std::rc::Rc;

use gpui::{
    canvas, div, prelude::*, px, App, Context, ElementId, Entity, KeyDownEvent, MouseButton,
    MouseDownEvent,
    Render, ScrollWheelEvent, SharedString, Subscription, Window,
};

use crate::model::select::{Order, Scope, Selection};
use crate::model::{search, InputAction, Library, PlaylistId, SongId};
use crate::ui::albums::{self, AlbumSection, RowActions, SectionSource};
use crate::ui::animation::{animate, Advance, Animator, Tween, WakeHandle};
use crate::ui::config_state::ConfigState;
use crate::ui::cover_store::CoverStore;
use crate::ui::eq;
use crate::ui::drag::DragInfo;
use crate::ui::format::format_bitrate;
use crate::ui::input::action_for_key;
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::marquee;
use crate::ui::menu::{MenuOrigin, SongMenuRequest};
use crate::ui::playback_state::PlaybackState;
use crate::ui::row_list::{Cell, RowAction, RowList, SmoothScroll};
use crate::ui::theme::Theme;
use crate::ui::widgets::{scroll_area, wheel_pixels};

/// What a plain click on a row plays: the section's whole playlist, or only
/// the section's matched songs (search results).
#[derive(Clone, Copy)]
pub enum Play {
    Playlist,
    Result,
}

/// Snapshot a scope into drawable sections, with `query` filtering the songs
/// *within* it — an empty query is the whole scope. The library scope runs the
/// query engine over the whole library; an artist scope filters their
/// discography, keeping its newest-first order; a folder scope resolves the
/// albums (and singles) under its directory, in folder order.
pub fn sections(library: &Library, scope: &Scope, query: &search::Query) -> Vec<AlbumSection> {
    let groups = match scope {
        Scope::Library => search::search(library, query),
        Scope::Artist(_) => library.select(&Selection {
            scope,
            query,
            order: Order::NewestFirst,
        }),
        Scope::Folder { .. } => library.select(&Selection {
            scope,
            query,
            order: Order::TrackOrder,
        }),
    };
    albums::sections_from_groups(library, groups)
}

/// The list state and entities every album view needs: the snapshot rows, the
/// library and playback the row actions play from, and the covers the headers
/// request.
pub struct AlbumListView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    /// The shared config, held so the playing row's equalizer (built lazily,
    /// below) can resolve the list's theme the way the view itself does.
    config: Entity<ConfigState>,
    /// The playing row's equalizer — a retained view that animates itself on
    /// the shared clock, repainting only its own subtree. Built lazily so a
    /// never-rendered view (a search overlay behind the scenes) costs nothing.
    eq: Option<Entity<eq::EqBars>>,
    /// The shared frame clock, handed to the playing row's equalizer.
    animator: Entity<Animator>,
    /// A no-`Context` handle to that clock, so any row operation that starts a
    /// fade, slide, or eased scroll can wake it.
    wake: WakeHandle,
    play: Play,
    rows: RowList,
    /// The icon grid's scroll offset, in px from the top — owned outright (no
    /// native scroller to fight: the grid is translated by this amount and
    /// clipped), so wheel scrolling and the windowing can never disagree.
    icon_scroll_y: std::cell::Cell<f32>,
    /// The eased wheel scroll driving `icon_scroll_y` — the same
    /// accumulate-and-settle the list view scrolls with.
    icon_scroll: SmoothScroll,
    /// The icon grid's scrollable height, recorded by `render_icons` so the
    /// eased scroll can clamp without re-packing the rows.
    icon_max_scroll: std::cell::Cell<f32>,
    /// Whether the icon grid is what's on screen — `dispatch` scrolls it
    /// instead of driving the row list's selection. Set by the render
    /// functions, which are the only ones that know which one is showing.
    icon_focus: bool,
    /// The icon grid viewport's last measured size, in whole px. Measured by a
    /// canvas each frame; a change schedules one re-render so the columns
    /// re-pack against the new size (never trusting a previous-frame guess).
    icon_viewport: std::rc::Rc<std::cell::Cell<(u32, u32)>>,
    /// The library revision the rows were last built from, so a highlight-only
    /// notify (a tab switch) doesn't rebuild them.
    seen_revision: u64,
    /// The icon card whose info panel is up (or fading out). The tween drives
    /// the panel's fade; while it's fading out the card is kept so there is
    /// something to draw until it reaches zero.
    icon_info: Option<IconInfo>,
    /// The hover panel's eased song-list scroll, and the offset/clamp it moves
    /// within. Separate from the grid's scroll because the panel has its own
    /// inner overflow area.
    icon_info_scroll: SmoothScroll,
    icon_info_scroll_y: std::cell::Cell<f32>,
    icon_info_max_scroll: std::cell::Cell<f32>,
    /// The hover panel title's marquee — the same slide-and-snap the track rows
    /// use, triggered from the right edge of the heading.
    icon_info_marquee: marquee::Marquee,
    /// The marquee of each hover-panel row's text cell, keyed by the row's
    /// index (a song's index). Each slides on its own hover, like a track row's
    /// title.
    icon_info_rows: std::collections::HashMap<usize, marquee::Marquee>,
    /// The icon grid viewport's origin in window coordinates, recorded by the
    /// measurer. The panel pops up at a window-space position (the click), but
    /// is drawn in the grid's own space, so the two need reconciling.
    icon_viewport_origin: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
}

/// Every album list renders the browse container's typography (the views
/// resolve their theme through `BrowseView::themed`), so the equalizer
/// resolves its bars' look the same way.
const EQ_CONTAINER: &str = "browse";

/// Seconds the info panel takes to fade in when a cover is right-clicked.
const ICON_INFO_IN_SECS: f32 = 0.16;

/// Seconds it takes to fade out — a touch quicker, so dismissing the panel
/// doesn't feel sluggish.
const ICON_INFO_OUT_SECS: f32 = 0.1;

/// The info panel's state for one icon card.
struct IconInfo {
    /// The card the panel is for.
    card: usize,
    /// The card actually drawn. Lags `card` while the old panel fades out, so
    /// switching covers never snaps the old content into the new one.
    shown: usize,
    /// 0 = hidden, 1 = fully shown.
    t: Tween,
    /// Where the panel was raised, in window coordinates — the right-click's
    /// pointer position. The panel pops up here instead of beside the cover.
    at: (f32, f32),
    /// A dismissal is under way (right-click the same cover or the panel
    /// again, or a mouse-down outside it). Like the song menu, hover has no
    /// say: the panel stays up until it is explicitly dismissed.
    closing: bool,
}

impl AlbumListView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        wake: WakeHandle,
        play: Play,
    ) -> Self {
        Self {
            library,
            playback,
            covers,
            config,
            animator,
            eq: None,
            wake,
            play,
            rows: RowList::new(),
            icon_scroll_y: std::cell::Cell::new(0.0),
            icon_scroll: SmoothScroll::default(),
            icon_max_scroll: std::cell::Cell::new(0.0),
            icon_focus: false,
            icon_viewport: std::rc::Rc::new(std::cell::Cell::new((0, 0))),
            seen_revision: 0,
            icon_info: None,
            icon_info_scroll: SmoothScroll::default(),
            icon_info_scroll_y: std::cell::Cell::new(0.0),
            icon_info_max_scroll: std::cell::Cell::new(0.0),
            icon_info_marquee: marquee::Marquee::new(),
            icon_info_rows: std::collections::HashMap::new(),
            icon_viewport_origin: std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0))),
        }
    }

    /// Reset the panel's per-card state — the inner song-list starts at the top
    /// and a new title starts unslid.
    fn reset_icon_info_scroll(&mut self) {
        self.icon_info_scroll = SmoothScroll::default();
        self.icon_info_scroll_y.set(0.0);
        self.icon_info_max_scroll.set(0.0);
        self.icon_info_marquee.reset();
        self.icon_info_rows.clear();
    }

    /// Note an info-panel row's hover — the background its marquee's fade has
    /// to match. Returns whether anything the renderer draws changed.
    pub fn set_icon_info_row_hover(&mut self, row: usize, hovered: bool) -> bool {
        self.icon_info_rows.entry(row).or_default().set_region_hovered(hovered)
    }

    /// Note an info-panel row's title-cell hover; `travel` is how far its text
    /// overflows the cell, measured by the renderer.
    pub fn set_icon_info_cell_hover(&mut self, row: usize, hovered: bool, travel: f32) -> bool {
        self.icon_info_rows.entry(row).or_default().set_hovered(hovered, travel)
    }

    /// One row's marquee state for this frame.
    fn icon_info_row_motion(&self, row: usize) -> albums::InfoRowMotion {
        match self.icon_info_rows.get(&row) {
            Some(cell) => albums::InfoRowMotion {
                offset: cell.offset(),
                hovered: cell.is_region_hovered(),
            },
            None => albums::InfoRowMotion::default(),
        }
    }

    /// Raise or dismiss the popup info panel for `card` — the icon grid's
    /// right-click action, opened at `at` (window coordinates, the click's
    /// pointer position). Right-clicking the card already on screen closes it;
    /// right-clicking another card switches the panel over.
    pub fn toggle_icon_info(&mut self, card: usize, at: gpui::Point<gpui::Pixels>) {
        if matches!(&self.icon_info, Some(info) if info.card == card && info.shown == card) {
            self.fade_out_icon_info();
            return;
        }
        self.open_icon_info(card, at);
    }

    /// Start the panel's fade-out. Idempotent, and deliberately *not* an
    /// instant clear: a mouse-down outside the panel (capture phase) arrives
    /// before the click's own bubble handlers, and a right-click on the same
    /// cover must read as a dismissal, not as a dismissal followed by a
    /// reopen.
    fn fade_out_icon_info(&mut self) {
        if let Some(info) = &mut self.icon_info {
            info.closing = true;
            info.t.to(0.0, ICON_INFO_OUT_SECS);
        }
    }

    /// Open the panel for `card`, switching content if another card's panel is
    /// up. The old panel fades out first (the tick swaps the content in), so a
    /// switch never snaps one album's songs into another's.
    fn open_icon_info(&mut self, card: usize, at: gpui::Point<gpui::Pixels>) {
        let at = (f32::from(at.x), f32::from(at.y));
        let mut reset_scroll = false;
        if let Some(info) = self.icon_info.as_mut() {
            info.closing = false;
            info.at = at;
            info.card = card;
            if info.shown == card {
                if info.t.value() < 1.0 {
                    info.t.to(1.0, ICON_INFO_IN_SECS);
                }
            } else if info.t.value() > 0.0 {
                // Fade the old card's panel out; the tick swaps in the new
                // content once it has fully gone.
                info.t.to(0.0, ICON_INFO_OUT_SECS);
            } else {
                info.shown = card;
                info.t.set(0.0);
                info.t.to(1.0, ICON_INFO_IN_SECS);
                reset_scroll = true;
            }
        } else {
            let mut t = Tween::new(0.0);
            t.to(1.0, ICON_INFO_IN_SECS);
            self.icon_info = Some(IconInfo {
                card,
                shown: card,
                t,
                at,
                closing: false,
            });
            reset_scroll = true;
        }
        if reset_scroll {
            self.reset_icon_info_scroll();
        }
    }

    /// The pointer entered or left the hover title's right-edge fade zone. The
    /// render pass syncs the slide's travel against the current heading.
    pub fn set_icon_info_title_hover(&mut self, hovered: bool) -> bool {
        // Travel is a placeholder: the next render syncs the slide against the
        // heading actually drawn, so the pointer aim can never be stale.
        self.icon_info_marquee.set_hovered(hovered, 0.0)
    }

    /// Scroll the hover panel's song list by `pixels` (positive scrolls down),
    /// eased like every other scroll in the app.
    pub fn scroll_icon_info(&mut self, pixels: f32) {
        self.icon_info_scroll.accumulate(pixels);
        self.request_frame();
    }

    /// Advance the info panel's fade and inner scroll. The panel only ever
    /// fades when it is explicitly dismissed (see [`fade_out_icon_info`]);
    /// hover has no say. Returns `(moved, awake)`: `moved` asks for a redraw,
    /// `awake` only for more frames.
    pub fn tick_icon_info(&mut self, dt: f32) -> (bool, bool) {
        let mut moved = false;
        let mut awake = false;
        let mut clear = false;
        let mut reset_scroll = false;
        {
            let Some(info) = self.icon_info.as_mut() else {
                return (false, false);
            };
            // A switch has faded the old panel out; once it's gone, swap in
            // the new card and reveal its panel.
            if info.shown != info.card && !info.t.is_animating() && info.t.value() <= 0.0 {
                info.shown = info.card;
                info.t.set(0.0);
                info.t.to(1.0, ICON_INFO_IN_SECS);
                info.closing = false;
                reset_scroll = true;
                moved = true;
            }
            if info.t.tick(dt) {
                moved = true;
            }
            if self.icon_info_marquee.tick(dt) {
                moved = true;
                awake = true;
            }
            let step = self.icon_info_scroll.step(dt);
            if step != 0.0 {
                let next = (self.icon_info_scroll_y.get() + step)
                    .clamp(0.0, self.icon_info_max_scroll.get());
                if next != self.icon_info_scroll_y.get() {
                    self.icon_info_scroll_y.set(next);
                    moved = true;
                }
                awake = true;
            }
            if self.icon_info_rows.values_mut().any(|cell| cell.tick(dt)) {
                moved = true;
                awake = true;
            }

            if info.closing && !info.t.is_animating() && info.t.value() <= 0.0 {
                clear = true;
            }
        }
        if reset_scroll {
            self.reset_icon_info_scroll();
        }
        if clear {
            self.clear_icon_info();
            moved = true;
        }
        (moved, awake)
    }

    /// Whether an info panel is up or fading out.
    pub fn icon_info_open(&self) -> bool {
        self.icon_info.is_some()
    }

    /// Drop the info panel without animating — the icon grid is leaving the
    /// screen, or the grid scrolled, so there is nothing left for it to fade
    /// over. Returns whether a panel was actually up.
    pub fn close_icon_info(&mut self) -> bool {
        if self.icon_info.is_none() {
            return false;
        }
        self.clear_icon_info();
        true
    }

    fn clear_icon_info(&mut self) {
        self.icon_info = None;
        self.reset_icon_info_scroll();
        self.forget_icon_info_title_hover();
    }

    /// The title cell is gone, so its hover callback will never arrive to say
    /// the pointer left: drop the flag here. Otherwise the *next* album's title
    /// would be born already slid, as if the pointer were still over it.
    fn forget_icon_info_title_hover(&mut self) {
        self.icon_info_marquee.set_hovered(false, 0.0);
    }

    /// Advance the icon grid's eased scroll. Returns whether it moved.
    pub fn tick_icon_scroll(&mut self, dt: f32) -> bool {
        let step = self.icon_scroll.step(dt);
        if step == 0.0 {
            return false;
        }
        let next = (self.icon_scroll_y.get() + step).clamp(0.0, self.icon_max_scroll.get());
        self.icon_scroll_y.set(next);
        true
    }

    pub fn library(&self) -> &Entity<LibraryState> {
        &self.library
    }

    pub fn playback(&self) -> &Entity<PlaybackState> {
        &self.playback
    }

    pub fn covers(&self) -> &Entity<CoverStore> {
        &self.covers
    }

    pub fn rows(&self) -> &RowList {
        &self.rows
    }

    pub fn rows_mut(&mut self) -> &mut RowList {
        &mut self.rows
    }

    /// Ask the frame clock for another frame. Called wherever a row operation
    /// may have started a highlight fade, marquee slide, or eased scroll.
    pub fn request_frame(&self) {
        self.wake.request();
    }

    /// The playing row's equalizer, built on first use. Returns `None` only
    /// before the list has ever rendered a row area.
    pub fn eq_bars(&mut self, cx: &mut App) -> Option<Entity<eq::EqBars>> {
        if self.eq.is_none() {
            let animator = self.animator.clone();
            let playback = self.playback.clone();
            let config = self.config.clone();
            self.eq = Some(cx.new(|cx| {
                eq::EqBars::new(
                    animator,
                    playback,
                    config,
                    EQ_CONTAINER,
                    crate::model::config::DEFAULT_FONT_SIZE,
                    cx,
                )
            }));
        }
        self.eq.clone()
    }

    /// Switch what a plain click plays — the library's filter box toggles
    /// between whole albums (unfiltered) and the matched songs only.
    pub fn set_play(&mut self, play: Play) {
        self.play = play;
    }

    /// Remember `revision` and mark the rows stale. Returns whether this was
    /// a new revision, so the view's hook only runs on real library changes.
    fn note_revision(&mut self, revision: u64) -> bool {
        if self.seen_revision == revision {
            return false;
        }
        self.seen_revision = revision;
        self.rows.mark_dirty();
        true
    }

    /// The keyboard handler.
    ///
    /// In list view this is the rows' navigation: selection, reveal, and play.
    /// In icon view there are no rows to select, so the same actions scroll
    /// instead — up/down step one card row, pgup/pgdn a page — eased like every
    /// other scroll, and clamped to the laid-out height.
    pub fn dispatch<V: Render>(&mut self, action: InputAction, cx: &mut Context<V>) {
        if self.icon_focus {
            match action {
                InputAction::SelectNext => self.icon_scroll.queue(albums::icon_card_height()),
                InputAction::SelectPrev => self.icon_scroll.queue(-albums::icon_card_height()),
                InputAction::PageDown => self.icon_scroll.queue(self.icon_viewport_height()),
                InputAction::PageUp => self.icon_scroll.queue(-self.icon_viewport_height()),
                // Escape dismisses the panel without moving the grid.
                InputAction::CloseOverlay => {
                    if self.close_icon_info() {
                        self.request_frame();
                        cx.notify();
                    }
                    return;
                }
                _ => return,
            }
            // The grid is about to move, so the panel would be pinned to a
            // card that is no longer under the pointer: drop it.
            self.close_icon_info();
            self.request_frame();
            cx.notify();
            return;
        }
        match self.rows.handle(action) {
            RowAction::Play(source, index) => self.play(source, index, cx),
            RowAction::Handled => {
                self.request_frame();
                cx.notify();
            }
            RowAction::Ignored => {}
        }
    }

    /// The icon grid's viewport height, in px — 0 before the first layout, so
    /// a page action waits until the panel is actually sized.
    fn icon_viewport_height(&self) -> f32 {
        let (_, height) = self.icon_viewport.get();
        if height > 1 {
            height as f32
        } else {
            0.0
        }
    }

    /// Anchor the rows' no-cursor navigation to the playing song's row — what
    /// up/down step from when nothing is selected. Views call this after
    /// [`RowList::sync`], once the row indices are current.
    pub fn anchor_to_playing(&mut self, cx: &gpui::App) {
        let song = self.playback.read(cx).current_song();
        let (items, sections) = (self.rows.items(), self.rows.sections());
        self.rows.set_anchor(song.and_then(|song| albums::row_of_song(&items, &sections, song)));
    }

    /// Play `source` from `index` the way `play` chose at construction: the
    /// whole playlist (browse/playlist views), only the section's matched
    /// songs (search), or the queue entry at `index`.
    fn play(&mut self, source: SectionSource, index: usize, cx: &mut gpui::App) {
        // Playing consumes the selection: the played row carries the playing
        // highlight from here, and up/down re-anchor from it.
        self.rows.clear_selection();
        self.request_frame();
        let playback = self.playback.clone();
        match self.play {
            Play::Playlist => match source {
                SectionSource::Playlist(playlist) => self
                    .library
                    .update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx)),
                SectionSource::Queue => playback
                    .update(cx, |state, cx| state.play_queue_index(index, cx)),
            },
            Play::Result => {
                let songs: Vec<_> = self
                    .rows
                    .sections()
                    .iter()
                    .find(|section| section.source == source)
                    .map(|section| section.tracks.iter().map(|track| track.song))
                    .into_iter()
                    .flatten()
                    .collect();
                self.library.update(cx, |state, cx| state.play(&songs, index, &playback, cx));
            }
        }
    }
}

/// A view owning an [`AlbumListView`]. Implementing this grants the shared
/// helpers access to the list; the views stay thin wrappers otherwise.
pub trait AlbumList: Render + Sized {
    fn list(&self) -> &AlbumListView;
    fn list_mut(&mut self) -> &mut AlbumListView;
    /// Runs once on each library update, after the rows are marked stale —
    /// e.g. the playlist view restarts its add-songs candidates.
    fn library_changed(&mut self, _cx: &mut Context<Self>) {}
}

/// The row behavior of every album list: a modifier extends or toggles the
/// selection; a plain click plays.
struct Rows;

/// The row behavior of the icon grid: there are no rows to select, so a click
/// plays straight away and a right-click opens the same song menu.
struct Icons;

/// The shared right-click: build the menu request from a row's songs and raise
/// it through the library's request queue. `card` is the playlist an icon-grid
/// card represents, if any — it turns on the card-only menu items.
fn raise_menu<V: AlbumList>(
    view: &mut V,
    songs: Vec<SongId>,
    context: Option<PlaylistId>,
    event: &MouseDownEvent,
    card: Option<PlaylistId>,
    cx: &mut Context<V>,
) {
    let origin = match card {
        Some(card) => MenuOrigin::Card(card),
        None => MenuOrigin::Row { playlist: context },
    };
    let request = SongMenuRequest {
        songs,
        position: event.position,
        origin,
        heading: None,
    };
    let library = view.list().library.clone();
    library.update(cx, |state, cx| state.request(Request::SongMenu(request), cx));
}

impl<V: AlbumList> RowActions<V> for Icons {
    fn activate(
        &self,
        view: &mut V,
        _item_ix: usize,
        source: SectionSource,
        index: usize,
        _event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        view.list_mut().play(source, index, cx);
    }

    fn context(
        &self,
        view: &mut V,
        _item_ix: usize,
        songs: Vec<crate::model::SongId>,
        source: SectionSource,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        // Raised from a song row inside the info panel: the card's playlist
        // turns on the card-only items ("Go to {artist}" per credited artist
        // and "Go to playlist").
        let card = match source {
            SectionSource::Playlist(id) => Some(id),
            SectionSource::Queue => None,
        };
        raise_menu(view, songs, context, event, card, cx);
    }

    fn card_context(
        &self,
        view: &mut V,
        card: usize,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        // A section is not `Clone` (its tracks are shared, its strings aren't),
        // so take just what the menu needs while the borrow is live.
        let single = view.list().rows.sections().get(card).and_then(|section| {
            (section.tracks.len() == 1).then(|| {
                (
                    section.tracks[0].song,
                    section.source,
                    albums::icon_info_heading(section),
                )
            })
        });
        let Some((song, source, heading)) = single else {
            // A cover with more than one song raises its song-list panel: the
            // album's songs, each clickable and right-clickable.
            view.list_mut().toggle_icon_info(card, event.position);
            view.list().request_frame();
            cx.notify();
            return;
        };

        // A single has no song list worth opening, so its right-click is that
        // song's own menu — the real popup, submenus and all — at the pointer,
        // headed by the song.
        //
        // The panel, if one is up, belongs to a *different* card: this
        // right-click means it is done, or the two would sit on screen
        // together.
        if view.list_mut().close_icon_info() {
            cx.notify();
        }
        let origin = match source {
            SectionSource::Playlist(id) => MenuOrigin::Card(id),
            SectionSource::Queue => MenuOrigin::Row { playlist: None },
        };
        let request = SongMenuRequest {
            songs: vec![song],
            position: event.position,
            origin,
            heading: Some(heading),
        };
        let library = view.list().library.clone();
        library.update(cx, |state, cx| state.request(Request::SongMenu(request), cx));
    }

    fn hover(&self, _view: &mut V, _item_ix: usize, _hovered: bool, _cx: &mut Context<V>) {
        // Icon cards have no hover behavior of their own: the info panel is a
        // popup now, dismissed by click and scroll rather than by hover.
    }

    fn info_row_hover(&self, view: &mut V, row: usize, hovered: bool, cx: &mut Context<V>) {
        if view.list_mut().set_icon_info_row_hover(row, hovered) {
            view.list().request_frame();
            cx.notify();
        }
    }

    fn info_cell_hover(
        &self,
        view: &mut V,
        row: usize,
        hovered: bool,
        travel: f32,
        cx: &mut Context<V>,
    ) {
        if view.list_mut().set_icon_info_cell_hover(row, hovered, travel) {
            view.list().request_frame();
            cx.notify();
        }
    }
}

impl<V: AlbumList> RowActions<V> for Rows {
    fn activate(
        &self,
        view: &mut V,
        item_ix: usize,
        source: SectionSource,
        index: usize,
        event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        if view.list_mut().rows.click(item_ix, event.modifiers()) {
            view.list().request_frame();
            cx.notify();
            return;
        }
        view.list_mut().play(source, index, cx);
    }

    fn context(
        &self,
        view: &mut V,
        item_ix: usize,
        _songs: Vec<crate::model::SongId>,
        _source: SectionSource,
        context: Option<PlaylistId>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        // The row's selection-aware songs — a right-click on one row of a
        // multi-row selection menus them all.
        let songs = view.list_mut().rows.context_songs(item_ix);
        raise_menu(view, songs, context, event, None, cx);
    }

    fn hover(&self, view: &mut V, item_ix: usize, hovered: bool, cx: &mut Context<V>) {
        if view.list_mut().rows.set_hover(item_ix, hovered) {
            view.list().request_frame();
            cx.notify();
        }
    }

    fn cell_hover(
        &self,
        view: &mut V,
        item_ix: usize,
        cell: Cell,
        hovered: bool,
        cx: &mut Context<V>,
    ) {
        if view.list_mut().rows.set_cell_hover(item_ix, cell, hovered) {
            view.list().request_frame();
            cx.notify();
        }
    }
}

/// The subscriptions an album list subs to: library revisions rebuild the
/// rows, playback and covers re-render, the animator ticks the fades. The
/// search overlay skips the playback observer (it only reads playback during
/// render).
pub struct AlbumListSubs {
    _library: Subscription,
    _playback: Option<Subscription>,
    _covers: Subscription,
    _animator: Subscription,
}

pub fn observe<V: AlbumList>(
    library: &Entity<LibraryState>,
    playback: Option<&Entity<PlaybackState>>,
    covers: &Entity<CoverStore>,
    animator: &Entity<Animator>,
    cx: &mut Context<V>,
) -> AlbumListSubs {
    let _library = cx.observe(library, |this, state, cx| {
        let revision = state.read(cx).revision();
        if this.list_mut().note_revision(revision) {
            this.library_changed(cx);
        }
        cx.notify();
    });
    let _playback = playback.map(|playback| cx.observe(playback, |_this, _state, cx| cx.notify()));
    let _covers = cx.observe(covers, |_this, _state, cx| cx.notify());
    let _animator = animate(animator, cx, |this, dt, _cx| {
        let rows = this.list_mut().rows.tick(dt);
        // The icon grid's eased scroll and info panel run on the same clock.
        let scrolled = this.list_mut().tick_icon_scroll(dt);
        let (info_moved, info_awake) = this.list_mut().tick_icon_info(dt);
        let moved = rows || scrolled || info_moved;
        // The playing row's equalizer animates itself (see `crate::ui::eq`):
        // it holds its own clock subscription and repaints only its own
        // subtree, so a playing track costs the list nothing here. The linger
        // timer keeps the clock awake without repainting the grid.
        Advance { redraw: moved, awake: moved || info_awake }
    });
    AlbumListSubs { _library, _playback, _covers, _animator }
}

/// The virtualized list area: row render through `albums::render_item`, the
/// eased scroll area on top. `context` is the playlist the song menu offers as
/// a destination (the open playlist) — `None` for the whole library.
pub fn render_rows<V: AlbumList>(
    view: &mut V,
    theme: Theme,
    context: Option<PlaylistId>,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    render_rows_with(view, theme, context, None, Rc::new(Rows), cx)
}

/// [`render_rows`](Self::render_rows) with the view's own row actions and an
/// optional in-flight drag-to-reorder — the queue panel plays from the queue
/// and reorders by drag.
pub fn render_rows_with<V: AlbumList>(
    view: &mut V,
    theme: Theme,
    context: Option<PlaylistId>,
    drag: Option<DragInfo>,
    actions: Rc<dyn RowActions<V>>,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    // Leaving icon view: its hover panel has no surface to fade over any more.
    if view.list().icon_focus {
        view.list_mut().clear_icon_info();
    }
    view.list_mut().icon_focus = false;
    let list = view.list();
    let current = list.playback.read(cx).current_song();
    let live_bitrate_label =
        list.playback.read(cx).live_bitrate().map(|bps| SharedString::from(format_bitrate(bps)));
    let sections = list.rows.sections();
    let items = list.rows.items();
    let columns = list.rows.columns();
    let list_state = list.rows.list_state().clone();
    let highlight = list.rows.highlight();
    let slim = list.rows.slim();
    let covers = list.covers.clone();
    let library = list.library.clone();

    let eq = view.list_mut().eq_bars(cx);
    let render = cx.processor(move |_this, ix, _window, cx| {
        let row = albums::RowContext {
            theme,
            covers: &covers,
            library: &library,
            current,
            eq: eq.clone(),
            live_bitrate_label: live_bitrate_label.clone(),
            context,
            highlight: &highlight,
            drag,
            slim,
        };
        albums::render_item(&row, ix, &sections, &items, &columns, Some(&actions), cx)
    });
    scroll_area(
        gpui::list(list_state, render).size_full().into_any_element(),
        cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
            if this.list_mut().rows.wheel(wheel_pixels(event)) {
                this.list().request_frame();
                cx.notify();
            }
            cx.stop_propagation();
        }),
    )
}

/// The icon view of the same sections: album covers packed into rows that
/// fill left to right and wrap down when full (see `albums::pack_rows`),
/// windowed to the viewport — only the cards near the scroll offset render,
/// so a huge library stays at full speed. Right-clicking a cover pops an
/// interactive song panel up at the pointer; left-clicking still plays the
/// album. A single-song cover has no list worth opening, so its right-click
/// raises that song's own menu at the pointer instead (headed by the song,
/// with no cover or panel chrome). The panel is hoverable, so the pointer can
/// move into it to play or right-click its songs without the panel closing. It
/// closes again when the pointer leaves it, when the grid scrolls, or on
/// another right-click.
///
/// The viewport size is measured by a canvas inside the tree; when it changes
/// (first layout, a window resize, the rails moving) one re-render is
/// scheduled and the rows re-pack against the real size.
pub fn render_icons<V: AlbumList>(
    view: &mut V,
    theme: Theme,
    context: Option<PlaylistId>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    view.list_mut().icon_focus = true;
    let (viewport_w, viewport_h) = view.list().icon_viewport.get();
    let (viewport_w, viewport_h) = if viewport_w > 1 && viewport_h > 1 {
        (viewport_w as f32, viewport_h as f32)
    } else {
        // Not measured yet (first frame) — a provisional pack, corrected as
        // soon as the canvas reports the real size.
        (1400.0, 900.0)
    };

    let sections = view.list().rows.sections().clone();
    let layout = albums::pack_rows(sections.len(), viewport_w);
    let max_scroll = (layout.height - viewport_h).max(0.0);
    // Record for the eased scroll's clamp, and re-clamp the offset itself —
    // the layout can shrink (a narrower viewport packs fewer rows).
    view.list_mut().icon_max_scroll.set(max_scroll);
    if view.list().icon_scroll_y.get() > max_scroll {
        view.list_mut().icon_scroll_y.set(max_scroll);
    }

    // The info panel's geometry is worked out before the list borrow so the
    // song list's scroll clamp is current for this frame.
    let hover_info = view
        .list()
        .icon_info
        .as_ref()
        .filter(|info| info.t.value() > 0.0 || info.t.is_animating())
        .map(|info| (info.shown, info.t.value(), info.at));
    let info_render = hover_info.and_then(|(card, t, at)| {
        let section = sections.get(card)?;
        let rows = section.tracks.len();
        Some((card, t, at, albums::icon_info_metrics(theme, rows, viewport_h)))
    });
    let mut row_motion = Vec::new();
    if let Some((card, _, _, metrics)) = &info_render {
        view.list().icon_info_max_scroll.set(metrics.max_scroll);
        if view.list().icon_info_scroll_y.get() > metrics.max_scroll {
            view.list().icon_info_scroll_y.set(metrics.max_scroll);
        }
        // Keep the heading's slide armed against the heading currently drawn.
        let heading = albums::icon_info_heading(&sections[*card]);
        let title_w = albums::icon_info_heading_width(metrics.panel_w);
        let title_chars = albums::icon_info_heading_chars(window, theme, metrics.panel_w);
        let travel = marquee::travel_for(&heading, title_w, title_chars);
        view.list_mut().icon_info_marquee.sync(travel);
        row_motion = (0..sections[*card].tracks.len())
            .map(|row| view.list().icon_info_row_motion(row))
            .collect();
    }
    let info_scroll = view.list().icon_info_scroll_y.get();
    let title_offset = view.list().icon_info_marquee.offset();
    let scroll_y = view.list().icon_scroll_y.get();

    // The card whose side is showing takes the panel's own surface instead of
    // the hover wash, so the cover and the panel read as one shape.
    let active_card = info_render.as_ref().map(|(card, _, _, _)| *card);

    let eq = view.list_mut().eq_bars(cx);
    let list = view.list();
    let current = list.playback.read(cx).current_song();
    let highlight = list.rows.highlight();
    let covers = list.covers.clone();
    let library = list.library.clone();
    let row = albums::RowContext {
        theme,
        covers: &covers,
        library: &library,
        current,
        eq,
        live_bitrate_label: None,
        context,
        highlight: &highlight,
        drag: None,
        slim: false,
    };
    let icons: Rc<dyn RowActions<V>> = Rc::new(Icons);

    // The measurer: a canvas stretched over the viewport. Its layout pass
    // sees the real size every frame; when it changes, one re-render is
    // scheduled so the pack runs against it.
    let entity = cx.entity();
    let measured = list.icon_viewport.clone();
    let measured_origin = list.icon_viewport_origin.clone();
    let measurer = canvas(
        move |bounds, window, _cx| {
            let key =
                (f32::from(bounds.size.width).round() as u32, f32::from(bounds.size.height).round() as u32);
            // The origin rides along so the panel — raised at a window-space
            // click position but drawn in the grid's space — can be placed
            // there. It moves with the docks without a resize, so it is
            // recorded on every layout pass.
            measured_origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
            if measured.get() != key {
                measured.set(key);
                window.on_next_frame(move |_, cx| entity.update(cx, |_, cx| cx.notify()));
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0();

    // The scrollable area: the grid, translated up by the scroll offset
    // inside a clipped viewport. There is no native scroller to race with —
    // the wheel overlay below is the only thing that moves it, updating the
    // offset and re-rendering so the windowing follows every scroll.
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .child(measurer)
        .child(
            div()
                .relative()
                .top(px(-scroll_y))
                .child(
                    albums::render_icon_grid(
                        &row,
                        &sections,
                        &layout,
                        scroll_y,
                        viewport_h,
                        active_card,
                        &icons,
                        cx,
                    )
                    .into_any_element(),
                ),
        )
        .child(
            div().absolute().inset_0().on_scroll_wheel(
                cx.listener(move |this, event: &ScrollWheelEvent, _window, cx| {
                    // Scrolling re-pins every card under the pointer, so a
                    // panel opened for one of them would be left hanging in
                    // space: dismiss it before the grid moves.
                    this.list_mut().fade_out_icon_info();
                    // The same eased scroll the list view uses: a burst of
                    // wheel events accumulates into a target distance that
                    // decays toward zero, instead of stepping per event.
                    // (Clamping happens in `tick_icon_scroll`, against the
                    // laid-out height recorded by this render.)
                    this.list_mut().icon_scroll.accumulate(-wheel_pixels(event));
                    this.list().request_frame();
                    cx.notify();
                    cx.stop_propagation();
                }),
            ),
        )
        .children(icon_info_overlay(
            info_render,
            &sections,
            info_scroll,
            list.icon_viewport_origin.get(),
            title_offset,
            &row_motion,
            viewport_w,
            viewport_h,
            theme,
            context,
            &covers,
            &library,
            &icons,
            window,
            cx,
        ))
        .into_any_element()
}

/// The info card's popup panel, placed at the right-click's pointer position
/// (converted from window coordinates into the grid's own) and faded from the
/// icon grid's layout numbers. Returns nothing when no card's panel is showing
/// or the panel has faded away.
#[allow(clippy::too_many_arguments)]
fn icon_info_overlay<V: AlbumList>(
    hover: Option<(usize, f32, (f32, f32), albums::IconInfoMetrics)>,
    sections: &[AlbumSection],
    info_scroll: f32,
    origin: (f32, f32),
    title_offset: f32,
    row_motion: &[albums::InfoRowMotion],
    viewport_w: f32,
    viewport_h: f32,
    theme: Theme,
    context: Option<PlaylistId>,
    covers: &Entity<CoverStore>,
    library: &Entity<LibraryState>,
    actions: &Rc<dyn RowActions<V>>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Option<gpui::AnyElement> {
    let (card, t, at, metrics) = hover?;
    let section = sections.get(card)?;

    // The card itself already asked for this cover; asking again is free and
    // covers the frame where the pointer lands before the card's request has
    // been recorded.
    albums::request_cover(covers, library, section.cover_song, cx);
    let cover = covers.read(cx).cover_of(section.cover_song);

    // The panel pops up where the cover was right-clicked. The click is in
    // window coordinates; the overlay is drawn in the viewport's own space (a
    // sibling of the scrolled grid, not inside it), so only the viewport
    // origin comes off. Like a menu, it flips to grow up/left when it would
    // run off the viewport, then clamps to the viewport's margin.
    let m = albums::ICON_INFO_MARGIN_PX;
    let (origin_x, origin_y) = origin;
    let gx = at.0 - origin_x;
    let x = if gx + metrics.panel_w > viewport_w - m { gx - metrics.panel_w } else { gx };
    let x = x.clamp(m, (viewport_w - metrics.panel_w - m).max(m));
    let gy = at.1 - origin_y;
    let y = if gy + metrics.panel_h > viewport_h - m { gy - metrics.panel_h } else { gy };
    let y = y.clamp(m, (viewport_h - metrics.panel_h - m).max(m));
    let on_title_hover = hover_listener(cx, AlbumListView::set_icon_info_title_hover);
    let heading = albums::icon_info_heading(section);
    let panel = albums::icon_info_panel(
        theme,
        section,
        &heading,
        cover,
        context,
        actions,
        metrics,
        info_scroll,
        title_offset,
        row_motion,
        on_title_hover,
        window,
        cx,
    );

    let on_panel_wheel = cx.listener(|view, event: &ScrollWheelEvent, _window, cx| {
        view.list_mut().scroll_icon_info(-wheel_pixels(event));
        cx.notify();
        cx.stop_propagation();
    });
    // The panel dismisses like the song menu does: a right-click on it (its
    // padding — rows and the heading stop propagation to open their own menus)
    // or any mouse-down outside it closes it. Hover is deliberately not a
    // dismissal path.
    let dismiss = |view: &mut V, cx: &mut Context<V>| {
        view.list_mut().fade_out_icon_info();
        view.list().request_frame();
        cx.notify();
    };
    let on_panel_context = cx.listener(move |view, _e: &MouseDownEvent, _w, cx| dismiss(view, cx));
    let on_panel_dismiss = cx.listener(move |view, _e: &MouseDownEvent, _w, cx| dismiss(view, cx));
    let panel = div()
        .id(ElementId::Name("icon-info-panel".into()))
        .absolute()
        .left(px(x))
        .top(px(y))
        .opacity(t)
        .occlude()
        .on_scroll_wheel(on_panel_wheel)
        .on_mouse_down(MouseButton::Right, on_panel_context)
        .on_mouse_down_out(on_panel_dismiss)
        .child(panel);
    Some(div().absolute().inset_0().child(panel).into_any_element())
}

/// A hover callback that only asks for a frame when the panel's state actually
/// changed — the boilerplate every one of the panel's hover regions shares.
fn hover_listener<'a, V: AlbumList>(
    cx: &'a mut Context<V>,
    set: fn(&mut AlbumListView, bool) -> bool,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static + use<'a, V> {
    cx.listener(move |view, hovered: &bool, _window, cx| {
        if set(view.list_mut(), *hovered) {
            view.list().request_frame();
            cx.notify();
        }
    })
}

/// Route a navigation key on the list's root. Used by containers that keep
/// the list's key handling as-is (browse, playlist) — the search container
/// has its own, so it passes the box's keys there first.
pub fn list_key_listener<V: AlbumList>(view: &mut V, event: &KeyDownEvent, cx: &mut Context<V>) {
    if let Some(action) = action_for_key(&event.keystroke) {
        view.list_mut().dispatch(action, cx);
    }
}
