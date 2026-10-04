use anyhow::{Result, bail};

use super::safe_filename_segment;

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PartialBilingualName {
    #[serde(default)]
    pub english: String,
    #[serde(default)]
    pub chinese: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BilingualName {
    pub english: String,
    pub chinese: String,
}

impl BilingualName {
    /// Creates a validated English/Chinese name pair.
    ///
    /// # Errors
    ///
    /// Returns an error if English is empty/non-ASCII or Chinese lacks a CJK
    /// character.
    pub fn new(english: impl Into<String>, chinese: impl Into<String>) -> Result<Self> {
        let english = english.into().trim().to_string();
        let chinese = chinese.into().trim().to_string();
        if english.is_empty() || !english.is_ascii() {
            bail!("English term is invalid: {english:?}");
        }
        if !chinese.chars().any(is_cjk) {
            bail!("Chinese term is invalid: {chinese:?}");
        }
        Ok(Self { english, chinese })
    }

    #[must_use]
    pub fn title(&self) -> String {
        format!("{} ({})", self.chinese, self.english)
    }

    #[must_use]
    pub fn filename(&self) -> String {
        format!(
            "{}.md",
            safe_filename_segment(&format!("{}_{}", self.english, self.chinese))
        )
    }
}

fn is_cjk(character: char) -> bool {
    ('\u{3400}'..='\u{9fff}').contains(&character)
}
