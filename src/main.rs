//! The full multi-call `shellglass` binary: every compiled-in mode as a
//! subcommand. The per-mode binaries live in `src/bin/`; all of them dispatch
//! through [`shellglass::cli`].

use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = shellglass::cli::Cli::parse();
    // `push --daemon` forks here, while still single-threaded, before the tokio
    // runtime (and its worker threads) exist. A no-op for every other mode.
    cli.daemonize_if_requested()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(cli.run())
}
