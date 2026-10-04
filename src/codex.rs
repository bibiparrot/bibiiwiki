use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::config::Config;
use crate::gateway::{AppState, app};

const PROVIDER_ID: &str = "bibiiwiki";
const CODEX_PROCESS_GRACE: Duration = Duration::from_secs(30);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
const fn codex_creation_flags() -> u32 {
    CREATE_NO_WINDOW
}

fn hide_codex_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.as_std_mut().creation_flags(codex_creation_flags());
    }
}

/// Starts a local gateway, launches Codex with a provider override, and waits.
///
/// # Errors
///
/// Returns an error when the gateway cannot bind, Codex cannot be launched, or
/// the local gateway exits unsuccessfully.
pub async fn run(config: Config, codex_args: Vec<OsString>) -> Result<i32> {
    let gateway = Gateway::start(&config, false).await?;
    let binary = resolve_codex_binary(&config.codex.binary);
    let mut command = Command::new(&binary);
    configure_command(&mut command, &config, &gateway.base_url);
    if !has_model_arg(&codex_args) {
        command.arg("--model").arg(config.codex_model());
    }
    command.args(codex_args);

    let result = command.status().await.with_context(|| {
        format!(
            "failed to launch Codex at {} (configured as {:?})",
            binary.display(),
            config.codex.binary
        )
    });
    gateway.shutdown().await?;
    let status = result?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Clone, Debug)]
pub struct CodexPromptRequest {
    pub prompt: String,
    pub workspace: PathBuf,
    pub state_path: Option<PathBuf>,
    pub output_schema: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexPromptResult {
    pub response: String,
    pub thread_id: Option<String>,
}

/// Runs a non-interactive Codex prompt through an ephemeral BIBIIWIKI gateway.
///
/// When `state_path` is supplied, the Codex session ID is persisted and reused
/// by later calls. Without one, Codex runs ephemerally.
///
/// # Errors
///
/// Returns an error when the gateway, Codex process, state, schema, or final
/// response cannot be created or read.
pub async fn prompt(config: Config, request: CodexPromptRequest) -> Result<CodexPromptResult> {
    let temporary = temporary_paths();
    let schema_path =
        write_output_schema(request.output_schema.as_ref(), &temporary.schema).await?;
    let prior_thread = read_thread_id(request.state_path.as_ref()).await?;
    let gateway = Gateway::start(&config, true).await?;
    let binary = resolve_codex_binary(&config.codex.binary);
    let mut command = Command::new(&binary);
    hide_codex_console(&mut command);
    configure_command(&mut command, &config, &gateway.base_url);
    command.arg("exec");
    if prior_thread.is_some() {
        command.arg("resume");
    }
    command
        // Semantic wiki jobs must not inherit user MCP servers, project rules,
        // or other interactive Codex configuration. Those enlarge the local
        // model context and can cause an otherwise small structured prompt to
        // time out before producing its final answer. CLI overrides and Codex
        // authentication remain available with these isolation switches.
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("--disable")
        .arg("shell_tool")
        .arg("--model")
        .arg(config.codex_model());
    if prior_thread.is_none() {
        command
            .arg("-C")
            .arg(&request.workspace)
            .arg("-s")
            .arg("read-only");
    }
    command
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("--output-last-message")
        .arg(&temporary.output);
    if request.state_path.is_none() {
        command.arg("--ephemeral");
    }
    if let Some(path) = schema_path {
        command.arg("--output-schema").arg(path);
    }
    if let Some(thread_id) = prior_thread.as_deref() {
        command.arg(thread_id);
    }
    command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let process_timeout = config.request_timeout().saturating_add(CODEX_PROCESS_GRACE);
    let process_result = run_prompt_command(
        command,
        &request.prompt,
        &binary.display().to_string(),
        process_timeout,
    )
    .await;
    let shutdown_result = gateway.shutdown().await;
    let process_output = match process_result {
        Ok(output) => {
            shutdown_result?;
            output
        }
        Err(error) => {
            cleanup_temporary(&temporary).await;
            if let Err(shutdown_error) = shutdown_result {
                tracing::warn!(%shutdown_error, "gateway shutdown also failed after Codex error");
            }
            return Err(error);
        }
    };
    if !process_output.status.success() {
        cleanup_temporary(&temporary).await;
        let stdout = String::from_utf8_lossy(&process_output.stdout);
        let stderr = String::from_utf8_lossy(&process_output.stderr);
        bail!(
            "Codex exited with status {}:\n{}",
            process_output.status.code().unwrap_or(1),
            process_failure_detail(&stdout, &stderr, 8192)
        );
    }
    let response = read_prompt_response(&temporary.output).await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            cleanup_temporary(&temporary).await;
            return Err(error);
        }
    };
    let stdout = String::from_utf8(process_output.stdout).context("Codex JSONL was not UTF-8")?;
    let thread_id = parse_thread_id(&stdout).or(prior_thread);
    if let (Some(path), Some(thread_id)) = (request.state_path.as_ref(), thread_id.as_deref()) {
        write_thread_id(path, thread_id).await?;
    }
    cleanup_temporary(&temporary).await;
    Ok(CodexPromptResult {
        response,
        thread_id,
    })
}

