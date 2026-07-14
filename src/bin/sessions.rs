//! `shellglass-sessions` — manage a hub's sessions over its management API.
//! Same flags as `shellglass sessions` (both wrap [`shellglass::cli::SessionsArgs`]).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass-sessions",
    version,
    about = "Manage a hub's sessions over its management API: list, add, remove, recordings"
)]
struct Cli {
    #[command(flatten)]
    args: shellglass::cli::SessionsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Cli::parse().args.run().await
}
