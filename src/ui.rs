use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use eframe::egui::{
    self, Align, Color32, Context, FontData, FontDefinitions, FontFamily, Frame, Id, Layout, Order,
    Response, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2, ViewportBuilder,
    ViewportCommand, WidgetInfo, WidgetType,
    containers::scroll_area::{ScrollBarVisibility, ScrollSource},
    style::ScrollStyle,
};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use url::form_urlencoded;
use yaml_edit::{MappingBuilder, YamlFile};

use crate::color_theme::{ColorTheme, SearchResultPalette};
use crate::commonmark_editor::{EditorMode, MarkdownEditor};
use crate::config::Config;
use crate::i18n::{AppLocale, LocaleManager, LocalePreference};
use crate::icons::{FaIcon, icon_button as fa_icon_button, text_button as fa_text_button};
use crate::jinja_editor::highlight_jinja;
use crate::llm_preflight::{probe_codex_agent, probe_direct_llm, probe_responses_proxy};
use crate::output_terminal::OutputTerminal;
use crate::toml_editor::highlight_toml;
use crate::wiki::{
    SearchBackend, SearchHit, TemplateLibrary, WikiIngestCheckpointState, WikiIngestLlmCheck,
    WikiIngestMode, WikiIngestProgress, WikiIngestRunState, WikiIngestStage, WikiIngestStageState,
    WikiIngestStatus, WikiSearch,
};
use crate::yaml_editor::highlight_yaml;

const ACCENT: Color32 = Color32::from_rgb(53, 133, 246);
const WORKSPACE_DOCK_COLOR: Color32 = Color32::from_rgb(37, 99, 235);
const SEARCH_DOCK_COLOR: Color32 = Color32::from_rgb(234, 120, 32);
const QUERY_DOCK_COLOR: Color32 = Color32::from_rgb(34, 160, 99);
const RESTORE_TRIANGLE_SIZE: f32 = 14.0;
const RESTORE_FROM_LEFT_GLYPH: &str = "▶";
const RESTORE_FROM_RIGHT_GLYPH: &str = "◀";
const DOCK_RESIZE_GRAB_RADIUS: f32 = 10.0;
const WORKSPACE_DOCK_MIN_WIDTH: f32 = 150.0;
const SEARCH_DOCK_MIN_WIDTH: f32 = 180.0;
const QUERY_DOCK_MIN_WIDTH: f32 = 220.0;
const WORKSPACE_DOCK_DEFAULT_WIDTH: f32 = 250.0;
const SEARCH_DOCK_DEFAULT_WIDTH: f32 = 330.0;
const QUERY_DOCK_DEFAULT_WIDTH: f32 = 390.0;
const OUTPUT_DOCK_DEFAULT_HEIGHT: f32 = 210.0;
const OUTPUT_DOCK_MIN_HEIGHT: f32 = 120.0;
const OUTPUT_DOCK_MAX_HEIGHT: f32 = 720.0;
const ACTIVITY_RAIL_WIDTH: f32 = 36.0;
const ACTIVITY_BUTTON_SIZE: f32 = 28.0;
const EXPLORER_ROW_HEIGHT: f32 = 20.0;
const EXPLORER_ICON_SIZE: f32 = 13.0;
const EXPLORER_FONT_SIZE: f32 = 13.0;
const EXPLORER_INDENT: f32 = 12.0;
const EXPLORER_ROW_GAP: f32 = 1.0;
const LLM_YAML_EDITOR_MIN_HEIGHT: f32 = 520.0;
const LLM_YAML_EDITOR_MAX_HEIGHT: f32 = 760.0;
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/bibi-icon.png");
const APP_LOGO_PNG: &[u8] = include_bytes!("../assets/bibiiwiki-logo.png");

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Theme {
    #[default]
    #[serde(alias = "Light")]
    Light,
    #[serde(alias = "Dark")]
    Dark,
}

#[derive(Clone, Copy, Debug)]
struct Palette {
    panel: Color32,
    chrome: Color32,
    canvas: Color32,
    border: Color32,
    control: Color32,
    control_hovered: Color32,
    text: Color32,
    muted: Color32,
}

impl Theme {
    const fn palette(self) -> Palette {
        match self {
            Self::Light => Palette {
                panel: Color32::from_rgb(246, 248, 252),
                chrome: Color32::from_rgb(233, 238, 246),
                canvas: Color32::from_rgb(255, 255, 255),
                border: Color32::from_rgb(196, 205, 219),
                control: Color32::from_rgb(226, 232, 241),
                control_hovered: Color32::from_rgb(211, 222, 239),
                text: Color32::from_rgb(29, 39, 54),
                muted: Color32::from_rgb(98, 111, 132),
            },
            Self::Dark => Palette {
                panel: Color32::from_rgb(24, 29, 39),
                chrome: Color32::from_rgb(17, 21, 29),
                canvas: Color32::from_rgb(14, 18, 25),
                border: Color32::from_rgb(48, 58, 76),
                control: Color32::from_rgb(31, 38, 51),
                control_hovered: Color32::from_rgb(42, 52, 69),
                text: Color32::from_rgb(224, 231, 242),
                muted: Color32::from_rgb(145, 157, 179),
            },
        }
    }
}

/// Opens the native BIBIIWIKI desktop application.
///
/// # Errors
///
/// Returns an error when the native window or graphics context cannot start.
pub fn run(config_path: PathBuf, wiki_root: PathBuf) -> Result<()> {
    let config_path = resolve_startup_path(config_path);
    let wiki_root = resolve_startup_path(wiki_root);
    let mut startup_issues = Vec::new();
    if let Err(error) = Config::ensure_exists(&config_path) {
        startup_issues.push(format!(
            "Unified configuration {} could not be created: {error:#}",
            config_path.display()
        ));
    }
    let mut viewport = ViewportBuilder::default()
        .with_title("BIBIIWIKI")
        .with_inner_size([1440.0, 920.0])
        .with_min_inner_size([960.0, 640.0]);
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => startup_issues.push(format!("Application icon could not be loaded: {error}")),
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "bibiiwiki",
        options,
        Box::new(move |creation| {
            let recovery_config = config_path.clone();
            let recovery_root = wiki_root.clone();
            let application = catch_unwind(AssertUnwindSafe(|| {
                BibiiWikiApp::new(creation, config_path, wiki_root, startup_issues)
            }))
            .unwrap_or_else(|payload| {
                let error = panic_payload_message(payload.as_ref());
                BibiiWikiApp::startup_recovery(recovery_config, recovery_root, &error)
            });
            Ok(Box::new(application))
        }),
    )
    .map_err(|error| anyhow!("failed to run BIBIIWIKI UI: {error}"))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Inspector {
    #[default]
    #[serde(alias = "Tasks", alias = "tasks")]
    Ingest,
    Lint,
    Update,
    #[serde(alias = "Llm")]
    Llm,
    #[serde(alias = "Tools")]
    Tools,
    #[serde(alias = "Prompts")]
    Prompts,
}

