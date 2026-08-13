#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "hht-daemon", about = "Harness Hat background service")]
struct Args {
    /// Path to the global Harness Hat configuration.
    #[arg(long)]
    config: PathBuf,
    /// Run without native graphical approval dialogs.
    #[arg(long)]
    headless: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    harness_hat::manager::run_service(args.config, args.headless).await
}
