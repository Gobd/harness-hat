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

use crate::config::{ContainerDef, ContainerMount};
use crate::container::core::{
    TermSize, loopback_to_host_docker, mount_mode_arg, sanitize_docker_name,
};
use crate::container::helpers::detect_default_colors;
use crate::container::{ContainerSession, SessionEventProxy, compose_no_proxy, read_container_id};
use crate::fs_util::write_env_file_entry;

const PRIMARY_PROXY_CONN_LIMIT: usize = 0;
const SUBAGENT_PROXY_CONN_LIMIT: usize = 16;
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
#[instrument(skip(ctr, command_argv, workspace_path, extra_env, scoped_proxy))]
pub fn spawn(
    ctr: &ContainerDef,
    command_argv: Option<&[String]>,
    project_name: &str,
    workspace_path: &Path,
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
        let (mounts, snapshot) = prepare_subagent_mount_snapshot(&ctr.mounts)?;
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
                "subagent snapshot copied from {} configured mount path(s)",
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

    for mount in &effective_mounts {
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
        grayscale_palette: ctr.grayscale_palette,
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
            agent_kind: crate::config::infer_agent_kind_from_argv(command_argv),
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
            _cred_tempfile: None,
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

fn prepare_subagent_mount_snapshot(
    mounts: &[ContainerMount],
) -> Result<(Vec<ContainerMount>, Option<AgentConfigSnapshot>)> {
    let effective_mounts = Vec::new();
    let mut mappings: Vec<(String, PathBuf)> = Vec::new();
    let tempdir = tempfile::Builder::new()
        .prefix("harness-hat-agent-config-")
        .tempdir()
        .context("creating subagent config snapshot directory")?;

    for (idx, mount) in mounts.iter().enumerate() {
        if !mount.host.exists() {
            continue;
        }

        let staged_name = format!("item-{idx}");
        let staged_path = tempdir.path().join(&staged_name);
        copy_snapshot_path(
            &mount.host,
            &staged_path,
            mount.container.as_path(),
            Path::new(""),
        )
        .with_context(|| format!("copying mount snapshot from {}", mount.host.display()))?;
        mappings.push((staged_name.clone(), mount.container.clone()));
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

fn copy_snapshot_path(
    source: &Path,
    dest: &Path,
    container_target: &Path,
    relative_path: &Path,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !should_snapshot_path(container_target, relative_path, meta.is_dir()) {
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dest)
            .with_context(|| format!("creating config snapshot dir {}", dest.display()))?;
        copy_snapshot_dir(source, dest, container_target, relative_path)
    } else {
        copy_snapshot_file(source, dest)
    }
}

fn copy_snapshot_dir(
    source: &Path,
    dest: &Path,
    container_target: &Path,
    relative_path: &Path,
) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry in {}", source.display()))?;
        let name = entry.file_name();
        let src = entry.path();
        let meta = std::fs::symlink_metadata(&src)
            .with_context(|| format!("reading metadata for {}", src.display()))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let dst = dest.join(&name);
        let child_relative = relative_path.join(&name);
        if !should_snapshot_path(container_target, &child_relative, meta.is_dir()) {
            continue;
        }
        if meta.is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("creating config snapshot dir {}", dst.display()))?;
            copy_snapshot_dir(&src, &dst, container_target, &child_relative)?;
        } else if meta.is_file() {
            copy_snapshot_file(&src, &dst)?;
        }
    }
    Ok(())
}

fn should_snapshot_path(container_target: &Path, relative_path: &Path, is_dir: bool) -> bool {
    let is_codex_home = matches!(
        container_target.to_str(),
        Some("/home/ubuntu/.codex") | Some("/root/.codex")
    );
    if is_codex_home {
        if let Some(first_component) = relative_path.components().next().and_then(|component| {
            if let std::path::Component::Normal(name) = component {
                name.to_str()
            } else {
                None
            }
        }) {
            if matches!(first_component, ".tmp" | "log" | "sessions" | "tmp") {
                return false;
            }
        }
    }

    if is_dir {
        return true;
    }

    let Some(file_name) = relative_path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };

    if is_codex_home
        && (file_name.ends_with(".sqlite")
            || file_name.ends_with(".sqlite-wal")
            || file_name.ends_with(".sqlite-shm"))
    {
        return false;
    }

    true
}

