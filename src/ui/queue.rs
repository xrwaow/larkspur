//! The play-queue panel.
//!
//! An overlay over the tab container's right side, opened from the queue
//! button in the transport (to the right of "next"). It spans from the player
//! controls' right edge to the tabs' right end — the region right of the
//! transport block — and slides in from behind the tabs' right edge when
//! shown, sliding back out when hidden.
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

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, ClickEvent, Context, Entity, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Render, Window,
};

use crate::model::{generate_song_id, PlaylistId};
use crate::ui::album_list::{self, AlbumList, AlbumListSubs, AlbumListView, Play};
use crate::ui::albums::{AlbumSection, RowActions, TrackRow};
use crate::ui::animation::{Animator, Tween};
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::{Container, SECTION_GAP};
use crate::ui::cover_store::CoverStore;
use crate::ui::drag::Drag;
use crate::ui::format::format_duration;
use crate::ui::layout::RAIL_PX;
use crate::ui::library_state::LibraryState;
use crate::ui::marquee;
use crate::ui::menu::SongMenuRequest;
use crate::ui::playback::BAR_WIDTH;
use crate::ui::playback_state::PlaybackState;
use crate::ui::row_list::Cell;
use crate::ui::tabs::{self, TabsView};
use crate::ui::widgets::empty_hint;

/// How long the slide in/out takes.
const SLIDE_SECS: f32 = 0.22;

/// Narrowest the panel may get, so a small window can't squeeze it to nothing
/// (it then overlaps the transport's right portion instead).
const MIN_WIDTH_PX: f32 = 240.0;

/// A row's horizontal padding (`px_4`) and its inter-cell gaps, subtracted
/// from the panel width before the text columns are sized. Slim rows end at
/// the filler after the artist — three `gap_3` gaps between four children.
const ROW_H_PAD_PX: f32 = 32.0;
const ROW_GAPS_PX: f32 = 36.0;

