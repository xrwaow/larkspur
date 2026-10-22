//! The lyrics panel — the right rail, above the now-playing cover.
//!
//! Reads the shared playback state, so it follows the current track. For synced
//! (`.lrc`) lyrics the active line is pinned to the middle of the panel — 50% of
//! its height — whatever the height of the lines around it: the lines before it
//! hug its top edge from below and the lines after it flow on beneath. At the
//! start of a song the upper half is simply empty and the text scrolls up
//! through it as playback advances. Clicking a line seeks to it.
//!
//! **The scroll leads the song.** A line change doesn't start the slide when the
//! line begins — the slide *finishes* on the line's timestamp, so it begins at
//! `timestamp - TRANSITION_SECS`. That means the layout and the highlight are
//! separate: the stack moves toward the next line while the current one is still
//! the one being sung, and the highlight only transfers when the line actually
//! starts. Timing that precisely is why this panel drives off the shared frame
//! clock and reads the playback position at full resolution rather than waiting
//! for the playback tick.
//!
//! Lines wrap on word boundaries rather than being clipped. The anchor is kept
//! at 50% by construction rather than by centering a fixed block, so a wrapped
//! line no longer drags it off centre.
//!
//! Lyrics arrive with the rest of a song's metadata, so this panel never loads
//! anything for the text itself. Its one load is the procedural texture that
//! fills the panel when a track has no lyrics: the embedded cover is read on a
//! background thread and the finished texture is cached for the current song
//! and theme base.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, relative, AnyElement, ClickEvent, Context, Entity, ObjectFit, Render,
    RenderImage, Rgba, Subscription, Window,
};

use crate::model::{lrclib, DynamicBase, LyricLine, Lyrics, SongId, TextureMode};
use crate::ui::animation::{Animator, Tween};
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::format::format_duration;
use crate::ui::playback_state::PlaybackState;
use crate::ui::playback::svg_icon;
use crate::ui::queue::QueueView;
use crate::ui::cover_store::CoverStore;
use crate::ui::texture::LoadedTexture;
use crate::ui::tabs::{strip_height_px, TabsView};
use crate::ui::library_state::LibraryState;
use crate::ui::theme::Theme;
use crate::ui::widgets::{blend, dim, edge_fade, empty_hint, Side};

/// How long the lyric stack takes to slide to a new line — and therefore how
/// far *before* a line's timestamp the slide starts, so it lands on time.
const TRANSITION_SECS: f32 = 0.20;

/// How much each line further from the sung one dims toward the panel
/// background. The line right next to the sung one keeps full colour, then each
/// line out drops 20%, 40%, ... of its colour.
const DIM_STEP: f32 = 0.20;

/// Far lines stop dimming here so they never bleed completely into the panel —
/// their shape still reads as text, just barely.
const DIM_MAX: f32 = 0.80;

/// A slide further than this many lines (a seek) snaps instead of scrolling.
const SNAP_LINES: f32 = 5.0;

/// How many square texture tiles to stack down the panel. The texture is
/// generated at a fixed square size; tiling it keeps that scale instead of
/// stretching one copy over the whole (usually tall, narrow) rail. The body
/// clips any surplus, so this only needs to exceed the tallest panel divided by
/// its width.
const TEXTURE_TILES: usize = 32;

/// How much of the texture shows through behind actual lyrics. Enough to read
/// the procedural surface, faint enough that the text stays the focus.
const TEXTURE_BEHIND_TEXT_OPACITY: f32 = 0.35;

/// An in-flight or completed LRCLIB lookup for one song.
///
/// A preview is deliberately not written to disk: it is shown in place of the
/// current lyrics until the user accepts it. Keeping the song id here lets a
/// lookup that finishes after a track change be discarded instead of showing
/// the wrong track's lyrics.
struct LyricsFetch {
    song: SongId,
    state: FetchState,
}

enum FetchState {
    Loading,
    Preview(Lyrics),
    Failed,
}

/// A rendered procedural texture for one cover on one theme base. Rendering
/// happens off-thread; the panel draws the last completed one while the next
/// is being prepared.
struct CachedTexture {
    /// The song this texture has been resolved for.
    song: SongId,
    /// The cover hash this texture was rendered from. Two songs with the same
    /// cover bytes share a seed, and therefore the same texture.
    seed: u64,
    base: DynamicBase,
    image: Arc<RenderImage>,
}

