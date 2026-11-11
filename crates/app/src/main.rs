//! The `seatswap` program: CLI subcommands, and the tray app when built with the
//! `gui` feature. All code is in the library.

fn main() -> std::process::ExitCode {
    seatswap::run()
}
