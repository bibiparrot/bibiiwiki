use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikiPage {
    #[serde(default)]
    pub title: String,
    #[serde(skip)]
    pub body: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default = "default_page_type", rename = "type")]
    pub page_type: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub backtesttype: String,
    #[serde(default)]
    pub backtestdate: String,
}

impl Default for WikiPage {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            status: default_status(),
            page_type: default_page_type(),
            tags: Vec::new(),
            related: Vec::new(),
            sources: Vec::new(),
            created: String::new(),
            updated: String::new(),
            version: 1,
            backtesttype: String::new(),
            backtestdate: String::new(),
        }
    }
}

/// Parses one Markdown wiki page and its optional YAML frontmatter.
///
/// # Errors
///
/// Returns an error when frontmatter exists but is not valid YAML.
pub fn parse_page(content: &str) -> Result<WikiPage, serde_yaml::Error> {
    if !content.starts_with("---\n") {
        return Ok(WikiPage {
            body: content.to_string(),
            version: 0,
            ..WikiPage::default()
        });
    }

    let mut parts = content.splitn(3, "---");
    let _opening = parts.next();
    let frontmatter = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    let mut page: WikiPage = serde_yaml::from_str(frontmatter)?;
    page.body = format!("{}\n", body.trim_start_matches(['\r', '\n']).trim_end());
    Ok(page)
}

/// Renders a page using the Python `llm_wiki` frontmatter contract.
///
/// # Errors
///
/// Returns an error when factor lifecycle metadata is invalid or YAML cannot
/// be serialized.
pub fn render_page(page: &WikiPage) -> Result<String> {
    if page.page_type == "factor"
        && !matches!(
            page.backtesttype.as_str(),
            "" | "todo" | "positive" | "negative"
        )
    {
        bail!("Factor backtesttype must be todo, positive, or negative");
    }
    let frontmatter = Frontmatter::from(page);
    let yaml = serde_yaml::to_string(&frontmatter)?.trim().to_string();
    Ok(format!("---\n{yaml}\n---\n\n{}\n", page.body.trim_end()))
}

#[derive(Serialize)]
struct Frontmatter<'a> {
    title: &'a str,
    #[serde(rename = "type")]
    page_type: &'a str,
    status: &'a str,
    created: &'a str,
    updated: &'a str,
    tags: &'a [String],
    related: &'a [String],
    sources: &'a [String],
    version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    backtesttype: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backtestdate: Option<&'a str>,
}

impl<'a> From<&'a WikiPage> for Frontmatter<'a> {
    fn from(page: &'a WikiPage) -> Self {
        let factor = page.page_type == "factor";
        Self {
            title: &page.title,
            page_type: &page.page_type,
            status: &page.status,
            created: &page.created,
            updated: &page.updated,
            tags: &page.tags,
            related: &page.related,
            sources: &page.sources,
            version: page.version,
            backtesttype: factor.then_some(if page.backtesttype.is_empty() {
                "todo"
            } else {
                &page.backtesttype
            }),
            backtestdate: factor.then_some(&page.backtestdate),
        }
    }
}

fn default_status() -> String {
    "draft".to_string()
}

fn default_page_type() -> String {
    "concept".to_string()
}
