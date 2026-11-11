//! Where Claude Code's files and seatswap's own files live (spec §2, §5).
//!
//! Every path the program uses comes from here; no other module builds one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `sun_path` size of a Unix socket address, including the trailing NUL.
const SOCKADDR_UN_LIMIT: usize = 108;

#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("cannot find the home folder")]
    NoHome,
    #[error("cannot find the user config folder for seatswap")]
    NoConfigDir,
    #[error("{path} is too long for a Unix socket path (108 bytes or more)")]
    SocketPathTooLong { path: PathBuf },
    #[error("cannot make {path} absolute: {source}")]
    Absolute {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("data folder {path}: {source}")]
    DataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Claude Code's files for one config folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePaths {
    /// `~/.claude`, or `$CLAUDE_CONFIG_DIR`.
    pub config_dir: PathBuf,
    /// `<config_dir>/.credentials.json`: the live login (Linux, Windows).
    pub credentials_file: PathBuf,
    /// `.claude.json`: the account identity (`oauthAccount`) and much else.
    /// `~/.claude.json` for the default folder, `<config_dir>/.claude.json` otherwise.
    pub identity_file: PathBuf,
}

impl ClaudePaths {
    /// Resolve from the real environment: `explicit` (the `claudeConfigDir`
    /// setting) wins, then a non-empty `$CLAUDE_CONFIG_DIR`, then `~/.claude`.
    pub fn resolve(explicit: Option<&Path>) -> Result<ClaudePaths, PathsError> {
        let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
        Ok(Self::resolve_with(
            explicit,
            std::env::var_os("CLAUDE_CONFIG_DIR"),
            &home,
        ))
    }

    /// Same as [`resolve`](Self::resolve) without reading the process environment (for tests).
    pub fn resolve_with(
        explicit: Option<&Path>,
        env_config_dir: Option<OsString>,
        home: &Path,
    ) -> ClaudePaths {
        let dir = match (explicit, env_config_dir) {
            (Some(p), _) => p.to_path_buf(),
            (None, Some(v)) if !v.is_empty() => PathBuf::from(v),
            _ => home.join(".claude"),
        };
        Self::for_config_dir(&dir, home)
    }

    /// The files of a given config folder. Claude Code keeps `.claude.json` next
    /// to the default folder (`~/.claude.json`) and inside any other one.
    pub fn for_config_dir(config_dir: &Path, home: &Path) -> ClaudePaths {
        let identity_file = if config_dir == home.join(".claude") {
            home.join(".claude.json")
        } else {
            config_dir.join(".claude.json")
        };
        ClaudePaths {
            config_dir: config_dir.to_path_buf(),
            credentials_file: config_dir.join(".credentials.json"),
            identity_file,
        }
    }

    /// Claude Code's renewal lock: `<config_dir>/.oauth_refresh.lock` (spec §2.6).
    pub fn refresh_lock(&self) -> PathBuf {
        self.config_dir.join(".oauth_refresh.lock")
    }

    /// The renewal lock older versions use: `<realpath(config_dir)>.lock`,
    /// e.g. `~/.claude.lock`. Falls back to the path as given when it cannot be
    /// resolved (folder missing), which is what Claude Code does too.
    pub fn legacy_refresh_lock(&self) -> PathBuf {
        let real =
            std::fs::canonicalize(&self.config_dir).unwrap_or_else(|_| self.config_dir.clone());
        append_to_file_name(&real, ".lock")
    }

    /// Claude Code's renewal lock for the `designOauth` login:
    /// `<config_dir>/.design_oauth_refresh.lock`.
    pub fn design_refresh_lock(&self) -> PathBuf {
        self.config_dir.join(".design_oauth_refresh.lock")
    }

    /// Claude Code's renewal lock for the `cloudSessionsOauth` login:
    /// `<config_dir>/.cloud_sessions_oauth_refresh.lock`.
    pub fn cloud_sessions_refresh_lock(&self) -> PathBuf {
        self.config_dir.join(".cloud_sessions_oauth_refresh.lock")
    }

    /// Lock for every write of `.credentials.json`: `<config_dir>/.storage-write.lock`.
    pub fn storage_write_lock(&self) -> PathBuf {
        self.config_dir.join(".storage-write.lock")
    }

    /// Lock for every write of `.claude.json`: the identity file's path + `.lock`.
    pub fn identity_lock(&self) -> PathBuf {
        append_to_file_name(&self.identity_file, ".lock")
    }
}

/// seatswap's own files (spec §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// `~/.config/seatswap` on Linux, `~/Library/Application Support/seatswap`
    /// on macOS, `%APPDATA%\seatswap` on Windows.
    pub data_dir: PathBuf,
}

impl AppPaths {
    /// `explicit` (a command-line override) wins; otherwise the platform config folder.
    pub fn resolve(explicit: Option<&Path>) -> Result<AppPaths, PathsError> {
        let data_dir = match explicit {
            Some(p) => p.to_path_buf(),
            None => dirs::config_dir()
                .ok_or(PathsError::NoConfigDir)?
                .join("seatswap"),
        };
        Ok(AppPaths { data_dir })
    }

