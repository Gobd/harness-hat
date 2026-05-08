/// Parses `harness-rules.toml` files and composes global + per-project rules.
///
/// `harness-rules.toml` lives in the canonical project root (committed to git).
/// It controls what the AI agent is allowed to do: which host-side commands
/// can run, and which network destinations are reachable.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::AliasValue;

pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_AGENTCTL_SPAWN_DELAY_MS: u64 = 500;
pub const MIN_AGENTCTL_SPAWN_DELAY_MS: u64 = 100;
pub const DEFAULT_AGENTCTL_MAX_SUBAGENTS: usize = 10;

// ── Enums (re-used across config and rules) ──────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Auto,
    Prompt,
    Deny,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        Self::Prompt
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    Queue,
    Reject,
    Parallel,
}

impl Default for ConcurrencyPolicy {
    fn default() -> Self {
        Self::Queue
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Auto,
    Prompt,
    Deny,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::Prompt
    }
}

// ── harness-rules.toml schema ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ProjectRules {
    /// Optional instructions for a human or LLM agent. This field is preserved
    /// across automatic edits to this file (e.g. when harness-hat appends a new
    /// `hostdo` command rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_instructions: Option<String>,
    #[serde(default)]
    pub agentctl: AgentctlRules,
    #[serde(default)]
    pub hostdo: HostdoRules,
    #[serde(default)]
    pub network: NetworkRules,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AgentctlRules {
    #[serde(default = "default_agentctl_spawn_delay_ms")]
    pub spawn_delay_ms: u64,
    #[serde(default = "default_agentctl_max_subagents")]
    pub max_subagents: usize,
}

impl AgentctlRules {
    pub fn effective_spawn_delay_ms(&self) -> u64 {
        self.spawn_delay_ms.max(MIN_AGENTCTL_SPAWN_DELAY_MS)
    }
}

impl Default for AgentctlRules {
    fn default() -> Self {
        Self {
            spawn_delay_ms: DEFAULT_AGENTCTL_SPAWN_DELAY_MS,
            max_subagents: DEFAULT_AGENTCTL_MAX_SUBAGENTS,
        }
    }
}

fn default_agentctl_spawn_delay_ms() -> u64 {
    DEFAULT_AGENTCTL_SPAWN_DELAY_MS
}

fn default_agentctl_max_subagents() -> usize {
    DEFAULT_AGENTCTL_MAX_SUBAGENTS
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct HostdoRules {
    #[serde(default)]
    pub default_policy: ApprovalMode,
    #[serde(default)]
    pub commands: Vec<RuleCommand>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub command_aliases: HashMap<String, AliasValue>,
}

/// A single allowed host-side command.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RuleCommand {
    /// Optional human-readable label shown in the TUI and audit log.
    /// Defaults to the argv joined with spaces when absent.
    pub name: Option<String>,
    /// Exact argv that must match the request.
    pub argv: Vec<String>,
    /// Optional Docker image for short-lived container execution.
    ///
    /// `None` means the command runs directly on the host. `Some(image)` means
    /// the command only matches requests made as `hostdo --image <image> ...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Absolute path on the host. Use the canonical project path.
    pub cwd: String,
    pub env_profile: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub concurrency: ConcurrencyPolicy,
    pub approval_mode: ApprovalMode,
}

impl RuleCommand {
    /// Human-readable label for TUI display and audit log.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.argv.join(" "))
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetworkRules {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denylist: Vec<String>,
}

impl Default for NetworkRules {
    fn default() -> Self {
        Self {
            allowlist: vec![],
            denylist: vec![],
        }
    }
}

/// One parsed network rule in Coder Agent Firewall style:
/// `method=GET,POST domain=api.example.com path=/api/*,/auth/* port=443,8443`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NetworkRule {
    pub methods: Vec<String>,
    pub domains: Vec<String>,
    pub paths: Vec<String>,
    pub ports: Vec<u16>,
}

// ── ComposedRules ────────────────────────────────────────────────────────────

/// Effective rule set for a given request context.
/// Global rules and all project rules are unioned together.
#[derive(Debug, Clone, Default)]
pub struct ComposedRules {
    pub agentctl: AgentctlRules,
    pub hostdo: HostdoRules,
    pub network_rules: Vec<NetworkRule>,
    pub network_deny_rules: Vec<NetworkRule>,
    pub network_default: NetworkPolicy,
}

