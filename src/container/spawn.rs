use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Instant;
use tempfile::NamedTempFile;
use tracing::info;
use tracing::instrument;

use crate::config::ContainerDef;
use crate::container::core::{
    LABEL_ALIAS, LABEL_SESSION, LABEL_TEMPLATE, LABEL_WORKSPACE, TermSize, loopback_to_host_docker,
    mount_mode_arg, parse_docker_label, sanitize_docker_name,
};
use crate::container::helpers::detect_default_colors;
use crate::container::{ContainerSession, SessionEventProxy, compose_no_proxy, read_container_id};
use crate::fs_util::write_env_file_entry;

const PRIMARY_PROXY_CONN_LIMIT: usize = 0;
const HARNESS_HAT_CA_CERT_PATH: &str = "/usr/local/share/ca-certificates/harness-hat-ca.crt";
const HARNESS_HAT_CA_BUNDLE_PATH: &str = "/tmp/harness-hat-ca-bundle.crt";
const CODER_HOME: &str = "/home/coder";

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
    control_url: &str,
    proxy_url: &str,
    ca_cert_host_path: &str,
    control_script_host_path: Option<&Path>,
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
    let alias = allocate_session_alias();

    let container_control_url = loopback_to_host_docker(control_url);
    let container_proxy_url = loopback_to_host_docker(proxy_url);
    let container_proxy_addr = proxy_addr_without_auth(&container_proxy_url);
    let scoped_proxy_auth = scoped_proxy
        .as_ref()
        .map(|proxy| proxy.proxy_auth_token().to_string())
        .unwrap_or_default();
    let launch_notes = Vec::new();

    let mut docker_args: Vec<String> = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        docker_run_name.clone(),
        "--cidfile".to_string(),
        cidfile.display().to_string(),
        // Discovery labels so `hh shell` can find and identify this session
        // without the manager process being alive.
        "--label".to_string(),
        format!("{LABEL_ALIAS}={alias}"),
        "--label".to_string(),
        format!("{LABEL_WORKSPACE}={project_name}"),
        "--label".to_string(),
        format!("{LABEL_TEMPLATE}={}", ctr.name),
        "--label".to_string(),
        format!("{LABEL_SESSION}={session_token}"),
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

    if let Some(memory) = ctr
        .memory
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        docker_args.extend_from_slice(&["--memory".to_string(), memory.to_string()]);
    }
    if let Some(cpus) = ctr.cpus.as_deref().filter(|value| !value.trim().is_empty()) {
        docker_args.extend_from_slice(&["--cpus".to_string(), cpus.to_string()]);
    }
    if let Some(shm_size) = ctr
        .shm_size
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        docker_args.extend_from_slice(&["--shm-size".to_string(), shm_size.to_string()]);
    }

    docker_args.extend_from_slice(&[
        "-v".to_string(),
        format!("{}:{}:rw", workspace_path.display(), mount_str),
        "-v".to_string(),
        format!("{ca_cert_host_path}:{ca_env_path}:ro"),
        "-w".to_string(),
        mount_str.clone(),
    ]);

    let control_tempfile = match control_script_host_path {
        Some(path) => Some(prepare_executable_helper_script(
            path,
            "harness-hat-control-",
        )?),
        None => None,
    };

    // Prepare secure env file to prevent token leakage via `ps`
    let mut env_file = tempfile::Builder::new()
        .prefix("harness-hat-env-")
        .tempfile()
        .context("failed to create temp env file")?;

    write_ca_env_entries(&mut env_file)?;
    for (key, value) in &ctr.env {
        write_env_file_entry(&mut env_file, key, value)?;
    }
    for (key, value) in extra_env {
        write_env_file_entry(&mut env_file, key, value)?;
    }
    if should_inject_coder_home(&ctr.mounts)
        && !ctr.env.contains_key("HOME")
        && !extra_env.iter().any(|(key, _)| key == "HOME")
    {
        write_env_file_entry(&mut env_file, "HOME", CODER_HOME)?;
    }

    write_env_file_entry(&mut env_file, "HARNESS_HAT_TOKEN", token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_SESSION_TOKEN", session_token)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_PROJECT", project_name)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_MOUNT_TARGET", &mount_str)?;
    write_env_file_entry(&mut env_file, "HARNESS_HAT_URL", &container_control_url)?;
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
    write_env_file_entry(
        &mut env_file,
        "HARNESS_HAT_PROXY_CONN_LIMIT",
        PRIMARY_PROXY_CONN_LIMIT.to_string(),
    )?;
    if let Some(forwards) = format_localhost_forwards(&ctr.localhost_forwards) {
        write_env_file_entry(&mut env_file, "HARNESS_HAT_LOCALHOST_FORWARDS", forwards)?;
    }
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

    docker_args.push("--env-file".to_string());
    docker_args.push(env_file.path().display().to_string());

    // `.claude.json` is rewritten in place by every Claude Code instance
    // (numStartups, project trust, etc.). Bind-mounting the host's live file
    // means the host's own Claude and the container's Claude race on the same
    // file, tearing it into `JSON Parse error: Unterminated string` corruption
    // and resetting the host config (losing the OAuth account → the container
    // appears logged out). Seeded mounts (see `should_seed_mount`) get a private
    // per-session copy instead: the container reads the session through, then
    // owns the file privately. The handles live for the container's lifetime.
    let mut seed_tempfiles: Vec<NamedTempFile> = Vec::new();
    for mount in &ctr.mounts {
        let host_arg = match seed_private_mount(mount)? {
            Some(tempfile) => {
                let path = tempfile.path().display().to_string();
                seed_tempfiles.push(tempfile);
                path
            }
            None => mount.host.display().to_string(),
        };
        docker_args.push("-v".to_string());
        docker_args.push(format!(
            "{}:{}:{}",
            host_arg,
            mount.container.display(),
            mount_mode_arg(&mount.mode),
        ));
    }

    for name in &ctr.env_passthrough {
        if ctr.env.contains_key(name) || extra_env.iter().any(|(key, _)| key == name) {
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
    term_cfg.scrolling_history = 100_000;
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
            mouse_scroll: ctr.mouse_scroll,
            container_id,
            docker_name,
            alias,
            project: project_name.to_owned(),
            session_token: session_token.to_string(),
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
            _seed_tempfiles: seed_tempfiles,
            _env_tempfile: Some(env_file),
            _control_tempfile: control_tempfile,
        },
        launch_notes,
    ))
}

