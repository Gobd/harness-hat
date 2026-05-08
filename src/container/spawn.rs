use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Instant;
use tempfile::NamedTempFile;
use tracing::info;
use tracing::instrument;

use crate::config::{AgentKind, ContainerDef, ContainerMount};
use crate::container::core::{
    TermSize, append_codex_home_args, append_gemini_home_args,
    append_missing_gemini_home_mount_args, extract_claude_keychain_credential,
    find_codex_home_container_path, find_gemini_home_container_path, find_gemini_home_mount,
    loopback_to_host_docker, mount_mode_arg, mounts_include_codex_session_state,
    mounts_include_gemini_session_state, read_claude_setup_token, sanitize_docker_name,
};
use crate::container::helpers::detect_default_colors;
use crate::container::{ContainerSession, SessionEventProxy, compose_no_proxy, read_container_id};
use crate::fs_util::write_env_file_entry;

const PRIMARY_PROXY_CONN_LIMIT: usize = 0;
const SUBAGENT_PROXY_CONN_LIMIT: usize = 128;
const AGENT_CONFIG_SNAPSHOT_ROOT: &str = "/tmp/harness-hat-agent-config";
const AGENT_CONFIG_SNAPSHOT_MANIFEST: &str = "/tmp/harness-hat-agent-config/manifest.tsv";
const HARNESS_HAT_CA_CERT_PATH: &str = "/usr/local/share/ca-certificates/harness-hat-ca.crt";
const HARNESS_HAT_CA_BUNDLE_PATH: &str = "/tmp/harness-hat-ca-bundle.crt";

struct AgentConfigSnapshot {
    tempdir: tempfile::TempDir,
    targets: Vec<PathBuf>,
}

