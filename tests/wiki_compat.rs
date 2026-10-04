use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use bibiiwiki::codex::{CodexPromptRequest, prompt as codex_prompt};
use bibiiwiki::config::{ChunkingConfig, CodexConfig, Config, ServerConfig};
use bibiiwiki::wiki::{
    Authority, BilingualName, FactorCatalogReader, QueryArtifactService, RawSourceBackup,
    SearchBackend, SearchHit, TemplateLibrary, WikiIngestOptions, WikiIngestPipeline,
    WikiMaintainer, WikiPage, WikiSearch, WikiStore,
};
use litellm_core::router::{Deployment, LiteLLMParams};
use serde_json::json;

fn python_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_wiki_root")
}

#[test]
#[ignore = "requires the local Python golden wiki fixture, which is not distributed"]
fn search_ranking_matches_python_golden_vault() {
    let hits = WikiSearch::new(python_fixture().join("wiki"))
        .search("momentum", 5)
        .expect("golden wiki should be searchable");

    assert_eq!(hits.len(), 5);
    assert_eq!(hits[0].path, "mining/loop_20260818T222331+0800.md");
    assert_eq!(hits[0].score, 3645);
}

#[test]
fn wiki_search_builds_and_reuses_a_tantivy_index_without_changing_ranked_hits() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-tantivy-wiki-{unique}"));
    let index = std::env::temp_dir().join(format!("bibiiwiki-tantivy-index-{unique}"));
    fs::create_dir_all(root.join("wiki/factors")).expect("search fixture");
    fs::write(
        root.join("wiki/factors/momentum.md"),
        "# Momentum Factor\n\nMomentum ranks assets by prior returns.",
    )
    .expect("write search fixture");

    let search = WikiSearch::new(root.clone()).with_index_dir(index.clone());
    let first = search
        .search_detailed("momentum", 10)
        .expect("first indexed search");
    assert_eq!(first.backend, SearchBackend::Tantivy);
    assert_eq!(first.hits[0].path, "wiki/factors/momentum.md");
    assert!(first.index_updated);

    let second = search
        .search_detailed("momentum", 10)
        .expect("reused indexed search");
    assert_eq!(second.backend, SearchBackend::Tantivy);
    assert_eq!(second.hits, first.hits);
    assert!(!second.index_updated);

    fs::write(
        root.join("wiki/factors/value.md"),
        "# Value Factor\n\nValue and momentum can complement one another.",
    )
    .expect("write changed search fixture");
    let refreshed = search
        .search_detailed("momentum", 10)
        .expect("refreshed indexed search");
    assert_eq!(refreshed.backend, SearchBackend::Tantivy);
    assert!(refreshed.index_updated);
    assert_eq!(refreshed.hits.len(), 2);

    fs::remove_file(root.join("wiki/factors/momentum.md")).expect("remove indexed page");
    let after_removal = search
        .search_detailed("momentum", 10)
        .expect("search after indexed page removal");
    assert_eq!(after_removal.backend, SearchBackend::Tantivy);
    assert!(after_removal.index_updated);
    assert_eq!(after_removal.hits.len(), 1);
    assert_eq!(after_removal.hits[0].path, "wiki/factors/value.md");

    fs::remove_dir_all(root).expect("wiki fixture cleanup");
    fs::remove_dir_all(index).expect("index fixture cleanup");
}

#[test]
fn wiki_search_falls_back_to_ripgrep_when_the_tantivy_cache_is_unavailable() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-fallback-wiki-{unique}"));
    let blocked_index = std::env::temp_dir().join(format!("bibiiwiki-blocked-index-{unique}"));
    fs::create_dir_all(&root).expect("search fixture");
    fs::write(
        root.join("momentum.md"),
        "# Momentum Factor\n\nMomentum ranks assets by prior returns.",
    )
    .expect("write search fixture");
    fs::write(&blocked_index, "not a directory").expect("block index directory");

    let results = WikiSearch::new(root.clone())
        .with_index_dir(blocked_index.clone())
        .search_detailed("momentum", 10)
        .expect("ripgrep should recover the search");
    assert_eq!(results.backend, SearchBackend::RipgrepFallback);
    assert_eq!(results.hits[0].path, "momentum.md");
    assert!(results.fallback_reason.is_some());

    fs::remove_dir_all(root).expect("wiki fixture cleanup");
    fs::remove_file(blocked_index).expect("blocked index cleanup");
}

