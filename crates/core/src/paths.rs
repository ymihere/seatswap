//! Where Claude Code's files and seatswap's own files live (spec §2, §5).
//!
//! Every path the program uses comes from here; no other module builds one.

use std::ffi::{OsStr, OsString};
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
    #[error("{0}; set hostName in the settings")]
    HostName(String),
    #[error("historyDir must be an absolute path, got {0}")]
    HistoryDir(String),
    #[error("the Codex folder (codexHome or $CODEX_HOME) must be an absolute path, got {0}")]
    RelativeCodexHome(PathBuf),
    #[error("${variable} must be an absolute path, got {value}")]
    RelativeHarnessFolder {
        variable: &'static str,
        value: PathBuf,
    },
    #[error("Dropbox's info file {path}: {detail}")]
    DropboxInfo { path: PathBuf, detail: String },
    #[error("$USER is not set; Claude Code names its Keychain login after it")]
    NoUser,
    #[error("{LOGIN_STORE_VARIABLE}: {0}")]
    LoginStore(String),
}

/// Claude Code's files for one config folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePaths {
    /// `~/.claude`, or `$CLAUDE_CONFIG_DIR`.
    pub config_dir: PathBuf,
    /// The user's home folder this config folder was resolved against. Project
    /// identity and transcript token records need it; it is stored here so no
    /// other module reads the environment again.
    pub home: PathBuf,
    /// `<config_dir>/.credentials.json`: the live login (Linux, Windows).
    pub credentials_file: PathBuf,
    /// `.claude.json`: the account identity (`oauthAccount`) and much else.
    /// `~/.claude.json` for the default folder, `<config_dir>/.claude.json` otherwise.
    pub identity_file: PathBuf,
    /// Some on macOS when resolved from the real environment: the live login is this
    /// Keychain item and `credentials_file` is not used for it. None: the file.
    pub login_keychain: Option<KeychainItem>,
}

/// Claude Code's login on macOS: one generic-password item (spec §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
    /// The keychain file; None = the user's default keychain. Tests always name a throw-away keychain.
    pub keychain: Option<PathBuf>,
}

impl KeychainItem {
    /// Claude Code's item for `config_dir` and the user name `user` (default keychain).
    ///
    /// The service is `Claude Code-credentials` for the default folder and
    /// `Claude Code-credentials-<h>` for any other, where `<h>` is the first 8
    /// lowercase hex characters of the SHA-256 of the folder path as given (its
    /// bytes, not normalized). "Default" is decided exactly as for
    /// [`ClaudePaths::identity_file`]: `config_dir == home.join(".claude")`.
    /// Claude Code itself asks whether `CLAUDE_CONFIG_DIR` is set, so a
    /// `CLAUDE_CONFIG_DIR` that names `~/.claude` itself is the one case where
    /// the two can differ.
    pub fn for_config_dir(config_dir: &Path, home: &Path, user: &str) -> KeychainItem {
        use sha2::{Digest, Sha256};
        let service = if config_dir == home.join(".claude") {
            "Claude Code-credentials".to_string()
        } else {
            let digest = Sha256::digest(config_dir.as_os_str().as_encoded_bytes());
            let short: String = digest[..4]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("Claude Code-credentials-{short}")
        };
        KeychainItem {
            service,
            account: user.to_string(),
            keychain: None,
        }
    }
}

impl ClaudePaths {
    /// Resolve from the real environment: `explicit` (the `claudeConfigDir`
    /// setting) wins, then a non-empty `$CLAUDE_CONFIG_DIR`, then `~/.claude`.
    ///
    /// The live login is where [`LOGIN_STORE_VARIABLE`] says
    /// ([`login_store`]): on macOS by default Claude Code's Keychain item for
    /// that folder under the name `$USER` (spec §2.2), elsewhere the file.
    /// Never called by tests: with the Keychain, even a temporary folder names
    /// an item in the user's real default keychain.
    pub fn resolve(explicit: Option<&Path>) -> Result<ClaudePaths, PathsError> {
        let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
        let mut paths = Self::resolve_with(explicit, std::env::var_os("CLAUDE_CONFIG_DIR"), &home);
        let store = login_store(
            std::env::var_os(LOGIN_STORE_VARIABLE).as_deref(),
            cfg!(target_os = "macos"),
        )?;
        if store == LoginStore::Keychain {
            paths.login_keychain = Some(live_login_keychain(&paths.config_dir, &home)?);
        }
        Ok(paths)
    }

    /// Same as [`resolve`](Self::resolve) without reading the process
    /// environment (for tests). The live login is always the file
    /// (`login_keychain` is None).
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
    /// to the default folder (`~/.claude.json`) and inside any other one. The
    /// live login is the file (`login_keychain` is None).
    pub fn for_config_dir(config_dir: &Path, home: &Path) -> ClaudePaths {
        let identity_file = if config_dir == home.join(".claude") {
            home.join(".claude.json")
        } else {
            config_dir.join(".claude.json")
        };
        ClaudePaths {
            config_dir: config_dir.to_path_buf(),
            home: home.to_path_buf(),
            credentials_file: config_dir.join(".credentials.json"),
            identity_file,
            login_keychain: None,
        }
    }

    /// The user's home folder. Every module that needs it takes it from here
    /// instead of reading the environment itself.
    pub fn home(&self) -> &Path {
        &self.home
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

    /// Claude Code's settings: `<config_dir>/settings.json`. seatswap writes
    /// only its `statusLine` key.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    /// Claude Code's running session files: `<config_dir>/sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }

    /// Claude Code's transcripts: `<config_dir>/projects`.
    pub fn projects_dir(&self) -> PathBuf {
        self.config_dir.join("projects")
    }

    /// Claude Code's skill folder: `<config_dir>/skills`.
    pub fn skills_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }
}

/// The variable that says where Claude Code's live login is: unset or empty
/// for the system's own place (macOS: the Keychain; Linux, Windows: the file),
/// `file` for `.credentials.json` on every system (Claude Code on macOS falls
/// back to that file when it cannot use the Keychain; the CLI's integration
/// tests use it), `keychain` for the Keychain (macOS only).
pub const LOGIN_STORE_VARIABLE: &str = "SEATSWAP_CLAUDE_LOGIN";

