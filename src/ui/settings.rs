//! The settings panel.
//!
//! Opened from the left rail's settings button, it's an overlay the tab
//! container swaps in over the active tab (like search). It edits the shared
//! [`ConfigState`]: the theme, per-container typefaces and font sizes, and the
//! synced-path list. Every change is applied live and persisted — there's no
//! "save".
//!
//! The synced-path list is the source of truth for what's scanned into the
//! library; [`LibraryState`](super::library_state::LibraryState) observes the
//! config and rescans when it changes.

use std::path::PathBuf;

use gpui::{
    div, prelude::*, px, AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, Subscription, Window,
};

use crate::model::config::{FontKind, ThemeKind, MAX_FONT_SIZE, MIN_FONT_SIZE};
use crate::ui::browse::BrowseView;
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::lyrics::LyricsView;
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::playback::PlaybackView;
use crate::ui::playlist::PlaylistView;
use crate::ui::playlists::PlaylistsView;
use crate::ui::search::SearchView;
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;

/// An open per-container typeface menu: where it was raised and which container
/// it edits.
struct FontMenu {
    position: Point<Pixels>,
    container: &'static str,
}

pub struct SettingsView {
    config: Entity<ConfigState>,
    theme: Theme,
    path: TextField,
    /// The open typeface menu, if any.
    font_menu: Option<FontMenu>,
    focus_handle: FocusHandle,
    _observe: Subscription,
}

impl SettingsView {
    pub fn new(config: Entity<ConfigState>, cx: &mut Context<Self>) -> Self {
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
        let observe = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        Self {
            config,
            theme,
            path: TextField::default(),
            font_menu: None,
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// The focus handle the tab container hands focus to when it opens settings.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Add whatever path is typed in the box, then clear it.
    fn add_path(&mut self, cx: &mut Context<Self>) {
        let path = self.path.value.trim();
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(path);
        self.config.update(cx, |config, cx| {
            config.add_root(path, cx);
        });
        self.path.clear();
        cx.notify();
    }
}

impl Container for SettingsView {
    fn container_id() -> &'static str {
        "settings"
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Every container the settings panel exposes a typeface and font size for: id,
/// label, and its built-in default size (from [`Container::default_font_size`]).
fn containers() -> Vec<(&'static str, &'static str, f32)> {
    vec![
        (TabsView::container_id(), "Tabs", TabsView::default_font_size()),
        (BrowseView::container_id(), "Browse", BrowseView::default_font_size()),
        (PlaylistView::container_id(), "Playlist", PlaylistView::default_font_size()),
        (SearchView::container_id(), "Search", SearchView::default_font_size()),
        (PlaylistsView::container_id(), "Playlists rail", PlaylistsView::default_font_size()),
        (LyricsView::container_id(), "Lyrics", LyricsView::default_font_size()),
        (PlaybackView::container_id(), "Transport", PlaybackView::default_font_size()),
        (SettingsView::container_id(), "Settings", SettingsView::default_font_size()),
    ]
}

/// One container's current typeface and font size, snapshotted for rendering.
struct ContainerRow {
    id: &'static str,
    label: &'static str,
    default_size: f32,
    size: f32,
    font: FontKind,
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let config = self.config.read(cx);
        let theme_kind = config.theme_kind();
        let roots: Vec<PathBuf> = config.roots().to_vec();
        let rows: Vec<ContainerRow> = containers()
            .into_iter()
            .map(|(id, label, default_size)| ContainerRow {
                id,
                label,
                default_size,
                size: config.font_size(id, default_size),
                font: config.font_for(id, FontKind::default()),
            })
            .collect();

        let mut items: Vec<AnyElement> = Vec::new();

        // --- theme ---
        items.push(section(theme, "Theme"));
        let theme_buttons = ThemeKind::ALL.iter().map(|kind| {
            let kind = *kind;
            theme_button(
                theme,
                kind,
                theme_kind == kind,
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.config.update(cx, |config, cx| config.set_theme(kind, cx));
                }),
            )
        });
        items.push(div().flex().gap_2().px_4().pb_2().children(theme_buttons).into_any_element());

        // --- typeface + font size, per container ---
        items.push(section(theme, "Font"));
        items.push(
            div()
                .px_4()
                .pb_1()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child("Per container. Pick a family, step the size; Reset restores both defaults.")
                .into_any_element(),
        );
        for (index, row) in rows.into_iter().enumerate() {
            items.push(container_row(theme, index, row, cx));
        }

        // --- synced paths ---
        items.push(section(theme, "Synced paths"));
        if roots.is_empty() {
            items.push(
                div()
                    .px_4()
                    .pb_1()
                    .text_size(px(theme.small_px()))
                    .text_color(theme.text_faint)
                    .child("No paths yet — add a music directory below.")
                    .into_any_element(),
            );
        }
        for (index, root) in roots.into_iter().enumerate() {
            items.push(root_row(theme, index, root, cx));
        }
        items.push(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .pt_2()
                .child(div().flex_1().min_w_0().child(self.path.render(theme, "/path/to/music")))
                .child(action_button(
                    theme,
                    "add-path",
                    "Add",
                    cx.listener(|this, _event: &ClickEvent, _window, cx| this.add_path(cx)),
                ))
                .into_any_element(),
        );

        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" && this.font_menu.is_some() {
                    this.font_menu = None;
                    cx.notify();
                    return;
                }
                if event.keystroke.key == "enter" {
                    this.add_path(cx);
                    return;
                }
                if this.path.handle_key(event) {
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_4()
                    .py_3()
                    .child(div().text_base().text_color(theme.text).child("Settings"))
                    .child(
                        div().text_size(px(theme.small_px())).text_color(theme.text_faint).child("esc"),
                    ),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pb_4()
                    .children(items),
            );

