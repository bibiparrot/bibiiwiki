use std::ops::Range;

use eframe::egui::{Color32, FontId, TextFormat, text::LayoutJob};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YamlToken {
    Plain,
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
    token: YamlToken,
}

#[derive(Clone, Copy)]
struct YamlColors {
    plain: Color32,
    key: Color32,
    string: Color32,
    number: Color32,
    keyword: Color32,
    comment: Color32,
    punctuation: Color32,
}

impl YamlColors {
    const fn light() -> Self {
        Self {
            plain: Color32::from_rgb(30, 41, 59),
            key: Color32::from_rgb(0, 61, 165),
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
            key: Color32::from_rgb(125, 211, 252),
            string: Color32::from_rgb(253, 186, 116),
            number: Color32::from_rgb(196, 181, 253),
            keyword: Color32::from_rgb(244, 114, 182),
            comment: Color32::from_rgb(134, 239, 172),
            punctuation: Color32::from_rgb(244, 114, 182),
        }
    }

    const fn color(self, token: YamlToken) -> Color32 {
        match token {
            YamlToken::Plain => self.plain,
            YamlToken::Key => self.key,
            YamlToken::String => self.string,
            YamlToken::Number => self.number,
            YamlToken::Keyword => self.keyword,
            YamlToken::Comment => self.comment,
            YamlToken::Punctuation => self.punctuation,
        }
    }
}

pub(crate) fn highlight_yaml(
    source: &str,
    dark_mode: bool,
    font_id: &FontId,
    wrap_width: f32,
) -> LayoutJob {
    let colors = if dark_mode {
        YamlColors::dark()
    } else {
        YamlColors::light()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for span in tokenize_yaml(source) {
        let mut format = TextFormat {
            font_id: font_id.clone(),
            color: colors.color(span.token),
            ..Default::default()
        };
        format.italics = span.token == YamlToken::Comment;
        job.append(&source[span.range], 0.0, format);
    }
    job
}

fn tokenize_yaml(source: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        let content_len = line.strip_suffix('\n').map_or(line.len(), str::len);
        let content_end = line_start + content_len;
        tokenize_line(source, line_start, content_end, &mut spans);
        if content_end < line_start + line.len() {
            push_span(
                &mut spans,
                content_end,
                line_start + line.len(),
                YamlToken::Plain,
            );
        }
        line_start += line.len();
    }
    spans
}

fn tokenize_line(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let comment_start =
        find_yaml_comment(&source[start..end]).map_or(end, |relative| start + relative);
    let mut cursor = start;
    while cursor < comment_start && source.as_bytes()[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    push_span(spans, start, cursor, YamlToken::Plain);

    if cursor < comment_start && source.as_bytes()[cursor] == b'-' {
        let after_dash = cursor + 1;
        if after_dash == comment_start || source.as_bytes()[after_dash].is_ascii_whitespace() {
            push_span(spans, cursor, after_dash, YamlToken::Punctuation);
            cursor = after_dash;
            while cursor < comment_start && source.as_bytes()[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            push_span(spans, after_dash, cursor, YamlToken::Plain);
        }
    }

    if let Some(relative_colon) = find_mapping_colon(&source[cursor..comment_start]) {
        let colon = cursor + relative_colon;
        let key_end = trim_ascii_end(source, cursor, colon);
        push_span(spans, cursor, key_end, YamlToken::Key);
        push_span(spans, key_end, colon, YamlToken::Plain);
        push_span(spans, colon, colon + 1, YamlToken::Punctuation);
        tokenize_scalar(source, colon + 1, comment_start, spans);
    } else {
        tokenize_scalar(source, cursor, comment_start, spans);
    }

    push_span(spans, comment_start, end, YamlToken::Comment);
}

fn tokenize_scalar(source: &str, start: usize, end: usize, spans: &mut Vec<TokenSpan>) {
    let mut value_start = start;
    while value_start < end && source.as_bytes()[value_start].is_ascii_whitespace() {
        value_start += 1;
    }
    let value_end = trim_ascii_end(source, value_start, end);
    push_span(spans, start, value_start, YamlToken::Plain);
    if value_start < value_end {
        let value = &source[value_start..value_end];
        push_span(spans, value_start, value_end, classify_scalar(value));
    }
    push_span(spans, value_end, end, YamlToken::Plain);
}

fn classify_scalar(value: &str) -> YamlToken {
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
        || value.starts_with("http://")
        || value.starts_with("https://")
    {
        return YamlToken::String;
    }
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off"
    ) {
        return YamlToken::Keyword;
    }
    if value.replace('_', "").parse::<f64>().is_ok() {
        return YamlToken::Number;
    }
    if matches!(value, "|" | ">" | "---" | "...") {
        return YamlToken::Punctuation;
    }
    YamlToken::String
}

fn find_yaml_comment(line: &str) -> Option<usize> {
    find_unquoted(line, |index, byte| {
        byte == b'#' && (index == 0 || line.as_bytes()[index - 1].is_ascii_whitespace())
    })
}

fn find_mapping_colon(line: &str) -> Option<usize> {
    find_unquoted(line, |index, byte| {
        byte == b':'
            && line
                .as_bytes()
                .get(index + 1)
                .is_none_or(u8::is_ascii_whitespace)
    })
}

fn find_unquoted(line: &str, predicate: impl Fn(usize, u8) -> bool) -> Option<usize> {
    let mut single_quote = false;
    let mut double_quote = false;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if double_quote && byte == b'\\' && !escaped {
            escaped = true;
            continue;
        }
        if !escaped {
            if byte == b'\'' && !double_quote {
                single_quote = !single_quote;
            } else if byte == b'"' && !single_quote {
                double_quote = !double_quote;
            } else if !single_quote && !double_quote && predicate(index, byte) {
                return Some(index);
            }
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

fn push_span(spans: &mut Vec<TokenSpan>, start: usize, end: usize, token: YamlToken) {
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
    fn yaml_tokens_distinguish_keys_values_numbers_and_comments() {
        let source = "# stack\nserver:\n  bind: 127.0.0.1:4000\n  timeout: 600\n  enabled: true\n";
        let spans = tokenize_yaml(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("# stack", YamlToken::Comment)));
        assert!(tokens.contains(&("server", YamlToken::Key)));
        assert!(tokens.contains(&("127.0.0.1:4000", YamlToken::String)));
        assert!(tokens.contains(&("600", YamlToken::Number)));
        assert!(tokens.contains(&("true", YamlToken::Keyword)));
    }

    #[test]
    fn yaml_urls_and_hashes_inside_quotes_are_not_comments() {
        let source = "api_base: http://127.0.0.1:11434/v1\nlabel: \"value # one\" # note\n";
        let spans = tokenize_yaml(source);
        let tokens = spans
            .iter()
            .map(|span| (&source[span.range.clone()], span.token))
            .collect::<Vec<_>>();

        assert!(tokens.contains(&("http://127.0.0.1:11434/v1", YamlToken::String)));
        assert!(tokens.contains(&("\"value # one\"", YamlToken::String)));
        assert!(tokens.contains(&("# note", YamlToken::Comment)));
    }

    #[test]
    fn highlighted_layout_preserves_every_source_byte() {
        let source = "model_list:\n  - model_name: ornith-1.5:9b\n";
        let job = highlight_yaml(source, false, &FontId::monospace(14.0), 800.0);

        assert_eq!(job.text, source);
        assert!(job.sections.iter().any(|section| {
            section.format.color == YamlColors::light().key
                && &job.text[section.byte_range.clone()] == "model_list"
        }));
    }
}
