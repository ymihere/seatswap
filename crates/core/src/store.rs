//! The account list (`accounts.json`, no secrets) and each account's login
//! (the OS keyring, or 0600 files when there is none). See spec §5, §8.2, §10.
//!
//! Every path comes from [`AppPaths`] and every file write goes through
//! [`crate::atomic::write_atomic`].

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::write_atomic;
use crate::paths::AppPaths;
use crate::types::{Login, OAuthAccount};

/// The colours accounts are assigned from, in order (spec §8.1, §11.3).
pub const PALETTE: [&str; 10] = [
    "#3b82f6", "#f59e0b", "#10b981", "#ef4444", "#8b5cf6", "#ec4899", "#14b8a6", "#f97316",
    "#6366f1", "#84cc16",
];

/// Keyring service name; the keyring user is the account id (spec §5).
const KEYRING_SERVICE: &str = "seatswap";
/// Throw-away entry used to find out whether a keyring works at all.
const KEYRING_PROBE_USER: &str = "seatswap-probe";
const ACCOUNTS_VERSION: u32 = 1;

/// Everything the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not valid JSON; not touching it")]
    Corrupt { path: PathBuf },
    #[error("accounts file version {0} is not supported; this build reads version 1")]
    UnsupportedVersion(u32),
    #[error("invalid abbreviation {0:?}: use 1-2 letters A-Z or digits")]
    InvalidAbbr(String),
    #[error("abbreviation {0:?} is already used by another account")]
    DuplicateAbbr(String),
    #[error("invalid colour {0:?}: use #rrggbb")]
    InvalidColor(String),
    #[error("invalid label {0:?}: use 1 to 40 characters")]
    InvalidLabel(String),
    #[error("unknown account {0}")]
    UnknownAccount(String),
    #[error("account {email} is already stored")]
    DuplicateAccount { email: String },
    #[error("keyring: {0}")]
    Keyring(String),
    #[error("invalid setting: {0}")]
    InvalidSetting(String),
    #[error("invalid account id {0:?}: expected 32 lower-case hex characters")]
    InvalidId(String),
    #[error("Claude Code's account identity (oauthAccount in {path}) is missing or incomplete")]
    MissingIdentity { path: PathBuf },
}

/// One account as stored in `accounts.json`; carries no secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// uuid v4 in simple form: 32 lower-case hex characters.
    pub id: String,
    pub account_uuid: String,
    pub organization_uuid: String,
    /// Free text, 1..=40 characters after trim.
    pub label: String,
    /// 1-2 characters A-Z or 0-9, upper case, unique.
    pub abbr: String,
    /// `#rrggbb`, lower case.
    pub color: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
    /// [`plan_label`], e.g. `"Ultra"` or `"Team"`.
    pub plan: Option<String>,
    /// A copy of `.claude.json`'s `oauthAccount`.
    pub oauth_account: OAuthAccount,
    pub added_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub login_state: LoginState,
    /// `refreshTokenExpiresAt` of the stored login, ms since the epoch.
    pub login_ends_at_ms: Option<i64>,
}

/// Whether the stored login can still be used (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginState {
    Ok,
    LoggedOut,
}

/// Where an account's login is kept. Chosen once, on first use, and recorded in
/// `accounts.json`, so the logins never end up split between two places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretsBackend {
    Keyring,
    File,
}

/// What the tray text next to the menu accounts shows (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrayText {
    Percent,
    PercentReset,
    Off,
}

/// Which limit the tray ring shows (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IconLimit {
    Binding,
    Session,
    Week,
}

/// The UI theme (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

const DEFAULT_POLL_ACTIVE_SECS: u64 = 120;
const DEFAULT_POLL_INACTIVE_SECS: u64 = 900;
const DEFAULT_SUGGEST_AT: u32 = 90;
const DEFAULT_SUGGEST_TARGET_BELOW: u32 = 50;
const DEFAULT_HOTKEY: &str = "Ctrl+Alt+C";

fn default_tray_text() -> TrayText {
    TrayText::Percent
}

fn default_icon_limit() -> IconLimit {
    IconLimit::Binding
}

fn default_poll_active_secs() -> u64 {
    DEFAULT_POLL_ACTIVE_SECS
}

fn default_poll_inactive_secs() -> u64 {
    DEFAULT_POLL_INACTIVE_SECS
}

fn default_suggest_at() -> u32 {
    DEFAULT_SUGGEST_AT
}

fn default_suggest_target_below() -> u32 {
    DEFAULT_SUGGEST_TARGET_BELOW
}

fn default_notify_on_switch() -> bool {
    true
}

fn default_mirror_live() -> bool {
    true
}

fn default_refresh_inactive() -> bool {
    true
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

fn default_start_at_login() -> bool {
    true
}

fn default_theme() -> Theme {
    Theme::System
}

/// Settings (spec §10). Each field has a serde default, so a file written by an
/// older build still loads with that setting's default; a present but invalid
/// value is rejected by [`Settings::validate`], never silently replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_tray_text")]
    pub tray_text: TrayText,
    #[serde(default = "default_icon_limit")]
    pub icon_limit: IconLimit,
    #[serde(default = "default_poll_active_secs")]
    pub poll_active_secs: u64,
    #[serde(default = "default_poll_inactive_secs")]
    pub poll_inactive_secs: u64,
    #[serde(default = "default_suggest_at")]
    pub suggest_at: u32,
    #[serde(default = "default_suggest_target_below")]
    pub suggest_target_below: u32,
    #[serde(default = "default_notify_on_switch")]
    pub notify_on_switch: bool,
    #[serde(default = "default_mirror_live")]
    pub mirror_live: bool,
    #[serde(default = "default_refresh_inactive")]
    pub refresh_inactive: bool,
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    #[serde(default = "default_start_at_login")]
    pub start_at_login: bool,
    #[serde(default = "default_theme")]
    pub theme: Theme,
    #[serde(default)]
    pub claude_config_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tray_text: default_tray_text(),
            icon_limit: default_icon_limit(),
            poll_active_secs: default_poll_active_secs(),
            poll_inactive_secs: default_poll_inactive_secs(),
            suggest_at: default_suggest_at(),
            suggest_target_below: default_suggest_target_below(),
            notify_on_switch: default_notify_on_switch(),
            mirror_live: default_mirror_live(),
            refresh_inactive: default_refresh_inactive(),
            hotkey: default_hotkey(),
            start_at_login: default_start_at_login(),
            theme: default_theme(),
            claude_config_dir: None,
        }
    }
}

