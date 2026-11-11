# seatswap — Windows port (spec for the implementer)

Written 2026-11-11, after an investigation on a Windows 11 machine. Read `AGENTS.md` first; its hard rules apply to every line of this work. `docs/spec.md` says what the program does, `docs/implementation.md` fixes the existing module contracts. This file adds the contracts for Windows. Signatures here are binding; bodies are free.

## 1. Goal

`seatswap` builds, passes its tests and runs on Windows 10 (1803 or later) and Windows 11, with the same behaviour as on Linux. Linux behaviour does not change, except for part C and the notification fix that is already in the tree (section 6.6, point 3).

Done means: the six commands in section 9 succeed on Windows, and the report in section 11 is written.

## 2. Ground rules for this work

1. **You work on Windows and cannot run the Linux build.** Cross-checking from Windows fails (`libdbus-sys` needs a Linux `pkg-config`), and WSL on this machine has no Rust. So on the Unix side: move code behind `#[cfg(unix)]` / `#[cfg(not(windows))]` without rewriting it. Every line you change that Linux also compiles goes into your report, so it can be reviewed and run on Linux before release.
2. **Never run the tray app.** Do not start `seatswap.exe` or `seatswap-tray.exe` without a subcommand. A running tray app polls the network, writes the autostart entry of this machine, and may touch the OS keyring. Andreas does the manual run (section 10). Running CLI subcommands by hand is allowed only the way `crates/app/tests/cli.rs` does it: explicit `--data-dir` and `--claude-config-dir` in a temp folder, with `"secretsBackend":"file"` already recorded in `accounts.json`.
3. **`HOME` does not protect you on Windows.** `dirs::home_dir()` asks Windows for the profile folder and ignores the `HOME` variable. The integration tests stay safe only because they pass both folders explicitly and record the file backend. Never write a test that relies on `HOME`, and never call `ClaudePaths::resolve`, `AppPaths::resolve` or `store::open_secrets` in a test (AGENTS.md rule 1).
4. **Registry in tests:** only under a throw-away key `HKCU\Software\seatswap-test-<uuid>`, deleted at the end of the test. Never the real `Run` key, never `Software\Classes`.
5. **One place per platform difference.** A `cfg` for the operating system may appear only in: `core/src/atomic.rs`, `app/src/gui/ipc.rs` (the socket import and the socket mode), `app/src/gui/autostart.rs`, `app/src/gui/registry.rs`, `app/src/gui/notify.rs` (the app id; the `default` action in `clickable()` is there already), `app/src/gui/tray.rs` (tooltip length), the two places that already have one (`panel.rs`, `tray.rs`), and tests. If you need one anywhere else, stop and report.
6. **Stop and report** instead of deciding yourself when: a contract here does not fit the code, a third dependency seems needed, a test can only pass by weakening an assertion that holds on Unix, or anything in section 3 turns out to be wrong.

## 3. Facts already checked (do not re-derive)

Checked on Windows 11 with Claude Code 2.1.322, Rust 1.99, the crate versions in `Cargo.lock`.

