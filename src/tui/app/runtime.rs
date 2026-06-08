use super::*;

impl App {
    const MAX_PENDING_NETWORK_APPROVALS: usize = 64;

    pub(crate) fn drain_channels(&mut self) {
        let activity_selected_before = {
            let items = self.sidebar_items();
            self.selected_sidebar_item_from(&items)
        };
        for _ in 0..32 {
            match self.stop_pending_rx.try_recv() {
                Ok(item) => self.pending_stop.push(item),
                Err(_) => break,
            }
        }
        for _ in 0..32 {
            match self.net_pending_rx.try_recv() {
                Ok(item) => self.enqueue_pending_network(item),
                Err(_) => break,
            }
        }
        self.refresh_session_terminal_states();
        for _ in 0..64 {
            match self.container_usage_rx.try_recv() {
                Ok(update) => {
                    self.container_usage.insert(
                        update.docker_name,
                        CachedContainerUsage {
                            fetched_at: std::time::Instant::now(),
                            stats: update.stats,
                            in_flight: false,
                        },
                    );
                }
                Err(_) => break,
            }
        }
        for _ in 0..8 {
            match self.rules_scan_rx.try_recv() {
                Ok(stamps) => self.apply_rules_scan(stamps),
                Err(_) => break,
            }
        }
        for _ in 0..128 {
            match self.activity_rx.try_recv() {
                Ok(event) => self.apply_activity_event(event),
                Err(_) => break,
            }
        }
        let items = self.sidebar_items();
        self.restore_sidebar_selection(activity_selected_before.as_ref(), &items);
        for _ in 0..32 {
            match self.audit_rx.try_recv() {
                Ok(entry) => {
                    self.log.push_front(LogEntry::Audit(entry));
                    if self.log.len() > 500 {
                        self.log.pop_back();
                    }
                }
                Err(_) => break,
            }
        }
        for _ in 0..256 {
            match self.build_event_rx.try_recv() {
                Ok(BuildEvent::Output { line, is_error }) => {
                    self.push_build_output(line, is_error);
                }
                Ok(BuildEvent::Finished {
                    label,
                    launch_project_idx,
                    launch_container_idx,
                    launch_session_group,
                    success,
                    cancelled,
                    exit_code,
                    error,
                    diagnostic,
                }) => {
                    self.build_task = None;
                    if let Some(error) = error {
                        self.build_project_idx = None;
                        self.build_session_group = None;
                        self.push_log(format!("{label} failed: {error}"), true);
                        if let Some(diagnostic) = diagnostic {
                            self.push_log(format!("  build detail: {diagnostic}"), true);
                        }
                        self.focus = Focus::ImageBuild;
                        continue;
                    }
                    if cancelled {
                        self.build_project_idx = None;
                        self.build_session_group = None;
                        self.push_log(format!("{label} cancelled"), true);
                        self.focus = Focus::ImageBuild;
                        continue;
                    }
                    if success {
                        self.build_project_idx = None;
                        self.build_session_group = None;
                        self.push_log(format!("{label} finished successfully"), false);
                        self.build_container_idx = None;
                        self.do_launch_container_on_project_with_priority(
                            launch_project_idx,
                            launch_container_idx,
                            crate::proxy::SourcePriority::Primary,
                            launch_session_group,
                        );
                    } else {
                        self.build_project_idx = None;
                        self.build_session_group = None;
                        let suffix = exit_code
                            .map(|code| format!(" (exit code {code})"))
                            .unwrap_or_default();
                        self.push_log(format!("{label} failed{suffix}"), true);
                        if let Some(diagnostic) = diagnostic {
                            self.push_log(format!("  build detail: {diagnostic}"), true);
                        }
                        self.focus = Focus::ImageBuild;
                    }
                }
                Err(_) => break,
            }
        }

        for i in (0..self.sessions.len()).rev() {
            if !self.sessions[i].is_exited() {
                continue;
            }
            let exited_for = self.sessions[i].launched_at.elapsed();
            if !self.sessions[i].exit_reported {
                self.sessions[i].exit_reported = true;
                let label = self.sessions[i].tab_label();
                match crate::container::inspect_container_exit(&self.sessions[i].docker_name) {
                    Ok(Some((exit_code, error))) => {
                        let suffix = exit_code
                            .map(|code| format!(" (exit code {code})"))
                            .unwrap_or_default();
                        if error.is_empty() {
                            self.push_log(format!("{label} exited immediately{suffix}"), true);
                        } else {
                            self.push_log(
                                format!("{label} exited immediately{suffix}: {error}"),
                                true,
                            );
                        }
                    }
                    Ok(None) => {
                        self.push_log(format!("{label} exited immediately"), true);
                    }
                    Err(e) => {
                        self.push_log(
                            format!(
                                "{label} exited immediately; failed to inspect exit status: {e}"
                            ),
                            true,
                        );
                    }
                }
                continue;
            }
            if exited_for < std::time::Duration::from_secs(15) {
                continue;
            }
            let label = self.sessions[i].tab_label();
            self.push_log(format!("container '{}' exited", label), false);
            self.close_session(i);
            if self.active_session.is_none() && self.focus != Focus::ImageBuild {
                self.focus = Focus::Sidebar;
            }
        }

        for idx in (0..self.pending_stop.len()).rev() {
            let Some((project, container_id)) = self
                .pending_stop
                .get(idx)
                .map(|item| (item.project.clone(), item.container_id.clone()))
            else {
                continue;
            };
            let decision = self.handle_stop_request(&project, &container_id);
            if let Some(tx) = self.pending_stop[idx].response_tx.take() {
                let _ = tx.send(decision);
            }
            self.pending_stop.remove(idx);
        }

        if self.focus == Focus::Terminal {
            if let Some(si) = self.active_session {
                if let Some(session) = self.sessions.get(si) {
                    session.clear_bell();
                }
            }
        }

        let selected_before_prune = {
            let items = self.sidebar_items();
            self.selected_sidebar_item_from(&items)
        };
        self.prune_terminal_activities();
        let items = self.sidebar_items();
        self.restore_sidebar_selection(selected_before_prune.as_ref(), &items);
    }

    pub(crate) fn enqueue_pending_network(&mut self, item: crate::proxy::PendingNetworkItem) {
        if let Some(existing) = self
            .pending_net
            .iter_mut()
            .find(|pending| pending_network_merge_key_matches(pending, &item))
        {
            existing.merged_response_txs.push(item.response_tx);
            return;
        }

        if self.pending_net.len() < Self::MAX_PENDING_NETWORK_APPROVALS {
            self.pending_net.push(item);
            return;
        }

        let activity_id = item.activity_id.clone();
        let mut item = item;
        crate::tui::app::approvals::send_pending_network_decision(
            &mut item,
            crate::proxy::NetworkDecision::Deny,
        );
        if let Some(activity) = self.activity_by_id_mut(&activity_id) {
            activity.state = crate::activity::ActivityState::Denied;
            activity.status = Some("too many pending network approvals".to_string());
            activity.updated_at = std::time::Instant::now();
            activity.finished_at = Some(activity.updated_at);
            activity.terminal_unselected_at = None;
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let _ = mouse;
    }
}

fn pending_network_merge_key_matches(
    left: &crate::proxy::PendingNetworkItem,
    right: &crate::proxy::PendingNetworkItem,
) -> bool {
    left.source_project == right.source_project
        && left.method.eq_ignore_ascii_case(&right.method)
        && left.host.eq_ignore_ascii_case(&right.host)
        && left.port == right.port
        && left.path == right.path
}
