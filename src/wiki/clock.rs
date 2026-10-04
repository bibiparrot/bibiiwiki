use chrono::{FixedOffset, SecondsFormat, Utc};

fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 60 * 60).expect("UTC+08:00 is a valid fixed offset")
}

#[must_use]
pub fn today_beijing() -> String {
    Utc::now()
        .with_timezone(&beijing())
        .format("%Y-%m-%d")
        .to_string()
}

#[must_use]
pub fn now_beijing() -> String {
    Utc::now()
        .with_timezone(&beijing())
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}