pub struct LyricsView {
    state: Entity<PlaybackState>,
    /// The shared config, read for the tab strip's height so this panel's
    /// header rule lines up with it.
    config: Entity<ConfigState>,
    /// Read for song durations, to total the queue time in the header.
    library: Entity<LibraryState>,
    /// The queue, embedded in this rail behind the Queue tab. Shares the same
    /// `queue_open` flag as the transport's queue button.
    queue: Entity<QueueView>,
    themed: Themed,
    /// The pixel offset the stack is drawn at while it eases into place.
    offset: Tween,
    /// The line the stack is laid out around — the middle slot. Leads the active
    /// line by [`TRANSITION_SECS`].
    anchor: Option<usize>,
    /// The position the highlight amounts are computed from.
    position: Duration,
    /// The track the state above belongs to, so a track change snaps rather than
    /// scrolling from the previous song's position.
    song: Option<SongId>,
    /// The queue's total time, shown beside the tabs. Computed with the queue
    /// snapshot — only when the controller's generation moves.
    total_secs: u64,
    queue_generation: u64,
    /// The procedural texture that fills the panel when a track has no lyrics.
    texture: Option<CachedTexture>,
    /// A (song, base) pair with a render in flight, so frames don't spawn
    /// duplicates.
    texture_attempt: Option<(SongId, DynamicBase)>,
    /// A (song, base) pair that produced no texture — no embedded art, or art
    /// too small to quilt. Cached so the panel doesn't retry every frame.
    texture_failed: Option<(SongId, DynamicBase)>,
    /// The frame clock, held so a new line's slide can wake it from sleep.
    animator: Entity<Animator>,
    /// The current LRCLIB lookup or its unsaved preview. Only rendered when it
    /// belongs to the playing song.
    fetch: Option<LyricsFetch>,
    _observe: Subscription,
    _observe_library: Subscription,
    _observe_animator: Subscription,
}

