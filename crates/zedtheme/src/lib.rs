//! Reuses Zed's theme *format*: a Zed theme family JSON goes in, a `gpui-component` theme set
//! JSON comes out.
//!
//! We deliberately do not link Zed's `theme`/`ui` crates: they are GPL-3.0 and are written
//! against Zed's git `gpui`, which is a different type universe from the `gpui-pre` snapshot
//! that `gpui-component` uses.
//!
//! * Chrome colors (`colors`) are mapped key by key; anything left unmapped is derived by
//!   `gpui-component` (hover/active variants and so on).
//! * The Zed `style` block is passed through untouched as `highlight`: `gpui-component`'s
//!   syntax-highlight theme deserializes Zed's `editor.*` and `syntax` keys directly.

pub mod settings;

pub use settings::{ThemeChoice, ZedSettings, parse_zed_settings};

use serde_json::{Map, Value, json};

/// Bundled theme families (MIT-licensed, copied from Zed's `assets/themes`).
pub const BUNDLED: &[(&str, &str)] = &[
    ("ayu", include_str!("../themes/ayu/ayu.json")),
    ("one", include_str!("../themes/one/one.json")),
    ("gruvbox", include_str!("../themes/gruvbox/gruvbox.json")),
];

pub const DEFAULT_DARK: &str = "Ayu Dark";
pub const DEFAULT_LIGHT: &str = "Ayu Light";

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("theme file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("theme file has no `themes` array")]
    NoThemes,
    #[error("theme `{0}` has no `style` object")]
    NoStyle(String),
}

/// Optional font overrides applied to every converted theme (from the user's Zed settings).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontOptions {
    pub ui_size: Option<f32>,
    pub mono_size: Option<f32>,
    pub ui_family: Option<String>,
    pub mono_family: Option<String>,
}

impl FontOptions {
    pub fn from_settings(s: &ZedSettings) -> Self {
        // Zed's own bundled fonts (`.ZedSans`, `.ZedMono`) do not exist outside Zed.
        let usable = |f: &Option<String>| f.clone().filter(|f| !f.starts_with(".Zed"));
        Self {
            ui_size: s.ui_font_size,
            mono_size: s.buffer_font_size,
            ui_family: usable(&s.ui_font_family),
            mono_family: usable(&s.buffer_font_family),
        }
    }
}

/// Converts a Zed theme family into a `gpui-component` `ThemeSet` JSON document.
pub fn convert(zed_json: &str) -> Result<String, ConvertError> {
    convert_with(zed_json, &FontOptions::default())
}

pub fn convert_with(zed_json: &str, fonts: &FontOptions) -> Result<String, ConvertError> {
    let family: Value = serde_json::from_str(zed_json)?;
    let themes = family
        .get("themes")
        .and_then(Value::as_array)
        .ok_or(ConvertError::NoThemes)?;

    let mut out = Vec::with_capacity(themes.len());
    for theme in themes {
        let name = theme.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
        let style = theme
            .get("style")
            .filter(|s| s.is_object())
            .ok_or_else(|| ConvertError::NoStyle(name.to_string()))?;
        let dark = theme.get("appearance").and_then(Value::as_str) != Some("light");
        let mut converted = json!({
            "name": name,
            "mode": if dark { "dark" } else { "light" },
            "colors": map_colors(style),
            "highlight": style,
        });
        if let Some(size) = fonts.ui_size {
            converted["font.size"] = json!(size);
        }
        if let Some(size) = fonts.mono_size {
            converted["mono_font.size"] = json!(size);
        }
        if let Some(family) = &fonts.ui_family {
            converted["font.family"] = json!(family);
        }
        if let Some(family) = &fonts.mono_family {
            converted["mono_font.family"] = json!(family);
        }
        out.push(converted);
    }
    Ok(serde_json::to_string(&json!({
        "name": family.get("name").and_then(Value::as_str).unwrap_or("Zed theme"),
        "author": family.get("author"),
        "themes": out,
    }))?)
}

/// Looks up a Zed color by key, tolerating `null`/missing values.
fn color<'a>(style: &'a Value, key: &str) -> Option<&'a str> {
    style.get(key).and_then(Value::as_str)
}

/// Collects mapped colors for one theme.
struct Palette<'a> {
    style: &'a Value,
    colors: Map<String, Value>,
}

impl Palette<'_> {
    /// Takes the first present source key, so callers list preferred → fallback.
    fn put(&mut self, target: &str, sources: &[&str]) {
        if let Some(c) = sources.iter().find_map(|k| color(self.style, k)) {
            self.set(target, c);
        }
    }

    fn set(&mut self, target: &str, value: &str) {
        self.colors.insert(target.to_string(), Value::String(value.to_string()));
    }
}

