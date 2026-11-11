# Contributing to seatswap

Thanks for helping! Bug reports and reports from desktops other than Cinnamon and from Windows
are especially welcome, and so is help with macOS.

## Development setup

Rust ≥ 1.91 and, on Linux, the Tauri libraries listed in the [README](README.md#install).

```sh
cargo build --workspace
cargo run -p seatswap -- --help
```

## Never test against your real login

seatswap rewrites Claude Code's login. While developing, point it at a throwaway setup:

```sh
CLAUDE_CONFIG_DIR=~/cs-test claude          # log in to a test account in its own folder
cargo run -p seatswap -- --claude-config-dir ~/cs-test --data-dir /tmp/sc-data list
```

On Windows use the same two options with Windows folders. Setting `HOME` does not move the home
folder there, so the explicit folders are what keeps a test run away from your real login.

Tests never touch `~/.claude`, `~/.claude.json`, the app's data folder or the system keyring:
they build a fake config folder in a temporary directory and pass every path explicitly. On
Windows, tests that write to the registry use a throw-away key and delete it afterwards.
Keep it that way in new tests.

## Tests and lints

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p seatswap --no-default-features --all-targets -- -D warnings   # CLI only
cargo fmt --check
```

## Looking at the pop-up

The pop-up's page runs in a normal browser with built-in demo data:

```sh
cd crates/app/ui && python3 -m http.server 8765
# open http://127.0.0.1:8765/index.html#main  (or #settings, #add)
```

Screenshots for the docs come from that demo data only.

## Rules the code follows

- **One way per job.** Every file write goes through `atomic::write_atomic`, every path through
  `paths`, every HTTP call through `api`. Extend the module that owns a job instead of adding a
  helper next to it.
- **No silent defaults.** An invalid value is an error with a clear message, never quietly
  replaced.
- **Never print a token,** not even in a test failure; at most the last four characters of a
  refresh token.
- **Keep unknown data.** Rewriting `.credentials.json` or `.claude.json` keeps every key we do not
  own, in the same order.
- No `unsafe`; no `unwrap()`/`expect()` outside tests.

[AGENTS.md](AGENTS.md) has the same rules in the form coding agents read.

## Design documents

[docs/spec.md](docs/spec.md) says what the app does and why;
[docs/implementation.md](docs/implementation.md) holds the module contracts. Change them together
with the code when behaviour changes, and add a line to [CHANGELOG.md](CHANGELOG.md) under
*Unreleased*.
