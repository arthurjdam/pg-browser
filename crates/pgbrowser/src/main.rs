mod app_actions;
mod data_view;
mod value_viewer;
mod runtime;
mod screenshot;
mod stories;
mod theme;
mod workspace;

use gpui_kit::component::*;
use gpui_kit::*;
use workspace::Workspace;

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        theme::install(cx);
        app_actions::init(cx);
        cx.spawn(async move |cx| {
            let window = cx
                .update(|cx| {
                    // Dev hook: `PGB_STORY=cells` shows a component sandbox instead (see stories.rs).
                    let story = std::env::var("PGB_STORY").ok();
                    let window_size = match story {
                        Some(_) => size(px(1640.), px(2560.)),
                        None => size(px(1280.), px(800.)),
                    };
                    let options = WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, window_size, cx))),
                        window_min_size: Some(size(px(720.), px(480.))),
                        ..TitleBar::window_options()
                    };
                    cx.open_window(options, |window, cx| {
                        let story = story.and_then(|name| stories::open(&name, window, cx));
                        let view = story.unwrap_or_else(|| cx.new(|cx| Workspace::new(window, cx)).into());
                        cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                    })
                })
                .expect("failed to open window");
            screenshot::maybe_capture(window.into(), cx).await;
        })
        .detach();
    });
}
