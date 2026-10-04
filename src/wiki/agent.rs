use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use text_splitter::{ChunkConfig, MarkdownSplitter};

use crate::codex::{CodexPromptRequest, prompt as codex_prompt};
use crate::config::Config;

use super::{BilingualName, PartialBilingualName, TemplateLibrary};

const ANALYZER_IDENTITY: &str = "codex-dynamic-wiki-analysis-v6-required-factors";
const DETERMINISTIC_FORMULAIC_FACTOR_THRESHOLD: usize = 8;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KnowledgeTerm {
    pub english: String,
    pub chinese: String,
    pub definition: String,
    pub evidence: Vec<String>,
}

impl KnowledgeTerm {
    /// Returns this term's validated bilingual name.
    ///
    /// # Errors
    ///
    /// Returns an error when either language slot violates the wiki contract.
    pub fn name(&self) -> Result<BilingualName> {
        BilingualName::new(&self.english, &self.chinese)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MethodologyTerm {
    pub english: String,
    pub chinese: String,
    pub term_type: String,
    pub definition: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceMethodology {
    pub definition: String,
    pub terms: Vec<MethodologyTerm>,
    pub data_inputs: Vec<String>,
    pub benefits: Vec<String>,
    pub limitations: Vec<String>,
    pub coverage_gaps: Vec<String>,
    pub connections: Vec<String>,
    pub review_status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MediaSelection {
    pub asset_key: String,
    pub english: String,
    pub chinese: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceFactor {
    pub english: String,
    pub chinese: String,
    pub definition: String,
    pub category: String,
    pub formula: String,
    pub required_inputs: Vec<String>,
    pub polars_expression: String,
    pub scenario: String,
    pub evaluation: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceAnalysis {
    pub source_name: BilingualName,
    pub summary: String,
    pub key_concepts: Vec<KnowledgeTerm>,
    pub findings: Vec<String>,
    pub connections: Vec<String>,
    pub tensions: Vec<String>,
    pub recommendations: Vec<String>,
    pub entities: Vec<KnowledgeTerm>,
    pub selected_media: Vec<MediaSelection>,
    pub methodology: SourceMethodology,
    pub factors: Vec<SourceFactor>,
}

#[derive(Clone, Debug)]
pub struct SourceAnalysisRequest {
    pub source_identity: String,
    pub source_text: String,
    pub purpose: String,
    pub schema: String,
    pub wiki_index: String,
    pub media_candidates: Vec<String>,
    pub workspace: PathBuf,
    pub state_path: PathBuf,
    pub translation_state_path: PathBuf,
    pub cache_mode: SourceAnalysisCacheMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SourceAnalysisCacheMode {
    #[default]
    Use,
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceChunkPlan {
    pub chunks: usize,
    pub max_characters: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceChunkProgress {
    pub completed: usize,
    pub total: usize,
    pub max_characters: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FactorProposal {
    pub name: String,
    pub hypothesis: String,
    pub polars_expression: String,
}

#[derive(Debug)]
pub struct CodexWikiAgent {
    config: Config,
    templates: TemplateLibrary,
    cache_namespace: String,
}

impl CodexWikiAgent {
    /// Creates the Codex-backed semantic stages used by `llm_wiki`.
    ///
    /// # Errors
    ///
    /// Returns an error if copied prompt templates cannot be compiled.
    pub fn new(config: Config) -> Result<Self> {
        let cache_namespace = analysis_configuration_fingerprint(&config);
        Ok(Self {
            config,
            templates: TemplateLibrary::load()?,
            cache_namespace,
        })
    }

    /// Completes bilingual names in batches using the preserved Python prompt.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid Codex response or a name that still does
    /// not satisfy the English-ASCII/Chinese-CJK contract after three attempts.
    pub async fn translate(
        &self,
        values: &[PartialBilingualName],
        workspace: &Path,
        state_path: &Path,
    ) -> Result<Vec<BilingualName>> {
        let mut output = Vec::with_capacity(values.len());
        for batch in values.chunks(25) {
            output.extend(self.translate_batch(batch, workspace, state_path).await?);
        }
        Ok(output)
    }

    async fn translate_batch(
        &self,
        values: &[PartialBilingualName],
        workspace: &Path,
        _state_path: &Path,
    ) -> Result<Vec<BilingualName>> {
        let payload = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                json!({"key": index.to_string(), "english": value.english, "chinese": value.chinese})
            })
            .collect::<Vec<_>>();
        let input = serde_json::to_string_pretty(&payload)?;
        let mut retry_note = String::new();
        let mut attempts = Vec::with_capacity(3);
        for attempt in 1..=3 {
            let prompt = self.templates.render(
                "agent/wiki_translation.md.j2",
                &json!({
                    "terms_json": &input,
                    "retry_note": &retry_note
                }),
            )?;
            let output = match self
                .run_structured_response(
                    prompt,
                    workspace,
                    StructuredSession::Ephemeral,
                    translation_schema(),
                )
                .await
            {
                Ok(output) => output,
                Err(error) => {
                    let validation_error = format!("model request failed: {error:#}");
                    attempts.push(TranslationAttemptDiagnostic {
                        attempt,
                        output: "<no model output was returned>".to_owned(),
                        validation_error: validation_error.clone(),
                    });
                    retry_note = validation_error;
                    continue;
                }
            };
            let translated = parse_structured_response(&output)
                .and_then(|response| validate_translation_response(&response, values.len()));
            match translated {
                Ok(translated) => return Ok(translated),
                Err(error) => {
                    let validation_error = format!("{error:#}");
                    attempts.push(TranslationAttemptDiagnostic {
                        attempt,
                        output,
                        validation_error: validation_error.clone(),
                    });
                    retry_note = validation_error;
                }
            }
        }
        let diagnostic = translation_failure_diagnostic(&input, &attempts);
        tracing::error!(diagnostic = %diagnostic, "Codex bilingual-name translation failed");
        bail!("{diagnostic}")
    }

    /// Produces and caches structured source knowledge with the copied prompt.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, Codex, schema, translation, or validation
    /// failures.
    pub async fn analyze_source(&self, request: SourceAnalysisRequest) -> Result<SourceAnalysis> {
        self.analyze_source_with_progress(request, |_| {}).await
    }

    /// Plans Markdown-aware chunks using the selected model's configured
    /// character limit. The same deterministic factor preprocessing used by
    /// analysis is included, so the reported total matches actual LLM calls.
    #[must_use]
    pub fn source_chunk_plan(&self, source: &str) -> SourceChunkPlan {
        let prepared = prepare_source_analysis_text(source);
        let max_characters = self.config.max_markdown_chunk_characters();
        SourceChunkPlan {
            chunks: source_analysis_chunks(&prepared.source_text, max_characters).len(),
            max_characters,
        }
    }

    /// Produces structured source knowledge while reporting each completed
    /// Markdown chunk synchronously to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error for Codex, schema, translation, or validation failures.
    pub async fn analyze_source_with_progress<F>(
        &self,
        mut request: SourceAnalysisRequest,
        mut on_chunk: F,
    ) -> Result<SourceAnalysis>
    where
        F: FnMut(SourceChunkProgress),
    {
        let prepared = prepare_source_analysis_text(&request.source_text);
        request.source_text = prepared.source_text;

        let mut analysis = self
            .analyze_source_chunks(request, prepared.formulaic_mode, &mut on_chunk)
            .await?;
        if !prepared.extracted_factors.is_empty() {
            merge_factors(&mut analysis.factors, prepared.extracted_factors);
            validate_analysis(&analysis)?;
        }
        Ok(analysis)
    }

    async fn analyze_source_chunks(
        &self,
        request: SourceAnalysisRequest,
        formulaic_mode: bool,
        on_chunk: &mut impl FnMut(SourceChunkProgress),
    ) -> Result<SourceAnalysis> {
        let max_characters = self.config.max_markdown_chunk_characters();
        let chunks = source_analysis_chunks(&request.source_text, max_characters);
        let total = chunks.len();
        if chunks.len() == 1 {
            let analysis = self.analyze_source_chunk(request, formulaic_mode).await?;
            on_chunk(SourceChunkProgress {
                completed: 1,
                total,
                max_characters,
            });
            return Ok(analysis);
        }

        let mut analyses = Vec::with_capacity(total);
        for (index, source_text) in chunks.into_iter().enumerate() {
            let mut chunk_request = request.clone();
            let focus = if formulaic_mode {
                "The numbered formulas were already extracted. Analyze only the remaining concepts, entities, findings, and methodology"
            } else {
                "Analyze every factor and all other knowledge present in this chunk"
            };
            chunk_request.source_text = format!(
                "BIBIIWIKI source-analysis chunk {}/{}. {focus}. The source_name must name the whole source, not this chunk. Do not infer content from omitted chunks.\n\n{}",
                index + 1,
                total,
                source_text
            );
            analyses.push(
                self.analyze_source_chunk(chunk_request, formulaic_mode)
                    .await?,
            );
            on_chunk(SourceChunkProgress {
                completed: index + 1,
                total,
                max_characters,
            });
        }
        merge_source_analyses(analyses)
    }

    async fn analyze_source_chunk(
        &self,
        request: SourceAnalysisRequest,
        formulaic_mode: bool,
    ) -> Result<SourceAnalysis> {
        let factors_required =
            !formulaic_mode && source_requires_factor_output(&request.source_text);
        let cache = self.source_analysis_cache_path(&request);
        if request.cache_mode == SourceAnalysisCacheMode::Use
            && let Some(analysis) = self
                .cached_source_analysis(&cache, &request.workspace, &request.translation_state_path)
                .await?
        {
            return Ok(analysis);
        }

        let base_prompt = self.templates.render(
            "agent/wiki_source_analysis.md.j2",
            &json!({
                "source_identity": request.source_identity,
                "source_text": request.source_text,
                "purpose": request.purpose,
                "schema": request.schema,
                "wiki_index": request.wiki_index,
                "media_candidates": request.media_candidates.join("\n")
            }),
        )?;
        let schema = source_schema_for_mode(formulaic_mode);
        let mut correction = String::new();
        let mut last_error = None;
        let mut last_output = None;
        for _ in 0..3 {
            let prompt = if correction.is_empty() {
                base_prompt.clone()
            } else {
                format!(
                    "{base_prompt}\n\n## Correction required\nThe previous object failed validation: {correction}\nAnalyze the non-empty source evidence instead of returning blank strings or empty semantic arrays. If the source names, defines, formulates, or implements a factor, `factors_tsv` must contain at least one specific factor row."
                )
            };
            let raw_value = match self
                .run_structured(
                    prompt,
                    &request.workspace,
                    StructuredSession::Ephemeral,
                    schema.clone(),
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    correction = truncate_for_prompt(&format!("{error:#}"), 1000);
                    last_error = Some(error);
                    continue;
                }
            };
            last_output = Some(
                serde_json::to_string_pretty(&raw_value).unwrap_or_else(|_| raw_value.to_string()),
            );
            let value = match normalize_source_value(raw_value, formulaic_mode) {
                Ok(value) => value,
                Err(error) => {
                    correction = truncate_for_prompt(&format!("{error:#}"), 1000);
                    last_error = Some(error);
                    continue;
                }
            };
            match self
                .validated_source_analysis(
                    value,
                    &request.workspace,
                    &request.translation_state_path,
                )
                .await
            {
                Ok((analysis, value)) => {
                    if factors_required && analysis.factors.is_empty() {
                        let error = anyhow::anyhow!(
                            "source chunk contains an explicit factor table or definition, but factors_tsv produced no valid factor rows; return at least one specific factor using exactly ten TAB-separated fields"
                        );
                        correction = error.to_string();
                        last_error = Some(error);
                        continue;
                    }
                    write_source_analysis_cache(&cache, &value)?;
                    return Ok(analysis);
                }
                Err(error) => {
                    correction = truncate_for_prompt(&format!("{error:#}"), 1000);
                    last_error = Some(error);
                }
            }
        }
        let error =
            last_error.unwrap_or_else(|| anyhow::anyhow!("Codex returned no source analysis"));
        if let Some(output) = last_output {
            bail!(
                "{error:#}\nLast structured model output:\n{}",
                truncate_for_prompt(&output, 8_000)
            );
        }
        Err(error)
    }

    fn source_analysis_cache_path(&self, request: &SourceAnalysisRequest) -> PathBuf {
        analysis_cache_path(
            &request.state_path,
            &self.cache_namespace,
            &request.source_identity,
            &request.source_text,
        )
    }

    async fn cached_source_analysis(
        &self,
        cache: &Path,
        workspace: &Path,
        translation_state_path: &Path,
    ) -> Result<Option<SourceAnalysis>> {
        if !cache.exists() {
            return Ok(None);
        }
        let cached = serde_json::from_slice(
            &fs::read(cache).with_context(|| format!("failed to read {}", cache.display()))?,
        )
        .with_context(|| format!("invalid analysis cache {}", cache.display()))?;
        match self
            .validated_source_analysis(cached, workspace, translation_state_path)
            .await
        {
            Ok((analysis, _)) => Ok(Some(analysis)),
            Err(error) => {
                tracing::warn!(
                    cache = %cache.display(),
                    error = %format!("{error:#}"),
                    "ignoring invalid cached source analysis"
                );
                Ok(None)
            }
        }
    }

    async fn validated_source_analysis(
        &self,
        mut value: Value,
        workspace: &Path,
        translation_state_path: &Path,
    ) -> Result<(SourceAnalysis, Value)> {
        self.repair_names(&mut value, workspace, translation_state_path)
            .await?;
        let analysis: SourceAnalysis = serde_json::from_value(value.clone())
            .context("Codex source analysis does not match the wiki contract")?;
        validate_analysis(&analysis)?;
        Ok((analysis, value))
    }

    /// Proposes one structured factor grounded in retrieved wiki context.
    ///
    /// # Errors
    ///
    /// Returns an error when Codex does not return a complete proposal.
    pub async fn propose(
        &self,
        objective: &str,
        wiki_context: &str,
        workspace: &Path,
        state_path: &Path,
    ) -> Result<FactorProposal> {
        let prompt = self.templates.render(
            "agent/factor_proposal.md.j2",
            &json!({"objective": objective.trim(), "wiki_context": wiki_context.trim()}),
        )?;
        let value = self
            .run_structured(
                prompt,
                workspace,
                StructuredSession::Persistent(state_path),
                proposal_schema(),
            )
            .await?;
        let proposal: FactorProposal = serde_json::from_value(value)
            .context("Codex factor proposal does not match the expected contract")?;
        if proposal.name.trim().is_empty() || proposal.polars_expression.trim().is_empty() {
            bail!("Codex factor proposal omitted a name or Polars expression");
        }
        Ok(proposal)
    }

    async fn repair_names(
        &self,
        value: &mut Value,
        workspace: &Path,
        state_path: &Path,
    ) -> Result<()> {
        let mut invalid = Vec::new();
        collect_invalid_names(value, &mut invalid);
        if invalid.is_empty() {
            return Ok(());
        }
        let translations = self.translate(&invalid, workspace, state_path).await?;
        let mut translated = translations.into_iter();
        replace_invalid_names(value, &mut translated);
        Ok(())
    }

    async fn run_structured(
        &self,
        prompt: String,
        workspace: &Path,
        session: StructuredSession<'_>,
        schema: Value,
    ) -> Result<Value> {
        let response = self
            .run_structured_response(prompt, workspace, session, schema)
            .await?;
        parse_structured_response(&response)
    }

    async fn run_structured_response(
        &self,
        prompt: String,
        workspace: &Path,
        session: StructuredSession<'_>,
        schema: Value,
    ) -> Result<String> {
        let prompt = prompt_with_schema(&prompt, &schema)?;
        let provider_model = self
            .config
            .model_list
            .iter()
            .find(|deployment| deployment.model_name == self.config.codex_model())
            .map(|deployment| deployment.litellm_params.model.as_str())
            .unwrap_or_default();
        let codex_schema = codex_output_schema(provider_model, &schema);
        let result = codex_prompt(
            self.config.clone(),
            CodexPromptRequest {
                prompt,
                workspace: workspace.to_path_buf(),
                state_path: session.state_path().map(Path::to_path_buf),
                output_schema: Some(codex_schema),
            },
        )
        .await?;
        Ok(result.response)
    }
}

#[derive(Clone, Copy, Debug)]
enum StructuredSession<'a> {
    /// Source analysis and translation retries are complete, independent prompts.
    /// Resuming them would duplicate the full input and schema in model context.
    Ephemeral,
    Persistent(&'a Path),
}

impl<'a> StructuredSession<'a> {
    fn state_path(self) -> Option<&'a Path> {
        match self {
            Self::Ephemeral => None,
            Self::Persistent(path) => Some(path),
        }
    }
}

#[derive(Clone, Debug)]
struct TranslationAttemptDiagnostic {
    attempt: usize,
    output: String,
    validation_error: String,
}

fn validate_translation_response(
    response: &Value,
    expected_count: usize,
) -> Result<Vec<BilingualName>> {
    let rows = response
        .get("translations")
        .and_then(Value::as_array)
        .context("Codex translation omitted the `translations` array")?;
    if rows.len() != expected_count {
        bail!(
            "Codex translation returned {} rows, but {expected_count} were requested",
            rows.len()
        );
    }
    let mut by_key = rows
        .iter()
        .filter_map(|row| Some((row.get("key")?.as_str()?.to_owned(), row)))
        .collect::<std::collections::HashMap<_, _>>();
    if by_key.len() != rows.len() {
        bail!("Codex translation returned a missing, non-string, or duplicate key");
    }

    let mut translated = Vec::with_capacity(expected_count);
    for index in 0..expected_count {
        let key = index.to_string();
        let row = by_key
            .remove(&key)
            .with_context(|| format!("Codex translation omitted key {key}"))?;
        let english = row
            .get("english")
            .and_then(Value::as_str)
            .with_context(|| format!("Codex translation key {key} omitted string `english`"))?;
        let chinese = row
            .get("chinese")
            .and_then(Value::as_str)
            .with_context(|| format!("Codex translation key {key} omitted string `chinese`"))?;
        translated.push(
            BilingualName::new(ascii_english(english), chinese).with_context(|| {
                format!(
                    "Codex translation key {key} failed bilingual validation; output english={english:?}, chinese={chinese:?}"
                )
            })?,
        );
    }
    Ok(translated)
}

fn translation_failure_diagnostic(
    input: &str,
    attempts: &[TranslationAttemptDiagnostic],
) -> String {
    use std::fmt::Write as _;

    let mut diagnostic = String::from(
        "Codex could not produce valid bilingual names after three attempts.\nBilingual translation input:\n",
    );
    diagnostic.push_str(input);
    for attempt in attempts {
        let _ = write!(
            diagnostic,
            "\n\nAttempt {} validation error:\n{}\nAttempt {} model output:\n{}",
            attempt.attempt, attempt.validation_error, attempt.attempt, attempt.output
        );
    }
    diagnostic
}

fn codex_output_schema(provider_model: &str, required_schema: &Value) -> Value {
    if provider_model
        .split_once('/')
        .is_some_and(|(provider, _)| provider == "ollama")
    {
        return ollama_top_level_schema(required_schema);
    }
    required_schema.clone()
}

fn ollama_top_level_schema(required_schema: &Value) -> Value {
    let properties = required_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, schema)| {
                    let value_type = schema
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("string");
                    let mut compact = json!({"type": value_type});
                    if let Some(maximum) = schema.get("maxLength") {
                        compact["maxLength"] = maximum.clone();
                    }
                    (name.clone(), compact)
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required_schema
            .get("required")
            .cloned()
            .unwrap_or_else(|| json!([]))
    })
}

fn prompt_with_schema(prompt: &str, schema: &Value) -> Result<String> {
    Ok(format!(
        "{}\n\n## Required JSON contract\nReturn one JSON object that matches this schema exactly. Include every required field even when its value is an empty array or empty string. Do not rename fields or add Markdown fences.\n\n```json\n{}\n```",
        prompt.trim_end(),
        serde_json::to_string_pretty(schema)?
    ))
}

fn parse_structured_response(response: &str) -> Result<Value> {
    let response = response.trim();
    if let Ok(value) = serde_json::from_str(response) {
        return Ok(value);
    }
    let unfenced = response
        .strip_prefix("```json")
        .or_else(|| response.strip_prefix("```JSON"))
        .or_else(|| response.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map_or(response, str::trim);
    if let Ok(value) = serde_json::from_str(unfenced) {
        return Ok(value);
    }
    if let Some(start) = response.find('{')
        && let Some(end) = response.rfind('}')
        && start < end
        && let Ok(value) = serde_json::from_str(&response[start..=end])
    {
        return Ok(value);
    }
    let preview = response.chars().take(500).collect::<String>();
    bail!("Codex wiki response was not valid JSON; response began: {preview:?}")
}

fn truncate_for_prompt(value: &str, maximum_chars: usize) -> String {
    let mut output = value.chars().take(maximum_chars).collect::<String>();
    if value.chars().count() > maximum_chars {
        output.push('…');
    }
    output
}

fn collect_invalid_names(value: &Value, output: &mut Vec<PartialBilingualName>) {
    match value {
        Value::Object(object) => {
            if let (Some(english), Some(chinese)) = (
                object.get("english").and_then(Value::as_str),
                object.get("chinese").and_then(Value::as_str),
            ) && BilingualName::new(english, chinese).is_err()
            {
                output.push(PartialBilingualName {
                    english: english.to_string(),
                    chinese: chinese.to_string(),
                });
            }
            for child in object.values() {
                collect_invalid_names(child, output);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_invalid_names(child, output);
            }
        }
        _ => {}
    }
}

fn replace_invalid_names(value: &mut Value, translated: &mut impl Iterator<Item = BilingualName>) {
    match value {
        Value::Object(object) => {
            let invalid = object
                .get("english")
                .and_then(Value::as_str)
                .zip(object.get("chinese").and_then(Value::as_str))
                .is_some_and(|(english, chinese)| BilingualName::new(english, chinese).is_err());
            if invalid && let Some(name) = translated.next() {
                object.insert("english".to_string(), Value::String(name.english));
                object.insert("chinese".to_string(), Value::String(name.chinese));
            }
            for child in object.values_mut() {
                replace_invalid_names(child, translated);
            }
        }
        Value::Array(values) => {
            for child in values {
                replace_invalid_names(child, translated);
            }
        }
        _ => {}
    }
}

fn validate_analysis(analysis: &SourceAnalysis) -> Result<()> {
    BilingualName::new(&analysis.source_name.english, &analysis.source_name.chinese)?;
    if analysis.summary.trim().is_empty() {
        bail!("Codex source analysis returned an empty summary");
    }
    if analysis.key_concepts.is_empty()
        && analysis.entities.is_empty()
        && analysis.factors.is_empty()
        && analysis.methodology.terms.is_empty()
    {
        bail!(
            "Codex source analysis returned no addressable concepts, entities, factors, or methodology terms"
        );
    }
    for (english, chinese) in analysis
        .key_concepts
        .iter()
        .map(|item| (&item.english, &item.chinese))
        .chain(
            analysis
                .entities
                .iter()
                .map(|item| (&item.english, &item.chinese)),
        )
        .chain(
            analysis
                .methodology
                .terms
                .iter()
                .map(|item| (&item.english, &item.chinese)),
        )
        .chain(
            analysis
                .factors
                .iter()
                .map(|item| (&item.english, &item.chinese)),
        )
        .chain(
            analysis
                .selected_media
                .iter()
                .map(|item| (&item.english, &item.chinese)),
        )
    {
        BilingualName::new(english, chinese)?;
    }
    Ok(())
}

fn source_requires_factor_output(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    (source.contains("大类因子") && source.contains("具体因子"))
        || source.contains("主要因子及其描述")
        || source.contains("主要的风格因子")
        || lower.contains("factor formula")
        || (lower.contains("factor name") && lower.contains("factor description"))
}

fn source_analysis_chunks(source: &str, maximum_chars: usize) -> Vec<String> {
    let config = ChunkConfig::new(maximum_chars.max(1)).with_trim(false);
    let splitter = MarkdownSplitter::new(config);
    let chunks = splitter
        .chunks(source)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        vec![source.to_owned()]
    } else {
        chunks
    }
}

struct PreparedSourceAnalysisText {
    source_text: String,
    extracted_factors: Vec<SourceFactor>,
    formulaic_mode: bool,
}

fn prepare_source_analysis_text(source: &str) -> PreparedSourceAnalysisText {
    let (semantic_source, extracted_factors) = extract_numbered_formulaic_factors(source);
    let formulaic_mode = extracted_factors.len() >= DETERMINISTIC_FORMULAIC_FACTOR_THRESHOLD;
    if formulaic_mode {
        PreparedSourceAnalysisText {
            source_text: format!(
                "BIBIIWIKI deterministic-factor note: {} numbered formula definitions were already extracted losslessly and will be merged after semantic analysis. Do not recreate those numbered formulas in `factors`; analyze the remaining concepts, entities, findings, and methodology.\n\n{}",
                extracted_factors.len(),
                semantic_source
            ),
            extracted_factors,
            formulaic_mode,
        }
    } else {
        PreparedSourceAnalysisText {
            source_text: source.to_owned(),
            extracted_factors: Vec::new(),
            formulaic_mode,
        }
    }
}

fn extract_numbered_formulaic_factors(source: &str) -> (String, Vec<SourceFactor>) {
    let lines = source.lines().collect::<Vec<_>>();
    let mut semantic_source = String::with_capacity(source.len());
    let mut factors = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some((number, first_line)) = numbered_alpha_start(lines[index]) else {
            semantic_source.push_str(lines[index]);
            semantic_source.push('\n');
            index += 1;
            continue;
        };

        let mut formula_parts = vec![first_line.to_string()];
        index += 1;
        while index < lines.len() && !lines[index].trim().is_empty() {
            if numbered_alpha_start(lines[index]).is_some()
                || lines[index].trim_start().starts_with("## Page ")
            {
                break;
            }
            formula_parts.push(lines[index].trim().to_string());
            index += 1;
        }
        let formula = formula_parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let evidence = format!("Alpha#{number}: {formula}");
        factors.push(SourceFactor {
            english: format!("Formulaic Alpha {number}"),
            chinese: format!("公式化 Alpha {number}"),
            definition: format!(
                "Numbered formulaic alpha {number}, preserved directly from the source."
            ),
            category: "Formulaic alpha".to_string(),
            required_inputs: formula_required_inputs(&formula),
            formula,
            polars_expression: String::new(),
            scenario: "Use only after mapping the source operators to the target data model."
                .to_string(),
            evaluation: "Requires operator validation and historical backtesting before use."
                .to_string(),
            evidence: vec![evidence],
        });

        while index < lines.len() && lines[index].trim().is_empty() {
            semantic_source.push('\n');
            index += 1;
        }
    }
    (semantic_source, factors)
}

fn numbered_alpha_start(line: &str) -> Option<(u32, &str)> {
    let rest = line.trim_start().strip_prefix("Alpha#")?;
    let digit_count = rest.chars().take_while(char::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let (digits, rest) = rest.split_at(digit_count);
    let formula = rest.strip_prefix(':')?.trim();
    Some((digits.parse().ok()?, formula))
}

fn formula_required_inputs(formula: &str) -> Vec<String> {
    const FIELDS: &[&str] = &[
        "cap", "close", "high", "indclass", "low", "open", "returns", "volume", "vwap",
    ];
    let mut inputs = formula
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|token| {
            FIELDS.contains(&token.as_str())
                || token.strip_prefix("adv").is_some_and(|window| {
                    !window.is_empty() && window.chars().all(|c| c.is_ascii_digit())
                })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    inputs.sort();
    inputs
}

fn merge_source_analyses(analyses: Vec<SourceAnalysis>) -> Result<SourceAnalysis> {
    let mut analyses = analyses.into_iter();
    let mut merged = analyses
        .next()
        .context("source analysis returned no chunks")?;
    for mut analysis in analyses {
        append_distinct_text(&mut merged.summary, &analysis.summary);
        merge_knowledge_terms(&mut merged.key_concepts, analysis.key_concepts);
        merge_strings(&mut merged.findings, analysis.findings);
        merge_strings(&mut merged.connections, analysis.connections);
        merge_strings(&mut merged.tensions, analysis.tensions);
        merge_strings(&mut merged.recommendations, analysis.recommendations);
        merge_knowledge_terms(&mut merged.entities, analysis.entities);
        merged.selected_media.append(&mut analysis.selected_media);
        merge_methodology(&mut merged.methodology, analysis.methodology);
        merge_factors(&mut merged.factors, analysis.factors);
    }
    dedupe_by_key(&mut merged.selected_media, |item| {
        item.asset_key.trim().to_ascii_lowercase()
    });
    validate_analysis(&merged)?;
    Ok(merged)
}

fn merge_knowledge_terms(target: &mut Vec<KnowledgeTerm>, values: Vec<KnowledgeTerm>) {
    for mut value in values {
        if let Some(existing) = target.iter_mut().find(|existing| {
            existing.english.eq_ignore_ascii_case(&value.english)
                && existing.chinese == value.chinese
        }) {
            append_distinct_text(&mut existing.definition, &value.definition);
            merge_strings(&mut existing.evidence, std::mem::take(&mut value.evidence));
        } else {
            target.push(value);
        }
    }
}

fn merge_methodology(target: &mut SourceMethodology, mut value: SourceMethodology) {
    append_distinct_text(&mut target.definition, &value.definition);
    for mut term in value.terms.drain(..) {
        if let Some(existing) = target.terms.iter_mut().find(|existing| {
            existing.english.eq_ignore_ascii_case(&term.english) && existing.chinese == term.chinese
        }) {
            append_distinct_text(&mut existing.definition, &term.definition);
            merge_strings(&mut existing.evidence, std::mem::take(&mut term.evidence));
        } else {
            target.terms.push(term);
        }
    }
    merge_strings(&mut target.data_inputs, value.data_inputs);
    merge_strings(&mut target.benefits, value.benefits);
    merge_strings(&mut target.limitations, value.limitations);
    merge_strings(&mut target.coverage_gaps, value.coverage_gaps);
    merge_strings(&mut target.connections, value.connections);
    if target.review_status.trim().is_empty() {
        target.review_status = value.review_status;
    }
}

fn merge_factors(target: &mut Vec<SourceFactor>, values: Vec<SourceFactor>) {
    for mut value in values {
        if let Some(existing) = target.iter_mut().find(|existing| {
            existing.english.eq_ignore_ascii_case(&value.english)
                && existing.chinese == value.chinese
        }) {
            append_distinct_text(&mut existing.definition, &value.definition);
            fill_empty(&mut existing.category, &value.category);
            fill_empty(&mut existing.formula, &value.formula);
            fill_empty(&mut existing.polars_expression, &value.polars_expression);
            fill_empty(&mut existing.scenario, &value.scenario);
            append_distinct_text(&mut existing.evaluation, &value.evaluation);
            merge_strings(
                &mut existing.required_inputs,
                std::mem::take(&mut value.required_inputs),
            );
            merge_strings(&mut existing.evidence, std::mem::take(&mut value.evidence));
        } else {
            target.push(value);
        }
    }
}

fn merge_strings(target: &mut Vec<String>, values: Vec<String>) {
    let mut seen = target
        .iter()
        .map(|value| value.trim().to_lowercase())
        .collect::<BTreeSet<_>>();
    target.extend(values.into_iter().filter(|value| {
        let normalized = value.trim().to_lowercase();
        !normalized.is_empty() && seen.insert(normalized)
    }));
}

fn append_distinct_text(target: &mut String, value: &str) {
    let value = value.trim();
    if value.is_empty() || target.contains(value) {
        return;
    }
    if !target.trim().is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(value);
}

fn fill_empty(target: &mut String, value: &str) {
    if target.trim().is_empty() && !value.trim().is_empty() {
        value.trim().clone_into(target);
    }
}

fn dedupe_by_key<T>(values: &mut Vec<T>, key: impl Fn(&T) -> String) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(key(value)));
}

fn analysis_configuration_fingerprint(config: &Config) -> String {
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
        "{ANALYZER_IDENTITY}\0{alias}\0{provider_model}\0{api_base}\0{}\0{}\0{}",
        config.codex.reasoning_effort.as_deref().unwrap_or_default(),
        config.max_markdown_chunk_characters(),
        config.server.default_max_output_tokens
    );
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

fn analysis_cache_path(
    state_path: &Path,
    cache_namespace: &str,
    identity: &str,
    source_text: &str,
) -> PathBuf {
    let digest = Sha256::digest(
        format!("{ANALYZER_IDENTITY}\0{cache_namespace}\0{identity}\0{source_text}").as_bytes(),
    );
    let prefix = format!("{digest:x}");
    state_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("wiki_analysis_cache")
        .join(format!("{}.json", &prefix[..20]))
}

fn write_source_analysis_cache(cache: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(cache, format!("{}\n", serde_json::to_string_pretty(value)?))
        .with_context(|| format!("failed to write {}", cache.display()))
}

fn ascii_english(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            other => other,
        })
        .collect()
}

#[cfg(test)]
fn bounded_text(maximum_length: usize) -> Value {
    json!({"type": "string", "maxLength": maximum_length})
}

#[cfg(test)]
fn string_array(maximum_items: usize, maximum_length: usize) -> Value {
    json!({
        "type": "array",
        "maxItems": maximum_items,
        "items": bounded_text(maximum_length)
    })
}

#[cfg(test)]
fn name_properties() -> Value {
    json!({"english": bounded_text(100), "chinese": bounded_text(100)})
}

fn translation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {"translations": {"type": "array", "items": {
            "type": "object", "additionalProperties": false,
            "properties": {"key": {"type": "string"}, "english": {"type": "string"}, "chinese": {"type": "string"}},
            "required": ["key", "english", "chinese"]
        }}},
        "required": ["translations"]
    })
}