impl Settings {
    /// Check the ranges the spec fixes: percents 1..=100, poll intervals >= 30 s.
    pub fn validate(&self) -> Result<(), StoreError> {
        percent("suggestAt", self.suggest_at)?;
        percent("suggestTargetBelow", self.suggest_target_below)?;
        if self.poll_active_secs < 30 {
            return Err(StoreError::InvalidSetting(format!(
                "pollActiveSecs must be at least 30 s, got {}",
                self.poll_active_secs
            )));
        }
        if self.poll_inactive_secs < 30 {
            return Err(StoreError::InvalidSetting(format!(
                "pollInactiveSecs must be at least 30 s, got {}",
                self.poll_inactive_secs
            )));
        }
        if let Some(dir) = &self.claude_config_dir {
            if !dir.is_absolute() {
                return Err(StoreError::InvalidSetting(format!(
                    "claudeConfigDir must be an absolute path, got {}",
                    dir.display()
                )));
            }
        }
        Ok(())
    }
}

fn percent(name: &str, value: u32) -> Result<(), StoreError> {
    if (1..=100).contains(&value) {
        Ok(())
    } else {
        Err(StoreError::InvalidSetting(format!(
            "{name} must be 1..=100, got {value}"
        )))
    }
}

/// `accounts.json` with its version. Only version 1 exists; a different version
/// is [`StoreError::UnsupportedVersion`], never silently upgraded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsFile {
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub settings: Settings,
    /// The secret backend chosen on first use; `None` means no choice was
    /// recorded yet (an older file, or a fresh install).
    #[serde(default)]
    pub secrets_backend: Option<SecretsBackend>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        AccountsFile {
            version: ACCOUNTS_VERSION,
            accounts: Vec::new(),
            settings: Settings::default(),
            secrets_backend: None,
        }
    }
}

impl AccountsFile {
    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn find_by_identity(
        &self,
        account_uuid: &str,
        organization_uuid: &str,
    ) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.account_uuid == account_uuid && a.organization_uuid == organization_uuid)
    }

    /// Look up by id or by abbreviation (case-insensitive) — what the CLI accepts.
    pub fn find_by_key(&self, key: &str) -> Option<&Account> {
        self.find(key).or_else(|| {
            self.accounts
                .iter()
                .find(|a| a.abbr.eq_ignore_ascii_case(key))
        })
    }
}

/// Where one account's login lives. `encrypted()` false means the UI warns that
/// the fallback files are not encrypted (spec §5).
pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError>;
    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError>;
    /// Deleting a login that is not there is fine.
    fn delete(&self, id: &str) -> Result<(), StoreError>;
    fn encrypted(&self) -> bool;
}

/// The OS keyring: service `seatswap`, user = account id, value = the login
/// as JSON text (spec §5).
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringSecrets;

impl KeyringSecrets {
    fn entry(id: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYRING_SERVICE, id).map_err(|e| StoreError::Keyring(e.to_string()))
    }
}

impl SecretStore for KeyringSecrets {
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
        match Self::entry(id)?.get_password() {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| StoreError::Keyring(format!("login for {id} is not valid JSON: {e}"))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError> {
        let text = serde_json::to_string(login)
            .map_err(|e| StoreError::Keyring(format!("cannot encode login for {id}: {e}")))?;
        Self::entry(id)?
            .set_password(&text)
            .map_err(|e| StoreError::Keyring(e.to_string()))
    }

    fn delete(&self, id: &str) -> Result<(), StoreError> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    fn encrypted(&self) -> bool {
        true
    }
}

/// Fallback secret storage: `<dir>/<id>.json`, files 0600 in a 0700 folder.
#[derive(Debug, Clone)]
pub struct FileSecrets {
    dir: PathBuf,
}

impl FileSecrets {
    pub fn new(dir: PathBuf) -> FileSecrets {
        FileSecrets { dir }
    }

    /// The file path for `id`; ids that are not 32 lower-case hex characters are
    /// rejected so a bad id can never escape the folder.
    fn path_for(&self, id: &str) -> Result<PathBuf, StoreError> {
        if id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(self.dir.join(format!("{id}.json")))
        } else {
            Err(StoreError::InvalidId(id.to_string()))
        }
    }

