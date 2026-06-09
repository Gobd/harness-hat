use anyhow::Result;

// current_thread keeps all async tasks on one thread, which allows
// ContainerSession (containing Box<dyn MasterPty>, which is !Send) to be
// held in App across await points in the TUI event loop.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    use harness_hat::cli::Command;

    let cli = harness_hat::cli::parse()?;
    match cli.command {
        Some(Command::Init { path }) => {
            let path = path.unwrap_or_else(|| std::path::PathBuf::from("harness-hat.toml"));
            harness_hat::init::write_sample_config(&path)?;
            println!("config written to: {}", path.display());
            println!("edit it, then run: hh --config {}", path.display());
        }
        Some(Command::Shell { id }) => {
            // Pure-Docker passthrough; intentionally bypasses manager init.
            let code = harness_hat::shell::run(id)?;
            std::process::exit(code);
        }
        None => harness_hat::manager::run(cli).await?,
    }
    Ok(())
}
