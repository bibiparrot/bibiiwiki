use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::clock::now_beijing;
use super::lint::{WikiHealth, wikilinks};
use super::search::{markdown_files, slash_path};
use super::{
    Authority, TemplateLibrary, WikiLinter, WikiPage, WikiStore, parse_page, safe_filename_segment,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WikiMaintenanceResult {
    pub created_stubs: usize,
    pub repaired_links: usize,
    pub indexed_pages: usize,
    pub health: WikiHealth,
}

#[derive(Clone, Debug)]
pub struct WikiMaintainer {
    wiki_dir: PathBuf,
}

impl WikiMaintainer {
    #[must_use]
    pub fn new(wiki_dir: PathBuf) -> Self {
        Self { wiki_dir }
    }

    /// Repairs broken links with stubs and rebuilds the application-owned index.
    ///
    /// # Errors
    ///
    /// Returns an error when pages cannot be read, parsed, or written.
    pub fn update(&self, record_log: bool) -> Result<WikiMaintenanceResult> {
        let initial = WikiLinter::new(self.wiki_dir.clone()).check()?;
        let repairs: BTreeMap<String, String> = initial
            .broken_links
            .iter()
            .map(|item| (clean_target(&item.target), canonical_target(&item.target)))
            .collect();
        let created = self.create_stubs(&repairs, &initial)?;
        let repaired = self.repair_links(&repairs)?;
        let indexed = self.rebuild_index()?;
        let health = WikiLinter::new(self.wiki_dir.clone()).check()?;
        let result = WikiMaintenanceResult {
            created_stubs: created,
            repaired_links: repaired,
            indexed_pages: indexed,
            health,
        };
        if record_log {
            self.append_log(&result)?;
        }
        Ok(result)
    }

    fn append_log(&self, result: &WikiMaintenanceResult) -> Result<()> {
        let store = WikiStore::new(self.wiki_dir.clone());
        let existing = store.read_page("log.md")?;
        let templates = TemplateLibrary::load()?;
        let body = existing.as_ref().map_or_else(
            || templates.render("wiki/project/log_header.md.j2", &serde_json::json!({})),
            |page| Ok(page.body.clone()),
        )?;
        let entry = templates.render(
            "wiki/project/maintenance_log_entry.md.j2",
            &serde_json::json!({
                "event_date": now_beijing(),
                "result": {
                    "created_stubs": result.created_stubs,
                    "repaired_links": result.repaired_links,
                    "indexed_pages": result.indexed_pages,
                    "health": {
                        "broken_links": vec![(); result.health.broken_links.len()],
                        "unindexed": vec![(); result.health.unindexed.len()],
                    }
                }
            }),
        )?;
        store.write_page(
            "log.md",
            &WikiPage {
                title: "Wiki日志 (Wiki Log)".to_string(),
                body: format!("{}\n\n{}", body.trim_end(), entry.trim()),
                status: "active".to_string(),
                page_type: "log".to_string(),
                tags: vec!["log".to_string(), "append-only".to_string()],
                created: existing.map_or_else(String::new, |page| page.created),
                ..WikiPage::default()
            },
            Authority::Human,
        )?;
        Ok(())
    }

    fn create_stubs(
        &self,
        repairs: &BTreeMap<String, String>,
        health: &WikiHealth,
    ) -> Result<usize> {
        let mut sources: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for item in &health.broken_links {
            if item.source != "index.md" {
                sources
                    .entry(clean_target(&item.target))
                    .or_default()
                    .insert(item.source.clone());
            }
        }
        let store = WikiStore::new(self.wiki_dir.clone());
        let mut created = 0;
        for (original, target) in repairs {
            let Some(backlink_sources) = sources.get(original) else {
                continue;
            };
            let relative = format!("{target}.md");
            if self.wiki_dir.join(&relative).exists() {
                continue;
            }
            let backlinks = backlink_sources
                .iter()
                .map(|source| {
                    let clean = source.strip_suffix(".md").unwrap_or(source);
                    let title = Path::new(source)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or(source);
                    format!("- [[{clean}|{title}]]")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let (section, title) = target
                .split_once('/')
                .map_or(("concepts", original.as_str()), |(section, title)| {
                    (section, title)
                });
            store.write_page(
                &relative,
                &WikiPage {
                    title: title.to_string(),
                    body: stub_body(section, title, &backlinks),
                    status: "stub".to_string(),
                    page_type: section.strip_suffix('s').unwrap_or(section).to_string(),
                    tags: vec!["stub".to_string(), "link-repair".to_string()],
                    sources: backlink_sources.iter().cloned().collect(),
                    ..WikiPage::default()
                },
                Authority::Human,
            )?;
            created += 1;
        }
        Ok(created)
    }

    fn repair_links(&self, repairs: &BTreeMap<String, String>) -> Result<usize> {
        let mut repaired = 0;
        for path in markdown_files(&self.wiki_dir)? {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let links = wikilinks(&content);
            let mut output = String::with_capacity(content.len());
            let mut cursor = 0;
            for link in links {
                output.push_str(&content[cursor..link.start]);
                let (raw_target, anchor) = link
                    .target
                    .split_once('#')
                    .map_or((link.target.as_str(), ""), |(target, anchor)| {
                        (target, anchor)
                    });
                let cleaned = clean_target(raw_target);
                if let Some(canonical) = repairs.get(&cleaned)
                    && canonical != &cleaned
                {
                    let label = if link.alias.trim().is_empty() {
                        &cleaned
                    } else {
                        link.alias.trim()
                    };
                    let separator = if link.escaped_alias { r"\|" } else { "|" };
                    let anchor = if anchor.is_empty() {
                        String::new()
                    } else {
                        format!("#{anchor}")
                    };
                    write!(output, "[[{canonical}{anchor}{separator}{label}]]")
                        .expect("writing to String cannot fail");
                    repaired += 1;
                } else {
                    output.push_str(&content[link.start..link.end]);
                }
                cursor = link.end;
            }
            output.push_str(&content[cursor..]);
            if output != content {
                fs::write(&path, output)
                    .with_context(|| format!("failed to repair {}", path.display()))?;
            }
        }
        Ok(repaired)
    }

    fn rebuild_index(&self) -> Result<usize> {
        let mut groups: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        let index_path = self.wiki_dir.join("index.md");
        let pages: Vec<_> = markdown_files(&self.wiki_dir)?
            .into_iter()
            .filter(|path| *path != index_path)
            .collect();
        for path in &pages {
            let relative = path
                .strip_prefix(&self.wiki_dir)
                .expect("walked path remains inside wiki root");
            if relative
                .components()
                .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
            {
                continue;
            }
            let relative_text = slash_path(relative);
            let target = relative_text
                .strip_suffix(".md")
                .unwrap_or(&relative_text)
                .to_string();
            let content = fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let page = parse_page(&content)
                .with_context(|| format!("invalid frontmatter in {}", path.display()))?;
            let title = if page.title.trim().is_empty() {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string()
            } else {
                page.title.trim().to_string()
            };
            let group = relative
                .components()
                .next()
                .and_then(|part| part.as_os_str().to_str())
                .filter(|_| relative.components().count() > 1)
                .unwrap_or("other");
            groups.entry(group.to_string()).or_default().push((
                target,
                title,
                description(&page.body),
            ));
        }
        let mut body = String::new();
        for (group, mut entries) in groups {
            entries.sort();
            writeln!(body, "## {}\n", group_title(&group)).expect("writing to String cannot fail");
            for (target, title, description) in entries {
                writeln!(body, "- [[{target}|{title}]] — {description}")
                    .expect("writing to String cannot fail");
            }
            body.push('\n');
        }
        WikiStore::new(self.wiki_dir.clone()).write_page(
            "index.md",
            &WikiPage {
                title: "Wiki Index".to_string(),
                body,
                status: "active".to_string(),
                page_type: "index".to_string(),
                tags: vec!["index".to_string(), "generated".to_string()],
                ..WikiPage::default()
            },
            Authority::Human,
        )?;
        Ok(pages.len())
    }
}

fn clean_target(target: &str) -> String {
    let cleaned = target
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\\', "/")
        .trim_matches('/')
        .to_string();
    cleaned.strip_suffix(".md").unwrap_or(&cleaned).to_string()
}

fn canonical_target(target: &str) -> String {
    let mut cleaned = clean_target(target);
    if let Some(value) = cleaned.strip_prefix("wiki/") {
        cleaned = value.to_string();
    }
    if let Some(value) = cleaned.strip_prefix("概念/") {
        cleaned = format!("concepts/{value}");
    }
    let sections = [
        "concepts",
        "entities",
        "experience",
        "factors",
        "methodology",
        "opportunities",
        "queries",
        "sources",
    ];
    if let Some((section, remainder)) = cleaned.split_once('/')
        && sections.contains(&section)
    {
        return format!("{section}/{}", safe_filename_segment(remainder));
    }
    if sections.contains(&cleaned.as_str()) {
        cleaned
    } else {
        format!("concepts/{}", safe_filename_segment(&cleaned))
    }
}

fn stub_body(section: &str, title: &str, backlinks: &str) -> String {
    match section {
        "factors" => format!(
            "## 因子定义 (Definition)\n\nDefinition requires source review.\n\n## 因子公式 (Formula)\n\n$$\n\\operatorname{{{title}}} = f(\\text{{inputs}})\n$$\n\nStatus: `needs-review`\n\n## 引用来源 (Referenced By)\n\n{backlinks}\n"
        ),
        "sources" => format!(
            "## 摘要 (Summary)\n\nThis source is cited by the wiki, but the original is unavailable.\n\n## 引用来源 (Referenced By)\n\n{backlinks}\n"
        ),
        _ => format!(
            "This page is a link-repair placeholder. Its definition still requires review.\n\n## 引用来源 (Referenced By)\n\n{backlinks}\n"
        ),
    }
}

fn group_title(group: &str) -> &str {
    match group {
        "backtest" => "回测 (Backtest)",
        "calculated" => "计算记录 (Calculated)",
        "concepts" => "概念 (Concepts)",
        "entities" => "实体 (Entities)",
        "experience" => "经验 (Experience)",
        "factors" => "因子 (Factors)",
        "media" => "媒体 (Media)",
        "methodology" => "投资方法论 (Methodology)",
        "mining" => "因子挖掘 (Factor Mining)",
        "opportunities" => "机会 (Opportunities)",
        "queries" => "查询 (Queries)",
        "sources" => "来源 (Sources)",
        _ => "其他 (Other)",
    }
}

fn description(body: &str) -> String {
    let mut in_fence = false;
    for raw in body.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence
            && !line.is_empty()
            && !line.starts_with(['#', '-', '*', '|'])
            && !line.starts_with("<!--")
        {
            let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
            return if compact.chars().count() <= 140 {
                compact
            } else {
                format!(
                    "{}…",
                    compact.chars().take(139).collect::<String>().trim_end()
                )
            };
        }
    }
    "No summary available.".to_string()
}
