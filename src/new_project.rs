use anyhow::{Context, Result};
use std::path::Path;

use crate::rules::{NetworkRules, ProjectRules};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Built-in project template families used when seeding a new workspace.
pub enum ProjectType {
    None,
    Go,
    Rust,
    Node,
    Python,
}

impl ProjectType {
    pub fn all() -> [Self; 5] {
        [Self::None, Self::Go, Self::Rust, Self::Node, Self::Python]
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
        }
    }

    pub fn next(self) -> Self {
        let all = Self::all();
        let pos = all.iter().position(|t| *t == self).unwrap_or(0);
        all[(pos + 1) % all.len()]
    }

    pub fn prev(self) -> Self {
        let all = Self::all();
        let pos = all.iter().position(|t| *t == self).unwrap_or(0);
        all[(pos + all.len() - 1) % all.len()]
    }
}

/// Create `harness-rules.toml` in a workspace directory if it does not exist.
pub fn write_rules_if_missing(workspace_dir: &Path, project_type: ProjectType) -> Result<bool> {
    let rules_path = workspace_dir.join("harness-rules.toml");
    if rules_path.exists() {
        return Ok(false);
    }
    let rules = if matches!(project_type, ProjectType::None) {
        ProjectRules::default()
    } else {
        default_rules(project_type)
    };
    crate::rules::write_rules_file(&rules_path, &rules)
        .with_context(|| format!("writing {}", rules_path.display()))?;
    Ok(true)
}

/// Return the starter rule set for a new project type.
pub fn default_rules(project_type: ProjectType) -> ProjectRules {
    let mut allowlist = Vec::new();
    if !matches!(project_type, ProjectType::None) {
        allowlist.extend([
            "domain=github.com".to_string(),
            "domain=api.github.com".to_string(),
            "domain=raw.githubusercontent.com".to_string(),
            "domain=objects.githubusercontent.com".to_string(),
        ]);
    }
    match project_type {
        ProjectType::None => {}
        ProjectType::Rust => allowlist.extend([
            "domain=crates.io".to_string(),
            "domain=static.crates.io".to_string(),
            "domain=index.crates.io".to_string(),
        ]),
        ProjectType::Node => allowlist.extend([
            "domain=registry.npmjs.org".to_string(),
            "domain=*.npmjs.org".to_string(),
        ]),
        ProjectType::Python => allowlist.extend([
            "domain=pypi.org".to_string(),
            "domain=files.pythonhosted.org".to_string(),
        ]),
        ProjectType::Go => allowlist.extend([
            "domain=pkg.go.dev".to_string(),
            "domain=sum.golang.org".to_string(),
            "domain=proxy.golang.org".to_string(),
        ]),
    }

    ProjectRules {
        network: NetworkRules {
            allowlist,
            denylist: Vec::new(),
        },
        ..ProjectRules::default()
    }
}

/// Append a workspace block to `harness-hat.toml` using `toml_edit` to
/// preserve formatting and avoid corruption from partial writes. The write
/// goes through an advisory file lock + tmp-rename like the rest of the
/// config-mutation path.
pub fn append_project_block(
    config_path: &Path,
    project_name: &str,
    canonical_path: &Path,
    sidebar_hotkey: Option<char>,
) -> Result<()> {
    anyhow::ensure!(
        !project_name.trim().is_empty(),
        "workspace name must not be empty"
    );
    // Validate that name and canonical path can be encoded as TOML basic
    // strings even though `toml_edit` is doing the actual encoding now —
    // rejects control chars and newlines that would otherwise round-trip
    // through `toml_edit::value` but render misleadingly elsewhere.
    let _ = toml_basic_string(project_name)?;
    let canonical_display = canonical_path.display().to_string();
    let _ = toml_basic_string(&canonical_display)?;

    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading config for append: {}", config_path.display()));
        }
    };
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing config document: {}", config_path.display()))?;

    let mut new_table = toml_edit::Table::new();
    new_table["name"] = toml_edit::value(project_name.to_string());
    new_table["canonical_path"] = toml_edit::value(canonical_display);
    if let Some(ch) = sidebar_hotkey {
        new_table["sidebar_hotkey"] = toml_edit::value(ch.to_ascii_lowercase().to_string());
    }

    // Ensure `workspaces` exists and is an array-of-tables; if absent, seed
    // it. Then push the new table.
    if doc.get("workspaces").is_none() {
        doc.insert(
            "workspaces",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let workspaces = doc
        .get_mut("workspaces")
        .and_then(|i| i.as_array_of_tables_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: [[workspaces]] is not an array of tables; refusing to append",
                config_path.display()
            )
        })?;
    workspaces.push(new_table);

    crate::config::atomic_write_with_lock(config_path, doc.to_string().as_bytes())
        .with_context(|| format!("appending workspace to {}", config_path.display()))?;
    Ok(())
}

