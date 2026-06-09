use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, value};
use tracing::instrument;

use crate::config::{
    Config, ContainerMount, LocalhostForward, WorkspaceConfig, default_mount_target,
};

const WORKSPACE_SIDEBAR_HOTKEY_POOL: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

// ── Rule loading ─────────────────────────────────────────────────────────────

/// Load and compose rules for a specific project (global + that project's
/// harness-rules.toml). Called at request time so edits take effect without
/// restart.
#[instrument(skip(config))]
pub fn load_composed_rules_for_workspace(
    config: &Config,
    project_name: Option<&str>,
) -> Result<crate::rules::ComposedRules> {
    let mut errors = Vec::new();

    let global = match crate::rules::load(&config.manager.global_rules_file) {
        Ok(rules) => rules,
        Err(e) => {
            errors.push(format!(
                "global rules '{}': {e}",
                config.manager.global_rules_file.display()
            ));
            crate::rules::ProjectRules::default()
        }
    };

    let mut proj_rules = Vec::new();
    if let Some(project_name) = project_name {
        if let Some(project) = config.workspaces.iter().find(|p| p.name == project_name) {
            let path = project.canonical_path.join("harness-rules.toml");
            match crate::rules::load(&path) {
                Ok(rules) => proj_rules.push(rules),
                Err(e) => {
                    errors.push(format!(
                        "project '{}' rules '{}': {e}",
                        project.name,
                        path.display()
                    ));
                }
            }
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "failed to load one or more rule files:\n{}",
            errors.join("\n")
        );
    }

    Ok(crate::rules::ComposedRules::compose(&global, &proj_rules))
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// Hard cap on config file size; configs are normally a few KiB. Refuse to
/// load anything larger (including `/dev/zero` via symlink) so a corrupted or
/// hostile file cannot trigger an OOM.
const CONFIG_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[instrument(skip(path))]
pub fn load(path: &Path) -> Result<Config> {
    let raw = read_config_to_string(path)?;
    let mut config: Config =
        toml::from_str(&raw).with_context(|| format!("parsing config: {}", path.display()))?;
    validate_config_version(config.version, path)?;
    expand_config_paths(&mut config)?;
    validate_docker_dir(&config, path)?;
    resolve_container_profiles(&mut config)?;
    canonicalize_workspace_paths(&mut config)?;
    validate(&config)?;
    ensure_logging_instance_id(path, &raw, &mut config)?;
    Ok(config)
}

fn read_config_to_string(path: &Path) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening config: {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("statting config: {}", path.display()))?;
    anyhow::ensure!(
        meta.is_file(),
        "config {}: not a regular file",
        path.display()
    );
    let mut out = String::new();
    file.take(CONFIG_MAX_BYTES + 1)
        .read_to_string(&mut out)
        .with_context(|| format!("reading config: {}", path.display()))?;
    anyhow::ensure!(
        (out.len() as u64) <= CONFIG_MAX_BYTES,
        "config {}: file exceeds {} bytes",
        path.display(),
        CONFIG_MAX_BYTES
    );
    Ok(out)
}

pub fn workspace_sidebar_hotkey_pool() -> &'static [char] {
    WORKSPACE_SIDEBAR_HOTKEY_POOL
}

pub fn normalize_workspace_sidebar_hotkey(raw: &str) -> Option<char> {
    let mut chars = raw.trim().chars();
    let ch = chars.next()?.to_ascii_lowercase();
    if chars.next().is_some() || !workspace_sidebar_hotkey_pool().contains(&ch) {
        return None;
    }
    Some(ch)
}

pub fn resolve_workspace_sidebar_hotkeys(workspaces: &[WorkspaceConfig]) -> Vec<Option<char>> {
    let mut out = vec![None; workspaces.len()];
    let mut used = std::collections::HashSet::new();

    for (idx, workspace) in workspaces.iter().enumerate() {
        let Some(raw) = workspace.sidebar_hotkey.as_deref() else {
            continue;
        };
        let Some(ch) = normalize_workspace_sidebar_hotkey(raw) else {
            continue;
        };
        if used.insert(ch) {
            out[idx] = Some(ch);
        }
    }

    for (idx, workspace) in workspaces.iter().enumerate() {
        if out[idx].is_some() {
            continue;
        }

        let preferred = workspace
            .name
            .chars()
            .map(|ch| ch.to_ascii_lowercase())
            .filter(|ch| workspace_sidebar_hotkey_pool().contains(ch));

        let fallback = workspace_sidebar_hotkey_pool().iter().copied();
        let choice = preferred.chain(fallback).find(|ch| used.insert(*ch));
        out[idx] = choice;
    }

    out
}

