use anyhow::Context;
use std::path::{Path, PathBuf};

use crate::rules::{ApprovalMode, HostdoRules, NetworkRules, ProjectRules};
// ── harness-rules.toml starter ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CreatedRulesFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentConfigInjectionResult {
    pub created_rules: Option<CreatedRulesFile>,
}

/// Generate a starter `harness-rules.toml` for the given container profile.
///
/// Includes common-sense `auto`-approved rules for developer tools (GitHub,
/// npm, PyPI, crates.io) plus any profile-specific allowlist entries from the
/// config file. The default policy for anything not listed is `prompt`, so the
/// developer still sees and approves unexpected destinations.
pub fn generate_starter_project_rules(extra_allowlist: &[String]) -> ProjectRules {
    let mut allowlist = vec![
        "domain=github.com".to_string(),
        "domain=api.github.com".to_string(),
        "domain=raw.githubusercontent.com".to_string(),
        "domain=objects.githubusercontent.com".to_string(),
        "domain=registry.npmjs.org".to_string(),
        "domain=*.npmjs.org".to_string(),
        "domain=pypi.org".to_string(),
        "domain=files.pythonhosted.org".to_string(),
        "domain=crates.io".to_string(),
        "domain=static.crates.io".to_string(),
        "domain=index.crates.io".to_string(),
        "domain=rubygems.org".to_string(),
        "domain=api.rubygems.org".to_string(),
        "domain=pkg.go.dev".to_string(),
        "domain=sum.golang.org".to_string(),
        "domain=proxy.golang.org".to_string(),
    ];
    allowlist.extend(extra_allowlist.iter().cloned());

    ProjectRules {
        version: crate::rules::CURRENT_RULES_VERSION,
        llm_instructions: None,
        agentctl: Default::default(),
        hostdo: HostdoRules {
            default_policy: ApprovalMode::Prompt,
            ..HostdoRules::default()
        },
        network: NetworkRules {
            allowlist,
            denylist: Vec::new(),
        },
    }
}

// ── inject_agent_config ───────────────────────────────────────────────────────

/// Inject workspace guidance and, if no
/// `harness-rules.toml` exists in the canonical project directory, write a
/// starter one with sensible network allowlist rules.
///
/// Called just before spawning a container so the files are present on the
/// bind-mounted workspace when the container starts.
pub fn inject_agent_config(
    workspace_path: &Path,
    canonical_path: &Path,
    project_name: &str,
    direct_mount: bool,
    _mount_target: &Path,
    _exec_url: &str,
    _proxy_url: &str,
    starter_network_allowlist: &[String],
) -> anyhow::Result<AgentConfigInjectionResult> {
    // Ensure the workspace directory exists (it may not have been seeded yet).
    std::fs::create_dir_all(workspace_path).with_context(|| {
        format!(
            "creating workspace directory '{}'",
            workspace_path.display()
        )
    })?;

    // Write a starter harness-rules.toml to the canonical project dir if absent.
    // This is the file the server/proxy reads for policy enforcement.
    let rules_path = canonical_path.join("harness-rules.toml");
    let created_rules = if !rules_path.exists() {
        std::fs::create_dir_all(canonical_path).with_context(|| {
            format!(
                "creating canonical project directory '{}'",
                canonical_path.display()
            )
        })?;
        let mut starter = generate_starter_project_rules(starter_network_allowlist);
        starter.llm_instructions = Some(format!(
            "Project: {project_name}\n\
\n\
Environment:\n\
- You are operating inside a Linux Docker container.\n\
- Workspace mount path (inside container): {}\n\
{}\n\
- Use `hostdo ...` for host-side build/package tooling such as cargo, npm, pnpm, yarn, go, make, pytest, or similar commands.\n\
- Examples: `hostdo cargo test`, `hostdo npm install`, `hostdo go test ./...`.\n\
- Only use `hostdo --image <docker-image> ...` when the user explicitly asks you to run against a Docker image or containerized runner; it runs a command in a short-lived Docker runner instead of directly on the host, for example `hostdo --image node:20 npm test` or `hostdo --image rust:1.88 cargo test`.\n\
- Prefer existing auto-approved `hostdo` commands or `hostdo.command_aliases` before asking for a new host command approval.\n\
- Run `agentctl list` first to see the configured subagent profiles available in this environment. Use the profile name from the first column; do not guess hardcoded names such as `codex`, `claude`, or `qwen`.\n\
- Use `agentctl spawn <profile> [--name <child>]` to start same-workspace subagents from one of those configured container profiles.\n\
- After spawning, give the child a task with `agentctl send <child> \"task prompt\" --enter`.\n\
- A typical sequence is `agentctl list`, `agentctl spawn <profile> --name review`, `agentctl status review`, `agentctl tail review --rows 30`, then `agentctl send review \"inspect the failing test\" --enter`.\n\
- Use `agentctl spawn-many <profile> <count> --prefix <name>` for larger batches; launches are paced by `[agentctl].spawn_delay_ms` and never below 100ms between spawn requests.\n\
- `[agentctl].max_subagents` limits live descendants under a single top-level agent, including subagents, sub-subagents, and deeper descendants.\n\
- Use `agentctl status <child>`, `agentctl tail <child> --rows 30`, `agentctl tail <child> --all`, `agentctl send <child> \"text\" --enter`, `agentctl send <child> --key enter`, and `agentctl stop <child>` to inspect and control direct child agents.\n\
- If `agentctl list` reports `image-missing`, the profile exists but its Docker image must be built or pulled before `agentctl spawn` will work.\n\
- Subagent names are scoped to the parent that created them; duplicate names may exist elsewhere in the tree.\n\
\n\
Rules of engagement:\n\
- Read and follow this file before taking actions.\n\
- Use `hostdo` only when the user explicitly asks for host activity.\n\
- Prefer `hostdo` when the requested command needs host tools or host package/build caches.\n\
- Use `killme` only when the user explicitly asks to end this container.\n\
- Network access is filtered by harness-hat; allowed destinations are in `[network]`.\n",
            _mount_target.display(),
            if direct_mount {
                "- This project uses direct-mount sync; edits persist to the host."
            } else {
                "- This project uses a managed workspace; be careful about canonical vs workspace paths."
            }
        ));
        let content = crate::rules::render_rules_file(&starter)
            .with_context(|| format!("rendering starter rules file '{}'", rules_path.display()))?;
        std::fs::write(&rules_path, &content)
            .with_context(|| format!("writing starter rules file '{}'", rules_path.display()))?;
        Some(CreatedRulesFile {
            path: rules_path,
            content,
        })
    } else {
        None
    };

    Ok(AgentConfigInjectionResult { created_rules })
}