/// Where the live login is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginStore {
    File,
    Keychain,
}

/// [`LOGIN_STORE_VARIABLE`]'s value read as a [`LoginStore`]. `macos` says
/// whether this is macOS (a parameter so every system tests both answers).
/// Any other value, and `keychain` off macOS, is
/// [`PathsError::LoginStore`].
fn login_store(value: Option<&OsStr>, macos: bool) -> Result<LoginStore, PathsError> {
    let default = if macos {
        LoginStore::Keychain
    } else {
        LoginStore::File
    };
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match value.to_str() {
        Some("file") => Ok(LoginStore::File),
        Some("keychain") if macos => Ok(LoginStore::Keychain),
        Some("keychain") => Err(PathsError::LoginStore(
            "keychain works on macOS only".to_string(),
        )),
        _ => Err(PathsError::LoginStore(format!(
            "it must be file or keychain, got {}",
            value.to_string_lossy()
        ))),
    }
}

/// Claude Code's Keychain item for `config_dir` under the name `$USER` (the
/// variable [`user_name`] reads on macOS; missing or empty is
/// [`PathsError::NoUser`]). Reached on macOS only ([`login_store`]).
fn live_login_keychain(config_dir: &Path, home: &Path) -> Result<KeychainItem, PathsError> {
    let user = user_name().map_err(|_| PathsError::NoUser)?;
    Ok(KeychainItem::for_config_dir(config_dir, home, &user))
}

/// Codex's login file and the two locks Codex takes to write it (spec §7.2, R8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPaths {
    /// `~/.codex`, or `$CODEX_HOME`. seatswap reads the login here and writes it
    /// only when switching, under Codex's two locks.
    pub home: PathBuf,
    /// `<home>/auth.json`: the login seatswap reads at every poll and writes only
    /// when switching, from the stored copy of the target login.
    pub auth_file: PathBuf,
    /// `<home>/auth.lock`: Codex's first write lock; seatswap takes it before
    /// `auth_json_lock` when it switches.
    pub auth_lock: PathBuf,
    /// `<home>/auth.json.lock`: Codex's second write lock, held while it replaces
    /// `auth.json`; seatswap takes it after `auth_lock`.
    pub auth_json_lock: PathBuf,
}

impl CodexPaths {
    /// Resolve from the real environment: `explicit` (the `codexHome` setting)
    /// wins, then a non-empty `$CODEX_HOME`, then `~/.codex`. A relative folder
    /// is [`PathsError::RelativeCodexHome`], never resolved against the current
    /// folder.
    pub fn resolve(explicit: Option<&Path>) -> Result<CodexPaths, PathsError> {
        let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
        Self::resolve_with(explicit, std::env::var_os("CODEX_HOME"), &home)
    }

    /// Same as [`resolve`](Self::resolve) without reading the process environment
    /// (for tests): `explicit` wins, then a non-empty `env_codex_home`, then
    /// `home/.codex`. A relative `explicit` folder or `env_codex_home` is
    /// [`PathsError::RelativeCodexHome`]: the setting is checked by
    /// `Settings::validate`, but a hand-edited file is not checked on load, and
    /// `$CODEX_HOME` is never checked at all.
    pub fn resolve_with(
        explicit: Option<&Path>,
        env_codex_home: Option<OsString>,
        home: &Path,
    ) -> Result<CodexPaths, PathsError> {
        let dir = folder_from(
            explicit,
            env_codex_home,
            home,
            &[".codex"],
            FolderSource::Codex,
        )?;
        Ok(Self::at(&dir))
    }

    /// The files of a given Codex folder: the login is `<home>/auth.json`, and
    /// Codex's two write locks sit beside it.
    pub fn at(home: &Path) -> CodexPaths {
        CodexPaths {
            home: home.to_path_buf(),
            auth_file: home.join("auth.json"),
            auth_lock: home.join("auth.lock"),
            auth_json_lock: home.join("auth.json.lock"),
        }
    }

    /// Codex's rollout files: `<home>/sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }
}

/// The Antigravity CLI's (`agy`) folder, `~/.gemini/antigravity-cli` (spec
/// §7.9). seatswap only checks that it exists and reads and writes the
/// `statusLine` key of its `settings.json`, the write only when the person
/// clicks Connect on the Google card. Tests use [`AgyPaths::at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyPaths {
    pub dir: PathBuf,
}

impl AgyPaths {
    /// The folder under the real home. `agy` names no variable that moves it.
    pub fn resolve() -> Result<AgyPaths, PathsError> {
        let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
        Ok(Self::at(&home.join(".gemini").join("antigravity-cli")))
    }

    /// The files of a given `agy` folder.
    pub fn at(dir: &Path) -> AgyPaths {
        AgyPaths {
            dir: dir.to_path_buf(),
        }
    }

    /// `<dir>/settings.json`: where `agy` reads its `statusLine`.
    pub fn settings_file(&self) -> PathBuf {
        self.dir.join("settings.json")
    }
}

/// The environment variables that place the Harness's sessions folder and
/// flash-agents' state folder. Only `HarnessPaths::resolve` fills it from the
/// process environment; tests build one by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessEnv {
    pub dsh_home: Option<OsString>,
    pub flash_data_dir: Option<OsString>,
    pub xdg_state_home: Option<OsString>,
    pub local_app_data: Option<OsString>,
}

/// The operating system whose rule places flash-agents' state folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Windows,
    MacOs,
    /// Linux and every other Unix.
    Unix,
}

impl Platform {
    const CURRENT: Platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Unix
    };
}

/// The DeepSeek Harness sessions folder and the flash-agents state folder
/// (whose `<key>/patches/` hold the job reports), from `env` and `home`:
/// `$DSH_HOME/sessions` or `<home>/.dsh/sessions`, and flash-agents' own rule
/// (its `durableBaseDir`): `$FLASH_DATA_DIR` as it is; else on Windows
/// `%LOCALAPPDATA%\flash-agents` or `<home>\AppData\Local\flash-agents`, on
/// macOS `<home>/Library/Application Support/flash-agents`, elsewhere
/// `$XDG_STATE_HOME/flash-agents` or `<home>/.local/state/flash-agents`. A
/// relative variable is [`PathsError::RelativeHarnessFolder`], never resolved
/// against the current folder.
pub fn harness_folders(env: &HarnessEnv, home: &Path) -> Result<(PathBuf, PathBuf), PathsError> {
    harness_folders_on(Platform::CURRENT, env, home)
}

