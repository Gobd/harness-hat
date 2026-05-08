use super::*;

const SUBAGENT_WAITING_AFTER: std::time::Duration = std::time::Duration::from_millis(1800);
const SUBAGENT_INITIAL_INPUT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const CODEX_MCP_DIAGNOSTIC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
const CODEX_MCP_LOG_DIAGNOSTIC_LIMIT: usize = 40;
const AGENT_CONTROL_DRAIN_LIMIT: usize = 4096;

impl App {
    pub(crate) fn drain_agent_control_requests(&mut self) {
        for _ in 0..AGENT_CONTROL_DRAIN_LIMIT {
            match self.agent_control_rx.try_recv() {
                Ok(request) => self.handle_agent_control_request(request),
                Err(_) => break,
            }
        }
    }

    fn handle_agent_control_request(&mut self, request: AgentControlRequest) {
        match request {
            AgentControlRequest::Spawn {
                parent_session_token,
                project,
                agent,
                name,
                codex_connectors_token,
                response_tx,
            } => {
                let result = self.spawn_subagent(
                    parent_session_token,
                    project,
                    agent,
                    name,
                    codex_connectors_token,
                );
                let _ = response_tx.send(result);
            }
            AgentControlRequest::Status {
                parent_session_token,
                child,
                include_log_diagnostics,
                response_tx,
            } => {
                let result =
                    self.subagent_status(&parent_session_token, &child, include_log_diagnostics);
                let _ = response_tx.send(result);
            }
            AgentControlRequest::Tail {
                parent_session_token,
                child,
                rows,
                response_tx,
            } => {
                let result = self.subagent_tail(&parent_session_token, &child, rows);
                let _ = response_tx.send(result);
            }
            AgentControlRequest::Send {
                parent_session_token,
                child,
                input,
                response_tx,
            } => {
                self.handle_subagent_send(parent_session_token, child, input, response_tx);
            }
            AgentControlRequest::SendMany {
                parent_session_token,
                items,
                response_tx,
            } => {
                let result = self.subagent_send_many(&parent_session_token, items);
                let _ = response_tx.send(result);
            }
            AgentControlRequest::Stop {
                parent_session_token,
                child,
                response_tx,
            } => {
                let result = self.subagent_stop(&parent_session_token, &child);
                let _ = response_tx.send(result);
            }
        }
    }

    fn spawn_subagent(
        &mut self,
        parent_session_token: String,
        project: String,
        agent: crate::config::AgentKind,
        name: Option<String>,
        codex_connectors_token: Option<String>,
    ) -> std::result::Result<crate::server::AgentSpawnResponse, String> {
        let parent = self
            .sessions
            .iter()
            .find(|session| session.session_token == parent_session_token)
            .ok_or_else(|| "parent session is no longer running".to_string())?;
        if parent.project != project {
            return Err("subagents must be spawned in the parent workspace".to_string());
        }
        let root_session_token = self
            .top_level_session_token_for(&parent_session_token)
            .ok_or_else(|| "top-level parent session is no longer running".to_string())?;

        let cfg = self.config.get();
        let rules = crate::config::load_composed_rules_for_workspace(&cfg, Some(project.as_str()))
            .map_err(|e| format!("failed to load workspace rules before spawning subagent: {e}"))?;
        let max_subagents = rules.agentctl.max_subagents;
        let spawn_delay =
            std::time::Duration::from_millis(rules.agentctl.effective_spawn_delay_ms());
        let live_descendants = self.live_descendant_count_for_root(&root_session_token);
        if live_descendants >= max_subagents {
            return Err(format!(
                "top-level agent already has {live_descendants} live descendant subagents; [agentctl].max_subagents is {max_subagents}"
            ));
        }
        let project_idx = cfg
            .workspaces
            .iter()
            .position(|workspace| workspace.name == project)
            .ok_or_else(|| "parent workspace is no longer configured".to_string())?;
        let container_idx = cfg
            .containers
            .iter()
            .position(|container| container.agent == agent)
            .ok_or_else(|| format!("no container profile is configured for {agent:?}"))?;
        let image = cfg.containers[container_idx].image.clone();
        drop(cfg);

        if !docker_image_exists(&image).unwrap_or(false) {
            return Err(format!(
                "container image '{image}' is not present; build or pull it before spawning a subagent"
            ));
        }

        let subagent_name = name
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty())
            .unwrap_or_else(|| self.default_subagent_name(&parent_session_token, &agent));
        if self.direct_child_name_exists(&parent_session_token, &subagent_name) {
            return Err(format!(
                "parent already has a direct subagent named '{subagent_name}'"
            ));
        }

