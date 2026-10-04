//! Editable Markdown source paired with an `egui_commonmark` preview.

use eframe::egui::{
    Id, ScrollArea, TextEdit, Ui, Vec2, WidgetInfo, WidgetType,
    containers::scroll_area::ScrollBarVisibility, style::ScrollStyle,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use serde::{Deserialize, Serialize};

use crate::markdown_highlighter::highlight_markdown;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorMode {
    Source,
    Preview,
    #[default]
    Split,
}

pub struct MarkdownEditor {
    document_key: String,
    markdown: String,
    mode: EditorMode,
    cache: CommonMarkCache,
}

impl MarkdownEditor {
    #[must_use]
    pub fn new(key: impl Into<String>, markdown: impl Into<String>, mode: EditorMode) -> Self {
        Self {
            document_key: key.into(),
            markdown: markdown.into(),
            mode,
            cache: CommonMarkCache::default(),
        }
    }

    pub fn set_markdown(&mut self, markdown: impl Into<String>) {
        self.markdown = markdown.into();
    }

    #[must_use]
    pub fn markdown(&self) -> &str {
        &self.markdown
    }

    #[must_use]
    pub const fn mode(&self) -> EditorMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: EditorMode) {
        self.mode = mode;
    }

    pub fn show(&mut self, ui: &mut Ui, id_salt: impl std::hash::Hash, height: f32) -> bool {
        let id = Id::new(("commonmark_editor", &self.document_key, id_salt));
        let source_label = format!("{} Markdown source", self.document_key);
        match self.mode {
            EditorMode::Source => show_source(ui, id, &source_label, &mut self.markdown, height),
            EditorMode::Preview => {
                show_preview(ui, id, &mut self.cache, &self.markdown, height);
                false
            }
            EditorMode::Split => {
                let mut changed = false;
                ui.columns(2, |columns| {
                    changed = show_source(
                        &mut columns[0],
                        id.with("source"),
                        &source_label,
                        &mut self.markdown,
                        height,
                    );
                    show_preview(
                        &mut columns[1],
                        id.with("preview"),
                        &mut self.cache,
                        &self.markdown,
                        height,
                    );
                });
                changed
            }
        }
    }
}

fn show_source(
    ui: &mut Ui,
    id: Id,
    accessibility_label: &str,
    markdown: &mut String,
    height: f32,
) -> bool {
    ui.style_mut().spacing.scroll = ScrollStyle::thin();
    let viewport_width = ui.available_width().max(1.0);
    let viewport_height = height.max(72.0).min(ui.available_height().max(1.0));
    ScrollArea::both()
        .id_salt(id.with("source-scroll"))
        .max_width(viewport_width)
        .max_height(viewport_height)
        .min_scrolled_width(viewport_width)
        .min_scrolled_height(viewport_height)
        .auto_shrink([false, false])
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            let dark_mode = ui.visuals().dark_mode;
            let mut layouter = |ui: &Ui,
                                buffer: &dyn eframe::egui::TextBuffer,
                                _wrap_width: f32| {
                let font_id = eframe::egui::TextStyle::Monospace.resolve(ui.style());
                let job = highlight_markdown(buffer.as_str(), dark_mode, &font_id, f32::INFINITY);
                ui.fonts(|fonts| fonts.layout_job(job))
            };
            let response = ui.add(
                TextEdit::multiline(markdown)
                    .id(id)
                    .code_editor()
                    .layouter(&mut layouter)
                    .desired_width(f32::INFINITY)
                    .desired_rows(1)
                    .min_size(Vec2::new(viewport_width, viewport_height))
                    .hint_text("Write Markdown…"),
            );
            response.widget_info(|| {
                WidgetInfo::labeled(WidgetType::TextEdit, true, accessibility_label)
            });
            response.changed()
        })
        .inner
}

fn show_preview(ui: &mut Ui, id: Id, cache: &mut CommonMarkCache, markdown: &str, height: f32) {
    ui.style_mut().url_in_tooltip = true;
    ui.style_mut().spacing.scroll = ScrollStyle::thin();
    let viewport_width = ui.available_width().max(1.0);
    let viewport_height = height.max(72.0).min(ui.available_height().max(1.0));
    ScrollArea::both()
        .id_salt(id)
        .max_width(viewport_width)
        .max_height(viewport_height)
        .min_scrolled_width(viewport_width)
        .min_scrolled_height(viewport_height)
        .auto_shrink([false, false])
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            // Keep ordinary prose wrapped to the dock. Unbreakable content and wide tables can
            // still exceed this width and are reachable with the horizontal scrollbar.
            ui.set_max_width((viewport_width - 16.0).max(1.0));
            // egui_commonmark uses `strong_text_color` for headings and list markers. The app's
            // selected-button color is white, which otherwise makes both disappear on the light
            // preview canvas. Keep the correction local so selected buttons remain unchanged.
            ui.scope(|ui| {
                correct_commonmark_strong_text_color(ui);
                CommonMarkViewer::new().show(ui, cache, preview_body(markdown));
            });
        });
}