fn harness_folders_on(
    platform: Platform,
    env: &HarnessEnv,
    home: &Path,
) -> Result<(PathBuf, PathBuf), PathsError> {
    let dsh = folder_from(
        None,
        env.dsh_home.clone(),
        home,
        &[".dsh"],
        FolderSource::DshHome,
    )?;
    let flash = match named_folder(None, env.flash_data_dir.clone(), FolderSource::FlashDataDir)? {
        Some(folder) => folder,
        None => match platform {
            Platform::Windows => folder_from(
                None,
                env.local_app_data.clone(),
                home,
                &["AppData", "Local"],
                FolderSource::LocalAppData,
            )?
            .join("flash-agents"),
            Platform::MacOs => home
                .join("Library")
                .join("Application Support")
                .join("flash-agents"),
            Platform::Unix => folder_from(
                None,
                env.xdg_state_home.clone(),
                home,
                &[".local", "state"],
                FolderSource::XdgStateHome,
            )?
            .join("flash-agents"),
        },
    };
    Ok((dsh.join("sessions"), flash))
}

/// The variable or setting a folder comes from, so a relative folder names it.
#[derive(Debug, Clone, Copy)]
enum FolderSource {
    Codex,
    DshHome,
    FlashDataDir,
    LocalAppData,
    XdgStateHome,
}

impl FolderSource {
    fn relative(self, value: PathBuf) -> PathsError {
        let variable = match self {
            FolderSource::Codex => return PathsError::RelativeCodexHome(value),
            FolderSource::DshHome => "DSH_HOME",
            FolderSource::FlashDataDir => "FLASH_DATA_DIR",
            FolderSource::LocalAppData => "LOCALAPPDATA",
            FolderSource::XdgStateHome => "XDG_STATE_HOME",
        };
        PathsError::RelativeHarnessFolder { variable, value }
    }
}

/// `explicit` (a setting) or `env` names the folder, else `home` joined with
/// `default`. A named folder that is relative is an error naming its source and
/// the value, never resolved against the current folder. The one resolver for
/// Codex's folder, the Harness's sessions folder and flash-agents' state folder.
fn folder_from(
    explicit: Option<&Path>,
    env: Option<OsString>,
    home: &Path,
    default: &[&str],
    source: FolderSource,
) -> Result<PathBuf, PathsError> {
    Ok(named_folder(explicit, env, source)?.unwrap_or_else(|| {
        default
            .iter()
            .fold(home.to_path_buf(), |folder, part| folder.join(part))
    }))
}

/// The folder `explicit` or a non-empty `env` names, or `None` when neither
/// does. A relative one is an error naming its source and the value.
fn named_folder(
    explicit: Option<&Path>,
    env: Option<OsString>,
    source: FolderSource,
) -> Result<Option<PathBuf>, PathsError> {
    let folder = match (explicit, env) {
        (Some(folder), _) => folder.to_path_buf(),
        (None, Some(value)) if !value.is_empty() => PathBuf::from(value),
        _ => return Ok(None),
    };
    if !folder.is_absolute() {
        return Err(source.relative(folder));
    }
    Ok(Some(folder))
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
    /// The status line's live numbers, waiting for the tray app (spec §5.3).
    pub fn feed_file(&self) -> PathBuf {
        self.data_dir.join("feed.json")
    }
    /// The Google account's numbers as `agy` last handed them to the status line
    /// command (spec §7.9).
    pub fn google_file(&self) -> PathBuf {
        self.data_dir.join("google.json")
    }
    /// The field names and JSON kinds `seatswap statusline` recorded while the
    /// recorder is on (spec §5.4).
    pub fn statusline_keys_file(&self) -> PathBuf {
        self.data_dir.join("statusline-keys.json")
    }
    /// The next account auto-burn plans to switch to, for the status line
    /// (spec §7.6).
    pub fn burn_next_file(&self) -> PathBuf {
        self.data_dir.join("burn-next.json")
    }
    /// The models.dev prices with their dates (spec §1.4.1).
    pub fn prices_file(&self) -> PathBuf {
        self.data_dir.join("prices.json")
    }
    /// The dollar rate with its dates (spec §1.4.1).
    pub fn fx_file(&self) -> PathBuf {
        self.data_dir.join("fx.json")
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
    /// The app icon a desktop notification shows: `<data_dir>/notification-icon.png`.
    /// Written from the embedded PNG when a notification needs it, so the file
    /// lives in the user's own folder and never in a shared temp folder.
    pub fn notification_icon(&self) -> PathBuf {
        self.data_dir.join("notification-icon.png")
    }
    /// The history folder when the setting names none: `<data_dir>/history`.
    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join("history")
    }
    /// The history root this host's folder was last brought to or opened in:
    /// `<data_dir>/history-root.json` (spec §1.5).
    pub fn history_root_file(&self) -> PathBuf {
        self.data_dir.join("history-root.json")
    }
    /// The id that tells this machine apart from another host writing the same history.
    pub fn machine_id_file(&self) -> PathBuf {
        self.data_dir.join("machine-id")
    }
    /// One state file per Claude Code project folder: `<data_dir>/transcripts-state`.
    pub fn transcripts_state_dir(&self) -> PathBuf {
        self.data_dir.join("transcripts-state")
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

/// Check a `historyDir`: it must be absolute. A relative folder is an error,
/// never silently resolved against the current folder.
pub fn check_history_dir(path: &Path) -> Result<(), PathsError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PathsError::HistoryDir(path.display().to_string()))
    }
}

/// The history folder's root: the explicit `historyDir` setting when it is
/// absolute, else `app.history_dir()`. A relative explicit folder is an error,
/// never silently resolved against the current folder.
pub fn history_root(explicit: Option<&Path>, app: &AppPaths) -> Result<PathBuf, PathsError> {
    match explicit {
        Some(path) => {
            check_history_dir(path)?;
            Ok(path.to_path_buf())
        }
        None => Ok(app.history_dir()),
    }
}

