use ansi_to_tui::IntoText as _;
use eframe::egui::scroll_area::ScrollBarVisibility;
use eframe::egui::text::LayoutJob;
use eframe::egui::{
    Color32, FontFamily, FontId, Frame, Label, Response, ScrollArea, Stroke, TextFormat,
    TextWrapMode, Ui, WidgetInfo, WidgetType,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

const TERMINAL_FONT_SIZE: f32 = 14.5;
const TERMINAL_LINE_HEIGHT: f32 = 19.0;

/// Native, read-only process output with ANSI colors.
///
/// A native egui label keeps every glyph selectable and copyable. The former
/// `egui_ratatui` backend painted a terminal bitmap, which looked like text but
/// could not participate in egui's text selection or clipboard handling.
#[derive(Default)]
pub(crate) struct OutputTerminal;

impl OutputTerminal {
    pub(crate) fn show(ui: &mut Ui, output: &str, dark_theme: bool) -> Response {
        let layout = terminal_layout_job(output, dark_theme);
        let background = if dark_theme {
            Color32::from_rgb(13, 20, 32)
        } else {
            Color32::from_rgb(248, 250, 252)
        };
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        let output_fingerprint = hasher.finish();

        Frame::new()
            .fill(background)
            .inner_margin(6.0)
            .show(ui, |ui| {
                ScrollArea::both()
                    .id_salt(("selectable_job_output_scroll", output_fingerprint))
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        let response = ui
                            .add(
                                Label::new(layout)
                                    .selectable(true)
                                    .wrap_mode(TextWrapMode::Extend),
                            )
                            .on_hover_text("Drag to select output; press Ctrl+C to copy");
                        response.widget_info(|| {
                            WidgetInfo::labeled(
                                WidgetType::Label,
                                true,
                                "Selectable colorized job output",
                            )
                        });
                        response
                    })
                    .inner
            })
            .inner
    }
}

fn terminal_layout_job(output: &str, dark_theme: bool) -> LayoutJob {
    let text = terminal_text(output, dark_theme);
    let default_style = Style::default().fg(if dark_theme {
        Color::Rgb(220, 228, 240)
    } else {
        Color::Rgb(30, 41, 59)
    });
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    for (line_index, line) in text.lines.iter().enumerate() {
        for span in &line.spans {
            job.append(
                span.content.as_ref(),
                0.0,
                terminal_text_format(default_style.patch(span.style), dark_theme),
            );
        }
        if line_index + 1 < text.lines.len() {
            job.append("\n", 0.0, terminal_text_format(default_style, dark_theme));
        }
    }
    job
}

fn terminal_text_format(style: Style, dark_theme: bool) -> TextFormat {
    let mut color = terminal_color32(style.fg.unwrap_or(Color::Reset), dark_theme, false);
    if style.add_modifier.contains(Modifier::DIM) {
        color = color.gamma_multiply(0.72);
    }
    let underline = if style.add_modifier.contains(Modifier::UNDERLINED) {
        Stroke::new(1.0_f32, color)
    } else {
        Stroke::NONE
    };
    let strikethrough = if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        Stroke::new(1.0_f32, color)
    } else {
        Stroke::NONE
    };

    TextFormat {
        font_id: FontId::new(TERMINAL_FONT_SIZE, FontFamily::Monospace),
        extra_letter_spacing: 0.1,
        line_height: Some(TERMINAL_LINE_HEIGHT),
        color,
        background: style.bg.map_or(Color32::TRANSPARENT, |background| {
            terminal_color32(background, dark_theme, true)
        }),
        italics: style.add_modifier.contains(Modifier::ITALIC),
        underline,
        strikethrough,
        ..TextFormat::default()
    }
}

fn terminal_text(output: &str, dark_theme: bool) -> Text<'static> {
    let mut text = output
        .as_bytes()
        .into_text()
        .unwrap_or_else(|_| Text::raw(strip_terminal_controls(output)));
    for line in &mut text.lines {
        for span in &mut line.spans {
            span.style.fg = span
                .style
                .fg
                .map(|color| readable_terminal_color(color, dark_theme));
            span.style.bg = span
                .style
                .bg
                .map(|color| readable_terminal_background(color, dark_theme));
        }
    }
    text
}

fn terminal_color32(color: Color, dark_theme: bool, background: bool) -> Color32 {
    let color = if background {
        readable_terminal_background(color, dark_theme)
    } else {
        readable_terminal_color(color, dark_theme)
    };
    match color {
        Color::Reset => {
            if dark_theme {
                Color32::from_rgb(220, 228, 240)
            } else {
                Color32::from_rgb(30, 41, 59)
            }
        }
        Color::Black => Color32::BLACK,
        Color::Red => Color32::from_rgb(128, 0, 0),
        Color::Green => Color32::from_rgb(0, 128, 0),
        Color::Yellow => Color32::from_rgb(128, 128, 0),
        Color::Blue => Color32::from_rgb(0, 0, 128),
        Color::Magenta => Color32::from_rgb(128, 0, 128),
        Color::Cyan => Color32::from_rgb(0, 128, 128),
        Color::Gray => Color32::from_rgb(192, 192, 192),
        Color::DarkGray => Color32::from_rgb(128, 128, 128),
        Color::LightRed => Color32::from_rgb(255, 0, 0),
        Color::LightGreen => Color32::from_rgb(0, 255, 0),
        Color::LightYellow => Color32::from_rgb(255, 255, 0),
        Color::LightBlue => Color32::from_rgb(0, 0, 255),
        Color::LightMagenta => Color32::from_rgb(255, 0, 255),
        Color::LightCyan => Color32::from_rgb(0, 255, 255),
        Color::White => Color32::WHITE,
        Color::Indexed(index) => {
            let Color::Rgb(red, green, blue) = indexed_terminal_color(index) else {
                unreachable!("indexed terminal colors always resolve to RGB")
            };
            Color32::from_rgb(red, green, blue)
        }
        Color::Rgb(red, green, blue) => Color32::from_rgb(red, green, blue),
    }
}