fn correct_commonmark_strong_text_color(ui: &mut Ui) {
    let preview_text = ui.visuals().text_color();
    ui.visuals_mut().widgets.active.fg_stroke.color = preview_text;
}

fn preview_body(markdown: &str) -> &str {
    let mut lines = markdown.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return markdown;
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return markdown;
    }

    let mut consumed = first.len();
    for line in lines {
        consumed += line.len();
        if matches!(line.trim_end_matches(['\r', '\n']), "---" | "...") {
            return &markdown[consumed..];
        }
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_preserves_markdown_while_switching_modes() {
        let mut editor = MarkdownEditor::new("test", "# Heading\n\n- item", EditorMode::Source);
        editor.set_mode(EditorMode::Preview);
        editor.set_mode(EditorMode::Split);

        assert_eq!(editor.markdown(), "# Heading\n\n- item");
        assert_eq!(editor.mode(), EditorMode::Split);
    }

    #[test]
    fn preview_hides_yaml_frontmatter_without_losing_headings_or_lists() {
        let source = "---\r\ntitle: 目的\r\ntags:\r\n- project\r\n---\r\n# 目的 (Purpose)\r\n\r\n1. Preserve sources.\r\n2. Build the wiki.\r\n";

        let body = preview_body(source);

        assert!(!body.contains("title: 目的"));
        assert!(body.starts_with("# 目的 (Purpose)"));
        assert!(body.contains("1. Preserve sources."));
        assert!(body.contains("2. Build the wiki."));
    }

    #[test]
    fn incomplete_frontmatter_is_not_silently_removed() {
        let source = "---\ntitle: Draft\n# Still metadata";

        assert_eq!(preview_body(source), source);
    }

    #[test]
    fn preview_strong_text_uses_readable_body_color() {
        eframe::egui::__run_test_ui(|ui| {
            ui.visuals_mut().override_text_color = Some(eframe::egui::Color32::BLACK);
            ui.visuals_mut().widgets.active.fg_stroke.color = eframe::egui::Color32::WHITE;

            correct_commonmark_strong_text_color(ui);

            assert_eq!(
                ui.visuals().strong_text_color(),
                eframe::egui::Color32::BLACK
            );
        });
    }

    #[test]
    fn markdown_source_scrolls_horizontally_and_vertically_inside_its_viewport() {
        use egui_kittest::{Harness, kittest::Queryable};

        let long_line = "wide Markdown source ".repeat(80);
        let source = (0..80)
            .map(|line| format!("{line:02}: {long_line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let editor = MarkdownEditor::new("scroll-test", source, EditorMode::Source);
        let mut harness = Harness::builder()
            .with_size(Vec2::new(480.0, 260.0))
            .build_ui_state(
                |ui, editor: &mut MarkdownEditor| {
                    let _changed = editor.show(ui, "fixed-dock", 220.0);
                },
                editor,
            );
        harness.run();

        let initial = harness.get_by_label("scroll-test Markdown source").rect();
        harness
            .get_by_label("scroll-test Markdown source")
            .scroll_left();
        harness.run();
        let after_horizontal = harness.get_by_label("scroll-test Markdown source").rect();
        assert!(
            after_horizontal.left() < initial.left(),
            "horizontal scrolling must move wide source content inside the viewport"
        );

        harness
            .get_by_label("scroll-test Markdown source")
            .scroll_down();
        harness.run();
        let after_vertical = harness.get_by_label("scroll-test Markdown source").rect();
        assert!(
            after_vertical.top() < after_horizontal.top(),
            "vertical scrolling must move tall source content inside the viewport"
        );
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_MARKDOWN_QA_CAPTURE"]
    fn capture_markdown_source_and_preview_for_visual_qa() {
        use std::fmt::Write as _;
        use std::path::PathBuf;

        use egui_kittest::Harness;

        let mut source = "---\ntitle: Purpose\ntags:\n- project\n---\n# Purpose\n\n## Operating Loop\n\n1. Preserve every input source.\n2. Build linked wiki pages.\n3. Keep evidence traceable.\n".to_owned();
        source.push_str("\n`wide-content:");
        source.push_str(&"0123456789".repeat(90));
        source.push_str("`\n");
        for line in 0..60 {
            writeln!(source, "\n{line}. Additional scrollable Markdown evidence.")
                .expect("writing to a String cannot fail");
        }
        let editor = MarkdownEditor::new("visual-qa", source, EditorMode::Split);
        let mut harness = Harness::builder()
            .with_size(Vec2::new(900.0, 520.0))
            .with_theme(eframe::egui::Theme::Light)
            .build_ui_state(
                |ui, editor: &mut MarkdownEditor| {
                    ui.visuals_mut().override_text_color = Some(eframe::egui::Color32::BLACK);
                    ui.visuals_mut().widgets.active.fg_stroke.color = eframe::egui::Color32::WHITE;
                    let _changed = editor.show(ui, "visual-qa", 480.0);
                },
                editor,
            );
        harness.run();

        let output = std::env::var_os("BIBIIWIKI_MARKDOWN_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_MARKDOWN_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui Markdown visual-QA render")
            .save(&output)
            .expect("save egui Markdown visual-QA render");
        assert!(output.is_file());
    }
}
