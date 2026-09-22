//! Registers Zed-format themes with gpui-component and applies the user's Zed theme.
//!
//! Order of precedence for the active theme: `PGB_THEME` (dev override) → the `theme` setting in
//! `~/.config/zed/settings.json` → Ayu, following the system light/dark appearance.

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::*;
use std::path::{Path, PathBuf};
use zedtheme::{FontOptions, ThemeChoice, ZedSettings};

pub fn install(cx: &mut App) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let settings = home.as_deref().and_then(read_zed_settings);
    let fonts = settings.as_ref().map(FontOptions::from_settings).unwrap_or_default();

    for (family, zed_json) in zedtheme::BUNDLED {
        let converted = zedtheme::convert_with(zed_json, &fonts)
            .unwrap_or_else(|err| panic!("bundled theme `{family}` is invalid: {err}"));
        if let Err(err) = ThemeRegistry::global_mut(cx).load_themes_from_str(&converted) {
            panic!("bundled theme `{family}` was rejected by gpui-component: {err}");
        }
    }
    // The user's own Zed themes and theme extensions. A broken file must not stop the app.
    for path in home.as_deref().map(user_theme_files).unwrap_or_default() {
        let loaded = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|src| zedtheme::convert_with(&src, &fonts).map_err(|e| e.to_string()))
            .and_then(|json| {
                ThemeRegistry::global_mut(cx)
                    .load_themes_from_str(&json)
                    .map_err(|e| e.to_string())
            });
        if let Err(err) = loaded {
            eprintln!("skipping theme file {}: {err}", path.display());
        }
    }

    let choice = std::env::var("PGB_THEME")
        .ok()
        .map(ThemeChoice::Fixed)
        .or_else(|| settings.and_then(|s| s.theme))
        .unwrap_or_else(default_choice);
    if !apply(&choice, cx) {
        eprintln!("theme {choice:?} is not installed; falling back to Ayu");
        apply(&default_choice(), cx);
    }
}

fn default_choice() -> ThemeChoice {
    ThemeChoice::BySystem {
        light: zedtheme::DEFAULT_LIGHT.into(),
        dark: zedtheme::DEFAULT_DARK.into(),
    }
}

/// Applies a choice; returns false when a named theme is not registered.
fn apply(choice: &ThemeChoice, cx: &mut App) -> bool {
    let find = |name: &str, cx: &App| ThemeRegistry::global(cx).themes().get(name).cloned();
    match choice {
        ThemeChoice::Fixed(name) => {
            let Some(config) = find(name, cx) else { return false };
            Theme::global_mut(cx).apply_config(&config);
            // Switch the mode to the theme's own, so a dark theme stays dark on a light system.
            Theme::change(config.mode, None, cx);
            true
        }
        ThemeChoice::BySystem { light, dark } => {
            let (Some(light), Some(dark)) = (find(light, cx), find(dark, cx)) else { return false };
            Theme::global_mut(cx).apply_config(&light);
            Theme::global_mut(cx).apply_config(&dark);
            Theme::sync_system_appearance(None, cx);
            true
        }
    }
}

fn read_zed_settings(home: &Path) -> Option<ZedSettings> {
    let path = home.join(".config/zed/settings.json");
    let src = std::fs::read_to_string(&path).ok()?;
    match zedtheme::parse_zed_settings(&src) {
        Ok(settings) => Some(settings),
        Err(err) => {
            eprintln!("could not read {}: {err}", path.display());
            None
        }
    }
}

/// `~/.config/zed/themes/*.json` plus theme extensions installed by Zed.
fn user_theme_files(home: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let json_files_in = |dir: PathBuf, files: &mut Vec<PathBuf>| {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        found.sort();
        files.extend(found);
    };
    json_files_in(home.join(".config/zed/themes"), &mut files);
    if let Ok(extensions) = std::fs::read_dir(home.join("Library/Application Support/Zed/extensions/installed")) {
        let mut dirs: Vec<PathBuf> = extensions.filter_map(Result::ok).map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            json_files_in(dir.join("themes"), &mut files);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::user_theme_files;
    use gpui_kit::component::ThemeSet;

    /// Every converted bundled theme must be accepted by gpui-component's own parser; this is
    /// the guard against schema drift when either Zed's format or gpui-component changes.
    #[test]
    fn converted_themes_parse_as_gpui_component_theme_sets() {
        for (family, zed_json) in zedtheme::BUNDLED {
            let converted = zedtheme::convert(zed_json).unwrap();
            let set: ThemeSet = serde_json::from_str(&converted)
                .unwrap_or_else(|e| panic!("{family}: gpui-component rejected the theme: {e}"));
            assert!(!set.themes.is_empty(), "{family}");
            for theme in &set.themes {
                let highlight = theme.highlight.as_ref().unwrap_or_else(|| {
                    panic!("{family}/{}: highlight (syntax) section was dropped", theme.name)
                });
                assert!(
                    highlight.editor_background.is_some(),
                    "{family}/{}: editor.background not read",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn font_overrides_survive_gpui_components_parser() {
        let fonts = zedtheme::FontOptions { ui_size: Some(14.0), mono_size: Some(13.0), ..Default::default() };
        let json = zedtheme::convert_with(zedtheme::BUNDLED[0].1, &fonts).unwrap();
        let set: ThemeSet = serde_json::from_str(&json).unwrap();
        assert!(set.themes.iter().all(|t| t.font_size == Some(14.0) && t.mono_font_size == Some(13.0)));
    }

    #[test]
    fn finds_user_themes_and_extension_themes_in_order_and_ignores_the_rest() {
        let root = std::env::temp_dir().join(format!("pgb-theme-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "{}").unwrap();
        };
        write(".config/zed/themes/b.json");
        write(".config/zed/themes/a.json");
        write(".config/zed/themes/notes.txt");
        write("Library/Application Support/Zed/extensions/installed/nord/themes/nord.json");
        write("Library/Application Support/Zed/extensions/installed/html/languages/x.json"); // not a theme
        let found: Vec<String> = user_theme_files(&root)
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            found,
            [
                ".config/zed/themes/a.json",
                ".config/zed/themes/b.json",
                "Library/Application Support/Zed/extensions/installed/nord/themes/nord.json",
            ]
        );
        assert!(user_theme_files(std::path::Path::new("/nonexistent-home")).is_empty());
    }
}