#[test]
fn tantivy_search_supports_cjk_factor_terms() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-cjk-wiki-{unique}"));
    let index = std::env::temp_dir().join(format!("bibiiwiki-cjk-index-{unique}"));
    fs::create_dir_all(root.join("wiki/factors")).expect("search fixture");
    fs::write(
        root.join("wiki/factors/momentum.md"),
        "# 动量因子 (Momentum Factor)\n\nMomentum ranks assets by prior returns.",
    )
    .expect("write CJK fixture");

    let results = WikiSearch::new(root.clone())
        .with_index_dir(index.clone())
        .search_detailed("动量因子", 10)
        .expect("CJK indexed search");
    assert_eq!(results.backend, SearchBackend::Tantivy);
    assert_eq!(results.hits[0].path, "wiki/factors/momentum.md");

    let partial = WikiSearch::new(root.clone())
        .with_index_dir(index.clone())
        .search_detailed("动量", 10)
        .expect("partial CJK indexed search");
    assert_eq!(partial.backend, SearchBackend::Tantivy);
    assert_eq!(partial.hits[0].path, "wiki/factors/momentum.md");

    fs::remove_dir_all(root).expect("wiki fixture cleanup");
    fs::remove_dir_all(index).expect("index fixture cleanup");
}

#[test]
fn agent_cannot_replace_an_active_page_body() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-store-{unique}"));
    let store = WikiStore::new(root.clone());
    let original = WikiPage {
        title: "动量 (Momentum)".to_string(),
        body: "Human-reviewed definition.\n".to_string(),
        status: "active".to_string(),
        page_type: "factor".to_string(),
        ..WikiPage::default()
    };
    store
        .write_page("factors/momentum.md", &original, Authority::Human)
        .expect("human write should succeed");

    let mut replacement = original.clone();
    replacement.body = "Agent replacement.\n".to_string();
    let error = store
        .write_page("factors/momentum.md", &replacement, Authority::Agent)
        .expect_err("agent must not overwrite active content");
    assert!(
        error
            .to_string()
            .contains("Active page requires human review")
    );
    assert_eq!(
        store
            .read_page("factors/momentum.md")
            .expect("read should succeed")
            .expect("page should exist")
            .body,
        original.body
    );

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn copied_prompt_library_compiles_and_preserves_source_analysis_contract() {
    let templates = TemplateLibrary::load().expect("all copied templates should compile");
    assert_eq!(templates.names().len(), 46);

    let prompt = templates
        .render(
            "wiki/prompts/source_analysis_system.md.j2",
            &json!({
                "purpose": "Factor research",
                "schema": "Use factor and methodology pages.",
                "wiki_index": "- [[factors/ep|EP]]",
                "output_language": "English"
            }),
        )
        .expect("source-analysis prompt should render");
    for required in [
        "Return ONLY valid JSON",
        "dynamic methodology terms",
        "fixed translation map",
        "English_Chinese.md",
        "Do not invent",
        "Only link to slugs listed",
    ] {
        assert!(
            prompt.contains(required),
            "missing prompt contract: {required}"
        );
    }
    assert!(
        templates
            .render(
                "wiki/prompts/source_analysis_system.md.j2",
                &json!({"purpose": "incomplete"}),
            )
            .is_err(),
        "undefined template variables must fail"
    );
}