pub fn select_workspace_sidebar_hotkey(
    existing_workspaces: &[WorkspaceConfig],
    workspace_name: &str,
) -> Option<char> {
    let mut workspaces = existing_workspaces.to_vec();
    workspaces.push(WorkspaceConfig {
        name: workspace_name.to_string(),
        canonical_path: PathBuf::new(),
        sidebar_hotkey: None,
    });
    resolve_workspace_sidebar_hotkeys(&workspaces)
        .into_iter()
        .last()
        .flatten()
}

fn validate_config_version(version: u32, path: &Path) -> Result<()> {
    anyhow::ensure!(
        version > 0,
        "config {}: version must be greater than zero",
        path.display()
    );
    anyhow::ensure!(
        version <= crate::config::CURRENT_CONFIG_VERSION,
        "config {}: unsupported version {}; this build supports up to {}",
        path.display(),
        version,
        crate::config::CURRENT_CONFIG_VERSION
    );
    Ok(())
}

/// Expand `~` in all path fields so downstream code always sees absolute paths.
fn expand_config_paths(config: &mut Config) -> Result<()> {
    config.manager.global_rules_file = expand_path(&config.manager.global_rules_file)?;
    config.logging.log_dir = expand_path(&config.logging.log_dir)?;
    if !config.docker_dir.as_os_str().is_empty() {
        config.docker_dir = expand_path(&config.docker_dir)?;
    }
    for proj in &mut config.workspaces {
        proj.canonical_path = expand_path(&proj.canonical_path)?;
    }
    for ctr in &mut config.containers {
        for mount in &mut ctr.mounts {
            mount.host = expand_path(&mount.host)?;
        }
    }
    if let Some(p) = &config.defaults.containers.mount_target {
        config.defaults.containers.mount_target = Some(expand_path(p)?);
    }
    for mount in &mut config.defaults.containers.mounts {
        mount.host = expand_path(&mount.host)?;
    }
    for profile in config.container_profiles.values_mut() {
        if let Some(p) = &profile.mount_target {
            profile.mount_target = Some(expand_path(p)?);
        }
        for mount in &mut profile.mounts {
            mount.host = expand_path(&mount.host)?;
        }
    }
    Ok(())
}

fn resolve_container_profiles(config: &mut Config) -> Result<()> {
    anyhow::ensure!(
        config.containers.is_empty(),
        "legacy [[containers]] is no longer supported; define launchable entries under [container_profiles.<name>] only"
    );

    let defaults = config.defaults.containers.clone();
    let session_state_mounts = shared_session_state_mounts()?;
    let mut profile_names = config
        .container_profiles
        .keys()
        .cloned()
        .collect::<Vec<String>>();
    profile_names.sort();

    let mut resolved = Vec::with_capacity(profile_names.len());
    for profile_name in profile_names {
        let profile = config
            .container_profiles
            .get(&profile_name)
            .ok_or_else(|| anyhow::anyhow!("unknown container profile '{}'", profile_name))?;

        let image_stem_raw = profile.image.as_deref().unwrap_or("default").trim();
        anyhow::ensure!(
            !image_stem_raw.is_empty(),
            "container profile '{}': image must not be empty",
            profile_name
        );
        let image_stem = image_stem_raw.to_string();
        anyhow::ensure!(
            image_stem.chars().all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, '-' | '_' | '.')),
            "container profile '{}': image must be a lowercase stem (allowed: a-z, 0-9, '-', '_', '.')",
            profile_name
        );
        let image_tag = image_tag_for_stem(&image_stem);

        let mounts = merge_mounts(&defaults.mounts, &session_state_mounts, &profile.mounts);

        // Prefer the profile's value for an `Option` field, else the default's.
        macro_rules! prefer {
            ($field:ident) => {
                profile.$field.clone().or_else(|| defaults.$field.clone())
            };
        }

        resolved.push(crate::config::ContainerDef {
            name: profile_name.clone(),
            profile: None,
            image: image_tag,
            mount_target: prefer!(mount_target).unwrap_or_else(default_mount_target),
            command: profile.command.clone(),
            grayscale_palette: prefer!(grayscale_palette).unwrap_or(false),
            starter_network_allowlist: profile.starter_network_allowlist.clone(),
            mcp_log_paths: merge_unique_paths(&defaults.mcp_log_paths, &profile.mcp_log_paths),
            mcp_log_pattern: prefer!(mcp_log_pattern),
            mounts,
            env: merge_env_vars(&defaults.env, &profile.env),
            env_passthrough: merge_unique_strings(
                &defaults.env_passthrough,
                &profile.env_passthrough,
                &[],
            ),
            bypass_proxy: merge_unique_strings(&defaults.bypass_proxy, &profile.bypass_proxy, &[]),
            localhost_forwards: merge_localhost_forwards(
                &defaults.localhost_forwards,
                &profile.localhost_forwards,
            ),
            memory: prefer!(memory),
            cpus: prefer!(cpus),
            shm_size: prefer!(shm_size),
            image_stem,
        });
    }
    config.containers = resolved;

    Ok(())
}