/// Pick a zero-padded 4-digit id not currently used by a running harness-hat
/// container. Randomness is derived from a fresh UUID to avoid pulling in a
/// dedicated RNG dependency. Collisions among the ~10k space are rare; we retry
/// a bounded number of times and fall back to a random value if every probe
/// somehow clashes (effectively impossible for realistic session counts).
fn allocate_session_alias() -> String {
    let used = running_session_aliases();
    for _ in 0..64 {
        let candidate = random_four_digit();
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    random_four_digit()
}

fn random_four_digit() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let value = u16::from_le_bytes([bytes[0], bytes[1]]) as u32 % 10_000;
    format!("{value:04}")
}

fn running_session_aliases() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label={LABEL_ALIAS}"),
            "--format",
            "{{.Labels}}",
        ])
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(alias) = parse_docker_label(line, LABEL_ALIAS) {
                set.insert(alias);
            }
        }
    }
    set
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

fn format_localhost_forwards(forwards: &[crate::config::LocalhostForward]) -> Option<String> {
    if forwards.is_empty() {
        return None;
    }
    Some(
        forwards
            .iter()
            .map(|forward| {
                format!(
                    "{}:{}",
                    forward.container_port,
                    forward.effective_host_port()
                )
            })
            .collect::<Vec<_>>()
            .join(","),
    )
}

fn should_inject_coder_home(mounts: &[crate::config::ContainerMount]) -> bool {
    mounts
        .iter()
        .any(|mount| mount.container.starts_with(CODER_HOME))
}

/// True when this mount targets Claude Code's `.claude.json` config file — the
/// single hot-rewritten file that must not be shared live with the host.
/// Matched by the container-side basename so it holds regardless of the home
/// directory the template mounts into.
fn is_claude_config_mount(mount: &crate::config::ContainerMount) -> bool {
    mount.container.file_name() == Some(std::ffi::OsStr::new(".claude.json"))
}

/// Whether this mount should be seeded as a private per-session copy rather than
/// bind-mounted live. Driven by the TOML `seed` flag; when unset it defaults to
/// the smart heuristic of privatizing only `.claude.json` (see [`ContainerMount`]).
fn should_seed_mount(mount: &crate::config::ContainerMount) -> bool {
    mount.seed.unwrap_or_else(|| is_claude_config_mount(mount))
}

/// If `mount` is configured for seeding and the host path is a regular file,
/// copy its current contents into a fresh tempfile and return it; the caller
/// bind-mounts that copy instead of the host's live file and keeps the handle
/// alive for the container's lifetime (cleaned up on session drop). For
/// `.claude.json` this seeds the session through (OAuth account, onboarding)
/// while the container's writes land on the private copy — the host file is
/// never touched.
///
/// Returns `None` (mount the host path as-is) when the mount isn't flagged for
/// seeding or the host path is not a regular file — bind-mounting a missing
/// path would make Docker materialize a directory, and a directory can't be
/// copied into a single tempfile, so we leave that pre-existing behavior alone.
fn seed_private_mount(mount: &crate::config::ContainerMount) -> Result<Option<NamedTempFile>> {
    if !should_seed_mount(mount) || !mount.host.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read(&mount.host)
        .with_context(|| format!("reading {} to seed container copy", mount.host.display()))?;
    let mut tempfile = tempfile::Builder::new()
        .prefix("harness-hat-seed-")
        .tempfile()
        .with_context(|| format!("creating private copy of {}", mount.host.display()))?;
    tempfile
        .write_all(&contents)
        .with_context(|| format!("writing private copy of {}", mount.host.display()))?;
    tempfile
        .flush()
        .with_context(|| format!("flushing private copy of {}", mount.host.display()))?;
    Ok(Some(tempfile))
}

