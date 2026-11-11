# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- First version, for Linux: tray icon with the active account's abbreviation and a usage ring,
  pop-up with every account's session and weekly usage, switch suggestion and notification,
  refresh of inactive logins, end-of-login warnings, settings, start at login.
- Command line: `list`, `use`, `next`, `add`, `edit`, `rm`, `status` (`--json`, `--waybar`,
  `--i3blocks`), `panel`.
- Left click on the tray icon opens the pop-up and the tooltip shows the usage, via the
  StatusNotifierItem backend of `tray-icon`.
