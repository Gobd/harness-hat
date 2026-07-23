use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

pub const COMMAND_NAME: &str = "hht";

#[derive(Debug, Clone, Parser)]
#[command(name = COMMAND_NAME, version, about = "Harness Hat — manager UI")]
struct CliOptions {
    /// Path to config file. Used by the interactive workspace manager (the
    /// default action when no subcommand is given).
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands. When none is given, `hht` launches the interactive manager.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Generate a sample config file (defaults to ./harness-hat.toml).
    Init {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Open an interactive shell in a running session. With no id, lists the
    /// running sessions and their ids. Pass `--kill ID` to terminate a
    /// session. Any args after the id are passed verbatim to `docker exec` as
    /// the command to run instead of bash — e.g. `hht shell 0042 claude --resume`.
    Shell {
        #[arg(value_name = "ID")]
        id: Option<String>,
        /// Terminate and remove a running session by ID.
        #[arg(long, value_name = "ID", conflicts_with_all = ["id", "args"])]
        kill: Option<String>,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<OsString>,
    },
    /// Attach to (or start) a session for the current working directory.
    ///
    /// If the cwd is inside a configured workspace and a session is already
    /// running for it, attach to the most recent. Otherwise launch a new
    /// session against the running manager. If the cwd does not match any
    /// configured workspace, a new `[[workspaces]]` entry is appended to the
    /// config file using the directory's basename as the workspace name.
    ///
    /// Any args after the subcommand are passed verbatim to `docker exec`
    /// (same passthrough behavior as `hht shell ID …`).
    Workspace {
        /// List configured workspaces without starting or attaching to a session.
        #[arg(long, conflicts_with_all = ["template", "name", "rebuild", "args"])]
        list: bool,
        /// Use a specific container template instead of prompting.
        #[arg(long, value_name = "NAME")]
        template: Option<String>,
        /// Jump directly to a named workspace instead of matching by cwd.
        #[arg(long, value_name = "WORKSPACE")]
        name: Option<String>,
        /// Rebuild the container image (and its base) before launching,
        /// bypassing the Docker layer cache. Useful after updating Dockerfiles
        /// or to pick up a newer version of an installed tool (e.g. claude-code).
        #[arg(long)]
        rebuild: bool,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<OsString>,
    },
    /// Rebuild the base image followed by selected templates, or every
    /// Dockerfile template in the configured docker_dir when none are named.
    Rebuild {
        /// Disable Docker's layer cache for the base and template builds.
        #[arg(long)]
        no_cache: bool,
        /// Dockerfile template stems to rebuild, for example `go` or `python`.
        #[arg(value_name = "TEMPLATE")]
        templates: Vec<String>,
    },
    /// Install Harness Hat as a per-user background agent that starts when
    /// the graphical desktop session starts.
    Install,
    /// Remove the per-user Harness Hat background agent.
    Uninstall,
    /// Internal: pop a native system dialog and print the result to stdout.
    /// Invoked by the manager as a subprocess so the dialog has its own
    /// main thread / event loop; not intended for direct end-user use.
    #[command(name = "__dialog", hide = true, subcommand)]
    Dialog(DialogCommand),
    /// Internal background-agent entry point. Installed service definitions
    /// invoke this rather than the terminal UI.
    #[command(name = "__service", hide = true)]
    Service,
}

/// Dialog kinds the `__dialog` subcommand can render. Each variant maps to
/// one concrete native dialog; output is a single machine-readable line on
/// stdout (see `native_approval::Outcome::encode`).
#[derive(Debug, Clone, Subcommand)]
pub enum DialogCommand {
    /// Network-approval prompt: Allow / Deny + a "remember" checkbox.
    NetworkApproval {
        #[arg(long, value_name = "HOST")]
        host: String,
        #[arg(long, value_name = "METHOD", default_value = "")]
        method: String,
        #[arg(long, value_name = "PATH", default_value = "")]
        path: String,
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        #[arg(long, value_name = "WORKSPACE")]
        workspace: Option<String>,
    },
    /// Host command-approval prompt.
    HostdoApproval {
        #[arg(long, value_name = "COMMAND")]
        command: String,
        #[arg(long, value_name = "CWD")]
        cwd: Option<String>,
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        #[arg(long = "timeout", value_name = "TIMEOUT_SECS")]
        timeout_secs: Option<u64>,
        #[arg(long, value_name = "WORKSPACE")]
        workspace: Option<String>,
    },
    /// Rules-file tampering prompt. Only explicit trust unblocks decisions.
    RulesChanged {
        #[arg(long, value_name = "PATH")]
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub config: Option<PathBuf>,
    pub command: Option<Command>,
}

pub fn parse() -> Result<Cli> {
    let raw: Vec<OsString> = std::env::args_os().collect();
    parse_from(raw)
}

pub fn parse_from(raw: Vec<OsString>) -> Result<Cli> {
    let usage = format!(
        "Usage: {COMMAND_NAME} [--config PATH] [init [PATH] | shell [ID] [COMMAND...] | shell --kill ID | workspace [OPTIONS] [COMMAND...] | rebuild [OPTIONS] [TEMPLATE...] | install | uninstall]"
    );
    if raw.is_empty() {
        bail!("missing argv[0]. {usage}");
    }

    // `Error::exit()` prints --help/--version to stdout and exits 0, and prints
    // genuine usage errors to stderr (exit 2) with clap's formatting, rather
    // than surfacing them as an anyhow "Error: ..." message.
    let options = match CliOptions::try_parse_from(raw) {
        Ok(options) => options,
        Err(err) => err.exit(),
    };
    Ok(Cli {
        config: options.config,
        command: options.command,
    })
}

#[cfg(test)]
mod tests {
    use super::{Command, DialogCommand, parse_from};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = parse_from(argv(&["hht"])).expect("parse");
        assert!(cli.command.is_none());
        assert!(cli.config.is_none());
    }

