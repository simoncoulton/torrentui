//! Colour themes: a small semantic palette, built-in presets, and user-defined
//! themes from the config file.

use std::{collections::BTreeMap, str::FromStr};

use color_eyre::eyre::{Result, eyre};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// Semantic colours used by the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub fg: Color,
    /// Secondary text: hints, labels, empty cells.
    pub muted: Color,
    /// Titles, key hints, focused borders.
    pub accent: Color,
    pub border: Color,
    /// Background of the selected row. `Color::Reset` falls back to reverse video.
    pub selection_bg: Color,
    /// Downloading, download speed.
    pub success: Color,
    /// Seeding, upload speed.
    pub info: Color,
    /// Paused, pending work.
    pub warning: Color,
    pub error: Color,
    /// Checking / initializing.
    pub special: Color,
}

impl Theme {
    pub fn base(&self) -> Style {
        Style::new().fg(self.fg)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn selection(&self) -> Style {
        if self.selection_bg == Color::Reset {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
                .bg(self.selection_bg)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// The six accent colours, handy for swatches.
    pub fn swatch(&self) -> [Color; 6] {
        [
            self.accent,
            self.success,
            self.info,
            self.warning,
            self.error,
            self.special,
        ]
    }
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `c` is: fg, muted, accent, border, selection_bg, success, info,
/// warning, error, special.
fn preset(name: &str, c: [u32; 10]) -> Theme {
    Theme {
        name: name.to_string(),
        fg: rgb(c[0]),
        muted: rgb(c[1]),
        accent: rgb(c[2]),
        border: rgb(c[3]),
        selection_bg: rgb(c[4]),
        success: rgb(c[5]),
        info: rgb(c[6]),
        warning: rgb(c[7]),
        error: rgb(c[8]),
        special: rgb(c[9]),
    }
}

/// Plain ANSI colours: works everywhere and follows the terminal's palette.
pub fn default_theme() -> Theme {
    Theme {
        name: "default".into(),
        fg: Color::Reset,
        muted: Color::DarkGray,
        accent: Color::Cyan,
        border: Color::Reset,
        selection_bg: Color::Reset,
        success: Color::Green,
        info: Color::Blue,
        warning: Color::Yellow,
        error: Color::Red,
        special: Color::Magenta,
    }
}

/// All built-in themes; `default` first. The RGB presets need a true-colour
/// terminal.
pub fn builtin_themes() -> Vec<Theme> {
    vec![
        default_theme(),
        preset(
            "tokyo-night",
            [
                0xc0caf5, 0x565f89, 0x7aa2f7, 0x3b4261, 0x283457, 0x9ece6a, 0x7dcfff, 0xe0af68,
                0xf7768e, 0xbb9af7,
            ],
        ),
        preset(
            "catppuccin-frappe",
            [
                0xc6d0f5, 0x737994, 0x8caaee, 0x51576d, 0x51576d, 0xa6d189, 0x85c1dc, 0xe5c890,
                0xe78284, 0xca9ee6,
            ],
        ),
        preset(
            "catppuccin-mocha",
            [
                0xcdd6f4, 0x6c7086, 0x89b4fa, 0x45475a, 0x45475a, 0xa6e3a1, 0x74c7ec, 0xf9e2af,
                0xf38ba8, 0xcba6f7,
            ],
        ),
        preset(
            "dracula",
            [
                0xf8f8f2, 0x6272a4, 0xbd93f9, 0x44475a, 0x44475a, 0x50fa7b, 0x8be9fd, 0xf1fa8c,
                0xff5555, 0xff79c6,
            ],
        ),
        preset(
            "one-dark",
            [
                0xabb2bf, 0x5c6370, 0x61afef, 0x4b5263, 0x3e4451, 0x98c379, 0x56b6c2, 0xe5c07b,
                0xe06c75, 0xc678dd,
            ],
        ),
    ]
}

/// Lower-case, accent-free, punctuation-free form used to match theme names, so
/// `Catppuccin Frappé`, `catppuccin_frappe` and `catpuccin-frappe` all agree.
pub fn normalize(name: &str) -> String {
    let s: String = name
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'é' { 'e' } else { c })
        .filter(char::is_ascii_alphanumeric)
        .collect();
    // A very common misspelling.
    match s.strip_prefix("catpuccin") {
        Some(rest) => format!("catppuccin{rest}"),
        None => s,
    }
}

/// A user-defined theme from the config file. Every colour is optional and
/// falls back to `base` (a built-in theme, default `default`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSpec {
    pub base: Option<String>,
    /// Ignored: themes never set a background so terminal transparency works.
    #[allow(dead_code)]
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub muted: Option<String>,
    pub accent: Option<String>,
    pub border: Option<String>,
    pub selection_bg: Option<String>,
    pub success: Option<String>,
    pub info: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub special: Option<String>,
}

fn color(theme: &str, field: &str, value: &Option<String>, fallback: Color) -> Result<Color> {
    match value {
        None => Ok(fallback),
        Some(v) => Color::from_str(v.trim()).map_err(|_| {
            eyre!(
                "theme '{theme}': invalid colour '{v}' for '{field}' \
                 (use #rrggbb, a name like 'red', or a 0-255 index)"
            )
        }),
    }
}

impl ThemeSpec {
    fn resolve(&self, name: &str, builtins: &[Theme]) -> Result<Theme> {
        let base = match &self.base {
            None => default_theme(),
            Some(b) => find(builtins, b)
                .cloned()
                .ok_or_else(|| eyre!("theme '{name}': unknown base theme '{b}'"))?,
        };
        Ok(Theme {
            name: name.to_string(),
            fg: color(name, "fg", &self.fg, base.fg)?,
            muted: color(name, "muted", &self.muted, base.muted)?,
            accent: color(name, "accent", &self.accent, base.accent)?,
            border: color(name, "border", &self.border, base.border)?,
            selection_bg: color(name, "selection_bg", &self.selection_bg, base.selection_bg)?,
            success: color(name, "success", &self.success, base.success)?,
            info: color(name, "info", &self.info, base.info)?,
            warning: color(name, "warning", &self.warning, base.warning)?,
            error: color(name, "error", &self.error, base.error)?,
            special: color(name, "special", &self.special, base.special)?,
        })
    }
}

/// Built-in themes plus the user's. A custom theme with a built-in's name
/// replaces it.
pub fn all_themes(custom: &BTreeMap<String, ThemeSpec>) -> Result<Vec<Theme>> {
    let builtins = builtin_themes();
    let mut all = builtins.clone();
    for (name, spec) in custom {
        let theme = spec.resolve(name, &builtins)?;
        match all
            .iter()
            .position(|t| normalize(&t.name) == normalize(name))
        {
            Some(i) => all[i] = theme,
            None => all.push(theme),
        }
    }
    Ok(all)
}

pub fn find<'a>(themes: &'a [Theme], name: &str) -> Option<&'a Theme> {
    let wanted = normalize(name);
    themes.iter().find(|t| normalize(&t.name) == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_builtins_exist() {
        let all = builtin_themes();
        for n in [
            "Tokyo Night",
            "Catppuccin Frappé",
            "Catpuccin Mocha",
            "dracula",
            "One_Dark",
            "default",
        ] {
            assert!(find(&all, n).is_some(), "missing {n}");
        }
        assert!(find(&all, "nope").is_none());
    }

    #[test]
    fn names_are_unique_after_normalizing() {
        let all = builtin_themes();
        let mut names: Vec<_> = all.iter().map(|t| normalize(&t.name)).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn spot_check_palettes() {
        let all = builtin_themes();
        let t = find(&all, "tokyo-night").unwrap();
        assert_eq!(t.fg, Color::Rgb(0xc0, 0xca, 0xf5));
        let m = find(&all, "catppuccin-mocha").unwrap();
        assert_eq!(m.error, Color::Rgb(0xf3, 0x8b, 0xa8));
        let d = find(&all, "dracula").unwrap();
        assert_eq!(d.accent, Color::Rgb(0xbd, 0x93, 0xf9));
    }

    #[test]
    fn custom_theme_inherits_from_base() {
        let mut custom = BTreeMap::new();
        custom.insert(
            "mine".to_string(),
            ThemeSpec {
                base: Some("dracula".into()),
                accent: Some("#ff0000".into()),
                error: Some("magenta".into()),
                ..Default::default()
            },
        );
        let all = all_themes(&custom).unwrap();
        let t = find(&all, "mine").unwrap();
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.error, Color::Magenta);
        assert_eq!(t.fg, Color::Rgb(0xf8, 0xf8, 0xf2)); // from dracula
    }

    #[test]
    fn custom_theme_can_replace_builtin() {
        let mut custom = BTreeMap::new();
        custom.insert(
            "Dracula".to_string(),
            ThemeSpec {
                accent: Some("#00ff00".into()),
                ..Default::default()
            },
        );
        let all = all_themes(&custom).unwrap();
        assert_eq!(all.len(), builtin_themes().len());
        assert_eq!(find(&all, "dracula").unwrap().accent, Color::Rgb(0, 255, 0));
    }

    #[test]
    fn bad_colours_and_bases_are_errors() {
        let mut custom = BTreeMap::new();
        custom.insert(
            "x".to_string(),
            ThemeSpec {
                accent: Some("not-a-colour".into()),
                ..Default::default()
            },
        );
        let err = all_themes(&custom).unwrap_err().to_string();
        assert!(
            err.contains("accent") && err.contains("not-a-colour"),
            "{err}"
        );

        let mut custom = BTreeMap::new();
        custom.insert(
            "y".to_string(),
            ThemeSpec {
                base: Some("zzz".into()),
                ..Default::default()
            },
        );
        assert!(all_themes(&custom).unwrap_err().to_string().contains("zzz"));
    }
}