        if let Some(last_spawn_at) = self.last_agent_spawn_at.get(&root_session_token).copied() {
            let elapsed = last_spawn_at.elapsed();
            if elapsed < spawn_delay {
                std::thread::sleep(spawn_delay - elapsed);
            }
        }
        self.last_agent_spawn_at
            .insert(root_session_token, std::time::Instant::now());

        let old_focus = self.focus.clone();
        let old_active_session = self.active_session;
        let old_preview_session = self.preview_session;
        let old_sidebar_idx = self.sidebar_idx;
        let old_sidebar_offset = self.sidebar_offset;
        let old_len = self.sessions.len();

        let extra_env = extra_subagent_env(&agent, codex_connectors_token)?;

        self.do_launch_container_on_project_with_priority_and_env(
            project_idx,
            container_idx,
            crate::proxy::SourcePriority::Subagent,
            &extra_env,
        );

        if self.sessions.len() == old_len {
            self.focus = old_focus;
            self.active_session = old_active_session;
            self.preview_session = old_preview_session;
            self.sidebar_idx = old_sidebar_idx;
            self.sidebar_offset = old_sidebar_offset;
            return Err("subagent launch failed".to_string());
        }

        let session = &mut self.sessions[old_len];
        session.parent_session_token = Some(parent_session_token);
        session.subagent_name = Some(subagent_name.clone());
        session.terminal_changed_at = std::time::Instant::now();
        session.terminal_snapshot_hash = 0;
        self.subagent_first_output_at.remove(&session.session_token);
        self.ready_subagent_tokens.remove(&session.session_token);

        let response = crate::server::AgentSpawnResponse {
            id: session.session_token.clone(),
            name: subagent_name,
            agent,
            container_id: session.container_id.clone(),
        };

