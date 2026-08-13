use super::*;

/// Type-safe selector for `persist_network_rule_forever`. Constraining the
/// helper to exactly two variants — instead of accepting `NetworkPolicy` —
/// removes the silent "any non-`Deny` policy is allow" hazard flagged in M22.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistKind {
    Allow,
    Deny,
}

impl App {
    pub(crate) fn approve_exec(&mut self, idx: usize, remember: bool) {
        if idx >= self.pending_exec.len() {
            return;
        }
        let workspace_name = self.pending_exec[idx].workspace_name.clone();
        let reason = self.pending_exec[idx].reason.clone();
        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(Some(&workspace_name))
        {
            self.push_log(
                format!("blocked host command approval until rules are reviewed: {error}"),
                true,
            );
            self.deny_exec(idx);
            return;
        }
        let remembered = if remember {
            let item = &self.pending_exec[idx];
            let image = item.image.clone();
            let timeout_secs = item.timeout_secs;
            let argv = item.argv.clone();
            let workspace_name = item.workspace_name.clone();
            let rule_cwd = item.rule_cwd.clone();
            if let Some(rules_path) = self.workspace_rules_path(&workspace_name) {
                let cwd = self.portable_cwd(&rule_cwd, &workspace_name);
                match self.persist_exec_rule(
                    &rules_path,
                    &argv,
                    image.as_deref(),
                    timeout_secs,
                    &cwd,
                    reason.as_deref(),
                    NetworkPolicy::Auto,
                ) {
                    Ok(()) => {
                        self.push_log(
                            format!(
                                "saved hostdo allow rule in {}: {}",
                                rules_path.display(),
                                argv.join(" ")
                            ),
                            false,
                        );
                        true
                    }
                    Err(e) => {
                        self.push_log(format!("failed to save hostdo rule: {e}"), true);
                        false
                    }
                }
            } else {
                self.push_log(
                    format!(
                        "failed to save hostdo rule: workspace {workspace_name:?} is no longer configured"
                    ),
                    true,
                );
                false
            }
        } else {
            false
        };
        if let Some(tx) = self.pending_exec[idx].response_tx.take() {
            let _ = tx.send(crate::server::ApprovalDecision::Approve {
                remember: remembered,
            });
        }
        self.pending_exec.remove(idx);
    }

    pub(crate) fn deny_exec(&mut self, idx: usize) {
        if idx >= self.pending_exec.len() {
            return;
        }
        if let Some(tx) = self.pending_exec[idx].response_tx.take() {
            let _ = tx.send(crate::server::ApprovalDecision::Deny);
        }
        self.pending_exec.remove(idx);
    }

    pub(crate) fn deny_exec_forever(&mut self, idx: usize) {
        if idx >= self.pending_exec.len() {
            return;
        }
        let item = &self.pending_exec[idx];
        let image = item.image.clone();
        let timeout_secs = item.timeout_secs;
        let argv = item.argv.clone();
        let reason = item.reason.clone();
        let project = item.workspace_name.clone();
        let rule_cwd = item.rule_cwd.clone();
        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(Some(&project))
        {
            self.push_log(
                format!("blocked rules update until reviewed: {error}"),
                true,
            );
            self.deny_exec(idx);
            return;
        }
        if let Some(rules_path) = self.workspace_rules_path(&project) {
            let cwd = self.portable_cwd(&rule_cwd, &project);
            match self.persist_exec_rule(
                &rules_path,
                &argv,
                image.as_deref(),
                timeout_secs,
                &cwd,
                reason.as_deref(),
                NetworkPolicy::Deny,
            ) {
                Ok(()) => self.push_log(
                    format!(
                        "saved hostdo deny rule in {}: {}",
                        rules_path.display(),
                        argv.join(" ")
                    ),
                    false,
                ),
                Err(e) => self.push_log(format!("failed to save hostdo deny rule: {e}"), true),
            }
        }
        self.deny_exec(idx);
    }

