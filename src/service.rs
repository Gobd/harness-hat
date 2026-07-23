//! Per-user desktop-agent installation.
//!
//! The installed process is deliberately a *user-session* agent, not a system
//! daemon: Harness Hat needs access to Docker Desktop/the user's Docker socket
//! and must be able to display approval dialogs. The generated definitions
//! therefore start at graphical login on every supported platform.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;

#[cfg(any(target_os = "macos", test))]
const LABEL: &str = "com.harness-hat.manager";
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT: &str = "harness-hat.service";
#[cfg(target_os = "windows")]
const WINDOWS_TASK: &str = "Harness Hat";

pub fn install(explicit_config: Option<PathBuf>) -> Result<()> {
    ensure_desktop_user()?;
    let (config_path, created_config) = resolve_config_path(explicit_config)?;
    if created_config {
        println!("Created default global config: {}", config_path.display());
    }
    // Validate before making a persistent startup change. In particular this
    // catches an invalid Docker directory rather than creating a restart loop.
    crate::config::load(&config_path)?;
    let executable = std::env::current_exe()
        .context("locating the current hht executable")?
        .canonicalize()
        .context("canonicalizing the current hht executable")?;

    #[cfg(target_os = "macos")]
    install_macos(&executable, &config_path)?;
    #[cfg(target_os = "linux")]
    install_linux(&executable, &config_path)?;
    #[cfg(target_os = "windows")]
    install_windows(&executable, &config_path)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("hht install supports macOS, Linux with systemd, and Windows only");

    println!("Harness Hat background agent installed for this desktop user.");
    println!("Config: {}", config_path.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    ensure_desktop_user()?;
    #[cfg(target_os = "macos")]
    uninstall_macos()?;
    #[cfg(target_os = "linux")]
    uninstall_linux()?;
    #[cfg(target_os = "windows")]
    uninstall_windows()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("hht uninstall supports macOS, Linux with systemd, and Windows only");

    println!("Harness Hat background agent removed for this desktop user.");
    Ok(())
}

fn ensure_desktop_user() -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        bail!("hht install/uninstall must run as the signed-in desktop user; do not use sudo");
    }
    Ok(())
}

fn resolve_config_path(explicit_config: Option<PathBuf>) -> Result<(PathBuf, bool)> {
    let (path, created) = match explicit_config {
        Some(path) => (path, false),
        None => {
            let path = crate::manager::default_home_config_path()?;
            let created = ensure_default_config(&path)?;
            (path, created)
        }
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalizing config path {}", path.display()))?;
    Ok((path, created))
}

fn ensure_default_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    crate::init::write_sample_config(path)
        .with_context(|| format!("creating default global config at {}", path.display()))?;
    Ok(true)
}

fn write_definition(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("service definition has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating service directory {}", parent.display()))?;
    crate::config::atomic_write_with_lock(path, contents.as_bytes())
        .with_context(|| format!("writing service definition {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting service definition {}", path.display()))?;
    }
    Ok(())
}