#[cfg(test)]
fn knowledge_schema(methodology: bool) -> Value {
    let mut properties = name_properties().as_object().expect("object").clone();
    if methodology {
        properties.insert("term_type".to_string(), json!({"type": "string"}));
    }
    properties.insert("definition".to_string(), bounded_text(240));
    properties.insert("evidence".to_string(), string_array(1, 160));
    let mut required = vec!["english", "chinese"];
    if methodology {
        required.push("term_type");
    }
    required.extend(["definition", "evidence"]);
    json!({
        "type": "object", "additionalProperties": false,
        "properties": properties, "required": required
    })
}

#[cfg(test)]
fn source_analysis_schema() -> Value {
    let factor_properties = json!({
        "english": {"type": "string"}, "chinese": {"type": "string"},
        "definition": bounded_text(240), "category": bounded_text(80),
        "formula": bounded_text(600), "required_inputs": string_array(12, 80),
        "polars_expression": bounded_text(600), "scenario": bounded_text(240),
        "evaluation": bounded_text(240), "evidence": string_array(1, 160)
    });
    json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "source_name": {"type": "object", "additionalProperties": false, "properties": name_properties(), "required": ["english", "chinese"]},
            "summary": bounded_text(300),
            "key_concepts": {"type": "array", "maxItems": 2, "items": knowledge_schema(false)},
            "entities": {"type": "array", "maxItems": 2, "items": knowledge_schema(false)},
            "findings": string_array(3, 180), "connections": string_array(2, 180),
            "tensions": string_array(2, 180), "recommendations": string_array(2, 180),
            "selected_media": {"type": "array", "maxItems": 2, "items": {"type": "object", "additionalProperties": false,
                "properties": {"asset_key": bounded_text(160), "english": bounded_text(100), "chinese": bounded_text(100), "description": bounded_text(200)},
                "required": ["asset_key", "english", "chinese", "description"]}},
            "factors": {"type": "array", "maxItems": 2, "items": {"type": "object", "additionalProperties": false,
                "properties": factor_properties,
                "required": ["english", "chinese", "definition", "category", "formula", "required_inputs", "polars_expression", "scenario", "evaluation", "evidence"]}},
            "methodology": {"type": "object", "additionalProperties": false,
                "properties": {"definition": bounded_text(300), "terms": {"type": "array", "maxItems": 2, "items": knowledge_schema(true)},
                    "data_inputs": string_array(12, 80), "benefits": string_array(2, 180), "limitations": string_array(2, 180),
                    "coverage_gaps": string_array(2, 180), "connections": string_array(2, 180), "review_status": bounded_text(80)},
                "required": ["definition", "terms", "data_inputs", "benefits", "limitations", "coverage_gaps", "connections", "review_status"]}
        },
        "required": ["source_name", "summary", "key_concepts", "entities", "findings", "connections", "tensions", "recommendations", "selected_media", "factors", "methodology"]
    })
}