/// The folder Dropbox syncs into, from Dropbox's own `info.json` (spec §1.5):
/// `personal.path` when it has one, else `business.path`. Dropbox not installed
/// (`info.json` missing) and an `info.json` without either key are `Ok(None)`;
/// an unreadable file, a file that is not JSON, a section that is neither an
/// object nor `null`, or a path that is not a non-empty string is
/// [`PathsError::DropboxInfo`], never a made-up folder.
pub fn dropbox_folder() -> Result<Option<PathBuf>, PathsError> {
    dropbox_folder_in_files(&dropbox_info_files()?)
}

/// The first of Dropbox's candidate files that exists, or `Ok(None)` when none
/// does. A file whose existence cannot be checked is
/// [`PathsError::DropboxInfo`], never treated as absent.
fn dropbox_folder_in_files(files: &[PathBuf]) -> Result<Option<PathBuf>, PathsError> {
    for info_json in files {
        match info_json.try_exists() {
            Ok(true) => return dropbox_folder_in(info_json),
            Ok(false) => {}
            Err(source) => {
                return Err(dropbox_error(
                    info_json,
                    format!("cannot check whether the file exists: {source}"),
                ))
            }
        }
    }
    Ok(None)
}

/// The one reader of Dropbox's `info.json`, so a test names the file it built
/// in a tempdir instead of the real Dropbox folder (AGENTS.md hard rule 1).
pub fn dropbox_folder_in(info_json: &Path) -> Result<Option<PathBuf>, PathsError> {
    let bytes = match std::fs::read(info_json) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(dropbox_error(
                info_json,
                format!("cannot read the file: {source}"),
            ))
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| dropbox_error(info_json, format!("not JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| dropbox_error(info_json, "the file is not a JSON object"))?;
    for key in ["personal", "business"] {
        // An absent or `null` section has no path; another kind is refused.
        let Some(section) = object.get(key).filter(|section| !section.is_null()) else {
            continue;
        };
        let section = section
            .as_object()
            .ok_or_else(|| dropbox_error(info_json, format!("{key} is not an object")))?;
        let Some(path) = section.get("path") else {
            continue;
        };
        let path = path
            .as_str()
            .ok_or_else(|| dropbox_error(info_json, format!("{key}.path is not a string")))?;
        if path.is_empty() {
            return Err(dropbox_error(info_json, format!("{key}.path is empty")));
        }
        return Ok(Some(PathBuf::from(path)));
    }
    Ok(None)
}

fn dropbox_error(info_json: &Path, detail: impl Into<String>) -> PathsError {
    PathsError::DropboxInfo {
        path: info_json.to_path_buf(),
        detail: detail.into(),
    }
}

/// Dropbox's `info.json`, in the order the platforms write it. The operating
/// system's difference belongs here, not at the caller. Linux and macOS both
/// keep it in `~/.dropbox`.
#[cfg(not(windows))]
fn dropbox_info_files() -> Result<Vec<PathBuf>, PathsError> {
    let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
    Ok(vec![home.join(".dropbox").join("info.json")])
}

#[cfg(windows)]
fn dropbox_info_files() -> Result<Vec<PathBuf>, PathsError> {
    let mut files = Vec::new();
    for variable in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(folder) = std::env::var_os(variable) {
            if !folder.is_empty() {
                files.push(PathBuf::from(folder).join("Dropbox").join("info.json"));
            }
        }
    }
    Ok(files)
}

/// The name this host writes its history under: `explicit` if set, else the
/// machine's host name (Linux: /proc/sys/kernel/hostname; macOS: the Bonjour
/// name from `scutil --get LocalHostName`; Windows: %COMPUTERNAME%).
/// 1..=63 characters of [A-Za-z0-9._-] whose first character is a letter or a
/// digit; anything else is [`PathsError::HostName`] with the text "set hostName
/// in the settings". The first character rules out `.` and `..`, which would
/// name another folder. The result is always in lower case: Windows has the
/// same name in upper case in %COMPUTERNAME% and in lower case in Claude Code's
/// files (checked 15 November 2026).
pub fn host_name(explicit: Option<&str>) -> Result<String, PathsError> {
    let name = match explicit {
        Some(name) => name.to_string(),
        None => machine_host_name()?,
    };
    if !(1..=63).contains(&name.len())
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        || !name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return Err(PathsError::HostName(format!(
            "host name {name:?} must be 1 to 63 characters of A-Z, a-z, 0-9, '.', '_' or '-' \
             and must start with a letter or a digit"
        )));
    }
    Ok(name.to_ascii_lowercase())
}

/// The user name of the status line's `user@host`: `$USER` on Unix,
/// `%USERNAME%` on Windows. The operating system's naming difference belongs
/// here, not at the caller. Not set is an error the line shows, never a made-up
/// name.
pub fn user_name() -> Result<String, String> {
    #[cfg(windows)]
    let variable = "USERNAME";
    #[cfg(not(windows))]
    let variable = "USER";
    match std::env::var(variable) {
        Ok(name) if !name.is_empty() => Ok(name),
        _ => Err(format!("{variable} is not set")),
    }
}