| Fact | Consequence |
|---|---|
| Claude Code keeps the login in `%USERPROFILE%\.claude\.credentials.json` and the identity in `%USERPROFILE%\.claude.json`, same JSON keys as on Linux. Nothing is in Windows Credential Manager. | `paths.rs` and `live.rs` logic is right as it is. |
| Claude Code's locks are the same folders with the same names and time limits on Windows. | `locks.rs` logic is right as it is. |
| `File::open(folder)` fails with "Access is denied" (OS error 5). | The folder sync in `write_atomic` cannot run on Windows. |
| `fs::rename` over a file that another program holds open **without** delete sharing fails with OS error 5; with delete sharing it works. | `write_atomic` needs a short retry (section 5.1). |
| `fs::create_dir`, `fs::remove_dir` and `filetime::set_file_mtime` work on lock folders. | `DirLock` needs no change. |
| A file locked with `fs4` cannot be read by another handle (OS error 33). | Harmless: `switch.lock` and `gui.lock` are only locked, never read. Do not start reading them. |
| `fs::canonicalize` returns `\\?\C:\…`. | `legacy_refresh_lock` then is `\\?\C:\Users\x\.claude.lock`: the same folder Claude Code names `C:\Users\x\.claude.lock`. Keep `canonicalize`; add the test in section 5.4. |
| `dirs::config_dir()` is `%APPDATA%` (Roaming); `dirs::runtime_dir()` is `None`. | Data folder `%APPDATA%\seatswap`; the socket is `<data>\gui.sock`. |
| `Path::new("/home/u").is_absolute()` is `false`. | Test fixtures that need an absolute path must build it from a temp folder. |
| The file watcher reports create, modify, remove and rename events for a temp-file-plus-rename write, and nothing for a read. | `watcher.rs` needs no change; its tests must pass as they are. |
| Windows permissions are inherited from the folder; there are no mode bits. | "Private" on Windows means "whatever the user's folder allows". No ACL code (out of scope, section 12). |
| Keyring: Windows allows 2560 bytes per entry; a login is about 1100 bytes as UTF-16; the crate returns an error when one is too long. | No change. |
| Tray (tray-icon 0.25 on Windows): left and right clicks arrive as `Click` events with `Up`/`Down` and the icon rectangle; the tooltip and menu texts update live; `set_title` does nothing; the tooltip is cut without notice at 128 UTF-16 units. | Tray code works as it is, except the tooltip (section 6.7). |
| Pop-up: a borderless, always-on-top window opens at the click, gets focus, and hides on focus loss. | `panel.rs` works with its existing non-Linux branches. |
| notify-rust without an app id shows notifications under the name "Windows PowerShell". | Section 6.6. |
| `tauri-build` on Windows stops without `icons/icon.ico`. | Section 6.2. |
| Unix sockets work on this Windows. Rust's own `std::os::windows::net` exists but is unstable (issue 150487). The crate `uds_windows` 1.2.1 has `UnixListener::{bind, accept, set_nonblocking}` and `UnixStream::{connect, try_clone, set_read_timeout, set_write_timeout}`. | Section 6.3. |

## 4. Dependencies

