use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Workspaces ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    pub canonical_path: PathBuf,
    #[serde(default)]
    pub sidebar_hotkey: Option<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            canonical_path: PathBuf::new(),
            sidebar_hotkey: None,
        }
    }
}

// ── Containers ───────────────────────────────────────────────────────────────

/// Internal resolved container launch definition synthesized from
/// `[container_profiles.<name>]` entries.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ContainerDef {
    /// Human-readable identifier shown in the TUI tab bar.
    pub name: String,
    /// Docker image to run.
    #[serde(default)]
    pub image: String,
    /// Dockerfile stem (e.g. `default` -> `<docker_dir>/default.dockerfile`).
    #[serde(default)]
    pub image_stem: String,
    /// Optional path to a local workspace dockerfile for this template.
    /// When set, build and validation helpers prefer this path over
    /// `<docker_dir>/<image_stem>.dockerfile`.
    #[serde(default)]
    pub dockerfile_path: Option<PathBuf>,
    /// Optional profile key (legacy/internal compatibility field).
    #[serde(default)]
    pub profile: Option<String>,
    /// Path inside the container where the project workspace is mounted.
    /// Defaults to `/workspace`.
    #[serde(default = "default_mount_target")]
    pub mount_target: PathBuf,
    /// Optional argv override used instead of the image default command.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Render ANSI palette requests in grayscale for this profile.
    #[serde(default)]
    pub grayscale_palette: bool,
    /// Additional allowlist entries written into a starter `harness-rules.toml`.
    #[serde(default)]
    pub starter_network_allowlist: Vec<String>,
    /// Hosts/domains allowed by default without network approval.
    /// Merged with rules from harness-rules.toml at runtime.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Optional container log paths to scan for MCP startup diagnostics.
    #[serde(default)]
    pub mcp_log_paths: Vec<PathBuf>,
    /// Optional grep pattern used when scanning `mcp_log_paths`.
    #[serde(default)]
    pub mcp_log_pattern: Option<String>,
    /// Extra host paths to mount into the container (for auth/session reuse).
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
    /// Fixed environment variables to set in the container.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Host env var names to pass through with `docker run -e NAME`.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// TCP ports on container localhost that forward to the host.
    #[serde(default)]
    pub localhost_forwards: Vec<LocalhostForward>,
    /// Optional Docker memory limit, e.g. `4g`, `512m`.
    #[serde(default)]
    pub memory: Option<String>,
    /// Optional Docker CPU quota, e.g. `2` or `1.5`.
    #[serde(default)]
    pub cpus: Option<String>,
    /// Optional Docker shared memory size, e.g. `1g`.
    #[serde(default)]
    pub shm_size: Option<String>,
}

/// Named container profile used directly as a launch target.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
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
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub mcp_log_paths: Vec<PathBuf>,
    #[serde(default)]
    pub mcp_log_pattern: Option<String>,
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    #[serde(default)]
    pub localhost_forwards: Vec<LocalhostForward>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpus: Option<String>,
    #[serde(default)]
    pub shm_size: Option<String>,
}

/// Shared defaults merged into every container definition.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
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
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub localhost_forwards: Vec<LocalhostForward>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpus: Option<String>,
    #[serde(default)]
    pub shm_size: Option<String>,
}

pub(crate) fn default_mount_target() -> PathBuf {
    PathBuf::from("/workspace")
}

pub(crate) fn container_path_string(path: &Path) -> String {
    path.as_os_str().to_string_lossy().replace('\\', "/")
}

pub(crate) fn is_absolute_container_path(path: &Path) -> bool {
    container_path_string(path).starts_with('/')
}

pub(crate) fn container_path_file_name(path: &Path) -> Option<String> {
    container_path_string(path)
        .rsplit('/')
        .find(|part| !part.is_empty())
        .map(str::to_string)
}

pub(crate) fn join_container_path(base: &str, rel: &Path) -> String {
    let mut base = base.replace('\\', "/");
    while base.len() > 1 && base.ends_with('/') {
        base.pop();
    }

    let rel = container_path_string(rel);
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return base;
    }

    if base.is_empty() {
        format!("/{rel}")
    } else if base == "/" {
        format!("/{rel}")
    } else {
        format!("{base}/{rel}")
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
#[serde(deny_unknown_fields)]
pub struct ContainerMount {
    /// Host-side source path (supports `~` expansion).
    pub host: PathBuf,
    /// Container target path.
    pub container: PathBuf,
    /// Mount mode: `ro` or `rw` (default).
    #[serde(default)]
    pub mode: MountMode,
    /// Seed a private per-session copy instead of bind-mounting the host file
    /// live. The container reads the host's current contents at launch, then
    /// writes to its own copy — the host file is never modified. This protects
    /// files that the agent CLI rewrites in place (notably `.claude.json`) from
    /// corruption when the host and one or more containers run concurrently.
    ///
    /// Unset (the default) seeds iff the container path is `.claude.json`; set
    /// `seed = true` to privatize any other file mount, or `seed = false` to
    /// force the shared live bind mount. Only applies to regular files.
    #[serde(default)]
    pub seed: Option<bool>,
}

impl ContainerMount {
    /// Whether this mount is seeded as a private per-session copy rather than
    /// bind-mounted live (see [`ContainerMount::seed`]). When `seed` is unset,
    /// defaults to seeding only `.claude.json`. This is the single source of
    /// truth shared by container launch and the TUI mounts view.
    pub fn is_seeded(&self) -> bool {
        self.seed.unwrap_or_else(|| {
            container_path_file_name(&self.container).as_deref() == Some(".claude.json")
        })
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    Ro,
    #[default]
    Rw,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalhostForward {
    /// Port to bind on 127.0.0.1 inside the container.
    pub container_port: u16,
    /// Port to connect to on the host. Defaults to `container_port`.
    #[serde(default)]
    pub host_port: Option<u16>,
}

impl LocalhostForward {
    pub fn effective_host_port(&self) -> u16 {
        self.host_port.unwrap_or(self.container_port)
    }
}

// ── Enums ────────────────────────────────────────────────────────────────────

// ── Logging ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// Every control/proxy event (including auto-approved).
    All,
    /// Only events that required a manual developer approval prompt.
    #[default]
    Approvals,
    /// No OTel spans emitted.
    None,
}
