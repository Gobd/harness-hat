use super::*;

impl App {
    pub const BASE_IMAGE_TAG: &'static str = "harness-hat-base:local";

    pub(crate) fn start_docker_build(
        &mut self,
        label: &str,
        docker_commands: Vec<Vec<String>>,
        command_display: String,
        launch_workspace_idx: usize,
        launch_container_idx: usize,
    ) {
        if self.build_task.is_some() {
            self.push_log("a docker build is already running", true);
            return;
        }

        self.build_output.clear();
        self.build_finished = None;
        self.build_scroll = 0;
        if self.build_workspace_idx.is_none() {
            self.build_workspace_idx = self.selected_workspace_idx();
        }
        self.active_session = None;
        self.focus = Focus::ImageBuild;
        self.push_log(format!("starting {label}"), false);
        self.push_log(format!("$ {command_display}"), false);

        if let Some(pi) = self.build_workspace_idx {
            let items = self.sidebar_items();
            if let Some(pos) = items
                .iter()
                .position(|item| *item == SidebarItem::Build(pi))
            {
                self.sidebar_idx = pos;
            }
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let command_display_for_state = command_display.clone();

        let tx = self.build_event_tx.clone();
        let launch_session_group = self.build_session_group;
        let label = label.to_string();
        let task_cancel = Arc::clone(&cancel_flag);
        let handle = tokio::spawn(async move {
            run_build_docker_commands(
                label,
                docker_commands,
                launch_workspace_idx,
                launch_container_idx,
                launch_session_group,
                task_cancel,
                tx,
            )
            .await;
        });

        self.build_task = Some(BuildTaskState {
            command_display: command_display_for_state,
            cancel_flag,
            handle,
        });
    }

    /// Request cancellation of the in-flight build (if any).
    /// Called on quit and when the user presses Esc on the build pane.
    pub(crate) fn cancel_docker_build(&mut self) {
        if let Some(task) = self.build_task.as_ref() {
            if !task.cancel_flag.swap(true, Ordering::SeqCst) {
                self.push_log("docker build cancellation requested", true);
            }
        }
    }

    pub(crate) async fn cancel_docker_build_for_shutdown(&mut self) {
        let Some(task) = self.build_task.take() else {
            return;
        };

        if !task.cancel_flag.swap(true, Ordering::SeqCst) {
            self.push_log("docker build cancellation requested", true);
        }

        let mut handle = task.handle;
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle).await {
            Ok(_) => {}
            Err(_) => {
                self.push_log("docker build cancellation timed out", true);
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    pub(crate) fn push_build_output(&mut self, line: impl Into<String>, is_error: bool) {
        self.build_output.push_back((line.into(), is_error));
        if self.build_output.len() > 400 {
            self.build_output.pop_front();
        }
        if self.build_scroll > 0 {
            self.build_scroll = self.build_scroll.saturating_add(1);
        }
    }

    pub fn build_commands_for(
        dockerfile_path: &Path,
        image: &str,
        dockerfile_context: &Path,
        base_dockerfile_dir: &Path,
        no_cache: bool,
    ) -> (Vec<String>, Option<Vec<String>>) {
        let mut cmd = vec!["build".to_string()];
        if no_cache {
            cmd.push("--no-cache".to_string());
        }
        cmd.extend([
            "-t".to_string(),
            image.to_string(),
            "-f".to_string(),
            dockerfile_path.display().to_string(),
            dockerfile_context.display().to_string(),
        ]);
        let base_cmd = if image == Self::BASE_IMAGE_TAG {
            None
        } else {
            let mut base = vec!["build".to_string()];
            if no_cache {
                base.push("--no-cache".to_string());
            }
            base.extend([
                "-t".to_string(),
                Self::BASE_IMAGE_TAG.to_string(),
                "-f".to_string(),
                base_dockerfile_dir
                    .join("harness-hat-base.dockerfile")
                    .display()
                    .to_string(),
                base_dockerfile_dir.display().to_string(),
            ]);
            Some(base)
        };
        (cmd, base_cmd)
    }

    pub fn dockerfile_stem_for_image(image: &str) -> String {
        let raw_name = image
            .split(':')
            .next()
            .unwrap_or(image)
            .split('/')
            .next_back()
            .unwrap_or(image);
        if raw_name == "harness-hat-base" {
            return "harness-hat-base".to_string();
        }
        raw_name
            .strip_prefix("harness-hat-")
            .unwrap_or(raw_name)
            .to_string()
    }

    pub(crate) fn open_image_build_prompt(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        image: &str,
        build_session_group: Option<usize>,
    ) {
        self.build_workspace_idx = Some(pi);
        self.build_container_idx = Some(ctr_idx);
        self.build_session_group = build_session_group;
        self.build_cursor = 0;
        self.build_output.clear();
        self.build_scroll = 0;
        self.active_session = None;
        self.active_settings_workspace = None;
        self.container_picker = None;
        self.build_finished = None;
        self.focus = Focus::ImageBuild;
        self.push_log(
            format!("docker image '{image}' not found locally; build required"),
            true,
        );
    }

    pub(crate) fn preflight_image_or_prompt_build<F>(
        &mut self,
        pi: usize,
        ctr_idx: usize,
        image: &str,
        build_session_group: Option<usize>,
        image_exists: F,
    ) -> bool
    where
        F: FnOnce(&str) -> std::io::Result<bool>,
    {
        match image_exists(image) {
            Ok(true) => true,
            Ok(false) => {
                self.open_image_build_prompt(pi, ctr_idx, image, build_session_group);
                false
            }
            Err(e) => {
                // If we can't check, preserve legacy behavior: attempt to run and
                // surface the real docker error in the session/logs.
                self.push_log(
                    format!("warning: failed to check docker image '{image}': {e}"),
                    true,
                );
                true
            }
        }
    }
}