fn source_schema_for_mode(formulaic_mode: bool) -> Value {
    if formulaic_mode {
        formulaic_semantic_schema()
    } else {
        compact_source_analysis_schema()
    }
}

fn normalize_source_value(value: Value, formulaic_mode: bool) -> Result<Value> {
    if formulaic_mode {
        expand_formulaic_semantic_value(&value)
    } else if value.get("source_name").is_some() {
        normalize_full_source_value(value)
    } else {
        expand_compact_source_analysis_value(&value)
    }
}

fn normalize_full_source_value(mut value: Value) -> Result<Value> {
    let object = value
        .as_object_mut()
        .context("Codex source analysis was not a JSON object")?;
    for name in [
        "key_concepts",
        "entities",
        "findings",
        "connections",
        "tensions",
        "recommendations",
        "selected_media",
        "factors",
    ] {
        object
            .entry(name.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    let methodology = object.entry("methodology".to_string()).or_insert_with(|| {
        json!({
            "definition": "",
            "terms": [],
            "data_inputs": [],
            "benefits": [],
            "limitations": [],
            "coverage_gaps": [],
            "connections": [],
            "review_status": "generated"
        })
    });
    let methodology = methodology
        .as_object_mut()
        .context("Codex source analysis methodology was not a JSON object")?;
    for name in [
        "terms",
        "data_inputs",
        "benefits",
        "limitations",
        "coverage_gaps",
        "connections",
    ] {
        methodology
            .entry(name.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    methodology
        .entry("definition".to_string())
        .or_insert_with(|| Value::String(String::new()));
    methodology
        .entry("review_status".to_string())
        .or_insert_with(|| Value::String("generated".to_string()));
    Ok(value)
}

fn formulaic_semantic_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "source_english": {"type": "string", "maxLength": 100},
            "source_chinese": {"type": "string", "maxLength": 100},
            "summary": {"type": "string", "maxLength": 400},
            "concepts_tsv": {"type": "string", "maxLength": 1200},
            "entities_tsv": {"type": "string", "maxLength": 800},
            "findings_lines": {"type": "string", "maxLength": 600},
            "methodology_definition": {"type": "string", "maxLength": 400},
            "methodology_terms_tsv": {"type": "string", "maxLength": 1200},
            "data_inputs_csv": {"type": "string", "maxLength": 400}
        },
        "required": ["source_english", "source_chinese", "summary", "concepts_tsv", "entities_tsv", "findings_lines", "methodology_definition", "methodology_terms_tsv", "data_inputs_csv"]
    })
}

