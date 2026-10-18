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

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    canvas, div, point, prelude::*, px, AnyElement, App, ClickEvent, Context, Entity,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point,
    Render, Rgba, Window,
};

use crate::model::config::{
    DynamicBase, FontKind, ThemeKind, WaveformStyle, MAX_FONT_SIZE, MIN_FONT_SIZE,
};
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::lyrics::LyricsView;
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::playback::{svg_icon, PlaybackView};
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

/// One option row's segmented control. These are the only rows with a
/// fixed-size choice, so they share one renderer and one keyboard model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Segmented {
    Theme,
    DynamicBase,
    Waveform,
    Visualizer,
}

impl Segmented {
    /// All groups, in render order-ish. Used to create their focus handles.
    const ALL: [Segmented; 4] =
        [Segmented::Theme, Segmented::DynamicBase, Segmented::Waveform, Segmented::Visualizer];

    fn key(self) -> &'static str {
        match self {
            Segmented::Theme => "seg-theme",
            Segmented::DynamicBase => "seg-dynamic-base",
            Segmented::Waveform => "seg-waveform",
            Segmented::Visualizer => "seg-visualizer",
        }
    }

    fn labels(self) -> &'static [&'static str] {
        match self {
            Segmented::Theme => &["Dark", "Light", "Dynamic"],
            Segmented::DynamicBase => &["Black", "White"],
            Segmented::Waveform => &["Bars", "Line"],
            Segmented::Visualizer => &["On", "Off"],
        }
    }

    fn real_len(self) -> usize {
        self.labels().len()
    }
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
    /// One focus handle per segmented option group, so arrow keys move within
    /// the focused bar.
    segment_focus: HashMap<Segmented, FocusHandle>,
    /// The segmented option the pointer is over, painted as the row hover.
    hovered_segment: Option<(Segmented, usize)>,
}

impl SettingsView {
    pub fn new(config: Entity<ConfigState>, cx: &mut Context<Self>) -> Self {
        let themed = Themed::new(&config, cx);
        let segment_focus = Segmented::ALL
            .into_iter()
            .map(|segment| (segment, cx.focus_handle()))
            .collect();
        Self {
            config,
            themed,
            path: TextField::default(),
            path_focus: cx.focus_handle(),
            font_menu: None,
            focus_handle: cx.focus_handle(),
            segment_focus,
            hovered_segment: None,
        }
    }

