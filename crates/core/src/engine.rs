//! Usage polling, refreshing inactive logins, the usage cache, and the derived
//! views the CLI and the GUI show (spec §6.3, §7.1, §7.2, §7.4).
//!
//! The engine never refreshes the live account: Claude Code owns that login, and
//! a refresh uses up the old tokens at once. `now` is always a parameter so the
//! tests control time; the only exception is [`UsageCache::set_aside`], whose new
//! name carries the current UTC time.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{Api, ApiError, Usage};
use crate::atomic::{self, write_atomic, Access};
use crate::live;
use crate::locks::{self, LockError};
use crate::paths::{AppPaths, ClaudePaths};
use crate::store::{Account, AccountsFile, LoginState, Settings, Store, StoreError};
use crate::switcher::{LiveState, APP_LOCK_TIMEOUT};
use crate::types::Login;

/// The only usage-cache version this build reads.
const CACHE_VERSION: u32 = 1;
const DAY_MS: i64 = 86_400_000;
/// An inactive login is refreshed this long before its access token expires.
const ACCESS_TOKEN_MARGIN_MS: i64 = 60_000;
/// A pop-up opening refreshes the active account when its data is older than this.
const PANEL_ACTIVE_AFTER_SECS: u64 = 30;
/// A pop-up opening refreshes an inactive account when its data is older than this.
const PANEL_INACTIVE_AFTER_SECS: u64 = 5 * 60;
const BASE_BACKOFF_SECS: u64 = 60;
const MAX_BACKOFF_SECS: u64 = 30 * 60;
const WAITING_MESSAGE: &str = "waiting for Claude Code to renew its login";
const LOGGED_OUT_MESSAGE: &str = "logged out — log in again";
const NO_STORED_LOGIN_MESSAGE: &str = "no stored login — log in again";

/// Everything the engine can fail with. Only failures that affect every account
/// (the store or the cache file) leave [`Engine::poll`] as an error; a failure of
/// one account is reported in that account's [`CacheEntry::last_error`].
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Live(#[from] live::LiveError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not valid JSON; not touching it")]
    CacheCorrupt { path: PathBuf },
}

/// The two calls the engine needs. [`crate::api::Api`] implements it; tests use
/// a fake that records every call and scripts the replies.
pub trait Remote: Send {
    fn usage(&self, access_token: &str) -> Result<Usage, ApiError>;
    fn refresh(
        &self,
        login: &Login,
        expected: (&str, &str),
        now_ms: i64,
    ) -> Result<Login, ApiError>;
}

impl Remote for Api {
    fn usage(&self, access_token: &str) -> Result<Usage, ApiError> {
        Api::usage(self, access_token)
    }

    fn refresh(
        &self,
        login: &Login,
        expected: (&str, &str),
        now_ms: i64,
    ) -> Result<Login, ApiError> {
        Api::refresh(self, login, expected, now_ms)
    }
}

/// What the engine remembers about one account between polls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    /// The last successful usage reply.
    pub usage: Option<Usage>,
    /// When `usage` was fetched.
    pub fetched_at: Option<DateTime<Utc>>,
    /// When the last poll was attempted, successful or not. Due times run from
    /// the later of this and `fetched_at`, so a non-transient failure is not
    /// retried on every tick.
    #[serde(default)]
    pub attempted_at: Option<DateTime<Utc>>,
    /// Human text for the last failure; never a token.
    pub last_error: Option<String>,
    /// Consecutive transient failures.
    pub failures: u32,
    /// No poll before this time (transient back-off, spec §7.1).
    pub backoff_until: Option<DateTime<Utc>>,
}

/// `usage-cache.json`: the last usage reply per account id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCache {
    pub version: u32,
    pub entries: BTreeMap<String, CacheEntry>,
}

impl Default for UsageCache {
    fn default() -> Self {
        UsageCache {
            version: CACHE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl UsageCache {
    /// Read the cache. A missing file is an empty cache; a file that does not
    /// parse (or has another version) is [`EngineError::CacheCorrupt`] and is
    /// left untouched, so the caller can decide to delete it.
    pub fn load(app: &AppPaths) -> Result<UsageCache, EngineError> {
        let path = app.usage_cache_file();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(UsageCache::default())
            }
            Err(source) => return Err(EngineError::Io { path, source }),
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| EngineError::CacheCorrupt { path: path.clone() })?;
        let version = value
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|version| u32::try_from(version).ok());
        if version != Some(CACHE_VERSION) {
            return Err(EngineError::CacheCorrupt { path });
        }
        serde_json::from_value(value).map_err(|_| EngineError::CacheCorrupt { path })
    }

    /// Write the cache as pretty JSON, mode 0600, through
    /// [`crate::atomic::write_atomic`]; creates the data folder (0700) if needed.
    pub fn save(&self, app: &AppPaths) -> Result<(), EngineError> {
        atomic::ensure_private_dir(&app.data_dir).map_err(|source| EngineError::Io {
            path: app.data_dir.clone(),
            source,
        })?;
        let path = app.usage_cache_file();
        let bytes = serde_json::to_vec_pretty(self).map_err(|source| EngineError::Io {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, source),
        })?;
        write_atomic(&path, &bytes, Access::Private)
            .map_err(|source| EngineError::Io { path, source })
    }

    /// Rename a corrupt cache to `usage-cache.json.corrupt-<UTC yyyymmddThhmmss>`
    /// and return the new path, so the GUI can report where it went. The caller
    /// calls this after [`load`](Self::load) returned `CacheCorrupt`.
    pub fn set_aside(app: &AppPaths) -> Result<PathBuf, EngineError> {
        let path = app.usage_cache_file();
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".corrupt-{}", Utc::now().format("%Y%m%dT%H%M%S")));
        let aside = PathBuf::from(name);
        fs::rename(&path, &aside).map_err(|source| EngineError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(aside)
    }
}

/// Why [`Engine::poll`] is being called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollReason {
    /// The regular timer.
    Timer,
    /// The pop-up was opened.
    PanelOpened,
    /// A switch just happened.
    AfterSwitch,
    /// The machine woke up.
    Wake,
}

/// One account as the CLI and the GUI show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub abbr: String,
    pub color: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub is_active: bool,
    pub login_state: LoginState,
    pub login_ends_at_ms: Option<i64>,
    /// floor((ends - now) / 1 day); may be negative.
    pub login_days_left: Option<i64>,
    pub usage: Option<Usage>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// A suggested switch (spec §7.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub from_id: String,
    pub to_id: String,
    pub from_percent: f64,
    pub to_percent: f64,
    pub from_resets_at: Option<DateTime<Utc>>,
}

/// A login that ends soon or has ended (spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LoginWarning {
    Soon { id: String, days_left: i64 },
    Tomorrow { id: String },
    Ended { id: String },
}

/// Polls usage, refreshes inactive logins, and keeps the usage cache. Single
/// threaded; the GUI runs it on a worker thread.
pub struct Engine<R: Remote> {
    claude: ClaudePaths,
    store: Store,
    remote: R,
    cache: UsageCache,
}

