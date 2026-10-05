//! The play-queue panel.
//!
//! Rendered inline inside the lyrics rail, behind its Queue tab (the same
//! shared `queue_open` flag the transport's queue button toggles — both open
//! this one). Its header lives in the lyrics container's strip: the Lyrics |
//! Queue tabs, the queue's total time, and the CLEAR button.
//!
//! The rows are the playlist tables themselves: an [`AlbumListView`] in flat
//! mode (no album headers — every entry is a song), so the marquee's edge
//! fade and sprung snap-back, the selection crossfades, and the square /
//! rounded song-entry convention all come from the shared machinery. The
//! columns are sized to the panel's width rather than the tables' fixed
//! 48-character ones. Clicking a row jumps to it; right-click raises the
//! shared song menu, which offers "Remove from queue" here.
//!
//! The queue itself is the audio controller's path list; the section resolves
//! each path back to its library song for display, falling back to the file
//! name for paths the library doesn't know (files queued from the command
//! line before any scan).

use std::path::PathBuf;
use std::rc::Rc;
use gpui::{
    div, prelude::*, App, ClickEvent, Context, Entity, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Render, Window,
};

use crate::model::{PlaylistId, SongId};
use crate::ui::album_list::{self, AlbumList, AlbumListSubs, AlbumListView, Play};
use crate::ui::albums::{AlbumSection, RowActions, TrackRow};
use crate::ui::animation::Animator;
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::drag::Drag;
use crate::ui::format::{format_bitrate, format_duration};
use crate::ui::layout::RAIL_PX;
use crate::ui::library_state::{LibraryState, Request};
use crate::ui::marquee;
use crate::ui::menu::SongMenuRequest;
use crate::ui::playback_state::PlaybackState;
use crate::ui::row_list::Cell;
use crate::ui::widgets::empty_hint;

/// A row's horizontal padding (`px_4`) and its inter-cell gaps, subtracted
/// from the panel width before the text columns are sized. Slim rows end at
/// the filler after the artist — three `gap_3` gaps between four children.
const ROW_H_PAD_PX: f32 = 32.0;
const ROW_GAPS_PX: f32 = 36.0;

pub struct QueueView {
    playback: Entity<PlaybackState>,
    library: Entity<LibraryState>,
    themed: Themed,
    /// The shared table shell — rows, selection, marquees, virtualized list.
    list: AlbumListView,
    /// The queue entries the rows were last built from, and the controller
    /// generation they were built at — the snapshot happens only when the
    /// generation moves, never per frame.
    queue_entries: Vec<(SongId, PathBuf)>,
    queue_generation: u64,
    /// The queue's total time, computed with the snapshot.
    total_secs: u64,
    /// The queue-open flag last seen in render, so opening can scroll the
    /// playing entry into view.
    open_last: bool,
    /// A row to reveal once the panel has a viewport (the first render of a
    /// freshly opened panel has none yet).
    pending_reveal: Option<usize>,
    /// The row grab in flight — hold a row and move to reorder it.
    drag: Drag,
    _subs: AlbumListSubs,
}

impl QueueView {
    pub fn new(
        playback: Entity<PlaybackState>,
        library: Entity<LibraryState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut list =
            AlbumListView::new(library.clone(), playback.clone(), covers.clone(), animator.clone(), Play::Playlist);
        list.rows_mut().set_flat();
        list.rows_mut().set_slim();
        let subs = album_list::observe(&library, Some(&playback), &covers, &animator, cx);
        // Follow the playback state (the queue contents are read during
        // render; the open flag decides whether to draw at all).
        cx.observe(&playback, |_this, _state, cx| cx.notify())
        .detach();
        // Advance the drag ghost's chase spring on the shared frame clock —
        // the ghost renders at SMOOTH_FPS between the pointer's bursty events.
        // The rows tick through the album-list subscription, so only this is
        // handled here.
        cx.observe(&animator, |this, animator, cx| {
            let dt = animator.read(cx).dt();
            let drag = this.drag.tick(dt);
            if drag {
                cx.notify();
            }
        })
        .detach();
        // The queue renders the library's rows, so it shares the browse
        // container's typography — the same as the playlist and search tabs.
        let themed = BrowseView::themed(&config, cx);
        Self {
            playback,
            library,
            themed,
            list,
            queue_entries: Vec::new(),
            queue_generation: 0,
            total_secs: 0,
            open_last: false,
            pending_reveal: None,
            drag: Drag::default(),
            _subs: subs,
        }
    }