impl Inspector {
    const fn title_key(self) -> &'static str {
        match self {
            Self::Ingest => "inspector.ingest_title",
            Self::Lint => "inspector.lint_title",
            Self::Update => "inspector.update_title",
            Self::Llm => "inspector.llm_title",
            Self::Tools => "inspector.tools_title",
            Self::Prompts => "inspector.prompts_title",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DockResize {
    Workspace,
    Search,
    Query,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct WorkspaceEntry {
    name: String,
    root: PathBuf,
    #[serde(default)]
    added_at_millis: u64,
    #[serde(default)]
    obsidian_vault_id: Option<String>,
    #[serde(default)]
    obsidian_uri: Option<String>,
}

impl WorkspaceEntry {
    fn new(root: PathBuf) -> Self {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("wiki")
            .to_owned();
        Self {
            name,
            root,
            added_at_millis: unix_millis(SystemTime::now()),
            obsidian_vault_id: None,
            obsidian_uri: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceSort {
    #[default]
    #[serde(alias = "Name")]
    Name,
    #[serde(alias = "AddedNewest")]
    AddedNewest,
    #[serde(alias = "ChangedLatest")]
    ChangedLatest,
}

impl WorkspaceSort {
    const ALL: [Self; 3] = [Self::Name, Self::AddedNewest, Self::ChangedLatest];

    const fn label_key(self) -> &'static str {
        match self {
            Self::Name => "sort.name",
            Self::AddedNewest => "sort.added",
            Self::ChangedLatest => "sort.changed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these are independent persisted UI toggles, not domain state"
)]
struct PersistedState {
    theme: Theme,
    locale: LocalePreference,
    toolbar_visible: bool,
    workspace_dock_visible: bool,
    search_dock_visible: bool,
    #[serde(default = "default_visible")]
    query_dock_visible: bool,
    workspace_dock_width: f32,
    search_dock_width: f32,
    query_dock_width: f32,
    bottom_dock_visible: bool,
    bottom_dock_height: f32,
    inspector_visible: bool,
    inspector: Inspector,
    workspaces: Vec<WorkspaceEntry>,
    selected_workspace: usize,
    workspace_sort: WorkspaceSort,
    source_dir: PathBuf,
    agent_workspace: PathBuf,
    force_ingest: bool,
    search_limit: usize,
    query_uses_llm: bool,
    save_query_memory: bool,
    llm_protocol: Option<LlmProtocol>,
    llm_provider: Option<String>,
    markdown_editor_mode: EditorMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct ToolConfig {
    ingest: IngestToolConfig,
    codex: CodexToolConfig,
    search: SearchToolConfig,
    query: QueryToolConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct IngestToolConfig {
    source_dir: PathBuf,
    force: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct CodexToolConfig {
    workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct SearchToolConfig {
    limit: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct QueryToolConfig {
    use_llm: bool,
    save_memory: bool,
}

impl ToolConfig {
    fn from_persisted(state: &PersistedState) -> Self {
        Self {
            ingest: IngestToolConfig {
                source_dir: state.source_dir.clone(),
                force: state.force_ingest,
            },
            codex: CodexToolConfig {
                workspace: state.agent_workspace.clone(),
            },
            search: SearchToolConfig {
                limit: state.search_limit,
            },
            query: QueryToolConfig {
                use_llm: state.query_uses_llm,
                save_memory: state.save_query_memory,
            },
        }
    }

    fn apply_to(&self, state: &mut PersistedState) {
        state.source_dir.clone_from(&self.ingest.source_dir);
        state.agent_workspace.clone_from(&self.codex.workspace);
        state.force_ingest = self.ingest.force;
        state.search_limit = self.search.limit;
        state.query_uses_llm = self.query.use_llm;
        state.save_query_memory = self.query.save_memory;
        state.sanitize();
    }
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self::from_persisted(&PersistedState::default())
    }
}

impl Default for IngestToolConfig {
    fn default() -> Self {
        let state = PersistedState::default();
        Self {
            source_dir: state.source_dir,
            force: state.force_ingest,
        }
    }
}

impl Default for CodexToolConfig {
    fn default() -> Self {
        Self {
            workspace: PersistedState::default().agent_workspace,
        }
    }
}

impl Default for SearchToolConfig {
    fn default() -> Self {
        Self {
            limit: PersistedState::default().search_limit,
        }
    }
}

impl Default for QueryToolConfig {
    fn default() -> Self {
        let state = PersistedState::default();
        Self {
            use_llm: state.query_uses_llm,
            save_memory: state.save_query_memory,
        }
    }
}

impl Default for PersistedState {
    fn default() -> Self {
        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            theme: Theme::Light,
            locale: LocalePreference::System,
            toolbar_visible: true,
            workspace_dock_visible: true,
            search_dock_visible: true,
            query_dock_visible: true,
            workspace_dock_width: WORKSPACE_DOCK_DEFAULT_WIDTH,
            search_dock_width: SEARCH_DOCK_DEFAULT_WIDTH,
            query_dock_width: QUERY_DOCK_DEFAULT_WIDTH,
            bottom_dock_visible: true,
            bottom_dock_height: OUTPUT_DOCK_DEFAULT_HEIGHT,
            inspector_visible: false,
            inspector: Inspector::Ingest,
            workspaces: Vec::new(),
            selected_workspace: 0,
            workspace_sort: WorkspaceSort::Name,
            source_dir: current.join("wiki_sources"),
            agent_workspace: current,
            force_ingest: false,
            search_limit: 10,
            query_uses_llm: true,
            save_query_memory: true,
            llm_protocol: None,
            llm_provider: None,
            markdown_editor_mode: EditorMode::Split,
        }
    }
}

const fn default_visible() -> bool {
    true
}

fn sanitize_dock_width(value: f32, default: f32, minimum: f32, maximum: f32) -> f32 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        default
    }
}

impl PersistedState {
    fn ensure_workspace(&mut self, root: PathBuf) {
        if let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| same_path(&workspace.root, &root))
        {
            self.selected_workspace = index;
        } else {
            self.workspaces.push(WorkspaceEntry::new(root));
            self.selected_workspace = self.workspaces.len().saturating_sub(1);
        }
        self.sanitize();
    }

    fn sanitize(&mut self) {
        self.search_limit = self.search_limit.clamp(1, 100);
        self.workspace_dock_width = sanitize_dock_width(
            self.workspace_dock_width,
            WORKSPACE_DOCK_DEFAULT_WIDTH,
            WORKSPACE_DOCK_MIN_WIDTH,
            380.0,
        );
        self.search_dock_width = sanitize_dock_width(
            self.search_dock_width,
            SEARCH_DOCK_DEFAULT_WIDTH,
            SEARCH_DOCK_MIN_WIDTH,
            480.0,
        );
        self.query_dock_width = sanitize_dock_width(
            self.query_dock_width,
            QUERY_DOCK_DEFAULT_WIDTH,
            QUERY_DOCK_MIN_WIDTH,
            560.0,
        );
        self.bottom_dock_height = sanitize_dock_width(
            self.bottom_dock_height,
            OUTPUT_DOCK_DEFAULT_HEIGHT,
            OUTPUT_DOCK_MIN_HEIGHT,
            OUTPUT_DOCK_MAX_HEIGHT,
        );
        if self.workspaces.is_empty() || self.selected_workspace >= self.workspaces.len() {
            self.selected_workspace = 0;
        }
        if let (Some(protocol), Some(provider)) = (self.llm_protocol, &self.llm_provider)
            && !protocol
                .providers()
                .iter()
                .any(|preset| preset.name == provider)
        {
            self.llm_provider = Some(protocol.providers()[0].name.to_owned());
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JobPurpose {
    Task,
    Ingest,
    Query,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum IngestStepState {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct IngestUnitProgress {
    completed: Option<usize>,
    total: Option<usize>,
    chunks: Option<usize>,
    max_chunk_characters: Option<usize>,
}

const INGEST_STAGE_COUNT: usize = WikiIngestStage::ALL.len();
const INGEST_LLM_CHECK_COUNT: usize = WikiIngestLlmCheck::ALL.len();

impl From<WikiIngestProgress> for IngestUnitProgress {
    fn from(progress: WikiIngestProgress) -> Self {
        Self {
            completed: progress.completed,
            total: progress.total,
            chunks: progress.chunks,
            max_chunk_characters: progress.max_chunk_characters,
        }
    }
}

impl IngestStepState {
    const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JobStatus {
    Running,
    Succeeded,
    Failed,
}

impl JobStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "done",
            Self::Failed => "failed",
        }
    }

    const fn color(self) -> Color32 {
        match self {
            Self::Running => Color32::from_rgb(250, 193, 76),
            Self::Succeeded => Color32::from_rgb(83, 195, 132),
            Self::Failed => Color32::from_rgb(241, 103, 103),
        }
    }
}

#[derive(Debug)]
struct JobRecord {
    id: u64,
    label: String,
    purpose: JobPurpose,
    status: JobStatus,
    output: String,
    ingest_steps: Option<[IngestStepState; INGEST_STAGE_COUNT]>,
    ingest_units: Option<[IngestUnitProgress; INGEST_STAGE_COUNT]>,
    ingest_llm_checks: Option<[IngestStepState; INGEST_LLM_CHECK_COUNT]>,
}

#[derive(Debug)]
enum JobMessage {
    IngestProgress {
        id: u64,
        progress: WikiIngestProgress,
    },
    Finished {
        id: u64,
        purpose: JobPurpose,
        success: bool,
        stdout: String,
        stderr: String,
    },
}

enum CliProcessLine {
    Stdout(String),
    Stderr(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LlmDiagnosticPhase {
    #[default]
    Idle,
    TestingModel,
    TestingProxy,
    TestingCodex,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiagnosticVisualState {
    Pending,
    Success,
    Failed,
}

#[derive(Debug)]
enum LlmDiagnosticMessage {
    ModelPassed(String),
    ModelFailed(String),
    ProxyStarted,
    ProxyPassed(String),
    ProxyFailed(String),
    CodexStarted,
    CodexPassed(String),
    CodexFailed(String),
    WorkerFailed(String),
}

#[derive(Debug, Default)]
struct LlmDiagnosticState {
    phase: LlmDiagnosticPhase,
    model_result: Option<Result<String, String>>,
    proxy_result: Option<Result<String, String>>,
    codex_result: Option<Result<String, String>>,
}

type CliJobResult = Result<(bool, String, String)>;
type CliLineObserver = dyn Fn(&str);
type CliJobRunner = dyn Fn(&[OsString], &Path, &CliLineObserver) -> CliJobResult + Send + Sync;
type WorkspaceOpener = dyn Fn(&Path) -> Result<()> + Send + Sync;
type NoteOpener = dyn Fn(&Path, &Path) -> Result<()> + Send + Sync;
type WorkspaceRegistrar = dyn Fn(&Path) -> Result<ObsidianRegistration> + Send + Sync;
type DirectoryPicker = dyn Fn(&Path) -> Result<Option<PathBuf>> + Send + Sync;
type IngestSourcePicker =
    dyn Fn(IngestPickerMode, &Path) -> Result<Option<Vec<PathBuf>>> + Send + Sync;
type LlmDiagnosticRunner = dyn Fn(&str, &Path, &Sender<LlmDiagnosticMessage>) + Send + Sync;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IngestPickerMode {
    Files,
    Directories,
}

#[derive(Clone, Debug)]
struct FileTreeNode {
    path: PathBuf,
    name: String,
    children: Vec<Self>,
    is_markdown: bool,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceFileTree {
    nodes: Vec<FileTreeNode>,
    error: Option<String>,
    latest_changed_millis: u64,
}

struct OpenDocument {
    path: PathBuf,
    workspace_root: PathBuf,
    editor: MarkdownEditor,
    dirty: bool,
}

fn startup_status(issue_count: usize) -> String {
    if issue_count == 0 {
        "Ready".to_owned()
    } else {
        format!("Ready with {issue_count} startup issue(s)")
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum LlmProtocol {
    Chat,
    Responses,
    Messages,
    Completions,
    #[default]
    Local,
    Native,
}

impl LlmProtocol {
    const ALL: [Self; 6] = [
        Self::Chat,
        Self::Responses,
        Self::Messages,
        Self::Completions,
        Self::Local,
        Self::Native,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::Chat => "CHAT",
            Self::Responses => "RESP",
            Self::Messages => "MSG",
            Self::Completions => "COMP",
            Self::Local => "LOCAL",
            Self::Native => "NATIVE",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Chat => "OpenAI Chat Completions",
            Self::Responses => "OpenAI Responses",
            Self::Messages => "Anthropic Messages",
            Self::Completions => "Completion / FIM",
            Self::Local => "Local OpenAI-compatible",
            Self::Native => "Native provider API",
        }
    }

    const fn endpoint(self) -> &'static str {
        match self {
            Self::Chat => "/chat/completions",
            Self::Responses => "/responses",
            Self::Messages => "/messages",
            Self::Completions => "/completions",
            Self::Local => "/v1/chat/completions",
            Self::Native => "custom",
        }
    }

    fn label(self) -> String {
        format!("{} · {} · {}", self.id(), self.name(), self.endpoint())
    }

    const fn providers(self) -> &'static [LlmProviderPreset] {
        match self {
            Self::Chat => &CHAT_PROVIDERS,
            Self::Responses => &RESPONSES_PROVIDERS,
            Self::Messages => &MESSAGES_PROVIDERS,
            Self::Completions => &COMPLETIONS_PROVIDERS,
            Self::Local => &LOCAL_PROVIDERS,
            Self::Native => &NATIVE_PROVIDERS,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LlmProviderPreset {
    name: &'static str,
    model_prefix: &'static str,
    api_base: &'static str,
    api_key_env: &'static str,
}

const CHAT_PROVIDERS: [LlmProviderPreset; 6] = [
    LlmProviderPreset {
        name: "OpenAI",
        model_prefix: "openai",
        api_base: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
    },
    LlmProviderPreset {
        name: "DeepSeek",
        model_prefix: "deepseek",
        api_base: "https://api.deepseek.com/v1",
        api_key_env: "DEEPSEEK_API_KEY",
    },
    LlmProviderPreset {
        name: "Kimi",
        model_prefix: "moonshot",
        api_base: "https://api.moonshot.cn/v1",
        api_key_env: "MOONSHOT_API_KEY",
    },
    LlmProviderPreset {
        name: "GLM",
        model_prefix: "zai",
        api_base: "https://open.bigmodel.cn/api/paas/v4",
        api_key_env: "ZAI_API_KEY",
    },
    LlmProviderPreset {
        name: "Qwen",
        model_prefix: "dashscope",
        api_base: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        api_key_env: "DASHSCOPE_API_KEY",
    },
    LlmProviderPreset {
        name: "Ollama",
        model_prefix: "ollama",
        api_base: "http://127.0.0.1:11434/v1",
        api_key_env: "",
    },
];

const RESPONSES_PROVIDERS: [LlmProviderPreset; 2] = [CHAT_PROVIDERS[0], CHAT_PROVIDERS[1]];

const MESSAGES_PROVIDERS: [LlmProviderPreset; 3] = [
    LlmProviderPreset {
        name: "Anthropic",
        model_prefix: "anthropic",
        api_base: "https://api.anthropic.com",
        api_key_env: "ANTHROPIC_API_KEY",
    },
    CHAT_PROVIDERS[1],
    CHAT_PROVIDERS[3],
];

const COMPLETIONS_PROVIDERS: [LlmProviderPreset; 2] = [
    LlmProviderPreset {
        name: "OpenAI-compatible",
        model_prefix: "openai",
        api_base: "",
        api_key_env: "OPENAI_API_KEY",
    },
    LlmProviderPreset {
        name: "Legacy / code model",
        model_prefix: "openai",
        api_base: "",
        api_key_env: "OPENAI_API_KEY",
    },
];

const LOCAL_PROVIDERS: [LlmProviderPreset; 4] = [
    LlmProviderPreset {
        name: "Ollama",
        model_prefix: "ollama",
        api_base: "http://127.0.0.1:11434/v1",
        api_key_env: "",
    },
    LlmProviderPreset {
        name: "vLLM",
        model_prefix: "hosted_vllm",
        api_base: "http://127.0.0.1:8000/v1",
        api_key_env: "",
    },
    LlmProviderPreset {
        name: "SGLang",
        model_prefix: "openai",
        api_base: "http://127.0.0.1:30000/v1",
        api_key_env: "",
    },
    LlmProviderPreset {
        name: "LM Studio",
        model_prefix: "lm_studio",
        api_base: "http://127.0.0.1:1234/v1",
        api_key_env: "",
    },
];

const NATIVE_PROVIDERS: [LlmProviderPreset; 3] = [
    LlmProviderPreset {
        name: "Gemini",
        model_prefix: "gemini",
        api_base: "",
        api_key_env: "GEMINI_API_KEY",
    },
    LlmProviderPreset {
        name: "Vertex AI",
        model_prefix: "vertex_ai",
        api_base: "",
        api_key_env: "",
    },
    LlmProviderPreset {
        name: "Bedrock",
        model_prefix: "bedrock",
        api_base: "",
        api_key_env: "",
    },
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ApiKeySource {
    #[default]
    Environment,
    Direct,
}

impl ApiKeySource {
    const ALL: [Self; 2] = [Self::Environment, Self::Direct];

    const fn label(self) -> &'static str {
        match self {
            Self::Environment => "Environment variable",
            Self::Direct => "Direct value",
        }
    }
}

#[derive(Clone, Debug)]
struct LlmWizardState {
    protocol: LlmProtocol,
    provider: String,
    api_base_url: String,
    api_key_source: ApiKeySource,
    api_key_env: String,
    api_key_direct: String,
    model_name: String,
    litellm_model: String,
    max_markdown_chunk_characters: usize,
}

impl Default for LlmWizardState {
    fn default() -> Self {
        let preset = LOCAL_PROVIDERS[0];
        Self {
            protocol: LlmProtocol::Local,
            provider: preset.name.to_owned(),
            api_base_url: preset.api_base.to_owned(),
            api_key_source: ApiKeySource::Environment,
            api_key_env: preset.api_key_env.to_owned(),
            api_key_direct: String::new(),
            model_name: String::new(),
            litellm_model: String::new(),
            max_markdown_chunk_characters: 10_000,
        }
    }
}

impl LlmWizardState {
    fn from_config_text(source: &str) -> Self {
        let Ok(config) = serde_yaml::from_str::<Config>(source) else {
            return Self::default();
        };
        let Some(deployment) = config.model_list.first() else {
            return Self::default();
        };
        let max_markdown_chunk_characters = config.max_markdown_chunk_characters();
        let litellm_model = deployment.litellm_params.model.clone();
        let (prefix, provider_model) = litellm_model
            .split_once('/')
            .unwrap_or(("", litellm_model.as_str()));
        let api_base_url = deployment
            .litellm_params
            .api_base
            .clone()
            .unwrap_or_default();
        let (protocol, provider) = infer_llm_protocol_and_provider(prefix, &api_base_url);
        let (api_key_source, api_key_env, api_key_direct) =
            deployment.litellm_params.api_key.as_deref().map_or(
                (ApiKeySource::Environment, String::new(), String::new()),
                |key| {
                    key.strip_prefix("os.environ/").map_or_else(
                        || (ApiKeySource::Direct, String::new(), key.to_owned()),
                        |variable| {
                            (
                                ApiKeySource::Environment,
                                variable.to_owned(),
                                String::new(),
                            )
                        },
                    )
                },
            );
        Self {
            protocol,
            provider: provider.to_owned(),
            api_base_url,
            api_key_source,
            api_key_env,
            api_key_direct,
            model_name: provider_model.to_owned(),
            litellm_model,
            max_markdown_chunk_characters,
        }
    }

    fn from_config_and_saved_selection(source: &str, persisted: &PersistedState) -> Self {
        let mut wizard = Self::from_config_text(source);
        wizard.apply_saved_selection(persisted.llm_protocol, persisted.llm_provider.as_deref());
        wizard
    }

    fn selected_preset(&self) -> LlmProviderPreset {
        self.protocol
            .providers()
            .iter()
            .copied()
            .find(|preset| preset.name == self.provider)
            .unwrap_or(self.protocol.providers()[0])
    }

    fn apply_saved_selection(&mut self, protocol: Option<LlmProtocol>, provider: Option<&str>) {
        let (Some(protocol), Some(provider)) = (protocol, provider) else {
            return;
        };
        if protocol
            .providers()
            .iter()
            .any(|preset| preset.name == provider)
        {
            self.protocol = protocol;
            provider.clone_into(&mut self.provider);
        }
    }

    fn select_protocol(&mut self, protocol: LlmProtocol) {
        self.protocol = protocol;
        self.select_provider(protocol.providers()[0]);
    }

    fn select_provider(&mut self, preset: LlmProviderPreset) {
        preset.name.clone_into(&mut self.provider);
        preset.api_base.clone_into(&mut self.api_base_url);
        preset.api_key_env.clone_into(&mut self.api_key_env);
        self.sync_litellm_model();
    }

    fn sync_litellm_model(&mut self) {
        let model = self.model_name.trim();
        self.litellm_model = if model.is_empty() {
            String::new()
        } else {
            format!("{}/{model}", self.selected_preset().model_prefix)
        };
    }
}

fn infer_llm_protocol_and_provider(prefix: &str, api_base: &str) -> (LlmProtocol, &'static str) {
    let local = api_base.contains("127.0.0.1") || api_base.contains("localhost");
    match prefix {
        "ollama" => (LlmProtocol::Local, "Ollama"),
        "hosted_vllm" => (LlmProtocol::Local, "vLLM"),
        "lm_studio" => (LlmProtocol::Local, "LM Studio"),
        "anthropic" => (LlmProtocol::Messages, "Anthropic"),
        "gemini" => (LlmProtocol::Native, "Gemini"),
        "vertex_ai" => (LlmProtocol::Native, "Vertex AI"),
        "bedrock" => (LlmProtocol::Native, "Bedrock"),
        "deepseek" => (LlmProtocol::Chat, "DeepSeek"),
        "moonshot" => (LlmProtocol::Chat, "Kimi"),
        "zai" => (LlmProtocol::Chat, "GLM"),
        "dashscope" => (LlmProtocol::Chat, "Qwen"),
        "openai" if local => (LlmProtocol::Local, "SGLang"),
        _ => (LlmProtocol::Chat, "OpenAI"),
    }
}

fn apply_llm_wizard_to_yaml(source: &str, wizard: &LlmWizardState) -> Result<String> {
    let model_name = wizard.model_name.trim();
    let litellm_model = wizard.litellm_model.trim();
    if model_name.is_empty() {
        bail!("model name is required");
    }
    if litellm_model.is_empty() {
        bail!("LiteLLM model identifier is required");
    }
    if !(256..=2_000_000).contains(&wizard.max_markdown_chunk_characters) {
        bail!("Markdown chunk size must be between 256 and 2000000 characters");
    }
    let yaml = YamlFile::from_str(source).context("invalid YAML configuration")?;
    let document = yaml.document().context("YAML document is empty")?;
    let root = document
        .as_mapping()
        .context("YAML configuration root must be a mapping")?;
    let model_list = root
        .get_sequence("model_list")
        .context("model_list must be a sequence")?;
    let deployment = model_list
        .first()
        .and_then(|node| node.as_mapping().cloned())
        .context("model_list must contain a mapping deployment")?;
    let params = deployment
        .get_mapping("litellm_params")
        .context("deployment must contain litellm_params")?;

    deployment.set("model_name", model_name);
    params.set("model", litellm_model);
    if wizard.api_base_url.trim().is_empty() {
        params.remove("api_base");
    } else {
        params.set("api_base", wizard.api_base_url.trim());
    }
    match wizard.api_key_source {
        ApiKeySource::Environment if wizard.api_key_env.trim().is_empty() => {
            params.remove("api_key");
        }
        ApiKeySource::Environment => {
            params.set(
                "api_key",
                format!("os.environ/{}", wizard.api_key_env.trim()),
            );
        }
        ApiKeySource::Direct if wizard.api_key_direct.is_empty() => {
            params.remove("api_key");
        }
        ApiKeySource::Direct => {
            params.set("api_key", wizard.api_key_direct.as_str());
        }
    }
    if let Some(codex) = root.get_mapping("codex") {
        codex.set("model", model_name);
    } else {
        root.set(
            "codex",
            MappingBuilder::new()
                .pair("model", model_name)
                .build_document()
                .as_mapping()
                .expect("mapping builder must produce a mapping"),
        );
    }
    let chunk_limit = i64::try_from(wizard.max_markdown_chunk_characters)
        .context("Markdown chunk size is too large")?;
    let chunking = if let Some(chunking) = root.get_mapping("chunking") {
        chunking
    } else {
        let model_limits = MappingBuilder::new()
            .pair(model_name, chunk_limit)
            .build_document()
            .as_mapping()
            .expect("model chunk limits builder must produce a mapping");
        let chunking = MappingBuilder::new()
            .pair("default_max_characters", 10_000_i64)
            .pair("model_max_characters", model_limits)
            .build_document()
            .as_mapping()
            .expect("chunking builder must produce a mapping");
        root.set("chunking", chunking);
        root.get_mapping("chunking")
            .expect("new chunking mapping must be readable")
    };
    let model_limits = if let Some(model_limits) = chunking.get_mapping("model_max_characters") {
        model_limits
    } else {
        let model_limits = MappingBuilder::new()
            .build_document()
            .as_mapping()
            .expect("empty model limits builder must produce a mapping");
        chunking.set("model_max_characters", model_limits);
        chunking
            .get_mapping("model_max_characters")
            .expect("new model chunk limits mapping must be readable")
    };
    model_limits.set(model_name, chunk_limit);
    let updated = yaml.to_string();
    serde_yaml::from_str::<Config>(&updated).context("generated LiteLLM YAML is invalid")?;
    Ok(updated)
}

/// Immediate-mode application state. Durable UI preferences are kept separate
/// from transient search results and background-process handles.
pub struct BibiiWikiApp {
    persisted: PersistedState,
    persisted_snapshot: PersistedState,
    locale_manager: LocaleManager,
    local_persistence: bool,
    config_path: PathBuf,
    color_theme_path: PathBuf,
    color_theme: ColorTheme,
    config_text: String,
    config_status: String,
    llm_wizard: LlmWizardState,
    tool_config_path: PathBuf,
    tool_config_text: String,
    tool_config_status: String,
    status: String,
    startup_issues: Vec<String>,
    recovery_error: Option<String>,
    logo_texture: Option<egui::TextureHandle>,
    search_query: String,
    search_hits: Vec<SearchHit>,
    selected_hit: Option<usize>,
    search_preview: MarkdownEditor,
    query_evidence: MarkdownEditor,
    workspace_trees: HashMap<PathBuf, WorkspaceFileTree>,
    open_document: Option<OpenDocument>,
    question: MarkdownEditor,
    answer: MarkdownEditor,
    workspace_pending_removal: Option<PathBuf>,
    ingest_source_panel_open: bool,
    ingest_status_root: Option<PathBuf>,
    ingest_status: Option<WikiIngestStatus>,
    show_about: bool,
    prompt_filter: String,
    prompt_names: Vec<String>,
    selected_prompt: Option<String>,
    prompt_source: String,
    jobs: VecDeque<JobRecord>,
    selected_job: Option<u64>,
    output_dock_drag_start_height: Option<f32>,
    active_dock_resize: Option<DockResize>,
    next_job_id: u64,
    job_tx: Sender<JobMessage>,
    job_rx: Receiver<JobMessage>,
    job_runner: Arc<CliJobRunner>,
    workspace_opener: Arc<WorkspaceOpener>,
    note_opener: Arc<NoteOpener>,
    workspace_registrar: Arc<WorkspaceRegistrar>,
    directory_picker: Arc<DirectoryPicker>,
    ingest_source_picker: Arc<IngestSourcePicker>,
    llm_diagnostic: LlmDiagnosticState,
    llm_diagnostic_tx: Sender<LlmDiagnosticMessage>,
    llm_diagnostic_rx: Receiver<LlmDiagnosticMessage>,
    llm_diagnostic_runner: Arc<LlmDiagnosticRunner>,
}

impl BibiiWikiApp {
    fn startup_recovery(config_path: PathBuf, wiki_root: PathBuf, error: &str) -> Self {
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(wiki_root);
        let mut application =
            Self::from_state_with_assets(persisted, config_path, Vec::new(), None, None);
        application.recovery_error = Some(format!("Unexpected startup failure: {error}"));
        application
    }

    fn new(
        creation: &eframe::CreationContext<'_>,
        config_path: PathBuf,
        wiki_root: PathBuf,
        mut startup_issues: Vec<String>,
    ) -> Self {
        configure_fonts(&creation.egui_ctx);
        egui_extras::install_image_loaders(&creation.egui_ctx);
        let legacy_persisted: Option<PersistedState> = creation
            .storage
            .and_then(|storage| eframe::get_value(storage, "bibiiwiki.ui.state.v1"));
        let has_legacy_catalog_setting = fs::read_to_string(&config_path).is_ok_and(|source| {
            source
                .lines()
                .any(|line| line.trim_start().starts_with("catalog_dir:"))
        });
        let (loaded_local, can_migrate) = match load_local_settings_file(&config_path) {
            Ok(state) => (state, true),
            Err(error) => {
                startup_issues.push(format!(
                    "Local settings in {} could not be loaded: {error:#}",
                    config_path.display()
                ));
                (None, false)
            }
        };
        let needs_migration = (loaded_local.is_none() || has_legacy_catalog_setting) && can_migrate;
        let mut persisted = loaded_local.or(legacy_persisted).unwrap_or_else(|| {
            let mut state = PersistedState::default();
            state.ensure_workspace(wiki_root);
            state
        });
        persisted.sanitize();
        if needs_migration && let Err(error) = persist_local_settings_file(&config_path, &persisted)
        {
            startup_issues.push(format!(
                "Local settings could not be migrated to {}: {error:#}",
                config_path.display()
            ));
        }
        let color_theme_path = Config::user_color_theme_path().unwrap_or_else(|error| {
            startup_issues.push(format!("User color-theme path is unavailable: {error:#}"));
            color_theme_path_for_config(&config_path)
        });
        let color_theme = ColorTheme::load_or_create(&color_theme_path).unwrap_or_else(|error| {
            startup_issues.push(format!(
                "Color theme {} could not be loaded; safe defaults are active: {error:#}",
                color_theme_path.display()
            ));
            ColorTheme::default()
        });
        configure_style(&creation.egui_ctx, persisted.theme);

        let logo_texture = embedded_texture(&creation.egui_ctx, "bibiiwiki-logo", APP_LOGO_PNG)
            .map_err(|error| startup_issues.push(format!("Logo could not be loaded: {error:#}")))
            .ok();

        let mut application = Self::from_state_with_assets(
            persisted,
            config_path,
            startup_issues,
            logo_texture,
            None,
        );
        application.color_theme_path = color_theme_path;
        application.color_theme = color_theme;
        application.local_persistence = true;
        application
    }

    #[cfg(test)]
    fn from_state(persisted: PersistedState, config_path: PathBuf) -> Self {
        Self::from_state_with_assets(persisted, config_path, Vec::new(), None, Some("en-US"))
    }

    #[allow(clippy::too_many_lines)]
    fn from_state_with_assets(
        mut persisted: PersistedState,
        config_path: PathBuf,
        mut startup_issues: Vec<String>,
        logo_texture: Option<egui::TextureHandle>,
        system_locale: Option<&str>,
    ) -> Self {
        let (job_tx, job_rx) = mpsc::channel();
        let (llm_diagnostic_tx, llm_diagnostic_rx) = mpsc::channel();
        if !persisted.workspaces.is_empty()
            && persisted.selected_workspace >= persisted.workspaces.len()
        {
            persisted.selected_workspace = 0;
            startup_issues
                .push("Saved workspace selection was invalid and has been reset.".to_owned());
        }

        let color_theme_path = color_theme_path_for_config(&config_path);
        let config_text = match fs::read_to_string(&config_path) {
            Ok(source) => source,
            Err(error) => {
                startup_issues.push(format!(
                    "Configuration {} is unavailable: {error}",
                    config_path.display()
                ));
                String::new()
            }
        };
        let prompt_names = match TemplateLibrary::load() {
            Ok(library) => library.names().to_vec(),
            Err(error) => {
                startup_issues.push(format!("Prompt library is unavailable: {error:#}"));
                Vec::new()
            }
        };
        let config_status = if config_text.is_empty() {
            "Configuration unavailable; LLM actions are disabled until it is corrected.".to_owned()
        } else {
            validate_config_text(&config_text)
                .unwrap_or_else(|error| format!("Configuration error: {error:#}"))
        };
        if config_status.starts_with("Configuration error:") {
            startup_issues.push(config_status.clone());
        }
        let llm_wizard = LlmWizardState::from_config_and_saved_selection(&config_text, &persisted);
        let (tool_config_path, tool_config_text, tool_config_status) =
            load_tool_config_editor(&config_path, &mut persisted, &mut startup_issues);
        let status = startup_status(startup_issues.len());
        let locale_manager = system_locale.map_or_else(
            || LocaleManager::new(persisted.locale),
            |tag| LocaleManager::with_system_tag(persisted.locale, tag),
        );
        Self {
            persisted_snapshot: persisted.clone(),
            persisted,
            locale_manager,
            local_persistence: false,
            config_path,
            color_theme_path,
            color_theme: ColorTheme::default(),
            config_text,
            config_status,
            llm_wizard,
            tool_config_path,
            tool_config_text,
            tool_config_status,
            status,
            startup_issues,
            recovery_error: None,
            logo_texture,
            search_query: String::new(),
            search_hits: Vec::new(),
            selected_hit: None,
            search_preview: MarkdownEditor::new("search-preview", "", EditorMode::Preview),
            query_evidence: MarkdownEditor::new("query-evidence", "", EditorMode::Preview),
            workspace_trees: HashMap::new(),
            open_document: None,
            question: MarkdownEditor::new("ai-question", "", EditorMode::Source),
            answer: MarkdownEditor::new(
                "ai-answer",
                "Ask a question grounded in the selected wiki.",
                EditorMode::Preview,
            ),
            workspace_pending_removal: None,
            ingest_source_panel_open: false,
            ingest_status_root: None,
            ingest_status: None,
            show_about: false,
            prompt_filter: String::new(),
            prompt_names,
            selected_prompt: None,
            prompt_source: String::new(),
            jobs: VecDeque::new(),
            selected_job: None,
            output_dock_drag_start_height: None,
            active_dock_resize: None,
            next_job_id: 1,
            job_tx,
            job_rx,
            job_runner: Arc::new(run_cli_process),
            workspace_opener: Arc::new(open_in_obsidian),
            note_opener: Arc::new(open_note_in_obsidian),
            workspace_registrar: Arc::new(register_workspace_with_obsidian),
            directory_picker: Arc::new(|directory| Ok(pick_workspace_directory(directory))),
            ingest_source_picker: Arc::new(|mode, directory| {
                Ok(pick_ingest_sources(mode, directory))
            }),
            llm_diagnostic: LlmDiagnosticState::default(),
            llm_diagnostic_tx,
            llm_diagnostic_rx,
            llm_diagnostic_runner: Arc::new(run_llm_diagnostic),
        }
    }

    fn render(&mut self, ctx: &Context) {
        // Eframe may restore persisted egui visuals after app construction, so
        // apply the application palette at the frame boundary.
        configure_style(ctx, self.persisted.theme);
        self.poll_jobs();
        self.poll_llm_diagnostic();
        self.menu_bar(ctx);
        if self.persisted.toolbar_visible {
            self.toolbar(ctx);
        }
        if !self.startup_issues.is_empty() {
            self.startup_banner(ctx);
        }
        if self.persisted.bottom_dock_visible {
            self.bottom_dock(ctx);
        }
        self.activity_rail(ctx);
        if self.persisted.workspace_dock_visible {
            self.workspace_dock(ctx);
        }
        if self.persisted.search_dock_visible {
            self.search_dock(ctx);
        }
        if self.persisted.query_dock_visible {
            self.query_dock(ctx);
        }
        if self.persisted.inspector_visible {
            self.inspector(ctx);
        } else {
            self.central_workspace(ctx);
        }
        self.dock_resize_handles(ctx);
        self.restore_docks(ctx);
        self.dialogs(ctx);
    }

    fn dock_resize_handles(&mut self, ctx: &Context) {
        use egui::containers::panel::Side;

        if self.persisted.workspace_dock_visible {
            side_panel_resize_handle(
                ctx,
                DockResizeSpec {
                    panel_id: "workspace_dock",
                    handle_id: "workspace_dock_resize_handle",
                    label: "Resize workspace dock",
                    dock: DockResize::Workspace,
                    side: Side::Left,
                    minimum_width: WORKSPACE_DOCK_MIN_WIDTH,
                    maximum_width: 380.0,
                    color: WORKSPACE_DOCK_COLOR,
                },
                &mut self.active_dock_resize,
                &mut self.persisted.workspace_dock_width,
            );
        }
        if self.persisted.search_dock_visible {
            side_panel_resize_handle(
                ctx,
                DockResizeSpec {
                    panel_id: "search_dock",
                    handle_id: "search_dock_resize_handle",
                    label: "Resize search dock",
                    dock: DockResize::Search,
                    side: Side::Left,
                    minimum_width: SEARCH_DOCK_MIN_WIDTH,
                    maximum_width: 480.0,
                    color: SEARCH_DOCK_COLOR,
                },
                &mut self.active_dock_resize,
                &mut self.persisted.search_dock_width,
            );
        }
        if self.persisted.query_dock_visible {
            side_panel_resize_handle(
                ctx,
                DockResizeSpec {
                    panel_id: "query_dock",
                    handle_id: "query_dock_resize_handle",
                    label: "Resize AI query dock",
                    dock: DockResize::Query,
                    side: Side::Right,
                    minimum_width: QUERY_DOCK_MIN_WIDTH,
                    maximum_width: 560.0,
                    color: QUERY_DOCK_COLOR,
                },
                &mut self.active_dock_resize,
                &mut self.persisted.query_dock_width,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn menu_bar(&mut self, ctx: &Context) {
        let palette = self.persisted.theme.palette();
        let language_options = LocalePreference::ALL
            .map(|preference| (preference, self.locale_manager.preference_label(preference)));
        egui::TopBottomPanel::top("menu_bar")
            .exact_height(30.0)
            .frame(
                Frame::NONE
                    .fill(palette.chrome)
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button(self.locale_manager.text("menu.file"), |ui| {
                        if ui
                            .add_enabled(
                                self.open_document.is_some(),
                                fa_text_button(
                                    FaIcon::Save,
                                    self.locale_manager.text("menu.save_note"),
                                ),
                            )
                            .clicked()
                        {
                            self.save_open_document();
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                self.open_document.is_some(),
                                fa_text_button(
                                    FaIcon::Close,
                                    self.locale_manager.text("menu.close_note"),
                                ),
                            )
                            .clicked()
                        {
                            self.close_open_document();
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add(fa_text_button(
                                FaIcon::Add,
                                self.locale_manager.text("menu.add_workspace"),
                            ))
                            .clicked()
                        {
                            self.choose_workspace_directory();
                            ui.close();
                        }
                        if ui
                            .add(fa_text_button(
                                FaIcon::FolderOpen,
                                self.locale_manager.text("menu.open_selected_obsidian"),
                            ))
                            .clicked()
                        {
                            self.open_selected_workspace();
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add(fa_text_button(
                                FaIcon::Close,
                                self.locale_manager.text("menu.quit"),
                            ))
                            .clicked()
                        {
                            ctx.send_viewport_cmd(ViewportCommand::Close);
                        }
                    });
                    ui.menu_button(self.locale_manager.text("menu.view"), |ui| {
                        ui.checkbox(
                            &mut self.persisted.toolbar_visible,
                            self.locale_manager.text("menu.toolbar"),
                        );
                        ui.checkbox(
                            &mut self.persisted.workspace_dock_visible,
                            self.locale_manager.text("menu.workspace_dock"),
                        );
                        ui.checkbox(
                            &mut self.persisted.search_dock_visible,
                            self.locale_manager.text("menu.search_dock"),
                        );
                        ui.checkbox(
                            &mut self.persisted.query_dock_visible,
                            self.locale_manager.text("menu.ai_query_dock"),
                        );
                        ui.checkbox(
                            &mut self.persisted.inspector_visible,
                            self.locale_manager.text("menu.inspector"),
                        );
                        ui.checkbox(
                            &mut self.persisted.bottom_dock_visible,
                            self.locale_manager.text("menu.output_dock"),
                        );
                        ui.separator();
                        ui.label(
                            RichText::new(self.locale_manager.text("menu.theme"))
                                .small()
                                .color(palette.muted),
                        );
                        ui.radio_value(
                            &mut self.persisted.theme,
                            Theme::Light,
                            self.locale_manager.text("menu.light_theme"),
                        );
                        ui.radio_value(
                            &mut self.persisted.theme,
                            Theme::Dark,
                            self.locale_manager.text("menu.dark_theme"),
                        );
                        ui.separator();
                        if ui
                            .add(fa_text_button(
                                FaIcon::Update,
                                self.locale_manager.text("menu.reload_color_theme"),
                            ))
                            .clicked()
                        {
                            self.reload_color_theme();
                            ui.close();
                        }
                    });
                    ui.menu_button(self.locale_manager.text("menu.language"), |ui| {
                        for (preference, label) in language_options {
                            if ui
                                .selectable_value(&mut self.persisted.locale, preference, label)
                                .changed()
                            {
                                self.locale_manager.set_preference(preference);
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button(self.locale_manager.text("menu.tasks"), |ui| {
                        if ui
                            .add(fa_text_button(
                                FaIcon::Ingest,
                                self.locale_manager.text("menu.ingest_sources"),
                            ))
                            .clicked()
                        {
                            self.request_ingest_sources();
                            ui.close();
                        }
                        if ui
                            .add(fa_text_button(
                                FaIcon::Lint,
                                self.locale_manager.text("menu.lint_wiki"),
                            ))
                            .clicked()
                        {
                            self.launch_task(ctx, "Lint", TaskCommand::Lint);
                            ui.close();
                        }
                        if ui
                            .add(fa_text_button(
                                FaIcon::Update,
                                self.locale_manager.text("menu.update_wiki"),
                            ))
                            .clicked()
                        {
                            self.launch_task(ctx, "Update", TaskCommand::Update);
                            ui.close();
                        }
                    });
                    ui.menu_button(self.locale_manager.text("menu.help"), |ui| {
                        if ui
                            .add(fa_text_button(
                                FaIcon::Info,
                                self.locale_manager.text("menu.about"),
                            ))
                            .clicked()
                        {
                            self.show_about = true;
                            ui.close();
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(logo) = &self.logo_texture {
                            ui.add(
                                egui::Image::new(logo).fit_to_exact_size(Vec2::new(116.0, 22.0)),
                            );
                        } else {
                            ui.label(RichText::new("BIBIIWIKI").strong().color(ACCENT));
                        }
                    });
                });
            });
    }

    fn startup_banner(&mut self, ctx: &Context) {
        let mut dismiss = false;
        egui::TopBottomPanel::top("startup_issues")
            .frame(
                Frame::NONE
                    .fill(Color32::from_rgb(255, 243, 205))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(226, 184, 72)))
                    .inner_margin(egui::Margin::symmetric(10, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "BIBIIWIKI opened safely with {} issue(s): {}",
                            self.startup_issues.len(),
                            self.startup_issues.join(" · ")
                        ))
                        .color(Color32::from_rgb(76, 57, 12)),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui.button("Dismiss").clicked();
                    });
                });
            });
        if dismiss {
            self.startup_issues.clear();
        }
    }

    fn toolbar(&mut self, ctx: &Context) {
        let palette = self.persisted.theme.palette();
        egui::TopBottomPanel::top("toolbar")
            .exact_height(42.0)
            .frame(
                Frame::NONE
                    .fill(palette.chrome)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add(fa_text_button(
                            FaIcon::Add,
                            self.locale_manager.text("toolbar.workspace"),
                        ))
                        .clicked()
                    {
                        self.choose_workspace_directory();
                    }
                    ui.separator();
                    if ui
                        .add(fa_text_button(
                            FaIcon::Ingest,
                            self.locale_manager.text("toolbar.ingest"),
                        ))
                        .clicked()
                    {
                        self.request_ingest_sources();
                    }
                    if ui
                        .add(fa_text_button(
                            FaIcon::Lint,
                            self.locale_manager.text("toolbar.lint"),
                        ))
                        .clicked()
                    {
                        self.launch_task(ctx, "Lint", TaskCommand::Lint);
                    }
                    if ui
                        .add(fa_text_button(
                            FaIcon::Update,
                            self.locale_manager.text("toolbar.update"),
                        ))
                        .clicked()
                    {
                        self.launch_task(ctx, "Update", TaskCommand::Update);
                    }
                    ui.separator();
                    if ui
                        .add(fa_text_button(
                            FaIcon::Query,
                            self.locale_manager.text("toolbar.ask_ai"),
                        ))
                        .clicked()
                    {
                        self.persisted.query_dock_visible = true;
                        self.submit_query(ctx);
                    }
                    if ui
                        .add(fa_text_button(
                            FaIcon::Editor,
                            self.locale_manager.text("toolbar.editor"),
                        ))
                        .clicked()
                    {
                        self.persisted.inspector_visible = false;
                    }
                    if ui
                        .add(fa_text_button(
                            FaIcon::Output,
                            self.locale_manager.text("toolbar.output"),
                        ))
                        .clicked()
                    {
                        self.persisted.bottom_dock_visible = !self.persisted.bottom_dock_visible;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if fa_icon_button(
                            ui,
                            "hide_toolbar",
                            FaIcon::CaretUp,
                            &self.locale_manager.text("toolbar.hide"),
                        )
                        .clicked()
                        {
                            self.persisted.toolbar_visible = false;
                        }
                        let workspace_name = self.selected_workspace().map_or_else(
                            || self.locale_manager.text("toolbar.no_workspace"),
                            |workspace| workspace.name.clone(),
                        );
                        ui.label(
                            RichText::new(format!("● {workspace_name}"))
                                .color(Color32::from_rgb(83, 195, 132)),
                        );
                    });
                });
            });
    }

    fn activity_rail(&mut self, ctx: &Context) {
        let palette = self.persisted.theme.palette();
        egui::SidePanel::left("activity_rail")
            .exact_width(ACTIVITY_RAIL_WIDTH)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(palette.chrome)
                    .inner_margin(egui::Margin::symmetric(3, 5)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if compact_rail_button(
                        ui,
                        FaIcon::Folder,
                        &self.locale_manager.text("menu.workspace_dock"),
                        self.persisted.workspace_dock_visible,
                        WORKSPACE_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.workspace_dock_visible =
                            !self.persisted.workspace_dock_visible;
                    }
                    if compact_rail_button(
                        ui,
                        FaIcon::Search,
                        &self.locale_manager.text("menu.search_dock"),
                        self.persisted.search_dock_visible,
                        SEARCH_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.search_dock_visible = !self.persisted.search_dock_visible;
                    }
                    if compact_rail_button(
                        ui,
                        FaIcon::Query,
                        &self.locale_manager.text("menu.ai_query_dock"),
                        self.persisted.query_dock_visible,
                        QUERY_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.query_dock_visible = !self.persisted.query_dock_visible;
                    }
                    ui.separator();
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Ingest,
                        &self.locale_manager.text("dock.ingest"),
                        Inspector::Ingest,
                    );
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Lint,
                        &self.locale_manager.text("dock.lint"),
                        Inspector::Lint,
                    );
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Update,
                        &self.locale_manager.text("dock.update"),
                        Inspector::Update,
                    );
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Brain,
                        &self.locale_manager.text("dock.llm"),
                        Inspector::Llm,
                    );
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Tools,
                        &self.locale_manager.text("dock.tools"),
                        Inspector::Tools,
                    );
                    self.inspector_rail_button(
                        ui,
                        FaIcon::Prompts,
                        &self.locale_manager.text("dock.prompts"),
                        Inspector::Prompts,
                    );
                });
            });
    }

    fn inspector_rail_button(
        &mut self,
        ui: &mut Ui,
        icon: FaIcon,
        label: &str,
        inspector: Inspector,
    ) {
        let selected = self.persisted.inspector_visible && self.persisted.inspector == inspector;
        if compact_rail_button(ui, icon, label, selected, ACCENT).clicked() {
            if selected {
                self.persisted.inspector_visible = false;
            } else {
                self.persisted.inspector = inspector;
                self.persisted.inspector_visible = true;
                if inspector == Inspector::Ingest {
                    self.ingest_source_panel_open = true;
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn workspace_dock(&mut self, ctx: &Context) {
        for workspace in self.persisted.workspaces.clone() {
            self.workspace_trees
                .entry(workspace.root.clone())
                .or_insert_with(|| build_workspace_file_tree(&workspace.root));
        }
        let theme = self.persisted.theme;
        egui::SidePanel::left("workspace_dock")
            .exact_width(self.persisted.workspace_dock_width)
            .resizable(false)
            .frame(panel_frame(theme))
            .show(ctx, |ui| {
                let mut content_ui = fixed_panel_content_ui(ui, "workspace_dock_content");
                let ui = &mut content_ui;
                panel_header(ui, &self.locale_manager.text("workspace.title"), |ui| {
                    if fa_icon_button(
                        ui,
                        "refresh_workspace_tree",
                        FaIcon::Refresh,
                        &self.locale_manager.text("workspace.refresh_files"),
                    )
                    .clicked()
                    {
                        self.workspace_trees.clear();
                    }
                    if dock_triangle_button(
                        ui,
                        "hide_workspace_dock",
                        "◀",
                        &self.locale_manager.text("dock.hide_workspace"),
                        WORKSPACE_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.workspace_dock_visible = false;
                    }
                });
                ui.add_space(5.0);
                if ui
                    .add(fa_text_button(
                        FaIcon::Add,
                        self.locale_manager.text("workspace.add_root"),
                    ))
                    .clicked()
                {
                    self.choose_workspace_directory();
                }
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(self.locale_manager.text("workspace.sort"))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    egui::ComboBox::from_id_salt("workspace_sort")
                        .selected_text(
                            self.locale_manager
                                .text(self.persisted.workspace_sort.label_key()),
                        )
                        .width(132.0)
                        .show_ui(ui, |ui| {
                            for sort in WorkspaceSort::ALL {
                                ui.selectable_value(
                                    &mut self.persisted.workspace_sort,
                                    sort,
                                    self.locale_manager.text(sort.label_key()),
                                );
                            }
                        });
                });
                ui.separator();

                if self.persisted.workspaces.is_empty() {
                    ui.label(
                        RichText::new(self.locale_manager.text("workspace.no_roots"))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }

                let mut open_index = None;
                let mut remove_root = None;
                let mut selected_index = None;
                let mut open_file = None;
                let workspace_order = workspace_sort_order(
                    &self.persisted.workspaces,
                    self.persisted.workspace_sort,
                    &self.workspace_trees,
                );
                let workspace_viewport_width = ui.available_width();
                ScrollArea::vertical()
                    .id_salt("workspaces")
                    .max_width(workspace_viewport_width)
                    .min_scrolled_width(workspace_viewport_width)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(workspace_viewport_width);
                        ui.spacing_mut().item_spacing.y = EXPLORER_ROW_GAP;
                        ui.spacing_mut().indent = EXPLORER_INDENT;
                        ui.visuals_mut().widgets.noninteractive.bg_stroke =
                            Stroke::new(1.0_f32, explorer_connector_color(ui.visuals().dark_mode));
                        for index in workspace_order {
                            let workspace = &self.persisted.workspaces[index];
                            let selected = index == self.persisted.selected_workspace;
                            let exists = workspace.root.exists();
                            let tree = self
                                .workspace_trees
                                .get(&workspace.root)
                                .cloned()
                                .unwrap_or_default();
                            let state_id =
                                ui.make_persistent_id(("workspace_tree", &workspace.root));
                            let mut state =
                                egui::collapsing_header::CollapsingState::load_with_default_open(
                                    ui.ctx(),
                                    state_id,
                                    selected,
                                );
                            // Selection and disclosure are separate states: the current wiki
                            // may be collapsed, while every inactive wiki remains closed.
                            if !selected {
                                state.set_open(false);
                            }
                            let row = file_explorer_row(
                                ui,
                                state_id.with("row"),
                                ExplorerEntryKind::Workspace,
                                &workspace.name,
                                selected,
                                state.is_open(),
                            )
                            .on_hover_text(if exists {
                                workspace.root.display().to_string()
                            } else {
                                format!(
                                    "{}: {}",
                                    self.locale_manager.text("workspace.missing_folder"),
                                    workspace.root.display()
                                )
                            });
                            if row.double_clicked() {
                                selected_index = Some(index);
                                state.set_open(true);
                                open_index = Some(index);
                            } else if row.clicked() {
                                if selected {
                                    state.toggle(ui);
                                } else {
                                    selected_index = Some(index);
                                    state.set_open(true);
                                }
                                ui.ctx().request_repaint();
                            }
                            row.context_menu(|ui| {
                                if ui
                                    .add(fa_text_button(
                                        FaIcon::FolderOpen,
                                        self.locale_manager.text("workspace.open_obsidian"),
                                    ))
                                    .clicked()
                                {
                                    open_index = Some(index);
                                    ui.close();
                                }
                                if ui
                                    .add(fa_text_button(
                                        FaIcon::Trash,
                                        self.locale_manager.text("workspace.remove_list"),
                                    ))
                                    .clicked()
                                {
                                    remove_root = Some(workspace.root.clone());
                                    ui.close();
                                }
                            });
                            state.show_body_indented(&row, ui, |ui| {
                                if let Some(error) = &tree.error {
                                    ui.label(RichText::new(error).small().color(Color32::RED));
                                } else if tree.nodes.is_empty() {
                                    ui.label(
                                        RichText::new(
                                            self.locale_manager.text("workspace.no_markdown"),
                                        )
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                    );
                                } else {
                                    render_file_tree_nodes(
                                        ui,
                                        &workspace.root,
                                        &tree.nodes,
                                        self.open_document.as_ref().map(|doc| doc.path.as_path()),
                                        &mut open_file,
                                    );
                                }
                            });
                            ui.add_space(3.0);
                        }
                    });

                if let Some(index) = selected_index {
                    self.persisted.selected_workspace = index;
                }
                if let Some(index) = open_index {
                    self.open_workspace(index);
                }
                if let Some((root, path)) = open_file {
                    if let Some(index) = self
                        .persisted
                        .workspaces
                        .iter()
                        .position(|workspace| same_path(&workspace.root, &root))
                    {
                        self.persisted.selected_workspace = index;
                    }
                    self.open_markdown_file(root, path);
                }
                if let Some(root) = remove_root {
                    self.workspace_pending_removal = Some(root);
                }
            });
    }

    #[allow(clippy::too_many_lines)]
    fn search_dock(&mut self, ctx: &Context) {
        let theme = self.persisted.theme;
        egui::SidePanel::left("search_dock")
            .exact_width(self.persisted.search_dock_width)
            .resizable(false)
            .frame(panel_frame(theme))
            .show(ctx, |ui| {
                let mut content_ui = fixed_panel_content_ui(ui, "search_dock_content");
                let ui = &mut content_ui;
                panel_header(ui, &self.locale_manager.text("search.title"), |ui| {
                    if dock_triangle_button(
                        ui,
                        "hide_search_dock",
                        "◀",
                        &self.locale_manager.text("dock.hide_search"),
                        SEARCH_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.search_dock_visible = false;
                    }
                });
                ui.add_space(6.0);
                let search_input = ui.add(
                    TextEdit::singleline(&mut self.search_query)
                        .hint_text(self.locale_manager.text("search.placeholder"))
                        .desired_width(f32::INFINITY),
                );
                search_input.widget_info(|| {
                    WidgetInfo::labeled(
                        WidgetType::TextEdit,
                        true,
                        self.locale_manager.text("search.input"),
                    )
                });
                let submitted = search_input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui
                        .add(fa_text_button(
                            FaIcon::Search,
                            self.locale_manager.text("search.button"),
                        ))
                        .clicked()
                        || submitted
                    {
                        self.run_search();
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            self.search_hits.len(),
                            self.locale_manager.text("search.results")
                        ))
                        .color(ui.visuals().weak_text_color()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.persisted.search_limit)
                                .range(1..=100)
                                .prefix(self.locale_manager.text("search.limit")),
                        );
                    });
                });
                ui.separator();

                let mut selected = None;
                let mut open_in_editor = None;
                let mut open_in_obsidian = None;
                let search_viewport_width = ui.available_width();
                ScrollArea::vertical()
                    .id_salt("search_results")
                    .max_width(search_viewport_width)
                    .min_scrolled_width(search_viewport_width)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(search_viewport_width);
                        for (index, hit) in self.search_hits.iter().enumerate() {
                            let interaction = search_hit_row(
                                ui,
                                hit,
                                self.selected_hit == Some(index),
                                self.color_theme
                                    .search_result(self.persisted.theme == Theme::Dark),
                            );
                            if interaction.selected {
                                selected = Some(index);
                            }
                            if interaction.open_in_editor {
                                open_in_editor = Some(index);
                            }
                            if interaction.open_in_obsidian {
                                open_in_obsidian = Some(index);
                            }
                        }
                    });
                if let Some(index) = selected {
                    self.select_search_hit(index);
                }
                if let Some(index) = open_in_editor {
                    self.open_search_hit_in_editor(index);
                }
                if let Some(index) = open_in_obsidian {
                    self.open_search_hit_in_obsidian(index);
                }
            });
    }

    fn reload_color_theme(&mut self) {
        match ColorTheme::load_or_create(&self.color_theme_path) {
            Ok(theme) => {
                self.color_theme = theme;
                self.status = format!("Reloaded {}", self.color_theme_path.display());
            }
            Err(error) => {
                self.status = format!(
                    "Could not reload {}; the current colors remain active: {error:#}",
                    self.color_theme_path.display()
                );
            }
        }
    }

    fn inspector(&mut self, ctx: &Context) {
        let palette = self.persisted.theme.palette();
        let inspector_title = self
            .locale_manager
            .text(self.persisted.inspector.title_key());
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.canvas).inner_margin(16.0))
            .show(ctx, |ui| {
                panel_header(ui, &inspector_title, |ui| {
                    let label = if self.locale_manager.locale() == AppLocale::En {
                        format!("Close {inspector_title} dock")
                    } else {
                        format!(
                            "{}: {inspector_title}",
                            self.locale_manager.text("inspector.close_dock")
                        )
                    };
                    if fa_icon_button(ui, "hide_inspector", FaIcon::Close, &label).clicked() {
                        self.persisted.inspector_visible = false;
                    }
                });
                ui.label(
                    RichText::new(self.locale_manager.text("inspector.close_hint"))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.separator();
                let viewport_height = ui.available_height();
                ui.scope(|ui| {
                    // Keep a permanent, solid scrollbar at the right edge of the
                    // inspector. This makes the scroll affordance visible even when
                    // the OS uses overlay scrollbars and keeps the viewport bounded
                    // when the bottom output dock reduces the available height.
                    ui.style_mut().spacing.scroll = ScrollStyle::solid();
                    let viewport_width = ui.available_width();
                    let output = ScrollArea::vertical()
                        .id_salt("inspector_content_scroll")
                        .max_width(viewport_width)
                        .max_height(viewport_height)
                        .min_scrolled_width(viewport_width)
                        .min_scrolled_height(viewport_height)
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                        .show(ui, |ui| match self.persisted.inspector {
                            Inspector::Ingest => self.ingest_inspector(ui, ctx),
                            Inspector::Lint => self.lint_inspector(ui, ctx),
                            Inspector::Update => self.update_inspector(ui, ctx),
                            Inspector::Llm => self.llm_inspector(ui, ctx, viewport_height),
                            Inspector::Tools => self.tools_inspector(ui),
                            Inspector::Prompts => self.prompts_inspector(ui),
                        });
                    let accessibility_label = match self.persisted.inspector {
                        Inspector::Llm => "LLM configuration vertical scroll viewport",
                        Inspector::Ingest => "Ingest vertical scroll viewport",
                        Inspector::Lint => "Lint vertical scroll viewport",
                        Inspector::Update => "Update vertical scroll viewport",
                        Inspector::Tools => "Tools vertical scroll viewport",
                        Inspector::Prompts => "Prompts vertical scroll viewport",
                    };
                    ui.interact(
                        output.inner_rect,
                        output.id.with("accessible_viewport"),
                        Sense::hover(),
                    )
                    .widget_info(|| {
                        WidgetInfo::labeled(WidgetType::Other, true, accessibility_label)
                    });
                });
            });
    }

    fn ingest_inspector(&mut self, ui: &mut Ui, ctx: &Context) {
        ui.label(self.locale_manager.text("inspector.import_documents"));
        if self.ingest_source_panel_open {
            ui.add_space(8.0);
            self.ingest_source_panel(ui, ctx);
        } else if ui
            .add(fa_text_button(
                FaIcon::Ingest,
                self.locale_manager.text("inspector.choose_sources"),
            ))
            .clicked()
        {
            self.ingest_source_panel_open = true;
        }
        ui.separator();
        ui.checkbox(
            &mut self.persisted.force_ingest,
            self.locale_manager.text("inspector.force_reingest"),
        );
        self.recent_task_jobs(
            ui,
            &self.locale_manager.text("inspector.recent_ingest"),
            "Ingest",
        );
    }

    fn lint_inspector(&mut self, ui: &mut Ui, ctx: &Context) {
        ui.label(self.locale_manager.text("inspector.lint_description"));
        ui.add_space(8.0);
        if ui
            .add(fa_text_button(
                FaIcon::Lint,
                self.locale_manager.text("inspector.run_lint"),
            ))
            .clicked()
        {
            self.launch_task(ctx, "Lint", TaskCommand::Lint);
        }
        ui.separator();
        self.recent_task_jobs(
            ui,
            &self.locale_manager.text("inspector.recent_lint"),
            "Lint",
        );
    }

    fn update_inspector(&mut self, ui: &mut Ui, ctx: &Context) {
        ui.label(self.locale_manager.text("inspector.update_description"));
        ui.add_space(8.0);
        if ui
            .add(fa_text_button(
                FaIcon::Update,
                self.locale_manager.text("inspector.run_update"),
            ))
            .clicked()
        {
            self.launch_task(ctx, "Update", TaskCommand::Update);
        }
        ui.separator();
        self.recent_task_jobs(
            ui,
            &self.locale_manager.text("inspector.recent_update"),
            "Update",
        );
    }

    fn recent_task_jobs(&self, ui: &mut Ui, heading: &str, label: &str) {
        ui.label(RichText::new(heading).strong());
        for job in self
            .jobs
            .iter()
            .rev()
            .filter(|job| job.label == label)
            .take(12)
        {
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").color(job.status.color()));
                ui.label(&job.label);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(job.status.label())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
        }
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_source_panel(&mut self, ui: &mut Ui, ctx: &Context) {
        let selected_root = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone());
        if self.ingest_status_root != selected_root {
            self.refresh_ingest_status();
        }
        let durable_status = self.ingest_status.clone();
        let ingest_running = self
            .jobs
            .iter()
            .any(|job| job.purpose == JobPurpose::Ingest && job.status == JobStatus::Running);
        let palette = self.persisted.theme.palette();
        let mut picker_mode = None;
        let mut requested_mode = None;
        let mut close = false;
        let response = Frame::NONE
            .fill(palette.panel)
            .stroke(Stroke::new(1.0_f32, palette.border))
            .corner_radius(6.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let _tab = ui.selectable_label(
                        true,
                        RichText::new(self.locale_manager.text("ingest.sources")).strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = fa_icon_button(
                            ui,
                            "close_ingest_source_panel",
                            FaIcon::Close,
                            &self.locale_manager.text("ingest.close_panel"),
                        )
                        .clicked();
                    });
                });
                ui.separator();
                ui.label(self.locale_manager.text("ingest.choose_documents"));
                ui.label(
                    RichText::new(self.locale_manager.text("ingest.supported_files"))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                if let Some(status) = durable_status.as_ref() {
                    requested_mode = durable_ingest_session_controls(
                        ui,
                        status,
                        ingest_running,
                        palette,
                        &self.locale_manager,
                    );
                }
                ui.add_space(6.0);
                picker_mode = ingest_picker_menu(ui, &self.locale_manager);
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new(self.locale_manager.text("ingest.checkpoints")).strong());
                let (steps, units, llm_checks) = self
                    .jobs
                    .iter()
                    .rev()
                    .find_map(|job| {
                        Some((job.ingest_steps?, job.ingest_units?, job.ingest_llm_checks?))
                    })
                    .or_else(|| durable_status.as_ref().map(durable_ingest_progress))
                    .unwrap_or((
                        [IngestStepState::Pending; INGEST_STAGE_COUNT],
                        [IngestUnitProgress::default(); INGEST_STAGE_COUNT],
                        [IngestStepState::Pending; INGEST_LLM_CHECK_COUNT],
                    ));
                for stage in WikiIngestStage::ALL {
                    ingest_step_row(
                        ui,
                        stage,
                        steps[stage.index()],
                        units[stage.index()],
                        &self.locale_manager,
                    );
                    if stage == WikiIngestStage::LlmCheck {
                        for check in WikiIngestLlmCheck::ALL {
                            ingest_llm_check_row(
                                ui,
                                check,
                                llm_checks[check.index()],
                                &self.locale_manager,
                            );
                        }
                    }
                }
            });
        response.response.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Other,
                true,
                self.locale_manager.text("ingest.panel"),
            )
        });

        if close {
            self.ingest_source_panel_open = false;
            self.status = self.locale_manager.text("ingest.panel_closed");
        } else if let Some(mode) = picker_mode {
            self.choose_ingest_sources(ctx, mode);
        } else if let Some(mode) = requested_mode {
            self.launch_ingest(ctx, Vec::new(), mode);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn llm_inspector(&mut self, ui: &mut Ui, ctx: &Context, viewport_height: f32) {
        self.llm_configuration_wizard(ui);
        ui.separator();
        self.llm_connection_test(ui, ctx);
        ui.separator();
        ui.heading(self.locale_manager.text("llm.yaml_title"));
        ui.label(
            RichText::new(self.locale_manager.text("llm.yaml_description"))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.horizontal(|ui| {
            if ui
                .add(fa_text_button(
                    FaIcon::Refresh,
                    self.locale_manager.text("llm.reload"),
                ))
                .clicked()
            {
                self.reload_config();
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Lint,
                    self.locale_manager.text("llm.validate"),
                ))
                .clicked()
            {
                self.config_status = validate_config_text(&self.config_text)
                    .unwrap_or_else(|error| format!("Configuration error: {error:#}"));
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Save,
                    self.locale_manager.text("llm.save"),
                ))
                .clicked()
            {
                self.save_config();
            }
        });
        ui.label(
            RichText::new(self.config_path.display().to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(
            RichText::new(self.locale_manager.text("llm.lossless"))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(RichText::new(&self.config_status).color(
            if self.config_status.starts_with('✓') {
                Color32::from_rgb(83, 195, 132)
            } else {
                Color32::from_rgb(241, 103, 103)
            },
        ));
        ui.separator();
        // The YAML editor has its own bounded viewport. The enclosing inspector
        // scroll area then keeps the diagnostics reachable when the output dock
        // leaves very little vertical room.
        // The whole inspector is independently scrollable, so the YAML editor
        // can stay genuinely useful even when it is below a long wizard and
        // three diagnostic result boxes. A tall bounded viewport shows roughly
        // a full configuration page instead of only the last few lines.
        let editor_height =
            (viewport_height * 0.72).clamp(LLM_YAML_EDITOR_MIN_HEIGHT, LLM_YAML_EDITOR_MAX_HEIGHT);
        let output = ScrollArea::vertical()
            .id_salt("llm_yaml")
            .max_height(editor_height)
            .min_scrolled_height(editor_height)
            .auto_shrink([false, false])
            .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
            // The enclosing inspector owns mouse-wheel movement. Otherwise this
            // nested YAML viewport consumes the wheel and makes the whole LLM
            // configuration panel appear stuck. Its own thumb remains draggable.
            .scroll_source(ScrollSource::SCROLL_BAR)
            .show(ui, |ui| {
                let dark_mode = self.persisted.theme == Theme::Dark;
                let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
                    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                    let job = highlight_yaml(buffer.as_str(), dark_mode, &font_id, wrap_width);
                    ui.fonts(|fonts| fonts.layout_job(job))
                };
                let response = ui.add(
                    TextEdit::multiline(&mut self.config_text)
                        .code_editor()
                        .layouter(&mut layouter)
                        .desired_width(f32::INFINITY)
                        .desired_rows(40),
                );
                response.widget_info(|| {
                    WidgetInfo::labeled(
                        WidgetType::TextEdit,
                        true,
                        self.locale_manager.text("llm.yaml_editor"),
                    )
                });
            });
        ui.interact(
            output.inner_rect,
            output.id.with("accessible_viewport"),
            Sense::hover(),
        )
        .widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Other,
                true,
                self.locale_manager.text("llm.yaml_scroll"),
            )
        });
    }

    fn llm_configuration_wizard(&mut self, ui: &mut Ui) {
        ui.heading(self.locale_manager.text("llm.wizard_title"));
        ui.label(
            RichText::new(self.locale_manager.text("llm.wizard_description"))
                .small()
                .color(ui.visuals().weak_text_color()),
        );

        let (protocol, mut provider, key_source) = self.llm_wizard_controls(ui);
        if protocol != self.llm_wizard.protocol {
            self.llm_wizard.select_protocol(protocol);
            provider.clone_from(&self.llm_wizard.provider);
        }
        if provider != self.llm_wizard.provider
            && let Some(preset) = self
                .llm_wizard
                .protocol
                .providers()
                .iter()
                .copied()
                .find(|preset| preset.name == provider)
        {
            self.llm_wizard.select_provider(preset);
        }
        self.llm_wizard.api_key_source = key_source;
        self.persisted.llm_protocol = Some(self.llm_wizard.protocol);
        self.persisted.llm_provider = Some(self.llm_wizard.provider.clone());

        self.llm_wizard_actions(ui);
    }

    fn llm_wizard_controls(&mut self, ui: &mut Ui) -> (LlmProtocol, String, ApiKeySource) {
        let mut protocol = self.llm_wizard.protocol;
        let mut provider = self.llm_wizard.provider.clone();
        let mut key_source = self.llm_wizard.api_key_source;
        let configured_env_hint = self.llm_wizard.selected_preset().api_key_env;
        let api_key_env_hint = if configured_env_hint.is_empty() {
            self.locale_manager.text("llm.no_key")
        } else {
            configured_env_hint.to_owned()
        };
        egui::Grid::new("llm_configuration_wizard")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(self.locale_manager.text("llm.interface"));
                egui::ComboBox::from_id_salt("llm_protocol")
                    .selected_text(protocol.label())
                    .width(360.0)
                    .show_ui(ui, |ui| {
                        for candidate in LlmProtocol::ALL {
                            ui.selectable_value(&mut protocol, candidate, candidate.label());
                        }
                    });
                ui.end_row();

                ui.label(self.locale_manager.text("llm.provider"));
                egui::ComboBox::from_id_salt("llm_provider")
                    .selected_text(&provider)
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for preset in protocol.providers() {
                            ui.selectable_value(&mut provider, preset.name.to_owned(), preset.name);
                        }
                    });
                ui.end_row();

                ui.label(self.locale_manager.text("llm.endpoint"));
                ui.monospace(protocol.endpoint());
                ui.end_row();

                ui.label(self.locale_manager.text("llm.api_base"));
                ui.add(
                    TextEdit::singleline(&mut self.llm_wizard.api_base_url)
                        .desired_width(f32::INFINITY)
                        .hint_text("https://provider.example/v1"),
                );
                ui.end_row();

                ui.label(self.locale_manager.text("llm.key_source"));
                egui::ComboBox::from_id_salt("llm_api_key_source")
                    .selected_text(key_source.label())
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for candidate in ApiKeySource::ALL {
                            ui.selectable_value(&mut key_source, candidate, candidate.label());
                        }
                    });
                ui.end_row();

                match key_source {
                    ApiKeySource::Environment => {
                        ui.label(self.locale_manager.text("llm.environment"));
                        ui.add(
                            TextEdit::singleline(&mut self.llm_wizard.api_key_env)
                                .desired_width(f32::INFINITY)
                                .hint_text(&api_key_env_hint),
                        );
                    }
                    ApiKeySource::Direct => {
                        ui.label(self.locale_manager.text("llm.api_key"));
                        ui.add(
                            TextEdit::singleline(&mut self.llm_wizard.api_key_direct)
                                .password(true)
                                .desired_width(f32::INFINITY)
                                .hint_text(self.locale_manager.text("llm.direct_hint")),
                        );
                    }
                }
                ui.end_row();

                ui.label(self.locale_manager.text("llm.model_name"));
                let model_response = ui.add(
                    TextEdit::singleline(&mut self.llm_wizard.model_name)
                        .desired_width(f32::INFINITY)
                        .hint_text("model-name"),
                );
                ui.end_row();

                ui.label(self.locale_manager.text("llm.litellm_model"));
                ui.monospace(&self.llm_wizard.litellm_model);
                ui.end_row();

                ui.label(self.locale_manager.text("llm.chunk_size"));
                llm_chunk_size_control(ui, &mut self.llm_wizard.max_markdown_chunk_characters);
                ui.end_row();

                if model_response.changed() {
                    self.llm_wizard.sync_litellm_model();
                }
            });
        (protocol, provider, key_source)
    }

    fn llm_wizard_actions(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui
                .add(fa_text_button(
                    FaIcon::Update,
                    self.locale_manager.text("llm.apply"),
                ))
                .on_hover_text(self.locale_manager.text("llm.apply_hint"))
                .clicked()
            {
                match apply_llm_wizard_to_yaml(&self.config_text, &self.llm_wizard) {
                    Ok(updated) => {
                        self.config_text = updated;
                        "✓ Wizard applied to YAML editor; review and Save to persist"
                            .clone_into(&mut self.config_status);
                    }
                    Err(error) => {
                        self.config_status = format!("Wizard could not update YAML: {error:#}");
                    }
                }
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Refresh,
                    self.locale_manager.text("llm.load"),
                ))
                .clicked()
            {
                self.llm_wizard = LlmWizardState::from_config_and_saved_selection(
                    &self.config_text,
                    &self.persisted,
                );
                "Wizard reloaded from the YAML editor".clone_into(&mut self.config_status);
            }
        });
    }

    fn llm_connection_test(&mut self, ui: &mut Ui, ctx: &Context) {
        let overall_state = if self.llm_diagnostic.phase == LlmDiagnosticPhase::Complete {
            if matches!(self.llm_diagnostic.model_result, Some(Ok(_)))
                && matches!(self.llm_diagnostic.proxy_result, Some(Ok(_)))
                && matches!(self.llm_diagnostic.codex_result, Some(Ok(_)))
            {
                DiagnosticVisualState::Success
            } else {
                DiagnosticVisualState::Failed
            }
        } else {
            DiagnosticVisualState::Pending
        };
        llm_diagnostic_status_row(
            ui,
            &self.locale_manager.text("llm.testing"),
            overall_state,
            Some(ACCENT),
        );
        if let Some(route) = llm_route_summary(&self.config_text) {
            ui.label(
                RichText::new(route)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }
        if let Some(endpoint) = llm_proxy_endpoint_summary(&self.config_text) {
            ui.label(
                RichText::new(endpoint)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }
        ui.horizontal(|ui| {
            let running = matches!(
                self.llm_diagnostic.phase,
                LlmDiagnosticPhase::TestingModel
                    | LlmDiagnosticPhase::TestingProxy
                    | LlmDiagnosticPhase::TestingCodex
            );
            if ui
                .add_enabled(
                    !running,
                    fa_text_button(FaIcon::Refresh, self.locale_manager.text("llm.test_button")),
                )
                .on_hover_text(self.locale_manager.text("llm.test_hint"))
                .clicked()
            {
                self.start_llm_diagnostic();
            }
            match self.llm_diagnostic.phase {
                LlmDiagnosticPhase::TestingModel => {
                    llm_connection_progress(
                        ui,
                        &self.locale_manager.text("llm.testing_model"),
                        "Testing configured LLM progress",
                    );
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                LlmDiagnosticPhase::TestingProxy => {
                    llm_connection_progress(
                        ui,
                        &self.locale_manager.text("llm.testing_proxy"),
                        "Testing Responses proxy progress",
                    );
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                LlmDiagnosticPhase::TestingCodex => {
                    llm_connection_progress(
                        ui,
                        &self.locale_manager.text("llm.testing_codex"),
                        "Testing Codex adapter progress",
                    );
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                LlmDiagnosticPhase::Idle | LlmDiagnosticPhase::Complete => {}
            }
        });
        llm_diagnostic_result(
            ui,
            &self.locale_manager.text("llm.configured"),
            &self.locale_manager.text("llm.configured_works"),
            self.llm_diagnostic.model_result.as_ref(),
            &self.locale_manager,
        );
        llm_diagnostic_result(
            ui,
            &self.locale_manager.text("llm.proxy"),
            &self.locale_manager.text("llm.proxy_works"),
            self.llm_diagnostic.proxy_result.as_ref(),
            &self.locale_manager,
        );
        llm_diagnostic_result(
            ui,
            &self.locale_manager.text("llm.codex"),
            &self.locale_manager.text("llm.codex_works"),
            self.llm_diagnostic.codex_result.as_ref(),
            &self.locale_manager,
        );
    }

    #[allow(clippy::too_many_lines)]
    fn tools_inspector(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(self.locale_manager.text("tools.ingest_paths")).strong());
        let mut controls_changed = false;
        controls_changed |= path_editor(
            ui,
            &self.locale_manager.text("tools.sources"),
            &mut self.persisted.source_dir,
        );
        controls_changed |= path_editor(
            ui,
            &self.locale_manager.text("tools.codex_workspace"),
            &mut self.persisted.agent_workspace,
        );
        controls_changed |= ui
            .checkbox(
                &mut self.persisted.force_ingest,
                self.locale_manager.text("tools.force_analysis"),
            )
            .changed();
        ui.separator();
        ui.label(RichText::new(self.locale_manager.text("tools.selected_root")).strong());
        let selected_root = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone());
        ui.monospace(selected_root.as_ref().map_or_else(
            || self.locale_manager.text("tools.no_workspace"),
            |root| root.display().to_string(),
        ));
        ui.add_space(6.0);
        if ui
            .add_enabled(
                selected_root.is_some(),
                fa_text_button(FaIcon::Folder, self.locale_manager.text("tools.use_parent")),
            )
            .clicked()
            && let Some(root) = selected_root
        {
            self.persisted.agent_workspace = root
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            controls_changed = true;
        }
        if controls_changed {
            self.sync_tool_config_from_controls();
        }

        ui.separator();
        ui.label(RichText::new(self.locale_manager.text("tools.full_toml")).strong());
        ui.horizontal(|ui| {
            if ui
                .add(fa_text_button(
                    FaIcon::Refresh,
                    self.locale_manager.text("tools.reload_toml"),
                ))
                .clicked()
            {
                self.reload_tool_config();
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Lint,
                    self.locale_manager.text("tools.validate_toml"),
                ))
                .clicked()
            {
                self.validate_tool_config();
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Update,
                    self.locale_manager.text("tools.apply_toml"),
                ))
                .clicked()
            {
                self.apply_tool_config();
            }
            if ui
                .add(fa_text_button(
                    FaIcon::Save,
                    self.locale_manager.text("tools.save_toml"),
                ))
                .clicked()
            {
                self.save_tool_config();
            }
        });
        ui.label(
            RichText::new(self.tool_config_path.display().to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        let tool_status_color = if self.tool_config_status.starts_with('✓') {
            Color32::from_rgb(83, 195, 132)
        } else if self.tool_config_status.starts_with("New") {
            ui.visuals().weak_text_color()
        } else {
            Color32::from_rgb(241, 103, 103)
        };
        ui.label(RichText::new(&self.tool_config_status).color(tool_status_color));
        ScrollArea::vertical()
            .id_salt("tool_toml")
            .max_height(320.0)
            .show(ui, |ui| {
                let dark_mode = self.persisted.theme == Theme::Dark;
                let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
                    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                    let job = highlight_toml(buffer.as_str(), dark_mode, &font_id, wrap_width);
                    ui.fonts(|fonts| fonts.layout_job(job))
                };
                let response = ui.add(
                    TextEdit::multiline(&mut self.tool_config_text)
                        .code_editor()
                        .layouter(&mut layouter)
                        .hint_text(self.locale_manager.text("tools.complete_hint"))
                        .desired_width(f32::INFINITY)
                        .desired_rows(18),
                );
                response.widget_info(|| {
                    WidgetInfo::labeled(
                        WidgetType::TextEdit,
                        true,
                        self.locale_manager.text("tools.editor"),
                    )
                });
            });
    }

    fn prompts_inspector(&mut self, ui: &mut Ui) {
        let prompt_filter = ui.add(
            TextEdit::singleline(&mut self.prompt_filter)
                .hint_text(self.locale_manager.text("prompt.filter"))
                .desired_width(f32::INFINITY),
        );
        prompt_filter.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::TextEdit,
                true,
                self.locale_manager.text("prompt.filter_label"),
            )
        });
        ui.horizontal(|ui| {
            if ui
                .add(fa_text_button(
                    FaIcon::Refresh,
                    self.locale_manager.text("prompt.reload"),
                ))
                .clicked()
            {
                self.reload_prompt_source();
            }
            if ui
                .add_enabled(
                    self.selected_prompt.is_some(),
                    fa_text_button(FaIcon::Save, self.locale_manager.text("prompt.save")),
                )
                .clicked()
            {
                self.save_prompt_source();
            }
        });
        let filter = self.prompt_filter.to_lowercase();
        let mut selected = None;
        ScrollArea::vertical()
            .id_salt("prompt_names")
            .max_height(210.0)
            .show(ui, |ui| {
                for name in &self.prompt_names {
                    if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if ui
                        .selectable_label(self.selected_prompt.as_ref() == Some(name), name)
                        .clicked()
                    {
                        selected = Some(name.clone());
                    }
                }
            });
        if let Some(name) = selected {
            self.selected_prompt = Some(name);
            self.reload_prompt_source();
        }
        ui.separator();
        let dark_mode = self.persisted.theme == Theme::Dark;
        let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let job = highlight_jinja(buffer.as_str(), dark_mode, &font_id, wrap_width);
            ui.fonts(|fonts| fonts.layout_job(job))
        };
        let response = ui.add(
            TextEdit::multiline(&mut self.prompt_source)
                .code_editor()
                .layouter(&mut layouter)
                .hint_text(self.locale_manager.text("prompt.select"))
                .desired_width(f32::INFINITY)
                .desired_rows(20),
        );
        response.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::TextEdit,
                true,
                self.locale_manager.text("prompt.editor"),
            )
        });
        ui.label(
            RichText::new(self.locale_manager.text("prompt.rebuild_hint"))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }

    #[allow(clippy::too_many_lines)]
    fn query_dock(&mut self, ctx: &Context) {
        let theme = self.persisted.theme;
        egui::SidePanel::right("query_dock")
            .exact_width(self.persisted.query_dock_width)
            .resizable(false)
            .frame(panel_frame(theme))
            .show(ctx, |ui| {
                let mut content_ui = fixed_panel_content_ui(ui, "query_dock_content");
                let ui = &mut content_ui;
                panel_header(ui, &self.locale_manager.text("query.title"), |ui| {
                    if dock_triangle_button(
                        ui,
                        "hide_query_dock",
                        "▶",
                        &self.locale_manager.text("dock.hide_query"),
                        QUERY_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.query_dock_visible = false;
                    }
                });
                let viewport_height = ui.available_height();
                let viewport_width = ui.available_width();
                ScrollArea::vertical()
                    .id_salt("query_dock_scroll")
                    .max_height(viewport_height)
                    .max_width(viewport_width)
                    .min_scrolled_width(viewport_width)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(viewport_width);
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let workspace_name = self.selected_workspace().map_or_else(
                                || self.locale_manager.text("query.no_workspace"),
                                |workspace| workspace.name.clone(),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} {workspace_name}",
                                    self.locale_manager.text("query.grounded_in")
                                ))
                                .color(ui.visuals().weak_text_color()),
                            );
                            if self.query_running() {
                                ui.spinner();
                                ui.label(
                                    RichText::new(self.locale_manager.text("query.thinking"))
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(self.locale_manager.text("query.question"))
                                    .strong()
                                    .color(ACCENT),
                            );
                            markdown_mode_controls(
                                ui,
                                &mut self.question,
                                true,
                                &self.locale_manager,
                            );
                        });
                        self.question.show(ui, "ai-question", 105.0);
                        let shortcut = ui.input(|input| {
                            input.modifiers.ctrl && input.key_pressed(egui::Key::Enter)
                        });
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    self.selected_workspace().is_some()
                                        && !self.question.markdown().trim().is_empty(),
                                    fa_text_button(
                                        FaIcon::Query,
                                        self.locale_manager.text("query.ask_ai"),
                                    ),
                                )
                                .clicked()
                                || shortcut
                            {
                                self.submit_query(ctx);
                            }
                            ui.checkbox(
                                &mut self.persisted.query_uses_llm,
                                self.locale_manager.text("query.use_llm"),
                            );
                            ui.checkbox(
                                &mut self.persisted.save_query_memory,
                                self.locale_manager.text("query.save_memory"),
                            );
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(self.locale_manager.text("query.answer"))
                                    .strong()
                                    .color(ACCENT),
                            );
                            markdown_mode_controls(
                                ui,
                                &mut self.answer,
                                true,
                                &self.locale_manager,
                            );
                        });
                        let evidence_open = !self.query_evidence.markdown().is_empty();
                        let answer_height = if evidence_open {
                            viewport_height.mul_add(0.58, -20.0).max(150.0)
                        } else {
                            (viewport_height - 210.0).max(180.0)
                        };
                        self.answer.show(ui, "ai-answer", answer_height);
                        if evidence_open {
                            ui.add_space(8.0);
                            egui::CollapsingHeader::new(self.locale_manager.text("query.evidence"))
                                .default_open(true)
                                .show(ui, |ui| {
                                    markdown_mode_controls(
                                        ui,
                                        &mut self.query_evidence,
                                        true,
                                        &self.locale_manager,
                                    );
                                    self.query_evidence.show(ui, "query-evidence", 180.0);
                                });
                        }
                    });
            });
    }

    fn central_workspace(&mut self, ctx: &Context) {
        let palette = self.persisted.theme.palette();
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.canvas).inner_margin(16.0))
            .show(ctx, |ui| {
                if self.open_document.is_some() {
                    self.markdown_editor_ui(ui, ctx);
                } else {
                    self.markdown_preview_ui(ui);
                }
            });
    }

    fn markdown_preview_ui(&mut self, ui: &mut Ui) {
        ui.heading(self.locale_manager.text("editor.title"));
        ui.label(
            RichText::new(self.locale_manager.text("editor.description"))
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(8.0);
        if self.search_preview.markdown().is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(self.locale_manager.text("editor.no_document"))
                        .size(18.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
            return;
        }
        markdown_mode_controls(ui, &mut self.search_preview, true, &self.locale_manager);
        ui.separator();
        self.search_preview
            .show(ui, "central-search-preview", ui.available_height());
    }

    fn markdown_editor_ui(&mut self, ui: &mut Ui, ctx: &Context) {
        let save_shortcut =
            ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::S));
        let mut save_requested = save_shortcut;
        let mut close_requested = false;

        let Some(document) = self.open_document.as_mut() else {
            return;
        };
        let file_name = document
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or_else(
                || self.locale_manager.text("editor.note"),
                ToOwned::to_owned,
            );
        ui.horizontal_wrapped(|ui| {
            ui.heading(&file_name);
            if document.dirty {
                ui.label(
                    RichText::new(self.locale_manager.text("editor.unsaved"))
                        .color(Color32::from_rgb(230, 126, 34)),
                );
            }
            save_requested |= ui
                .add(fa_text_button(
                    FaIcon::Save,
                    self.locale_manager.text("editor.save"),
                ))
                .clicked();
            if ui
                .add(fa_text_button(
                    FaIcon::Query,
                    self.locale_manager.text("editor.ai_query"),
                ))
                .clicked()
            {
                self.persisted.query_dock_visible = true;
            }
            close_requested = ui
                .add(fa_text_button(
                    FaIcon::Close,
                    self.locale_manager.text("editor.close"),
                ))
                .clicked();
        });
        ui.label(
            RichText::new(document.path.display().to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if markdown_mode_controls(ui, &mut document.editor, true, &self.locale_manager) {
                self.persisted.markdown_editor_mode = document.editor.mode();
            }
            ui.label(
                RichText::new(self.locale_manager.text("editor.help"))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        ui.separator();
        let editor_height = ui.available_height();
        document.dirty |= document
            .editor
            .show(ui, "workspace-document", editor_height);
        if save_requested {
            self.save_open_document();
        }
        if close_requested {
            self.close_open_document();
        }
    }

    #[allow(clippy::too_many_lines)]
    fn bottom_dock(&mut self, ctx: &Context) {
        let theme = self.persisted.theme;
        let maximum_height = (ctx.screen_rect().height() * 0.75)
            .clamp(OUTPUT_DOCK_MIN_HEIGHT, OUTPUT_DOCK_MAX_HEIGHT);
        let height_changed = self.update_output_dock_height_from_pointer(ctx, maximum_height);
        self.persisted.bottom_dock_height = self
            .persisted
            .bottom_dock_height
            .clamp(OUTPUT_DOCK_MIN_HEIGHT, maximum_height);
        if height_changed {
            ctx.data_mut(|data| {
                data.remove::<egui::containers::panel::PanelState>(Id::new("output_dock"));
            });
        }
        let panel = egui::TopBottomPanel::bottom("output_dock")
            .default_height(self.persisted.bottom_dock_height)
            .height_range(OUTPUT_DOCK_MIN_HEIGHT..=maximum_height)
            .resizable(false)
            .show_separator_line(true)
            .frame(panel_frame(theme))
            .show(ctx, |ui| {
                let mut content_ui = fixed_panel_content_ui(ui, "output_dock_content");
                let ui = &mut content_ui;
                panel_header(ui, &self.locale_manager.text("output.title"), |ui| {
                    ui.label(
                        RichText::new(self.locale_manager.text("output.drag_resize"))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    if ui
                        .add(fa_text_button(
                            FaIcon::Trash,
                            self.locale_manager.text("output.clear_finished"),
                        ))
                        .clicked()
                    {
                        self.jobs.retain(|job| job.status == JobStatus::Running);
                    }
                    if fa_icon_button(
                        ui,
                        "hide_output_dock",
                        FaIcon::CaretDown,
                        &self.locale_manager.text("dock.hide_output"),
                    )
                    .clicked()
                    {
                        self.persisted.bottom_dock_visible = false;
                    }
                });
                ui.separator();
                let selected_output = self
                    .selected_job
                    .and_then(|id| self.jobs.iter().find(|job| job.id == id))
                    .map(|job| job.output.clone());
                let workspace_context = self.selected_workspace().map_or_else(
                    || self.locale_manager.text("output.no_workspace"),
                    |workspace| workspace.root.display().to_string(),
                );
                let search_hits = self.search_hits.len();
                let prompt_count = self.prompt_names.len();
                let status = self.status.clone();
                let config_status = self.config_status.clone();
                let dark_theme = self.persisted.theme == Theme::Dark;
                let pane_height = (ui.available_height() - 24.0).max(1.0);
                let available_width = ui.available_width();
                let output_pane_width =
                    (available_width - 165.0 - 155.0 - 190.0 - ui.spacing().item_spacing.x * 3.0)
                        .max(400.0);
                let width_id = ui.make_persistent_id("activity_output_available_width");
                let previous_width =
                    ui.data(|data| data.get_temp::<f32>(width_id).unwrap_or_default());
                let reset_column_widths = (previous_width - available_width).abs() > 1.0;
                if reset_column_widths {
                    ui.data_mut(|data| data.insert_temp(width_id, available_width));
                }
                let table = TableBuilder::new(ui)
                    .id_salt("activity_output_panes")
                    .striped(false)
                    .resizable(true)
                    .vscroll(false)
                    .drag_to_scroll(false)
                    .auto_shrink(false)
                    .cell_layout(Layout::top_down(Align::Min))
                    .column(
                        Column::initial(165.0)
                            .range(105.0..=280.0)
                            .clip(true)
                            .resizable(true),
                    )
                    .column(
                        Column::initial(output_pane_width)
                            .at_least(400.0)
                            .clip(true)
                            .resizable(true),
                    )
                    .column(
                        Column::initial(155.0)
                            .range(105.0..=270.0)
                            .clip(true)
                            .resizable(true),
                    )
                    .column(
                        Column::initial(190.0)
                            .range(120.0..=320.0)
                            .clip(true)
                            .resizable(true),
                    );
                if reset_column_widths {
                    table.reset();
                }
                table
                    .header(20.0, |mut header| {
                        for title in [
                            self.locale_manager.text("output.jobs"),
                            self.locale_manager.text("output.output"),
                            self.locale_manager.text("output.context"),
                            self.locale_manager.text("output.status"),
                        ] {
                            header.col(|ui| {
                                let muted = ui.visuals().weak_text_color();
                                ui.label(RichText::new(title).strong().color(muted));
                            });
                        }
                    })
                    .body(|mut body| {
                        body.row(pane_height, |mut row| {
                            row.col(|ui| {
                                let pane_rect = ui.max_rect();
                                ui.allocate_ui_with_layout(
                                    ui.available_size(),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        ScrollArea::vertical().id_salt("job_list").show(ui, |ui| {
                                            for job in self.jobs.iter().rev() {
                                                let label = format!(
                                                    "● {} · {}",
                                                    job.label,
                                                    job.status.label()
                                                );
                                                if ui
                                                    .selectable_label(
                                                        self.selected_job == Some(job.id),
                                                        RichText::new(label)
                                                            .color(job.status.color()),
                                                    )
                                                    .clicked()
                                                {
                                                    self.selected_job = Some(job.id);
                                                }
                                            }
                                        });
                                    },
                                );
                                ui.interact(pane_rect, ui.id().with("jobs_pane"), Sense::hover())
                                    .widget_info(|| {
                                        WidgetInfo::labeled(WidgetType::Other, true, "Jobs pane")
                                    });
                            });
                            row.col(|ui| {
                                let pane_rect = ui.max_rect();
                                ui.allocate_ui_with_layout(
                                    ui.available_size(),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        if let Some(output) = selected_output.as_deref() {
                                            OutputTerminal::show(ui, output, dark_theme);
                                        } else {
                                            let muted = ui.visuals().weak_text_color();
                                            ui.label(
                                                RichText::new(
                                                    self.locale_manager.text("output.select_job"),
                                                )
                                                .color(muted),
                                            );
                                        }
                                    },
                                );
                                ui.interact(pane_rect, ui.id().with("output_pane"), Sense::hover())
                                    .widget_info(|| {
                                        WidgetInfo::labeled(WidgetType::Other, true, "Output pane")
                                    });
                            });
                            row.col(|ui| {
                                let pane_rect = ui.max_rect();
                                ui.allocate_ui_with_layout(
                                    ui.available_size(),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        let muted = ui.visuals().weak_text_color();
                                        ui.label(&workspace_context);
                                        ui.label(
                                            RichText::new(format!(
                                                "{search_hits} {}",
                                                self.locale_manager.text("output.search_hits")
                                            ))
                                            .color(muted),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{prompt_count} {}",
                                                self.locale_manager.text("output.prompts")
                                            ))
                                            .color(muted),
                                        );
                                    },
                                );
                                ui.interact(
                                    pane_rect,
                                    ui.id().with("context_pane"),
                                    Sense::hover(),
                                )
                                .widget_info(|| {
                                    WidgetInfo::labeled(WidgetType::Other, true, "Context pane")
                                });
                            });
                            row.col(|ui| {
                                let pane_rect = ui.max_rect();
                                ui.allocate_ui_with_layout(
                                    ui.available_size(),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        let muted = ui.visuals().weak_text_color();
                                        ui.label(&status);
                                        ui.label(
                                            RichText::new(&config_status).small().color(muted),
                                        );
                                    },
                                );
                                ui.interact(pane_rect, ui.id().with("status_pane"), Sense::hover())
                                    .widget_info(|| {
                                        WidgetInfo::labeled(WidgetType::Other, true, "Status pane")
                                    });
                            });
                        });
                    });
            });
        self.persisted.bottom_dock_height = panel.response.rect.height();
        self.output_dock_resize_handle(ctx, panel.response.rect);
    }

    fn update_output_dock_height_from_pointer(&mut self, ctx: &Context, maximum: f32) -> bool {
        let Some(start_height) = self.output_dock_drag_start_height else {
            return false;
        };
        let requested_height = ctx.input(|input| {
            let origin = input.pointer.press_origin()?;
            let pointer = input.pointer.latest_pos()?;
            Some((start_height + origin.y - pointer.y).clamp(OUTPUT_DOCK_MIN_HEIGHT, maximum))
        });
        if let Some(height) = requested_height {
            let changed = (self.persisted.bottom_dock_height - height).abs() > 0.5;
            self.persisted.bottom_dock_height = height;
            changed
        } else {
            false
        }
    }

    fn output_dock_resize_handle(&mut self, ctx: &Context, panel_rect: egui::Rect) {
        let handle_height = DOCK_RESIZE_GRAB_RADIUS * 2.0;
        let position = panel_rect.left_top() - Vec2::new(0.0, DOCK_RESIZE_GRAB_RADIUS);
        let handle_rect =
            egui::Rect::from_min_size(position, Vec2::new(panel_rect.width(), handle_height));
        let pressed_on_handle = ctx.input(|input| {
            input.pointer.button_pressed(egui::PointerButton::Primary)
                && input
                    .pointer
                    .press_origin()
                    .is_some_and(|origin| handle_rect.contains(origin))
        });
        if pressed_on_handle {
            self.output_dock_drag_start_height = Some(panel_rect.height());
        }
        egui::Area::new(Id::new("output_dock_resize_handle"))
            .order(Order::Foreground)
            .fixed_pos(position)
            .movable(false)
            .interactable(true)
            .show(ctx, |ui| {
                let (_, rect) = ui.allocate_space(Vec2::new(panel_rect.width(), handle_height));
                let response = ui.interact(
                    rect,
                    Id::new("output_dock_resize_handle").with("__resize"),
                    Sense::drag(),
                );
                let response = response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
                response.widget_info(|| {
                    WidgetInfo::labeled(WidgetType::Other, true, "Resize output dock")
                });
                if response.hovered() || response.dragged() {
                    ui.painter().hline(
                        rect.x_range(),
                        rect.center().y,
                        Stroke::new(2.0_f32, ACCENT),
                    );
                }
            });
        if self.output_dock_drag_start_height.is_some() {
            ctx.request_repaint();
        }
        if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
            self.output_dock_drag_start_height = None;
        }
    }

    fn restore_docks(&mut self, ctx: &Context) {
        if self.persisted.workspace_dock_visible
            && self.persisted.search_dock_visible
            && self.persisted.query_dock_visible
        {
            return;
        }
        let top = if self.persisted.toolbar_visible {
            82.0
        } else {
            38.0
        };
        if !self.persisted.workspace_dock_visible || !self.persisted.search_dock_visible {
            egui::Area::new(Id::new("restore_left_docks"))
                .order(Order::Foreground)
                .fixed_pos(egui::pos2(ACTIVITY_RAIL_WIDTH + 13.0, top))
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.vertical(|ui| {
                        if !self.persisted.workspace_dock_visible
                            && restore_triangle_button(
                                ui,
                                "restore_workspace_dock",
                                RESTORE_FROM_LEFT_GLYPH,
                                &self.locale_manager.text("dock.restore_workspace"),
                                WORKSPACE_DOCK_COLOR,
                            )
                            .clicked()
                        {
                            self.persisted.workspace_dock_visible = true;
                        }
                        if !self.persisted.search_dock_visible
                            && restore_triangle_button(
                                ui,
                                "restore_search_dock",
                                RESTORE_FROM_LEFT_GLYPH,
                                &self.locale_manager.text("dock.restore_search"),
                                SEARCH_DOCK_COLOR,
                            )
                            .clicked()
                        {
                            self.persisted.search_dock_visible = true;
                        }
                    });
                });
        }
        if !self.persisted.query_dock_visible {
            egui::Area::new(Id::new("restore_right_dock"))
                .order(Order::Foreground)
                .fixed_pos(egui::pos2(ctx.screen_rect().right() - 28.0, top))
                .show(ctx, |ui| {
                    if restore_triangle_button(
                        ui,
                        "restore_query_dock",
                        RESTORE_FROM_RIGHT_GLYPH,
                        &self.locale_manager.text("dock.restore_query"),
                        QUERY_DOCK_COLOR,
                    )
                    .clicked()
                    {
                        self.persisted.query_dock_visible = true;
                    }
                });
        }
    }

    fn dialogs(&mut self, ctx: &Context) {
        if let Some(root) = self.workspace_pending_removal.clone() {
            let mut open = true;
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new(self.locale_manager.text("dialog.remove_title"))
                .id(Id::new("confirm_remove_workspace_dialog"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(self.locale_manager.text("dialog.remove_question"));
                    ui.label(RichText::new(root.display().to_string()).strong());
                    ui.label(
                        RichText::new(self.locale_manager.text("dialog.retain_files"))
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        confirm = ui
                            .add(fa_text_button(
                                FaIcon::Trash,
                                self.locale_manager.text("dialog.remove"),
                            ))
                            .clicked();
                        cancel = ui
                            .add(fa_text_button(
                                FaIcon::Close,
                                self.locale_manager.text("dialog.cancel"),
                            ))
                            .clicked();
                    });
                });
            if confirm {
                self.remove_workspace(&root);
                self.workspace_pending_removal = None;
            } else if cancel || !open {
                self.workspace_pending_removal = None;
            }
        }

        if self.show_about {
            egui::Window::new(self.locale_manager.text("about.title"))
                .open(&mut self.show_about)
                .resizable(false)
                .show(ctx, |ui| {
                    if let Some(logo) = &self.logo_texture {
                        ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::new(440.0, 83.0)));
                    } else {
                        ui.heading("BIBIIWIKI");
                    }
                    ui.label(self.locale_manager.text("about.description"));
                    ui.label("Rust · egui · LiteLLM · Codex");
                    ui.hyperlink_to("egui", "https://github.com/emilk/egui");
                });
        }
    }

    fn selected_workspace(&self) -> Option<&WorkspaceEntry> {
        self.persisted
            .workspaces
            .get(self.persisted.selected_workspace)
    }

    fn open_markdown_file(&mut self, workspace_root: PathBuf, path: PathBuf) {
        let result = (|| -> Result<OpenDocument> {
            if !path.starts_with(&workspace_root) {
                bail!("note is outside the selected workspace");
            }
            if !is_markdown_path(&path) {
                bail!("only .md and .markdown files can be opened in the editor");
            }
            if !path.is_file() {
                bail!("note does not exist: {}", path.display());
            }
            let source = fs::read_to_string(&path)
                .with_context(|| format!("could not read {} as UTF-8", path.display()))?;
            let editor = MarkdownEditor::new(
                path.to_string_lossy(),
                source,
                self.persisted.markdown_editor_mode,
            );
            Ok(OpenDocument {
                path,
                workspace_root,
                editor,
                dirty: false,
            })
        })();

        match result {
            Ok(document) => {
                self.status = format!("Opened {}", document.path.display());
                self.open_document = Some(document);
                self.persisted.inspector_visible = false;
            }
            Err(error) => self.status = format!("Could not open note: {error:#}"),
        }
    }

    fn save_open_document(&mut self) {
        let Some(document) = self.open_document.as_mut() else {
            "No Markdown note is open".clone_into(&mut self.status);
            return;
        };
        if !document.path.starts_with(&document.workspace_root) {
            "Could not save note: path is outside its workspace".clone_into(&mut self.status);
            return;
        }
        match fs::write(&document.path, document.editor.markdown()) {
            Ok(()) => {
                document.dirty = false;
                self.status = format!("Saved {}", document.path.display());
            }
            Err(error) => {
                self.status = format!("Could not save {}: {error}", document.path.display());
            }
        }
    }

    fn close_open_document(&mut self) {
        if self
            .open_document
            .as_ref()
            .is_some_and(|document| document.dirty)
        {
            "This note has unsaved changes; save it before closing".clone_into(&mut self.status);
            return;
        }
        self.open_document = None;
        "Closed Markdown editor".clone_into(&mut self.status);
    }

    fn choose_workspace_directory(&mut self) {
        let initial_directory = self.selected_workspace().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            |workspace| {
                if workspace.root.is_dir() {
                    workspace.root.clone()
                } else {
                    workspace
                        .root
                        .parent()
                        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
                }
            },
        );
        let selection = catch_unwind(AssertUnwindSafe(|| {
            (self.directory_picker)(&initial_directory)
        }));
        match selection {
            Ok(Ok(Some(root))) => self.add_workspace_path(root),
            Ok(Ok(None)) => "Workspace selection cancelled".clone_into(&mut self.status),
            Ok(Err(error)) => {
                self.status = format!("Could not open directory picker: {error:#}");
            }
            Err(payload) => {
                self.status = format!(
                    "Could not open directory picker: {}",
                    panic_payload_message(payload.as_ref())
                );
            }
        }
    }

    fn request_ingest_sources(&mut self) {
        if self.selected_workspace().is_none() {
            "Cannot run Ingest: add and select a wiki root first".clone_into(&mut self.status);
            return;
        }
        self.persisted.inspector = Inspector::Ingest;
        self.persisted.inspector_visible = true;
        self.ingest_source_panel_open = true;
        self.refresh_ingest_status();
        "Choose source files or directories to ingest".clone_into(&mut self.status);
    }

    fn choose_ingest_sources(&mut self, ctx: &Context, mode: IngestPickerMode) {
        let initial_directory = ingest_picker_initial_directory(&self.persisted.source_dir);
        let selection = catch_unwind(AssertUnwindSafe(|| {
            (self.ingest_source_picker)(mode, &initial_directory)
        }));
        match selection {
            Ok(Ok(Some(inputs))) if !inputs.is_empty() => {
                let mode = if self.persisted.force_ingest {
                    WikiIngestMode::Redo
                } else {
                    WikiIngestMode::Start
                };
                self.launch_ingest(ctx, inputs, mode);
            }
            Ok(Ok(Some(_) | None)) => {
                "Ingest source selection cancelled".clone_into(&mut self.status);
            }
            Ok(Err(error)) => {
                self.status = format!("Could not open ingest source picker: {error:#}");
            }
            Err(payload) => {
                self.status = format!(
                    "Could not open ingest source picker: {}",
                    panic_payload_message(payload.as_ref())
                );
            }
        }
    }

    fn launch_ingest(&mut self, ctx: &Context, inputs: Vec<PathBuf>, mode: WikiIngestMode) {
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            "Cannot run Ingest: add and select a wiki root first".clone_into(&mut self.status);
            return;
        };
        let mut args = self.base_cli_args();
        args.extend(["wiki".into(), "ingest".into()]);
        for input in inputs {
            push_path_arg(&mut args, "--input", &input);
        }
        push_path_arg(&mut args, "--source-dir", &self.persisted.source_dir);
        push_path_arg(&mut args, "--wiki-root", &root);
        push_path_arg(&mut args, "--workspace", &self.persisted.agent_workspace);
        match mode {
            WikiIngestMode::Start => {}
            WikiIngestMode::Resume => args.push("--resume".into()),
            WikiIngestMode::Redo => args.push("--redo".into()),
        }
        self.spawn_cli_job(ctx, "Ingest", JobPurpose::Ingest, args);
    }

    fn refresh_ingest_status(&mut self) {
        let root = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone());
        self.ingest_status = root
            .as_deref()
            .and_then(|root| match WikiIngestStatus::load(root) {
                Ok(status) => status,
                Err(error) => {
                    self.status = format!("Could not read ingest status: {error:#}");
                    None
                }
            });
        self.ingest_status_root = root;
    }

    fn add_workspace_path(&mut self, selected: PathBuf) {
        let result = (|| -> Result<PathBuf> {
            let root = absolute_path(selected)?;
            if !root.is_dir() {
                bail!("selected path is not a directory: {}", root.display());
            }
            Ok(root)
        })();
        match result {
            Ok(root) => {
                self.persisted.ensure_workspace(root.clone());
                let registration =
                    catch_unwind(AssertUnwindSafe(|| (self.workspace_registrar)(&root)));
                self.status = match registration {
                    Ok(Ok(registration)) => {
                        if let Some(workspace) = self
                            .persisted
                            .workspaces
                            .get_mut(self.persisted.selected_workspace)
                        {
                            workspace.obsidian_vault_id = Some(registration.vault_id.clone());
                            workspace.obsidian_uri = Some(registration.uri.clone());
                        }
                        format!(
                            "Selected workspace {}; Obsidian vault {} indexed",
                            root.display(),
                            registration.vault_id
                        )
                    }
                    Ok(Err(error)) => format!(
                        "Selected workspace {}; Obsidian setup failed: {error:#}",
                        root.display()
                    ),
                    Err(payload) => format!(
                        "Selected workspace {}; Obsidian setup failed: {}",
                        root.display(),
                        panic_payload_message(payload.as_ref())
                    ),
                };
                self.search_hits.clear();
                self.clear_search_selection();
                self.workspace_trees
                    .insert(root.clone(), build_workspace_file_tree(&root));
            }
            Err(error) => self.status = format!("Could not add workspace: {error:#}"),
        }
    }

    fn remove_workspace(&mut self, root: &Path) {
        let Some(index) = self
            .persisted
            .workspaces
            .iter()
            .position(|workspace| same_path(&workspace.root, root))
        else {
            "Workspace is no longer registered".clone_into(&mut self.status);
            return;
        };

        let selected = self.persisted.selected_workspace;
        let removed = self.persisted.workspaces.remove(index);
        self.workspace_trees.remove(&removed.root);
        if index < selected {
            self.persisted.selected_workspace = selected - 1;
        } else if index == selected {
            self.persisted.selected_workspace =
                index.min(self.persisted.workspaces.len().saturating_sub(1));
        }
        self.persisted.sanitize();
        self.search_hits.clear();
        self.clear_search_selection();
        self.status = format!(
            "Removed {} from the workspace list; files were not deleted",
            removed.root.display()
        );
    }

    fn open_selected_workspace(&mut self) {
        if self.selected_workspace().is_some() {
            self.open_workspace(self.persisted.selected_workspace);
        } else {
            "No workspace selected".clone_into(&mut self.status);
        }
    }

    fn open_workspace(&mut self, index: usize) {
        let Some(workspace) = self.persisted.workspaces.get(index) else {
            return;
        };
        match (self.workspace_opener)(&workspace.root) {
            Ok(()) => self.status = format!("Opened {} in Obsidian", workspace.name),
            Err(error) => self.status = format!("Could not open Obsidian: {error:#}"),
        }
    }

    fn open_search_hit_in_editor(&mut self, index: usize) {
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            "No workspace selected".clone_into(&mut self.status);
            return;
        };
        let Some(relative_path) = self.search_hits.get(index).map(|hit| hit.path.clone()) else {
            return;
        };
        // A double-click means "open this file", not "attach this file to AI Query".
        // Clear the transient single-click selection that precedes egui's double-click event.
        self.clear_search_selection();
        let path = root.join(relative_path);
        self.open_markdown_file(root, path);
    }

    fn clear_search_selection(&mut self) {
        self.selected_hit = None;
        self.search_preview.set_markdown("");
        self.query_evidence.set_markdown("");
    }

    fn open_search_hit_in_obsidian(&mut self, index: usize) {
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            "No workspace selected".clone_into(&mut self.status);
            return;
        };
        let Some(hit) = self.search_hits.get(index) else {
            return;
        };
        match (self.note_opener)(&root, Path::new(&hit.path)) {
            Ok(()) => self.status = format!("Opened {} in a new Obsidian tab", hit.title),
            Err(error) => self.status = format!("Could not open Obsidian note: {error:#}"),
        }
    }

    fn run_search(&mut self) {
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            self.search_hits.clear();
            self.clear_search_selection();
            "No workspace selected; add a wiki root before searching".clone_into(&mut self.status);
            return;
        };
        match WikiSearch::new(root).search_detailed(&self.search_query, self.persisted.search_limit)
        {
            Ok(results) => {
                self.search_hits = results.hits;
                self.clear_search_selection();
                let backend = match results.backend {
                    SearchBackend::Tantivy if results.index_updated => "Tantivy (index refreshed)",
                    SearchBackend::Tantivy => "Tantivy",
                    SearchBackend::RipgrepFallback => "ripgrep fallback",
                };
                self.status = format!(
                    "Search returned {} result(s) via {backend}",
                    self.search_hits.len()
                );
            }
            Err(error) => {
                self.search_hits.clear();
                self.status = format!("Search failed: {error:#}");
            }
        }
    }

    fn select_search_hit(&mut self, index: usize) {
        self.selected_hit = Some(index);
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            self.clear_search_selection();
            "No workspace selected".clone_into(&mut self.status);
            return;
        };
        let Some(hit) = self.search_hits.get(index) else {
            return;
        };
        let path = root.join(&hit.path);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| format!("Could not read {}: {error}", path.display()));
        self.search_preview.set_markdown(content.clone());
        self.query_evidence.set_markdown(content);
    }

    fn reload_config(&mut self) {
        match fs::read_to_string(&self.config_path) {
            Ok(source) => {
                if let Ok(Some(mut state)) = parse_local_settings(&source) {
                    state.sanitize();
                    self.persisted = state;
                    self.persisted_snapshot = self.persisted.clone();
                    self.sync_tool_config_from_controls();
                }
                self.config_text = source;
                self.llm_wizard = LlmWizardState::from_config_and_saved_selection(
                    &self.config_text,
                    &self.persisted,
                );
                self.config_status = validate_config_text(&self.config_text)
                    .unwrap_or_else(|error| format!("Configuration error: {error:#}"));
            }
            Err(error) => self.config_status = format!("Could not reload configuration: {error}"),
        }
    }

    fn save_config(&mut self) {
        match Config::prepare_edit(&self.config_text) {
            Ok(preserved) => match merge_local_settings(&preserved, &self.persisted) {
                Ok(unified) => match fs::write(&self.config_path, &unified) {
                    Ok(()) => {
                        self.config_text = unified;
                        self.persisted_snapshot = self.persisted.clone();
                        "✓ Unified configuration saved and valid"
                            .clone_into(&mut self.config_status);
                        self.status = format!("Saved {}", self.config_path.display());
                    }
                    Err(error) => {
                        self.config_status = format!("Could not save configuration: {error}");
                    }
                },
                Err(error) => {
                    self.config_status = format!("Could not merge local settings: {error:#}");
                }
            },
            Err(error) => {
                self.config_status = format!("Not saved; configuration error: {error:#}");
            }
        }
    }

    fn sync_tool_config_from_controls(&mut self) {
        match render_tool_config(&ToolConfig::from_persisted(&self.persisted)) {
            Ok(source) => {
                self.tool_config_text = source;
                self.tool_config_status =
                    "TOML updated from quick controls; save to persist".into();
            }
            Err(error) => {
                self.tool_config_status = format!("Could not render tool configuration: {error:#}");
            }
        }
    }

    fn reload_tool_config(&mut self) {
        match load_local_settings_file(&self.config_path) {
            Ok(Some(state)) => {
                ToolConfig::from_persisted(&state).apply_to(&mut self.persisted);
                self.sync_tool_config_from_controls();
                "✓ Tool configuration reloaded from unified YAML"
                    .clone_into(&mut self.tool_config_status);
            }
            Ok(None) => {
                self.sync_tool_config_from_controls();
                "No saved tool section; using current defaults"
                    .clone_into(&mut self.tool_config_status);
            }
            Err(error) => {
                self.tool_config_status =
                    format!("Could not reload unified configuration: {error:#}");
            }
        }
    }

    fn validate_tool_config(&mut self) {
        self.tool_config_status = parse_tool_config(&self.tool_config_text).map_or_else(
            |error| format!("TOML tool configuration error: {error:#}"),
            |_| "✓ TOML tool configuration is valid".to_owned(),
        );
    }

    fn apply_tool_config(&mut self) {
        match parse_tool_config(&self.tool_config_text) {
            Ok(config) => {
                config.apply_to(&mut self.persisted);
                "✓ TOML tool configuration applied".clone_into(&mut self.tool_config_status);
                "Applied TOML tool configuration".clone_into(&mut self.status);
            }
            Err(error) => {
                self.tool_config_status =
                    format!("TOML tool configuration was not applied: {error:#}");
            }
        }
    }

    fn save_tool_config(&mut self) {
        match parse_tool_config(&self.tool_config_text) {
            Ok(config) => {
                config.apply_to(&mut self.persisted);
                match self.persist_local_settings() {
                    Ok(()) => {
                        "✓ TOML tool values saved inside unified YAML"
                            .clone_into(&mut self.tool_config_status);
                        self.status = format!("Saved {}", self.config_path.display());
                    }
                    Err(error) => {
                        self.tool_config_status =
                            format!("Could not save unified configuration: {error:#}");
                    }
                }
            }
            Err(error) => {
                self.tool_config_status =
                    format!("Not saved; TOML tool configuration error: {error:#}");
            }
        }
    }

    fn persist_local_settings(&mut self) -> Result<()> {
        persist_local_settings_file(&self.config_path, &self.persisted)?;
        self.persisted_snapshot = self.persisted.clone();
        Ok(())
    }

    fn persist_changed_local_settings(&mut self) {
        if !self.local_persistence || self.persisted == self.persisted_snapshot {
            return;
        }
        if let Err(error) = self.persist_local_settings() {
            self.persisted_snapshot = self.persisted.clone();
            self.status = format!("Could not save local settings: {error:#}");
        }
    }

    fn reload_prompt_source(&mut self) {
        let Some(name) = self.selected_prompt.as_deref() else {
            return;
        };
        let path = prompt_path(name);
        match fs::read_to_string(&path) {
            Ok(source) => {
                self.prompt_source = source;
                self.status = format!("Loaded prompt {name}");
            }
            Err(error) => {
                self.prompt_source = format!("Prompt source is unavailable: {error}");
            }
        }
    }

    fn save_prompt_source(&mut self) {
        let Some(name) = self.selected_prompt.as_deref() else {
            return;
        };
        let path = prompt_path(name);
        match fs::write(&path, &self.prompt_source) {
            Ok(()) => self.status = format!("Saved prompt {}; rebuild to embed it", path.display()),
            Err(error) => self.status = format!("Could not save prompt: {error}"),
        }
    }

    fn launch_task(&mut self, ctx: &Context, label: &str, task: TaskCommand) {
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            self.status = format!("Cannot run {label}: add and select a wiki root first");
            return;
        };
        let mut args = self.base_cli_args();
        args.push("wiki".into());
        match task {
            TaskCommand::Lint => {
                args.push("lint".into());
                push_path_arg(&mut args, "--wiki-root", &root);
            }
            TaskCommand::Update => {
                args.push("update".into());
                push_path_arg(&mut args, "--wiki-root", &root);
            }
        }
        self.spawn_cli_job(ctx, label, JobPurpose::Task, args);
    }

    fn submit_query(&mut self, ctx: &Context) {
        let question = self.question.markdown().trim().to_owned();
        if question.is_empty() || self.query_running() {
            return;
        }
        let Some(root) = self
            .selected_workspace()
            .map(|workspace| workspace.root.clone())
        else {
            "Cannot query: add and select a wiki root first".clone_into(&mut self.status);
            return;
        };
        let mut args = self.base_cli_args();
        args.extend(["wiki".into(), "query".into(), question.clone().into()]);
        push_path_arg(&mut args, "--wiki-root", &root);
        push_path_arg(&mut args, "--workspace", &self.persisted.agent_workspace);
        if !self.persisted.query_uses_llm {
            args.push("--no-llm".into());
        }
        if !self.persisted.save_query_memory {
            args.push("--no-save".into());
        }
        self.answer.set_markdown("Query running…");
        self.spawn_cli_job(ctx, "AI query", JobPurpose::Query, args);
    }

    fn base_cli_args(&self) -> Vec<OsString> {
        vec![
            "--config".into(),
            self.config_path.as_os_str().to_os_string(),
        ]
    }

    fn spawn_cli_job(
        &mut self,
        ctx: &Context,
        label: &str,
        purpose: JobPurpose,
        args: Vec<OsString>,
    ) {
        let id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push_back(JobRecord {
            id,
            label: label.to_owned(),
            purpose,
            status: JobStatus::Running,
            output: format!("Starting {label}…"),
            ingest_steps: (purpose == JobPurpose::Ingest).then(|| {
                let mut steps = [IngestStepState::Pending; INGEST_STAGE_COUNT];
                steps[WikiIngestStage::Initialize.index()] = IngestStepState::Running;
                steps
            }),
            ingest_units: (purpose == JobPurpose::Ingest)
                .then(|| [IngestUnitProgress::default(); INGEST_STAGE_COUNT]),
            ingest_llm_checks: (purpose == JobPurpose::Ingest)
                .then_some([IngestStepState::Pending; INGEST_LLM_CHECK_COUNT]),
        });
        self.selected_job = Some(id);
        self.persisted.bottom_dock_visible = true;
        self.status = format!("{label} started");

        let tx = self.job_tx.clone();
        let repaint = ctx.clone();
        let working_directory = self.persisted.agent_workspace.clone();
        let runner = Arc::clone(&self.job_runner);
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let progress_repaint = repaint.clone();
            let observe_stdout = move |line: &str| {
                if let Some(progress) = WikiIngestProgress::parse_marker(line) {
                    let _ = progress_tx.send(JobMessage::IngestProgress { id, progress });
                    progress_repaint.request_repaint();
                }
            };
            let result = runner(&args, &working_directory, &observe_stdout);
            let message = match result {
                Ok((success, stdout, stderr)) => JobMessage::Finished {
                    id,
                    purpose,
                    success,
                    stdout,
                    stderr,
                },
                Err(error) => JobMessage::Finished {
                    id,
                    purpose,
                    success: false,
                    stdout: String::new(),
                    stderr: format!("{error:#}"),
                },
            };
            let _ = tx.send(message);
            repaint.request_repaint();
        });
    }

    fn poll_jobs(&mut self) {
        let messages: Vec<_> = self.job_rx.try_iter().collect();
        let mut refresh_ingest_status = false;
        for message in messages {
            match message {
                JobMessage::IngestProgress { id, progress } => {
                    let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
                        continue;
                    };
                    let step_state = match progress.state {
                        WikiIngestStageState::Running => IngestStepState::Running,
                        WikiIngestStageState::Succeeded => IngestStepState::Succeeded,
                        WikiIngestStageState::Failed => IngestStepState::Failed,
                    };
                    if let Some(check) = progress.llm_check {
                        if let Some(checks) = job.ingest_llm_checks.as_mut() {
                            checks[check.index()] = step_state;
                        }
                        if let Some(steps) = job.ingest_steps.as_mut() {
                            steps[progress.stage.index()] = if step_state == IngestStepState::Failed
                            {
                                IngestStepState::Failed
                            } else {
                                IngestStepState::Running
                            };
                        }
                    } else if let Some(steps) = job.ingest_steps.as_mut() {
                        steps[progress.stage.index()] = step_state;
                    }
                    if let Some(units) = job.ingest_units.as_mut() {
                        units[progress.stage.index()] = progress.into();
                    }
                    append_job_output_line(&mut job.output, &ingest_progress_output_line(progress));
                    self.status =
                        format!("Ingest: {} {}", progress.stage.label(), step_state.label());
                }
                JobMessage::Finished {
                    id,
                    purpose,
                    success,
                    stdout,
                    stderr,
                } => {
                    let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
                        continue;
                    };
                    let checkpoints_complete =
                        job.ingest_steps.as_ref().is_none_or(|steps| {
                            steps.iter().all(|step| *step == IngestStepState::Succeeded)
                        }) && job.ingest_llm_checks.as_ref().is_none_or(|checks| {
                            checks
                                .iter()
                                .all(|check| *check == IngestStepState::Succeeded)
                        });
                    let success = success && checkpoints_complete;
                    job.status = if success {
                        JobStatus::Succeeded
                    } else {
                        JobStatus::Failed
                    };
                    let stderr = if !checkpoints_complete && stderr.trim().is_empty() {
                        "Ingest exited without completing every reported checkpoint".to_owned()
                    } else {
                        stderr
                    };
                    let final_output = combined_output(&stdout, &stderr);
                    if purpose == JobPurpose::Ingest {
                        append_job_output_block(&mut job.output, &final_output);
                        refresh_ingest_status = true;
                    } else {
                        job.output = final_output;
                    }
                    if let Some(steps) = job.ingest_steps.as_mut()
                        && !success
                        && let Some(step) = steps
                            .iter_mut()
                            .find(|step| **step == IngestStepState::Running)
                    {
                        *step = IngestStepState::Failed;
                    }
                    self.status = format!("{} {}", job.label, job.status.label());
                    if purpose == JobPurpose::Query {
                        self.answer.set_markdown(if success {
                            stdout.trim().to_owned()
                        } else {
                            format!("Query failed:\n{}", stderr.trim())
                        });
                    } else if success {
                        self.workspace_trees.clear();
                    }
                }
            }
        }
        if refresh_ingest_status {
            self.refresh_ingest_status();
        }
        while self.jobs.len() > 50 {
            self.jobs.pop_front();
        }
    }

    fn start_llm_diagnostic(&mut self) {
        self.llm_diagnostic = LlmDiagnosticState {
            phase: LlmDiagnosticPhase::TestingModel,
            model_result: None,
            proxy_result: None,
            codex_result: None,
        };
        "Testing configured LLM".clone_into(&mut self.status);
        let config_text = self.config_text.clone();
        let sender = self.llm_diagnostic_tx.clone();
        let runner = Arc::clone(&self.llm_diagnostic_runner);
        let workspace = self.persisted.agent_workspace.clone();
        thread::spawn(move || {
            let failure_sender = sender.clone();
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                runner(&config_text, &workspace, &sender);
            })) {
                let _ = failure_sender.send(LlmDiagnosticMessage::WorkerFailed(format!(
                    "Diagnostic worker failed: {}",
                    panic_payload_message(payload.as_ref())
                )));
            }
        });
    }

    fn poll_llm_diagnostic(&mut self) {
        for message in self.llm_diagnostic_rx.try_iter() {
            match message {
                LlmDiagnosticMessage::ModelPassed(reply) => {
                    self.llm_diagnostic.model_result = Some(Ok(reply));
                    "Configured LLM works; testing Responses proxy".clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::ModelFailed(error) => {
                    self.llm_diagnostic.model_result = Some(Err(error));
                    self.llm_diagnostic.proxy_result = None;
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::Complete;
                    "Configured LLM test failed".clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::ProxyStarted => {
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::TestingProxy;
                }
                LlmDiagnosticMessage::ProxyPassed(reply) => {
                    self.llm_diagnostic.proxy_result = Some(Ok(reply));
                    "Configured LLM and LLM proxy work; testing Codex adapter"
                        .clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::ProxyFailed(error) => {
                    self.llm_diagnostic.proxy_result = Some(Err(error));
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::Complete;
                    "Configured LLM works, but the Responses proxy test failed"
                        .clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::CodexStarted => {
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::TestingCodex;
                }
                LlmDiagnosticMessage::CodexPassed(reply) => {
                    self.llm_diagnostic.codex_result = Some(Ok(reply));
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::Complete;
                    "Configured LLM, LLM proxy, and Codex adapter work"
                        .clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::CodexFailed(error) => {
                    self.llm_diagnostic.codex_result = Some(Err(error));
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::Complete;
                    "Configured LLM and LLM proxy work, but the Codex adapter test failed"
                        .clone_into(&mut self.status);
                }
                LlmDiagnosticMessage::WorkerFailed(error) => {
                    if self.llm_diagnostic.phase == LlmDiagnosticPhase::TestingCodex {
                        self.llm_diagnostic.codex_result = Some(Err(error));
                        "Configured LLM and LLM proxy work, but the diagnostic worker failed"
                            .clone_into(&mut self.status);
                    } else if self.llm_diagnostic.model_result.is_some() {
                        self.llm_diagnostic.proxy_result = Some(Err(error));
                        "Configured LLM works, but the diagnostic worker failed"
                            .clone_into(&mut self.status);
                    } else {
                        self.llm_diagnostic.model_result = Some(Err(error));
                        "Configured LLM test failed".clone_into(&mut self.status);
                    }
                    self.llm_diagnostic.phase = LlmDiagnosticPhase::Complete;
                }
            }
        }
    }

    fn query_running(&self) -> bool {
        self.jobs
            .iter()
            .any(|job| job.purpose == JobPurpose::Query && job.status == JobStatus::Running)
    }

    fn render_recovery(&mut self, ctx: &Context) {
        configure_style(ctx, self.persisted.theme);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(36.0);
                if let Some(logo) = &self.logo_texture {
                    ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::new(440.0, 83.0)));
                } else {
                    ui.heading("BIBIIWIKI");
                }
                ui.add_space(24.0);
                ui.heading("The workspace recovered from a UI error");
                ui.label("The window remains available so the problem can be corrected.");
                ui.add_space(12.0);
                if let Some(error) = &self.recovery_error {
                    ui.monospace(error);
                }
                ui.add_space(12.0);
                ui.label(format!("Configuration: {}", self.config_path.display()));
                if ui
                    .add(fa_text_button(FaIcon::Refresh, "Retry normal workspace"))
                    .clicked()
                {
                    self.recovery_error = None;
                }
            });
        });
    }
}

