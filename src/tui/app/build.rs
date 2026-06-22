use super::*;

impl App {
    pub const BASE_IMAGE_TAG: &'static str = "harness-hat-base:local";

    pub(crate) fn start_docker_build(
        &mut self,
        label: &str,
        shell_command: String,
        launch_project_idx: usize,
        launch_container_idx: usize,
    ) {
        if self.build_task.is_some() {
            self.push_log("a docker build is already running", true);
            return;
        }

        self.build_output.clear();
        self.build_finished = None;
        self.build_scroll = 0;
        if self.build_project_idx.is_none() {
            self.build_project_idx = self.selected_project_idx();
        }
        self.active_session = None;
        self.focus = Focus::ImageBuild;
        self.push_log(format!("starting {label} in shell"), false);
        self.push_log(format!("$ {shell_command}"), false);

        if let Some(pi) = self.build_project_idx {
            let items = self.sidebar_items();
            if let Some(pos) = items
                .iter()
                .position(|item| *item == SidebarItem::Build(pi))
            {
                self.sidebar_idx = pos;
            }
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let shell_command_for_state = shell_command.clone();

        let tx = self.build_event_tx.clone();
        let launch_session_group = self.build_session_group;
        let label = label.to_string();
        let task_cancel = Arc::clone(&cancel_flag);
        let handle = tokio::spawn(async move {
            run_build_shell_command(
                label,
                shell_command,
                launch_project_idx,
                launch_container_idx,
                launch_session_group,
                task_cancel,
                tx,
            )
            .await;
        });

        self.build_task = Some(BuildTaskState {
            shell_command: shell_command_for_state,
            cancel_flag,
            handle,
        });
    }

    /// Request cancellation of the in-flight build (if any) and abort the task.
    /// Called on quit and when the user presses Esc on the build pane.
    pub(crate) fn cancel_docker_build(&mut self) {
        if let Some(task) = self.build_task.take() {
            task.cancel_flag.store(true, Ordering::SeqCst);
            task.handle.abort();
            self.push_log("docker build cancelled", true);
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
    ) -> (Vec<String>, Option<Vec<String>>) {
        let cmd = vec![
            "build".to_string(),
            "-t".to_string(),
            image.to_string(),
            "-f".to_string(),
            dockerfile_path.display().to_string(),
            dockerfile_context.display().to_string(),
        ];
        let base_cmd = if image == Self::BASE_IMAGE_TAG {
            None
        } else {
            Some(vec![
                "build".to_string(),
                "-t".to_string(),
                Self::BASE_IMAGE_TAG.to_string(),
                "-f".to_string(),
                base_dockerfile_dir
                    .join("harness-hat-base.dockerfile")
                    .display()
                    .to_string(),
                base_dockerfile_dir.display().to_string(),
            ])
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
        self.build_project_idx = Some(pi);
        self.build_container_idx = Some(ctr_idx);
        self.build_session_group = build_session_group;
        self.build_cursor = 0;
        self.build_output.clear();
        self.build_scroll = 0;
        self.active_session = None;
        self.active_settings_project = None;
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
