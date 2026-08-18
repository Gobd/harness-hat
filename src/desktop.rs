//! Claude Desktop attachment for Harness Hat workspaces.
//!
//! The native app remains a host process, but Claude Code connects to the
//! selected Hat container through key-only SSH bound to host loopback. A
//! read-only managed settings file disables Desktop capabilities that would
//! otherwise cross the container boundary.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const AUTHORIZED_KEY_ENV: &str = "HARNESS_HAT_DESKTOP_SSH_AUTHORIZED_KEY";
pub const LABEL_DESKTOP: &str = "dev.harness-hat.desktop";
pub const MANAGED_POLICY_CONTAINER_PATH: &str = "/etc/claude-code/managed-settings.json";
const SSH_CONTAINER_PORT: &str = "2222/tcp";

pub const MANAGED_POLICY: &str = r#"{
  "disableBrowserExternalNavigation": true,
  "browserExternalPageTools": "disabled",
  "disableClaudeAiConnectors": true,
  "deniedMcpServers": [
    { "serverName": "computer-use" },
    { "serverName": "claude-in-chrome" }
  ],
  "permissions": {
    "deny": [
      "mcp__computer-use__*",
      "mcp__claude-in-chrome__*"
    ]
  }
}
"#;

pub fn authorized_key_env(state_dir: &Path) -> Result<(String, String)> {
    let identity = ensure_identity(state_dir)?;
    let public_key = fs::read_to_string(identity.with_extension("pub"))
        .context("reading Harness Hat Claude Desktop SSH public key")?;
    let public_key = public_key.trim();
    anyhow::ensure!(
        public_key.starts_with("ssh-ed25519 ") && !public_key.contains(['\n', '\r']),
        "invalid Harness Hat Claude Desktop SSH public key"
    );
    Ok((AUTHORIZED_KEY_ENV.to_string(), public_key.to_string()))
}

pub fn is_desktop_container(container_name: &str) -> Result<bool> {
    let output = docker_output(&[
        "inspect",
        "-f",
        &format!("{{{{index .Config.Labels {:?}}}}}", LABEL_DESKTOP),
        container_name,
    ])?;
    Ok(output.trim() == "true")
}

pub fn open(
    container_name: &str,
    workspace_name: &str,
    workdir: &str,
    state_dir: &Path,
) -> Result<()> {
    let identity = ensure_identity(state_dir)?;
    let port = published_port(container_name)?;
    let host_key = wait_for_host_key(container_name, Duration::from_secs(10))?;
    let ssh_alias = register_connection(workspace_name, port, &host_key, &identity, state_dir)?;
    launch_app()?;
    println!(
        "Claude Desktop opened. In Code, add or select SSH host \"{ssh_alias}\" and folder \"{workdir}\"."
    );
    println!(
        "Safety boundary: that SSH session runs in container {container_name}; separate Local, Chat, or Cowork sessions are outside Harness Hat."
    );
    Ok(())
}

fn ensure_identity(state_dir: &Path) -> Result<PathBuf> {
    let dir = state_dir.join("claude-desktop");
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating Claude Desktop state directory {}", dir.display()))?;
    set_private_dir_permissions(&dir)?;
    let identity = dir.join("id_ed25519");
    let public_identity = identity.with_extension("pub");
    if identity.exists() && public_identity.exists() {
        return Ok(identity);
    }
    let ssh_keygen = which::which("ssh-keygen")
        .context("ssh-keygen is required for `hat ws --desktop`; install the OpenSSH client")?;
    if identity.exists() {
        let output = Command::new(&ssh_keygen)
            .args(["-y", "-f"])
            .arg(&identity)
            .output()
            .context("recovering Claude Desktop SSH public key")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen could not recover the public key: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let key = String::from_utf8(output.stdout).context("SSH public key was not UTF-8")?;
        fs::write(&public_identity, key)
            .context("writing recovered Claude Desktop SSH public key")?;
        return Ok(identity);
    }
    if public_identity.exists() {
        fs::remove_file(&public_identity)
            .context("removing orphaned Claude Desktop SSH public key")?;
    }
    let output = Command::new(ssh_keygen)
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "harness-hat-desktop",
            "-f",
        ])
        .arg(&identity)
        .output()
        .context("generating Claude Desktop SSH identity")?;
    if !output.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(identity)
}