    /// The focus handle the tab container hands focus to when it opens settings.
    pub fn focus_handle_for_window(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// The selected segment within `segment`'s group. Read from config in key
    /// handlers so arrows start from whatever is current.
    fn segment_active(&self, segment: Segmented, cx: &App) -> usize {
        let config = self.config.read(cx);
        match segment {
            Segmented::Theme => ThemeKind::ALL
                .iter()
                .position(|kind| *kind == config.theme_kind())
                .unwrap_or(0),
            Segmented::DynamicBase => DynamicBase::ALL
                .iter()
                .position(|base| *base == config.dynamic_base())
                .unwrap_or(0),
            Segmented::Waveform => WaveformStyle::ALL
                .iter()
                .position(|style| *style == config.waveform())
                .unwrap_or(0),
            Segmented::Visualizer => usize::from(!config.visualizer()),
        }
    }

    /// Pick segment `index` of `segment`'s group and persist it.
    fn select_segment(&mut self, segment: Segmented, index: usize, cx: &mut Context<Self>) {
        if index >= segment.real_len() {
            return;
        }
        match segment {
            Segmented::Theme => {
                let kind = ThemeKind::ALL[index];
                self.config.update(cx, |config, cx| config.set_theme(kind, cx));
            }
            Segmented::DynamicBase => {
                let base = DynamicBase::ALL[index];
                self.config.update(cx, |config, cx| config.set_dynamic_base(base, cx));
            }
            Segmented::Waveform => {
                let style = WaveformStyle::ALL[index];
                self.config.update(cx, |config, cx| config.set_waveform(style, cx));
            }
            Segmented::Visualizer => {
                let on = index == 0;
                self.config.update(cx, |config, cx| config.set_visualizer(on, cx));
            }
        }
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
        let dynamic_base = config.dynamic_base();
        let visualizer_on = config.visualizer();
        let theme_active = ThemeKind::ALL.iter().position(|kind| *kind == theme_kind).unwrap_or(0);
        let base_active = DynamicBase::ALL
            .iter()
            .position(|base| *base == dynamic_base)
            .unwrap_or(0);
        let waveform_active =
            WaveformStyle::ALL.iter().position(|style| *style == waveform).unwrap_or(0);
        let visualizer_active = usize::from(!visualizer_on);
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
        let theme_focus = self.segment_focus[&Segmented::Theme].clone();
        items.push(setting_row(
            theme,
            "Theme",
            segmented_control(
                theme,
                Segmented::Theme,
                theme_active,
                self.hovered_segment,
                &theme_focus,
                cx,
            ),
        ));

        // --- dynamic base (only while Dynamic is selected) ---
        if theme_kind == ThemeKind::Dynamic {
            let base_focus = self.segment_focus[&Segmented::DynamicBase].clone();
            items.push(setting_row(
                theme,
                "Cover base",
                segmented_control(
                    theme,
                    Segmented::DynamicBase,
                    base_active,
                    self.hovered_segment,
                    &base_focus,
                    cx,
                ),
            ));
        }

        // --- waveform style ---
        let waveform_focus = self.segment_focus[&Segmented::Waveform].clone();
        items.push(setting_row(
            theme,
            "Waveform",
            segmented_control(
                theme,
                Segmented::Waveform,
                waveform_active,
                self.hovered_segment,
                &waveform_focus,
                cx,
            ),
        ));

        // --- visualizer band ---
        let visualizer_focus = self.segment_focus[&Segmented::Visualizer].clone();
        items.push(setting_row(
            theme,
            "Visualizer",
            segmented_control(
                theme,
                Segmented::Visualizer,
                visualizer_active,
                self.hovered_segment,
                &visualizer_focus,
                cx,
            ),
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

/// One `{label} {control}` row — the label sits in a fixed column so the
/// segmented bars and other controls line up down the panel.
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
                .child(label.to_string()),
        )
        .child(control)
        .into_any_element()
}

/// A colour with its alpha replaced — for the empty segment's faint stripe
/// texture and nothing else.
fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

/// One option row's segmented bar: three equal segments in one outline, with
/// slanted dividers. Groups with two choices leave the third segment empty
/// and textured. The canvas paints the fills, dividers and texture; transparent
/// foreground cells carry the labels and interactions on top.
fn segmented_control(
    theme: Theme,
    segment: Segmented,
    active: usize,
    hovered: Option<(Segmented, usize)>,
    focus: &FocusHandle,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let labels = segment.labels().to_vec();
    let real_len = labels.len();
    let hovered = match hovered {
        Some((group, index)) if group == segment => Some(index),
        _ => None,
    };
    let height = theme.font_size * 2.0;
    let width = theme.font_size * 18.0;
    let focus_for_click = focus.clone();

    let paint_active = active;
    let paint_hovered = hovered;
    let paint_theme = theme;
    let painter = canvas(
        |bounds, _window, _cx| bounds,
        move |bounds, _prepaint, window, _cx| {
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let seg_w = w / 3.0;
            // The top edge of a "/" divider sits right of its bottom edge.
            let slant = (h * 0.30).min(seg_w * 0.28) / 2.0;
            let point_at = |x: f32, y: f32| point(px(x0 + x), px(y0 + y));

            for index in 0..3 {
                let left_top = if index == 0 { 0.0 } else { index as f32 * seg_w + slant };
                let left_bottom = if index == 0 { 0.0 } else { index as f32 * seg_w - slant };
                let right_top =
                    if index == 2 { w } else { (index + 1) as f32 * seg_w + slant };
                let right_bottom =
                    if index == 2 { w } else { (index + 1) as f32 * seg_w - slant };
                let fill_color = if index == paint_active && index < real_len {
                    paint_theme.accent
                } else if paint_hovered == Some(index) && index < real_len {
                    paint_theme.row_hover
                } else {
                    paint_theme.panel_bg
                };
                let mut builder = PathBuilder::fill();
                builder.add_polygon(
                    &[
                        point_at(left_top, 0.0),
                        point_at(right_top, 0.0),
                        point_at(right_bottom, h),
                        point_at(left_bottom, h),
                    ],
                    true,
                );
                if let Ok(path) = builder.build() {
                    window.paint_path(path, fill_color);
                }
            }

            // The empty third segment reads as texture: faint slanted stripes
            // in the muted text colour, clipped to the bar.
            if real_len < 3 {
                let stripe = with_alpha(paint_theme.text_faint, 0.22);
                let mut x = 2.0 * seg_w - slant;
                while x < w {
                    let mut builder = PathBuilder::stroke(px(1.0));
                    builder.move_to(point_at(x, h));
                    builder.line_to(point_at((x + 2.0 * slant).min(w), 0.0));
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, stripe);
                    }
                    x += (h * 0.42).max(5.0);
                }
            }

            // Dividers share the outline's stroke colour so the shape reads as
            // one continuous border.
            let stroke = paint_theme.border;
            for index in 1..3 {
                let mut builder = PathBuilder::stroke(px(1.0));
                builder.move_to(point_at(index as f32 * seg_w + slant, 0.0));
                builder.line_to(point_at(index as f32 * seg_w - slant, h));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, stroke);
                }
            }
        },
    )
    .absolute()
    .inset(px(1.0));

    let cells = (0..3).map(|index| {
        if index >= real_len {
            return div().flex_1().h_full().into_any_element();
        }
        let label = labels[index];
        let is_active = index == active;
        let focus = focus_for_click.clone();
        div()
            .id((segment.key(), index))
            .flex_1()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_size(px(theme.small_px()))
            .text_color(if is_active { theme.panel_bg } else { theme.text })
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                let current = Some((segment, index));
                let next = if *hovered {
                    current
                } else if this.hovered_segment == current {
                    None
                } else {
                    // A leave from a sibling that the new enter already
                    // superseded must not clear the hover.
                    return;
                };
                if this.hovered_segment != next {
                    this.hovered_segment = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                window.focus(&focus);
                this.select_segment(segment, index, cx);
            }))
            .child(label.to_string())
            .into_any_element()
    });

    div()
        .id(segment.key())
        .relative()
        .w(px(width))
        .h(px(height))
        .flex_none()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .tab_index(0)
        .track_focus(focus)
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
            let step = match event.keystroke.key.as_str() {
                "left" => -1,
                "right" => 1,
                _ => return,
            };
            let current = this.segment_active(segment, cx);
            let next = (current as isize + step).rem_euclid(real_len as isize) as usize;
            this.select_segment(segment, next, cx);
            cx.stop_propagation();
        }))
        .child(painter)
        .child(div().absolute().inset(px(1.0)).flex().children(cells))
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