        self.focus = old_focus;
        self.active_session = old_active_session;
        self.preview_session = old_preview_session;
        self.sidebar_idx = old_sidebar_idx;
        self.sidebar_offset = old_sidebar_offset;
        Ok(response)
    }

    fn default_subagent_name(
        &self,
        parent_session_token: &str,
        agent: &crate::config::AgentKind,
    ) -> String {
        let base = match agent {
            crate::config::AgentKind::Claude => "claude",
            crate::config::AgentKind::Codex => "codex",
            crate::config::AgentKind::Gemini => "gemini",
            crate::config::AgentKind::Opencode => "opencode",
            crate::config::AgentKind::None => "agent",
        };
        let mut count = self
            .direct_children(parent_session_token)
            .into_iter()
            .filter(|idx| self.sessions[*idx].agent_kind == *agent)
            .count()
            + 1;
        loop {
            let candidate = format!("{base}-{count}");
            if !self.direct_child_name_exists(parent_session_token, &candidate) {
                return candidate;
            }
            count += 1;
        }
    }

    fn subagent_status(
        &mut self,
        parent_session_token: &str,
        child: &str,
        include_log_diagnostics: bool,
    ) -> std::result::Result<crate::server::AgentStatusResponse, String> {
        let idx = self.resolve_child_session_idx(parent_session_token, child)?;
        self.sessions[idx].refresh_terminal_snapshot();
        self.sync_subagent_readiness(idx);
        let session = &self.sessions[idx];
        let state = if session.is_exited() {
            crate::server::AgentTerminalState::Exited
        } else if !self.subagent_has_reached_initial_wait(idx) {
            crate::server::AgentTerminalState::Launching
        } else if session.terminal_stable_for() >= SUBAGENT_WAITING_AFTER {
            crate::server::AgentTerminalState::Waiting
        } else {
            crate::server::AgentTerminalState::Working
        };
        let terminal_lines = session.terminal_bottom_lines(1000);
        let warnings = terminal_health_warnings(&terminal_lines);
        let mcp = agent_mcp_diagnostics(session, &state, &terminal_lines, include_log_diagnostics);
        Ok(crate::server::AgentStatusResponse {
            id: session.session_token.clone(),
            name: session.display_name().to_string(),
            agent: session.agent_kind.clone(),
            state,
            stable_for_ms: session.terminal_stable_for().as_millis(),
            rows: session.terminal_visible_rows(),
            warnings,
            mcp,
        })
    }

    fn subagent_tail(
        &mut self,
        parent_session_token: &str,
        child: &str,
        rows: usize,
    ) -> std::result::Result<crate::server::AgentTailResponse, String> {
        let idx = self.resolve_child_session_idx(parent_session_token, child)?;
        self.sessions[idx].refresh_terminal_snapshot();
        let available_rows = self.sessions[idx].terminal_total_rows();
        Ok(crate::server::AgentTailResponse {
            id: self.sessions[idx].session_token.clone(),
            available_rows,
            rows: self.sessions[idx].terminal_bottom_lines(rows),
        })
    }

    fn handle_subagent_send(
        &mut self,
        parent_session_token: String,
        child: String,
        input: String,
        response_tx: tokio::sync::oneshot::Sender<
            std::result::Result<crate::server::AgentOkResponse, String>,
        >,
    ) {
        let idx = match self.resolve_child_session_idx(&parent_session_token, &child) {
            Ok(idx) => idx,
            Err(err) => {
                let _ = response_tx.send(Err(err));
                return;
            }
        };
        if self.sessions[idx].is_exited() {
            let _ = response_tx.send(Err("subagent has exited".to_string()));
            return;
        }
        self.sessions[idx].refresh_terminal_snapshot();
        self.sync_subagent_readiness(idx);
        if !self.subagent_has_reached_initial_wait(idx) {
            self.pending_agent_sends.push_back(PendingAgentSend {
                session_token: self.sessions[idx].session_token.clone(),
                input,
                response_tx,
            });
            return;
        }
        self.sessions[idx].send_input(input.into_bytes());
        let _ = response_tx.send(Ok(crate::server::AgentOkResponse { ok: true }));
    }

    fn subagent_send_many(
        &mut self,
        parent_session_token: &str,
        items: Vec<crate::server::AgentSendManyItem>,
    ) -> std::result::Result<crate::server::AgentBatchResponse, String> {
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            let result = self.subagent_send_ready(parent_session_token, &item.child, item.input);
            match result {
                Ok(()) => results.push(crate::server::AgentBatchItemResponse {
                    child: item.child,
                    ok: true,
                    reason: None,
                }),
                Err(reason) => results.push(crate::server::AgentBatchItemResponse {
                    child: item.child,
                    ok: false,
                    reason: Some(reason),
                }),
            }
        }
        let ok = results.iter().all(|result| result.ok);
        Ok(crate::server::AgentBatchResponse { ok, results })
    }

    fn subagent_send_ready(
        &mut self,
        parent_session_token: &str,
        child: &str,
        input: String,
    ) -> std::result::Result<(), String> {
        let idx = self.resolve_child_session_idx(parent_session_token, child)?;
        if self.sessions[idx].is_exited() {
            return Err("subagent has exited".to_string());
        }
        self.sessions[idx].refresh_terminal_snapshot();
        self.sync_subagent_readiness(idx);
        if !self.subagent_has_reached_initial_wait(idx) {
            return Err("subagent is not ready for input".to_string());
        }
        self.sessions[idx].send_input(input.into_bytes());
        Ok(())
    }

    fn subagent_stop(
        &mut self,
        parent_session_token: &str,
        child: &str,
    ) -> std::result::Result<crate::server::AgentOkResponse, String> {
        let idx = self.resolve_child_session_idx(parent_session_token, child)?;
        self.close_session(idx);
        Ok(crate::server::AgentOkResponse { ok: true })
    }

    fn resolve_child_session_idx(
        &self,
        parent_session_token: &str,
        child: &str,
    ) -> std::result::Result<usize, String> {
        let child = child.trim();
        if child.is_empty() {
            return Err("subagent id must not be empty".to_string());
        }
        self.sessions
            .iter()
            .position(|session| {
                child_identity_matches(
                    parent_session_token,
                    child,
                    session.parent_session_token.as_deref(),
                    &session.session_token,
                    session.subagent_name.as_deref(),
                    &session.container_name,
                    &session.container_id,
                    &session.docker_name,
                )
            })
            .ok_or_else(|| "no subagent matched that id for this parent".to_string())
    }

    pub(crate) fn direct_children(&self, parent_session_token: &str) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, session)| {
                (session.parent_session_token.as_deref() == Some(parent_session_token))
                    .then_some(idx)
            })
            .collect()
    }

    fn top_level_session_token_for(&self, session_token: &str) -> Option<String> {
        let mut current = self
            .sessions
            .iter()
            .position(|session| session.session_token == session_token)?;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(current) {
            let Some(parent_token) = self.sessions[current].parent_session_token.as_deref() else {
                return Some(self.sessions[current].session_token.clone());
            };
            let Some(parent_idx) = self
                .sessions
                .iter()
                .position(|session| session.session_token == parent_token)
            else {
                return Some(self.sessions[current].session_token.clone());
            };
            current = parent_idx;
        }
        None
    }

    fn live_descendant_count_for_root(&self, root_session_token: &str) -> usize {
        self.sessions
            .iter()
            .filter(|session| {
                !session.is_exited()
                    && session.session_token != root_session_token
                    && self.session_descends_from(&session.session_token, root_session_token)
            })
            .count()
    }

    fn session_descends_from(&self, session_token: &str, root_session_token: &str) -> bool {
        let mut current = match self
            .sessions
            .iter()
            .position(|session| session.session_token == session_token)
        {
            Some(idx) => idx,
            None => return false,
        };
        let mut seen = std::collections::HashSet::new();
        while seen.insert(current) {
            let Some(parent_token) = self.sessions[current].parent_session_token.as_deref() else {
                return false;
            };
            if parent_token == root_session_token {
                return true;
            }
            let Some(parent_idx) = self
                .sessions
                .iter()
                .position(|session| session.session_token == parent_token)
            else {
                return false;
            };
            current = parent_idx;
        }
        false
    }

    fn direct_child_name_exists(&self, parent_session_token: &str, name: &str) -> bool {
        self.direct_children(parent_session_token)
            .into_iter()
            .any(|idx| self.sessions[idx].display_name() == name)
    }

    pub(crate) fn session_depth(&self, session_idx: usize) -> usize {
        let mut depth = 0usize;
        let mut current = session_idx;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(current) {
            let Some(parent_token) = self.sessions[current].parent_session_token.as_deref() else {
                break;
            };
            let Some(parent_idx) = self
                .sessions
                .iter()
                .position(|session| session.session_token == parent_token)
            else {
                break;
            };
            depth += 1;
            current = parent_idx;
        }
        depth
    }

    pub(crate) fn refresh_session_terminal_states(&mut self) {
        for idx in 0..self.sessions.len() {
            self.sessions[idx].refresh_terminal_snapshot();
            self.sync_subagent_readiness(idx);
        }
    }

    pub(crate) fn session_is_waiting(&self, session_idx: usize) -> bool {
        self.sessions.get(session_idx).is_some_and(|session| {
            !session.is_exited() && session.terminal_stable_for() >= SUBAGENT_WAITING_AFTER
        })
    }

    fn subagent_has_reached_initial_wait(&self, session_idx: usize) -> bool {
        self.sessions
            .get(session_idx)
            .is_some_and(|session| self.ready_subagent_tokens.contains(&session.session_token))
    }

    fn sync_subagent_readiness(&mut self, session_idx: usize) {
        let Some(session) = self.sessions.get(session_idx) else {
            return;
        };
        if !session.is_subagent() || session.is_exited() {
            return;
        }
        let session_token = session.session_token.clone();
        if !self.subagent_first_output_at.contains_key(&session_token)
            && subagent_has_visible_output(session)
        {
            self.subagent_first_output_at
                .insert(session_token.clone(), std::time::Instant::now());
        }
        if initial_input_ready_after_first_output(
            self.subagent_first_output_at.get(&session_token).copied(),
            std::time::Instant::now(),
        ) {
            self.ready_subagent_tokens
                .insert(session.session_token.clone());
        }
    }

    pub(crate) fn flush_pending_agent_sends(&mut self) {
        let mut i = 0;
        while i < self.pending_agent_sends.len() {
            let session_token = self.pending_agent_sends[i].session_token.clone();
            let Some(idx) = self
                .sessions
                .iter()
                .position(|session| session.session_token == session_token)
            else {
                if let Some(pending) = self.pending_agent_sends.remove(i) {
                    let _ = pending
                        .response_tx
                        .send(Err("subagent is no longer running".to_string()));
                }
                continue;
            };

            if self.sessions[idx].is_exited() {
                if let Some(pending) = self.pending_agent_sends.remove(i) {
                    let _ = pending
                        .response_tx
                        .send(Err("subagent has exited".to_string()));
                }
                continue;
            }

            self.sync_subagent_readiness(idx);
            if !self.subagent_has_reached_initial_wait(idx) {
                i += 1;
                continue;
            }

            if let Some(pending) = self.pending_agent_sends.remove(i) {
                self.sessions[idx].send_input(pending.input.into_bytes());
                let _ = pending
                    .response_tx
                    .send(Ok(crate::server::AgentOkResponse { ok: true }));
            }
        }
    }
}

