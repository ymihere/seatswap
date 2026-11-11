//! The switch (spec §6.2), saving the live login (spec §6.1, §6.4) and removing
//! or editing accounts.
//!
//! Every write of Claude Code's files happens while holding seatswap's own
//! lock first and then Claude Code's four lock folders, in that order; dropping
//! the guards releases them in reverse. The live files are read again under those
//! locks, never from a value read before them.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::live::{self, LiveError};
use crate::locks::{self, LockError};
use crate::paths::ClaudePaths;
use crate::store::{self, Account, AccountsFile, LoginState, Store, StoreError};
use crate::types::{Login, OAuthAccount};

/// How long to wait for Claude Code's four lock folders (spec §6.2).
pub const CLAUDE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for seatswap's own lock (spec §6.2).
pub const APP_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum SwitchError {
    Lock(LockError),
    Live(LiveError),
    Store(StoreError),
    /// The key given by the caller matches neither an id nor an abbreviation.
    UnknownAccount(String),
    /// The stored login is marked as ended; show the re-login steps.
    TargetLoggedOut {
        label: String,
    },
    /// No secret is stored for the target.
    TargetHasNoLogin {
        label: String,
    },
    /// The live login belongs to no stored account and the policy is `Refuse`.
    CurrentNotSaved {
        email: Option<String>,
    },
    /// The login file was written but the identity was not.
    PartlyDone {
        state: String,
    },
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwitchError::Lock(LockError::Busy { path }) => {
                if path.file_name().is_some_and(|name| name == "switch.lock") {
                    write!(f, "another seatswap is running")
                } else {
                    write!(f, "Claude Code is busy with its login, try again")
                }
            }
            SwitchError::Lock(error) => write!(f, "{error}"),
            SwitchError::Live(error) => write!(f, "{error}"),
            SwitchError::Store(error) => write!(f, "{error}"),
            SwitchError::UnknownAccount(key) => write!(f, "unknown account {key}"),
            SwitchError::TargetLoggedOut { label } => {
                write!(
                    f,
                    "the stored login for {label} has ended; log in with Claude Code again"
                )
            }
            SwitchError::TargetHasNoLogin { label } => {
                write!(f, "no login is stored for {label}; save it again")
            }
            SwitchError::CurrentNotSaved { email } => match email {
                Some(email) => write!(f, "the current login ({email}) is not saved"),
                None => write!(f, "the current login is not saved"),
            },
            SwitchError::PartlyDone { state } => write!(f, "{state}"),
        }
    }
}

impl std::error::Error for SwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwitchError::Lock(error) => Some(error),
            SwitchError::Live(error) => Some(error),
            SwitchError::Store(error) => Some(error),
            SwitchError::UnknownAccount(_)
            | SwitchError::TargetLoggedOut { .. }
            | SwitchError::TargetHasNoLogin { .. }
            | SwitchError::CurrentNotSaved { .. }
            | SwitchError::PartlyDone { .. } => None,
        }
    }
}

impl From<LockError> for SwitchError {
    fn from(error: LockError) -> Self {
        SwitchError::Lock(error)
    }
}

impl From<LiveError> for SwitchError {
    fn from(error: LiveError) -> Self {
        SwitchError::Live(error)
    }
}

impl From<StoreError> for SwitchError {
    fn from(error: StoreError) -> Self {
        SwitchError::Store(error)
    }
}

/// What to do when the live login belongs to no stored account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsavedPolicy {
    /// Release every lock and return [`SwitchError::CurrentNotSaved`]; the caller
    /// asks the person outside the locks and calls again with `Save` or `Discard`.
    Refuse,
    /// Switch anyway and store nothing.
    Discard,
    /// Store the live login as a new account, then switch.
    Save { label: Option<String> },
}

/// The result of a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// Account id that was live, if it matched a stored account before the switch.
    pub from: Option<String>,
    /// Target account id.
    pub to: String,
    /// Id of an account created by [`UnsavedPolicy::Save`].
    pub saved_new: Option<String>,
    /// Target was already live: nothing was written to Claude's files.
    pub already_active: bool,
}

/// What the live login is, compared with the store. Read-only; takes no lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveState {
    /// No file, or no `claudeAiOauth` in it.
    NoLogin,
    /// `is_ended()`; the id is `Some` when the identity still matches a stored account.
    Ended { account_id: Option<String> },
    /// The live identity matches a stored account.
    Known { account_id: String },
    /// A live login whose identity matches no stored account.
    Unknown {
        email: Option<String>,
        org_name: Option<String>,
    },
}

/// §6.2. The whole switch happens under seatswap's lock and Claude Code's
/// four lock folders, taken in that order.
pub fn switch_to(
    claude: &ClaudePaths,
    store: &Store,
    target_key: &str,
    unsaved: UnsavedPolicy,
    now: DateTime<Utc>,
) -> Result<SwitchOutcome, SwitchError> {
    switch_to_with_timeouts(
        claude,
        store,
        target_key,
        unsaved,
        now,
        Timeouts {
            claude: CLAUDE_LOCK_TIMEOUT,
            app: APP_LOCK_TIMEOUT,
        },
    )
}

/// The two lock timeouts. Tests pass shorter ones; [`switch_to`] always uses the
/// contract constants.
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    claude: Duration,
    app: Duration,
}