#[cfg(target_os = "linux")]
fn machine_host_name() -> Result<String, PathsError> {
    const FILE: &str = "/proc/sys/kernel/hostname";
    let text = std::fs::read_to_string(FILE).map_err(|e| {
        PathsError::HostName(format!(
            "cannot read the machine's host name from {FILE}: {e}"
        ))
    })?;
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

#[cfg(windows)]
fn machine_host_name() -> Result<String, PathsError> {
    std::env::var("COMPUTERNAME").map_err(|_| {
        PathsError::HostName("COMPUTERNAME does not give the machine's host name".to_string())
    })
}

/// macOS: the Bonjour local host name the person sets in System Settings →
/// General → Sharing. Not `gethostname`/`uname`, which on a Mac follow the
/// network (DHCP names, `.local`) and would move this host's history folder.
#[cfg(target_os = "macos")]
fn machine_host_name() -> Result<String, PathsError> {
    let output = std::process::Command::new(SCUTIL)
        .args(["--get", "LocalHostName"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            PathsError::HostName(format!(
                "cannot run {SCUTIL} to read the machine's host name: {e}"
            ))
        })?;
    local_host_name(output.status.success(), &output.stdout, &output.stderr)
}

#[cfg(any(target_os = "macos", test))]
const SCUTIL: &str = "/usr/sbin/scutil";

/// The name in the output of `scutil --get LocalHostName`: its one line
/// without the line end. A failed run (scutil prints `LocalHostName: not set`
/// when there is none) or an empty name is an error naming what scutil said;
/// [`host_name`] puts the name in lower case and checks its characters.
#[cfg(any(target_os = "macos", test))]
fn local_host_name(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String, PathsError> {
    let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
    if !success {
        let message = [said(stderr), said(stdout)]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or_else(|| "no output".to_string());
        return Err(PathsError::HostName(format!(
            "{SCUTIL} --get LocalHostName failed: {message}"
        )));
    }
    let text = std::str::from_utf8(stdout).map_err(|_| {
        PathsError::HostName(format!(
            "{SCUTIL} --get LocalHostName gave a name that is not UTF-8"
        ))
    })?;
    let name = text.strip_suffix('\n').unwrap_or(text);
    if name.is_empty() {
        return Err(PathsError::HostName(format!(
            "{SCUTIL} --get LocalHostName gave no name"
        )));
    }
    Ok(name.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn machine_host_name() -> Result<String, PathsError> {
    Err(PathsError::HostName(
        "the machine's host name cannot be read on this system".to_string(),
    ))
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

/// Find a program in a `PATH` value (spec R6): the first `<folder>/<name>` that
/// is a file, and on Unix has an execute bit; on Windows the name gets `.exe`.
/// Only the value passed in is read, never the process environment, and no
/// shell or `which` is called.
pub fn program_on_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    #[cfg(windows)]
    let file_name = format!("{name}.exe");
    #[cfg(not(windows))]
    let file_name = name.to_string();
    std::env::split_paths(path_var)
        // An empty entry (`""`, a leading/trailing `:`, `::`) or a relative
        // one (`"."`) would name a file in the process's current folder, which
        // for a tray app started by a launcher is anywhere. Only folders on
        // `PATH` are searched.
        .filter(|folder| !folder.as_os_str().is_empty() && folder.is_absolute())
        .map(|folder| folder.join(&file_name))
        .find(|candidate| is_program(candidate))
}

#[cfg(windows)]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

/// The user's runtime folder (`$XDG_RUNTIME_DIR`), where the GUI's socket lives
/// when it exists.
pub fn runtime_dir() -> Option<PathBuf> {
    dirs::runtime_dir()
}

/// The autostart desktop file the GUI writes with [`crate::atomic::write_atomic`]
/// (spec §9.4, §10). Linux; macOS uses [`launch_agent_file`].
pub fn autostart_file(xdg_config_home: &Path) -> PathBuf {
    xdg_config_home.join("autostart").join("seatswap.desktop")
}

/// The label of the macOS LaunchAgent that starts the tray app at login: the
/// Tauri identifier (`crates/app/tauri.conf.json`), which also names its file.
pub const LAUNCH_AGENT_LABEL: &str = "dev.seatswap.app";

/// The per-user LaunchAgent the GUI writes on macOS with
/// [`crate::atomic::write_atomic`] for "Start at login"
/// (`<home>/Library/LaunchAgents/dev.seatswap.app.plist`). launchd reads that
/// folder at the next login.
pub fn launch_agent_file(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

/// [`launch_agent_file`] in this user's home folder. Only macOS has one; tests
/// pass a home folder to [`launch_agent_file`] instead and never call this.
#[cfg(target_os = "macos")]
pub fn user_launch_agent_file() -> Result<PathBuf, PathsError> {
    let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
    Ok(launch_agent_file(&home))
}

/// The Linux menu entry the tray app writes at start
/// (`$XDG_DATA_HOME/applications/seatswap.desktop`).
pub fn applications_entry(xdg_data_home: &Path) -> PathBuf {
    xdg_data_home.join("applications").join("seatswap.desktop")
}

/// The icon of that menu entry
/// (`$XDG_DATA_HOME/icons/hicolor/256x256/apps/seatswap.png`).
pub fn app_icon(xdg_data_home: &Path) -> PathBuf {
    xdg_data_home
        .join("icons")
        .join("hicolor")
        .join("256x256")
        .join("apps")
        .join("seatswap.png")
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
    fn host_name_takes_the_explicit_one() {
        assert_eq!(host_name(Some("vega")).unwrap(), "vega");
    }

    #[test]
    fn host_name_is_lower_case() {
        assert_eq!(host_name(Some("VEGA-PC")).unwrap(), "vega-pc");
    }

    #[test]
    fn host_name_rejects_other_characters() {
        assert!(matches!(
            host_name(Some("a/b")),
            Err(PathsError::HostName(_))
        ));
    }

    #[test]
    fn a_host_name_must_start_with_a_letter_or_digit() {
        for name in [".", "..", ".atlas", "-atlas", "_atlas"] {
            match host_name(Some(name)) {
                Err(PathsError::HostName(text)) => assert!(
                    text.contains("must start with a letter or a digit"),
                    "{name:?}: {text}"
                ),
                other => panic!("{name:?} must be refused, got {other:?}"),
            }
        }
        for name in ["atlas", "9vega", "vega-2.lan", "a"] {
            assert_eq!(host_name(Some(name)).unwrap(), name);
        }
    }

    #[test]
    fn the_local_host_name_is_scutils_line() {
        assert_eq!(
            local_host_name(true, b"Sams-MacBook-Pro\n", b"").unwrap(),
            "Sams-MacBook-Pro"
        );
        assert_eq!(local_host_name(true, b"atlas", b"").unwrap(), "atlas");
        // host_name then puts it in lower case and checks its characters.
        let name = local_host_name(true, b"Sams-MacBook-Pro\n", b"").unwrap();
        assert_eq!(host_name(Some(&name)).unwrap(), "sams-macbook-pro");
    }

    #[test]
    fn a_missing_local_host_name_is_an_error_naming_scutil() {
        for (success, stdout, stderr, said) in [
            (
                false,
                &b"LocalHostName: not set\n"[..],
                &b""[..],
                "failed: LocalHostName: not set",
            ),
            (
                false,
                b"",
                b"scutil: permission denied\n",
                "failed: scutil: permission denied",
            ),
            (false, b"", b"", "failed: no output"),
            (true, b"\n", b"", "gave no name"),
            (true, b"", b"", "gave no name"),
            (true, b"\xff\xfe\n", b"", "not UTF-8"),
        ] {
            let error = local_host_name(success, stdout, stderr).unwrap_err();
            let text = error.to_string();
            assert!(matches!(error, PathsError::HostName(_)), "{text}");
            assert!(
                text.contains("/usr/sbin/scutil --get LocalHostName"),
                "{text}"
            );
            assert!(text.contains(said), "{text}");
            assert!(text.ends_with("; set hostName in the settings"), "{text}");
        }
    }

    #[test]
    fn the_user_name_is_the_environments_variable() {
        let variable = if cfg!(windows) { "USERNAME" } else { "USER" };
        match std::env::var(variable) {
            Ok(name) if !name.is_empty() => assert_eq!(user_name().unwrap(), name),
            _ => assert_eq!(user_name().unwrap_err(), format!("{variable} is not set")),
        }
    }

    #[test]
    fn the_login_store_defaults_to_the_keychain_on_macos_only() {
        for value in [None, Some(OsStr::new(""))] {
            assert_eq!(login_store(value, true).unwrap(), LoginStore::Keychain);
            assert_eq!(login_store(value, false).unwrap(), LoginStore::File);
        }
    }

    #[test]
    fn the_login_store_variable_names_file_or_keychain() {
        let file = Some(OsStr::new("file"));
        let keychain = Some(OsStr::new("keychain"));
        assert_eq!(login_store(file, true).unwrap(), LoginStore::File);
        assert_eq!(login_store(file, false).unwrap(), LoginStore::File);
        assert_eq!(login_store(keychain, true).unwrap(), LoginStore::Keychain);
        let error = login_store(keychain, false).unwrap_err();
        assert!(matches!(error, PathsError::LoginStore(_)));
        assert_eq!(
            error.to_string(),
            "SEATSWAP_CLAUDE_LOGIN: keychain works on macOS only"
        );
        for other in ["File", "keychain ", "files", "0"] {
            let error = login_store(Some(OsStr::new(other)), true).unwrap_err();
            assert!(
                error.to_string().contains("must be file or keychain"),
                "{other:?}: {error}"
            );
        }
    }

    #[test]
    fn the_default_folder_uses_the_plain_keychain_service() {
        let item = KeychainItem::for_config_dir(
            Path::new("/Users/sam/.claude"),
            Path::new("/Users/sam"),
            "sam",
        );
        assert_eq!(
            item,
            KeychainItem {
                service: "Claude Code-credentials".to_string(),
                account: "sam".to_string(),
                keychain: None,
            }
        );
    }

    #[test]
    fn another_folder_uses_the_hashed_keychain_service() {
        // printf %s '/Users/sam/work-claude' | shasum -a 256
        //   45574e61e5868a5cec013c5c60c6a0c274d66553e7887cf22ac9550dba130550
        let item = KeychainItem::for_config_dir(
            Path::new("/Users/sam/work-claude"),
            Path::new("/Users/sam"),
            "sam",
        );
        assert_eq!(item.service, "Claude Code-credentials-45574e61");
        assert_eq!(item.account, "sam");
        assert_eq!(item.keychain, None);

        // `.claude` under another home is not the default folder either.
        // printf %s '/Users/sam/.claude' | shasum -a 256
        //   737caf6a474b63167db2345e61780712c82e5f7b17f3bb3bd0e14324d06d7b10
        let other_home = KeychainItem::for_config_dir(
            Path::new("/Users/sam/.claude"),
            Path::new("/Users/max"),
            "max",
        );
        assert_eq!(other_home.service, "Claude Code-credentials-737caf6a");
    }

    #[test]
    fn explicitly_built_claude_paths_use_the_credentials_file() {
        let home = Path::new("/Users/sam");
        assert_eq!(
            ClaudePaths::for_config_dir(&home.join(".claude"), home).login_keychain,
            None
        );
        assert_eq!(
            ClaudePaths::resolve_with(None, Some("/Users/sam/work-claude".into()), home)
                .login_keychain,
            None
        );
    }

    #[test]
    fn claude_folders() {
        let p = ClaudePaths::for_config_dir(Path::new("/x/c"), Path::new("/x"));
        assert_eq!(p.sessions_dir(), Path::new("/x/c/sessions"));
        assert_eq!(p.projects_dir(), Path::new("/x/c/projects"));
        assert_eq!(p.skills_dir(), Path::new("/x/c/skills"));
    }

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
    fn codex_paths_at() {
        let p = CodexPaths::at(Path::new("/x/codex"));
        assert_eq!(p.home, Path::new("/x/codex"));
        assert_eq!(p.auth_file, Path::new("/x/codex/auth.json"));
        assert_eq!(p.auth_lock, Path::new("/x/codex/auth.lock"));
        assert_eq!(p.auth_json_lock, Path::new("/x/codex/auth.json.lock"));
    }

    #[test]
    fn codex_paths_resolve_order() {
        // Absolute on the platform the test runs on: `/x` is not absolute on
        // Windows, where a folder needs a drive.
        let root = Path::new(if cfg!(windows) { r"C:\" } else { "/" });
        let explicit = root.join("x").join("codex");
        let variable = root.join("y").join("codex");
        let home = root.join("home").join("u");

        // An explicit folder wins over the variable.
        let p = CodexPaths::resolve_with(
            Some(&explicit),
            Some(variable.clone().into_os_string()),
            &home,
        )
        .expect("an explicit absolute folder is fine");
        assert_eq!(p.home, explicit);
        assert_eq!(p.auth_file, explicit.join("auth.json"));

        // A non-empty variable wins over the home folder.
        let p = CodexPaths::resolve_with(None, Some(variable.clone().into_os_string()), &home)
            .expect("an absolute variable is fine");
        assert_eq!(p.home, variable);
        assert_eq!(p.auth_file, variable.join("auth.json"));

        // An empty variable is ignored.
        let p = CodexPaths::resolve_with(None, Some("".into()), &home)
            .expect("an empty variable falls back to the home folder");
        assert_eq!(p.home, home.join(".codex"));
        assert_eq!(p.auth_file, home.join(".codex").join("auth.json"));
    }

    /// A relative `$CODEX_HOME` must not become a folder relative to the
    /// current one: seatswap would then read a different folder than Codex
    /// does, without saying so. The history folder has the same rule
    /// ([`check_history_dir`]), and so has the `codexHome` setting
    /// (`Settings::validate`), but a hand-edited settings file is not checked
    /// on load.
    #[test]
    fn the_codex_folder_from_the_environment_is_never_relative() {
        let error =
            CodexPaths::resolve_with(None, Some("codex".into()), Path::new("/home/u")).unwrap_err();
        assert!(
            matches!(&error, PathsError::RelativeCodexHome(path) if path == Path::new("codex")),
            "expected RelativeCodexHome for codex, got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "the Codex folder (codexHome or $CODEX_HOME) must be an absolute path, got codex"
        );

        // The explicit `codexHome` folder is the same error.
        let error = CodexPaths::resolve_with(Some(Path::new("codex")), None, Path::new("/home/u"))
            .unwrap_err();
        assert!(
            matches!(&error, PathsError::RelativeCodexHome(path) if path == Path::new("codex")),
            "expected RelativeCodexHome for codex, got {error:?}"
        );
    }

    /// flash-agents places its state folder (and so its job reports) by its own
    /// rule, `durableBaseDir` in flash-mcp's `isolation.js` (0.1.12): a set
    /// `$FLASH_DATA_DIR` as it is, else one folder per platform. On Windows it
    /// is `%LOCALAPPDATA%\flash-agents` (seen 18 Nov 2026), and
    /// `$XDG_STATE_HOME` plays no part there.
    #[test]
    fn the_flash_agents_folder_follows_flash_agents_rule_on_every_platform() {
        let root = Path::new(if cfg!(windows) { r"C:\" } else { "/" });
        let home = root.join("home").join("sam");
        let local = root.join("Users").join("sam").join("AppData").join("Local");
        let xdg = root.join("state").join("xdg");
        let data = root.join("state").join("flash");
        let env = HarnessEnv {
            xdg_state_home: Some(xdg.clone().into_os_string()),
            local_app_data: Some(local.clone().into_os_string()),
            ..HarnessEnv::default()
        };
        let reports = |platform, env: &HarnessEnv| {
            harness_folders_on(platform, env, &home)
                .expect("absolute folders are fine")
                .1
        };

        assert_eq!(reports(Platform::Windows, &env), local.join("flash-agents"));
        assert_eq!(
            reports(Platform::MacOs, &env),
            home.join("Library")
                .join("Application Support")
                .join("flash-agents")
        );
        assert_eq!(reports(Platform::Unix, &env), xdg.join("flash-agents"));

        // Unset or empty variables: the home folder's defaults.
        let unset = HarnessEnv {
            xdg_state_home: Some("".into()),
            local_app_data: Some("".into()),
            ..HarnessEnv::default()
        };
        assert_eq!(
            reports(Platform::Windows, &unset),
            home.join("AppData").join("Local").join("flash-agents")
        );
        assert_eq!(
            reports(Platform::Unix, &HarnessEnv::default()),
            home.join(".local").join("state").join("flash-agents")
        );

        // `$FLASH_DATA_DIR` wins everywhere, as it is (no `flash-agents` added).
        let named = HarnessEnv {
            flash_data_dir: Some(data.clone().into_os_string()),
            ..env.clone()
        };
        for platform in [Platform::Windows, Platform::MacOs, Platform::Unix] {
            assert_eq!(reports(platform, &named), data, "{platform:?}");
        }
    }

    /// A relative variable must not become a folder relative to the current
    /// one, as for `$CODEX_HOME`; only the variables a platform reads count.
    #[test]
    fn a_relative_harness_variable_is_an_error_naming_it() {
        let home = Path::new(if cfg!(windows) {
            r"C:\home\sam"
        } else {
            "/home/sam"
        });
        let cases = [
            (
                Platform::Unix,
                HarnessEnv {
                    dsh_home: Some("dsh".into()),
                    ..HarnessEnv::default()
                },
                "DSH_HOME",
                "dsh",
            ),
            (
                Platform::Unix,
                HarnessEnv {
                    flash_data_dir: Some("flash".into()),
                    ..HarnessEnv::default()
                },
                "FLASH_DATA_DIR",
                "flash",
            ),
            (
                Platform::Unix,
                HarnessEnv {
                    xdg_state_home: Some("state".into()),
                    ..HarnessEnv::default()
                },
                "XDG_STATE_HOME",
                "state",
            ),
            (
                Platform::Windows,
                HarnessEnv {
                    local_app_data: Some("Local".into()),
                    ..HarnessEnv::default()
                },
                "LOCALAPPDATA",
                "Local",
            ),
        ];
        for (platform, env, name, given) in cases {
            let error = harness_folders_on(platform, &env, home).expect_err(name);
            assert!(
                matches!(
                    &error,
                    PathsError::RelativeHarnessFolder { variable, value }
                        if *variable == name && value == Path::new(given)
                ),
                "expected a relative {name} error, got {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!("${name} must be an absolute path, got {given}")
            );
        }

        // A variable the platform does not read is not checked.
        let relative_xdg = HarnessEnv {
            xdg_state_home: Some("state".into()),
            ..HarnessEnv::default()
        };
        assert!(harness_folders_on(Platform::Windows, &relative_xdg, home).is_ok());
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
    fn the_menu_entry_and_its_icon_live_in_the_xdg_data_folder() {
        assert_eq!(
            applications_entry(Path::new("/home/u/.local/share")),
            Path::new("/home/u/.local/share/applications/seatswap.desktop")
        );
        assert_eq!(
            app_icon(Path::new("/home/u/.local/share")),
            Path::new("/home/u/.local/share/icons/hicolor/256x256/apps/seatswap.png")
        );
    }

    #[test]
    fn the_launch_agent_lives_in_the_users_launch_agents_folder() {
        assert_eq!(
            launch_agent_file(Path::new("/Users/u")),
            Path::new("/Users/u/Library/LaunchAgents/dev.seatswap.app.plist")
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

    #[test]
    fn a_personal_dropbox_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        std::fs::write(
            &info,
            r#"{"personal":{"path":"/home/sam/Dropbox","host":123456789,"is_team":false,"subscription_type":"Basic"},"business":{"path":"/home/sam/Dropbox (Acme)","host":987654321}}"#,
        )
        .unwrap();
        assert_eq!(
            dropbox_folder_in(&info).unwrap(),
            Some(PathBuf::from("/home/sam/Dropbox"))
        );
    }

    #[test]
    fn business_is_used_without_personal() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        std::fs::write(
            &info,
            r#"{"business":{"path":"/home/sam/Dropbox (Acme)","host":987654321}}"#,
        )
        .unwrap();
        assert_eq!(
            dropbox_folder_in(&info).unwrap(),
            Some(PathBuf::from("/home/sam/Dropbox (Acme)"))
        );
    }

    #[test]
    fn no_info_json_is_no_suggestion() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        assert_eq!(dropbox_folder_in(&info).unwrap(), None);
        assert_eq!(
            dropbox_folder_in_files(&[info]).unwrap(),
            None,
            "a missing file is no suggestion"
        );
    }

    // A file used as a parent folder is ENOTDIR on Unix; Windows reports it as not found.
    #[test]
    #[cfg(unix)]
    fn an_info_json_that_cannot_be_checked_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file cannot be the folder of `info.json`, so checking the
        // path is an error, not "not there".
        let not_a_folder = tmp.path().join("not-a-folder");
        std::fs::write(&not_a_folder, b"x").unwrap();
        let info = not_a_folder.join("info.json");
        let named = info.display().to_string();

        let error = dropbox_folder_in_files(&[info]).unwrap_err();
        assert!(
            error.to_string().contains(&named),
            "the error must name {named}, got {error}"
        );
        assert!(matches!(&error, PathsError::DropboxInfo { .. }));
        assert!(
            error
                .to_string()
                .contains("cannot check whether the file exists"),
            "the error says a metadata check failed: {error}"
        );
    }

    #[test]
    fn a_broken_info_json_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        let named = info.display().to_string();

        std::fs::write(&info, "{\"personal\":").unwrap();
        let error = dropbox_folder_in(&info).unwrap_err();
        assert!(
            error.to_string().contains(&named),
            "the error must name {named}, got {error}"
        );
        assert!(matches!(error, PathsError::DropboxInfo { .. }));

        // The file is JSON, but `personal.path` is not a string.
        std::fs::write(&info, r#"{"personal":{"path":7}}"#).unwrap();
        let error = dropbox_folder_in(&info).unwrap_err();
        assert!(
            error.to_string().contains(&named),
            "the error must name {named}, got {error}"
        );
        assert!(error.to_string().contains("personal.path is not a string"));
    }

    /// The rule of the task: `personal.path` if present, else `business.path`.
    /// A `personal` section without a `path` has no `personal.path`, so the
    /// business folder is the one that counts.
    #[test]
    fn a_personal_section_without_a_path_leaves_the_business_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        std::fs::write(
            &info,
            r#"{"personal":{"host":123456789,"is_team":false},"business":{"path":"/home/sam/Dropbox (Acme)","host":987654321}}"#,
        )
        .unwrap();
        assert_eq!(
            dropbox_folder_in(&info).unwrap(),
            Some(PathBuf::from("/home/sam/Dropbox (Acme)"))
        );
    }

    /// The same rule when the `personal` section is not an object at all: it
    /// has no `personal.path` either, so the business folder counts and the
    /// file is not refused.
    #[test]
    fn a_personal_section_that_is_not_an_object_leaves_the_business_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("info.json");
        std::fs::write(
            &info,
            r#"{"personal":null,"business":{"path":"/home/sam/Dropbox (Acme)","host":987654321}}"#,
        )
        .unwrap();
        assert_eq!(
            dropbox_folder_in(&info).unwrap(),
            Some(PathBuf::from("/home/sam/Dropbox (Acme)"))
        );
    }

    /// macOS is a supported desktop (docs/spec.md §1) and Dropbox writes the
    /// same `~/.dropbox/info.json` there as on Linux; the suggestion must not
    /// be silently missing there.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_dropbox_file_is_looked_for_on_macos() {
        let files = dropbox_info_files().expect("the Dropbox files");
        assert!(
            !files.is_empty(),
            "macOS must look for Dropbox's info.json as Linux does"
        );
    }

    #[test]
    fn a_program_is_found_in_a_path_folder() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "ollama.exe"
        } else {
            "ollama"
        };
        let program = second.path().join(name);
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(program_on_path("ollama", &path_var), Some(program));
    }

    /// The path of `to` as seen from `from`, or `None` when the two have no
    /// common root (a different drive on Windows).
    fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
        let from: Vec<_> = from.components().collect();
        let to: Vec<_> = to.components().collect();
        if from.first() != to.first() {
            return None;
        }
        let common = from
            .iter()
            .zip(&to)
            .take_while(|(first, second)| first == second)
            .count();
        let mut relative = PathBuf::new();
        for _ in common..from.len() {
            relative.push("..");
        }
        for component in &to[common..] {
            relative.push(component.as_os_str());
        }
        Some(relative)
    }

    /// R6 review: an `ollama` file in a folder is not found through a `PATH`
    /// entry that is empty or relative. The relative entry is built from the
    /// current folder to the temp folder, so the same file is reached without
    /// changing the process's current folder; `""`, `"."` and `":"` are the
    /// empty and relative entries `split_paths` makes of them.
    #[test]
    fn a_relative_or_empty_path_entry_is_not_searched() {
        let folder = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "ollama.exe"
        } else {
            "ollama"
        };
        let program = folder.path().join(name);
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let absolute = std::env::join_paths([folder.path()]).unwrap();
        assert_eq!(program_on_path("ollama", &absolute), Some(program.clone()));

        let current = std::env::current_dir().unwrap().canonicalize().unwrap();
        let target = folder.path().canonicalize().unwrap();
        if let Some(relative) = relative_path(&current, &target) {
            let path_var = std::env::join_paths([relative.as_path()]).unwrap();
            assert_eq!(
                program_on_path("ollama", &path_var),
                None,
                "a relative PATH entry was searched: {relative:?}"
            );
        }

        for value in ["", ".", ":"] {
            let path_var = OsString::from(value);
            assert_eq!(
                program_on_path("ollama", &path_var),
                None,
                "an empty or relative PATH entry was searched: {value:?}"
            );
        }
    }
}
