use gpui::{div, prelude::*, px, rgb, Context, Render, Window};

use crate::ui::container::Container;

pub struct CoverView {
    pub has_art: bool,
}

impl Container for CoverView {
    fn container_id() -> &'static str { "cover" }
}

impl Render for CoverView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size(px(240.0))
            .bg(rgb(0x202020))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(0x555555))
            .child(if self.has_art { "🎵" } else { "No Cover" })
    }
}
