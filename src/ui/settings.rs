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
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, Window,
};

use crate::model::config::{
    DynamicBase, FontKind, ThemeKind, WaveformStyle, MAX_FONT_SIZE, MIN_FONT_SIZE,
};
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::lyrics::LyricsView;
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::playback::PlaybackView;
use crate::ui::playlists::PlaylistsView;
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;
use crate::ui::widgets::{empty_hint, panel_header, section_header};

/// An open per-container typeface menu: where it was raised and which container
/// it edits.
struct FontMenu {
    position: Point<Pixels>,
    container: &'static str,
}

pub struct SettingsView {
    config: Entity<ConfigState>,
    themed: Themed,
    path: TextField,
    /// The path box's focus handle — the box takes keys (and shows its caret)
    /// only while it's focused, i.e. once clicked.
    path_focus: FocusHandle,
    /// The open typeface menu, if any.
    font_menu: Option<FontMenu>,
    focus_handle: FocusHandle,
}

impl SettingsView {
    pub fn new(config: Entity<ConfigState>, cx: &mut Context<Self>) -> Self {
        let themed = Themed::new(&config, cx);
        Self {
            config,
            themed,
            path: TextField::default(),
            path_focus: cx.focus_handle(),
            font_menu: None,
            focus_handle: cx.focus_handle(),
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
///
/// The playlist and search tabs are deliberately absent: they draw the
/// library's rows, so they share the browse list's typography and have no
/// separate setting to expose.
fn containers() -> Vec<(&'static str, &'static str, f32)> {
    vec![
        (TabsView::container_id(), "Tabs", TabsView::default_font_size()),
        (BrowseView::container_id(), "Song lists", BrowseView::default_font_size()),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();
        let config = self.config.read(cx);
        let theme_kind = config.theme_kind();
        let waveform = config.waveform();
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
        let theme_buttons = ThemeKind::ALL.iter().map(|kind| {
            let kind = *kind;
            choice_button(
                theme,
                ("theme", kind as usize),
                kind.label(),
                theme_kind == kind,
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.config.update(cx, |config, cx| config.set_theme(kind, cx));
                }),
            )
        });
        items.push(setting_row(
            theme,
            "Theme",
            div().flex().gap_2().children(theme_buttons).into_any_element(),
        ));

        // --- dynamic base (only while Dynamic is selected) ---
        if theme_kind == ThemeKind::Dynamic {
            let base = config.dynamic_base();
            let base_buttons = DynamicBase::ALL.iter().map(|base_kind| {
                let base_kind = *base_kind;
                choice_button(
                    theme,
                    ("dynamic-base", base_kind as usize),
                    base_kind.label(),
                    base == base_kind,
                    cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.config
                            .update(cx, |config, cx| config.set_dynamic_base(base_kind, cx));
                    }),
                )
            });
            items.push(setting_row(
                theme,
                "Cover base",
                div().flex().gap_2().children(base_buttons).into_any_element(),
            ));
        }

        // --- waveform style ---
        let waveform_buttons = WaveformStyle::ALL.iter().map(|style| {
            let style = *style;
            choice_button(
                theme,
                ("waveform", style as usize),
                style.label(),
                waveform == style,
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.config.update(cx, |config, cx| config.set_waveform(style, cx));
                }),
            )
        });
        items.push(setting_row(
            theme,
            "Waveform",
            div().flex().gap_2().children(waveform_buttons).into_any_element(),
        ));

        // --- visualizer band ---
        let visualizer_on = config.visualizer();
        let visualizer_buttons = [true, false].iter().map(|&on| {
            choice_button(
                theme,
                ("visualizer", on as usize),
                if on { "On" } else { "Off" },
                visualizer_on == on,
                cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.config.update(cx, |config, cx| config.set_visualizer(on, cx));
                }),
            )
        });
        items.push(setting_row(
            theme,
            "Visualizer",
            div().flex().gap_2().children(visualizer_buttons).into_any_element(),
        ));

        // --- typeface + font size, per container, as a table ---
        items.push(section_header(theme, "Font"));
        for (index, row) in rows.into_iter().enumerate() {
            items.push(font_table_row(theme, index, row, cx));
        }

        // --- synced paths ---
        items.push(section_header(theme, "Synced paths"));
        if roots.is_empty() {
            items.push(empty_hint(theme, "No paths yet — add a music directory below.", false));
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
                .child(div().flex_1().min_w_0().child(
                    self.path.render(theme, "/path/to/music", &self.path_focus, window),
                ))
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.font_menu.is_some() {
                    this.font_menu = None;
                    cx.notify();
                    return;
                }
                // The path box takes keys only while it's focused; elsewhere
                // `enter` does nothing and typing bubbles.
                let path_focused = this.path_focus.is_focused(window);
                if path_focused && event.keystroke.key == "enter" {
                    this.add_path(cx);
                    return;
                }
                if path_focused && this.path.handle_routed(event, cx) {
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel_bg)
            .font_family(theme.font)
            .child(panel_header(
                theme,
                "Settings",
                theme.cell_px() + 2.0,
                None,
                None,
                Some(("esc", theme.text_faint)),
                None,
            ))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pt_2()
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

/// One `{label}: {control}` row — the label sits in a fixed column so the
/// controls line up down the panel.
fn setting_row(theme: Theme, label: &str, control: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_1()
        .child(
            div()
                .w(px(theme.label_col()))
                .flex_none()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text_muted)
                .child(format!("{label}:")),
        )
        .child(control)
        .into_any_element()
}

fn choice_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: &str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.cell_px()))
        .when(active, |d| d.bg(theme.accent).text_color(theme.panel_bg))
        .when(!active, |d| d.bg(theme.row_active).text_color(theme.text))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(label.to_string())
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

/// One row of the font table: the container's name on the left, its size
/// stepper and typeface picker right-aligned on the right. Striped like the
/// album rows — [`Theme::row_bg`] alternates lighter and darker per row.
fn font_table_row(
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
        .bg(theme.row_bg(index))
        .hover(|d| d.bg(theme.row_hover))
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
        // Fixed, right-aligned columns: [`Theme::value_col`] fits `32 px`,
        // `9em` the longest family label — anything wider opens the gaps
        // between the cells.
        .child(
            div()
                .w(px(theme.value_col()))
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
                .w(px(theme.font_size * 9.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
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
                .child(div().min_w_0().truncate().child(font.label().to_string()))
                .child(div().flex_none().text_color(theme.text_faint).child("▾")),
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
        .bg(theme.row_bg(index))
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
