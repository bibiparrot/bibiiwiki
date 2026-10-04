rust_i18n::i18n!("locales", fallback = "en");

pub mod backend;
pub mod codex;
mod color_theme;
pub mod commonmark_editor;
pub mod config;
pub mod gateway;
pub mod i18n;
pub mod icons;
mod jinja_editor;
pub mod llm_preflight;
mod markdown_highlighter;
mod output_terminal;
pub mod protocol;
mod toml_editor;
pub mod ui;
pub mod wiki;
mod yaml_editor;
