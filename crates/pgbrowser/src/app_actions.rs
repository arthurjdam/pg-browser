//! App-level keyboard shortcuts and the native macOS menu bar.
//!
//! GPUI's action-dispatch routes a bound key to whichever entity is focused (or its ancestors);
//! these are bound with `context: None`, so they fire from anywhere in the window, not just one
//! pane — matching how a native menu shortcut behaves. Each is also a real menu item, so it is
//! discoverable without knowing the shortcut, and the menu and the keystroke share one handler.

use gpui_kit::{App, KeyBinding, Menu, MenuItem, SystemMenuType, actions};

actions!(pg_browser, [CloseActiveTab, NextTab, PrevTab, ReloadActive, FocusFilter, FocusNavigator, Quit]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-w", CloseActiveTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("cmd-r", ReloadActive, None),
        KeyBinding::new("cmd-f", FocusFilter, None),
        KeyBinding::new("cmd-shift-f", FocusNavigator, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

    cx.set_menus(vec![
        Menu::new("pg-browser").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit pg-browser", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Reload", ReloadActive),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseActiveTab),
        ]),
        Menu::new("View").items([
            MenuItem::action("Focus Filter", FocusFilter),
            MenuItem::action("Focus Navigator", FocusNavigator),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PrevTab),
        ]),
    ]);
}
