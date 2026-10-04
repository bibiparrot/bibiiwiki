use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use eframe::egui::Color32;
use serde::{Deserialize, Serialize};

const DEFAULT_COLOR_THEME_YAML: &str = r##"version: 1
light:
  search_result:
    background: "#F7F9FD"
    background_hovered: "#EAF2FF"
    background_selected: "#D9E9FF"
    border: "#C9D4E5"
    title: "#172033"
    title_selected: "#0B3A75"
    path: "#2463C7"
    excerpt: "#4F5F76"
    score: "#667085"
dark:
  search_result:
    background: "#171D29"
    background_hovered: "#202A3A"
    background_selected: "#173B65"
    border: "#34445D"
    title: "#E8EEF9"
    title_selected: "#FFFFFF"
    path: "#76AEFF"
    excerpt: "#BAC6D8"
    score: "#9EACC2"
"##;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchResultPalette {
    pub background: Color32,
    pub background_hovered: Color32,
    pub background_selected: Color32,
    pub border: Color32,
    pub title: Color32,
    pub title_selected: Color32,
    pub path: Color32,
    pub excerpt: Color32,
    pub score: Color32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ColorTheme {
    light: SearchResultPalette,
    dark: SearchResultPalette,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ColorThemeFile {
    version: u32,
    light: ThemeSection,
    dark: ThemeSection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ThemeSection {
    search_result: SearchResultColors,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchResultColors {
    background: String,
    background_hovered: String,
    background_selected: String,
    border: String,
    title: String,
    title_selected: String,
    path: String,
    excerpt: String,
    score: String,
}

impl ColorTheme {
    pub(crate) fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create color-theme directory {}",
                        parent.display()
                    )
                })?;
            }
            fs::write(path, DEFAULT_COLOR_THEME_YAML)
                .with_context(|| format!("failed to create color theme {}", path.display()))?;
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read color theme {}", path.display()))?;
        Self::parse(&source).with_context(|| format!("invalid color theme in {}", path.display()))
    }

    pub(crate) fn parse(source: &str) -> Result<Self> {
        let file: ColorThemeFile =
            serde_yaml::from_str(source).context("invalid color-theme YAML")?;
        if file.version != 1 {
            bail!(
                "unsupported color-theme version {}; expected 1",
                file.version
            );
        }
        Ok(Self {
            light: parse_search_result(&file.light.search_result)?,
            dark: parse_search_result(&file.dark.search_result)?,
        })
    }

    pub(crate) const fn search_result(self, dark: bool) -> SearchResultPalette {
        if dark { self.dark } else { self.light }
    }
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self::parse(DEFAULT_COLOR_THEME_YAML).expect("embedded color theme is valid")
    }
}

fn parse_search_result(colors: &SearchResultColors) -> Result<SearchResultPalette> {
    Ok(SearchResultPalette {
        background: parse_hex(&colors.background)?,
        background_hovered: parse_hex(&colors.background_hovered)?,
        background_selected: parse_hex(&colors.background_selected)?,
        border: parse_hex(&colors.border)?,
        title: parse_hex(&colors.title)?,
        title_selected: parse_hex(&colors.title_selected)?,
        path: parse_hex(&colors.path)?,
        excerpt: parse_hex(&colors.excerpt)?,
        score: parse_hex(&colors.score)?,
    })
}

fn parse_hex(source: &str) -> Result<Color32> {
    let value = source
        .strip_prefix('#')
        .filter(|value| value.len() == 6)
        .with_context(|| format!("color {source:?} must use #RRGGBB"))?;
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&value[range], 16)
            .with_context(|| format!("color {source:?} contains invalid hexadecimal digits"))
    };
    Ok(Color32::from_rgb(
        channel(0..2)?,
        channel(2..4)?,
        channel(4..6)?,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn default_light_and_dark_search_tiles_have_readable_contrast() {
        let theme = ColorTheme::default();
        for palette in [theme.light, theme.dark] {
            assert!(contrast_ratio(palette.title, palette.background) >= 4.5);
            assert!(contrast_ratio(palette.excerpt, palette.background) >= 4.5);
            assert!(contrast_ratio(palette.title_selected, palette.background_selected) >= 4.5);
        }
        assert_ne!(theme.light.background, theme.dark.background);
    }

    #[test]
    fn malformed_or_incomplete_theme_is_rejected() {
        assert!(ColorTheme::parse("version: 1\nlight: {}\ndark: {}\n").is_err());
        assert!(ColorTheme::parse(&DEFAULT_COLOR_THEME_YAML.replace("#172033", "white")).is_err());
    }

    #[test]
    fn missing_theme_file_is_created_with_both_schemes() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("color-theme-{unique}"));
        let path = root.join("color_theme.yaml");

        let loaded = ColorTheme::load_or_create(&path).expect("create color theme");
        let source = fs::read_to_string(&path).expect("saved color theme");

        assert_eq!(loaded, ColorTheme::default());
        assert!(source.contains("light:"));
        assert!(source.contains("dark:"));
        assert!(source.contains("background_selected:"));
        fs::remove_dir_all(root).expect("color-theme cleanup");
    }

    fn contrast_ratio(foreground: Color32, background: Color32) -> f32 {
        let luminance = |color: Color32| {
            let channel = |value: u8| {
                let value = f32::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
        };
        let left = luminance(foreground);
        let right = luminance(background);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }
}