impl LyricsView {
    pub fn new(
        state: Entity<PlaybackState>,
        library: Entity<LibraryState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let queue = cx.new(|cx| {
            QueueView::new(
                state.clone(),
                library.clone(),
                covers.clone(),
                config.clone(),
                animator.clone(),
                cx,
            )
        });
        let wake_animator = animator.clone();
        let observe = cx.observe(&state, move |this, _state, cx| {
            this.sync(cx);
            wake_animator.read(cx).request();
            cx.notify();
        });
        // Accepting lyrics replaces the library's metadata for this track; the
        // panel re-syncs and redraws from the new value.
        let observe_library = cx.observe(&library, |this, _library, cx| {
            this.sync(cx);
            cx.notify();
        });
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            let dt = animator.read(cx).dt();
            let moved = this.offset.tick(dt);
            let changed = this.sync(cx);
            if moved || changed {
                animator.read(cx).request();
                cx.notify();
            }
        });
        let themed = Themed::new(&config, cx);
        let mut this = Self {
            state,
            config,
            library,
            queue,
            themed,
            offset: Tween::new(0.0),
            anchor: None,
            position: Duration::ZERO,
            song: None,
            total_secs: 0,
            queue_generation: 0,
            texture: None,
            texture_attempt: None,
            texture_failed: None,
            animator,
            fetch: None,
            _observe: observe,
            _observe_library: observe_library,
            _observe_animator: observe_animator,
        };
        this.sync(cx);
        this
    }

    /// Keep the procedural texture for this cover/base up to date.
    ///
    /// Rendering reads and decodes the embedded art, so it runs on the
    /// background executor. Before rendering, the cover's seed is compared
    /// with the texture on screen: a song that shares the previous song's art
    /// keeps that texture instead of rendering an identical one.
    fn ensure_texture(
        &mut self,
        song: SongId,
        path: PathBuf,
        base: DynamicBase,
        cx: &mut Context<Self>,
    ) {
        let key = (song, base);
        if self
            .texture
            .as_ref()
            .is_some_and(|cached| cached.song == song && cached.base == base)
            || self.texture_attempt == Some(key)
            || self.texture_failed == Some(key)
        {
            return;
        }
        self.texture_attempt = Some(key);

        // Only a texture on the same theme base can be reused.
        let keep = self
            .texture
            .as_ref()
            .filter(|cached| cached.base == base)
            .map(|cached| cached.seed);

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { crate::ui::texture::load_for_song(&path, base, keep) })
                .await;

            this.update(cx, |this, cx| {
                if this.texture_attempt != Some(key) {
                    return; // A newer request superseded this one.
                }
                this.texture_attempt = None;
                match loaded {
                    Some(LoadedTexture::Unchanged) => {
                        // Same cover: the texture already on screen applies.
                        if let Some(cached) = this.texture.as_mut() {
                            cached.song = song;
                        }
                        this.texture_failed = None;
                    }
                    Some(LoadedTexture::Rendered { seed, image }) => {
                        this.texture = Some(CachedTexture { song, seed, base, image });
                        this.texture_failed = None;
                    }
                    None => {
                        // Don't leave the previous song's texture standing in
                        // for art that couldn't be read.
                        this.texture = None;
                        this.texture_failed = Some(key);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the position and line up the anchor, the highlight, and the
    /// slide. Returns whether anything the panel draws changed.
    ///
    /// Called both when playback reports and on every frame of the shared clock,
    /// because the slide has to *start* between playback reports to land on a
    /// line's timestamp.
    fn sync(&mut self, cx: &mut Context<Self>) -> bool {
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        let (song, position, anchor, fading) = {
            let state = self.state.read(cx);
            let metadata = state.metadata(cx);
            let position = state.live_position();
            let song = metadata.as_ref().map(|m| m.id);
            let lyrics = preview_lyrics(&self.fetch, song)
                .or_else(|| metadata.as_ref().map(|m| &m.lyrics));
            let (anchor, fading) = match lyrics {
                Some(Lyrics::Synced(lines)) if !lines.is_empty() => (
                    Some(anchor_at(lines, position, transition)),
                    // Keep re-rendering while a colour is crossing, so the fade
                    // advances frame by frame even between playback reports.
                    is_fading(lines, position, transition),
                ),
                _ => (None, false),
            };
            (song, position, anchor, fading)
        };

        let mut changed = fading;
        self.position = position;

        // The header's total queue time — re-summed only when the queue moves.
        let generation = self.state.read(cx).queue_generation();
        if generation != self.queue_generation {
            self.queue_generation = generation;
            let library = self.library.read(cx).library();
            let (queue, _) = self.state.read(cx).queue();
            self.total_secs = queue
                .iter()
                .filter_map(|entry| library.get(entry.song))
                .map(|song| song.duration.as_secs())
                .sum();
            changed = true;
        }

        if song != self.song {
            self.song = song;
            self.anchor = None;
            self.offset.set(0.0);
            // A lookup for the previous track is useless now, even if it's
            // still in flight (its completion checks the song before landing).
            if self.fetch.as_ref().is_some_and(|fetch| Some(fetch.song) != song) {
                self.fetch = None;
            }
            changed = true;
        }

        if anchor != self.anchor {
            // Draw the new layout one line off and ease back to zero: the
            // content appears to scroll into place, landing as the line starts.
            let line = self.themed.theme().line_height();
            match (self.anchor, anchor) {
                (Some(previous), Some(next)) => {
                    let delta = (next as f32 - previous as f32) * line;
                    if delta.abs() <= line * SNAP_LINES {
                        self.offset.set(delta);
                        self.offset.to(0.0, TRANSITION_SECS);
                        self.animator.read(cx).request();
                    } else {
                        self.offset.set(0.0);
                    }
                }
                _ => self.offset.set(0.0),
            }
            self.anchor = anchor;
            changed = true;
        }

        changed
    }

    /// Ask LRCLIB for the playing track's lyrics and hold the result as an
    /// unsaved preview. The request runs on the background executor; the
    /// completion is dropped unless it still belongs to the current lookup.
    fn fetch_lyrics(&mut self, cx: &mut Context<Self>) {
        let Some(song) = self.state.read(cx).metadata(cx) else { return };
        let id = song.id;
        self.fetch = Some(LyricsFetch { song: id, state: FetchState::Loading });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { lrclib::lookup(&song) })
                .await;
            this.update(cx, |this, cx| {
                if !matches!(&this.fetch, Some(fetch) if fetch.song == id) {
                    return; // Superseded by another lookup or a track change.
                }
                let state = match result {
                    Ok(Some(lyrics)) => FetchState::Preview(lyrics),
                    Ok(None) => FetchState::Failed,
                    Err(error) => {
                        eprintln!("lrclib: {error:#}");
                        FetchState::Failed
                    }
                };
                this.fetch = Some(LyricsFetch { song: id, state });
                this.sync(cx);
                this.animator.read(cx).request();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Embed the previewed lyrics in the song's tag, then sync just this
    /// library entry — no filesystem rescan.
    fn accept_lyrics(&mut self, cx: &mut Context<Self>) {
        let Some(song) = self.state.read(cx).metadata(cx) else { return };
        let Some(LyricsFetch { song: fetched, state: FetchState::Preview(lyrics) }) =
            self.fetch.as_ref()
        else {
            return;
        };
        if *fetched != song.id {
            return;
        }
        let lyrics = lyrics.clone();
        let path = song.path.clone();

        match crate::model::song::embed_lyrics(&path, &lyrics) {
            Ok(()) => {
                self.fetch = None;
                self.library.update(cx, |library, cx| {
                    library.set_lyrics(song.id, lyrics, cx);
                });
                self.sync(cx);
                cx.notify();
            }
            Err(error) => {
                eprintln!("failed to embed lyrics in {}: {error}", path.display());
                self.fetch = Some(LyricsFetch { song: song.id, state: FetchState::Failed });
                cx.notify();
            }
        }
    }

    /// Discard the preview without touching disk; the previous lyrics (if any)
    /// come straight back.
    fn reject_lyrics(&mut self, cx: &mut Context<Self>) {
        self.fetch = None;
        self.sync(cx);
        cx.notify();
    }
}

impl Container for LyricsView {
    fn container_id() -> &'static str {
        "lyrics"
    }
}

impl Render for LyricsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let tabs_font = self
            .config
            .read(cx)
            .font_size(TabsView::container_id(), TabsView::default_font_size());
        let header_h = strip_height_px(tabs_font, theme.cell_px() + 2.0);
        let offset = self.offset.value();
        let anchor = self.anchor.unwrap_or(0);
        let position = self.position;
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        // The body is built against the borrowed lyrics, so nothing is cloned
        // per frame — the click handlers only need `cx` immutably.
        let metadata = self.state.read(cx).metadata(cx);
        let queue_open = self.state.read(cx).queue_open();
        let current_song = metadata.as_ref().map(|m| m.id);

        // The procedural cover-art texture fills the body, according to the
        // Texture setting: off, only when there are no lyrics, or always. The
        // queue takes precedence over all of it and still opens as before.
        // A preview counts as "lyrics" here, so the texture steps aside while
        // the user is deciding whether to keep it.
        let no_lyrics = {
            let preview = preview_lyrics(&self.fetch, current_song);
            !has_lyrics(preview.or_else(|| metadata.as_ref().map(|m| &m.lyrics)))
        };
        let texture_mode = self.config.read(cx).lyrics_texture();
        let texture_wanted = match texture_mode {
            TextureMode::Off => false,
            TextureMode::NoLyrics => no_lyrics,
            TextureMode::Always => true,
        };
        let mut texture = None;
        if texture_wanted && !queue_open {
            if let Some(song) = metadata.as_ref() {
                let base = self.config.read(cx).texture_base();
                self.ensure_texture(song.id, song.path.clone(), base, cx);
                texture = self
                    .texture
                    .as_ref()
                    .filter(|cached| cached.base == base)
                    .map(|cached| cached.image.clone());
            }
        }

        // The header is just a label: "Lyrics" normally, and "Queue" plus the
        // total time and the CLEAR button while the queue is swapped in (the
        // transport's queue button toggles the same shared `queue_open` flag
        // that switches this panel).

        // The queue swaps in over the lyrics body. The edge fades belong to
        // the lyrics panel itself, so they apply to the texture body too —
        // the queue has its own viewport.
        let body: AnyElement = if queue_open {
            self.queue.clone().into_any_element()
        } else {
            let shown = preview_lyrics(&self.fetch, current_song)
                .or_else(|| metadata.as_ref().map(|m| &m.lyrics));
            let content: AnyElement = match shown {
                Some(Lyrics::Plain(text)) if !text.trim().is_empty() => div()
                    .id("lyrics-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .pb_3()
                    .text_size(px(theme.cell_px()))
                    .text_color(theme.text_muted)
                    .children(text.lines().map(|line| {
                        // `flex_none` stops a wrapped line from being shrunk below
                        // its content height and drawing over the next one.
                        div()
                            .w_full()
                            .flex_none()
                            .whitespace_normal()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(centered_lyric_text(line))
                    }))
                    .into_any_element(),
                Some(Lyrics::Synced(lines)) if !lines.is_empty() => {
                    // Dim relative to the line actually being sung, not the anchor:
                    // the anchor leads the slide, but the highlighted line is what
                    // the eye calls "current". `progress` carries that line toward
                    // the next one over the same window the highlight crossfades, so
                    // the whole stack breathes in and out rather than snapping dim
                    // levels on each stamp.
                    let current = current_line(lines, position);
                    let progress = transition_progress(lines, current, position, transition);
                    let line = |index: usize, cx: &Context<Self>| -> AnyElement {
                        let timestamp = lines[index].timestamp;
                        let resting = dim(
                            theme.text_faint,
                            theme.rail_bg,
                            dim_amount(index, current, progress),
                        );
                        let color = blend(
                            resting,
                            theme.accent,
                            highlight_amount(lines, index, position, transition),
                        );
                        synced_line(
                            theme,
                            index,
                            &lines[index].text,
                            color,
                            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.seek_to(timestamp);
                                    cx.notify();
                                });
                            }),
                        )
                    };

                    let before: Vec<AnyElement> = (0..anchor).map(|index| line(index, cx)).collect();
                    let after: Vec<AnyElement> =
                        (anchor + 1..lines.len()).map(|index| line(index, cx)).collect();

                    // The anchor line is pinned to the middle of the panel no matter
                    // how tall the lines around it are. Each half is exactly half
                    // the panel tall, so centering the stack (`justify_center`) puts
                    // the anchor line's *centre* at 50%: the lines before it hug its
                    // top edge from below, the lines after it flow on beneath. The
                    // halves aren't clipped themselves — the panel is — so the
                    // `offset` above can slide the whole stack without a line
                    // vanishing at the half boundary.
                    div()
                        .size_full()
                        .overflow_hidden()
                        .child(
                            div()
                                .relative()
                                .top(px(offset))
                                .size_full()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap_1()
                                .px_3()
                                .child(
                                    div()
                                        .w_full()
                                        .h(relative(0.5))
                                        .flex_none()
                                        .flex()
                                        .flex_col()
                                        .justify_end()
                                        .gap_1()
                                        .children(before),
                                )
                                .child(line(anchor, cx))
                                .child(
                                    div()
                                        .w_full()
                                        .h(relative(0.5))
                                        .flex_none()
                                        .flex()
                                        .flex_col()
                                        .justify_start()
                                        .gap_1()
                                        .children(after),
                                ),
                        )
                        .into_any_element()
                }
                _ => empty_lyrics_hint(theme),
            };

            match texture {
                // No lyrics: the texture is the body.
                Some(image) if no_lyrics => tiled_texture(image),
                // Lyrics: keep them in front, with the texture dimmed behind.
                Some(image) => texture_behind_text(image, content),
                // Still rendering, no art to quilt, or the setting is off.
                None => content,
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .child(if queue_open {
                div()
                    .h(px(header_h))
                    .pt_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text)
                            .child("Queue"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .when(self.total_secs > 0, |d| {
                                d.child(
                                    div()
                                        .text_size(px(theme.small_px()))
                                        .text_color(theme.text)
                                        .child(format_duration(Duration::from_secs(self.total_secs))),
                                )
                            })
                            .child(
                                div()
                                    .id("queue-clear")
                                    .flex_none()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.state.update(cx, |state, cx| state.clear_queue(cx));
                                    }))
                                    .child(svg_icon(
                                        theme.small_px(),
                                        theme.small_px(),
                                        "assets/clear_queue.svg",
                                        theme.text,
                                    )),
                            ),
                    )
            } else {
                let fetch = self
                    .fetch
                    .as_ref()
                    .filter(|fetch| Some(fetch.song) == current_song);
                // The search icon covers both actions: a fresh lookup, and
                // looking for a better version when lyrics already exist.
                let get = |cx: &mut Context<Self>| {
                    header_icon_button(theme, "lyrics-get", "assets/search.svg", theme.text_muted)
                        .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.fetch_lyrics(cx);
                        }))
                        .into_any_element()
                };
                let right: AnyElement = match fetch.map(|fetch| &fetch.state) {
                    Some(FetchState::Loading) => div()
                        .flex_none()
                        .opacity(0.4)
                        .child(svg_icon(
                            theme.small_px(),
                            theme.small_px(),
                            "assets/search.svg",
                            theme.text_muted,
                        ))
                        .into_any_element(),
                    Some(FetchState::Preview(_)) => div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            header_icon_button(
                                theme,
                                "lyrics-accept",
                                "assets/accept.svg",
                                theme.accent,
                            )
                            .on_click(cx.listener(
                                |this, _event: &ClickEvent, _window, cx| {
                                    this.accept_lyrics(cx);
                                },
                            )),
                        )
                        .child(
                            header_icon_button(
                                theme,
                                "lyrics-cancel",
                                "assets/cancel.svg",
                                theme.text,
                            )
                            .on_click(cx.listener(
                                |this, _event: &ClickEvent, _window, cx| {
                                    this.reject_lyrics(cx);
                                },
                            )),
                        )
                        .into_any_element(),
                    Some(FetchState::Failed) | None => get(cx),
                };
                div()
                    .h(px(header_h))
                    .pt_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text)
                            .child("Lyrics"),
                    )
                    .child(right)
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .when(!queue_open, |d| {
                        d.child(edge_fade(theme.rail_bg, theme.font_size * 2.0, Side::Top))
                            .child(edge_fade(theme.rail_bg, theme.font_size * 2.0, Side::Bottom))
                    }),
            )
    }
}

