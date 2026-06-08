use super::*;

impl App {
    fn container_command_for_profile(ctr: &crate::config::ContainerDef) -> Option<Vec<String>> {
        ctr.command.clone()
    }

    pub(crate) fn do_launch_container_on_project_with_priority(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
        session_group: Option<usize>,
    ) {
        self.do_launch_container_on_project_with_priority_and_env(
            pi,
            ctr_idx,
            proxy_priority,
            &[],
            session_group,
        );
    }

    pub(crate) fn do_launch_container_on_project_with_priority_and_env(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        proxy_priority: crate::proxy::SourcePriority,
        extra_env: &[(String, String)],
        session_group: Option<usize>,
    ) {
        let cfg = self.config.get();
        let ctr = match cfg.containers.get(ctr_idx) {
            Some(c) => c.clone(),
            None => return,
        };

        let proj = match cfg.workspaces.get(pi) {
            Some(p) => p.clone(),
            None => return,
        };

        if !self.preflight_image_or_prompt_build(
            pi,
            ctr_idx,
            &ctr.image,
            session_group,
            docker_image_exists,
        ) {
            return;
        }

        let mount_source_path = proj.canonical_path.clone();
        self.log_project_rules_status(&proj);

        let control_port = cfg.defaults.control.server_port;
        let control_host = &cfg.defaults.control.server_host;
        let control_url = format!("http://{control_host}:{control_port}");
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
        let group_idx = self.resolve_or_create_session_group(session_group, pi, ctr_idx);

        self.push_log(
            format!("launching '{}' on '{}'", ctr.name, proj.name),
            false,
        );

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
            &control_url,
            &proxy_url,
            &self.ca_cert_path,
            None,
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
                self.add_session_terminal(group_idx, new_si);
                for note in launch_notes {
                    self.push_log(note, false);
                }
                let pos = self
                    .sidebar_items()
                    .iter()
                    .position(|item| *item == SidebarItem::Session(group_idx));
                if let Some(pos) = pos {
                    self.sidebar_idx = pos;
                }
                self.active_session = Some(new_si);
                self.preview_session = Some(new_si);
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
    fn container_command_for_template_uses_configured_override() {
        let profile = ContainerDef {
            name: "dev".to_string(),
            image: String::new(),
            image_stem: String::new(),
            profile: None,
            mount_target: default_mount_target(),
            command: Some(vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "htop".to_string(),
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
            memory: None,
            cpus: None,
            shm_size: None,
        };

        assert_eq!(
            App::container_command_for_profile(&profile),
            Some(vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "htop".to_string()
            ])
        );
    }

    #[test]
    fn container_command_for_template_requires_explicit_command() {
        let profile = ContainerDef {
            name: "dev".to_string(),
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
            memory: None,
            cpus: None,
            shm_size: None,
        };
        assert_eq!(App::container_command_for_profile(&profile), None);
    }
}