async fn write_output_schema<'a>(
    schema: Option<&Value>,
    path: &'a Path,
) -> Result<Option<&'a Path>> {
    let Some(schema) = schema else {
        return Ok(None);
    };
    tokio::fs::write(path, serde_json::to_vec_pretty(schema)?)
        .await
        .with_context(|| format!("failed to write schema {}", path.display()))?;
    Ok(Some(path))
}

async fn read_prompt_response(path: &Path) -> Result<String> {
    let response = tokio::fs::read_to_string(path)
        .await
        .with_context(|| {
            format!(
                "Codex did not write its final response to {}",
                path.display()
            )
        })?
        .trim()
        .to_owned();
    if response.is_empty() {
        bail!("Codex returned an empty final response");
    }
    Ok(response)
}

fn truncate_error(value: &str, limit: usize) -> String {
    const MARKER: &str = "\n… omitted …\n";
    let value = value.trim();
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= limit {
        return value.to_owned();
    }
    let marker_chars = MARKER.chars().count();
    if limit <= marker_chars + 2 {
        return characters.into_iter().take(limit).collect();
    }
    let retained = limit - marker_chars;
    let head = retained / 2;
    let tail = retained - head;
    let mut output = characters.iter().take(head).collect::<String>();
    output.push_str(MARKER);
    output.extend(characters.iter().skip(characters.len() - tail));
    output
}

fn process_failure_detail(stdout: &str, stderr: &str, limit: usize) -> String {
    let per_stream = (limit / 2).max(1);
    format!(
        "Codex stderr:\n{}\n\nCodex stdout JSONL:\n{}",
        truncate_error(stderr, per_stream),
        truncate_error(stdout, per_stream)
    )
}

fn resolve_codex_binary(configured: &str) -> PathBuf {
    let configured_path = PathBuf::from(configured);
    if configured_path.is_absolute() || configured_path.components().count() > 1 {
        return configured_path;
    }
    if let Some(path) = find_executable_on_path(configured) {
        return path;
    }
    #[cfg(target_os = "windows")]
    if is_default_codex_name(configured)
        && let Some(path) = find_windows_codex_install()
    {
        return path;
    }
    configured_path
}

fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    let search_path = env::var_os("PATH")?;
    for directory in env::split_paths(&search_path) {
        for candidate in executable_names(name) {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn executable_names(name: &str) -> Vec<OsString> {
    let name = OsString::from(name);
    #[cfg(target_os = "windows")]
    {
        let path = Path::new(&name);
        if path.extension().is_none() {
            let mut candidates = ["exe", "cmd", "bat"]
                .into_iter()
                .map(|extension| {
                    let mut candidate = name.clone();
                    candidate.push(format!(".{extension}"));
                    candidate
                })
                .collect::<Vec<_>>();
            candidates.push(name);
            return candidates;
        }
    }
    vec![name]
}

#[cfg(target_os = "windows")]
fn is_default_codex_name(configured: &str) -> bool {
    configured.eq_ignore_ascii_case("codex") || configured.eq_ignore_ascii_case("codex.exe")
}

#[cfg(target_os = "windows")]
fn find_windows_codex_install() -> Option<PathBuf> {
    let install_root = PathBuf::from(env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    find_codex_under(&install_root)
}

#[cfg(target_os = "windows")]
fn find_codex_under(root: &Path) -> Option<PathBuf> {
    let direct = root.join("codex.exe");
    if direct.is_file() {
        return Some(direct);
    }
    let mut candidates = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("codex.exe"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        modified_time(right)
            .cmp(&modified_time(left))
            .then_with(|| right.cmp(left))
    });
    candidates.into_iter().next()
}

#[cfg(target_os = "windows")]
fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

struct Gateway {
    base_url: String,
    shutdown_tx: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Gateway {
    async fn start(config: &Config, semantic_prompts: bool) -> Result<Self> {
        if !config.server.bind.ip().is_loopback() {
            bail!("the integrated Codex adapter requires a loopback server.bind address");
        }
        let state = if semantic_prompts {
            AppState::from_config(config)?.for_semantic_prompts()
        } else {
            AppState::from_config(config)?
        };
        let listener = TcpListener::bind(config.server.bind)
            .await
            .with_context(|| format!("failed to bind {}", config.server.bind))?;
        let address = listener
            .local_addr()
            .context("failed to read listener address")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app(state))
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });
        Ok(Self {
            base_url: base_url(address),
            shutdown_tx,
            server,
        })
    }

    async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        self.server
            .await
            .context("gateway task panicked")?
            .context("gateway server failed")
    }
}