    /// The queue as one drawable section: every path a track row, numbered by
    /// queue position. `PlaylistId(0)` is inert — the row actions play from
    /// the queue index, not the section's playlist.
    fn queue_section(&self, queue: &[(SongId, PathBuf)], cx: &App) -> AlbumSection {
        let library = self.library.read(cx).library();
        let tracks = queue
            .iter()
            .enumerate()
            .map(|(index, (id, path))| {
                let (title, artist, duration, bitrate) = match library.get(*id) {
                    Some(song) => (
                        song.display_title(),
                        song.artists.join(", "),
                        format_duration(song.duration),
                        song.nominal_bitrate.map(format_bitrate).unwrap_or_default(),
                    ),
                    None => (
                        path.file_stem()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        "Unknown Artist".to_string(),
                        String::new(),
                        String::new(),
                    ),
                };
                TrackRow {
                    song: *id,
                    number: index + 1,
                    title,
                    artist,
                    duration: duration.into(),
                    bitrate: bitrate.into(),
                }
            })
            .collect();
        AlbumSection {
            playlist: PlaylistId(0),
            artist: String::new(),
            album: "Queue".to_string(),
            meta: String::new(),
            year: String::new(),
            date: None,
            cover_song: None,
            tracks,
        }
    }

    /// End the drag: drop the grabbed entry at the row it's over. A release
    /// without a grab does nothing.
    fn release_drag(&mut self, cx: &mut Context<Self>) {
        let dropped = self.drag.release(self.queue_entries.len());
        if let Some((from, to)) = dropped.filter(|(from, to)| from != to) {
            self.playback.update(cx, |state, cx| state.move_queue_entry(from, to, cx));
        }
        if dropped.is_some() {
            cx.notify();
        }
    }
}

impl Container for QueueView {
    fn container_id() -> &'static str {
        "queue"
    }
}

impl AlbumList for QueueView {
    fn list(&self) -> &AlbumListView {
        &self.list
    }

    fn list_mut(&mut self) -> &mut AlbumListView {
        &mut self.list
    }
}

/// The queue's row behavior: a modifier extends or toggles the selection; a
/// plain click jumps to that queue entry; a right-click raises the song menu
/// in its queue mode ("Remove from queue").
struct QueueRows;

impl RowActions<QueueView> for QueueRows {
    fn activate(
        &self,
        view: &mut QueueView,
        item_ix: usize,
        _playlist: PlaylistId,
        index: usize,
        event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<QueueView>,
    ) {
        // The press turned into a drag; the release isn't a click.
        if view.drag.is_drag() {
            return;
        }
        if view.list_mut().rows_mut().click(item_ix, event.modifiers()) {
            cx.notify();
            return;
        }
        view.playback.update(cx, |state, cx| state.play_queue_index(index, cx));
        // Playing consumes the selection; up/down re-anchor from the playing
        // entry.
        view.list_mut().rows_mut().clear_selection();
    }

    fn press(
        &self,
        view: &mut QueueView,
        item_ix: usize,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<QueueView>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let Some(bounds) = view.list().rows().item_bounds(item_ix) else {
            return;
        };
        view.drag.grab(item_ix, event.position, bounds);
        cx.notify();
    }

    fn context(
        &self,
        view: &mut QueueView,
        item_ix: usize,
        _songs: Vec<crate::model::SongId>,
        _playlist: PlaylistId,
        _context: Option<PlaylistId>,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<QueueView>,
    ) {
        let songs = view.list_mut().rows_mut().context_songs(item_ix);
        let request = SongMenuRequest {
            songs,
            position: event.position,
            playlist: None,
            queue: true,
            card: None,
        };
        let library = view.list().library().clone();
        library.update(cx, |state, cx| state.request(Request::SongMenu(request), cx));
    }

    fn hover(&self, view: &mut QueueView, item_ix: usize, hovered: bool, cx: &mut Context<QueueView>) {
        if view.list_mut().rows_mut().set_hover(item_ix, hovered) {
            cx.notify();
        }
    }

