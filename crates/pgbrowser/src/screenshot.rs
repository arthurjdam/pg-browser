//! Dev-only offscreen capture, enabled by the `screenshot` cargo feature and the
//! `PGB_SCREENSHOT=<path.png>` environment variable. A no-op otherwise.

use gpui_kit::{AnyWindowHandle, AsyncApp};

#[cfg(not(feature = "screenshot"))]
pub async fn maybe_capture(_window: AnyWindowHandle, _cx: &mut AsyncApp) {}

#[cfg(feature = "screenshot")]
pub async fn maybe_capture(window: AnyWindowHandle, cx: &mut AsyncApp) {
    let Some(path) = std::env::var_os("PGB_SCREENSHOT") else {
        return;
    };
    // Give connecting, loading and the first frames time to finish (`PGB_SCREENSHOT_DELAY_MS`).
    let delay = std::env::var("PGB_SCREENSHOT_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1200);
    cx.background_executor()
        .timer(std::time::Duration::from_millis(delay))
        .await;
    let result = window.update(cx, |_, window, _| {
        window
            .render_to_image()
            .and_then(|img| img.save(&path).map_err(Into::into))
    });
    match result {
        Ok(Ok(())) => eprintln!("screenshot written to {}", path.to_string_lossy()),
        Ok(Err(err)) => eprintln!("screenshot failed: {err:#}"),
        Err(err) => eprintln!("screenshot failed (window gone): {err:#}"),
    }
    cx.update(|cx| cx.quit());
}
