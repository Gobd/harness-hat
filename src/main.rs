use anyhow::{Context, Result};

// current_thread keeps all async tasks on one thread, which allows
// ContainerSession (containing Box<dyn MasterPty>, which is !Send) to be
// held in App across await points in the TUI event loop.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    use harness_hat::cli::Command;

    let cli = harness_hat::cli::parse()?;
    match cli.command {
        Some(Command::Dialog(dialog_cmd)) => {
            // Subprocess entry point — pop a native dialog, print the result
            // line to stdout, exit. Runs on the OS main thread (this whole
            // subprocess does nothing else) so AppKit/Win32 modal calls are
            // happy. Tokio is still spun up by `#[tokio::main]` but is idle.
            use harness_hat::cli::DialogCommand;
            let outcome = match dialog_cmd {
                DialogCommand::NetworkApproval {
                    host,
                    method,
                    path,
                    port,
                    workspace,
                } => {
                    let req = harness_hat::native_approval::ApprovalRequest {
                        host,
                        method,
                        path,
                        port,
                        workspace,
                    };
                    harness_hat::native_approval::run_network_approval(&req)
                }
                DialogCommand::HostdoApproval {
                    command,
                    image,
                    cwd,
                    timeout_secs,
                    workspace,
                } => {
                    let req = harness_hat::native_approval::HostdoApprovalRequest {
                        command,
                        cwd,
                        image,
                        timeout_secs,
                        workspace,
                    };
                    harness_hat::native_approval::run_hostdo_approval(&req)
                }
            };
            println!("{}", outcome.encode());
            return Ok(());
        }
        Some(Command::Init { path }) => {
            let path = path.unwrap_or_else(|| std::path::PathBuf::from("harness-hat.toml"));
            harness_hat::init::write_sample_config(&path)?;
            println!("config written to: {}", path.display());
            println!("edit it, then run: hh --config {}", path.display());
        }
        Some(Command::Shell { id, args }) => {
            // Pure-Docker passthrough; intentionally bypasses manager init.
            let code = harness_hat::shell::run(id, args)?;
            std::process::exit(code);
        }
        Some(Command::Workspace { template, args }) => {
            // `workspace::run` uses `reqwest::blocking`, which spins up an
            // inner tokio runtime and drops it on the way out. Doing that
            // from inside `#[tokio::main]`'s runtime context panics
            // ("Cannot drop a runtime in a context where blocking is not
            // allowed"), so move the whole synchronous flow onto the
            // blocking pool — the inner runtime drop happens off the main
            // runtime thread.
            let config = cli.config.clone();
            let code = tokio::task::spawn_blocking(move || {
                harness_hat::workspace::run(args, template, config)
            })
            .await
            .context("workspace task panicked")??;
            std::process::exit(code);
        }
        None => harness_hat::manager::run(cli).await?,
    }
    Ok(())
}