#[test]
fn wiki_search_cli_works_without_an_llm_configuration() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-cli-{unique}"));
    WikiStore::new(root.join("wiki"))
        .write_page(
            "factors/momentum.md",
            &WikiPage {
                title: "动量 (Momentum)".to_string(),
                body: "Historical returns rank assets by momentum.\n".to_string(),
                page_type: "factor".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("fixture page");

    let output = Command::new(env!("CARGO_BIN_EXE_bibiiwiki"))
        .args([
            "wiki",
            "search",
            "momentum",
            "--wiki-root",
            root.to_str().expect("UTF-8 temp path"),
            "--limit",
            "1",
            "--no-save",
        ])
        .output()
        .expect("wiki CLI should launch");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 CLI output");
    assert!(stdout.contains("动量 (Momentum)"));
    assert!(stdout.contains("factors/momentum.md"));

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn wiki_query_cli_can_answer_locally_without_configuration() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-query-{unique}"));
    WikiStore::new(root.join("wiki"))
        .write_page(
            "factors/momentum.md",
            &WikiPage {
                title: "动量 (Momentum)".to_string(),
                body: "Historical returns rank assets by momentum.\n".to_string(),
                page_type: "factor".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("fixture page");

    let output = Command::new(env!("CARGO_BIN_EXE_bibiiwiki"))
        .args([
            "wiki",
            "query",
            "How is momentum defined?",
            "--wiki-root",
            root.to_str().expect("UTF-8 temp path"),
            "--no-llm",
            "--no-save",
        ])
        .output()
        .expect("wiki query CLI should launch");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 CLI output");
    assert!(stdout.contains("Local wiki matches"));
    assert!(stdout.contains("[[factors/momentum|动量 (Momentum)]]"));

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn wiki_query_cli_falls_back_to_local_evidence_when_codex_is_unavailable() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-query-fallback-{unique}"));
    WikiStore::new(root.join("wiki"))
        .write_page(
            "factors/momentum.md",
            &WikiPage {
                title: "动量 (Momentum)".to_string(),
                body: "Executable expression: log(close[t-1] / open[t-1]).\n".to_string(),
                page_type: "factor".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("fixture page");
    let config = root.join("bibiiwiki.yaml");
    fs::write(
        &config,
        r"
server:
  bind: 127.0.0.1:0
  request_timeout_seconds: 1
model_list:
  - model_name: unavailable
    litellm_params:
      model: ollama/unavailable
      api_base: http://127.0.0.1:11434/v1
codex:
  binary: definitely-missing-bibiiwiki-codex
  model: unavailable
  reasoning_effort: none
",
    )
    .expect("test configuration");

    let output = Command::new(env!("CARGO_BIN_EXE_bibiiwiki"))
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "wiki",
            "query",
            "momentum expression",
            "--wiki-root",
            root.to_str().expect("UTF-8 temp path"),
            "--no-save",
        ])
        .output()
        .expect("wiki query CLI should launch");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 CLI output");
    assert!(stdout.contains("LLM synthesis unavailable"));
    assert!(stdout.contains("log(close[t-1] / open[t-1])"));

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn maintenance_repairs_missing_links_and_rebuilds_index() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-maintain-{unique}"));
    let store = WikiStore::new(root.clone());
    store
        .write_page(
            "factors/alpha.md",
            &WikiPage {
                title: "Alpha".to_string(),
                body: "See [[Missing Concept]] and [[Known]].\n".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("factor page");
    store
        .write_page(
            "concepts/known.md",
            &WikiPage {
                title: "Known".to_string(),
                body: "A known concept.\n".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("known page");
    store
        .write_page(
            "index.md",
            &WikiPage {
                title: "Old Index".to_string(),
                body: "Old content\n".to_string(),
                status: "active".to_string(),
                ..WikiPage::default()
            },
            Authority::Human,
        )
        .expect("old index");

    let result = WikiMaintainer::new(root.clone())
        .update(false)
        .expect("maintenance should succeed");
    assert_eq!(result.created_stubs, 1);
    assert_eq!(result.repaired_links, 1);
    assert_eq!(result.indexed_pages, 3);
    assert!(result.health.broken_links.is_empty());
    assert!(result.health.unindexed.is_empty());
    assert!(root.join("concepts/Missing Concept.md").exists());
    let factor = fs::read_to_string(root.join("factors/alpha.md")).expect("factor text");
    assert!(factor.contains("[[concepts/Missing Concept|Missing Concept]]"));
    let index = fs::read_to_string(root.join("index.md")).expect("index text");
    assert!(index.contains("[[factors/alpha|Alpha]]"));
    assert!(index.contains("[[concepts/known|Known]]"));

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[tokio::test]
#[ignore = "requires the installed Codex CLI"]
async fn codex_prompt_runs_through_the_local_litellm_gateway() {
    async fn respond(State(response): State<serde_json::Value>) -> Json<serde_json::Value> {
        Json(response)
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock listener");
    let upstream = listener.local_addr().expect("mock address");
    let router = axum::Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(json!({
            "model": "mock-coder",
            "choices": [{"message": {"role": "assistant", "content": "WIKI_CODEX_OK"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        }));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("mock provider");
    });
    let config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback address"),
            api_key_env: None,
            request_timeout_seconds: 30,
            default_max_output_tokens: 1024,
        },
        model_list: vec![Deployment {
            model_name: "mock-coder".to_string(),
            litellm_params: LiteLLMParams {
                model: "openai/mock-coder".to_string(),
                api_key: None,
                api_base: Some(format!("http://{upstream}/v1")),
            },
        }],
        chunking: ChunkingConfig::default(),
        codex: CodexConfig {
            binary: "codex".to_string(),
            model: Some("mock-coder".to_string()),
            reasoning_effort: None,
        },
    };

    let result = codex_prompt(
        config,
        CodexPromptRequest {
            prompt: "Reply exactly once.".to_string(),
            workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            state_path: None,
            output_schema: None,
        },
    )
    .await
    .expect("Codex prompt should complete");
    assert_eq!(result.response, "WIKI_CODEX_OK");
}

#[test]
fn search_artifact_matches_python_page_contract() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-artifact-{unique}"));
    let service = QueryArtifactService::new(root.clone()).expect("artifact service");
    let path = service
        .write_search(
            "动量",
            &[SearchHit {
                path: "factors/Momentum_动量.md".to_string(),
                title: "动量 (Momentum)".to_string(),
                score: 7,
                excerpt: "动量因子使用历史收益排序股票。".to_string(),
            }],
            &BilingualName::new("Momentum Search", "动量检索").expect("valid bilingual name"),
        )
        .expect("search artifact");
    assert_eq!(
        path.file_name().and_then(|value| value.to_str()),
        Some("Momentum Search_动量检索.md")
    );
    let page = WikiStore::new(root.clone())
        .read_page("queries/Momentum Search_动量检索.md")
        .expect("artifact read")
        .expect("artifact exists");
    assert_eq!(page.page_type, "search");
    assert_eq!(page.title, "动量检索 (Momentum Search)");
    assert!(
        page.body
            .contains("[[factors/Momentum_动量|动量 (Momentum)]]")
    );
    assert!(page.body.contains("Score: `7`"));

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn raw_source_backup_is_immutable_and_content_addressed() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-raw-{unique}"));
    let input = root.join("input/report.txt");
    fs::create_dir_all(input.parent().expect("input parent")).expect("input directory");
    fs::write(&input, "first edition").expect("first source");

    let backup = RawSourceBackup::new(root.join("wiki_root/raw/sources"));
    let first = backup.copy(&input).expect("first backup");
    assert_eq!(first.status, "copied");
    assert_eq!(
        first.backup.file_name().and_then(|value| value.to_str()),
        Some("report.txt")
    );
    let unchanged = backup.copy(&input).expect("deduplicated backup");
    assert_eq!(unchanged.status, "unchanged");
    assert_eq!(unchanged.backup, first.backup);

    fs::write(&input, "second edition").expect("changed source");
    let changed = backup.copy(&input).expect("content-addressed backup");
    assert_eq!(changed.status, "copied");
    assert_ne!(changed.backup, first.backup);
    assert!(
        changed
            .backup
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.starts_with("report-"))
    );
    assert_eq!(
        fs::read_to_string(first.backup).expect("preserved backup"),
        "first edition"
    );

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
fn wiki_cli_exposes_ingest_without_an_explicit_init_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_bibiiwiki"))
        .args(["wiki", "--help"])
        .output()
        .expect("wiki help should launch");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("ingest"));
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("init"))
    );
}

#[test]
fn factor_catalog_reader_preserves_python_header_mapping() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-catalog-{unique}"));
    fs::create_dir_all(&root).expect("catalog directory");
    let path = root.join("factors.csv");
    fs::write(
        &path,
        "\u{feff}序号,因子类别,因子名称(英文),因子名称(中文),因子说明,因子公式,所需输入数据,Polars表达式\n1,Momentum,Momentum,动量,Historical return,close/shift(close),close,pl.col(\"close\")\n",
    )
    .expect("catalog fixture");

    let rows = FactorCatalogReader::read(&path).expect("catalog should parse");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sequence, "1");
    assert_eq!(rows[0].english_name, "Momentum");
    assert_eq!(rows[0].chinese_name, "动量");
    assert_eq!(rows[0].polars_expression, "pl.col(\"close\")");

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[test]
#[ignore = "requires the local Python golden wiki fixture, which is not distributed"]
fn factor_catalog_reader_accepts_the_python_xlsx_golden() {
    let rows = FactorCatalogReader::read(
        &python_fixture().join("raw/catalogs/486因子知识库_分类_公式_Polars.xlsx"),
    )
    .expect("golden XLSX should parse");
    assert!(rows.len() >= 400, "expected the 486-factor catalog");
    assert!(rows.iter().any(|row| !row.english_name.is_empty()));
    assert!(rows.iter().any(|row| !row.polars_expression.is_empty()));
}

#[tokio::test]
async fn csv_is_a_single_anydoc_source_instead_of_a_separate_catalog_input() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-ingest-{unique}"));
    let source_dir = root.join("input/sources");
    fs::create_dir_all(&source_dir).expect("source directory");
    fs::write(
        source_dir.join("factors.csv"),
        "因子类别,因子名称(英文),因子名称(中文),因子说明,因子公式,因子适用场景,因子评价,所需输入数据,Polars表达式\nMomentum,Momentum,动量,Historical return signal,close / shift(close),Cross-sectional ranking,Common baseline,close,pl.col(\"close\")\n",
    )
    .expect("catalog fixture");
    let config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback address"),
            api_key_env: None,
            request_timeout_seconds: 30,
            default_max_output_tokens: 1024,
        },
        model_list: vec![Deployment {
            model_name: "unused".to_string(),
            litellm_params: LiteLLMParams {
                model: "openai/unused".to_string(),
                api_key: None,
                api_base: Some("http://127.0.0.1:9/v1".to_string()),
            },
        }],
        chunking: ChunkingConfig::default(),
        codex: CodexConfig {
            binary: "definitely-missing-bibiiwiki-codex".to_owned(),
            ..CodexConfig::default()
        },
    };
    let error = WikiIngestPipeline::new(root.join("wiki_root"), config)
        .expect("pipeline")
        .run(WikiIngestOptions {
            input_paths: Vec::new(),
            source_dir,
            mode: bibiiwiki::wiki::WikiIngestMode::Start,
            workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            state_root: root.join("state"),
        })
        .await
        .expect_err("missing Codex must not report semantic extraction as complete");
    let error = format!("{error:#}");
    assert!(error.contains("LLM preflight failed during Original LLM"));
    assert!(root.join("wiki_root/purpose.md").is_file());
    assert!(root.join("wiki_root/schema.md").is_file());
    assert!(root.join("wiki_root/wiki/index.md").is_file());
    assert!(root.join("wiki_root/raw/sources/factors.csv").exists());
    let converted = fs::read_dir(root.join("wiki_root/wiki/sources"))
        .expect("converted source directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|value| value.to_str()) != Some("index.md"))
        .expect("converted CSV Markdown");
    let content = fs::read_to_string(converted).expect("converted source page");
    assert!(content.contains("Historical return signal"));
    let extraction_manifest = fs::read_dir(root.join("wiki_root/raw/extracted"))
        .expect("extraction manifest directory")
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .expect("CSV extraction manifest");
    assert!(
        fs::read_to_string(extraction_manifest.path())
            .expect("extraction manifest source")
            .contains("firecrawl-anydoc-0.2.4")
    );
    for component in ["concepts", "entities", "factors", "queries"] {
        let generated_pages = fs::read_dir(root.join("wiki_root/wiki").join(component))
            .expect("component directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "index.md")
            .count();
        assert_eq!(generated_pages, 0, "{component} must not claim extraction");
    }
    let methodology_pages = fs::read_dir(root.join("wiki_root/wiki/methodology"))
        .expect("methodology directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != "index.md")
        .count();
    assert_eq!(
        methodology_pages, 0,
        "LLM preflight failure must stop before wiki component extraction"
    );

    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}

#[tokio::test]
#[ignore = "requires the installed Codex CLI"]
async fn codex_translation_runs_the_copied_prompt_with_structured_output() {
    async fn respond() -> Json<serde_json::Value> {
        Json(json!({
            "model": "mock-coder",
            "choices": [{"message": {"role": "assistant", "content":
                "{\"translations\":[{\"key\":\"0\",\"english\":\"Momentum\",\"chinese\":\"动量\"}]}"
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 8}
        }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock listener");
    let upstream = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/v1/chat/completions", post(respond)),
        )
        .await
        .expect("mock provider");
    });
    let config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback address"),
            api_key_env: None,
            request_timeout_seconds: 30,
            default_max_output_tokens: 1024,
        },
        model_list: vec![Deployment {
            model_name: "mock-coder".to_string(),
            litellm_params: LiteLLMParams {
                model: "openai/mock-coder".to_string(),
                api_key: None,
                api_base: Some(format!("http://{upstream}/v1")),
            },
        }],
        chunking: ChunkingConfig::default(),
        codex: CodexConfig {
            binary: "codex".to_string(),
            model: Some("mock-coder".to_string()),
            reasoning_effort: None,
        },
    };
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state = std::env::temp_dir().join(format!("bibiiwiki-translate-{unique}/state.json"));
    let names = bibiiwiki::wiki::CodexWikiAgent::new(config)
        .expect("wiki agent")
        .translate(
            &[bibiiwiki::wiki::PartialBilingualName {
                english: "Momentum".to_string(),
                chinese: String::new(),
            }],
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            &state,
        )
        .await
        .expect("Codex translation");
    assert_eq!(
        names,
        vec![BilingualName::new("Momentum", "动量").expect("name")]
    );
    assert!(state.exists(), "Codex thread state should be persisted");
    fs::remove_dir_all(state.parent().expect("state parent")).expect("temporary state cleanup");
}

#[tokio::test]
#[ignore = "requires the installed Codex CLI"]
async fn text_source_ingest_materializes_structured_codex_knowledge() {
    async fn respond() -> Json<serde_json::Value> {
        Json(json!({
            "model": "mock-coder",
            "choices": [{"message": {"role": "assistant", "content": serde_json::to_string(&json!({
                "source_name": {"english": "Momentum Note", "chinese": "动量笔记"},
                "summary": "A note about ranking assets by historical return.",
                "key_concepts": [{"english": "Momentum", "chinese": "动量", "definition": "Persistence in returns.", "evidence": ["rank historical returns"]}],
                "entities": [],
                "findings": ["Historical return can rank assets."],
                "connections": [], "tensions": [], "recommendations": ["Backtest the signal."],
                "selected_media": [],
                "factors": [{"english": "Simple Momentum", "chinese": "简单动量", "definition": "Lagged return.", "category": "Momentum", "formula": "close / shift(close, 20) - 1", "required_inputs": ["close"], "polars_expression": "pl.col(\"close\") / pl.col(\"close\").shift(20) - 1", "scenario": "Cross-sectional ranking", "evaluation": "Requires backtest.", "evidence": ["rank historical returns"]}],
                "methodology": {"definition": "Rank assets by trailing return.", "terms": [{"english": "Cross-Sectional Ranking", "chinese": "横截面排序", "term_type": "selection", "definition": "Rank assets at each observation date.", "evidence": ["rank historical returns"]}], "data_inputs": ["close"], "benefits": ["Comparable scores"], "limitations": ["Turnover"], "coverage_gaps": [], "connections": [], "review_status": "generated"}
            })).expect("analysis JSON")}}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 80}
        }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock listener");
    let upstream = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/v1/chat/completions", post(respond)),
        )
        .await
        .expect("mock provider");
    });
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bibiiwiki-source-{unique}"));
    fs::create_dir_all(root.join("input/sources")).expect("source directory");
    fs::write(
        root.join("input/sources/momentum.txt"),
        "Rank assets by historical returns.",
    )
    .expect("source fixture");
    let config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".parse().expect("address"),
            api_key_env: None,
            request_timeout_seconds: 30,
            default_max_output_tokens: 4096,
        },
        model_list: vec![Deployment {
            model_name: "mock-coder".to_string(),
            litellm_params: LiteLLMParams {
                model: "openai/mock-coder".to_string(),
                api_key: None,
                api_base: Some(format!("http://{upstream}/v1")),
            },
        }],
        chunking: ChunkingConfig::default(),
        codex: CodexConfig {
            binary: "codex".to_string(),
            model: Some("mock-coder".to_string()),
            reasoning_effort: None,
        },
    };
    let report = WikiIngestPipeline::new(root.join("wiki_root"), config)
        .expect("pipeline")
        .run(WikiIngestOptions {
            input_paths: Vec::new(),
            source_dir: root.join("input/sources"),
            mode: bibiiwiki::wiki::WikiIngestMode::Start,
            workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            state_root: root.join("state"),
        })
        .await
        .expect("source ingest");
    assert_eq!(report.documents, 1);
    assert_eq!(report.sources_analyzed, 1);
    assert_eq!(report.factors, 1);
    for path in [
        "wiki/concepts/Momentum_动量.md",
        "wiki/factors/Simple Momentum_简单动量.md",
        "wiki/methodology/Cross-Sectional Ranking_横截面排序.md",
        "wiki/methodology/Momentum Note Methodology_动量笔记方法论.md",
        "raw/sources/momentum.txt",
    ] {
        assert!(root.join("wiki_root").join(path).exists(), "missing {path}");
    }
    let source_pages = fs::read_dir(root.join("wiki_root/wiki/sources"))
        .expect("source pages")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != "index.md")
        .count();
    assert_eq!(source_pages, 1);
    fs::remove_dir_all(root).expect("temporary wiki cleanup");
}