fn compact_source_analysis_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "source_english": {"type": "string", "maxLength": 100},
            "source_chinese": {"type": "string", "maxLength": 100},
            "summary": {"type": "string", "maxLength": 300},
            "concepts_tsv": {"type": "string", "maxLength": 800},
            "entities_tsv": {"type": "string", "maxLength": 600},
            "findings_lines": {"type": "string", "maxLength": 400},
            "factors_tsv": {"type": "string", "maxLength": 3600},
            "methodology_definition": {"type": "string", "maxLength": 300},
            "methodology_terms_tsv": {"type": "string", "maxLength": 800},
            "data_inputs_csv": {"type": "string", "maxLength": 400},
            "benefits_lines": {"type": "string", "maxLength": 300},
            "limitations_lines": {"type": "string", "maxLength": 300}
        },
        "required": [
            "source_english", "source_chinese", "summary", "concepts_tsv",
            "entities_tsv", "findings_lines", "factors_tsv",
            "methodology_definition", "methodology_terms_tsv", "data_inputs_csv",
            "benefits_lines", "limitations_lines"
        ]
    })
}

fn expand_compact_source_analysis_value(value: &Value) -> Result<Value> {
    let mut object = value
        .as_object()
        .cloned()
        .context("Codex compact source analysis was not an object")?;
    let mut take = |name: &str| -> Result<String> {
        object
            .remove(name)
            .with_context(|| format!("Codex compact source analysis omitted {name}"))?
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("Codex compact source analysis {name} was not text"))
    };
    let source_english = take("source_english")?;
    let source_chinese = take("source_chinese")?;
    let summary = take("summary")?;
    let key_concepts = compact_knowledge_rows(&take("concepts_tsv")?, false);
    let entities = compact_knowledge_rows(&take("entities_tsv")?, false);
    let findings = compact_lines(&take("findings_lines")?, 3);
    let factors = compact_factor_rows(&take("factors_tsv")?);
    let methodology_definition = take("methodology_definition")?;
    let methodology_terms = compact_knowledge_rows(&take("methodology_terms_tsv")?, true);
    let data_inputs = compact_csv(&take("data_inputs_csv")?);
    let benefits = compact_lines(&take("benefits_lines")?, 2);
    let limitations = compact_lines(&take("limitations_lines")?, 2);
    normalize_full_source_value(json!({
        "source_name": {"english": source_english, "chinese": source_chinese},
        "summary": summary,
        "key_concepts": key_concepts,
        "entities": entities,
        "findings": findings,
        "connections": [],
        "tensions": [],
        "recommendations": [],
        "selected_media": [],
        "factors": factors,
        "methodology": {
            "definition": methodology_definition,
            "terms": methodology_terms,
            "data_inputs": data_inputs,
            "benefits": benefits,
            "limitations": limitations,
            "coverage_gaps": [],
            "connections": [],
            "review_status": "generated"
        }
    }))
}

