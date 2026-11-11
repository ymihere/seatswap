#![cfg_attr(windows, windows_subsystem = "windows")]
//! The `seatswap-tray` program: the same as `seatswap`, built without a console
//! window on Windows so that autostart and the Start menu open none. It takes the
//! same arguments and does the same thing; a program without a console prints
//! nothing in a terminal, so use `seatswap` there.

fn main() -> std::process::ExitCode {
    seatswap::run()
}
