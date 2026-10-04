#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use bibiiwiki::codex;
use bibiiwiki::codex::{CodexPromptRequest, prompt as codex_prompt};
use bibiiwiki::config::Config;
use bibiiwiki::gateway::{AppState, app};
use bibiiwiki::wiki::{
    BilingualName, CodexWikiAgent, PartialBilingualName, QueryArtifactService, TemplateLibrary,
    WikiIngestMode, WikiIngestOptions, WikiIngestPipeline, WikiLinter, WikiMaintainer,
    WikiQueryService, WikiSearch,
};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "bibiiwiki", version, about)]
struct Cli {
    /// Unified BIBIIWIKI configuration. Defaults to ~/.bibiiwik/bibiiwiki.yaml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the `OpenAI` Responses-compatible gateway.
    Serve,
    /// Start an ephemeral gateway and launch Codex against it.
    Codex {
        /// Arguments passed through to Codex. Put these after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Print a Codex config.toml provider block for a persistent gateway.
    CodexConfig {
        /// Public base URL ending in /v1. Defaults to server.bind.
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Validate configuration and secret references without making an LLM call.
    Check,
    /// Open the native egui desktop workspace.
    Ui {
        /// Initial wiki project root shown in the workspace dock.
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
    },
    /// Build, search, query, and maintain an `llm_wiki` vault.
    Wiki {
        #[command(subcommand)]
        command: WikiCommand,
    },
}

#[derive(Debug, Subcommand)]
enum WikiCommand {
    /// Initialize the wiki when needed, convert documents with `AnyDoc`, and analyze them.
    Ingest {
        /// Exact source files or directories to ingest. May be repeated; when
        /// present these override source-dir for this run.
        #[arg(long = "input", value_name = "PATH")]
        input_paths: Vec<PathBuf>,
        #[arg(long, default_value = "wiki_sources")]
        source_dir: PathBuf,
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
        #[arg(long)]
        force: bool,
        /// Continue the latest interrupted or failed ingest session.
        #[arg(long, conflicts_with_all = ["force", "redo", "input_paths"])]
        resume: bool,
        /// Rerun the latest session from the beginning, bypassing ingest caches.
        #[arg(long, conflicts_with = "resume")]
        redo: bool,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, default_value = ".bibiiwiki/Agent")]
        state_root: PathBuf,
    },
    /// Search the local Markdown wiki without calling an LLM.
    Search {
        /// Search terms or question.
        query: String,
        /// Wiki project root recursively searched for Markdown files.
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
        /// Maximum number of ranked results.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Do not persist a query-memory page.
        #[arg(long)]
        no_save: bool,
        /// Codex workspace used for bilingual artifact naming.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Compatibility state path for independent bilingual translation calls.
        #[arg(long, default_value = ".bibiiwiki/Agent/wiki_translation_thread.json")]
        state_path: PathBuf,
    },
    /// Answer from ranked wiki evidence, optionally synthesizing with Codex.
    Query {
        /// Question to answer from the wiki.
        question: String,
        /// Wiki project root containing the wiki/ content directory.
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
        /// Maximum number of evidence pages.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Use deterministic local matches instead of Codex synthesis.
        #[arg(long)]
        no_llm: bool,
        /// Do not persist a query-memory page.
        #[arg(long)]
        no_save: bool,
        /// Codex workspace used for the query session.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Persistent Codex session state.
        #[arg(long, default_value = ".bibiiwiki/Agent/wiki_query_thread.json")]
        state_path: PathBuf,
        /// Compatibility state path for independent bilingual translation calls.
        #[arg(long, default_value = ".bibiiwiki/Agent/wiki_translation_thread.json")]
        translation_state_path: PathBuf,
    },
    /// Repair links, create stubs, rebuild the index, and append maintenance history.
    Update {
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
    },
    /// Check links, indexing, frontmatter, bilingual names, and headings.
    Lint {
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
    },
    /// Ask Codex for one structured factor proposal grounded in wiki evidence.
    Propose {
        objective: String,
        #[arg(long, default_value = "wiki_root")]
        wiki_root: PathBuf,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, default_value = ".bibiiwiki/Agent/codex_thread.json")]
        state_path: PathBuf,
    },
    /// List every embedded Python-compatible Jinja prompt and page template.
    Prompts,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Cli { config, command } = Cli::parse();
    attach_parent_console_for_cli(command.as_ref());
    let using_user_config = config.is_none();
    let config = config.map_or_else(Config::user_config_path, Ok)?;
    if using_user_config && command_requires_config(command.as_ref()) {
        Config::ensure_exists(&config)?;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("bibiiwiki=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match command {
        Some(Command::Serve) => serve(Config::load(&config)?).await,
        Some(Command::Codex { args }) => {
            let config = Config::load(&config)?;
            let code = codex::run(config, args).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Some(Command::CodexConfig { base_url }) => {
            let config = Config::load(&config)?;
            let base_url = base_url.unwrap_or_else(|| format!("http://{}/v1", config.server.bind));
            println!("{}", codex::config_snippet(&config, &base_url));
            Ok(())
        }
        Some(Command::Check) => {
            let config = Config::load(&config)?;
            println!(
                "configuration OK: {} deployment(s), Codex model {:?}",
                config.model_list.len(),
                config.codex_model()
            );
            Ok(())
        }
        Some(Command::Ui { wiki_root }) => bibiiwiki::ui::run(config, wiki_root),
        Some(Command::Wiki { command }) => run_wiki(command, config).await,
        None => bibiiwiki::ui::run(config, PathBuf::from("wiki_root")),
    }
}

fn is_ui_launch(command: Option<&Command>) -> bool {
    command.is_none() || matches!(command, Some(Command::Ui { .. }))
}

fn command_requires_config(command: Option<&Command>) -> bool {
    match command {
        None | Some(Command::Ui { .. }) => false,
        Some(
            Command::Serve | Command::Codex { .. } | Command::CodexConfig { .. } | Command::Check,
        ) => true,
        Some(Command::Wiki { command }) => match command {
            WikiCommand::Ingest { .. } | WikiCommand::Propose { .. } => true,
            WikiCommand::Search { no_save, .. } => !no_save,
            WikiCommand::Query {
                no_llm, no_save, ..
            } => !no_llm || !no_save,
            WikiCommand::Update { .. } | WikiCommand::Lint { .. } | WikiCommand::Prompts => false,
        },
    }
}

#[cfg(target_os = "windows")]
#[allow(
    unsafe_code,
    reason = "the Windows console API has no safe binding; attaching to an existing parent console is pointer-free and best-effort"
)]
fn attach_parent_console_for_cli(command: Option<&Command>) {
    if is_ui_launch(command) {
        return;
    }

    // SAFETY: `AttachConsole` takes a process identifier, not a pointer. A
    // failed attachment is deliberately ignored so redirected CLI execution
    // and service launches can continue without a parent console.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console_for_cli(_command: Option<&Command>) {}

#[allow(clippy::too_many_lines)]
async fn run_wiki(command: WikiCommand, config_path: PathBuf) -> Result<()> {
    match command {
        WikiCommand::Ingest {
            input_paths,
            source_dir,
            wiki_root,
            force,
            resume,
            redo,
            workspace,
            state_root,
        } => {
            let report = WikiIngestPipeline::new(wiki_root, Config::load(&config_path)?)?
                .run_with_progress(
                    WikiIngestOptions {
                        input_paths,
                        source_dir,
                        mode: if resume {
                            WikiIngestMode::Resume
                        } else if redo || force {
                            WikiIngestMode::Redo
                        } else {
                            WikiIngestMode::Start
                        },
                        workspace,
                        state_root,
                    },
                    |event| {
                        use std::io::Write as _;

                        println!("{}", event.marker());
                        let _ = std::io::stdout().flush();
                    },
                )
                .await?;
            println!(
                "documents={} sources_copied={} sources_analyzed={} sources_pending_analysis={} factors={} derived_formulas={} similarity_links={} methodology_pages={}",
                report.documents,
                report.sources_copied,
                report.sources_analyzed,
                report.sources_pending_analysis,
                report.factors,
                report.derived_formulas,
                report.similarity_links,
                report.methodology_pages
            );
            Ok(())
        }
        WikiCommand::Search {
            query,
            wiki_root,
            limit,
            no_save,
            workspace,
            state_path,
        } => {
            let hits = WikiSearch::new(wiki_root.clone()).search(&query, limit)?;
            for hit in &hits {
                println!("{} ({})\n{}", hit.title, hit.path, hit.excerpt);
            }
            if !no_save {
                let agent = CodexWikiAgent::new(Config::load(&config_path)?)?;
                let term = translate_text(&agent, &query, &workspace, &state_path).await?;
                let name = BilingualName::new(
                    format!("{} Search", term.english),
                    format!("{}检索", term.chinese),
                )?;
                let path = QueryArtifactService::new(wiki_root.join("wiki"))?
                    .write_search(&query, &hits, &name)?;
                println!("saved={}", path.display());
            }
            Ok(())
        }
        WikiCommand::Query {
            question,
            wiki_root,
            limit,
            no_llm,
            no_save,
            workspace,
            state_path,
            translation_state_path,
        } => {
            let service = WikiQueryService::new(wiki_root.join("wiki"))?;
            let hits = service.search(&question, limit)?;
            let (answer, synthesis_failed) = if no_llm {
                (service.grounded_answer(&question, &hits)?, false)
            } else {
                let prompt = service.codex_prompt(&question, &hits)?;
                match codex_prompt(
                    Config::load(&config_path)?,
                    CodexPromptRequest {
                        prompt,
                        workspace: workspace.clone(),
                        state_path: Some(state_path),
                        output_schema: None,
                    },
                )
                .await
                {
                    Ok(result) => (result.response, false),
                    Err(error) => {
                        eprintln!(
                            "warning: LLM synthesis unavailable ({error}); showing grounded local evidence"
                        );
                        let local = service.grounded_answer(&question, &hits)?;
                        (
                            format!(
                                "> **LLM synthesis unavailable.** BIBIIWIKI kept the UI usable and returned grounded local evidence instead.\n\n{local}"
                            ),
                            true,
                        )
                    }
                }
            };
            println!("{answer}");
            if !no_save {
                let name = if synthesis_failed {
                    fallback_query_name(&question)?
                } else {
                    let agent = CodexWikiAgent::new(Config::load(&config_path)?)?;
                    match translate_text(&agent, &question, &workspace, &translation_state_path)
                        .await
                    {
                        Ok(name) => name,
                        Err(error) => {
                            eprintln!(
                                "warning: bilingual query naming unavailable ({error}); using a stable fallback name"
                            );
                            fallback_query_name(&question)?
                        }
                    }
                };
                let path = QueryArtifactService::new(wiki_root.join("wiki"))?
                    .write_query(&question, &answer, &hits, &name)?;
                println!("saved={}", path.display());
            }
            Ok(())
        }
        WikiCommand::Update { wiki_root } => {
            let result = WikiMaintainer::new(wiki_root.join("wiki")).update(true)?;
            println!(
                "created_stubs={} repaired_links={} indexed_pages={} broken_links={} unindexed={} invalid_frontmatter={} invalid_naming={} invalid_headings={}",
                result.created_stubs,
                result.repaired_links,
                result.indexed_pages,
                result.health.broken_links.len(),
                result.health.unindexed.len(),
                result.health.invalid_frontmatter.len(),
                result.health.invalid_naming.len(),
                result.health.invalid_headings.len()
            );
            for item in result.health.broken_links {
                println!("broken {} -> {}", item.source, item.target);
            }
            Ok(())
        }
        WikiCommand::Lint { wiki_root } => {
            let health = WikiLinter::new(wiki_root.join("wiki")).check()?;
            println!(
                "broken_links={} unindexed={} invalid_frontmatter={} invalid_naming={} invalid_headings={}",
                health.broken_links.len(),
                health.unindexed.len(),
                health.invalid_frontmatter.len(),
                health.invalid_naming.len(),
                health.invalid_headings.len()
            );
            for item in &health.broken_links {
                println!("broken {} -> {}", item.source, item.target);
            }
            for item in &health.unindexed {
                println!("unindexed {item}");
            }
            Ok(())
        }
        WikiCommand::Propose {
            objective,
            wiki_root,
            workspace,
            state_path,
        } => {
            let hits = WikiSearch::new(wiki_root.join("wiki")).search(&objective, 8)?;
            let templates = TemplateLibrary::load()?;
            let context = hits
                .iter()
                .map(|hit| {
                    templates.render(
                        "agent/wiki_context_entry.md.j2",
                        &json!({"title": hit.title, "excerpt": hit.excerpt}),
                    )
                })
                .collect::<Result<Vec<_>>>()?
                .join("");
            let proposal = CodexWikiAgent::new(Config::load(&config_path)?)?
                .propose(&objective, context.trim(), &workspace, &state_path)
                .await?;
            println!("{}", serde_json::to_string_pretty(&proposal)?);
            Ok(())
        }
        WikiCommand::Prompts => {
            for name in TemplateLibrary::load()?.names() {
                println!("{name}");
            }
            Ok(())
        }
    }
}

fn fallback_query_name(question: &str) -> Result<BilingualName> {
    let digest = Sha256::digest(question.trim().as_bytes());
    let mut short = String::with_capacity(12);
    for byte in &digest[..6] {
        write!(&mut short, "{byte:02x}").expect("writing to a String cannot fail");
    }
    BilingualName::new(format!("Query {short}"), format!("查询 {short}"))
}

async fn translate_text(
    agent: &CodexWikiAgent,
    value: &str,
    workspace: &std::path::Path,
    state_path: &std::path::Path,
) -> Result<BilingualName> {
    let partial = if value.trim().is_ascii() {
        PartialBilingualName {
            english: value.trim().to_string(),
            chinese: String::new(),
        }
    } else {
        PartialBilingualName {
            english: String::new(),
            chinese: value.trim().to_string(),
        }
    };
    agent
        .translate(&[partial], workspace, state_path)
        .await?
        .into_iter()
        .next()
        .context("Codex did not return a bilingual name")
}

async fn serve(config: Config) -> Result<()> {
    let state = AppState::from_config(&config)?;
    let listener = TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;
    let address = listener
        .local_addr()
        .context("failed to read listener address")?;
    tracing::info!(%address, "BIBIIWIKI gateway listening");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("gateway server failed")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_executable_defaults_to_the_native_ui() {
        let cli = Cli::try_parse_from(["bibiiwiki"]).expect("bare executable should parse");
        assert!(cli.command.is_none());
        assert!(cli.config.is_none());
    }

    #[test]
    fn native_ui_launches_are_identified_without_creating_a_console() {
        assert!(is_ui_launch(None));
        assert!(is_ui_launch(Some(&Command::Ui {
            wiki_root: PathBuf::from("wiki_root"),
        })));
        assert!(!is_ui_launch(Some(&Command::Serve)));
        assert!(!is_ui_launch(Some(&Command::Check)));
    }

    #[test]
    fn ingest_accepts_repeated_explicit_files_and_directories() {
        let cli = Cli::try_parse_from([
            "bibiiwiki",
            "wiki",
            "ingest",
            "--input",
            "notes/momentum.md",
            "--input",
            "factor_catalogs",
        ])
        .expect("repeated ingest inputs should parse");

        let Some(Command::Wiki {
            command: WikiCommand::Ingest { input_paths, .. },
        }) = cli.command
        else {
            panic!("expected wiki ingest command");
        };
        assert_eq!(
            input_paths,
            vec![
                PathBuf::from("notes/momentum.md"),
                PathBuf::from("factor_catalogs")
            ]
        );
    }

    #[test]
    fn ingest_resume_and_redo_are_explicit_mutually_exclusive_modes() {
        for (flag, expected_resume, expected_redo) in
            [("--resume", true, false), ("--redo", false, true)]
        {
            let cli = Cli::try_parse_from(["bibiiwiki", "wiki", "ingest", flag])
                .expect("durable ingest mode should parse");
            let Some(Command::Wiki {
                command: WikiCommand::Ingest { resume, redo, .. },
            }) = cli.command
            else {
                panic!("expected wiki ingest command");
            };
            assert_eq!(resume, expected_resume);
            assert_eq!(redo, expected_redo);
        }
        assert!(
            Cli::try_parse_from(["bibiiwiki", "wiki", "ingest", "--resume", "--redo"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "bibiiwiki",
                "wiki",
                "ingest",
                "--resume",
                "--input",
                "source.md"
            ])
            .is_err()
        );
    }

    #[test]
    fn fallback_query_names_are_stable_and_bilingual() {
        let first = fallback_query_name("How is momentum defined?").expect("fallback name");
        let second = fallback_query_name("How is momentum defined?").expect("fallback name");

        assert_eq!(first, second);
        assert!(first.english.starts_with("Query "));
        assert!(first.chinese.starts_with("查询 "));
    }
}