impl eframe::App for BibiiWikiApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.recovery_error.is_some() {
            self.render_recovery(ctx);
            return;
        }
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.render(ctx))) {
            self.recovery_error = Some(format!(
                "Unexpected UI failure: {}",
                panic_payload_message(payload.as_ref())
            ));
            ctx.request_repaint();
        }
        self.persist_changed_local_settings();
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        if let Err(error) = self.persist_local_settings() {
            self.status = format!("Could not save local settings: {error:#}");
        }
    }
}

fn compact_rail_button(
    ui: &mut Ui,
    icon: FaIcon,
    label: &str,
    selected: bool,
    color: Color32,
) -> Response {
    let border = ui.visuals().widgets.inactive.bg_stroke.color;
    let text_color = if selected {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().interact_size = Vec2::splat(ACTIVITY_BUTTON_SIZE);
            ui.spacing_mut().button_padding = Vec2::splat(3.0);
            ui.add_sized(
                Vec2::splat(ACTIVITY_BUTTON_SIZE),
                egui::Button::image(icon.tintable_image(14.0).tint(text_color))
                    .fill(if selected {
                        color
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(Stroke::new(1.0_f32, if selected { color } else { border })),
            )
        })
        .inner;
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    response.on_hover_text(label)
}

fn dock_triangle_button(
    ui: &mut Ui,
    id: &str,
    glyph: &str,
    label: &str,
    color: Color32,
) -> Response {
    let response = ui
        .push_id(id, |ui| {
            ui.spacing_mut().button_padding = Vec2::ZERO;
            ui.spacing_mut().interact_size = Vec2::splat(RESTORE_TRIANGLE_SIZE);
            ui.add_sized(
                Vec2::splat(RESTORE_TRIANGLE_SIZE),
                egui::Button::new(RichText::new(glyph).size(11.0).strong().color(color))
                    .frame(false),
            )
        })
        .inner;
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    response.on_hover_text(label)
}

fn restore_triangle_button(
    ui: &mut Ui,
    id: &str,
    glyph: &str,
    label: &str,
    color: Color32,
) -> Response {
    dock_triangle_button(ui, id, glyph, label, color)
}

#[derive(Clone, Copy)]
struct DockResizeSpec {
    panel_id: &'static str,
    handle_id: &'static str,
    label: &'static str,
    dock: DockResize,
    side: egui::containers::panel::Side,
    minimum_width: f32,
    maximum_width: f32,
    color: Color32,
}

fn side_panel_resize_handle(
    ctx: &Context,
    spec: DockResizeSpec,
    active_dock: &mut Option<DockResize>,
    dock_width: &mut f32,
) {
    use egui::containers::panel::PanelState;

    let Some(mut state) = PanelState::load(ctx, Id::new(spec.panel_id)) else {
        return;
    };
    let resize_id = Id::new(spec.handle_id).with("__resize");
    let initial_boundary_x = match spec.side {
        egui::containers::panel::Side::Left => state.rect.right(),
        egui::containers::panel::Side::Right => state.rect.left(),
    };
    let initial_handle_rect = egui::Rect::from_min_size(
        egui::pos2(
            initial_boundary_x - DOCK_RESIZE_GRAB_RADIUS,
            state.rect.top(),
        ),
        Vec2::new(DOCK_RESIZE_GRAB_RADIUS * 2.0, state.rect.height()),
    );
    let pressed_on_handle = ctx.input(|input| {
        input.pointer.button_pressed(egui::PointerButton::Primary)
            && input
                .pointer
                .press_origin()
                .is_some_and(|position| initial_handle_rect.contains(position))
    });
    if pressed_on_handle {
        *active_dock = Some(spec.dock);
    }
    let active = *active_dock == Some(spec.dock);
    let pointer = ctx.input(|input| input.pointer.latest_pos());
    if active && let Some(pointer) = pointer {
        match spec.side {
            egui::containers::panel::Side::Left => {
                let width =
                    (pointer.x - state.rect.left()).clamp(spec.minimum_width, spec.maximum_width);
                state.rect.max.x = state.rect.min.x + width;
                *dock_width = width;
            }
            egui::containers::panel::Side::Right => {
                let width =
                    (state.rect.right() - pointer.x).clamp(spec.minimum_width, spec.maximum_width);
                state.rect.min.x = state.rect.max.x - width;
                *dock_width = width;
            }
        }
        ctx.request_repaint();
    }
    if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) && active {
        *active_dock = None;
    }
    let boundary_x = match spec.side {
        egui::containers::panel::Side::Left => state.rect.right(),
        egui::containers::panel::Side::Right => state.rect.left(),
    };
    let handle_width = DOCK_RESIZE_GRAB_RADIUS * 2.0;
    egui::Area::new(Id::new(spec.handle_id))
        .order(Order::Foreground)
        .fixed_pos(egui::pos2(
            boundary_x - DOCK_RESIZE_GRAB_RADIUS,
            state.rect.top(),
        ))
        .movable(false)
        .interactable(true)
        .show(ctx, |ui| {
            let (_, rect) = ui.allocate_space(Vec2::new(handle_width, state.rect.height()));
            let response = ui.interact(rect, resize_id, egui::Sense::drag());
            response.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, spec.label));
            if response.hovered() || response.dragged() {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                ui.painter().vline(
                    rect.center().x,
                    rect.y_range(),
                    Stroke::new(
                        if response.dragged() { 2.0_f32 } else { 1.0_f32 },
                        spec.color,
                    ),
                );
            }
        });
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

