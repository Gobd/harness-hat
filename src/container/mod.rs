use anyhow::{Context, Result, bail};

mod core;
mod helpers;
mod spawn;

pub use core::*;
pub use helpers::{ContainerUsageStats, inspect_container_exit, inspect_container_usage};
pub(crate) use helpers::{docker_image_exists, read_container_id};
pub use spawn::*;

pub fn ensure_docker_installed_and_running() -> Result<()> {
    if which::which("docker").is_err() {
        bail!("docker not found in PATH — harness-hat requires Docker to run containers");
    }

    let output = std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("checking whether Docker is running")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        bail!("docker is installed but not running — start the Docker daemon and try again");
    }

    bail!("docker is installed but not running: {stderr}");
}