/// One row of the font table: name on the left, then a bare stepper, a fixed
/// reset slot, and a flat family dropdown, all right-aligned and the same width
/// on every row. No zebra — rows light up on hover like the rail.
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
    let modified = size_overridden || font_overridden;

    div()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_1()
        .hover(|d| d.bg(theme.row_hover))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(theme.text)
                .child(label.to_string()),
        )
        // Bare stepper: - value +, with a fixed-width value field so the
        // buttons don't jitter between "9 px" and "14 px".
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_none()
                .child(step_button(
                    theme,
                    ("font-down", index),
                    "−",
                    can_down,
                    cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        let current = this.config.read(cx).font_size(id, default_size);
                        this.config
                            .update(cx, |config, cx| config.set_font_size(id, current - 1.0, cx));
                    }),
                ))
                .child(
                    div()
                        .w(px(theme.font_size * 3.4))
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
                        this.config
                            .update(cx, |config, cx| config.set_font_size(id, current + 1.0, cx));
                    }),
                )),
        )
        // Reset slot: always reserves its width; the undo icon only appears
        // while the row differs from its defaults.
        .child(
            div()
                .w(px(theme.font_size * 2.2))
                .flex_none()
                .flex()
                .justify_center()
                .when(modified, |d| {
                    d.child(icon_button(
                        theme,
                        ("font-reset", index),
                        "assets/undo.svg",
                        "Reset typeface and size",
                        cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            this.config.update(cx, |config, cx| {
                                config.reset_font_size(id, cx);
                                config.set_font(id, None, cx);
                            });
                        }),
                    ))
                }),
        )
        .child(
            div()
                .id(("font-picker", index))
                .w(px(theme.font_size * 9.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .font_family(font.family())
                .text_size(px(theme.small_px()))
                .text_color(if font_overridden { theme.accent } else { theme.text })
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

/// A bare glyph stepper button: no filled surface, just the mark and the rail's
/// hover wash.
fn step_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: &str,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .w(px(theme.font_size * 1.4))
        .flex_none()
        .flex()
        .justify_center()
        .rounded_md()
        .text_size(px(theme.cell_px()))
        .when(enabled, |d| {
            d.cursor_pointer()
                .text_color(theme.text)
                .hover(|d| d.bg(theme.row_hover))
                .on_click(on_click)
        })
        .when(!enabled, |d| d.text_color(theme.text_faint))
        .child(label.to_string())
        .into_any_element()
}

/// A flat icon button — used for the font rows' reset affordance.
fn icon_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    path: &'static str,
    _label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .w(px(theme.font_size * 1.6))
        .h(px(theme.font_size * 1.6))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(svg_icon(
            theme.small_px(),
            theme.small_px(),
            path,
            theme.text_muted,
        ))
        .into_any_element()
}

/// Flat action text (Add, Remove) — no filled pill, same hover as rail rows.
fn action_button(
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.small_px()))
        .text_color(theme.text)
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
    fn every_segmented_group_fits_the_three_segment_bar() {
        for group in Segmented::ALL {
            assert!(
                (2..=3).contains(&group.real_len()),
                "{} has {} options",
                group.key(),
                group.real_len()
            );
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
