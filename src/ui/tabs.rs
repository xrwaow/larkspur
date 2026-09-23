//! The center tab container — a strip of tabs over the active container.
//!
//! One container per tab: browse, plus one per playlist the user has opened
//! from the left rail. The strip sits on top and clicking a tab switches which
//! container is mounted; only the active one is in the element tree, so only it
//! receives keys.
//!
//! Search and settings are deliberately *not* tabs. `ctrl+shift+f` swaps the
//! search panel in over whatever tab is active and `esc` (or the same chord)
//! puts it back; the rail's settings button swaps in the settings panel the
//! same way. Both are detours from what you were reading rather than another
//! tab to manage.
//!
//! Focus is re-pointed at the newly active container on every switch, which is
//! what keeps keyboard input (row navigation, typing in the search box) going
//! to what the user is looking at. Keys the active container doesn't act on
//! bubble up here, which is where tab switching, closing, and the transport
//! keys live.

use std::path::PathBuf;

use gpui::{
    div, prelude::*, px, AnyElement, AnyView, App, ClickEvent, Context, Entity, FocusHandle,
    KeyDownEvent, MouseDownEvent, Render, Subscription, Window,
};

use crate::model::{InputAction, PlaylistId, TabId};
use crate::ui::animation::Animator;
use crate::ui::browse::BrowseView;
use crate::ui::config_state::{ConfigState, Themed};
use crate::ui::container::Container;
use crate::ui::cover_store::CoverStore;
use crate::ui::input::action_for_key;
use crate::ui::library_state::LibraryState;
use crate::ui::menu::{context_menu, song_menu_items, MenuHandler, SongMenuRequest};
use crate::ui::playlist::PlaylistView;
use crate::ui::search::SearchView;
use crate::ui::settings::SettingsView;
use crate::ui::playback_state::PlaybackState;
use crate::ui::theme::Theme;

/// One playlist tab: what it shows, where its focus goes, and — for a folder
/// view — the temporary playlists it was built from (deleted with the tab).
struct Tab {
    id: TabId,
    view: AnyView,
    focus: FocusHandle,
    temp: Vec<PlaylistId>,
}

pub struct TabsView {
    library: Entity<LibraryState>,
    playback: Entity<PlaybackState>,
    covers: Entity<CoverStore>,
    config: Entity<ConfigState>,
    animator: Entity<Animator>,
    themed: Themed,
    browse: Entity<BrowseView>,
    search: Entity<SearchView>,
    settings: Entity<SettingsView>,
    /// The non-browse tabs (playlists and artist scopes), in the order they
    /// were opened.
    tabs: Vec<Tab>,
    active: TabId,
    /// Whether the search panel is swapped in over the active tab.
    search_open: bool,
    /// Whether the settings panel is swapped in over the active tab.
    settings_open: bool,
    /// The open song context menu, if any. Rendered here, on top of every
    /// container, since rows anywhere can raise it.
    menu: Option<SongMenuRequest>,
    /// The window title last set, so the titlebar is only touched on change.
    last_title: String,
    /// Set when the library notified, so the request poll only runs when
    /// something may actually be pending rather than every frame.
    library_dirty: bool,
    focus_handle: FocusHandle,
    _observe: Subscription,
    _observe_playback: Subscription,
}

impl TabsView {
    pub fn new(
        library: Entity<LibraryState>,
        playback: Entity<PlaybackState>,
        covers: Entity<CoverStore>,
        config: Entity<ConfigState>,
        animator: Entity<Animator>,
        cx: &mut Context<Self>,
    ) -> Self {
        let browse = cx.new(|cx| {
            BrowseView::new(
                library.clone(),
                playback.clone(),
                covers.clone(),
                config.clone(),
                animator.clone(),
                cx,
            )
        });
        let search = cx.new(|cx| {
            SearchView::new(
                library.clone(),
                playback.clone(),
                covers.clone(),
                config.clone(),
                animator.clone(),
                cx,
            )
        });
        let settings = cx.new(|cx| SettingsView::new(config.clone(), cx));
        let observe = cx.observe(&library, |this, _state, cx| {
            this.library_dirty = true;
            cx.notify();
        });
        let observe_playback = cx.observe(&playback, |_this, _state, cx| cx.notify());
        let themed = Themed::new(&config, cx);
        Self {
            library,
            playback,
            covers,
            config,
            animator,
            themed,
            browse,
            search,
            settings,
            tabs: Vec::new(),
            active: TabId::Browse,
            search_open: false,
            settings_open: false,
            menu: None,
            last_title: String::new(),
            library_dirty: false,
            focus_handle: cx.focus_handle(),
            _observe: observe,
            _observe_playback: observe_playback,
        }
    }