const fn readable_terminal_color(color: Color, dark_theme: bool) -> Color {
    match (color, dark_theme) {
        (Color::Reset, false) => Color::Rgb(30, 41, 59),
        (Color::Reset, true) => Color::Rgb(220, 228, 240),
        (Color::Black, false) => Color::Rgb(15, 23, 42),
        (Color::White, false) => Color::Rgb(51, 65, 85),
        (Color::Red | Color::LightRed, false) => Color::Rgb(190, 24, 93),
        (Color::Green | Color::LightGreen, false) => Color::Rgb(21, 128, 61),
        (Color::Yellow | Color::LightYellow, false) => Color::Rgb(180, 83, 9),
        (Color::Blue | Color::LightBlue, false) => Color::Rgb(37, 99, 235),
        (Color::Magenta | Color::LightMagenta, false) => Color::Rgb(147, 51, 234),
        (Color::Cyan | Color::LightCyan, false) => Color::Rgb(8, 145, 178),
        (Color::Gray | Color::DarkGray, false) => Color::Rgb(71, 85, 105),
        (Color::Black | Color::DarkGray, true) => Color::Rgb(148, 163, 184),
        (Color::Red | Color::LightRed, true) => Color::Rgb(251, 113, 133),
        (Color::Green | Color::LightGreen, true) => Color::Rgb(74, 222, 128),
        (Color::Yellow | Color::LightYellow, true) => Color::Rgb(250, 204, 21),
        (Color::Blue | Color::LightBlue, true) => Color::Rgb(96, 165, 250),
        (Color::Magenta | Color::LightMagenta, true) => Color::Rgb(216, 180, 254),
        (Color::Cyan | Color::LightCyan, true) => Color::Rgb(103, 232, 249),
        (Color::Gray | Color::White, true) => Color::Rgb(226, 232, 240),
        (Color::Indexed(index), _) => indexed_terminal_color(index),
        (color, _) => color,
    }
}

const fn readable_terminal_background(color: Color, dark_theme: bool) -> Color {
    match (color, dark_theme) {
        (Color::Reset, false) => Color::Rgb(248, 250, 252),
        (Color::Reset, true) => Color::Rgb(13, 20, 32),
        (Color::Black | Color::DarkGray, false) => Color::Rgb(226, 232, 240),
        (Color::White | Color::Gray, true) => Color::Rgb(51, 65, 85),
        (Color::Indexed(index), _) => indexed_terminal_color(index),
        (color, _) => color,
    }
}

const fn indexed_terminal_color(index: u8) -> Color {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if index < 16 {
        let (red, green, blue) = ANSI[index as usize];
        return Color::Rgb(red, green, blue);
    }
    if index < 232 {
        let value = index - 16;
        let red = value / 36;
        let green = (value % 36) / 6;
        let blue = value % 6;
        return Color::Rgb(color_cube(red), color_cube(green), color_cube(blue));
    }
    let gray = 8 + (index - 232) * 10;
    Color::Rgb(gray, gray, gray)
}

const fn color_cube(component: u8) -> u8 {
    if component == 0 {
        0
    } else {
        55 + component * 40
    }
}

fn strip_terminal_controls(output: &str) -> String {
    let mut clean = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.next_if_eq(&'[').is_some() {
                for sequence_character in characters.by_ref() {
                    if ('@'..='~').contains(&sequence_character) {
                        break;
                    }
                }
            } else {
                let _ = characters.next();
            }
        } else if !character.is_control() || matches!(character, '\n' | '\r' | '\t') {
            clean.push(character);
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_warning_and_error_colors_are_parsed_without_escape_glyphs() {
        let text = terminal_text("\u{1b}[33mWARN\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m", false);
        let spans = &text.lines[0].spans;

        assert_eq!(spans[0].content, "WARN");
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(180, 83, 9)));
        assert_eq!(spans[2].content, "ERROR");
        assert_eq!(spans[2].style.fg, Some(Color::Rgb(190, 24, 93)));
        assert!(spans.iter().all(|span| !span.content.contains('\u{1b}')));
    }

    #[test]
    fn native_layout_preserves_clear_selectable_unicode_and_ansi_colors() {
        let layout = terminal_layout_job(
            "\u{1b}[33mWARN\u{1b}[0m 中文路径\n\u{1b}[31mERROR\u{1b}[0m failure",
            false,
        );

        assert_eq!(layout.text, "WARN 中文路径\nERROR failure");
        assert!(layout.sections.len() >= 4);
        assert!(
            layout
                .sections
                .iter()
                .all(|section| section.format.font_id.family == FontFamily::Monospace)
        );
        assert!(
            layout
                .sections
                .iter()
                .any(|section| section.format.color == Color32::from_rgb(180, 83, 9))
        );
        assert!(
            layout
                .sections
                .iter()
                .any(|section| section.format.color == Color32::from_rgb(190, 24, 93))
        );
    }

    #[test]
    fn malformed_terminal_sequences_degrade_to_readable_plain_text() {
        let clean = strip_terminal_controls("before\u{1b}[broken\nafter\u{7}");

        assert!(clean.starts_with("before"));
        assert!(clean.ends_with("after"));
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{7}'));
    }

    #[test]
    fn indexed_ansi_colors_use_the_standard_xterm_palette() {
        assert_eq!(indexed_terminal_color(196), Color::Rgb(255, 0, 0));
        assert_eq!(indexed_terminal_color(244), Color::Rgb(128, 128, 128));
    }
}
