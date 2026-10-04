//! The view-mode toggle bar — a thin band docked to the very bottom of the
//! window, below the transport bar.
//!
//! It holds two pill buttons, "List View" and "Icon View", styled like the
//! dots of a sliding switch: the active mode's pill carries the accent colour,
//! the inactive one the dimmed grey, with the bar's own background colour as
//! the text — the label set into the dot. Clicking a pill picks that mode; the
//! choice lives in [`Config`], so it persists across restarts and every
//! library container (browse, artist, folder, search) flips live.

use gpui::{
    div, prelude::*, px, ClickEvent, Context, ElementId, Render, SharedString, Window,
};

use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::format::format_bitrate;
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;

pub struct ViewToggleView {
    themed: Themed,
    config: gpui::Entity<ConfigState>,
    playback: gpui::Entity<PlaybackState>,
    _observe_playback: gpui::Subscription,
}

impl ViewToggleView {
    pub fn new(
        config: gpui::Entity<ConfigState>,
        playback: gpui::Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The playing song's info is read from shared playback state, so the
        // bar re-renders on every track change (and every live-bitrate tick).
        let _observe_playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        Self { themed: Themed::new(&config, cx), config, playback, _observe_playback }
    }

    fn set_icon(&self, on: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.config.update(cx, |state, cx| state.set_icon_view(on, cx));
    }

    /// The playing song's `{title} by {artist}` — the label the bottom bar
    /// carries, since icon view names no songs over the covers.
    fn track_label(&self, cx: &Context<Self>) -> SharedString {
        let state = self.playback.read(cx);
        match state.metadata(cx) {
            Some(meta) => {
                let artist = meta.artists.join(", ");
                if artist.is_empty() {
                    meta.display_title().into()
                } else {
                    format!("{} by {}", meta.display_title(), artist).into()
                }
            }
            None => "".into(),
        }
    }

    /// The live bitrate while it's known, the declared one otherwise.
    fn bitrate_label(&self, cx: &Context<Self>) -> SharedString {
        let state = self.playback.read(cx);
        let meta = state.metadata(cx);
        state
            .live_bitrate()
            .or(meta.as_ref().and_then(|meta| meta.nominal_bitrate))
            .map(|bps| SharedString::from(format_bitrate(bps)))
            .unwrap_or_default()
    }
}

impl Container for ViewToggleView {
    fn container_id() -> &'static str {
        "view_toggle"
    }
}

impl Render for ViewToggleView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let icon_on = self.config.read(cx).icon_view();
        let track_label = self.track_label(cx);
        let bitrate_label = self.bitrate_label(cx);

        // The bar: the mode pills on the left, a divider, then the playing
        // song's info and the live bitrate. The info is centered against the
        // *container* — laid out in an absolutely positioned full-width layer
        // — so a 3- vs 4-digit bitrate or the pills' width can't shift it.
        div()
            .relative()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .py_1()
            .bg(theme.rail_bg)
            .font_family(theme.font)
            .text_size(px(theme.small_px()))
            .child(mode_button(theme, "List View", !icon_on, false, cx))
            .child(mode_button(theme, "Icon View", icon_on, true, cx))
            .child(divider(theme))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .max_w_full()
                            .truncate()
                            .text_center()
                            .text_size(px(theme.cell_px()))
                            .text_color(theme.text)
                            .child(track_label),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0(),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(theme.small_px()))
                    .text_color(theme.text_muted)
                    .child(bitrate_label),
            )
            .into_any_element()
    }
}

/// The thin vertical rule between the mode pills and the song info.
fn divider(theme: Theme) -> gpui::AnyElement {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .flex_none()
        .bg(theme.border)
        .into_any_element()
}

/// One of the two mode pills, in the sliding switch's language: the active
/// mode fills with the accent, the inactive one with the dimmed grey, and the
/// text is set in the bar's background colour — the label inside the dot.
/// Clicking it picks that mode.
fn mode_button(
    theme: crate::ui::theme::Theme,
    text: &'static str,
    active: bool,
    icon: bool,
    cx: &mut Context<ViewToggleView>,
) -> gpui::AnyElement {
    div()
        .id(ElementId::Name(text.into()))
        .cursor_pointer()
        .px_2()
        .py(px(2.0))
        .rounded_full()
        .bg(if active { theme.accent } else { theme.text_faint })
        .text_color(theme.rail_bg)
        .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
            this.set_icon(icon, window, cx);
        }))
        .child(text)
        .into_any_element()
}
