use gpui::{
    px, size, App, Application, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};

use larkspur::audio::PlaybackController;
use larkspur::ui::container::{col, module, Dock, Workspace, EmptyView};
use larkspur::ui::cover::CoverView;
use larkspur::ui::playback::PlaybackView;

fn main() {
    let queue: Vec<std::path::PathBuf> = std::env::args().skip(1).map(Into::into).collect();
    if queue.is_empty() {
        eprintln!("usage: larkspur_ui <path-to-song> [more songs...]");
        std::process::exit(2);
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1920.0), px(1080.0)), cx);
        let controller =
            PlaybackController::new(queue).expect("failed to load songs");
        let has_art = matches!(
            controller.metadata.cover,
            larkspur::datatypes::CoverState::NotRequested | larkspur::datatypes::CoverState::Ready(_)
        );

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
                let cover = cx.new(|_| CoverView { has_art });
                let playback = cx.new(|cx| PlaybackView::new(controller, cx));
                let empty_center = cx.new(|_| EmptyView);
                let empty_left = cx.new(|_| EmptyView);
                let empty_above_cover = cx.new(|_| EmptyView);

                let focus_handle = playback.read(cx).focus_handle_for_window();
                window.focus(&focus_handle);

                // Edge-docked layout: playback bar across the bottom, cover
                // square docked right, with matching placeholder rails on
                // the left and above the cover. The rails carry z(2), so
                // they hug their window edges at full height and the bottom
                // bar (z=1) fits between them — keeping it centered in the
                // window instead of shifted left by the cover. The cover
                // sits in the bottom-right corner with a placeholder filling
                // the space above it.
                let mut workspace = Workspace::new(module("empty"));
                workspace.dock(Dock::bottom(module("playback")));
                workspace.dock(
                    Dock::right(
                        col(vec![
                            module("empty-above-cover").fill(),
                            module("cover"),
                        ])
                        .px(240.0),
                    )
                    .z(2),
                );
                workspace.dock(Dock::left(module("empty-left").px(240.0)).z(2));
                workspace.push_as("empty", empty_center);
                workspace.push_as("empty-left", empty_left);
                workspace.push_as("empty-above-cover", empty_above_cover);
                workspace.push(cover);
                workspace.push(playback);
                cx.new(|_| workspace)
            }
        )
        .unwrap();

        cx.activate(true);
    });
}
