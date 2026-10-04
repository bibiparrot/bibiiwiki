use std::ops::Range;

use eframe::egui::{Color32, FontId, Stroke, TextFormat, text::LayoutJob};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JinjaToken {
    Plain,
    MarkdownHeading,
    MarkdownFence,
    Delimiter,
    Keyword,
    Identifier,
    Filter,
    String,
    Number,
    Operator,
    Comment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenSpan {
    range: Range<usize>,
    token: JinjaToken,
}

#[derive(Clone, Copy)]
struct JinjaColors {
    plain: Color32,
    markdown_heading: Color32,
    markdown_fence: Color32,
    delimiter: Color32,
    delimiter_background: Color32,
    keyword: Color32,
    identifier: Color32,
    filter: Color32,
    string: Color32,
    number: Color32,
    operator: Color32,
    comment: Color32,
}

impl JinjaColors {
    const fn light() -> Self {
        Self {
            plain: Color32::from_rgb(30, 41, 59),
            markdown_heading: Color32::from_rgb(0, 61, 165),
            markdown_fence: Color32::from_rgb(3, 105, 161),
            delimiter: Color32::from_rgb(190, 24, 93),
            delimiter_background: Color32::from_rgb(252, 231, 243),
            keyword: Color32::from_rgb(126, 34, 206),
            identifier: Color32::from_rgb(2, 101, 135),
            filter: Color32::from_rgb(180, 83, 9),
            string: Color32::from_rgb(153, 76, 0),
            number: Color32::from_rgb(109, 40, 217),
            operator: Color32::from_rgb(190, 24, 93),
            comment: Color32::from_rgb(54, 119, 82),
        }
    }

    const fn dark() -> Self {
        Self {
            plain: Color32::from_rgb(226, 232, 240),
            markdown_heading: Color32::from_rgb(125, 211, 252),
            markdown_fence: Color32::from_rgb(103, 232, 249),
            delimiter: Color32::from_rgb(244, 114, 182),
            delimiter_background: Color32::from_rgb(80, 32, 59),
            keyword: Color32::from_rgb(216, 180, 254),
            identifier: Color32::from_rgb(103, 232, 249),
            filter: Color32::from_rgb(253, 186, 116),
            string: Color32::from_rgb(253, 186, 116),
            number: Color32::from_rgb(196, 181, 253),
            operator: Color32::from_rgb(244, 114, 182),
            comment: Color32::from_rgb(134, 239, 172),
        }
    }

    const fn color(self, token: JinjaToken) -> Color32 {
        match token {
            JinjaToken::Plain => self.plain,
            JinjaToken::MarkdownHeading => self.markdown_heading,
            JinjaToken::MarkdownFence => self.markdown_fence,
            JinjaToken::Delimiter => self.delimiter,
            JinjaToken::Keyword => self.keyword,
            JinjaToken::Identifier => self.identifier,
            JinjaToken::Filter => self.filter,
            JinjaToken::String => self.string,
            JinjaToken::Number => self.number,
            JinjaToken::Operator => self.operator,
            JinjaToken::Comment => self.comment,
        }
    }
}

pub(crate) fn highlight_jinja(
    source: &str,
    dark_mode: bool,
    font_id: &FontId,
    wrap_width: f32,
) -> LayoutJob {
    let colors = if dark_mode {
        JinjaColors::dark()
    } else {
        JinjaColors::light()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for span in tokenize_jinja(source) {
        let mut format = TextFormat {
            font_id: font_id.clone(),
            color: colors.color(span.token),
            ..Default::default()
        };
        format.italics = span.token == JinjaToken::Comment;
        if span.token == JinjaToken::Filter {
            format.underline = Stroke::new(1.0_f32, colors.filter);
        }
        if span.token == JinjaToken::Delimiter {
            format.background = colors.delimiter_background;
        }
        if span.token == JinjaToken::MarkdownHeading {
            format.extra_letter_spacing = 0.2;
        }
        job.append(&source[span.range], 0.0, format);
    }
    job
}

fn tokenize_jinja(source: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    while let Some((tag_start, open, close)) = find_next_tag(source, cursor) {
        tokenize_markdown(source, cursor, tag_start, &mut spans);
        let body_start = tag_start + open.len();
        push_span(&mut spans, tag_start, body_start, JinjaToken::Delimiter);
        if open == "{#" {
            if let Some(relative_end) = source[body_start..].find(close) {
                let body_end = body_start + relative_end;
                push_span(&mut spans, body_start, body_end, JinjaToken::Comment);
                push_span(
                    &mut spans,
                    body_end,
                    body_end + close.len(),
                    JinjaToken::Delimiter,
                );
                cursor = body_end + close.len();
            } else {
                push_span(&mut spans, body_start, source.len(), JinjaToken::Comment);
                cursor = source.len();
            }
        } else if let Some(relative_end) = find_unquoted_close(&source[body_start..], close) {
            let body_end = body_start + relative_end;
            tokenize_code(source, body_start, body_end, &mut spans);
            push_span(
                &mut spans,
                body_end,
                body_end + close.len(),
                JinjaToken::Delimiter,
            );
            cursor = body_end + close.len();
        } else {
            tokenize_code(source, body_start, source.len(), &mut spans);
            cursor = source.len();
        }
    }
    tokenize_markdown(source, cursor, source.len(), &mut spans);
    spans
}

fn find_next_tag(source: &str, start: usize) -> Option<(usize, &'static str, &'static str)> {
    [("{{", "}}"), ("{%", "%}"), ("{#", "#}")]
        .into_iter()
        .filter_map(|(open, close)| {
            source[start..]
                .find(open)
                .map(|offset| (start + offset, open, close))
        })
        .min_by_key(|(offset, _, _)| *offset)
}

fn find_unquoted_close(source: &str, close: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let close_bytes = close.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if quote == Some(b'"') && byte == b'\\' && !escaped {
            escaped = true;
            cursor += 1;
            continue;
        }
        if !escaped && matches!(byte, b'"' | b'\'') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
        } else if quote.is_none() && bytes[cursor..].starts_with(close_bytes) {
            return Some(cursor);
        }
        escaped = false;
        cursor += source[cursor..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn tokenize_markdown(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let mut line_start = start;
    for line in source[start..end].split_inclusive('\n') {
        let line_end = line_start + line.len();
        let content_end = line
            .strip_suffix('\n')
            .map_or(line_end, |content| line_start + content.len());
        let trimmed_start = line_start
            + source[line_start..content_end]
                .bytes()
                .take_while(u8::is_ascii_whitespace)
                .count();
        let trimmed = &source[trimmed_start..content_end];
        let token = if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            JinjaToken::MarkdownFence
        } else if is_markdown_heading(trimmed) {
            JinjaToken::MarkdownHeading
        } else {
            JinjaToken::Plain
        };
        push_span(spans, line_start, content_end, token);
        push_span(spans, content_end, line_end, JinjaToken::Plain);
        line_start = line_end;
    }
}

fn is_markdown_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&hashes)
        && line
            .as_bytes()
            .get(hashes)
            .is_some_and(u8::is_ascii_whitespace)
}

