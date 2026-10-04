use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks::Bytes};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::BooleanQuery;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{Index, TantivyDocument, Term, doc};

use super::parse_page;
use crate::config::Config;

const SEARCH_INDEX_VERSION: u32 = 2;
const INDEX_WRITER_BYTES_PER_THREAD: usize = 20_000_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub score: usize,
    pub excerpt: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    Tantivy,
    RipgrepFallback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub backend: SearchBackend,
    pub index_updated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WikiSearch {
    root: PathBuf,
    index_dir: Option<PathBuf>,
}

impl WikiSearch {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let index_dir = default_index_dir(&root);
        Self { root, index_dir }
    }

    /// Overrides the persistent Tantivy cache directory.
    #[must_use]
    pub fn with_index_dir(mut self, index_dir: PathBuf) -> Self {
        self.index_dir = Some(index_dir);
        self
    }

    /// Searches Markdown through Tantivy while preserving the deterministic
    /// Python-compatible ranker and a transparent ripgrep fallback.
    ///
    /// # Errors
    ///
    /// Returns an error when a wiki page cannot be read or parsed.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        Ok(self.search_detailed(query, limit)?.hits)
    }

    /// Searches and reports which backend served the request.
    ///
    /// # Errors
    ///
    /// Returns an error only when Tantivy and the ripgrep fallback both fail.
    pub fn search_detailed(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(SearchResults {
                hits: Vec::new(),
                backend: SearchBackend::Tantivy,
                index_updated: false,
                fallback_reason: None,
            });
        }

        let Some(index_dir) = self.index_dir.as_deref() else {
            return Ok(SearchResults {
                hits: self.search_ripgrep(query, limit)?,
                backend: SearchBackend::RipgrepFallback,
                index_updated: false,
                fallback_reason: Some("per-user Tantivy cache path is unavailable".to_owned()),
            });
        };
        match self.search_tantivy(index_dir, &needle, limit) {
            Ok((hits, index_updated)) => Ok(SearchResults {
                hits,
                backend: SearchBackend::Tantivy,
                index_updated,
                fallback_reason: None,
            }),
            Err(tantivy_error) => self
                .search_ripgrep(query, limit)
                .map(|hits| SearchResults {
                    hits,
                    backend: SearchBackend::RipgrepFallback,
                    index_updated: false,
                    fallback_reason: Some(format!("{tantivy_error:#}")),
                })
                .with_context(|| {
                    format!("Tantivy failed ({tantivy_error:#}) and ripgrep fallback also failed")
                }),
        }
    }

    fn search_ripgrep(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let terms = query_terms(&needle);
        let mut literals = terms.clone();
        if !literals.iter().any(|term| term == &needle) {
            literals.push(needle.clone());
        }
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(true)
            .fixed_strings(true)
            .build_literals(&literals)
            .context("failed to build ripgrep literal matcher")?;
        let mut walker = WalkBuilder::new(&self.root);
        let walk_root = self.root.clone();
        walker
            .standard_filters(false)
            .follow_links(false)
            .threads(ripgrep_search_threads())
            .filter_entry(move |entry| searchable_markdown_entry(entry, &walk_root));

        let (sender, receiver) = mpsc::channel::<Result<Option<SearchHit>, String>>();
        let root = self.root.clone();
        walker.build_parallel().run(|| {
            let sender = sender.clone();
            let matcher = matcher.clone();
            let root = root.clone();
            let needle = needle.clone();
            let terms = terms.clone();
            let mut searcher = SearcherBuilder::new().line_number(true).build();
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        let _ = sender.send(Err(format!("ripgrep walk failed: {error}")));
                        return WalkState::Continue;
                    }
                };
                let Some(file_type) = entry.file_type() else {
                    return WalkState::Continue;
                };
                if !file_type.is_file() || !is_markdown_path(entry.path()) {
                    return WalkState::Continue;
                }
                let path = entry.into_path();
                let mut found_match = false;
                let search_result = searcher.search_path(
                    &matcher,
                    &path,
                    Bytes(|_, _| {
                        found_match = true;
                        Ok(false)
                    }),
                );
                if let Err(error) = search_result {
                    let _ = sender.send(Err(format!(
                        "ripgrep could not search {}: {error}",
                        path.display()
                    )));
                    return WalkState::Continue;
                }
                if found_match {
                    let hit = rank_markdown_hit(&root, &path, &needle, &terms)
                        .map_err(|error| format!("{error:#}"));
                    let _ = sender.send(hit);
                }
                WalkState::Continue
            })
        });
        drop(sender);

        let mut hits = Vec::new();
        let mut errors = Vec::new();
        for result in receiver {
            match result {
                Ok(Some(hit)) => hits.push(hit),
                Ok(None) => {}
                Err(error) => errors.push(error),
            }
        }
        if !errors.is_empty() {
            errors.sort();
            bail!(errors.remove(0));
        }
        sort_and_truncate(&mut hits, limit);
        Ok(hits)
    }

    fn search_tantivy(
        &self,
        index_dir: &Path,
        needle: &str,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        let terms = query_terms(needle);
        if terms.is_empty() {
            bail!("query has no indexable terms");
        }

        let files = scan_markdown_files_parallel(&self.root)?;
        let manifest = IndexManifest::from_files(&files);
        fs::create_dir_all(index_dir)
            .with_context(|| format!("failed to create Tantivy cache {}", index_dir.display()))?;
        let (schema, fields) = search_schema();
        let directory = MmapDirectory::open(index_dir)
            .with_context(|| format!("failed to open Tantivy cache {}", index_dir.display()))?;
        let index = Index::open_or_create(directory, schema.clone()).with_context(|| {
            format!(
                "failed to open or create Tantivy index {}",
                index_dir.display()
            )
        })?;
        if index.schema() != schema {
            bail!(
                "Tantivy schema mismatch in {}; remove this disposable cache to rebuild it",
                index_dir.display()
            );
        }

        let manifest_path = index_dir.join("bibiiwiki-manifest.json");
        let previous_manifest = read_index_manifest(&manifest_path);
        let index_updated = previous_manifest.as_ref() != Some(&manifest);
        if index_updated {
            rebuild_tantivy_index(&index, fields, &files)?;
            write_index_manifest(&manifest_path, &manifest)?;
        }

        let reader = index.reader().context("failed to create Tantivy reader")?;
        let searcher = reader.searcher();
        let query = BooleanQuery::new_multiterms_query(
            terms
                .iter()
                .map(|term| Term::from_field_text(fields.terms, term))
                .collect(),
        );
        let result_limit = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if result_limit == 0 {
            return Ok((Vec::new(), index_updated));
        }
        let documents = searcher
            .search(&query, &TopDocs::with_limit(result_limit).order_by_score())
            .context("Tantivy query failed")?;
        let mut hits = Vec::with_capacity(documents.len().min(limit));
        for (_, address) in documents {
            let document: TantivyDocument = searcher
                .doc(address)
                .context("failed to load a Tantivy result")?;
            let path = stored_text(&document, fields.path, "path")?;
            let title = stored_text(&document, fields.title, "title")?;
            let body = stored_text(&document, fields.body, "body")?;
            if let Some(hit) = rank_indexed_hit(path, title, body, needle, &terms) {
                hits.push(hit);
            }
        }
        sort_and_truncate(&mut hits, limit);
        Ok((hits, index_updated))
    }
}

