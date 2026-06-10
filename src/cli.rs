use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(name = "hh", version, about = "Harness Hat — manager UI")]
struct CliOptions {
    /// Path to config file. Used by the interactive workspace manager (the
    /// default action when no subcommand is given).
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands. When none is given, `hh` launches the interactive manager.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Generate a sample config file (defaults to ./harness-hat.toml).
    Init {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Open an interactive shell in a running session. With no id, lists the
    /// running sessions and their ids. Any args after the id are passed
    /// verbatim to `docker exec` as the command to run instead of bash —
    /// e.g. `hh shell 0042 claude --resume`.
    Shell {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<OsString>,
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
    const USAGE: &str = "Usage: hh [--config PATH] [init [PATH] | shell [ID]]";
    if raw.is_empty() {
        bail!("missing argv[0]. {USAGE}");
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
    use super::{Command, parse_from};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = parse_from(argv(&["hh"])).expect("parse");
        assert!(cli.command.is_none());
        assert!(cli.config.is_none());
    }

    #[test]
    fn config_flag_applies_to_default_action() {
        let cli = parse_from(argv(&["hh", "--config", "/tmp/x.toml"])).expect("parse");
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn init_subcommand_takes_optional_path() {
        let cli = parse_from(argv(&["hh", "init"])).expect("parse");
        assert!(matches!(cli.command, Some(Command::Init { path: None })));

        let cli = parse_from(argv(&["hh", "init", "custom.toml"])).expect("parse");
        assert!(
            matches!(cli.command, Some(Command::Init { path: Some(p) }) if p == PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn shell_subcommand_takes_optional_id() {
        let cli = parse_from(argv(&["hh", "shell"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: None, ref args }) if args.is_empty()
        ));

        let cli = parse_from(argv(&["hh", "shell", "0042"])).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Shell { id: Some(id), ref args }) if id == "0042" && args.is_empty()
        ));
    }

    #[test]
    fn shell_subcommand_collects_trailing_args_verbatim() {
        let cli = parse_from(argv(&["hh", "shell", "0042", "claude", "--resume"])).expect("parse");
        let Some(Command::Shell { id, args }) = cli.command else {
            panic!("expected Shell subcommand");
        };
        assert_eq!(id.as_deref(), Some("0042"));
        assert_eq!(
            args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            vec!["claude".to_string(), "--resume".to_string()],
        );
    }
}