fn child_identity_matches(
    parent_session_token: &str,
    child: &str,
    session_parent_token: Option<&str>,
    session_token: &str,
    subagent_name: Option<&str>,
    container_name: &str,
    container_id: &str,
    docker_name: &str,
) -> bool {
    session_parent_token == Some(parent_session_token)
        && (session_token == child
            || session_token.starts_with(child)
            || subagent_name == Some(child)
            || container_name == child
            || container_id == child
            || container_id.starts_with(child)
            || docker_name == child)
}

fn extra_subagent_env(
    agent: &crate::config::AgentKind,
    codex_connectors_token: Option<String>,
) -> std::result::Result<Vec<(String, String)>, String> {
    if *agent != crate::config::AgentKind::Codex {
        return Ok(Vec::new());
    }

    let token = codex_connectors_token
        .or_else(|| std::env::var("CODEX_CONNECTORS_TOKEN").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let Some(token) = token else {
        return Ok(Vec::new());
    };
    if token.contains('\n') || token.contains('\r') {
        return Err("CODEX_CONNECTORS_TOKEN must not contain newlines".to_string());
    }

    Ok(vec![("CODEX_CONNECTORS_TOKEN".to_string(), token)])
}

fn subagent_has_visible_output(session: &crate::container::ContainerSession) -> bool {
    let rows = session.terminal_visible_rows();
    session
        .terminal_bottom_lines(rows)
        .iter()
        .any(|line| !line.trim().is_empty())
}

fn initial_input_ready_after_first_output(
    first_output_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    first_output_at.is_some_and(|first_output_at| {
        now.duration_since(first_output_at) >= SUBAGENT_INITIAL_INPUT_DELAY
    })
}

fn agent_mcp_diagnostics(
    session: &crate::container::ContainerSession,
    state: &crate::server::AgentTerminalState,
    terminal_lines: &[String],
    include_log_diagnostics: bool,
) -> Option<crate::server::AgentMcpDiagnostics> {
    if session.agent_kind != crate::config::AgentKind::Codex {
        return None;
    }

    let mut diagnostics = diagnostic_lines("terminal", terminal_lines);
    if include_log_diagnostics {
        diagnostics.extend(codex_log_diagnostic_lines(session));
    }
    dedup_diagnostics(&mut diagnostics);

    let failed = diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.kind.as_str(),
            "mcp_startup_incomplete"
                | "mcp_startup_failed"
                | "mcp_timeout"
                | "tls_certificate"
                | "dns_lookup"
                | "cloudflare_challenge"
                | "unexpected_content_type"
                | "auth_required"
                | "auth_refresh_failed"
                | "network_resolution"
        )
    });
    let elapsed = session.launched_at.elapsed();
    let state = if failed {
        crate::server::AgentMcpStartupState::Failed
    } else if elapsed >= CODEX_MCP_DIAGNOSTIC_TIMEOUT
        && *state == crate::server::AgentTerminalState::Waiting
    {
        crate::server::AgentMcpStartupState::Clean
    } else if elapsed >= CODEX_MCP_DIAGNOSTIC_TIMEOUT {
        crate::server::AgentMcpStartupState::DiagnosticTimeout
    } else {
        crate::server::AgentMcpStartupState::Pending
    };

    Some(crate::server::AgentMcpDiagnostics {
        state,
        elapsed_ms: elapsed.as_millis(),
        diagnostic_timeout_ms: CODEX_MCP_DIAGNOSTIC_TIMEOUT.as_millis(),
        diagnostics,
    })
}