/// One clickable header icon. The caller attaches the click handler, which
/// keeps the search/accept/cancel actions visually identical.
fn header_icon_button(
    theme: Theme,
    id: &'static str,
    icon: &'static str,
    color: Rgba,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .cursor_pointer()
        .child(svg_icon(theme.small_px(), theme.small_px(), icon, color))
}

/// Whether lyrics have anything to show — used for the texture decision.
fn has_lyrics(lyrics: Option<&Lyrics>) -> bool {
    match lyrics {
        Some(Lyrics::Plain(text)) => !text.trim().is_empty(),
        Some(Lyrics::Synced(lines)) => !lines.is_empty(),
        Some(Lyrics::None) | None => false,
    }
}

/// The unsaved preview for `song`, if the active lookup has finished and
/// belongs to that song.
fn preview_lyrics(fetch: &Option<LyricsFetch>, song: Option<SongId>) -> Option<&Lyrics> {
    fetch
        .as_ref()
        .filter(|fetch| Some(fetch.song) == song)
        .and_then(|fetch| match &fetch.state {
            FetchState::Preview(lyrics) => Some(lyrics),
            _ => None,
        })
}

/// The panel's empty body: the procedural texture repeated downward at its
/// natural square scale. Each tile is a full-width square, so adjacent copies
/// meet seamlessly — the texture itself is tileable.
fn tiled_texture(image: Arc<RenderImage>) -> AnyElement {
    // Keep each copy at the rendered image's own pixel size rather than
    // stretching one over the panel. The texture is square and tileable, so
    // stacking them downward is seamless.
    let tile_px = image.size(0).width.0 as f32;
    let mut column = div().size_full().overflow_hidden().flex().flex_col();
    for index in 0..TEXTURE_TILES {
        column = column.child(
            img(image.clone())
                .id(("lyrics-texture", index))
                .w_full()
                .h(px(tile_px))
                .flex_none()
                .object_fit(ObjectFit::Fill),
        );
    }
    column.into_any_element()
}

