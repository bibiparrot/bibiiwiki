use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use litellm_core::router::Deployment;
use serde::Deserialize;
use yaml_edit::YamlFile;

const DEFAULT_CONFIG_SOURCE: &str = include_str!("../bibiiwiki.yaml");
const USER_CONFIG_DIRECTORY: &str = ".bibiiwik";
const USER_CONFIG_FILE: &str = "bibiiwiki.yaml";
const DEFAULT_CHUNKING_SOURCE: &str = r#"
# Markdown-aware semantic extraction limits. Character counts intentionally
# leave room for prompts, schemas, output, and multilingual tokenization.
chunking:
  default_max_characters: 10000
  model_max_characters:
    "ornith-1.5": 10000
    "qwen3": 12000
    "deepseek-chat": 24000
    "deepseek-reasoner": 24000
    "deepseek-v4-flash": 24000
    "deepseek-v4-pro": 24000
    "gpt-5.6": 64000
    "claude-sonnet-5": 64000
    "gemini-2.5": 64000
"#;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub model_list: Vec<Deployment>,
    #[serde(default)]
    pub chunking: ChunkingConfig,
    #[serde(default)]
    pub codex: CodexConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ChunkingConfig {
    /// Conservative fallback for semantic Markdown extraction. This is a
    /// character limit, not the provider's token context-window limit.
    #[serde(default = "default_max_markdown_chunk_characters")]
    pub default_max_characters: usize,
    /// Exact deployment aliases, provider model IDs, or provider-model
    /// prefixes mapped to their Markdown chunk character limit.
    #[serde(default = "default_model_max_characters")]
    pub model_max_characters: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// Optional environment variable containing the bearer token accepted by
    /// this gateway. The secret itself never needs to be written to YAML.
    pub api_key_env: Option<String>,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    #[serde(default = "default_max_output_tokens")]
    pub default_max_output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexConfig {
    #[serde(default = "default_codex_binary")]
    pub binary: String,
    pub model: Option<String>,
    /// Optional Codex reasoning effort override. Local completion-only Ollama
    /// models generally work best with `none`, which also lets the proxy send
    /// Ollama's `think: false` request flag.
    pub reasoning_effort: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            api_key_env: None,
            request_timeout_seconds: default_request_timeout_seconds(),
            default_max_output_tokens: default_max_output_tokens(),
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            binary: default_codex_binary(),
            model: None,
            reasoning_effort: None,
        }
    }
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            default_max_characters: default_max_markdown_chunk_characters(),
            model_max_characters: default_model_max_characters(),
        }
    }
}

impl Config {
    /// Returns the single per-user BIBIIWIKI configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system does not expose a user-home
    /// directory through `USERPROFILE` (Windows) or `HOME`.
    pub fn user_config_path() -> Result<PathBuf> {
        Ok(user_config_path_from_home(&user_home()?))
    }

    /// Returns the per-user visual color-theme path.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system does not expose a user-home
    /// directory through `USERPROFILE` (Windows) or `HOME`.
    pub fn user_color_theme_path() -> Result<PathBuf> {
        Ok(user_home()?
            .join(USER_CONFIG_DIRECTORY)
            .join("color_theme.yaml"))
    }

    /// Returns the per-user directory for disposable workspace search indexes.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system does not expose a user-home
    /// directory through `USERPROFILE` (Windows) or `HOME`.
    pub fn user_search_index_root() -> Result<PathBuf> {
        Ok(user_home()?
            .join(USER_CONFIG_DIRECTORY)
            .join("search-indexes"))
    }