fn expand_formulaic_semantic_value(value: &Value) -> Result<Value> {
    let mut object = value
        .as_object()
        .cloned()
        .context("Codex formulaic semantic analysis was not an object")?;
    let mut take = |name: &str| -> Result<String> {
        object
            .remove(name)
            .with_context(|| format!("Codex formulaic semantic analysis omitted {name}"))?
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("Codex formulaic semantic analysis {name} was not text"))
    };
    let source_english = take("source_english")?;
    let source_chinese = take("source_chinese")?;
    let summary = take("summary")?;
    let key_concepts = compact_knowledge_rows(&take("concepts_tsv")?, false);
    let entities = compact_knowledge_rows(&take("entities_tsv")?, false);
    let findings = compact_lines(&take("findings_lines")?, 3);
    let methodology_definition = take("methodology_definition")?;
    let mut methodology_terms = compact_knowledge_rows(&take("methodology_terms_tsv")?, true);
    if methodology_terms.is_empty() {
        methodology_terms.push(json!({
            "english": "Formulaic Alpha Analysis",
            "chinese": "公式化阿尔法分析",
            "term_type": "analysis",
            "definition": methodology_definition,
            "evidence": [summary]
        }));
    }
    let data_inputs = compact_csv(&take("data_inputs_csv")?);
    Ok(json!({
        "source_name": {"english": source_english, "chinese": source_chinese},
        "summary": summary,
        "key_concepts": key_concepts,
        "entities": entities,
        "findings": findings,
        "connections": [],
        "tensions": [],
        "recommendations": [],
        "selected_media": [],
        "factors": [],
        "methodology": {
            "definition": methodology_definition,
            "terms": methodology_terms,
            "data_inputs": data_inputs,
            "benefits": [],
            "limitations": [],
            "coverage_gaps": [],
            "connections": [],
            "review_status": "generated"
        }
    }))
}