#[derive(Clone, Copy)]
struct SearchFields {
    path: Field,
    title: Field,
    body: Field,
    terms: Field,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FileStamp {
    bytes: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug)]
struct IndexedFile {
    absolute: PathBuf,
    relative: String,
    stamp: FileStamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct IndexManifest {
    version: u32,
    files: BTreeMap<String, FileStamp>,
}

impl IndexManifest {
    fn from_files(files: &[IndexedFile]) -> Self {
        Self {
            version: SEARCH_INDEX_VERSION,
            files: files
                .iter()
                .map(|file| (file.relative.clone(), file.stamp.clone()))
                .collect(),
        }
    }
}

fn search_schema() -> (Schema, SearchFields) {
    let mut builder = Schema::builder();
    let path = builder.add_text_field("path", STRING | STORED);
    let title = builder.add_text_field("title", STORED);
    let body = builder.add_text_field("body", STORED);
    let term_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::Basic),
    );
    let terms = builder.add_text_field("terms", term_options);
    let schema = builder.build();
    (
        schema,
        SearchFields {
            path,
            title,
            body,
            terms,
        },
    )
}

fn rebuild_tantivy_index(index: &Index, fields: SearchFields, files: &[IndexedFile]) -> Result<()> {
    let threads = ripgrep_search_threads().min(4);
    let memory_budget = INDEX_WRITER_BYTES_PER_THREAD.saturating_mul(threads);
    let mut writer = index
        .writer_with_num_threads::<TantivyDocument>(threads, memory_budget)
        .context("failed to acquire Tantivy index writer")?;
    writer
        .delete_all_documents()
        .context("failed to clear stale Tantivy documents")?;
    for file in files {
        let content = fs::read_to_string(&file.absolute)
            .with_context(|| format!("failed to read Markdown file {}", file.absolute.display()))?;
        let (title, body) = match parse_page(&content) {
            Ok(page) => (page.title, page.body),
            Err(_) => (plain_markdown_title(&file.absolute, &content), content),
        };
        let terms = indexed_terms(&title, &body);
        writer
            .add_document(doc!(
                fields.path => file.relative.as_str(),
                fields.title => title,
                fields.body => body,
                fields.terms => terms,
            ))
            .with_context(|| format!("failed to index {}", file.absolute.display()))?;
    }
    writer.commit().context("failed to commit Tantivy index")?;
    writer
        .wait_merging_threads()
        .context("failed while finalizing Tantivy index segments")?;
    Ok(())
}