    /// The focus handle of whatever is on top — the settings or search panel if
    /// one is up, otherwise the active tab.
    pub fn focus_handle_for_window(&self, cx: &App) -> Option<FocusHandle> {
        if self.settings_open {
            return Some(self.settings.read(cx).focus_handle_for_window());
        }
        if self.search_open {
            return Some(self.search.read(cx).focus_handle_for_window());
        }
        self.focus_of(self.active.clone(), cx)
    }

    /// Open (or focus) a playlist's tab.
    pub fn open_playlist(&mut self, id: PlaylistId, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = TabId::Playlist(id);
        if !self.tabs.iter().any(|tab| tab.id == tab_id) {
            let view = cx.new(|cx| {
                PlaylistView::new(
                    self.library.clone(),
                    self.playback.clone(),
                    self.covers.clone(),
                    self.config.clone(),
                    self.animator.clone(),
                    id,
                    cx,
                )
            });
            let focus = view.read(cx).focus_handle_for_window();
            self.tabs.push(Tab { id: tab_id.clone(), view: view.into(), focus, temp: Vec::new() });
        }
        self.activate(tab_id, window, cx);
    }

    /// Open (or focus) an artist's tab — the library scoped to their
    /// discography. A separate tab rather than a filter on the library, so the
    /// library keeps its own place in the strip.
    pub fn open_artist(&mut self, artist: String, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = TabId::Artist(artist.clone());
        if !self.tabs.iter().any(|tab| tab.id == tab_id) {
            let view = cx.new(|cx| {
                let mut browse = BrowseView::new(
                    self.library.clone(),
                    self.playback.clone(),
                    self.covers.clone(),
                    self.config.clone(),
                    self.animator.clone(),
                    cx,
                );
                browse.set_artist_filter(Some(artist.clone()), cx);
                browse
            });
            let focus = view.read(cx).focus_handle_for_window();
            self.tabs.push(Tab { id: tab_id.clone(), view: view.into(), focus, temp: Vec::new() });
        }
        self.activate(tab_id, window, cx);
    }

    /// Open (or focus) a folder's tab: the temporary playlists "Play folder"
    /// created, shown as one grouped view. Identified by path, so re-playing
    /// the same folder focuses the tab that's already open.
    pub fn open_folder(
        &mut self,
        path: PathBuf,
        playlists: Vec<PlaylistId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = TabId::Folder(path.clone());
        if !self.tabs.iter().any(|tab| tab.id == tab_id) {
            let title = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            let view = cx.new(|cx| {
                let mut browse = BrowseView::new(
                    self.library.clone(),
                    self.playback.clone(),
                    self.covers.clone(),
                    self.config.clone(),
                    self.animator.clone(),
                    cx,
                );
                browse.set_playlist_scope(title, playlists.clone(), cx);
                browse
            });
            let focus = view.read(cx).focus_handle_for_window();
            self.tabs.push(Tab { id: tab_id.clone(), view: view.into(), focus, temp: playlists });
        }
        self.activate(tab_id, window, cx);
    }