#[derive(Clone, Copy, Debug)]
enum TaskCommand {
    Lint,
    Update,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SearchHitInteraction {
    selected: bool,
    open_in_editor: bool,
    open_in_obsidian: bool,
}

fn search_hit_row(
    ui: &mut Ui,
    hit: &SearchHit,
    is_selected: bool,
    palette: SearchResultPalette,
) -> SearchHitInteraction {
    let id = ui.make_persistent_id(("search-result", &hit.path));
    let was_hovered = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let fill = if is_selected {
        palette.background_selected
    } else if was_hovered {
        palette.background_hovered
    } else {
        palette.background
    };
    let title_color = if is_selected {
        palette.title_selected
    } else {
        palette.title
    };
    let available_width = ui.available_width();
    let tile = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, palette.border))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width((available_width - 18.0).max(80.0));
            ui.label(RichText::new(&hit.title).strong().color(title_color));
            ui.horizontal(|ui| {
                ui.label(RichText::new(&hit.path).small().color(palette.path));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("score {}", hit.score))
                            .small()
                            .color(palette.score),
                    );
                });
            });
            ui.label(RichText::new(&hit.excerpt).small().color(palette.excerpt));
        });
    let response = tile
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Double-click to edit; right-click to open in Obsidian");
    response
        .widget_info(|| WidgetInfo::labeled(WidgetType::SelectableLabel, true, hit.title.clone()));
    ui.ctx()
        .data_mut(|data| data.insert_temp(id, response.hovered()));
    let mut interaction = SearchHitInteraction {
        selected: response.clicked(),
        open_in_editor: response.double_clicked(),
        open_in_obsidian: false,
    };
    response.context_menu(|ui| {
        if ui
            .add(fa_text_button(
                FaIcon::FolderOpen,
                "Open note in Obsidian tab",
            ))
            .clicked()
        {
            interaction.open_in_obsidian = true;
            ui.close();
        }
    });
    ui.add_space(5.0);
    interaction
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn path_modified_millis(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_or(0, unix_millis)
}

fn workspace_sort_order(
    workspaces: &[WorkspaceEntry],
    sort: WorkspaceSort,
    trees: &HashMap<PathBuf, WorkspaceFileTree>,
) -> Vec<usize> {
    let mut indices = (0..workspaces.len()).collect::<Vec<_>>();
    indices.sort_by(|&left_index, &right_index| {
        let left = &workspaces[left_index];
        let right = &workspaces[right_index];
        let primary = match sort {
            WorkspaceSort::Name => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
            WorkspaceSort::AddedNewest => right.added_at_millis.cmp(&left.added_at_millis),
            WorkspaceSort::ChangedLatest => trees
                .get(&right.root)
                .map_or(0, |tree| tree.latest_changed_millis)
                .cmp(
                    &trees
                        .get(&left.root)
                        .map_or(0, |tree| tree.latest_changed_millis),
                ),
        };
        primary
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.root.cmp(&right.root))
    });
    indices
}

fn build_workspace_file_tree(root: &Path) -> WorkspaceFileTree {
    let mut file_count = 0;
    let mut latest_changed_millis = path_modified_millis(root);
    match scan_markdown_directory(root, 0, &mut file_count, &mut latest_changed_millis) {
        Ok(nodes) => WorkspaceFileTree {
            nodes,
            error: None,
            latest_changed_millis,
        },
        Err(error) => WorkspaceFileTree {
            nodes: Vec::new(),
            error: Some(format!("Files unavailable: {error:#}")),
            latest_changed_millis,
        },
    }
}