fn command_status(program: &str, args: &[String], action: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {action}: {program}"))?;
    if !status.success() {
        bail!("{action} failed with status {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn install_macos(executable: &Path, config_path: &Path) -> Result<()> {
    let path = launch_agent_path()?;
    write_definition(&path, &render_launchd_plist(executable, config_path))?;
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    command_status(
        "launchctl",
        &[
            "bootstrap".into(),
            domain.clone(),
            path.display().to_string(),
        ],
        "loading Harness Hat launch agent",
    )?;
    command_status(
        "launchctl",
        &["kickstart".into(), "-k".into(), format!("{domain}/{LABEL}")],
        "starting Harness Hat launch agent",
    )
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<()> {
    let path = launch_agent_path()?;
    if !path.exists() {
        return Ok(());
    }
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::fs::remove_file(&path).with_context(|| format!("removing launch agent {}", path.display()))
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(".config/systemd/user").join(SYSTEMD_UNIT))
}

#[cfg(target_os = "linux")]
fn install_linux(executable: &Path, config_path: &Path) -> Result<()> {
    let path = systemd_unit_path()?;
    write_definition(&path, &render_systemd_unit(executable, config_path))?;
    // A user manager commonly inherits these at desktop login. Import them at
    // install time as well so native approval dialogs can reach the current
    // graphical session immediately; service startup remains fail-closed if a
    // desktop session is not available.
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "import-environment",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
        ])
        .status();
    command_status(
        "systemctl",
        &["--user".into(), "daemon-reload".into()],
        "reloading systemd user units",
    )?;
    command_status(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            SYSTEMD_UNIT.into(),
        ],
        "enabling Harness Hat user service",
    )
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<()> {
    let path = systemd_unit_path()?;
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT])
        .status();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing systemd user unit {}", path.display()))?;
    }
    command_status(
        "systemctl",
        &["--user".into(), "daemon-reload".into()],
        "reloading systemd user units",
    )
}

#[cfg(target_os = "windows")]
fn install_windows(executable: &Path, config_path: &Path) -> Result<()> {
    let task_command = format!(
        "\"{}\" --config \"{}\" __service",
        executable.display(),
        config_path.display()
    );
    command_status(
        "schtasks",
        &[
            "/Create".into(),
            "/TN".into(),
            WINDOWS_TASK.into(),
            "/TR".into(),
            task_command,
            "/SC".into(),
            "ONLOGON".into(),
            "/RL".into(),
            "LIMITED".into(),
            "/IT".into(),
            "/F".into(),
        ],
        "creating Harness Hat scheduled task",
    )?;
    command_status(
        "schtasks",
        &["/Run".into(), "/TN".into(), WINDOWS_TASK.into()],
        "starting Harness Hat scheduled task",
    )
}

#[cfg(target_os = "windows")]
fn uninstall_windows() -> Result<()> {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", WINDOWS_TASK, "/F"])
        .status();
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn render_systemd_unit(executable: &Path, config_path: &Path) -> String {
    format!(
        "[Unit]\nDescription=Harness Hat background agent\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart={} --config {} __service\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(executable),
        systemd_escape(config_path),
    )
}

#[cfg(any(target_os = "macos", test))]
fn render_launchd_plist(executable: &Path, config_path: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string><string>__service</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>ProcessType</key><string>Interactive</string>\n</dict></plist>\n",
        xml_escape(&executable.display().to_string()),
        xml_escape(&config_path.display().to_string()),
    )
}

#[cfg(any(target_os = "linux", test))]
fn systemd_escape(path: &Path) -> String {
    // systemd's ExecStart parser accepts C-style double-quoted arguments.
    let escaped = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_paths_and_uses_internal_service_mode() {
        let unit = render_systemd_unit(
            Path::new("/home/me/.cargo/bin/hht"),
            Path::new("/home/me/My Config/harness-hat.toml"),
        );
        assert!(unit.contains("ExecStart=\"/home/me/.cargo/bin/hht\" --config \"/home/me/My Config/harness-hat.toml\" __service"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_unit_escapes_specifier_characters() {
        let unit = render_systemd_unit(Path::new("/home/me/hht%stable"), Path::new("/tmp/x"));
        assert!(unit.contains("hht%%stable"));
    }

    #[test]
    fn launchd_plist_escapes_arguments() {
        let plist = render_launchd_plist(
            Path::new("/Applications/Harness & Hat/hht"),
            Path::new("/Users/me/rules & config.toml"),
        );
        assert!(plist.contains("Harness &amp; Hat"));
        assert!(plist.contains("rules &amp; config.toml"));
        assert!(plist.contains("<string>__service</string>"));
    }

    #[test]
    fn missing_default_config_is_created_without_replacing_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("harness-hat.toml");

        assert!(ensure_default_config(&config_path).unwrap());
        assert!(config_path.is_file());
        assert!(!ensure_default_config(&config_path).unwrap());
    }
}