impl ComposedRules {
    /// Compose global rules + one or more project rule sets.
    /// Global rules come first (higher priority in declaration order).
    pub fn compose(global: &ProjectRules, projects: &[ProjectRules]) -> Self {
        let mut commands = global.hostdo.commands.clone();
        let mut network_allowlist = global.network.allowlist.clone();
        let mut network_denylist = global.network.denylist.clone();
        let mut agentctl = global.agentctl.clone();

        // Union: add project rules that don't duplicate a global argv.
        for proj in projects {
            agentctl = proj.agentctl.clone();
            for cmd in &proj.hostdo.commands {
                if !commands
                    .iter()
                    .any(|c| c.argv == cmd.argv && c.image == cmd.image)
                {
                    commands.push(cmd.clone());
                }
            }
            network_allowlist.extend(proj.network.allowlist.clone());
            network_denylist.extend(proj.network.denylist.clone());
        }

        // Hostdo default policy: most restrictive wins (deny > prompt > auto).
        let hostdo_default = std::iter::once(&global.hostdo.default_policy)
            .chain(projects.iter().map(|p| &p.hostdo.default_policy))
            .fold(ApprovalMode::Auto, |acc, p| match (&acc, p) {
                (ApprovalMode::Deny, _) | (_, ApprovalMode::Deny) => ApprovalMode::Deny,
                (ApprovalMode::Prompt, _) | (_, ApprovalMode::Prompt) => ApprovalMode::Prompt,
                _ => ApprovalMode::Auto,
            });

        let mut network_rules = Vec::new();
        let mut seen = HashSet::new();
        for raw in network_allowlist {
            if !seen.insert(raw.clone()) {
                continue;
            }
            // `load()` validates the syntax, so skip invalid entries defensively.
            if let Ok(parsed) = parse_network_allowlist_rule(&raw) {
                network_rules.push(parsed);
            }
        }
        let mut network_deny_rules = Vec::new();
        let mut seen = HashSet::new();
        for raw in network_denylist {
            if !seen.insert(raw.clone()) {
                continue;
            }
            // `load()` validates the syntax, so skip invalid entries defensively.
            if let Ok(parsed) = parse_network_allowlist_rule(&raw) {
                network_deny_rules.push(parsed);
            }
        }

        Self {
            agentctl,
            hostdo: HostdoRules {
                default_policy: hostdo_default,
                commands,
                command_aliases: HashMap::new(),
            },
            network_rules,
            network_deny_rules,
            // Coder-style rules engine is explicit allowlist with prompt-by-default.
            network_default: NetworkPolicy::Prompt,
        }
    }

    /// Find the effective network policy for a given request.
    pub fn match_network(&self, method: &str, host: &str, path: &str) -> NetworkPolicy {
        self.match_network_for_port(method, host, path, None)
    }

    /// Find the effective network policy for a request with a known destination port.
    pub fn match_network_for_port(
        &self,
        method: &str,
        host: &str,
        path: &str,
        port: Option<u16>,
    ) -> NetworkPolicy {
        if self
            .network_deny_rules
            .iter()
            .any(|r| network_rule_matches(r, method, host, path, port))
        {
            return NetworkPolicy::Deny;
        }
        if self
            .network_rules
            .iter()
            .any(|r| network_rule_matches(r, method, host, path, port))
        {
            NetworkPolicy::Auto
        } else {
            self.network_default.clone()
        }
    }

    /// Find the effective policy for CONNECT preflight.
    ///
    /// Domain-only allow rules approve HTTPS CONNECT on 443. Raw TCP CONNECT to
    /// other ports must be explicitly allowed with `port=...`. Domain-only deny
    /// rules remain broad and deny every port for that host.
    pub fn match_connect(&self, host: &str, port: u16) -> NetworkPolicy {
        if self
            .network_deny_rules
            .iter()
            .any(|r| connect_rule_matches(r, host, port, true))
        {
            return NetworkPolicy::Deny;
        }
        if self
            .network_rules
            .iter()
            .any(|r| connect_rule_matches(r, host, port, false))
        {
            NetworkPolicy::Auto
        } else {
            self.network_default.clone()
        }
    }