    fn ensure_dir(&self) -> Result<(), StoreError> {
        ensure_dir_0700(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_error(&self.dir, e))
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
        let path = self.path_for(id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&path, e)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| StoreError::Corrupt { path })
    }

    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError> {
        let path = self.path_for(id)?;
        self.ensure_dir()?;
        let bytes = serde_json::to_vec(login).map_err(|e| StoreError::Io {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?;
        write_atomic(&path, &bytes, 0o600).map_err(|e| io_error(&path, e))
    }

    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let path = self.path_for(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    fn encrypted(&self) -> bool {
        false
    }
}

/// Open the secrets for the backend recorded in `accounts.json` (`recorded`) or,
/// on first use (`None`), choose one by probing the keyring and report it so the
/// caller can record it. The second value is the backend to record; the third is
/// the warning text shown when the files are used (spec §5).
///
/// A recorded keyring is never abandoned for files: if it does not answer, the
/// error says the logins are stored in the keyring (they are, and falling back
/// would hide them).
// The tuple shape is the binding contract from docs/implementation.md.
#[allow(clippy::type_complexity)]
pub fn open_secrets(
    app: &AppPaths,
    recorded: Option<SecretsBackend>,
) -> Result<(Box<dyn SecretStore>, SecretsBackend, Option<String>), StoreError> {
    open_secrets_with(app, recorded, probe_keyring)
}

/// [`open_secrets`] with the keyring probe injectable, so tests never touch the
/// real keyring. The probe succeeds when the keyring can store and return a
/// throw-away entry.
#[allow(clippy::type_complexity)]
fn open_secrets_with(
    app: &AppPaths,
    recorded: Option<SecretsBackend>,
    probe: impl FnOnce() -> Result<(), String>,
) -> Result<(Box<dyn SecretStore>, SecretsBackend, Option<String>), StoreError> {
    match recorded {
        Some(SecretsBackend::Keyring) => match probe() {
            Ok(()) => Ok((Box::new(KeyringSecrets), SecretsBackend::Keyring, None)),
            Err(reason) => Err(StoreError::Keyring(format!(
                "the OS keyring is not available: {reason}; the logins are stored in the keyring"
            ))),
        },
        Some(SecretsBackend::File) => {
            let (secrets, warning) = file_secrets(app);
            Ok((secrets, SecretsBackend::File, Some(warning)))
        }
        None => match probe() {
            Ok(()) => Ok((Box::new(KeyringSecrets), SecretsBackend::Keyring, None)),
            Err(reason) => {
                let (secrets, warning) = file_secrets(app);
                Ok((
                    secrets,
                    SecretsBackend::File,
                    Some(format!(
                        "the OS keyring is not available ({reason}); {warning}"
                    )),
                ))
            }
        },
    }
}

/// The fallback store under `app.secrets_dir()` and the warning that names it.
fn file_secrets(app: &AppPaths) -> (Box<dyn SecretStore>, String) {
    let dir = app.secrets_dir();
    let secrets: Box<dyn SecretStore> = Box::new(FileSecrets::new(dir.clone()));
    (
        secrets,
        format!("logins are stored unencrypted in {}", dir.display()),
    )
}

/// Set, read back and delete a throw-away entry: the only reliable keyring test.
fn probe_keyring() -> Result<(), String> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_PROBE_USER).map_err(|e| e.to_string())?;
    entry.set_password("probe").map_err(|e| e.to_string())?;
    entry.get_password().map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The account store: `accounts.json` plus a [`SecretStore`].
pub struct Store {
    app: AppPaths,
    secrets: Box<dyn SecretStore>,
}

impl Store {
    pub fn new(app: AppPaths, secrets: Box<dyn SecretStore>) -> Store {
        Store { app, secrets }
    }

    pub fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    pub fn app_paths(&self) -> &AppPaths {
        &self.app
    }

    /// A missing file is an empty list with default settings. A file that does
    /// not parse is [`StoreError::Corrupt`] and is left untouched.
    pub fn load(&self) -> Result<AccountsFile, StoreError> {
        load_accounts(&self.app)
    }

    /// Pretty JSON, mode 0600, through [`write_atomic`]; creates the data folder
    /// (0700) when it is missing.
    pub fn save(&self, file: &AccountsFile) -> Result<(), StoreError> {
        let path = self.app.accounts_file();
        if file.version != ACCOUNTS_VERSION {
            return Err(StoreError::UnsupportedVersion(file.version));
        }
        ensure_dir_0700(&self.app.data_dir)?;
        let bytes = serde_json::to_vec_pretty(file).map_err(|e| StoreError::Io {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?;
        write_atomic(&path, &bytes, 0o600).map_err(|e| io_error(&path, e))
    }
}

/// Read `accounts.json` without opening a secret store. A missing file is an
/// empty list with default settings. A file that does not parse is
/// [`StoreError::Corrupt`] and is left untouched.
pub fn load_accounts(app: &AppPaths) -> Result<AccountsFile, StoreError> {
    let path = app.accounts_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AccountsFile::default()),
        Err(e) => return Err(io_error(&path, e)),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Corrupt { path: path.clone() })?;
    // Check the version before the rest of the shape: a newer file must be
    // reported as unsupported, not as corrupt.
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| StoreError::Corrupt { path: path.clone() })?;
    if version != ACCOUNTS_VERSION {
        return Err(StoreError::UnsupportedVersion(version));
    }
    serde_json::from_value::<AccountsFile>(value).map_err(|_| StoreError::Corrupt { path })
}

/// The one way to open the store (CLI and GUI): load `accounts.json`, open the
/// secrets for the recorded backend, record the backend on first use (save), and
/// return the store and the warning from [`open_secrets`].
pub fn open_store(app: AppPaths) -> Result<(Store, Option<String>), StoreError> {
    open_store_with(app, probe_keyring)
}

/// [`open_store`] with the keyring probe injectable for tests; no test reaches
/// the real keyring.
fn open_store_with(
    app: AppPaths,
    probe: impl FnOnce() -> Result<(), String>,
) -> Result<(Store, Option<String>), StoreError> {
    // Loading only reads accounts.json; the fallback secrets are never touched
    // before the backend is chosen.
    let mut file = load_accounts(&app)?;
    let (secrets, backend, warning) = open_secrets_with(&app, file.secrets_backend, probe)?;
    let store = Store::new(app, secrets);
    if file.secrets_backend.is_none() {
        file.secrets_backend = Some(backend);
        store.save(&file)?;
    }
    Ok((store, warning))
}

/// "Acme Team" → "AT", "Acme" → "AC", "Private" → "PR", "x" → "X".
/// Words split on whitespace and punctuation. Letters/digits only; a one-word
/// label contributes its first two, a multi-word label the first letter/digit of
/// each of its first two contributing words. Non-ASCII characters are skipped:
/// each word contributes its first ASCII letter or digit, so "Ünïcode Bank" →
/// "NB" and a label with no ASCII letter or digit at all is [`StoreError::InvalidLabel`].
pub fn default_abbr(label: &str) -> Result<String, StoreError> {
    let words: Vec<&str> = label
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut letters: Vec<char> = Vec::new();
    if words.len() >= 2 {
        for word in &words {
            if let Some(c) = word.chars().find(|c| c.is_ascii_alphanumeric()) {
                letters.push(c.to_ascii_uppercase());
                if letters.len() == 2 {
                    break;
                }
            }
        }
    } else if let Some(word) = words.first() {
        letters = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(2)
            .map(|c| c.to_ascii_uppercase())
            .collect();
    }
    if letters.is_empty() {
        return Err(StoreError::InvalidLabel(label.to_string()));
    }
    Ok(letters.into_iter().collect())
}