    pub(crate) fn approve_net(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let workspace_name = self.pending_net[idx].source_workspace.clone();
        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(workspace_name.as_deref())
        {
            self.push_log(
                format!("blocked network approval until rules are reviewed: {error}"),
                true,
            );
            self.deny_net(idx);
            return;
        }
        let item = self.pending_net.remove(idx);
        send_pending_network_decision_owned(item, NetworkDecision::Allow);
    }

    pub(crate) fn deny_net(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let item = self.pending_net.remove(idx);
        send_pending_network_decision_owned(item, NetworkDecision::Deny);
    }

    pub(crate) fn approve_net_forever(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        // Persisting "allow forever" MUST be attributed to an unambiguous source
        // workspace, otherwise we'd weaken whichever workspace the user happens
        // to be looking at. Per-request (one-shot) decisions are still safe
        // because they never touch on-disk rules.
        let project_name = match self.unambiguous_pending_network_workspace(idx) {
            Some(name) => name,
            None => {
                self.log_ambiguous_network_workspace_context(idx, "allow");
                return;
            }
        };
        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(Some(&project_name))
        {
            self.push_log(
                format!("blocked network approval until rules are reviewed: {error}"),
                true,
            );
            self.deny_net(idx);
            return;
        }
        self.persist_network_rule_forever(idx, PersistKind::Allow, &project_name);
        self.approve_net(idx);
    }

    pub(crate) fn deny_net_forever(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let project_name = match self.unambiguous_pending_network_workspace(idx) {
            Some(name) => name,
            None => {
                self.log_ambiguous_network_workspace_context(idx, "deny");
                return;
            }
        };
        if let Err(error) = self
            .config
            .ensure_rules_trusted_for_workspace(Some(&project_name))
        {
            self.push_log(
                format!("blocked rules update until reviewed: {error}"),
                true,
            );
            self.deny_net(idx);
            return;
        }
        self.persist_network_rule_forever(idx, PersistKind::Deny, &project_name);
        self.deny_net(idx);
    }

    /// Persist a permanent allow/deny rule for `pending_net[idx]` and log the
    /// outcome. Shared by `approve_net_forever`/`deny_net_forever`, which differ
    /// only in how they resolve `project_name` and the final approve/deny call.
    /// `kind` (a strict two-variant enum, not the open-ended `NetworkPolicy`)
    /// keeps the call sites type-safe — there is no "what if it's `Prompt`?"
    /// branch to get wrong.
    fn persist_network_rule_forever(&mut self, idx: usize, kind: PersistKind, project_name: &str) {
        let (verb, past, policy) = match kind {
            PersistKind::Allow => ("allow", "allowed", NetworkPolicy::Auto),
            PersistKind::Deny => ("deny", "denied", NetworkPolicy::Deny),
        };
        let entry = pending_network_rule_entry(&self.pending_net[idx], policy);
        match self.persist_network_rule_entry(&entry, policy, Some(project_name)) {
            Ok(Some(path)) => self.push_log(
                format!(
                    "added permanent {verb} rule for '{entry}' in {}",
                    path.display()
                ),
                false,
            ),
            Ok(None) => self.push_log(
                format!("network rule '{entry}' already permanently {past}"),
                false,
            ),
            Err(e) => self.push_log(
                format!("failed to persist permanent {verb} rule for '{entry}': {e}"),
                true,
            ),
        }
    }

    /// Resolve the source workspace for `pending_net[idx]` **only if it can be
    /// determined unambiguously** from the request itself. Used by the "forever"
    /// approve/deny paths, which write to disk and therefore must never fall
    /// back to whichever workspace the user happens to have selected in the
    /// sidebar (see CR7).
    pub(crate) fn unambiguous_pending_network_workspace(&self, idx: usize) -> Option<String> {
        let item = self.pending_net.get(idx)?;
        if let Some(project) = item.source_workspace.clone() {
            return Some(project);
        }
        // No `source_workspace` attribution on the request. Accept the resolution
        // only if every running container that matches `source_container` maps
        // to the same workspace.
        let container_name = item.source_container.as_deref()?;
        let mut workspaces = self
            .sessions
            .iter()
            .filter(|s| !s.is_exited() && s.container_name == container_name)
            .map(|s| s.workspace_name.clone())
            .collect::<Vec<_>>();
        workspaces.sort();
        workspaces.dedup();
        if workspaces.len() == 1 {
            workspaces.into_iter().next()
        } else {
            None
        }
    }

