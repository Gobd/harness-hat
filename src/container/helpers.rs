use anyhow::{Context, Result};
use std::env;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerUsageStats {
    pub cpu_percent: String,
    pub memory_usage: String,
}

/// Docker's current lifecycle state for a named container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    pub running: bool,
    pub exit_code: Option<i32>,
    pub error: String,
}

pub(crate) fn read_container_id(
    cidfile: &Path,
    docker_name: &str,
    process_exited: &AtomicBool,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(cidfile) {
            let id = contents.trim().to_string();
            if !id.is_empty() {
                return Ok(id);
            }
        }

        if process_exited.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break;
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    // The cidfile is Docker's primary launch handshake. Inspect once as a
    // race-safe fallback, rather than spawning `docker inspect` hundreds of
    // times on the TUI thread after an early docker-run failure.
    if let Some(id) = inspect_container_id(docker_name)? {
        return Ok(id);
    }

    if process_exited.load(Ordering::Relaxed) {
        anyhow::bail!(
            "docker run exited before creating container {docker_name} or writing {}",
            cidfile.display()
        );
    }

    anyhow::bail!(
        "timed out waiting for docker container {docker_name} or cidfile {}",
        cidfile.display(),
    );
}

fn inspect_container_id(docker_name: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", docker_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .context("running docker inspect")?;

    if !output.status.success() {
        return Ok(None);
    }

    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        Ok(None)
    } else {
        Ok(Some(id))
    }
}

/// Inspect whether a container is still running. A missing container returns
/// `Ok(None)`; callers should not infer an exit merely from a disconnected
/// Docker attach/PTY client.
pub fn inspect_container_state(docker_name: &str) -> Result<Option<ContainerState>> {
    let output = std::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Running}}\t{{.State.ExitCode}}\t{{.State.Error}}",
            docker_name,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .context("running docker inspect")?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(parse_container_state(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_container_state(output: &str) -> Option<ContainerState> {
    let mut parts = output.trim().splitn(3, '\t');
    let running = parts.next()?.trim().parse::<bool>().ok()?;
    let exit_code = parts.next().and_then(|s| s.trim().parse::<i32>().ok());
    let error = parts.next().unwrap_or("").trim().to_string();
    Some(ContainerState {
        running,
        exit_code,
        error,
    })
}

pub fn inspect_container_usage(docker_name: &str) -> Result<Option<ContainerUsageStats>> {
    let output = std::process::Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.CPUPerc}}\t{{.MemUsage}}",
            docker_name,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .context("running docker stats")?;

    if !output.status.success() {
        return Ok(None);
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let line = raw.trim();
    if line.is_empty() {
        return Ok(None);
    }

    let mut parts = line.splitn(2, '\t');
    let cpu_percent = parts.next().unwrap_or("").trim().to_string();
    let memory_usage = parts.next().unwrap_or("").trim().to_string();
    if cpu_percent.is_empty() && memory_usage.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ContainerUsageStats {
            cpu_percent,
            memory_usage,
        }))
    }
}

pub(crate) fn docker_image_exists(image: &str) -> std::io::Result<bool> {
    let status = std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

pub(crate) fn detect_default_colors() -> ((u8, u8, u8), (u8, u8, u8)) {
    parse_colorfgbg(env::var("COLORFGBG").ok().as_deref())
}

fn parse_colorfgbg(colorfgbg: Option<&str>) -> ((u8, u8, u8), (u8, u8, u8)) {
    let fallback = (
        crate::ansi::ansi_16_to_rgb(15),
        crate::ansi::ansi_16_to_rgb(0),
    );
    let Some(val) = colorfgbg else {
        return fallback;
    };
    let parts: Vec<u8> = val
        .split(';')
        .filter_map(|s| s.trim().parse::<u8>().ok())
        .collect();
    if parts.len() < 2 {
        return fallback;
    }
    let fg_idx = parts[parts.len().saturating_sub(2)];
    let bg_idx = parts[parts.len().saturating_sub(1)];
    if fg_idx == bg_idx {
        return fallback;
    }
    let fg = crate::ansi::ansi_16_to_rgb(fg_idx);
    let bg = crate::ansi::ansi_16_to_rgb(bg_idx);
    if fg == bg {
        return fallback;
    }
    (fg, bg)
}

pub(crate) fn xterm_256_index_to_rgb(idx: u8) -> (u8, u8, u8) {
    crate::ansi::xterm_256_to_rgb(idx)
}

pub(crate) fn blend_toward_bg(fg: (u8, u8, u8), bg: (u8, u8, u8), fg_weight: f32) -> (u8, u8, u8) {
    let fg_weight = fg_weight.clamp(0.0, 1.0);
    let bg_weight = 1.0 - fg_weight;
    let blend = |f: u8, b: u8| -> u8 {
        ((f as f32) * fg_weight + (b as f32) * bg_weight)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (blend(fg.0, bg.0), blend(fg.1, bg.1), blend(fg.2, bg.2))
}

pub(crate) fn luma_u8((r, g, b): (u8, u8, u8)) -> u8 {
    let y = 0.2126 * (r as f32) + 0.7152 * (g as f32) + 0.0722 * (b as f32);
    y.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::{ContainerState, parse_container_state};

    #[test]
    fn parse_container_state_preserves_running_containers_with_zero_exit_code() {
        assert_eq!(
            parse_container_state("true\t0\t\n"),
            Some(ContainerState {
                running: true,
                exit_code: Some(0),
                error: String::new(),
            })
        );
    }

    #[test]
    fn parse_container_state_reads_stopped_container_diagnostics() {
        assert_eq!(
            parse_container_state("false\t137\tOOMKilled\n"),
            Some(ContainerState {
                running: false,
                exit_code: Some(137),
                error: "OOMKilled".to_string(),
            })
        );
    }
}