/// Trim, upper-case, check 1-2 of A-Z 0-9, and check it is not in `others`
/// (the abbreviations of the other accounts; compared case-insensitively).
pub fn validate_abbr(abbr: &str, others: &[&str]) -> Result<String, StoreError> {
    let normalized = abbr.trim().to_ascii_uppercase();
    let valid = (1..=2).contains(&normalized.chars().count())
        && normalized
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !valid {
        return Err(StoreError::InvalidAbbr(abbr.to_string()));
    }
    if others
        .iter()
        .any(|other| other.trim().eq_ignore_ascii_case(&normalized))
    {
        return Err(StoreError::DuplicateAbbr(normalized));
    }
    Ok(normalized)
}

/// `"#rrggbb"` in any case → lower case; anything else is [`StoreError::InvalidColor`].
pub fn validate_color(color: &str) -> Result<String, StoreError> {
    let valid = color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if valid {
        Ok(color.to_ascii_lowercase())
    } else {
        Err(StoreError::InvalidColor(color.to_string()))
    }
}

/// The plan as Claude names it: the identity's `planDisplayName` ("Pro",
/// "Max", "Ultra"), else the login's `subscriptionType` with a capital first
/// letter ("Team", "Enterprise"). Rate-limit tiers (`default_claude_max_20x`)
/// are not shown: the personal plans have names, not multipliers.
pub fn plan_label(login: &Login, identity: &OAuthAccount) -> Option<String> {
    let named = identity
        .plan_display_name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    named.or_else(|| {
        let subscription = login.subscription_type()?.trim();
        let mut chars = subscription.chars();
        let first = chars.next()?;
        Some(first.to_uppercase().chain(chars).collect())
    })
}

impl Account {
    /// Bring the fields that come from the stored login up to date after a
    /// login was stored for this account (switch, live sync, refresh): the plan
    /// ([`plan_label`] with the account's identity), the login's end date and
    /// the login state. Returns whether anything changed. The one place these
    /// fields are written for an existing account.
    pub fn note_stored_login(&mut self, login: &Login) -> bool {
        let plan = plan_label(login, &self.oauth_account);
        let ends_at_ms = login.refresh_token_expires_at_ms();
        let changed = self.plan != plan
            || self.login_ends_at_ms != ends_at_ms
            || self.login_state != LoginState::Ok;
        self.plan = plan;
        self.login_ends_at_ms = ends_at_ms;
        self.login_state = LoginState::Ok;
        changed
    }
}

/// A new [`Account`] for a login and identity. The label is the explicit one,
/// else the organization name, else the email; the abbreviation is the label's
/// [`default_abbr`] made unique; the colour is the first free palette entry, or
/// the least-used one when all ten are taken (ties in palette order).
pub fn new_account(
    login: &Login,
    identity: &OAuthAccount,
    label: Option<&str>,
    file: &AccountsFile,
    now: DateTime<Utc>,
) -> Result<Account, StoreError> {
    // `new_account` has no path argument, so callers that know Claude's
    // identity file (switcher) validate the identity first and name the real
    // path. A direct call still gets a clear message naming the file.
    let (account_uuid, organization_uuid) =
        identity
            .identity()
            .ok_or_else(|| StoreError::MissingIdentity {
                path: PathBuf::from(".claude.json"),
            })?;
    if let Some(existing) = file.find_by_identity(account_uuid, organization_uuid) {
        let email = existing
            .email
            .clone()
            .or_else(|| identity.email().map(str::to_string))
            .unwrap_or_default();
        return Err(StoreError::DuplicateAccount { email });
    }
    let label = match label {
        Some(raw) => validate_label(raw)?,
        None => {
            let raw = identity
                .organization_name()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| identity.email())
                .ok_or_else(|| StoreError::InvalidLabel(String::new()))?;
            validate_label(raw)?
        }
    };
    let taken: HashSet<String> = file
        .accounts
        .iter()
        .map(|a| a.abbr.trim().to_ascii_uppercase())
        .collect();
    let abbr = unique_abbr(&label, &taken)?;
    let color = pick_color(file);
    Ok(Account {
        id: uuid::Uuid::new_v4().simple().to_string(),
        account_uuid: account_uuid.to_string(),
        organization_uuid: organization_uuid.to_string(),
        label,
        abbr,
        color,
        email: identity.email().map(str::to_string),
        org_name: identity.organization_name().map(str::to_string),
        plan: plan_label(login, identity),
        oauth_account: identity.clone(),
        added_at: now,
        last_used_at: None,
        login_state: if login.is_ended() {
            LoginState::LoggedOut
        } else {
            LoginState::Ok
        },
        login_ends_at_ms: login.refresh_token_expires_at_ms(),
    })
}

/// Trim a label and check it is 1..=40 characters; [`StoreError::InvalidLabel`]
/// otherwise. The one label rule, used by account creation and editing.
pub fn validate_label(label: &str) -> Result<String, StoreError> {
    let trimmed = label.trim();
    if (1..=40).contains(&trimmed.chars().count()) {
        Ok(trimmed.to_string())
    } else {
        Err(StoreError::InvalidLabel(label.to_string()))
    }
}

