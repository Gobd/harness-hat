/// Parses `harness-rules.toml` files and composes global + per-workspace
/// network policy.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

pub const CURRENT_RULES_VERSION: u32 = 1;

// ── Enums (re-used across config and rules) ──────────────────────────────────

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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectRules {
    /// Schema version for future harness-rules.toml migrations.
    #[serde(default = "current_rules_version")]
    pub version: u32,
    /// Optional project instructions. This field is preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_instructions: Option<String>,
    #[serde(default)]
    pub network: NetworkRules,
}

fn current_rules_version() -> u32 {
    CURRENT_RULES_VERSION
}

impl Default for ProjectRules {
    fn default() -> Self {
        Self {
            version: CURRENT_RULES_VERSION,
            llm_instructions: None,
            network: NetworkRules::default(),
        }
    }
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

/// One parsed network rule:
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
    pub network_rules: Vec<NetworkRule>,
    pub network_deny_rules: Vec<NetworkRule>,
    pub network_default: NetworkPolicy,
}

impl ComposedRules {
    /// Compose global rules + one or more project rule sets.
    /// Global rules come first (higher priority in declaration order).
    pub fn compose(global: &ProjectRules, projects: &[ProjectRules]) -> Self {
        let mut network_allowlist = global.network.allowlist.clone();
        let mut network_denylist = global.network.denylist.clone();

        for proj in projects {
            network_allowlist.extend(proj.network.allowlist.clone());
            network_denylist.extend(proj.network.denylist.clone());
        }

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
    validate_rules_version(parsed.version, path)?;
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

fn validate_rules_version(version: u32, path: &Path) -> Result<()> {
    anyhow::ensure!(
        version > 0,
        "harness-rules.toml {}: version must be greater than zero",
        path.display()
    );
    anyhow::ensure!(
        version <= CURRENT_RULES_VERSION,
        "harness-rules.toml {}: unsupported version {}; this build supports up to {}",
        path.display(),
        version,
        CURRENT_RULES_VERSION
    );
    Ok(())
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
# harness-rules.toml - network policy for Harness Hat container sessions.
# Commit this file to your repository. Harness Hat reads it but never pushes
# changes back during workspace sync.
#
# Preferred place for human/LLM instructions:
# llm_instructions = \"\"\"\n\
# Environment:
# - You are operating inside a Linux Docker container managed by Harness Hat.
# - Your workspace is mounted into the container; edits usually persist to the host.
# - Do not assume host tools, credentials, or services are directly available in the container.
# - Use `killme` only when the user explicitly asks you to end this container.
# \"\"\"
#
# Network (HTTP/HTTPS proxy policy)
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