fn published_port(container_name: &str) -> Result<u16> {
    let output = docker_output(&["port", container_name, SSH_CONTAINER_PORT])?;
    output
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, port)| port.trim()))
        .find_map(|port| port.parse::<u16>().ok())
        .context("Docker did not publish the Claude Desktop SSH port on loopback")
}

fn wait_for_host_key(container_name: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let path = "/home/coder/.ssh/harness-hat-desktop-host-key.pub";
    loop {
        let mut command = Command::new("docker");
        crate::process_util::hide_console_window(&mut command);
        let output = command.args(["exec", container_name, "cat", path]).output();
        if let Ok(output) = output
            && output.status.success()
        {
            let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if key.starts_with("ssh-ed25519 ") {
                return Ok(key);
            }
        }
        if Instant::now() >= deadline {
            bail!("Claude Desktop SSH service did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn register_connection(
    workspace_name: &str,
    port: u16,
    host_key: &str,
    identity: &Path,
    state_dir: &Path,
) -> Result<String> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).with_context(|| format!("creating {}", ssh_dir.display()))?;
    set_private_dir_permissions(&ssh_dir)?;
    register_connection_with_user_config(
        workspace_name,
        port,
        host_key,
        identity,
        state_dir,
        &ssh_dir.join("config"),
    )
}

fn register_connection_with_user_config(
    workspace_name: &str,
    port: u16,
    host_key: &str,
    identity: &Path,
    state_dir: &Path,
    user_config: &Path,
) -> Result<String> {
    let ssh_alias = workspace_ssh_alias(workspace_name);
    let desktop_dir = state_dir.join("claude-desktop");
    let config_dir = desktop_dir.join("ssh-config.d");
    let known_hosts_dir = desktop_dir.join("known-hosts");
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating {}", config_dir.display()))?;
    fs::create_dir_all(&known_hosts_dir)
        .with_context(|| format!("creating {}", known_hosts_dir.display()))?;
    set_private_dir_permissions(&config_dir)?;
    set_private_dir_permissions(&known_hosts_dir)?;

    let key_fields = host_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let known_hosts = known_hosts_dir.join(&ssh_alias);
    crate::config::atomic_write_with_lock(
        &known_hosts,
        format!("{ssh_alias} {key_fields}\n").as_bytes(),
    )
    .with_context(|| format!("writing {}", known_hosts.display()))?;

    let config_path = config_dir.join(format!("{ssh_alias}.conf"));
    let connection = format!(
        "Host {ssh_alias}\n  HostName 127.0.0.1\n  Port {port}\n  User coder\n  IdentityFile {}\n  IdentitiesOnly yes\n  HostKeyAlias {ssh_alias}\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  ForwardAgent no\n  ForwardX11 no\n",
        ssh_path(identity)?,
        ssh_path(&known_hosts)?,
    );
    crate::config::atomic_write_with_lock(&config_path, connection.as_bytes())
        .with_context(|| format!("writing {}", config_path.display()))?;
    ensure_user_ssh_include(user_config, &config_dir)?;
    Ok(ssh_alias)
}

fn ensure_user_ssh_include(path: &Path, config_dir: &Path) -> Result<()> {
    let existed = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    let existing = fs::read_to_string(path).unwrap_or_default();
    let include_pattern = config_dir.join("*");
    let line = format!("Include {}", ssh_path(&include_pattern)?);
    if !existing.lines().any(|existing| existing.trim() == line) {
        if !existing.is_empty() && !existing.ends_with('\n') {
            writeln!(file)?;
        }
        writeln!(file, "{line}")?;
        file.sync_all()?;
    }
    if !existed {
        set_private_file_permissions(path)?;
    }
    Ok(())
}

fn workspace_ssh_alias(workspace_name: &str) -> String {
    use sha2::{Digest, Sha256};
    let slug = workspace_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "workspace" } else { slug };
    let slug = &slug[..slug.len().min(40)];
    let digest = hex::encode(Sha256::digest(workspace_name.as_bytes()));
    format!("hat-{slug}-{}", &digest[..8])
}