#[instrument(skip(config, config_path))]
fn validate_docker_dir(config: &Config, config_path: &Path) -> Result<()> {
    anyhow::ensure!(
        !config.docker_dir.as_os_str().is_empty(),
        "config {}: docker_dir is required",
        config_path.display()
    );
    anyhow::ensure!(
        !config.docker_dir.exists() || config.docker_dir.is_dir(),
        "config {}: docker_dir exists but is not a directory: {}",
        config_path.display(),
        config.docker_dir.display()
    );
    Ok(())
}

fn canonicalize_workspace_paths(config: &mut Config) -> Result<()> {
    for proj in &mut config.workspaces {
        anyhow::ensure!(
            !proj.canonical_path.as_os_str().is_empty(),
            "project '{}': canonical_path is required",
            proj.name
        );
        proj.canonical_path = proj.canonical_path.canonicalize().with_context(|| {
            format!(
                "project '{}': canonical_path is not accessible: {}",
                proj.name,
                proj.canonical_path.display()
            )
        })?;
        reject_sensitive_workspace_path(&proj.name, &proj.canonical_path)?;
    }
    Ok(())
}

/// Refuse workspaces whose canonical path equals or lives under user-secret or
/// system-config directories. Mounting these into a container as `/workspace`
/// rw would let any agent CLI exfiltrate or rewrite the host's credentials.
fn reject_sensitive_workspace_path(name: &str, canonical: &Path) -> Result<()> {
    let mut sensitive: Vec<PathBuf> = vec![PathBuf::from("/etc")];
    if let Some(home) = dirs::home_dir() {
        sensitive.push(home.join(".ssh"));
        sensitive.push(home.join(".gnupg"));
    }
    for s in &sensitive {
        // Canonicalize the sensitive root too so we compare apples to apples
        // (e.g. `/etc` may itself be a symlink on some distros).
        let s_can = s.canonicalize();
        let s_ref: &Path = s_can.as_deref().unwrap_or(s.as_path());
        if canonical == s_ref || canonical.starts_with(s_ref) {
            anyhow::bail!(
                "project '{}': canonical_path {} resolves under a sensitive directory ({}); refusing to mount",
                name,
                canonical.display(),
                s_ref.display()
            );
        }
    }
    Ok(())
}

/// Concatenate `parts` in order, keeping the first item for which `eq` finds no
/// earlier duplicate (i.e. base takes precedence over profile over override).
/// Shared by the list-merge helpers below.
fn merge_dedup<T: Clone>(parts: &[&[T]], eq: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in parts.iter().flat_map(|part| part.iter()) {
        if !out.iter().any(|existing| eq(existing, item)) {
            out.push(item.clone());
        }
    }
    out
}

pub(crate) fn merge_unique_strings(
    base: &[String],
    profile: &[String],
    override_items: &[String],
) -> Vec<String> {
    merge_dedup(&[base, profile, override_items], |a, b| a == b)
}

pub(crate) fn merge_unique_paths(base: &[PathBuf], profile: &[PathBuf]) -> Vec<PathBuf> {
    merge_dedup(&[base, profile], |a, b| a == b)
}

fn merge_env_vars(
    base: &std::collections::HashMap<String, String>,
    profile: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = base.clone();
    out.extend(profile.clone());
    out
}

pub(crate) fn merge_localhost_forwards(
    base: &[LocalhostForward],
    profile: &[LocalhostForward],
) -> Vec<LocalhostForward> {
    let profile_ports = profile
        .iter()
        .map(|forward| forward.container_port)
        .collect::<std::collections::HashSet<_>>();
    let mut out = base
        .iter()
        .filter(|forward| !profile_ports.contains(&forward.container_port))
        .cloned()
        .collect::<Vec<_>>();
    for forward in profile {
        if !out
            .iter()
            .any(|existing| existing.container_port == forward.container_port)
        {
            out.push(forward.clone());
        }
    }
    out
}

pub(crate) fn merge_mounts(
    base: &[ContainerMount],
    profile: &[ContainerMount],
    override_items: &[ContainerMount],
) -> Vec<ContainerMount> {
    merge_dedup(&[base, profile, override_items], |a, b| {
        a.host == b.host && a.container == b.container && a.mode == b.mode
    })
}

