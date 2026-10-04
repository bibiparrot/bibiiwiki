use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::llm_preflight::{probe_codex_agent, probe_direct_llm, probe_responses_proxy};

use super::agent::{
    SourceAnalysis, SourceAnalysisCacheMode, SourceAnalysisRequest, SourceChunkProgress,
    SourceFactor,
};
use super::clock::today_beijing;
use super::{
    Authority, BilingualName, CodexWikiAgent, FactorDefinition, RawSourceBackup, TemplateLibrary,
    WikiMaintainer, WikiPage, WikiProject, WikiStore, parse_page,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiIngestStage {
    Initialize,
    Markdown,
    Chunking,
    LlmCheck,
    Components,
    Concepts,
    Entities,
    Factors,
    Methodologies,
    Queries,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiIngestLlmCheck {
    Direct,
    Proxy,
    Codex,
}

impl WikiIngestLlmCheck {
    pub const ALL: [Self; 3] = [Self::Direct, Self::Proxy, Self::Codex];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Direct => "Original LLM",
            Self::Proxy => "LiteLLM proxy",
            Self::Codex => "Codex agent",
        }
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
            Self::Codex => "codex",
        }
    }

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    fn from_key(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|check| check.key() == value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiIngestStageState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiIngestRunState {
    Running,
    Failed,
    Succeeded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WikiIngestCheckpointState {
    Pending,
    Running,
    Failed,
    Succeeded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikiIngestCheckpoint {
    pub stage: WikiIngestStage,
    pub state: WikiIngestCheckpointState,
    #[serde(default)]
    pub llm_checks: Vec<WikiIngestLlmCheckpoint>,
    pub completed: Option<usize>,
    pub total: Option<usize>,
    pub chunks: Option<usize>,
    pub max_chunk_characters: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikiIngestLlmCheckpoint {
    pub check: WikiIngestLlmCheck,
    pub state: WikiIngestCheckpointState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikiIngestStatus {
    version: u32,
    session_id: String,
    run_state: WikiIngestRunState,
    source_paths: Vec<PathBuf>,
    source_fingerprints: BTreeMap<PathBuf, String>,
    configuration_fingerprint: String,
    stages: Vec<WikiIngestCheckpoint>,
    last_error: Option<String>,
    updated_at: String,
}

impl WikiIngestStatus {
    const VERSION: u32 = 1;
    const RELATIVE_PATH: &'static str = ".bibiiwiki/ingest-status.json";

    fn new(source_paths: Vec<PathBuf>, configuration_fingerprint: String) -> Self {
        let session_seed = format!(
            "{}\0{}\0{:?}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            source_paths
        );
        let session_digest = format!("{:x}", Sha256::digest(session_seed.as_bytes()));
        Self {
            version: Self::VERSION,
            session_id: session_digest[..20].to_owned(),
            run_state: WikiIngestRunState::Running,
            source_paths,
            source_fingerprints: BTreeMap::new(),
            configuration_fingerprint,
            stages: WikiIngestStage::ALL
                .into_iter()
                .map(|stage| WikiIngestCheckpoint {
                    stage,
                    state: WikiIngestCheckpointState::Pending,
                    llm_checks: llm_checkpoints(WikiIngestCheckpointState::Pending, stage),
                    completed: None,
                    total: None,
                    chunks: None,
                    max_chunk_characters: None,
                })
                .collect(),
            last_error: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    #[must_use]
    pub const fn run_state(&self) -> WikiIngestRunState {
        self.run_state
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub const fn can_resume(&self) -> bool {
        matches!(
            self.run_state,
            WikiIngestRunState::Running | WikiIngestRunState::Failed
        )
    }

    #[must_use]
    pub fn failed_stage(&self) -> Option<WikiIngestStage> {
        self.stages
            .iter()
            .find(|progress| progress.state == WikiIngestCheckpointState::Failed)
            .map(|progress| progress.stage)
    }

    #[must_use]
    pub fn source_paths(&self) -> &[PathBuf] {
        &self.source_paths
    }

    #[must_use]
    pub fn checkpoints(&self) -> &[WikiIngestCheckpoint] {
        &self.stages
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    /// Loads the latest durable ingest status for a wiki root.
    ///
    /// # Errors
    ///
    /// Returns an error when the status exists but cannot be read or validated.
    pub fn load(wiki_root: &Path) -> Result<Option<Self>> {
        let path = wiki_root.join(Self::RELATIVE_PATH);
        if !path.is_file() {
            return Ok(None);
        }
        let mut status: Self = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
        )
        .with_context(|| format!("invalid ingest status {}", path.display()))?;
        if status.version != Self::VERSION {
            bail!(
                "unsupported ingest status version {} in {}",
                status.version,
                path.display()
            );
        }
        status.normalize_checkpoints();
        Ok(Some(status))
    }

    fn normalize_checkpoints(&mut self) {
        let default_state = if self.run_state == WikiIngestRunState::Succeeded {
            WikiIngestCheckpointState::Succeeded
        } else {
            WikiIngestCheckpointState::Pending
        };
        for stage in WikiIngestStage::ALL {
            if !self
                .stages
                .iter()
                .any(|checkpoint| checkpoint.stage == stage)
            {
                self.stages.push(WikiIngestCheckpoint {
                    stage,
                    state: default_state,
                    llm_checks: llm_checkpoints(default_state, stage),
                    completed: None,
                    total: None,
                    chunks: None,
                    max_chunk_characters: None,
                });
            }
        }
        if let Some(checkpoint) = self
            .stages
            .iter_mut()
            .find(|checkpoint| checkpoint.stage == WikiIngestStage::LlmCheck)
            && checkpoint.llm_checks.is_empty()
        {
            checkpoint.llm_checks = llm_checkpoints(default_state, WikiIngestStage::LlmCheck);
        }
        self.stages
            .sort_by_key(|checkpoint| checkpoint.stage.index());
    }

    fn apply(&mut self, progress: WikiIngestProgress) {
        self.run_state = WikiIngestRunState::Running;
        self.last_error = None;
        if let Some(stage) = self
            .stages
            .iter_mut()
            .find(|stage| stage.stage == progress.stage)
        {
            let checkpoint_state = match progress.state {
                WikiIngestStageState::Running => WikiIngestCheckpointState::Running,
                WikiIngestStageState::Succeeded => WikiIngestCheckpointState::Succeeded,
                WikiIngestStageState::Failed => WikiIngestCheckpointState::Failed,
            };
            if let Some(check) = progress.llm_check {
                stage.state = if checkpoint_state == WikiIngestCheckpointState::Failed {
                    WikiIngestCheckpointState::Failed
                } else {
                    WikiIngestCheckpointState::Running
                };
                if let Some(substep) = stage
                    .llm_checks
                    .iter_mut()
                    .find(|substep| substep.check == check)
                {
                    substep.state = checkpoint_state;
                }
            } else {
                stage.state = checkpoint_state;
            }
            stage.completed = progress.completed;
            stage.total = progress.total;
            stage.chunks = progress.chunks;
            stage.max_chunk_characters = progress.max_chunk_characters;
        }
        self.updated_at = Utc::now().to_rfc3339();
    }

    fn fail(&mut self, error: &anyhow::Error) {
        self.run_state = WikiIngestRunState::Failed;
        if let Some(stage) = self
            .stages
            .iter_mut()
            .find(|stage| stage.state == WikiIngestCheckpointState::Running)
        {
            stage.state = WikiIngestCheckpointState::Failed;
        }
        self.last_error = Some(format!("{error:#}"));
        self.updated_at = Utc::now().to_rfc3339();
    }

    fn succeed(&mut self) {
        self.run_state = WikiIngestRunState::Succeeded;
        self.last_error = None;
        self.updated_at = Utc::now().to_rfc3339();
    }

    fn resume(&mut self, configuration_fingerprint: &str) -> Result<()> {
        if !self.can_resume() {
            bail!("the latest ingest session is complete; choose Redo to run it again");
        }
        if self.configuration_fingerprint != configuration_fingerprint {
            bail!(
                "the LLM model or chunk configuration changed after this ingest failed; choose Redo instead of Continue"
            );
        }
        self.run_state = WikiIngestRunState::Running;
        self.last_error = None;
        for progress in &mut self.stages {
            if matches!(
                progress.state,
                WikiIngestCheckpointState::Running | WikiIngestCheckpointState::Failed
            ) {
                progress.state = WikiIngestCheckpointState::Pending;
                for check in &mut progress.llm_checks {
                    check.state = WikiIngestCheckpointState::Pending;
                }
            }
        }
        self.updated_at = Utc::now().to_rfc3339();
        Ok(())
    }

    fn accept_source_fingerprint(
        &mut self,
        source: &Path,
        digest: &str,
        resuming: bool,
    ) -> Result<()> {
        if resuming
            && let Some(expected) = self.source_fingerprints.get(source)
            && expected != digest
        {
            bail!(
                "source {} changed after this ingest started; choose Redo instead of Continue",
                source.display()
            );
        }
        self.source_fingerprints
            .insert(source.to_path_buf(), digest.to_owned());
        self.updated_at = Utc::now().to_rfc3339();
        Ok(())
    }

    fn save(&self, wiki_root: &Path) -> Result<()> {
        let path = wiki_root.join(Self::RELATIVE_PATH);
        let parent = path.parent().context("ingest status path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let temporary = parent.join(format!(".ingest-status-{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = fs::File::create(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            file.write_all(&serde_json::to_vec_pretty(self)?)
                .with_context(|| format!("failed to write {}", temporary.display()))?;
            file.write_all(b"\n")
                .with_context(|| format!("failed to write {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", temporary.display()))?;
            replace_file_atomic(&temporary, &path).with_context(|| {
                format!(
                    "failed to publish ingest status {} as {}",
                    temporary.display(),
                    path.display()
                )
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn llm_checkpoints(
    initial: WikiIngestCheckpointState,
    owner: WikiIngestStage,
) -> Vec<WikiIngestLlmCheckpoint> {
    if owner != WikiIngestStage::LlmCheck {
        return Vec::new();
    }
    WikiIngestLlmCheck::ALL
        .into_iter()
        .map(|check| WikiIngestLlmCheckpoint {
            check,
            state: initial,
        })
        .collect()
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "MoveFileExW is required to atomically replace a durable checkpoint on Windows"
)]
fn replace_file_atomic(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that live
    // through the call, and the flags request an atomic same-volume replace.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl WikiIngestStageState {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "done",
            Self::Failed => "failed",
        }
    }

    fn from_key(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "done" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WikiIngestProgress {
    pub stage: WikiIngestStage,
    pub state: WikiIngestStageState,
    pub llm_check: Option<WikiIngestLlmCheck>,
    pub completed: Option<usize>,
    pub total: Option<usize>,
    pub chunks: Option<usize>,
    pub max_chunk_characters: Option<usize>,
}

impl WikiIngestProgress {
    const MARKER_PREFIX: &'static str = "BIBIIWIKI_INGEST_STAGE";

    #[must_use]
    pub const fn stage(stage: WikiIngestStage, stage_state: WikiIngestStageState) -> Self {
        Self {
            stage,
            state: stage_state,
            llm_check: None,
            completed: None,
            total: None,
            chunks: None,
            max_chunk_characters: None,
        }
    }

    #[must_use]
    pub const fn llm_check(check: WikiIngestLlmCheck, stage_state: WikiIngestStageState) -> Self {
        let mut progress = Self::stage(WikiIngestStage::LlmCheck, stage_state);
        progress.llm_check = Some(check);
        progress
    }

    #[must_use]
    pub const fn with_units(mut self, completed: usize, total: usize) -> Self {
        self.completed = Some(completed);
        self.total = Some(total);
        self
    }

    #[must_use]
    pub const fn with_chunk_details(mut self, chunks: usize, max_chunk_characters: usize) -> Self {
        self.chunks = Some(chunks);
        self.max_chunk_characters = Some(max_chunk_characters);
        self
    }

    #[must_use]
    pub fn marker(self) -> String {
        let mut marker = format!(
            "{} {} {}",
            Self::MARKER_PREFIX,
            self.stage.key(),
            self.state.key()
        );
        if let Some(check) = self.llm_check {
            marker.push_str(" check=");
            marker.push_str(check.key());
        }
        for (key, value) in [
            ("completed", self.completed),
            ("total", self.total),
            ("chunks", self.chunks),
            ("max_chars", self.max_chunk_characters),
        ] {
            if let Some(value) = value {
                marker.push(' ');
                marker.push_str(key);
                marker.push('=');
                marker.push_str(&value.to_string());
            }
        }
        marker
    }

    #[must_use]
    pub fn parse_marker(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        if fields.next()? != Self::MARKER_PREFIX {
            return None;
        }
        let checkpoint = WikiIngestStage::from_key(fields.next()?)?;
        let transition = WikiIngestStageState::from_key(fields.next()?)?;
        let mut progress = Self::stage(checkpoint, transition);
        for field in fields {
            let (key, value) = field.split_once('=')?;
            if key == "check" {
                if progress
                    .llm_check
                    .replace(WikiIngestLlmCheck::from_key(value)?)
                    .is_some()
                {
                    return None;
                }
                continue;
            }
            let value = value.parse().ok()?;
            let target = match key {
                "completed" => &mut progress.completed,
                "total" => &mut progress.total,
                "chunks" => &mut progress.chunks,
                "max_chars" => &mut progress.max_chunk_characters,
                _ => return None,
            };
            if target.replace(value).is_some() {
                return None;
            }
        }
        if progress.completed.is_some() != progress.total.is_some() {
            return None;
        }
        Some(progress)
    }
}

impl WikiIngestStage {
    pub const ALL: [Self; 10] = [
        Self::Initialize,
        Self::Markdown,
        Self::Chunking,
        Self::LlmCheck,
        Self::Components,
        Self::Concepts,
        Self::Entities,
        Self::Factors,
        Self::Methodologies,
        Self::Queries,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Initialize => "Initialize wiki directories",
            Self::Markdown => "Extract Markdown",
            Self::Chunking => "Chunk Markdown",
            Self::LlmCheck => "Check LLM availability",
            Self::Components => "Extract wiki components",
            Self::Concepts => "Extract concepts",
            Self::Entities => "Extract entities",
            Self::Factors => "Extract factors",
            Self::Methodologies => "Extract methodologies",
            Self::Queries => "Extract queries",
        }
    }

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Markdown => "markdown",
            Self::Chunking => "chunking",
            Self::LlmCheck => "llm-check",
            Self::Components => "components",
            Self::Concepts => "concepts",
            Self::Entities => "entities",
            Self::Factors => "factors",
            Self::Methodologies => "methodologies",
            Self::Queries => "queries",
        }
    }

    fn from_key(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.key() == value)
    }
}

#[derive(Clone, Debug)]
pub struct WikiIngestOptions {
    /// Explicit files and/or directories selected for this ingest run. When
    /// empty, `source_dir` is scanned recursively.
    pub input_paths: Vec<PathBuf>,
    pub source_dir: PathBuf,
    pub mode: WikiIngestMode,
    pub workspace: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WikiIngestMode {
    #[default]
    Start,
    Resume,
    Redo,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WikiIngestReport {
    pub documents: usize,
    pub sources_copied: usize,
    pub sources_analyzed: usize,
    pub sources_pending_analysis: usize,
    pub factors: usize,
    pub derived_formulas: usize,
    pub similarity_links: usize,
    pub methodology_pages: usize,
}

#[derive(Debug)]
pub struct WikiIngestPipeline {
    root: PathBuf,
    config: Config,
    agent: CodexWikiAgent,
    templates: TemplateLibrary,
    configuration_fingerprint: String,
    #[cfg(test)]
    run_llm_preflight: bool,
}

struct AnalysisPageContext<'a> {
    source_target: &'a str,
    source_reference: &'a str,
    digest: &'a str,
    extracted: &'a ExtractedDocument,
    pending_analysis: bool,
}

impl WikiIngestPipeline {
    /// Creates the `AnyDoc` conversion and semantic-analysis pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if the preserved prompt library cannot be compiled.
    pub fn new(root: PathBuf, config: Config) -> Result<Self> {
        let configuration_fingerprint = ingest_configuration_fingerprint(&config);
        Ok(Self {
            root,
            agent: CodexWikiAgent::new(config.clone())?,
            config,
            templates: TemplateLibrary::load()?,
            configuration_fingerprint,
            #[cfg(test)]
            run_llm_preflight: true,
        })
    }

    /// Converts every supported document into Markdown and analyzes that
    /// Markdown through Codex using the configured local `LiteLLM` gateway.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inputs, extraction, Codex, rendering, or
    /// storage failures.
    pub async fn run(&self, options: WikiIngestOptions) -> Result<WikiIngestReport> {
        self.run_with_progress(options, |_| {}).await
    }

    /// Runs the ingest pipeline while reporting durable checkpoint transitions.
    ///
    /// The callback is invoked synchronously on the pipeline task and must not block.
    /// Each successful run reports `Running` then `Succeeded` for every stage in
    /// [`WikiIngestStage::ALL`] order.
    ///
    /// # Errors
    ///
    /// Returns the same initialization, extraction, analysis, or storage errors as [`Self::run`].
    pub async fn run_with_progress<F>(
        &self,
        options: WikiIngestOptions,
        mut on_progress: F,
    ) -> Result<WikiIngestReport>
    where
        F: FnMut(WikiIngestProgress),
    {
        let previous = WikiIngestStatus::load(&self.root)?;
        let (source_paths, status) = match options.mode {
            WikiIngestMode::Resume => {
                let mut status = previous.context(
                    "there is no interrupted or failed ingest session to continue for this wiki root",
                )?;
                status.resume(&self.configuration_fingerprint)?;
                (status.source_paths.clone(), status)
            }
            WikiIngestMode::Redo if options.input_paths.is_empty() => {
                let source_paths = previous
                    .context("there is no previous ingest session to redo for this wiki root")?
                    .source_paths;
                let status = WikiIngestStatus::new(
                    source_paths.clone(),
                    self.configuration_fingerprint.clone(),
                );
                (source_paths, status)
            }
            WikiIngestMode::Start | WikiIngestMode::Redo => {
                let source_paths = if options.input_paths.is_empty() {
                    files_with_extensions(&options.source_dir, SOURCE_EXTENSIONS)?
                } else {
                    classify_ingest_inputs(&options.input_paths)?
                };
                let status = WikiIngestStatus::new(
                    source_paths.clone(),
                    self.configuration_fingerprint.clone(),
                );
                (source_paths, status)
            }
        };
        status.save(&self.root)?;
        let status = RefCell::new(status);
        let resuming = options.mode == WikiIngestMode::Resume;
        let result = self
            .run_resolved(options, source_paths, |progress| {
                let mut status = status.borrow_mut();
                status.apply(progress);
                if let Err(error) = status.save(&self.root) {
                    tracing::warn!(error = %format!("{error:#}"), "could not persist ingest progress");
                }
                on_progress(progress);
            }, |source, digest| {
                let mut status = status.borrow_mut();
                status.accept_source_fingerprint(source, digest, resuming)?;
                status.save(&self.root)
            })
            .await;
        let mut status = status.into_inner();
        match result {
            Ok(report) => {
                status.succeed();
                status.save(&self.root)?;
                Ok(report)
            }
            Err(error) => {
                status.fail(&error);
                if let Err(status_error) = status.save(&self.root) {
                    tracing::warn!(error = %format!("{status_error:#}"), "could not persist failed ingest status");
                }
                Err(error)
            }
        }
    }

    async fn run_resolved<F, S>(
        &self,
        options: WikiIngestOptions,
        source_paths: Vec<PathBuf>,
        mut on_progress: F,
        mut on_source: S,
    ) -> Result<WikiIngestReport>
    where
        F: FnMut(WikiIngestProgress),
        S: FnMut(&Path, &str) -> Result<()>,
    {
        report_ingest_progress(
            &mut on_progress,
            WikiIngestStage::Initialize,
            WikiIngestStageState::Running,
        );
        WikiProject::new(self.root.clone())?.ensure_initialized()?;
        report_ingest_progress(
            &mut on_progress,
            WikiIngestStage::Initialize,
            WikiIngestStageState::Succeeded,
        );
        let wiki_dir = self.root.join("wiki");
        let prepared =
            self.prepare_sources(&source_paths, &options, &mut on_progress, &mut on_source)?;
        let (planned_sources, total_chunks, max_chunk_characters) =
            self.plan_source_chunks(prepared.sources, &mut on_progress);

        self.check_llm_availability(&options.workspace, &mut on_progress)
            .await?;

        report_ingest_units(
            &mut on_progress,
            WikiIngestStage::Components,
            WikiIngestStageState::Running,
            0,
            total_chunks,
            Some((total_chunks, max_chunk_characters)),
        );
        let semantic = self
            .analyze_prepared_sources(
                planned_sources,
                &options,
                total_chunks,
                max_chunk_characters,
                &mut on_progress,
            )
            .await?;
        report_ingest_units(
            &mut on_progress,
            WikiIngestStage::Components,
            WikiIngestStageState::Succeeded,
            total_chunks,
            total_chunks,
            Some((total_chunks, max_chunk_characters)),
        );

        for stage in [
            WikiIngestStage::Concepts,
            WikiIngestStage::Entities,
            WikiIngestStage::Factors,
            WikiIngestStage::Methodologies,
        ] {
            report_ingest_progress(&mut on_progress, stage, WikiIngestStageState::Running);
            verify_ingest_checkpoint(&wiki_dir, stage)?;
            report_ingest_progress(&mut on_progress, stage, WikiIngestStageState::Succeeded);
        }

        report_ingest_progress(
            &mut on_progress,
            WikiIngestStage::Queries,
            WikiIngestStageState::Running,
        );
        let maintenance = WikiMaintainer::new(wiki_dir.clone()).update(false)?;
        let factors = count_factor_pages(&wiki_dir)?;
        let report = WikiIngestReport {
            documents: source_paths.len(),
            sources_copied: prepared.copied,
            sources_analyzed: semantic.analyzed,
            sources_pending_analysis: semantic.pending_analysis,
            factors,
            derived_formulas: 0,
            similarity_links: 0,
            methodology_pages: semantic.methodology_pages.len(),
        };
        self.append_ingest_log(&report, &source_paths, &prepared.event_inputs)?;
        if maintenance.health.invalid_frontmatter.is_empty() {
            // Rebuild once more so a newly created log page is indexed.
            WikiMaintainer::new(wiki_dir).update(false)?;
        }
        report_ingest_progress(
            &mut on_progress,
            WikiIngestStage::Queries,
            WikiIngestStageState::Succeeded,
        );
        Ok(report)
    }

    async fn check_llm_availability<F>(&self, workspace: &Path, on_progress: &mut F) -> Result<()>
    where
        F: FnMut(WikiIngestProgress),
    {
        #[cfg(test)]
        if !self.run_llm_preflight {
            return Ok(());
        }
        report_ingest_progress(
            on_progress,
            WikiIngestStage::LlmCheck,
            WikiIngestStageState::Running,
        );

        self.run_llm_check(WikiIngestLlmCheck::Direct, on_progress, async {
            probe_direct_llm(&self.config).await.map(|_| ())
        })
        .await?;
        self.run_llm_check(WikiIngestLlmCheck::Proxy, on_progress, async {
            probe_responses_proxy(&self.config).await.map(|_| ())
        })
        .await?;
        self.run_llm_check(WikiIngestLlmCheck::Codex, on_progress, async {
            probe_codex_agent(self.config.clone(), workspace)
                .await
                .map(|_| ())
        })
        .await?;
        report_ingest_progress(
            on_progress,
            WikiIngestStage::LlmCheck,
            WikiIngestStageState::Succeeded,
        );
        Ok(())
    }

    async fn run_llm_check<F, Fut>(
        &self,
        check: WikiIngestLlmCheck,
        on_progress: &mut F,
        future: Fut,
    ) -> Result<()>
    where
        F: FnMut(WikiIngestProgress),
        Fut: std::future::Future<Output = Result<()>>,
    {
        on_progress(WikiIngestProgress::llm_check(
            check,
            WikiIngestStageState::Running,
        ));
        match future.await {
            Ok(()) => {
                on_progress(WikiIngestProgress::llm_check(
                    check,
                    WikiIngestStageState::Succeeded,
                ));
                Ok(())
            }
            Err(error) => {
                on_progress(WikiIngestProgress::llm_check(
                    check,
                    WikiIngestStageState::Failed,
                ));
                report_ingest_progress(
                    on_progress,
                    WikiIngestStage::LlmCheck,
                    WikiIngestStageState::Failed,
                );
                Err(error).with_context(|| format!("LLM preflight failed during {}", check.label()))
            }
        }
    }

    fn prepare_sources<F, S>(
        &self,
        source_paths: &[PathBuf],
        options: &WikiIngestOptions,
        on_progress: &mut F,
        on_source: &mut S,
    ) -> Result<PreparedIngestSources>
    where
        F: FnMut(WikiIngestProgress),
        S: FnMut(&Path, &str) -> Result<()>,
    {
        let source_backup = RawSourceBackup::new(self.root.join("raw/sources"));
        let mut result = PreparedIngestSources::with_capacity(source_paths.len());
        report_ingest_units(
            on_progress,
            WikiIngestStage::Markdown,
            WikiIngestStageState::Running,
            0,
            source_paths.len(),
            None,
        );
        for (index, path) in source_paths.iter().enumerate() {
            let backup = source_backup.copy(path)?;
            on_source(&backup.source, &backup.sha256)?;
            result.copied += usize::from(backup.status == "copied");
            result
                .event_inputs
                .push((path.display().to_string(), backup.sha256.clone()));
            result.sources.push(self.prepare_source(
                path,
                &backup.backup,
                &backup.sha256,
                options,
            )?);
            report_ingest_units(
                on_progress,
                WikiIngestStage::Markdown,
                WikiIngestStageState::Running,
                index + 1,
                source_paths.len(),
                None,
            );
        }
        report_ingest_units(
            on_progress,
            WikiIngestStage::Markdown,
            WikiIngestStageState::Succeeded,
            source_paths.len(),
            source_paths.len(),
            None,
        );
        Ok(result)
    }

    fn plan_source_chunks<F>(
        &self,
        prepared_sources: Vec<PreparedSource>,
        on_progress: &mut F,
    ) -> (Vec<PlannedSource>, usize, usize)
    where
        F: FnMut(WikiIngestProgress),
    {
        let max_characters = self.agent.source_chunk_plan("").max_characters;
        let source_count = prepared_sources.len();
        let mut total_chunks = 0;
        let mut planned_sources = Vec::with_capacity(source_count);
        report_ingest_units(
            on_progress,
            WikiIngestStage::Chunking,
            WikiIngestStageState::Running,
            0,
            source_count,
            Some((0, max_characters)),
        );
        for (index, prepared) in prepared_sources.into_iter().enumerate() {
            let chunks = prepared
                .markdown()
                .map_or(0, |markdown| self.agent.source_chunk_plan(markdown).chunks);
            total_chunks += chunks;
            planned_sources.push(PlannedSource { prepared, chunks });
            report_ingest_units(
                on_progress,
                WikiIngestStage::Chunking,
                WikiIngestStageState::Running,
                index + 1,
                source_count,
                Some((total_chunks, max_characters)),
            );
        }
        report_ingest_units(
            on_progress,
            WikiIngestStage::Chunking,
            WikiIngestStageState::Succeeded,
            source_count,
            source_count,
            Some((total_chunks, max_characters)),
        );
        (planned_sources, total_chunks, max_characters)
    }

    async fn analyze_prepared_sources<F>(
        &self,
        planned_sources: Vec<PlannedSource>,
        options: &WikiIngestOptions,
        total_chunks: usize,
        max_chunk_characters: usize,
        on_progress: &mut F,
    ) -> Result<SemanticIngestSummary>
    where
        F: FnMut(WikiIngestProgress),
    {
        let mut summary = SemanticIngestSummary::default();
        let mut completed_chunks = 0;
        for planned in planned_sources {
            let PlannedSource { prepared, chunks } = planned;
            let source = prepared.source().to_path_buf();
            let completed_before_source = completed_chunks;
            let outcome = self
                .analyze_prepared_source(prepared, options, |chunk| {
                    report_ingest_units(
                        on_progress,
                        WikiIngestStage::Components,
                        WikiIngestStageState::Running,
                        completed_before_source + chunk.completed,
                        total_chunks,
                        Some((total_chunks, max_chunk_characters)),
                    );
                })
                .await?;
            completed_chunks += chunks;
            summary.analyzed += usize::from(outcome.ingested);
            summary.pending_analysis += usize::from(outcome.pending_analysis);
            summary.methodology_pages.insert(outcome.methodology_target);
            if let Some(error) = outcome.analysis_error {
                bail!(
                    "Codex semantic analysis failed for {}; raw and converted Markdown were preserved, but wiki components were not extracted: {error}",
                    source.display()
                );
            }
        }
        Ok(summary)
    }

    fn write_factor_records(&self, records: &[FactorRecord]) -> Result<()> {
        let mut groups: BTreeMap<String, Vec<&FactorRecord>> = BTreeMap::new();
        for record in records {
            let name = BilingualName::new(
                &record.definition.english_name,
                &record.definition.chinese_name,
            )?;
            groups.entry(name.filename()).or_default().push(record);
        }
        for records in groups.values() {
            let first = records[0];
            let name = BilingualName::new(
                &first.definition.english_name,
                &first.definition.chinese_name,
            )?;
            let sources = records
                .iter()
                .map(|record| record.source.clone())
                .collect::<BTreeSet<_>>();
            let definitions = records
                .iter()
                .map(|record| record.definition.description.trim())
                .filter(|value| !value.is_empty())
                .collect::<BTreeSet<_>>();
            let categories = records
                .iter()
                .map(|record| record.definition.category.trim())
                .filter(|value| !value.is_empty())
                .collect::<BTreeSet<_>>();
            let formulas = records
                .iter()
                .filter(|record| !record.definition.formula.trim().is_empty())
                .map(|record| {
                    json!({
                        "value": {
                            "markdown": record.definition.formula.trim(),
                            "latex": record.definition.formula.trim(),
                            "status": "supplied"
                        },
                        "sources": [record.source]
                    })
                })
                .collect::<Vec<_>>();
            let expressions = records
                .iter()
                .filter(|record| !record.definition.polars_expression.trim().is_empty())
                .map(|record| {
                    json!({"expression": record.definition.polars_expression.trim(), "sources": [record.source]})
                })
                .collect::<Vec<_>>();
            let body = self.templates.render(
                "wiki/pages/factor.md.j2",
                &json!({"factor": {
                    "descriptions": definitions,
                    "english_names": [name.english],
                    "chinese_names": [name.chinese],
                    "categories": categories,
                    "variant_status": if formulas.is_empty() { "needs-review" } else { "supplied" },
                    "canonical_variant": formulas.first().map(|_| 1),
                    "formulas": formulas,
                    "expressions": expressions,
                    "required_inputs": nonempty_values(records, |definition| &definition.required_inputs),
                    "scenarios": nonempty_values(records, |definition| &definition.scenario),
                    "evaluations": nonempty_values(records, |definition| &definition.evaluation),
                    "similar_factors": Vec::<Value>::new(),
                    "sources": sources,
                }}),
            )?;
            let tags = std::iter::once("factor".to_string())
                .chain(categories.into_iter().map(ToString::to_string))
                .collect();
            WikiStore::new(self.root.join("wiki")).write_page(
                &format!("factors/{}", name.filename()),
                &WikiPage {
                    title: name.title(),
                    body,
                    page_type: "factor".to_string(),
                    tags,
                    sources: sources.into_iter().collect(),
                    backtesttype: "todo".to_string(),
                    ..WikiPage::default()
                },
                Authority::Agent,
            )?;
        }
        Ok(())
    }

    fn prepare_source(
        &self,
        source: &Path,
        backup: &Path,
        digest: &str,
        options: &WikiIngestOptions,
    ) -> Result<PreparedSource> {
        let extracted_root = self.root.join("raw/extracted");
        let source_id = hex_prefix(&Sha256::digest(source.display().to_string().as_bytes()), 12);
        let stem = super::safe_filename_segment(
            source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("source"),
        );
        let source_target = format!("sources/{stem}-{source_id}");
        let sidecar = extracted_root.join(format!("{stem}-{source_id}.md"));
        let manifest = extracted_root.join(format!("{stem}-{source_id}.json"));
        let source_reference = backup
            .strip_prefix(&self.root)
            .unwrap_or(backup)
            .to_string_lossy()
            .replace('\\', "/");
        if options.mode != WikiIngestMode::Redo
            && let Some(outcome) = cached_source(
                &manifest,
                &sidecar,
                &self.root.join("wiki"),
                digest,
                &self.configuration_fingerprint,
            )?
        {
            return Ok(PreparedSource::Cached {
                source: source.to_path_buf(),
                outcome,
            });
        }
        if options.mode == WikiIngestMode::Resume
            && let Some(converted_markdown) = resumable_converted_markdown(
                &manifest,
                &sidecar,
                &self.root.join("wiki"),
                digest,
                &source_target,
            )?
        {
            let kind = source
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .map_or("document", |extension| document_kind(&extension));
            return Ok(PreparedSource::Extracted(Box::new(ExtractedSource {
                source: source.to_path_buf(),
                digest: digest.to_owned(),
                extracted_root,
                source_id,
                source_target,
                sidecar,
                manifest,
                extracted: ExtractedDocument {
                    text: converted_markdown.clone(),
                    extractor: "resumed-converted-markdown",
                    kind,
                },
                source_reference,
                converted_markdown,
            })));
        }

        let extracted = extract_document(source)?;
        let converted_markdown =
            self.write_converted_source(source, &source_target, &source_reference, &extracted)?;
        self.write_converted_manifest(
            source,
            digest,
            &extracted_root,
            &sidecar,
            &manifest,
            &extracted,
            &source_target,
        )?;
        Ok(PreparedSource::Extracted(Box::new(ExtractedSource {
            source: source.to_path_buf(),
            digest: digest.to_owned(),
            extracted_root,
            source_id,
            source_target,
            sidecar,
            manifest,
            extracted,
            source_reference,
            converted_markdown,
        })))
    }

    #[allow(clippy::too_many_arguments)]
    fn write_converted_manifest(
        &self,
        source: &Path,
        digest: &str,
        extracted_root: &Path,
        sidecar: &Path,
        manifest: &Path,
        extracted: &ExtractedDocument,
        source_target: &str,
    ) -> Result<()> {
        let source_name = source
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("source");
        WikiStore::new(extracted_root.to_path_buf()).write_page(
            sidecar
                .file_name()
                .and_then(|value| value.to_str())
                .context("invalid extraction sidecar name")?,
            &WikiPage {
                title: format!("原始提取：{source_name} (Raw Extraction: {source_name})"),
                body: self.templates.render(
                    "wiki/pages/extracted_source.md.j2",
                    &json!({
                        "title": source_name,
                        "extracted_text": extracted.text.trim_end()
                    }),
                )?,
                status: "generated".to_owned(),
                page_type: "extraction".to_owned(),
                tags: vec!["raw".to_owned(), "extraction".to_owned()],
                sources: vec![source.display().to_string()],
                ..WikiPage::default()
            },
            Authority::Agent,
        )?;
        fs::create_dir_all(extracted_root)
            .with_context(|| format!("failed to create {}", extracted_root.display()))?;
        let manifest_value = json!({
            "source": source,
            "sidecar": sidecar,
            "sha256": digest,
            "status": "converted",
            "analysis_error": null,
            "extractor": extracted.extractor,
            "analyzer_schema": ANALYZER_SCHEMA,
            "configuration_fingerprint": self.configuration_fingerprint,
            "source_target": source_target,
            "methodology_target": null
        });
        fs::write(
            manifest,
            format!("{}\n", serde_json::to_string_pretty(&manifest_value)?),
        )
        .with_context(|| format!("failed to write {}", manifest.display()))
    }

    async fn analyze_prepared_source<F>(
        &self,
        prepared: PreparedSource,
        options: &WikiIngestOptions,
        on_chunk: F,
    ) -> Result<SourceOutcome>
    where
        F: FnMut(SourceChunkProgress),
    {
        let prepared = match prepared {
            PreparedSource::Cached { outcome, .. } => return Ok(outcome),
            PreparedSource::Extracted(prepared) => prepared,
        };
        let ExtractedSource {
            source,
            digest,
            extracted_root,
            source_id,
            source_target,
            sidecar,
            manifest,
            extracted,
            source_reference,
            converted_markdown,
        } = *prepared;
        let wiki_dir = self.root.join("wiki");
        let analysis_result = self
            .agent
            .analyze_source_with_progress(
                SourceAnalysisRequest {
                    source_identity: source
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("source")
                        .to_string(),
                    source_text: converted_markdown,
                    purpose: read_optional(&self.root.join("purpose.md"))?,
                    schema: read_optional(&self.root.join("schema.md"))?,
                    wiki_index: build_wiki_index(&wiki_dir)?,
                    media_candidates: Vec::new(),
                    workspace: options.workspace.clone(),
                    state_path: options.state_root.join("wiki_analysis_thread.json"),
                    translation_state_path: options.state_root.join("wiki_translation_thread.json"),
                    cache_mode: if options.mode == WikiIngestMode::Redo {
                        SourceAnalysisCacheMode::Refresh
                    } else {
                        SourceAnalysisCacheMode::Use
                    },
                },
                on_chunk,
            )
            .await;
        let (analysis, manifest_status, analysis_error, pending_analysis) = match analysis_result {
            Ok(analysis) => (analysis, "ingested", None, false),
            Err(error) => {
                let detail = format!("{error:#}");
                tracing::warn!(source = %source.display(), error = %detail, "semantic source analysis is pending; preserving extracted source");
                (
                    pending_source_analysis(&source, &source_id, &detail)?,
                    "pending-analysis",
                    Some(detail),
                    true,
                )
            }
        };
        let methodology_target = self.write_analysis_pages(
            &analysis,
            &AnalysisPageContext {
                source_target: &source_target,
                source_reference: &source_reference,
                digest: &digest,
                extracted: &extracted,
                pending_analysis,
            },
        )?;

        self.write_extraction_and_manifest(
            &source,
            &digest,
            &extracted_root,
            &sidecar,
            &manifest,
            &extracted,
            &analysis,
            &source_target,
            &methodology_target,
            manifest_status,
            analysis_error.as_deref(),
        )?;
        Ok(SourceOutcome {
            ingested: !pending_analysis,
            pending_analysis,
            methodology_target,
            analysis_error,
        })
    }

    fn write_converted_source(
        &self,
        source: &Path,
        source_target: &str,
        source_reference: &str,
        extracted: &ExtractedDocument,
    ) -> Result<String> {
        let relative = format!("{source_target}.md");
        let store = WikiStore::new(self.root.join("wiki"));
        store.write_page(
            &relative,
            &WikiPage {
                title: source
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Converted source")
                    .to_owned(),
                body: extracted.text.clone(),
                status: "generated".to_owned(),
                page_type: "source".to_owned(),
                tags: vec![
                    "source".to_owned(),
                    extracted.kind.to_owned(),
                    "anydoc-converted".to_owned(),
                ],
                sources: vec![source_reference.to_owned()],
                ..WikiPage::default()
            },
            Authority::Agent,
        )?;
        Ok(store
            .read_page(&relative)?
            .context("converted source page was not written")?
            .body)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_extraction_and_manifest(
        &self,
        source: &Path,
        digest: &str,
        extracted_root: &Path,
        sidecar: &Path,
        manifest: &Path,
        extracted: &ExtractedDocument,
        analysis: &SourceAnalysis,
        source_target: &str,
        methodology_target: &str,
        manifest_status: &str,
        analysis_error: Option<&str>,
    ) -> Result<()> {
        WikiStore::new(extracted_root.to_path_buf()).write_page(
            sidecar
                .file_name()
                .and_then(|value| value.to_str())
                .context("invalid extraction sidecar name")?,
            &WikiPage {
                title: format!(
                    "原始提取：{} (Raw Extraction: {})",
                    analysis.source_name.chinese, analysis.source_name.english
                ),
                body: self.templates.render(
                    "wiki/pages/extracted_source.md.j2",
                    &json!({
                        "title": source.file_name().and_then(|value| value.to_str()).unwrap_or("source"),
                        "extracted_text": extracted.text.trim_end()
                    }),
                )?,
                status: "generated".to_string(),
                page_type: "extraction".to_string(),
                tags: vec!["raw".to_string(), "extraction".to_string()],
                sources: vec![source.display().to_string()],
                ..WikiPage::default()
            },
            Authority::Agent,
        )?;
        fs::create_dir_all(extracted_root)
            .with_context(|| format!("failed to create {}", extracted_root.display()))?;
        let manifest_value = json!({
            "source": source, "sidecar": sidecar, "sha256": digest,
            "status": manifest_status, "analysis_error": analysis_error,
            "extractor": extracted.extractor,
            "analyzer_schema": ANALYZER_SCHEMA,
            "configuration_fingerprint": self.configuration_fingerprint,
            "source_target": source_target, "methodology_target": methodology_target
        });
        fs::write(
            manifest,
            format!("{}\n", serde_json::to_string_pretty(&manifest_value)?),
        )
        .with_context(|| format!("failed to write {}", manifest.display()))?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn write_analysis_pages(
        &self,
        analysis: &SourceAnalysis,
        context: &AnalysisPageContext<'_>,
    ) -> Result<String> {
        let source_target = context.source_target;
        let source_reference = context.source_reference;
        let digest = context.digest;
        let extracted = context.extracted;
        let pending_analysis = context.pending_analysis;
        let store = WikiStore::new(self.root.join("wiki"));
        let mut component_targets = Vec::new();
        for (directory, page_type, terms) in [
            ("concepts", "concept", analysis.key_concepts.as_slice()),
            ("entities", "entity", analysis.entities.as_slice()),
        ] {
            for term in terms {
                let name = term.name()?;
                let relative = format!("{directory}/{}", name.filename());
                let target = relative.trim_end_matches(".md").to_string();
                component_targets.push(target.clone());
                if store.read_page(&relative)?.is_none() {
                    store.write_page(
                        &relative,
                        &WikiPage {
                            title: name.title(),
                            body: self.templates.render(
                                "wiki/pages/knowledge_component.md.j2",
                                &json!({
                                    "definition": term.definition,
                                    "evidence": term.evidence,
                                    "component_type": page_type,
                                    "source_target": source_target,
                                    "source_title": analysis.source_name.title(),
                                }),
                            )?,
                            status: "generated".to_string(),
                            page_type: page_type.to_string(),
                            tags: vec![
                                directory.to_string(),
                                "source-derived".to_string(),
                                "codex-generated".to_string(),
                            ],
                            related: vec![source_target.to_string()],
                            sources: vec![source_reference.to_string()],
                            ..WikiPage::default()
                        },
                        Authority::Agent,
                    )?;
                }
            }
        }
        for factor in &analysis.factors {
            self.write_source_factor(factor, source_reference)?;
            let name = BilingualName::new(&factor.english, &factor.chinese)?;
            component_targets.push(format!(
                "factors/{}",
                name.filename().trim_end_matches(".md")
            ));
        }

        let methodology_name = BilingualName::new(
            format!("{} Methodology", analysis.source_name.english),
            format!("{}方法论", analysis.source_name.chinese),
        )?;
        let methodology_relative = format!("methodology/{}", methodology_name.filename());
        let methodology_target = methodology_relative.trim_end_matches(".md").to_string();
        let mut term_targets = Vec::new();
        for term in &analysis.methodology.terms {
            let name = BilingualName::new(&term.english, &term.chinese)?;
            let relative = format!("methodology/{}", name.filename());
            let target = relative.trim_end_matches(".md").to_string();
            let existing = store.read_page(&relative)?;
            let mut body = existing.as_ref().map_or_else(
                || {
                    self.templates.render(
                        "wiki/pages/methodology_term.md.j2",
                        &json!({"name": name_context(&name), "term": term}),
                    )
                },
                |page| Ok(page.body.clone()),
            )?;
            if !body.contains(source_target) {
                let contribution = self.templates.render(
                    "wiki/pages/methodology_term_contribution.md.j2",
                    &json!({
                        "source_page_target": source_target,
                        "source_name": name_context(&analysis.source_name),
                        "evidence": term.evidence,
                    }),
                )?;
                body = format!("{}\n\n{}", body.trim_end(), contribution.trim());
            }
            store.write_page(
                &relative,
                &WikiPage {
                    title: name.title(),
                    body,
                    status: existing
                        .as_ref()
                        .map_or("generated", |page| &page.status)
                        .to_string(),
                    page_type: "methodology-term".to_string(),
                    tags: vec![
                        "methodology".to_string(),
                        "term".to_string(),
                        "source-derived".to_string(),
                        "codex-generated".to_string(),
                    ],
                    related: merged(existing.as_ref().map(|page| &page.related), source_target),
                    sources: merged(
                        existing.as_ref().map(|page| &page.sources),
                        source_reference,
                    ),
                    created: existing.map_or_else(String::new, |page| page.created),
                    ..WikiPage::default()
                },
                Authority::Agent,
            )?;
            term_targets.push((target, name.title()));
        }
        store.write_page(
            &methodology_relative,
            &WikiPage {
                title: methodology_name.title(),
                body: self.templates.render(
                    "wiki/pages/methodology.md.j2",
                    &json!({
                        "methodology": analysis.methodology,
                        "source_name": name_context(&analysis.source_name),
                        "source_reference": source_reference,
                        "source_page_target": source_target,
                        "term_targets": term_targets,
                    }),
                )?,
                page_type: "methodology".to_string(),
                tags: vec![
                    "methodology".to_string(),
                    "source-derived".to_string(),
                    "codex-generated".to_string(),
                ],
                related: std::iter::once(source_target.to_string())
                    .chain(term_targets.iter().map(|(target, _)| target.clone()))
                    .collect(),
                sources: vec![source_reference.to_string()],
                ..WikiPage::default()
            },
            Authority::Agent,
        )?;
        let analysis_markdown = self.templates.render(
            "wiki/pages/source_analysis.md.j2",
            &json!({"analysis": analysis_context(analysis)}),
        )?;
        store.write_page(
            &format!("{source_target}.md"),
            &WikiPage {
                title: analysis.source_name.title(),
                body: self.templates.render(
                    "wiki/pages/converted_source.md.j2",
                    &json!({
                        "converted_markdown": extracted.text.trim_end(),
                        "analysis_markdown": analysis_markdown.trim_end(),
                        "sha256": digest,
                        "extractor": extracted.extractor,
                        "raw_backup": source_reference,
                        "methodology_target": methodology_target,
                        "methodology_title": methodology_name.title(),
                        "component_targets": component_targets,
                        "pending_analysis": pending_analysis,
                    }),
                )?,
                status: if pending_analysis {
                    "draft"
                } else {
                    "generated"
                }
                .to_string(),
                page_type: "source".to_string(),
                tags: vec![
                    "source".to_string(),
                    extracted.kind.to_string(),
                    if pending_analysis {
                        "pending-analysis"
                    } else {
                        "codex-generated"
                    }
                    .to_string(),
                ],
                related: std::iter::once(methodology_target.clone())
                    .chain(component_targets)
                    .collect(),
                sources: vec![source_reference.to_string()],
                ..WikiPage::default()
            },
            if pending_analysis {
                Authority::Agent
            } else {
                Authority::Human
            },
        )?;
        Ok(methodology_target)
    }

    fn write_source_factor(&self, factor: &SourceFactor, source: &str) -> Result<()> {
        let name = BilingualName::new(&factor.english, &factor.chinese)?;
        let relative = format!("factors/{}", name.filename());
        let store = WikiStore::new(self.root.join("wiki"));
        if store.read_page(&relative)?.is_some() {
            return Ok(());
        }
        self.write_factor_records(&[FactorRecord {
            definition: FactorDefinition {
                category: factor.category.clone(),
                english_name: factor.english.clone(),
                chinese_name: factor.chinese.clone(),
                description: factor.definition.clone(),
                formula: factor.formula.clone(),
                scenario: factor.scenario.clone(),
                evaluation: factor.evaluation.clone(),
                required_inputs: factor.required_inputs.join("; "),
                polars_expression: factor.polars_expression.clone(),
                ..FactorDefinition::default()
            },
            source: source.to_string(),
        }])
    }

    fn append_ingest_log(
        &self,
        report: &WikiIngestReport,
        sources: &[PathBuf],
        event_inputs: &[(String, String)],
    ) -> Result<()> {
        let store = WikiStore::new(self.root.join("wiki"));
        let existing = store.read_page("log.md")?;
        let mut body = existing.as_ref().map_or_else(
            || {
                self.templates
                    .render("wiki/project/log_header.md.j2", &json!({}))
            },
            |page| Ok(page.body.clone()),
        )?;
        let event_id = event_id(event_inputs, report);
        if body.contains(&format!("event:{event_id}")) {
            return Ok(());
        }
        let entry = self.templates.render(
            "wiki/project/log_entry.md.j2",
            &json!({
                "event_date": today_beijing(),
                "stats": {
                    "event_id": event_id,
                    "sources_copied": report.sources_copied,
                    "sources_analyzed": report.sources_analyzed,
                    "sources_pending_analysis": report.sources_pending_analysis,
                    "merged_factors": report.factors,
                    "derived_formulas": report.derived_formulas,
                    "similarity_links": report.similarity_links,
                },
                "source_titles": sources.iter().filter_map(|path| path.file_name().and_then(|value| value.to_str())).collect::<Vec<_>>(),
                "methodology_pages": report.methodology_pages,
            }),
        )?;
        body = format!("{}\n\n{}", body.trim_end(), entry.trim());
        store.write_page(
            "log.md",
            &WikiPage {
                title: "Wiki日志 (Wiki Log)".to_string(),
                body,
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
}

fn report_ingest_progress<F>(
    on_progress: &mut F,
    checkpoint: WikiIngestStage,
    transition: WikiIngestStageState,
) where
    F: FnMut(WikiIngestProgress),
{
    on_progress(WikiIngestProgress::stage(checkpoint, transition));
}

fn report_ingest_units<F>(
    on_progress: &mut F,
    stage: WikiIngestStage,
    stage_state: WikiIngestStageState,
    completed: usize,
    total: usize,
    chunk_details: Option<(usize, usize)>,
) where
    F: FnMut(WikiIngestProgress),
{
    let mut progress = WikiIngestProgress::stage(stage, stage_state).with_units(completed, total);
    if let Some((chunks, max_characters)) = chunk_details {
        progress = progress.with_chunk_details(chunks, max_characters);
    }
    on_progress(progress);
}

fn verify_ingest_checkpoint(wiki_dir: &Path, stage: WikiIngestStage) -> Result<()> {
    let directory = match stage {
        WikiIngestStage::Markdown => "sources",
        WikiIngestStage::Initialize
        | WikiIngestStage::Chunking
        | WikiIngestStage::LlmCheck
        | WikiIngestStage::Components => {
            return Ok(());
        }
        WikiIngestStage::Concepts => "concepts",
        WikiIngestStage::Entities => "entities",
        WikiIngestStage::Factors => "factors",
        WikiIngestStage::Methodologies => "methodology",
        WikiIngestStage::Queries => "queries",
    };
    let index = wiki_dir.join(directory).join("index.md");
    if !index.is_file() {
        bail!(
            "ingest checkpoint {} is incomplete: {} is missing",
            stage.label(),
            index.display()
        );
    }
    Ok(())
}

const ANALYZER_SCHEMA: &str = "codex-dynamic-wiki-analysis-v2-source-factors";

fn ingest_configuration_fingerprint(config: &Config) -> String {
    let alias = config.codex_model();
    let deployment = config
        .model_list
        .iter()
        .find(|deployment| deployment.model_name == alias);
    let provider_model =
        deployment.map_or("", |deployment| deployment.litellm_params.model.as_str());
    let api_base = deployment
        .and_then(|deployment| deployment.litellm_params.api_base.as_deref())
        .unwrap_or_default();
    let identity = format!(
        "{ANALYZER_SCHEMA}\0{alias}\0{provider_model}\0{api_base}\0{}\0{}\0{}",
        config.codex.reasoning_effort.as_deref().unwrap_or_default(),
        config.max_markdown_chunk_characters(),
        config.server.default_max_output_tokens
    );
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

#[derive(Clone, Debug)]
struct FactorRecord {
    definition: FactorDefinition,
    source: String,
}

#[derive(Clone, Debug)]
struct ExtractedDocument {
    text: String,
    extractor: &'static str,
    kind: &'static str,
}

#[derive(Clone, Debug)]
enum PreparedSource {
    Cached {
        source: PathBuf,
        outcome: SourceOutcome,
    },
    Extracted(Box<ExtractedSource>),
}

impl PreparedSource {
    fn source(&self) -> &Path {
        match self {
            Self::Cached { source, .. } => source,
            Self::Extracted(prepared) => &prepared.source,
        }
    }

    fn markdown(&self) -> Option<&str> {
        match self {
            Self::Cached { .. } => None,
            Self::Extracted(prepared) => Some(&prepared.converted_markdown),
        }
    }
}

#[derive(Clone, Debug)]
struct PlannedSource {
    prepared: PreparedSource,
    chunks: usize,
}

struct PreparedIngestSources {
    event_inputs: Vec<(String, String)>,
    copied: usize,
    sources: Vec<PreparedSource>,
}

impl PreparedIngestSources {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            event_inputs: Vec::with_capacity(capacity),
            copied: 0,
            sources: Vec::with_capacity(capacity),
        }
    }
}

#[derive(Clone, Debug)]
struct ExtractedSource {
    source: PathBuf,
    digest: String,
    extracted_root: PathBuf,
    source_id: String,
    source_target: String,
    sidecar: PathBuf,
    manifest: PathBuf,
    extracted: ExtractedDocument,
    source_reference: String,
    converted_markdown: String,
}

#[derive(Clone, Debug, Default)]
struct SemanticIngestSummary {
    analyzed: usize,
    pending_analysis: usize,
    methodology_pages: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct SourceOutcome {
    ingested: bool,
    pending_analysis: bool,
    methodology_target: String,
    analysis_error: Option<String>,
}

fn pending_source_analysis(source: &Path, source_id: &str, error: &str) -> Result<SourceAnalysis> {
    let file_name = source
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    Ok(SourceAnalysis {
        source_name: BilingualName::new(
            format!("Pending Source {source_id}"),
            format!("待分析来源 {source_id}"),
        )?,
        summary: format!(
            "Semantic analysis is pending, but the complete extracted source remains searchable and recoverable. Source: {file_name}. Error: {error}"
        ),
        key_concepts: Vec::new(),
        findings: Vec::new(),
        connections: Vec::new(),
        tensions: Vec::new(),
        recommendations: vec![
            "Retry ingestion after correcting the model configuration or provider timeout."
                .to_owned(),
        ],
        entities: Vec::new(),
        selected_media: Vec::new(),
        methodology: super::agent::SourceMethodology {
            definition: "Pending semantic analysis; consult the preserved extraction.".to_owned(),
            terms: Vec::new(),
            data_inputs: Vec::new(),
            benefits: Vec::new(),
            limitations: vec!["Structured knowledge was not generated in this run.".to_owned()],
            coverage_gaps: vec!["All semantic fields require retry and review.".to_owned()],
            connections: Vec::new(),
            review_status: "pending-analysis".to_owned(),
        },
        factors: Vec::new(),
    })
}

fn extract_document(source: &Path) -> Result<ExtractedDocument> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "md" | "txt") {
        return Ok(ExtractedDocument {
            text: fs::read_to_string(source)
                .with_context(|| format!("failed to extract text {}", source.display()))?,
            extractor: "rust-utf8-text",
            kind: "text",
        });
    }

    match anydoc::to_markdown(source) {
        Ok(text) => Ok(ExtractedDocument {
            text,
            extractor: "firecrawl-anydoc-0.2.4",
            kind: document_kind(&extension),
        }),
        Err(anydoc_error) if extension == "pdf" => {
            tracing::warn!(
                source = %source.display(),
                error = %anydoc_error,
                "AnyDoc requested OCR; trying the local PDF text fallback"
            );
            let pages = pdf_extract::extract_text_by_pages(source).with_context(|| {
                format!(
                    "AnyDoc could not convert {} ({anydoc_error}); PDF fallback also failed",
                    source.display()
                )
            })?;
            let text = pages
                .iter()
                .enumerate()
                .map(|(index, page)| format!("## Page {}\n\n{}", index + 1, page.trim()))
                .collect::<Vec<_>>()
                .join("\n\n");
            Ok(ExtractedDocument {
                text,
                extractor: "firecrawl-anydoc-0.2.4+pdf-extract-fallback",
                kind: "pdf",
            })
        }
        Err(error) => {
            Err(error).with_context(|| format!("AnyDoc could not convert {}", source.display()))
        }
    }
}

fn document_kind(extension: &str) -> &'static str {
    match extension {
        "ppt" | "pps" | "pot" | "pptx" | "pptm" | "ppsx" | "ppsm" | "odp" => "presentation",
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | "csv" => "spreadsheet",
        "epub" => "ebook",
        "pdf" => "pdf",
        _ => "document",
    }
}

fn cached_source(
    manifest: &Path,
    sidecar: &Path,
    wiki_dir: &Path,
    digest: &str,
    configuration_fingerprint: &str,
) -> Result<Option<SourceOutcome>> {
    if !manifest.exists() || !sidecar.exists() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(
        &fs::read(manifest).with_context(|| format!("failed to read {}", manifest.display()))?,
    )
    .with_context(|| format!("invalid source manifest {}", manifest.display()))?;
    if value.get("sha256").and_then(Value::as_str) != Some(digest)
        || value.get("analyzer_schema").and_then(Value::as_str) != Some(ANALYZER_SCHEMA)
        || value
            .get("configuration_fingerprint")
            .and_then(Value::as_str)
            != Some(configuration_fingerprint)
        || value.get("status").and_then(Value::as_str) != Some("ingested")
    {
        return Ok(None);
    }
    let methodology_target = value
        .get("methodology_target")
        .and_then(Value::as_str)
        .context("cached source manifest omitted methodology_target")?;
    let source_target = value
        .get("source_target")
        .and_then(Value::as_str)
        .context("cached source manifest omitted source_target")?;
    if !wiki_dir.join(format!("{source_target}.md")).exists()
        || !wiki_dir.join(format!("{methodology_target}.md")).exists()
    {
        return Ok(None);
    }
    Ok(Some(SourceOutcome {
        ingested: false,
        pending_analysis: false,
        methodology_target: methodology_target.to_string(),
        analysis_error: None,
    }))
}

fn resumable_converted_markdown(
    manifest: &Path,
    sidecar: &Path,
    wiki_dir: &Path,
    digest: &str,
    expected_source_target: &str,
) -> Result<Option<String>> {
    if !manifest.is_file() || !sidecar.is_file() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(
        &fs::read(manifest).with_context(|| format!("failed to read {}", manifest.display()))?,
    )
    .with_context(|| format!("invalid source manifest {}", manifest.display()))?;
    if value.get("sha256").and_then(Value::as_str) != Some(digest)
        || value.get("analyzer_schema").and_then(Value::as_str) != Some(ANALYZER_SCHEMA)
        || !matches!(
            value.get("status").and_then(Value::as_str),
            Some("converted" | "pending-analysis")
        )
        || value.get("source_target").and_then(Value::as_str) != Some(expected_source_target)
    {
        return Ok(None);
    }
    let page = WikiStore::new(wiki_dir.to_path_buf())
        .read_page(&format!("{expected_source_target}.md"))?;
    Ok(page.map(|page| page.body))
}

fn nonempty_values<'a>(
    records: &'a [&FactorRecord],
    field: impl Fn(&'a FactorDefinition) -> &'a str,
) -> BTreeSet<&'a str> {
    records
        .iter()
        .map(|record| field(&record.definition).trim())
        .filter(|value| !value.is_empty())
        .collect()
}

fn name_context(name: &BilingualName) -> Value {
    json!({"english": name.english, "chinese": name.chinese, "title": name.title()})
}

fn analysis_context(analysis: &SourceAnalysis) -> Value {
    json!({
        "summary": analysis.summary,
        "key_concepts": analysis.key_concepts.iter().map(|term| json!({
            "name": {"title": format!("{} ({})", term.chinese, term.english)},
            "definition": term.definition,
        })).collect::<Vec<_>>(),
        "findings": analysis.findings,
        "connections": analysis.connections,
        "tensions": analysis.tensions,
        "recommendations": analysis.recommendations,
    })
}

fn merged(existing: Option<&Vec<String>>, value: &str) -> Vec<String> {
    existing
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(std::iter::once(value))
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn build_wiki_index(wiki_dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    for directory in ["concepts", "entities", "factors", "methodology"] {
        for path in files_with_extensions(&wiki_dir.join(directory), &["md"])? {
            if path.file_name().and_then(|value| value.to_str()) == Some("index.md") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let page = parse_page(&text)?;
            let relative = path
                .strip_prefix(wiki_dir)
                .expect("wiki file remains within wiki root")
                .to_string_lossy()
                .replace('\\', "/");
            entries.push(format!(
                "- [[{}|{}]]",
                relative.trim_end_matches(".md"),
                page.title
            ));
        }
    }
    entries.sort();
    Ok(entries.join("\n"))
}

fn files_with_extensions(root: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    collect_files(root, extensions, &mut output)?;
    output.sort();
    Ok(output)
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "doc", "docx", "docm", "odt", "rtf", "epub", "pdf", "ppt", "pps", "pot", "pptx", "pptm",
    "ppsx", "ppsm", "odp", "xls", "xlsx", "xlsm", "xlsb", "ods", "csv", "md", "txt",
];

fn classify_ingest_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    for input in inputs {
        if input.is_dir() {
            collect_files(input, SOURCE_EXTENSIONS, &mut sources)?;
            continue;
        }
        if !input.is_file() {
            bail!("selected ingest input does not exist: {}", input.display());
        }
        if has_extension(input, SOURCE_EXTENSIONS) {
            sources.push(input.clone());
        } else {
            bail!(
                "unsupported ingest input {}; choose an AnyDoc-supported document, Markdown, or text file",
                input.display()
            );
        }
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            extensions
                .iter()
                .any(|item| value.eq_ignore_ascii_case(item))
        })
}

fn collect_files(root: &Path, extensions: &[&str], output: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to list input directory {}", root.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, extensions, output)?;
        } else if path.is_file() && has_extension(&path, extensions) {
            output.push(path);
        }
    }
    Ok(())
}

fn count_factor_pages(wiki_dir: &Path) -> Result<usize> {
    let mut count = 0;
    for path in files_with_extensions(&wiki_dir.join("factors"), &["md"])? {
        if path.file_name().and_then(|value| value.to_str()) == Some("index.md") {
            continue;
        }
        let page = parse_page(&fs::read_to_string(&path)?)?;
        count += usize::from(page.page_type == "factor");
    }
    Ok(count)
}

fn read_optional(path: &Path) -> Result<String> {
    if path.exists() {
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
    } else {
        Ok(String::new())
    }
}

fn event_id(inputs: &[(String, String)], report: &WikiIngestReport) -> String {
    let mut inputs = inputs.to_vec();
    inputs.sort();
    let mut digest = Sha256::new();
    digest.update(b"llm-wiki-rust-v1\0");
    for (path, checksum) in inputs {
        digest.update(path.as_bytes());
        digest.update(b"\0");
        digest.update(checksum.as_bytes());
        digest.update(b"\0");
    }
    digest.update(
        format!(
            "{}:{}:{}:{}:{}",
            report.factors,
            report.derived_formulas,
            report.similarity_links,
            report.methodology_pages,
            report.sources_pending_analysis
        )
        .as_bytes(),
    );
    format!("ingest-{}", hex_prefix(&digest.finalize(), 16))
}

fn hex_prefix(bytes: &[u8], characters: usize) -> String {
    let mut output = String::with_capacity(characters);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
        if output.len() >= characters {
            output.truncate(characters);
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn pipeline_without_llm_preflight(root: PathBuf, config: Config) -> WikiIngestPipeline {
        let mut pipeline = WikiIngestPipeline::new(root, config).expect("pipeline");
        pipeline.run_llm_preflight = false;
        pipeline
    }

    #[test]
    #[ignore = "requires the local Python golden PDF fixture, which is not distributed"]
    fn pdf_fallback_preserves_a_golden_document_when_anydoc_requests_ocr() {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/python_wiki_root/raw/sources/101 Formulaic Alphas 1601.00991v3.pdf",
        );
        let document = extract_document(&source).expect("golden PDF should convert");
        assert_eq!(
            document.extractor,
            "firecrawl-anydoc-0.2.4+pdf-extract-fallback"
        );
        assert!(document.text.contains("Alpha"));
    }

    #[test]
    fn anydoc_converts_rtf_to_markdown_in_process() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("anydoc-rtf-{unique}"));
        fs::create_dir_all(&root).expect("AnyDoc test directory");
        let source = root.join("factor.rtf");
        fs::write(
            &source,
            r"{\rtf1\ansi Momentum uses {\b historical returns} as a signal.}",
        )
        .expect("RTF fixture");

        let document = extract_document(&source).expect("AnyDoc RTF conversion");

        assert_eq!(document.extractor, "firecrawl-anydoc-0.2.4");
        assert!(document.text.contains("**historical returns**"));
        fs::remove_dir_all(root).expect("AnyDoc test cleanup");
    }

    #[test]
    fn explicit_ingest_inputs_classify_files_and_recursive_directories() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("selected-ingest-inputs-{unique}"));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("input fixture directory");
        let markdown = root.join("momentum.md");
        let catalog = nested.join("factors.csv");
        fs::write(&markdown, "# Momentum").expect("document fixture");
        fs::write(&catalog, "english_name,chinese_name\nMomentum,动量\n").expect("catalog fixture");
        fs::write(nested.join("ignored.png"), "not an ingest input")
            .expect("unsupported nested fixture");

        let sources = classify_ingest_inputs(&[root.clone(), markdown.clone()])
            .expect("selected directory and file should classify");

        assert_eq!(sources, vec![markdown, catalog]);
        fs::remove_dir_all(root).expect("input fixture cleanup");
    }

    #[test]
    fn explicit_unsupported_file_reports_a_clear_error() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("unsupported-ingest-input-{unique}"));
        fs::create_dir_all(&root).expect("input fixture directory");
        let unsupported = root.join("image.png");
        fs::write(&unsupported, "not supported").expect("unsupported fixture");

        let error = classify_ingest_inputs(&[unsupported])
            .expect_err("explicit unsupported files must not silently disappear");

        assert!(error.to_string().contains("unsupported ingest input"));
        fs::remove_dir_all(root).expect("input fixture cleanup");
    }

    #[test]
    fn ingest_progress_markers_round_trip_and_reject_regular_output() {
        for stage in WikiIngestStage::ALL {
            for state in [
                WikiIngestStageState::Running,
                WikiIngestStageState::Succeeded,
            ] {
                let progress = WikiIngestProgress::stage(stage, state);
                assert_eq!(
                    WikiIngestProgress::parse_marker(&progress.marker()),
                    Some(progress)
                );
            }
        }
        let detailed =
            WikiIngestProgress::stage(WikiIngestStage::Chunking, WikiIngestStageState::Running)
                .with_units(2, 4)
                .with_chunk_details(7, 10_000);
        assert_eq!(
            WikiIngestProgress::parse_marker(&detailed.marker()),
            Some(detailed)
        );
        assert_eq!(
            WikiIngestProgress::parse_marker("documents=2 sources_analyzed=2"),
            None
        );
        assert_eq!(
            WikiIngestProgress::parse_marker("BIBIIWIKI_INGEST_STAGE concepts unknown"),
            None
        );

        for check in WikiIngestLlmCheck::ALL {
            let progress = WikiIngestProgress::llm_check(check, WikiIngestStageState::Succeeded);
            assert_eq!(
                WikiIngestProgress::parse_marker(&progress.marker()),
                Some(progress),
                "LLM preflight substep should round-trip for {}",
                check.label()
            );
        }
    }

    #[tokio::test]
    async fn ingest_stops_before_wiki_extraction_when_original_llm_check_fails() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("llm-preflight-failure-{unique}"));
        let wiki_root = test_root.join("wiki_root");
        let source_dir = test_root.join("sources");
        fs::create_dir_all(&source_dir).expect("source directory");
        fs::write(source_dir.join("momentum.md"), "# Momentum\n\nSource body.")
            .expect("source fixture");
        let config = Config::parse(
            r"
server:
  bind: 127.0.0.1:0
  request_timeout_seconds: 1
model_list:
  - model_name: unavailable
    litellm_params:
      model: ollama/unavailable
      api_base: http://127.0.0.1:9/v1
codex:
  binary: definitely-missing-bibiiwiki-codex
  model: unavailable
",
        )
        .expect("unavailable model configuration");
        let mut progress = Vec::new();

        let error = WikiIngestPipeline::new(wiki_root.clone(), config)
            .expect("pipeline")
            .run_with_progress(
                WikiIngestOptions {
                    input_paths: Vec::new(),
                    source_dir,
                    mode: WikiIngestMode::Start,
                    workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                    state_root: test_root.join("state"),
                },
                |event| progress.push(event),
            )
            .await
            .expect_err("the direct LLM preflight must fail");

        assert!(error.to_string().contains("Original LLM"));
        assert!(progress.contains(&WikiIngestProgress::llm_check(
            WikiIngestLlmCheck::Direct,
            WikiIngestStageState::Running,
        )));
        assert!(progress.contains(&WikiIngestProgress::llm_check(
            WikiIngestLlmCheck::Direct,
            WikiIngestStageState::Failed,
        )));
        assert!(
            !progress
                .iter()
                .any(|event| event.stage == WikiIngestStage::Components)
        );
        let status = WikiIngestStatus::load(&wiki_root)
            .expect("durable ingest status")
            .expect("failed ingest status");
        assert_eq!(status.failed_stage(), Some(WikiIngestStage::LlmCheck));
        let llm_checkpoint = status
            .checkpoints()
            .iter()
            .find(|checkpoint| checkpoint.stage == WikiIngestStage::LlmCheck)
            .expect("LLM checkpoint");
        assert_eq!(
            llm_checkpoint.llm_checks,
            vec![
                WikiIngestLlmCheckpoint {
                    check: WikiIngestLlmCheck::Direct,
                    state: WikiIngestCheckpointState::Failed,
                },
                WikiIngestLlmCheckpoint {
                    check: WikiIngestLlmCheck::Proxy,
                    state: WikiIngestCheckpointState::Pending,
                },
                WikiIngestLlmCheckpoint {
                    check: WikiIngestLlmCheck::Codex,
                    state: WikiIngestCheckpointState::Pending,
                },
            ]
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn unavailable_semantic_analyzer_preserves_source_and_fails_semantic_stages() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("pending-analysis-{unique}"));
        let wiki_root = test_root.join("wiki_root");
        let source_dir = test_root.join("sources");
        fs::create_dir_all(&source_dir).expect("source directory");
        fs::write(
            source_dir.join("momentum.md"),
            "# Momentum\n\nA 20-day momentum factor uses close / delay(close, 20) - 1.",
        )
        .expect("source fixture");

        let mut config =
            Config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"))
                .expect("checked configuration");
        config.server.bind = "127.0.0.1:0".parse().expect("loopback address");
        config.codex.binary = "definitely-missing-bibiiwiki-codex".to_owned();
        let mut progress = Vec::new();
        let error = pipeline_without_llm_preflight(wiki_root.clone(), config)
            .run_with_progress(
                WikiIngestOptions {
                    input_paths: Vec::new(),
                    source_dir,
                    mode: WikiIngestMode::Start,
                    workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                    state_root: test_root.join("state"),
                },
                |event| progress.push(event),
            )
            .await
            .expect_err("analysis failure must not report semantic extraction as complete");

        assert!(
            error
                .to_string()
                .contains("wiki components were not extracted")
        );
        assert!(error.to_string().contains("failed to launch"));
        let status = WikiIngestStatus::load(&wiki_root)
            .expect("durable ingest status")
            .expect("failed ingest status should exist");
        assert_eq!(status.run_state(), WikiIngestRunState::Failed);
        assert!(status.can_resume());
        assert_eq!(status.failed_stage(), Some(WikiIngestStage::Components));
        assert_eq!(status.source_paths().len(), 1);
        assert!(wiki_root.join("raw/sources/momentum.md").is_file());
        let manifest = fs::read_dir(wiki_root.join("raw/extracted"))
            .expect("extraction directory")
            .filter_map(Result::ok)
            .find(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
            .expect("pending manifest");
        assert!(
            fs::read_to_string(manifest.path())
                .expect("manifest source")
                .contains("pending-analysis")
        );
        let hits = crate::wiki::WikiSearch::new(wiki_root.join("wiki"))
            .search("momentum factor", 5)
            .expect("pending extraction should remain searchable");
        assert!(!hits.is_empty());
        assert_eq!(
            progress,
            vec![
                WikiIngestProgress::stage(
                    WikiIngestStage::Initialize,
                    WikiIngestStageState::Running,
                ),
                WikiIngestProgress::stage(
                    WikiIngestStage::Initialize,
                    WikiIngestStageState::Succeeded,
                ),
                WikiIngestProgress::stage(
                    WikiIngestStage::Markdown,
                    WikiIngestStageState::Running,
                )
                .with_units(0, 1),
                WikiIngestProgress::stage(
                    WikiIngestStage::Markdown,
                    WikiIngestStageState::Running,
                )
                .with_units(1, 1),
                WikiIngestProgress::stage(
                    WikiIngestStage::Markdown,
                    WikiIngestStageState::Succeeded,
                )
                .with_units(1, 1),
                WikiIngestProgress::stage(
                    WikiIngestStage::Chunking,
                    WikiIngestStageState::Running,
                )
                .with_units(0, 1)
                .with_chunk_details(0, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Chunking,
                    WikiIngestStageState::Running,
                )
                .with_units(1, 1)
                .with_chunk_details(1, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Chunking,
                    WikiIngestStageState::Succeeded,
                )
                .with_units(1, 1)
                .with_chunk_details(1, 10_000),
                WikiIngestProgress::stage(
                    WikiIngestStage::Components,
                    WikiIngestStageState::Running,
                )
                .with_units(0, 1)
                .with_chunk_details(1, 10_000),
            ]
        );
    }

    #[tokio::test]
    async fn failed_ingest_continues_the_same_session_and_redo_accepts_changed_sources() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let test_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("resumable-ingest-{unique}"));
        let wiki_root = test_root.join("wiki_root");
        let source_dir = test_root.join("sources");
        let source = source_dir.join("momentum.md");
        fs::create_dir_all(&source_dir).expect("source directory");
        fs::write(&source, "# Momentum\n\nA supplied momentum factor.").expect("source fixture");

        let mut config =
            Config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"))
                .expect("checked configuration");
        config.server.bind = "127.0.0.1:0".parse().expect("loopback address");
        config.codex.binary = "definitely-missing-bibiiwiki-codex".to_owned();
        let options = WikiIngestOptions {
            input_paths: vec![source.clone()],
            source_dir: source_dir.clone(),
            mode: WikiIngestMode::Start,
            workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            state_root: test_root.join("state"),
        };

        pipeline_without_llm_preflight(wiki_root.clone(), config.clone())
            .run(options.clone())
            .await
            .expect_err("first analysis should fail");
        let first = WikiIngestStatus::load(&wiki_root)
            .expect("first status")
            .expect("first session");

        let mut resume = options.clone();
        resume.input_paths.clear();
        resume.mode = WikiIngestMode::Resume;
        pipeline_without_llm_preflight(wiki_root.clone(), config.clone())
            .run(resume.clone())
            .await
            .expect_err("unavailable analyzer should still fail after continuing");
        let continued = WikiIngestStatus::load(&wiki_root)
            .expect("continued status")
            .expect("continued session");
        assert_eq!(continued.session_id(), first.session_id());
        assert_eq!(continued.failed_stage(), Some(WikiIngestStage::Components));

        let mut changed_config = config.clone();
        changed_config
            .chunking
            .model_max_characters
            .insert(changed_config.codex_model().to_owned(), 9_000);
        let changed_configuration =
            pipeline_without_llm_preflight(wiki_root.clone(), changed_config)
                .run(resume.clone())
                .await
                .expect_err("Continue must reject a changed model configuration");
        assert!(
            changed_configuration
                .to_string()
                .contains("configuration changed")
        );

        fs::write(&source, "# Momentum\n\nThe source changed after failure.")
            .expect("changed source");
        let changed = pipeline_without_llm_preflight(wiki_root.clone(), config.clone())
            .run(resume)
            .await
            .expect_err("Continue must reject a changed source snapshot");
        assert!(changed.to_string().contains("choose Redo"));

        let mut redo = options;
        redo.input_paths.clear();
        redo.mode = WikiIngestMode::Redo;
        pipeline_without_llm_preflight(wiki_root.clone(), config)
            .run(redo)
            .await
            .expect_err("redo should reach the still-unavailable analyzer");
        let redone = WikiIngestStatus::load(&wiki_root)
            .expect("redo status")
            .expect("redo session");
        assert_ne!(redone.session_id(), first.session_id());
        assert_eq!(redone.failed_stage(), Some(WikiIngestStage::Components));
    }
}