fn compact_knowledge_rows(value: &str, methodology: bool) -> Vec<Value> {
    value
        .lines()
        .filter_map(|line| {
            let fields = line
                .trim()
                .trim_start_matches(['-', '*', ' '])
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>();
            let expected = if methodology { 5 } else { 4 };
            if fields.len() != expected || fields.iter().any(|field| field.is_empty()) {
                return None;
            }
            if methodology {
                Some(json!({
                    "english": fields[0], "chinese": fields[1], "term_type": fields[2],
                    "definition": fields[3], "evidence": [fields[4]]
                }))
            } else {
                Some(json!({
                    "english": fields[0], "chinese": fields[1],
                    "definition": fields[2], "evidence": [fields[3]]
                }))
            }
        })
        .take(4)
        .collect()
}

fn compact_factor_rows(value: &str) -> Vec<Value> {
    value
        .lines()
        .filter_map(|line| {
            let mut fields = if line.contains('\t') {
                line.split('\t').map(str::trim).collect::<Vec<_>>()
            } else if line.contains("\\t") {
                line.split("\\t").map(str::trim).collect::<Vec<_>>()
            } else {
                line.split('|').map(str::trim).collect::<Vec<_>>()
            };
            if !(5..=10).contains(&fields.len())
                || fields[0].is_empty()
                || fields[1].is_empty()
            {
                return None;
            }
            // Smaller local models commonly follow the semantic order but use
            // pipes and omit trailing optional fields despite the requested
            // ten-column TSV contract. Preserve the useful grounded fields and
            // represent only genuinely missing optional values as empty.
            fields.resize(10, "");
            Some(json!({
                "english": fields[0],
                "chinese": fields[1],
                "category": fields[2],
                "definition": fields[3],
                "formula": fields[4],
                "required_inputs": compact_csv(fields[5]),
                "polars_expression": fields[6],
                "scenario": fields[7],
                "evaluation": fields[8],
                "evidence": if fields[9].is_empty() { Vec::<String>::new() } else { vec![fields[9].to_string()] }
            }))
        })
        .take(2)
        .collect()
}

fn compact_lines(value: &str, maximum: usize) -> Vec<String> {
    value
        .lines()
        .map(|line| line.trim().trim_start_matches(['-', '*', ' ']).trim())
        .filter(|line| !line.is_empty())
        .take(maximum)
        .map(str::to_string)
        .collect()
}

fn compact_csv(value: &str) -> Vec<String> {
    value
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .take(12)
        .map(str::to_string)
        .collect()
}

fn proposal_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false,
        "properties": {"name": {"type": "string"}, "hypothesis": {"type": "string"}, "polars_expression": {"type": "string"}},
        "required": ["name", "hypothesis", "polars_expression"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_analysis_uses_an_ephemeral_codex_session() {
        assert_eq!(
            StructuredSession::Ephemeral.state_path(),
            None,
            "independent source chunks and retries must not resume a stale Codex thread"
        );
    }

    #[test]
    fn source_analysis_cache_is_namespaced_by_model_and_chunk_configuration() {
        let first = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/model-a
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 9000
codex:
  model: local
",
        )
        .expect("first analysis configuration");
        let changed_model = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/model-b
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 9000
codex:
  model: local
",
        )
        .expect("changed model configuration");
        let changed_chunk_size = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/model-a
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 12000
codex:
  model: local
