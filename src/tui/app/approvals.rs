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
    pub(crate) fn approve_net(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
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
        let project_name = match self.unambiguous_pending_network_project(idx) {
            Some(name) => name,
            None => {
                self.log_ambiguous_network_project_context(idx, "allow");
                return;
            }
        };
        self.persist_network_rule_forever(idx, PersistKind::Allow, &project_name);
        self.approve_net(idx);
    }

    pub(crate) fn deny_net_forever(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let project_name = match self.unambiguous_pending_network_project(idx) {
            Some(name) => name,
            None => {
                self.log_ambiguous_network_project_context(idx, "deny");
                return;
            }
        };
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
    pub(crate) fn unambiguous_pending_network_project(&self, idx: usize) -> Option<String> {
        let item = self.pending_net.get(idx)?;
        if let Some(project) = item.source_project.clone() {
            return Some(project);
        }
        // No `source_project` attribution on the request. Accept the resolution
        // only if every running container that matches `source_container` maps
        // to the same workspace.
        let container_name = item.source_container.as_deref()?;
        let mut workspaces = self
            .sessions
            .iter()
            .filter(|s| !s.is_exited() && s.container_name == container_name)
            .map(|s| s.project.clone())
            .collect::<Vec<_>>();
        workspaces.sort();
        workspaces.dedup();
        if workspaces.len() == 1 {
            workspaces.into_iter().next()
        } else {
            None
        }
    }

    /// Resolve the source workspace for `pending_net[idx]`, allowing the
    /// sidebar selection as a last-resort fallback. Safe for display-only uses;
    /// **never** call this for persistence — use
    /// `unambiguous_pending_network_project` instead (CR7).
    #[allow(dead_code)]
    pub(crate) fn resolve_pending_network_project(&self, idx: usize) -> Option<String> {
        if let Some(unambiguous) = self.unambiguous_pending_network_project(idx) {
            return Some(unambiguous);
        }
        let cfg = self.config.get();
        self.selected_project_idx()
            .and_then(|pi| cfg.workspaces.get(pi))
            .map(|p| p.name.clone())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn persist_network_rule(
        &mut self,
        host: &str,
        policy: NetworkPolicy,
        project_name: Option<&str>,
    ) -> Result<Option<std::path::PathBuf>> {
        let entry = format!("domain={host}");
        self.persist_network_rule_entry(&entry, policy, project_name)
    }

    pub(crate) fn persist_network_rule_entry(
        &mut self,
        entry: &str,
        policy: NetworkPolicy,
        project_name: Option<&str>,
    ) -> Result<Option<std::path::PathBuf>> {
        let rules_path = match project_name {
            Some(name) => match self.project_rules_path(name) {
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
        self.note_rules_internal_write(rules_path.clone(), expected_content);
        crate::rules::write_rules_file(&rules_path, &rules)
            .with_context(|| format!("writing rules file '{}'", rules_path.display()))?;
        Ok(Some(rules_path))
    }

    pub(crate) fn log_ambiguous_network_project_context(&mut self, idx: usize, action: &str) {
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

    #[allow(dead_code)]
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

    pub(crate) fn project_rules_path(&self, project_name: &str) -> Option<std::path::PathBuf> {
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

/// Borrowing variant kept for call sites that still need `&mut` access to the
/// item afterwards. Avoid in new code — prefer the owning form, which skips
/// the dummy-channel allocation.
#[allow(dead_code)]
pub(crate) fn send_pending_network_decision(
    item: &mut crate::proxy::PendingNetworkItem,
    decision: NetworkDecision,
) {
    let tx = std::mem::replace(&mut item.response_tx, oneshot_dummy());
    let _ = tx.send(decision);
    for tx in item.merged_response_txs.drain(..) {
        let _ = tx.send(decision);
    }
}
