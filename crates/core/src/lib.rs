//! seatswap core: everything except the GUI. See docs/spec.md and docs/implementation.md.
//!
//! Module owners (one module per job; do not add a second way to do what a module does):
//! - `types`    the login and identity value types shared by every module
//! - `paths`    where Claude Code's files and seatswap's own files live
//! - `atomic`   the one way to write a file (temp file + fsync + rename, mode kept)
//!   and the only module that knows file modes and private files and folders
//! - `locks`    Claude Code's six lock folders + seatswap's own switch.lock
//! - `live`     read/write the live login (.credentials.json) and identity (.claude.json)
//! - `store`    accounts.json (no secrets) + secrets (OS keyring, or 0600 files)
//! - `api`      the two HTTP endpoints: usage and token refresh
//! - `switcher` the switch algorithm (spec §6.2) and saving the current login
//! - `engine`   usage polling, refreshing inactive logins, usage cache (spec §6.3, §7)
//! - `watcher`  noticing changes Claude Code makes to the live login (spec §6.4)

pub mod api;
pub mod atomic;
pub mod engine;
pub mod live;
pub mod locks;
pub mod paths;
pub mod store;
pub mod switcher;
pub mod types;
pub mod watcher;

pub use types::{Login, OAuthAccount, LOGIN_KEYS};