    pub(crate) fn persist_network_rule_entry(
        &mut self,
        entry: &str,
        policy: NetworkPolicy,
        project_name: Option<&str>,
    ) -> Result<Option<std::path::PathBuf>> {
        let rules_path = match project_name {
            Some(name) => match self.workspace_rules_path(name) {
                Some(path) => path,
                None => anyhow::bail!(
                    "cannot persist network rule: workspace '{}' not found",
                    name
                ),
            },
            None => anyhow::bail!(
                "cannot persist network rule: unknown workspace (request lacked workspace attribution)"
            ),
        };
        let mut rules = crate::rules::load(&rules_path)
            .with_context(|| format!("loading rules file '{}'", rules_path.display()))?;

        let entry = entry.trim().to_string();
        let mut changed = false;
        let entries = match policy {
            NetworkPolicy::Auto => {
                let original_len = rules.network.denylist.len();
                rules
                    .network
                    .denylist
                    .retain(|raw| !raw.trim().eq_ignore_ascii_case(&entry));
                changed |= rules.network.denylist.len() != original_len;
                &mut rules.network.allowlist
            }
            NetworkPolicy::Deny => {
                let original_len = rules.network.allowlist.len();
                rules
                    .network
                    .allowlist
                    .retain(|raw| !raw.trim().eq_ignore_ascii_case(&entry));
                changed |= rules.network.allowlist.len() != original_len;
                &mut rules.network.denylist
            }
            NetworkPolicy::Prompt => {
                // `Prompt` means "ask the user every time", so there is nothing
                // to persist. Reaching here from the "forever" code paths would
                // be a programmer error — those funnel through `PersistKind`
                // (M22). Kept as an explicit, exhaustive arm rather than `_ =>`
                // so future `NetworkPolicy` variants force a compile error.
                debug_assert!(
                    false,
                    "persist_network_rule_entry called with NetworkPolicy::Prompt"
                );
                return Ok(None);
            }
        };

        let exists = entries
            .iter()
            .any(|raw| raw.trim().eq_ignore_ascii_case(&entry));
        if !exists {
            entries.push(entry);
            changed = true;
        }
        if !changed {
            return Ok(None);
        }

        let expected_content = crate::rules::render_rules_file(&rules)
            .with_context(|| format!("rendering rules file '{}'", rules_path.display()))?;
        self.note_rules_internal_write(rules_path.clone(), expected_content.clone());
        crate::rules::write_rules_file(&rules_path, &rules)
            .with_context(|| format!("writing rules file '{}'", rules_path.display()))?;
        self.config
            .trust_rules_file_if_contents(&rules_path, &expected_content)
            .with_context(|| {
                format!(
                    "verifying rules file '{}' after writing",
                    rules_path.display()
                )
            })?;
        Ok(Some(rules_path))
    }

