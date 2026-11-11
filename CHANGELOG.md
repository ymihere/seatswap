# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Windows support (Windows 10 1803+ and 11): the same login files as on Linux, the single-instance
  socket, start at login through the `Run` registry key, notifications under the name
  "seatswap". Windows gets two programs: `seatswap.exe` for the terminal and `seatswap-tray.exe`,
  the same program without a console window, for autostart and the Start menu.

### Fixed
- A switch suggestion or "new login found" notification that timed out or was dismissed opened
  the pop-up, as if it had been clicked. Now only a click on its button or on its body does
  anything.

## [0.1.0] - 2026-11-11

### Added
- First version, for Linux: tray icon with the active account's abbreviation and a usage ring,
  pop-up with every account's session and weekly usage, switch suggestion and notification,
  refresh of inactive logins, end-of-login warnings, settings, start at login.
- Command line: `list`, `use`, `next`, `add`, `edit`, `rm`, `status` (`--json`, `--waybar`,
  `--i3blocks`), `panel`.
- Left click on the tray icon opens the pop-up and the tooltip shows the usage, via the
  StatusNotifierItem backend of `tray-icon`.
- Runs without a system tray (GNOME without the AppIndicator extension, or a desktop panel that
  starts later): it says so and shows the icon as soon as a tray appears; `seatswap panel` opens
  the pop-up meanwhile.

[Unreleased]: https://github.com/ymihere/seatswap/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ymihere/seatswap/releases/tag/v0.1.0