Two new direct dependencies in `crates/app/Cargo.toml`, approved by Andreas on 2026-11-11 and **already added by the lead session** (also `notify-rust` now asks for 4.19, the version in `Cargo.lock`). Do not edit the dependency sections. Both crates were in `Cargo.lock` and downloaded before (`uds_windows` through notify-rust, `winreg` through Tauri's build tooling), so `--offline` keeps working. What is there:

```toml
[features]
gui = [ …existing entries…, "dep:winreg" ]

[target.'cfg(windows)'.dependencies]
# Unix sockets for the single-instance socket, until std::os::windows::net is stable
# (rust-lang/rust#150487); then this crate goes and ipc.rs imports from std.
uds_windows = "1.2"
# Autostart (Run key) and the notification name.
winreg = { version = "0.56", optional = true }
```

Nothing else may be added.

## 5. Part A — core library (`crates/core`)

After this part, `cargo test --offline -p seatswap-core` passes on Windows.

### 5.1 `atomic.rs` — the only file with Unix file modes

`atomic.rs` becomes the one module that knows about file modes and how to create private files and folders. No other core module imports `std::os::unix` outside tests. Update the module list at the top of `lib.rs` to say so.

```rust
/// Who may read a file seatswap creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Private, // Unix: mode 0600
    Shared,  // Unix: mode 0644
}

/// Unchanged contract, new third parameter. An existing file keeps its mode (Unix).
pub fn write_atomic(path: &Path, bytes: &[u8], access_if_new: Access) -> io::Result<()>;

/// Unchanged contract. The backup is written with Access::Private.
pub fn backup_once(path: &Path) -> io::Result<bool>;

/// Make sure `dir` exists as a folder only its owner can use.
/// Missing: created with its parents; on Unix the mode is then set to 0700 explicitly
/// (the umask filters DirBuilder's mode). Existing folder: left exactly as it is.
/// Exists but is not a folder: io::ErrorKind::AlreadyExists, message "exists but is not a folder".
pub fn ensure_private_dir(dir: &Path) -> io::Result<()>;

/// Tighten an existing folder to its owner. Unix: chmod 0700. Windows: nothing to set, Ok(()).
pub fn restrict_dir(dir: &Path) -> io::Result<()>;

/// Open `path` for reading and writing, creating it when missing, never truncating.
/// A file created here is private (Unix: mode 0600).
pub fn open_private(path: &Path) -> io::Result<File>;
```

Of the 16 call sites of `write_atomic` (tests included), 15 pass `Access::Private`; `autostart.rs` passes `Access::Shared`. Remove the second `fs::set_permissions` in `backup_once`: the backup never exists before, so `access_if_new` already applies.

`write_atomic` steps:

| Step | Unix (exactly as today) | Windows |
|---|---|---|
| Mode | the existing file's mode, else from `access_if_new` | none |
| Temp file | `create_new` in the same folder with that mode, then `set_permissions` | `create_new` in the same folder |
| Content | `write_all`, `sync_all`, close | same |
| Replace | `fs::rename(tmp, path)`, one attempt | `fs::rename(tmp, path)` with the retry below |
| Folder sync | `File::open(dir)?.sync_all()` | not done |
| On any error | remove the temp file, return the error | same |

Windows retry: when `rename` fails with raw OS error 5 (access denied) or 32 (sharing violation), wait and try again: first wait 10 ms, doubling up to 200 ms, until `RENAME_RETRY` = 2 s have passed since the first attempt. Then return the last OS error unchanged. Any other error returns at once. Put the loop in a private function that takes the time budget as a parameter, so tests can use a short one. On Windows the replaced file gets the permissions of its folder; Claude Code's own writes do the same.

Tests:
- The two existing tests keep every assertion on Unix (`#[cfg(unix)]` where they read modes).
- All platforms: a new file has the given content; replacing keeps no temp file in the folder; a failed write (target folder missing) leaves no temp file.
- Windows only (`std::os::windows::fs::OpenOptionsExt::share_mode`, `1 | 2` = read + write sharing, no delete sharing):
  - a second thread holds the target open without delete sharing for 300 ms: the write succeeds and the file has the new content;
  - the holder never lets go, budget 200 ms: the error has `raw_os_error() == Some(5)`, the file has the old content, no temp file is left;
  - the holder opens with delete sharing (`1 | 2 | 4`): the write succeeds at once.
- All platforms: `ensure_private_dir` creates nested folders, leaves an existing folder alone, and rejects a file in the folder's place. Unix only: the new folder has mode 0700, an existing folder with 0755 keeps it, `restrict_dir` makes it 0700, `open_private` creates with 0600.

### 5.2 `locks.rs`

- Remove the `std::os::unix` import and the private `ensure_data_dir`.
- `acquire_file_lock`: the parent folder through `atomic::ensure_private_dir`, the file through `atomic::open_private`; errors mapped to `LockError::Io { path, source }` as today.
- `DirLock` and everything about Claude Code's lock folders: unchanged.
- Tests that read modes become `#[cfg(unix)]`; all others must pass on Windows unchanged.

### 5.3 `store.rs`, `engine.rs`

- Remove `store::ensure_dir_0700`. Its three callers (`store.rs` twice, `engine.rs` once) call `atomic::ensure_private_dir` and map the error as before. This also removes the duplicate of `locks::ensure_data_dir`.
- `FileSecrets::ensure_dir` = `ensure_private_dir`, then `restrict_dir`.
- No `std::os::unix` import outside tests.

### 5.4 `paths.rs`

- Replace `OsStrExt::as_bytes()` with `OsStr::as_encoded_bytes()` (std, same bytes on Unix) in `gui_socket`, for the hash and for the length check. Remove the `std::os::unix` import. The pinned hash in the test (`0xb6fe_9d8b_b8b6_e275`) must not change.
- `PathsError::NoHome` text becomes "cannot find the home folder".
- `gui_socket`, `legacy_refresh_lock`, `absolute`: logic unchanged.
- New Windows-only test: create the folder `ClaudePaths::legacy_refresh_lock()` returns, then show that `fs::create_dir` of the plain path (`<temp config folder>` + `.lock`, without `\\?\`) fails with `AlreadyExists`. It proves seatswap and Claude Code name the same lock folder.
- Tests with symlinks or with `strip_prefix("/")` become `#[cfg(unix)]`. Tests that use `/run/user/1000` or `/home/u` only as a value to join or compare stay on all platforms. `absolute_joins_a_relative_folder_to_the_current_one`: keep the relative half everywhere; the absolute half uses a path under `std::env::temp_dir()`.

### 5.5 Rule for every test in both crates

- A test of behaviour that only exists on Unix (file modes, symlinks, making a folder unwritable with `chmod` to force a failure, the XDG autostart file) gets `#[cfg(unix)]` and keeps every assertion.
- Every other test must pass on both platforms. Where it needs an absolute path, it builds one from `tempfile::tempdir()` or `std::env::temp_dir()`.
- Test helpers named `write_private` (in `live.rs`, `switcher.rs`, `tests/cli.rs`) set mode 0600 on Unix and only write on Windows.
- Do not delete a test. Report how many tests run on Windows and how many are Unix-only.

## 6. Part B — app (`crates/app`)

### 6.1 One library, two programs

A program started from the Windows desktop or at login shows a console window unless it is built without one; a program built without one prints nothing in a terminal. So Windows gets the same program twice.

```toml
[lib]
name = "seatswap"
path = "src/lib.rs"

[[bin]]
name = "seatswap"
path = "src/main.rs"

[[bin]]
name = "seatswap-tray"
path = "src/bin/seatswap-tray.rs"
required-features = ["gui"]
```

- `src/lib.rs`: the `mod` lines that are in `main.rs` today, plus `pub fn run() -> std::process::ExitCode { cli::run() }`. Nothing else becomes `pub`.
- `src/main.rs`: `fn main() -> std::process::ExitCode { seatswap::run() }`.
- `src/bin/seatswap-tray.rs`: first line `#![cfg_attr(windows, windows_subsystem = "windows")]`, then the same `main`.

Both programs take the same arguments and do the same thing. `seatswap-tray` exists so that autostart and the Start menu have a program without a console. Cargo cannot build a program for one platform only, so Linux builds it too; it is not installed or released there. `tests/cli.rs` keeps using `CARGO_BIN_EXE_seatswap`.

### 6.2 Icon

- Add `crates/app/icons/icon.ico`. This recipe wraps the existing 256×256 PNG and was accepted by `tauri-build` (run from `crates/app`):
  ```
  python -I -c "import struct;d=open('icons/icon.png','rb').read();open('icons/icon.ico','wb').write(struct.pack('<HHH',0,1,1)+struct.pack('<BBBBHHII',0,0,0,0,1,32,len(d),22)+d)"
  ```
- `tauri.conf.json`: `"icon": ["icons/icon.png", "icons/icon.ico"]`.
- `.gitattributes`: add `*.ico binary`.
- The Windows build writes `gen/schemas/windows-schema.json`. Keep it; `linux-schema.json` is checked in the same way.

### 6.3 `gui/ipc.rs` — the same Unix socket on Windows

```rust
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};
```

- Setting the socket file to mode 0600 is `#[cfg(unix)]`; on Windows the file has the data folder's permissions.
- Everything else stays one code path: lock, leftover-socket removal, protocol, timeouts.
- On Windows `paths::runtime_dir()` is `None`, so the socket is `<data>\gui.sock`.
- All six existing `ipc` tests must pass on Windows. Add one for all platforms: a socket file left behind by a crash (bind a plain `UnixListener` to `<data>/gui.sock` and drop it, which leaves the file) does not stop `Listener::start`, and `send_open_panel` then reaches the new listener.

### 6.4 `gui/registry.rs` — new, `#[cfg(windows)]`, the only module that names `winreg`

```rust
pub(crate) const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub(crate) const APP_ID: &str = "dev.seatswap.app"; // the identifier in tauri.conf.json
pub(crate) const APP_ID_KEY: &str = r"Software\Classes\AppUserModelId\dev.seatswap.app";

// All three work under HKEY_CURRENT_USER; `key` is the path below it.
pub(crate) fn set_string(key: &str, name: &str, value: &str) -> io::Result<()>;     // creates the key when missing
pub(crate) fn get_string(key: &str, name: &str) -> io::Result<Option<String>>;      // missing key or value: None
pub(crate) fn delete_value(key: &str, name: &str) -> io::Result<()>;                // missing key or value: Ok
```

Tests (throw-away key, rule 2.4): set then get; overwrite; delete; delete of a missing value; get from a missing key.

### 6.5 `gui/autostart.rs`

Stays the one entry point: `apply_autostart(enabled, arguments) -> Result<(), String>`, called at start and on `save_settings` as today. `AutostartError` and `startup_arguments` are shared. Only where the entry is stored differs.

- `#[cfg(not(windows))]`: the XDG code exactly as it is (`config_home_from`, `config_home`, `quote_argument`, `exec_line`, `apply`), with `Access::Shared`.
- `#[cfg(windows)]`:

```rust
const RUN_VALUE: &str = "seatswap";
const RUN_COMMAND_LIMIT: usize = 260; // Microsoft's documented limit for a Run command (not tested here)

/// The program the entry starts: `seatswap-tray.exe` in the folder of `current_exe`.
/// Missing: AutostartError::NoTrayProgram { path } ("cannot start at login: <path> is missing").
fn tray_program(current_exe: &Path) -> Result<PathBuf, AutostartError>;

/// `"<exe>" "<arg>" …` with Windows quoting. A path or argument that is not valid Unicode:
/// AutostartError::NotUnicode { path }. Longer than RUN_COMMAND_LIMIT UTF-16 units:
/// AutostartError::CommandTooLong { length }. Never shortened.
fn command_line(exe: &Path, arguments: &[String]) -> Result<String, AutostartError>;

/// enabled: set value RUN_VALUE under `run_key` to the command line. Not enabled: delete it.
pub(crate) fn apply(enabled: bool, current_exe: &Path, run_key: &str, arguments: &[String]) -> Result<(), AutostartError>;
```

`apply_autostart` passes `registry::RUN_KEY`. Quoting rule (the one Windows programs parse their arguments with): every part is wrapped in double quotes; inside, a `"` is written `\"`; backslashes are literal, except that a run of backslashes directly before a `"` or at the end of the part is doubled.

| Part | Written as |
|---|---|
| `C:\Program Files\seatswap\seatswap-tray.exe` | `"C:\Program Files\seatswap\seatswap-tray.exe"` |
| `--data-dir` | `"--data-dir"` |
| `C:\my data` | `"C:\my data"` |
| `C:\dir\` | `"C:\dir\\"` |
| `a"b` | `"a\"b"` |
| `a\"b` | `"a\\\"b"` |

Tests on Windows: the table above; `tray_program` finds a sibling file and reports a missing one; `apply` under a throw-away key writes the value, replaces it, and removes it; a command of 261 units is refused and nothing is written. The existing XDG tests move behind `#[cfg(not(windows))]` unchanged.

### 6.6 `gui/notify.rs`

1. **One constructor.** A private `fn base() -> DesktopNotification` sets the summary `"seatswap"` and, on Windows, `.app_id(registry::APP_ID)`. It is the only place that calls `DesktopNotification::new()`: the three plain senders build from it, and the existing `clickable()` (point 3) builds on it, so the two clickable senders no longer set the summary themselves.
2. **Notification name on Windows.** `pub(crate) fn register_app_id() -> Result<(), String>` (`#[cfg(windows)]`) writes `set_string(APP_ID_KEY, "DisplayName", "seatswap")`. `gui::setup` calls it once, before `ipc::Listener::start` (the first place that can send a notification); a failure goes into `startup_errors` as `notification name: <error>`. This is the way Microsoft documents for programs without an installer. It has **not** been tried on this machine; it is on Andreas' checklist. Do not add a fallback to PowerShell's id.
3. **React only to real clicks — already done by the lead session, both platforms; do not redo it.** Before, every answer that was not the button counted as a click on the body, so a notification that timed out or was dismissed opened the pop-up. Now `send_suggestion` and `send_unknown_login` use `wait_for_response` (same call on Linux and Windows, type `notify_rust::NotificationResponse`) and decide in the pure, tested `click(response, button) -> Option<Click>`:

| Response | Suggestion (`button` = `switch`) | Unknown login (`button` = `open`) |
|---|---|---|
| `Action(button)` | switch (`UnsavedPolicy::Refuse`) | open the `add` view |
| `Default` (click on the body) | open the `main` view | open the `add` view |
| `Closed(_)`, `Reply(_)`, any other action | nothing | nothing |

`clickable()` announces the action `default` on Linux only (`cfg!(all(unix, not(target_os = "macos")))`): a Linux notification service reports a body click only for an announced `default` action and shows no button for it; Windows reports a body click by itself and would draw a button. The code was compiled and its test run against notify-rust's Windows backend in a scratch crate, not inside this repository (the core library did not build on Windows yet), and never on Linux. Your part: it compiles and its test passes here once the app builds; if it does not, fix the cause and say so in the report.

### 6.7 `gui/tray.rs` — tooltip length

```rust
/// `text` if it has at most `max_units` UTF-16 units; otherwise its longest prefix that,
/// with "…" appended, has at most `max_units` units. Never splits a character.
fn fit_tooltip(text: &str, max_units: usize) -> String;
```

On Windows the two places that hand the tooltip to the tray (`build`, `update`) pass it through `fit_tooltip(…, 127)`; elsewhere the text goes through unchanged. Tests on all platforms: short text unchanged; exactly 127 units unchanged; 128 units cut to 126 + "…"; a text whose cut point falls inside a two-unit character (an emoji) is cut before it.

### 6.8 `tests/cli.rs`

- The socket import as in 6.3; `write_private` as in 5.5.
- A comment at the top states rule 2.3: on Windows `HOME` is not what keeps the tests away from the real files.
- All tests must pass on Windows, including `panel_sends_the_view_to_the_running_app`.

### 6.9 Remaining compile errors

The compiler stopped at the core library during the investigation, so errors in the app that only show up afterwards are not listed here. Fix them in the same spirit (rule 2.5). The X11 focus code and the Linux tray waiting already have non-Linux branches; do not change the Linux branches.

## 7. Part C — two more Claude Code locks (all platforms, independent of Windows)

Claude Code 2.1.322 renews two secondary logins under their own lock folders, read from its code:

| Login key in `.credentials.json` | Lock folder in the config folder | Abandoned after |
|---|---|---|
| `designOauth` | `.design_oauth_refresh.lock` | 60 s |
| `cloudSessionsOauth` | `.cloud_sessions_oauth_refresh.lock` | 60 s |

The renewal reads the key under the lock, calls the server, and saves the new tokens only if the key is unchanged, the same pattern as the main login (spec §2.6). Both keys are among the seven a switch swaps, so a switch that lands in the middle of such a renewal can save a used-up login for the old account.

- `ClaudePaths`: `pub fn design_refresh_lock(&self) -> PathBuf` and `pub fn cloud_sessions_refresh_lock(&self) -> PathBuf` (`<config_dir>/<name above>`).
- `locks::claude_lock_specs` returns `[LockSpec; 6]` in this order: `refresh_lock` 60 s, `legacy_refresh_lock` 60 s, `design_refresh_lock` 60 s, `cloud_sessions_refresh_lock` 60 s, `storage_write_lock` 15 s, `identity_lock` 10 s. Claude Code takes a renewal lock first and saves inside it, so the two new ones go before `storage_write_lock`. (That its save takes `.storage-write.lock` was read for the main login; for these two it is assumed, not read. Lock-order tests cannot show it; say so in the report.)
- `acquire_claude_locks` and `claude_locks_busy` follow from the specs. Update the tests that count four locks, the module comment in `locks.rs`, the module list in `lib.rs`, spec §2.6 and §6.2, and the `locks.rs` section of `docs/implementation.md`.

## 8. CI (`.github/workflows/ci.yml`)

One job with a matrix over `ubuntu-latest` and `windows-latest`. The apt step runs only on Linux (`if: runner.os == 'Linux'`); `cargo fmt --check` runs only on Linux; the two clippy runs and `cargo test --workspace` run on both.

## 9. Verification on Windows (from the repository root)

```
cargo build --offline --workspace
cargo build --offline -p seatswap --no-default-features
cargo test --offline --workspace
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo clippy --offline -p seatswap --no-default-features --all-targets -- -D warnings
cargo fmt --check
```

All six must succeed. `target\debug` must contain `seatswap.exe` and `seatswap-tray.exe`.

## 10. Manual checks — Andreas, not the implementer

1. `seatswap.exe list` and `seatswap.exe add` in a terminal print as on Linux.
2. `seatswap-tray.exe` from Explorer: no console window; tray icon with ring and letters; tooltip.
3. Left click opens the pop-up next to the icon; Esc, a click elsewhere and Alt+Tab close it; a click on the icon while it is open closes it.
4. In the pop-up's settings, opening a drop-down list does not close the pop-up.
5. A switch from the pop-up and from the right-click menu; a running Claude Code session follows it.
6. Notifications appear under the name "seatswap". A switch suggestion that times out does not open the pop-up; its "Switch" button switches; a click on its body opens the pop-up.
7. "Start at login" on: after signing in again the tray icon is there without a console window. Off: the `seatswap` value is gone from the `Run` key.
8. A second `seatswap-tray.exe` opens the pop-up of the first and exits; `seatswap.exe panel settings` opens the settings.

## 11. Docs to update, and the report

Docs (same change set):
- `docs/implementation.md`: the title no longer says "Linux v1"; the sentence "Windows … later work" points to this file; the `atomic.rs`, `locks.rs`, GUI single-instance, autostart and notification paragraphs match the new contracts.
- `docs/spec.md`: §14 item 7 is done (the file location and format are the same as on Linux, checked 2026-11-11); §2.6 and §6.2 name the two new locks; §9.4/§10 name the `Run` key and `seatswap-tray.exe`.
- `README.md` platform table: Windows 11 — "Builds and passes its tests; first real run pending" until section 10 is done.
- `CHANGELOG.md`, under Unreleased (the notification fix is entered already): Windows support; two more Claude Code locks are taken during a switch.

Report at the end:
1. The output summary of the six commands in section 9, with the number of tests run on Windows and the number that are Unix-only.
2. Every changed line that Linux also compiles, by file, for the Linux review.
3. Anything in this file that turned out wrong or unclear, and what you did about it.
4. What you could not verify.

No git commits (AGENTS.md rule 7).

## 12. Out of scope

- Installer, code signing, winget. The first Windows release is a zip with the two programs.
- Explicit Windows permissions (ACLs) on the data folder or the fallback secret files.
- A notification icon (`IconUri`); it needs an installed icon file.
- Treating two spellings of the config folder that differ only in upper and lower case as the same folder.
- macOS.