    /// Creates the per-user configuration from the embedded safe default when
    /// it does not exist. Existing formatting is preserved; a missing
    /// backward-compatible chunking section is appended once.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory or file cannot be created.
    pub fn ensure_exists(path: &Path) -> Result<()> {
        if path.exists() {
            return ensure_chunking_section(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        if let Some(legacy_path) = legacy_user_config_path(path)
            && legacy_path.is_file()
        {
            fs::rename(&legacy_path, path).with_context(|| {
                format!(
                    "failed to move legacy config {} to {}",
                    legacy_path.display(),
                    path.display()
                )
            })?;
            return ensure_chunking_section(path);
        }
        fs::write(path, DEFAULT_CONFIG_SOURCE)
            .with_context(|| format!("failed to create config {}", path.display()))
    }

    /// Parses and validates a LiteLLM-style YAML configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid YAML or an unsafe/invalid configuration.
    /// Environment-secret references are resolved only when the configuration
    /// is loaded for runtime use, so they can be saved before the variable is set.
    pub fn parse(source: &str) -> Result<Self> {
        let document = parse_lossless_document(source)?;
        let mut config: Self = serde_yaml::from_str(&document.to_string())
            .context("invalid BIBIIWIKI YAML configuration")?;
        config.resolve_secrets()?;
        config.validate()?;
        Ok(config)
    }

    /// Validates an edited configuration and returns its lossless YAML text.
    ///
    /// `yaml-edit` owns the syntax round-trip so comments, whitespace, quoting,
    /// and ordering survive UI edits. Typed deserialization remains the second
    /// validation layer for BIBIIWIKI-specific rules.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid YAML, missing referenced environment
    /// variables, or an unsafe/invalid configuration.
    pub fn prepare_edit(source: &str) -> Result<String> {
        let document = parse_lossless_document(source)?;
        let preserved = document.to_string();
        // Editing must not require the referenced secret to exist yet. This
        // lets the GUI safely save `os.environ/NAME`, while runtime loading
        // still resolves and requires the variable through `Config::parse`.
        let config: Self =
            serde_yaml::from_str(&preserved).context("invalid BIBIIWIKI YAML configuration")?;
        config.validate()?;
        Ok(preserved)
    }

    /// Loads, resolves environment-secret references, and validates YAML.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable/invalid YAML, missing referenced
    /// environment variables, or an unsafe/invalid configuration.
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        Self::parse(&source).with_context(|| format!("invalid configuration in {}", path.display()))
    }

    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.server.request_timeout_seconds)
    }

    /// Resolves the optional inbound gateway bearer token.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured environment variable is absent or
    /// empty.
    pub fn inbound_api_key(&self) -> Result<Option<String>> {
        self.server
            .api_key_env
            .as_deref()
            .map(read_required_env)
            .transpose()
    }

    #[must_use]
    pub fn codex_model(&self) -> &str {
        self.codex
            .model
            .as_deref()
            .unwrap_or(&self.model_list[0].model_name)
    }

    /// Returns the Markdown-aware semantic extraction limit for the selected
    /// Codex deployment. Exact aliases/model IDs win; otherwise the longest
    /// configured provider-model prefix wins before the fallback is used.
    #[must_use]
    pub fn max_markdown_chunk_characters(&self) -> usize {
        let alias = self.codex_model();
        let provider_model = self
            .model_list
            .iter()
            .find(|deployment| deployment.model_name == alias)
            .map_or(alias, |deployment| deployment.litellm_params.model.as_str());
        let provider_model_without_prefix = provider_model
            .split_once('/')
            .map_or(provider_model, |(_, model)| model);
        let candidates = [alias, provider_model, provider_model_without_prefix];

        for candidate in candidates {
            if let Some(limit) = self.chunking.model_max_characters.get(candidate) {
                return *limit;
            }
        }
        self.chunking
            .model_max_characters
            .iter()
            .filter(|(prefix, _)| {
                !prefix.is_empty()
                    && candidates
                        .iter()
                        .any(|candidate| candidate.starts_with(prefix.as_str()))
            })
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or(self.chunking.default_max_characters, |(_, limit)| *limit)
    }

    fn resolve_secrets(&mut self) -> Result<()> {
        for deployment in &mut self.model_list {
            if let Some(value) = deployment.litellm_params.api_key.as_deref()
                && let Some(variable) = secret_reference(value)
            {
                deployment.litellm_params.api_key = Some(read_required_env(variable)?);
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.model_list.is_empty() {
            bail!("model_list must contain at least one deployment");
        }
        if self.server.request_timeout_seconds == 0 {
            bail!("server.request_timeout_seconds must be greater than zero");
        }
        if self.server.default_max_output_tokens == 0 {
            bail!("server.default_max_output_tokens must be greater than zero");
        }
        validate_chunk_size(
            "chunking.default_max_characters",
            self.chunking.default_max_characters,
        )?;
        for (model, limit) in &self.chunking.model_max_characters {
            if model.trim().is_empty() {
                bail!("chunking.model_max_characters keys must not be empty");
            }
            validate_chunk_size(&format!("chunking.model_max_characters[{model:?}]"), *limit)?;
        }
        if !self.server.bind.ip().is_loopback() && self.server.api_key_env.is_none() {
            bail!(
                "refusing to bind {} without server.api_key_env; use loopback or configure authentication",
                self.server.bind
            );
        }
        if let Some(model) = self.codex.model.as_deref()
            && !self
                .model_list
                .iter()
                .any(|deployment| deployment.model_name == model)
        {
            bail!("codex.model {model:?} does not exist in model_list");
        }
        if let Some(effort) = self.codex.reasoning_effort.as_deref()
            && !matches!(
                effort,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
            )
        {
            bail!(
                "codex.reasoning_effort {effort:?} is invalid; use none, minimal, low, medium, high, or xhigh"
            );
        }
        Ok(())
    }
}

fn ensure_chunking_section(path: &Path) -> Result<()> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let Ok(yaml) = YamlFile::from_str(&source) else {
        // Leave malformed user configuration byte-for-byte intact so the UI
        // can present its existing recovery and validation workflow.
        return Ok(());
    };
    let Some(root) = yaml.document().and_then(|document| document.as_mapping()) else {
        return Ok(());
    };
    if root.get("chunking").is_some() {
        return Ok(());
    }
    if Config::prepare_edit(&source).is_err() {
        // Syntactically valid but incomplete legacy files must remain exactly
        // as the user wrote them. The recovery editor can repair those files;
        // silently appending defaults would both change recovery evidence and
        // turn a safe migration into a startup failure.
        return Ok(());
    }
    let upgraded = format!(
        "{}\n{}",
        source.trim_end(),
        DEFAULT_CHUNKING_SOURCE.trim_start()
    );
    Config::prepare_edit(&upgraded)?;
    fs::write(path, upgraded)
        .with_context(|| format!("failed to add chunking defaults to {}", path.display()))
}

