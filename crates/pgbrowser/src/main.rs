mod data_view;
mod runtime;
mod screenshot;
mod theme;
mod workspace;

use gpui_kit::component::*;
use gpui_kit::*;
use workspace::Workspace;

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        theme::install(cx);
        cx.spawn(async move |cx| {
            let window = cx
                .update(|cx| {
                    let options = WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                            None,
                            size(px(1280.), px(800.)),
                            cx,
                        ))),
                        window_min_size: Some(size(px(720.), px(480.))),
                        ..TitleBar::window_options()
                    };
                    cx.open_window(options, |window, cx| {
                        let view = cx.new(|cx| Workspace::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                    })
                })
                .expect("failed to open window");
            screenshot::maybe_capture(window.into(), cx).await;
        })
        .detach();
    });
}
