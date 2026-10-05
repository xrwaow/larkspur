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

use gpui::{canvas, div, prelude::*, px, Context, Entity, KeyDownEvent, Render, ScrollWheelEvent, SharedString, Subscription, Window};

use crate::model::select::{Order, Scope, Selection};
use crate::model::{search, InputAction, Library, PlaylistId, SongId};
use crate::ui::albums::{self, AlbumSection, RowActions};
use crate::ui::animation::Animator;
use crate::ui::cover_store::CoverStore;
use crate::ui::drag::DragInfo;
use crate::ui::format::format_bitrate;
use crate::ui::input::action_for_key;
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::menu::SongMenuRequest;
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
        Scope::Artist(artist) => library.select(&Selection {
            scope: Scope::Artist(artist.clone()),
            query: query.clone(),
            order: Order::NewestFirst,
        }),
        Scope::Folder { dir, mode } => library.select(&Selection {
            scope: Scope::Folder { dir: dir.clone(), mode: *mode },
            query: query.clone(),
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
    /// The shared frame clock, read for the playing row's equalizer phase.
    animator: Entity<Animator>,
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
}

impl AlbumListView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        animator: Entity<Animator>,
        play: Play,
    ) -> Self {
        Self {
            library,
            playback,
            covers,
            animator,
            play,
            rows: RowList::new(),
            icon_scroll_y: std::cell::Cell::new(0.0),
            icon_scroll: SmoothScroll::default(),
            icon_max_scroll: std::cell::Cell::new(0.0),
            icon_focus: false,
            icon_viewport: std::rc::Rc::new(std::cell::Cell::new((0, 0))),
            seen_revision: 0,
        }
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
                _ => return,
            }
            cx.notify();
            return;
        }
        match self.rows.handle(action) {
            RowAction::Play(playlist, index) => self.play(playlist, index, cx),
            RowAction::Handled => cx.notify(),
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

    /// Play `playlist` from `index` the way `play` chose at construction: the
    /// whole playlist (browse/playlist views), or only the section's matched
    /// songs (search).
    fn play(&mut self, playlist: PlaylistId, index: usize, cx: &mut gpui::App) {
        // Playing consumes the selection: the played row carries the playing
        // highlight from here, and up/down re-anchor from it.
        self.rows.clear_selection();
        let playback = self.playback.clone();
        match self.play {
            Play::Playlist => self
                .library
                .update(cx, |state, cx| state.play_playlist(playlist, index, &playback, cx)),
            Play::Result => {
                let songs: Vec<_> = self
                    .rows
                    .sections()
                    .iter()
                    .find(|section| section.playlist == playlist)
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
    event: &gpui::MouseDownEvent,
    card: Option<PlaylistId>,
    cx: &mut Context<V>,
) {
    let request =
        SongMenuRequest { songs, position: event.position, playlist: context, queue: false, card };
    let library = view.list().library.clone();
    library.update(cx, |state, cx| state.request(Request::SongMenu(request), cx));
}

impl<V: AlbumList> RowActions<V> for Icons {
    fn activate(
        &self,
        view: &mut V,
        _item_ix: usize,
        playlist: PlaylistId,
        index: usize,
        _event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        view.list_mut().play(playlist, index, cx);
    }

    fn context(
        &self,
        view: &mut V,
        _item_ix: usize,
        songs: Vec<crate::model::SongId>,
        playlist: PlaylistId,
        context: Option<PlaylistId>,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        // The card's playlist turns on the card-only items: "Go to {artist}"
        // per credited artist and "Go to playlist".
        raise_menu(view, songs, context, event, Some(playlist), cx);
    }

    fn hover(&self, _view: &mut V, _item_ix: usize, _hovered: bool, _cx: &mut Context<V>) {}
}

impl<V: AlbumList> RowActions<V> for Rows {
    fn activate(
        &self,
        view: &mut V,
        item_ix: usize,
        playlist: PlaylistId,
        index: usize,
        event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        if view.list_mut().rows.click(item_ix, event.modifiers()) {
            cx.notify();
            return;
        }
        view.list_mut().play(playlist, index, cx);
    }

    fn context(
        &self,
        view: &mut V,
        item_ix: usize,
        _songs: Vec<crate::model::SongId>,
        _playlist: PlaylistId,
        context: Option<PlaylistId>,
        event: &gpui::MouseDownEvent,
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
    let _animator = cx.observe(animator, |this, animator, cx| {
        let dt = animator.read(cx).dt();
        let moved = this.list_mut().rows.tick(dt);
        // The icon grid's eased scroll advances on the same clock.
        let scrolled = this.list_mut().tick_icon_scroll(dt);
        // Keep the playing row's equalizer moving even when nothing else is.
        let playing = this.list().playback().read(cx).is_playing();
        if moved || scrolled || playing {
            cx.notify();
        }
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
    view.list_mut().icon_focus = false;
    let list = view.list();
    let current = list.playback.read(cx).current_song();
    let playing = list.playback.read(cx).is_playing();
    let live_bitrate_label =
        list.playback.read(cx).live_bitrate().map(|bps| SharedString::from(format_bitrate(bps)));
    let sections = list.rows.sections();
    let items = list.rows.items();
    let columns = list.rows.columns();
    let eq_phase = list.animator.read(cx).elapsed();
    let list_state = list.rows.list_state().clone();
    let highlight = list.rows.highlight();
    let slim = list.rows.slim();
    let covers = list.covers.clone();
    let library = list.library.clone();

    let render = cx.processor(move |_this, ix, _window, cx| {
        let row = albums::RowContext {
            theme,
            covers: &covers,
            library: &library,
            current,
            playing,
            eq_phase,
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
                cx.notify();
            }
            cx.stop_propagation();
        }),
    )
}

/// The icon view of the same sections: album covers packed into rows that
/// fill left to right and wrap down when full (see `albums::pack_rows`),
/// windowed to the viewport — only the cards near the scroll offset render,
/// so a huge library stays at full speed.
///
/// The viewport size is measured by a canvas inside the tree; when it changes
/// (first layout, a window resize, the rails moving) one re-render is
/// scheduled and the rows re-pack against the real size.
pub fn render_icons<V: AlbumList>(
    view: &mut V,
    theme: Theme,
    context: Option<PlaylistId>,
    _window: &mut Window,
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

    let list = view.list();
    let scroll_y = list.icon_scroll_y.get();

    let current = list.playback.read(cx).current_song();
    let eq_phase = list.animator.read(cx).elapsed();
    let highlight = list.rows.highlight();
    let covers = list.covers.clone();
    let library = list.library.clone();
    let row = albums::RowContext {
        theme,
        covers: &covers,
        library: &library,
        current,
        playing: false,
        eq_phase,
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
    let measurer = canvas(
        move |bounds, window, _cx| {
            let key =
                (f32::from(bounds.size.width).round() as u32, f32::from(bounds.size.height).round() as u32);
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
                    albums::render_icon_grid(&row, &sections, &layout, scroll_y, viewport_h, &icons, cx)
                        .into_any_element(),
                ),
        )
        .child(
            div().absolute().inset_0().on_scroll_wheel(
                cx.listener(move |this, event: &ScrollWheelEvent, _window, cx| {
                    // The same eased scroll the list view uses: a burst of
                    // wheel events accumulates into a target distance that
                    // decays toward zero, instead of stepping per event.
                    // (Clamping happens in `tick_icon_scroll`, against the
                    // laid-out height recorded by this render.)
                    this.list_mut().icon_scroll.accumulate(-wheel_pixels(event));
                    cx.notify();
                    cx.stop_propagation();
                }),
            ),
        )
        .into_any_element()
}

/// Route a navigation key on the list's root. Used by containers that keep
/// the list's key handling as-is (browse, playlist) — the search container
/// has its own, so it passes the box's keys there first.
pub fn list_key_listener<V: AlbumList>(view: &mut V, event: &KeyDownEvent, cx: &mut Context<V>) {
    if let Some(action) = action_for_key(&event.keystroke) {
        view.list_mut().dispatch(action, cx);
    }
}
