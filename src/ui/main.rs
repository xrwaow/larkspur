use gpui::{
    px, size, App, Application, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};

use larkspur::audio::PlaybackController;
use larkspur::ui::container::{EmptyView, Workspace};
use larkspur::ui::cover::CoverView;
use larkspur::ui::layout;
use larkspur::ui::playback::PlaybackView;
use larkspur::ui::state::PlaybackState;

fn main() {
    let queue: Vec<std::path::PathBuf> = std::env::args().skip(1).map(Into::into).collect();
    if queue.is_empty() {
        eprintln!("usage: larkspur_ui <path-to-song> [more songs...]");
        std::process::exit(2);
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1920.0), px(1080.0)), cx);
        let controller = PlaybackController::new(queue).expect("failed to load songs");

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
                // One shared, observable playback state. The views below take
                // a clone of it and observe it — none of them own the
                // controller, and none of them poll.
                let state = cx.new(|cx| PlaybackState::new(controller, cx));

                let playback = cx.new(|cx| PlaybackView::new(state.clone(), cx));
                let cover = cx.new(|cx| CoverView::new(state.clone(), cx));
                let empty_center = cx.new(|_| EmptyView);
                let empty_left = cx.new(|_| EmptyView);
                let empty_above_cover = cx.new(|_| EmptyView);

                let focus_handle = playback.read(cx).focus_handle_for_window();
                window.focus(&focus_handle);

                // The dock arrangement lives in `ui::layout::app_layout`, so
                // it's shared with the text-introspection tooling. Here we
                // only bind live views to its module ids.
                let mut workspace = Workspace::from_plan(layout::app_layout());
                workspace.push_as("empty", empty_center);
                workspace.push_as("empty-left", empty_left);
                workspace.push_as("empty-above-cover", empty_above_cover);
                workspace.push(cover);
                workspace.push(playback);
                cx.new(|_| workspace)
            },
        )
        .unwrap();

        cx.activate(true);
    });
}