fn copy_snapshot_file(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config snapshot dir {}", parent.display()))?;
    }
    std::fs::copy(source, dest)
        .with_context(|| format!("copying {} to {}", source.display(), dest.display()))?;
    Ok(())
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
    grayscale_palette: bool,
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
        grayscale_palette,
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
        agent_kind: crate::config::infer_agent_kind_from_argv(Some(command_argv)),
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
        HARNESS_HAT_CA_BUNDLE_PATH, HARNESS_HAT_CA_CERT_PATH, prepare_subagent_mount_snapshot,
        proxy_addr_without_auth, write_ca_env_entries,
    };
    use crate::config::{ContainerMount, MountMode};
    use std::path::PathBuf;

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
    fn subagent_config_snapshot_skips_live_codex_sqlite_state() {
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
        std::fs::create_dir_all(codex_home.join("log")).expect("log dir");
        std::fs::write(codex_home.join("log/codex-tui.log"), "live log").expect("log file");
        std::fs::create_dir_all(codex_home.join("sessions/2026/05/08")).expect("sessions");
        std::fs::write(
            codex_home.join("sessions/2026/05/08/session.jsonl"),
            "live session",
        )
        .expect("session file");
        std::fs::create_dir_all(codex_home.join(".tmp/plugins/.git")).expect("tmp plugins");
        std::fs::write(codex_home.join(".tmp/plugins/.git/index"), "tmp index").expect("tmp index");
        std::fs::create_dir_all(codex_home.join("tmp/arg0/current")).expect("arg0");
        std::fs::write(codex_home.join("tmp/arg0/current/.lock"), "").expect("arg0 lock");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            "/definitely/not/a/host/path/codex",
            codex_home.join("tmp/arg0/current/codex"),
        )
        .expect("dangling codex symlink");
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

        let (effective, snapshot) = prepare_subagent_mount_snapshot(&mounts).expect("snapshot");
        assert!(effective.is_empty());

        let snapshot = snapshot.expect("snapshot exists");
        let staged = snapshot.tempdir.path().join("item-0");
        assert!(staged.join("auth.json").is_file());
        assert!(staged.join("config.toml").is_file());
        assert!(staged.join("models_cache.json").is_file());
        assert!(staged.join("cache/codex_apps_tools/tools.json").is_file());
        assert!(staged.join("plugins/cache/plugin.json").is_file());
        assert!(staged.join("shell_snapshots/old.sh").is_file());
        assert!(!staged.join("state_5.sqlite").exists());
        assert!(!staged.join("state_5.sqlite-wal").exists());
        assert!(!staged.join("logs_2.sqlite").exists());
        assert!(!staged.join("logs_2.sqlite-wal").exists());
        assert!(!staged.join("log").exists());
        assert!(!staged.join("sessions").exists());
        assert!(!staged.join(".tmp").exists());
        assert!(!staged.join("tmp").exists());

        let manifest =
            std::fs::read_to_string(snapshot.tempdir.path().join("manifest.tsv")).unwrap();
        assert!(manifest.contains("item-0\t/home/ubuntu/.codex"));
        assert!(manifest.contains("item-1\t/workspace/other"));
    }

    #[test]
    fn subagent_snapshot_preserves_exact_mount_targets() {
        let root = tempfile::tempdir().expect("tempdir");
        let gemini_home = root.path().join(".gemini");
        std::fs::create_dir_all(&gemini_home).expect("gemini home");
        std::fs::write(gemini_home.join("oauth_creds.json"), "{}").expect("creds");
        let mounts = vec![ContainerMount {
            host: gemini_home,
            container: PathBuf::from("/home/ubuntu/.gemini"),
            mode: MountMode::Rw,
        }];

        let (_effective, snapshot) = prepare_subagent_mount_snapshot(&mounts).expect("snapshot");
        let snapshot = snapshot.expect("snapshot exists");
        let manifest =
            std::fs::read_to_string(snapshot.tempdir.path().join("manifest.tsv")).unwrap();
        assert!(manifest.contains("item-0\t/home/ubuntu/.gemini"));
    }

    #[test]
    fn subagent_snapshot_keeps_sqlite_files_for_non_codex_mounts() {
        let root = tempfile::tempdir().expect("tempdir");
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).expect("other");
        std::fs::write(other.join("state.sqlite"), "other sqlite").expect("sqlite");
        let mounts = vec![ContainerMount {
            host: other,
            container: PathBuf::from("/workspace/other"),
            mode: MountMode::Rw,
        }];

        let (_effective, snapshot) = prepare_subagent_mount_snapshot(&mounts).expect("snapshot");
        let snapshot = snapshot.expect("snapshot exists");
        assert!(
            snapshot
                .tempdir
                .path()
                .join("item-0/state.sqlite")
                .is_file()
        );
    }
}