fn configure_command(command: &mut Command, config: &Config, base_url: &str) {
    let no_proxy = no_proxy_with_loopback();
    command
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy)
        .arg("-c")
        .arg(provider_override(config, base_url))
        .arg("-c")
        .arg(format!("model_provider=\"{PROVIDER_ID}\""));
    if let Some(effort) = config.codex.reasoning_effort.as_deref() {
        command
            .arg("-c")
            .arg(format!("model_reasoning_effort={effort:?}"));
    }
}

async fn run_prompt_command(
    mut command: Command,
    prompt: &str,
    binary: &str,
    process_timeout: Duration,
) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to launch {binary}"))?;
    write_prompt(&mut child, prompt).await?;
    match tokio::time::timeout(process_timeout, child.wait_with_output()).await {
        Ok(output) => output.with_context(|| format!("failed to wait for {binary}")),
        Err(_) => bail!(
            "{binary} exceeded its {} second process timeout",
            process_timeout.as_secs()
        ),
    }
}

async fn write_prompt(child: &mut Child, prompt: &str) -> Result<()> {
    let mut stdin = child.stdin.take().context("failed to open Codex stdin")?;
    stdin
        .write_all(prompt.as_bytes())
        .await
        .context("failed to send prompt to Codex")?;
    stdin
        .shutdown()
        .await
        .context("failed to close Codex stdin")
}

async fn read_thread_id(path: Option<&PathBuf>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let state: Value = serde_json::from_slice(
        &tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read Codex state {}", path.display()))?,
    )
    .with_context(|| format!("invalid Codex state {}", path.display()))?;
    let thread_id = state
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("stored Codex thread id is invalid")?;
    Ok(Some(thread_id.to_string()))
}

async fn write_thread_id(path: &PathBuf, thread_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let value = serde_json::to_vec_pretty(&serde_json::json!({"thread_id": thread_id}))?;
    tokio::fs::write(path, value)
        .await
        .with_context(|| format!("failed to write Codex state {}", path.display()))
}

fn parse_thread_id(jsonl: &str) -> Option<String> {
    jsonl.lines().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        (value.get("type").and_then(Value::as_str) == Some("thread.started"))
            .then(|| {
                value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten()
    })
}

struct TemporaryPaths {
    output: PathBuf,
    schema: PathBuf,
}

fn temporary_paths() -> TemporaryPaths {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let stem = format!("bibiiwiki-codex-{}-{unique}", std::process::id());
    let root = env::temp_dir();
    TemporaryPaths {
        output: root.join(format!("{stem}.txt")),
        schema: root.join(format!("{stem}.schema.json")),
    }
}

async fn cleanup_temporary(paths: &TemporaryPaths) {
    for path in [&paths.output, &paths.schema] {
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %error, "failed to remove temporary Codex file");
        }
    }
}