fn diagnostic_lines(source: &str, lines: &[String]) -> Vec<crate::server::AgentDiagnostic> {
    lines
        .iter()
        .filter_map(|line| diagnostic_for_line(source, line))
        .collect()
}

fn diagnostic_for_line(source: &str, line: &str) -> Option<crate::server::AgentDiagnostic> {
    let lower = line.to_ascii_lowercase();
    let kind = diagnostic_kind(&lower)?;
    let message = compact_diagnostic_message(line);
    if message.is_empty() {
        return None;
    }
    Some(crate::server::AgentDiagnostic {
        source: source.to_string(),
        kind: kind.to_string(),
        message,
    })
}

fn diagnostic_kind(lower: &str) -> Option<&'static str> {
    if lower.contains("mcp startup incomplete") {
        return Some("mcp_startup_incomplete");
    }
    if lower.contains("mcp client")
        && (lower.contains("failed to start") || lower.contains("failed:"))
    {
        return Some("mcp_startup_failed");
    }
    if (lower.contains("mcp client") || lower.contains("codex_apps"))
        && (lower.contains("request timed out") || lower.contains("timed out handshaking"))
    {
        return Some("mcp_timeout");
    }
    if lower.contains("invalid peer certificate")
        || lower.contains("unknownissuer")
        || lower.contains("tls certificate")
    {
        return Some("tls_certificate");
    }
    if lower.contains("failed to lookup address information")
        || lower.contains("nodename nor servname")
    {
        return Some("dns_lookup");
    }
    if lower.contains("cf-mitigated")
        || lower.contains("cloudflare")
        || lower.contains("enable javascript and cookies")
        || lower.contains("just a moment")
    {
        return Some("cloudflare_challenge");
    }
    if lower.contains("unexpectedcontenttype")
        || lower.contains("unexpected content type")
        || lower.contains("text/html")
    {
        return Some("unexpected_content_type");
    }
    if lower.contains("auth required") {
        return Some("auth_required");
    }
    if lower.contains("failed to refresh token")
        || lower.contains("refresh token expired")
        || lower.contains("refresh token reused")
        || lower.contains("refresh token invalidated")
    {
        return Some("auth_refresh_failed");
    }
    if lower.contains("resolving chatgpt.com") && lower.contains("failed") {
        return Some("network_resolution");
    }
    if lower.contains("wham/apps") {
        return Some("codex_apps");
    }
    if lower.contains("codex_apps") {
        return Some("codex_apps");
    }
    None
}

