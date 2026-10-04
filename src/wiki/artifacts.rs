use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::{Authority, BilingualName, SearchHit, TemplateLibrary, WikiPage, WikiStore};

#[derive(Debug)]
pub struct QueryArtifactService {
    wiki_dir: PathBuf,
    store: WikiStore,
    templates: TemplateLibrary,
}

impl QueryArtifactService {
    /// Creates a query-memory writer.
    ///
    /// # Errors
    ///
    /// Returns an error if bundled templates cannot be compiled.
    pub fn new(wiki_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            store: WikiStore::new(wiki_dir.clone()),
            wiki_dir,
            templates: TemplateLibrary::load()?,
        })
    }

    /// Persists deterministic search results as wiki memory.
    ///
    /// # Errors
    ///
    /// Returns an error if rendering or writing fails.
    pub fn write_search(
        &self,
        keyword: &str,
        hits: &[SearchHit],
        name: &BilingualName,
    ) -> Result<PathBuf> {
        let relative = format!("queries/{}", name.filename());
        self.store.write_page(
            &relative,
            &WikiPage {
                title: name.title(),
                body: self.templates.render(
                    "wiki/queries/search.md.j2",
                    &json!({"keyword": keyword.trim(), "hits": hits}),
                )?,
                status: "generated".to_string(),
                page_type: "search".to_string(),
                tags: vec!["query-memory".to_string(), "search".to_string()],
                related: related(hits),
                ..WikiPage::default()
            },
            Authority::Agent,
        )
    }

    /// Persists a grounded answer and its evidence links as wiki memory.
    ///
    /// # Errors
    ///
    /// Returns an error if rendering or writing fails.
    pub fn write_query(
        &self,
        question: &str,
        answer: &str,
        hits: &[SearchHit],
        name: &BilingualName,
    ) -> Result<PathBuf> {
        let relative = format!("queries/{}", name.filename());
        self.store.write_page(
            &relative,
            &WikiPage {
                title: name.title(),
                body: self.templates.render(
                    "wiki/queries/query.md.j2",
                    &json!({
                        "question": question.trim(),
                        "answer": answer.trim(),
                        "hits": hits
                    }),
                )?,
                status: "generated".to_string(),
                page_type: "query".to_string(),
                tags: vec!["query-memory".to_string(), "query".to_string()],
                related: related(hits),
                ..WikiPage::default()
            },
            Authority::Agent,
        )
    }

    #[must_use]
    pub fn wiki_dir(&self) -> &Path {
        &self.wiki_dir
    }
}

fn related(hits: &[SearchHit]) -> Vec<String> {
    hits.iter()
        .map(|hit| {
            hit.path
                .strip_suffix(".md")
                .unwrap_or(&hit.path)
                .to_string()
        })
        .collect()
}