fn read_index_manifest(path: &Path) -> Option<IndexManifest> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_index_manifest(path: &Path, manifest: &IndexManifest) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(manifest).context("failed to serialize search manifest")?;
    fs::write(path, bytes)
        .with_context(|| format!("failed to write search manifest {}", path.display()))
}

fn scan_markdown_files_parallel(root: &Path) -> Result<Vec<IndexedFile>> {
    let mut walker = WalkBuilder::new(root);
    let walk_root = root.to_path_buf();
    walker
        .standard_filters(false)
        .follow_links(false)
        .threads(ripgrep_search_threads())
        .filter_entry(move |entry| searchable_markdown_entry(entry, &walk_root));
    let (sender, receiver) = mpsc::channel::<Result<Option<IndexedFile>, String>>();
    let root = root.to_path_buf();
    walker.build_parallel().run(|| {
        let sender = sender.clone();
        let root = root.clone();
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let _ = sender.send(Err(format!("ripgrep walk failed: {error}")));
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file())
                || !is_markdown_path(entry.path())
            {
                return WalkState::Continue;
            }
            let absolute = entry.into_path();
            let result = indexed_file(&root, absolute)
                .map(Some)
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            WalkState::Continue
        })
    });
    drop(sender);

    let mut files = Vec::new();
    let mut errors = Vec::new();
    for result in receiver {
        match result {
            Ok(Some(file)) => files.push(file),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        errors.sort();
        bail!(errors.remove(0));
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

fn indexed_file(root: &Path, absolute: PathBuf) -> Result<IndexedFile> {
    let relative = absolute
        .strip_prefix(root)
        .with_context(|| format!("wiki page escaped the root: {}", absolute.display()))?;
    let metadata = fs::metadata(&absolute)
        .with_context(|| format!("failed to inspect {}", absolute.display()))?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    Ok(IndexedFile {
        relative: slash_path(relative),
        absolute,
        stamp: FileStamp {
            bytes: metadata.len(),
            modified_nanos,
        },
    })
}

fn searchable_markdown_entry(entry: &ignore::DirEntry, root: &Path) -> bool {
    let Some(file_type) = entry.file_type() else {
        return true;
    };
    if file_type.is_dir() {
        if entry.path() == root {
            return true;
        }
        return !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name == ".git" || name == "target");
    }
    !file_type.is_file() || is_markdown_path(entry.path())
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
}

fn default_index_dir(root: &Path) -> Option<PathBuf> {
    let stable_root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/");
    #[cfg(windows)]
    let stable_root = stable_root.to_lowercase();
    let digest = Sha256::digest(stable_root.as_bytes());
    let mut key = String::with_capacity(24);
    for byte in digest.iter().take(12) {
        write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Config::user_search_index_root()
        .ok()
        .map(|root| root.join(format!("v{SEARCH_INDEX_VERSION}")).join(key))
}

fn indexed_terms(title: &str, body: &str) -> String {
    let text = format!("{title}\n{body}").to_lowercase();
    let mut terms = Vec::new();
    let mut ascii = String::new();
    let mut cjk = String::new();
    for character in text.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_' {
            flush_index_cjk(&mut cjk, &mut terms);
            ascii.push(character);
        } else if is_cjk(character) {
            flush_ascii(&mut ascii, &mut terms);
            cjk.push(character);
        } else {
            flush_ascii(&mut ascii, &mut terms);
            flush_index_cjk(&mut cjk, &mut terms);
        }
    }
    let mut seen = HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms.join(" ")
}

fn flush_index_cjk(value: &mut String, terms: &mut Vec<String>) {
    let characters = value.chars().collect::<Vec<_>>();
    for width in 1..=characters.len().min(4) {
        for window in characters.windows(width) {
            terms.push(window.iter().collect());
        }
    }
    value.clear();
}

fn stored_text<'a>(document: &'a TantivyDocument, field: Field, name: &str) -> Result<&'a str> {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .with_context(|| format!("Tantivy result is missing stored {name}"))
}