/// Lyrics over the texture: the tiled surface sits behind, softened so the
/// text keeps its contrast, and the content is layered on top at full opacity.
fn texture_behind_text(image: Arc<RenderImage>, content: AnyElement) -> AnyElement {
    div()
        .relative()
        .size_full()
        .child(
            div()
                .absolute()
                .inset_0()
                .opacity(TEXTURE_BEHIND_TEXT_OPACITY)
                .child(tiled_texture(image)),
        )
        .child(div().relative().size_full().child(content))
        .into_any_element()
}

/// The panel's empty-state hint, nudged down a touch so the top fade doesn't
/// dim it into the background.
fn empty_lyrics_hint(theme: Theme) -> AnyElement {
    div()
        .pt_2()
        .child(empty_hint(theme, "No lyrics for this track.", false))
        .into_any_element()
}

/// The line the stack should be laid out around at `position`.
///
/// Leads the colour transition by `transition`, so the slide toward a line
/// *finishes* when that line starts rather than beginning then — the text
/// arrives with the vocal instead of trailing it.
fn anchor_at(lines: &[LyricLine], position: Duration, transition: Duration) -> usize {
    lines
        .iter()
        .rposition(|line| line.timestamp.saturating_sub(transition) <= position)
        .unwrap_or(0)
}

