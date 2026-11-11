# seatswap — notes for every worker

A tray app + CLI that switches Claude Code between several Claude accounts by swapping only the login in Claude Code's files. Design: `docs/spec.md` (what and why), `docs/implementation.md` (module contracts and signatures). Read the sections your task names.

## Layout

- `crates/core` — library `seatswap_core`. No GUI. Modules and their single job are listed at the top of `crates/core/src/lib.rs`.
- `crates/app` — binary `seatswap`: CLI (clap) always; tray + pop-up (Tauri 2) behind the default feature `gui`.
- `phase0/` — Python test scripts from the research phase. Do not touch.
- `docs/` — spec and implementation notes. Do not edit unless the task says so.

## Commands (run from the repository root)

- Build everything: `cargo build --offline --workspace`
- Tests: `cargo test --offline --workspace`
- CLI only (no GUI): `cargo build --offline -p seatswap --no-default-features`
- Lints: `cargo clippy --offline --workspace --all-targets -- -D warnings`

Always pass `--offline`: workers have no network, and every crate the project uses is already downloaded. Do not add dependencies; if a task truly needs one that is not in a `Cargo.toml`, stop and report it. New dependencies need Andreas' approval and are fetched by the lead session, never by a worker.

## Hard rules

1. **Never touch the real Claude Code files.** No code, test or command may read or write `~/.claude`, `~/.claude.json`, `~/.config/seatswap` or the OS keyring of this machine. Tests build their own fake config folder in a `tempfile::tempdir()` and pass paths explicitly (`ClaudePaths::for_config_dir`, `AppPaths::at`). Tests never call `ClaudePaths::resolve`, `AppPaths::resolve` or `store::open_secrets`, and never use `KeyringSecrets`.
2. **No silent defaults, clamps or fallbacks.** An invalid value is an error with a clear message, never quietly replaced. The only allowed fallbacks are the ones the spec names (e.g. keyring → 0600 files, with a warning the UI shows).
3. **One way per job.** Use the module that owns a job (`atomic::write_atomic` for every file write, `paths` for every path, `api` for every HTTP call). Never write a second helper beside an existing one.
4. **Never log or print a token.** At most the last 4 characters of a refresh token.
5. **Keep unknown data.** When rewriting `.credentials.json` or `.claude.json`, every key we do not own stays exactly as it was, in the same order (`serde_json` is built with `preserve_order`).
6. No `unsafe`. No `unwrap()`/`expect()` outside tests, except on values the code just proved present.
7. No git commits.

## Style

Rust 2021, `thiserror` for error enums (one per module), `serde` camelCase for files we own. Tests live in a `#[cfg(test)] mod tests` at the bottom of the module, use real-shaped data (36-character UUIDs, realistic token lengths, real JSON layout of Claude Code's files as shown in `docs/implementation.md`).
