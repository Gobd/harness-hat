use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "hht-daemon", about = "Harness Hat background service")]
struct Args {
    /// Path to the global Harness Hat configuration.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    harness_hat::manager::run_service(args.config).await
}
