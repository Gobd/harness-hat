use anyhow::{Context, Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod core;
mod helpers;
mod spawn;

pub use core::*;
pub use helpers::{
    ContainerState, ContainerUsageStats, inspect_container_state, inspect_container_usage,
};
pub(crate) use helpers::{docker_image_exists, read_container_id};
pub use spawn::*;

/// Process-wide Docker availability shared by the daemon's health endpoint and
/// its background readiness monitor.
#[derive(Clone)]
pub struct DockerStatus {
    inner: Arc<DockerStatusInner>,
}

struct DockerStatusInner {
    available: AtomicBool,
    reason: Mutex<String>,
}

impl DockerStatus {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DockerStatusInner {
                available: AtomicBool::new(false),
                reason: Mutex::new("Docker availability has not been checked yet".into()),
            }),
        }
    }

    /// Refresh the cached readiness state and return whether Docker is ready.
    pub fn refresh(&self) -> bool {
        match ensure_docker_installed_and_running() {
            Ok(()) => {
                self.inner.available.store(true, Ordering::Release);
                if let Ok(mut reason) = self.inner.reason.lock() {
                    reason.clear();
                }
                true
            }
            Err(error) => {
                self.inner.available.store(false, Ordering::Release);
                if let Ok(mut reason) = self.inner.reason.lock() {
                    *reason = error.to_string();
                }
                false
            }
        }
    }

    pub fn is_available(&self) -> bool {
        self.inner.available.load(Ordering::Acquire)
    }

    pub fn reason(&self) -> String {
        self.inner
            .reason
            .lock()
            .map(|reason| reason.clone())
            .unwrap_or_else(|_| "Docker availability is unknown".into())
    }
}

impl Default for DockerStatus {
    fn default() -> Self {
        Self::new()
    }
}

pub fn ensure_docker_installed_and_running() -> Result<()> {
    if which::which("docker").is_err() {
        bail!("docker not found in PATH — harness-hat requires Docker to run containers");
    }

    let mut command = std::process::Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
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
