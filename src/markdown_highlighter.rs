use std::ops::Range;

use eframe::egui::{Color32, FontId, Stroke, TextFormat, text::LayoutJob};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkdownToken {
    Plain,
    Heading,
    Marker,
    Code,
    Link,
    Quote,
    FrontmatterKey,
    FrontmatterValue,
    FrontmatterDelimiter,
    Comment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenSpan {
    range: Range<usize>,
    token: MarkdownToken,
}

#[derive(Clone, Copy)]
struct MarkdownColors {
    plain: Color32,
    heading: Color32,
    marker: Color32,
    code: Color32,
    code_background: Color32,
    link: Color32,
    quote: Color32,
    frontmatter_key: Color32,
    frontmatter_value: Color32,
    frontmatter_delimiter: Color32,
    comment: Color32,
}

impl MarkdownColors {
    const fn light() -> Self {
        Self {
            plain: Color32::from_rgb(30, 41, 59),
            heading: Color32::from_rgb(0, 61, 165),
            marker: Color32::from_rgb(190, 24, 93),
            code: Color32::from_rgb(153, 76, 0),
            code_background: Color32::from_rgb(245, 241, 235),
            link: Color32::from_rgb(2, 101, 135),
            quote: Color32::from_rgb(54, 119, 82),
            frontmatter_key: Color32::from_rgb(0, 61, 165),
            frontmatter_value: Color32::from_rgb(153, 76, 0),
            frontmatter_delimiter: Color32::from_rgb(126, 34, 206),
            comment: Color32::from_rgb(54, 119, 82),
        }
    }

    const fn dark() -> Self {
        Self {
            plain: Color32::from_rgb(226, 232, 240),
            heading: Color32::from_rgb(125, 211, 252),
            marker: Color32::from_rgb(244, 114, 182),
            code: Color32::from_rgb(253, 186, 116),
            code_background: Color32::from_rgb(50, 42, 36),
            link: Color32::from_rgb(103, 232, 249),
            quote: Color32::from_rgb(134, 239, 172),
            frontmatter_key: Color32::from_rgb(125, 211, 252),
            frontmatter_value: Color32::from_rgb(253, 186, 116),
            frontmatter_delimiter: Color32::from_rgb(216, 180, 254),
            comment: Color32::from_rgb(134, 239, 172),
        }
    }

    const fn color(self, token: MarkdownToken) -> Color32 {
        match token {
            MarkdownToken::Plain => self.plain,
            MarkdownToken::Heading => self.heading,
            MarkdownToken::Marker => self.marker,
            MarkdownToken::Code => self.code,
            MarkdownToken::Link => self.link,
            MarkdownToken::Quote => self.quote,
            MarkdownToken::FrontmatterKey => self.frontmatter_key,
            MarkdownToken::FrontmatterValue => self.frontmatter_value,
            MarkdownToken::FrontmatterDelimiter => self.frontmatter_delimiter,
            MarkdownToken::Comment => self.comment,
        }
    }
}

pub(crate) fn highlight_markdown(
    source: &str,
    dark_mode: bool,
    font_id: &FontId,
    wrap_width: f32,
) -> LayoutJob {
    let colors = if dark_mode {
        MarkdownColors::dark()
    } else {
        MarkdownColors::light()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for span in tokenize_markdown(source) {
        let mut format = TextFormat {
            font_id: font_id.clone(),
            color: colors.color(span.token),
            ..Default::default()
        };
        if span.token == MarkdownToken::Code {
            format.background = colors.code_background;
        }
        if span.token == MarkdownToken::Link {
            format.underline = Stroke::new(1.0_f32, colors.link);
        }
        if span.token == MarkdownToken::Comment {
            format.italics = true;
        }
        job.append(&source[span.range], 0.0, format);
    }
    job
}

fn tokenize_markdown(source: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    let mut in_frontmatter = false;
    let mut code_fence: Option<&str> = None;

    for line in source.split_inclusive('\n') {
        let content_len = line.strip_suffix('\n').map_or(line.len(), str::len);
        let content_end = offset + content_len;
        let content = &source[offset..content_end];
        let logical = content.strip_suffix('\r').unwrap_or(content);
        let logical_end = offset + logical.len();
        let trimmed = logical.trim_start_matches([' ', '\t']);
        let leading = logical.len() - trimmed.len();

        if offset == 0 && trimmed == "---" && leading == 0 {
            in_frontmatter = true;
            push_span(
                &mut spans,
                offset,
                logical_end,
                MarkdownToken::FrontmatterDelimiter,
            );
        } else if in_frontmatter && matches!(trimmed, "---" | "...") && leading == 0 {
            push_span(
                &mut spans,
                offset,
                logical_end,
                MarkdownToken::FrontmatterDelimiter,
            );
            in_frontmatter = false;
        } else if in_frontmatter {
            tokenize_frontmatter_line(source, offset, logical_end, &mut spans);
        } else if let Some(fence) = code_fence {
            push_span(&mut spans, offset, logical_end, MarkdownToken::Code);
            if trimmed.starts_with(fence) {
                code_fence = None;
            }
        } else if let Some(fence) = fence_marker(trimmed) {
            push_span(&mut spans, offset, logical_end, MarkdownToken::Code);
            code_fence = Some(fence);
        } else {
            tokenize_markdown_line(source, offset, logical_end, &mut spans);
        }

        push_span(&mut spans, logical_end, content_end, MarkdownToken::Plain);
        push_span(
            &mut spans,
            content_end,
            offset + line.len(),
            MarkdownToken::Plain,
        );
        offset += line.len();
    }

    spans
}

fn tokenize_frontmatter_line(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let line = &source[start..end];
    let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
    let content_start = start + leading;
    push_span(spans, start, content_start, MarkdownToken::Plain);

    if source[content_start..end].starts_with('#') {
        push_span(spans, content_start, end, MarkdownToken::Comment);
        return;
    }

    let mut key_start = content_start;
    if source[content_start..end].starts_with('-')
        && source
            .as_bytes()
            .get(content_start + 1)
            .is_none_or(u8::is_ascii_whitespace)
    {
        push_span(
            spans,
            content_start,
            content_start + 1,
            MarkdownToken::Marker,
        );
        key_start += 1;
        while key_start < end && source.as_bytes()[key_start].is_ascii_whitespace() {
            key_start += 1;
        }
        push_span(spans, content_start + 1, key_start, MarkdownToken::Plain);
    }

    if let Some(relative) = source[key_start..end].find(':') {
        let colon = key_start + relative;
        push_span(spans, key_start, colon, MarkdownToken::FrontmatterKey);
        push_span(spans, colon, colon + 1, MarkdownToken::FrontmatterDelimiter);
        push_span(spans, colon + 1, end, MarkdownToken::FrontmatterValue);
    } else {
        push_span(spans, key_start, end, MarkdownToken::FrontmatterValue);
    }
}

fn tokenize_markdown_line(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let line = &source[start..end];
    let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
    let content_start = start + leading;
    let content = &source[content_start..end];
    push_span(spans, start, content_start, MarkdownToken::Plain);

    if content.starts_with("<!--") {
        push_span(spans, content_start, end, MarkdownToken::Comment);
        return;
    }

    let hashes = content.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes)
        && content
            .as_bytes()
            .get(hashes)
            .is_some_and(u8::is_ascii_whitespace)
    {
        push_span(
            spans,
            content_start,
            content_start + hashes,
            MarkdownToken::Marker,
        );
        push_span(spans, content_start + hashes, end, MarkdownToken::Heading);
        return;
    }

    if content.starts_with('>') {
        push_span(
            spans,
            content_start,
            content_start + 1,
            MarkdownToken::Quote,
        );
        tokenize_inline(source, content_start + 1, end, spans);
        return;
    }

    if let Some(marker_len) = list_marker_len(content) {
        push_span(
            spans,
            content_start,
            content_start + marker_len,
            MarkdownToken::Marker,
        );
        tokenize_inline(source, content_start + marker_len, end, spans);
        return;
    }

    tokenize_inline(source, content_start, end, spans);
}

fn tokenize_inline(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let mut cursor = start;
    let mut plain_start = start;
    while cursor < end {
        let rest = &source[cursor..end];
        if rest.starts_with('`') {
            push_span(spans, plain_start, cursor, MarkdownToken::Plain);
            let ticks = rest.bytes().take_while(|byte| *byte == b'`').count();
            let delimiter_end = cursor + ticks;
            push_span(spans, cursor, delimiter_end, MarkdownToken::Marker);
            if let Some(relative) = source[delimiter_end..end].find(&"`".repeat(ticks)) {
                let close = delimiter_end + relative;
                push_span(spans, delimiter_end, close, MarkdownToken::Code);
                push_span(spans, close, close + ticks, MarkdownToken::Marker);
                cursor = close + ticks;
            } else {
                cursor = delimiter_end;
            }
            plain_start = cursor;
        } else if rest.starts_with('[') {
            if let Some(label_end_relative) = rest.find("](") {
                let label_end = cursor + label_end_relative;
                if let Some(destination_end_relative) = source[label_end + 2..end].find(')') {
                    push_span(spans, plain_start, cursor, MarkdownToken::Plain);
                    push_span(spans, cursor, cursor + 1, MarkdownToken::Marker);
                    push_span(spans, cursor + 1, label_end, MarkdownToken::Link);
                    push_span(spans, label_end, label_end + 2, MarkdownToken::Marker);
                    let destination_end = label_end + 2 + destination_end_relative;
                    push_span(spans, label_end + 2, destination_end, MarkdownToken::Link);
                    push_span(
                        spans,
                        destination_end,
                        destination_end + 1,
                        MarkdownToken::Marker,
                    );
                    cursor = destination_end + 1;
                    plain_start = cursor;
                    continue;
                }
            }
            cursor += 1;
        } else if matches!(source.as_bytes()[cursor], b'*' | b'_' | b'~') {
            push_span(spans, plain_start, cursor, MarkdownToken::Plain);
            let marker = source.as_bytes()[cursor];
            let marker_len = source[cursor..end]
                .bytes()
                .take_while(|byte| *byte == marker)
                .take(2)
                .count();
            push_span(spans, cursor, cursor + marker_len, MarkdownToken::Marker);
            cursor += marker_len;
            plain_start = cursor;
        } else {
            let character = rest.chars().next().expect("valid UTF-8 boundary");
            cursor += character.len_utf8();
        }
    }
    push_span(spans, plain_start, end, MarkdownToken::Plain);
}

fn fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn list_marker_len(line: &str) -> Option<usize> {
    if matches!(line.as_bytes().first(), Some(b'-' | b'*' | b'+'))
        && line.as_bytes().get(1).is_some_and(u8::is_ascii_whitespace)
    {
        return Some(1);
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && matches!(line.as_bytes().get(digits), Some(b'.' | b')'))
        && line
            .as_bytes()
            .get(digits + 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        Some(digits + 1)
    } else {
        None
    }
}

fn push_span(spans: &mut Vec<TokenSpan>, start: usize, end: usize, token: MarkdownToken) {
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
    fn markdown_tokens_cover_frontmatter_headings_lists_links_and_code() {
        let source = "---\ntitle: Purpose\ntags:\n- project\n---\n# 目的 (Purpose)\n\n1. Read `raw` and [index](wiki/index.md).\n";
        let spans = tokenize_markdown(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("title", MarkdownToken::FrontmatterKey)));
        assert!(tokens.contains(&("---", MarkdownToken::FrontmatterDelimiter)));
        assert!(tokens.contains(&("#", MarkdownToken::Marker)));
        assert!(tokens.contains(&(" 目的 (Purpose)", MarkdownToken::Heading)));
        assert!(tokens.contains(&("1.", MarkdownToken::Marker)));
        assert!(tokens.contains(&("raw", MarkdownToken::Code)));
        assert!(tokens.contains(&("index", MarkdownToken::Link)));
    }

    #[test]
    fn highlighted_layout_preserves_every_markdown_byte_including_crlf_and_unicode() {
        let source = "---\r\ntitle: 目的\r\n---\r\n## 运行循环\r\n- 项目 **一**\r\n";
        let job = highlight_markdown(source, false, &FontId::monospace(14.0), 800.0);

        assert_eq!(job.text, source);
        assert!(job.sections.iter().any(|section| {
            section.format.color == MarkdownColors::light().heading
                && &job.text[section.byte_range.clone()] == " 运行循环"
        }));
        assert!(job.sections.iter().any(|section| {
            section.format.color == MarkdownColors::light().marker
                && &job.text[section.byte_range.clone()] == "-"
        }));
    }
}