/// The label's default abbreviation, or the first free one made from the label's
/// first letter/digit and each following one, then that first character plus
/// digits 2-9; [`StoreError::DuplicateAbbr`] when all of those are taken.
fn unique_abbr(label: &str, taken: &HashSet<String>) -> Result<String, StoreError> {
    let default = default_abbr(label)?;
    if !taken.contains(&default) {
        return Ok(default);
    }
    let chars: Vec<char> = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let Some(&first) = chars.first() else {
        return Err(StoreError::DuplicateAbbr(default));
    };
    let mut tried: HashSet<String> = HashSet::new();
    for c in &chars[1..] {
        let candidate = format!("{first}{c}");
        if !taken.contains(&candidate) && tried.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    for digit in '2'..='9' {
        let candidate = format!("{first}{digit}");
        if !taken.contains(&candidate) && tried.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(StoreError::DuplicateAbbr(default))
}

fn pick_color(file: &AccountsFile) -> String {
    let mut counts = [0usize; PALETTE.len()];
    for account in &file.accounts {
        for (i, color) in PALETTE.iter().enumerate() {
            if account.color.eq_ignore_ascii_case(color) {
                counts[i] += 1;
            }
        }
    }
    if let Some(i) = counts.iter().position(|&n| n == 0) {
        return PALETTE[i].to_string();
    }
    let mut best = 0;
    for i in 1..counts.len() {
        if counts[i] < counts[best] {
            best = i;
        }
    }
    PALETTE[best].to_string()
}

pub(crate) fn ensure_dir_0700(path: &Path) -> Result<(), StoreError> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(io_error(
            path,
            io::Error::new(io::ErrorKind::AlreadyExists, "exists but is not a folder"),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|e| io_error(path, e))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|e| io_error(path, e))
        }
        Err(e) => Err(io_error(path, e)),
    }
}