fn map_colors(style: &Value) -> Map<String, Value> {
    let mut p = Palette { style, colors: Map::new() };

    // Surfaces & text. Zed's `editor.background` is the deepest surface, which is what a data
    // grid / content area should use; `surface.background` is the panel colour.
    p.put("background", &["editor.background", "background"]);
    p.put("foreground", &["text"]);
    p.put("border", &["border"]);
    p.put("input.border", &["border"]);
    p.put("ring", &["border.focused"]);
    p.put("muted.background", &["element.background"]);
    p.put("muted.foreground", &["text.muted"]);
    p.put("accent.background", &["element.hover"]);
    p.put("accent.foreground", &["text"]);
    p.put("secondary.background", &["element.background"]);
    p.put("secondary.hover.background", &["element.hover"]);
    p.put("secondary.active.background", &["element.active"]);
    p.put("secondary.foreground", &["text"]);
    p.put("popover.background", &["elevated_surface.background", "surface.background"]);
    p.put("popover.foreground", &["text"]);
    p.put("link", &["text.accent"]);
    p.put("drop_target.background", &["drop_target.background"]);
    p.put("window.border", &["border"]);

    // Primary = Zed's accent; pick a readable foreground for it.
    p.put("primary.background", &["text.accent"]);
    if let Some(fg) = readable_on(
        color(style, "text.accent"),
        color(style, "editor.background"),
        color(style, "text"),
    ) {
        p.set("primary.foreground", fg);
    }

    // Sidebar (navigator).
    p.put("sidebar.background", &["surface.background", "background"]);
    p.put("sidebar.foreground", &["text"]);
    p.put("sidebar.border", &["border.variant", "border"]);
    p.put("sidebar.accent.background", &["element.selected", "element.hover"]);
    p.put("sidebar.accent.foreground", &["text"]);
    p.put("sidebar.primary.background", &["text.accent"]);
    p.put("sidebar.primary.foreground", &["editor.background"]);

    // Window chrome.
    p.put("title_bar.background", &["title_bar.background"]);
    p.put("title_bar.border", &["border.variant", "border"]);
    p.put("status_bar.background", &["status_bar.background"]);
    p.put("status_bar.border", &["border.variant", "border"]);

    // Tabs.
    p.put("tab_bar.background", &["tab_bar.background"]);
    p.put("tab.background", &["tab.inactive_background"]);
    p.put("tab.foreground", &["text.muted"]);
    p.put("tab.active.background", &["tab.active_background"]);
    p.put("tab.active.foreground", &["text"]);

    // Lists & the data grid.
    p.put("list.background", &["editor.background", "background"]);
    p.put("list.head.background", &["surface.background"]);
    p.put("list.hover.background", &["ghost_element.hover", "element.hover"]);
    p.put("list.active.background", &["element.selected"]);
    p.put("list.active.border", &["border.focused"]);
    p.put("table.background", &["editor.background", "background"]);
    p.put("table.head.background", &["surface.background"]);
    p.put("table.head.foreground", &["text.muted"]);
    p.put("table.hover.background", &["ghost_element.hover", "element.hover"]);
    p.put("table.active.background", &["element.selected"]);
    p.put("table.active.border", &["border.focused"]);
    p.put("table.row.border", &["border.variant", "border"]);

    // Scrollbars.
    p.put("scrollbar.background", &["scrollbar.track.background"]);
    p.put("scrollbar.thumb.background", &["scrollbar.thumb.background"]);
    p.put("scrollbar.thumb.hover.background", &["scrollbar.thumb.hover_background"]);

    // Editor caret / selection come from the first player.
    if let Some(player) = style.get("players").and_then(|players| players.get(0)) {
        if let Some(c) = player.get("cursor").and_then(Value::as_str) {
            p.set("caret", c);
        }
        if let Some(c) = player.get("selection").and_then(Value::as_str) {
            p.set("selection.background", c);
        }
    }

    // Status colours; foregrounds are picked for contrast.
    for (target, source) in [
        ("danger", "error"),
        ("warning", "warning"),
        ("success", "success"),
        ("info", "info"),
    ] {
        if let Some(bg) = color(style, source) {
            p.set(&format!("{target}.background"), bg);
            if let Some(fg) = readable_on(
                Some(bg),
                color(style, "editor.background"),
                color(style, "text"),
            ) {
                p.set(&format!("{target}.foreground"), fg);
            }
        }
    }

    // ANSI palette → gpui-component's base hues.
    for (hue, ansi) in [
        ("red", "red"),
        ("green", "green"),
        ("yellow", "yellow"),
        ("blue", "blue"),
        ("magenta", "magenta"),
        ("cyan", "cyan"),
    ] {
        p.put(&format!("base.{hue}"), &[&format!("terminal.ansi.{ansi}")]);
        p.put(
            &format!("base.{hue}.light"),
            &[&format!("terminal.ansi.bright_{ansi}"), &format!("terminal.ansi.{ansi}")],
        );
    }
    for (i, ansi) in ["blue", "green", "yellow", "magenta", "cyan"].iter().enumerate() {
        p.put(&format!("chart.{}", i + 1), &[&format!("terminal.ansi.{ansi}")]);
    }

    p.colors
}

