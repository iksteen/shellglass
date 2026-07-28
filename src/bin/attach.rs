//! `shellglass-attach` — attach a terminal to a detached `push --detachable`
//! session, as its own binary. Same flags as `shellglass attach` (both wrap
//! [`shellglass::cli::AttachArgs`]). Unix only.

#[cfg(unix)]
use clap::Parser;

#[cfg(unix)]
#[derive(Parser, Debug)]
#[command(
    name = "shellglass-attach",
    version,
    about = "Attach this terminal to a detached shellglass push session"
)]
struct Cli {
    #[command(flatten)]
    args: shellglass::cli::AttachArgs,
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    Cli::parse().args.run()
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("shellglass-attach is only supported on Unix");
}