/// Remove a workspace block from `harness-hat.toml` by name.
///
/// Removes entries from both `[[workspaces]]` and legacy `[[projects]]` arrays.
/// Returns true if at least one block was removed.
pub fn remove_workspace_block(config_path: &Path, workspace_name: &str) -> Result<bool> {
    anyhow::ensure!(
        !workspace_name.trim().is_empty(),
        "workspace name must not be empty"
    );
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config: {}", config_path.display()))?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing config document: {}", config_path.display()))?;

    let mut removed = false;
    for key in ["workspaces", "projects"] {
        let Some(item) = doc.get_mut(key) else {
            continue;
        };
        let Some(aot) = item.as_array_of_tables_mut() else {
            continue;
        };
        for idx in (0..aot.len()).rev() {
            let matches = aot
                .get(idx)
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str())
                .map(|n| n == workspace_name)
                .unwrap_or(false);
            if matches {
                aot.remove(idx);
                removed = true;
            }
        }
    }

    if removed {
        std::fs::write(config_path, doc.to_string())
            .with_context(|| format!("writing config: {}", config_path.display()))?;
    }
    Ok(removed)
}

fn toml_basic_string(s: &str) -> Result<String> {
    anyhow::ensure!(
        !s.contains('\n') && !s.contains('\r'),
        "value must not contain newlines"
    );
    // TOML basic strings forbid all control characters below 0x20 (and DEL,
    // U+007F) except `\t`. Reject these explicitly rather than emitting a
    // string that the config parser would later reject — and that would
    // otherwise have the chance to silently corrupt the file.
    if let Some(bad) = s
        .chars()
        .find(|c| (*c < ' ' && *c != '\t') || *c == '\u{7f}')
    {
        anyhow::bail!(
            "value must not contain control character U+{:04X}",
            bad as u32
        );
    }
    let escaped = s.replace('\\', "\\\\").replace('\"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

#[cfg(test)]
mod tests {
    use super::{
        ProjectType, append_project_block, default_rules, remove_workspace_block,
        write_rules_if_missing,
    };
    use crate::config::Config;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_temp_root() -> PathBuf {
        if cfg!(unix) {
            let unix_tmp = Path::new("/tmp");
            if unix_tmp.exists() {
                return unix_tmp.to_path_buf();
            }
        }
        std::env::temp_dir()
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = test_temp_root().join(format!("harness-hat-new-project-{prefix}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn with_temp_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::TEST_ENV_LOCK.lock().expect("test env lock");
        let original_home = std::env::var_os("HOME");
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        let result = f();
        match original_home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match original_xdg_data_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_DATA_HOME");
            },
        }
        result
    }

    #[test]
    fn templates_include_expected_network_allowlist() {
        let rules = default_rules(ProjectType::Rust);
        assert!(
            rules
                .network
                .allowlist
                .contains(&"domain=github.com".to_string())
        );
        assert!(
            rules
                .network
                .allowlist
                .contains(&"domain=crates.io".to_string())
        );
    }

    #[test]
    fn none_project_type_has_no_starter_network_rules() {
        let rules = default_rules(ProjectType::None);
        assert!(rules.network.allowlist.is_empty());
    }

    #[test]
    fn rules_write_is_idempotent_when_file_exists() {
        let root = unique_temp_dir("rules-idempotent");
        fs::create_dir_all(root.join("canon")).expect("create canon");
        let rules_path = root.join("canon").join("harness-rules.toml");
        fs::write(&rules_path, "sentinel").expect("write sentinel");

        let wrote = write_rules_if_missing(&root.join("canon"), ProjectType::Node).expect("write");
        assert!(!wrote);
        assert_eq!(fs::read_to_string(&rules_path).expect("read"), "sentinel");
    }

    #[test]
    fn none_project_type_creates_empty_rules_file() {
        let root = unique_temp_dir("rules-none");
        fs::create_dir_all(root.join("canon")).expect("create canon");
        let wrote = write_rules_if_missing(&root.join("canon"), ProjectType::None).expect("write");
        assert!(wrote);
        let rules =
            crate::rules::load(&root.join("canon").join("harness-rules.toml")).expect("load rules");
        assert!(rules.network.allowlist.is_empty());
    }

    #[test]
    fn starter_rules_header_is_network_only() {
        let root = unique_temp_dir("rules-timeout-guidance");
        let canon = root.join("canon");
        fs::create_dir_all(&canon).expect("create canon");

        let wrote = write_rules_if_missing(&canon, ProjectType::Rust).expect("write rules");
        assert!(wrote);

        let rendered = fs::read_to_string(canon.join("harness-rules.toml")).expect("read rules");
        assert!(rendered.contains("Network (HTTP/HTTPS proxy policy)"));
        assert!(rendered.contains("[network]"));
        assert!(rendered.contains("allowlist"));
    }

    #[test]
    fn config_append_block_parses_direct_workspace() {
        let root = unique_temp_dir("append-config");
        let config_path = root.join("harness-hat.toml");
        let canon = root.join("canon");
        let docker_dir = root.join("docker-root");
        fs::create_dir_all(&canon).expect("create canon");
        fs::create_dir_all(&docker_dir).expect("create docker dir");

        let toml_path = |path: &Path| toml::Value::String(path.display().to_string()).to_string();
        let raw = format!(
            r#"
docker_dir = {}

[manager]
global_rules_file = {}
"#,
            toml_path(&docker_dir),
            toml_path(&root.join("global-rules.toml")),
        );
        fs::write(&config_path, raw).expect("write base config");

        append_project_block(&config_path, "proj", &canon, Some('p')).expect("append");
        let cfg: Config =
            with_temp_home(&root, || crate::config::load(&config_path)).expect("load");

        let proj = cfg.workspaces.first().expect("project");
        assert_eq!(proj.name, "proj");
        assert_eq!(proj.sidebar_hotkey.as_deref(), Some("p"));
        assert_eq!(
            proj.canonical_path,
            canon.canonicalize().expect("canonical workspace")
        );
    }

    #[test]
    fn config_append_block_does_not_emit_sync_section() {
        let root = unique_temp_dir("append-config-direct");
        let config_path = root.join("harness-hat.toml");
        let canon = root.join("canon");
        let docker_dir = root.join("docker-root");
        fs::create_dir_all(&canon).expect("create canon");
        fs::create_dir_all(&docker_dir).expect("create docker dir");

        let toml_path = |path: &Path| toml::Value::String(path.display().to_string()).to_string();
        let raw = format!(
            r#"
docker_dir = {}

[manager]
global_rules_file = {}
"#,
            toml_path(&docker_dir),
            toml_path(&root.join("global-rules.toml")),
        );
        fs::write(&config_path, raw).expect("write base config");

        append_project_block(&config_path, "proj", &canon, None).expect("append");
        let cfg: Config =
            with_temp_home(&root, || crate::config::load(&config_path)).expect("load");

        let proj = cfg.workspaces.first().expect("project");
        assert_eq!(proj.name, "proj");
        assert_eq!(
            proj.canonical_path,
            canon.canonicalize().expect("canonical workspace")
        );
        let raw = fs::read_to_string(&config_path).expect("read config");
        assert!(!raw.contains("[workspaces.sync]"));
    }

    #[test]
    fn remove_workspace_block_removes_matching_workspaces() {
        let root = unique_temp_dir("config-remove-workspace");
        let config_path = root.join("harness-hat.toml");
        fs::write(
            &config_path,
            r#"
[[workspaces]]
name = "a"
canonical_path = "/tmp/a"

[[workspaces]]
name = "b"
canonical_path = "/tmp/b"
"#,
        )
        .expect("write config");

        let removed = remove_workspace_block(&config_path, "a").expect("remove");
        assert!(removed);

        let cfg = fs::read_to_string(&config_path).expect("read config");
        assert!(!cfg.contains("name = \"a\""));
        assert!(cfg.contains("name = \"b\""));
    }

    #[test]
    fn remove_workspace_block_supports_legacy_projects_key() {
        let root = unique_temp_dir("config-remove-legacy-projects");
        let config_path = root.join("harness-hat.toml");
        fs::write(
            &config_path,
            r#"
[[projects]]
name = "legacy-a"
canonical_path = "/tmp/a"
"#,
        )
        .expect("write config");

        let removed = remove_workspace_block(&config_path, "legacy-a").expect("remove");
        assert!(removed);
        let cfg = fs::read_to_string(&config_path).expect("read config");
        assert!(!cfg.contains("legacy-a"));
    }
}