    pub fn at(data_dir: &Path) -> AppPaths {
        AppPaths {
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// The account list and settings, no secrets.
    pub fn accounts_file(&self) -> PathBuf {
        self.data_dir.join("accounts.json")
    }
    /// Last usage reply per account.
    pub fn usage_cache_file(&self) -> PathBuf {
        self.data_dir.join("usage-cache.json")
    }
    /// seatswap's own lock, keeps two seatswap processes apart.
    pub fn switch_lock(&self) -> PathBuf {
        self.data_dir.join("switch.lock")
    }
    /// Fallback secret storage when no OS keyring is available (0700 folder).
    pub fn secrets_dir(&self) -> PathBuf {
        self.data_dir.join("secrets")
    }
    /// The tray app's single-instance lock, held for the life of the process.
    pub fn gui_lock(&self) -> PathBuf {
        self.data_dir.join("gui.lock")
    }
    /// The Unix socket of the running tray app. `Some(dir)` names one socket per
    /// data folder inside that runtime directory; `None` puts it in the data
    /// folder. The name comes from the folder's real path, so every spelling of
    /// one folder (relative, absolute, through a link) finds the same socket;
    /// the folder must therefore exist. A path that does not fit `sockaddr_un`
    /// is an error, never shortened.
    pub fn gui_socket(&self, runtime_dir: Option<&Path>) -> Result<PathBuf, PathsError> {
        let path = match runtime_dir {
            None => self.data_dir.join("gui.sock"),
            Some(dir) => {
                let real = std::fs::canonicalize(&self.data_dir).map_err(|source| {
                    PathsError::DataDir {
                        path: self.data_dir.clone(),
                        source,
                    }
                })?;
                let hash = fnv1a64(real.as_os_str().as_encoded_bytes());
                dir.join(format!("seatswap-{hash:016x}.sock"))
            }
        };
        if path.as_os_str().as_encoded_bytes().len() >= SOCKADDR_UN_LIMIT {
            return Err(PathsError::SocketPathTooLong { path });
        }
        Ok(path)
    }
}

/// A folder named on the command line as an absolute path, resolved against the
/// current folder once at startup, so every later use (the autostart entry,
/// messages, files) sees the same path. Lexical only: `..` and links stay as
/// written (the socket name uses the real path, see [`AppPaths::gui_socket`]).
pub fn absolute(path: &Path) -> Result<PathBuf, PathsError> {
    std::path::absolute(path).map_err(|source| PathsError::Absolute {
        path: path.to_path_buf(),
        source,
    })
}

/// The user's runtime folder (`$XDG_RUNTIME_DIR`), where the GUI's socket lives
/// when it exists.
pub fn runtime_dir() -> Option<PathBuf> {
    dirs::runtime_dir()
}

/// The autostart desktop file the GUI writes with [`crate::atomic::write_atomic`]
/// (spec §9.4, §10).
pub fn autostart_file(xdg_config_home: &Path) -> PathBuf {
    xdg_config_home.join("autostart").join("seatswap.desktop")
}

/// FNV-1a 64 written out by hand, so the socket name stays the same across Rust
/// versions (unlike `DefaultHasher`).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn append_to_file_name(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_folder_keeps_identity_next_to_it() {
        let p = ClaudePaths::resolve_with(None, None, Path::new("/home/u"));
        assert_eq!(p.config_dir, Path::new("/home/u/.claude"));
        assert_eq!(
            p.credentials_file,
            Path::new("/home/u/.claude/.credentials.json")
        );
        assert_eq!(p.identity_file, Path::new("/home/u/.claude.json"));
        assert_eq!(p.identity_lock(), Path::new("/home/u/.claude.json.lock"));
        assert_eq!(
            p.storage_write_lock(),
            Path::new("/home/u/.claude/.storage-write.lock")
        );
        assert_eq!(
            p.refresh_lock(),
            Path::new("/home/u/.claude/.oauth_refresh.lock")
        );
        assert_eq!(
            p.design_refresh_lock(),
            Path::new("/home/u/.claude/.design_oauth_refresh.lock")
        );
        assert_eq!(
            p.cloud_sessions_refresh_lock(),
            Path::new("/home/u/.claude/.cloud_sessions_oauth_refresh.lock")
        );
    }

    #[test]
    fn env_folder_keeps_identity_inside() {
        let p =
            ClaudePaths::resolve_with(None, Some("/home/u/cs-test".into()), Path::new("/home/u"));
        assert_eq!(p.identity_file, Path::new("/home/u/cs-test/.claude.json"));
        assert_eq!(
            p.identity_lock(),
            Path::new("/home/u/cs-test/.claude.json.lock")
        );
        // folder does not exist: legacy lock falls back to the path as given
        assert_eq!(p.legacy_refresh_lock(), Path::new("/home/u/cs-test.lock"));
    }

