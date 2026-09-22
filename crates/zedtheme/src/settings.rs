//! Reads the theme-related parts of a Zed `settings.json` (JSONC: comments and trailing commas).

use serde_json::Value;

/// Which theme Zed would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeChoice {
    /// `"theme": "Ayu Dark"`, or `{ "mode": "dark", "dark": "Ayu Dark" }`: always this theme.
    Fixed(String),
    /// `{ "mode": "system", "light": "...", "dark": "..." }`: follow the OS appearance.
    BySystem { light: String, dark: String },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZedSettings {
    pub theme: Option<ThemeChoice>,
    pub ui_font_size: Option<f32>,
    pub buffer_font_size: Option<f32>,
    pub ui_font_family: Option<String>,
    pub buffer_font_family: Option<String>,
}

pub fn parse_zed_settings(jsonc: &str) -> Result<ZedSettings, serde_json::Error> {
    let v: Value = serde_json::from_str(&strip_jsonc(jsonc))?;
    let num = |k: &str| v.get(k).and_then(Value::as_f64).map(|n| n as f32);
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let theme = match v.get("theme") {
        Some(Value::String(name)) => Some(ThemeChoice::Fixed(name.clone())),
        Some(Value::Object(o)) => {
            let get = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
            // Zed's defaults for the object form are One Light / One Dark.
            let light = get("light").unwrap_or_else(|| "One Light".into());
            let dark = get("dark").unwrap_or_else(|| "One Dark".into());
            Some(match get("mode").as_deref() {
                Some("light") => ThemeChoice::Fixed(light),
                Some("dark") => ThemeChoice::Fixed(dark),
                _ => ThemeChoice::BySystem { light, dark },
            })
        }
        _ => None,
    };
    Ok(ZedSettings {
        theme,
        ui_font_size: num("ui_font_size"),
        buffer_font_size: num("buffer_font_size"),
        ui_font_family: text("ui_font_family"),
        buffer_font_family: text("buffer_font_family"),
    })
}

/// Removes `//` and `/* */` comments and trailing commas, leaving string contents untouched.
pub fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // A comma directly before `}` or `]` (ignoring whitespace/comments) is trailing.
                let mut j = i + 1;
                loop {
                    while j < chars.len() && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'/') {
                        while j < chars.len() && chars[j] != '\n' {
                            j += 1;
                        }
                    } else {
                        break;
                    }
                }
                if !matches!(chars.get(j), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed copy of a real Zed settings file: a header comment block, nested objects, a URL-free
    /// string theme, and font sizes.
    const REAL: &str = r#"// documentation: https://zed.dev/docs/configuring-zed
//
// To see all of Zed's default settings...
{
  "project_panel": { "dock": "left" },
  "agent": { "default_model": { "provider": "copilot_chat", "model": "claude-3-5-sonnet" } },
  "theme": "Ayu Dark",
  "ui_font_size": 14,
  "buffer_font_size": 13,
  "vim_mode": true,
  "relative_line_numbers": "enabled"
}"#;

    #[test]
    fn reads_a_real_settings_file() {
        let s = parse_zed_settings(REAL).unwrap();
        assert_eq!(s.theme, Some(ThemeChoice::Fixed("Ayu Dark".into())));
        assert_eq!(s.ui_font_size, Some(14.0));
        assert_eq!(s.buffer_font_size, Some(13.0));
        assert_eq!(s.ui_font_family, None);
    }

    #[test]
    fn object_form_follows_the_mode() {
        let by_system = r#"{"theme": {"mode": "system", "light": "Ayu Light", "dark": "Ayu Mirage"}}"#;
        assert_eq!(
            parse_zed_settings(by_system).unwrap().theme,
            Some(ThemeChoice::BySystem { light: "Ayu Light".into(), dark: "Ayu Mirage".into() })
        );
        let fixed = r#"{"theme": {"mode": "dark", "light": "A", "dark": "B"}}"#;
        assert_eq!(parse_zed_settings(fixed).unwrap().theme, Some(ThemeChoice::Fixed("B".into())));
        // No mode means system, and missing names fall back to Zed's One pair.
        assert_eq!(
            parse_zed_settings(r#"{"theme": {}}"#).unwrap().theme,
            Some(ThemeChoice::BySystem { light: "One Light".into(), dark: "One Dark".into() })
        );
    }

    #[test]
    fn missing_theme_is_none() {
        assert_eq!(parse_zed_settings("{}").unwrap().theme, None);
    }

    #[test]
    fn comments_and_trailing_commas_are_stripped_but_strings_are_not() {
        let src = r#"{
            // line comment
            "url": "https://example.com/a//b", /* block */
            "note": "a \" // not a comment",
            "list": [1, 2, /* x */ 3,],
            "obj": { "k": "v", },
        }"#;
        let v: Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["url"], "https://example.com/a//b");
        assert_eq!(v["note"], "a \" // not a comment");
        assert_eq!(v["list"], serde_json::json!([1, 2, 3]));
        assert_eq!(v["obj"]["k"], "v");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_zed_settings("{ not json").is_err());
        assert!(parse_zed_settings("").is_err());
    }
}
