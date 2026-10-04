use std::ops::Range;

use eframe::egui::{Color32, FontId, TextFormat, text::LayoutJob};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TomlToken {
    Plain,
    Section,
    Key,
    String,
    Number,
    Keyword,
    Comment,
    Punctuation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenSpan {
    range: Range<usize>,
    token: TomlToken,
}

#[derive(Clone, Copy)]
struct TomlColors {
    plain: Color32,
    section: Color32,
    key: Color32,
    string: Color32,
    number: Color32,
    keyword: Color32,
    comment: Color32,
    punctuation: Color32,
}

impl TomlColors {
    const fn light() -> Self {
        Self {
            plain: Color32::from_rgb(30, 41, 59),
            section: Color32::from_rgb(0, 61, 165),
            key: Color32::from_rgb(3, 105, 161),
            string: Color32::from_rgb(153, 76, 0),
            number: Color32::from_rgb(109, 40, 217),
            keyword: Color32::from_rgb(190, 24, 93),
            comment: Color32::from_rgb(54, 119, 82),
            punctuation: Color32::from_rgb(190, 24, 93),
        }
    }

    const fn dark() -> Self {
        Self {
            plain: Color32::from_rgb(226, 232, 240),
            section: Color32::from_rgb(125, 211, 252),
            key: Color32::from_rgb(103, 232, 249),
            string: Color32::from_rgb(253, 186, 116),
            number: Color32::from_rgb(196, 181, 253),
            keyword: Color32::from_rgb(244, 114, 182),
            comment: Color32::from_rgb(134, 239, 172),
            punctuation: Color32::from_rgb(244, 114, 182),
        }
    }

    const fn color(self, token: TomlToken) -> Color32 {
        match token {
            TomlToken::Plain => self.plain,
            TomlToken::Section => self.section,
            TomlToken::Key => self.key,
            TomlToken::String => self.string,
            TomlToken::Number => self.number,
            TomlToken::Keyword => self.keyword,
            TomlToken::Comment => self.comment,
            TomlToken::Punctuation => self.punctuation,
        }
    }
}

pub(crate) fn highlight_toml(
    source: &str,
    dark_mode: bool,
    font_id: &FontId,
    wrap_width: f32,
) -> LayoutJob {
    let colors = if dark_mode {
        TomlColors::dark()
    } else {
        TomlColors::light()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for span in tokenize_toml(source) {
        let mut format = TextFormat {
            font_id: font_id.clone(),
            color: colors.color(span.token),
            ..Default::default()
        };
        format.italics = span.token == TomlToken::Comment;
        job.append(&source[span.range], 0.0, format);
    }
    job
}

fn tokenize_toml(source: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        let content_len = line.strip_suffix('\n').map_or(line.len(), str::len);
        let content_end = line_start + content_len;
        tokenize_line(source, line_start, content_end, &mut spans);
        push_span(
            &mut spans,
            content_end,
            line_start + line.len(),
            TomlToken::Plain,
        );
        line_start += line.len();
    }
    spans
}

fn tokenize_line(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let comment_start =
        find_unquoted_byte(&source[start..end], b'#').map_or(end, |relative| start + relative);
    let mut cursor = start;
    while cursor < comment_start && source.as_bytes()[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    push_span(spans, start, cursor, TomlToken::Plain);

    let body_end = trim_ascii_end(source, cursor, comment_start);
    if cursor < body_end && source.as_bytes()[cursor] == b'[' {
        tokenize_section(source, cursor, body_end, spans);
    } else if let Some(relative_equals) = find_unquoted_byte(&source[cursor..body_end], b'=') {
        let equals = cursor + relative_equals;
        let key_end = trim_ascii_end(source, cursor, equals);
        push_span(spans, cursor, key_end, TomlToken::Key);
        push_span(spans, key_end, equals, TomlToken::Plain);
        push_span(spans, equals, equals + 1, TomlToken::Punctuation);
        tokenize_value(source, equals + 1, body_end, spans);
    } else {
        push_span(spans, cursor, body_end, TomlToken::Plain);
    }
    push_span(spans, body_end, comment_start, TomlToken::Plain);
    push_span(spans, comment_start, end, TomlToken::Comment);
}

fn tokenize_section(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let open_len = usize::from(source[start..end].starts_with("[[")) + 1;
    let close_len = usize::from(source[start..end].ends_with("]]")) + 1;
    if start + open_len + close_len <= end {
        push_span(spans, start, start + open_len, TomlToken::Punctuation);
        push_span(spans, start + open_len, end - close_len, TomlToken::Section);
        push_span(spans, end - close_len, end, TomlToken::Punctuation);
    } else {
        push_span(spans, start, end, TomlToken::Plain);
    }
}

fn tokenize_value(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let mut value_start = start;
    while value_start < end && source.as_bytes()[value_start].is_ascii_whitespace() {
        value_start += 1;
    }
    push_span(spans, start, value_start, TomlToken::Plain);
    if value_start < end {
        let value = &source[value_start..end];
        push_span(spans, value_start, end, classify_value(value));
    }
}

fn classify_value(value: &str) -> TomlToken {
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        return TomlToken::String;
    }
    if matches!(value, "true" | "false") {
        return TomlToken::Keyword;
    }
    if value.replace('_', "").parse::<f64>().is_ok() {
        return TomlToken::Number;
    }
    if value.starts_with('[') || value.starts_with('{') {
        return TomlToken::Punctuation;
    }
    TomlToken::Plain
}

fn find_unquoted_byte(line: &str, target: u8) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if quote == Some(b'"') && byte == b'\\' && !escaped {
            escaped = true;
            continue;
        }
        if !escaped && matches!(byte, b'"' | b'\'') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
        } else if quote.is_none() && byte == target {
            return Some(index);
        }
        escaped = false;
    }
    None
}

fn trim_ascii_end(source: &str, start: usize, mut end: usize) -> usize {
    while end > start && source.as_bytes()[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn push_span(spans: &mut Vec<TokenSpan>, start: usize, end: usize, token: TomlToken) {
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
    fn toml_tokens_distinguish_sections_keys_values_and_comments() {
        let source = "[ingest]\nsource_dir = 'D:\\\\docs'\nforce = false\nlimit = 10 # max\n";
        let spans = tokenize_toml(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("ingest", TomlToken::Section)));
        assert!(tokens.contains(&("source_dir", TomlToken::Key)));
        assert!(tokens.contains(&("'D:\\\\docs'", TomlToken::String)));
        assert!(tokens.contains(&("false", TomlToken::Keyword)));
        assert!(tokens.contains(&("10", TomlToken::Number)));
        assert!(tokens.contains(&("# max", TomlToken::Comment)));
    }

    #[test]
    fn toml_hashes_and_equals_inside_strings_are_not_syntax() {
        let source = "value = \"a=b # still text\" # comment\n";
        let spans = tokenize_toml(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("\"a=b # still text\"", TomlToken::String)));
        assert!(tokens.contains(&("# comment", TomlToken::Comment)));
    }

    #[test]
    fn highlighted_layout_preserves_every_toml_byte() {
        let source = "[query]\nuse_llm = true\n";
        let job = highlight_toml(source, false, &FontId::monospace(14.0), 800.0);

        assert_eq!(job.text, source);
        assert!(job.sections.iter().any(|section| {
            section.format.color == TomlColors::light().section
                && &job.text[section.byte_range.clone()] == "query"
        }));
    }
}
