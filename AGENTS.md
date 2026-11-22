# seatswap — notes for every worker

A tray app + CLI for Linux, Windows and macOS that switches Claude Code between several Claude accounts by swapping only the login in Claude Code's files (on macOS: its Keychain item). Design: `docs/spec.md` (what and why), `docs/implementation.md` (module contracts and signatures), `docs/windows.md` (the Windows contracts and what was checked there). Read the sections your task names.

## Layout

- `crates/core` — library `seatswap_core`. No GUI. Modules and their single job are listed at the top of `crates/core/src/lib.rs`.
- `crates/app` — library `seatswap` with two programs that both call its `run()`: `seatswap`, and `seatswap-tray` (the same program without a console window on Windows). CLI (clap) always; tray + pop-up (Tauri 2) behind the default feature `gui`.
- `phase0/` — Python test scripts from the research phase. Do not touch.
- `docs/` — spec and implementation notes. Do not edit unless the task says so.

## Commands (run from the repository root)

- Build everything: `cargo build --offline --workspace`
- Tests: `cargo test --offline --workspace`
- CLI only (no GUI): `cargo build --offline -p seatswap --no-default-features`
- Lints: `cargo clippy --offline --workspace --all-targets -- -D warnings`

Always pass `--offline`: every crate the project uses is already downloaded, and a build must not fetch anything. A worker's shell can reach the network; no code, test or command may use it. Do not add dependencies; if a task truly needs one that is not in a `Cargo.toml`, stop and report it. New dependencies need Andreas' approval and are fetched by the lead session, never by a worker.

## Hard rules

1. **Never touch the real Claude Code files.** No code, test or command may read or write `~/.claude`, `~/.claude.json`, seatswap's data folder (`~/.config/seatswap`, on Windows `%APPDATA%\seatswap`), Codex's folder (`~/.codex`, or `$CODEX_HOME`), DeepSeek Harness's folder (`~/.dsh`, or `$DSH_HOME`), flash-agents' state folder (`~/.local/state/flash-agents`, or `$XDG_STATE_HOME/flash-agents`; on Windows `%LOCALAPPDATA%\flash-agents`; or `$FLASH_DATA_DIR`) or the OS keyring of this machine, or call a real Ollama server. Tests build their own fake config folder in a `tempfile::tempdir()` and pass paths explicitly (`ClaudePaths::for_config_dir`, `AppPaths::at`, Codex's and Harness's folders, Dropbox's `info.json` and the Ollama URL likewise). Tests never call `ClaudePaths::resolve`, `AppPaths::resolve`, `CodexPaths::resolve`, `HarnessPaths::resolve`, `paths::dropbox_folder` or `store::open_secrets`, and never use `KeyringSecrets`. On macOS `ClaudePaths::resolve` names a Keychain item in the user's default keychain even for a temporary folder, which is one more reason tests never call it; a Keychain test names a throw-away keychain file it made with `security create-keychain` in a tempdir, and every `security` command a worker runs by hand names such a file too (for a missing file `security` quietly uses the default keychain). On Windows, setting `HOME` does not move the home folder, so only the explicit paths protect a test; registry tests write only under a throw-away key `HKCU\Software\seatswap-test-…` and delete it; and never start the tray app (`seatswap` or `seatswap-tray` without a subcommand) on a machine with a real login.
2. **No silent defaults, clamps or fallbacks.** An invalid value is an error with a clear message, never quietly replaced. The only allowed fallbacks are the ones the spec names (e.g. keyring → 0600 files, with a warning the UI shows).
3. **One way per job.** Use the module that owns a job (`atomic` for every file write, file mode and private folder; `paths` for every path; `api` for every HTTP call; `gui/registry.rs` for the Windows registry). Never write a second helper beside an existing one. A `cfg` for the operating system belongs in the module that owns the difference, not at its callers.
4. **Never log or print a token.** At most the last 4 characters of a refresh token.
5. **Keep unknown data.** When rewriting `.credentials.json` or `.claude.json`, every key we do not own stays exactly as it was, in the same order (`serde_json` is built with `preserve_order`).
6. No `unsafe`. No `unwrap()`/`expect()` outside tests, except on values the code just proved present.
7. No git commits.

## Parallel work

Work is planned so that several agents can build at once without a mess when their changes come together.

- **A task owns its files.** Its brief lists every file it may change; it changes no other file. Two tasks that need the same file never run at the same time: one waits until the other is committed.
- **Shared files are not edited by parallel tasks.** `CHANGELOG.md`, `docs/spec.md`, `docs/implementation.md`, `README.md`, `crates/core/src/lib.rs`, `crates/app/ui/fixture.js` and `crates/app/ui/common.css` are changed only by a task of their own, or by the lead after the code tasks of a batch are in. A task that needs a line there says so in its report instead of writing it.
- **Interfaces are fixed before work starts.** Names, signatures and data shapes that one task uses from another are written in the plan, so no task waits for another's code or guesses it.
- **Every task starts from a committed tree** and is checked and committed on its own before a task that uses it starts. A change that no longer applies cleanly is built again on the new tree, not merged by hand.
- **One task, one commit**, with the checks passing on that commit alone.

## Style

Rust 2021, `thiserror` for error enums (one per module), `serde` camelCase for files we own. Tests live in a `#[cfg(test)] mod tests` at the bottom of the module, use real-shaped data (36-character UUIDs, realistic token lengths, real JSON layout of Claude Code's files as shown in `docs/implementation.md`).