fn shared_session_state_mounts() -> Result<Vec<ContainerMount>> {
    let mut mounts = Vec::new();
    for (host, container) in [
        ("~/.claude.json", "/home/coder/.claude.json"),
        ("~/.claude", "/home/coder/.claude"),
        ("~/.codex", "/home/coder/.codex"),
        ("~/.config/codex", "/home/coder/.config/codex"),
        ("~/.gemini", "/home/coder/.gemini"),
        ("~/.pi", "/home/coder/.pi"),
    ] {
        // Skip mounts whose host source does not exist rather than asking
        // Docker to bind a missing path (which silently creates a root-owned
        // empty dir on the host, or fails the run outright).
        if let Some(mount) = shared_session_mount(host, container)? {
            mounts.push(mount);
        }
    }
    Ok(mounts)
}

fn shared_session_mount(host: &str, container: &str) -> Result<Option<ContainerMount>> {
    let host = expand_path(Path::new(host))?;
    if !host.exists() {
        return Ok(None);
    }
    Ok(Some(ContainerMount {
        host: host.clone(),
        container: PathBuf::from(container),
        mode: Default::default(),
        // Unset: the `.claude.json` mount picks up the seed-by-default heuristic
        // in container::spawn; the directory mounts (.claude, .codex, …) don't.
        seed: None,
    }))
}

pub fn image_tag_for_stem(stem: &str) -> String {
    let mut slug = String::new();
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('-');
        }
    }
    if slug.is_empty() {
        slug.push_str("default");
    }
    format!("harness-hat-{slug}:local")
}

fn validate(config: &Config) -> Result<()> {
    for (profile_name, profile) in &config.env_profiles {
        for (key, value) in &profile.vars {
            anyhow::ensure!(
                crate::fs_util::is_valid_env_name(key),
                "env profile '{}': invalid environment variable name: {}",
                profile_name,
                key
            );
            anyhow::ensure!(
                !value.contains('\n') && !value.contains('\r'),
                "env profile '{}': value for {} must not contain newlines",
                profile_name,
                key
            );
        }
    }

    let mut seen = std::collections::HashSet::new();
    for proj in &config.workspaces {
        anyhow::ensure!(
            seen.insert(&proj.name),
            "duplicate project name: {}",
            proj.name
        );
        anyhow::ensure!(
            !proj.canonical_path.as_os_str().is_empty(),
            "project '{}': canonical_path is required",
            proj.name
        );
        anyhow::ensure!(
            proj.canonical_path.exists(),
            "project '{}': canonical_path does not exist: {}",
            proj.name,
            proj.canonical_path.display()
        );
        anyhow::ensure!(
            proj.canonical_path.is_dir(),
            "project '{}': canonical_path is not a directory: {}",
            proj.name,
            proj.canonical_path.display()
        );
    }
    let mut seen_containers = std::collections::HashSet::new();
    for ctr in &config.containers {
        anyhow::ensure!(
            seen_containers.insert(&ctr.name),
            "duplicate container name: {}",
            ctr.name
        );
        validate_command_argv(
            &format!("container profile '{}': command", ctr.name),
            ctr.command.as_deref(),
        )?;
        for path in &ctr.mcp_log_paths {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "container '{}': mcp_log_paths contains an empty path",
                ctr.name
            );
            anyhow::ensure!(
                path.is_absolute(),
                "container '{}': mcp_log_paths must be absolute container paths: {}",
                ctr.name,
                path.display()
            );
        }
        if let Some(pattern) = &ctr.mcp_log_pattern {
            anyhow::ensure!(
                !pattern.trim().is_empty(),
                "container '{}': mcp_log_pattern must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                !pattern.contains('\n') && !pattern.contains('\r'),
                "container '{}': mcp_log_pattern must not contain newlines",
                ctr.name
            );
        }
        for mount in &ctr.mounts {
            anyhow::ensure!(
                !mount.host.as_os_str().is_empty(),
                "container '{}': mount.host must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                !mount.container.as_os_str().is_empty(),
                "container '{}': mount.container must not be empty",
                ctr.name
            );
            anyhow::ensure!(
                mount.container.is_absolute(),
                "container '{}': mount.container must be an absolute path: {}",
                ctr.name,
                mount.container.display()
            );
        }
        for (key, value) in &ctr.env {
            anyhow::ensure!(
                crate::fs_util::is_valid_env_name(key),
                "container '{}': invalid environment variable name: {}",
                ctr.name,
                key
            );
            anyhow::ensure!(
                !value.contains('\n') && !value.contains('\r'),
                "container '{}': env value for {} must not contain newlines",
                ctr.name,
                key
            );
        }
        for name in &ctr.env_passthrough {
            anyhow::ensure!(
                !name.trim().is_empty(),
                "container '{}': env_passthrough contains an empty name",
                ctr.name
            );
            anyhow::ensure!(
                !name.contains('='),
                "container '{}': env_passthrough must be env var names only (no '='): {}",
                ctr.name,
                name
            );
        }
        for host in &ctr.bypass_proxy {
            anyhow::ensure!(
                !host.trim().is_empty(),
                "container '{}': bypass_proxy contains an empty host",
                ctr.name
            );
        }
        for forward in &ctr.localhost_forwards {
            anyhow::ensure!(
                forward.container_port > 0,
                "container '{}': localhost_forwards.container_port must be greater than zero",
                ctr.name
            );
            anyhow::ensure!(
                forward.effective_host_port() > 0,
                "container '{}': localhost_forwards.host_port must be greater than zero",
                ctr.name
            );
        }
        validate_optional_docker_value(&format!("container '{}': memory", ctr.name), &ctr.memory)?;
        validate_optional_docker_value(&format!("container '{}': cpus", ctr.name), &ctr.cpus)?;
        validate_optional_docker_value(
            &format!("container '{}': shm_size", ctr.name),
            &ctr.shm_size,
        )?;
    }
    Ok(())
}