fn ssh_path(path: &Path) -> Result<String> {
    let rendered = path.to_string_lossy().replace('\\', "/");
    anyhow::ensure!(
        !rendered.contains(['\n', '\r', '"']),
        "SSH path contains unsupported characters: {}",
        path.display()
    );
    Ok(format!("\"{rendered}\""))
}

fn docker_output(args: &[&str]) -> Result<String> {
    let mut command = Command::new("docker");
    crate::process_util::hide_console_window(&mut command);
    let output = command
        .args(args)
        .output()
        .context("running docker command")?;
    if !output.status.success() {
        bail!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn launch_app() -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Claude"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("launching Claude Desktop")?;
    anyhow::ensure!(
        status.success(),
        "Claude Desktop is not installed; install it and retry"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_app() -> Result<()> {
    let local_app_data =
        dirs::data_local_dir().context("cannot determine local app data directory")?;
    let candidates = [
        local_app_data.join("AnthropicClaude/Claude.exe"),
        local_app_data.join("Programs/Claude/Claude.exe"),
        local_app_data.join("Claude/Claude.exe"),
    ];
    let executable = candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| which::which("Claude.exe").ok())
        .context("Claude Desktop is not installed; install it and retry")?;
    let mut command = Command::new(executable);
    crate::process_util::hide_console_window(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching Claude Desktop")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launch_app() -> Result<()> {
    let _ = Stdio::null();
    bail!("Claude Desktop launching is currently supported on macOS and Windows")
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn managed_policy_disables_host_escape_tools() {
        let policy: Value = serde_json::from_str(MANAGED_POLICY).unwrap();
        assert_eq!(policy["disableBrowserExternalNavigation"], true);
        assert_eq!(policy["browserExternalPageTools"], "disabled");
        assert_eq!(policy["disableClaudeAiConnectors"], true);
        let deny = policy["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|value| value == "mcp__computer-use__*"));
        assert!(deny.iter().any(|value| value == "mcp__claude-in-chrome__*"));
    }

    #[test]
    fn workspace_alias_is_stable_safe_and_collision_resistant() {
        let first = workspace_ssh_alias("My Project");
        assert!(first.starts_with("hat-my-project-"));
        assert_eq!(first, workspace_ssh_alias("My Project"));
        assert_ne!(first, workspace_ssh_alias("my_project"));
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        );
    }

    #[test]
    fn repeated_registration_replaces_workspace_files_and_adds_one_include() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let identity = state.join("claude-desktop/id_ed25519");
        fs::create_dir_all(identity.parent().unwrap()).unwrap();
        fs::write(&identity, "test identity").unwrap();
        let user_config = root.path().join("home/.ssh/config");
        fs::create_dir_all(user_config.parent().unwrap()).unwrap();

        let alias = register_connection_with_user_config(
            "My Project",
            41001,
            "ssh-ed25519 AAAAfirst comment",
            &identity,
            &state,
            &user_config,
        )
        .unwrap();
        register_connection_with_user_config(
            "My Project",
            41002,
            "ssh-ed25519 AAAAsecond comment",
            &identity,
            &state,
            &user_config,
        )
        .unwrap();

        let include = fs::read_to_string(&user_config).unwrap();
        assert_eq!(
            include
                .lines()
                .filter(|line| line.starts_with("Include "))
                .count(),
            1
        );
        let config =
            fs::read_to_string(state.join(format!("claude-desktop/ssh-config.d/{alias}.conf")))
                .unwrap();
        assert!(config.contains("Port 41002"));
        assert!(!config.contains("Port 41001"));
        let known_host =
            fs::read_to_string(state.join(format!("claude-desktop/known-hosts/{alias}"))).unwrap();
        assert!(known_host.contains("AAAAsecond"));
        assert!(!known_host.contains("AAAAfirst"));
    }
}