fn scan_markdown_directory(
    directory: &Path,
    depth: usize,
    file_count: &mut usize,
    latest_changed_millis: &mut u64,
) -> Result<Vec<FileTreeNode>> {
    if depth > 32 {
        bail!(
            "directory nesting exceeds 32 levels at {}",
            directory.display()
        );
    }
    let entries = fs::read_dir(directory)
        .with_context(|| format!("could not read {}", directory.display()))?;
    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("could not inspect {}", directory.display()))?;
        let path = entry.path();
        *latest_changed_millis = (*latest_changed_millis).max(path_modified_millis(path.as_path()));
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not inspect {}", path.display()))?;
        if file_type.is_symlink() || name.starts_with('.') || name == "target" {
            continue;
        }
        if file_type.is_dir() {
            let children =
                scan_markdown_directory(&path, depth + 1, file_count, latest_changed_millis)?;
            if !children.is_empty() {
                nodes.push(FileTreeNode {
                    path,
                    name,
                    children,
                    is_markdown: false,
                });
            }
        } else if file_type.is_file() && is_markdown_path(&path) {
            *file_count += 1;
            if *file_count > 10_000 {
                bail!("workspace contains more than 10,000 Markdown files");
            }
            nodes.push(FileTreeNode {
                path,
                name,
                children: Vec::new(),
                is_markdown: true,
            });
        }
    }
    nodes.sort_by(|left, right| {
        left.is_markdown
            .cmp(&right.is_markdown)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(nodes)
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
}

fn render_file_tree_nodes(
    ui: &mut Ui,
    workspace_root: &Path,
    nodes: &[FileTreeNode],
    selected_path: Option<&Path>,
    open_file: &mut Option<(PathBuf, PathBuf)>,
) {
    for node in nodes {
        if node.is_markdown {
            let row_id = ui.make_persistent_id(("wiki_file", &node.path));
            if file_explorer_row(
                ui,
                row_id,
                ExplorerEntryKind::File,
                &node.name,
                selected_path.is_some_and(|path| same_path(path, &node.path)),
                false,
            )
            .on_hover_text(node.path.display().to_string())
            .clicked()
            {
                *open_file = Some((workspace_root.to_path_buf(), node.path.clone()));
            }
        } else {
            let default_open = selected_path.is_some_and(|path| path.starts_with(&node.path));
            let state_id = ui.make_persistent_id(("wiki_directory", &node.path));
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                state_id,
                default_open,
            );
            let row = file_explorer_row(
                ui,
                state_id.with("row"),
                ExplorerEntryKind::Directory,
                &node.name,
                false,
                state.is_open(),
            )
            .on_hover_text(node.path.display().to_string());
            if row.clicked() {
                state.toggle(ui);
            }
            state.show_body_indented(&row, ui, |ui| {
                render_file_tree_nodes(
                    ui,
                    workspace_root,
                    &node.children,
                    selected_path,
                    open_file,
                );
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExplorerEntryKind {
    Workspace,
    Directory,
    File,
}

fn file_explorer_row(
    ui: &mut Ui,
    id: Id,
    kind: ExplorerEntryKind,
    label: &str,
    selected: bool,
    expanded: bool,
) -> Response {
    let visuals = ui.visuals();
    let dark_mode = visuals.dark_mode;
    let directory_color = explorer_directory_color(dark_mode);
    let workspace_color = if selected {
        explorer_selected_workspace_color(dark_mode)
    } else {
        explorer_inactive_workspace_color(dark_mode)
    };
    let text_color = if selected {
        visuals.selection.stroke.color
    } else {
        match kind {
            ExplorerEntryKind::Workspace => workspace_color,
            ExplorerEntryKind::Directory => directory_color,
            ExplorerEntryKind::File => visuals.text_color(),
        }
    };
    let icon_color = match kind {
        ExplorerEntryKind::Workspace => workspace_color,
        ExplorerEntryKind::Directory => directory_color,
        ExplorerEntryKind::File if selected => text_color,
        ExplorerEntryKind::File => visuals.weak_text_color(),
    };
    let mut row_response = None;
    TableBuilder::new(ui)
        .id_salt(id)
        .vscroll(false)
        .sense(egui::Sense::empty())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder())
        .body(|mut body| {
            body.row(EXPLORER_ROW_HEIGHT, |mut row| {
                row.col(|ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), EXPLORER_ROW_HEIGHT),
                        egui::Sense::hover(),
                    );
                    let background = ui.painter().add(egui::Shape::Noop);
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            ui.shrink_clip_rect(rect);
                            ui.add_space(4.0);
                            match kind {
                                ExplorerEntryKind::Workspace | ExplorerEntryKind::Directory => {
                                    ui.add(
                                        explorer_folder_icon(expanded)
                                            .tintable_image(EXPLORER_ICON_SIZE + 1.0)
                                            .tint(icon_color),
                                    );
                                }
                                ExplorerEntryKind::File => {
                                    ui.add(
                                        FaIcon::File
                                            .tintable_image(EXPLORER_ICON_SIZE)
                                            .tint(icon_color),
                                    );
                                }
                            }
                            ui.add_space(3.0);
                            let mut text = RichText::new(label)
                                .size(EXPLORER_FONT_SIZE)
                                .color(text_color);
                            if kind == ExplorerEntryKind::Workspace {
                                text = text.strong();
                            }
                            ui.add(egui::Label::new(text).truncate());
                        },
                    );
                    // Register the full-row interaction after its decorative
                    // image/label children so clicks and context menus always
                    // resolve to the row rather than a child paint widget.
                    let response = ui.interact(rect, id, egui::Sense::click());
                    let fill = if selected || response.is_pointer_button_down_on() {
                        explorer_selected_color(dark_mode)
                    } else if response.hovered() {
                        explorer_hover_color(dark_mode)
                    } else {
                        Color32::TRANSPARENT
                    };
                    if fill != Color32::TRANSPARENT {
                        ui.painter()
                            .set(background, egui::Shape::rect_filled(rect, 3.0, fill));
                    }
                    row_response = Some(response);
                });
            });
        });
    let response = row_response.expect("explorer table always renders its row cell");
    response.widget_info(|| {
        WidgetInfo::labeled(
            if kind == ExplorerEntryKind::File {
                WidgetType::SelectableLabel
            } else {
                WidgetType::CollapsingHeader
            },
            true,
            label,
        )
    });
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

const fn explorer_folder_icon(expanded: bool) -> FaIcon {
    if expanded {
        FaIcon::FolderOpen
    } else {
        FaIcon::Folder
    }
}

const fn explorer_directory_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(96, 165, 250)
    } else {
        Color32::from_rgb(37, 99, 235)
    }
}

const fn explorer_selected_workspace_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(74, 222, 128)
    } else {
        Color32::from_rgb(22, 163, 74)
    }
}

const fn explorer_inactive_workspace_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(226, 232, 240)
    } else {
        Color32::BLACK
    }
}

const fn explorer_connector_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(49, 76, 112)
    } else {
        Color32::from_rgb(191, 219, 254)
    }
}

const fn explorer_hover_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(28, 50, 82)
    } else {
        Color32::from_rgb(239, 246, 255)
    }
}

const fn explorer_selected_color(dark_mode: bool) -> Color32 {
    if dark_mode {
        Color32::from_rgb(30, 64, 112)
    } else {
        Color32::from_rgb(219, 234, 254)
    }
}

fn panel_frame(theme: Theme) -> Frame {
    let palette = theme.palette();
    Frame::NONE
        .fill(palette.panel)
        .stroke(Stroke::new(1.0_f32, palette.border))
        .inner_margin(10.0)
}

fn fixed_panel_content_ui(ui: &mut Ui, id_salt: &'static str) -> Ui {
    let available = ui.available_rect_before_wrap();
    let (_, rect) = ui.allocate_space(available.size());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect);
    child
}

fn markdown_mode_controls(
    ui: &mut Ui,
    editor: &mut MarkdownEditor,
    allow_split: bool,
    locale_manager: &LocaleManager,
) -> bool {
    let mut changed = false;
    let modes = [
        (
            EditorMode::Source,
            FaIcon::Code,
            locale_manager.text("editor.source"),
        ),
        (
            EditorMode::Preview,
            FaIcon::Preview,
            locale_manager.text("editor.preview"),
        ),
        (
            EditorMode::Split,
            FaIcon::Split,
            locale_manager.text("editor.split"),
        ),
    ];
    for (mode, icon, label) in modes {
        if mode == EditorMode::Split && !allow_split {
            continue;
        }
        if ui
            .add(fa_text_button(icon, label).selected(editor.mode() == mode))
            .clicked()
            && editor.mode() != mode
        {
            editor.set_mode(mode);
            changed = true;
        }
    }
    changed
}

fn panel_header(ui: &mut Ui, title: &str, controls: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .strong()
                .color(ui.visuals().weak_text_color()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), controls);
    });
}

fn path_editor(ui: &mut Ui, label: &str, value: &mut PathBuf) -> bool {
    ui.label(
        RichText::new(label)
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    let mut text = value.display().to_string();
    let response = ui.add(TextEdit::singleline(&mut text).desired_width(f32::INFINITY));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, label));
    let changed = response.changed();
    if changed {
        *value = PathBuf::from(text);
    }
    changed
}

fn llm_diagnostic_result(
    ui: &mut Ui,
    title: &str,
    success_message: &str,
    result: Option<&std::result::Result<String, String>>,
    locale_manager: &LocaleManager,
) {
    let visual_state = match result {
        None => DiagnosticVisualState::Pending,
        Some(Ok(_)) => DiagnosticVisualState::Success,
        Some(Err(_)) => DiagnosticVisualState::Failed,
    };
    let label = if matches!(result, Some(Err(_))) {
        format!("{title} {}", locale_manager.text("llm.test_failed"))
    } else {
        success_message.to_owned()
    };
    llm_diagnostic_status_row(ui, &label, visual_state, None);
    let box_state = match result {
        None => "empty",
        Some(Ok(_)) => "complete",
        Some(Err(_)) => "failed",
    };
    let reply_box = Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.set_min_height(44.0);
        match result {
            Some(Ok(reply)) => {
                ui.label(
                    RichText::new(format!(
                        "{title} {}: {reply}",
                        locale_manager.text("llm.reply")
                    ))
                    .small(),
                );
            }
            Some(Err(error)) => {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            None => {
                ui.label(
                    RichText::new(locale_manager.text("llm.waiting"))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        }
    });
    let accessibility_label = format!("{title} reply box ({box_state})");
    reply_box
        .response
        .widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, accessibility_label.clone()));
}

fn llm_diagnostic_status_row(
    ui: &mut Ui,
    label: &str,
    state: DiagnosticVisualState,
    color_override: Option<Color32>,
) {
    let color = color_override.unwrap_or_else(|| match state {
        DiagnosticVisualState::Pending => ui.visuals().weak_text_color(),
        DiagnosticVisualState::Success => Color32::from_rgb(34, 160, 99),
        DiagnosticVisualState::Failed => Color32::from_rgb(218, 73, 73),
    });
    ui.horizontal(|ui| {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        let square = rect.shrink(1.5);
        ui.painter().rect_stroke(
            square,
            1.5,
            Stroke::new(1.4_f32, color),
            egui::StrokeKind::Inside,
        );
        match state {
            DiagnosticVisualState::Pending => {}
            DiagnosticVisualState::Success => {
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::new(2.0, 5.5),
                        square.left_top() + Vec2::new(4.5, 8.0),
                    ],
                    Stroke::new(1.6_f32, color),
                );
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::new(4.5, 8.0),
                        square.left_top() + Vec2::new(9.5, 2.5),
                    ],
                    Stroke::new(1.6_f32, color),
                );
            }
            DiagnosticVisualState::Failed => {
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::splat(3.0),
                        square.right_bottom() - Vec2::splat(3.0),
                    ],
                    Stroke::new(1.5_f32, color),
                );
                ui.painter().line_segment(
                    [
                        square.right_top() + Vec2::new(-3.0, 3.0),
                        square.left_bottom() + Vec2::new(3.0, -3.0),
                    ],
                    Stroke::new(1.5_f32, color),
                );
            }
        }
        let status_label = format!(
            "{label} {}",
            match state {
                DiagnosticVisualState::Pending => "pending",
                DiagnosticVisualState::Success => "complete",
                DiagnosticVisualState::Failed => "failed",
            }
        );
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, status_label.clone()));
        ui.label(RichText::new(label).strong().color(color));
    });
}

fn localized_ingest_stage(stage: WikiIngestStage, locale_manager: &LocaleManager) -> String {
    locale_manager.text(match stage {
        WikiIngestStage::Initialize => "ingest.stage_initialize",
        WikiIngestStage::Markdown => "ingest.stage_markdown",
        WikiIngestStage::Chunking => "ingest.stage_chunking",
        WikiIngestStage::LlmCheck => "ingest.stage_llm_check",
        WikiIngestStage::Components => "ingest.stage_components",
        WikiIngestStage::Concepts => "ingest.stage_concepts",
        WikiIngestStage::Entities => "ingest.stage_entities",
        WikiIngestStage::Factors => "ingest.stage_factors",
        WikiIngestStage::Methodologies => "ingest.stage_methodologies",
        WikiIngestStage::Queries => "ingest.stage_queries",
    })
}

fn localized_ingest_llm_check(check: WikiIngestLlmCheck, locale_manager: &LocaleManager) -> String {
    locale_manager.text(match check {
        WikiIngestLlmCheck::Direct => "ingest.check_direct",
        WikiIngestLlmCheck::Proxy => "ingest.check_proxy",
        WikiIngestLlmCheck::Codex => "ingest.check_codex",
    })
}

fn localized_ingest_state(state: IngestStepState, locale_manager: &LocaleManager) -> String {
    locale_manager.text(match state {
        IngestStepState::Pending => "ingest.pending",
        IngestStepState::Running => "ingest.running",
        IngestStepState::Succeeded => "ingest.done",
        IngestStepState::Failed => "ingest.failed",
    })
}

fn ingest_step_row(
    ui: &mut Ui,
    checkpoint: WikiIngestStage,
    status: IngestStepState,
    progress: IngestUnitProgress,
    locale_manager: &LocaleManager,
) {
    let color = match status {
        IngestStepState::Pending => ui.visuals().weak_text_color(),
        IngestStepState::Running => ACCENT,
        IngestStepState::Succeeded => Color32::from_rgb(34, 160, 99),
        IngestStepState::Failed => Color32::from_rgb(218, 73, 73),
    };
    let row = ui.horizontal(|ui| {
        if status == IngestStepState::Running {
            ui.add(egui::Spinner::new().size(14.0).color(color));
        } else {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            let square = rect.shrink(1.5);
            ui.painter().rect_stroke(
                square,
                1.5,
                Stroke::new(1.4_f32, color),
                egui::StrokeKind::Inside,
            );
            if status == IngestStepState::Succeeded {
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::new(2.0, 5.5),
                        square.left_top() + Vec2::new(4.5, 8.0),
                    ],
                    Stroke::new(1.6_f32, color),
                );
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::new(4.5, 8.0),
                        square.left_top() + Vec2::new(9.5, 2.5),
                    ],
                    Stroke::new(1.6_f32, color),
                );
            } else if status == IngestStepState::Failed {
                ui.painter().line_segment(
                    [
                        square.left_top() + Vec2::splat(3.0),
                        square.right_bottom() - Vec2::splat(3.0),
                    ],
                    Stroke::new(1.5_f32, color),
                );
                ui.painter().line_segment(
                    [
                        square.right_top() + Vec2::new(-3.0, 3.0),
                        square.left_bottom() + Vec2::new(3.0, -3.0),
                    ],
                    Stroke::new(1.5_f32, color),
                );
            }
        }
        ui.label(RichText::new(localized_ingest_stage(checkpoint, locale_manager)).color(color));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(localized_ingest_state(status, locale_manager))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
    });
    let accessibility_label = format!(
        "Ingest step: {} — {}",
        localized_ingest_stage(checkpoint, locale_manager),
        localized_ingest_state(status, locale_manager)
    );
    row.response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::ProgressIndicator,
            true,
            accessibility_label.clone(),
        )
    });
    if let Some((fraction, text)) =
        ingest_unit_progress_display(checkpoint, progress, locale_manager)
    {
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            let bar = ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width())
                    .animate(status == IngestStepState::Running)
                    .text(text.clone()),
            );
            bar.widget_info(|| {
                WidgetInfo::labeled(
                    WidgetType::ProgressIndicator,
                    true,
                    format!(
                        "{} progress: {text}",
                        localized_ingest_stage(checkpoint, locale_manager)
                    ),
                )
            });
        });
    }
}

fn ingest_llm_check_row(
    ui: &mut Ui,
    check: WikiIngestLlmCheck,
    status: IngestStepState,
    locale_manager: &LocaleManager,
) {
    let color = match status {
        IngestStepState::Pending => ui.visuals().weak_text_color(),
        IngestStepState::Running => ACCENT,
        IngestStepState::Succeeded => Color32::from_rgb(34, 160, 99),
        IngestStepState::Failed => Color32::from_rgb(218, 73, 73),
    };
    let row = ui.horizontal(|ui| {
        ui.add_space(22.0);
        if status == IngestStepState::Running {
            ui.add(egui::Spinner::new().size(11.0).color(color));
        } else {
            ui.label(
                RichText::new(match status {
                    IngestStepState::Pending => "○",
                    IngestStepState::Succeeded => "✓",
                    IngestStepState::Failed => "×",
                    IngestStepState::Running => "",
                })
                .color(color),
            );
        }
        ui.label(
            RichText::new(localized_ingest_llm_check(check, locale_manager))
                .small()
                .color(color),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(localized_ingest_state(status, locale_manager))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
    });
    let accessibility_label = format!(
        "Ingest LLM check: {} — {}",
        localized_ingest_llm_check(check, locale_manager),
        localized_ingest_state(status, locale_manager)
    );
    row.response.widget_info(|| {
        WidgetInfo::labeled(
            WidgetType::ProgressIndicator,
            true,
            accessibility_label.clone(),
        )
    });
}

fn ingest_unit_progress_display(
    stage: WikiIngestStage,
    progress: IngestUnitProgress,
    locale_manager: &LocaleManager,
) -> Option<(f32, String)> {
    let (completed, total) = (progress.completed?, progress.total?);
    let basis_points = completed
        .saturating_mul(10_000)
        .checked_div(total)
        .unwrap_or(10_000);
    let basis_points =
        u16::try_from(basis_points.min(10_000)).expect("progress basis points are bounded to u16");
    let fraction = f32::from(basis_points) / 10_000.0;
    let max = progress
        .max_chunk_characters
        .map(|value| {
            format!(
                " • {} {value} {}",
                locale_manager.text("ingest.max_unit"),
                locale_manager.text("ingest.characters_unit")
            )
        })
        .unwrap_or_default();
    let text = match stage {
        WikiIngestStage::Markdown => format!(
            "{completed}/{total} {}",
            locale_manager.text("ingest.files_unit")
        ),
        WikiIngestStage::Chunking => format!(
            "{completed}/{total} {} • {} {}{max}",
            locale_manager.text("ingest.files_unit"),
            progress.chunks.unwrap_or(0),
            locale_manager.text("ingest.chunks_unit")
        ),
        WikiIngestStage::Components => format!(
            "{completed}/{total} {}{max}",
            locale_manager.text("ingest.chunks_unit")
        ),
        _ => return None,
    };
    Some((fraction, text))
}

fn ingest_picker_menu(ui: &mut Ui, locale_manager: &LocaleManager) -> Option<IngestPickerMode> {
    let mut picker_mode = None;
    ui.menu_image_text_button(
        FaIcon::FolderOpen.image(13.0),
        locale_manager.text("ingest.select_inputs"),
        |ui| {
            if ui
                .add(fa_text_button(
                    FaIcon::File,
                    locale_manager.text("ingest.files"),
                ))
                .clicked()
            {
                picker_mode = Some(IngestPickerMode::Files);
                ui.close();
            }
            if ui
                .add(fa_text_button(
                    FaIcon::FolderOpen,
                    locale_manager.text("ingest.directories"),
                ))
                .clicked()
            {
                picker_mode = Some(IngestPickerMode::Directories);
                ui.close();
            }
        },
    );
    picker_mode
}

fn durable_ingest_session_controls(
    ui: &mut Ui,
    status: &WikiIngestStatus,
    ingest_running: bool,
    palette: Palette,
    locale_manager: &LocaleManager,
) -> Option<WikiIngestMode> {
    let mut requested_mode = None;
    ui.add_space(6.0);
    Frame::NONE
        .fill(palette.canvas)
        .stroke(Stroke::new(1.0_f32, palette.border))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(locale_manager.text("ingest.current_session")).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(localized_ingest_run_state(
                            status.run_state(),
                            locale_manager,
                        ))
                        .color(ingest_run_state_color(status.run_state())),
                    );
                });
            });
            ui.label(
                RichText::new(format!(
                    "{} {} • {} {}",
                    status.source_paths().len(),
                    locale_manager.text("ingest.sources_count"),
                    locale_manager.text("ingest.updated"),
                    status.updated_at()
                ))
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            if let Some(error) = status.last_error() {
                ui.label(
                    RichText::new(first_error_line(error))
                        .small()
                        .color(Color32::from_rgb(211, 74, 74)),
                );
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        status.can_resume() && !ingest_running,
                        fa_text_button(FaIcon::Ingest, locale_manager.text("ingest.continue")),
                    )
                    .on_hover_text(locale_manager.text("ingest.continue_hint"))
                    .clicked()
                {
                    requested_mode = Some(WikiIngestMode::Resume);
                }
                if ui
                    .add_enabled(
                        !ingest_running,
                        fa_text_button(FaIcon::Refresh, locale_manager.text("ingest.redo")),
                    )
                    .on_hover_text(locale_manager.text("ingest.redo_hint"))
                    .clicked()
                {
                    requested_mode = Some(WikiIngestMode::Redo);
                }
            });
        });
    requested_mode
}

fn durable_ingest_progress(
    status: &WikiIngestStatus,
) -> (
    [IngestStepState; INGEST_STAGE_COUNT],
    [IngestUnitProgress; INGEST_STAGE_COUNT],
    [IngestStepState; INGEST_LLM_CHECK_COUNT],
) {
    let mut steps = [IngestStepState::Pending; INGEST_STAGE_COUNT];
    let mut units = [IngestUnitProgress::default(); INGEST_STAGE_COUNT];
    let mut llm_checks = [IngestStepState::Pending; INGEST_LLM_CHECK_COUNT];
    for checkpoint in status.checkpoints() {
        steps[checkpoint.stage.index()] = match checkpoint.state {
            WikiIngestCheckpointState::Pending => IngestStepState::Pending,
            WikiIngestCheckpointState::Running => IngestStepState::Running,
            WikiIngestCheckpointState::Failed => IngestStepState::Failed,
            WikiIngestCheckpointState::Succeeded => IngestStepState::Succeeded,
        };
        units[checkpoint.stage.index()] = IngestUnitProgress {
            completed: checkpoint.completed,
            total: checkpoint.total,
            chunks: checkpoint.chunks,
            max_chunk_characters: checkpoint.max_chunk_characters,
        };
        for llm_check in &checkpoint.llm_checks {
            llm_checks[llm_check.check.index()] = match llm_check.state {
                WikiIngestCheckpointState::Pending => IngestStepState::Pending,
                WikiIngestCheckpointState::Running => IngestStepState::Running,
                WikiIngestCheckpointState::Failed => IngestStepState::Failed,
                WikiIngestCheckpointState::Succeeded => IngestStepState::Succeeded,
            };
        }
    }
    (steps, units, llm_checks)
}

fn localized_ingest_run_state(state: WikiIngestRunState, locale_manager: &LocaleManager) -> String {
    locale_manager.text(match state {
        WikiIngestRunState::Running => "ingest.run_interrupted",
        WikiIngestRunState::Failed => "ingest.run_resumable",
        WikiIngestRunState::Succeeded => "ingest.run_completed",
    })
}

const fn ingest_run_state_color(state: WikiIngestRunState) -> Color32 {
    match state {
        WikiIngestRunState::Running => Color32::from_rgb(250, 193, 76),
        WikiIngestRunState::Failed => Color32::from_rgb(241, 103, 103),
        WikiIngestRunState::Succeeded => Color32::from_rgb(83, 195, 132),
    }
}

fn first_error_line(error: &str) -> &str {
    error
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(error)
}

fn llm_chunk_size_control(ui: &mut Ui, max_characters: &mut usize) {
    ui.horizontal(|ui| {
        ui.add(
            egui::DragValue::new(max_characters)
                .range(256..=2_000_000)
                .speed(100.0)
                .suffix(" characters"),
        );
        ui.label(
            RichText::new("per semantic LLM call")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
}

fn llm_route_summary(source: &str) -> Option<String> {
    let config = Config::parse(source).ok()?;
    let model_name = config.codex_model();
    let deployment = config
        .model_list
        .iter()
        .find(|deployment| deployment.model_name == model_name)?;
    Some(format!(
        "Configured route: {model_name} -> {}",
        deployment.litellm_params.model
    ))
}

fn llm_proxy_endpoint_summary(source: &str) -> Option<String> {
    let config = Config::parse(source).ok()?;
    Some(format!(
        "Responses proxy endpoint: http://{}/v1/responses",
        config.server.bind
    ))
}

fn llm_connection_progress(ui: &mut Ui, message: &str, accessibility_label: &str) {
    let spinner = ui.add(egui::Spinner::new().size(18.0).color(ACCENT));
    spinner.widget_info(|| {
        WidgetInfo::labeled(WidgetType::ProgressIndicator, true, accessibility_label)
    });
    ui.label(message);
}

fn configure_style(ctx: &Context, theme: Theme) {
    let palette = theme.palette();
    let mut visuals = match theme {
        Theme::Light => egui::Visuals::light(),
        Theme::Dark => egui::Visuals::dark(),
    };
    visuals.panel_fill = palette.panel;
    visuals.window_fill = palette.panel;
    visuals.extreme_bg_color = palette.canvas;
    visuals.faint_bg_color = palette.chrome;
    visuals.override_text_color = Some(palette.text);
    visuals.hyperlink_color = ACCENT;
    visuals.window_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.noninteractive.bg_fill = palette.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.muted);
    visuals.widgets.inactive.bg_fill = palette.control;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, palette.text);
    visuals.widgets.hovered.bg_fill = palette.control_hovered;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, palette.text);
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    visuals.widgets.open.bg_fill = palette.control_hovered;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, palette.text);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(9.0, 5.0);
    style.interaction.resize_grab_radius_side = DOCK_RESIZE_GRAB_RADIUS;
    ctx.set_style(style);
}

fn configure_fonts(ctx: &Context) {
    let mut definitions = FontDefinitions::default();
    #[cfg(target_os = "windows")]
    let candidates = [
        ("bibiiwiki-zh", Path::new(r"C:\Windows\Fonts\msyh.ttc")),
        ("bibiiwiki-ja", Path::new(r"C:\Windows\Fonts\YuGothR.ttc")),
        (
            "bibiiwiki-ja-alt",
            Path::new(r"C:\Windows\Fonts\meiryo.ttc"),
        ),
        ("bibiiwiki-ko", Path::new(r"C:\Windows\Fonts\malgun.ttf")),
    ];
    #[cfg(target_os = "macos")]
    let candidates = [
        (
            "bibiiwiki-cjk",
            Path::new("/System/Library/Fonts/PingFang.ttc"),
        ),
        (
            "bibiiwiki-ko",
            Path::new("/System/Library/Fonts/AppleSDGothicNeo.ttc"),
        ),
    ];
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let candidates = [
        (
            "bibiiwiki-cjk",
            Path::new("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
        ),
        (
            "bibiiwiki-cjk-alt",
            Path::new("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc"),
        ),
    ];

    let mut loaded = false;
    for (name, path) in candidates {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        loaded = true;
        definitions
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            definitions
                .families
                .entry(family)
                .or_default()
                .push(name.to_owned());
        }
    }
    if loaded {
        ctx.set_fonts(definitions);
    }
}

fn embedded_texture(ctx: &Context, name: &str, png: &[u8]) -> Result<egui::TextureHandle> {
    let icon = eframe::icon_data::from_png_bytes(png)
        .with_context(|| format!("failed to decode embedded image {name}"))?;
    let width = usize::try_from(icon.width).context("embedded image width is unsupported")?;
    let height = usize::try_from(icon.height).context("embedded image height is unsupported")?;
    let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &icon.rgba);
    Ok(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
}

fn validate_config_text(source: &str) -> Result<String> {
    let config = Config::parse(source)?;
    Ok(format!(
        "✓ {} deployment(s) · Codex model {}",
        config.model_list.len(),
        config.codex_model()
    ))
}

fn run_llm_diagnostic(config_text: &str, workspace: &Path, sender: &Sender<LlmDiagnosticMessage>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sender.send(LlmDiagnosticMessage::ModelFailed(format!(
                "Could not start the diagnostic runtime: {error}"
            )));
            return;
        }
    };
    runtime.block_on(run_llm_diagnostic_async(config_text, workspace, sender));
}

async fn run_llm_diagnostic_async(
    config_text: &str,
    workspace: &Path,
    sender: &Sender<LlmDiagnosticMessage>,
) {
    let model_test = async {
        let mut config = Config::parse(config_text).context("configuration is not valid")?;
        config.server.request_timeout_seconds = config.server.request_timeout_seconds.clamp(1, 120);
        let reply = probe_direct_llm(&config).await?;
        Ok::<_, anyhow::Error>((config, panel_reply(&reply)))
    }
    .await;

    let (config, reply) = match model_test {
        Ok(result) => result,
        Err(error) => {
            let _ = sender.send(LlmDiagnosticMessage::ModelFailed(format!("{error:#}")));
            return;
        }
    };
    if sender
        .send(LlmDiagnosticMessage::ModelPassed(reply))
        .is_err()
        || sender.send(LlmDiagnosticMessage::ProxyStarted).is_err()
    {
        return;
    }

    let proxy_reply = match probe_responses_proxy(&config).await {
        Ok(reply) => reply,
        Err(error) => {
            let _ = sender.send(LlmDiagnosticMessage::ProxyFailed(format!("{error:#}")));
            return;
        }
    };
    if sender
        .send(LlmDiagnosticMessage::ProxyPassed(panel_reply(&proxy_reply)))
        .is_err()
        || sender.send(LlmDiagnosticMessage::CodexStarted).is_err()
    {
        return;
    }

    match probe_codex_agent(config, workspace).await {
        Ok(reply) => {
            let _ = sender.send(LlmDiagnosticMessage::CodexPassed(panel_reply(&reply)));
        }
        Err(error) => {
            let _ = sender.send(LlmDiagnosticMessage::CodexFailed(format!("{error:#}")));
        }
    }
}

fn panel_reply(text: &str) -> String {
    const MAX_CHARS: usize = 800;
    let mut reply: String = text.trim().chars().take(MAX_CHARS).collect();
    if text.trim().chars().count() > MAX_CHARS {
        reply.push('…');
    }
    reply
}

fn tool_config_path(config_path: &Path) -> PathBuf {
    config_path.to_path_buf()
}

fn load_tool_config_editor(
    llm_config_path: &Path,
    persisted: &mut PersistedState,
    _startup_issues: &mut Vec<String>,
) -> (PathBuf, String, String) {
    let path = tool_config_path(llm_config_path);
    let source = render_tool_config(&ToolConfig::from_persisted(persisted)).unwrap_or_default();
    (
        path,
        source,
        "✓ Tool values loaded from unified YAML".to_owned(),
    )
}

#[derive(Deserialize)]
struct UnifiedConfigEnvelope {
    #[serde(default)]
    local: Option<PersistedState>,
}

fn parse_local_settings(source: &str) -> Result<Option<PersistedState>> {
    let envelope: UnifiedConfigEnvelope =
        serde_yaml::from_str(source).context("invalid unified BIBIIWIKI YAML")?;
    Ok(envelope.local)
}

fn load_local_settings_file(path: &Path) -> Result<Option<PersistedState>> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read unified config {}", path.display()))?;
    parse_local_settings(&source)
}

fn merge_local_settings(source: &str, state: &PersistedState) -> Result<String> {
    const LOCAL_BEGIN: &str = "# --- BIBIIWIKI local settings (managed) ---";
    const LOCAL_END: &str = "# --- end BIBIIWIKI local settings ---";

    let _ = YamlFile::from_str(source).context("invalid unified YAML syntax")?;
    let local_source =
        serde_yaml::to_string(state).context("failed to serialize local BIBIIWIKI settings")?;
    let base = remove_local_settings_section(source, LOCAL_BEGIN, LOCAL_END)?;
    let indented = local_source
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let unified = format!(
        "{}\n\n{LOCAL_BEGIN}\nlocal:\n{indented}\n{LOCAL_END}\n",
        base.trim_end()
    );
    let _ = YamlFile::from_str(&unified).context("failed to construct unified YAML")?;
    let _ = Config::prepare_edit(&unified)?;
    Ok(unified)
}

fn remove_local_settings_section(source: &str, begin: &str, end: &str) -> Result<String> {
    if let Some(start) = source.find(begin) {
        let tail = &source[start..];
        let end_offset = tail
            .find(end)
            .context("managed local settings section is missing its end marker")?;
        let after = start + end_offset + end.len();
        return Ok(format!("{}{}", &source[..start], &source[after..]));
    }

    let mut offset = 0;
    let mut local_start = None;
    let mut local_end = source.len();
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if local_start.is_none() && trimmed == "local:" {
            local_start = Some(offset);
        } else if local_start.is_some()
            && !trimmed.is_empty()
            && !trimmed.starts_with(char::is_whitespace)
            && !trimmed.starts_with('#')
        {
            local_end = offset;
            break;
        }
        offset += line.len();
    }
    Ok(local_start.map_or_else(
        || source.to_owned(),
        |start| format!("{}{}", &source[..start], &source[local_end..]),
    ))
}

fn persist_local_settings_file(path: &Path, state: &PersistedState) -> Result<()> {
    Config::ensure_exists(path)?;
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read unified config {}", path.display()))?;
    let unified = merge_local_settings(&source, state)?;
    fs::write(path, unified)
        .with_context(|| format!("failed to write unified config {}", path.display()))
}

fn parse_tool_config(source: &str) -> Result<ToolConfig> {
    let config: ToolConfig =
        toml::from_str(source).context("invalid BIBIIWIKI tool TOML configuration")?;
    if config.ingest.source_dir.as_os_str().is_empty() {
        bail!("ingest.source_dir must not be empty");
    }
    if config.codex.workspace.as_os_str().is_empty() {
        bail!("codex.workspace must not be empty");
    }
    if !(1..=100).contains(&config.search.limit) {
        bail!("search.limit must be between 1 and 100");
    }
    Ok(config)
}

fn render_tool_config(config: &ToolConfig) -> Result<String> {
    toml::to_string_pretty(config).context("failed to render BIBIIWIKI tool TOML configuration")
}

fn prompt_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("templates")
        .join(name)
}

fn color_theme_path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("color_theme.yaml")
}