fn io_error(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ONE_ID: &str = "6f1c2a9be0d34c118f2d4e5a6b7c8d9e";
    const TWO_ID: &str = "a92d4c1e8b3f4a5d9e6c7b8a1d2e3f40";
    const ONE_ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ONE_ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const TWO_ACCOUNT_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const TWO_ORG_UUID: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample_login() -> Login {
        let credentials = json!({
            "mcpOAuth": {
                "plugin:engineering:linear|5f1c2a9be0d34c11": {
                    "serverName": "plugin:engineering:linear",
                    "accessToken": format!("lin_oauth_{}", "c".repeat(64)),
                    "expiresAt": 1794400000000_i64
                }
            },
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", "a".repeat(95)),
                "refreshToken": format!("sk-ant-ort01-{}", "b".repeat(95)),
                "expiresAt": 1794356040000_i64,
                "refreshTokenExpiresAt": 1796772840000_i64,
                "scopes": [
                    "user:inference", "user:profile", "user:file_upload",
                    "user:mcp_servers", "user:plugins", "user:sessions:claude_code"
                ],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": ONE_ORG_UUID
        });
        Login::from_credentials(credentials.as_object().unwrap()).unwrap()
    }

    fn identity(
        account_uuid: &str,
        organization_uuid: &str,
        org_name: &str,
        email: &str,
    ) -> OAuthAccount {
        serde_json::from_value(json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": organization_uuid,
            "organizationName": org_name,
            "organizationRole": "admin",
            "billingType": "stripe_subscription",
            "hasExtraUsageEnabled": false
        }))
        .unwrap()
    }

    fn sample_identity() -> OAuthAccount {
        identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        )
    }

    fn login_with(subscription: Option<&str>, tier: Option<&str>) -> Login {
        let mut oauth = serde_json::Map::new();
        oauth.insert(
            "refreshToken".to_string(),
            json!(format!("sk-ant-ort01-{}", "b".repeat(95))),
        );
        if let Some(subscription) = subscription {
            oauth.insert("subscriptionType".to_string(), json!(subscription));
        }
        if let Some(tier) = tier {
            oauth.insert("rateLimitTier".to_string(), json!(tier));
        }
        let mut map = serde_json::Map::new();
        map.insert("claudeAiOauth".to_string(), Value::Object(oauth));
        Login(map)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn store_at(dir: &Path) -> Store {
        let app = AppPaths::at(dir);
        let secrets = FileSecrets::new(app.secrets_dir());
        Store::new(app, Box::new(secrets))
    }

    #[test]
    fn load_missing_file_gives_empty_list_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let file = store.load().unwrap();
        assert_eq!(file.version, 1);
        assert!(file.accounts.is_empty());
        assert_eq!(file.settings, Settings::default());
        assert!(!store.app_paths().accounts_file().exists());
    }

    #[test]
    fn save_and_load_round_trip_two_real_shaped_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let now = now();

        let mut file = AccountsFile::default();
        let mut first = new_account(&sample_login(), &sample_identity(), None, &file, now).unwrap();
        first.id = ONE_ID.to_string();
        first.last_used_at = Some(now);
        file.accounts.push(first);
        let second_identity =
            identity(TWO_ACCOUNT_UUID, TWO_ORG_UUID, "Private", "two@example.com");
        let mut second = new_account(&sample_login(), &second_identity, None, &file, now).unwrap();
        second.id = TWO_ID.to_string();
        second.login_state = LoginState::LoggedOut;
        file.accounts.push(second);

        file.settings = Settings {
            tray_text: TrayText::PercentReset,
            icon_limit: IconLimit::Week,
            theme: Theme::Dark,
            claude_config_dir: Some(PathBuf::from("/home/u/.claude")),
            ..Settings::default()
        };

        store.save(&file).unwrap();

        let folder = store.app_paths().data_dir.clone();
        let path = store.app_paths().accounts_file();
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(file.accounts[0].id.len(), 32);
        assert!(file.accounts[0].id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(file.accounts[1].id, TWO_ID);

        let raw = fs::read_to_string(&path).unwrap();
        for expected in [
            "\"version\": 1",
            "\"accountUuid\"",
            "\"organizationUuid\"",
            "\"addedAt\"",
            "\"lastUsedAt\"",
            "\"loginState\": \"loggedOut\"",
            "\"loginEndsAtMs\"",
            "\"oauthAccount\"",
            "\"emailAddress\"",
            "\"trayText\": \"percentReset\"",
            "\"iconLimit\": \"week\"",
            "\"theme\": \"dark\"",
            "\"claudeConfigDir\"",
        ] {
            assert!(raw.contains(expected), "missing {expected} in {raw}");
        }
        assert!(!raw.ends_with('\n'));

        let loaded = store.load().unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.accounts[0].oauth_account, sample_identity());
        assert_eq!(loaded.accounts[0].plan.as_deref(), Some("Team"));
        assert_eq!(loaded.accounts[0].abbr, "AC");
        assert_eq!(loaded.accounts[1].abbr, "PR");
    }

    #[test]
    fn settings_missing_poll_active_secs_loads_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"trayText":"percentReset"}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.poll_active_secs, 120);
        assert_eq!(file.settings.poll_inactive_secs, 900);
        assert_eq!(file.settings.suggest_at, 90);
        assert_eq!(file.settings.suggest_target_below, 50);
        assert_eq!(file.settings.tray_text, TrayText::PercentReset);
        assert_eq!(file.settings.icon_limit, IconLimit::Binding);
        assert_eq!(file.settings.theme, Theme::System);
        assert_eq!(file.settings.hotkey, "Ctrl+Alt+C");
        assert!(file.settings.notify_on_switch);
        assert!(file.settings.mirror_live);
        assert!(file.settings.refresh_inactive);
        assert!(file.settings.start_at_login);
        assert_eq!(file.settings.claude_config_dir, None);
    }

    #[test]
    fn version_two_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":2,"accounts":[],"settings":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            store.load(),
            Err(StoreError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn corrupt_file_stays_corrupt_and_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        let path = store.app_paths().accounts_file();
        let bytes = b"{ this is not json";
        fs::write(&path, bytes).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), bytes);

        // A version-1 file of the wrong shape is corrupt too, and still untouched.
        let wrong_shape = br#"{"version":1,"accounts":{}}"#;
        fs::write(&path, wrong_shape).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), wrong_shape);
    }

    #[test]
    fn settings_validate_rejects_values_outside_the_contract() {
        assert!(Settings::default().validate().is_ok());

        let mut settings = Settings {
            suggest_at: 0,
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(StoreError::InvalidSetting(_))
        ));
        settings.suggest_at = 101;
        assert!(matches!(
            settings.validate(),
            Err(StoreError::InvalidSetting(_))
        ));
        settings.suggest_at = 100;
        assert!(settings.validate().is_ok());

        settings = Settings {
            poll_active_secs: 10,
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(StoreError::InvalidSetting(_))
        ));
        settings.poll_active_secs = 29;
        assert!(settings.validate().is_err());
        settings.poll_active_secs = 30;
        assert!(settings.validate().is_ok());

        settings = Settings {
            poll_inactive_secs: 29,
            ..Settings::default()
        };
        assert!(settings.validate().is_err());

        settings = Settings {
            suggest_target_below: 0,
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
        settings.suggest_target_below = 101;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn settings_validate_rejects_a_relative_claude_config_dir() {
        for relative in ["~/.claude", "claude"] {
            let settings = Settings {
                claude_config_dir: Some(PathBuf::from(relative)),
                ..Settings::default()
            };
            match settings.validate() {
                Err(StoreError::InvalidSetting(message)) => {
                    assert!(message.contains("absolute path"), "got {message}");
                    assert!(message.contains(relative), "got {message}");
                }
                other => panic!("expected InvalidSetting for {relative}, got {other:?}"),
            }
        }

        let absolute = Settings {
            claude_config_dir: Some(PathBuf::from("/home/u/.claude")),
            ..Settings::default()
        };
        assert!(absolute.validate().is_ok());
    }

    #[test]
    fn load_accounts_reads_a_saved_file_without_a_secret_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let mut file = AccountsFile::default();
        file.settings.claude_config_dir = Some(PathBuf::from("/home/u/.claude"));
        store.save(&file).unwrap();

        let loaded = load_accounts(store.app_paths()).unwrap();
        assert_eq!(loaded, file);

        let path = store.app_paths().accounts_file();
        fs::write(&path, b"{ not json").unwrap();
        match load_accounts(store.app_paths()) {
            Err(StoreError::Corrupt { path: reported }) => assert_eq!(reported, path),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn file_secrets_round_trip_overwrite_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let secrets: Box<dyn SecretStore> = Box::new(FileSecrets::new(dir.clone()));
        let login = sample_login();
        assert!(!secrets.encrypted());
        assert_eq!(secrets.get(ONE_ID).unwrap(), None);

        secrets.set(ONE_ID, &login).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), Some(login.clone()));
        let path = dir.join(format!("{ONE_ID}.json"));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);

        let mut changed = login.clone();
        let oauth = changed
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap();
        oauth.insert(
            "accessToken".to_string(),
            json!(format!("sk-ant-oat01-{}", "d".repeat(95))),
        );
        secrets.set(ONE_ID, &changed).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), Some(changed));

        secrets.delete(ONE_ID).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), None);
        assert!(!path.exists());
        // Deleting a missing login is fine.
        secrets.delete(ONE_ID).unwrap();
    }

    #[test]
    fn file_secrets_rejects_ids_that_are_not_32_lowercase_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let secrets = FileSecrets::new(dir.clone());
        let login = sample_login();
        let bad_ids = [
            "../x".to_string(),
            String::new(),
            "A".repeat(32),
            "a".repeat(31),
            "a".repeat(33),
            "g".repeat(32),
            "../../../../etc/passwd".to_string(),
        ];
        for bad in &bad_ids {
            assert!(matches!(secrets.get(bad), Err(StoreError::InvalidId(_))));
            assert!(matches!(
                secrets.set(bad, &login),
                Err(StoreError::InvalidId(_))
            ));
            assert!(matches!(secrets.delete(bad), Err(StoreError::InvalidId(_))));
        }
        assert!(!dir.exists());
    }

    #[test]
    fn default_abbr_cases() {
        assert_eq!(default_abbr("Acme Team").unwrap(), "AT");
        assert_eq!(default_abbr("Acme").unwrap(), "AC");
        assert_eq!(default_abbr("Private").unwrap(), "PR");
        assert_eq!(default_abbr("x").unwrap(), "X");
        assert_eq!(default_abbr("acme-team").unwrap(), "AT");
        assert_eq!(default_abbr("Ünïcode Bank").unwrap(), "NB");
        assert_eq!(default_abbr("42").unwrap(), "42");
        assert!(matches!(
            default_abbr("  "),
            Err(StoreError::InvalidLabel(_))
        ));
        assert!(matches!(default_abbr(""), Err(StoreError::InvalidLabel(_))));
        assert!(matches!(
            default_abbr("🚀"),
            Err(StoreError::InvalidLabel(_))
        ));
    }

    #[test]
    fn validate_abbr_cases() {
        assert_eq!(validate_abbr("as", &[]).unwrap(), "AS");
        assert_eq!(validate_abbr(" a ", &[]).unwrap(), "A");
        assert_eq!(validate_abbr("9", &[]).unwrap(), "9");
        assert!(matches!(
            validate_abbr("ABC", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("A-", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("Ü", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("A", &["a"]),
            Err(StoreError::DuplicateAbbr(_))
        ));
        assert_eq!(validate_abbr("b", &["A", "C"]).unwrap(), "B");
    }

    #[test]
    fn validate_color_cases() {
        assert_eq!(validate_color("#3B82F6").unwrap(), "#3b82f6");
        assert_eq!(validate_color("#3b82f6").unwrap(), "#3b82f6");
        assert!(matches!(
            validate_color("3b82f6"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#3b82f"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#3b82f60"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#gggggg"),
            Err(StoreError::InvalidColor(_))
        ));
    }

    #[test]
    fn plan_label_cases() {
        let personal = |name: &str| {
            let mut identity = sample_identity();
            identity
                .0
                .insert("planDisplayName".to_string(), json!(name));
            identity
        };
        // Personal plans: Claude's own name, never the tier multiplier.
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_20x")),
                &personal("Ultra")
            )
            .as_deref(),
            Some("Ultra")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_5x")),
                &personal("Max")
            )
            .as_deref(),
            Some("Max")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("pro"), Some("default_claude_pro")),
                &personal("Pro")
            )
            .as_deref(),
            Some("Pro")
        );
        // Team seats have no display name: the subscription type, capitalised.
        assert_eq!(
            plan_label(
                &login_with(Some("team"), Some("default_claude_max_5x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Team")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_20x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Max")
        );
        // A blank display name does not hide the subscription type.
        assert_eq!(
            plan_label(&login_with(Some("pro"), None), &personal("  ")).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            plan_label(&login_with(None, None), &sample_identity()),
            None
        );
    }

    #[test]
    fn noting_a_stored_login_updates_plan_end_and_state_once() {
        let identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        let login = login_with(Some("team"), Some("default_claude_max_5x"));
        let mut account =
            new_account(&login, &identity, None, &AccountsFile::default(), now()).unwrap();
        account.plan = Some("team · max 5x".to_string());
        account.login_state = LoginState::LoggedOut;

        assert!(account.note_stored_login(&login));
        assert_eq!(account.plan.as_deref(), Some("Team"));
        assert_eq!(account.login_state, LoginState::Ok);
        assert_eq!(
            account.login_ends_at_ms,
            login.refresh_token_expires_at_ms()
        );
        assert!(
            !account.note_stored_login(&login),
            "nothing changes the second time"
        );
    }

    #[test]
    fn new_account_defaults_from_the_identity() {
        let identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        let file = AccountsFile::default();
        let now = now();
        let account = new_account(&sample_login(), &identity, None, &file, now).unwrap();
        assert_eq!(account.label, "Acme");
        assert_eq!(account.abbr, "AC");
        assert_eq!(account.color, "#3b82f6");
        assert_eq!(account.email.as_deref(), Some("someone@example.com"));
        assert_eq!(account.org_name.as_deref(), Some("Acme"));
        assert_eq!(account.plan.as_deref(), Some("Team"));
        assert_eq!(account.account_uuid, ONE_ACCOUNT_UUID);
        assert_eq!(account.organization_uuid, ONE_ORG_UUID);
        assert_eq!(account.oauth_account, identity);
        assert_eq!(account.added_at, now);
        assert_eq!(account.last_used_at, None);
        assert_eq!(account.login_state, LoginState::Ok);
        assert_eq!(account.login_ends_at_ms, Some(1796772840000));
        assert_eq!(account.id.len(), 32);
        assert!(account
            .id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn new_account_abbr_falls_back_when_the_default_is_taken() {
        let now = now();
        let mut file = AccountsFile::default();
        let first_identity = identity(ONE_ACCOUNT_UUID, ONE_ORG_UUID, "Acme", "one@example.com");
        file.accounts
            .push(new_account(&sample_login(), &first_identity, None, &file, now).unwrap());
        let second_identity = identity(TWO_ACCOUNT_UUID, TWO_ORG_UUID, "Acme", "two@example.com");
        let second = new_account(&sample_login(), &second_identity, None, &file, now).unwrap();
        // "AC" is taken; the label's first letter plus the next free letter is "AM".
        assert_eq!(second.abbr, "AM");
        assert_eq!(second.color, "#f59e0b");
        assert_eq!(
            second.label, "Acme",
            "the second account still gets the org name"
        );
    }

    #[test]
    fn new_account_rejects_a_duplicate_identity() {
        let now = now();
        let mut file = AccountsFile::default();
        let existing_identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        file.accounts
            .push(new_account(&sample_login(), &existing_identity, None, &file, now).unwrap());
        assert!(matches!(
            new_account(&sample_login(), &existing_identity, None, &file, now),
            Err(StoreError::DuplicateAccount { .. })
        ));
    }

    #[test]
    fn new_account_trims_the_explicit_label_and_rejects_a_long_one() {
        let now = now();
        let file = AccountsFile::default();
        let account = new_account(
            &sample_login(),
            &sample_identity(),
            Some("  My Account  "),
            &file,
            now,
        )
        .unwrap();
        assert_eq!(account.label, "My Account");
        assert_eq!(account.abbr, "MA");

        assert!(matches!(
            new_account(&sample_login(), &sample_identity(), Some("   "), &file, now),
            Err(StoreError::InvalidLabel(_))
        ));
        let long = "x".repeat(41);
        assert!(matches!(
            new_account(&sample_login(), &sample_identity(), Some(&long), &file, now),
            Err(StoreError::InvalidLabel(_))
        ));
    }

    #[test]
    fn find_by_key_accepts_the_id_and_the_abbreviation_in_lower_case() {
        let now = now();
        let mut file = AccountsFile::default();
        let account = new_account(
            &sample_login(),
            &sample_identity(),
            Some("Acme"),
            &file,
            now,
        )
        .unwrap();
        let id = account.id.clone();
        file.accounts.push(account);

        assert_eq!(
            file.find_by_key(&id).map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert_eq!(
            file.find_by_key("ac").map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert_eq!(file.find(&id).map(|a| a.id.as_str()), Some(id.as_str()));
        assert_eq!(
            file.find_by_identity(ONE_ACCOUNT_UUID, ONE_ORG_UUID)
                .map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert!(file.find_by_key("zz").is_none());
    }

    #[test]
    fn first_use_with_a_working_probe_records_the_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));

        let (store, warning) = open_store_with(app.clone(), || Ok(())).unwrap();
        assert_eq!(warning, None);
        assert!(store.secrets().encrypted());

        let raw = fs::read_to_string(app.accounts_file()).unwrap();
        assert!(
            raw.contains("\"secretsBackend\": \"keyring\""),
            "accounts.json must record the keyring: {raw}"
        );
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::Keyring)
        );
    }

    #[test]
    fn first_use_with_a_failing_probe_records_files_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let secrets_dir = app.secrets_dir();

        let (store, warning) =
            open_store_with(app.clone(), || Err("D-Bus is slow".to_string())).unwrap();
        let warning = warning.expect("using files always warns");
        assert!(
            warning.contains(&secrets_dir.display().to_string()),
            "the warning must name the secrets folder, got: {warning}"
        );
        assert!(
            warning.contains("unencrypted"),
            "the warning must say the files are not encrypted, got: {warning}"
        );
        assert!(!store.secrets().encrypted());

        let raw = fs::read_to_string(app.accounts_file()).unwrap();
        assert!(
            raw.contains("\"secretsBackend\": \"file\""),
            "accounts.json must record the files: {raw}"
        );
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::File)
        );
    }

    #[test]
    fn a_recorded_keyring_never_falls_back_to_files() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let bootstrap = store_at(&app.data_dir);
        let file = AccountsFile {
            secrets_backend: Some(SecretsBackend::Keyring),
            ..AccountsFile::default()
        };
        bootstrap.save(&file).unwrap();
        let before = fs::read(app.accounts_file()).unwrap();

        match open_store_with(app.clone(), || Err("the keyring is locked".to_string())) {
            Err(StoreError::Keyring(message)) => {
                assert!(
                    message.contains("the logins are stored in the keyring"),
                    "the error must say where the logins are, got: {message}"
                );
                assert!(
                    message.contains("the keyring is locked"),
                    "the error must give the probe reason, got: {message}"
                );
            }
            Err(e) => panic!("expected StoreError::Keyring, got {e}"),
            Ok(_) => panic!("a recorded keyring must not fall back to files"),
        }
        assert_eq!(
            fs::read(app.accounts_file()).unwrap(),
            before,
            "accounts.json must be unchanged when opening fails"
        );
        assert!(
            !app.secrets_dir().exists(),
            "no fallback folder may be created for a recorded keyring"
        );
    }

    #[test]
    fn a_recorded_file_backend_never_probes_the_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));

        let no_probe = || -> Result<(), String> {
            panic!("the probe must not run when the file backend is recorded")
        };
        let (secrets, backend, warning) =
            open_secrets_with(&app, Some(SecretsBackend::File), no_probe).unwrap();
        assert_eq!(backend, SecretsBackend::File);
        assert!(!secrets.encrypted());
        assert!(warning
            .unwrap()
            .contains(&app.secrets_dir().display().to_string()));

        // Same through open_store, with the choice already recorded on disk.
        let bootstrap = store_at(&app.data_dir);
        let file = AccountsFile {
            secrets_backend: Some(SecretsBackend::File),
            ..AccountsFile::default()
        };
        bootstrap.save(&file).unwrap();
        let no_probe = || -> Result<(), String> {
            panic!("the probe must not run when the file backend is recorded")
        };
        let (store, warning) = open_store_with(app.clone(), no_probe).unwrap();
        assert!(!store.secrets().encrypted());
        assert!(warning.is_some());
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::File)
        );
    }

    #[test]
    fn an_old_accounts_file_without_the_backend_loads_and_is_recorded_on_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        fs::create_dir_all(&app.data_dir).unwrap();
        fs::write(
            app.accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{}}"#,
        )
        .unwrap();

        let bootstrap = store_at(&app.data_dir);
        assert_eq!(bootstrap.load().unwrap().secrets_backend, None);

        let (store, warning) = open_store_with(app.clone(), || Ok(())).unwrap();
        assert_eq!(warning, None);
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::Keyring)
        );
    }

    #[test]
    fn the_recorded_backend_survives_a_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let store = store_at(&app.data_dir);

        for (backend, text) in [
            (SecretsBackend::Keyring, "\"keyring\""),
            (SecretsBackend::File, "\"file\""),
        ] {
            let file = AccountsFile {
                secrets_backend: Some(backend),
                ..AccountsFile::default()
            };
            store.save(&file).unwrap();
            let raw = fs::read_to_string(app.accounts_file()).unwrap();
            assert!(
                raw.contains(&format!("\"secretsBackend\": {text}")),
                "unexpected accounts.json: {raw}"
            );
            assert_eq!(store.load().unwrap().secrets_backend, Some(backend));
        }

        let mut file = store.load().unwrap();
        file.secrets_backend = None;
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap().secrets_backend, None);
    }
}