/// The line being sung at `position` — the last one whose stamp has passed.
/// Unlike [`anchor_at`] this does **not** lead the slide: it tracks the actual
/// highlight, which is what the dimming is measured outward from.
fn current_line(lines: &[LyricLine], position: Duration) -> usize {
    lines
        .iter()
        .rposition(|line| line.timestamp <= position)
        .unwrap_or(0)
}

/// How far the sung line has travelled toward the next one at `position`, as
/// 0..1 over the same second-half window [`highlight_amount`] uses. This is what
/// lets the dim levels change gradually instead of stepping on each timestamp:
/// `current + progress` is the continuously moving "now" the dimming measures
/// distance from.
fn transition_progress(
    lines: &[LyricLine],
    current: usize,
    position: Duration,
    transition: Duration,
) -> f32 {
    let half = transition / 2;
    lines
        .get(current + 1)
        .map(|next| ramp(position, next.timestamp.saturating_sub(half), next.timestamp))
        .unwrap_or(0.0)
}

/// How far to dim the line at `index`, given the sung line at `current` and how
/// far it has moved toward the next (`progress`, 0..1), as a fraction of its
/// colour toward the background. The sung line and its immediate neighbours are
/// undimmed; from there each line out dims another [`DIM_STEP`], capped at
/// [`DIM_MAX`] so far lines don't disappear entirely.
fn dim_amount(index: usize, current: usize, progress: f32) -> f32 {
    let now = current as f32 + progress.clamp(0.0, 1.0);
    let distance = (index as f32 - now).abs();
    let steps = (distance - 1.0).max(0.0);
    (steps * DIM_STEP).min(DIM_MAX)
}

/// How strongly the line at `index` is highlighted at `position`, as 0..1.
///
/// The colour **fades in over the second half of the slide** — the next line
/// starts colouring up once the stack is already moving toward it — and the line
/// it replaces fades out over the very same window, so the two cross. That lands
/// the colour change on the beat rather than switching it instantly.
fn highlight_amount(
    lines: &[LyricLine],
    index: usize,
    position: Duration,
    transition: Duration,
) -> f32 {
    let half = transition / 2;
    let start = lines[index].timestamp;
    let rising = ramp(position, start.saturating_sub(half), start);
    let falling = lines
        .get(index + 1)
        .map(|next| ramp(position, next.timestamp.saturating_sub(half), next.timestamp))
        .unwrap_or(0.0);
    (rising - falling).clamp(0.0, 1.0)
}

