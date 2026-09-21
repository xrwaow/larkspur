//! The playlist list — the left rail.
//!
//! Shows custom playlists, autogen album playlists, and the artists derived
//! from them. An artist expands into its discography (the album playlists it
//! appears on), since an artist is a *list* of playlists, not one playlist.
//!
//! Custom playlists are edited here: right-clicking one opens a menu to rename
//! or delete it, and renaming happens inline. The settings button in the header
//! opens the settings overlay in the center.

use gpui::{
    div, prelude::*, px, AnyElement, App, ClickEvent, Context, ElementId, Entity, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, Subscription, Window,
};

use crate::model::PlaylistId;
use crate::ui::config_state::ConfigState;
use crate::ui::container::Container;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::{context_menu, MenuHandler};
use crate::ui::tabs::TabsView;
use crate::ui::text_field::TextField;
use crate::ui::theme::Theme;

/// An open right-click menu on a custom playlist.
struct PlaylistMenu {
    position: Point<Pixels>,
    playlist: PlaylistId,
}

pub struct PlaylistsView {
    library: Entity<LibraryState>,
    /// Clicking a playlist opens it as a tab in the center — the rail is how
    /// tabs get created, so it holds the tab container.
    tabs: Entity<TabsView>,
    theme: Theme,
    expanded_artist: Option<String>,
    /// The playlist being renamed inline, and its edit buffer.
    renaming: Option<(PlaylistId, TextField)>,
    /// The open right-click menu, if any.
    menu: Option<PlaylistMenu>,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_config: Subscription,
}

impl PlaylistsView {
    pub fn new(
        library: Entity<LibraryState>,
        tabs: Entity<TabsView>,
        config: Entity<ConfigState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&library, |_this, _state, cx| cx.notify());
        let observe_config = cx.observe(&config, |this, config, cx| {
            this.theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
            cx.notify();
        });
        let theme = config.read(cx).theme_for(Self::container_id(), Self::default_font_size());
        Self {
            library,
            tabs,
            theme,
            expanded_artist: None,
            renaming: None,
            menu: None,
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_config: observe_config,
        }
    }

    /// Commit the inline rename, if one is open.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((id, field)) = self.renaming.take() {
            let title = field.value.trim().to_string();
            if !title.is_empty() {
                self.library.update(cx, |state, cx| state.rename_playlist(id, title, cx));
            }
        }
        cx.notify();
    }

    /// Cancel the inline rename.
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }
}

impl Container for PlaylistsView {
    fn container_id() -> &'static str {
        "playlists"
    }
}

/// A playlist row, snapshotted out of the library so the borrow is dropped
/// before elements are built.
struct Row {
    id: PlaylistId,
    title: String,
    count: usize,
}

impl Render for PlaylistsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        let (custom, albums, artists, selected, scanning, discography) = {
            let state = self.library.read(cx);
            let lib = state.library();

            let custom: Vec<Row> = lib
                .custom_playlists()
                .iter()
                .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                .collect();
            let albums: Vec<Row> = lib
                .playlists()
                .iter()
                .filter(|p| !p.is_custom())
                .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                .collect();
            let artists = lib.artists();

            let discography: Vec<Row> = self
                .expanded_artist
                .as_ref()
                .map(|artist| {
                    lib.discography(artist)
                        .into_iter()
                        .filter_map(|id| lib.playlist(id))
                        .map(|p| Row { id: p.id, title: p.meta.title.clone(), count: p.len() })
                        .collect()
                })
                .unwrap_or_default();