    pub(crate) fn persist_exec_rule(
        &mut self,
        rules_path: &std::path::Path,
        argv: &[String],
        image: Option<&str>,
        timeout_secs: u64,
        cwd: &str,
        reason: Option<&str>,
        approval_mode: NetworkPolicy,
    ) -> Result<()> {
        let timeout_secs = crate::rules::clamp_hostdo_timeout_secs(timeout_secs);
        let mut rules = crate::rules::load(rules_path)
            .with_context(|| format!("loading rules file '{}'", rules_path.display()))?;
        let mut changed = false;
        if let Some(existing) = rules
            .hostdo
            .commands
            .iter_mut()
            .find(|entry| entry.argv == argv && entry.image.as_deref() == image)
        {
            if existing.approval_mode != approval_mode {
                existing.approval_mode = approval_mode;
                changed = true;
            }
            if existing.cwd.as_deref() != Some(cwd) {
                existing.cwd = Some(cwd.to_string());
                changed = true;
            }
            if existing.timeout_secs != timeout_secs {
                existing.timeout_secs = timeout_secs;
                changed = true;
            }
            if reason.is_some_and(|reason| existing.reason.as_deref() != Some(reason)) {
                existing.reason = reason.map(str::to_string);
                changed = true;
            }
        } else {
            rules.hostdo.commands.push(crate::rules::HostdoCommand {
                name: None,
                argv: argv.to_vec(),
                image: image.map(str::to_string),
                cwd: Some(cwd.to_string()),
                timeout_secs,
                approval_mode,
                reason: reason.map(str::to_string),
                env_allowlist: None,
            });
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        let expected_content = crate::rules::render_rules_file(&rules)
            .with_context(|| format!("rendering rules file '{}'", rules_path.display()))?;
        self.note_rules_internal_write(rules_path.to_path_buf(), expected_content.clone());
        crate::rules::write_rules_file(rules_path, &rules)
            .with_context(|| format!("writing rules file '{}'", rules_path.display()))?;
        self.config
            .trust_rules_file_if_contents(rules_path, &expected_content)
            .with_context(|| {
                format!(
                    "verifying rules file '{}' after writing",
                    rules_path.display()
                )
            })?;
        Ok(())
    }

    pub(crate) fn log_ambiguous_network_workspace_context(&mut self, idx: usize, action: &str) {
        if idx >= self.pending_net.len() {
            return;
        }
        let host = self.pending_net[idx].host.clone();
        self.push_log(
            format!(
                "refusing to persist permanent {action} rule for '{}': source workspace is unknown or ambiguous (decide per-request instead)",
                host
            ),
            true,
        );
    }

    pub(crate) fn portable_cwd(&self, cwd: &Path, project_name: &str) -> String {
        let cfg = self.config.get();
        let project = cfg.workspaces.iter().find(|p| p.name == project_name);
        let mount_target = "/workspace";
        let cwd_str = cwd.display().to_string();
        if cwd_str == mount_target {
            "$WORKSPACE".to_string()
        } else if let Some(rest) = cwd_str.strip_prefix(&format!("{}/", mount_target)) {
            format!("$WORKSPACE/{rest}")
        } else if let Some(project) = project {
            let root = project.canonical_path.display().to_string();
            if cwd_str == root {
                "$WORKSPACE".to_string()
            } else if let Some(rest) = cwd_str.strip_prefix(&format!("{}/", root)) {
                format!("$WORKSPACE/{rest}")
            } else {
                cwd_str
            }
        } else {
            cwd_str
        }
    }

    pub(crate) fn workspace_rules_path(&self, project_name: &str) -> Option<std::path::PathBuf> {
        let cfg = self.config.get();
        cfg.workspaces
            .iter()
            .find(|p| p.name == project_name)
            .map(|p| p.canonical_path.join("harness-rules.toml"))
    }
}

fn pending_network_rule_entry(
    item: &crate::proxy::PendingNetworkItem,
    policy: NetworkPolicy,
) -> String {
    if matches!(policy, NetworkPolicy::Auto)
        && item.method.eq_ignore_ascii_case("CONNECT")
        && let Some(port) = item.port
        && port != 443
    {
        return format!("domain={} port={port}", item.host);
    }
    format!("domain={}", item.host)
}

pub(crate) fn pending_network_request_count(item: &crate::proxy::PendingNetworkItem) -> usize {
    1 + item.merged_response_txs.len()
}

/// Owning variant: consumes the item and forwards the decision to every
/// merged response channel. Preferred over the borrowing form below because it
/// moves the sender out instead of swapping in a freshly allocated dummy
/// channel on each call (L10).
pub(crate) fn send_pending_network_decision_owned(
    item: crate::proxy::PendingNetworkItem,
    decision: NetworkDecision,
) {
    let _ = item.response_tx.send(decision);
    for tx in item.merged_response_txs {
        let _ = tx.send(decision);
    }
}