/// Launch `docker run` for a container definition and wire it to a PTY-backed
/// terminal session.
#[instrument(skip(
    ctr,
    command_argv,
    workspace_path,
    codex_home_host_path,
    gemini_home_host_path,
    extra_env,
    scoped_proxy
))]
pub fn spawn(
    ctr: &ContainerDef,
    command_argv: Option<&[String]>,
    project_name: &str,
    workspace_path: &Path,
    codex_home_host_path: Option<&Path>,
    gemini_home_host_path: Option<&Path>,
    session_token: &str,
    token: &str,
    exec_url: &str,
    proxy_url: &str,
    ca_cert_host_path: &str,
    hostdo_script_host_path: Option<&Path>,
    scoped_proxy: Option<crate::proxy::ScopedProxyListener>,
    proxy_priority: crate::proxy::SourcePriority,
    strict_network: bool,
    extra_env: &[(String, String)],
    rows: u16,
    cols: u16,
) -> Result<(ContainerSession, Vec<String>)> {
    let ca_env_path = HARNESS_HAT_CA_CERT_PATH;
    let no_proxy = if strict_network {
        compose_no_proxy(&[])
    } else {
        compose_no_proxy(&ctr.bypass_proxy)
    };
    let mount_str = ctr.mount_target.display().to_string();

    let cidfile =
        std::env::temp_dir().join(format!("harness-hat-cid-{}.txt", uuid::Uuid::new_v4()));
    let docker_run_name = format!(
        "harness-hat-{}-{}",
        sanitize_docker_name(&ctr.name),
        uuid::Uuid::new_v4().simple()
    );

    let container_exec_url = loopback_to_host_docker(exec_url);
    let container_proxy_url = loopback_to_host_docker(proxy_url);
    let container_proxy_addr = proxy_addr_without_auth(&container_proxy_url);
    let scoped_proxy_auth = scoped_proxy
        .as_ref()
        .map(|proxy| proxy.proxy_auth_token().to_string())
        .unwrap_or_default();
    let mut launch_notes = Vec::new();
    let subagent_launch = proxy_priority == crate::proxy::SourcePriority::Subagent;

    let mut docker_args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        docker_run_name.clone(),
        "--cidfile".to_string(),
        cidfile.display().to_string(),
    ];

    #[cfg(target_os = "linux")]
    docker_args.push("--add-host=host.docker.internal:host-gateway".to_string());

    #[cfg(target_os = "linux")]
    if !strict_network {
        docker_args.extend_from_slice(&["--user".to_string(), "1000:1000".to_string()]);
    }

    if strict_network {
        docker_args.extend_from_slice(&["--user".to_string(), "0:0".to_string()]);
        #[cfg(target_os = "macos")]
        {
            // Docker Desktop exposes `/dev/net/tun` for strict mode only when the
            // container is privileged.
            docker_args.push("--privileged".to_string());
        }

        #[cfg(target_os = "linux")]
        {
            docker_args.extend_from_slice(&["--cap-add".to_string(), "NET_ADMIN".to_string()]);
            if Path::new("/dev/net/tun").exists() {
                docker_args.extend_from_slice(&[
                    "--device".to_string(),
                    "/dev/net/tun:/dev/net/tun".to_string(),
                ]);
            } else {
                anyhow::bail!(
                    "Strict network mode requires /dev/net/tun on the host. Cannot safely fallback to --privileged."
                );
            }
        }
    }

    docker_args.extend_from_slice(&[
        "-v".to_string(),
        format!("{}:{}:rw", workspace_path.display(), mount_str),
        "-v".to_string(),
        format!("{ca_cert_host_path}:{ca_env_path}:ro"),
        "-w".to_string(),
        mount_str.clone(),
    ]);

    let hostdo_tempfile = match hostdo_script_host_path {
        Some(path) => Some(prepare_executable_helper_script(
            path,
            "harness-hat-hostdo-",
        )?),
        None => None,
    };
    if let Some(hostdo) = hostdo_tempfile.as_ref() {
        docker_args.extend_from_slice(&[
            "-v".to_string(),
            format!("{}:/usr/local/bin/hostdo:ro", hostdo.path().display()),
        ]);
    }

    // Prepare secure env file to prevent token leakage via `ps`
    let mut env_file = tempfile::Builder::new()
        .prefix("harness-hat-env-")
        .tempfile()
        .context("failed to create temp env file")?;

    write_ca_env_entries(&mut env_file)?;
    for (key, value) in extra_env {
        write_env_file_entry(&mut env_file, key, value)?;
    }

    write_env_file_entry(&mut env_file, "HARNESS_HAT_TOKEN", token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_SESSION_TOKEN", session_token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_PROJECT", project_name)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_MOUNT_TARGET", &mount_str)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_URL", &container_exec_url)?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_STRICT_NETWORK",
        if strict_network { "1" } else { "0" },
    )?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_SCOPED_PROXY_ADDR",
        &container_proxy_addr,
    )?;
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_SCOPED_PROXY_AUTH",
        &scoped_proxy_auth,
    )?;
    let proxy_conn_limit = match proxy_priority {
        crate::proxy::SourcePriority::Primary => PRIMARY_PROXY_CONN_LIMIT,
        crate::proxy::SourcePriority::Subagent => SUBAGENT_PROXY_CONN_LIMIT,
    };
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_PROXY_CONN_LIMIT",
        &proxy_conn_limit.to_string(),
    )?;
    if !strict_network {
        write_env_file_entry(&mut env_file, "HTTP_PROXY", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "HTTPS_PROXY", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "ALL_PROXY", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "NO_PROXY", &no_proxy)?;
        write_env_file_entry(&mut env_file, "http_proxy", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "https_proxy", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "all_proxy", &container_proxy_url)?;
        write_env_file_entry(&mut env_file, "no_proxy", &no_proxy)?;
    }

    let (effective_mounts, agent_config_snapshot) = if subagent_launch {
        let (mounts, snapshot) = prepare_subagent_agent_config_snapshot(&ctr.agent, &ctr.mounts)?;
        if let Some(snapshot) = snapshot.as_ref() {
            docker_args.push("-v".to_string());
            docker_args.push(format!(
                "{}:{AGENT_CONFIG_SNAPSHOT_ROOT}:ro",
                snapshot.tempdir.path().display()
            ));
            write_env_file_entry(
                &mut env_file,
                "HARNESS_HAT_AGENT_CONFIG_SNAPSHOT",
                AGENT_CONFIG_SNAPSHOT_MANIFEST,
            )?;
            let note = format!(
                "{} config copied into an isolated subagent home from {} host path(s)",
                agent_label(&ctr.agent),
                snapshot.targets.len()
            );
            info!("{note}");
            launch_notes.push(note);
        }
        (mounts, snapshot)
    } else {
        (ctr.mounts.clone(), None)
    };

    docker_args.push("--env-file".to_string());
    docker_args.push(env_file.path().display().to_string());

    if ctr.agent == AgentKind::Codex && !ctr.env_passthrough.iter().any(|v| v == "CODEX_HOME") {
        // Prefer a real host-mounted Codex home when the container already has
        // one; otherwise create a project-scoped cache directory so sessions
        // survive container restarts without leaking across projects.
        if subagent_launch {
            docker_args.push("-e".to_string());
            docker_args.push("CODEX_HOME=/home/ubuntu/.codex".to_string());
        } else if let Some(container_codex_home) = find_codex_home_container_path(&effective_mounts)
        {
            let note = format!(
                "Codex session data imported from mounted CODEX_HOME at {}",
                container_codex_home.display()
            );
            info!("{note}");
            launch_notes.push(note);
            docker_args.push("-e".to_string());
            docker_args.push(format!("CODEX_HOME={}", container_codex_home.display()));
        } else if mounts_include_codex_session_state(&effective_mounts) {
            let note =
                "Codex session data is already mounted in the container; leaving existing Codex state paths untouched"
                    .to_string();
            info!("{note}");
            launch_notes.push(note);
        } else if let Some(host_path) = codex_home_host_path {
            // No existing host-state mounts — use per-project persistence.
            let note = format!(
                "Codex session data imported from host cache at {}",
                host_path.join(".codex").display()
            );
            info!("{note}");
            launch_notes.push(note);
            append_codex_home_args(&mut docker_args, host_path)?;
        }
    }

    if ctr.agent == AgentKind::Gemini {
        if subagent_launch {
            // Subagents receive a copied snapshot instead of shared Gemini state mounts.
        } else if let Some((host_gemini_home, mode)) = find_gemini_home_mount(&effective_mounts) {
            let container_gemini_home = find_gemini_home_container_path(&effective_mounts)
                .unwrap_or(Path::new("/home/ubuntu/.gemini"));
            let note = format!(
                "Gemini session data imported from mounted .gemini at {}",
                container_gemini_home.display()
            );
            info!("{note}");
            launch_notes.push(note);
            append_missing_gemini_home_mount_args(
                &mut docker_args,
                &effective_mounts,
                host_gemini_home,
                mode,
            );
        } else if mounts_include_gemini_session_state(&effective_mounts) {
            let note =
                "Gemini session data is already mounted in the container; leaving existing Gemini state paths untouched"
                    .to_string();
            info!("{note}");
            launch_notes.push(note);
        } else if let Some(host_path) = gemini_home_host_path {
            let note = format!(
                "Gemini session data imported from host cache at {}",
                host_path.join(".gemini").display()
            );
            info!("{note}");
            launch_notes.push(note);
            append_gemini_home_args(&mut docker_args, host_path)?;
        }
    }

    for mount in &effective_mounts {
        if ctr.agent == crate::config::AgentKind::Claude {
            if mount.container == PathBuf::from("/home/ubuntu/.claude.json") {
                if let Ok(meta) = std::fs::metadata(&mount.host) {
                    if meta.is_dir() {
                        anyhow::bail!(
                            "invalid Claude mount: host path '{}' is a directory, but '{}' must be a file; fix by replacing ~/.claude.json with the credential file",
                            mount.host.display(),
                            mount.container.display()
                        );
                    } else {
                        let note = format!(
                            "Claude session data imported via mount {} -> {}",
                            mount.host.display(),
                            mount.container.display()
                        );
                        info!("{note}");
                        launch_notes.push(note);
                    }
                }
            }
            if mount.container == PathBuf::from("/home/ubuntu/.claude") {
                if let Ok(meta) = std::fs::metadata(&mount.host) {
                    if meta.is_file() {
                        anyhow::bail!(
                            "invalid Claude mount: host path '{}' is a file, but '{}' must be a directory",
                            mount.host.display(),
                            mount.container.display()
                        );
                    } else {
                        let note = format!(
                            "Claude session data imported via mount {} -> {}",
                            mount.host.display(),
                            mount.container.display()
                        );
                        info!("{note}");
                        launch_notes.push(note);
                    }
                }
            }
        }
        docker_args.push("-v".to_string());
        docker_args.push(format!(
            "{}:{}:{}",
            mount.host.display(),
            mount.container.display(),
            mount_mode_arg(&mount.mode),
        ));
    }

    for name in &ctr.env_passthrough {
        if extra_env.iter().any(|(key, _)| key == name) {
            continue;
        }
        docker_args.push("-e".to_string());
        docker_args.push(name.to_string());
    }

    let mut _cred_tempfile = None;
    if ctr.agent == crate::config::AgentKind::Claude {
        if let Some((setup_token, source)) = read_claude_setup_token() {
            let note = format!(
                "Claude session data imported from {:?} and exported as CLAUDE_CODE_OAUTH_TOKEN",
                source
            );
            info!("{note}");
            launch_notes.push(note);
            write_env_file_entry(&mut env_file, "CLAUDE_CODE_OAUTH_TOKEN", &setup_token)?;
        } else if let Some(cred_json) = extract_claude_keychain_credential() {
            let access_token: Option<String> =
                serde_json::from_str::<serde_json::Value>(&cred_json)
                    .ok()
                    .and_then(|v| {
                        v.get("claudeAiOauth")?
                            .get("accessToken")?
                            .as_str()
                            .map(String::from)
                    });

            if let Some(ref tok) = access_token {
                let note = "Claude session data imported from the macOS keychain credential and exported as CLAUDE_CODE_OAUTH_TOKEN".to_string();
                info!("{note}");
                launch_notes.push(note);
                write_env_file_entry(&mut env_file, "CLAUDE_CODE_OAUTH_TOKEN", tok)?;
            }

            let staging_path = "/tmp/.harness-hat-claude-credentials.json";
            let mut tf = tempfile::Builder::new()
                .prefix("harness-hat-claude-cred-")
                .suffix(".json")
                .tempfile()
                .context("creating Claude credential temp file")?;
            tf.write_all(cred_json.as_bytes())
                .context("writing Claude credential temp file")?;
            tf.flush().context("flushing Claude credential temp file")?;
            let host_path = tf.path().display().to_string();
            docker_args.push("-v".to_string());
            docker_args.push(format!("{host_path}:{staging_path}:ro"));
            _cred_tempfile = Some(tf);
        }
    }

    env_file.flush().context("flushing container env file")?;

    docker_args.push(ctr.image.clone());
    if let Some(argv) = command_argv {
        docker_args.extend(argv.iter().cloned());
    }

    info!(
        "launching container: docker {}",
        docker_args
            .iter()
            .map(|a| if a.contains(' ') || a.contains('=') {
                format!("'{a}'")
            } else {
                a.clone()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );

    let (fg, bg) = detect_default_colors();
    let default_fg = alacritty_terminal::vte::ansi::Rgb {
        r: fg.0,
        g: fg.1,
        b: fg.2,
    };
    let default_bg = alacritty_terminal::vte::ansi::Rgb {
        r: bg.0,
        g: bg.1,
        b: bg.2,
    };

    let window_size = WindowSize {
        num_lines: rows,
        num_cols: cols,
        cell_width: 0,
        cell_height: 0,
    };
    let window_size_arc = Arc::new(Mutex::new(window_size));

    let exited = Arc::new(AtomicBool::new(false));
    let has_bell = Arc::new(AtomicBool::new(false));

    let proxy = SessionEventProxy {
        sender: Arc::new(Mutex::new(None)),
        window_size: Arc::clone(&window_size_arc),
        exited: Arc::clone(&exited),
        has_bell: Arc::clone(&has_bell),
        default_fg,
        default_bg,
        grayscale_palette: ctr.agent == crate::config::AgentKind::Codex,
    };

    let mut term_cfg = TermConfig::default();
    term_cfg.scrolling_history = 50_000;
    let term_size = TermSize {
        cols: cols as usize,
        lines: rows as usize,
    };
    let term = Arc::new(FairMutex::new(Term::new(
        term_cfg,
        &term_size,
        proxy.clone(),
    )));

    let mut options = tty::Options::default();
    options.shell = Some(tty::Shell::new("docker".to_string(), docker_args));
    options.working_directory = None;
    options.drain_on_exit = false;
    options.env = HashMap::new();

    let pty = tty::new(&options, window_size, 0).context("open PTY")?;
    let event_loop = EventLoop::new(Arc::clone(&term), proxy.clone(), pty, false, false)
        .context("event loop")?;
    let sender = event_loop.channel();
    let notifier = Notifier(sender.clone());
    if let Ok(mut s) = proxy.sender.lock() {
        *s = Some(sender);
    }
    let _handle = event_loop.spawn();

    let container_id =
        read_container_id(&cidfile, &docker_run_name).context("reading docker container id")?;
    let docker_name = docker_run_name.clone();
    let _ = std::fs::remove_file(&cidfile);

    Ok((
        ContainerSession {
            container_name: ctr.name.clone(),
            agent_kind: ctr.agent.clone(),
            container_id,
            docker_name,
            project: project_name.to_owned(),
            session_token: session_token.to_string(),
            parent_session_token: None,
            subagent_name: None,
            mount_target: mount_str,
            launched_at: Instant::now(),
            terminal_snapshot_hash: 0,
            terminal_changed_at: Instant::now(),
            last_input_at: Arc::new(Mutex::new(None)),
            term,
            notifier,
            window_size: window_size_arc,
            exited,
            has_bell,
            exit_reported: false,
            _scoped_proxy: scoped_proxy,
            _cred_tempfile,
            _env_tempfile: Some(env_file),
            _hostdo_tempfile: hostdo_tempfile,
            _agent_config_tempdir: agent_config_snapshot.map(|snapshot| snapshot.tempdir),
        },
        launch_notes,
    ))
}

fn write_ca_env_entries<W: Write>(env_file: &mut W) -> Result<()> {
    let ca_bundle_env_vars = [
        "CODEX_CA_CERTIFICATE",
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "DENO_CERT",
        "REQUESTS_CA_BUNDLE",
        "AWS_CA_BUNDLE",
        "GIT_SSL_CAINFO",
        "GRPC_DEFAULT_SSL_ROOTS_FILE_PATH",
    ];
    for var in ca_bundle_env_vars {
        write_env_file_entry(env_file, var, HARNESS_HAT_CA_BUNDLE_PATH)?;
    }
    write_env_file_entry(env_file, "NODE_EXTRA_CA_CERTS", HARNESS_HAT_CA_CERT_PATH)?;
    Ok(())
}

fn prepare_subagent_agent_config_snapshot(
    agent: &AgentKind,
    mounts: &[ContainerMount],
) -> Result<(Vec<ContainerMount>, Option<AgentConfigSnapshot>)> {
    let mut effective_mounts = Vec::with_capacity(mounts.len());
    let mut mappings: Vec<(String, PathBuf)> = Vec::new();
    let tempdir = tempfile::Builder::new()
        .prefix("harness-hat-agent-config-")
        .tempdir()
        .context("creating subagent config snapshot directory")?;

    for (idx, mount) in mounts.iter().enumerate() {
        if !is_agent_config_mount(agent, &mount.container) {
            effective_mounts.push(mount.clone());
            continue;
        }

        if !mount.host.exists() {
            continue;
        }

        let staged_name = format!("item-{idx}");
        let staged_path = tempdir.path().join(&staged_name);
        copy_agent_config_path(agent, &mount.host, &staged_path)
            .with_context(|| format!("copying agent config from {}", mount.host.display()))?;
        mappings.push((staged_name.clone(), mount.container.clone()));

        for extra_target in mirrored_agent_config_targets(agent, &mount.container) {
            if !mappings.iter().any(|(_, target)| target == &extra_target) {
                mappings.push((staged_name.clone(), extra_target));
            }
        }
    }

    if mappings.is_empty() {
        return Ok((effective_mounts, None));
    }

    let manifest_path = tempdir.path().join("manifest.tsv");
    let mut manifest = std::fs::File::create(&manifest_path)
        .with_context(|| format!("creating {}", manifest_path.display()))?;
    for (staged_name, container_target) in &mappings {
        writeln!(manifest, "{}\t{}", staged_name, container_target.display())
            .context("writing subagent config manifest")?;
    }
    manifest
        .flush()
        .context("flushing subagent config manifest")?;

    let targets = mappings
        .into_iter()
        .map(|(_, target)| target)
        .collect::<Vec<_>>();
    Ok((
        effective_mounts,
        Some(AgentConfigSnapshot { tempdir, targets }),
    ))
}

fn agent_label(agent: &AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "Claude",
        AgentKind::Codex => "Codex",
        AgentKind::Gemini => "Gemini",
        AgentKind::Opencode => "opencode",
        AgentKind::None => "Agent",
    }
}

