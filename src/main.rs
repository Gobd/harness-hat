use anyhow::{Context, Result};

// Multi-thread runtime so the control server and proxy stay responsive even
// when the TUI thread blocks (stalled terminal emulator, synchronous docker
// calls). The TUI itself runs on a dedicated thread with its own
// current-thread runtime (see `manager::run`) because ContainerSession
// contains Box<dyn MasterPty>, which is !Send and must stay on one thread.
#[tokio::main]
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
                    reason,
                    image,
                    cwd,
                    timeout_secs,
                    workspace,
                } => {
                    let req = harness_hat::native_approval::HostdoApprovalRequest {
                        command,
                        reason,
                        cwd,
                        image,
                        timeout_secs,
                        workspace,
                    };
                    harness_hat::native_approval::run_hostdo_approval(&req)
                }
                DialogCommand::RulesChanged { path } => {
                    let req = harness_hat::native_approval::RulesChangedRequest { path };
                    harness_hat::native_approval::run_rules_changed_dialog(&req)
                }
            };
            println!("{}", outcome.encode());
            return Ok(());
        }
        Some(Command::Init { path }) => {
            let path = path.unwrap_or_else(|| std::path::PathBuf::from("harness-hat.toml"));
            harness_hat::init::write_sample_config(&path)?;
            println!("config written to: {}", path.display());
            println!("Run `hht install` to create and start the default background service.");
        }
        Some(Command::Shell { id, kill, args }) => {
            // Pure-Docker passthrough; intentionally bypasses manager init.
            let code = harness_hat::shell::run(id, kill, args)?;
            std::process::exit(code);
        }
        Some(Command::Workspace {
            list,
            template,
            name,
            rebuild,
            args,
        }) => {
            // `workspace::run` uses `reqwest::blocking`, which spins up an
            // inner tokio runtime and drops it on the way out. Doing that
            // from inside `#[tokio::main]`'s runtime context panics
            // ("Cannot drop a runtime in a context where blocking is not
            // allowed"), so move the whole synchronous flow onto the
            // blocking pool — the inner runtime drop happens off the main
            // runtime thread.
            let code = tokio::task::spawn_blocking(move || {
                harness_hat::workspace::run(args, list, template, name, rebuild, None)
            })
            .await
            .context("workspace task panicked")??;
            std::process::exit(code);
        }
        Some(Command::Rebuild {
            no_cache,
            templates,
        }) => {
            harness_hat::rebuild::run(templates, no_cache, None)?;
        }
        Some(Command::Install) => {
            harness_hat::service::install(None)?;
        }
        Some(Command::Uninstall) => {
            harness_hat::service::uninstall()?;
        }
        None => harness_hat::manager::run().await?,
    }
    Ok(())
}
