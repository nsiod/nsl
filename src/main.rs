// Unsafe is forbidden crate-wide on Unix. On Windows, the `platform::windows`
// module needs it to call Win32 FFI (OpenProcess, TerminateProcess, ...);
// those are the only unsafe blocks in the crate.
#![cfg_attr(all(not(test), not(windows)), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

use clap::Parser;

mod certs;
mod cli;
mod config;
mod discover;
mod error;
mod hosts;
mod npx_guard;
mod pages;
mod platform;
mod proxy;
mod routes;
mod run;
mod status;
mod utils;

fn main() -> anyhow::Result<()> {
    npx_guard::check_npx_execution();

    let cli = cli::Cli::parse();

    // The daemonize subcommand must fork BEFORE any tokio runtime exists --
    // forking an active runtime leaves the child with corrupt worker state.
    // `daemonize_and_start_proxy` handles its own runtime construction in the
    // daemonized child.
    if let cli::Commands::Start {
        daemonize: true,
        listen,
        https,
        ..
    } = &cli.command
    {
        let mut config = config::load_config();
        cli::apply_cli_overrides(&mut config, listen.clone(), *https)?;
        return proxy::daemonize_and_start_proxy(&config);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(cli.run())
}