impl<R: Remote> Engine<R> {
    /// Open the engine with the cache from `app.usage_cache_file()`.
    pub fn new(claude: ClaudePaths, store: Store, remote: R) -> Result<Self, EngineError> {
        let cache = UsageCache::load(store.app_paths())?;
        Ok(Engine {
            claude,
            store,
            remote,
            cache,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn claude_paths(&self) -> &ClaudePaths {
        &self.claude
    }

    /// Poll every account that is due (spec §7.1), save the cache, and return
    /// the fresh views. An error only for failures that affect every account.
    pub fn poll(
        &mut self,
        reason: PollReason,
        now: DateTime<Utc>,
    ) -> Result<Vec<AccountView>, EngineError> {
        // The list is read only. Every write below loads accounts.json again
        // under the app lock and changes only the one account it is about.
        let file = self.store.load()?;
        let active = self.active_account_id(&file);
        let settings = file.settings.clone();

        for index in 0..file.accounts.len() {
            if file.accounts[index].login_state == LoginState::LoggedOut {
                continue;
            }
            let id = file.accounts[index].id.clone();
            let is_active = active.as_deref() == Some(id.as_str());
            if !self.due(&id, is_active, reason, now, &settings) {
                continue;
            }
            self.cache
                .entries
                .entry(id.clone())
                .or_default()
                .attempted_at = Some(now);
            if is_active {
                self.poll_active(&id, now);
            } else {
                self.poll_inactive(&id, now, &settings)?;
            }
        }

        self.cache.save(self.store.app_paths())?;
        self.views(now)
    }

    /// Views from the cache without any network call. The active account first,
    /// then the account list's order.
    pub fn views(&self, now: DateTime<Utc>) -> Result<Vec<AccountView>, EngineError> {
        let file = self.store.load()?;
        let active = self.active_account_id(&file);
        let mut views: Vec<AccountView> = file
            .accounts
            .iter()
            .map(|account| self.view_for(account, active.as_deref(), now))
            .collect();
        // Stable sort: the active account first, the rest in list order.
        views.sort_by_key(|view| !view.is_active);
        Ok(views)
    }

    /// The active account is the one [`crate::switcher::live_state`] calls
    /// `Known`; that is the only definition of "live". NoLogin, Ended and
    /// Unknown mean no active account. A broken file is only a hint for the
    /// view; it must not stop the other accounts.
    fn active_account_id(&self, file: &AccountsFile) -> Option<String> {
        match crate::switcher::live_state(&self.claude, file) {
            Ok(LiveState::Known { account_id }) => Some(account_id),
            Ok(_) | Err(_) => None,
        }
    }

    fn view_for(&self, account: &Account, active: Option<&str>, now: DateTime<Utc>) -> AccountView {
        let entry = self.cache.entries.get(&account.id);
        AccountView {
            id: account.id.clone(),
            label: account.label.clone(),
            abbr: account.abbr.clone(),
            color: account.color.clone(),
            email: account.email.clone(),
            plan: account.plan.clone(),
            is_active: active == Some(account.id.as_str()),
            login_state: account.login_state,
            login_ends_at_ms: account.login_ends_at_ms,
            login_days_left: account
                .login_ends_at_ms
                .map(|ends_at_ms| (ends_at_ms - now.timestamp_millis()).div_euclid(DAY_MS)),
            usage: entry.and_then(|entry| entry.usage.clone()),
            fetched_at: entry.and_then(|entry| entry.fetched_at),
            last_error: entry.and_then(|entry| entry.last_error.clone()),
        }
    }

    /// Whether this account is due at `now` for `reason` and not backing off.
    /// The interval runs from the later of the last success and the last
    /// attempt, so a failure that is not transient is not retried every tick.
    fn due(
        &self,
        id: &str,
        is_active: bool,
        reason: PollReason,
        now: DateTime<Utc>,
        settings: &Settings,
    ) -> bool {
        let entry = self.cache.entries.get(id);
        if let Some(backoff_until) = entry.and_then(|entry| entry.backoff_until) {
            // A transient failure is retried when its back-off ends, as before.
            return now >= backoff_until;
        }
        let fetched_at = entry.and_then(|entry| entry.fetched_at);
        let attempted_at = entry.and_then(|entry| entry.attempted_at);
        let Some(last) = (match (fetched_at, attempted_at) {
            (Some(fetched), Some(attempted)) => Some(fetched.max(attempted)),
            (Some(fetched), None) => Some(fetched),
            (None, Some(attempted)) => Some(attempted),
            (None, None) => None,
        }) else {
            return true;
        };
        let interval_secs = match reason {
            PollReason::Wake => return true,
            PollReason::AfterSwitch if is_active => return true,
            PollReason::PanelOpened => {
                if is_active {
                    PANEL_ACTIVE_AFTER_SECS
                } else {
                    PANEL_INACTIVE_AFTER_SECS
                }
            }
            PollReason::Timer | PollReason::AfterSwitch => {
                if is_active {
                    settings.poll_active_secs
                } else {
                    settings.poll_inactive_secs
                }
            }
        };
        age_at_least(now, last, interval_secs)
    }

    /// The active account's usage always uses the live file's token, freshly
    /// read, and is never refreshed (spec §6.2, §7.1).
    fn poll_active(&mut self, id: &str, now: DateTime<Utc>) {
        let token = match live::read_login(&self.claude) {
            Ok(Some(login)) => login.access_token().map(str::to_string),
            Ok(None) => None,
            Err(error) => {
                self.set_last_error(id, format!("cannot read the live login: {error}"));
                return;
            }
        };
        let Some(token) = token else {
            self.set_last_error(id, WAITING_MESSAGE.to_string());
            return;
        };

        match self.remote.usage(&token) {
            Ok(usage) => self.record_success(id, usage, now),
            Err(ApiError::Unauthorized) => {
                // Claude Code may have renewed the login between our read and
                // the request: read the live file again and retry exactly once.
                let retry = match live::read_login(&self.claude) {
                    Ok(Some(login)) => login.access_token().map(str::to_string),
                    Ok(None) | Err(_) => None,
                };
                match retry {
                    Some(token) => match self.remote.usage(&token) {
                        Ok(usage) => self.record_success(id, usage, now),
                        Err(ApiError::Unauthorized) => {
                            self.set_last_error(id, WAITING_MESSAGE.to_string())
                        }
                        Err(error) => self.record_api_error(id, error, now),
                    },
                    None => self.set_last_error(id, WAITING_MESSAGE.to_string()),
                }
            }
            Err(error) => self.record_api_error(id, error, now),
        }
    }

    /// An inactive account uses its stored login; when the access token is (about
    /// to be) expired it is refreshed first (spec §6.3).
    fn poll_inactive(
        &mut self,
        id: &str,
        now: DateTime<Utc>,
        settings: &Settings,
    ) -> Result<(), EngineError> {
        let stored = match self.store.secrets().get(id) {
            Ok(Some(login)) => login,
            Ok(None) => {
                self.set_last_error(id, NO_STORED_LOGIN_MESSAGE.to_string());
                return Ok(());
            }
            Err(error) => {
                self.set_last_error(id, format!("cannot read the stored login: {error}"));
                return Ok(());
            }
        };

        if needs_refresh(&stored, now) {
            if settings.refresh_inactive {
                return self.refresh_account(id, &stored, now, false);
            }
            // Nothing to do: the view keeps showing the cached usage and its age.
            return Ok(());
        }

        let Some(token) = stored.access_token().map(str::to_string) else {
            // `needs_refresh` above already covered this; kept for clarity.
            return Ok(());
        };
        match self.remote.usage(&token) {
            Ok(usage) => {
                self.record_success(id, usage, now);
                Ok(())
            }
            // Spec §7.1: a 401 for an inactive account means the stored token is
            // dead; refresh it (same path as an expired token).
            Err(ApiError::Unauthorized) if settings.refresh_inactive => {
                self.refresh_account(id, &stored, now, true)
            }
            Err(error) => {
                self.record_api_error(id, error, now);
                Ok(())
            }
        }
    }

    /// Refresh one inactive login under seatswap's own lock, save it first,
    /// then fetch its usage with the new token (spec §6.3).
    fn refresh_account(
        &mut self,
        id: &str,
        initial: &Login,
        now: DateTime<Utc>,
        from_unauthorized: bool,
    ) -> Result<(), EngineError> {
        let app = self.store.app_paths().clone();
        let lock = match locks::acquire_app_lock(&app, APP_LOCK_TIMEOUT) {
            Ok(lock) => lock,
            // Someone else is switching or refreshing: try this account next round.
            Err(LockError::Busy { .. }) => return Ok(()),
            Err(error) => {
                self.set_last_error(id, format!("cannot take the app lock: {error}"));
                return Ok(());
            }
        };

        // Under the lock the account list and the stored login are read again:
        // never act on the copy read before the lock.
        let file = match self.store.load() {
            Ok(file) => file,
            Err(error) => {
                self.set_last_error(id, format!("cannot read the account list: {error}"));
                return Ok(());
            }
        };
        let Some(account) = file.find(id) else {
            // Removed meanwhile: nothing to refresh or save.
            return Ok(());
        };
        let account_uuid = account.account_uuid.clone();
        let organization_uuid = account.organization_uuid.clone();

        // Make sure the account did not become the live one. If the identity
        // cannot be read, we cannot prove it is inactive, so we leave the login
        // alone rather than risk refreshing the active account.
        match live::read_identity(&self.claude) {
            Ok(Some(identity))
                if identity.identity()
                    == Some((account_uuid.as_str(), organization_uuid.as_str())) =>
            {
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }

        let stored = match self.store.secrets().get(id) {
            Ok(Some(login)) => login,
            Ok(None) => {
                self.set_last_error(id, NO_STORED_LOGIN_MESSAGE.to_string());
                return Ok(());
            }
            Err(error) => {
                self.set_last_error(id, format!("cannot read the stored login: {error}"));
                return Ok(());
            }
        };

        // Someone stored a fresh login while we were waiting for the lock: use
        // it without refreshing. On a 401 a changed login is fresh enough too;
        // the unchanged login still has to be refreshed.
        let fresh = !needs_refresh(&stored, now) && stored.access_token().is_some();
        let changed = stored != *initial;
        if fresh && (changed || !from_unauthorized) {
            let Some(token) = stored.access_token().map(str::to_string) else {
                return Ok(());
            };
            drop(lock);
            match self.remote.usage(&token) {
                Ok(usage) => self.record_success(id, usage, now),
                Err(error) => self.record_api_error(id, error, now),
            }
            return Ok(());
        }

        let refreshed = match self.remote.refresh(
            &stored,
            (&account_uuid, &organization_uuid),
            now.timestamp_millis(),
        ) {
            Ok(login) => login,
            Err(ApiError::InvalidGrant(_)) => {
                self.mark_logged_out(id)?;
                let entry = self.cache.entries.entry(id.to_string()).or_default();
                entry.last_error = Some(LOGGED_OUT_MESSAGE.to_string());
                entry.failures = 0;
                entry.backoff_until = None;
                return Ok(());
            }
            Err(error) => {
                if matches!(error, ApiError::OwnerMismatch { .. }) {
                    // The reply belongs to another account: store nothing.
                    self.set_last_error(id, error.to_string());
                } else {
                    self.record_api_error(id, error, now);
                }
                return Ok(());
            }
        };

        // Save first: the old tokens are dead as soon as the server answered.
        if let Err(error) = self.store.secrets().set(id, &refreshed) {
            self.mark_logged_out(id)?;
            self.set_last_error(id, format!("could not store the renewed login: {error}"));
            return Ok(());
        }
        self.set_last_error_cleared(id);
        self.save_refreshed_login(id, &refreshed)?;
        // The refresh and the saves are done; do not hold the switch lock across
        // the usage HTTP call.
        drop(lock);

        match refreshed.access_token() {
            Some(token) => {
                let token = token.to_string();
                match self.remote.usage(&token) {
                    Ok(usage) => self.record_success(id, usage, now),
                    Err(error) => self.record_api_error(id, error, now),
                }
            }
            None => self.set_last_error(id, "the renewed login has no access token".to_string()),
        }
        Ok(())
    }

    /// Mark one account logged out. Loaded under the caller's app lock and
    /// changed by id; an account that is gone is skipped.
    fn mark_logged_out(&self, id: &str) -> Result<(), EngineError> {
        let mut file = self.store.load()?;
        if let Some(account) = file.accounts.iter_mut().find(|account| account.id == id) {
            if account.login_state != LoginState::LoggedOut {
                account.login_state = LoginState::LoggedOut;
                self.store.save(&file)?;
            }
        }
        Ok(())
    }

    /// Note the renewed login on that one account (plan, end date, state),
    /// loaded under the caller's app lock; an account that is gone is skipped.
    fn save_refreshed_login(&self, id: &str, login: &Login) -> Result<(), EngineError> {
        let mut file = self.store.load()?;
        let Some(account) = file.accounts.iter_mut().find(|account| account.id == id) else {
            return Ok(());
        };
        if account.note_stored_login(login) {
            self.store.save(&file)?;
        }
        Ok(())
    }

    fn record_success(&mut self, id: &str, usage: Usage, now: DateTime<Utc>) {
        let entry = self.cache.entries.entry(id.to_string()).or_default();
        entry.usage = Some(usage);
        entry.fetched_at = Some(now);
        entry.last_error = None;
        entry.failures = 0;
        entry.backoff_until = None;
    }

    fn set_last_error(&mut self, id: &str, message: String) {
        self.cache
            .entries
            .entry(id.to_string())
            .or_default()
            .last_error = Some(message);
    }

    fn set_last_error_cleared(&mut self, id: &str) {
        if let Some(entry) = self.cache.entries.get_mut(id) {
            entry.last_error = None;
        }
    }

    /// Record an API failure. Transient failures back off 1, 2, 4, 8, 16, 30,
    /// 30 … minutes (spec §7.1); a rate-limit reply waits at least its
    /// `Retry-After`. Anything else is not retried until it is due again.
    fn record_api_error(&mut self, id: &str, error: ApiError, now: DateTime<Utc>) {
        if !error.is_transient() {
            let entry = self.cache.entries.entry(id.to_string()).or_default();
            entry.failures = 0;
            entry.backoff_until = None;
            entry.last_error = Some(error.to_string());
            return;
        }

        let retry_after_secs = match &error {
            ApiError::RateLimited { retry_after_secs } => *retry_after_secs,
            _ => None,
        };
        let entry = self.cache.entries.entry(id.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        // failures 1..=6 → 1, 2, 4, 8, 16, 32 minutes, capped at 30.
        let exponent = entry.failures.saturating_sub(1).min(6);
        let base_secs = BASE_BACKOFF_SECS
            .saturating_mul(1_u64 << exponent)
            .min(MAX_BACKOFF_SECS);
        let secs = retry_after_secs.map_or(base_secs, |retry_after| base_secs.max(retry_after));
        let backoff = chrono::Duration::try_seconds(i64::try_from(secs).unwrap_or(i64::MAX))
            .unwrap_or(chrono::Duration::MAX);
        entry.backoff_until = now.checked_add_signed(backoff);
        entry.last_error = Some(error.to_string());
    }
}

/// §7.4: the active account's binding limit is at least `suggest_at` and another
/// Ok account is under `suggest_target_below`; the lowest such candidate wins.
pub fn suggestion(views: &[AccountView], settings: &Settings) -> Option<Suggestion> {
    let active = views.iter().find(|view| view.is_active)?;
    let from_limit = active.usage.as_ref()?.binding()?;
    if from_limit.percent < f64::from(settings.suggest_at) {
        return None;
    }
    let (candidate, to_percent) = views
        .iter()
        .filter(|view| !view.is_active && view.login_state == LoginState::Ok)
        .filter_map(|view| {
            let limit = view.usage.as_ref()?.binding()?;
            if limit.percent < f64::from(settings.suggest_target_below) {
                Some((view, limit.percent))
            } else {
                None
            }
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))?;
    Some(Suggestion {
        from_id: active.id.clone(),
        to_id: candidate.id.clone(),
        from_percent: from_limit.percent,
        to_percent,
        from_resets_at: from_limit.resets_at,
    })
}

/// §6.3: 5..=2 days left → [`LoginWarning::Soon`], 0..=1 → `Tomorrow`,
/// negative → `Ended`; 6 or more days is not a warning.
pub fn login_warnings(views: &[AccountView]) -> Vec<LoginWarning> {
    let mut warnings = Vec::new();
    for view in views {
        let Some(days_left) = view.login_days_left else {
            continue;
        };
        if days_left < 0 {
            warnings.push(LoginWarning::Ended {
                id: view.id.clone(),
            });
        } else if days_left <= 1 {
            warnings.push(LoginWarning::Tomorrow {
                id: view.id.clone(),
            });
        } else if days_left <= 5 {
            warnings.push(LoginWarning::Soon {
                id: view.id.clone(),
                days_left,
            });
        }
    }
    warnings
}

/// True when an inactive account's stored access token is missing or expires
/// within the refresh margin (spec §6.3).
fn needs_refresh(login: &Login, now: DateTime<Utc>) -> bool {
    login.access_token().is_none()
        || login.expires_at_ms().is_none_or(|expires_at_ms| {
            expires_at_ms
                <= now
                    .timestamp_millis()
                    .saturating_add(ACCESS_TOKEN_MARGIN_MS)
        })
}

fn age_at_least(now: DateTime<Utc>, fetched_at: DateTime<Utc>, secs: u64) -> bool {
    let elapsed_ms = now
        .timestamp_millis()
        .saturating_sub(fetched_at.timestamp_millis());
    let needed_ms = i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000);
    elapsed_ms >= needed_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use crate::api::{Limit, LimitKind, Severity};
    use crate::paths::AppPaths;
    use crate::store::FileSecrets;
    use crate::types::OAuthAccount;

    const A_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const A_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const A_ORG: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const B_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const B_ORG: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";
    const C_UUID: &str = "2b7e9c4d-1a3f-4c5e-8b6d-7f0a1c2e3d4b";
    const C_ORG: &str = "9a1c3e5f-7b9d-4f2a-8c6e-0d1b3a5c7e9f";
    const HOUR_MS: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        t0() + chrono::Duration::seconds(seconds)
    }

    fn t0_ms() -> i64 {
        t0().timestamp_millis()
    }

    fn token(prefix: &str, fill: char) -> String {
        format!("{prefix}{}", fill.to_string().repeat(95))
    }

    fn live_access() -> String {
        token("sk-ant-oat01-", 'a')
    }

    fn new_live_access() -> String {
        token("sk-ant-oat01-", 'd')
    }

    fn b_old_access() -> String {
        token("sk-ant-oat01-", 'b')
    }

    fn b_new_access() -> String {
        token("sk-ant-oat01-", 'c')
    }

    fn b_refresh_token() -> String {
        token("sk-ant-ort01-", 'e')
    }

    fn b_new_refresh_token() -> String {
        token("sk-ant-ort01-", 'f')
    }

    fn login(
        access: &str,
        refresh: &str,
        expires_at_ms: i64,
        end_ms: i64,
        organization_uuid: &str,
    ) -> Login {
        let credentials = json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at_ms,
                "refreshTokenExpiresAt": end_ms,
                "scopes": [
                    "user:inference", "user:profile", "user:file_upload",
                    "user:mcp_servers", "user:plugins", "user:sessions:claude_code"
                ],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": organization_uuid
        });
        Login::from_credentials(credentials.as_object().unwrap()).unwrap()
    }

    fn live_login() -> Login {
        login(
            &live_access(),
            &token("sk-ant-ort01-", '1'),
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            A_ORG,
        )
    }

    fn b_stored_login() -> Login {
        login(
            &b_old_access(),
            &b_refresh_token(),
            t0_ms() - 10 * 60_000,
            t0_ms() + 20 * DAY,
            B_ORG,
        )
    }

    fn identity(
        account_uuid: &str,
        organization_uuid: &str,
        organization_name: &str,
        email: &str,
    ) -> OAuthAccount {
        serde_json::from_value(json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": organization_uuid,
            "organizationName": organization_name,
            "organizationRole": "admin",
            "billingType": "stripe_subscription",
            "hasExtraUsageEnabled": false
        }))
        .unwrap()
    }

    fn account(
        id: &str,
        label: &str,
        abbr: &str,
        color: &str,
        identity: &OAuthAccount,
        login_ends_at_ms: Option<i64>,
    ) -> Account {
        Account {
            id: id.to_string(),
            account_uuid: identity.account_uuid().unwrap().to_string(),
            organization_uuid: identity.organization_uuid().unwrap().to_string(),
            label: label.to_string(),
            abbr: abbr.to_string(),
            color: color.to_string(),
            email: identity.email().map(str::to_string),
            org_name: identity.organization_name().map(str::to_string),
            plan: Some("Team".to_string()),
            oauth_account: identity.clone(),
            added_at: t0(),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms,
        }
    }

    fn usage_with(percent: f64, resets_at: Option<DateTime<Utc>>) -> Usage {
        Usage {
            limits: vec![Limit {
                kind: LimitKind::Session,
                percent,
                severity: Severity::Normal,
                resets_at,
                model: None,
                is_active: true,
            }],
            extra_usage_enabled: false,
        }
    }

    fn write_live(claude: &ClaudePaths, login: &Login, identity: &OAuthAccount) {
        fs::create_dir_all(&claude.config_dir).unwrap();
        live::write_login(claude, Some(login)).unwrap();
        let document = json!({"numStartups": 41, "oauthAccount": identity});
        fs::write(
            &claude.identity_file,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Usage {
            token: String,
        },
        Refresh {
            refresh_token: String,
            expected: (String, String),
        },
    }

    type UsageHook = Box<dyn Fn(&str) + Send>;
    type RefreshHook = Box<dyn Fn() + Send>;

    #[derive(Default)]
    struct FakeInner {
        usage_results: Mutex<HashMap<String, VecDeque<Result<Usage, ApiError>>>>,
        refresh_results: Mutex<HashMap<String, VecDeque<Result<Login, ApiError>>>>,
        calls: Mutex<Vec<Call>>,
        on_usage: Mutex<Option<UsageHook>>,
        on_refresh: Mutex<Option<RefreshHook>>,
        forbidden_refresh: Mutex<Option<(String, String)>>,
    }

    #[derive(Clone, Default)]
    struct FakeRemote {
        inner: Arc<FakeInner>,
    }

    impl FakeRemote {
        fn script_usage(&self, token: &str, result: Result<Usage, ApiError>) {
            self.inner
                .usage_results
                .lock()
                .unwrap()
                .entry(token.to_string())
                .or_default()
                .push_back(result);
        }

        fn script_usage_ok(&self, token: &str, percent: f64) {
            self.script_usage(token, Ok(usage_with(percent, None)));
        }

        fn script_refresh(&self, refresh_token: &str, result: Result<Login, ApiError>) {
            self.inner
                .refresh_results
                .lock()
                .unwrap()
                .entry(refresh_token.to_string())
                .or_default()
                .push_back(result);
        }

        fn script_refresh_ok(&self, refresh_token: &str, login: Login) {
            self.script_refresh(refresh_token, Ok(login));
        }

        fn on_usage(&self, hook: UsageHook) {
            *self.inner.on_usage.lock().unwrap() = Some(hook);
        }

        fn on_refresh(&self, hook: RefreshHook) {
            *self.inner.on_refresh.lock().unwrap() = Some(hook);
        }

        fn forbid_refresh(&self, account_uuid: &str, organization_uuid: &str) {
            *self.inner.forbidden_refresh.lock().unwrap() =
                Some((account_uuid.to_string(), organization_uuid.to_string()));
        }

        fn calls(&self) -> Vec<Call> {
            self.inner.calls.lock().unwrap().clone()
        }

        fn usage_tokens(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Usage { token } => Some(token),
                    Call::Refresh { .. } => None,
                })
                .collect()
        }

        fn refresh_calls(&self) -> Vec<(String, (String, String))> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Refresh {
                        refresh_token,
                        expected,
                    } => Some((refresh_token, expected)),
                    Call::Usage { .. } => None,
                })
                .collect()
        }
    }

    impl Remote for FakeRemote {
        fn usage(&self, access_token: &str) -> Result<Usage, ApiError> {
            self.inner.calls.lock().unwrap().push(Call::Usage {
                token: access_token.to_string(),
            });
            if let Some(hook) = self.inner.on_usage.lock().unwrap().as_ref() {
                hook(access_token);
            }
            let mut results = self.inner.usage_results.lock().unwrap();
            results
                .get_mut(access_token)
                .unwrap_or_else(|| panic!("unexpected usage call"))
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted usage reply left"))
        }

        fn refresh(
            &self,
            login: &Login,
            expected: (&str, &str),
            _now_ms: i64,
        ) -> Result<Login, ApiError> {
            let refresh_token = login
                .refresh_token()
                .expect("a refresh call needs a refresh token")
                .to_string();
            self.inner.calls.lock().unwrap().push(Call::Refresh {
                refresh_token: refresh_token.clone(),
                expected: (expected.0.to_string(), expected.1.to_string()),
            });
            if let Some((account_uuid, organization_uuid)) =
                self.inner.forbidden_refresh.lock().unwrap().as_ref()
            {
                assert!(
                    expected.0 != account_uuid || expected.1 != organization_uuid,
                    "the engine tried to refresh the active account"
                );
            }
            if let Some(hook) = self.inner.on_refresh.lock().unwrap().as_ref() {
                hook();
            }
            let mut results = self.inner.refresh_results.lock().unwrap();
            results
                .get_mut(&refresh_token)
                .unwrap_or_else(|| panic!("unexpected refresh call"))
                .pop_front()
                .unwrap_or_else(|| panic!("no scripted refresh reply left"))
        }
    }

    struct FailingSetSecrets {
        inner: FileSecrets,
    }

    impl crate::store::SecretStore for FailingSetSecrets {
        fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
            self.inner.get(id)
        }

        fn set(&self, _id: &str, _login: &Login) -> Result<(), StoreError> {
            Err(StoreError::Keyring(
                "the test secret store is read-only".to_string(),
            ))
        }

        fn delete(&self, id: &str) -> Result<(), StoreError> {
            self.inner.delete(id)
        }

        fn encrypted(&self) -> bool {
            false
        }
    }

    /// Returns the old login on the first read and `newer` from the second read
    /// on, so a test can swap the stored login while the engine is between its
    /// unlocked read and the read under the app lock.
    struct SwappingSecrets {
        inner: FileSecrets,
        id: String,
        newer: Login,
        gets: AtomicUsize,
    }

    impl SwappingSecrets {
        fn new(inner: FileSecrets, id: &str, newer: Login) -> SwappingSecrets {
            SwappingSecrets {
                inner,
                id: id.to_string(),
                newer,
                gets: AtomicUsize::new(0),
            }
        }
    }

    impl crate::store::SecretStore for SwappingSecrets {
        fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
            if id == self.id && self.gets.fetch_add(1, Ordering::SeqCst) == 1 {
                return Ok(Some(self.newer.clone()));
            }
            self.inner.get(id)
        }

        fn set(&self, id: &str, login: &Login) -> Result<(), StoreError> {
            self.inner.set(id, login)
        }

        fn delete(&self, id: &str) -> Result<(), StoreError> {
            self.inner.delete(id)
        }

        fn encrypted(&self) -> bool {
            false
        }
    }

    struct Fixture {
        claude: ClaudePaths,
        app: AppPaths,
        store: Store,
        fake: FakeRemote,
        file: AccountsFile,
    }

    impl Fixture {
        fn new(tmp: &tempfile::TempDir) -> Fixture {
            let config_dir = tmp.path().join("cfg");
            fs::create_dir(&config_dir).unwrap();
            let claude = ClaudePaths::for_config_dir(&config_dir, tmp.path());
            let app = AppPaths::at(&tmp.path().join("app"));

            let a_identity = identity(A_UUID, A_ORG, "Acme", "someone@example.com");
            let b_identity = identity(B_UUID, B_ORG, "Private", "private@example.com");
            write_live(&claude, &live_login(), &a_identity);

            let store = Store::new(app.clone(), Box::new(FileSecrets::new(app.secrets_dir())));
            let b_stored = b_stored_login();
            let mut file = AccountsFile::default();
            file.accounts.push(account(
                A_ID,
                "Acme",
                "AC",
                "#3b82f6",
                &a_identity,
                Some(t0_ms() + 20 * DAY),
            ));
            file.accounts.push(account(
                B_ID,
                "Private",
                "P",
                "#f59e0b",
                &b_identity,
                b_stored.refresh_token_expires_at_ms(),
            ));
            store.save(&file).unwrap();
            store.secrets().set(B_ID, &b_stored).unwrap();

            let fake = FakeRemote::default();
            fake.forbid_refresh(A_UUID, A_ORG);
            Fixture {
                claude,
                app,
                store,
                fake,
                file,
            }
        }

        fn save_file(&mut self) {
            self.store.save(&self.file).unwrap();
        }

        fn into_engine(self) -> (AppPaths, FakeRemote, Engine<FakeRemote>) {
            let fake = self.fake.clone();
            let engine = Engine::new(self.claude, self.store, self.fake).unwrap();
            (self.app, fake, engine)
        }
    }

    fn view(
        id: &str,
        active: bool,
        state: LoginState,
        percent: Option<f64>,
        days_left: Option<i64>,
    ) -> AccountView {
        AccountView {
            id: id.to_string(),
            label: id.to_string(),
            abbr: "AB".to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            is_active: active,
            login_state: state,
            login_ends_at_ms: days_left.map(|days| t0_ms() + days * DAY),
            login_days_left: days_left,
            usage: percent.map(|percent| usage_with(percent, None)),
            fetched_at: Some(t0()),
            last_error: None,
        }
    }

    #[test]
    fn poll_refreshes_the_inactive_account_and_uses_the_live_token_for_the_active_one() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let app = fixture.app.clone();
        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh_ok(
            &b_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 40 * DAY,
                B_ORG,
            ),
        );
        fake.script_usage_ok(&b_new_access(), 12.0);

        let secret_path = app.secrets_dir().join(format!("{B_ID}.json"));
        let new_token = b_new_access();
        fake.on_usage(Box::new(move |token| {
            if token == new_token {
                let stored: Login =
                    serde_json::from_slice(&fs::read(&secret_path).unwrap()).unwrap();
                assert_eq!(stored.access_token(), Some(new_token.as_str()));
            }
        }));

        let (_app, fake, mut engine) = fixture.into_engine();
        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        assert_eq!(
            fake.usage_tokens(),
            vec![live_access(), b_new_access()],
            "A with the live token, then B with the refreshed token"
        );
        assert_eq!(
            fake.refresh_calls(),
            vec![(b_refresh_token(), (B_UUID.to_string(), B_ORG.to_string()))]
        );

        let stored = engine
            .store()
            .secrets()
            .get(B_ID)
            .unwrap()
            .expect("B's login must be stored");
        assert_eq!(stored.access_token(), Some(b_new_access().as_str()));

        let file = engine.store().load().unwrap();
        assert_eq!(file.accounts[1].login_state, LoginState::Ok);

        assert!(app.usage_cache_file().exists(), "the cache must be written");
        let cache = UsageCache::load(&app).unwrap();
        assert_eq!(
            cache.entries.get(A_ID).and_then(|entry| entry.fetched_at),
            Some(t0())
        );
        assert_eq!(
            cache.entries.get(B_ID).and_then(|entry| entry.fetched_at),
            Some(t0())
        );
        assert!(cache.entries.get(A_ID).unwrap().usage.is_some());
        assert!(cache.entries.get(B_ID).unwrap().usage.is_some());

        assert!(views.iter().find(|view| view.id == A_ID).unwrap().is_active);
        assert!(views.iter().all(|view| view.last_error.is_none()));
    }

    #[test]
    fn a_refresh_replaces_a_stale_plan_label() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        // A label written by an older version, before plan names.
        fixture.file.accounts[1].plan = Some("team · max 5x".to_string());
        fixture.save_file();
        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 40.0);
        fake.script_refresh_ok(
            &b_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 40 * DAY,
                B_ORG,
            ),
        );
        fake.script_usage_ok(&b_new_access(), 12.0);

        let (_app, _fake, mut engine) = fixture.into_engine();
        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        let file = engine.store().load().unwrap();
        assert_eq!(file.find(B_ID).unwrap().plan.as_deref(), Some("Team"));
        assert_eq!(
            views
                .iter()
                .find(|view| view.id == B_ID)
                .unwrap()
                .plan
                .as_deref(),
            Some("Team")
        );
    }

    #[test]
    fn refresh_uses_the_login_stored_under_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        // The first, unlocked read sees an expired login with the old refresh
        // token; the read under the app lock sees the newer stored login.
        let newer = login(
            &b_old_access(),
            &b_new_refresh_token(),
            t0_ms() - 10 * 60_000,
            t0_ms() + 20 * DAY,
            B_ORG,
        );
        let inner = FileSecrets::new(fixture.app.secrets_dir());
        fixture.store = Store::new(
            fixture.app.clone(),
            Box::new(SwappingSecrets::new(inner, B_ID, newer)),
        );

        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh_ok(
            &b_new_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 20 * DAY,
                B_ORG,
            ),
        );
        fake.script_usage_ok(&b_new_access(), 12.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        assert_eq!(
            fake.refresh_calls(),
            vec![(
                b_new_refresh_token(),
                (B_UUID.to_string(), B_ORG.to_string())
            )],
            "the refresh must use the login read under the lock"
        );
        let b = views.iter().find(|view| view.id == B_ID).unwrap();
        assert_eq!(b.login_state, LoginState::Ok);
        assert!(b.last_error.is_none());
        let stored = engine.store().secrets().get(B_ID).unwrap().unwrap();
        assert_eq!(stored.access_token(), Some(b_new_access().as_str()));
    }

    #[test]
    fn refresh_skips_a_login_that_became_fresh_under_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        let newer = login(
            &b_new_access(),
            &b_new_refresh_token(),
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            B_ORG,
        );
        let inner = FileSecrets::new(fixture.app.secrets_dir());
        fixture.store = Store::new(
            fixture.app.clone(),
            Box::new(SwappingSecrets::new(inner, B_ID, newer)),
        );

        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_usage_ok(&b_new_access(), 12.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        assert!(
            fake.refresh_calls().is_empty(),
            "a fresh stored login must not be refreshed"
        );
        assert_eq!(fake.usage_tokens(), vec![live_access(), b_new_access()]);
        assert_eq!(
            views
                .iter()
                .find(|view| view.id == B_ID)
                .unwrap()
                .login_state,
            LoginState::Ok
        );
    }

    #[test]
    fn poll_keeps_an_account_added_during_the_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh_ok(
            &b_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 20 * DAY,
                B_ORG,
            ),
        );
        fake.script_usage_ok(&b_new_access(), 12.0);

        let added = Arc::new(Mutex::new(None));
        let added_in_hook = Arc::clone(&added);
        let app = fixture.app.clone();
        fake.on_refresh(Box::new(move || {
            let store = Store::new(app.clone(), Box::new(FileSecrets::new(app.secrets_dir())));
            let mut file = store.load().unwrap();
            let third_identity = identity(C_UUID, C_ORG, "Third", "third@example.com");
            let third_login = login(
                &token("sk-ant-oat01-", 'g'),
                &token("sk-ant-ort01-", 'h'),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 20 * DAY,
                C_ORG,
            );
            let account = crate::store::new_account(
                &third_login,
                &third_identity,
                Some("Third"),
                &file,
                t0(),
            )
            .unwrap();
            *added_in_hook.lock().unwrap() = Some(account.id.clone());
            file.accounts.push(account);
            store.save(&file).unwrap();
        }));

        let (_app, fake, mut engine) = fixture.into_engine();
        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        let third_id = added
            .lock()
            .unwrap()
            .clone()
            .expect("the hook added a third account");
        let file = engine.store().load().unwrap();
        assert!(
            file.find(&third_id).is_some(),
            "the poll must not overwrite the account added during the refresh"
        );
        let b = file.find(B_ID).unwrap();
        assert_eq!(b.login_state, LoginState::Ok);
        assert_eq!(b.login_ends_at_ms, Some(t0_ms() + 20 * DAY));
        assert_eq!(
            views
                .iter()
                .find(|view| view.id == B_ID)
                .unwrap()
                .login_state,
            LoginState::Ok
        );
        assert_eq!(fake.refresh_calls().len(), 1);
    }

    #[test]
    fn poll_intervals_and_panel_opened_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let fake = fixture.fake.clone();
        for _ in 0..4 {
            fake.script_usage_ok(&live_access(), 97.0);
        }
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh_ok(
            &b_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 20 * DAY,
                B_ORG,
            ),
        );
        fake.script_usage_ok(&b_new_access(), 12.0);
        fake.script_usage_ok(&b_new_access(), 12.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        engine.poll(PollReason::Timer, t0()).unwrap();
        assert_eq!(fake.usage_tokens().len(), 2);

        // 60 s later neither is due.
        engine.poll(PollReason::Timer, at(60)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 2);

        // 130 s later only the active account (120 s rule).
        engine.poll(PollReason::Timer, at(130)).unwrap();
        assert_eq!(
            fake.usage_tokens(),
            vec![live_access(), b_new_access(), live_access()]
        );

        // Panel opened 40 s later: only the active account (30 s rule).
        engine.poll(PollReason::PanelOpened, at(170)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 4);
        assert_eq!(fake.usage_tokens()[3], live_access());

        // 299 s after the first poll B is still not due; A is.
        engine.poll(PollReason::PanelOpened, at(299)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 5);
        assert_eq!(fake.usage_tokens()[4], live_access());

        // At exactly 5 minutes B is due and A is not (1 s after its own fetch).
        engine.poll(PollReason::PanelOpened, at(300)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 6);
        assert_eq!(fake.usage_tokens()[5], b_new_access());
        assert_eq!(fake.refresh_calls().len(), 1);
    }

    #[test]
    fn the_active_account_is_never_refreshed_even_with_an_expired_live_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        let a_identity = identity(A_UUID, A_ORG, "Acme", "someone@example.com");
        let expired_live = login(
            &live_access(),
            &token("sk-ant-ort01-", '1'),
            t0_ms() - 10 * 60_000,
            t0_ms() + 20 * DAY,
            A_ORG,
        );
        write_live(&fixture.claude, &expired_live, &a_identity);
        // B is not part of this check.
        fixture.file.accounts[1].login_state = LoginState::LoggedOut;
        fixture.save_file();

        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        assert_eq!(fake.usage_tokens(), vec![live_access()]);
        assert!(
            fake.refresh_calls().is_empty(),
            "the active login must never be refreshed"
        );
        assert!(views.iter().find(|view| view.id == A_ID).unwrap().is_active);
    }

    #[test]
    fn active_401_rereads_the_live_file_and_retries_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        fixture.file.accounts[1].login_state = LoginState::LoggedOut;
        fixture.save_file();

        let fake = fixture.fake.clone();
        fake.script_usage(&live_access(), Err(ApiError::Unauthorized));
        fake.script_usage_ok(&new_live_access(), 97.0);
        let claude = fixture.claude.clone();
        let new_live = login(
            &new_live_access(),
            &token("sk-ant-ort01-", '2'),
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            A_ORG,
        );
        let old_token = live_access();
        fake.on_usage(Box::new(move |token| {
            if token == old_token {
                let identity = identity(A_UUID, A_ORG, "Acme", "someone@example.com");
                write_live(&claude, &new_live, &identity);
            }
        }));
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();

        assert_eq!(
            fake.usage_tokens(),
            vec![live_access(), new_live_access()],
            "the 401 must be retried once with the freshly read live token"
        );
        let active = views.iter().find(|view| view.id == A_ID).unwrap();
        assert!(active.usage.is_some());
        assert!(active.last_error.is_none());
    }

    #[test]
    fn a_second_401_waits_for_claude_code_to_renew_the_login() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        fixture.file.accounts[1].login_state = LoginState::LoggedOut;
        fixture.save_file();

        let fake = fixture.fake.clone();
        fake.script_usage(&live_access(), Err(ApiError::Unauthorized));
        fake.script_usage(&new_live_access(), Err(ApiError::Unauthorized));
        let claude = fixture.claude.clone();
        let new_live = login(
            &new_live_access(),
            &token("sk-ant-ort01-", '2'),
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            A_ORG,
        );
        let old_token = live_access();
        fake.on_usage(Box::new(move |token| {
            if token == old_token {
                let identity = identity(A_UUID, A_ORG, "Acme", "someone@example.com");
                write_live(&claude, &new_live, &identity);
            }
        }));
        let (_app, _fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();
        let active = views.iter().find(|view| view.id == A_ID).unwrap();
        assert_eq!(active.last_error.as_deref(), Some(WAITING_MESSAGE));
    }

    #[test]
    fn invalid_grant_marks_the_account_logged_out_and_stops_polling_it() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh(
            &b_refresh_token(),
            Err(ApiError::InvalidGrant(
                "Refresh token not found or invalid".to_string(),
            )),
        );
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();
        let b = views.iter().find(|view| view.id == B_ID).unwrap();
        assert_eq!(b.login_state, LoginState::LoggedOut);
        assert_eq!(b.last_error.as_deref(), Some(LOGGED_OUT_MESSAGE));
        assert!(!b
            .last_error
            .as_deref()
            .unwrap()
            .contains(&b_refresh_token()));

        let file = engine.store().load().unwrap();
        assert_eq!(file.accounts[1].login_state, LoginState::LoggedOut);

        let calls_after_first = fake.calls().len();
        fake.script_usage_ok(&live_access(), 97.0);
        engine.poll(PollReason::Timer, at(1000)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 2, "B must not be polled again");
        assert_eq!(fake.refresh_calls().len(), 1);
        assert_eq!(fake.calls().len(), calls_after_first + 1);
    }

    #[test]
    fn a_failed_secret_save_logs_the_account_out_and_skips_the_usage_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        let secrets = FileSecrets::new(fixture.app.secrets_dir());
        fixture.store = Store::new(
            fixture.app.clone(),
            Box::new(FailingSetSecrets { inner: secrets }),
        );
        let app = fixture.app.clone();
        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_refresh_ok(
            &b_refresh_token(),
            login(
                &b_new_access(),
                &b_new_refresh_token(),
                t0_ms() + 8 * HOUR_MS,
                t0_ms() + 20 * DAY,
                B_ORG,
            ),
        );
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();
        let b = views.iter().find(|view| view.id == B_ID).unwrap();
        assert_eq!(b.login_state, LoginState::LoggedOut);
        let error = b.last_error.as_deref().unwrap();
        assert!(
            error.starts_with("could not store the renewed login: "),
            "got {error}"
        );
        assert!(!error.contains(&b_new_access()));
        assert_eq!(
            fake.usage_tokens(),
            vec![live_access()],
            "no usage call with the token that could not be stored"
        );
        let file = engine.store().load().unwrap();
        assert_eq!(file.accounts[1].login_state, LoginState::LoggedOut);

        // The old login was never replaced.
        let cache = UsageCache::load(&app).unwrap();
        assert!(cache.entries.get(B_ID).unwrap().usage.is_none());
    }

    #[test]
    fn transient_failures_back_off_one_two_four_minutes_and_a_success_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        fixture.file.accounts[1].login_state = LoginState::LoggedOut;
        fixture.save_file();
        let fake = fixture.fake.clone();
        for _ in 0..3 {
            fake.script_usage(&live_access(), Err(ApiError::Server(503)));
        }
        fake.script_usage_ok(&live_access(), 97.0);
        fake.script_usage_ok(&live_access(), 97.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        let entry = |engine: &Engine<FakeRemote>| engine.cache.entries.get(A_ID).unwrap().clone();

        engine.poll(PollReason::Timer, t0()).unwrap();
        assert_eq!(entry(&engine).failures, 1);
        assert_eq!(entry(&engine).backoff_until, Some(at(60)));
        assert_eq!(fake.usage_tokens().len(), 1);

        engine.poll(PollReason::Timer, at(30)).unwrap();
        assert_eq!(
            fake.usage_tokens().len(),
            1,
            "still inside the 1 minute window"
        );

        engine.poll(PollReason::Timer, at(60)).unwrap();
        assert_eq!(entry(&engine).failures, 2);
        assert_eq!(entry(&engine).backoff_until, Some(at(60 + 120)));
        assert_eq!(fake.usage_tokens().len(), 2);

        engine.poll(PollReason::Timer, at(170)).unwrap();
        assert_eq!(
            fake.usage_tokens().len(),
            2,
            "still inside the 2 minute window"
        );

        engine.poll(PollReason::Timer, at(180)).unwrap();
        assert_eq!(entry(&engine).failures, 3);
        assert_eq!(entry(&engine).backoff_until, Some(at(180 + 240)));
        assert_eq!(fake.usage_tokens().len(), 3);

        engine.poll(PollReason::Timer, at(419)).unwrap();
        assert_eq!(
            fake.usage_tokens().len(),
            3,
            "still inside the 4 minute window"
        );

        engine.poll(PollReason::Timer, at(420)).unwrap();
        let entry_after = entry(&engine);
        assert_eq!(entry_after.failures, 0);
        assert_eq!(entry_after.backoff_until, None);
        assert_eq!(entry_after.last_error, None);
        assert_eq!(entry_after.fetched_at, Some(at(420)));
        assert_eq!(fake.usage_tokens().len(), 4);

        // The next regular poll after the reset uses the normal 120 s interval.
        engine.poll(PollReason::Timer, at(540)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 5);
    }

    #[test]
    fn a_non_transient_failure_is_not_retried_until_the_interval_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        // A is not part of this check; B stays inactive and has a usable token.
        fixture.file.accounts[0].login_state = LoginState::LoggedOut;
        fixture.save_file();
        let b_login = login(
            &b_old_access(),
            &b_refresh_token(),
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            B_ORG,
        );
        fixture.store.secrets().set(B_ID, &b_login).unwrap();

        let fake = fixture.fake.clone();
        for _ in 0..3 {
            fake.script_usage(
                &b_old_access(),
                Err(ApiError::Http {
                    status: 403,
                    body: "error code: 1010".to_string(),
                }),
            );
        }
        let (_app, fake, mut engine) = fixture.into_engine();

        engine.poll(PollReason::Timer, t0()).unwrap();
        assert_eq!(fake.usage_tokens(), vec![b_old_access()]);

        // 60 s later the 403 is not retried: the interval runs from the attempt.
        engine.poll(PollReason::Timer, at(60)).unwrap();
        assert_eq!(
            fake.usage_tokens(),
            vec![b_old_access()],
            "a persistent 403 must not be retried on every tick"
        );

        // At the normal inactive interval it is due again.
        engine.poll(PollReason::Timer, at(900)).unwrap();
        assert_eq!(fake.usage_tokens(), vec![b_old_access(), b_old_access()]);
    }

    #[test]
    fn rate_limited_waits_at_least_its_retry_after() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        fixture.file.accounts[1].login_state = LoginState::LoggedOut;
        fixture.save_file();
        let fake = fixture.fake.clone();
        fake.script_usage(
            &live_access(),
            Err(ApiError::RateLimited {
                retry_after_secs: Some(300),
            }),
        );
        let (_app, fake, mut engine) = fixture.into_engine();

        engine.poll(PollReason::Timer, t0()).unwrap();
        let entry = engine.cache.entries.get(A_ID).unwrap();
        assert_eq!(entry.failures, 1);
        assert_eq!(entry.backoff_until, Some(at(300)));

        engine.poll(PollReason::Timer, at(299)).unwrap();
        assert_eq!(fake.usage_tokens().len(), 1);
    }

    #[test]
    fn refresh_inactive_off_keeps_the_cached_usage_and_makes_no_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        fixture.file.settings.refresh_inactive = false;
        fixture.save_file();

        let cached_at = at(-1000);
        let mut cache = UsageCache::default();
        cache.entries.insert(
            B_ID.to_string(),
            CacheEntry {
                usage: Some(usage_with(48.0, None)),
                fetched_at: Some(cached_at),
                ..CacheEntry::default()
            },
        );
        cache.save(&fixture.app).unwrap();

        let fake = fixture.fake.clone();
        fake.script_usage_ok(&live_access(), 97.0);
        let (_app, fake, mut engine) = fixture.into_engine();

        let views = engine.poll(PollReason::Timer, t0()).unwrap();
        let b = views.iter().find(|view| view.id == B_ID).unwrap();
        assert_eq!(b.login_state, LoginState::Ok);
        assert_eq!(b.fetched_at, Some(cached_at));
        assert_eq!(b.usage, Some(usage_with(48.0, None)));
        assert_eq!(fake.usage_tokens(), vec![live_access()]);
        assert!(fake.refresh_calls().is_empty());
    }

    #[test]
    fn active_account_requires_a_known_live_login() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let claude = fixture.claude.clone();
        let (_app, _fake, engine) = fixture.into_engine();
        let a_identity = identity(A_UUID, A_ORG, "Acme", "someone@example.com");

        // The identity still names A, but .credentials.json has no claudeAiOauth.
        fs::write(&claude.credentials_file, br#"{"mcpOAuth":{}}"#).unwrap();
        let views = engine.views(t0()).unwrap();
        assert_eq!(views.len(), 2, "a missing login must not hide the accounts");
        assert!(
            views.iter().all(|view| !view.is_active),
            "no login means no active account"
        );

        // An ended login (empty refresh token) is no live account either.
        let ended = login(
            &live_access(),
            "",
            t0_ms() + 8 * HOUR_MS,
            t0_ms() + 20 * DAY,
            A_ORG,
        );
        write_live(&claude, &ended, &a_identity);
        let views = engine.views(t0()).unwrap();
        assert!(
            views.iter().all(|view| !view.is_active),
            "an ended login means no active account"
        );

        // A broken identity file means no active account, not an error.
        fs::write(&claude.identity_file, b"{ not json").unwrap();
        let views = engine.views(t0()).unwrap();
        assert_eq!(
            views.len(),
            2,
            "a broken identity must not hide the accounts"
        );
        assert!(
            views.iter().all(|view| !view.is_active),
            "a broken identity means no active account"
        );

        // The normal case is unchanged.
        write_live(&claude, &live_login(), &a_identity);
        let views = engine.views(t0()).unwrap();
        assert_eq!(views[0].id, A_ID);
        assert!(views[0].is_active);
    }

    #[test]
    fn no_active_account_when_the_live_credentials_have_no_oauth() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        // The identity still names A, but /logout removed the login keys.
        let credentials = json!({
            "mcpOAuth": {},
            "organizationUuid": A_ORG,
        });
        fs::write(
            &fixture.claude.credentials_file,
            serde_json::to_vec(&credentials).unwrap(),
        )
        .unwrap();

        let (_app, _fake, engine) = fixture.into_engine();
        let views = engine.views(t0()).unwrap();

        assert!(
            views.iter().all(|view| !view.is_active),
            "a missing live login must leave no active account: {views:?}"
        );
    }

    #[test]
    fn no_active_account_when_the_live_login_ended() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        // Claude Code marked the login ended by emptying its refresh token.
        let mut ended = live_login();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), json!(""));
        write_live(
            &fixture.claude,
            &ended,
            &identity(A_UUID, A_ORG, "Acme", "someone@example.com"),
        );

        let (_app, _fake, engine) = fixture.into_engine();
        let views = engine.views(t0()).unwrap();

        assert!(
            views.iter().all(|view| !view.is_active),
            "an ended live login must leave no active account: {views:?}"
        );
    }

    #[test]
    fn views_put_the_active_account_first_and_compute_days_left() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::new(&tmp);
        // The active account (A) is second in the file.
        fixture.file.accounts.swap(0, 1);
        fixture.file.accounts[1].login_ends_at_ms = Some(t0_ms() + 6 * DAY + DAY / 2);
        fixture.file.accounts[0].login_ends_at_ms = Some(t0_ms() - 1);
        fixture.save_file();

        let (_app, _fake, engine) = fixture.into_engine();
        let views = engine.views(t0()).unwrap();

        assert_eq!(views[0].id, A_ID);
        assert!(views[0].is_active);
        assert_eq!(views[0].login_days_left, Some(6));
        assert_eq!(views[1].id, B_ID);
        assert!(!views[1].is_active);
        assert_eq!(views[1].login_days_left, Some(-1));
    }

    #[test]
    fn suggestion_picks_the_lowest_candidate_below_the_target() {
        let settings = Settings::default();
        let resets_at = Some(at(3600));
        let mut active = view(A_ID, true, LoginState::Ok, Some(97.0), Some(10));
        active.usage = Some(usage_with(97.0, resets_at));

        let views = vec![
            active.clone(),
            view(B_ID, false, LoginState::Ok, Some(30.0), Some(10)),
            view("cccc", false, LoginState::Ok, Some(12.0), Some(10)),
            view("dddd", false, LoginState::LoggedOut, Some(5.0), Some(10)),
            view("eeee", false, LoginState::Ok, Some(60.0), Some(10)),
        ];
        let picked = suggestion(&views, &settings).unwrap();
        assert_eq!(picked.from_id, A_ID);
        assert_eq!(picked.to_id, "cccc");
        assert_eq!(picked.from_percent, 97.0);
        assert_eq!(picked.to_percent, 12.0);
        assert_eq!(picked.from_resets_at, resets_at);

        let mut below = views.clone();
        below[0].usage = Some(usage_with(89.0, None));
        assert!(suggestion(&below, &settings).is_none());

        let none_below = vec![
            active,
            view(B_ID, false, LoginState::Ok, Some(50.0), Some(10)),
            view("cccc", false, LoginState::Ok, Some(80.0), Some(10)),
        ];
        assert!(suggestion(&none_below, &settings).is_none());

        let unknown_active = vec![
            view(A_ID, true, LoginState::Ok, None, Some(10)),
            view(B_ID, false, LoginState::Ok, Some(12.0), Some(10)),
        ];
        assert!(suggestion(&unknown_active, &settings).is_none());
    }

    #[test]
    fn login_warnings_boundaries() {
        let views = vec![
            view("six", false, LoginState::Ok, None, Some(6)),
            view("five", false, LoginState::Ok, None, Some(5)),
            view("four", false, LoginState::Ok, None, Some(4)),
            view("three", false, LoginState::Ok, None, Some(3)),
            view("two", false, LoginState::Ok, None, Some(2)),
            view("one", false, LoginState::Ok, None, Some(1)),
            view("zero", false, LoginState::Ok, None, Some(0)),
            view("minus", false, LoginState::Ok, None, Some(-1)),
            view("unknown", false, LoginState::Ok, None, None),
        ];
        assert_eq!(
            login_warnings(&views),
            vec![
                LoginWarning::Soon {
                    id: "five".to_string(),
                    days_left: 5
                },
                LoginWarning::Soon {
                    id: "four".to_string(),
                    days_left: 4
                },
                LoginWarning::Soon {
                    id: "three".to_string(),
                    days_left: 3
                },
                LoginWarning::Soon {
                    id: "two".to_string(),
                    days_left: 2
                },
                LoginWarning::Tomorrow {
                    id: "one".to_string()
                },
                LoginWarning::Tomorrow {
                    id: "zero".to_string()
                },
                LoginWarning::Ended {
                    id: "minus".to_string()
                },
            ]
        );
    }

    #[test]
    fn suggestion_and_login_warning_json_shapes() {
        let suggestion = Suggestion {
            from_id: "a".to_string(),
            to_id: "b".to_string(),
            from_percent: 97.0,
            to_percent: 12.0,
            from_resets_at: Some(at(3600)),
        };
        assert_eq!(
            serde_json::to_value(&suggestion).unwrap(),
            json!({
                "fromId": "a",
                "toId": "b",
                "fromPercent": 97.0,
                "toPercent": 12.0,
                "fromResetsAt": "2026-11-10T13:00:00Z"
            })
        );
        assert_eq!(
            serde_json::to_value(LoginWarning::Soon {
                id: "a".to_string(),
                days_left: 3
            })
            .unwrap(),
            json!({"kind": "soon", "id": "a", "daysLeft": 3})
        );
        assert_eq!(
            serde_json::to_value(LoginWarning::Tomorrow {
                id: "a".to_string()
            })
            .unwrap(),
            json!({"kind": "tomorrow", "id": "a"})
        );
        assert_eq!(
            serde_json::to_value(LoginWarning::Ended {
                id: "a".to_string()
            })
            .unwrap(),
            json!({"kind": "ended", "id": "a"})
        );
    }

    #[test]
    fn an_old_cache_entry_without_attempted_at_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        fs::create_dir_all(&app.data_dir).unwrap();
        fs::write(
            app.usage_cache_file(),
            br#"{"version":1,"entries":{"a":{"usage":null,"fetchedAt":null,"lastError":null,"failures":0,"backoffUntil":null}}}"#,
        )
        .unwrap();

        let cache = UsageCache::load(&app).unwrap();
        assert_eq!(
            cache.entries.get("a").and_then(|entry| entry.attempted_at),
            None
        );
    }

    #[test]
    fn set_aside_renames_a_corrupt_cache_and_leaves_an_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        fs::create_dir_all(&app.data_dir).unwrap();
        let path = app.usage_cache_file();
        fs::write(&path, b"{ not json").unwrap();

        let aside = UsageCache::set_aside(&app).unwrap();

        assert!(!path.exists(), "the corrupt cache must be moved away");
        assert!(
            aside.exists(),
            "the corrupt cache must be kept for the person"
        );
        assert_eq!(aside.parent(), path.parent());
        let name = aside.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("usage-cache.json.corrupt-"), "got {name}");
        assert_eq!(fs::read(&aside).unwrap(), b"{ not json");
        assert_eq!(UsageCache::load(&app).unwrap(), UsageCache::default());
    }

    #[test]
    fn a_corrupt_cache_file_is_reported_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(&tmp);
        let app = fixture.app.clone();
        fs::create_dir_all(&app.data_dir).unwrap();
        let path = app.usage_cache_file();
        fs::write(&path, b"{ not json").unwrap();

        match Engine::new(fixture.claude, fixture.store, fixture.fake) {
            Err(EngineError::CacheCorrupt { path: reported }) => assert_eq!(reported, path),
            Err(error) => panic!("expected CacheCorrupt, got {error}"),
            Ok(_) => panic!("a corrupt cache must not be loaded"),
        }
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
    }
}
