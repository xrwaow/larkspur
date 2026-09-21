use std::path::PathBuf;

use gpui::{
    px, size, App, Application, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};

use larkspur::audio::PlaybackController;
use larkspur::model::{Config, LibraryCache};
use larkspur::ui::config_state::ConfigState;
use larkspur::ui::container::Workspace;
use larkspur::ui::cover::CoverView;
use larkspur::ui::cover_store::CoverStore;
use larkspur::ui::layout;
use larkspur::ui::library_state::LibraryState;
use larkspur::ui::lyrics::LyricsView;
use larkspur::ui::playback::PlaybackView;
use larkspur::ui::playlists::PlaylistsView;
use larkspur::ui::state::PlaybackState;
use larkspur::ui::tabs::TabsView;

fn main() {
    // Command-line arguments seed the library: directories are merged into the
    // synced-path list (which lives in the config from then on), and files are
    // queued for playback. Neither is required — with none, the app opens and
    // the settings panel is where you add music.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1).map(PathBuf::from) {
        if arg.is_dir() {
            dirs.push(arg);
        } else {
            files.push(arg);
        }
    }

    // Given only files, scan their parent directories too, so the library and
    // its autogen playlists still populate.
    if dirs.is_empty() {
        for file in &files {
            if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
                dirs.push(parent.to_path_buf());
            }
        }
    }

    // Load the config and fold in any directories from the command line, so the
    // documented `larkspur_ui <dir>` still works and the path sticks.
    let config_path = config_dir().join("config.json");
    let mut config = Config::load(&config_path);
    let mut changed = false;
    for dir in dirs {
        changed |= config.add_root(dir);
    }
    if changed {
        if let Err(e) = config.save(&config_path) {
            eprintln!("failed to save config: {e}");
        }
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1920.0), px(1080.0)), cx);
        let controller = PlaybackController::new(files).expect("failed to open the audio device");

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Larkspur".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                // The shared, observable state. Views below observe these —
                // none own the controller, the library, the covers, or the
                // config, and none poll.
                let config_state = cx.new(|cx| ConfigState::new(config, config_path, cx));
                let playback_state = cx.new(|cx| PlaybackState::new(controller, cx));

                // The library is installed from the cache instantly, then
                // synced against the filesystem in the background. Its roots
                // come from the config, which it observes.
                let cache = LibraryCache::open(cache_dir().join("library.json"));
                let library_state = cx.new(|cx| LibraryState::new(cache, config_state.clone(), cx));

                // Covers are decoded off-thread and shared by the now-playing
                // square and the browse/search thumbnails.
                let cover_store = cx.new(|cx| CoverStore::new(cache_dir().join("covers"), cx));

                // The center is the tab container; the rail opens playlists
                // into it, so it needs a handle to the tabs.
                let tabs = cx.new(|cx| {
                    TabsView::new(
                        library_state.clone(),
                        playback_state.clone(),
                        cover_store.clone(),
                        config_state.clone(),
                        cx,
                    )
                });
                let playlists = cx.new(|cx| {
                    PlaylistsView::new(library_state.clone(), tabs.clone(), config_state.clone(), cx)
                });

                let playback = cx.new(|cx| PlaybackView::new(playback_state.clone(), config_state.clone(), cx));
                let lyrics = cx.new(|cx| LyricsView::new(playback_state.clone(), config_state.clone(), cx));
                let cover = cx.new(|cx| {
                    CoverView::new(playback_state.clone(), cover_store.clone(), config_state.clone(), cx)
                });

                // The active tab is the input hub, so focus lands there.
                if let Some(focus) = tabs.read(cx).focus_handle_for_window(cx) {
                    window.focus(&focus);
                }

                // The dock arrangement lives in `ui::layout::app_layout`, so
                // it's shared with the text-introspection tooling. Here we
                // only bind live views to its module ids.
                let mut workspace = Workspace::from_plan(layout::app_layout());
                workspace.push(playlists);
                workspace.push(lyrics);
                workspace.push(cover);
                workspace.push(playback);
                workspace.push(tabs);
                cx.new(|_| workspace)
            },
        )
        .unwrap();

        cx.activate(true);
    });
}

/// Where the library cache and cover thumbnails live, following the XDG cache
/// convention.
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("larkspur")
}

/// Where the config lives, following the XDG config convention.
fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("larkspur")
}