    /// Expand `$WORKSPACE` prefixes in rule command cwds.
    pub fn expand_cwd_vars(&mut self, workspace_path: &str) {
        for cmd in &mut self.hostdo.commands {
            if cmd.cwd == "$WORKSPACE" {
                cmd.cwd = workspace_path.to_string();
            } else if let Some(rest) = cmd.cwd.strip_prefix("$WORKSPACE/") {
                cmd.cwd = format!("{workspace_path}/{rest}");
            }
        }
    }

    /// Find an exact-match hostdo command by argv.
    ///
    /// `cwd` remains part of the rule because it still determines where the
    /// command runs and is persisted for developer review, but it is not part
    /// of the approval identity.
    pub fn find_hostdo_command<'a>(&'a self, argv: &[String]) -> Option<&'a RuleCommand> {
        self.find_hostdo_command_for_target(argv, None)
    }

    /// Find an exact-match hostdo command by argv and execution image.
    pub fn find_hostdo_command_for_target<'a>(
        &'a self,
        argv: &[String],
        image: Option<&str>,
    ) -> Option<&'a RuleCommand> {
        self.hostdo
            .commands
            .iter()
            .find(|c| c.argv == argv && c.image.as_deref() == image)
    }
}

fn network_rule_matches(
    rule: &NetworkRule,
    method: &str,
    host: &str,
    path: &str,
    port: Option<u16>,
) -> bool {
    method_matches(&rule.methods, method)
        && domain_matches(&rule.domains, host)
        && path_matches(&rule.paths, path)
        && port_matches(&rule.ports, port)
}

fn connect_rule_matches(rule: &NetworkRule, host: &str, port: u16, deny_rule: bool) -> bool {
    method_matches(&rule.methods, "CONNECT")
        && domain_matches(&rule.domains, host)
        && path_matches(&rule.paths, "/")
        && connect_port_matches(&rule.ports, port, deny_rule)
}

fn method_matches(patterns: &[String], method: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|m| m.eq_ignore_ascii_case(method))
}

fn domain_matches(patterns: &[String], host: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|pattern| {
        if pattern == "*" {
            return true;
        }
        // Coder semantics: "*.example.com" matches subdomains, not apex.
        if let Some(apex) = pattern.strip_prefix("*.") {
            let host_lc = host.to_ascii_lowercase();
            let apex_lc = apex.to_ascii_lowercase();
            return host_lc.ends_with(&format!(".{apex_lc}"));
        }
        pattern.eq_ignore_ascii_case(host)
    })
}

#[cfg(test)]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    domain_matches(&[pattern.to_string()], host)
}

fn path_matches(patterns: &[String], path: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|pattern| wildcard_match(pattern, path))
}

fn port_matches(patterns: &[u16], port: Option<u16>) -> bool {
    patterns.is_empty() || port.is_some_and(|port| patterns.contains(&port))
}

fn connect_port_matches(patterns: &[u16], port: u16, deny_rule: bool) -> bool {
    if patterns.is_empty() {
        return deny_rule || port == 443;
    }
    patterns.contains(&port)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let mut parts = pattern.split('*').peekable();
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let mut idx = 0usize;

    if !starts_with_wildcard {
        let Some(first) = parts.next() else {
            return true;
        };
        if !text[idx..].starts_with(first) {
            return false;
        }
        idx += first.len();
    }

    let remaining: Vec<&str> = parts.collect();
    let last_idx = remaining.len().saturating_sub(1);
    for (i, part) in remaining.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == last_idx && !ends_with_wildcard {
            return text[idx..].ends_with(part);
        }
        if let Some(found) = text[idx..].find(part) {
            idx += found + part.len();
        } else {
            return false;
        }
    }
    true
}

