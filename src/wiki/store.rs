use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::clock::today_beijing;
use super::{WikiPage, parse_page, render_page};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authority {
    Agent,
    Human,
}

#[derive(Clone, Debug)]
pub struct WikiStore {
    wiki_dir: PathBuf,
}

impl WikiStore {
    #[must_use]
    pub fn new(wiki_dir: PathBuf) -> Self {
        Self { wiki_dir }
    }

    /// Writes a wiki page while protecting active human-authored content.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, invalid pages, I/O failures, or an
    /// attempted agent replacement of an active page body.
    pub fn write_page(
        &self,
        relative_path: &str,
        page: &WikiPage,
        authority: Authority,
    ) -> Result<PathBuf> {
        let path = self.contained_path(relative_path)?;
        let existing = self.read_page(relative_path)?;
        if authority == Authority::Agent
            && let Some(existing) = existing.as_ref()
            && existing.status == "active"
            && existing.body != page.body
        {
            bail!("Active page requires human review: {relative_path}");
        }

        let mut page = page.clone();
        let today = today_beijing();
        if page.created.is_empty() {
            page.created = existing
                .as_ref()
                .map(|value| value.created.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| today.clone());
        }
        page.updated = today;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&path, render_page(&page)?)
            .with_context(|| format!("failed to write wiki page {}", path.display()))?;
        Ok(path)
    }

    /// Reads a wiki page if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, I/O failures, or malformed YAML.
    pub fn read_page(&self, relative_path: &str) -> Result<Option<WikiPage>> {
        let path = self.contained_path(relative_path)?;
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read wiki page {}", path.display()))?;
        parse_page(&content)
            .with_context(|| format!("invalid wiki frontmatter in {}", path.display()))
            .map(Some)
    }

    fn contained_path(&self, relative_path: &str) -> Result<PathBuf> {
        let relative = Path::new(relative_path);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("wiki path must stay within the wiki root: {relative_path:?}");
        }
        Ok(self.wiki_dir.join(relative))
    }
}
