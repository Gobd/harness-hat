use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::AliasValue;
use crate::rules::{HostdoRules, ProjectRules};

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
    if matches!(project_type, ProjectType::None) {
        return Ok(false);
    }
    let rules_path = workspace_dir.join("harness-rules.toml");
    if rules_path.exists() {
        return Ok(false);
    }
    let rules = default_rules(project_type);
    crate::rules::write_rules_file(&rules_path, &rules)
        .with_context(|| format!("writing {}", rules_path.display()))?;
    Ok(true)
}

/// Return the starter rule set for a new project type.
pub fn default_rules(project_type: ProjectType) -> ProjectRules {
    let command_aliases = match project_type {
        ProjectType::None => HashMap::new(),
        ProjectType::Rust => aliases([
            ("build", "cargo build"),
            ("check", "cargo check"),
            ("test", "cargo test"),
            ("fmt", "cargo fmt"),
            ("lint", "cargo clippy"),
        ]),
        ProjectType::Node => aliases([
            ("install", "npm install"),
            ("test", "npm run test"),
            ("lint", "npm run lint"),
            ("build", "npm run build"),
            ("dev", "npm run dev"),
            ("pnpm_test", "pnpm test"),
            ("yarn_test", "yarn test"),
            ("bun_test", "bun test"),
        ]),
        ProjectType::Python => aliases([
            ("test", "pytest"),
            ("pytest", "pytest"),
            ("unittest", "python -m unittest"),
            ("ruff", "ruff check ."),
            ("black", "black ."),
            ("flake8", "flake8"),
            ("mypy", "mypy ."),
            ("pip_install", "pip install -r requirements.txt"),
            ("poetry_install", "poetry install"),
        ]),
        ProjectType::Go => aliases([
            ("build", "go build ./..."),
            ("test", "go test ./..."),
            ("fmt", "gofmt -w ."),
            ("vet", "go vet ./..."),
            ("tidy", "go mod tidy"),
        ]),
    };

    ProjectRules {
        hostdo: HostdoRules {
            command_aliases,
            ..HostdoRules::default()
        },
        ..ProjectRules::default()
    }
}

fn aliases<const N: usize>(
    items: [(&'static str, &'static str); N],
) -> HashMap<String, AliasValue> {
    items
        .into_iter()
        .map(|(name, cmd)| {
            (
                name.to_string(),
                AliasValue::WithOptions {
                    cmd: cmd.to_string(),
                    cwd: Some(PathBuf::from("$WORKSPACE")),
                },
            )
        })
        .collect()
}

/// Append a workspace block to `harness-hat.toml` using the built-in template.
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

    let name = toml_basic_string(project_name)?;
    let canonical = toml_basic_string(&canonical_path.display().to_string())?;

    let hotkey_line = sidebar_hotkey
        .map(|ch| format!("sidebar_hotkey = \"{}\"\n", ch.to_ascii_lowercase()))
        .unwrap_or_default();

    let block = format!(
        r#"

[[workspaces]]
name = {name}
canonical_path = {canonical}
{hotkey_line}"#
    );

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_path)
        .with_context(|| format!("opening config for append: {}", config_path.display()))?;
    f.write_all(block.as_bytes())
        .with_context(|| format!("appending to {}", config_path.display()))?;
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harness-hat-new-project-{prefix}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn templates_include_expected_aliases_and_excludes() {
        let rules = default_rules(ProjectType::Rust);
        assert!(rules.hostdo.command_aliases.contains_key("build"));
        let build = rules
            .hostdo
            .command_aliases
            .get("build")
            .expect("build alias");
        match build {
            crate::config::AliasValue::WithOptions { cmd, cwd } => {
                assert_eq!(cmd, "cargo build");
                assert_eq!(cwd.as_ref().unwrap().as_os_str(), "$WORKSPACE");
            }
            _ => panic!("expected WithOptions alias"),
        }
    }

    #[test]
    fn none_project_type_has_no_starter_rules() {
        let rules = default_rules(ProjectType::None);
        assert!(rules.hostdo.command_aliases.is_empty());
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
    fn none_project_type_skips_rules_creation() {
        let root = unique_temp_dir("rules-none");
        fs::create_dir_all(root.join("canon")).expect("create canon");
        let wrote = write_rules_if_missing(&root.join("canon"), ProjectType::None).expect("write");
        assert!(!wrote);
        assert!(!root.join("canon").join("harness-rules.toml").exists());
    }

    #[test]
    fn starter_rules_header_includes_hostdo_timeout_guidance() {
        let root = unique_temp_dir("rules-timeout-guidance");
        let canon = root.join("canon");
        fs::create_dir_all(&canon).expect("create canon");

        let wrote = write_rules_if_missing(&canon, ProjectType::Rust).expect("write rules");
        assert!(wrote);

        let rendered = fs::read_to_string(canon.join("harness-rules.toml")).expect("read rules");
        assert!(rendered.contains("hostdo run --timeout <seconds> ..."));
        assert!(rendered.contains("hostdo run --timeout 120 cargo test"));
        assert!(rendered.contains("hostdo run ..."));
        assert!(rendered.contains("hostdo tail <job-id> --rows <lines>"));
        assert!(rendered.contains("hostdo send <job-id>"));
    }

    #[test]
    fn config_append_block_parses_direct_workspace() {
        let root = unique_temp_dir("append-config");
        let config_path = root.join("harness-hat.toml");
        let canon = root.join("canon");
        let docker_dir = root.join("docker-root");
        fs::create_dir_all(&canon).expect("create canon");
        fs::create_dir_all(&docker_dir).expect("create docker dir");

        let raw = format!(
            r#"
docker_dir = "{}"

[workspace]

[manager]
global_rules_file = "{}"
"#,
            docker_dir.display(),
            root.join("global-rules.toml").display(),
        );
        fs::write(&config_path, raw).expect("write base config");

        append_project_block(&config_path, "proj", &canon, Some('p')).expect("append");
        let cfg: Config = crate::config::load(&config_path).expect("load");

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

        let raw = format!(
            r#"
docker_dir = "{}"

[workspace]

[manager]
global_rules_file = "{}"
"#,
            docker_dir.display(),
            root.join("global-rules.toml").display(),
        );
        fs::write(&config_path, raw).expect("write base config");

        append_project_block(&config_path, "proj", &canon, None).expect("append");
        let cfg: Config = crate::config::load(&config_path).expect("load");

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