pub fn parse_network_allowlist_rule(raw: &str) -> Result<NetworkRule> {
    let mut methods = Vec::new();
    let mut domains = Vec::new();
    let mut paths = Vec::new();
    let mut ports = Vec::new();

    let trimmed = raw.trim();
    anyhow::ensure!(!trimmed.is_empty(), "network rule entry must not be empty");
    for token in trimmed.split_whitespace() {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid token '{token}' in network entry '{raw}'"))?;
        let values: Vec<String> = value
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .collect();
        anyhow::ensure!(
            !values.is_empty(),
            "network token '{key}' has no values in '{raw}'"
        );
        match key {
            "method" => {
                anyhow::ensure!(methods.is_empty(), "duplicate method key in '{raw}'");
                methods = values.into_iter().map(|v| v.to_ascii_uppercase()).collect();
            }
            "domain" => {
                anyhow::ensure!(domains.is_empty(), "duplicate domain key in '{raw}'");
                domains = values;
            }
            "path" => {
                anyhow::ensure!(paths.is_empty(), "duplicate path key in '{raw}'");
                paths = values;
            }
            "port" => {
                anyhow::ensure!(ports.is_empty(), "duplicate port key in '{raw}'");
                ports = values
                    .into_iter()
                    .map(|value| {
                        value.parse::<u16>().with_context(|| {
                            format!("invalid port '{value}' in network entry '{raw}'")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            _ => anyhow::bail!("unknown key '{key}' in network entry '{raw}'"),
        }
    }
    anyhow::ensure!(
        !domains.is_empty(),
        "network entry '{raw}' is missing required 'domain=' key"
    );
    Ok(NetworkRule {
        methods,
        domains,
        paths,
        ports,
    })
}

// ── Loading / saving ─────────────────────────────────────────────────────────

/// Load a `harness-rules.toml` file.  Returns a default (empty) rule set if the
/// file does not exist, rather than an error.
pub fn load(path: &Path) -> Result<ProjectRules> {
    if !path.exists() {
        return Ok(ProjectRules::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading harness-rules.toml: {}", path.display()))?;
    let parsed: ProjectRules = toml::from_str(&raw)
        .with_context(|| format!("parsing harness-rules.toml: {}", path.display()))?;
    for entry in &parsed.network.allowlist {
        parse_network_allowlist_rule(entry).with_context(|| {
            format!(
                "invalid [network].allowlist entry '{}' in {}",
                entry,
                path.display()
            )
        })?;
    }
    for entry in &parsed.network.denylist {
        parse_network_allowlist_rule(entry).with_context(|| {
            format!(
                "invalid [network].denylist entry '{}' in {}",
                entry,
                path.display()
            )
        })?;
    }
    Ok(parsed)
}

/// Append an auto-approved command to the rules file at `path`.
///
/// If the argv already exists in the file, the file is left unchanged. The
/// parent directory is created if needed.
#[cfg(test)]
pub fn append_auto_approval(path: &Path, argv: &[String], cwd: &str) -> Result<()> {
    append_command_rule(path, argv, cwd)
}

#[cfg(test)]
fn append_command_rule(path: &Path, argv: &[String], cwd: &str) -> Result<()> {
    let mut rules = load(path)?;
    if rules
        .hostdo
        .commands
        .iter()
        .any(|c| c.argv == argv && c.image.is_none())
    {
        return Ok(());
    }
    rules.hostdo.commands.push(RuleCommand {
        name: None,
        argv: argv.to_vec(),
        image: None,
        cwd: cwd.to_string(),
        env_profile: None,
        timeout_secs: default_timeout(),
        concurrency: ConcurrencyPolicy::default(),
        approval_mode: ApprovalMode::Auto,
    });
    write_rules_file(path, &rules)
}

/// Write rules to a file with the standard comment header.
pub fn write_rules_file(path: &Path, rules: &ProjectRules) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory for {}", path.display()))?;
    }
    let content = render_rules_file(rules)?;
    std::fs::write(path, &content).with_context(|| format!("writing {}", path.display()))
}

/// Render rules file contents exactly as `write_rules_file` would serialize it.
pub fn render_rules_file(rules: &ProjectRules) -> Result<String> {
    let toml_str = toml::to_string_pretty(rules).context("serializing rules to TOML")?;
    Ok(format!("{RULES_FILE_HEADER}{toml_str}"))
}

const RULES_FILE_HEADER: &str = "\
# harness-rules.toml — policy for what the AI agent can do in this project.
# Commit this file to your repository. harness-hat reads it but never pushes
# changes back during workspace sync.
#
# Agents/LLMs are not permitted to edit this file directly. Harness Hat monitors
# this policy file and will notify the user if an agent attempts to modify it.
#
# Preferred place for *human/LLM instructions*:
# llm_instructions = \"\"\"\n\
# Environment:
# - You are operating inside a Linux Docker container managed by harness-hat.
# - Your workspace is mounted into the container; edits usually persist to the host.
# - Do not assume host tools, credentials, or services are directly available in the container.
#
# Host-side commands:
# - Use `hostdo ...` when you need host-side build/package tooling such as cargo,
#   npm, pnpm, yarn, go, make, pytest, or similar commands.
# - Examples: `hostdo cargo test`, `hostdo npm install`, `hostdo go test ./...`.
# - Only use `hostdo --image <docker-image> ...` when the user explicitly asks
#   you to run against a Docker image or containerized runner.
# - `hostdo --image` runs a command in a short-lived Docker runner instead of
#   directly on the host.
# - Examples: `hostdo --image node:20 npm test`,
#   `hostdo --image rust:1.88 cargo test`.
# - `hostdo` requests are policy checked against the `[hostdo]` rules below and may
#   prompt the developer.
#
# Subagents:
# - Use `agentctl spawn <agent> [--name <name>]` to start a same-workspace
#   subagent, where `<agent>` is `claude`, `codex`, `gemini`, or `opencode`.
# - Use `agentctl spawn-many <agent> <count> --prefix <name>` for larger
#   batches; launches are paced by `[agentctl].spawn_delay_ms` and never below
#   100ms between spawn requests.
# - Codex subagent launches additionally wait for the previous Codex launch to
#   fail MCP startup, clear MCP startup, sit waiting without MCP diagnostics for
#   a short stable window, or reach the 35s diagnostic timeout.
# - `[agentctl].max_subagents` caps live descendants under a single top-level
#   agent, including subagents, sub-subagents, and deeper descendants.
# - Use `agentctl status <child>`, `agentctl tail <child> --rows 30`,
#   `agentctl tail <child> --all`, `agentctl send <child> \"text\" --enter`,
#   `agentctl send <child> --key enter`, and `agentctl stop <child>` to inspect
#   and control direct child agents.
# - Subagent names are scoped to the parent that created them; duplicate names
#   may exist elsewhere in the tree.
#
# Container lifecycle:
# - Use `killme` only when the user explicitly asks you to end this container.
# \"\"\"
#
# ── Hostdo (host-side command execution) ─────────────────────────────────────
#
# default_policy: what happens when a command doesn't match any rule below.
#   auto   — run without prompting (use with caution)
#   prompt — ask the developer in the TUI (default)
#   deny   — reject silently
#
# Passthrough command (exact argv match, auto-approved):
#   [[hostdo.commands]]
#   argv = [\"cargo\", \"test\"] # run inside container with `hostdo cargo test`
#   cwd = \"$WORKSPACE\"         # execution cwd only, not part of approval matching
#   timeout_secs = 60
#   approval_mode = \"auto\"
#
# Short-lived Docker runner (exact argv + image match, auto-approved):
#   [[hostdo.commands]]
#   argv = [\"npm\", \"test\"]     # run with `hostdo --image node:20 npm test`
#   image = \"node:20\"
#   cwd = \"$WORKSPACE\"
#   timeout_secs = 60
#   approval_mode = \"auto\"
#
# Command alias (agent sends `hostdo tests`, expands server-side):
#   [hostdo.command_aliases]
#   tests = \"cargo test\" # run inside container with `hostdo test`
#   build = { cmd = \"cargo build --release\", cwd = \"$WORKSPACE\" }
#
# $WORKSPACE = workspace path on the host.
#
# ── Agentctl helper defaults ────────────────────────────────────────────────
#
# `agentctl spawn` and `spawn-many` read this value from the workspace rules file
# when `--delay-ms` is omitted. The effective delay is clamped to at least 100ms.
# `max_subagents` limits live descendants under a single top-level agent.
#   [agentctl]
#   spawn_delay_ms = 500
#   max_subagents = 10

# ── Network (HTTP/HTTPS proxy policy) ────────────────────────────────────────
#
# Coder-style network rules. Deny matches win over allow matches; if no rule
# matches, request is prompted.
# Rule format:
#   method=GET,POST domain=api.example.com path=/v1/*,/health port=443,8443
#
# Use [network].allowlist for permanent allows and [network].denylist for
# permanent denies.
#
# Domain matching:
# - `domain=example.com` exact only
# - `domain=*.example.com` subdomains only (not the apex)
#
# Path matching:
# - exact (`/v1/users`)
# - wildcard (`/v1/*`)
#
# Port matching:
# - omitted port matches normal HTTP/HTTPS requests on any port
# - CONNECT allow rules without `port=` only auto-approve port 443
# - raw CONNECT to other ports requires an explicit `port=...` allow rule

";
