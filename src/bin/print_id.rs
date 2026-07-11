//! `shellglass-print-id` — print the session id for a key.
//! Same behavior as `shellglass print-id`.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass-print-id",
    version,
    about = "Print the session id for a key (to add to a hub's --allow)"
)]
struct Cli {
    #[command(flatten)]
    key: shellglass::cli::KeyArg,
    /// Print the key's API id instead (for a hub's `--api-allow`).
    #[arg(long)]
    api: bool,
}

fn main() -> anyhow::Result<()> {
    let Cli { key, api } = Cli::parse();
    shellglass::cli::print_id(&key, api)
}