    fn cell_hover(
        &self,
        view: &mut QueueView,
        item_ix: usize,
        cell: Cell,
        hovered: bool,
        cx: &mut Context<QueueView>,
    ) {
        if view.list_mut().rows_mut().set_cell_hover(item_ix, cell, hovered) {
            cx.notify();
        }
    }
}

impl Render for QueueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        // Hidden: paint nothing and leave no hitbox. The lyrics container only
        // mounts this behind its Queue tab, so this is the closed state.
        if !self.playback.read(cx).queue_open() {
            return div().into_any_element();
        }

        // The rows size to the rail they live in.
        let width = f32::from(RAIL_PX);

        // The queue's contents can only have changed when the controller
        // bumped its generation — the cheap per-frame check. The full path
        // snapshot (and the total time it feeds) happens then, not every
        // frame, so a huge queue costs nothing while it just sits there.
        let generation = self.playback.read(cx).queue_generation();
        let queue_len = self.playback.read(cx).queue().0.len();
        let queue_changed = generation != self.queue_generation;
        if queue_changed {
            self.queue_generation = generation;
            self.queue_entries = self.playback.read(cx).queue().0.to_vec();
            let library = self.library.read(cx).library();
            self.total_secs = self
                .queue_entries
                .iter()
                .filter_map(|(id, _)| library.get(*id))
                .map(|song| song.duration.as_secs())
                .sum();
        }

        // A queue change under an active grab (a track finished, an entry was
        // removed) invalidates the row indices — drop the drag rather than
        // reorder into a stale list.
        if self.drag.is_active()
            && (queue_changed || self.drag.grabbed().is_some_and(|grabbed| grabbed >= queue_len))
        {
            self.drag.cancel();
        }

        // The columns scale to the panel's width — the tables' fixed
        // 48-character columns assume a window-wide center. One monospace
        // advance turns a width into the character count the marquee's travel
        // math wants.
        let advance = marquee::char_advance(window, theme, theme.cell_px());
        let text_w = (width - ROW_H_PAD_PX - ROW_GAPS_PX - theme.num_col()).max(0.0);
        let title_chars = ((text_w * 0.58 / advance) as usize).max(8);
        let artist_chars = ((text_w * 0.42 / advance) as usize).max(8);
        self.list.rows_mut().set_columns_chars(title_chars, artist_chars);

        // The in-flight drag, if the grab became one: rows part to make room
        // and the grabbed row follows the pointer as a ghost.
        let drag_info = if queue_len == 0 { None } else { self.drag.info(queue_len) };

        // Rebuild when the rows are stale or the queue moved; otherwise keep
        // the selection and marquee state.
        let fresh = if self.list.rows().is_dirty() || queue_changed {
            Some(vec![self.queue_section(&self.queue_entries, cx)])
        } else {
            None
        };
        self.list.rows_mut().sync(theme, window, fresh);
        // Up/down with nothing selected step from the playing entry — the flat
        // list's row index is the queue index.
        let loaded = self.playback.read(cx).queue().1;
        self.list.rows_mut().set_anchor(loaded);

        // Opening the panel scrolls the playing entry into view — no digging
        // through a long queue for it. The first render after the flip has no
        // viewport yet, so the reveal waits for one.
        let open = self.playback.read(cx).queue_open();
        if open && !self.open_last {
            self.pending_reveal = loaded;
        }
        self.open_last = open;
        if let Some(ix) = self.pending_reveal {
            if self.list.rows().viewport_height() > 0.0 {
                self.list.rows_mut().reveal(ix);
                self.pending_reveal = None;
            }
        }

        // The header is the tab strip's twin: same band height, same rule,
        // so the two containers' horizontal lines line up across the top.
        let mut panel = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .font_family(theme.font)
            // The drag's move/release listeners sit on the panel root, so a
            // drag keeps tracking however far the pointer roams inside it.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.drag.drag(event.position.y, this.queue_entries.len()) {
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                this.release_drag(cx);
            }))
            // A release outside the panel (past its top or bottom) ends the
            // drag too, so it can't be left hanging.
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                this.release_drag(cx);
            }));

        if queue_len == 0 {
            panel = panel.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(empty_hint(theme, "Queue is empty — play a playlist or a song.", true)),
            );
        } else {
            panel = panel.child(album_list::render_rows_with(
                self,
                theme,
                None,
                drag_info,
                Rc::new(QueueRows),
                cx,
            ));
        }

        panel.into_any_element()
    }
}
