use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::AliasValue;

// ── Workspaces ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    pub canonical_path: PathBuf,
    pub hostdo: Option<WorkspaceHostdo>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            canonical_path: PathBuf::new(),
            hostdo: None,
        }
    }
}

// ── Containers ───────────────────────────────────────────────────────────────

/// Known agent CLIs inferred from launch argv for runtime-specific diagnostics
/// and compatibility behavior.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// No recognized built-in runtime.
    #[default]
    None,
    /// Claude Code CLI (`@anthropic-ai/claude-code`).
    Claude,
    /// OpenAI Codex CLI (`@openai/codex`).
    Codex,
    /// Google Gemini CLI (`@google/gemini-cli`).
    Gemini,
    /// opencode (`opencode-ai`).
    Opencode,
}

/// Internal resolved container launch definition synthesized from
/// `[container_profiles.<name>]` entries.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ContainerDef {
    /// Human-readable identifier shown in the TUI tab bar.
    pub name: String,
    /// Docker image to run.
    #[serde(default)]
    pub image: String,
    /// Dockerfile stem (e.g. `default` -> `<docker_dir>/default.dockerfile`).
    #[serde(default)]
    pub image_stem: String,
    /// Optional profile key (legacy/internal compatibility field).
    #[serde(default)]
    pub profile: Option<String>,
    /// Path inside the container where the project workspace is mounted.
    /// Defaults to `/workspace`.
    #[serde(default = "default_mount_target")]
    pub mount_target: PathBuf,
    /// Optional argv override used to start the agent inside the container.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Render ANSI palette requests in grayscale for this profile.
    #[serde(default)]
    pub grayscale_palette: bool,
    /// Additional allowlist entries written into a starter `harness-rules.toml`.
    #[serde(default)]
    pub starter_network_allowlist: Vec<String>,
    /// Optional container log paths to scan for MCP startup diagnostics.
    #[serde(default)]
    pub mcp_log_paths: Vec<PathBuf>,
    /// Optional grep pattern used when scanning `mcp_log_paths`.
    #[serde(default)]
    pub mcp_log_pattern: Option<String>,
    /// Extra host paths to mount into the container (for auth/session reuse).
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
    /// Host env var names to pass through with `docker run -e NAME`.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// Hostnames/domains to add to NO_PROXY for this container.
    /// Use when specific endpoints must bypass the harness-hat proxy.
    #[serde(default)]
    pub bypass_proxy: Vec<String>,
}

/// Named container profile used directly as a launch target.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ContainerProfile {
    /// Dockerfile stem looked up as `<docker_dir>/<image>.dockerfile`.
    /// Defaults to `default` when omitted.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub mount_target: Option<PathBuf>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub grayscale_palette: Option<bool>,
    #[serde(default)]
    pub starter_network_allowlist: Vec<String>,
    #[serde(default)]
    pub mcp_log_paths: Vec<PathBuf>,
    #[serde(default)]
    pub mcp_log_pattern: Option<String>,
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    #[serde(default)]
    pub bypass_proxy: Vec<String>,
}

/// Shared defaults merged into every container definition.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ContainerDefaults {
    #[serde(default)]
    pub mount_target: Option<PathBuf>,
    #[serde(default)]
    pub grayscale_palette: Option<bool>,
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
    #[serde(default)]
    pub mcp_log_paths: Vec<PathBuf>,
    #[serde(default)]
    pub mcp_log_pattern: Option<String>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    #[serde(default)]
    pub bypass_proxy: Vec<String>,
}

pub(crate) fn default_mount_target() -> PathBuf {
    PathBuf::from("/workspace")
}

pub fn infer_agent_kind_from_argv(command: Option<&[String]>) -> AgentKind {
    let executable = command
        .and_then(|argv| argv.first())
        .map(String::as_str)
        .and_then(normalize_command_name);
    match executable.as_deref() {
        Some("claude") => AgentKind::Claude,
        Some("codex") => AgentKind::Codex,
        Some("gemini") => AgentKind::Gemini,
        Some("opencode") => AgentKind::Opencode,
        _ => AgentKind::None,
    }
}

pub fn normalize_command_name(command: &str) -> Option<String> {
    let raw = command.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        std::path::Path::new(raw)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(raw)
            .to_ascii_lowercase(),
    )
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ContainerMount {
    /// Host-side source path (supports `~` expansion).
    pub host: PathBuf,
    /// Container target path.
    pub container: PathBuf,
    /// Mount mode: `ro` or `rw` (default).
    #[serde(default)]
    pub mode: MountMode,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    Ro,
    #[default]
    Rw,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WorkspaceHostdo {
    pub denied_executables: Option<Vec<String>>,
    pub denied_argument_fragments: Option<Vec<String>>,
    pub command_aliases: Option<HashMap<String, AliasValue>>,
}

// ── Enums ────────────────────────────────────────────────────────────────────

// ── Logging ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LoggingConfig {
    /// Directory for runtime logs and local runtime state files.
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    /// Stable instance identifier persisted into `harness-hat.toml`.
    /// Used as `service.instance.id` in OpenTelemetry exports.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Optional OTLP export configuration. Absent = no OTel export.
    pub otlp: Option<OtlpConfig>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            instance_id: None,
            otlp: None,
        }
    }
}

fn default_log_dir() -> PathBuf {
    PathBuf::from("~/.local/share/harness-hat")
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OtlpConfig {
    /// Collector endpoint, e.g. `http://localhost:4317` (gRPC) or
    /// `http://localhost:4318/v1/traces` (HTTP/proto).
    pub endpoint: String,
    #[serde(default)]
    pub protocol: OtlpProtocol,
    #[serde(default)]
    pub level: AuditExportLevel,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OtlpProtocol {
    #[default]
    Grpc,
    Http,
}

/// Which events to emit as OTel spans.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuditExportLevel {
    /// Every hostdo / HTTP event (including auto-approved).
    All,
    /// Only events that required a manual developer approval prompt.
    #[default]
    Approvals,
    /// No OTel spans emitted.
    None,
}
