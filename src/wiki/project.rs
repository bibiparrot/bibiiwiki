use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

use super::{Authority, TemplateLibrary, WikiMaintainer, WikiPage, WikiStore};

#[derive(Debug)]
pub struct WikiProject {
    root: PathBuf,
    templates: TemplateLibrary,
}

impl WikiProject {
    /// Opens a wiki project root.
    ///
    /// # Errors
    ///
    /// Returns an error if the bundled templates cannot be compiled.
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
            templates: TemplateLibrary::load()?,
        })
    }

    /// Creates the project contract only when the selected root is not already
    /// a complete BIBIIWIKI workspace.
    ///
    /// Returns `true` when initialization was required and `false` when the
    /// existing project was ready for ingest.
    ///
    /// # Errors
    ///
    /// Returns an error when a missing project component cannot be created.
    pub fn ensure_initialized(&self) -> Result<bool> {
        if self.is_initialized() {
            return Ok(false);
        }
        self.initialize()?;
        Ok(true)
    }

    fn is_initialized(&self) -> bool {
        let wiki_dir = self.root.join("wiki");
        self.root.join("purpose.md").is_file()
            && self.root.join("schema.md").is_file()
            && wiki_dir.join("index.md").is_file()
            && components()
                .iter()
                .all(|(directory, _, _)| wiki_dir.join(directory).join("index.md").is_file())
            && ["backtest", "calculated", "experience"]
                .iter()
                .all(|directory| wiki_dir.join(directory).is_dir())
            && !wiki_dir.join("exprience").exists()
    }

    /// Creates the durable project documents and component indexes.
    ///
    /// Existing active documents retain human authority through `WikiStore`.
    ///
    /// # Errors
    ///
    /// Returns an error when a directory, template, or page cannot be created.
    fn initialize(&self) -> Result<()> {
        let wiki_dir = self.root.join("wiki");
        let root_store = WikiStore::new(self.root.clone());
        let store = WikiStore::new(wiki_dir.clone());
        root_store.write_page(
            "purpose.md",
            &WikiPage {
                title: "目的 (Purpose)".to_string(),
                body: self
                    .templates
                    .render("wiki/project/purpose.md.j2", &json!({}))?,
                status: "active".to_string(),
                page_type: "purpose".to_string(),
                tags: vec!["project".to_string(), "purpose".to_string()],
                ..WikiPage::default()
            },
            Authority::Human,
        )?;
        root_store.write_page(
            "schema.md",
            &WikiPage {
                title: "Wiki模式 (Wiki Schema)".to_string(),
                body: self.templates.render(
                    "wiki/project/schema.md.j2",
                    &json!({"filename_replacements": filename_replacements()}),
                )?,
                status: "active".to_string(),
                page_type: "schema".to_string(),
                tags: vec!["project".to_string(), "schema".to_string()],
                ..WikiPage::default()
            },
            Authority::Human,
        )?;

        for (directory, title, scope) in components() {
            fs::create_dir_all(wiki_dir.join(directory)).with_context(|| {
                format!("failed to create wiki component directory {directory}")
            })?;
            store.write_page(
                &format!("{directory}/index.md"),
                &WikiPage {
                    title: title.to_string(),
                    body: self.templates.render(
                        "wiki/project/component_index.md.j2",
                        &json!({"scope": scope, "directory": directory}),
                    )?,
                    status: "active".to_string(),
                    page_type: "index".to_string(),
                    tags: vec![directory.to_string(), "index".to_string()],
                    ..WikiPage::default()
                },
                Authority::Human,
            )?;
        }
        for directory in ["backtest", "calculated", "experience"] {
            fs::create_dir_all(wiki_dir.join(directory)).with_context(|| {
                format!("failed to create wiki component directory {directory}")
            })?;
        }
        migrate_legacy_experience(&wiki_dir)?;
        WikiMaintainer::new(wiki_dir).update(false)?;
        Ok(())
    }
}

fn migrate_legacy_experience(wiki_dir: &std::path::Path) -> Result<()> {
    let legacy = wiki_dir.join("exprience");
    if !legacy.is_dir() {
        return Ok(());
    }
    merge_directory(&legacy, &wiki_dir.join("experience"))
}

