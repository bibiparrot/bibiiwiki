use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

use super::{SearchHit, TemplateLibrary, WikiSearch, parse_page};

#[derive(Debug)]
pub struct WikiQueryService {
    wiki_dir: PathBuf,
    templates: TemplateLibrary,
}

impl WikiQueryService {
    /// Creates a query service over one wiki content directory.
    ///
    /// # Errors
    ///
    /// Returns an error if bundled templates cannot be compiled.
    pub fn new(wiki_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            wiki_dir,
            templates: TemplateLibrary::load()?,
        })
    }

    /// Retrieves ranked local evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when a page cannot be read or parsed.
    pub fn search(&self, question: &str, limit: usize) -> Result<Vec<SearchHit>> {
        WikiSearch::new(self.wiki_dir.clone()).search(question, limit)
    }

    /// Renders the deterministic no-LLM answer used by the Python CLI.
    ///
    /// # Errors
    ///
    /// Returns an error if the bundled template cannot be rendered.
    pub fn local_answer(&self, hits: &[SearchHit]) -> Result<String> {
        self.templates
            .render("wiki/queries/local_answer.md.j2", &json!({"hits": hits}))
            .map(|value| value.trim().to_string())
    }

    /// Renders a deterministic answer whose excerpts are recentered around
    /// the question's domain terms and formula-like context. This is used for
    /// offline queries and as a graceful fallback when the configured model is
    /// unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error when a matched page cannot be read or parsed, or when
    /// the local-answer template cannot be rendered.
    pub fn grounded_answer(&self, question: &str, hits: &[SearchHit]) -> Result<String> {
        let mut grounded = Vec::with_capacity(hits.len());
        for hit in hits {
            let path = self.wiki_dir.join(&hit.path);
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read evidence page {}", path.display()))?;
            let page = parse_page(&content)
                .with_context(|| format!("invalid frontmatter in {}", path.display()))?;
            let mut hit = hit.clone();
            hit.excerpt = grounded_excerpt(&page.body, question, &hit.excerpt, 520);
            grounded.push(hit);
        }
        self.local_answer(&grounded)
    }

    /// Builds a bounded, page-cited Codex prompt from retrieved evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when evidence pages cannot be read/parsed or the prompt
    /// template cannot be rendered.
    pub fn codex_prompt(&self, question: &str, hits: &[SearchHit]) -> Result<String> {
        let mut entries = Vec::new();
        let mut remaining = 24_000_usize;
        for hit in hits {
            let path = self.wiki_dir.join(&hit.path);
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read evidence page {}", path.display()))?;
            let page = parse_page(&content)
                .with_context(|| format!("invalid frontmatter in {}", path.display()))?;
            let body: String = page.body.chars().take(remaining.min(4_000)).collect();
            entries.push(
                self.templates
                    .render(
                        "agent/wiki_query_context_entry.md.j2",
                        &json!({
                            "target": hit.path.strip_suffix(".md").unwrap_or(&hit.path),
                            "title": hit.title,
                            "body": body.trim_end()
                        }),
                    )?
                    .trim()
                    .to_string(),
            );
            remaining = remaining.saturating_sub(body.chars().count());
            if remaining == 0 {
                break;
            }
        }
        let context = if entries.is_empty() {
            "No matching wiki pages were retrieved.".to_string()
        } else {
            entries.join("\n\n")
        };
        self.templates
            .render(
                "agent/wiki_query.md.j2",
                &json!({"question": question.trim(), "wiki_context": context}),
            )
            .map(|value| value.trim().to_string())
    }
}

fn grounded_excerpt(body: &str, question: &str, fallback: &str, limit: usize) -> String {
    let flattened = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flattened.to_lowercase();
    let keywords = domain_keywords(question);
    let mut best: Option<(usize, String)> = None;
    for keyword in &keywords {
        for (byte_index, _) in lower.match_indices(keyword) {
            let character_index = lower[..byte_index].chars().count();
            let start = character_index.saturating_sub(limit / 3);
            let candidate = flattened
                .chars()
                .skip(start)
                .take(limit)
                .collect::<String>();
            let candidate_lower = candidate.to_lowercase();
            let matched_terms = keywords
                .iter()
                .filter(|term| candidate_lower.contains(term.as_str()))
                .count();
            let cue_score = [
                ("given by", 18),
                ("example", 8),
                ("expression", 8),
                ("formula", 8),
                ("ln(", 18),
                ("rank(", 14),
                ("delta(", 14),
                ("表达式", 12),
                ("公式", 12),
                ("=", 5),
            ]
            .iter()
            .map(|(cue, weight)| candidate_lower.matches(cue).count() * weight)
            .sum::<usize>();
            let score = matched_terms * 24 + keyword.chars().count() + cue_score;
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score > *best_score)
            {
                best = Some((score, candidate));
            }
        }
    }
    best.map_or_else(|| fallback.to_string(), |(_, excerpt)| excerpt)
}

fn domain_keywords(question: &str) -> Vec<String> {
    const INTENT_WORDS: &[&str] = &[
        "a",
        "an",
        "and",
        "define",
        "defined",
        "definition",
        "executable",
        "expression",
        "factor",
        "for",
        "give",
        "how",
        "is",
        "of",
        "please",
        "show",
        "the",
        "what",
    ];
    let mut words = question
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| word.chars().count() >= 2 && !INTENT_WORDS.contains(word))
        .map(str::to_string)
        .collect::<Vec<_>>();
    words.sort();
    words.dedup();
    if words.is_empty() {
        words = question
            .to_lowercase()
            .split_whitespace()
            .map(|word| word.trim_matches(|character: char| !character.is_alphanumeric()))
            .filter(|word| word.chars().count() >= 2)
            .map(str::to_string)
            .collect();
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grounded_excerpt_prefers_the_formula_bearing_momentum_passage() {
        let body = format!(
            "Momentum is mentioned in a broad introduction. {} A simple example of a momentum alpha is given by ln(yesterday_close / yesterday_open). The trend may continue today.",
            "background ".repeat(100)
        );

        let excerpt = grounded_excerpt(
            &body,
            "What is the executable expression of a momentum factor?",
            "fallback",
            520,
        );

        assert!(excerpt.contains("ln(yesterday_close / yesterday_open)"));
        assert!(!excerpt.starts_with("Momentum is mentioned"));
    }
}
