//! The lyrics panel — the right rail, above the now-playing cover.
//!
//! Reads the shared playback state, so it follows the current track. For synced
//! (`.lrc`) lyrics the active line is pinned to the middle of the panel — 50% of
//! its height — whatever the height of the lines around it: the lines before it
//! hug its top edge from below and the lines after it flow on beneath. At the
//! start of a song the upper half is simply empty and the text scrolls up
//! through it as playback advances. Clicking a line seeks to it.
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

use crate::model::{LyricLine, Lyrics};
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::state::PlaybackState;
use crate::ui::theme::Theme;

pub struct LyricsView {
    state: Entity<PlaybackState>,
    theme: Theme,
    _observe: Subscription,
    _observe_config: Subscription,
}

impl LyricsView {
    pub fn new(
        state: Entity<PlaybackState>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |_this, _state, cx| cx.notify());
        let observe_config = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
        Self { state, theme, _observe: observe, _observe_config: observe_config }
    }
}

impl Container for LyricsView {
    fn container_id() -> &'static str {
        "lyrics"
    }
}

impl Render for LyricsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        // The body is built against the borrowed lyrics, so nothing is cloned
        // per frame — the click handlers only need `cx` immutably, which is
        // compatible with holding the playback read.
        let state = self.state.read(cx);
        let metadata = state.metadata();
        let title = metadata.display_title();

        let body: AnyElement = match &metadata.lyrics {
            Lyrics::None => hint(theme, "No lyrics for this track."),
            Lyrics::Plain(text) => div()
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
            Lyrics::Synced(lines) if lines.is_empty() => hint(theme, "No lyrics for this track."),
            Lyrics::Synced(lines) => {
                let active = active_line(lines, state.position());
                // Anchor on the active line, or the first one before playback
                // reaches it.
                let anchor = active.unwrap_or(0);

                let line = |index: usize, cx: &Context<Self>| -> AnyElement {
                    let timestamp = lines[index].timestamp;
                    synced_line(
                        theme,
                        index,
                        &lines[index].text,
                        active == Some(index),
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

                // The active line is pinned to the middle of the panel no matter
                // how tall the lines around it are. Each half is exactly half
                // the panel tall, so centering the stack (`justify_center`) puts
                // the active line's *centre* at 50%: the lines before it hug its
                // top edge from below, the lines after it flow on beneath. At the
                // start of a song the upper half is simply empty — "nothing above
                // the middle yet" — and the text scrolls up through it as
                // playback advances. Wrapping a line taller than a slot no longer
                // drags the anchor off centre, which is what centering the whole
                // block used to do.
                div()
                    .size_full()
                    .overflow_hidden()
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
                            .overflow_hidden()
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
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .justify_start()
                            .gap_1()
                            .children(after),
                    )
                    .into_any_element()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
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
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_faint)
                            .child("Lyrics"),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// The index of the line that should be highlighted at `position`.
fn active_line(lines: &[LyricLine], position: Duration) -> Option<usize> {
    lines.iter().rposition(|line| line.timestamp <= position)
}

fn synced_line(
    theme: Theme,
    index: usize,
    text: &str,
    active: bool,
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
        .text_color(if active { theme.accent } else { theme.text_faint })
        .hover(|d| d.bg(theme.row_hover).text_color(theme.text_muted))
        .on_click(on_click)
        .child(text.to_string())
        .into_any_element()
}

fn hint(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_string())
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
    fn the_active_line_is_the_last_one_that_has_started() {
        let lines = lines(&[0, 10, 20]);
        assert_eq!(active_line(&lines, Duration::ZERO), Some(0));
        assert_eq!(active_line(&lines, Duration::from_secs(5)), Some(0));
        assert_eq!(active_line(&lines, Duration::from_secs(10)), Some(1), "a line starts on its stamp");
        assert_eq!(active_line(&lines, Duration::from_secs(25)), Some(2));
    }

    #[test]
    fn nothing_is_active_before_the_first_line() {
        let lines = lines(&[5, 10]);
        assert_eq!(active_line(&lines, Duration::ZERO), None);
    }
}