/// Whether `position` is inside a highlight fade window, so the panel keeps
/// re-rendering while a colour crosses.
fn is_fading(lines: &[LyricLine], position: Duration, transition: Duration) -> bool {
    let half = transition / 2;
    // Timestamps are sorted, so the next line to start is a partition point.
    let next = lines.partition_point(|line| line.timestamp <= position);
    lines
        .get(next)
        .is_some_and(|line| line.timestamp <= position + half)
}

/// Where `position` sits in `[from, to]`, as 0..1. An empty window (a line at
/// zero, whose lead saturates) reads as already reached.
fn ramp(position: Duration, from: Duration, to: Duration) -> f32 {
    if position >= to {
        return 1.0;
    }
    if position <= from || to <= from {
        return 0.0;
    }
    position.saturating_sub(from).as_secs_f32() / (to - from).as_secs_f32()
}

fn synced_line(
    theme: Theme,
    index: usize,
    text: &str,
    color: Rgba,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(("lyric", index))
        .w_full()
        .min_w_0()
        .min_h(px(theme.line_height()))
        // Without this the flex column shrinks each line back toward `min_h`,
        // and a wrapped line's overflow paints over the line below it.
        .flex_none()
        .px_2()
        .py_1()
        .cursor_pointer()
        .whitespace_normal()
        .text_size(px(theme.cell_px()))
        .text_color(color)
        .flex()
        .items_center()
        .justify_center()
        .on_click(on_click)
        .child(centered_lyric_text(text))
        .into_any_element()
}