",
        )
        .expect("changed chunk configuration");

        let first = analysis_configuration_fingerprint(&first);
        let changed_model = analysis_configuration_fingerprint(&changed_model);
        let changed_chunk_size = analysis_configuration_fingerprint(&changed_chunk_size);

        assert_ne!(first, changed_model);
        assert_ne!(first, changed_chunk_size);
        assert_ne!(
            analysis_cache_path(Path::new("state.json"), &first, "source", "body"),
            analysis_cache_path(Path::new("state.json"), &changed_model, "source", "body")
        );
    }

    #[test]
    fn bilingual_prompt_requires_replacing_cjk_in_the_english_slot() {
        let prompt = TemplateLibrary::load()
            .expect("prompt library")
            .render(
                "agent/wiki_translation.md.j2",
                &json!({
                    "terms_json": r#"[{"key":"0","english":"HSBC Multi-Factor Model System Exploration (华泰多因子模型体系初探)","chinese":"华泰多因子模型体系初探"}]"#,
                    "retry_note": "key 0: English term is invalid because it contains CJK"
                }),
            )
            .expect("translation prompt");

        assert!(prompt.contains("Never copy Chinese/CJK characters into `english`"));
        assert!(prompt.contains("If `english` contains any Chinese/CJK characters"));
        assert!(prompt.contains("Huatai Multi-Factor Model Framework"));
        assert!(prompt.contains("key 0: English term is invalid"));
    }

    #[test]
    fn bilingual_failure_diagnostic_prints_input_output_and_validation_reason() {
        let input = r#"[{"key":"0","english":"Factor (因子)","chinese":"因子"}]"#;
        let attempts = vec![TranslationAttemptDiagnostic {
            attempt: 1,
            output: r#"{"translations":[{"key":"0","english":"Factor (因子)","chinese":"因子"}]}"#
                .to_owned(),
            validation_error: "key 0: English term is invalid because it contains CJK".to_owned(),
        }];

        let diagnostic = translation_failure_diagnostic(input, &attempts);

        assert!(diagnostic.contains("Bilingual translation input:"));
        assert!(diagnostic.contains(input));
        assert!(diagnostic.contains("Attempt 1 model output:"));
        assert!(diagnostic.contains(&attempts[0].output));
        assert!(diagnostic.contains(&attempts[0].validation_error));
    }

    #[test]
    fn bilingual_validation_identifies_the_invalid_output_field_and_value() {
        let response = json!({"translations": [{
            "key": "0",
            "english": "Factor (因子)",
            "chinese": "因子"
        }]});

        let error = validate_translation_response(&response, 1)
            .expect_err("CJK in the English slot must be rejected");
        let detail = format!("{error:#}");

        assert!(detail.contains("key 0 failed bilingual validation"));
        assert!(detail.contains("output english=\"Factor (因子)\""));
        assert!(detail.contains("English term is invalid"));
    }

    #[tokio::test]
    #[ignore = "requires a live local Ornith model through Codex"]
    async fn ornith_normalizes_the_huatai_cjk_english_regression() {
        let config = Config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("bibiiwiki.yaml"))
            .expect("live test configuration");
        let translated = CodexWikiAgent::new(config)
            .expect("wiki agent")
            .translate(
                &[PartialBilingualName {
                    english: "HSBC Multi-Factor Model System Exploration (华泰多因子模型体系初探)"
                        .to_owned(),
                    chinese: "华泰多因子模型体系初探".to_owned(),
                }],
                Path::new(env!("CARGO_MANIFEST_DIR")),
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("target/huatai-translation-regression-state.json"),
            )
            .await
            .expect("Ornith should repair the bilingual name");

        assert_eq!(translated.len(), 1);
        assert!(translated[0].english.is_ascii());
        assert!(!translated[0].english.contains("HSBC"));
        assert!(translated[0].english.contains("Huatai"));
        assert_eq!(translated[0].chinese, "华泰多因子模型体系初探");
    }

    #[test]
    fn source_analysis_chunks_are_bounded_utf8_safe_and_lossless() {
        let source = "第一页 alpha\n第二页 beta\n第三页 gamma\n";
        let chunks = source_analysis_chunks(source, 9);

        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 9));
        assert_eq!(chunks.concat(), source);
    }

    #[test]
    fn source_analysis_schema_bounds_local_model_output() {
        let schema = source_analysis_schema();

        assert_eq!(schema["properties"]["key_concepts"]["maxItems"], 2);
        assert_eq!(schema["properties"]["entities"]["maxItems"], 2);
        assert_eq!(schema["properties"]["factors"]["maxItems"], 2);
        assert_eq!(
            schema["properties"]["factors"]["items"]["properties"]["evidence"]["maxItems"],
            1
        );
        assert_eq!(
            schema["properties"]["methodology"]["properties"]["terms"]["maxItems"],
            2
        );
        assert_eq!(schema["properties"]["summary"]["maxLength"], 300);
    }

    #[test]
    fn full_source_normalization_fills_omitted_empty_collections() {
        let partial = json!({
            "source_name": {"english": "Huatai Multi-Factor Study", "chinese": "华泰多因子研究"},
            "summary": "A concise factor-research summary.",
            "key_concepts": [{
                "english": "Factor Investing", "chinese": "因子投资",
                "definition": "Investing with systematic signals.", "evidence": ["The report studies factors."]
            }],
            "methodology": {
                "definition": "Evaluate and combine factors.",
                "terms": [],
                "data_inputs": [],
                "benefits": [],
                "limitations": [],
                "connections": [],
                "review_status": "generated"
            }
        });

        let normalized = normalize_source_value(partial, false).unwrap();

        for name in [
            "entities",
            "findings",
            "connections",
            "tensions",
            "recommendations",
            "selected_media",
            "factors",
        ] {
            assert_eq!(normalized[name], json!([]), "missing default for {name}");
        }
        assert_eq!(normalized["methodology"]["coverage_gaps"], json!([]));
        let analysis: SourceAnalysis = serde_json::from_value(normalized).unwrap();
        validate_analysis(&analysis).unwrap();
    }

    #[test]
    fn compact_source_semantics_expand_to_factors_and_methodology() {
        let compact = json!({
            "source_english": "Huatai Multi-Factor Model Study",
            "source_chinese": "华泰多因子模型研究",
            "summary": "The report evaluates systematic factor construction and risk control.",
            "concepts_tsv": "Factor Investing|因子投资|Systematic investing with measurable signals.|The report evaluates multiple factors.",
            "entities_tsv": "Huatai Securities|华泰证券|The report publisher.|The title identifies Huatai.",
            "findings_lines": "Factor returns require risk adjustment.",
            "factors_tsv": "Standard Deviation Risk\t标准差风险\tRisk\tMeasures return dispersion.\tStd(r)\tret\tpl.col('ret').rolling_std(20).over('asset_code')\tRisk monitoring.\tUse with expected return.\tThe report defines standard deviation risk.",
            "methodology_definition": "Evaluate, neutralize, and combine factor signals.",
            "methodology_terms_tsv": "Factor Evaluation|因子评估|evaluation|Assess factor return and risk.|The report compares factor performance.",
            "data_inputs_csv": "ret, asset_code",
            "benefits_lines": "Separates signal return from risk.",
            "limitations_lines": "Historical estimates may not persist."
        });

        let normalized = normalize_source_value(compact, false).unwrap();
        let analysis: SourceAnalysis = serde_json::from_value(normalized).unwrap();

        assert_eq!(analysis.factors.len(), 1);
        assert_eq!(analysis.factors[0].english, "Standard Deviation Risk");
        assert_eq!(analysis.factors[0].required_inputs, ["ret"]);
        assert_eq!(analysis.methodology.terms.len(), 1);
        assert_eq!(analysis.methodology.benefits.len(), 1);
        validate_analysis(&analysis).unwrap();
    }

    #[test]
    fn compact_factors_accept_pipe_rows_from_local_models() {
        let factors = compact_factor_rows(
            "Value Factor|估值因子|value|Measures cheapness.|earnings / market_cap",
        );

        assert_eq!(factors.len(), 1);
        assert_eq!(factors[0]["english"], "Value Factor");
        assert_eq!(factors[0]["chinese"], "估值因子");
        assert_eq!(factors[0]["formula"], "earnings / market_cap");
        assert_eq!(factors[0]["required_inputs"], json!([]));
    }

    #[test]
    fn explicit_factor_tables_require_factor_output() {
        assert!(source_requires_factor_output(
            "表格 3：主要因子及其描述\n|大类因子|具体因子|因子描述|\n|估值因子|EP BP|盈利收益率和账面市值比|"
        ));
        assert!(source_requires_factor_output(
            "The source defines the following factor formula and required inputs."
        ));
        assert!(!source_requires_factor_output(
            "A multi-factor model reduces the dimension of portfolio risk."
        ));
    }

    #[test]
    fn numbered_formulaic_alphas_are_extracted_without_an_llm() {
        let source = "Introduction to the study.\n\nAlpha#1: rank(close - open)\n  * -1\n\nAlpha#2: correlation(vwap, adv20, 6)\n\nAppendix B\n";

        let (semantic_source, factors) = extract_numbered_formulaic_factors(source);

        assert_eq!(factors.len(), 2);
        assert_eq!(factors[0].english, "Formulaic Alpha 1");
        assert_eq!(factors[0].formula, "rank(close - open) * -1");
        assert_eq!(factors[0].required_inputs, ["close", "open"]);
        assert_eq!(factors[1].required_inputs, ["adv20", "vwap"]);
        assert!(semantic_source.contains("Introduction to the study."));
        assert!(semantic_source.contains("Appendix B"));
        assert!(!semantic_source.contains("Alpha#1:"));
        assert!(!semantic_source.contains("Alpha#2:"));
    }

    #[test]
    fn compact_formulaic_semantics_expand_to_the_full_source_contract() {
        let compact = json!({
            "source_english": "Formulaic Alphas",
            "source_chinese": "公式化阿尔法",
            "summary": "A collection of explicit quantitative alpha formulas.",
            "concepts_tsv": "Alpha Combination|阿尔法组合|Combine signals into a portfolio.|The paper describes a mega-alpha.",
            "entities_tsv": "",
            "findings_lines": "The formulas use price and volume data.",
            "methodology_definition": "Construct and backtest formulaic signals.",
            "methodology_terms_tsv": "Formulaic Signal Construction|公式化信号构建|construction|Translate formulas into testable signals.|Explicit formulas are also code.",
            "data_inputs_csv": "close, volume"
        });

        let expanded = expand_formulaic_semantic_value(&compact).expect("expanded contract");
        let analysis: SourceAnalysis =
            serde_json::from_value(expanded).expect("full source analysis");

        assert!(analysis.factors.is_empty());
        assert_eq!(analysis.key_concepts.len(), 1);
        assert_eq!(analysis.methodology.terms.len(), 1);
        assert_eq!(analysis.methodology.data_inputs, ["close", "volume"]);
        validate_analysis(&analysis).expect("valid source analysis");
    }

    #[test]
    fn structured_response_accepts_plain_fenced_and_surrounded_json() {
        let expected = json!({"summary": "momentum"});
        for response in [
            r#"{"summary":"momentum"}"#,
            "```json\n{\"summary\":\"momentum\"}\n```",
            "Completed analysis:\n{\"summary\":\"momentum\"}\nDone.",
        ] {
            assert_eq!(
                parse_structured_response(response).expect("structured response"),
                expected
            );
        }
    }

    #[test]
    fn structured_prompt_embeds_required_schema_and_field_names() {
        let prompt = prompt_with_schema(
            "Analyze the source.",
            &json!({
                "type": "object",
                "properties": {"source_name": {"type": "object"}},
                "required": ["source_name"]
            }),
        )
        .expect("structured prompt");

        assert!(prompt.contains("Analyze the source."));
        assert!(prompt.contains("Required JSON contract"));
        assert!(prompt.contains("\"source_name\""));
        assert!(prompt.contains("Include every required field"));
    }

    #[test]
    fn validation_rejects_vacuous_source_analysis() {
        let analysis = SourceAnalysis {
            source_name: BilingualName::new("Empty Source", "空来源").expect("name"),
            summary: String::new(),
            key_concepts: Vec::new(),
            findings: Vec::new(),
            connections: Vec::new(),
            tensions: Vec::new(),
            recommendations: Vec::new(),
            entities: Vec::new(),
            selected_media: Vec::new(),
            methodology: SourceMethodology {
                definition: String::new(),
                terms: Vec::new(),
                data_inputs: Vec::new(),
                benefits: Vec::new(),
                limitations: Vec::new(),
                coverage_gaps: Vec::new(),
                connections: Vec::new(),
                review_status: String::new(),
            },
            factors: Vec::new(),
        };

        assert!(validate_analysis(&analysis).is_err());
    }

    #[test]
    fn ollama_uses_object_grammar_while_other_providers_keep_full_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"source_name": {"type": "object"}},
            "required": ["source_name"]
        });

        assert_eq!(
            codex_output_schema("ollama/ornith-1.5:9b", &schema),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"source_name": {"type": "object"}},
                "required": ["source_name"]
            })
        );
        assert_eq!(codex_output_schema("openai/gpt-5", &schema), schema);
    }

    #[test]
    fn chunk_analyses_merge_duplicate_factors_and_distinct_evidence() {
        let mut first = sample_analysis("First summary", "close", "page 1");
        first.factors[0].formula.clear();
        let mut second = sample_analysis("Second summary", "volume", "page 2");
        second.factors[0].formula = "close / delay(close, 20) - 1".to_owned();

        let merged = merge_source_analyses(vec![first, second]).expect("merged analysis");

        assert!(merged.summary.contains("First summary"));
        assert!(merged.summary.contains("Second summary"));
        assert_eq!(merged.factors.len(), 1);
        assert_eq!(merged.factors[0].formula, "close / delay(close, 20) - 1");
        assert_eq!(merged.factors[0].required_inputs, ["close", "volume"]);
        assert_eq!(merged.factors[0].evidence, ["page 1", "page 2"]);
    }

    fn sample_analysis(summary: &str, input: &str, evidence: &str) -> SourceAnalysis {
        SourceAnalysis {
            source_name: BilingualName::new("Momentum Study", "动量研究").expect("name"),
            summary: summary.to_owned(),
            key_concepts: Vec::new(),
            findings: vec![summary.to_owned()],
            connections: Vec::new(),
            tensions: Vec::new(),
            recommendations: Vec::new(),
            entities: Vec::new(),
            selected_media: Vec::new(),
            methodology: SourceMethodology {
                definition: "Rank recent returns".to_owned(),
                terms: Vec::new(),
                data_inputs: vec![input.to_owned()],
                benefits: Vec::new(),
                limitations: Vec::new(),
                coverage_gaps: Vec::new(),
                connections: Vec::new(),
                review_status: "reviewed".to_owned(),
            },
            factors: vec![SourceFactor {
                english: "Momentum Factor".to_owned(),
                chinese: "动量因子".to_owned(),
                definition: "Recent return momentum".to_owned(),
                category: "momentum".to_owned(),
                formula: "close / delay(close, 20) - 1".to_owned(),
                required_inputs: vec![input.to_owned()],
                polars_expression: String::new(),
                scenario: String::new(),
                evaluation: String::new(),
                evidence: vec![evidence.to_owned()],
            }],
        }
    }
}
