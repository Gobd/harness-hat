//! Small graphical launcher for container-backed Claude Desktop sessions.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod gui {
    use anyhow::Result;
    use eframe::egui;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver};

    const TEMPLATES: &[Template] = &[
        Template::new("default", "General", "A small general-purpose environment"),
        Template::new(
            "typescript",
            "Node / TypeScript",
            "Node.js, npm, Bun, JavaScript and TypeScript",
        ),
        Template::new("python", "Python", "Python, uv and common build tools"),
        Template::new("go", "Go", "The Go toolchain and common development tools"),
        Template::new("rust", "Rust", "Rust, Cargo and common Cargo utilities"),
        Template::new("kotlin", "Kotlin / JVM", "Kotlin, Java and Gradle projects"),
        Template::new("android", "Android", "Android SDK, Kotlin, Java and Gradle"),
        Template::new("php", "PHP", "PHP and Composer projects"),
        Template::new("csharp", "C# / .NET", ".NET and C# projects"),
    ];

    #[derive(Clone, Copy)]
    struct Template {
        id: &'static str,
        label: &'static str,
        description: &'static str,
    }

    impl Template {
        const fn new(id: &'static str, label: &'static str, description: &'static str) -> Self {
            Self {
                id,
                label,
                description,
            }
        }
    }

    enum LaunchState {
        Idle,
        Launching(Receiver<Result<(), String>>),
        Opened,
        Failed(String),
    }

    pub struct Launcher {
        project: Option<PathBuf>,
        template: usize,
        template_note: String,
        state: LaunchState,
    }

    impl Default for Launcher {
        fn default() -> Self {
            Self {
                project: None,
                template: 0,
                template_note: "Choose a project and Hat will suggest an environment.".into(),
                state: LaunchState::Idle,
            }
        }
    }

    impl Launcher {
        fn choose_project(&mut self) {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Choose a project to protect with Harness Hat")
                .pick_folder()
            else {
                return;
            };
            let path = path.canonicalize().unwrap_or(path);
            let (template, note) = suggested_template(&path);
            self.project = Some(path);
            self.template = template_index(&template);
            self.template_note = note;
            self.state = LaunchState::Idle;
        }

        fn launch(&mut self, context: &egui::Context) {
            let Some(project) = self.project.clone() else {
                return;
            };
            let template = TEMPLATES[self.template].id.to_string();
            let config = match harness_hat::manager::discover_default_config_path() {
                Some(config) => config,
                None => {
                    self.state = LaunchState::Failed(
                        "Harness Hat is not set up yet. Run `hat init` and `hat install` once, then reopen this app."
                            .into(),
                    );
                    return;
                }
            };
            let (tx, rx) = mpsc::channel();
            let repaint = context.clone();
            std::thread::spawn(move || {
                let result = launch_workspace(project, template, config)
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                repaint.request_repaint();
            });
            self.state = LaunchState::Launching(rx);
        }

        fn poll_launch(&mut self) {
            let result = match &self.state {
                LaunchState::Launching(rx) => rx.try_recv().ok(),
                _ => None,
            };
            if let Some(result) = result {
                self.state = match result {
                    Ok(()) => LaunchState::Opened,
                    Err(error) => LaunchState::Failed(error),
                };
            }
        }
    }

    impl eframe::App for Launcher {
        fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll_launch();
            egui::CentralPanel::default().show(context, |ui| {
                ui.add_space(16.0);
                ui.heading("Open Claude safely");
                ui.label("Choose a project. Claude's code tools will run inside a Harness Hat container.");
                ui.add_space(18.0);

                ui.label(egui::RichText::new("PROJECT FOLDER").small().strong());
                ui.horizontal(|ui| {
                    let path = self.project.as_ref().map_or_else(
                        || "No folder selected".to_string(),
                        |path| path.display().to_string(),
                    );
                    ui.add_sized([360.0, 28.0], egui::Label::new(path).truncate());
                    if ui.button("Choose…").clicked() {
                        self.choose_project();
                    }
                });

                ui.add_space(16.0);
                ui.label(egui::RichText::new("DEVELOPMENT ENVIRONMENT").small().strong());
                ui.add_enabled_ui(self.project.is_some(), |ui| {
                    egui::ComboBox::from_id_salt("template")
                        .selected_text(TEMPLATES[self.template].label)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for (index, template) in TEMPLATES.iter().enumerate() {
                                ui.selectable_value(&mut self.template, index, template.label);
                            }
                        });
                    ui.label(TEMPLATES[self.template].description);
                    ui.label(egui::RichText::new(&self.template_note).small().weak());
                });

                ui.add_space(22.0);
                let launching = matches!(self.state, LaunchState::Launching(_));
                let button = egui::Button::new(if launching {
                    "Starting protected workspace…"
                } else {
                    "Open in Claude Desktop"
                });
                if ui
                    .add_enabled(self.project.is_some() && !launching, button)
                    .clicked()
                {
                    self.launch(context);
                }

                ui.add_space(12.0);
                match &self.state {
                    LaunchState::Idle => {
                        ui.label(egui::RichText::new("Container protected • strict network policy uses your Hat configuration").small());
                    }
                    LaunchState::Launching(_) => {
                        ui.spinner();
                        context.request_repaint_after(std::time::Duration::from_millis(100));
                    }
                    LaunchState::Opened => {
                        ui.colored_label(egui::Color32::from_rgb(40, 150, 80), "Claude Desktop opened. Select the Hat environment in Code.");
                    }
                    LaunchState::Failed(error) => {
                        ui.colored_label(egui::Color32::from_rgb(190, 55, 55), error);
                    }
                }
            });
        }
    }

    fn launch_workspace(project: PathBuf, template: String, config: PathBuf) -> Result<()> {
        let exit = harness_hat::workspace::run(
            Vec::new(),
            false,
            Some(template),
            None,
            false,
            false,
            Some(project),
            true,
            true,
            None,
            Some(config),
        )?;
        anyhow::ensure!(
            exit == 0,
            "the workspace launcher exited with status {exit}"
        );
        Ok(())
    }

    fn suggested_template(project: &Path) -> (String, String) {
        if let Some(saved) = saved_template(project) {
            return (
                saved.clone(),
                format!(
                    "Using the environment already saved for this workspace ({saved}). You can change it above."
                ),
            );
        }
        let detected = detect_template(project);
        if detected == "default" {
            (
                detected.into(),
                "No dominant language detected; General is a safe starting point.".into(),
            )
        } else {
            (
                detected.into(),
                format!("Suggested from files in this project: {detected}."),
            )
        }
    }

    fn saved_template(project: &Path) -> Option<String> {
        let config_path = harness_hat::manager::discover_default_config_path()?;
        let config = harness_hat::config::load(&config_path).ok()?;
        let workspace = config
            .workspaces
            .iter()
            .filter(|workspace| project.starts_with(&workspace.canonical_path))
            .max_by_key(|workspace| workspace.canonical_path.components().count())?;
        let saved = workspace.template.clone().or_else(|| {
            harness_hat::rules::load(&workspace.canonical_path.join("harness-rules.toml"))
                .ok()
                .and_then(|rules| rules.template)
        })?;
        TEMPLATES
            .iter()
            .any(|template| template.id == saved)
            .then_some(saved)
    }

    fn detect_template(project: &Path) -> &'static str {
        let marker = |name: &str| project.join(name).exists();
        if marker("Cargo.toml") {
            return "rust";
        }
        if marker("go.mod") || marker("go.work") {
            return "go";
        }
        if marker("tsconfig.json") || marker("package.json") {
            return "typescript";
        }
        if marker("pyproject.toml") || marker("requirements.txt") || marker("Pipfile") {
            return "python";
        }
        if marker("AndroidManifest.xml") || marker("app/src/main/AndroidManifest.xml") {
            return "android";
        }
        if marker("build.gradle.kts") || marker("settings.gradle.kts") {
            return "kotlin";
        }
        if marker("composer.json") {
            return "php";
        }
        if std::fs::read_dir(project)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| matches!(extension, "sln" | "csproj"))
            })
        {
            return "csharp";
        }

        let mut scores = [0u16; 8];
        for entry in ignore::WalkBuilder::new(project)
            .max_depth(Some(4))
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .take(2_000)
        {
            match entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some("rs") => scores[0] += 1,
                Some("go") => scores[1] += 1,
                Some("ts" | "tsx" | "js" | "jsx") => scores[2] += 1,
                Some("py") => scores[3] += 1,
                Some("kt" | "kts") => scores[4] += 1,
                Some("php") => scores[5] += 1,
                Some("cs") => scores[6] += 1,
                _ => {}
            }
        }
        let templates = [
            "rust",
            "go",
            "typescript",
            "python",
            "kotlin",
            "php",
            "csharp",
            "default",
        ];
        scores
            .iter()
            .enumerate()
            .max_by_key(|(_, score)| *score)
            .filter(|(_, score)| **score > 0)
            .map_or("default", |(index, _)| templates[index])
    }

    fn template_index(id: &str) -> usize {
        TEMPLATES
            .iter()
            .position(|template| template.id == id)
            .unwrap_or(0)
    }

    pub fn run() -> Result<()> {
        super::add_gui_tool_paths();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([560.0, 390.0])
                .with_min_inner_size([480.0, 340.0]),
            ..Default::default()
        };
        eframe::run_native(
            "Harness Hat",
            options,
            Box::new(|context| {
                context.egui_ctx.set_visuals(egui::Visuals::light());
                Ok(Box::<Launcher>::default())
            }),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() {
    if let Err(error) = gui::run() {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Harness Hat could not start")
            .set_description(format!("{error:#}"))
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("the graphical Harness Hat launcher currently supports macOS and Windows");
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn add_gui_tool_paths() {
    use std::path::PathBuf;
    let mut paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ]);
    #[cfg(target_os = "windows")]
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("Docker/Docker/resources/bin"));
    }
    for candidate in candidates {
        if candidate.is_dir() && !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    if let Ok(path) = std::env::join_paths(paths) {
        // Called before eframe starts worker threads.
        unsafe { std::env::set_var("PATH", path) };
    }
}