fn is_agent_config_mount(agent: &AgentKind, container_path: &Path) -> bool {
    match agent {
        AgentKind::Claude => matches_agent_path(
            container_path,
            &[
                "/home/ubuntu/.claude.json",
                "/home/ubuntu/.claude",
                "/root/.claude.json",
                "/root/.claude",
            ],
        ),
        AgentKind::Codex => matches_agent_path(
            container_path,
            &[
                "/home/ubuntu/.codex",
                "/home/ubuntu/.config/codex",
                "/root/.codex",
                "/root/.config/codex",
            ],
        ),
        AgentKind::Gemini => matches_agent_path(
            container_path,
            &[
                "/home/ubuntu/.gemini",
                "/home/ubuntu/.config/gemini",
                "/root/.gemini",
                "/root/.config/gemini",
            ],
        ),
        AgentKind::Opencode => matches_agent_path(
            container_path,
            &[
                "/home/ubuntu/.opencode",
                "/home/ubuntu/.config/opencode",
                "/root/.opencode",
                "/root/.config/opencode",
            ],
        ),
        AgentKind::None => false,
    }
}

fn matches_agent_path(path: &Path, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| path == Path::new(candidate))
}

fn mirrored_agent_config_targets(agent: &AgentKind, target: &Path) -> Vec<PathBuf> {
    if *agent != AgentKind::Gemini {
        return Vec::new();
    }
    match target.to_str() {
        Some("/home/ubuntu/.gemini") => vec![PathBuf::from("/root/.gemini")],
        Some("/root/.gemini") => vec![PathBuf::from("/home/ubuntu/.gemini")],
        _ => Vec::new(),
    }
}

