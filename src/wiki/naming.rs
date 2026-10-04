use sha2::{Digest, Sha256};

const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[must_use]
pub fn safe_filename_segment(value: &str) -> String {
    let mut segment = String::new();
    for character in value.chars() {
        if character <= '\u{1f}' {
            segment.push(' ');
        } else {
            segment.push(match character {
                '/' => '／',
                '\\' => '＼',
                ':' => '：',
                '*' => '＊',
                '?' => '？',
                '"' => '＂',
                '<' => '＜',
                '>' => '＞',
                '|' => '｜',
                '#' => '＃',
                other => other,
            });
        }
    }
    segment = segment.split_whitespace().collect::<Vec<_>>().join(" ");
    while segment.ends_with('.') {
        segment.pop();
        segment.push('．');
    }
    if RESERVED_NAMES
        .iter()
        .any(|reserved| segment.eq_ignore_ascii_case(reserved))
    {
        segment.insert(0, '_');
    }
    if segment.is_empty() || segment == "." || segment == ".." {
        let digest = Sha256::digest(value.as_bytes());
        return format!("untitled-{}", hex_prefix(&digest, 12));
    }
    segment
}

fn hex_prefix(bytes: &[u8], characters: usize) -> String {
    let mut output = String::with_capacity(characters);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
        if output.len() >= characters {
            output.truncate(characters);
            break;
        }
    }
    output
}
