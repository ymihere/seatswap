# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.6] - 2026-11-14

### Fixed
- The pop-up's keyboard cursor (the blue ring) no longer stays on an account from an earlier
  opening or a click. It shows only after ↑/↓, starting from the account in use, and is gone
  each time the pop-up opens; until then Enter does nothing.

## [1.0.5] - 2026-11-13

### Added
- Clicking an account's gauge in the pop-up switches to that account, like its Switch button.

## [1.0.4] - 2026-11-13

### Changed
- The pop-up shows the account in use as the bottom card; the other accounts stay above it in
  list order.

## [1.0.3] - 2026-11-13

### Added
- The settings view shows seatswap's version at the right of its header.

## [1.0.2] - 2026-11-12

### Changed
- Pop-up gauges: the outer ring now shows the week and the inner ring the 5 h session; the
  week's percent is the large figure in the middle. On plans without a session limit (Ultra) the
  inner ring is left out instead of showing an empty grey track.
- The usage bars beside the gauge follow the same order: week, session, then the per-model
  limits.

## [1.0.1] - 2026-11-11

### Fixed
- No more "no system tray" notification at login when the desktop's tray starts a moment after
  seatswap (Linux Mint: same second). seatswap now waits 30 s for a tray before it notifies.

## [1.0.0] - 2026-11-11

### Added
- Windows support (Windows 10 1803+ and 11): the same login files as on Linux, the single-instance
  socket, start at login through the `Run` registry key, notifications under the name
  "seatswap". Windows gets two programs: `seatswap.exe` for the terminal and `seatswap-tray.exe`,
  the same program without a console window, for autostart and the Start menu.

### Changed
- A switch also takes Claude Code's two newer renewal locks, `.design_oauth_refresh.lock` and
  `.cloud_sessions_oauth_refresh.lock`, so it cannot land in the middle of a renewal of the
  `designOauth` or `cloudSessionsOauth` login.

### Fixed
- "Start at login" with a program path or a `--data-dir` / `--claude-config-dir` folder that is
  not valid Unicode is now an error. Before, the autostart entry was written with the odd
  characters replaced, which would have started seatswap with another folder.
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

[Unreleased]: https://github.com/ymihere/seatswap/compare/v1.0.6...HEAD
[1.0.6]: https://github.com/ymihere/seatswap/releases/tag/v1.0.6
[1.0.5]: https://github.com/ymihere/seatswap/releases/tag/v1.0.5
[1.0.4]: https://github.com/ymihere/seatswap/releases/tag/v1.0.4
[1.0.3]: https://github.com/ymihere/seatswap/releases/tag/v1.0.3
[1.0.2]: https://github.com/ymihere/seatswap/releases/tag/v1.0.2
[1.0.1]: https://github.com/ymihere/seatswap/releases/tag/v1.0.1
[1.0.0]: https://github.com/ymihere/seatswap/releases/tag/v1.0.0
[0.1.0]: https://github.com/ymihere/seatswap/releases/tag/v0.1.0
