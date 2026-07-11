//! `shellglass-gen-key` — mint a secret key and print it with its session id.
//! Same behavior as `shellglass gen-key`.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "shellglass-gen-key",
    version,
    about = "Generate a secure random secret key and print it with its session id"
)]
struct Cli {
    /// Mint a management-API key instead: print the key with its API id
    /// (for a hub's `--api-allow`).
    #[arg(long)]
    api: bool,
    #[command(flatten)]
    id_salt: shellglass::cli::IdSaltArg,
}

fn main() -> anyhow::Result<()> {
    let Cli { api, id_salt } = Cli::parse();
    shellglass::cli::gen_key(api, &id_salt)
}
