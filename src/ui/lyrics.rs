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
//! Lines wrap rather than being clipped: GPUI wraps on word boundaries and
//! breaks a single over-long word per character, which is exactly the fallback
//! we want. The anchor is kept at 50% by construction rather than by centering a
//! fixed block, so a wrapped line no longer drags it off centre.
//!
//! Lyrics arrive with the rest of a song's metadata, so this panel never loads
//! anything itself.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, relative, AnyElement, ClickEvent, Context, Entity, Render, Subscription,
    Window,
};

use crate::model::{LyricLine, Lyrics, SongId};
use crate::ui::animation::{Animator, Tween};
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::format::format_duration;
use crate::ui::playback_state::PlaybackState;
use crate::ui::queue::QueueView;
use crate::ui::cover_store::CoverStore;
use crate::ui::library_state::LibraryState;
use crate::ui::theme::Theme;
use crate::ui::widgets::{blend, edge_fade, empty_hint, Side};

/// How long the lyric stack takes to slide to a new line — and therefore how
/// far *before* a line's timestamp the slide starts, so it lands on time.
const TRANSITION_SECS: f32 = 0.20;

/// A slide further than this many lines (a seek) snaps instead of scrolling.
const SNAP_LINES: f32 = 5.0;

pub struct LyricsView {
    state: Entity<PlaybackState>,
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
    _observe: Subscription,
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
        let observe = cx.observe(&state, |this, _state, cx| {
            this.sync(cx);
            cx.notify();
        });
        let observe_animator = cx.observe(&animator, |this, animator, cx| {
            let dt = animator.read(cx).dt();
            let moved = this.offset.tick(dt);
            let changed = this.sync(cx);
            if moved || changed {
                cx.notify();
            }
        });
        let themed = Themed::new(&config, cx);
        let mut this = Self {
            state,
            library,
            queue,
            themed,
            offset: Tween::new(0.0),
            anchor: None,
            position: Duration::ZERO,
            song: None,
            total_secs: 0,
            queue_generation: 0,
            _observe: observe,
            _observe_animator: observe_animator,
        };
        this.sync(cx);
        this
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
            let (anchor, fading) = match metadata.as_ref().map(|m| &m.lyrics) {
                Some(Lyrics::Synced(lines)) if !lines.is_empty() => (
                    Some(anchor_at(lines, position, transition)),
                    // Keep re-rendering while a colour is crossing, so the fade
                    // advances frame by frame even between playback reports.
                    is_fading(lines, position, transition),
                ),
                _ => (None, false),
            };
            (metadata.map(|m| m.id), position, anchor, fading)
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
                .filter_map(|(id, _)| library.get(*id))
                .map(|song| song.duration.as_secs())
                .sum();
            changed = true;
        }

        if song != self.song {
            self.song = song;
            self.anchor = None;
            self.offset.set(0.0);
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
}

impl Container for LyricsView {
    fn container_id() -> &'static str {
        "lyrics"
    }
}

impl Render for LyricsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let offset = self.offset.value();
        let anchor = self.anchor.unwrap_or(0);
        let position = self.position;
        let transition = Duration::from_secs_f32(TRANSITION_SECS);
        // The body is built against the borrowed lyrics, so nothing is cloned
        // per frame — the click handlers only need `cx` immutably.
        let metadata = self.state.read(cx).metadata(cx);
        let queue_open = self.state.read(cx).queue_open();

        // The header is just a label: "Lyrics" normally, and "Queue" plus the
        // total time and the CLEAR button while the queue is swapped in (the
        // transport's queue button toggles the same shared `queue_open` flag
        // that switches this panel).

        // The queue swaps in over the lyrics body. The edge fades belong to
        // the scrolling lyrics only — the queue has its own viewport.
        let body: AnyElement = if queue_open {
            self.queue.clone().into_any_element()
        } else { match metadata.as_ref().map(|m| &m.lyrics) {
            Some(Lyrics::Plain(text)) => div()
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
                    div().w_full().flex_none().whitespace_normal().child(line.to_string())
                }))
                .into_any_element(),
            Some(Lyrics::Synced(lines)) if !lines.is_empty() => {
                let line = |index: usize, cx: &Context<Self>| -> AnyElement {
                    let timestamp = lines[index].timestamp;
                    synced_line(
                        theme,
                        index,
                        &lines[index].text,
                        highlight_amount(lines, index, position, transition),
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
        }
        .into_any_element() };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .child(if queue_open {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
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
                                        .text_color(theme.text_faint)
                                        .child(format_duration(Duration::from_secs(self.total_secs))),
                                )
                            })
                            .child(
                                div()
                                    .id("queue-clear")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(theme.small_px()))
                                    .text_color(theme.text)
                                    .bg(theme.row_active)
                                    .hover(|d| d.bg(theme.row_hover))
                                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.state.update(cx, |state, cx| state.clear_queue(cx));
                                    }))
                                    .child("CLEAR"),
                            ),
                    )
            } else {
                div()
                    .flex()
                    .items_center()
                    .px_4()
                    .py_3()
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_faint)
                            .child("Lyrics"),
                    )
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
    highlight: f32,
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
        // Wrap on word boundaries, breaking a single over-long word per
        // character — the fallback for a line with no spaces at all.
        .whitespace_normal()
        .text_size(px(theme.cell_px()))
        .text_color(blend(theme.text_faint, theme.accent, highlight))
        .hover(|d| d.bg(theme.row_hover).text_color(theme.text_muted))
        .on_click(on_click)
        .child(text.to_string())
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
}
