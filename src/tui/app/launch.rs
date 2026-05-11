use super::*;

impl App {
    fn container_command_for_profile(ctr: &crate::config::ContainerDef) -> Option<Vec<String>> {
        ctr.command.clone()
    }

    pub(crate) fn do_launch_container_on_project(&mut self, pi: usize, ctr_idx: usize) {
        self.do_launch_container_on_project_with_priority(
            pi,
            ctr_idx,
            crate::proxy::SourcePriority::Primary,
        );
    }

    pub(crate) fn do_launch_container_on_project_with_priority(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
    ) {
        self.do_launch_container_on_project_with_priority_and_env(pi, ctr_idx, proxy_priority, &[]);
    }

    pub(crate) fn do_launch_container_on_project_with_priority_and_env(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
        extra_env: &[(String, String)],
    ) {
        let cfg = self.config.get();
        let exec_host = cfg.defaults.hostdo.server_host.trim();
        if host_bind_is_loopback(exec_host) {
            self.push_log(
                format!("cannot launch container: defaults.hostdo.server_host='{}' is loopback; set it to '0.0.0.0'", exec_host),
                true,
            );
            return;
        }
        let ctr = match cfg.containers.get(ctr_idx) {
            Some(c) => c.clone(),
            None => return,
        };

        let proj = match cfg.workspaces.get(pi) {
            Some(p) => p.clone(),
            None => return,
        };

        if !self.preflight_image_or_prompt_build(pi, ctr_idx, &ctr.image, docker_image_exists) {
            return;
        }

        let mount_source_path = proj.canonical_path.clone();
        self.log_project_rules_status(&proj);

        let exec_port = cfg.defaults.hostdo.server_port;
        let exec_host = &cfg.defaults.hostdo.server_host;
        let exec_url = format!("http://{exec_host}:{exec_port}");
        let hostdo_script_host_path = cfg.docker_dir.join("scripts/hostdo.py");
        let proxy_host = &cfg.defaults.proxy.proxy_host;
        let session_token = uuid::Uuid::new_v4().simple().to_string();
        let scoped_proxy = match crate::proxy::spawn_scoped_listener(
            &self.proxy_state,
            proxy_host,
            &proj.name,
            &ctr.name,
            &session_token,
            proxy_priority,
        ) {
            Ok(listener) => listener,
            Err(e) => {
                self.push_log(
                    format!("cannot launch '{}' on '{}': {e}", ctr.name, proj.name),
                    true,
                );
                return;
            }
        };
        let proxy_url = scoped_proxy.proxy_url();
        self.push_log(
            format!("launching '{}' on '{}'", ctr.name, proj.name),
            false,
        );

        match crate::agents::inject_agent_config(
            &mount_source_path,
            &proj.canonical_path,
            &proj.name,
            true,
            &ctr.mount_target,
            &exec_url,
            &proxy_url,
            &ctr.starter_network_allowlist,
        ) {
            Ok(result) => {
                if let Some(created) = result.created_rules {
                    self.record_completed_rules_internal_write(
                        created.path.clone(),
                        created.content,
                    );
                    self.push_log(
                        format!(
                            "created starter harness-rules.toml in '{}'",
                            proj.canonical_path.display()
                        ),
                        false,
                    );
                }
            }
            Err(e) => self.push_log(format!("agent config injection warning: {e}"), true),
        }

        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((120, 40));
        let pty_cols = term_cols.saturating_sub(38).max(20);
        let pty_rows = term_rows.saturating_sub(10).max(6);

        #[cfg(target_os = "macos")]
        if cfg.defaults.proxy.strict_network {
            self.push_log(
                "strict_network on macOS requires Docker `--privileged`; harness-hat applies it automatically for this container launch",
                false,
            );
        }

        self.session_registry.insert(
            session_token.clone(),
            crate::server::SessionIdentity {
                project: proj.name.clone(),
                container_id: String::new(),
                mount_target: ctr.mount_target.display().to_string(),
            },
        );

        let command_argv = Self::container_command_for_profile(&ctr);
        match crate::container::spawn(
            &ctr,
            command_argv.as_deref(),
            &proj.name,
            &mount_source_path,
            &session_token,
            &self.token,
            &exec_url,
            &proxy_url,
            &self.ca_cert_path,
            Some(hostdo_script_host_path.as_path()),
            Some(scoped_proxy),
            proxy_priority,
            cfg.defaults.proxy.strict_network,
            extra_env,
            pty_rows,
            pty_cols,
        ) {
            Ok((session, launch_notes)) => {
                let new_si = self.sessions.len();
                self.sessions.push(session);
                if let Some(s) = self.sessions.get(new_si) {
                    self.session_registry.insert(
                        s.session_token.clone(),
                        crate::server::SessionIdentity {
                            project: s.project.clone(),
                            container_id: s.container_id.clone(),
                            mount_target: s.mount_target.clone(),
                        },
                    );
                }
                self.active_session = Some(new_si);
                self.scroll_mode = false;
                self.scroll_mouse_passthrough = false;
                self.terminal_scroll = 0;
                self.focus = Focus::Terminal;
                for note in launch_notes {
                    self.push_log(note, false);
                }
                if let Some(pos) = self
                    .sidebar_items()
                    .iter()
                    .position(|item| *item == SidebarItem::Session(new_si))
                {
                    self.sidebar_idx = pos;
                }
            }
            Err(e) => {
                self.push_log(
                    format!("launch '{}' on '{}' failed: {e}", ctr.name, proj.name),
                    true,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use crate::config::{ContainerDef, default_mount_target};

    #[test]
    fn container_command_for_profile_uses_configured_override() {
        let profile = ContainerDef {
            name: "claude".to_string(),
            image: String::new(),
            image_stem: String::new(),
            profile: None,
            mount_target: default_mount_target(),
            command: Some(vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]),
            grayscale_palette: false,
            mouse_scroll: crate::config::MouseScrollMode::Auto,
            starter_network_allowlist: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            mounts: Vec::new(),
            env: std::collections::HashMap::new(),
            env_passthrough: Vec::new(),
            bypass_proxy: Vec::new(),
            localhost_forwards: Vec::new(),
        };

        assert_eq!(
            App::container_command_for_profile(&profile),
            Some(vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ])
        );
    }

    #[test]
    fn container_command_for_profile_requires_explicit_command() {
        let profile = ContainerDef {
            name: "codex".to_string(),
            image: String::new(),
            image_stem: String::new(),
            profile: None,
            mount_target: default_mount_target(),
            command: None,
            grayscale_palette: true,
            mouse_scroll: crate::config::MouseScrollMode::Auto,
            starter_network_allowlist: Vec::new(),
            mcp_log_paths: Vec::new(),
            mcp_log_pattern: None,
            mounts: Vec::new(),
            env: std::collections::HashMap::new(),
            env_passthrough: Vec::new(),
            bypass_proxy: Vec::new(),
            localhost_forwards: Vec::new(),
        };
        assert_eq!(App::container_command_for_profile(&profile), None);
    }
}