    /// Close a non-browse tab, activating the neighbour that takes its place.
    /// A temporary playlist lives exactly as long as its tab: closing one
    /// deletes the playlist(s) it was showing.
    pub fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else { return };
        let tab = self.tabs.remove(index);
        let mut temps = tab.temp;
        if let TabId::Playlist(playlist) = &id {
            if self
                .library
                .read(cx)
                .library()
                .playlist(*playlist)
                .is_some_and(|p| p.is_temporary())
            {
                temps.push(*playlist);
            }
        }
        // One batched delete: a folder tab can carry hundreds of temporary
        // playlists, and deleting per-playlist meant a full-library cache
        // write each.
        if !temps.is_empty() {
            self.library.update(cx, |state, cx| state.delete_playlists(temps, cx));
        }
        if self.active == id && !self.search_open && !self.settings_open {
            let fallback = self
                .tabs
                .get(index.saturating_sub(1))
                .map(|tab| tab.id.clone())
                .unwrap_or(TabId::Browse);
            self.activate(fallback, window, cx);
        } else {
            cx.notify();
        }
    }

    /// Switch to a tab and hand it keyboard focus.
    fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let TabId::Playlist(playlist) = &id {
            // The rail's highlight follows the active tab.
            self.library.update(cx, |state, cx| state.select(*playlist, cx));
        }
        self.active = id;
        self.search_open = false;
        self.settings_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_of(&self, id: TabId, cx: &App) -> Option<FocusHandle> {
        match id {
            TabId::Browse => Some(self.browse.read(cx).focus_handle_for_window()),
            _ => self.tabs.iter().find(|tab| tab.id == id).map(|tab| tab.focus.clone()),
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &App) {
        if let Some(focus) = self.focus_of(self.active.clone(), cx) {
            window.focus(&focus);
        }
    }

    /// Re-point focus at the active container. Used after an inline edit in
    /// another panel (the rail's rename) hands focus away.
    pub fn refocus(&self, window: &mut Window, cx: &App) {
        if self.settings_open {
            window.focus(&self.settings.read(cx).focus_handle_for_window());
        } else if self.search_open {
            window.focus(&self.search.read(cx).focus_handle_for_window());
        } else {
            self.focus_active(window, cx);
        }
    }

    /// Swap the search panel in over the active tab.
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = true;
        self.settings_open = false;
        let focus = self.search.read(cx).focus_handle_for_window();
        window.focus(&focus);
        cx.notify();
    }

    /// Put the active tab back.
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Swap the settings panel in over the active tab.
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.search_open = false;
        let focus = self.settings.read(cx).focus_handle_for_window();
        window.focus(&focus);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Every tab, in strip order.
    fn tab_ids(&self) -> Vec<TabId> {
        let mut ids = vec![TabId::Browse];
        ids.extend(self.tabs.iter().map(|tab| tab.id.clone()));
        ids
    }

    fn step_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.tab_ids();
        let Some(index) = ids.iter().position(|id| *id == self.active) else { return };
        let next = if forward {
            (index + 1) % ids.len()
        } else {
            (index + ids.len() - 1) % ids.len()
        };
        self.activate(ids[next].clone(), window, cx);
    }

    /// `ctrl+w`: dismiss an overlay if one is up, otherwise close the active tab.
    fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            return self.close_settings(window, cx);
        }
        if self.search_open {
            return self.close_search(window, cx);
        }
        if !matches!(self.active, TabId::Browse) {
            let id = self.active.clone();
            self.close_tab(id, window, cx);
        }
    }

    fn dispatch(&mut self, action: InputAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            InputAction::NextTab => self.step_tab(true, window, cx),
            InputAction::PrevTab => self.step_tab(false, window, cx),
            InputAction::ToggleSearch => {
                if self.search_open {
                    self.close_search(window, cx);
                } else {
                    self.open_search(window, cx);
                }
            }
            InputAction::CloseOverlay => {
                if self.settings_open {
                    self.close_settings(window, cx);
                } else if self.search_open {
                    self.close_search(window, cx);
                }
            }
            InputAction::CloseTab => self.close_active(window, cx),
            // Transport keys are global: the center container forwards them, so
            // they work on every tab, not only playlist tabs. `PlaybackState`
            // ignores everything else, so a view that already handled its own
            // action isn't double-acted on here.
            other => {
                self.playback.update(cx, |playback, cx| {
                    if playback.apply(other) {
                        cx.notify();
                    }
                });
            }
        }
    }

    /// Drop tabs whose backing is gone: a playlist tab whose playlist was
    /// deleted (a rescan also drops temporary playlists, taking their tabs
    /// with them), or a folder tab whose temporary playlists a rescan removed.
    /// Artist tabs are scoped by name, not by a library entity, so they're
    /// always kept.
    fn prune(&mut self, cx: &App) {
        let library = self.library.read(cx).library();
        let alive = |id: PlaylistId| library.playlist(id).is_some();
        self.tabs.retain(|tab| match &tab.id {
            TabId::Playlist(id) => alive(*id),
            TabId::Folder(_) => tab.temp.iter().all(|id| alive(*id)),
            _ => true,
        });
        let active_gone = match self.active.clone() {
            TabId::Playlist(id) => !alive(id),
            TabId::Folder(path) => {
                let id = TabId::Folder(path.clone());
                !self.tabs.iter().any(|tab| tab.id == id)
            }
            _ => false,
        };
        if active_gone {
            self.active = TabId::Browse;
        }
    }

    /// The live title of a tab — read from the library so a rename shows up.
    fn tab_title(&self, id: &TabId, cx: &App) -> String {
        match id {
            TabId::Browse => "Library".to_string(),
            TabId::Playlist(playlist) => self
                .library
                .read(cx)
                .library()
                .playlist(*playlist)
                .map(|playlist| playlist.meta().title)
                .unwrap_or_default(),
            TabId::Artist(artist) => artist.clone(),
            TabId::Folder(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
        }
    }
}

impl Container for TabsView {
    fn container_id() -> &'static str {
        "tabs"
    }
}

