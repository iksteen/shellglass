//! `shellglass-push` — the hub push client as its own binary.
//! Same flags as `shellglass push` (both wrap [`shellglass::cli::PushArgs`]).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass-push",
    version,
    about = "Mirror a terminal and push frames to a remote hub"
)]
struct Cli {
    #[command(flatten)]
    args: shellglass::cli::PushArgs,
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    // Daemonize (if `--daemon`) before the tokio runtime spins up its threads.
    cli.args.daemonize_if_requested()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(cli.args.run())
}