fn compact_diagnostic_message(line: &str) -> String {
    line.trim()
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .take(600)
        .collect()
}

fn dedup_diagnostics(diagnostics: &mut Vec<crate::server::AgentDiagnostic>) {
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.source.clone(),
            diagnostic.kind.clone(),
            diagnostic.message.clone(),
        ))
    });
}

fn codex_log_diagnostic_lines(
    session: &crate::container::ContainerSession,
) -> Vec<crate::server::AgentDiagnostic> {
    let target = if !session.container_id.is_empty() && session.container_id != "unknown" {
        session.container_id.as_str()
    } else {
        session.docker_name.as_str()
    };
    if target.is_empty() || target == "unknown" {
        return Vec::new();
    }

    let pattern = "codex_apps|MCP client|MCP startup|request timed out|timed out handshaking|UnexpectedContentType|unexpected content type|cf-mitigated|cloudflare|challenge|Auth required|refresh token|refresh_token|UnknownIssuer|certificate|failed to lookup|nodename|servname|wham/apps|text/html|Failed to refresh token";
    let script = format!(
        "for log in \"${{CODEX_HOME:-/home/ubuntu/.codex}}/log/codex-tui.log\" /home/ubuntu/.codex/log/codex-tui.log /root/.codex/log/codex-tui.log; do if [ -f \"$log\" ]; then grep -iE '{}' \"$log\" 2>/dev/null | tail -n {}; exit 0; fi; done",
        pattern, CODEX_MCP_LOG_DIAGNOSTIC_LIMIT
    );
    docker_exec_stdout_with_timeout(target, &script, std::time::Duration::from_secs(2))
        .map(|output| {
            output
                .lines()
                .filter_map(|line| diagnostic_for_line("codex_log", line))
                .collect()
        })
        .unwrap_or_default()
}