fn resolve_startup_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    let current_candidate = std::env::current_dir().ok().map(|root| root.join(&path));
    if let Some(candidate) = current_candidate
        .as_ref()
        .filter(|candidate| candidate.exists())
    {
        return candidate.clone();
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        for ancestor in parent.ancestors() {
            let candidate = ancestor.join(&path);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    current_candidate.unwrap_or(path)
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("path cannot be empty");
    }
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

fn pick_workspace_directory(initial_directory: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Select BIBIIWIKI wiki root")
        .set_directory(initial_directory)
        .pick_folder()
}

fn ingest_picker_initial_directory(configured_source: &Path) -> PathBuf {
    let mut candidate = if configured_source.is_absolute() {
        configured_source.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(configured_source)
    };
    while !candidate.is_dir() {
        if !candidate.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
    candidate
}

fn pick_ingest_sources(mode: IngestPickerMode, initial_directory: &Path) -> Option<Vec<PathBuf>> {
    let dialog = rfd::FileDialog::new()
        .set_title(match mode {
            IngestPickerMode::Files => "Select source files to ingest",
            IngestPickerMode::Directories => "Select source directories to ingest",
        })
        .set_directory(initial_directory);
    match mode {
        IngestPickerMode::Files => dialog
            .add_filter(
                "Documents",
                &[
                    "doc", "docx", "docm", "odt", "rtf", "epub", "pdf", "ppt", "pps", "pot",
                    "pptx", "pptm", "ppsx", "ppsm", "odp", "xls", "xlsx", "xlsm", "xlsb", "ods",
                    "csv", "md", "txt",
                ],
            )
            .pick_files(),
        IngestPickerMode::Directories => dialog.pick_folders(),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy().replace('\\', "/");
    let right = right.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ObsidianRegistry {
    #[serde(default)]
    vaults: HashMap<String, ObsidianVault>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ObsidianVault {
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObsidianRegistration {
    vault_id: String,
    uri: String,
}

fn obsidian_open_uri(vault: &str, file: Option<&Path>, new_tab: bool) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("vault", vault);
    if let Some(file) = file {
        let normalized = file.to_string_lossy().replace('\\', "/");
        query.append_pair("file", normalized.trim_start_matches('/'));
    }
    if new_tab {
        query.append_pair("paneType", "tab");
    }
    format!("obsidian://open?{}", query.finish())
}

fn obsidian_registry_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("obsidian").join("obsidian.json"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(test)]
fn registered_obsidian_vault(root: &Path) -> Result<Option<String>> {
    let Some(registry_path) = obsidian_registry_path() else {
        return Ok(None);
    };
    let source = match fs::read_to_string(&registry_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read {}", registry_path.display()));
        }
    };
    let registry: ObsidianRegistry = serde_json::from_str(&source)
        .with_context(|| format!("could not parse {}", registry_path.display()))?;
    Ok(find_registered_obsidian_vault(registry, root))
}

#[cfg(test)]
fn find_registered_obsidian_vault(registry: ObsidianRegistry, root: &Path) -> Option<String> {
    registry
        .vaults
        .into_iter()
        .find_map(|(id, vault)| same_path(&vault.path, root).then_some(id))
}

fn ensure_registered_obsidian_vault(root: &Path) -> Result<String> {
    if !root.is_dir() {
        bail!("workspace folder does not exist: {}", root.display());
    }
    let registry_path = obsidian_registry_path()
        .context("automatic Obsidian vault registration is currently supported on Windows only")?;
    register_obsidian_vault_at(&registry_path, root)
}

fn register_workspace_with_obsidian(root: &Path) -> Result<ObsidianRegistration> {
    let vault_id = ensure_registered_obsidian_vault(root)?;
    let uri = obsidian_open_uri(&vault_id, None, false);
    Ok(ObsidianRegistration { vault_id, uri })
}

fn register_obsidian_vault_at(registry_path: &Path, root: &Path) -> Result<String> {
    if !root.is_dir() {
        bail!("workspace folder does not exist: {}", root.display());
    }
    let source = match fs::read(registry_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{\"vaults\":{}}".to_vec(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read {}", registry_path.display()));
        }
    };
    let mut registry: Value = serde_json::from_slice(&source)
        .with_context(|| format!("could not parse {}", registry_path.display()))?;
    let registry_object = registry
        .as_object_mut()
        .context("Obsidian registry root must be a JSON object")?;
    let vaults = registry_object
        .entry("vaults")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Obsidian registry 'vaults' value must be a JSON object")?;

    if let Some((id, _)) = vaults.iter().find(|(_, vault)| {
        vault
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| same_path(Path::new(path), root))
    }) {
        let backend_dir = registry_path
            .parent()
            .context("Obsidian registry path has no parent directory")?;
        ensure_obsidian_vault_artifacts(backend_dir, root, id)?;
        return Ok(id.clone());
    }

    let id = new_obsidian_vault_id(root, vaults);
    let backend_dir = registry_path
        .parent()
        .context("Obsidian registry path has no parent directory")?;
    ensure_obsidian_vault_artifacts(backend_dir, root, &id)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    vaults.insert(
        id.clone(),
        json!({
            "path": root.to_string_lossy(),
            "ts": timestamp,
        }),
    );
    write_obsidian_registry_atomically(registry_path, &registry)?;
    Ok(id)
}

fn new_obsidian_vault_id(root: &Path, vaults: &serde_json::Map<String, Value>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    for nonce in 0_u32.. {
        let mut hasher = Sha256::new();
        hasher.update(root.to_string_lossy().as_bytes());
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        hasher.update(nonce.to_le_bytes());
        let digest = hasher.finalize();
        let mut id = String::with_capacity(16);
        for byte in &digest[..8] {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        if !vaults.contains_key(&id) {
            return id;
        }
    }
    unreachable!("u32 nonce space cannot be exhausted by an Obsidian registry")
}

fn ensure_obsidian_vault_artifacts(backend_dir: &Path, root: &Path, id: &str) -> Result<()> {
    fs::create_dir_all(root.join(".obsidian")).with_context(|| {
        format!(
            "could not initialize {} as an Obsidian vault",
            root.display()
        )
    })?;

    let index_path = backend_dir.join(format!("{id}.json"));
    if index_path.is_file() {
        return Ok(());
    }
    let index = serde_json::to_vec(&json!({
        "x": 100,
        "y": 100,
        "width": 1024,
        "height": 768,
        "isMaximized": false,
        "devTools": false,
        "zoom": 0,
    }))
    .context("could not serialize the Obsidian vault index")?;
    let temp_path = backend_dir.join(format!(
        ".{id}.json.bibiiwiki-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let write_result = (|| -> Result<()> {
        let mut temp = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("could not create {}", temp_path.display()))?;
        temp.write_all(&index)
            .with_context(|| format!("could not write {}", temp_path.display()))?;
        temp.sync_all()
            .with_context(|| format!("could not sync {}", temp_path.display()))?;
        match fs::rename(&temp_path, &index_path) {
            Ok(()) => Ok(()),
            Err(_) if index_path.is_file() => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("could not create {}", index_path.display()))
            }
        }
    })();
    if temp_path.exists() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn write_obsidian_registry_atomically(registry_path: &Path, registry: &Value) -> Result<()> {
    let parent = registry_path
        .parent()
        .context("Obsidian registry path has no parent directory")?;
    if !parent.is_dir() {
        bail!(
            "Obsidian backend directory does not exist: {}. Start Obsidian once, then retry.",
            parent.display()
        );
    }
    let data = serde_json::to_vec(registry).context("could not serialize Obsidian registry")?;
    let temp_path = parent.join(format!(
        ".obsidian.json.bibiiwiki-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let write_result = (|| -> Result<()> {
        let mut temp = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("could not create {}", temp_path.display()))?;
        temp.write_all(&data)
            .with_context(|| format!("could not write {}", temp_path.display()))?;
        temp.sync_all()
            .with_context(|| format!("could not sync {}", temp_path.display()))?;
        atomic_replace_file(&temp_path, registry_path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn atomic_replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_display = destination.display().to_string();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are valid, NUL-terminated UTF-16 buffers that remain
    // alive for the duration of this synchronous Win32 call.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("could not atomically replace {destination_display}"));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn atomic_replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("could not atomically replace {}", destination.display()))
}

fn open_in_obsidian(root: &Path) -> Result<()> {
    let registration = register_workspace_with_obsidian(root)?;
    open::that_detached(&registration.uri)
        .with_context(|| format!("failed to open {}", registration.uri))
}

fn open_note_in_obsidian(root: &Path, file: &Path) -> Result<()> {
    let vault = ensure_registered_obsidian_vault(root)?;
    let uri = obsidian_open_uri(&vault, Some(file), true);
    open::that_detached(&uri).with_context(|| format!("failed to open {uri}"))
}

fn push_path_arg(args: &mut Vec<OsString>, name: &str, value: &Path) {
    args.push(name.into());
    args.push(value.as_os_str().to_os_string());
}

fn run_cli_process(
    args: &[OsString],
    working_directory: &Path,
    on_stdout: &CliLineObserver,
) -> Result<(bool, String, String)> {
    let executable = std::env::current_exe().context("failed to locate BIBIIWIKI executable")?;
    let mut command = Command::new(executable);
    command.args(args);
    if working_directory.is_dir() {
        command.current_dir(working_directory);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("failed to start BIBIIWIKI task")?;
    let stdout = child
        .stdout
        .take()
        .context("BIBIIWIKI task did not expose stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("BIBIIWIKI task did not expose stderr")?;

    let (line_tx, line_rx) = mpsc::channel();
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    thread::scope(|scope| {
        let stdout_tx = line_tx.clone();
        scope.spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        let _ = stdout_tx.send(CliProcessLine::Stdout(line));
                    }
                    Err(error) => {
                        let _ = stdout_tx.send(CliProcessLine::Stderr(format!(
                            "failed to read BIBIIWIKI task stdout: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        let stderr_tx = line_tx.clone();
        scope.spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        let _ = stderr_tx.send(CliProcessLine::Stderr(line));
                    }
                    Err(error) => {
                        let _ = stderr_tx.send(CliProcessLine::Stderr(format!(
                            "failed to read BIBIIWIKI task stderr: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        drop(line_tx);

        for line in line_rx {
            match line {
                CliProcessLine::Stdout(line) => {
                    on_stdout(&line);
                    if WikiIngestProgress::parse_marker(&line).is_none() {
                        stdout_text.push_str(&line);
                        stdout_text.push('\n');
                    }
                }
                CliProcessLine::Stderr(line) => {
                    stderr_text.push_str(&line);
                    stderr_text.push('\n');
                }
            }
        }
    });
    let status = child.wait().context("failed to wait for BIBIIWIKI task")?;
    Ok((status.success(), stdout_text, stderr_text))
}

fn combined_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim(), stderr.trim()) {
        ("", "") => "Command completed without output.".to_owned(),
        (stdout, "") => stdout.to_owned(),
        ("", stderr) => stderr.to_owned(),
        (stdout, stderr) => format!("{stdout}\n\n--- stderr ---\n{stderr}"),
    }
}

fn ingest_progress_output_line(progress: WikiIngestProgress) -> String {
    if let Some(check) = progress.llm_check {
        let position = check.index() + 1;
        let total = WikiIngestLlmCheck::ALL.len();
        return match progress.state {
            WikiIngestStageState::Running => format!(
                "\u{1b}[34m  RUNNING [{position}/{total}] {}\u{1b}[0m",
                check.label()
            ),
            WikiIngestStageState::Succeeded => format!(
                "\u{1b}[32m  DONE    [{position}/{total}] {}\u{1b}[0m",
                check.label()
            ),
            WikiIngestStageState::Failed => format!(
                "\u{1b}[31m  FAILED  [{position}/{total}] {}\u{1b}[0m",
                check.label()
            ),
        };
    }
    let position = progress.stage.index() + 1;
    let total = WikiIngestStage::ALL.len();
    let details = ingest_progress_output_details(progress);
    match progress.state {
        WikiIngestStageState::Running => format!(
            "\u{1b}[34mRUNNING [{position}/{total}] {}{details}\u{1b}[0m",
            progress.stage.label(),
        ),
        WikiIngestStageState::Succeeded => format!(
            "\u{1b}[32mDONE    [{position}/{total}] {}{details}\u{1b}[0m",
            progress.stage.label(),
        ),
        WikiIngestStageState::Failed => format!(
            "\u{1b}[31mFAILED  [{position}/{total}] {}{details}\u{1b}[0m",
            progress.stage.label(),
        ),
    }
}

fn ingest_progress_output_details(progress: WikiIngestProgress) -> String {
    let (Some(completed), Some(total)) = (progress.completed, progress.total) else {
        return String::new();
    };
    let max = progress
        .max_chunk_characters
        .map(|value| format!(", max {value} chars"))
        .unwrap_or_default();
    match progress.stage {
        WikiIngestStage::Markdown => format!(" ({completed}/{total} files)"),
        WikiIngestStage::Chunking => format!(
            " ({completed}/{total} files, {} chunks{max})",
            progress.chunks.unwrap_or(0)
        ),
        WikiIngestStage::Components => format!(" ({completed}/{total} chunks{max})"),
        _ => String::new(),
    }
}

fn append_job_output_line(output: &mut String, line: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(line);
}

fn append_job_output_block(output: &mut String, block: &str) {
    if block.trim().is_empty() {
        return;
    }
    if !output.is_empty() {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
    }
    output.push_str(block.trim());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT as _, Queryable as _};

    fn test_app() -> BibiiWikiApp {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        )
    }

    fn public_wiki_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("public_wiki_root")
    }

    #[test]
    fn workspace_registry_deduplicates_equivalent_paths() {
        let mut state = PersistedState::default();
        state.ensure_workspace(PathBuf::from(r"D:\Knowledge\wiki_root"));
        state.ensure_workspace(PathBuf::from(r"D:\Knowledge\wiki_root"));
        assert_eq!(state.workspaces.len(), 1);
    }

    #[test]
    fn invalid_workspace_selection_returns_to_the_first_root() {
        let mut state = PersistedState {
            workspaces: vec![
                WorkspaceEntry::new(PathBuf::from("alpha_wiki_root")),
                WorkspaceEntry::new(PathBuf::from("beta_wiki_root")),
            ],
            selected_workspace: usize::MAX,
            ..PersistedState::default()
        };

        state.sanitize();

        assert_eq!(state.selected_workspace, 0);
    }

    #[test]
    fn workspace_sort_modes_keep_identity_and_use_expected_order() {
        let mut alpha = WorkspaceEntry::new(PathBuf::from("alpha_wiki_root"));
        alpha.name = "Alpha".to_owned();
        alpha.added_at_millis = 10;
        let mut zulu = WorkspaceEntry::new(PathBuf::from("zulu_wiki_root"));
        zulu.name = "Zulu".to_owned();
        zulu.added_at_millis = 20;
        let workspaces = vec![zulu.clone(), alpha.clone()];
        let trees = HashMap::from([
            (
                zulu.root.clone(),
                WorkspaceFileTree {
                    latest_changed_millis: 30,
                    ..WorkspaceFileTree::default()
                },
            ),
            (
                alpha.root.clone(),
                WorkspaceFileTree {
                    latest_changed_millis: 40,
                    ..WorkspaceFileTree::default()
                },
            ),
        ]);

        assert_eq!(
            workspace_sort_order(&workspaces, WorkspaceSort::Name, &trees),
            vec![1, 0]
        );
        assert_eq!(
            workspace_sort_order(&workspaces, WorkspaceSort::AddedNewest, &trees),
            vec![0, 1]
        );
        assert_eq!(
            workspace_sort_order(&workspaces, WorkspaceSort::ChangedLatest, &trees),
            vec![1, 0]
        );
    }

    #[test]
    fn only_the_selected_workspace_root_is_expanded() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("single-workspace-test-{unique}"));
        let alpha_root = test_root.join("alpha_wiki_root");
        let beta_root = test_root.join("beta_wiki_root");
        fs::create_dir_all(&alpha_root).expect("create alpha root");
        fs::create_dir_all(&beta_root).expect("create beta root");
        fs::write(alpha_root.join("only-alpha.md"), "# Alpha").expect("write alpha note");
        fs::write(beta_root.join("only-beta.md"), "# Beta").expect("write beta note");
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(alpha_root.clone());
        persisted.ensure_workspace(beta_root.clone());
        persisted.selected_workspace = 0;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_440.0, 900.0));
        harness.run_steps(2);

        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-alpha.md")
                .is_some()
        );
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-beta.md")
                .is_none()
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "alpha_wiki_root")
            .click();
        harness.run_steps(2);

        assert_eq!(harness.state().persisted.selected_workspace, 0);
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-alpha.md")
                .is_none(),
            "clicking the current wiki root again should collapse its tree"
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "alpha_wiki_root")
            .click();
        harness.run_steps(2);

        assert_eq!(harness.state().persisted.selected_workspace, 0);
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-alpha.md")
                .is_some(),
            "clicking the collapsed current wiki root should reopen its tree"
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "beta_wiki_root")
            .click();
        harness.run_steps(2);

        assert_eq!(harness.state().persisted.selected_workspace, 1);
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-alpha.md")
                .is_none()
        );
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "only-beta.md")
                .is_some()
        );

        drop(harness);
        fs::remove_dir_all(test_root).expect("remove single-workspace test tree");
    }

    #[test]
    fn light_theme_is_the_persisted_default() {
        assert_eq!(PersistedState::default().theme, Theme::Light);
    }

    #[test]
    fn persisted_locale_switches_visible_application_chrome() {
        let persisted = PersistedState {
            locale: crate::i18n::LocalePreference::ZhCn,
            workspace_dock_visible: false,
            search_dock_visible: false,
            query_dock_visible: false,
            bottom_dock_visible: false,
            ..PersistedState::default()
        };
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_200.0, 800.0));
        harness.run();

        assert!(harness.query_by_label("文件").is_some());
        assert_eq!(
            harness.state().persisted.locale,
            crate::i18n::LocalePreference::ZhCn
        );
    }

    #[test]
    fn chinese_locale_translates_the_ingest_workflow_surface() {
        let persisted = PersistedState {
            locale: LocalePreference::ZhCn,
            inspector_visible: true,
            inspector: Inspector::Ingest,
            workspace_dock_visible: false,
            search_dock_visible: false,
            query_dock_visible: false,
            bottom_dock_visible: false,
            ..PersistedState::default()
        };
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.ingest_source_panel_open = true;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_200.0, 800.0));
        harness.run();

        assert!(harness.query_by_label("导入源").is_some());
        assert!(harness.query_by_label("选择文件或目录…").is_some());
        assert!(harness.query_by_label("导入检查点").is_some());
    }

    #[test]
    fn chinese_locale_translates_the_llm_configuration_surface() {
        let persisted = PersistedState {
            locale: LocalePreference::ZhCn,
            inspector_visible: true,
            inspector: Inspector::Llm,
            workspace_dock_visible: false,
            search_dock_visible: false,
            query_dock_visible: false,
            bottom_dock_visible: false,
            ..PersistedState::default()
        };
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_400.0, 950.0));
        harness.run();

        assert!(harness.query_by_label("当前 LLM 配置").is_some());
        assert!(harness.query_by_label("LiteLLM YAML 配置").is_some());
        assert!(
            harness
                .query_by_role_and_label(egui::accesskit::Role::Button, "测试 LLM 连接")
                .is_some()
        );
    }

    #[test]
    fn chinese_locale_translates_the_tool_configuration_surface() {
        let persisted = PersistedState {
            locale: LocalePreference::ZhCn,
            inspector_visible: true,
            inspector: Inspector::Tools,
            workspace_dock_visible: false,
            search_dock_visible: false,
            query_dock_visible: false,
            bottom_dock_visible: false,
            ..PersistedState::default()
        };
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_300.0, 900.0));
        harness.run();

        assert!(harness.query_by_label("导入路径").is_some());
        assert!(harness.query_by_label("完整 TOML 工具配置").is_some());
        assert!(harness.query_by_label("验证 TOML").is_some());
    }

    #[test]
    fn ai_query_uses_commonmark_editors_for_input_and_output() {
        let app = test_app();

        assert_eq!(app.question.mode(), EditorMode::Source);
        assert_eq!(app.answer.mode(), EditorMode::Preview);
        assert!(app.answer.markdown().contains("Ask a question"));
    }

    #[test]
    fn markdown_preview_editor_is_the_default_center_and_ai_query_is_a_side_dock() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        assert!(harness.state().open_document.is_none());
        assert!(harness.state().persisted.query_dock_visible);
        let center = harness.get_by_label("Markdown Preview / Editor").rect();
        let query = harness.get_by_label("AI QUERY").rect();
        assert!(
            center.right() < query.left(),
            "AI Query must be a right side dock, not the center: center={center:?}, query={query:?}"
        );

        harness.get_by_label("Hide AI query dock").click();
        harness.run();
        assert!(harness.query_by_label("AI QUERY").is_none());
        assert!(
            harness
                .query_by_label("Markdown Preview / Editor")
                .is_some()
        );
    }

    #[test]
    #[ignore = "writes the default-center visual QA capture requested by BIBIIWIKI_DEFAULT_CENTER_QA_CAPTURE"]
    fn capture_markdown_default_center_with_ai_query_side_dock() {
        let mut app = test_app();
        app.persisted.bottom_dock_visible = false;
        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_600.0, 900.0))
            .with_step_dt(1.0 / 60.0)
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(2);

        assert!(
            harness
                .query_by_label("Markdown Preview / Editor")
                .is_some()
        );
        assert!(harness.query_by_label("AI QUERY").is_some());
        let output = std::env::var_os("BIBIIWIKI_DEFAULT_CENTER_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_DEFAULT_CENTER_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui default-center visual-QA render")
            .save(&output)
            .expect("save egui default-center visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    fn toml_tool_configuration_round_trips_every_tool_section() {
        let source = r#"
[ingest]
source_dir = "wiki_sources"
force = true

[codex]
workspace = "."

[search]
limit = 24

[query]
use_llm = false
save_memory = true
"#;

        let parsed = parse_tool_config(source).expect("complete tool TOML should parse");
        assert!(parsed.ingest.force);
        assert_eq!(parsed.search.limit, 24);
        assert!(!parsed.query.use_llm);

        let rendered = render_tool_config(&parsed).expect("tool TOML should render");
        assert_eq!(
            parse_tool_config(&rendered).expect("rendered TOML should parse"),
            parsed
        );
    }

    #[test]
    fn unified_yaml_round_trips_llm_workspace_sorting_and_tool_settings() {
        let source = include_str!("../bibiiwiki.yaml");
        let mut state = PersistedState {
            theme: Theme::Dark,
            locale: LocalePreference::Fr,
            workspace_sort: WorkspaceSort::ChangedLatest,
            search_limit: 24,
            query_uses_llm: false,
            markdown_editor_mode: EditorMode::Preview,
            ..PersistedState::default()
        };
        state.ensure_workspace(PathBuf::from(r"D:\knowledge\factor_wiki"));

        let unified = merge_local_settings(source, &state).expect("merge local settings");
        let loaded = parse_local_settings(&unified)
            .expect("parse unified config")
            .expect("local settings section");

        assert_eq!(loaded, state);
        assert!(unified.contains("# Default local development stack:"));
        assert!(unified.contains("model_name: ornith-1.5:9b"));
        assert!(unified.contains("local:"));
        assert!(unified.contains("workspace_sort: changed_latest"));
        assert!(unified.contains("locale: fr"));
        assert!(unified.contains("markdown_editor_mode: preview"));
        assert_eq!(
            Config::parse(&unified)
                .expect("valid LLM config")
                .model_list
                .len(),
            1
        );
    }

    #[test]
    fn local_changes_persist_only_to_the_unified_yaml_file() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("unified-config-{unique}"));
        let path = root.join("bibiiwiki.yaml");
        let mut state = PersistedState {
            workspace_sort: WorkspaceSort::AddedNewest,
            ..PersistedState::default()
        };
        state.ensure_workspace(root.join("wiki_root"));

        persist_local_settings_file(&path, &state).expect("persist unified settings");

        assert!(path.is_file());
        assert!(!root.join("bibiiwiki.tools.toml").exists());
        assert_eq!(
            load_local_settings_file(&path)
                .expect("load unified settings")
                .expect("local settings"),
            state
        );
        fs::remove_dir_all(root).expect("remove unified config fixture");
    }

    #[test]
    fn unified_yaml_persists_the_wizard_protocol_without_resolving_its_api_key() {
        let source = concat!(
            "model_list:\n",
            "  - model_name: response-model\n",
            "    litellm_params:\n",
            "      model: openai/response-model\n",
            "      api_key: os.environ/BIBIIWIKI_WIZARD_KEY_NOT_REQUIRED_TO_SAVE\n",
            "codex:\n",
            "  model: response-model\n",
        );
        let state = PersistedState {
            llm_protocol: Some(LlmProtocol::Responses),
            llm_provider: Some("OpenAI".to_owned()),
            ..PersistedState::default()
        };

        let unified = merge_local_settings(source, &state).expect("unified wizard config");
        let restored = parse_local_settings(&unified)
            .expect("parse local settings")
            .expect("local settings");

        assert_eq!(restored.llm_protocol, Some(LlmProtocol::Responses));
        assert_eq!(restored.llm_provider.as_deref(), Some("OpenAI"));
        assert!(unified.contains("api_key: os.environ/BIBIIWIKI_WIZARD_KEY_NOT_REQUIRED_TO_SAVE"));
    }

    #[test]
    fn llm_dock_exposes_the_syntax_highlighted_yaml_editor() {
        let app = test_app();
        let original_yaml = app.config_text.clone();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run();

        assert!(
            harness
                .query_by_label("YAML configuration editor")
                .is_some()
        );
        assert!(
            harness.query_by_label("Test LLM connection").is_some(),
            "the action must be labeled as a connection test"
        );
        assert!(
            harness.query_by_label("Who are you?").is_none(),
            "the connection-test action must not look like a chat prompt"
        );
        assert_eq!(harness.state().config_text, original_yaml);
    }

    #[test]
    fn llm_wizard_loads_the_current_local_ollama_route() {
        let wizard = LlmWizardState::from_config_text(include_str!("../bibiiwiki.yaml"));

        assert_eq!(wizard.protocol, LlmProtocol::Local);
        assert_eq!(wizard.provider, "Ollama");
        assert_eq!(wizard.api_base_url, "http://127.0.0.1:11434/v1");
        assert!(wizard.api_key_env.is_empty());
        assert_eq!(wizard.model_name, "ornith-1.5:9b");
        assert_eq!(wizard.litellm_model, "ollama/ornith-1.5:9b");
        assert_eq!(wizard.max_markdown_chunk_characters, 10_000);
    }

    #[test]
    fn llm_wizard_updates_the_route_and_preserves_unrelated_yaml() {
        let source = concat!(
            "# keep this comment\n",
            "model_list:\n",
            "  - model_name: old\n",
            "    litellm_params:\n",
            "      model: ollama/old\n",
            "      api_base: http://127.0.0.1:11434/v1\n",
            "codex:\n",
            "  model: old\n",
            "local:\n",
            "  theme: dark\n",
        );
        let wizard = LlmWizardState {
            protocol: LlmProtocol::Chat,
            provider: "DeepSeek".to_owned(),
            api_base_url: "https://api.deepseek.com/v1".to_owned(),
            api_key_source: ApiKeySource::Environment,
            api_key_env: "DEEPSEEK_API_KEY".to_owned(),
            api_key_direct: String::new(),
            model_name: "deepseek-chat".to_owned(),
            litellm_model: "deepseek/deepseek-chat".to_owned(),
            max_markdown_chunk_characters: 24_000,
        };

        let updated = apply_llm_wizard_to_yaml(source, &wizard).expect("wizard YAML");

        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("theme: dark"));
        assert!(updated.contains("model_name: deepseek-chat"));
        assert!(updated.contains("model: deepseek/deepseek-chat"));
        assert!(updated.contains("api_base: https://api.deepseek.com/v1"));
        assert!(updated.contains("api_key: os.environ/DEEPSEEK_API_KEY"));
        assert!(updated.contains("deepseek-chat: 24000"));
        assert_eq!(
            serde_yaml::from_str::<Config>(&updated)
                .expect("valid generated YAML")
                .codex_model(),
            "deepseek-chat"
        );
    }

    #[test]
    fn llm_wizard_supports_a_direct_api_key_without_exposing_it_as_an_env_reference() {
        let source = concat!(
            "model_list:\n",
            "  - model_name: old\n",
            "    litellm_params:\n",
            "      model: openai/old\n",
            "      api_key: os.environ/OLD_API_KEY\n",
        );
        let wizard = LlmWizardState {
            protocol: LlmProtocol::Responses,
            provider: "OpenAI".to_owned(),
            api_base_url: "https://api.openai.com/v1".to_owned(),
            api_key_source: ApiKeySource::Direct,
            api_key_env: String::new(),
            api_key_direct: "sk-direct-test".to_owned(),
            model_name: "gpt-test".to_owned(),
            litellm_model: "openai/gpt-test".to_owned(),
            max_markdown_chunk_characters: 48_000,
        };

        let updated = apply_llm_wizard_to_yaml(source, &wizard).expect("direct-key YAML");
        let reloaded = LlmWizardState::from_config_text(&updated);

        assert!(updated.contains("api_key: sk-direct-test"));
        assert!(!updated.contains("os.environ/OLD_API_KEY"));
        assert_eq!(reloaded.api_key_source, ApiKeySource::Direct);
        assert_eq!(reloaded.api_key_direct, "sk-direct-test");
    }

    #[test]
    fn llm_dock_places_the_current_configuration_wizard_above_the_yaml() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_440.0, 1_100.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run_steps(2);

        for label in [
            "Current LLM configuration",
            "Interface type",
            "Provider",
            "API base URL",
            "API key source",
            "Environment variable",
            "Model name",
            "Markdown chunk size",
            "Apply wizard to YAML",
            "LiteLLM YAML configuration",
        ] {
            assert!(
                harness.query_by_label(label).is_some(),
                "missing LLM wizard control {label}"
            );
        }
        let wizard = harness.get_by_label("Current LLM configuration").rect();
        let yaml = harness.get_by_label("LiteLLM YAML configuration").rect();
        let editor = harness.get_by_label("YAML configuration editor").rect();
        assert!(wizard.top() < yaml.top());
        assert!(yaml.top() < editor.top());
    }

    #[test]
    fn llm_connection_test_reports_all_three_checks_inline() {
        let mut app = test_app();
        app.llm_diagnostic_runner = Arc::new(|_, _, sender| {
            sender
                .send(LlmDiagnosticMessage::ModelPassed(
                    "I am the configured test model.".to_owned(),
                ))
                .expect("model diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::ProxyStarted)
                .expect("proxy diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::ProxyPassed(
                    "I am the proxied test model.".to_owned(),
                ))
                .expect("proxy diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::CodexStarted)
                .expect("Codex diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::CodexPassed(
                    "I am Codex using the proxied test model.".to_owned(),
                ))
                .expect("Codex diagnostic receiver");
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run();

        for label in [
            "Testing LLM connection",
            "Configured LLM works",
            "LLM proxy works",
            "Codex adapter works",
            "Configured LLM reply box (empty)",
            "Responses proxy reply box (empty)",
            "Codex adapter reply box (empty)",
        ] {
            assert!(harness.query_by_label(label).is_some(), "missing {label}");
        }
        assert!(
            harness
                .query_by_label("Configured route: ornith-1.5:9b -> ollama/ornith-1.5:9b")
                .is_some()
        );
        assert!(
            harness
                .query_by_label("Responses proxy endpoint: http://127.0.0.1:4000/v1/responses",)
                .is_some()
        );

        harness.get_by_label("Test LLM connection").click();
        for _ in 0..100 {
            // The spinner deliberately requests another repaint while the
            // background checks run, so advance one deterministic frame
            // instead of asking the harness to wait for a quiescent UI.
            harness.run_steps(1);
            if harness.state().llm_diagnostic.phase == LlmDiagnosticPhase::Complete {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(
            harness.state().llm_diagnostic.phase,
            LlmDiagnosticPhase::Complete
        );
        for label in [
            "Configured LLM works",
            "LLM proxy works",
            "Codex adapter works",
            "Configured LLM reply box (complete)",
            "Responses proxy reply box (complete)",
            "Codex adapter reply box (complete)",
            "Configured LLM reply: I am the configured test model.",
            "Responses proxy reply: I am the proxied test model.",
            "Codex adapter reply: I am Codex using the proxied test model.",
        ] {
            assert!(harness.query_by_label(label).is_some(), "missing {label}");
        }
    }

    #[test]
    fn llm_connection_test_shows_progress_while_waiting() {
        let mut app = test_app();
        app.llm_diagnostic_runner = Arc::new(|_, _, _| {
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run();
        harness.get_by_label("Test LLM connection").click();
        harness.run_steps(2);

        assert_eq!(
            harness.state().llm_diagnostic.phase,
            LlmDiagnosticPhase::TestingModel
        );
        assert!(harness.query_by_label("Testing LLM connection").is_some());
        assert!(harness.query_by_label("Testing configured LLM…").is_some());
        assert!(
            harness
                .query_by_role_and_label(
                    egui::accesskit::Role::ProgressIndicator,
                    "Testing configured LLM progress",
                )
                .is_some()
        );
        assert_eq!(
            harness
                .output()
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .expect("root viewport output")
                .repaint_delay,
            Duration::ZERO,
            "the spinner must continuously request frames for its circular animation"
        );
        assert!(
            harness
                .get_by_label("Test LLM connection")
                .accesskit_node()
                .is_disabled()
        );
    }

    #[test]
    fn llm_connection_test_shows_codex_adapter_progress() {
        let mut app = test_app();
        app.llm_diagnostic_runner = Arc::new(|_, _, sender| {
            sender
                .send(LlmDiagnosticMessage::ModelPassed(
                    "I am the configured test model.".to_owned(),
                ))
                .expect("model diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::ProxyStarted)
                .expect("proxy diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::ProxyPassed(
                    "I am the proxied test model.".to_owned(),
                ))
                .expect("proxy diagnostic receiver");
            sender
                .send(LlmDiagnosticMessage::CodexStarted)
                .expect("Codex diagnostic receiver");
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run();
        harness.get_by_label("Test LLM connection").click();
        for _ in 0..100 {
            harness.run_steps(1);
            if harness.state().llm_diagnostic.phase == LlmDiagnosticPhase::TestingCodex {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(
            harness.state().llm_diagnostic.phase,
            LlmDiagnosticPhase::TestingCodex
        );
        assert!(
            harness
                .query_by_label("Testing Codex adapter through proxy…")
                .is_some()
        );
        assert!(
            harness
                .query_by_role_and_label(
                    egui::accesskit::Role::ProgressIndicator,
                    "Testing Codex adapter progress",
                )
                .is_some()
        );
        assert!(
            harness
                .get_by_label("Test LLM connection")
                .accesskit_node()
                .is_disabled()
        );
    }

    #[tokio::test]
    async fn llm_connection_test_traverses_direct_and_responses_routes() {
        async fn answer(
            axum::extract::State(calls): axum::extract::State<Arc<std::sync::atomic::AtomicUsize>>,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            assert!(body.to_string().contains("Who are you?"));
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            axum::Json(json!({
                "id": "chatcmpl-diagnostic",
                "object": "chat.completion",
                "model": "diagnostic-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "I am a diagnostic model."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9}
            }))
        }

        async fn answer_ollama(
            axum::extract::State(calls): axum::extract::State<Arc<std::sync::atomic::AtomicUsize>>,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::Json<Value> {
            assert!(body.to_string().contains("Who are you?"));
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            axum::Json(json!({
                "model": "diagnostic-model",
                "message": {"role": "assistant", "content": "I am a diagnostic model."},
                "done": true,
                "done_reason": "stop",
                "prompt_eval_count": 4,
                "eval_count": 5
            }))
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock upstream listener");
        let address = listener.local_addr().expect("mock upstream address");
        let upstream = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(answer))
            .route("/api/chat", axum::routing::post(answer_ollama))
            .with_state(Arc::clone(&calls));
        tokio::spawn(async move {
            axum::serve(listener, upstream)
                .await
                .expect("mock upstream server");
        });
        let config = format!(
            "server:\n  bind: 127.0.0.1:0\n  request_timeout_seconds: 10\n  default_max_output_tokens: 256\nmodel_list:\n  - model_name: diagnostic\n    litellm_params:\n      model: ollama/diagnostic-model\n      api_base: http://{address}/v1\ncodex:\n  binary: bibiiwiki-missing-codex-diagnostic\n  model: diagnostic\n  reasoning_effort: none\n"
        );
        let (sender, receiver) = mpsc::channel();

        run_llm_diagnostic_async(&config, Path::new(env!("CARGO_MANIFEST_DIR")), &sender).await;

        let messages: Vec<_> = receiver.try_iter().collect();
        assert_eq!(messages.len(), 5);
        assert!(matches!(
            &messages[0],
            LlmDiagnosticMessage::ModelPassed(reply)
                if reply == "I am a diagnostic model."
        ));
        assert!(matches!(messages[1], LlmDiagnosticMessage::ProxyStarted));
        assert!(matches!(
            &messages[2],
            LlmDiagnosticMessage::ProxyPassed(reply)
                if reply == "I am a diagnostic model."
        ));
        assert!(matches!(messages[3], LlmDiagnosticMessage::CodexStarted));
        assert!(matches!(
            &messages[4],
            LlmDiagnosticMessage::CodexFailed(error)
                if error.contains("failed to launch")
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    #[ignore = "requires Ollama with ornith-1.5:9b installed and running"]
    fn live_llm_connection_test_checks_configured_model_and_proxy() {
        let config =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"))
                .expect("checked-in LLM configuration");
        let (sender, receiver) = mpsc::channel();

        run_llm_diagnostic(&config, Path::new(env!("CARGO_MANIFEST_DIR")), &sender);

        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            messages.first(),
            Some(LlmDiagnosticMessage::ModelPassed(reply)) if !reply.is_empty()
        ));
        assert!(matches!(
            messages.get(1),
            Some(LlmDiagnosticMessage::ProxyStarted)
        ));
        assert!(matches!(
            messages.get(2),
            Some(LlmDiagnosticMessage::ProxyPassed(reply)) if !reply.is_empty()
        ));
        assert!(matches!(
            messages.get(3),
            Some(LlmDiagnosticMessage::CodexStarted)
        ));
        assert!(matches!(
            messages.get(4),
            Some(LlmDiagnosticMessage::CodexPassed(reply)) if !reply.is_empty()
        ));
    }

    #[test]
    fn tools_dock_shows_the_complete_toml_editor_and_actions() {
        let app = test_app();
        let original_toml = app.tool_config_text.clone();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Tools dock").click();
        harness.run();

        assert!(
            harness
                .query_by_label("Full TOML tool configuration")
                .is_some()
        );
        assert!(harness.query_by_label("Reload TOML").is_some());
        assert!(harness.query_by_label("Validate TOML").is_some());
        assert!(harness.query_by_label("Apply TOML").is_some());
        assert!(harness.query_by_label("Save TOML").is_some());
        assert!(
            harness
                .query_by_label("TOML configuration editor")
                .is_some()
        );
        assert_eq!(harness.state().tool_config_text, original_toml);
    }

    #[test]
    fn missing_config_and_corrupt_workspace_state_do_not_block_ui_state_creation() {
        let persisted = PersistedState {
            workspaces: vec![WorkspaceEntry::new(PathBuf::from("wiki_root"))],
            selected_workspace: 99,
            ..PersistedState::default()
        };

        let app = BibiiWikiApp::from_state(persisted, PathBuf::from(r"Z:\missing\bibiiwiki.yaml"));

        assert_eq!(app.persisted.selected_workspace, 0);
        assert!(!app.startup_issues.is_empty());
        assert!(app.config_text.is_empty());
    }

    #[test]
    fn checked_in_icon_and_logo_decode() {
        let icon = eframe::icon_data::from_png_bytes(APP_ICON_PNG).expect("valid application icon");
        let logo = eframe::icon_data::from_png_bytes(APP_LOGO_PNG).expect("valid application logo");

        assert_eq!((icon.width, icon.height), (512, 512));
        assert_eq!((logo.width, logo.height), (1600, 302));
    }

    #[test]
    fn startup_panic_falls_back_to_recovery_workspace() {
        let app = BibiiWikiApp::startup_recovery(
            PathBuf::from(r"Z:\missing\bibiiwiki.yaml"),
            PathBuf::from("wiki_root"),
            "simulated failure",
        );

        assert!(
            app.recovery_error
                .as_deref()
                .is_some_and(|error| error.contains("simulated failure"))
        );
        assert!(!app.persisted.workspaces.is_empty());
    }

    #[test]
    fn recovery_workspace_remains_interactive_and_can_retry() {
        let app = BibiiWikiApp::startup_recovery(
            PathBuf::from(r"Z:\missing\bibiiwiki.yaml"),
            PathBuf::from("wiki_root"),
            "simulated failure",
        );
        let mut harness =
            Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render_recovery(ctx), app);
        harness.set_size(Vec2::new(1200.0, 760.0));
        harness.run();

        assert!(
            harness
                .query_by_label("The workspace recovered from a UI error")
                .is_some()
        );
        harness.get_by_label("Retry normal workspace").click();
        harness.run();

        assert!(harness.state().recovery_error.is_none());
    }

    #[test]
    fn view_menu_switches_to_the_dark_theme() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("View").click();
        harness.run();
        harness.get_by_label("Dark theme").click();
        harness.run();

        assert_eq!(harness.state().persisted.theme, Theme::Dark);
    }

    #[test]
    fn obsidian_vault_uri_has_no_spurious_action_slash() {
        let uri = obsidian_open_uri("0e01b465f940e844", None, false);
        assert_eq!(uri, "obsidian://open?vault=0e01b465f940e844");
    }

    #[test]
    fn obsidian_note_uri_encodes_file_and_opens_a_new_tab() {
        let uri = obsidian_open_uri(
            "0e01b465f940e844",
            Some(Path::new(r"wiki\因子 笔记.md")),
            true,
        );
        assert_eq!(
            uri,
            "obsidian://open?vault=0e01b465f940e844&file=wiki%2F%E5%9B%A0%E5%AD%90+%E7%AC%94%E8%AE%B0.md&paneType=tab"
        );
    }

    #[test]
    fn obsidian_registry_matches_the_exact_workspace_path() {
        let registry: ObsidianRegistry = serde_json::from_value(serde_json::json!({
            "vaults": {
                "known-vault-id": {"path": "D:\\Python\\invest_loop\\wiki_root"}
            }
        }))
        .expect("valid registry fixture");

        assert_eq!(
            find_registered_obsidian_vault(registry, Path::new(r"D:\Python\invest_loop\wiki_root")),
            Some("known-vault-id".to_owned())
        );
    }

    #[test]
    fn obsidian_auto_registration_preserves_backend_state_and_is_idempotent() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("obsidian-registration-{unique}"));
        let vault_root = test_root.join("new_wiki_root");
        let backend_dir = test_root.join("obsidian");
        let registry_path = backend_dir.join("obsidian.json");
        fs::create_dir_all(&vault_root).expect("test vault");
        fs::create_dir_all(&backend_dir).expect("test backend");
        fs::write(
            &registry_path,
            r#"{"vaults":{"existing-id":{"path":"D:\\existing","ts":123,"open":true}},"cli":true}"#,
        )
        .expect("registry fixture");

        let registered_id =
            register_obsidian_vault_at(&registry_path, &vault_root).expect("register vault");
        assert_eq!(registered_id.len(), 16);
        assert!(registered_id.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let saved: Value =
            serde_json::from_slice(&fs::read(&registry_path).expect("saved Obsidian registry"))
                .expect("valid saved registry");
        assert_eq!(saved.get("cli"), Some(&Value::Bool(true)));
        assert_eq!(
            saved.pointer("/vaults/existing-id/open"),
            Some(&Value::Bool(true))
        );
        let registered = saved
            .pointer(&format!("/vaults/{registered_id}"))
            .expect("new vault entry");
        assert_eq!(
            registered.get("path").and_then(Value::as_str),
            Some(vault_root.to_string_lossy().as_ref())
        );
        assert!(registered.get("ts").and_then(Value::as_u64).is_some());
        assert!(
            vault_root.join(".obsidian").is_dir(),
            "registration must initialize the folder as an Obsidian vault"
        );
        let vault_index_path = backend_dir.join(format!("{registered_id}.json"));
        let vault_index: Value =
            serde_json::from_slice(&fs::read(&vault_index_path).expect("per-vault Obsidian index"))
                .expect("valid per-vault Obsidian index");
        assert_eq!(vault_index.get("devTools"), Some(&Value::Bool(false)));
        assert_eq!(vault_index.get("zoom"), Some(&Value::from(0)));

        fs::remove_file(&vault_index_path).expect("simulate missing per-vault index");

        let repeated_id =
            register_obsidian_vault_at(&registry_path, &vault_root).expect("repeat registration");
        assert_eq!(repeated_id, registered_id);
        assert!(
            vault_index_path.is_file(),
            "repeat registration must repair a missing per-vault index"
        );
        let repeated: Value =
            serde_json::from_slice(&fs::read(&registry_path).expect("repeated Obsidian registry"))
                .expect("valid repeated registry");
        assert_eq!(
            repeated
                .get("vaults")
                .and_then(Value::as_object)
                .map(serde_json::Map::len),
            Some(2)
        );

        fs::remove_dir_all(&test_root).expect("remove scoped registration fixture");
    }

    #[test]
    fn obsidian_auto_registration_does_not_overwrite_a_malformed_registry() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("obsidian-malformed-{unique}"));
        let vault_root = test_root.join("wiki_root");
        let registry_path = test_root.join("obsidian").join("obsidian.json");
        fs::create_dir_all(&vault_root).expect("test vault");
        fs::create_dir_all(registry_path.parent().expect("backend directory"))
            .expect("test backend");
        fs::write(&registry_path, b"{broken registry").expect("malformed registry fixture");

        let error = register_obsidian_vault_at(&registry_path, &vault_root)
            .expect_err("malformed backend must be reported");
        assert!(error.to_string().contains("could not parse"));
        assert_eq!(
            fs::read(&registry_path).expect("original malformed registry"),
            b"{broken registry"
        );

        fs::remove_dir_all(&test_root).expect("remove scoped malformed fixture");
    }

    #[test]
    #[ignore = "writes the live Obsidian registry requested by BIBIIWIKI_OBSIDIAN_QA_ROOT"]
    fn live_obsidian_registration_creates_link_and_vault_index() {
        let root = std::env::var_os("BIBIIWIKI_OBSIDIAN_QA_ROOT")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_OBSIDIAN_QA_ROOT to an existing wiki root");
        let registration =
            register_workspace_with_obsidian(&root).expect("live Obsidian registration");
        let registry_path = obsidian_registry_path().expect("Windows Obsidian registry");
        let backend_dir = registry_path.parent().expect("Obsidian backend directory");

        assert_eq!(
            registered_obsidian_vault(&root).expect("read registered vault"),
            Some(registration.vault_id.clone())
        );
        assert!(root.join(".obsidian").is_dir());
        assert!(
            backend_dir
                .join(format!("{}.json", registration.vault_id))
                .is_file()
        );
        assert_eq!(
            registration.uri,
            obsidian_open_uri(&registration.vault_id, None, false)
        );
        eprintln!("{}", registration.uri);
    }

    #[test]
    fn dock_triangles_hide_and_restore_each_panel_independently() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Hide workspace dock").click();
        harness.run();
        assert!(!harness.state().persisted.workspace_dock_visible);
        assert!(harness.state().persisted.search_dock_visible);

        harness.get_by_label("Restore workspace dock").click();
        harness.run();
        assert!(harness.state().persisted.workspace_dock_visible);

        harness.get_by_label("Hide search dock").click();
        harness.run();
        assert!(!harness.state().persisted.search_dock_visible);
        assert!(harness.state().persisted.workspace_dock_visible);

        harness.get_by_label("Restore search dock").click();
        harness.run();
        assert!(harness.state().persisted.search_dock_visible);

        harness.get_by_label("Hide AI query dock").click();
        harness.run();
        assert!(!harness.state().persisted.query_dock_visible);
        assert!(harness.query_by_label("AI QUERY").is_none());

        harness.get_by_label("Restore AI query dock").click();
        harness.run();
        assert!(harness.state().persisted.query_dock_visible);
        assert!(harness.query_by_label("AI QUERY").is_some());
    }

    #[test]
    fn toolbar_and_output_dock_can_be_hidden_and_restored() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Hide toolbar").click();
        harness.run();
        assert!(!harness.state().persisted.toolbar_visible);
        assert!(harness.query_by_label("Hide toolbar").is_none());

        harness.get_by_label("View").click();
        harness.run();
        harness.get_by_label("Toolbar").click();
        harness.run();
        assert!(harness.state().persisted.toolbar_visible);
        assert!(harness.query_by_label("Hide toolbar").is_some());

        harness.get_by_label("Hide output dock").click();
        harness.run();
        assert!(!harness.state().persisted.bottom_dock_visible);
        assert!(harness.query_by_label("Hide output dock").is_none());

        harness.get_by_label("View").click();
        harness.run();
        harness.get_by_label("Output dock").click();
        harness.run();
        assert!(harness.state().persisted.bottom_dock_visible);
        assert!(harness.query_by_label("Hide output dock").is_some());
    }

    fn select_and_copy_job_output(
        harness: &mut Harness<'_, BibiiWikiApp>,
        output_pane: egui::Rect,
    ) -> String {
        let text_rect = harness
            .get_by_label("Selectable colorized job output")
            .rect()
            .intersect(output_pane);
        let from = text_rect.left_top() + Vec2::new(6.0, 9.0);
        let to = egui::pos2((from.x + 180.0).min(text_rect.right() - 4.0), from.y);
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.step();
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(to));
        harness.step();
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.input_mut().events.push(egui::Event::Copy);
        harness.step();
        harness
            .output()
            .platform_output
            .commands
            .iter()
            .find_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .expect("dragging across output then copying must send text to the clipboard")
    }

    #[test]
    fn job_output_is_native_selectable_clear_and_colorized() {
        use std::fmt::Write as _;

        let mut app = test_app();
        let mut output = concat!(
            "documents=3 sources_copied=3 sources_analyzed=0\n",
            "sources_pending_analysis=3 factors=0\n\n",
            "--- stderr ---\n",
            "\u{1b}[2m2026-08-29T09:12:28Z\u{1b}[0m ",
            "\u{1b}[33mWARN\u{1b}[0m AnyDoc requested OCR\n",
            "\u{1b}[31mERROR\u{1b}[0m semantic source analysis pending\n",
        )
        .to_owned();
        writeln!(&mut output, "long-line={}", "0123456789".repeat(30))
            .expect("write long terminal line");
        for index in 0..40 {
            writeln!(&mut output, "progress line {index}: 中文路径清晰可读")
                .expect("write terminal progress line");
        }
        app.jobs.push_back(JobRecord {
            id: 77,
            label: "Ingest".to_owned(),
            purpose: JobPurpose::Ingest,
            status: JobStatus::Failed,
            output,
            ingest_steps: None,
            ingest_units: None,
            ingest_llm_checks: None,
        });
        app.selected_job = Some(77);
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        configure_fonts(&harness.ctx);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();
        harness.step();

        assert!(
            harness
                .query_by_label("Selectable colorized job output")
                .is_some()
        );
        assert!(
            harness
                .query_by_label("Terminal renderer unavailable")
                .is_none()
        );
        assert!(
            harness
                .query_by_label("Drag the upper border to resize")
                .is_some()
        );
        let jobs = harness.get_by_label("Jobs pane").rect();
        let output = harness.get_by_label("Output pane").rect();
        let context = harness.get_by_label("Context pane").rect();
        let status = harness.get_by_label("Status pane").rect();
        assert!(
            output.width() > jobs.width() * 3.0,
            "{jobs:?} vs {output:?}"
        );
        assert!(
            output.width() > context.width() * 3.0,
            "{context:?} vs {output:?}"
        );
        assert!(
            output.width() > status.width() * 2.5,
            "{status:?} vs {output:?}"
        );

        assert!(!select_and_copy_job_output(&mut harness, output).is_empty());
        if let Some(output) = std::env::var_os("BIBIIWIKI_TERMINAL_QA_CAPTURE") {
            harness
                .render()
                .expect("render colorized terminal visual QA")
                .save(PathBuf::from(output))
                .expect("save colorized terminal visual QA");
        }
    }

    #[test]
    fn output_pane_separators_drag_independently() {
        fn drag_separator(
            harness: &mut Harness<'_, BibiiWikiApp>,
            left_label: &str,
            right_label: &str,
            delta: f32,
        ) {
            let left_before = harness.get_by_label(left_label).rect();
            let right_before = harness.get_by_label(right_label).rect();
            let from = egui::pos2(
                f32::midpoint(left_before.right(), right_before.left()),
                left_before.center().y,
            );
            let to = from + Vec2::new(delta, 0.0);
            harness.input_mut().events.extend([
                egui::Event::PointerMoved(from),
                egui::Event::PointerButton {
                    pos: from,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            harness.step();
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(to));
            harness.step();
            harness.step();
            harness.input_mut().events.push(egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            harness.run();

            let left_after = harness.get_by_label(left_label).rect();
            assert!(
                left_after.width() > left_before.width() + delta * 0.5,
                "{left_label} separator did not move: {left_before:?} -> {left_after:?}"
            );
        }

        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        drag_separator(&mut harness, "Jobs pane", "Output pane", 30.0);
        drag_separator(&mut harness, "Output pane", "Context pane", 30.0);
        drag_separator(&mut harness, "Context pane", "Status pane", 25.0);
    }

    #[test]
    fn output_dock_upper_border_drags_to_resize_and_persists_height() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        let before =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("output_dock"))
                .expect("output panel state")
                .rect;
        let upper_before = ["workspace_dock", "search_dock", "query_dock"].map(|panel_id| {
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new(panel_id))
                .unwrap_or_else(|| panic!("{panel_id} state before output resize"))
                .rect
        });
        let from = harness.get_by_label("Resize output dock").rect().center();
        let to = from - Vec2::new(0.0, 80.0);
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.step();
        assert!(
            harness.state().output_dock_drag_start_height.is_some(),
            "resize press at {from:?} did not activate handle {:?}",
            harness.get_by_label("Resize output dock").rect()
        );
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(to));
        harness.step();
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();

        let after = egui::containers::panel::PanelState::load(&harness.ctx, Id::new("output_dock"))
            .expect("resized output panel state")
            .rect;
        assert!(
            after.height() > before.height() + 50.0,
            "{before:?} -> {after:?}"
        );
        for ((panel_id, before), after_panel) in ["workspace_dock", "search_dock", "query_dock"]
            .into_iter()
            .zip(upper_before)
            .zip(
                ["workspace_dock", "search_dock", "query_dock"].map(|panel_id| {
                    egui::containers::panel::PanelState::load(&harness.ctx, Id::new(panel_id))
                        .unwrap_or_else(|| panic!("{panel_id} state after output resize"))
                        .rect
                }),
            )
        {
            assert!(
                before.height() > after_panel.height() + 50.0,
                "{panel_id} did not shrink with the output dock: {before:?} -> {after_panel:?}"
            );
            assert!(
                after_panel.bottom() <= after.top() + 1.0,
                "{panel_id} overlaps Activity & Output: panel={after_panel:?}, output={after:?}"
            );
        }
        assert!((harness.state().persisted.bottom_dock_height - after.height()).abs() < 1.0);
        for _ in 0..30 {
            harness.step();
        }
        let settled =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("output_dock"))
                .expect("settled output panel state")
                .rect;
        assert!(
            (settled.height() - after.height()).abs() < 1.0,
            "resized output dock kept growing: {after:?} -> {settled:?}"
        );
    }

    #[test]
    fn output_dock_height_stays_fixed_without_a_user_drag() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.step();

        let initial_height = harness.state().persisted.bottom_dock_height;
        for _ in 0..30 {
            harness.step();
        }
        let settled_height = harness.state().persisted.bottom_dock_height;

        assert!(
            (settled_height - initial_height).abs() < 1.0,
            "output dock drifted without input: {initial_height} -> {settled_height}"
        );
    }

    #[test]
    fn compressed_inspector_scrolls_above_a_tall_output_dock() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.query_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        app.persisted.bottom_dock_visible = true;
        app.persisted.bottom_dock_height = 520.0;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1000.0, 800.0));
        harness.run();

        let visible_diagnostic = |harness: &Harness<'_, BibiiWikiApp>| {
            harness.output().shapes.iter().any(|clipped| {
                let egui::Shape::Text(text) = &clipped.shape else {
                    return false;
                };
                text.galley.job.text == "Testing LLM connection"
                    && clipped
                        .clip_rect
                        .intersects(egui::Rect::from_min_size(text.pos, text.galley.size()))
            })
        };
        assert!(
            !visible_diagnostic(&harness),
            "the diagnostic should begin below the compressed inspector viewport"
        );

        harness
            .get_by_label("Testing LLM connection")
            .scroll_to_me();
        harness.run();

        assert!(
            visible_diagnostic(&harness),
            "the inspector must scroll vertically so content hidden by a tall output dock remains reachable"
        );
    }

    #[test]
    fn llm_configuration_panel_has_a_wheel_scrollable_vertical_viewport() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.query_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        app.persisted.bottom_dock_visible = true;
        app.persisted.bottom_dock_height = 520.0;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_000.0, 800.0));
        harness.run();

        let viewport = harness
            .query_by_label("LLM configuration vertical scroll viewport")
            .expect("the LLM configuration panel must expose its vertical scroll viewport");
        let pointer = viewport.rect().center();
        let initial_heading_top = harness
            .get_by_label("Current LLM configuration")
            .rect()
            .top();

        for _ in 0..8 {
            harness.input_mut().events.extend([
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: Vec2::new(0.0, -8.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            harness.step();
        }

        let scrolled_heading_top = harness
            .get_by_label("Current LLM configuration")
            .rect()
            .top();
        assert!(
            scrolled_heading_top < initial_heading_top - 20.0,
            "mouse-wheel input did not vertically scroll the LLM configuration panel: {initial_heading_top} -> {scrolled_heading_top}"
        );
    }

    #[test]
    fn llm_yaml_editor_has_a_tall_editing_viewport() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.query_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_200.0, 900.0));
        harness.run();

        let yaml_viewport = harness
            .query_by_label("YAML editor vertical scroll viewport")
            .expect("the YAML editor must expose its bounded editing viewport")
            .rect();
        assert!(
            yaml_viewport.height() >= 480.0,
            "the YAML editing viewport is still too short: {yaml_viewport:?}"
        );
    }

    #[test]
    fn chrome_and_direct_tool_docks_expose_the_required_controls() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        for label in [
            "File",
            "View",
            "Tasks",
            "Help",
            "Workspace",
            "Ingest",
            "Lint",
            "Update",
            "Ask AI",
            "Output",
        ] {
            assert!(
                harness.query_all_by_label(label).count() > 0,
                "missing {label}"
            );
        }

        harness.get_by_label("Ingest dock").click();
        harness.run();
        for label in [
            "Ingest sources panel",
            "Force re-ingest",
            "Recent ingest jobs",
        ] {
            assert!(
                harness.query_all_by_label(label).count() > 0,
                "missing {label}"
            );
        }
        assert!(harness.query_by_label("Run lint").is_none());
        assert!(harness.query_by_label("Run update").is_none());

        harness.get_by_label("Lint dock").click();
        harness.run();
        assert!(harness.query_by_label("Run lint").is_some());
        assert!(harness.query_by_label("Recent lint jobs").is_some());
        assert!(harness.query_by_label("Ingest sources panel").is_none());

        harness.get_by_label("Update dock").click();
        harness.run();
        assert!(harness.query_by_label("Run update").is_some());
        assert!(harness.query_by_label("Recent update jobs").is_some());
        assert!(harness.query_by_label("Run lint").is_none());
        assert_eq!(
            harness.query_all_by_label("Initialize").count(),
            0,
            "initialization must be an internal ingest precondition"
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Tasks")
            .click();
        harness.run();
        assert!(harness.query_by_label("Ingest sources").is_some());
        assert!(harness.query_by_label("Initialize wiki").is_none());
    }

    #[test]
    fn workspace_context_menu_exposes_obsidian_open() {
        let app = test_app();
        let workspace_name = app
            .selected_workspace()
            .expect("test workspace")
            .name
            .clone();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &workspace_name)
            .click_secondary();
        harness.run();

        assert!(harness.query_by_label("Open in Obsidian").is_some());
    }

    #[test]
    fn directory_picker_adds_and_selects_the_chosen_wiki_root() {
        let mut app = test_app();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let selected = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("picked-wiki-{unique}"));
        fs::create_dir_all(&selected).expect("selected directory fixture");
        let picked = selected.clone();
        app.directory_picker = Arc::new(move |_| Ok(Some(picked.clone())));
        let registrations = Arc::new(Mutex::new(Vec::new()));
        let recorded_registrations = Arc::clone(&registrations);
        app.workspace_registrar = Arc::new(move |root| {
            recorded_registrations
                .lock()
                .expect("registrations")
                .push(root.to_path_buf());
            Ok(ObsidianRegistration {
                vault_id: "new-vault-id".to_owned(),
                uri: "obsidian://open?vault=new-vault-id".to_owned(),
            })
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Add wiki root").click();
        harness.run();

        assert_eq!(harness.state().persisted.workspaces.len(), 2);
        assert!(same_path(
            &harness
                .state()
                .selected_workspace()
                .expect("picked workspace")
                .root,
            &selected
        ));
        let workspace = harness
            .state()
            .selected_workspace()
            .expect("picked workspace");
        assert_eq!(workspace.obsidian_vault_id.as_deref(), Some("new-vault-id"));
        assert_eq!(
            workspace.obsidian_uri.as_deref(),
            Some("obsidian://open?vault=new-vault-id")
        );
        assert_eq!(
            registrations.lock().expect("registrations").as_slice(),
            std::slice::from_ref(&selected)
        );
        assert!(
            harness
                .state()
                .status
                .contains("Obsidian vault new-vault-id indexed")
        );
        assert!(harness.query_by_label("Add wiki workspace").is_none());

        fs::remove_dir_all(selected).expect("selected directory fixture cleanup");
    }

    #[test]
    fn obsidian_registration_failure_does_not_block_adding_a_workspace() {
        let mut app = test_app();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let selected = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("picked-wiki-registration-failure-{unique}"));
        fs::create_dir_all(&selected).expect("selected directory fixture");
        let picked = selected.clone();
        app.directory_picker = Arc::new(move |_| Ok(Some(picked.clone())));
        app.workspace_registrar = Arc::new(|_| Err(anyhow!("simulated Obsidian registry failure")));
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Add wiki root").click();
        harness.run();

        assert!(
            harness
                .state()
                .persisted
                .workspaces
                .iter()
                .any(|workspace| same_path(&workspace.root, &selected))
        );
        assert!(
            harness
                .state()
                .status
                .contains("Obsidian setup failed: simulated Obsidian registry failure")
        );
        assert!(harness.query_by_label("WORKSPACES").is_some());

        fs::remove_dir_all(selected).expect("selected directory fixture cleanup");
    }

    #[test]
    fn directory_picker_cancel_and_failure_leave_the_workspace_usable() {
        let mut cancelled_app = test_app();
        cancelled_app.directory_picker = Arc::new(|_| Ok(None));
        let mut cancelled =
            Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), cancelled_app);
        cancelled.set_size(Vec2::new(1440.0, 900.0));
        cancelled.run();

        cancelled.get_by_label("Add wiki root").click();
        cancelled.run();

        assert_eq!(cancelled.state().persisted.workspaces.len(), 1);
        assert_eq!(cancelled.state().status, "Workspace selection cancelled");
        assert!(cancelled.query_by_label("WORKSPACES").is_some());

        let mut failed_app = test_app();
        failed_app.directory_picker = Arc::new(|_| Err(anyhow!("simulated native dialog failure")));
        let mut failed =
            Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), failed_app);
        failed.set_size(Vec2::new(1440.0, 900.0));
        failed.run();

        failed.get_by_label("Add wiki root").click();
        failed.run();

        assert_eq!(failed.state().persisted.workspaces.len(), 1);
        assert!(
            failed
                .state()
                .status
                .contains("simulated native dialog failure")
        );
        assert!(failed.query_by_label("WORKSPACES").is_some());
    }

    #[test]
    fn ingest_asks_for_files_and_passes_every_selection_to_the_cli() {
        let mut app = test_app();
        let selected = vec![
            PathBuf::from(r"D:\sources\momentum.md"),
            PathBuf::from(r"D:\sources\factors.xlsx"),
        ];
        let picker_selection = selected.clone();
        app.ingest_source_picker = Arc::new(move |mode, _| {
            assert_eq!(mode, IngestPickerMode::Files);
            Ok(Some(picker_selection.clone()))
        });
        let captured = Arc::new(Mutex::new(Vec::<OsString>::new()));
        let captured_args = Arc::clone(&captured);
        app.job_runner = Arc::new(move |args, _, _observe_stdout| {
            captured_args
                .lock()
                .expect("captured args")
                .clone_from(&args.to_vec());
            Ok((true, "documents=2".to_owned(), String::new()))
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Ingest")
            .next()
            .expect("toolbar ingest action")
            .click();
        harness.run();

        assert!(harness.query_by_label("Select ingest sources").is_none());
        assert!(harness.query_by_label("Ingest sources panel").is_some());
        assert!(harness.state().persisted.inspector_visible);
        assert_eq!(harness.state().persisted.inspector, Inspector::Ingest);
        assert!(
            harness
                .query_by_label("Select files or directories…")
                .is_some()
        );
        for stage in [
            "Initialize wiki directories",
            "Extract Markdown",
            "Chunk Markdown",
            "Check LLM availability",
            "Extract wiki components",
            "Extract concepts",
            "Extract entities",
            "Extract factors",
            "Extract methodologies",
            "Extract queries",
        ] {
            assert!(
                harness
                    .query_by_label(&format!("Ingest step: {stage} — pending"))
                    .is_some(),
                "missing pending ingest checkpoint {stage}"
            );
        }
        for check in ["Original LLM", "LiteLLM proxy", "Codex agent"] {
            assert!(
                harness
                    .query_by_label(&format!("Ingest LLM check: {check} — pending"))
                    .is_some(),
                "missing pending LLM preflight substep {check}"
            );
        }
        assert!(harness.query_by_label("Files…").is_none());
        assert!(harness.query_by_label("Directories…").is_none());
        assert!(harness.state().jobs.is_empty());
        harness.get_by_label("Select files or directories…").click();
        harness.run();
        assert!(harness.query_by_label("Files…").is_some());
        assert!(harness.query_by_label("Directories…").is_some());
        harness.get_by_label("Files…").click();
        wait_for_completed_job(&mut harness, "Ingest");

        let args = captured
            .lock()
            .expect("captured args")
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let inputs = args
            .windows(2)
            .filter(|pair| pair[0] == "--input")
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            inputs,
            vec![
                selected[0].display().to_string(),
                selected[1].display().to_string()
            ]
        );
        assert!(harness.query_by_label("Ingest sources panel").is_some());
        harness.get_by_label("Close ingest source panel").click();
        harness.run();
        assert!(harness.query_by_label("Ingest sources panel").is_none());
    }

    fn assert_ingest_checkpoint(
        harness: &Harness<'_, BibiiWikiApp>,
        stage: WikiIngestStage,
        expected_status: &str,
    ) {
        assert!(
            harness
                .query_by_label(&format!(
                    "Ingest step: {} — {expected_status}",
                    stage.label()
                ))
                .is_some(),
            "missing {expected_status} ingest checkpoint {}",
            stage.label()
        );
    }

    fn assert_completed_ingest_output(harness: &Harness<'_, BibiiWikiApp>) {
        let output = &harness
            .state()
            .jobs
            .iter()
            .find(|job| job.label == "Ingest")
            .expect("completed ingest job")
            .output;
        for stage in WikiIngestStage::ALL {
            assert_ingest_checkpoint(harness, stage, "done");
            assert!(
                output.contains(&format!(
                    "DONE    [{}/{}] {}",
                    stage.index() + 1,
                    WikiIngestStage::ALL.len(),
                    stage.label()
                )),
                "missing persisted output for {}",
                stage.label()
            );
        }
        assert!(output.contains("documents=1"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn ingest_checkpoints_advance_while_the_pipeline_is_still_running() {
        use std::sync::Barrier;

        let mut app = test_app();
        app.ingest_source_picker = Arc::new(|mode, _| {
            assert_eq!(mode, IngestPickerMode::Files);
            Ok(Some(vec![PathBuf::from(r"D:\sources\momentum.md")]))
        });
        let (reached_markdown_tx, reached_markdown_rx) = std::sync::mpsc::channel();
        let release_runner = Arc::new(Barrier::new(2));
        let runner_release = Arc::clone(&release_runner);
        app.job_runner = Arc::new(move |_, _, observe_stdout| {
            for progress in [
                WikiIngestProgress::stage(
                    WikiIngestStage::Initialize,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::stage(WikiIngestStage::Markdown, WikiIngestStageState::Running)
                    .with_units(1, 3),
            ] {
                observe_stdout(&progress.marker());
            }
            reached_markdown_tx
                .send(())
                .expect("test should still be waiting for Markdown progress");
            runner_release.wait();
            observe_stdout(
                &WikiIngestProgress::stage(
                    WikiIngestStage::Markdown,
                    WikiIngestStageState::Succeeded,
                )
                .with_units(3, 3)
                .marker(),
            );
            for progress in [
                WikiIngestProgress::stage(WikiIngestStage::Chunking, WikiIngestStageState::Running)
                    .with_units(0, 3)
                    .with_chunk_details(0, 10_000),
                WikiIngestProgress::stage(WikiIngestStage::Chunking, WikiIngestStageState::Running)
                    .with_units(3, 3)
                    .with_chunk_details(9, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Chunking,
                    WikiIngestStageState::Succeeded,
                )
                .with_units(3, 3)
                .with_chunk_details(9, 10_000),
                WikiIngestProgress::stage(WikiIngestStage::LlmCheck, WikiIngestStageState::Running),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Direct,
                    WikiIngestStageState::Running,
                ),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Direct,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Proxy,
                    WikiIngestStageState::Running,
                ),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Proxy,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Codex,
                    WikiIngestStageState::Running,
                ),
                WikiIngestProgress::llm_check(
                    WikiIngestLlmCheck::Codex,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::stage(
                    WikiIngestStage::LlmCheck,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::stage(
                    WikiIngestStage::Components,
                    WikiIngestStageState::Running,
                )
                .with_units(0, 9)
                .with_chunk_details(9, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Components,
                    WikiIngestStageState::Running,
                )
                .with_units(9, 9)
                .with_chunk_details(9, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Components,
                    WikiIngestStageState::Succeeded,
                )
                .with_units(9, 9)
                .with_chunk_details(9, 10_000),
            ] {
                observe_stdout(&progress.marker());
            }
            for stage in [
                WikiIngestStage::Concepts,
                WikiIngestStage::Entities,
                WikiIngestStage::Factors,
                WikiIngestStage::Methodologies,
                WikiIngestStage::Queries,
            ] {
                for state in [
                    WikiIngestStageState::Running,
                    WikiIngestStageState::Succeeded,
                ] {
                    observe_stdout(&WikiIngestProgress::stage(stage, state).marker());
                }
            }
            Ok((true, "documents=1".to_owned(), String::new()))
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Ingest")
            .next()
            .expect("toolbar ingest action")
            .click();
        harness.run();
        harness.get_by_label("Select files or directories…").click();
        harness.run();
        harness.get_by_label("Files…").click();
        harness.step();
        reached_markdown_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ingest runner should report Markdown progress");
        harness.step();

        assert_ingest_checkpoint(&harness, WikiIngestStage::Initialize, "done");
        assert_ingest_checkpoint(&harness, WikiIngestStage::Markdown, "running");
        assert_ingest_checkpoint(&harness, WikiIngestStage::Concepts, "pending");
        assert!(
            harness
                .query_by_label("Extract Markdown progress: 1/3 files")
                .is_some(),
            "file conversion progress bar should expose its count"
        );
        let live_output = &harness
            .state()
            .jobs
            .iter()
            .find(|job| job.label == "Ingest")
            .expect("running ingest job")
            .output;
        assert!(live_output.contains(&format!(
            "DONE    [1/{}] Initialize wiki directories",
            WikiIngestStage::ALL.len()
        )));
        assert!(live_output.contains(&format!(
            "RUNNING [2/{}] Extract Markdown (1/3 files)",
            WikiIngestStage::ALL.len()
        )));
        assert!(!live_output.contains("BIBIIWIKI_INGEST_STAGE"));
        if let Some(output) = std::env::var_os("BIBIIWIKI_INGEST_STEPS_QA_CAPTURE") {
            harness
                .render()
                .expect("render ingest checkpoint visual QA")
                .save(PathBuf::from(output))
                .expect("save ingest checkpoint visual QA");
        }

        release_runner.wait();
        wait_for_completed_job(&mut harness, "Ingest");
        assert_completed_ingest_output(&harness);
        let completed_output = &harness
            .state()
            .jobs
            .iter()
            .find(|job| job.label == "Ingest")
            .expect("completed ingest job")
            .output;
        assert!(completed_output.contains(&format!(
            "DONE    [3/{}] Chunk Markdown (3/3 files, 9 chunks, max 10000 chars)",
            WikiIngestStage::ALL.len()
        )));
        assert!(completed_output.contains(&format!(
            "DONE    [5/{}] Extract wiki components (9/9 chunks, max 10000 chars)",
            WikiIngestStage::ALL.len()
        )));
        for check in WikiIngestLlmCheck::ALL {
            assert!(completed_output.contains(&format!(
                "DONE    [{}/{}] {}",
                check.index() + 1,
                WikiIngestLlmCheck::ALL.len(),
                check.label()
            )));
        }
    }

    #[test]
    fn failed_ingest_status_exposes_continue_and_redo_actions() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ingest-status-ui-{unique}"));
        let source = root.join("source.md");
        fs::create_dir_all(root.join(".bibiiwiki")).expect("status directory");
        fs::write(&source, "# Source").expect("source fixture");
        let stages = WikiIngestStage::ALL
            .into_iter()
            .map(|stage| {
                let state = match stage {
                    WikiIngestStage::Initialize
                    | WikiIngestStage::Markdown
                    | WikiIngestStage::Chunking
                    | WikiIngestStage::LlmCheck => "succeeded",
                    WikiIngestStage::Components => "failed",
                    _ => "pending",
                };
                json!({
                    "stage": stage.key(),
                    "state": state,
                    "llm_checks": if stage == WikiIngestStage::LlmCheck {
                        WikiIngestLlmCheck::ALL.into_iter().map(|check| json!({
                            "check": check.key(),
                            "state": "succeeded"
                        })).collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    },
                    "completed": if stage == WikiIngestStage::Components { Some(4) } else { None },
                    "total": if stage == WikiIngestStage::Components { Some(12) } else { None },
                    "chunks": if stage == WikiIngestStage::Components { Some(12) } else { None },
                    "max_chunk_characters": if stage == WikiIngestStage::Components { Some(10000) } else { None }
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            root.join(".bibiiwiki/ingest-status.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "session_id": "test-session",
                "run_state": "failed",
                "source_paths": [source],
                "source_fingerprints": {},
                "configuration_fingerprint": "test-config",
                "stages": stages,
                "last_error": "provider unavailable",
                "updated_at": "2026-08-31T12:00:00+08:00"
            }))
            .expect("status JSON"),
        )
        .expect("status fixture");

        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root.clone());
        persisted.inspector = Inspector::Ingest;
        persisted.inspector_visible = true;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.ingest_source_panel_open = true;
        let (args_tx, args_rx) = std::sync::mpsc::channel();
        app.job_runner = Arc::new(move |args, _, _| {
            args_tx.send(args.to_vec()).expect("captured CLI arguments");
            Ok((false, String::new(), "still unavailable".to_owned()))
        });
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        assert!(harness.query_by_label("Continue ingest").is_some());
        assert!(harness.query_by_label("Redo ingest").is_some());
        assert!(
            harness
                .query_by_label("Extract wiki components progress: 4/12 chunks • max 10000 chars")
                .is_some()
        );
        harness.get_by_label("Continue ingest").click();
        harness.run();
        let args = args_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Continue should launch the CLI");
        assert!(args.iter().any(|argument| argument == "--resume"));
        assert!(!args.iter().any(|argument| argument == "--redo"));

        fs::remove_dir_all(root).expect("status fixture cleanup");
    }

    #[test]
    fn cancelled_or_failed_ingest_picker_does_not_start_a_job() {
        let mut cancelled_app = test_app();
        cancelled_app.ingest_source_picker = Arc::new(|mode, _| {
            assert_eq!(mode, IngestPickerMode::Directories);
            Ok(None)
        });
        let mut cancelled =
            Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), cancelled_app);
        cancelled.set_size(Vec2::new(1440.0, 900.0));
        cancelled.run();
        cancelled
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Ingest")
            .next()
            .expect("toolbar ingest action")
            .click();
        cancelled.run();
        cancelled
            .get_by_label("Select files or directories…")
            .click();
        cancelled.run();
        cancelled.get_by_label("Directories…").click();
        cancelled.run();
        assert!(cancelled.state().jobs.is_empty());
        assert_eq!(
            cancelled.state().status,
            "Ingest source selection cancelled"
        );

        let mut failed_app = test_app();
        failed_app.ingest_source_picker =
            Arc::new(|_, _| Err(anyhow!("simulated ingest picker failure")));
        let mut failed =
            Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), failed_app);
        failed.set_size(Vec2::new(1440.0, 900.0));
        failed.run();
        failed
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Ingest")
            .next()
            .expect("toolbar ingest action")
            .click();
        failed.run();
        failed.get_by_label("Select files or directories…").click();
        failed.run();
        failed.get_by_label("Files…").click();
        failed.run();
        assert!(failed.state().jobs.is_empty());
        assert!(
            failed
                .state()
                .status
                .contains("simulated ingest picker failure")
        );
    }

    #[test]
    fn workspace_removal_requires_explicit_confirmation() {
        let mut app = test_app();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let removable_name = format!("remove-me-{unique}");
        let removable = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(&removable_name);
        fs::create_dir_all(&removable).expect("removable workspace fixture");
        fs::write(removable.join("purpose.md"), "# Keep me\n").expect("workspace marker");
        app.persisted.ensure_workspace(removable.clone());
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &removable_name)
            .click_secondary();
        harness.run();
        harness.get_by_label("Remove from list").click();
        harness.run();

        assert_eq!(harness.state().persisted.workspaces.len(), 2);
        assert_eq!(
            harness.state().workspace_pending_removal.as_deref(),
            Some(removable.as_path())
        );
        assert!(harness.query_by_label("Remove workspace?").is_some());
        assert!(
            harness
                .query_by_label("The folder and all wiki files will remain on disk.")
                .is_some()
        );

        harness.get_by_label("Cancel").click();
        harness.run();
        assert_eq!(harness.state().persisted.workspaces.len(), 2);
        assert!(harness.state().workspace_pending_removal.is_none());

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &removable_name)
            .click_secondary();
        harness.run();
        harness.get_by_label("Remove from list").click();
        harness.run();
        harness.get_by_label("Remove workspace").click();
        harness.run();

        assert_eq!(harness.state().persisted.workspaces.len(), 1);
        assert!(harness.state().workspace_pending_removal.is_none());
        assert!(harness.state().status.contains("files were not deleted"));
        assert!(
            !harness
                .state()
                .persisted
                .workspaces
                .iter()
                .any(|workspace| { same_path(&workspace.root, &removable) })
        );
        assert!(removable.join("purpose.md").is_file());

        fs::remove_dir_all(removable).expect("workspace fixture cleanup");
    }

    #[test]
    fn final_workspace_can_be_removed_and_the_empty_state_can_add_one_back() {
        let mut app = test_app();
        let original = app
            .selected_workspace()
            .expect("initial workspace")
            .root
            .clone();
        let original_name = app
            .selected_workspace()
            .expect("initial workspace")
            .name
            .clone();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let replacement = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("replacement-wiki-{unique}"));
        fs::create_dir_all(&replacement).expect("replacement directory fixture");
        let picked = replacement.clone();
        app.directory_picker = Arc::new(move |_| Ok(Some(picked.clone())));
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &original_name)
            .click_secondary();
        harness.run();
        assert!(harness.query_by_label("Remove from list").is_some());
        harness.get_by_label("Remove from list").click();
        harness.run();
        harness.get_by_label("Remove workspace").click();
        harness.run();

        assert!(harness.state().persisted.workspaces.is_empty());
        assert!(harness.state().selected_workspace().is_none());
        assert!(
            harness
                .query_by_label("No wiki roots. Use Add wiki root to choose a folder.")
                .is_some()
        );
        assert!(original.is_dir(), "removal must not delete the wiki root");

        harness.get_by_label("Search").click();
        harness.run();
        assert!(harness.state().status.contains("No workspace selected"));

        harness.get_by_label("Add wiki root").click();
        harness.run();
        assert_eq!(harness.state().persisted.workspaces.len(), 1);
        assert!(same_path(
            &harness
                .state()
                .selected_workspace()
                .expect("replacement workspace")
                .root,
            &replacement
        ));

        fs::remove_dir_all(replacement).expect("replacement directory fixture cleanup");
    }

    #[test]
    fn prompts_dock_exposes_filter_source_and_file_actions() {
        let app = test_app();
        let first_prompt = app
            .prompt_names
            .first()
            .expect("embedded prompt library")
            .clone();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Prompts dock").click();
        harness.run();

        assert!(harness.query_by_label("Reload source").is_some());
        assert!(harness.query_by_label("Save source").is_some());
        assert!(!harness.state().prompt_names.is_empty());
        harness.get_by_label(&first_prompt).click();
        harness.run();
        assert!(harness.query_by_label("Jinja2 prompt editor").is_some());
        assert!(!harness.state().prompt_source.is_empty());
    }

    #[test]
    fn dock_restore_triangles_have_exact_compact_click_targets() {
        let mut app = test_app();
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.query_dock_visible = false;
        app.persisted.inspector_visible = true;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        for label in [
            "Restore workspace dock",
            "Restore search dock",
            "Restore AI query dock",
        ] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                (rect.width() - RESTORE_TRIANGLE_SIZE).abs() < 0.1,
                "{label} has unexpected width: {rect:?}"
            );
            assert!(
                (rect.height() - RESTORE_TRIANGLE_SIZE).abs() < 0.1,
                "{label} has unexpected height: {rect:?}"
            );
        }
    }

    #[test]
    fn each_dock_restore_triangle_has_its_own_color() {
        assert_ne!(WORKSPACE_DOCK_COLOR, SEARCH_DOCK_COLOR);
        assert_ne!(WORKSPACE_DOCK_COLOR, QUERY_DOCK_COLOR);
        assert_ne!(SEARCH_DOCK_COLOR, QUERY_DOCK_COLOR);
        assert_eq!(WORKSPACE_DOCK_COLOR, Color32::from_rgb(37, 99, 235));
        assert_eq!(SEARCH_DOCK_COLOR, Color32::from_rgb(234, 120, 32));
        assert_eq!(QUERY_DOCK_COLOR, Color32::from_rgb(34, 160, 99));
    }

    #[test]
    fn dock_restore_triangles_point_back_into_the_window() {
        assert_eq!(RESTORE_FROM_LEFT_GLYPH, "▶");
        assert_eq!(RESTORE_FROM_RIGHT_GLYPH, "◀");
    }

    #[test]
    fn left_dock_splitters_drag_left_to_make_both_panels_narrower() {
        fn drag_splitter(
            harness: &mut Harness<'_, BibiiWikiApp>,
            from: egui::Pos2,
            to: egui::Pos2,
            expected_dock: DockResize,
        ) {
            harness.input_mut().events.extend([
                egui::Event::PointerMoved(from),
                egui::Event::PointerButton {
                    pos: from,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            harness.step();
            assert_eq!(harness.state().active_dock_resize, Some(expected_dock));
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(to));
            harness.step();
            assert_eq!(harness.state().active_dock_resize, Some(expected_dock));
            harness.input_mut().events.push(egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            harness.run();
        }

        let mut app = test_app();
        app.persisted.query_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        assert!(
            (harness.ctx.style().interaction.resize_grab_radius_side - DOCK_RESIZE_GRAB_RADIUS)
                .abs()
                < f32::EPSILON
        );

        let workspace_before =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("workspace_dock"))
                .expect("workspace panel state")
                .rect;
        let workspace_handle = harness.get_by_label("Resize workspace dock").rect();
        let workspace_splitter = workspace_handle.center();
        drag_splitter(
            &mut harness,
            workspace_splitter,
            workspace_splitter - Vec2::new(60.0, 0.0),
            DockResize::Workspace,
        );
        let after_both =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("workspace_dock"))
                .expect("resized workspace panel state")
                .rect;
        assert!(
            after_both.width() < workspace_before.width() - 40.0,
            "workspace dock did not shrink left: before={workspace_before:?}, handle={workspace_handle:?}, saved_width={}, after={after_both:?}",
            harness.state().persisted.workspace_dock_width
        );

        let search_before =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("search_dock"))
                .expect("search panel state")
                .rect;
        let search_handle = harness.get_by_label("Resize search dock").rect();
        let search_splitter = search_handle.center();
        drag_splitter(
            &mut harness,
            search_splitter,
            search_splitter - Vec2::new(80.0, 0.0),
            DockResize::Search,
        );
        let search_after =
            egui::containers::panel::PanelState::load(&harness.ctx, Id::new("search_dock"))
                .expect("resized search panel state")
                .rect;
        assert!(
            search_after.width() < search_before.width() - 60.0,
            "search dock did not shrink left: before={search_before:?}, after={search_after:?}"
        );
    }

    #[test]
    fn every_inspector_rail_button_temporarily_replaces_the_center_dock() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();
        assert!(
            harness
                .query_by_label("Markdown Preview / Editor")
                .is_some()
        );

        for (label, inspector) in [
            ("Ingest dock", Inspector::Ingest),
            ("Lint dock", Inspector::Lint),
            ("Update dock", Inspector::Update),
            ("LLM dock", Inspector::Llm),
            ("Tools dock", Inspector::Tools),
            ("Prompts dock", Inspector::Prompts),
        ] {
            harness.get_by_label(label).click();
            harness.run();
            assert!(harness.state().persisted.inspector_visible);
            assert_eq!(harness.state().persisted.inspector, inspector);
            assert!(
                harness
                    .query_by_label("Markdown Preview / Editor")
                    .is_none()
            );
            assert!(harness.query_by_label("AI QUERY").is_some());

            harness.get_by_label(label).click();
            harness.run();
            assert!(!harness.state().persisted.inspector_visible);
            assert!(
                harness
                    .query_by_label("Markdown Preview / Editor")
                    .is_some()
            );
        }
    }

    #[test]
    fn activity_rail_uses_font_awesome_icons_and_compact_square_targets() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        for label in [
            "Workspace dock",
            "Search dock",
            "AI Query dock",
            "Ingest dock",
            "Lint dock",
            "Update dock",
            "LLM dock",
            "Tools dock",
            "Prompts dock",
        ] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                (rect.width() - ACTIVITY_BUTTON_SIZE).abs() < 0.1,
                "{label} has unexpected width: {rect:?}"
            );
            assert!(
                (rect.height() - ACTIVITY_BUTTON_SIZE).abs() < 0.1,
                "{label} has unexpected height: {rect:?}"
            );
        }
    }

    #[test]
    fn ai_query_rail_button_toggles_an_independent_side_dock() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Tools dock").click();
        harness.run();
        assert!(harness.state().persisted.inspector_visible);
        assert!(harness.state().persisted.query_dock_visible);

        harness.get_by_label("AI Query dock").click();
        harness.run();
        assert!(harness.state().persisted.inspector_visible);
        assert!(!harness.state().persisted.query_dock_visible);
        assert!(harness.query_by_label("Restore AI query dock").is_some());

        harness.get_by_label("AI Query dock").click();
        harness.run();
        assert!(harness.state().persisted.inspector_visible);
        assert!(harness.state().persisted.query_dock_visible);
        assert!(harness.query_by_label("AI QUERY").is_some());
    }

    #[test]
    fn inspector_close_button_restores_the_previous_center_view() {
        let app = test_app();
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Tools dock").click();
        harness.run();
        harness
            .get_by_label("Close Tool configuration dock")
            .click();
        harness.run();

        assert!(!harness.state().persisted.inspector_visible);
        assert!(
            harness
                .query_by_label("Markdown Preview / Editor")
                .is_some()
        );
        assert!(harness.query_by_label("AI QUERY").is_some());
    }

    #[test]
    fn workspace_tree_discovers_nested_markdown_and_skips_binary_files() {
        let root = public_wiki_fixture();
        let tree = build_workspace_file_tree(&root);

        assert!(tree.error.is_none(), "{:?}", tree.error);
        assert!(tree_contains(&tree.nodes, "purpose.md"));
        assert!(tree_contains(
            &tree.nodes,
            "Formulaic Alphas_公式化阿尔法.md"
        ));
        assert!(!tree_contains(&tree.nodes, "source.bin"));
    }

    #[test]
    fn workspace_tree_paint_is_clipped_above_the_activity_output_dock() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.toolbar_visible = false;
        persisted.search_dock_visible = false;
        persisted.query_dock_visible = false;
        persisted.bottom_dock_visible = true;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(800.0, 760.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "wiki")
            .click();
        harness.run();
        for directory in [
            "concepts",
            "entities",
            "factors",
            "media",
            "methodology",
            "mining",
            "opportunities",
            "queries",
            "sources",
        ] {
            if let Some(row) =
                harness.query_by_role_and_label(egui::accesskit::Role::Button, directory)
            {
                row.click();
                harness.run();
            }
        }

        let output_header_y = harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == "ACTIVITY & OUTPUT" => {
                    Some(text.pos.y)
                }
                _ => None,
            })
            .expect("activity/output header text should be painted");
        let workspace_rows = harness
            .output()
            .shapes
            .iter()
            .filter(|shape| {
                matches!(
                    &shape.shape,
                    egui::Shape::Text(text)
                        if text.galley.job.text == "purpose.md" || text.galley.job.text == "schema.md"
                )
            })
            .collect::<Vec<_>>();
        assert!(
            harness.output().shapes.iter().any(|shape| matches!(
                &shape.shape,
                egui::Shape::Text(text) if text.galley.job.text == "wiki"
            )),
            "expected the visible workspace tree to be painted"
        );
        for row in workspace_rows {
            assert!(
                row.clip_rect.max.y <= output_header_y,
                "workspace row clip {:?} crosses Activity & Output at y={output_header_y}",
                row.clip_rect
            );
        }
        if let Some(output) = std::env::var_os("BIBIIWIKI_DOCK_CLIP_QA_CAPTURE") {
            harness
                .render()
                .expect("egui dock-clipping visual-QA render")
                .save(PathBuf::from(output))
                .expect("save egui dock-clipping visual-QA capture");
        }
    }

    #[test]
    fn clicking_a_workspace_file_opens_the_markdown_editor() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        harness.get_by_label("Tools dock").click();
        harness.run();
        assert!(harness.state().persisted.inspector_visible);
        assert!(
            harness
                .query_by_label("Close Tool configuration dock")
                .is_some()
        );

        let purpose = harness.get_by_role_and_label(egui::accesskit::Role::Button, "purpose.md");
        purpose.click();
        harness.run();

        let app = harness.state();
        assert!(!app.persisted.inspector_visible);
        let document = app.open_document.as_ref().expect("document should be open");
        assert_eq!(
            document.path.file_name().and_then(|name| name.to_str()),
            Some("purpose.md")
        );
        assert_eq!(document.editor.mode(), EditorMode::Split);
        assert!(document.editor.markdown().contains("llm_wiki"));
        assert!(
            harness
                .query_by_label("Close Tool configuration dock")
                .is_none()
        );
        assert!(harness.query_all_by_label("Source").count() > 0);
    }

    #[test]
    fn markdown_editor_mode_follows_the_user_to_the_next_opened_file() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.search_dock_visible = false;
        persisted.query_dock_visible = false;
        persisted.bottom_dock_visible = false;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_200.0, 800.0));
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "purpose.md")
            .click();
        harness.run();
        assert_eq!(
            harness
                .state()
                .open_document
                .as_ref()
                .expect("purpose.md should be open")
                .editor
                .mode(),
            EditorMode::Split
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Preview")
            .click();
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "schema.md")
            .click();
        harness.run();

        let document = harness
            .state()
            .open_document
            .as_ref()
            .expect("schema.md should be open");
        assert_eq!(
            document.path.file_name().and_then(|name| name.to_str()),
            Some("schema.md")
        );
        assert_eq!(document.editor.mode(), EditorMode::Preview);
    }

    #[test]
    fn long_markdown_document_header_keeps_actions_inside_a_narrow_center_dock() {
        let root = public_wiki_fixture();
        let path = root.join("wiki").join("sources").join(
            "Long Markdown Source Name for Dock Header Layout_用于验证长文件标题在狭窄面板中仍可使用.md",
        );
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root.clone());
        persisted.bottom_dock_visible = false;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.open_markdown_file(root, path);
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(900.0, 600.0));
        harness.run();

        for label in ["Save", "AI Query", "Close"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.left() >= 0.0 && rect.right() <= 900.0,
                "{label} must remain within the application viewport, got {rect:?}"
            );
        }
    }

    #[test]
    fn search_scans_root_level_markdown_in_the_selected_workspace() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );

        app.search_query = "purpose".to_owned();
        app.run_search();

        assert!(
            app.search_hits.iter().any(|hit| hit.path == "purpose.md"),
            "the selected wiki root's purpose.md must be searchable"
        );
        assert!(
            app.status.starts_with(&format!(
                "Search returned {} result(s) via ",
                app.search_hits.len()
            )),
            "search status should identify the active backend: {}",
            app.status
        );
    }

    #[test]
    fn double_clicking_a_search_result_brings_its_markdown_editor_to_front() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.inspector_visible = true;
        persisted.inspector = Inspector::Tools;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.search_query = "momentum".to_owned();
        app.run_search();
        assert!(!app.search_hits.is_empty());
        let result_title = app.search_hits[0].title.clone();
        app.note_opener = Arc::new(|_, _| Ok(()));

        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_600.0, 960.0))
            .with_step_dt(1.0 / 60.0)
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.run();

        let result = harness.get_by_role_and_label(egui::accesskit::Role::Button, &result_title);
        result.click();
        harness.run();
        assert!(!harness.state().search_preview.markdown().is_empty());
        assert_eq!(
            harness.state().search_preview.markdown(),
            harness.state().query_evidence.markdown()
        );
        harness
            .state_mut()
            .query_evidence
            .set_mode(EditorMode::Source);
        assert_eq!(harness.state().search_preview.mode(), EditorMode::Preview);
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &result_title)
            .click();
        harness.run_steps(2);

        let app = harness.state();
        assert!(!app.persisted.inspector_visible);
        let document = app.open_document.as_ref().expect("search result note open");
        assert_eq!(
            document.path.file_name().and_then(|name| name.to_str()),
            Path::new(&app.search_hits[0].path)
                .file_name()
                .and_then(|name| name.to_str())
        );
        assert!(app.search_preview.markdown().is_empty());
        assert!(app.query_evidence.markdown().is_empty());
        assert!(!document.editor.markdown().is_empty());
        assert!(harness.query_all_by_label("Source").count() > 0);
    }

    #[test]
    #[ignore = "generates public README screenshots from examples/demo-wiki"]
    fn capture_public_readme_screenshots() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("demo-wiki");
        let output = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("screenshots");
        fs::create_dir_all(&output).expect("create screenshot output directory");

        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root.clone());
        persisted.bottom_dock_visible = false;
        persisted.query_dock_visible = false;
        persisted.markdown_editor_mode = EditorMode::Preview;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.search_query = "momentum".to_owned();
        app.run_search();
        app.open_markdown_file(root.clone(), root.join("wiki/factors/momentum.md"));
        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_600.0, 900.0))
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(2);
        harness
            .render()
            .expect("render public workspace screenshot")
            .save(output.join("workspace.png"))
            .expect("save public workspace screenshot");

        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.bottom_dock_visible = false;
        persisted.search_dock_visible = false;
        persisted.query_dock_visible = false;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_200.0, 760.0))
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();
        harness.get_by_label("Ingest dock").click();
        harness.run_steps(2);
        harness
            .render()
            .expect("render public ingest screenshot")
            .save(output.join("ingest.png"))
            .expect("save public ingest screenshot");
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_SEARCH_EDITOR_QA_CAPTURE"]
    fn capture_search_result_markdown_editor_for_visual_qa() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("python_wiki_root");
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.bottom_dock_visible = false;
        persisted.inspector_visible = true;
        persisted.inspector = Inspector::Tools;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.search_query = "momentum".to_owned();
        app.run_search();
        let result_title = app.search_hits[0].title.clone();

        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_600.0, 960.0))
            .with_step_dt(1.0 / 60.0)
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &result_title)
            .click();
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, &result_title)
            .click();
        harness.run_steps(2);

        assert!(harness.state().open_document.is_some());
        let output = std::env::var_os("BIBIIWIKI_SEARCH_EDITOR_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_SEARCH_EDITOR_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui search-result editor visual-QA render")
            .save(&output)
            .expect("save egui search-result editor visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes light/dark search-tile captures requested by BIBIIWIKI_SEARCH_THEME_QA_DIR"]
    fn capture_search_result_tiles_in_light_and_dark_themes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("python_wiki_root");
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.bottom_dock_visible = false;
        persisted.query_dock_visible = false;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        app.search_query = "momentum".to_owned();
        app.run_search();
        app.selected_hit = Some(0);
        let output = std::env::var_os("BIBIIWIKI_SEARCH_THEME_QA_DIR")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_SEARCH_THEME_QA_DIR to an output directory");
        fs::create_dir_all(&output).expect("create search-theme QA directory");

        let mut harness = Harness::builder()
            .with_size(Vec2::new(1_200.0, 820.0))
            .with_step_dt(1.0 / 60.0)
            .build_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(2);
        harness
            .render()
            .expect("light search-theme render")
            .save(output.join("search-results-light.png"))
            .expect("save light search-theme capture");

        harness.state_mut().persisted.theme = Theme::Dark;
        harness.run_steps(2);
        harness
            .render()
            .expect("dark search-theme render")
            .save(output.join("search-results-dark.png"))
            .expect("save dark search-theme capture");

        assert!(output.join("search-results-light.png").is_file());
        assert!(output.join("search-results-dark.png").is_file());
    }

    #[test]
    fn workspace_explorer_rows_show_icons_and_expand_directories() {
        let root = public_wiki_fixture();
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1440.0, 900.0));
        harness.run();

        assert_eq!(FaIcon::FolderOpen.label(), "Open folder");
        assert_eq!(FaIcon::File.label(), "File");

        let wiki_row = harness.get_by_role_and_label(egui::accesskit::Role::Button, "wiki");
        assert!(
            wiki_row.rect().height() <= EXPLORER_ROW_HEIGHT + 0.1,
            "workspace row was {:?}",
            wiki_row.rect()
        );
        wiki_row.click();
        harness.run();

        let backtest = harness
            .query_by_role_and_label(egui::accesskit::Role::Button, "backtest")
            .expect("expanded directory row");
        assert!(backtest.rect().height() <= EXPLORER_ROW_HEIGHT + 0.1);
        backtest.click();
        harness.run_steps(2);
        let first_backtest = harness.get_by_role_and_label(
            egui::accesskit::Role::Button,
            "bt_1-Month Amplitude-Adjusted Momentum.md",
        );
        let second_backtest = harness.get_by_role_and_label(
            egui::accesskit::Role::Button,
            "bt_3-Month Intraday Amplitude Volatility.md",
        );
        assert!(
            (first_backtest.rect().left() - second_backtest.rect().left()).abs() < 0.1,
            "sibling file rows must stay left-aligned"
        );

        let purpose = harness.get_by_role_and_label(egui::accesskit::Role::Button, "purpose.md");
        let schema = harness.get_by_role_and_label(egui::accesskit::Role::Button, "schema.md");
        assert!(
            schema.rect().top() - purpose.rect().top()
                <= EXPLORER_ROW_HEIGHT + EXPLORER_ROW_GAP + 0.1,
            "sibling explorer rows should use compact vertical rhythm"
        );
    }

    #[test]
    fn workspace_explorer_uses_stateful_folder_icons_and_workspace_palette() {
        assert_eq!(explorer_folder_icon(false), FaIcon::Folder);
        assert_eq!(explorer_folder_icon(true), FaIcon::FolderOpen);
        assert_eq!(
            explorer_directory_color(false),
            Color32::from_rgb(37, 99, 235)
        );
        assert_eq!(
            explorer_directory_color(true),
            Color32::from_rgb(96, 165, 250)
        );
        assert_ne!(
            explorer_connector_color(false),
            explorer_directory_color(false),
            "connector lines should remain visually quieter than directory labels"
        );
        assert_ne!(
            explorer_selected_workspace_color(false),
            explorer_directory_color(false),
            "the current workspace needs a distinct light-theme green"
        );
        assert_eq!(explorer_inactive_workspace_color(false), Color32::BLACK);
        assert_eq!(
            explorer_selected_workspace_color(false),
            Color32::from_rgb(22, 163, 74)
        );
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_EDITOR_FOREGROUND_QA_CAPTURE"]
    fn capture_markdown_editor_brought_to_front_for_visual_qa() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("python_wiki_root");
        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(root);
        persisted.bottom_dock_visible = false;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1400.0, 800.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();

        harness.get_by_label("Tools dock").click();
        harness.run();
        assert!(
            harness
                .query_by_label("Close Tool configuration dock")
                .is_some()
        );
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "purpose.md")
            .click();
        harness.run_steps(2);

        assert!(!harness.state().persisted.inspector_visible);
        assert!(harness.state().open_document.is_some());
        assert!(
            harness
                .query_by_label("Close Tool configuration dock")
                .is_none()
        );
        assert!(harness.query_all_by_label("Source").count() > 0);
        let output = std::env::var_os("BIBIIWIKI_EDITOR_FOREGROUND_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_EDITOR_FOREGROUND_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui editor-foreground visual-QA render")
            .save(&output)
            .expect("save egui editor-foreground visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_QA_CAPTURE"]
    fn capture_compact_workspace_tree_for_visual_qa() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("python_wiki_root");
        let mut persisted = PersistedState::default();
        persisted
            .ensure_workspace(Path::new(env!("CARGO_MANIFEST_DIR")).join("000_inactive_wiki_root"));
        persisted.ensure_workspace(root);
        persisted.toolbar_visible = false;
        persisted.search_dock_visible = false;
        persisted.bottom_dock_visible = false;
        let app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(520.0, 820.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "wiki")
            .click();
        harness.run_steps(2);
        for directory in [
            "backtest",
            "concepts",
            "factors",
            "media",
            "methodology",
            "mining",
            "opportunities",
            "queries",
            "sources",
        ] {
            if let Some(row) =
                harness.query_by_role_and_label(egui::accesskit::Role::Button, directory)
            {
                row.click();
                harness.run_steps(2);
            }
        }
        let output = std::env::var_os("BIBIIWIKI_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui visual-QA render")
            .save(&output)
            .expect("save egui visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_INGEST_QA_CAPTURE"]
    fn capture_inline_ingest_panel_for_visual_qa() {
        let mut app = test_app();
        app.persisted.bottom_dock_visible = false;
        app.persisted.search_dock_visible = false;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1100.0, 720.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run();

        harness.get_by_label("Ingest dock").click();
        harness.run_steps(2);

        assert!(harness.query_by_label("Select ingest sources").is_none());
        assert!(harness.query_by_label("Ingest sources panel").is_some());
        let output = std::env::var_os("BIBIIWIKI_INGEST_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_INGEST_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui inline-ingest visual-QA render")
            .save(&output)
            .expect("save egui inline-ingest visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_LLM_PROGRESS_QA_CAPTURE"]
    fn capture_llm_connection_progress_for_visual_qa() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        app.llm_diagnostic.phase = LlmDiagnosticPhase::TestingModel;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1100.0, 820.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(3);

        assert!(harness.query_by_label("Testing LLM connection").is_some());
        assert!(
            harness
                .query_by_role_and_label(
                    egui::accesskit::Role::ProgressIndicator,
                    "Testing configured LLM progress",
                )
                .is_some()
        );
        let output = std::env::var_os("BIBIIWIKI_LLM_PROGRESS_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_LLM_PROGRESS_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui LLM-progress visual-QA render")
            .save(&output)
            .expect("save egui LLM-progress visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_LLM_PLACEHOLDER_QA_CAPTURE"]
    fn capture_llm_connection_placeholders_for_visual_qa() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1100.0, 820.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(3);

        assert!(harness.query_by_label("Testing LLM connection").is_some());
        assert!(harness.query_by_label("Configured LLM works").is_some());
        assert!(harness.query_by_label("LLM proxy works").is_some());
        assert!(harness.query_by_label("Codex adapter works").is_some());
        let configured_box = harness
            .get_by_label("Configured LLM reply box (empty)")
            .rect();
        let proxy_box = harness
            .get_by_label("Responses proxy reply box (empty)")
            .rect();
        let codex_box = harness
            .get_by_label("Codex adapter reply box (empty)")
            .rect();
        assert!(configured_box.height() >= 44.0);
        assert!(proxy_box.height() >= 44.0);
        assert!(codex_box.height() >= 44.0);
        assert!(proxy_box.top() > configured_box.bottom());
        assert!(codex_box.top() > proxy_box.bottom());

        let output = std::env::var_os("BIBIIWIKI_LLM_PLACEHOLDER_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_LLM_PLACEHOLDER_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui LLM-placeholder visual-QA render")
            .save(&output)
            .expect("save egui LLM-placeholder visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the focused visual-QA capture requested by BIBIIWIKI_LLM_WIZARD_QA_CAPTURE"]
    fn capture_litellm_configuration_wizard_for_visual_qa() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_500.0, 1_100.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(3);

        assert!(harness.state().llm_wizard.api_key_env.is_empty());
        assert!(
            harness
                .get_by_label("Current LLM configuration")
                .rect()
                .top()
                < harness
                    .get_by_label("LiteLLM YAML configuration")
                    .rect()
                    .top()
        );
        let output = std::env::var_os("BIBIIWIKI_LLM_WIZARD_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_LLM_WIZARD_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui LiteLLM-wizard visual-QA render")
            .save(&output)
            .expect("save egui LiteLLM-wizard visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "writes the vertical-scroll visual-QA capture requested by BIBIIWIKI_LLM_SCROLL_QA_CAPTURE"]
    fn capture_scrollable_litellm_configuration_panel_for_visual_qa() {
        let mut app = test_app();
        app.persisted.toolbar_visible = false;
        app.persisted.workspace_dock_visible = false;
        app.persisted.search_dock_visible = false;
        app.persisted.query_dock_visible = false;
        app.persisted.bottom_dock_visible = false;
        app.persisted.inspector_visible = true;
        app.persisted.inspector = Inspector::Llm;
        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1_500.0, 780.0));
        egui_extras::install_image_loaders(&harness.ctx);
        harness.run_steps(3);

        let pointer = harness
            .get_by_label("LLM configuration vertical scroll viewport")
            .rect()
            .center();
        for _ in 0..5 {
            harness.input_mut().events.extend([
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: Vec2::new(0.0, -8.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            harness.step();
        }
        assert!(
            harness
                .get_by_label("Current LLM configuration")
                .rect()
                .top()
                < 0.0,
            "the capture must show the LLM panel after a real wheel scroll"
        );

        let output = std::env::var_os("BIBIIWIKI_LLM_SCROLL_QA_CAPTURE")
            .map(PathBuf::from)
            .expect("set BIBIIWIKI_LLM_SCROLL_QA_CAPTURE to an output PNG path");
        harness
            .render()
            .expect("egui scrollable LiteLLM-panel visual-QA render")
            .save(&output)
            .expect("save egui scrollable LiteLLM-panel visual-QA render");
        assert!(output.is_file());
    }

    #[test]
    #[ignore = "requires local source documents and the Python golden wiki fixture"]
    #[allow(clippy::too_many_lines)]
    fn sequential_gui_acceptance_auto_initializes_during_ingest_then_searches_and_queries() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let audit_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("egui-acceptance-{unique}"))
            .join("wiki_root");
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("wiki_sources");
        let golden_factor = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("python_wiki_root")
            .join("wiki")
            .join("factors")
            .join("3-Month Momentum Excluding Limit-Up Days_3个月去涨停动量.md");

        let mut persisted = PersistedState::default();
        persisted.ensure_workspace(audit_root.clone());
        persisted.source_dir.clone_from(&source_dir);
        persisted.agent_workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        persisted.query_uses_llm = true;
        persisted.save_query_memory = false;
        let mut app = BibiiWikiApp::from_state(
            persisted,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"),
        );

        let steps = Arc::new(Mutex::new(Vec::<String>::new()));
        let runner_steps = Arc::clone(&steps);
        let runner_root = audit_root.clone();
        let runner_sources = source_dir.clone();
        let runner_factor = golden_factor.clone();
        app.job_runner = Arc::new(move |args, _working_directory, _observe_stdout| {
            let args = args
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>();
            if args.iter().any(|value| value == "ingest") {
                crate::wiki::WikiProject::new(runner_root.clone())?.ensure_initialized()?;
                let raw = runner_root.join("raw").join("sources");
                fs::create_dir_all(&raw)?;
                let mut copied = 0;
                for entry in fs::read_dir(&runner_sources)? {
                    let entry = entry?;
                    if entry.path().extension().and_then(|value| value.to_str()) == Some("pdf") {
                        fs::copy(entry.path(), raw.join(entry.file_name()))?;
                        copied += 1;
                    }
                }
                let factors = runner_root.join("wiki").join("factors");
                fs::create_dir_all(&factors)?;
                fs::copy(
                    &runner_factor,
                    factors.join(runner_factor.file_name().expect("factor name")),
                )?;
                runner_steps
                    .lock()
                    .expect("steps")
                    .push(format!("ingest:{copied}"));
                return Ok((
                    true,
                    format!("documents={copied} sources_copied={copied} factors=1"),
                    String::new(),
                ));
            }
            if args.iter().any(|value| value == "query") {
                runner_steps.lock().expect("steps").push("query".to_owned());
                return Ok((
                    true,
                    "## Momentum expression\n\n`col(\"close\").pct_change(60).over(\"asset_code\")`"
                        .to_owned(),
                    String::new(),
                ));
            }
            Ok((false, String::new(), "unexpected task".to_owned()))
        });
        let picker_sources = source_dir.clone();
        app.ingest_source_picker = Arc::new(move |mode, _| {
            assert_eq!(mode, IngestPickerMode::Directories);
            Ok(Some(vec![picker_sources.clone()]))
        });
        let opener_steps = Arc::clone(&steps);
        app.workspace_opener = Arc::new(move |root| {
            opener_steps
                .lock()
                .expect("steps")
                .push(format!("obsidian:{}", root.display()));
            Ok(())
        });

        let mut harness = Harness::new_state(|ctx, app: &mut BibiiWikiApp| app.render(ctx), app);
        harness.set_size(Vec2::new(1600.0, 960.0));
        harness.run();

        harness.get_by_label("LLM dock").click();
        harness.run();
        assert!(harness.state().config_text.contains("ornith-1.5:9b"));
        assert!(
            harness
                .query_by_label("YAML configuration editor")
                .is_some()
        );

        harness.get_by_label("Ingest dock").click();
        harness.run();
        assert!(!audit_root.join("purpose.md").exists());
        assert_eq!(harness.query_all_by_label("Initialize").count(), 0);

        assert!(harness.query_by_label("Select ingest sources").is_none());
        assert!(harness.query_by_label("Ingest sources panel").is_some());
        harness.get_by_label("Select files or directories…").click();
        harness.run();
        harness.get_by_label("Directories…").click();
        wait_for_completed_job(&mut harness, "Ingest");
        assert!(audit_root.join("purpose.md").is_file());
        assert!(audit_root.join("schema.md").is_file());
        assert!(audit_root.join("wiki/index.md").is_file());
        assert_eq!(
            fs::read_dir(audit_root.join("raw").join("sources"))
                .expect("raw sources")
                .count(),
            3
        );

        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "wiki_root")
            .click_secondary();
        harness.run();
        harness.get_by_label("Open in Obsidian").click();
        harness.run();

        harness.get_by_label("Ingest dock").click();
        harness.run();
        let search =
            harness.get_by_role_and_label(egui::accesskit::Role::TextInput, "Wiki search input");
        search.focus();
        search.type_text("momentum factor");
        harness.run();
        let search_button = harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Search")
            .find(|node| node.rect().left() > ACTIVITY_RAIL_WIDTH)
            .expect("search action");
        search_button.click();
        harness.run();
        assert!(!harness.state().search_hits.is_empty());

        let question = harness.get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "ai-question Markdown source",
        );
        question.focus();
        question.type_text("What is the expression of the momentum factor?");
        harness.run();
        let ask = harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "Ask AI")
            .next()
            .expect("enabled Ask AI action");
        ask.click();
        wait_for_completed_job(&mut harness, "AI query");

        assert!(harness.state().answer.markdown().contains("pct_change(60)"));
        let expected_steps = vec![
            "ingest:3".to_owned(),
            format!("obsidian:{}", audit_root.display()),
            "query".to_owned(),
        ];
        assert_eq!(*steps.lock().expect("steps"), expected_steps);
    }

    fn wait_for_completed_job(harness: &mut Harness<'_, BibiiWikiApp>, label: &str) {
        for _ in 0..200 {
            harness.step();
            if harness
                .state()
                .jobs
                .iter()
                .any(|job| job.label == label && job.status != JobStatus::Running)
            {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("{label} did not complete through the GUI");
    }

    fn tree_contains(nodes: &[FileTreeNode], file_name: &str) -> bool {
        nodes
            .iter()
            .any(|node| node.name == file_name || tree_contains(&node.children, file_name))
    }
}