impl Render for TabsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themed.theme();

        // The titlebar names the current track while one is loaded, and falls
        // back to the app name otherwise.
        let title = {
            let state = self.playback.read(cx);
            let song = state.metadata();
            if song.id != 0 && !state.ended() {
                format!("Playing {}", song.display_title())
            } else {
                "Larkspur".to_string()
            }
        };
        if self.last_title != title {
            self.last_title = title.clone();
            window.set_window_title(&title);
        }

        // A row's "Go to playlist" / "Go to artist" / right-click left a
        // request behind; pick it up when the library notified (and only then,
        // so an unrelated frame doesn't poll).
        if self.library_dirty {
            self.library_dirty = false;
            if let Some(id) = self.library.update(cx, |state, _cx| state.take_open_playlist()) {
                self.open_playlist(id, window, cx);
            }
            if let Some(artist) = self.library.update(cx, |state, _cx| state.take_artist_view()) {
                self.open_artist(artist, window, cx);
            }
            if let Some(dir) = self.library.update(cx, |state, _cx| state.take_folder_play()) {
                let playback = self.playback.clone();
                let playlists =
                    self.library.update(cx, |state, cx| state.play_folder(&dir, &playback, cx));
                if !playlists.is_empty() {
                    self.open_folder(dir, playlists, window, cx);
                }
            }
            if let Some(request) = self.library.read(cx).pending_song_menu().cloned() {
                self.library.update(cx, |state, _cx| state.clear_song_menu());
                self.menu = Some(request);
            }
        }

        self.prune(cx);

        let mut strip: Vec<AnyElement> = Vec::new();
        for (index, id) in self.tab_ids().into_iter().enumerate() {
            let title = self.tab_title(&id, cx);
            let closable = !matches!(id, TabId::Browse);
            let activate_id = id.clone();
            let close_id = id.clone();
            strip.push(tab_button(
                theme,
                index,
                title,
                // An overlay takes over the content area, so no tab is "active"
                // while one is up.
                self.active == id && !self.search_open && !self.settings_open,
                closable,
                cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.activate(activate_id.clone(), window, cx);
                }),
                cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if !matches!(close_id, TabId::Browse) {
                        this.close_tab(close_id.clone(), window, cx);
                    }
                }),
            ));
        }
        if self.search_open {
            strip.push(overlay_chip(theme, "Search"));
        }
        if self.settings_open {
            strip.push(overlay_chip(theme, "Settings"));
        }

        let content: AnyElement = if self.settings_open {
            self.settings.clone().into_any_element()
        } else if self.search_open {
            self.search.clone().into_any_element()
        } else {
            match &self.active {
                TabId::Browse => self.browse.clone().into_any_element(),
                _ => self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == self.active)
                    .map(|tab| tab.view.clone().into_any_element())
                    .unwrap_or_else(|| div().into_any_element()),
            }
        };

        // The song menu is raised by a row anywhere and rendered here, on top of
        // every container. Each item's handler is wrapped so choosing one also
        // dismisses the menu — the action (open a tab, remove from a playlist)
        // shouldn't leave the menu hanging over what it just did.
        let menu_element = self.menu.clone().map(|menu| {
            let weak = cx.entity().downgrade();
            let items: Vec<(String, MenuHandler)> =
                song_menu_items(&self.library, &self.playback, &menu.songs, menu.playlist, cx)
                    .into_iter()
                    .map(|(label, handler)| {
                        let weak = weak.clone();
                        let wrapped: MenuHandler = Box::new(move |event, window, cx| {
                            handler(event, window, cx);
                            weak.update(cx, |this, cx| {
                                this.menu = None;
                                cx.notify();
                            })
                            .ok();
                        });
                        (label, wrapped)
                    })
                    .collect();
            context_menu(
                theme,
                menu.position,
                items,
                cx.listener(|this, _event: &MouseDownEvent, _window, cx| {
                    this.menu = None;
                    cx.notify();
                }),
            )
        });

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.menu.is_some() {
                    this.menu = None;
                    cx.notify();
                    return;
                }
                if let Some(action) = action_for_key(&event.keystroke) {
                    this.dispatch(action, window, cx);
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
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(strip),
            )
            .child(div().flex_1().min_h_0().child(content))
            .when_some(menu_element, |d, menu| d.child(menu))
    }
}

fn overlay_chip(theme: Theme, label: &str) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(theme.row_active)
        .text_size(px(theme.font_size - 1.0))
        .text_color(theme.accent)
        .child(label.to_string())
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn tab_button(
    theme: Theme,
    index: usize,
    title: String,
    active: bool,
    closable: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let tab_px = theme.font_size - 1.0;
    div()
        .id(("tab", index))
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .rounded_t_md()
        .cursor_pointer()
        .text_size(px(tab_px))
        .when(active, |d| d.bg(theme.row_active).text_color(theme.text))
        .when(!active, |d| d.text_color(theme.text_muted))
        .hover(|d| d.bg(theme.row_hover))
        .on_click(on_click)
        .child(div().max_w(px(220.0)).truncate().child(title))
        .when(closable, |d| {
            d.child(
                div()
                    .id(("tab-close", index))
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .text_size(px(tab_px))
                    .text_color(theme.text_faint)
                    .hover(|d| d.bg(theme.row_playing).text_color(theme.text))
                    .on_click(on_close)
                    .child("×"),
            )
        })
        .into_any_element()
}
