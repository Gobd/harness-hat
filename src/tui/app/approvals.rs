use super::*;

impl App {
    pub(crate) fn approve_net(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        send_pending_network_decision(&mut self.pending_net[idx], NetworkDecision::Allow);
        self.pending_net.remove(idx);
    }

    pub(crate) fn deny_net(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        send_pending_network_decision(&mut self.pending_net[idx], NetworkDecision::Deny);
        self.pending_net.remove(idx);
    }

    pub(crate) fn approve_net_forever(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let entry = pending_network_rule_entry(&self.pending_net[idx], NetworkPolicy::Auto);
        let project_name = self.pending_net[idx].source_project.clone();
        if project_name.is_none() {
            self.log_missing_network_project_context(idx, "allow");
        }
        match self.persist_network_rule_entry(&entry, NetworkPolicy::Auto, project_name.as_deref())
        {
            Ok(updated_path) => {
                if let Some(path) = &updated_path {
                    self.push_log(
                        format!(
                            "added permanent allow rule for '{}' in {}",
                            entry,
                            path.display()
                        ),
                        false,
                    );
                } else {
                    self.push_log(
                        format!("network rule '{}' already permanently allowed", entry),
                        false,
                    );
                }
            }
            Err(e) => {
                self.push_log(
                    format!(
                        "failed to persist permanent allow rule for '{}': {}",
                        entry, e
                    ),
                    true,
                );
            }
        }
        self.approve_net(idx);
    }

    pub(crate) fn deny_net_forever(&mut self, idx: usize) {
        if idx >= self.pending_net.len() {
            return;
        }
        let entry = pending_network_rule_entry(&self.pending_net[idx], NetworkPolicy::Deny);
        let project_name = self.resolve_pending_network_project(idx);
        match self.persist_network_rule_entry(&entry, NetworkPolicy::Deny, project_name.as_deref())
        {
            Ok(updated_path) => {
                if let Some(path) = &updated_path {
                    self.push_log(
                        format!(
                            "added permanent deny rule for '{}' in {}",
                            entry,
                            path.display()
                        ),
                        false,
                    );
                } else {
                    self.push_log(
                        format!("network rule '{}' already permanently denied", entry),
                        false,
                    );
                }
            }
            Err(e) => {
                self.push_log(
                    format!(
                        "failed to persist permanent deny rule for '{}': {}",
                        entry, e
                    ),
                    true,
                );
            }
        }
        self.deny_net(idx);
    }

    pub(crate) fn resolve_pending_network_project(&self, idx: usize) -> Option<String> {
        let item = self.pending_net.get(idx)?;
        if let Some(project) = item.source_project.clone() {
            return Some(project);
        }
        if let Some(container_name) = item.source_container.as_deref() {
            let mut workspaces = self
                .sessions
                .iter()
                .filter(|s| !s.is_exited() && s.container_name == container_name)
                .map(|s| s.project.clone())
                .collect::<Vec<_>>();
            workspaces.sort();
            workspaces.dedup();
            if workspaces.len() == 1 {
                return workspaces.into_iter().next();
            }
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
            NetworkPolicy::Prompt => return Ok(None),
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

    pub(crate) fn log_missing_network_project_context(&mut self, idx: usize, action: &str) {
        if idx >= self.pending_net.len() {
            return;
        }
        let host = self.pending_net[idx].host.clone();
        self.push_log(
            format!("cannot persist permanent {action} rule for '{}' because the network request had no source workspace metadata", host),
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