fn copy_agent_config_path(agent: &AgentKind, source: &Path, dest: &Path) -> Result<()> {
    let meta = std::fs::metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest)
            .with_context(|| format!("creating config snapshot dir {}", dest.display()))?;
        copy_agent_config_dir(agent, source, dest, Path::new(""))
    } else {
        copy_agent_config_file(source, dest)
    }
}

fn copy_agent_config_dir(
    agent: &AgentKind,
    source: &Path,
    dest: &Path,
    relative: &Path,
) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry in {}", source.display()))?;
        let name = entry.file_name();
        let relative_path = relative.join(&name);
        let src = entry.path();
        let meta = std::fs::metadata(&src)
            .with_context(|| format!("reading metadata for {}", src.display()))?;
        if should_skip_agent_config_entry(agent, &relative_path, meta.is_dir()) {
            continue;
        }
        let dst = dest.join(&name);
        if meta.is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("creating config snapshot dir {}", dst.display()))?;
            copy_agent_config_dir(agent, &src, &dst, &relative_path)?;
        } else if meta.is_file() {
            copy_agent_config_file(&src, &dst)?;
        }
    }
    Ok(())
}

fn copy_agent_config_file(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config snapshot dir {}", parent.display()))?;
    }
    std::fs::copy(source, dest)
        .with_context(|| format!("copying {} to {}", source.display(), dest.display()))?;
    Ok(())
}

