//! seatswap: CLI subcommands, and the tray app when built with the `gui` feature.
//! The two programs `seatswap` and `seatswap-tray` both call [`run`].

mod cli;
mod format;
mod gui;
mod ops;

#[cfg(feature = "gui")]
mod icon;

/// Parse the command line and do what it asks; the whole program.
pub fn run() -> std::process::ExitCode {
    cli::run()
}
