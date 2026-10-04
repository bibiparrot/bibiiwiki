use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::parse_page;
use super::search::{markdown_files, slash_path};

const CANONICAL_DIRECTORIES: [&str; 9] = [
    "concepts",
    "entities",
    "factors",
    "media",
    "methodology",
    "mining",
    "opportunities",
    "queries",
    "sources",
];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BrokenLink {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WikiHealth {
    pub broken_links: Vec<BrokenLink>,
    pub unindexed: Vec<String>,
    pub invalid_frontmatter: Vec<String>,
    pub invalid_naming: Vec<String>,
    pub invalid_headings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct WikiLinter {
    wiki_dir: PathBuf,
}

impl WikiLinter {
    #[must_use]
    pub fn new(wiki_dir: PathBuf) -> Self {
        Self { wiki_dir }
    }

    /// Checks links, index coverage, frontmatter, bilingual names, and headings.
    ///
    /// # Errors
    ///
    /// Returns an error when the wiki cannot be read or parsed.
    pub fn check(&self) -> Result<WikiHealth> {
        let pages = markdown_files(&self.wiki_dir)?;
        let known = self.known_targets(&pages)?;
        let mut health = WikiHealth::default();
        for path in &pages {
            let relative_path = path
                .strip_prefix(&self.wiki_dir)
                .with_context(|| format!("wiki page escaped the root: {}", path.display()))?;
            let relative = slash_path(relative_path);
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read wiki page {}", path.display()))?;
            let page = parse_page(&text)
                .with_context(|| format!("invalid wiki frontmatter in {}", path.display()))?;
            if !text.starts_with("---\n") || page.version < 1 {
                health.invalid_frontmatter.push(relative.clone());
            }
            if requires_bilingual_name(relative_path)
                && (!path
                    .file_stem()
                    .is_some_and(|stem| stem.to_string_lossy().contains('_'))
                    || !is_bilingual_label(&page.title)
                    || uses_translation_fallback(&page.title)
                    || path
                        .file_stem()
                        .is_some_and(|stem| uses_translation_fallback(&stem.to_string_lossy())))
            {
                health.invalid_naming.push(relative.clone());
            }
            if headings(&text)
                .iter()
                .any(|heading| !is_bilingual_label(heading))
            {
                health.invalid_headings.push(relative.clone());
            }
            for link in wikilinks(&text) {
                let target = link.target.split('#').next().unwrap_or_default().trim();
                if !known.contains(&normalize(target)) {
                    health.broken_links.push(BrokenLink {
                        source: relative.clone(),
                        target: target.to_string(),
                    });
                }
            }
        }
        health.broken_links.sort();
        health.invalid_frontmatter.sort();
        health.invalid_naming.sort();
        health.invalid_headings.sort();

        let index_path = self.wiki_dir.join("index.md");
        let index = fs::read_to_string(&index_path)
            .unwrap_or_default()
            .to_lowercase();
        for path in pages.iter().filter(|path| {
            path.as_path() != index_path.as_path() && !is_indexed(path, &self.wiki_dir, &index)
        }) {
            let relative = path
                .strip_prefix(&self.wiki_dir)
                .with_context(|| format!("wiki page escaped the root: {}", path.display()))?;
            health.unindexed.push(slash_path(relative));
        }
        Ok(health)
    }

    fn known_targets(&self, pages: &[PathBuf]) -> Result<HashSet<String>> {
        let mut targets = HashSet::new();
        for path in pages {
            let relative = path
                .strip_prefix(&self.wiki_dir)
                .expect("walked path remains inside wiki root");
            let relative_text = slash_path(relative);
            targets.insert(normalize(&relative_text));
            targets.insert(normalize(relative_text.trim_end_matches(".md")));
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                targets.insert(normalize(stem));
            }
            let content = fs::read_to_string(path)
                .with_context(|| format!("failed to read wiki page {}", path.display()))?;
            if let Ok(page) = parse_page(&content)
                && !page.title.is_empty()
            {
                targets.insert(normalize(&page.title));
            }
        }
        Ok(targets)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WikiLink {
    pub start: usize,
    pub end: usize,
    pub target: String,
    pub alias: String,
    pub escaped_alias: bool,
}

pub(crate) fn wikilinks(text: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let mut offset = 0;
    while let Some(open) = text[offset..].find("[[") {
        let start = offset + open;
        let inner_start = start + 2;
        let Some(close) = text[inner_start..].find("]]") else {
            break;
        };
        let end = inner_start + close + 2;
        let inner = &text[inner_start..inner_start + close];
        let (target, alias, escaped_alias) = if let Some(index) = inner.find(r"\|") {
            (&inner[..index], &inner[index + 2..], true)
        } else if let Some(index) = inner.find('|') {
            (&inner[..index], &inner[index + 1..], false)
        } else {
            (inner, "", false)
        };
        links.push(WikiLink {
            start,
            end,
            target: target.to_string(),
            alias: alias.to_string(),
            escaped_alias,
        });
        offset = end;
    }
    links
}

pub(crate) fn normalize(value: &str) -> String {
    let normalized = value.trim().replace('\\', "/").to_lowercase();
    normalized
        .strip_suffix(".md")
        .unwrap_or(&normalized)
        .to_string()
}

fn requires_bilingual_name(relative: &Path) -> bool {
    let parts: Vec<_> = relative.components().collect();
    parts.len() > 1
        && CANONICAL_DIRECTORIES
            .iter()
            .any(|directory| parts[0].as_os_str().to_string_lossy().as_ref() == *directory)
        && relative.file_name().and_then(|value| value.to_str()) != Some("index.md")
}

fn is_indexed(path: &Path, root: &Path, index: &str) -> bool {
    let relative = path
        .strip_prefix(root)
        .expect("walked path remains inside wiki root");
    let relative_text = slash_path(relative);
    let without_suffix = relative_text.strip_suffix(".md").unwrap_or(&relative_text);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    [relative_text.as_str(), without_suffix, stem]
        .iter()
        .any(|candidate| index.contains(&candidate.to_lowercase()))
}

fn headings(text: &str) -> Vec<String> {
    let mut in_code = false;
    let mut values = Vec::new();
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        } else if !in_code {
            let hashes = line
                .chars()
                .take_while(|character| *character == '#')
                .count();
            if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
                values.push(line.to_string());
            }
        }
    }
    values
}

fn is_bilingual_label(value: &str) -> bool {
    let Some((chinese, english)) = value.split_once(" (") else {
        return false;
    };
    english.ends_with(')')
        && chinese.chars().any(is_cjk)
        && english[..english.len() - 1]
            .chars()
            .any(|character| character.is_ascii_alphabetic())
}

fn is_cjk(character: char) -> bool {
    ('\u{3400}'..='\u{9fff}').contains(&character)
}

fn uses_translation_fallback(value: &str) -> bool {
    value.contains("TranslatedTerm-") || value.contains("术语")
}