#[cfg(test)]
mod tests {
    use super::{
        HARNESS_HAT_CA_BUNDLE_PATH, HARNESS_HAT_CA_CERT_PATH, format_localhost_forwards,
        proxy_addr_without_auth, seed_private_mount, should_inject_coder_home, should_seed_mount,
        write_ca_env_entries,
    };
    use crate::config::{ContainerMount, LocalhostForward, MountMode};
    use std::io::Write as _;
    use std::path::PathBuf;

    fn mount(host: &str, container: &str, seed: Option<bool>) -> ContainerMount {
        ContainerMount {
            host: PathBuf::from(host),
            container: PathBuf::from(container),
            mode: MountMode::Rw,
            seed,
        }
    }

    #[test]
    fn seed_defaults_to_claude_config_and_honors_explicit_flag() {
        // Unset: `.claude.json` seeds by default, everything else does not.
        assert!(should_seed_mount(&mount(
            "/Users/me/.claude.json",
            "/home/coder/.claude.json",
            None
        )));
        assert!(!should_seed_mount(&mount(
            "/Users/me/.claude",
            "/home/coder/.claude",
            None
        )));

        // Explicit flag overrides the heuristic in both directions.
        assert!(!should_seed_mount(&mount(
            "/Users/me/.claude.json",
            "/home/coder/.claude.json",
            Some(false)
        )));
        assert!(should_seed_mount(&mount(
            "/Users/me/.config/x",
            "/home/coder/.config/x",
            Some(true)
        )));
    }

    #[test]
    fn seed_private_mount_copies_existing_file_only() {
        // An unflagged non-config mount is left alone even if it's a real file.
        let other = tempfile::Builder::new()
            .tempfile()
            .expect("create temp source");
        let not_seeded = mount(
            other.path().to_str().unwrap(),
            "/home/coder/.codex/config.toml",
            None,
        );
        assert!(
            seed_private_mount(&not_seeded)
                .expect("seed non-config")
                .is_none()
        );

        // A seeded file mount is copied into a private tempfile with identical bytes.
        let mut source = tempfile::Builder::new()
            .suffix(".claude.json")
            .tempfile()
            .expect("create temp source");
        let payload = br#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#;
        source.write_all(payload).expect("write source");
        source.flush().expect("flush source");
        let config = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            None,
        );
        let seeded = seed_private_mount(&config)
            .expect("seed config")
            .expect("config file should be privatized");
        assert_ne!(seeded.path(), source.path());
        assert_eq!(std::fs::read(seeded.path()).expect("read copy"), payload);

        // `seed = false` forces the shared live bind mount even for `.claude.json`.
        let shared = mount(
            source.path().to_str().unwrap(),
            "/home/coder/.claude.json",
            Some(false),
        );
        assert!(
            seed_private_mount(&shared)
                .expect("seed disabled")
                .is_none()
        );

        // A missing host file is left as-is (don't let Docker create a dir).
        let missing = mount("/no/such/.claude.json", "/home/coder/.claude.json", None);
        assert!(
            seed_private_mount(&missing)
                .expect("seed missing")
                .is_none()
        );
    }

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
    fn localhost_forwards_are_encoded_for_init_script() {
        let forwards = vec![
            LocalhostForward {
                container_port: 8081,
                host_port: None,
            },
            LocalhostForward {
                container_port: 9090,
                host_port: Some(19090),
            },
        ];
        assert_eq!(
            format_localhost_forwards(&forwards).as_deref(),
            Some("8081:8081,9090:19090")
        );
    }

    #[test]
    fn coder_home_is_injected_for_tool_home_mounts() {
        assert!(should_inject_coder_home(&[mount(
            "/host/.cache/tool",
            "/home/coder/.cache/tool",
            None
        )]));
        assert!(should_inject_coder_home(&[mount(
            "/host/.config/tool",
            "/home/coder/.config/tool",
            None
        )]));
        assert!(should_inject_coder_home(&[mount(
            "/host/.tool",
            "/home/coder/.tool",
            None
        )]));
        assert!(!should_inject_coder_home(&[mount(
            "/host/cache",
            "/workspace/cache",
            None
        )]));
    }
}