/// Returns whichever of `a` / `b` has the higher contrast against `bg`.
fn readable_on<'a>(bg: Option<&str>, a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    let bg = luminance(bg?)?;
    let contrast = |c: &str| {
        let l = luminance(c)?;
        let (hi, lo) = if l > bg { (l, bg) } else { (bg, l) };
        Some((hi + 0.05) / (lo + 0.05))
    };
    match (a.and_then(|c| Some((c, contrast(c)?))), b.and_then(|c| Some((c, contrast(c)?)))) {
        (Some((ca, ra)), Some((cb, rb))) => Some(if ra >= rb { ca } else { cb }),
        (Some((c, _)), None) | (None, Some((c, _))) => Some(c),
        (None, None) => None,
    }
}

/// WCAG relative luminance of `#rrggbb` / `#rrggbbaa`.
fn luminance(hex: &str) -> Option<f64> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 && h.len() != 8 {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    let lin = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    Some(0.2126 * lin(ch(0)?) + 0.7152 * lin(ch(2)?) + 0.0722 * lin(ch(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converted(family: &str) -> Value {
        let src = BUNDLED.iter().find(|(n, _)| *n == family).unwrap().1;
        serde_json::from_str(&convert(src).unwrap()).unwrap()
    }

    fn theme<'a>(set: &'a Value, name: &str) -> &'a Value {
        set["themes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no theme {name}"))
    }

    #[test]
    fn ayu_dark_maps_core_colors() {
        let set = converted("ayu");
        let t = theme(&set, DEFAULT_DARK);
        assert_eq!(t["mode"], "dark");
        // From the bundled ayu.json: editor.background / text / text.accent / surface.background.
        assert_eq!(t["colors"]["background"], "#0d1016ff");
        assert_eq!(t["colors"]["foreground"], "#bfbdb6ff");
        assert_eq!(t["colors"]["primary.background"], "#5ac1feff");
        assert_eq!(t["colors"]["sidebar.background"], "#1f2127ff");
        assert_eq!(t["colors"]["danger.background"], "#ef7177ff");
    }

    #[test]
    fn style_block_is_passed_through_for_syntax_highlighting() {
        let set = converted("ayu");
        let h = &theme(&set, DEFAULT_DARK)["highlight"];
        assert_eq!(h["editor.background"], "#0d1016ff");
        assert!(h["syntax"]["keyword"]["color"].is_string());
    }

    #[test]
    fn every_bundled_theme_converts_with_a_complete_core_palette() {
        const REQUIRED: &[&str] = &[
            "background", "foreground", "border", "primary.background", "primary.foreground",
            "muted.foreground", "sidebar.background", "title_bar.background",
            "status_bar.background", "tab.active.background", "table.background",
            "table.head.background", "list.active.background", "caret", "selection.background",
            "danger.background", "danger.foreground", "success.background", "warning.background",
            "base.red", "base.blue",
        ];
        for (family, _) in BUNDLED {
            let set = converted(family);
            for t in set["themes"].as_array().unwrap() {
                for key in REQUIRED {
                    assert!(
                        t["colors"][key].is_string(),
                        "{} / {}: missing `{key}`",
                        family, t["name"]
                    );
                }
            }
        }
    }

    #[test]
    fn light_and_dark_modes_are_detected() {
        let set = converted("ayu");
        assert_eq!(theme(&set, DEFAULT_LIGHT)["mode"], "light");
        assert_eq!(theme(&set, "Ayu Mirage")["mode"], "dark");
    }

    #[test]
    fn primary_foreground_is_readable() {
        // Ayu Dark's accent is light, so the foreground must be the dark editor background.
        let set = converted("ayu");
        assert_eq!(theme(&set, DEFAULT_DARK)["colors"]["primary.foreground"], "#0d1016ff");
    }

    #[test]
    fn font_options_are_written_and_zed_private_fonts_are_dropped() {
        let settings = ZedSettings {
            ui_font_size: Some(14.0),
            buffer_font_size: Some(13.0),
            ui_font_family: Some(".ZedSans".into()),
            buffer_font_family: Some("JetBrains Mono".into()),
            ..Default::default()
        };
        let src = BUNDLED[0].1;
        let out: Value = serde_json::from_str(&convert_with(src, &FontOptions::from_settings(&settings)).unwrap()).unwrap();
        let t = theme(&out, DEFAULT_DARK);
        assert_eq!(t["font.size"], 14.0);
        assert_eq!(t["mono_font.size"], 13.0);
        assert_eq!(t["mono_font.family"], "JetBrains Mono");
        assert!(t.get("font.family").is_none(), ".ZedSans is Zed-internal and must not be requested");
        // With no options nothing font-related is emitted.
        let plain: Value = serde_json::from_str(&convert(src).unwrap()).unwrap();
        assert!(theme(&plain, DEFAULT_DARK).get("font.size").is_none());
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(matches!(convert("nope"), Err(ConvertError::Json(_))));
        assert!(matches!(convert("{}"), Err(ConvertError::NoThemes)));
        assert!(matches!(
            convert(r#"{"themes":[{"name":"x"}]}"#),
            Err(ConvertError::NoStyle(_))
        ));
    }
}