fn should_skip_agent_config_entry(agent: &AgentKind, relative_path: &Path, is_dir: bool) -> bool {
    let name = relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let top_level = relative_path.components().count() == 1;

    match agent {
        AgentKind::Codex => should_skip_codex_config_entry(name, top_level, is_dir),
        _ => should_skip_default_agent_config_entry(name),
    }
}

fn should_skip_default_agent_config_entry(name: &str) -> bool {
    is_common_volatile_agent_config_entry(name)
        || name.ends_with(".sqlite")
        || name.ends_with(".sqlite-shm")
        || name.ends_with(".sqlite-wal")
        || name.starts_with("logs_")
}

fn should_skip_codex_config_entry(name: &str, top_level: bool, is_dir: bool) -> bool {
    if is_common_volatile_agent_config_entry(name) {
        return true;
    }

    if top_level && name.starts_with("logs_") {
        return true;
    }

    if is_dir {
        return false;
    }

    if name.ends_with(".sqlite") || name.ends_with(".sqlite-shm") || name.ends_with(".sqlite-wal") {
        return !name.starts_with("state_");
    }

    false
}

fn is_common_volatile_agent_config_entry(name: &str) -> bool {
    name == ".tmp"
        || name == "tmp"
        || name == "log"
        || name == "logs"
        || name == "sessions"
        || name == "shell_snapshots"
        || name == "history.jsonl"
}

