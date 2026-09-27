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

use gpui::{prelude::*, Context, Entity, KeyDownEvent, Render, ScrollWheelEvent, SharedString, Subscription, Window};

use crate::model::select::{Order, Scope, Selection};
use crate::model::{search, InputAction, Library, PlaylistId};
use crate::ui::albums::{self, AlbumSection, RowActions};
use crate::ui::animation::Animator;
use crate::ui::cover_store::CoverStore;
use crate::ui::drag::DragInfo;
use crate::ui::format::format_bitrate;
use crate::ui::input::action_for_key;
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::menu::SongMenuRequest;
use crate::ui::playback_state::PlaybackState;
use crate::ui::row_list::{Cell, RowAction, RowList};
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
/// discography, keeping its newest-first order. A playlist scope is shown
/// as-is — its view offers no box.
pub fn sections(library: &Library, scope: &Scope, query: &search::Query) -> Vec<AlbumSection> {
    let groups = match scope {
        Scope::Library => search::search(library, query),
        Scope::Artist(artist) => library.select(&Selection {
            scope: Scope::Artist(artist.clone()),
            query: query.clone(),
            order: Order::NewestFirst,
        }),
        Scope::Playlists(ids) => {
            return ids
                .iter()
                .filter_map(|id| albums::section_for_playlist(library, *id))
                .collect();
        }
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
        Self { library, playback, covers, animator, play, rows: RowList::new(), seen_revision: 0 }
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

    /// The row keyboard handler: navigation, selection, and play.
    pub fn dispatch<V: Render>(&mut self, action: InputAction, cx: &mut Context<V>) {
        match self.rows.handle(action) {
            RowAction::Play(playlist, index) => self.play(playlist, index, cx),
            RowAction::Handled => cx.notify(),
            RowAction::Ignored => {}
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
        _song: crate::model::SongId,
        context: Option<PlaylistId>,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<V>,
    ) {
        let songs = view.list_mut().rows.context_songs(item_ix);
        let request =
            SongMenuRequest { songs, position: event.position, playlist: context, queue: false };
        let library = view.list().library.clone();
        library.update(cx, |state, cx| state.request(Request::SongMenu(request), cx));
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
        // Keep the playing row's equalizer moving even when nothing else is.
        let playing = this.list().playback().read(cx).is_playing();
        if moved || playing {
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

/// Route a navigation key on the list's root. Used by containers that keep
/// the list's key handling as-is (browse, playlist) — the search container
/// has its own, so it passes the box's keys there first.
pub fn list_key_listener<V: AlbumList>(view: &mut V, event: &KeyDownEvent, cx: &mut Context<V>) {
    if let Some(action) = action_for_key(&event.keystroke) {
        view.list_mut().dispatch(action, cx);
    }
}
