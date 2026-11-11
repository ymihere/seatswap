//! seatswap binary: CLI subcommands, and the tray app when built with the `gui` feature.

mod cli;
mod format;
mod gui;
mod ops;

#[cfg(feature = "gui")]
mod icon;

fn main() -> std::process::ExitCode {
    cli::run()
}