fn prepare_executable_helper_script(path: &Path, prefix: &str) -> Result<NamedTempFile> {
    let contents = std::fs::read(path)
        .with_context(|| format!("reading helper script '{}'", path.display()))?;
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile()
        .context("creating helper script temp file")?;
    file.write_all(&contents)
        .context("writing helper script temp file")?;
    file.flush().context("flushing helper script temp file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file
            .as_file()
            .metadata()
            .context("reading helper script temp file metadata")?
            .permissions();
        perms.set_mode(0o755);
        file.as_file()
            .set_permissions(perms)
            .context("marking helper script temp file executable")?;
    }

    Ok(file)
}

fn proxy_addr_without_auth(proxy_url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(proxy_url)
        && let Some(host) = parsed.host_str()
    {
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let port = parsed.port_or_known_default().unwrap_or(80);
        return format!("{host}:{port}");
    }

    let authority = proxy_url
        .strip_prefix("http://")
        .or_else(|| proxy_url.strip_prefix("https://"))
        .unwrap_or(proxy_url);
    authority
        .rsplit_once('@')
        .map(|(_, addr)| addr)
        .unwrap_or(authority)
        .to_string()
}

/// Launch a one-shot passthrough container session.
///
/// Unlike `spawn`, this path does not inject harness-hat proxy/hostdo runtime
/// environment variables. It is used by the `harness-hat -- ...` wrapper.
#[instrument(skip(command_argv, workspace_path, mount_target, mounts, env_passthrough))]
pub fn spawn_passthrough(
    image: &str,
    image_name: &str,
    command_argv: &[String],
    project_name: &str,
    workspace_path: &Path,
    mount_target: &Path,
    agent: AgentKind,
    mounts: &[crate::config::ContainerMount],
    env_passthrough: &[String],
    rows: u16,
    cols: u16,
) -> Result<ContainerSession> {
    let mount_str = mount_target.display().to_string();
    let cidfile =
        std::env::temp_dir().join(format!("harness-hat-cid-{}.txt", uuid::Uuid::new_v4()));
    let docker_run_name = format!(
        "harness-hat-{}-{}",
        sanitize_docker_name(image_name),
        uuid::Uuid::new_v4().simple()
    );

    let mut docker_args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        docker_run_name.clone(),
        "--cidfile".to_string(),
        cidfile.display().to_string(),
    ];

    #[cfg(target_os = "linux")]
    docker_args.push("--add-host=host.docker.internal:host-gateway".to_string());

    #[cfg(target_os = "linux")]
    docker_args.extend_from_slice(&["--user".to_string(), "1000:1000".to_string()]);

    docker_args.extend_from_slice(&[
        "-v".to_string(),
        format!("{}:{}:rw", workspace_path.display(), mount_str),
        "-w".to_string(),
        mount_str.clone(),
    ]);

    for mount in mounts {
        docker_args.push("-v".to_string());
        docker_args.push(format!(
            "{}:{}:{}",
            mount.host.display(),
            mount.container.display(),
            mount_mode_arg(&mount.mode),
        ));
    }

    for name in env_passthrough {
        docker_args.push("-e".to_string());
        docker_args.push(name.to_string());
    }

    docker_args.push(image.to_string());
    docker_args.extend(command_argv.iter().cloned());

    info!(
        "launching passthrough container: docker {}",
        docker_args
            .iter()
            .map(|a| if a.contains(' ') || a.contains('=') {
                format!("'{a}'")
            } else {
                a.clone()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );

    let (fg, bg) = detect_default_colors();
    let default_fg = alacritty_terminal::vte::ansi::Rgb {
        r: fg.0,
        g: fg.1,
        b: fg.2,
    };
    let default_bg = alacritty_terminal::vte::ansi::Rgb {
        r: bg.0,
        g: bg.1,
        b: bg.2,
    };

    let window_size = WindowSize {
        num_lines: rows,
        num_cols: cols,
        cell_width: 0,
        cell_height: 0,
    };
    let window_size_arc = Arc::new(Mutex::new(window_size));

    let exited = Arc::new(AtomicBool::new(false));
    let has_bell = Arc::new(AtomicBool::new(false));

    let proxy = SessionEventProxy {
        sender: Arc::new(Mutex::new(None)),
        window_size: Arc::clone(&window_size_arc),
        exited: Arc::clone(&exited),
        has_bell: Arc::clone(&has_bell),
        default_fg,
        default_bg,
        grayscale_palette: agent == AgentKind::Codex,
    };

    let mut term_cfg = TermConfig::default();
    term_cfg.scrolling_history = 50_000;
    let term_size = TermSize {
        cols: cols as usize,
        lines: rows as usize,
    };
    let term = Arc::new(FairMutex::new(Term::new(
        term_cfg,
        &term_size,
        proxy.clone(),
    )));

    let mut options = tty::Options::default();
    options.shell = Some(tty::Shell::new("docker".to_string(), docker_args));
    options.working_directory = None;
    options.drain_on_exit = false;
    options.env = HashMap::new();

    let pty = tty::new(&options, window_size, 0).context("open PTY")?;
    let event_loop = EventLoop::new(Arc::clone(&term), proxy.clone(), pty, false, false)
        .context("event loop")?;
    let sender = event_loop.channel();
    let notifier = Notifier(sender.clone());
    if let Ok(mut s) = proxy.sender.lock() {
        *s = Some(sender);
    }
    let _handle = event_loop.spawn();

    let container_id =
        read_container_id(&cidfile, &docker_run_name).context("reading docker container id")?;
    let _ = std::fs::remove_file(&cidfile);

    Ok(ContainerSession {
        container_name: format!("passthrough-{image_name}"),
        agent_kind: crate::config::AgentKind::None,
        container_id,
        docker_name: docker_run_name,
        project: project_name.to_owned(),
        session_token: uuid::Uuid::new_v4().simple().to_string(),
        parent_session_token: None,
        subagent_name: None,
        mount_target: mount_str,
        launched_at: Instant::now(),
        terminal_snapshot_hash: 0,
        terminal_changed_at: Instant::now(),
        last_input_at: Arc::new(Mutex::new(None)),
        term,
        notifier,
        window_size: window_size_arc,
        exited,
        has_bell,
        exit_reported: false,
        _scoped_proxy: None,
        _cred_tempfile: None,
        _env_tempfile: None,
        _hostdo_tempfile: None,
        _agent_config_tempdir: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        HARNESS_HAT_CA_BUNDLE_PATH, HARNESS_HAT_CA_CERT_PATH, is_agent_config_mount,
        prepare_subagent_agent_config_snapshot, proxy_addr_without_auth, write_ca_env_entries,
    };
    use crate::config::{AgentKind, ContainerMount, MountMode};
    use std::path::{Path, PathBuf};

    #[test]
    fn proxy_addr_without_auth_strips_userinfo() {
        assert_eq!(
            proxy_addr_without_auth("http://harness-hat:secret@host.docker.internal:54321"),
            "host.docker.internal:54321"
        );
    }

    #[test]
    fn proxy_addr_without_auth_formats_ipv6_hosts() {
        assert_eq!(
            proxy_addr_without_auth("http://harness-hat:secret@[::1]:54321"),
            "[::1]:54321"
        );
    }

    #[test]
    fn ca_env_entries_use_combined_bundle_for_replacement_vars() {
        let mut env = Vec::new();
        write_ca_env_entries(&mut env).expect("write CA env");
        let env = String::from_utf8(env).expect("utf8 env");

        assert!(env.contains(&format!("SSL_CERT_FILE={HARNESS_HAT_CA_BUNDLE_PATH}\n")));
        assert!(env.contains(&format!(
            "CODEX_CA_CERTIFICATE={HARNESS_HAT_CA_BUNDLE_PATH}\n"
        )));
        assert!(env.contains(&format!(
            "REQUESTS_CA_BUNDLE={HARNESS_HAT_CA_BUNDLE_PATH}\n"
        )));
        assert!(env.contains(&format!("NODE_EXTRA_CA_CERTS={HARNESS_HAT_CA_CERT_PATH}\n")));
        assert!(!env.contains(&format!("SSL_CERT_FILE={HARNESS_HAT_CA_CERT_PATH}\n")));
    }

    #[test]
    fn subagent_config_snapshot_filters_agent_mounts_and_preserves_codex_state() {
        let root = tempfile::tempdir().expect("tempdir");
        let codex_home = root.path().join(".codex");
        std::fs::create_dir_all(&codex_home).expect("codex home");
        std::fs::write(codex_home.join("auth.json"), "{}").expect("auth");
        std::fs::write(codex_home.join("config.toml"), "model = \"gpt\"").expect("config");
        std::fs::write(codex_home.join("models_cache.json"), "{}").expect("models cache");
        std::fs::write(codex_home.join("state_5.sqlite"), "codex session state").expect("state db");
        std::fs::write(codex_home.join("state_5.sqlite-wal"), "codex state wal")
            .expect("state wal");
        std::fs::write(codex_home.join("logs_2.sqlite"), "large runtime logs").expect("logs");
        std::fs::write(
            codex_home.join("logs_2.sqlite-wal"),
            "large runtime logs wal",
        )
        .expect("logs wal");
        std::fs::create_dir_all(codex_home.join("shell_snapshots")).expect("snapshots");
        std::fs::write(codex_home.join("shell_snapshots/old.sh"), "echo old").expect("snapshot");
        std::fs::create_dir_all(codex_home.join("cache/codex_apps_tools")).expect("cache");
        std::fs::write(codex_home.join("cache/codex_apps_tools/tools.json"), "{}")
            .expect("tools cache");
        std::fs::create_dir_all(codex_home.join("plugins/cache")).expect("plugin cache");
        std::fs::write(codex_home.join("plugins/cache/plugin.json"), "{}").expect("plugin");

        let other = root.path().join("other");
        std::fs::create_dir_all(&other).expect("other");
        let mounts = vec![
            ContainerMount {
                host: codex_home.clone(),
                container: PathBuf::from("/home/ubuntu/.codex"),
                mode: MountMode::Rw,
            },
            ContainerMount {
                host: other.clone(),
                container: PathBuf::from("/workspace/other"),
                mode: MountMode::Rw,
            },
        ];

        let (effective, snapshot) =
            prepare_subagent_agent_config_snapshot(&AgentKind::Codex, &mounts).expect("snapshot");
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].container, Path::new("/workspace/other"));

        let snapshot = snapshot.expect("snapshot exists");
        let staged = snapshot.tempdir.path().join("item-0");
        assert!(staged.join("auth.json").is_file());
        assert!(staged.join("config.toml").is_file());
        assert!(staged.join("models_cache.json").is_file());
        assert!(staged.join("state_5.sqlite").is_file());
        assert!(staged.join("state_5.sqlite-wal").is_file());
        assert!(staged.join("cache/codex_apps_tools/tools.json").is_file());
        assert!(staged.join("plugins/cache/plugin.json").is_file());
        assert!(!staged.join("logs_2.sqlite").exists());
        assert!(!staged.join("logs_2.sqlite-wal").exists());
        assert!(!staged.join("shell_snapshots").exists());

        let manifest =
            std::fs::read_to_string(snapshot.tempdir.path().join("manifest.tsv")).unwrap();
        assert!(manifest.contains("item-0\t/home/ubuntu/.codex"));
    }

    #[test]
    fn gemini_subagent_snapshot_mirrors_home_to_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let gemini_home = root.path().join(".gemini");
        std::fs::create_dir_all(&gemini_home).expect("gemini home");
        std::fs::write(gemini_home.join("oauth_creds.json"), "{}").expect("creds");
        let mounts = vec![ContainerMount {
            host: gemini_home,
            container: PathBuf::from("/home/ubuntu/.gemini"),
            mode: MountMode::Rw,
        }];

        let (_effective, snapshot) =
            prepare_subagent_agent_config_snapshot(&AgentKind::Gemini, &mounts).expect("snapshot");
        let snapshot = snapshot.expect("snapshot exists");
        let manifest =
            std::fs::read_to_string(snapshot.tempdir.path().join("manifest.tsv")).unwrap();
        assert!(manifest.contains("item-0\t/home/ubuntu/.gemini"));
        assert!(manifest.contains("item-0\t/root/.gemini"));
    }

    #[test]
    fn agent_config_mount_matching_is_agent_specific() {
        assert!(is_agent_config_mount(
            &AgentKind::Opencode,
            Path::new("/home/ubuntu/.config/opencode")
        ));
        assert!(!is_agent_config_mount(
            &AgentKind::Codex,
            Path::new("/home/ubuntu/.config/opencode")
        ));
    }
}