/// Renders a persistent Codex `config.toml` provider block.
#[must_use]
pub fn config_snippet(config: &Config, base_url: &str) -> String {
    let mut lines = vec![
        format!("model = {:?}", config.codex_model()),
        format!("model_provider = {PROVIDER_ID:?}"),
    ];
    if let Some(effort) = config.codex.reasoning_effort.as_deref() {
        lines.push(format!("model_reasoning_effort = {effort:?}"));
    }
    lines.extend([
        String::new(),
        format!("[model_providers.{PROVIDER_ID}]"),
        "name = \"BIBIIWIKI\"".to_string(),
        format!("base_url = {base_url:?}"),
        "wire_api = \"responses\"".to_string(),
        "requires_openai_auth = false".to_string(),
    ]);
    if let Some(variable) = config.server.api_key_env.as_deref() {
        lines.push(format!("env_key = {variable:?}"));
    }
    lines.join("\n")
}

fn provider_override(config: &Config, base_url: &str) -> String {
    let mut fields = vec![
        "name = \"BIBIIWIKI\"".to_string(),
        format!("base_url = {base_url:?}"),
        "wire_api = \"responses\"".to_string(),
        "requires_openai_auth = false".to_string(),
    ];
    if let Some(variable) = config.server.api_key_env.as_deref() {
        fields.push(format!("env_key = {variable:?}"));
    }
    format!("model_providers.{PROVIDER_ID}={{ {} }}", fields.join(", "))
}

fn has_model_arg(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "-m" || arg == "--model" || arg.starts_with("--model=")
    })
}

fn base_url(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(_) => format!("http://{address}/v1"),
        SocketAddr::V6(address) => format!("http://[{}]:{}/v1", address.ip(), address.port()),
    }
}

fn no_proxy_with_loopback() -> OsString {
    let mut value = env::var_os("NO_PROXY")
        .or_else(|| env::var_os("no_proxy"))
        .unwrap_or_default();
    if !value.is_empty() {
        value.push(",");
    }
    value.push("127.0.0.1,localhost,::1");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_model_arguments() {
        assert!(has_model_arg(&[OsString::from("--model=x")]));
        assert!(has_model_arg(&[
            OsString::from("exec"),
            OsString::from("-m"),
            OsString::from("x")
        ]));
        assert!(!has_model_arg(&[OsString::from("exec")]));
    }

    #[test]
    fn no_proxy_contains_all_loopback_forms() {
        let value = no_proxy_with_loopback().to_string_lossy().into_owned();
        assert!(value.contains("127.0.0.1"));
        assert!(value.contains("localhost"));
        assert!(value.contains("::1"));
    }

    #[cfg(windows)]
    #[test]
    fn background_codex_processes_use_the_no_window_flag() {
        assert_ne!(codex_creation_flags() & CREATE_NO_WINDOW, 0);
    }

    #[test]
    fn truncated_codex_errors_keep_the_final_failure_reason() {
        let diagnostic = format!("{}FINAL FAILURE", "startup warning\n".repeat(20));

        let truncated = truncate_error(&diagnostic, 80);

        assert!(truncated.starts_with("startup warning"));
        assert!(truncated.contains("omitted"));
        assert!(truncated.ends_with("FINAL FAILURE"));
        assert!(truncated.chars().count() <= 80);
    }

    #[test]
    fn failed_codex_diagnostic_includes_stdout_jsonl_and_stderr() {
        let diagnostic = process_failure_detail(
            r#"{"type":"error","message":"schema rejected"}"#,
            "startup warning",
            200,
        );

        assert!(diagnostic.contains("Codex stderr:\nstartup warning"));
        assert!(diagnostic.contains("Codex stdout JSONL:"));
        assert!(diagnostic.contains("schema rejected"));
    }

    #[cfg(windows)]
    #[test]
    fn discovers_codex_in_the_desktop_install_tree() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("bibiiwiki-codex-discovery-{unique}"));
        let installed = root.join("desktop-build").join("codex.exe");
        fs::create_dir_all(installed.parent().expect("install parent"))
            .expect("create install tree");
        fs::write(&installed, b"test executable marker").expect("write executable marker");

        assert_eq!(find_codex_under(&root), Some(installed));

        fs::remove_dir_all(root).expect("discovery fixture cleanup");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn prompt_process_timeout_terminates_a_stalled_child() {
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let error =
            run_prompt_command(command, "test", "powershell.exe", Duration::from_millis(50))
                .await
                .expect_err("stalled child should time out");

        assert!(
            error
                .to_string()
                .contains("exceeded its 0 second process timeout")
        );
    }
}