pub struct QueueView {
    playback: Entity<PlaybackState>,
    library: Entity<LibraryState>,
    /// Read for the tab strip's font size, so the header's rule lines up
    /// with the tabs' one even when the two containers' sizes differ.
    config: Entity<ConfigState>,
    themed: Themed,
    /// The panel's slide: 0.0 fully hidden (sunk past the tabs' right edge),
    /// 1.0 fully shown. Driven by the shared frame clock.
    slide: Tween,
    /// The queue-open flag last seen in the playback state, so the slide only
    /// retargets when the flag flips.
    open_target: bool,
    /// The shared table shell — rows, selection, marquees, virtualized list.
    list: AlbumListView,
    /// The queue paths the rows were last built from, and the controller
    /// generation they were built at — the snapshot happens only when the
    /// generation moves, never per frame.
    queue_paths: Vec<PathBuf>,
    queue_generation: u64,
    /// The queue's total time, computed with the snapshot.
    total_secs: u64,
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
        // Follow the playback state for the open flag that drives the slide
        // (the queue contents are read during render).
        cx.observe(&playback, |this, state, cx| {
            let open = state.read(cx).queue_open();
            if open != this.open_target {
                this.open_target = open;
                // Showing slides in from the right; hiding slides back out.
                this.slide.to(if open { 1.0 } else { 0.0 }, SLIDE_SECS);
            }
            cx.notify();
        })
        .detach();
        // Advance the slide and the drag ghost's chase spring on the shared
        // frame clock — the ghost renders at SMOOTH_FPS between the pointer's
        // bursty events. The rows tick through the album-list subscription,
        // so only these two are handled here.
        cx.observe(&animator, |this, animator, cx| {
            let dt = animator.read(cx).dt();
            let slide = this.slide.tick(dt);
            let drag = this.drag.tick(dt);
            if slide || drag {
                cx.notify();
            }
        })
        .detach();
        // The queue renders the library's rows, so it shares the browse
        // container's typography — the same as the playlist and search tabs.
        let themed =
            Themed::with_container(BrowseView::container_id(), BrowseView::default_font_size(), &config, cx);
        Self {
            playback,
            library,
            config: config.clone(),
            themed,
            slide: Tween::new(0.0),
            open_target: false,
            list,
            queue_paths: Vec::new(),
            queue_generation: 0,
            total_secs: 0,
            drag: Drag::default(),
            _subs: subs,
        }
    }

    /// The queue as one drawable section: every path a track row, numbered by
    /// queue position. `PlaylistId(0)` is inert — the row actions play from
    /// the queue index, not the section's playlist.
    fn queue_section(&self, queue: &[PathBuf], cx: &App) -> AlbumSection {
        let library = self.library.read(cx).library();
        let tracks = queue
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let song = generate_song_id(path).unwrap_or(0);
                match library.get(song) {
                    Some(song) => TrackRow {
                        song: song.id,
                        number: index + 1,
                        title: song.display_title(),
                        artist: song.artists.join(", "),
                        secs: song.duration.as_secs(),
                        nominal_bitrate: song.nominal_bitrate,
                    },
                    None => TrackRow {
                        song,
                        number: index + 1,
                        title: path
                            .file_stem()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        artist: "Unknown Artist".to_string(),
                        secs: 0,
                        nominal_bitrate: None,
                    },
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
        let dropped = self.drag.release(self.queue_paths.len());
        if let Some((from, to)) = dropped.filter(|(from, to)| from != to) {
            self.playback.update(cx, |state, cx| state.move_queue_entry(from, to, cx));
        }
        if dropped.is_some() {
            cx.notify();
        }
    }

    /// A queue entry's display labels — the library's metadata when the path
    /// is scanned, the file name otherwise.
    fn entry_labels(&self, path: &Path, cx: &App) -> (String, String) {
        let library = self.library.read(cx).library();
        match library.get(generate_song_id(path).unwrap_or(0)) {
            Some(song) => (song.display_title(), song.artists.join(", ")),
            None => (
                path.file_stem().unwrap_or(path.as_os_str()).to_string_lossy().into_owned(),
                "Unknown Artist".to_string(),
            ),
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
        _song: crate::model::SongId,
        _context: Option<PlaylistId>,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<QueueView>,
    ) {
        let songs = view.list_mut().rows_mut().context_songs(item_ix);
        let request =
            SongMenuRequest { songs, position: event.position, playlist: None, queue: true };
        let library = view.list().library().clone();
        library.update(cx, |state, cx| state.request_song_menu(request, cx));
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

        // Fully slid out: paint nothing and leave no hitbox.
        if self.slide.value() <= 0.0 && !self.slide.is_animating() {
            return div().into_any_element();
        }

        // The panel's settled span: from the player controls' right edge (the
        // centered BAR_WIDTH block) to the tabs' right end. Both rails and the
        // section gap inset the tab area from the window edges.
        let inset = RAIL_PX + SECTION_GAP;
        let viewport_w = f32::from(window.viewport_size().width);
        let tabs_w = (viewport_w - 2.0 * inset).max(0.0);
        let final_left =
            (((viewport_w + BAR_WIDTH) / 2.0 - inset).min(tabs_w - MIN_WIDTH_PX)).max(0.0);
        let width = (tabs_w - final_left).max(0.0);

        // Slide the rigid panel in from past the tabs' right edge — the dock
        // stack's overflow_hidden clips it there, so it reads as sliding out
        // from under the right rail. Leftward while showing, back out
        // rightward while hiding.
        let offset = (1.0 - ease_out_cubic(self.slide.value())) * width;

        // The queue's contents can only have changed when the controller
        // bumped its generation — the cheap per-frame check. The full path
        // snapshot (and the total time it feeds) happens then, not every
        // frame, so a huge queue costs nothing while it just sits there.
        let generation = self.playback.read(cx).queue_generation();
        let queue_len = self.playback.read(cx).queue().0.len();
        let queue_changed = generation != self.queue_generation;
        if queue_changed {
            self.queue_generation = generation;
            self.queue_paths = self.playback.read(cx).queue().0.to_vec();
            let library = self.library.read(cx).library();
            self.total_secs = self
                .queue_paths
                .iter()
                .filter_map(|path| library.get(generate_song_id(path).unwrap_or(0)))
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
        let advance = marquee::char_advance(window, theme);
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
            Some(vec![self.queue_section(&self.queue_paths, cx)])
        } else {
            None
        };
        self.list.rows_mut().sync(theme, window, fresh);
        // Up/down with nothing selected step from the playing entry — the flat
        // list's row index is the queue index.
        let loaded = self.playback.read(cx).queue().1;
        self.list.rows_mut().set_anchor(loaded);

        // The header is the tab strip's twin: same band height, same rule,
        // so the two containers' horizontal lines line up across the top.
        let tabs_font =
            self.config.read(cx).font_size(TabsView::container_id(), TabsView::default_font_size());
        let strip_h = tabs::strip_height_px(tabs_font, theme.font_size);

        let clear = div()
            .id("queue-clear")
            .flex_none()
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(theme.small_px()))
            .text_color(theme.text)
            .bg(theme.row_active)
            .hover(|d| d.bg(theme.row_hover))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.playback.update(cx, |state, cx| state.clear_queue(cx));
            }))
            .child("Clear");

        let mut panel = div()
            .absolute()
            .top(px(0.0))
            .bottom(px(0.0))
            .left(px(final_left + offset))
            .w(px(width))
            // Block clicks from falling through to the tabs underneath.
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.panel_bg)
            .font_family(theme.font)
            // The drag's move/release listeners sit on the panel root, so a
            // drag keeps tracking however far the pointer roams inside it.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.drag.drag(event.position.y, this.queue_paths.len()) {
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
            }))
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap_4()
                    .px_4()
                    .pb_1()
                    .h(px(strip_h))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(theme.cell_px() + 2.0))
                                    .text_color(theme.text)
                                    .child("Queue"),
                            )
                            .when(self.total_secs > 0, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(theme.small_px()))
                                        .text_color(theme.text_faint)
                                        .child(format_duration(Duration::from_secs(self.total_secs))),
                                )
                            }),
                    )
                    .child(clear),
            );

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

        // The dragged row, drawn on top of the list and following the pointer.
        // `top` is in window coordinates; the panel spans the tabs container,
        // whose top is the window's, so they coincide.
        if let Some(info) = drag_info {
            let path = self.queue_paths[info.grabbed].clone();
            let (title, artist) = self.entry_labels(&path, cx);
            let columns = self.list.rows().columns();
            panel = panel.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .right(px(0.0))
                    .top(px(info.top))
                    .h(px(info.row_height))
                    // Swallow the pointer while dragging: no row hover or
                    // click underneath, and the move/up events still bubble
                    // here to the panel root.
                    .occlude()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .rounded_md()
                    .bg(theme.row_active)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().w(px(theme.num_col())).flex_none())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.cell_px()))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(columns.artist))
                            .truncate()
                            .text_size(px(theme.cell_px()))
                            .text_color(theme.text_muted)
                            .child(artist),
                    ),
            );
        }

        panel.into_any_element()
    }
}

/// Fast at the start, easing to a stop — the standard "settle" curve.
fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}