fn merge_directory(source: &std::path::Path, target: &std::path::Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to list legacy directory {}", source.display()))?
    {
        let path = entry?.path();
        let destination = target.join(
            path.file_name()
                .context("legacy experience entry has no file name")?,
        );
        if path.is_dir() {
            merge_directory(&path, &destination)?;
        } else if destination.exists() {
            let fallback = unused_legacy_destination(&destination);
            fs::rename(&path, &fallback).with_context(|| {
                format!(
                    "failed to preserve {} as {}",
                    path.display(),
                    fallback.display()
                )
            })?;
        } else {
            fs::rename(&path, &destination).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    path.display(),
                    destination.display()
                )
            })?;
        }
    }
    fs::remove_dir(source)
        .with_context(|| format!("failed to remove legacy directory {}", source.display()))
}

fn unused_legacy_destination(destination: &std::path::Path) -> PathBuf {
    let parent = destination
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = destination
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("experience");
    let extension = destination
        .extension()
        .and_then(|value| value.to_str())
        .map_or_else(String::new, |value| format!(".{value}"));
    (1..=u32::MAX)
        .map(|index| parent.join(format!("{stem}_legacy_{index}{extension}")))
        .find(|candidate| !candidate.exists())
        .expect("an unused legacy experience path exists")
}

fn components() -> [(&'static str, &'static str, &'static str); 9] {
    [
        (
            "concepts",
            "概念索引 (Concept Index)",
            "理论、方法、技术与抽象概念 (theories, methods, techniques, and abstractions)",
        ),
        (
            "entities",
            "实体索引 (Entity Index)",
            "人物、组织、产品、数据集与工具 (people, organizations, products, datasets, and tools)",
        ),
        (
            "factors",
            "因子索引 (Factor Index)",
            "双语因子定义、公式、表达式与证据 (bilingual factor definitions, formulas, expressions, and evidence)",
        ),
        (
            "methodology",
            "方法论索引 (Methodology Index)",
            "来源支持的投资原则、规则与流程 (source-backed investment principles, rules, and processes)",
        ),
        (
            "sources",
            "来源索引 (Source Index)",
            "文档分析与逐字节来源追溯 (document analyses and byte-level provenance)",
        ),
        (
            "media",
            "媒体索引 (Media Index)",
            "源文档衍生媒体及其来源关系 (source-derived media and provenance relationships)",
        ),
        (
            "queries",
            "查询索引 (Query Index)",
            "持久化检索与基于 Wiki 证据的问答 (persisted searches and wiki-grounded queries)",
        ),
        (
            "mining",
            "因子挖掘索引 (Factor Mining Index)",
            "可恢复的因子提议、计算、回测与经验学习循环 (resumable factor proposal, calculation, backtest, and experience-learning loops)",
        ),
        (
            "opportunities",
            "机会索引 (Opportunity Index)",
            "宏观、行业、新闻催化与价格趋势共同支持的投资机会 (investment opportunities supported by macro, industry, news-catalyst, and price-trend evidence)",
        ),
    ]
}

fn filename_replacements() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/", "／"),
        (r"\", "＼"),
        (":", "："),
        ("*", "＊"),
        ("?", "？"),
        ("\"", "＂"),
        ("<", "＜"),
        (">", "＞"),
        ("|", "｜"),
        ("#", "＃"),
    ]
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn ensure_initialized_creates_a_blank_wiki_once() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("auto-initialize-{unique}"));
        let project = WikiProject::new(root.clone()).expect("project");

        assert!(project.ensure_initialized().expect("initialize blank wiki"));
        assert!(root.join("purpose.md").is_file());
        assert!(root.join("schema.md").is_file());
        assert!(root.join("wiki/index.md").is_file());
        assert!(
            !project
                .ensure_initialized()
                .expect("reuse initialized wiki")
        );

        fs::remove_dir_all(root).expect("test cleanup");
    }

    #[test]
    fn legacy_exprience_directory_is_merged_and_removed() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let wiki = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("experience-migration-{unique}"))
            .join("wiki");
        let legacy = wiki.join("exprience");
        let experience = wiki.join("experience");
        fs::create_dir_all(&legacy).expect("legacy directory");
        fs::create_dir_all(&experience).expect("experience directory");
        fs::write(legacy.join("positive.md"), "legacy positive").expect("legacy page");
        fs::write(experience.join("positive.md"), "current positive").expect("current page");

        migrate_legacy_experience(&wiki).expect("legacy experience migration");

        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(experience.join("positive.md")).expect("current page"),
            "current positive"
        );
        assert_eq!(
            fs::read_to_string(experience.join("positive_legacy_1.md"))
                .expect("preserved legacy page"),
            "legacy positive"
        );
        fs::remove_dir_all(wiki.parent().expect("test root")).expect("test cleanup");
    }
}