/// A lyric line centred word by word.
///
/// GPUI's `text_align: center` does not move a single-line text node: the node
/// is only as wide as its content, so centring it inside a full-width block is
/// a no-op, while stretching the node to full width breaks wrapping. Laying the
/// words out as shrink-to-fit flex items lets `flex_wrap` do both jobs — a
/// short line is one centred row, and a long line breaks into centred rows.
fn centered_lyric_text(text: &str) -> AnyElement {
    let words: Vec<&str> = text.split_whitespace().collect();
    let count = words.len();
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_center()
        .children(words.into_iter().enumerate().map(move |(index, word)| {
            // A non-breaking space keeps the gap measurable as its own item;
            // plain trailing spaces can be trimmed from the item's width.
            let word = if index + 1 < count {
                format!("{word}\u{00a0}")
            } else {
                word.to_string()
            };
            div().flex_none().child(word)
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(seconds: &[u64]) -> Vec<LyricLine> {
        seconds
            .iter()
            .map(|s| LyricLine { timestamp: Duration::from_secs(*s), text: format!("line {s}") })
            .collect()
    }

    #[test]
    fn a_line_is_fully_coloured_from_its_stamp() {
        let lines = lines(&[0, 10]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        assert_eq!(highlight_amount(&lines, 0, Duration::ZERO, transition), 1.0);
        assert_eq!(highlight_amount(&lines, 0, Duration::from_secs(9), transition), 1.0);
        assert_eq!(highlight_amount(&lines, 1, Duration::from_secs(10), transition), 1.0);
    }

    #[test]
    fn nothing_is_coloured_before_the_first_line() {
        let lines = lines(&[5, 10]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        assert_eq!(highlight_amount(&lines, 0, Duration::ZERO, transition), 0.0);
    }

    #[test]
    fn the_anchor_leads_the_colour_transition() {
        let lines = lines(&[0, 10, 20]);
        let transition = Duration::from_millis(500);

        // Well before a line the anchor is still the previous one.
        assert_eq!(anchor_at(&lines, Duration::from_secs(9), transition), 0);
        // Inside the lead window the stack has already moved on...
        assert_eq!(anchor_at(&lines, Duration::from_millis(9_500), transition), 1);
        // ...while the colour hasn't started crossing — it waits for halfway.
        assert_eq!(highlight_amount(&lines, 0, Duration::from_millis(9_500), transition), 1.0);
        assert_eq!(highlight_amount(&lines, 1, Duration::from_millis(9_500), transition), 0.0);
    }

    #[test]
    fn the_anchor_starts_on_the_first_line() {
        let lines = lines(&[5, 10]);
        // The first line's lead window saturates at zero rather than going
        // negative, so the anchor is the first line from the very start.
        assert_eq!(anchor_at(&lines, Duration::ZERO, Duration::from_millis(500)), 0);
    }

    #[test]
    fn the_next_line_colours_up_over_the_second_half_of_the_slide() {
        let lines = lines(&[0, 10]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        let half = transition / 2;
        let start = Duration::from_secs(10);

        // Before the halfway mark the next line isn't coloured at all.
        let early = start - half - Duration::from_millis(10);
        assert_eq!(highlight_amount(&lines, 1, early, transition), 0.0);
        // The fade begins exactly halfway into the slide...
        assert_eq!(highlight_amount(&lines, 1, start - half, transition), 0.0);
        // ...is half done a quarter of the way before the stamp...
        let mid = highlight_amount(&lines, 1, start - half / 2, transition);
        assert!((mid - 0.5).abs() < 0.02, "amount {mid}");
        // ...and lands fully coloured on the stamp.
        assert_eq!(highlight_amount(&lines, 1, start, transition), 1.0);
    }

    #[test]
    fn the_previous_line_fades_out_as_the_next_fades_in() {
        let lines = lines(&[0, 10]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        let half = transition / 2;
        let mid = Duration::from_secs(10) - half / 2;

        let leaving = highlight_amount(&lines, 0, mid, transition);
        let arriving = highlight_amount(&lines, 1, mid, transition);
        assert!((leaving - 0.5).abs() < 0.02, "leaving {leaving}");
        assert!((arriving - 0.5).abs() < 0.02, "arriving {arriving}");
    }

    #[test]
    fn the_panel_only_keeps_rendering_while_a_colour_is_crossing() {
        let lines = lines(&[0, 10]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        let half = transition / 2;

        assert!(!is_fading(&lines, Duration::from_secs(5), transition), "idle between lines");
        assert!(is_fading(&lines, Duration::from_secs(10) - half / 2, transition), "mid-cross");
        assert!(!is_fading(&lines, Duration::from_secs(10), transition), "settled on the stamp");
    }

    #[test]
    fn the_sung_line_tracks_the_stamp_without_leading_it() {
        let lines = lines(&[0, 10, 20]);
        assert_eq!(current_line(&lines, Duration::from_secs(5)), 0);
        assert_eq!(current_line(&lines, Duration::from_secs(10)), 1);
        assert_eq!(current_line(&lines, Duration::from_secs(19)), 1);
        // Before the first stamp it stays on the first line rather than underflowing.
        assert_eq!(current_line(&lines, Duration::ZERO), 0);
    }

    #[test]
    fn lines_dim_by_twenty_percent_for_each_step_away() {
        // At rest (`progress == 0`) the sung line and its immediate neighbours
        // keep full colour, then each line out drops another 20%, capped so far
        // lines stay legible.
        assert_eq!(dim_amount(4, 4, 0.0), 0.0);
        assert_eq!(dim_amount(3, 4, 0.0), 0.0);
        assert_eq!(dim_amount(5, 4, 0.0), 0.0);
        assert!((dim_amount(2, 4, 0.0) - 0.20).abs() < 1e-6);
        assert!((dim_amount(1, 4, 0.0) - 0.40).abs() < 1e-6);
        assert!((dim_amount(0, 4, 0.0) - 0.60).abs() < 1e-6);
        // Eight-plus lines out clamps at DIM_MAX instead of vanishing.
        assert_eq!(dim_amount(20, 4, 0.0), DIM_MAX);
    }

    #[test]
    fn dim_levels_crossfade_with_the_highlight() {
        // Halfway between lines 4 and 5 the dimming is measured from 4.5, so the
        // levels either side are partway to their next value — no snap on the
        // stamp.
        assert!((dim_amount(3, 4, 0.5) - 0.10).abs() < 1e-6);
        assert!((dim_amount(2, 4, 0.5) - 0.30).abs() < 1e-6);
        assert!((dim_amount(6, 4, 0.5) - 0.10).abs() < 1e-6);
        assert!((dim_amount(7, 4, 0.5) - 0.30).abs() < 1e-6);
        // At the end of the window the values match the next line being current.
        assert!((dim_amount(3, 4, 1.0) - dim_amount(3, 5, 0.0)).abs() < 1e-6);
    }

    #[test]
    fn dimming_progress_follows_the_highlight_window() {
        let lines = lines(&[0, 10, 20]);
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        let half = transition / 2;
        let start = Duration::from_secs(10);

        // Line 0 is current until line 1's stamp at `start`.
        assert_eq!(current_line(&lines, start - Duration::from_millis(1)), 0);
        // Long before line 1 there's no movement toward it.
        assert_eq!(
            transition_progress(&lines, 0, start - half - Duration::from_millis(10), transition),
            0.0
        );
        // It starts exactly halfway into the slide...
        assert_eq!(transition_progress(&lines, 0, start - half, transition), 0.0);
        // ...is half done a quarter before its stamp...
        let mid = transition_progress(&lines, 0, start - half / 2, transition);
        assert!((mid - 0.5).abs() < 0.02, "progress {mid}");
        // ...and reaches line 1 exactly on its stamp.
        assert_eq!(transition_progress(&lines, 0, start, transition), 1.0);
        // The last line has nowhere to move to.
        assert_eq!(transition_progress(&lines, 2, start + half, transition), 0.0);
    }
}