fn switch_to_with_timeouts(
    claude: &ClaudePaths,
    store: &Store,
    target_key: &str,
    unsaved: UnsavedPolicy,
    now: DateTime<Utc>,
    timeouts: Timeouts,
) -> Result<SwitchOutcome, SwitchError> {
    // Declaration order is the lock order; Rust drops them in reverse.
    let _app_lock = locks::acquire_app_lock(store.app_paths(), timeouts.app)?;
    let _claude_locks = locks::acquire_claude_locks(claude, timeouts.claude)?;

    // Never trust anything read before taking the locks.
    let mut file = store.load()?;

    let live = live::read_login(claude)?;
    let identity = live::read_identity(claude)?;
    let current_id = identity.as_ref().and_then(OAuthAccount::identity).and_then(
        |(account_uuid, organization_uuid)| {
            file.find_by_identity(account_uuid, organization_uuid)
                .map(|account| account.id.clone())
        },
    );

    // An unknown live login is asked about outside the locks. Refuse writes
    // nothing at all, so it returns before even looking at the target's secret.
    // An ended login is never saved, so it is handled like `Discard`.
    if current_id.is_none() && live.as_ref().is_some_and(|login| !login.is_ended()) {
        match &unsaved {
            UnsavedPolicy::Refuse => {
                return Err(SwitchError::CurrentNotSaved {
                    email: identity
                        .as_ref()
                        .and_then(|identity| identity.email())
                        .map(str::to_string),
                });
            }
            UnsavedPolicy::Discard | UnsavedPolicy::Save { .. } => {}
        }
    }

    let mut saved_new = None;
    let target_id = match file.find_by_key(target_key) {
        Some(account) => account.id.clone(),
        None => return Err(SwitchError::UnknownAccount(target_key.to_string())),
    };
    let target_label = file
        .find(&target_id)
        .map(|account| account.label.clone())
        .ok_or_else(|| SwitchError::UnknownAccount(target_id.clone()))?;

    // Target == current: a usable live login needs no writes to Claude's files.
    // No live login: fall through and write the stored one. An ended live login:
    // mark the account logged out and ask the caller for a fresh login.
    if current_id.as_deref() == Some(target_id.as_str()) {
        match &live {
            Some(login) if !login.is_ended() => {
                save_current_login(
                    store,
                    &mut file,
                    &target_id,
                    live.as_ref(),
                    identity.as_ref(),
                )?;
                store.save(&file)?;
                return Ok(SwitchOutcome {
                    from: current_id,
                    to: target_id,
                    saved_new,
                    already_active: true,
                });
            }
            Some(_) => {
                save_current_login(
                    store,
                    &mut file,
                    &target_id,
                    live.as_ref(),
                    identity.as_ref(),
                )?;
                store.save(&file)?;
                return Err(SwitchError::TargetLoggedOut {
                    label: target_label,
                });
            }
            None => {}
        }
    }

    // Rule: before writing anything, the target must be usable. Clone what the
    // rest of the function needs so `file` can be mutated afterwards.
    let target_account = file
        .find(&target_id)
        .ok_or_else(|| SwitchError::UnknownAccount(target_id.clone()))?;
    let target_identity = target_account.oauth_account.clone();
    let target_logged_out = target_account.login_state == LoginState::LoggedOut;
    let target_login = if target_logged_out {
        None
    } else {
        store.secrets().get(&target_id)?
    };
    if target_logged_out {
        return Err(SwitchError::TargetLoggedOut {
            label: target_label,
        });
    }
    let target_login = target_login.ok_or_else(|| SwitchError::TargetHasNoLogin {
        label: target_label.clone(),
    })?;
    if target_login.is_ended() {
        return Err(SwitchError::TargetLoggedOut {
            label: target_label,
        });
    }

    // Save the live login into the current account's slot only now that we know
    // its identity matches that account.
    let from_id = current_id.clone();
    let mut live_label = current_id
        .as_ref()
        .and_then(|id| file.find(id).map(|account| account.label.clone()));
    if let Some(account_id) = &current_id {
        save_current_login(
            store,
            &mut file,
            account_id,
            live.as_ref(),
            identity.as_ref(),
        )?;
    } else if let (Some(login), UnsavedPolicy::Save { label }) =
        (live.as_ref().filter(|login| !login.is_ended()), &unsaved)
    {
        let identity = identity
            .as_ref()
            .filter(|identity| identity.identity().is_some())
            .ok_or_else(|| StoreError::MissingIdentity {
                path: claude.identity_file.clone(),
            })?;
        let account = store::new_account(login, identity, label.as_deref(), &file, now)?;
        store.secrets().set(&account.id, login)?;
        live_label = Some(account.label.clone());
        saved_new = Some(account.id.clone());
        file.accounts.push(account);
    }
    if live_label.is_none() {
        live_label = identity
            .as_ref()
            .and_then(|identity| identity.email())
            .map(|email| format!("the unsaved login ({email})"));
    }

    live::write_login(claude, Some(&target_login))?;
    if let Err(error) = live::write_identity(claude, &target_identity) {
        let live_label = live_label.unwrap_or_else(|| "the unsaved login".to_string());
        return match live::write_login(claude, live.as_ref()) {
            Ok(()) => {
                // The new account was never saved in accounts.json, so its
                // secret must not stay behind. A failed delete is reported.
                if let Some(new_id) = saved_new.as_deref() {
                    if let Err(delete_error) = store.secrets().delete(new_id) {
                        return Err(SwitchError::PartlyDone {
                            state: format!(
                                "login switched to {target_label}, identity not: {error}; \
                                 rolled back to {live_label}, but the new account's login \
                                 could not be removed: {delete_error}"
                            ),
                        });
                    }
                }
                Err(SwitchError::Live(error))
            }
            Err(rollback) => Err(SwitchError::PartlyDone {
                state: format!(
                    "login switched to {target_label}, identity not: {error}; \
                     rolling back to {live_label} failed: {rollback}"
                ),
            }),
        };
    }

    let target_index = file
        .accounts
        .iter()
        .position(|account| account.id == target_id)
        .ok_or_else(|| SwitchError::UnknownAccount(target_id.clone()))?;
    file.accounts[target_index].last_used_at = Some(now);
    store.save(&file)?;

    Ok(SwitchOutcome {
        from: from_id,
        to: target_id,
        saved_new,
        already_active: false,
    })
}

/// Save a fresh live login into an account's slot and update the account's copy
/// of the identity. An ended login is never stored over the old one; the account
/// is marked logged out instead.
fn save_current_login(
    store: &Store,
    file: &mut AccountsFile,
    account_id: &str,
    live: Option<&Login>,
    identity: Option<&OAuthAccount>,
) -> Result<(), SwitchError> {
    let Some(login) = live else {
        return Ok(());
    };
    let Some(index) = file
        .accounts
        .iter()
        .position(|account| account.id == account_id)
    else {
        return Ok(());
    };
    if login.is_ended() {
        file.accounts[index].login_state = LoginState::LoggedOut;
        return Ok(());
    }
    store.secrets().set(account_id, login)?;
    let account = &mut file.accounts[index];
    if let Some(identity) = identity {
        account.oauth_account = identity.clone();
    }
    account.note_stored_login(login);
    Ok(())
}