fn validate_command_argv(field: &str, command: Option<&[String]>) -> Result<()> {
    let Some(command) = command else {
        return Ok(());
    };
    anyhow::ensure!(!command.is_empty(), "{field} must not be empty");
    for (idx, arg) in command.iter().enumerate() {
        anyhow::ensure!(!arg.trim().is_empty(), "{field}[{idx}] must not be empty");
    }
    Ok(())
}

fn validate_optional_docker_value(field: &str, value: &Option<String>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    anyhow::ensure!(!value.trim().is_empty(), "{field} must not be empty");
    anyhow::ensure!(
        !value.contains('\n') && !value.contains('\r'),
        "{field} must not contain newlines"
    );
    Ok(())
}

fn ensure_logging_instance_id(path: &Path, raw: &str, config: &mut Config) -> Result<()> {
    let current = config
        .logging
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(instance_id) = current {
        config.logging.instance_id = Some(instance_id);
        return Ok(());
    }

    let instance_id = uuid::Uuid::new_v4().to_string();
    let mut doc: DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing config document: {}", path.display()))?;
    doc["logging"]["instance_id"] = value(instance_id.clone());
    let rendered = doc.to_string();
    atomic_write_with_lock(path, rendered.as_bytes())
        .with_context(|| format!("writing config: {}", path.display()))?;
    config.logging.instance_id = Some(instance_id);
    Ok(())
}

/// Atomically write `contents` to `path` under an advisory exclusive file lock
/// taken on a sibling lockfile. The write goes to a tmp file in the same
/// directory (so `rename` is atomic on a single filesystem), is `fsync`'d, then
/// renamed over the destination.
pub(crate) fn atomic_write_with_lock(path: &Path, contents: &[u8]) -> Result<()> {
    use fs2::FileExt;
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating parent dir: {}", parent.display()))?;

    // Advisory lock co-located with the config so two harness processes serialize.
    let lock_path = path.with_extension({
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext.is_empty() {
            "lock".to_string()
        } else {
            format!("{ext}.lock")
        }
    });
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file: {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock_file)
        .with_context(|| format!("acquiring lock: {}", lock_path.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("creating tmp file in {}", parent.display()))?;
    tmp.write_all(contents)
        .with_context(|| format!("writing tmp file in {}", parent.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("fsyncing tmp file in {}", parent.display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("renaming tmp file over {}: {}", path.display(), e.error))?;

    // Best-effort: unlock on drop. Explicit unlock here is unnecessary because
    // `lock_file` dropping releases the lock.
    drop(lock_file);
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Expand `~` at the start of a path. `~user/foo` patterns are explicitly
/// rejected — silently treating them as a literal path would surprise users
/// who expect shell-style expansion.
pub fn expand_path(path: &Path) -> Result<PathBuf> {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(rest))
    } else if s == "~" {
        dirs::home_dir().context("cannot determine home directory")
    } else if s.starts_with('~') {
        anyhow::bail!(
            "path {:?} uses ~user-style expansion which is not supported; \
             use an explicit /home/<user>/... path or set $HOME",
            path
        )
    } else {
        Ok(path.to_path_buf())
    }
}