fn tokenize_code(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let mut cursor = start;
    let mut expect_filter = false;
    while cursor < end {
        let character = source[cursor..]
            .chars()
            .next()
            .expect("valid UTF-8 boundary");
        if character.is_whitespace() {
            let token_start = cursor;
            while cursor < end {
                let next = source[cursor..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary");
                if !next.is_whitespace() {
                    break;
                }
                cursor += next.len_utf8();
            }
            push_span(spans, token_start, cursor, JinjaToken::Plain);
        } else if matches!(character, '\'' | '"') {
            let token_start = cursor;
            cursor += character.len_utf8();
            let mut escaped = false;
            while cursor < end {
                let next = source[cursor..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary");
                cursor += next.len_utf8();
                if next == character && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
                if next != '\\' {
                    escaped = false;
                }
            }
            push_span(spans, token_start, cursor, JinjaToken::String);
            expect_filter = false;
        } else if character.is_ascii_digit() {
            let token_start = cursor;
            cursor += character.len_utf8();
            while cursor < end {
                let next = source[cursor..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary");
                if !(next.is_ascii_digit() || matches!(next, '.' | '_')) {
                    break;
                }
                cursor += next.len_utf8();
            }
            push_span(spans, token_start, cursor, JinjaToken::Number);
            expect_filter = false;
        } else if is_identifier_character(character) {
            let token_start = cursor;
            cursor += character.len_utf8();
            while cursor < end {
                let next = source[cursor..]
                    .chars()
                    .next()
                    .expect("valid UTF-8 boundary");
                if !is_identifier_character(next) {
                    break;
                }
                cursor += next.len_utf8();
            }
            let value = &source[token_start..cursor];
            let token = if expect_filter {
                JinjaToken::Filter
            } else if is_jinja_keyword(value) {
                JinjaToken::Keyword
            } else {
                JinjaToken::Identifier
            };
            push_span(spans, token_start, cursor, token);
            expect_filter = false;
        } else {
            let token_start = cursor;
            cursor += character.len_utf8();
            push_span(spans, token_start, cursor, JinjaToken::Operator);
            expect_filter = character == '|';
        }
    }
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '.')
}

fn is_jinja_keyword(value: &str) -> bool {
    matches!(
        value,
        "and"
            | "as"
            | "block"
            | "else"
            | "elif"
            | "endblock"
            | "endfilter"
            | "endfor"
            | "endif"
            | "endmacro"
            | "endraw"
            | "extends"
            | "false"
            | "filter"
            | "for"
            | "if"
            | "import"
            | "in"
            | "include"
            | "is"
            | "macro"
            | "none"
            | "not"
            | "or"
            | "raw"
            | "set"
            | "true"
            | "with"
    )
}

fn push_span(spans: &mut Vec<TokenSpan>, start: usize, end: usize, token: JinjaToken) {
    if start < end {
        spans.push(TokenSpan {
            range: start..end,
            token,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jinja_tokens_distinguish_markdown_tags_filters_and_comments() {
        let source = "# 因子提示\n{{ request.campaign | tojson(indent=2) }}\n{% for item in request.evidence %}\n{{ item.title }}\n{% endfor %}\n{# private note #}\n";
        let spans = tokenize_jinja(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("# 因子提示", JinjaToken::MarkdownHeading)));
        assert!(tokens.contains(&("{{", JinjaToken::Delimiter)));
        assert!(tokens.contains(&("request.campaign", JinjaToken::Identifier)));
        assert!(tokens.contains(&("tojson", JinjaToken::Filter)));
        assert!(tokens.contains(&("2", JinjaToken::Number)));
        assert!(tokens.contains(&("for", JinjaToken::Keyword)));
        assert!(tokens.contains(&(" private note ", JinjaToken::Comment)));
    }

    #[test]
    fn quoted_closing_delimiters_do_not_end_a_jinja_expression() {
        let source = "{{ '文本 }} remains string' | upper }}";
        let spans = tokenize_jinja(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("'文本 }} remains string'", JinjaToken::String)));
        assert!(tokens.contains(&("upper", JinjaToken::Filter)));
        assert_eq!(tokens.last(), Some(&("}}", JinjaToken::Delimiter)));
    }

    #[test]
    fn highlighted_layout_preserves_every_template_byte() {
        let source = "## Objective\n{{ request.objective }}\n```json\n{{ value | tojson }}\n```\n";
        let job = highlight_jinja(source, false, &FontId::monospace(14.0), 800.0);

        assert_eq!(job.text, source);
        assert!(job.sections.iter().any(|section| {
            section.format.color == JinjaColors::light().delimiter
                && &job.text[section.byte_range.clone()] == "{{"
        }));
        assert!(job.sections.iter().any(|section| {
            section.format.color == JinjaColors::light().filter
                && &job.text[section.byte_range.clone()] == "tojson"
        }));
    }
}