/// Instructions shown to the developer after first CA generation.
/// Return the CA bootstrap instructions used inside generated agent guidance.
pub fn ca_setup_instructions(_ca_cert_pem: &str, ca_cert_path: &str) -> String {
    format!(
        r#"── harness-hat CA Certificate ─────────────────────────────────────────
The proxy CA was generated.  Containers must trust it.

  Export path: {ca_cert_path}

  In your Dockerfile:
    COPY harness-hat-ca.crt /usr/local/share/ca-certificates/
    RUN update-ca-certificates          # Debian/Ubuntu
    # or: update-ca-trust               # RHEL/Fedora

  Runtime env vars (included in the docker run snippet):
    NODE_EXTRA_CA_CERTS, REQUESTS_CA_BUNDLE, CODEX_CA_CERTIFICATE, SSL_CERT_FILE

  Set HARNESS_HAT_CA_CERT_PATH to the cert file location so the snippet works:
    export HARNESS_HAT_CA_CERT_PATH={ca_cert_path}
────────────────────────────────────────────────────────────────────────────────
"#,
        ca_cert_path = ca_cert_path,
    )
}

#[cfg(test)]
mod tests {
    use super::{generate_starter_project_rules, inject_agent_config};
    use std::fs;

    #[test]
    fn starter_rules_include_profile_hosts() {
        let rules = generate_starter_project_rules(&[
            "domain=generativelanguage.googleapis.com".to_string(),
            "domain=accounts.google.com".to_string(),
            "domain=oauth2.googleapis.com".to_string(),
        ]);
        let allowlist = rules.network.allowlist;
        assert!(
            allowlist
                .iter()
                .any(|r| r == "domain=generativelanguage.googleapis.com")
        );
        assert!(allowlist.iter().any(|r| r == "domain=accounts.google.com"));
        assert!(
            allowlist
                .iter()
                .any(|r| r == "domain=oauth2.googleapis.com")
        );
    }

    #[test]
    fn injected_starter_rules_prefer_existing_approved_hostdo_commands() {
        let root =
            std::env::temp_dir().join(format!("harness-hat-agent-rules-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");

        let result = inject_agent_config(
            &workspace,
            &workspace,
            "project-a",
            true,
            std::path::Path::new("/workspace"),
            "http://127.0.0.1:0",
            "http://127.0.0.1:0",
            &[],
        )
        .expect("inject rules");

        let created = result.created_rules.expect("starter rules file");
        let contents = fs::read_to_string(created.path).expect("read starter rules");
        assert!(contents.contains(
            "Prefer existing auto-approved `hostdo` commands or `hostdo.command_aliases`"
        ));
    }
}