fn docker_exec_stdout_with_timeout(
    target: &str,
    script: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new("docker")
        .args(["exec", target, "sh", "-lc", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut output);
                }
                return Some(output);
            }
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
}

fn terminal_health_warnings(lines: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut push = |message: &str| {
        if !warnings.iter().any(|existing| existing == message) {
            warnings.push(message.to_string());
        }
    };

    for line in lines {
        let lower = line.to_ascii_lowercase();
        if lower.contains("mcp startup incomplete") {
            push("MCP startup incomplete");
        }
        if lower.contains("mcp client") && lower.contains("timed out") {
            push("MCP client startup timed out");
        }
        if lower.contains("invalid peer certificate") || lower.contains("unknownissuer") {
            push("TLS certificate verification failed");
        }
        if lower.contains("stream disconnected before completion") {
            push("model stream disconnected before completion");
        }
        if lower.contains("failed to lookup address information")
            || lower.contains("nodename nor servname")
        {
            push("DNS lookup failed");
        }
        if lower.contains("cf-mitigated")
            || lower.contains("cloudflare")
            || lower.contains("enable javascript and cookies")
        {
            push("Cloudflare challenge response");
        }
        if lower.contains("failed to refresh token")
            || lower.contains("refresh token expired")
            || lower.contains("refresh token reused")
            || lower.contains("refresh token invalidated")
        {
            push("Codex auth token refresh failed");
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::{
        SUBAGENT_INITIAL_INPUT_DELAY, SUBAGENT_WAITING_AFTER, child_identity_matches,
        diagnostic_for_line, extra_subagent_env, initial_input_ready_after_first_output,
        terminal_health_warnings,
    };

    #[test]
    fn child_identity_matches_only_within_parent_namespace() {
        assert!(child_identity_matches(
            "parent-a",
            "gemini-docs",
            Some("parent-a"),
            "child-token",
            Some("gemini-docs"),
            "gemini",
            "abcdef123456",
            "docker-gemini",
        ));
        assert!(!child_identity_matches(
            "parent-b",
            "gemini-docs",
            Some("parent-a"),
            "child-token",
            Some("gemini-docs"),
            "gemini",
            "abcdef123456",
            "docker-gemini",
        ));
    }

    #[test]
    fn initial_input_delay_is_shorter_than_ongoing_waiting_threshold() {
        assert!(SUBAGENT_INITIAL_INPUT_DELAY < SUBAGENT_WAITING_AFTER);
    }

    #[test]
    fn session_waiting_threshold_matches_subagent_status_threshold() {
        assert_eq!(
            SUBAGENT_WAITING_AFTER,
            std::time::Duration::from_millis(1800)
        );
    }

    #[test]
    fn initial_input_ready_requires_first_output_plus_delay() {
        let now = std::time::Instant::now();

        assert!(!initial_input_ready_after_first_output(None, now));
        assert!(!initial_input_ready_after_first_output(
            Some(now - SUBAGENT_INITIAL_INPUT_DELAY + std::time::Duration::from_millis(1)),
            now,
        ));
        assert!(initial_input_ready_after_first_output(
            Some(now - SUBAGENT_INITIAL_INPUT_DELAY),
            now,
        ));
    }

    #[test]
    fn terminal_health_warnings_reports_setup_and_network_failures() {
        let warnings = terminal_health_warnings(&[
            "MCP client for `codex_apps` timed out after 30 seconds.".to_string(),
            "MCP startup incomplete (failed: codex_apps)".to_string(),
            "Stream disconnected before completion: invalid peer certificate: UnknownIssuer"
                .to_string(),
            "Status  : resolving chatgpt.com:443: failed to lookup address information: nodename nor servname provided, or not known".to_string(),
        ]);

        assert_eq!(
            warnings,
            vec![
                "MCP client startup timed out",
                "MCP startup incomplete",
                "TLS certificate verification failed",
                "model stream disconnected before completion",
                "DNS lookup failed",
            ]
        );
    }

    #[test]
    fn codex_subagent_env_forwards_connector_token_only_for_codex() {
        let env = extra_subagent_env(
            &crate::config::AgentKind::Codex,
            Some(" connector-token ".to_string()),
        )
        .expect("valid token");

        assert_eq!(
            env,
            vec![(
                "CODEX_CONNECTORS_TOKEN".to_string(),
                "connector-token".to_string()
            )]
        );
        assert!(
            extra_subagent_env(
                &crate::config::AgentKind::Gemini,
                Some("connector-token".to_string())
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            extra_subagent_env(
                &crate::config::AgentKind::Codex,
                Some("bad\ntoken".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn diagnostic_lines_classify_codex_mcp_failures() {
        let cloudflare = diagnostic_for_line(
            "codex_log",
            "UnexpectedContentType text/html; cf-mitigated: challenge",
        )
        .expect("cloudflare diagnostic");
        assert_eq!(cloudflare.kind, "cloudflare_challenge");

        let auth = diagnostic_for_line("codex_log", "Failed to refresh token: 401")
            .expect("auth diagnostic");
        assert_eq!(auth.kind, "auth_refresh_failed");

        let info = diagnostic_for_line("codex_log", "POST /backend-api/wham/apps")
            .expect("codex_apps diagnostic");
        assert_eq!(info.kind, "codex_apps");
    }
}