fn validate_chunk_size(field: &str, value: usize) -> Result<()> {
    if !(256..=2_000_000).contains(&value) {
        bail!("{field} must be between 256 and 2000000 characters");
    }
    Ok(())
}

fn user_config_path_from_home(home: &Path) -> PathBuf {
    home.join(USER_CONFIG_DIRECTORY).join(USER_CONFIG_FILE)
}

fn user_home() -> Result<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("could not resolve the user home directory")
}

fn legacy_user_config_path(path: &Path) -> Option<PathBuf> {
    let config_directory = path.parent()?;
    (path.file_name()? == USER_CONFIG_FILE
        && config_directory.file_name()? == USER_CONFIG_DIRECTORY)
        .then(|| {
            config_directory
                .parent()
                .map(|home| home.join(USER_CONFIG_FILE))
        })
        .flatten()
}

fn parse_lossless_document(source: &str) -> Result<YamlFile> {
    YamlFile::from_str(source).context("invalid YAML syntax in BIBIIWIKI configuration")
}

fn secret_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("os.environ/")
        .or_else(|| value.strip_prefix("env/"))
        .or_else(|| value.strip_prefix("${").and_then(|v| v.strip_suffix('}')))
        .filter(|name| !name.is_empty())
}

fn read_required_env(name: &str) -> Result<String> {
    let value =
        env::var(name).with_context(|| format!("environment variable {name} is not set"))?;
    if value.trim().is_empty() {
        bail!("environment variable {name} is empty");
    }
    Ok(value)
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:4000"
        .parse()
        .expect("valid default bind address")
}

const fn default_request_timeout_seconds() -> u64 {
    300
}

const fn default_max_output_tokens() -> u64 {
    8192
}

const fn default_max_markdown_chunk_characters() -> usize {
    10_000
}