fn rank_indexed_hit(
    path: &str,
    title: &str,
    body: &str,
    needle: &str,
    terms: &[String],
) -> Option<SearchHit> {
    let title_lower = title.to_lowercase();
    let body_lower = body.to_lowercase();
    let mut score =
        title_lower.matches(needle).count() * 12 + body_lower.matches(needle).count() * 4;
    score += terms
        .iter()
        .map(|term| title_lower.matches(term).count() * 3 + body_lower.matches(term).count())
        .sum::<usize>();
    if score == 0 {
        return None;
    }
    let excerpt_term = terms
        .iter()
        .find(|term| body_lower.contains(String::as_str(term)))
        .map_or(needle, String::as_str);
    Some(SearchHit {
        path: path.to_owned(),
        title: title.to_owned(),
        score,
        excerpt: excerpt(body, excerpt_term, 240),
    })
}

fn sort_and_truncate(hits: &mut Vec<SearchHit>, limit: usize) {
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    hits.truncate(limit);
}

fn rank_markdown_hit(
    root: &Path,
    path: &Path,
    needle: &str,
    terms: &[String],
) -> Result<Option<SearchHit>> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("wiki page escaped the root: {}", path.display()))?;
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read Markdown file {}", path.display()))?;
    let (title, body) = match parse_page(&content) {
        Ok(page) => (page.title, page.body),
        Err(_) => (plain_markdown_title(path, &content), content),
    };
    Ok(rank_indexed_hit(
        &slash_path(relative),
        &title,
        &body,
        needle,
        terms,
    ))
}

fn plain_markdown_title(path: &Path, content: &str) -> String {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .map_or_else(
            || {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Untitled")
                    .to_owned()
            },
            str::to_owned,
        )
}

fn ripgrep_search_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, usize::from)
        .max(2)
}

pub(crate) fn markdown_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to read directory {}", directory.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("failed to enumerate directory {}", directory.display())
            })?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("md")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii = String::new();
    let mut cjk = String::new();
    for character in query.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_' {
            flush_cjk(&mut cjk, &mut terms);
            ascii.push(character);
        } else if is_cjk(character) {
            flush_ascii(&mut ascii, &mut terms);
            cjk.push(character);
        } else {
            flush_ascii(&mut ascii, &mut terms);
            flush_cjk(&mut cjk, &mut terms);
        }
    }
    let mut seen = HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms
}

fn flush_ascii(value: &mut String, terms: &mut Vec<String>) {
    if value.len() >= 2 {
        terms.push(std::mem::take(value));
    } else {
        value.clear();
    }
}

fn flush_cjk(value: &mut String, terms: &mut Vec<String>) {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() <= 4 {
        if !characters.is_empty() {
            terms.push(value.clone());
        }
    } else {
        for width in 2..=4 {
            for window in characters.windows(width) {
                terms.push(window.iter().collect());
            }
        }
    }
    value.clear();
}

fn is_cjk(character: char) -> bool {
    ('\u{3400}'..='\u{4dbf}').contains(&character) || ('\u{4e00}'..='\u{9fff}').contains(&character)
}

fn excerpt(body: &str, needle: &str, limit: usize) -> String {
    let flattened = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flattened.to_lowercase();
    let byte_start = lower.find(needle).unwrap_or(0);
    let character_start = lower[..byte_start]
        .chars()
        .count()
        .saturating_sub(limit / 4);
    flattened
        .chars()
        .skip(character_start)
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cjk_terms_match_python_ngram_contract() {
        assert_eq!(query_terms("动量因子"), vec!["动量因子".to_string()]);
        assert_eq!(query_terms("动量因子风险").len(), 12);
    }

    #[test]
    fn ripgrep_searches_root_and_nested_markdown_in_parallel() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ripgrep-search-{unique}"));
        fs::create_dir_all(root.join("wiki/factors")).expect("create search fixture");
        fs::write(
            root.join("purpose.md"),
            "# Purpose\n\nThe purpose is fast knowledge retrieval.",
        )
        .expect("write root Markdown");
        fs::write(
            root.join("wiki/factors/momentum.markdown"),
            "# Momentum Factor\n\nMomentum uses prior returns.",
        )
        .expect("write nested Markdown");
        fs::write(root.join("wiki/factors/ignored.txt"), "purpose momentum")
            .expect("write ignored text");

        let root_hits = WikiSearch::new(root.clone())
            .search("purpose", 10)
            .expect("search root Markdown");
        assert!(root_hits.iter().any(|hit| hit.path == "purpose.md"));
        assert!(root_hits.iter().all(|hit| {
            !Path::new(&hit.path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
        }));

        let nested_hits = WikiSearch::new(root.clone())
            .search("momentum", 10)
            .expect("search nested Markdown");
        assert!(
            nested_hits
                .iter()
                .any(|hit| hit.path == "wiki/factors/momentum.markdown")
        );
        assert!(ripgrep_search_threads() >= 2);

        fs::remove_dir_all(root).expect("remove ripgrep search fixture");
    }
}