/// Read the live login and compare it with the store. No lock is taken.
pub fn live_state(claude: &ClaudePaths, file: &AccountsFile) -> Result<LiveState, SwitchError> {
    let Some(login) = live::read_login(claude)? else {
        return Ok(LiveState::NoLogin);
    };
    let identity = live::read_identity(claude)?;
    let account_id = identity.as_ref().and_then(OAuthAccount::identity).and_then(
        |(account_uuid, organization_uuid)| {
            file.find_by_identity(account_uuid, organization_uuid)
                .map(|account| account.id.clone())
        },
    );
    if login.is_ended() {
        return Ok(LiveState::Ended { account_id });
    }
    match account_id {
        Some(account_id) => Ok(LiveState::Known { account_id }),
        None => Ok(LiveState::Unknown {
            email: identity
                .as_ref()
                .and_then(|identity| identity.email())
                .map(str::to_string),
            org_name: identity
                .as_ref()
                .and_then(|identity| identity.organization_name())
                .map(str::to_string),
        }),
    }
}

/// "Save as account" (§6.1) and the watcher's mirror (§6.4 mirrorLive). Under the
/// app lock, and only when Claude Code's locks are free.
pub fn sync_live(
    claude: &ClaudePaths,
    store: &Store,
    create_unknown: Option<Option<String>>,
    now: DateTime<Utc>,
) -> Result<(LiveState, bool), SwitchError> {
    let _app_lock = locks::acquire_app_lock(store.app_paths(), APP_LOCK_TIMEOUT)?;
    // Claude Code is mid-renewal: do not read the files at all, the caller retries.
    if locks::claude_locks_busy(claude) {
        return Err(SwitchError::Lock(LockError::Busy {
            path: claude.storage_write_lock(),
        }));
    }

    let mut file = store.load()?;
    let live = live::read_login(claude)?;
    let identity = live::read_identity(claude)?;
    let account_id = identity.as_ref().and_then(OAuthAccount::identity).and_then(
        |(account_uuid, organization_uuid)| {
            file.find_by_identity(account_uuid, organization_uuid)
                .map(|account| account.id.clone())
        },
    );

    let Some(login) = live else {
        return Ok((LiveState::NoLogin, false));
    };

    // An ended login is never copied over a stored one. The account it still
    // names is marked logged out instead.
    if login.is_ended() {
        let Some(account_id) = account_id else {
            return Ok((LiveState::Ended { account_id: None }, false));
        };
        let index = file
            .accounts
            .iter()
            .position(|account| account.id == account_id)
            .ok_or_else(|| SwitchError::UnknownAccount(account_id.clone()))?;
        if file.accounts[index].login_state == LoginState::LoggedOut {
            return Ok((
                LiveState::Ended {
                    account_id: Some(account_id),
                },
                false,
            ));
        }
        file.accounts[index].login_state = LoginState::LoggedOut;
        store.save(&file)?;
        return Ok((
            LiveState::Ended {
                account_id: Some(account_id),
            },
            true,
        ));
    }

    if let Some(account_id) = account_id {
        let mut written = false;
        if store.secrets().get(&account_id)?.as_ref() != Some(&login) {
            store.secrets().set(&account_id, &login)?;
            written = true;
        }
        let index = file
            .accounts
            .iter()
            .position(|account| account.id == account_id)
            .ok_or_else(|| SwitchError::UnknownAccount(account_id.clone()))?;
        let account = &mut file.accounts[index];
        if let Some(identity) = &identity {
            if &account.oauth_account != identity {
                account.oauth_account = identity.clone();
                written = true;
            }
        }
        if account.note_stored_login(&login) {
            written = true;
        }
        if written {
            store.save(&file)?;
        }
        return Ok((LiveState::Known { account_id }, written));
    }

    let email = identity
        .as_ref()
        .and_then(|identity| identity.email())
        .map(str::to_string);
    let org_name = identity
        .as_ref()
        .and_then(|identity| identity.organization_name())
        .map(str::to_string);
    match create_unknown {
        None => Ok((LiveState::Unknown { email, org_name }, false)),
        Some(label) => {
            let identity = identity
                .as_ref()
                .filter(|identity| identity.identity().is_some())
                .ok_or_else(|| StoreError::MissingIdentity {
                    path: claude.identity_file.clone(),
                })?;
            let account = store::new_account(&login, identity, label.as_deref(), &file, now)?;
            store.secrets().set(&account.id, &login)?;
            let account_id = account.id.clone();
            file.accounts.push(account);
            store.save(&file)?;
            Ok((LiveState::Known { account_id }, true))
        }
    }
}

/// Remove an account from the list and its login from the secrets. Claude Code's
/// files are never touched, even when the removed account is the live one.
pub fn remove_account(store: &Store, key: &str) -> Result<Account, SwitchError> {
    let _app_lock = locks::acquire_app_lock(store.app_paths(), APP_LOCK_TIMEOUT)?;
    let mut file = store.load()?;
    let index = file
        .find_by_key(key)
        .map(|account| account.id.clone())
        .and_then(|id| file.accounts.iter().position(|account| account.id == id))
        .ok_or_else(|| SwitchError::UnknownAccount(key.to_string()))?;
    let removed = file.accounts.remove(index);
    store.save(&file)?;
    store.secrets().delete(&removed.id)?;
    Ok(removed)
}