fn default_model_max_characters() -> BTreeMap<String, usize> {
    [
        ("ornith-1.5".to_owned(), 10_000),
        ("qwen3".to_owned(), 12_000),
        ("deepseek-chat".to_owned(), 24_000),
        ("deepseek-reasoner".to_owned(), 24_000),
        ("deepseek-v4-flash".to_owned(), 24_000),
        ("deepseek-v4-pro".to_owned(), 24_000),
        ("gpt-5.6".to_owned(), 64_000),
        ("claude-sonnet-5".to_owned(), 64_000),
        ("gemini-2.5".to_owned(), 64_000),
    ]
    .into_iter()
    .collect()
}

fn default_codex_binary() -> String {
    "codex".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_config_lives_in_hidden_bibiiwik_directory() {
        let home = Path::new(r"C:\Users\example");
        assert_eq!(
            user_config_path_from_home(home),
            home.join(".bibiiwik").join("bibiiwiki.yaml")
        );
    }

    #[test]
    fn color_theme_lives_beside_the_per_user_config() {
        let config = user_config_path_from_home(Path::new(r"C:\Users\example"));
        assert_eq!(
            config
                .parent()
                .expect("config directory")
                .join("color_theme.yaml"),
            Path::new(r"C:\Users\example\.bibiiwik\color_theme.yaml")
        );
    }

    #[test]
    fn ensure_exists_moves_legacy_home_config_without_overwriting_it() {
        let unique = format!(
            "bibiiwiki-config-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let home = std::env::temp_dir().join(unique);
        fs::create_dir_all(&home).expect("temporary home");
        let legacy = home.join(USER_CONFIG_FILE);
        let target = user_config_path_from_home(&home);
        fs::write(&legacy, "legacy: preserved\n").expect("legacy config");

        Config::ensure_exists(&target).expect("legacy config should move");

        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(&target).expect("migrated config"),
            "legacy: preserved\n"
        );
        fs::remove_dir_all(home).expect("remove temporary home");
    }

    #[test]
    fn ensure_exists_adds_chunking_presets_to_an_existing_config_once() {
        let unique = format!(
            "bibiiwiki-chunking-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        fs::create_dir_all(&directory).expect("temporary config directory");
        let path = directory.join(USER_CONFIG_FILE);
        let source = concat!(
            "# keep local comments\n",
            "model_list:\n",
            "  - model_name: local\n",
            "    litellm_params:\n",
            "      model: ollama/ornith-1.5:9b\n",
            "      api_base: http://127.0.0.1:11434/v1\n",
            "codex:\n",
            "  model: local\n",
        );
        fs::write(&path, source).expect("old configuration");

        Config::ensure_exists(&path).expect("chunking migration");
        let once = fs::read_to_string(&path).expect("upgraded config");
        Config::ensure_exists(&path).expect("idempotent chunking migration");
        let twice = fs::read_to_string(&path).expect("stable config");

        assert_eq!(once, twice);
        assert!(once.contains("# keep local comments"));
        assert!(once.contains("chunking:"));
        assert!(once.contains("\"ornith-1.5\": 10000"));
        assert_eq!(
            Config::parse(&once)
                .expect("valid upgraded config")
                .max_markdown_chunk_characters(),
            10_000
        );
        fs::remove_dir_all(directory).expect("remove temporary config directory");
    }

    #[test]
    fn parses_litellm_style_model_list() {
        let config: Config = serde_yaml::from_str(
            r"
server:
  bind: 127.0.0.1:0
model_list:
  - model_name: claude
    litellm_params:
      model: anthropic/claude-test
      api_key: test-key
codex:
  model: claude
",
        )
        .expect("config should parse");
        config.validate().expect("config should validate");
        assert_eq!(config.codex_model(), "claude");
    }

    #[test]
    fn parses_validated_configuration_from_memory() {
        let config = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/local
      api_base: http://127.0.0.1:11434/v1
",
        )
        .expect("in-memory config should parse");
        assert_eq!(config.codex_model(), "local");
    }

    #[test]
    fn rejects_unauthenticated_public_bind() {
        let config: Config = serde_yaml::from_str(
            r"
server:
  bind: 0.0.0.0:4000
model_list:
  - model_name: local
    litellm_params:
      model: openai/local
      api_base: http://127.0.0.1:11434/v1
",
        )
        .expect("config should parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn checked_in_default_uses_local_ornith_through_ollama() {
        let config: Config = serde_yaml::from_str(include_str!("../bibiiwiki.yaml"))
            .expect("default config should parse");
        config.validate().expect("default config should validate");
        assert_eq!(config.codex_model(), "ornith-1.5:9b");
        assert_eq!(config.model_list[0].model_name, "ornith-1.5:9b");
        assert_eq!(
            config.model_list[0].litellm_params.model,
            "ollama/ornith-1.5:9b"
        );
        assert_eq!(
            config.model_list[0].litellm_params.api_base.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(config.codex.reasoning_effort.as_deref(), Some("none"));
        assert_eq!(config.max_markdown_chunk_characters(), 10_000);
    }

    #[test]
    fn markdown_chunk_limit_prefers_exact_model_then_longest_prefix() {
        let exact = Config::parse(
            r"
model_list:
  - model_name: local-qwen
    litellm_params:
      model: ollama/qwen3:8b
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 9000
  model_max_characters:
    qwen: 11000
    qwen3: 12000
    local-qwen: 13000
codex:
  model: local-qwen
",
        )
        .expect("model-specific chunk configuration should parse");
        assert_eq!(exact.max_markdown_chunk_characters(), 13_000);

        let provider_prefix = Config::parse(
            r"
model_list:
  - model_name: local-qwen
    litellm_params:
      model: ollama/qwen3:8b
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 9000
  model_max_characters:
    qwen: 11000
    qwen3: 12000
codex:
  model: local-qwen
",
        )
        .expect("provider prefix chunk configuration should parse");
        assert_eq!(provider_prefix.max_markdown_chunk_characters(), 12_000);
    }

    #[test]
    fn markdown_chunk_limit_rejects_unsafe_sizes() {
        let error = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/local
      api_base: http://127.0.0.1:11434/v1
chunking:
  default_max_characters: 0
",
        )
        .expect_err("zero-sized chunks must be rejected");
        assert!(error.to_string().contains("default_max_characters"));
    }

    #[test]
    fn rejects_invalid_codex_reasoning_effort() {
        let error = Config::parse(
            r"
model_list:
  - model_name: local
    litellm_params:
      model: ollama/local
      api_base: http://127.0.0.1:11434/v1
codex:
  reasoning_effort: enormous
",
        )
        .expect_err("unknown effort should fail validation");

        assert!(error.to_string().contains("codex.reasoning_effort"));
    }

    #[test]
    fn yaml_edit_round_trip_preserves_comments_and_formatting() {
        let source = concat!(
            "# local model\n",
            "model_list:\n",
            "  - model_name: local # public name\n",
            "    litellm_params:\n",
            "      model: ollama/local\n",
            "      api_base: 'http://127.0.0.1:11434/v1'\n",
        );

        let preserved = Config::prepare_edit(source).expect("editable YAML should validate");

        assert!(preserved.contains("# local model"));
        assert!(preserved.contains("model_name: local # public name"));
        assert!(preserved.contains("api_base: 'http://127.0.0.1:11434/v1'"));
    }

    #[test]
    fn yaml_edit_rejects_a_broken_ui_edit() {
        let error = Config::prepare_edit("model_list: [\n").expect_err("YAML must be complete");

        assert!(error.to_string().contains("invalid YAML syntax"));
    }

    #[test]
    fn yaml_editor_can_save_an_environment_secret_reference_before_it_is_set() {
        let source = concat!(
            "model_list:\n",
            "  - model_name: remote\n",
            "    litellm_params:\n",
            "      model: openai/remote\n",
            "      api_key: os.environ/BIBIIWIKI_TEST_KEY_THAT_NEED_NOT_EXIST\n",
            "codex:\n",
            "  model: remote\n",
        );

        let prepared = Config::prepare_edit(source)
            .expect("an unresolved environment reference should still be editable and saveable");

        assert!(prepared.contains("os.environ/BIBIIWIKI_TEST_KEY_THAT_NEED_NOT_EXIST"));
    }
}