        if let Some(menu) = &self.font_menu {
            let current = self.config.read(cx).font_for(menu.container, FontKind::default());
            root = root.child(font_menu(theme, menu, current, cx));
        }

        root
    }
}

fn section(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_4()
        .pt_4()
        .pb_2()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
        .into_any_element()
}

fn theme_button(
    theme: Theme,
    kind: ThemeKind,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(("theme", kind as usize))
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.cell_px()))
        .when(active, |d| d.bg(theme.accent).text_color(theme.panel_bg))
        .when(!active, |d| d.bg(theme.row_active).text_color(theme.text))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(kind.label())
        .into_any_element()
}

/// The typeface menu for one container: every family, the current one ticked.
fn font_menu(
    theme: Theme,
    menu: &FontMenu,
    current: FontKind,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let container = menu.container;
    let items: Vec<(String, MenuHandler)> = FontKind::ALL
        .iter()
        .map(|kind| {
            let kind = *kind;
            let label = if kind == current {
                format!("✓ {}", kind.label())
            } else {
                format!("   {}", kind.label())
            };
            let handler: MenuHandler = Box::new(cx.listener(move |this, _event, _window, cx| {
                this.font_menu = None;
                this.config.update(cx, |config, cx| config.set_font(container, Some(kind), cx));
            }));
            (label, handler)
        })
        .collect();
    let dismiss = cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
        this.font_menu = None;
        cx.notify();
    });
    context_menu(theme, menu.position, items, dismiss)
}

fn container_row(
    theme: Theme,
    index: usize,
    row: ContainerRow,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let ContainerRow { id, label, default_size, size, font } = row;
    let can_down = size > MIN_FONT_SIZE + 0.01;
    let can_up = size < MAX_FONT_SIZE - 0.01;
    let size_overridden = (size - default_size).abs() > 0.01;
    let font_overridden = font != FontKind::default();

    div()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_1()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_muted)
                .child(label.to_string()),
        )
        .child(step_button(
            theme,
            ("font-down", index),
            "−",
            can_down,
            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                let current = this.config.read(cx).font_size(id, default_size);
                this.config.update(cx, |config, cx| config.set_font_size(id, current - 1.0, cx));
            }),
        ))
        .child(
            div()
                .w(px(48.0))
                .flex_none()
                .text_right()
                .text_size(px(theme.cell_px()))
                .text_color(if size_overridden { theme.accent } else { theme.text })
                .child(format!("{} px", size.round() as i32)),
        )
        .child(step_button(
            theme,
            ("font-up", index),
            "+",
            can_up,
            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                let current = this.config.read(cx).font_size(id, default_size);
                this.config.update(cx, |config, cx| config.set_font_size(id, current + 1.0, cx));
            }),
        ))
        .child(
            div()
                .id(("font-picker", index))
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .font_family(font.family())
                .text_size(px(theme.small_px()))
                .text_color(if font_overridden { theme.accent } else { theme.text })
                .bg(theme.row_active)
                .hover(|d| d.bg(theme.row_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        this.font_menu = Some(FontMenu { position: event.position, container: id });
                        cx.notify();
                    }),
                )
                .child(font.label().to_string())
                .child(div().text_color(theme.text_faint).child("▾")),
        )
        .child(action_button(
            theme,
            ("font-reset", index),
            "Reset",
            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.config.update(cx, |config, cx| {
                    config.reset_font_size(id, cx);
                    config.set_font(id, None, cx);
                });
            }),
        ))
        .into_any_element()
}

fn root_row(theme: Theme, index: usize, root: PathBuf, cx: &mut Context<SettingsView>) -> AnyElement {
    let label = root.display().to_string();
    let removed = root.clone();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .px_4()
        .py_1()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(action_button(
            theme,
            ("remove-root", index),
            "Remove",
            cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                let path = removed.clone();
                this.config.update(cx, |config, cx| {
                    config.remove_root(&path, cx);
                });
            }),
        ))
        .into_any_element()
}

fn step_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: &str,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .w(px(theme.font_size * 1.8))
        .flex_none()
        .flex()
        .justify_center()
        .rounded_md()
        .text_size(px(theme.cell_px()))
        .when(enabled, |d| {
            d.cursor_pointer()
                .bg(theme.row_active)
                .text_color(theme.text)
                .hover(|d| d.bg(theme.row_hover))
                .on_click(on_click)
        })
        .when(!enabled, |d| d.text_color(theme.text_faint))
        .child(label.to_string())
        .into_any_element()
}

fn action_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .flex_none()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.small_px()))
        .text_color(theme.text)
        .bg(theme.row_active)
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(label.to_string())
        .into_any_element()
}

/// A stable list of container ids, for tests.
#[cfg(test)]
fn container_ids() -> Vec<&'static str> {
    containers().into_iter().map(|(id, _, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[test]
    fn every_container_has_a_positive_default() {
        for (id, label, default) in containers() {
            assert!(default > 0.0, "{id} ({label}) needs a positive default");
        }
    }

    #[test]
    fn config_defaults_are_within_bounds() {
        let config = Config::default();
        for (id, _, _) in containers() {
            let size = config.font_size_for(id, 14.0);
            assert!((MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size), "{id} out of range");
        }
    }

    #[test]
    fn container_ids_are_distinct() {
        let ids = container_ids();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "duplicate container id");
    }
}