/// Change label, abbreviation or colour. Every value goes through the store's
/// validator first, so an invalid value leaves the account list untouched.
pub fn edit_account(
    store: &Store,
    key: &str,
    label: Option<&str>,
    abbr: Option<&str>,
    color: Option<&str>,
) -> Result<Account, SwitchError> {
    let _app_lock = locks::acquire_app_lock(store.app_paths(), APP_LOCK_TIMEOUT)?;
    let mut file = store.load()?;
    let index = file
        .find_by_key(key)
        .map(|account| account.id.clone())
        .and_then(|id| file.accounts.iter().position(|account| account.id == id))
        .ok_or_else(|| SwitchError::UnknownAccount(key.to_string()))?;

    let label = label.map(store::validate_label).transpose()?;
    let abbr = match abbr {
        Some(raw) => {
            let others: Vec<&str> = file
                .accounts
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, account)| account.abbr.as_str())
                .collect();
            Some(store::validate_abbr(raw, &others)?)
        }
        None => None,
    };
    let color = color.map(store::validate_color).transpose()?;

    let mut changed = false;
    let account = &mut file.accounts[index];
    if let Some(label) = label {
        if account.label != label {
            account.label = label;
            changed = true;
        }
    }
    if let Some(abbr) = abbr {
        if account.abbr != abbr {
            account.abbr = abbr;
            changed = true;
        }
    }
    if let Some(color) = color {
        if account.color != color {
            account.color = color;
            changed = true;
        }
    }
    if changed {
        store.save(&file)?;
    }
    Ok(file.accounts[index].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;

    use serde_json::{json, Map, Value};

    use crate::live::{read_identity, read_login};
    use crate::store::{new_account, FileSecrets};

    const A_ID: &str = "6f1c2a9be0d34c118f2d4e5a6b7c8d9e";
    const B_ID: &str = "a92d4c1e8b3f4a5d9e6c7b8a1d2e3f40";
    const A_ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const A_ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const B_ACCOUNT_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const B_ORG_UUID: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";
    const C_ACCOUNT_UUID: &str = "2b7e9c4d-1a3f-4c5e-8b6d-7f0a1c2e3d4b";
    const C_ORG_UUID: &str = "9a1c3e5f-7b9d-4f2a-8c6e-0d1b3a5c7e9f";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn token(prefix: &str, fill: char) -> String {
        format!("{prefix}{}", fill.to_string().repeat(95))
    }

    fn mcp_oauth() -> Value {
        json!({
            "plugin:engineering:linear|5f1c2a9be0d34c11": {
                "serverName": "plugin:engineering:linear",
                "accessToken": format!("lin_oauth_{}", "9".repeat(64)),
                "expiresAt": 1794400000000_i64
            }
        })
    }

    fn login(access: char, refresh: char, org_uuid: &str) -> Login {
        let credentials = json!({
            "mcpOAuth": mcp_oauth(),
            "claudeAiOauth": {
                "accessToken": token("sk-ant-oat01-", access),
                "refreshToken": token("sk-ant-ort01-", refresh),
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
            "organizationUuid": org_uuid,
            "trustedDeviceToken": token("trusted-", 't'),
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

    fn credentials_bytes(login: &Login) -> Vec<u8> {
        let mut map = Map::new();
        map.insert("mcpOAuth".to_string(), mcp_oauth());
        for (key, value) in &login.0 {
            map.insert(key.clone(), value.clone());
        }
        map.insert("futureKey".to_string(), json!({"a": 1}));
        serde_json::to_vec(&Value::Object(map)).unwrap()
    }

    fn identity_bytes(identity: &OAuthAccount) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "numStartups": 41,
            "oauthAccount": Value::Object(identity.0.clone()),
            "projects": { "/home/u/x": { "allowedTools": [] } }
        }))
        .unwrap()
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    struct World {
        tmp: tempfile::TempDir,
        paths: ClaudePaths,
        app: crate::paths::AppPaths,
        store: Store,
        a_id: String,
        b_id: String,
        a_identity: OAuthAccount,
        b_identity: OAuthAccount,
        c_identity: OAuthAccount,
        a_stored_login: Login,
        a_live_login: Login,
        b_login: Login,
        c_login: Login,
    }

    impl World {
        fn new() -> World {
            let tmp = tempfile::tempdir().unwrap();
            let config = tmp.path().join("claude");
            fs::create_dir(&config).unwrap();
            let paths = ClaudePaths::for_config_dir(&config, tmp.path());

            let a_identity = identity(A_ACCOUNT_UUID, A_ORG_UUID, "Acme", "one@example.com");
            let b_identity = identity(B_ACCOUNT_UUID, B_ORG_UUID, "Private", "two@example.com");
            let c_identity = identity(
                C_ACCOUNT_UUID,
                C_ORG_UUID,
                "Other GmbH",
                "current@example.com",
            );
            let a_stored_login = login('a', 'b', A_ORG_UUID);
            let a_live_login = login('r', 's', A_ORG_UUID);
            let b_login = login('c', 'd', B_ORG_UUID);
            let c_login = login('e', 'f', C_ORG_UUID);

            write_private(&paths.credentials_file, &credentials_bytes(&a_live_login));
            write_private(&paths.identity_file, &identity_bytes(&a_identity));

            let app = crate::paths::AppPaths::at(&tmp.path().join("data"));
            let store = Store::new(app.clone(), Box::new(FileSecrets::new(app.secrets_dir())));

            let mut file = AccountsFile::default();
            let mut a =
                new_account(&a_stored_login, &a_identity, Some("Acme"), &file, now()).unwrap();
            a.id = A_ID.to_string();
            store.secrets().set(&a.id, &a_stored_login).unwrap();
            file.accounts.push(a);
            let mut b = new_account(&b_login, &b_identity, Some("Private"), &file, now()).unwrap();
            b.id = B_ID.to_string();
            store.secrets().set(&b.id, &b_login).unwrap();
            file.accounts.push(b);
            store.save(&file).unwrap();

            World {
                tmp,
                paths,
                app,
                store,
                a_id: A_ID.to_string(),
                b_id: B_ID.to_string(),
                a_identity,
                b_identity,
                c_identity,
                a_stored_login,
                a_live_login,
                b_login,
                c_login,
            }
        }

        fn set_live(&self, login: &Login, identity: &OAuthAccount) {
            write_private(&self.paths.credentials_file, &credentials_bytes(login));
            write_private(&self.paths.identity_file, &identity_bytes(identity));
        }

        fn accounts_bytes(&self) -> Vec<u8> {
            fs::read(self.app.accounts_file()).unwrap()
        }

        fn account_file(&self) -> AccountsFile {
            self.store.load().unwrap()
        }

        fn secrets_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            let mut snapshot = BTreeMap::new();
            for entry in fs::read_dir(self.app.secrets_dir()).unwrap() {
                let path = entry.unwrap().path();
                snapshot.insert(path.clone(), fs::read(&path).unwrap());
            }
            snapshot
        }

        fn claude_files(&self) -> (Vec<u8>, Vec<u8>) {
            (
                fs::read(&self.paths.credentials_file).unwrap(),
                fs::read(&self.paths.identity_file).unwrap(),
            )
        }

        fn assert_no_claude_locks(&self) {
            for path in [
                self.paths.refresh_lock(),
                self.paths.legacy_refresh_lock(),
                self.paths.storage_write_lock(),
                self.paths.identity_lock(),
            ] {
                assert!(!path.exists(), "{} was left behind", path.display());
            }
        }
    }

    struct FailingDeleteSecrets {
        inner: FileSecrets,
    }

    impl crate::store::SecretStore for FailingDeleteSecrets {
        fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
            self.inner.get(id)
        }

        fn set(&self, id: &str, login: &Login) -> Result<(), StoreError> {
            self.inner.set(id, login)
        }

        fn delete(&self, _id: &str) -> Result<(), StoreError> {
            Err(StoreError::Keyring(
                "the test secret store refuses to delete".to_string(),
            ))
        }

        fn encrypted(&self) -> bool {
            false
        }
    }

    #[test]
    fn switch_saves_the_live_login_and_writes_the_target() {
        let w = World::new();
        let before: Value =
            serde_json::from_slice(&fs::read(&w.paths.credentials_file).unwrap()).unwrap();
        let later = now() + chrono::Duration::minutes(5);

        let outcome = switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, later).unwrap();
        assert_eq!(outcome.from.as_deref(), Some(w.a_id.as_str()));
        assert_eq!(outcome.to, w.b_id);
        assert_eq!(outcome.saved_new, None);
        assert!(!outcome.already_active);

        // The live login is the target's, whole.
        assert_eq!(read_login(&w.paths).unwrap(), Some(w.b_login.clone()));
        let after: Value =
            serde_json::from_slice(&fs::read(&w.paths.credentials_file).unwrap()).unwrap();
        assert_eq!(after["mcpOAuth"], before["mcpOAuth"]);
        assert_eq!(after["futureKey"], json!({"a": 1}));
        assert_eq!(
            after["organizationUuid"],
            Value::String(B_ORG_UUID.to_string())
        );

        // The identity is the target's, with the rest of .claude.json kept.
        assert_eq!(read_identity(&w.paths).unwrap(), Some(w.b_identity.clone()));
        let identity: Value =
            serde_json::from_slice(&fs::read(&w.paths.identity_file).unwrap()).unwrap();
        assert_eq!(identity["numStartups"], json!(41));
        assert_eq!(
            identity["projects"],
            json!({"/home/u/x": {"allowedTools": []}})
        );

        // The fresh live login from before the switch landed in A's slot.
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_live_login.clone())
        );
        let file = w.account_file();
        assert_eq!(
            file.find(&w.b_id).unwrap().last_used_at,
            Some(later),
            "the target's last_used_at must be set"
        );
        assert_eq!(file.find(&w.b_id).unwrap().login_state, LoginState::Ok);

        w.assert_no_claude_locks();
        assert!(backup_path(&w.paths.credentials_file).exists());
        assert!(backup_path(&w.paths.identity_file).exists());
    }

    #[test]
    fn switching_to_the_live_account_writes_no_claude_file() {
        let w = World::new();
        let before = w.claude_files();
        let before_accounts = w.accounts_bytes();

        let outcome = switch_to(&w.paths, &w.store, &w.a_id, UnsavedPolicy::Refuse, now()).unwrap();

        assert!(outcome.already_active);
        assert_eq!(outcome.from.as_deref(), Some(w.a_id.as_str()));
        assert_eq!(outcome.to, w.a_id);
        assert_eq!(w.claude_files(), before);
        assert_eq!(
            w.accounts_bytes(),
            before_accounts,
            "an already-live switch changes no stored field"
        );
        // It does refresh the stored copy, which the live file may have renewed.
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_live_login.clone())
        );
        w.assert_no_claude_locks();
    }

    #[test]
    fn unknown_live_login_refuse_writes_nothing_and_frees_the_app_lock() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);
        let before_claude = w.claude_files();
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();

        let error =
            switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now()).unwrap_err();

        match error {
            SwitchError::CurrentNotSaved { email } => {
                assert_eq!(email.as_deref(), Some("current@example.com"))
            }
            other => panic!("expected CurrentNotSaved, got {other:?}"),
        }
        assert_eq!(w.claude_files(), before_claude);
        assert_eq!(w.accounts_bytes(), before_accounts);
        assert_eq!(w.secrets_snapshot(), before_secrets);
        w.assert_no_claude_locks();
        // The app lock is free again.
        let lock = crate::locks::acquire_app_lock(&w.app, Duration::from_millis(200)).unwrap();
        drop(lock);
    }

    #[test]
    fn unknown_live_login_save_creates_the_account_then_switches() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);

        let outcome = switch_to(
            &w.paths,
            &w.store,
            &w.b_id,
            UnsavedPolicy::Save {
                label: Some("Work".to_string()),
            },
            now(),
        )
        .unwrap();

        let saved_id = outcome.saved_new.expect("an account was created");
        let file = w.account_file();
        assert_eq!(file.accounts.len(), 3);
        let saved = file.find(&saved_id).unwrap();
        assert_eq!(saved.label, "Work");
        assert_eq!(saved.account_uuid, C_ACCOUNT_UUID);
        assert_eq!(saved.organization_uuid, C_ORG_UUID);
        assert_eq!(
            w.store.secrets().get(&saved_id).unwrap(),
            Some(w.c_login.clone())
        );
        assert_eq!(read_login(&w.paths).unwrap(), Some(w.b_login.clone()));
        assert_eq!(read_identity(&w.paths).unwrap(), Some(w.b_identity.clone()));
    }

    #[test]
    fn unknown_live_login_discard_switches_and_stores_nothing() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);

        let outcome =
            switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Discard, now()).unwrap();

        assert_eq!(outcome.saved_new, None);
        assert_eq!(w.account_file().accounts.len(), 2);
        assert!(w
            .account_file()
            .find_by_identity(C_ACCOUNT_UUID, C_ORG_UUID)
            .is_none());
        assert_eq!(w.secrets_snapshot().len(), 2);
        assert_eq!(read_login(&w.paths).unwrap(), Some(w.b_login.clone()));
    }

    #[test]
    fn an_ended_live_login_marks_the_current_account_logged_out() {
        let w = World::new();
        let mut ended = w.a_live_login.clone();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), Value::String(String::new()));
        w.set_live(&ended, &w.a_identity);

        let outcome = switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now()).unwrap();

        assert!(!outcome.already_active);
        let file = w.account_file();
        assert_eq!(
            file.find(&w.a_id).unwrap().login_state,
            LoginState::LoggedOut
        );
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_stored_login.clone()),
            "an ended login must never replace the stored one"
        );
        assert_eq!(read_login(&w.paths).unwrap(), Some(w.b_login.clone()));
    }

    #[test]
    fn a_logged_out_target_is_rejected_before_anything_is_written() {
        let w = World::new();
        let mut file = w.account_file();
        file.accounts
            .iter_mut()
            .find(|account| account.id == w.b_id)
            .unwrap()
            .login_state = LoginState::LoggedOut;
        w.store.save(&file).unwrap();
        let before_claude = w.claude_files();
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();

        let error =
            switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now()).unwrap_err();

        match error {
            SwitchError::TargetLoggedOut { label } => assert_eq!(label, "Private"),
            other => panic!("expected TargetLoggedOut, got {other:?}"),
        }
        assert_eq!(w.claude_files(), before_claude);
        assert_eq!(w.accounts_bytes(), before_accounts);
        assert_eq!(
            w.secrets_snapshot(),
            before_secrets,
            "the current account must not be written before the target is checked"
        );
    }

    #[test]
    fn a_target_without_a_secret_is_rejected_before_anything_is_written() {
        let w = World::new();
        w.store.secrets().delete(&w.b_id).unwrap();
        let before_claude = w.claude_files();
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();

        let error =
            switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now()).unwrap_err();

        match error {
            SwitchError::TargetHasNoLogin { label } => assert_eq!(label, "Private"),
            other => panic!("expected TargetHasNoLogin, got {other:?}"),
        }
        assert_eq!(w.claude_files(), before_claude);
        assert_eq!(w.accounts_bytes(), before_accounts);
        assert_eq!(w.secrets_snapshot(), before_secrets);
    }

    #[test]
    fn an_unknown_target_key_is_an_error() {
        let w = World::new();
        let before = w.claude_files();
        let error = switch_to(&w.paths, &w.store, "ZZ", UnsavedPolicy::Refuse, now()).unwrap_err();
        assert!(matches!(error, SwitchError::UnknownAccount(_)));
        assert_eq!(w.claude_files(), before);
        w.assert_no_claude_locks();
    }

    #[test]
    fn a_busy_claude_lock_stops_the_switch_and_writes_nothing() {
        let w = World::new();
        let lock = w.paths.storage_write_lock();
        fs::create_dir(&lock).unwrap();

        // Keep the foreign lock's mtime fresh while the switch waits.
        let held = lock.clone();
        let (stop, receiver) = mpsc::channel();
        let heartbeat = thread::spawn(move || loop {
            let _ = filetime::set_file_mtime(&held, filetime::FileTime::now());
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => break,
            }
        });

        let before_claude = w.claude_files();
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();
        let error = switch_to_with_timeouts(
            &w.paths,
            &w.store,
            &w.b_id,
            UnsavedPolicy::Refuse,
            now(),
            Timeouts {
                claude: Duration::from_millis(150),
                app: APP_LOCK_TIMEOUT,
            },
        )
        .unwrap_err();

        let _ = stop.send(());
        heartbeat.join().unwrap();

        match error {
            SwitchError::Lock(LockError::Busy { path }) => {
                assert_eq!(path, w.paths.storage_write_lock())
            }
            other => panic!("expected Lock(Busy), got {other:?}"),
        }
        assert_eq!(w.claude_files(), before_claude);
        assert_eq!(w.accounts_bytes(), before_accounts);
        assert_eq!(w.secrets_snapshot(), before_secrets);
        assert!(!w.paths.refresh_lock().exists());
        assert!(!w.paths.legacy_refresh_lock().exists());
        assert!(!w.paths.identity_lock().exists());
        assert!(lock.is_dir(), "the foreign lock is not ours to remove");
    }

    #[test]
    fn a_failed_identity_write_rolls_the_live_login_back() {
        let mut w = World::new();
        // Keep the login keys last, as Claude Code writes them, so restoring the
        // exact login really restores the exact bytes.
        let mut credentials = Map::new();
        credentials.insert("mcpOAuth".to_string(), mcp_oauth());
        for (key, value) in &w.a_live_login.0 {
            credentials.insert(key.clone(), value.clone());
        }
        let before_credentials = serde_json::to_vec(&Value::Object(credentials.clone())).unwrap();
        write_private(&w.paths.credentials_file, &before_credentials);
        let before_identity = fs::read(&w.paths.identity_file).unwrap();

        let identity_dir = w.tmp.path().join("identity");
        fs::create_dir(&identity_dir).unwrap();
        let identity_path = identity_dir.join(".claude.json");
        fs::rename(&w.paths.identity_file, &identity_path).unwrap();
        w.paths.identity_file = identity_path.clone();
        // Write+execute lets the lock folder and the temp file be created, but
        // the final folder fsync opens the folder for reading, which needs read
        // permission and fails. The login write has already happened by then.
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o300)).unwrap();

        let result = switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now());
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o700)).unwrap();

        match result {
            Err(SwitchError::Live(_)) => {}
            other => panic!("expected Live, got {other:?}"),
        }
        assert_eq!(
            fs::read(&w.paths.credentials_file).unwrap(),
            before_credentials,
            "the live login must be byte-identical after the rollback"
        );
        assert_eq!(
            fs::read(&identity_path).unwrap(),
            before_identity,
            "the identity file must be untouched"
        );
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_live_login.clone())
        );
        assert_eq!(
            w.account_file().find(&w.b_id).unwrap().last_used_at,
            None,
            "the target was not switched to, so last_used_at stays unset"
        );
        w.assert_no_claude_locks();
    }

    #[test]
    fn a_rolled_back_save_switch_removes_the_new_accounts_secret() {
        let mut w = World::new();
        w.set_live(&w.c_login, &w.c_identity);
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();

        let identity_dir = w.tmp.path().join("identity");
        fs::create_dir(&identity_dir).unwrap();
        let identity_path = identity_dir.join(".claude.json");
        fs::rename(&w.paths.identity_file, &identity_path).unwrap();
        w.paths.identity_file = identity_path;
        // Write+execute is enough for the lock folder and the login write; the
        // identity write fails afterwards, so the switch rolls back.
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o300)).unwrap();

        let result = switch_to(
            &w.paths,
            &w.store,
            &w.b_id,
            UnsavedPolicy::Save {
                label: Some("Work".to_string()),
            },
            now(),
        );
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o700)).unwrap();

        match result {
            Err(SwitchError::Live(_)) => {}
            other => panic!("expected Live, got {other:?}"),
        }
        assert_eq!(
            w.accounts_bytes(),
            before_accounts,
            "the rolled-back switch must not save the new account"
        );
        assert_eq!(
            w.secrets_snapshot(),
            before_secrets,
            "the new account's secret must not be left without an account"
        );
        assert_eq!(
            read_login(&w.paths).unwrap(),
            Some(w.c_login.clone()),
            "the live login must be rolled back"
        );
        w.assert_no_claude_locks();
    }

    #[test]
    fn a_rolled_back_save_switch_reports_a_failed_secret_delete() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);
        // Make only delete fail: the new secret is written, its cleanup fails.
        let inner = FileSecrets::new(w.app.secrets_dir());
        let store = Store::new(w.app.clone(), Box::new(FailingDeleteSecrets { inner }));

        let identity_dir = w.tmp.path().join("identity");
        fs::create_dir(&identity_dir).unwrap();
        let identity_path = identity_dir.join(".claude.json");
        fs::rename(&w.paths.identity_file, &identity_path).unwrap();
        let mut paths = w.paths.clone();
        paths.identity_file = identity_path;
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o300)).unwrap();

        let result = switch_to(
            &paths,
            &store,
            &w.b_id,
            UnsavedPolicy::Save {
                label: Some("Work".to_string()),
            },
            now(),
        );
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o700)).unwrap();

        match result {
            Err(SwitchError::PartlyDone { state }) => {
                assert!(state.contains("could not be removed"), "got {state}");
                assert!(
                    state.contains("the test secret store refuses to delete"),
                    "got {state}"
                );
            }
            other => panic!("expected PartlyDone, got {other:?}"),
        }
    }

    #[test]
    fn a_live_account_without_a_login_takes_the_normal_switch_path() {
        let w = World::new();
        fs::remove_file(&w.paths.credentials_file).unwrap();

        let outcome = switch_to(&w.paths, &w.store, &w.a_id, UnsavedPolicy::Refuse, now()).unwrap();

        assert!(!outcome.already_active);
        assert_eq!(outcome.from.as_deref(), Some(w.a_id.as_str()));
        assert_eq!(
            read_login(&w.paths).unwrap(),
            Some(w.a_stored_login.clone()),
            "the stored login must be written when the live login is gone"
        );
        w.assert_no_claude_locks();
    }

    #[test]
    fn a_live_account_with_an_ended_login_returns_target_logged_out() {
        let w = World::new();
        let mut ended = w.a_live_login.clone();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), Value::String(String::new()));
        w.set_live(&ended, &w.a_identity);
        let before_claude = w.claude_files();

        let error =
            switch_to(&w.paths, &w.store, &w.a_id, UnsavedPolicy::Refuse, now()).unwrap_err();

        match error {
            SwitchError::TargetLoggedOut { label } => assert_eq!(label, "Acme"),
            other => panic!("expected TargetLoggedOut, got {other:?}"),
        }
        assert_eq!(
            w.account_file().find(&w.a_id).unwrap().login_state,
            LoginState::LoggedOut
        );
        assert_eq!(
            w.claude_files(),
            before_claude,
            "an ended live login must not be written to Claude's files"
        );
        w.assert_no_claude_locks();
    }

    #[test]
    fn an_unknown_ended_live_login_is_treated_as_discard() {
        let w = World::new();
        let mut ended = w.c_login.clone();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), Value::String(String::new()));
        w.set_live(&ended, &w.c_identity);

        let outcome = switch_to(&w.paths, &w.store, &w.b_id, UnsavedPolicy::Refuse, now()).unwrap();

        assert_eq!(outcome.saved_new, None);
        assert_eq!(w.account_file().accounts.len(), 2);
        assert!(w
            .account_file()
            .find_by_identity(C_ACCOUNT_UUID, C_ORG_UUID)
            .is_none());
        assert_eq!(read_login(&w.paths).unwrap(), Some(w.b_login.clone()));
    }

    #[test]
    fn busy_lock_messages_distinguish_claude_code_from_seatswap() {
        let claude = SwitchError::Lock(LockError::Busy {
            path: PathBuf::from("/home/u/.claude/.storage-write.lock"),
        });
        assert_eq!(
            claude.to_string(),
            "Claude Code is busy with its login, try again"
        );
        let app = SwitchError::Lock(LockError::Busy {
            path: PathBuf::from("/home/u/.config/seatswap/switch.lock"),
        });
        assert_eq!(app.to_string(), "another seatswap is running");
    }

    #[test]
    fn sync_live_create_without_an_identity_names_the_identity_file() {
        let w = World::new();
        // The live login is still there, but .claude.json has no oauthAccount.
        fs::write(&w.paths.identity_file, br#"{"numStartups":41}"#).unwrap();

        let error = sync_live(&w.paths, &w.store, Some(None), now()).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains(&w.paths.identity_file.display().to_string()),
            "the message must name the identity file, got {message}"
        );
        assert!(message.contains("oauthAccount"), "got {message}");

        // An oauthAccount without both ids is incomplete, not a usable identity.
        let incomplete = json!({"oauthAccount": {"accountUuid": C_ACCOUNT_UUID}});
        fs::write(
            &w.paths.identity_file,
            serde_json::to_vec(&incomplete).unwrap(),
        )
        .unwrap();
        let error = sync_live(&w.paths, &w.store, Some(None), now()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&w.paths.identity_file.display().to_string()),
            "the message must name the identity file, got {message}"
        );
        assert!(message.contains("incomplete"), "got {message}");
    }

    #[test]
    fn sync_live_updates_a_known_account_once() {
        let w = World::new();

        let (state, written) = sync_live(&w.paths, &w.store, None, now()).unwrap();
        assert_eq!(
            state,
            LiveState::Known {
                account_id: w.a_id.clone()
            }
        );
        assert!(written);
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_live_login.clone())
        );

        let before_accounts = w.accounts_bytes();
        let (state, written) = sync_live(&w.paths, &w.store, None, now()).unwrap();
        assert_eq!(
            state,
            LiveState::Known {
                account_id: w.a_id.clone()
            }
        );
        assert!(!written, "the same tokens must not be written again");
        assert_eq!(w.accounts_bytes(), before_accounts);
    }

    #[test]
    fn sync_live_does_not_create_an_unknown_account_without_asking() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);
        let before_accounts = w.accounts_bytes();
        let before_secrets = w.secrets_snapshot();

        let (state, written) = sync_live(&w.paths, &w.store, None, now()).unwrap();

        assert_eq!(
            state,
            LiveState::Unknown {
                email: Some("current@example.com".to_string()),
                org_name: Some("Other GmbH".to_string()),
            }
        );
        assert!(!written);
        assert_eq!(w.accounts_bytes(), before_accounts);
        assert_eq!(w.secrets_snapshot(), before_secrets);
    }

    #[test]
    fn sync_live_creates_an_unknown_account_when_asked() {
        let w = World::new();
        w.set_live(&w.c_login, &w.c_identity);

        let (state, written) =
            sync_live(&w.paths, &w.store, Some(Some("Work".to_string())), now()).unwrap();

        let LiveState::Known { account_id } = state else {
            panic!("the created account becomes known, got {state:?}");
        };
        assert!(written);
        let file = w.account_file();
        let account = file.find(&account_id).unwrap();
        assert_eq!(account.label, "Work");
        assert_eq!(account.account_uuid, C_ACCOUNT_UUID);
        assert_eq!(
            w.store.secrets().get(&account_id).unwrap(),
            Some(w.c_login.clone())
        );
        assert_eq!(file.accounts.len(), 3);
    }

    #[test]
    fn sync_live_marks_an_ended_login_logged_out_without_storing_it() {
        let w = World::new();
        let mut ended = w.a_live_login.clone();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), Value::String(String::new()));
        w.set_live(&ended, &w.a_identity);

        let (state, written) = sync_live(&w.paths, &w.store, None, now()).unwrap();

        assert_eq!(
            state,
            LiveState::Ended {
                account_id: Some(w.a_id.clone())
            }
        );
        assert!(written);
        assert_eq!(
            w.account_file().find(&w.a_id).unwrap().login_state,
            LoginState::LoggedOut
        );
        assert_eq!(
            w.store.secrets().get(&w.a_id).unwrap(),
            Some(w.a_stored_login.clone())
        );
    }

    #[test]
    fn sync_live_refuses_before_reading_when_claude_is_busy() {
        let w = World::new();
        // A corrupt file would fail if it were read.
        fs::write(&w.paths.credentials_file, b"not json").unwrap();
        fs::create_dir(w.paths.refresh_lock()).unwrap();

        let error = sync_live(&w.paths, &w.store, None, now()).unwrap_err();

        assert!(matches!(error, SwitchError::Lock(LockError::Busy { .. })));
    }

    #[test]
    fn live_state_reads_no_login_known_unknown_and_ended() {
        let w = World::new();
        let file = w.account_file();
        assert_eq!(
            live_state(&w.paths, &file).unwrap(),
            LiveState::Known {
                account_id: w.a_id.clone()
            }
        );

        w.set_live(&w.b_login, &w.b_identity);
        assert_eq!(
            live_state(&w.paths, &file).unwrap(),
            LiveState::Known {
                account_id: w.b_id.clone()
            }
        );

        w.set_live(&w.c_login, &w.c_identity);
        assert_eq!(
            live_state(&w.paths, &file).unwrap(),
            LiveState::Unknown {
                email: Some("current@example.com".to_string()),
                org_name: Some("Other GmbH".to_string()),
            }
        );

        let mut ended = w.c_login.clone();
        ended
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("refreshToken".to_string(), Value::String(String::new()));
        w.set_live(&ended, &w.c_identity);
        assert_eq!(
            live_state(&w.paths, &file).unwrap(),
            LiveState::Ended { account_id: None }
        );

        fs::remove_file(&w.paths.credentials_file).unwrap();
        assert_eq!(live_state(&w.paths, &file).unwrap(), LiveState::NoLogin);
    }

    #[test]
    fn remove_account_of_the_live_account_never_touches_claude_files() {
        let w = World::new();
        let before = w.claude_files();

        let removed = remove_account(&w.store, &w.a_id).unwrap();

        assert_eq!(removed.id, w.a_id);
        assert_eq!(w.claude_files(), before);
        assert!(w.store.secrets().get(&w.a_id).unwrap().is_none());
        let file = w.account_file();
        assert_eq!(file.accounts.len(), 1);
        assert_eq!(file.accounts[0].id, w.b_id);
    }

    #[test]
    fn edit_account_validates_and_normalises() {
        let w = World::new();
        let before_accounts = w.accounts_bytes();

        let clash = edit_account(&w.store, &w.a_id, None, Some("pr"), None);
        match clash {
            Err(SwitchError::Store(StoreError::DuplicateAbbr(_))) => {}
            other => panic!("expected DuplicateAbbr, got {other:?}"),
        }
        assert_eq!(w.accounts_bytes(), before_accounts);

        let bad_color = edit_account(&w.store, &w.a_id, None, None, Some("3b82f6"));
        match bad_color {
            Err(SwitchError::Store(StoreError::InvalidColor(_))) => {}
            other => panic!("expected InvalidColor, got {other:?}"),
        }
        assert_eq!(w.accounts_bytes(), before_accounts);

        let bad_label = edit_account(&w.store, &w.a_id, Some("   "), None, None);
        match bad_label {
            Err(SwitchError::Store(StoreError::InvalidLabel(_))) => {}
            other => panic!("expected InvalidLabel, got {other:?}"),
        }
        assert_eq!(w.accounts_bytes(), before_accounts);

        let edited = edit_account(
            &w.store,
            &w.a_id,
            Some("  Team A  "),
            Some("z9"),
            Some("#ABCDEF"),
        )
        .unwrap();
        assert_eq!(edited.label, "Team A");
        assert_eq!(edited.abbr, "Z9");
        assert_eq!(edited.color, "#abcdef");
        let stored = w.account_file();
        let stored = stored.find(&w.a_id).unwrap();
        assert_eq!(stored.label, "Team A");
        assert_eq!(stored.abbr, "Z9");
        assert_eq!(stored.color, "#abcdef");
    }

    #[test]
    fn world_has_two_distinct_real_shaped_accounts() {
        let w = World::new();
        assert_eq!(A_ACCOUNT_UUID.len(), 36);
        assert_eq!(B_ACCOUNT_UUID.len(), 36);
        assert_ne!(A_ACCOUNT_UUID, B_ACCOUNT_UUID);
        assert_eq!(
            w.a_live_login.refresh_token().map(str::len),
            Some("sk-ant-ort01-".len() + 95)
        );
        assert_eq!(
            w.b_login.access_token().map(str::len),
            Some("sk-ant-oat01-".len() + 95)
        );
        assert_ne!(w.a_identity, w.b_identity);
        assert_ne!(w.a_live_login, w.b_login);
        assert_ne!(w.a_stored_login, w.a_live_login);
    }

    fn backup_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".seatswap-backup");
        PathBuf::from(name)
    }
}