    #[test]
    fn config_flag_applies_to_default_action() {
        let cli = parse_from(argv(&["hht", "--config", "/tmp/x.toml"])).expect("parse");
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn init_subcommand_takes_optional_path() {
        let cli = parse_from(argv(&["hht", "init"])).expect("parse");
        assert!(matches!(cli.command, Some(Command::Init { path: None })));

        let cli = parse_from(argv(&["hht", "init", "custom.toml"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Init { path: Some(p) }) if p == PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn shell_subcommand_takes_optional_id() {
        let cli = parse_from(argv(&["hht", "shell"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: None, kill: None, ref args }) if args.is_empty()
        ));

        let cli = parse_from(argv(&["hht", "shell", "0042"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: Some(id), kill: None, ref args }) if id == "0042" && args.is_empty()
        ));
    }

    #[test]
    fn workspace_subcommand_parses_template_and_trailing_args() {
        let cli = parse_from(argv(&["hht", "workspace"])).expect("parse");
        let Some(Command::Workspace { template, args, .. }) = cli.command else {
            panic!("expected Workspace");
        };
        assert!(template.is_none());
        assert!(args.is_empty());

        let cli = parse_from(argv(&[
            "hht",
            "workspace",
            "--template",
            "dev",
            "claude",
            "--resume",
        ]))
        .expect("parse");
        let Some(Command::Workspace { template, args, .. }) = cli.command else {
            panic!("expected Workspace");
        };
        assert_eq!(template.as_deref(), Some("dev"));
        assert_eq!(
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["claude".to_string(), "--resume".to_string()],
        );
    }

    #[test]
    fn workspace_subcommand_parses_list() {
        let cli = parse_from(argv(&["hht", "workspace", "--list"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Workspace { list: true, .. })
        ));
    }

    #[test]
    fn rebuild_subcommand_parses_cache_and_template_options() {
        let cli =
            parse_from(argv(&["hht", "rebuild", "--no-cache", "go", "python"])).expect("parse");
        let Some(Command::Rebuild {
            no_cache,
            templates,
        }) = cli.command
        else {
            panic!("expected Rebuild subcommand");
        };
        assert!(no_cache);
        assert_eq!(templates, vec!["go", "python"]);
    }

    #[test]
    fn shell_subcommand_collects_trailing_args_verbatim() {
        let cli = parse_from(argv(&["hht", "shell", "0042", "claude", "--resume"])).expect("parse");
        let Some(Command::Shell {
            id,
            kill: None,
            args,
        }) = cli.command
        else {
            panic!("expected Shell subcommand");
        };
        assert_eq!(id.as_deref(), Some("0042"));
        assert_eq!(
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["claude".to_string(), "--resume".to_string()],
        );
    }

    #[test]
    fn shell_subcommand_parses_kill_id() {
        let cli = parse_from(argv(&["hht", "shell", "--kill", "42"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: None, kill: Some(id), args }) if id == "42" && args.is_empty()
        ));
    }

    #[test]
    fn rules_changed_dialog_parses_its_file_path() {
        let cli = parse_from(argv(&[
            "hht",
            "__dialog",
            "rules-changed",
            "--path",
            "/tmp/harness-rules.toml",
        ]))
        .expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Dialog(DialogCommand::RulesChanged { path }))
                if path == PathBuf::from("/tmp/harness-rules.toml")
        ));
    }

    #[test]
    fn install_and_uninstall_parse_as_top_level_commands() {
        assert!(matches!(
            parse_from(argv(&["hht", "install"])).unwrap().command,
            Some(Command::Install)
        ));
        assert!(matches!(
            parse_from(argv(&["hht", "uninstall"])).unwrap().command,
            Some(Command::Uninstall)
        ));
    }
}