    #[test]
    fn explicit_wins_and_empty_env_is_ignored() {
        let p =
            ClaudePaths::resolve_with(Some(Path::new("/x")), Some("/y".into()), Path::new("/h"));
        assert_eq!(p.config_dir, Path::new("/x"));
        let p = ClaudePaths::resolve_with(None, Some("".into()), Path::new("/h"));
        assert_eq!(p.config_dir, Path::new("/h/.claude"));
    }

    #[test]
    fn app_gui_paths_and_autostart_file() {
        let app = AppPaths::at(Path::new("/home/u/.config/seatswap"));
        assert_eq!(
            app.gui_lock(),
            Path::new("/home/u/.config/seatswap/gui.lock")
        );
        assert_eq!(
            app.gui_socket(None).unwrap(),
            Path::new("/home/u/.config/seatswap/gui.sock")
        );
        assert_eq!(
            autostart_file(Path::new("/home/u/.config")),
            Path::new("/home/u/.config/autostart/seatswap.desktop")
        );
    }

    #[test]
    fn gui_socket_name_is_pinned_to_the_data_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("seatswap");
        let other = tmp.path().join("seatswap-b");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&other).unwrap();
        let runtime = Path::new("/run/user/1000");

        let socket = AppPaths::at(&data).gui_socket(Some(runtime)).unwrap();
        let real = std::fs::canonicalize(&data).unwrap();
        let hash = fnv1a64(real.as_os_str().as_encoded_bytes());
        assert_eq!(
            socket,
            runtime.join(format!("seatswap-{hash:016x}.sock")),
            "the name is the FNV-1a hash of the real path"
        );
        assert_eq!(fnv1a64(b"/home/u/.config/seatswap"), 0xb6fe_9d8b_b8b6_e275);

        assert_ne!(
            socket,
            AppPaths::at(&other).gui_socket(Some(runtime)).unwrap(),
            "two data folders must not share one socket"
        );
    }

    /// A relative `--data-dir`, the same folder spelled with `..` or through a
    /// link, and the absolute path all name one socket; otherwise `seatswap
    /// panel` and a second start could not find the running tray app.
    #[test]
    #[cfg(unix)]
    fn every_spelling_of_the_data_folder_gives_one_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("seatswap");
        std::fs::create_dir(&data).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        let runtime = Path::new("/run/user/1000");
        let socket = |dir: &Path| AppPaths::at(dir).gui_socket(Some(runtime)).unwrap();

        let absolute = socket(&data);
        assert_eq!(socket(&link), absolute);
        assert_eq!(socket(&tmp.path().join("seatswap/../seatswap/")), absolute);
        // A relative path from the current folder, without changing it (tests
        // run in parallel): climb to the root and walk down again.
        let cwd = std::env::current_dir().unwrap();
        let up: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        let relative = up.join(data.strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        assert_eq!(socket(&relative), absolute);
    }

    #[test]
    fn gui_socket_of_a_missing_data_folder_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        match AppPaths::at(&missing).gui_socket(Some(Path::new("/run/user/1000"))) {
            Err(PathsError::DataDir { path, source }) => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected DataDir, got {other:?}"),
        }
    }

    #[test]
    fn gui_socket_rejects_a_path_too_long_for_sockaddr_un() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(tmp.path());
        let runtime = PathBuf::from(format!("/tmp/{}", "d".repeat(115)));
        assert_eq!(runtime.as_os_str().as_encoded_bytes().len(), 120);

        match app.gui_socket(Some(&runtime)) {
            Err(PathsError::SocketPathTooLong { path }) => {
                assert!(path.starts_with(&runtime));
            }
            other => panic!("expected SocketPathTooLong, got {other:?}"),
        }
    }

    #[test]
    fn absolute_joins_a_relative_folder_to_the_current_one() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute(Path::new("relative/data")).unwrap(),
            cwd.join("relative/data")
        );
        let already_absolute = std::env::temp_dir().join("seatswap");
        assert!(already_absolute.is_absolute());
        assert_eq!(absolute(&already_absolute).unwrap(), already_absolute);
    }

    #[test]
    #[cfg(unix)]
    fn legacy_lock_uses_the_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let p = ClaudePaths::for_config_dir(&link, Path::new("/nowhere"));
        let expect = append_to_file_name(&std::fs::canonicalize(&real).unwrap(), ".lock");
        assert_eq!(p.legacy_refresh_lock(), expect);
    }

    /// `canonicalize` gives `\\?\C:\…` on Windows; the lock folder it names is
    /// the one Claude Code names from the plain path.
    #[test]
    #[cfg(windows)]
    fn legacy_lock_names_the_same_folder_as_the_plain_path() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("cfg");
        std::fs::create_dir(&config).unwrap();
        let p = ClaudePaths::for_config_dir(&config, tmp.path());
        let legacy = p.legacy_refresh_lock();
        assert!(
            legacy.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "expected a verbatim path, got {}",
            legacy.display()
        );
        std::fs::create_dir(&legacy).unwrap();
        let plain = append_to_file_name(&config, ".lock");
        assert!(!plain.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        let err = std::fs::create_dir(&plain).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }
}
