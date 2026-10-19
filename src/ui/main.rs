use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    px, size, App, Application, AppContext, Bounds, Point, TitlebarOptions, WindowBounds,
    WindowOptions,
};

use larkspur::audio::PlaybackController;
use larkspur::model::{Config, LibraryCache, WindowPlacement};
use larkspur::ui::animation::Animator;
use larkspur::ui::assets::Assets;
use larkspur::ui::config_state::ConfigState;
use larkspur::ui::container::Workspace;
use larkspur::ui::cover::CoverView;
use larkspur::ui::cover_store::CoverStore;
use larkspur::ui::layout;
use larkspur::ui::library_state::LibraryState;
use larkspur::ui::lyrics::LyricsView;
use larkspur::ui::playback::PlaybackView;
use larkspur::ui::playlists::PlaylistsView;
use larkspur::ui::playback_state::PlaybackState;
use larkspur::ui::tabs::TabsView;
use larkspur::ui::visualizer::VisualizerView;

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

    // Load the config and fold in any directories from the command line, so
    // the documented `larkspur_ui <dir>` still works and the path sticks.
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
    // Read before `config` moves into the state: the layout is built from it.
    let visualizer_on = config.visualizer;
    let sidebar_minimized = config.sidebar_minimized;

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        // Reopen where the window was last closed; a first run opens at half
        // the monitor's size, centered.
        let bounds = config.window.map(|placement| {
            Bounds::<gpui::Pixels>::new(
                Point::new(px(placement.x), px(placement.y)),
                size(px(placement.width), px(placement.height)),
            )
        });
        let bounds = bounds.unwrap_or_else(|| {
            let (width, height) = cx
                .displays()
                .first()
                .map(|display| {
                    let bounds = display.bounds();
                    (bounds.size.width, bounds.size.height)
                })
                .unwrap_or((px(1920.0), px(1080.0)));
            Bounds::centered(None, size(width / 2.0, height / 2.0), cx)
        });
        let controller = PlaybackController::new(Vec::new()).expect("failed to open the audio device");

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
                let config_state = cx.new(|cx| {
                    let state = ConfigState::new(config, config_path, cx);
                    // Remember the window's placement whenever it moves or
                    // resizes, so the next launch reopens the same way. Only
                    // the windowed bounds are kept — a fullscreen/maximized
                    // session must not clobber the restored geometry.
                    cx.observe_window_bounds(window, |state: &mut ConfigState, window, cx| {
                        if let WindowBounds::Windowed(bounds) = window.window_bounds() {
                            let placement = WindowPlacement {
                                x: bounds.origin.x.into(),
                                y: bounds.origin.y.into(),
                                width: bounds.size.width.into(),
                                height: bounds.size.height.into(),
                            };
                            state.set_window_placement(placement, cx);
                        }
                    })
                    .detach();
                    state
                });
                // The library is installed from the cache instantly, then
                // synced against the filesystem in the background. Its roots
                // come from the config, which it observes.
                let cache = LibraryCache::open(cache_dir().join("library.json"));
                let library_state = cx.new(|cx| LibraryState::new(cache, config_state.clone(), cx));

                // Playback resolves the current track's metadata from the
                // library, so it's built after it.
                let playback_state =
                    cx.new(|cx| PlaybackState::new(controller, library_state.clone(), cx));

                // Stats are saved eagerly on every change, but a last-second
                // toggle still gets a flush at quit.
                cx.on_app_quit({
                    let library_state = library_state.clone();
                    move |cx| {
                        library_state.update(cx, |state, _cx| state.save_stats());
                        async {}
                    }
                })
                .detach();

                // Command-line files are queued now that the library exists, so
                // each is loaded into it and resolvable to metadata.
                if !files.is_empty() {
                    library_state.update(cx, |state, cx| {
                        state.play_files(files, 0, &playback_state, cx);
                    });
                }

                // Covers are decoded off-thread and shared by the now-playing
                // square and the browse/search thumbnails. Each is derived at
                // the window's physical pixel sizes, so it draws ~1:1.
                let cover_store = cx.new(|cx| {
                    CoverStore::new(cache_dir().join("covers-v2"), window.scale_factor(), cx)
                });

                // The shared frame clock: animations (selection fades, lyric
                // scrolling) observe this; the transport's coarse readouts tick
                // on the slow rate instead.
                let animator = cx.new(Animator::new);

                // The center is the tab container; the rail opens playlists
                // into it, so it needs a handle to the tabs.
                let tabs = cx.new(|cx| {
                    TabsView::new(
                        library_state.clone(),
                        playback_state.clone(),
                        cover_store.clone(),
                        config_state.clone(),
                        animator.clone(),
                        cx,
                    )
                });
                let playlists = cx.new(|cx| {
                    PlaylistsView::new(
                        library_state.clone(),
                        tabs.clone(),
                        config_state.clone(),
                        cover_store.clone(),
                        animator.clone(),
                        cx,
                    )
                });

                let playback = cx.new(|cx| {
                    PlaybackView::new(
                        playback_state.clone(),
                        config_state.clone(),
                        animator.clone(),
                        cx,
                    )
                });
                let lyrics = cx.new(|cx| {
                    LyricsView::new(
                        playback_state.clone(),
                        library_state.clone(),
                        cover_store.clone(),
                        config_state.clone(),
                        animator.clone(),
                        cx,
                    )
                });
                let visualizer = cx.new(|cx| {
                    VisualizerView::new(playback_state.clone(), config_state.clone(), animator.clone(), cx)
                });
                let cover = cx.new(|cx| {
                    CoverView::new(
                        library_state.clone(),
                        playback_state.clone(),
                        cover_store.clone(),
                        config_state.clone(),
                        cx,
                    )
                });

                // The active tab is the input hub, so focus lands there.
                if let Some(focus) = tabs.read(cx).focus_handle_for_window(cx) {
                    window.focus(&focus);
                }

                // The dock arrangement lives in `ui::layout::app_layout`, so
                // it's shared with the text-introspection tooling. Here we
                // only bind live views to its module ids.
                let workspace = cx.new(|cx| {
                    let mut workspace = Workspace::from_plan(
                        layout::app_layout_with_rail(
                            visualizer_on,
                            layout::rail_px(sidebar_minimized),
                        ),
                        &config_state,
                        cx,
                    );
                    workspace.push(playlists);
                    workspace.push(visualizer);
                    workspace.push(lyrics);
                    workspace.push(cover);
                    workspace.push(playback);
                    workspace.push(tabs);
                    // The rail's collapse/expand slides on the shared frame
                    // clock rather than snapping between widths.
                    workspace.attach_animator(animator.clone(), cx);

                    // The visualizer band is added/removed from the rail live
                    // when its setting flips, and the left rail slides to its
                    // collapsed width when it's minimized. The flags guard
                    // against re-planning (and re-rendering the whole
                    // workspace) on unrelated config changes.
                    let shown = Rc::new(Cell::new(visualizer_on));
                    let minimized = Rc::new(Cell::new(sidebar_minimized));
                    let shown = shown.clone();
                    let minimized = minimized.clone();
                    cx.observe(&config_state, move |workspace: &mut Workspace, config, cx| {
                        let on = config.read(cx).visualizer();
                        let mini = config.read(cx).sidebar_minimized();
                        if on != shown.get() {
                            shown.set(on);
                            // Keep the rail where it is on screen (it may be
                            // mid-slide) while the visualizer band changes.
                            workspace.set_plan(layout::app_layout_with_rail(
                                on,
                                workspace.rail_width(),
                            ));
                            cx.notify();
                        }
                        if mini != minimized.get() {
                            minimized.set(mini);
                            workspace.animate_rail_to(layout::rail_px(mini), cx);
                        }
                    })
                    .detach();

                    workspace
                });
                workspace
            },
        )
        .unwrap();

        cx.activate(true);
    });
}

/// A per-user data directory for `larkspur`, following the XDG convention: the
/// `var` directory if set, else `$HOME/fallback`, else the current directory.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("larkspur")
}

fn cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}
