//! `hht rebuild` — rebuild the base image and configured Dockerfile templates.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASE_IMAGE: &str = "harness-hat-base:local";
const BASE_DOCKERFILE: &str = "harness-hat-base.dockerfile";

/// Rebuild the base image, followed by selected templates (or every template
/// found in the configured Docker directory when `requested_templates` is
/// empty). Template builds run in parallel after the base image succeeds.
pub fn run(
    requested_templates: Vec<String>,
    no_cache: bool,
    explicit_config: Option<PathBuf>,
) -> Result<()> {
    crate::container::ensure_docker_installed_and_running()?;

    let Some(config_path) = crate::manager::resolve_or_prompt_config_path(explicit_config)? else {
        return Ok(());
    };
    let config = crate::config::load(&config_path)?;
    crate::init::ensure_docker_assets(&config.docker_dir)?;

    let templates = discover_templates(&config.docker_dir)?;
    let selected = select_templates(&templates, &requested_templates)?;

    let base_dockerfile = config.docker_dir.join(BASE_DOCKERFILE);
    if !base_dockerfile.is_file() {
        bail!("base Dockerfile not found: {}", base_dockerfile.display());
    }

    println!("==> Building {BASE_IMAGE}");
    run_docker_build(&base_dockerfile, &config.docker_dir, BASE_IMAGE, no_cache)?;

    if selected.is_empty() {
        println!(
            "==> No template Dockerfiles found in {}",
            config.docker_dir.display()
        );
        return Ok(());
    }

    println!(
        "==> Building templates in parallel: {}",
        selected.join(", ")
    );
    let docker_dir = config.docker_dir.clone();
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(selected.len());
        for stem in &selected {
            let dockerfile = templates
                .get(stem)
                .expect("selected templates originate from discovered templates")
                .clone();
            let stem = stem.clone();
            let docker_dir = docker_dir.clone();
            handles.push(scope.spawn(move || {
                let image = crate::config::image_tag_for_stem(&stem);
                println!("==> Building {image}");
                run_docker_build(&dockerfile, &docker_dir, &image, no_cache)
                    .with_context(|| format!("building template '{stem}'"))
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("template build thread panicked"))
            .collect::<Vec<_>>()
    });

    let failures = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        bail!(
            "one or more template builds failed:\n{}",
            failures.join("\n")
        );
    }

    println!("==> All images built successfully");
    Ok(())
}

fn discover_templates(docker_dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let entries = std::fs::read_dir(docker_dir)
        .with_context(|| format!("reading Docker directory {}", docker_dir.display()))?;
    let mut templates = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".dockerfile") else {
            continue;
        };
        if stem.is_empty() || stem == "harness-hat-base" {
            continue;
        }
        templates.insert(stem.to_string(), path);
    }
    Ok(templates)
}

fn select_templates(
    available: &BTreeMap<String, PathBuf>,
    requested: &[String],
) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(available.keys().cloned().collect());
    }

    let mut selected = Vec::new();
    for name in requested {
        if !available.contains_key(name) {
            let available = available.keys().cloned().collect::<Vec<_>>().join(", ");
            bail!("unknown template '{name}'; available templates: {available}");
        }
        if !selected.contains(name) {
            selected.push(name.clone());
        }
    }
    Ok(selected)
}

fn run_docker_build(dockerfile: &Path, context: &Path, image: &str, no_cache: bool) -> Result<()> {
    let mut command = Command::new("docker");
    command.arg("build");
    if no_cache {
        command.arg("--no-cache");
    }
    let status = command
        .arg("-t")
        .arg(image)
        .arg("-f")
        .arg(dockerfile)
        .arg(context)
        .status()
        .with_context(|| format!("running docker build for {image}"))?;
    if !status.success() {
        bail!("docker build for {image} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{discover_templates, select_templates};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn discover_templates_ignores_base_and_non_dockerfiles() {
        let dir = tempfile::tempdir().expect("temp Docker directory");
        std::fs::write(dir.path().join("go.dockerfile"), "FROM scratch").expect("write go");
        std::fs::write(
            dir.path().join("harness-hat-base.dockerfile"),
            "FROM scratch",
        )
        .expect("write base");
        std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write notes");

        let templates = discover_templates(dir.path()).expect("discover templates");
        assert_eq!(templates.keys().collect::<Vec<_>>(), vec!["go"]);
    }

    #[test]
    fn select_templates_deduplicates_requested_names() {
        let available = BTreeMap::from([
            ("go".to_string(), PathBuf::from("go.dockerfile")),
            ("rust".to_string(), PathBuf::from("rust.dockerfile")),
        ]);

        let selected = select_templates(
            &available,
            &["rust".to_string(), "go".to_string(), "rust".to_string()],
        )
        .expect("select templates");
        assert_eq!(selected, vec!["rust", "go"]);
    }

    #[test]
    fn select_templates_rejects_unknown_name() {
        let available = BTreeMap::from([("go".to_string(), PathBuf::from("go.dockerfile"))]);
        let error = select_templates(&available, &["python".to_string()])
            .expect_err("unknown template must fail");
        assert!(error.to_string().contains("unknown template 'python'"));
    }
}