            (custom, albums, artists, state.selected(), state.is_scanning(), discography)
        };

        let mut items: Vec<AnyElement> = Vec::new();

        items.push(section_header(theme, "Playlists"));
        items.push(nav_row(
            theme,
            ("new-playlist", 0u64),
            "+  New Playlist".to_string(),
            None,
            false,
            false,
            cx.listener(|this, _event, window, cx| {
                let id = this.library.update(cx, |state, cx| state.new_playlist(cx));
                this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
            }),
        ));
        if custom.is_empty() {
            items.push(empty_hint(theme, "No custom playlists yet"));
        }
        for row in custom {
            let id = row.id;

            // The row being renamed shows an editable field instead of a label.
            if let Some((renaming, field)) = &self.renaming {
                if *renaming == id {
                    items.push(rename_row(theme, field));
                    continue;
                }
            }

            let row_el = nav_row(
                theme,
                ("custom", id.0),
                row.title,
                Some(row.count),
                selected == Some(id),
                false,
                cx.listener(move |this, _event, window, cx| {
                    this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                }),
            );
            items.push(
                div()
                    .id(("custom-menu-target", id.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.menu = Some(PlaylistMenu { position: event.position, playlist: id });
                            cx.notify();
                        }),
                    )
                    .child(row_el)
                    .into_any_element(),
            );
        }

        items.push(section_header(theme, "Albums"));
        for row in albums {
            let id = row.id;
            items.push(nav_row(
                theme,
                ("album", id.0),
                row.title,
                Some(row.count),
                selected == Some(id),
                false,
                cx.listener(move |this, _event, window, cx| {
                    this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                }),
            ));
        }

        items.push(section_header(theme, "Artists"));
        for (index, artist) in artists.into_iter().enumerate() {
            let expanded = self.expanded_artist.as_deref() == Some(artist.as_str());
            let label = if expanded { format!("▾  {artist}") } else { format!("▸  {artist}") };
            let toggled = artist.clone();
            items.push(nav_row(
                theme,
                ("artist", index as u64),
                label,
                None,
                false,
                false,
                cx.listener(move |this, _event, _window, cx| {
                    this.expanded_artist =
                        if this.expanded_artist.as_deref() == Some(toggled.as_str()) {
                            None
                        } else {
                            Some(toggled.clone())
                        };
                    cx.notify();
                }),
            ));

            if expanded {
                for row in &discography {
                    let id = row.id;
                    items.push(nav_row(
                        theme,
                        ("discography", id.0),
                        row.title.clone(),
                        Some(row.count),
                        selected == Some(id),
                        true,
                        cx.listener(move |this, _event, window, cx| {
                            this.tabs.update(cx, |tabs, cx| tabs.open_playlist(id, window, cx));
                        }),
                    ));
                }
            }
        }

        let mut root = div()
            .relative()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.renaming.is_some() {
                    match event.keystroke.key.as_str() {
                        "enter" => {
                            this.commit_rename(cx);
                            this.tabs.update(cx, |tabs, cx| tabs.refocus(window, cx));
                            return;
                        }
                        "escape" => {
                            this.cancel_rename(cx);
                            this.tabs.update(cx, |tabs, cx| tabs.refocus(window, cx));
                            return;
                        }
                        _ => {}
                    }
                    if let Some((_, field)) = &mut this.renaming {
                        if field.handle_key(event) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }
                    return;
                }
                if event.keystroke.key == "escape" && this.menu.is_some() {
                    this.menu = None;
                    cx.notify();
                }
            }))
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(settings_button(theme, cx.listener(|this, _event, window, cx| {
                                this.tabs.update(cx, |tabs, cx| tabs.open_settings(window, cx));
                            })))
                            .child(
                                div()
                                    .text_size(px(theme.cell_px()))
                                    .text_color(theme.text)
                                    .child("Library"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme.small_px()))
                            .text_color(theme.text_muted)
                            .child(if scanning { "scanning…" } else { "" }),
                    ),
            )
            .child(
                div()
                    .id("playlists-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .pb_2()
                    .children(items),
            );

        if let Some(menu) = &self.menu {
            let position = menu.position;
            let playlist = menu.playlist;
            let title = self
                .library
                .read(cx)
                .library()
                .playlist(playlist)
                .map(|p| p.meta.title.clone())
                .unwrap_or_default();

            let rename: MenuHandler = Box::new(cx.listener(move |this, _event, window, cx| {
                this.menu = None;
                this.renaming = Some((playlist, TextField::new(title.clone())));
                window.focus(&this.focus_handle);
                cx.notify();
            }));
            let delete: MenuHandler = Box::new(cx.listener(move |this, _event, _window, cx| {
                this.menu = None;
                this.library.update(cx, |state, cx| state.delete_playlist(playlist, cx));
            }));
            let dismiss = cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                this.menu = None;
                cx.notify();
            });
            root = root.child(context_menu(
                theme,
                position,
                vec![("Rename".to_string(), rename), ("Delete".to_string(), delete)],
                dismiss,
            ));
        }

        root
    }
}

fn section_header(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_2()
        .pt_3()
        .pb_1()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
        .into_any_element()
}

fn empty_hint(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .text_size(px(theme.small_px()))
        .text_color(theme.text_faint)
        .child(label.to_string())
        .into_any_element()
}

fn settings_button(
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id("open-settings")
        .flex_none()
        .px_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(theme.cell_px()))
        .text_color(theme.text_muted)
        .hover(|d| d.bg(theme.row_hover).text_color(theme.text))
        .on_click(on_click)
        .child("⚙")
        .into_any_element()
}

/// The editable row shown while renaming a playlist.
fn rename_row(theme: Theme, field: &TextField) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .child(field.render(theme, "Playlist name"))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn nav_row(
    theme: Theme,
    id: impl Into<ElementId>,
    label: String,
    count: Option<usize>,
    active: bool,
    indent: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .when(indent, |d| d.pl_5())
        .when(active, |d| d.bg(theme.row_active))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.cell_px()))
                .text_color(if active { theme.text } else { theme.text_muted })
                .child(label),
        )
        .child(
            div()
                .text_size(px(theme.small_px()))
                .text_color(theme.text_faint)
                .child(count.map(|c| c.to_string()).unwrap_or_default()),
        )
        .into_any_element()
}
