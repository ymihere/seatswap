# seatswap — implementation notes (Linux v1)

The spec (`docs/spec.md`) says what the program does and why. This file fixes the module contracts, so slices built in parallel fit together. Signatures here are binding; bodies are free. Section numbers like §6.2 refer to the spec.

Done already (do not rewrite): `types.rs` (`Login`, `OAuthAccount`, `LOGIN_KEYS`), `paths.rs` (`ClaudePaths`, `AppPaths`), `atomic.rs` (`write_atomic`, `backup_once`).

Linux and macOS share the Unix code; Windows and the macOS Keychain are later work, not v1.

---

## Real-shaped test data

`.credentials.json` as Claude Code writes it: one line, no trailing newline, mode 0600.

```json
{"mcpOAuth":{"plugin:engineering:linear|5f1c2a9be0d34c11":{"serverName":"plugin:engineering:linear","accessToken":"lin_oauth_8d2f…(64 chars)","expiresAt":1794400000000}},"claudeAiOauth":{"accessToken":"sk-ant-oat01-<95 random chars>","refreshToken":"sk-ant-ort01-<95 random chars>","expiresAt":1794356040000,"refreshTokenExpiresAt":1796772840000,"scopes":["user:inference","user:profile","user:file_upload","user:mcp_servers","user:plugins","user:sessions:claude_code"],"subscriptionType":"team","subscriptionTypeSource":"profile","rateLimitTier":"default_claude_max_5x"},"organizationUuid":"e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62"}
```

`.claude.json`: pretty-printed with 2 spaces, no trailing newline, mode 0600, ~100 KB, many keys. The part we touch:

```json
{
  "numStartups": 41,
  "oauthAccount": {
    "accountUuid": "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30",
    "emailAddress": "someone@example.com",
    "organizationUuid": "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62",
    "organizationName": "Acme",
    "organizationRole": "admin",
    "billingType": "stripe_subscription",
    "hasExtraUsageEnabled": false
  },
  "projects": { "/home/u/x": { "allowedTools": [] } }
}
```

Usage reply (`GET /api/oauth/usage`), trimmed:

```json
{
  "five_hour":  { "utilization": 97.0, "resets_at": "2026-11-10T13:40:00.102645+00:00" },
  "seven_day":  { "utilization": 48.0, "resets_at": "2026-11-16T01:00:00+00:00" },
  "limits": [
    { "kind": "session", "group": "session", "percent": 97, "severity": "critical",
      "resets_at": "2026-11-10T13:40:00+00:00", "scope": null, "is_active": true },
    { "kind": "weekly_all", "group": "weekly", "percent": 48, "severity": "normal",
      "resets_at": "2026-11-16T01:00:00+00:00", "scope": null, "is_active": false },
    { "kind": "weekly_scoped", "group": "weekly", "percent": 43, "severity": "normal",
      "resets_at": "2026-11-16T01:00:00+00:00",
      "scope": { "model": { "display_name": "Fable" } }, "is_active": false }
  ],
  "extra_usage": { "is_enabled": false },
  "some_code_named_field": null
}
```

A personal account's reply can have `"five_hour": null` and no `session` entry in `limits`.

Refresh reply (`POST https://platform.claude.com/v1/oauth/token`):

```json
{ "token_type": "Bearer", "access_token": "sk-ant-oat01-…", "expires_in": 28800,
  "refresh_token": "sk-ant-ort01-…", "refresh_token_expires_in": 2374560,
  "scope": "user:inference user:profile user:file_upload user:mcp_servers user:plugins user:sessions:claude_code",
  "token_uuid": "…", "account": { "uuid": "4b7e2f91-…", "email_address": "someone@example.com" },
  "organization": { "uuid": "e2a6c9d1-…" } }
```

Errors seen: `400 {"error":"invalid_grant","error_description":"Refresh token not found or invalid"}`; `401` on usage with a dead access token; `403` with body `error code: 1010` when the User-Agent is a library default.

---

## locks.rs (§2.6, §6.2)

Claude Code's locks are folders made with `mkdir`. The holder keeps the folder's modification time fresh; others treat a lock older than its limit as abandoned.

```rust
pub const HEARTBEAT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct LockSpec { pub path: PathBuf, pub stale_after: Duration }

/// A held lock folder. A background thread sets its mtime to now every HEARTBEAT.
/// Drop stops the thread and removes the folder (a folder already gone is fine).
pub struct DirLock { /* … */ }
impl DirLock { pub fn path(&self) -> &Path; }

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{path} is held by another process")] Busy { path: PathBuf },
    #[error("lock {path}: {source}")] Io { path: PathBuf, #[source] source: std::io::Error },
}

/// mkdir the folder. If it exists: when its mtime is older than stale_after, remove it
/// (rmdir) and try again; otherwise sleep ~50 ms and try again until `deadline`, then Busy.
/// The parent folder must exist (no create_dir_all: a missing config folder is an Io error).
pub fn acquire_dir_lock(spec: &LockSpec, deadline: Instant) -> Result<DirLock, LockError>;

/// The four specs in the order Claude Code takes them:
/// refresh_lock 60 s, legacy_refresh_lock 60 s, storage_write_lock 15 s, identity_lock 10 s.
pub fn claude_lock_specs(paths: &ClaudePaths) -> [LockSpec; 4];

/// All four, in that order, sharing one deadline (now + timeout). On failure the ones
/// already taken are released before returning the error. Dropping ClaudeLocks releases
/// them in reverse order.
pub struct ClaudeLocks { /* Vec<DirLock> */ }
pub fn acquire_claude_locks(paths: &ClaudePaths, timeout: Duration) -> Result<ClaudeLocks, LockError>;

/// True when any of the four exists and is not stale. The watcher reads only when false.
pub fn claude_locks_busy(paths: &ClaudePaths) -> bool;

/// seatswap's own exclusive lock on <data_dir>/switch.lock (fs4 file lock, the file
/// is created if missing, the data folder too). Poll try_lock until timeout, then Busy.
pub struct AppLock { /* File */ }
pub fn acquire_app_lock(app: &AppPaths, timeout: Duration) -> Result<AppLock, LockError>;
```

Never write `.oauth_refresh.lock.owner`.

## live.rs (§2.1, §2.3, §6.2)

Reads and writes the live login and identity. **The caller holds `ClaudeLocks` for every write**; live.rs does not lock.

```rust
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("{path} is not valid JSON; not touching it")] Corrupt { path: PathBuf },
    #[error("{path} has no JSON object at the top level")] NotObject { path: PathBuf },
    #[error("{path} does not exist; run Claude Code once first")] Missing { path: PathBuf },
    #[error("login holds a key that is not a login key: {key}")] ForeignKey { key: String },
    #[error("{path}: {source}")] Io { path: PathBuf, #[source] source: std::io::Error },
}

/// The live login (the LOGIN_KEYS of .credentials.json). Ok(None) when the file is missing
/// or has no claudeAiOauth.
pub fn read_login(paths: &ClaudePaths) -> Result<Option<Login>, LiveError>;

/// Replace the login in .credentials.json: read the whole file (missing → empty object),
/// remove every LOGIN_KEYS key, insert the login's keys (None = log out: only remove),
/// keep every other key in place and order, write compact JSON without a trailing newline,
/// via atomic::write_atomic (mode 0600 if new). Call atomic::backup_once first.
pub fn write_login(paths: &ClaudePaths, login: Option<&Login>) -> Result<(), LiveError>;

/// oauthAccount of .claude.json. Ok(None) when the file or the key is missing.
pub fn read_identity(paths: &ClaudePaths) -> Result<Option<OAuthAccount>, LiveError>;

/// Replace oauthAccount only; the file must exist (Missing otherwise). Pretty JSON with
/// 2-space indent, no trailing newline, other keys in place and order. backup_once first.
pub fn write_identity(paths: &ClaudePaths, account: &OAuthAccount) -> Result<(), LiveError>;
```

## store.rs (§5, §8.2, §10)

```rust
#[derive(Debug, thiserror::Error)] pub enum StoreError { /* Io, Corrupt{path}, UnsupportedVersion(u32),
    InvalidAbbr(String), DuplicateAbbr(String), InvalidColor(String), InvalidLabel(String),
    UnknownAccount(String), DuplicateAccount{email}, Keyring(String), InvalidSetting(String),
    InvalidId(String), MissingIdentity */ }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,                 // uuid v4, simple form
    pub account_uuid: String,
    pub organization_uuid: String,
    pub label: String,              // free text, 1..=40 chars after trim
    pub abbr: String,               // 1–2 chars, A–Z 0–9, upper case, unique
    pub color: String,              // "#rrggbb", lower case
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub plan: Option<String>,       // plan_label(), e.g. "Ultra", "Team"
    pub oauth_account: OAuthAccount,
    pub added_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub login_state: LoginState,
    pub login_ends_at_ms: Option<i64>,  // refreshTokenExpiresAt of the stored login
}

#[derive(… Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum LoginState { Ok, LoggedOut }

/// Settings §10 (no clearOrgCaches). Defaults: tray_text Percent, icon_limit Binding,
/// poll_active_secs 120, poll_inactive_secs 900, suggest_at 90, suggest_target_below 50,
/// notify_on_switch true, mirror_live true, refresh_inactive true, hotkey "Ctrl+Alt+C",
/// start_at_login true, theme System, claude_config_dir None.
/// `#[serde(default)]` per field so an older file without a new setting still loads.
pub struct Settings { … }  // camelCase in the file
pub enum TrayText { Percent, PercentReset, Off }
pub enum IconLimit { Binding, Session, Week }
pub enum Theme { System, Light, Dark }
impl Settings { pub fn validate(&self) -> Result<(), StoreError>; } // percents 1..=100, polls >= 30 s

pub struct AccountsFile { pub version: u32, pub accounts: Vec<Account>, pub settings: Settings,
                          #[serde(default)] pub secrets_backend: Option<SecretsBackend> }
// version must be 1; anything else is UnsupportedVersion, never silently upgraded.

pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError>;
    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError>;
    fn delete(&self, id: &str) -> Result<(), StoreError>;   // missing is fine
    fn encrypted(&self) -> bool;                             // false → UI warns (§5)
}
pub struct KeyringSecrets;            // keyring crate, service "seatswap", user = account id, JSON value
pub struct FileSecrets { dir: PathBuf } // <data>/secrets/<id>.json; folder 0700, files 0600, via write_atomic
impl FileSecrets { pub fn new(dir: PathBuf) -> FileSecrets; }
/// Where the logins are kept. Chosen once, on first use, and recorded in accounts.json, so the
/// logins never end up split between two places.
#[derive(… Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum SecretsBackend { Keyring, File }

/// recorded = AccountsFile.secrets_backend.
///   Some(Keyring) → KeyringSecrets; if the keyring does not answer (probe fails) → Err(Keyring(
///                   "the OS keyring is not available: <reason>; the logins are stored there")).
///                   Never falls back to files: that would hide the stored logins.
///   Some(File)    → FileSecrets at app.secrets_dir(), no probe.
///   None (first use) → probe (set/get/delete of "seatswap-probe"); OK → Keyring, else File.
/// Returns the store, the backend to record, and a warning when the backend is File
/// ("logins are stored unencrypted in <dir>") — shown by the UI every time (§5).
pub fn open_secrets(app: &AppPaths, recorded: Option<SecretsBackend>)
    -> Result<(Box<dyn SecretStore>, SecretsBackend, Option<String>), StoreError>;

pub struct Store { app: AppPaths, secrets: Box<dyn SecretStore> }
impl Store {
    pub fn new(app: AppPaths, secrets: Box<dyn SecretStore>) -> Store;
    pub fn secrets(&self) -> &dyn SecretStore;
    pub fn app_paths(&self) -> &AppPaths;
    /// Missing file → empty list + default settings. Corrupt → error (never overwritten).
    pub fn load(&self) -> Result<AccountsFile, StoreError>;
    /// Pretty JSON, write_atomic, 0600; creates the data folder (0700) if missing.
    pub fn save(&self, file: &AccountsFile) -> Result<(), StoreError>;
}

/// The one way to open the store (CLI and GUI): load accounts.json, open the secrets for the
/// recorded backend, record the backend on first use (save), return the store and the warning
/// of open_secrets.
pub fn open_store(app: AppPaths) -> Result<(Store, Option<String>), StoreError>;

impl AccountsFile {
    pub fn find(&self, id: &str) -> Option<&Account>;
    pub fn find_by_identity(&self, account_uuid: &str, organization_uuid: &str) -> Option<&Account>;
    /// Look up by id or by abbreviation (case-insensitive) — what the CLI accepts.
    pub fn find_by_key(&self, key: &str) -> Option<&Account>;
}

pub const PALETTE: [&str; 10] = ["#3b82f6", "#f59e0b", "#10b981", "#ef4444", "#8b5cf6",
                                 "#ec4899", "#14b8a6", "#f97316", "#6366f1", "#84cc16"];
/// "Acme Team" → "AT", "Acme" → "AC", "Private" → "PR", "x" → "X".
/// Letters/digits only; words split on whitespace and punctuation. No letter or digit → InvalidLabel.
pub fn default_abbr(label: &str) -> Result<String, StoreError>;
/// Trim, upper-case, 1–2 of A–Z0–9, not used by another account (`others` = their abbrs).
pub fn validate_abbr(abbr: &str, others: &[&str]) -> Result<String, StoreError>;
/// "#rrggbb" (any case) → lower case; anything else InvalidColor.
pub fn validate_color(color: &str) -> Result<String, StoreError>;
/// The identity's planDisplayName ("Free", "Pro", "Max", "Ultra"), else the login's
/// subscriptionType capitalised ("Team"). Never the rate-limit tier ("max 20x").
/// Recomputed whenever a login is stored for the account, through Account::note_stored_login.
pub fn plan_label(login: &Login, identity: &OAuthAccount) -> Option<String>;
impl Account {
    /// After a login was stored for this account (switch, sync_live, the engine's refresh):
    /// sets plan (plan_label), login_ends_at_ms and login_state = Ok; true if anything changed.
    /// The only place these three fields are written for an existing account.
    pub fn note_stored_login(&mut self, login: &Login) -> bool;
}
/// A new Account for a login + identity. Label: org name, else email; abbr: default_abbr made
/// unique by trying the label's first letter + each next letter/digit; colour: first PALETTE
/// entry not used. Identity missing accountUuid/organizationUuid → error. Already stored → DuplicateAccount.
pub fn new_account(login: &Login, identity: &OAuthAccount, label: Option<&str>, file: &AccountsFile,
                   now: DateTime<Utc>) -> Result<Account, StoreError>;
```

## api.rs (§3, §7.2, §7.3)

All HTTP lives here. `ureq` 2, timeout 20 s, header `User-Agent: seatswap/<CARGO_PKG_VERSION>` on every request.

```rust
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    Unauthorized,                       // 401
    InvalidGrant(String),               // 400 error=invalid_grant; the description
    OwnerMismatch { expected: (String, String), got: (String, String) },
    RateLimited { retry_after_secs: Option<u64> },  // 429
    Server(u16),                        // 5xx
    Http { status: u16, body: String },  // any other status; body cut to 300 chars
    Network(String),
    Decode(String),
}
impl ApiError { pub fn is_transient(&self) -> bool; } // RateLimited, Server, Network

pub struct Api { /* agent, usage_url, token_url */ }
impl Api {
    pub fn new() -> Api;
    pub fn with_urls(usage_url: &str, token_url: &str) -> Api;   // tests: local server
    pub fn usage(&self, access_token: &str) -> Result<Usage, ApiError>;
    /// POST the refresh (JSON body: grant_type, refresh_token, client_id, scope = the login's
    /// scopes joined by spaces). Check account.uuid / organization.uuid against `expected`
    /// (accountUuid, organizationUuid) → OwnerMismatch. Return the login with claudeAiOauth updated:
    /// accessToken; expiresAt = now_ms + expires_in*1000; refreshToken when the reply has one;
    /// refreshTokenExpiresAt = min(old, now_ms + refresh_token_expires_in*1000) when the reply has
    /// it, else unchanged; scopes from "scope" when present. All other fields and keys unchanged.
    pub fn refresh(&self, login: &Login, expected: (&str, &str), now_ms: i64) -> Result<Login, ApiError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage { pub limits: Vec<Limit>, pub extra_usage_enabled: bool }
pub struct Limit {
    pub kind: LimitKind,             // Session | WeeklyAll | WeeklyScoped | Other(String)
    pub percent: f64,
    pub severity: Severity,          // Normal | Warning | Critical | Other(String)
    pub resets_at: Option<DateTime<Utc>>,
    pub model: Option<String>,       // scope.model.display_name
    pub is_active: bool,
}
impl Usage {
    /// limits[] when present; else build Session from five_hour and WeeklyAll from seven_day
    /// (utilization → percent, severity Normal); null or missing parts are left out.
    /// Unknown keys ignored. A reply that is not an object → Decode.
    pub fn from_json(v: &serde_json::Value) -> Result<Usage, ApiError>;
    pub fn session(&self) -> Option<&Limit>;
    pub fn weekly_all(&self) -> Option<&Limit>;
    pub fn weekly_scoped(&self) -> impl Iterator<Item = &Limit>;
    /// The limit with is_active, else the highest percent, else None.
    pub fn binding(&self) -> Option<&Limit>;
}
/// §7.3: < 70 Fine, 70–89 Close, ≥ 90 or Critical severity → Critical, ≥ 100 → Locked.
pub enum LoadLevel { Fine, Close, Critical, Locked }
impl Limit { pub fn level(&self) -> LoadLevel; }
impl LoadLevel { pub fn color(&self) -> &'static str; } // "#22c55e" "#f59e0b" "#ef4444" "#ef4444"
```

## switcher.rs (§6.1, §6.2, §6.4)

Everything that changes which login is live, or copies the live login into the store. Uses `locks`, `live`, `store`; never HTTP.

```rust
pub const CLAUDE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
pub const APP_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    Lock(#[from] LockError),          // message for Busy on a Claude lock: "Claude Code is busy with its login, try again"
    Live(#[from] LiveError),
    Store(#[from] StoreError),
    UnknownAccount(String),           // the key given
    TargetLoggedOut { label: String },// stored login marked LoggedOut: show the re-login steps
    TargetHasNoLogin { label: String },// no secret stored for it
    CurrentNotSaved { email: Option<String> }, // live login belongs to no stored account, policy Refuse
    PartlyDone { state: String },     // e.g. "login switched to P, identity not: <error>"
}

/// What to do when the live login belongs to no stored account.
pub enum UnsavedPolicy { Refuse, Discard, Save { label: Option<String> } }

pub struct SwitchOutcome {
    pub from: Option<String>,   // account id that was live, if known
    pub to: String,             // target account id
    pub saved_new: Option<String>, // id of an account created by UnsavedPolicy::Save
    pub already_active: bool,   // target was already live: nothing written to Claude's files
}

/// §6.2. Order: app lock (APP_LOCK_TIMEOUT) → Claude locks (CLAUDE_LOCK_TIMEOUT) → read live
/// login + identity → find current by identity → save live login into secrets[cur] and identity
/// into cur.oauth_account, login_ends_at_ms, login_state Ok (if the live login is_ended(): mark cur
/// LoggedOut instead and save nothing) → unknown current: apply the policy (Refuse: release all
/// locks, return CurrentNotSaved — the caller asks the person and calls again) → target login from
/// secrets → live::write_login, then live::write_identity (a failure after the first write →
/// PartlyDone naming what was done) → target.last_used_at = now → store.save → locks released.
/// Target == current → refresh the stored copy only, already_active = true.
pub fn switch_to(claude: &ClaudePaths, store: &Store, target_key: &str, unsaved: UnsavedPolicy,
                 now: DateTime<Utc>) -> Result<SwitchOutcome, SwitchError>;

/// What the live login is, compared with the store. Read-only; takes no lock.
pub enum LiveState {
    NoLogin,                                  // no file or no claudeAiOauth
    Ended { account_id: Option<String> },     // is_ended()
    Known { account_id: String },
    Unknown { email: Option<String>, org_name: Option<String> },
}
pub fn live_state(claude: &ClaudePaths, file: &AccountsFile) -> Result<LiveState, SwitchError>;

/// "Save as account" (§6.1) and the watcher's mirror (§6.4 mirrorLive). Under the app lock, and
/// only when claude_locks_busy() is false (else Lock(Busy) — the caller retries later): read the live
/// login + identity; known account → store the login if it differs from the stored one, update
/// oauth_account / login_ends_at_ms / login_state Ok; unknown → create it with
/// store::new_account(label) only when `create_unknown` is true. Returns the resulting LiveState
/// and whether anything was written.
pub fn sync_live(claude: &ClaudePaths, store: &Store, create_unknown: Option<Option<String>>,
                 now: DateTime<Utc>) -> Result<(LiveState, bool), SwitchError>;
// create_unknown: None = never create; Some(label) = create with that label (None = default label).

/// Remove an account from the list and its login from the secrets. Never touches Claude's files.
pub fn remove_account(store: &Store, key: &str) -> Result<Account, SwitchError>;

/// Change label / abbr / colour, validated by store's validators.
pub fn edit_account(store: &Store, key: &str, label: Option<&str>, abbr: Option<&str>,
                    color: Option<&str>) -> Result<Account, SwitchError>;
```

## engine.rs (§6.3, §7)

Usage polling, refreshing inactive logins, the usage cache, and the derived views the CLI and the GUI show. Single-threaded; the GUI runs it on a worker thread.

```rust
/// The two calls the engine needs. `api::Api` implements it; tests use a fake.
pub trait Remote: Send {
    fn usage(&self, access_token: &str) -> Result<Usage, ApiError>;
    fn refresh(&self, login: &Login, expected: (&str, &str), now_ms: i64) -> Result<Login, ApiError>;
}
impl Remote for Api { … }

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub usage: Option<Usage>,
    pub fetched_at: Option<DateTime<Utc>>,   // last successful fetch
    pub last_error: Option<String>,          // human text, never a token
    pub failures: u32,                       // consecutive transient failures
    pub backoff_until: Option<DateTime<Utc>>,
}
#[derive(… Serialize, Deserialize)] pub struct UsageCache { pub version: u32 /* 1 */, pub entries: BTreeMap<String, CacheEntry> }
impl UsageCache { pub fn load(app: &AppPaths) -> Result<UsageCache, EngineError>; pub fn save(&self, app: &AppPaths) -> Result<(), EngineError>; }
// Missing file → empty. Corrupt → EngineError::CacheCorrupt (the caller may delete it; never silently).

pub enum PollReason { Timer, PanelOpened, AfterSwitch, Wake }

pub struct Engine<R: Remote> { claude: ClaudePaths, store: Store, remote: R, cache: UsageCache }
impl<R: Remote> Engine<R> {
    pub fn new(claude: ClaudePaths, store: Store, remote: R) -> Result<Self, EngineError>; // loads the cache
    pub fn store(&self) -> &Store;
    pub fn claude_paths(&self) -> &ClaudePaths;
    /// Poll every account that is due (§7.1) and save the cache. Returns the fresh views.
    ///   active  : due when older than settings.poll_active_secs; PanelOpened → older than 30 s;
    ///             AfterSwitch/Wake → always. Token = the LIVE login's access token (read the file);
    ///             never refresh it. 401 → read the live file again and retry once; still 401 →
    ///             last_error "waiting for Claude Code to renew its login".
    ///   inactive: due when older than poll_inactive_secs; PanelOpened → older than 5 min; Wake → always.
    ///             Token = the stored login. If its access token expires within 60 s:
    ///             refresh_inactive off → skip (view shows the cached usage with its age);
    ///             on → refresh (§6.3): under the app lock, re-check with live::read_identity that the
    ///             account is still not live, Remote::refresh with expected identity, then FIRST
    ///             secrets.set(new login) (an error here: account LoggedOut + last_error), then update
    ///             login_ends_at_ms and save accounts.json, then fetch usage with the new token.
    ///             InvalidGrant → login_state LoggedOut, saved; the old tokens are never used again.
    ///             OwnerMismatch → last_error, nothing saved.
    ///   transient errors (ApiError::is_transient) → failures += 1, backoff_until = now +
    ///             min(30 min, 1 min · 2^(failures-1)) (RateLimited: at least retry_after); not due before.
    ///   LoggedOut accounts are not polled.
    pub fn poll(&mut self, reason: PollReason, now: DateTime<Utc>) -> Result<Vec<AccountView>, EngineError>;
    /// Views from the cache without any network call. Active account first, then list order.
    pub fn views(&self, now: DateTime<Utc>) -> Result<Vec<AccountView>, EngineError>;
}

#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: String, pub label: String, pub abbr: String, pub color: String,
    pub email: Option<String>, pub plan: Option<String>,
    pub is_active: bool,
    pub login_state: LoginState,
    pub login_ends_at_ms: Option<i64>,
    pub login_days_left: Option<i64>,         // floor((ends - now) / 1 day), may be negative
    pub usage: Option<Usage>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// §7.4: the active account's binding limit ≥ settings.suggest_at and another Ok account's binding
/// limit < suggest_target_below (or unknown usage → not a candidate). The candidate with the lowest
/// percent wins. None otherwise.
pub struct Suggestion { pub from_id: String, pub to_id: String, pub from_percent: f64, pub to_percent: f64,
                        pub from_resets_at: Option<DateTime<Utc>> }
pub fn suggestion(views: &[AccountView], settings: &Settings) -> Option<Suggestion>;

/// §6.3: accounts whose login ends within 5 days (days_left 5..=2 → Soon, ≤ 1 → Tomorrow, < 0 → Ended).
pub enum LoginWarning { Soon { id: String, days_left: i64 }, Tomorrow { id: String }, Ended { id: String } }
pub fn login_warnings(views: &[AccountView]) -> Vec<LoginWarning>;
```

## watcher.rs (§6.4) — wave 3

Tells the GUI that the live login may have changed. It does not read or interpret the files: the
callback calls `switcher::sync_live` (mirrorLive on) or `switcher::live_state` (off).

```rust
pub const DEBOUNCE: Duration = Duration::from_millis(500);
pub const BUSY_RECHECK: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum WatchError { Notify(#[from] notify::Error), Io { path: PathBuf, source: std::io::Error } }

/// Watches the folder holding credentials_file and the folder holding identity_file (non-recursive:
/// Claude Code and seatswap replace the files by rename, so the folders are watched, not the
/// files; one watch when both are the same folder). Events for other names are ignored. After a
/// relevant event: wait until DEBOUNCE passes without another one, then until
/// locks::claude_locks_busy() is false (re-checked every BUSY_RECHECK), then call `on_change` once.
/// The callback runs on the watcher's own thread, never concurrently with itself. A missing folder
/// is an error (no create). Drop stops the watching and joins the thread.
pub struct Watcher { /* … */ }
pub fn watch(claude: &ClaudePaths, on_change: impl Fn() + Send + 'static) -> Result<Watcher, WatchError>;
```

## locks.rs / paths.rs additions — wave 3

```rust
// locks.rs: the one file-lock helper. acquire_app_lock becomes a call of it on app.switch_lock().
pub struct FileLock { /* File */ }            // AppLock is renamed to FileLock (one type, two uses)
pub fn acquire_file_lock(path: &Path, timeout: Duration) -> Result<FileLock, LockError>; // timeout 0 = try once
// paths.rs, AppPaths:
pub fn gui_lock(&self) -> PathBuf;    // <data>/gui.lock   — held for the life of the tray app
pub fn gui_socket(&self, runtime_dir: Option<&Path>) -> Result<PathBuf, PathsError>;
    // Unix socket of the running tray app. Some(dir) → <dir>/seatswap-<16 hex FNV-1a 64 of the data
    // folder's real path (fs::canonicalize)>.sock: one socket per data folder, whatever its spelling
    // (relative, absolute, through a link). The folder must exist; otherwise PathsError::DataDir
    // { path, source } (`seatswap panel` reports a missing one as "not running"). None → <data>/gui.sock.
    // A path of 108 bytes or more → PathsError::SocketPathTooLong { path } (sockaddr_un limit), never a
    // shortened path.
pub fn runtime_dir() -> Option<PathBuf>;   // free function: $XDG_RUNTIME_DIR via dirs::runtime_dir
pub fn absolute(path: &Path) -> Result<PathBuf, PathsError>;
    // std::path::absolute (lexical, no link resolution); failure → PathsError::Absolute { path, source }.
    // cli::run applies it to --data-dir and --claude-config-dir right after parsing, so everything
    // downstream (autostart entry, messages, files) sees absolute folders; autostart::startup_arguments
    // refuses a relative one (AutostartError::RelativePath) instead of resolving it again.
// paths.rs, free function (the path only; the GUI writes it with atomic::write_atomic):
pub fn autostart_file(xdg_config_home: &Path) -> PathBuf; // <xdg_config_home>/autostart/seatswap.desktop
// store.rs: reading accounts.json needs no secrets. Store::load calls it (one way).
pub fn load_accounts(app: &AppPaths) -> Result<AccountsFile, StoreError>;
// store.rs: StoreError::MissingIdentity { path } names the identity file.
// store.rs: Settings::validate rejects a claudeConfigDir that is not absolute (no `~` expansion).
// engine.rs: UsageCache::set_aside(app) -> Result<PathBuf, EngineError> renames a corrupt cache to
//   usage-cache.json.corrupt-<UTC yyyymmddThhmmss> and returns the new path (the GUI reports it).
// engine.rs: the active account is the one switcher::live_state calls Known — the only definition of
//   "live" (NoLogin, Ended, Unknown → no active account).
```

## app: CLI (§4)

`crates/app/src/cli.rs`, clap derive. `main.rs` parses; no subcommand → tray app when built with `gui`, else help. Global options: `--claude-config-dir <DIR>` (else the `claudeConfigDir` setting, else `$CLAUDE_CONFIG_DIR`, else `~/.claude`), `--data-dir <DIR>` (else the platform folder). Every command prints errors as `seatswap: <message>` to stderr and exits 1; success exits 0.

| Command | Does |
|---|---|
| `list` | One line per account: `*` for the live one, abbr, label, email, plan, session %, week %, "login ends in N d", "LOGGED OUT" when so. Usage from the cache, with its age ("5 min ago"); `--refresh` polls first (PollReason::PanelOpened). |
| `use <KEY>` | `switch_to`. KEY = id or abbr. When the live login is not saved: `--save-current [LABEL]` saves it first, `--discard-current` drops it; without either, exit 2 with the message "the current login (<email>) is not saved; rerun with --save-current or --discard-current". After the switch: "Switched to P — Private". |
| `next` | Switch to the next Ok account after the live one in list order (wraps), same unsaved handling as `use`. |
| `add [--label L]` | `sync_live` with create: saves the live login as a new account or updates the stored copy. Prints the account. |
| `rm <KEY> [--yes]` | `remove_account`; without `--yes` asks "Remove AC — Acme? [y/N]" on a terminal, refuses when stdin is not a terminal. |
| `edit <KEY> [--label L] [--abbr A] [--color #rrggbb]` | `edit_account`. |
| `panel [main\|add\|settings]` | Asks the running tray app to open its pop-up (§ app: GUI, single instance). Not running → exit 1 "seatswap is not running". This is the hotkey on Linux v1: the person binds it in the desktop's keyboard settings (no global-shortcut crate is available). |
| `status` | The live account in one line: `AC 97% · 1h12m` (binding limit, time to its reset; format §11: `35m`, `1h05m`, `1d18h`, beyond 48 h `Sun 02:00` — `format::format_reset_at`; reset times are rounded up to the full minute first, `format::reset_minute`). `--json` prints the views as JSON (`views` array + `active` id + `suggestion`); `--waybar` prints one JSON line `{"text":"AC 97%","tooltip":"…","class":"fine|close|critical|locked|none","percentage":97}`; `--i3blocks` prints three lines (full text, short text, colour). `--refresh` polls first (PollReason::Timer). |

## app: icon.rs (§8.1, §8.2) — wave 1, `gui` feature

```rust
pub struct IconSpec {
    pub abbr: String,                 // 1–2 chars, already validated
    pub account_color: [u8; 3],
    pub ring: Option<(f64, [u8; 3])>, // percent 0..=100+ and its LoadLevel colour; None = unknown → grey ring
    pub locked: bool,                 // lock badge bottom right
    pub monochrome: bool,             // white ring and disc outline, letters cut out
}
/// Straight (not premultiplied) RGBA, size_px × size_px, row-major — what tauri::image::Image::new_owned takes.
pub fn render(spec: &IconSpec, size_px: u32) -> Vec<u8>;
```

Drawing (tiny-skia, anti-aliased): transparent background; ring of width max(2, size/8) from 12 o'clock clockwise, filled part in the ring colour, rest grey `#555b66`; disc inside in the account colour; letters centred in white or `#0f1115`, whichever has the higher WCAG contrast against the disc; font `crates/app/assets/DejaVuSans-Bold.ttf` via `include_bytes!` and `ab_glyph`. One letter is drawn larger than two. If the measured text is wider than the disc's inner width, squeeze it horizontally (scale x only) before ever making it smaller; the font height never goes below 9 px at size ≥ 16, and at smaller sizes is as large as fits. Percent above 100 draws a full ring.

## app: GUI (§8, §9, §11) — wave 3, `gui` feature

Files: `crates/app/src/gui/{mod.rs, worker.rs, tray.rs, panel.rs, notify.rs, autostart.rs, ipc.rs}`, `crates/app/ui/{index.html, panel.css, panel.js}`, `crates/app/capabilities/default.json`. `main.rs`: no subcommand → `gui::run(app: AppPaths, claude_override: Option<PathBuf>, data_override: Option<PathBuf>) -> ExitCode` (the same path resolution as the CLI, one function; the overrides are passed on so `seatswap panel` and autostart start the app with the same flags). Actions both front ends must take the same way live in `crates/app/src/ops.rs`: `save_live_login(claude, store, label, now) -> Result<Account, String>` (a missing, ended or unknown login saved nothing and is an error), `switch_message(store, account_id) -> Result<String, String>` ("Switched to AC — Acme"; an unreadable store or unknown id is an error, never the raw id) and `switched_without_list(error) -> String`.

**Threads.**
- *Main thread:* Tauri event loop, tray, menu, panel window.
- *Engine thread:* owns `Engine<Api>` (its own `Store` from `store::open_store`). Loop on a command channel with a 30 s tick: `Poll(PollReason)`, `Publish`, `Shutdown`. Each tick polls with `Timer` (the engine decides what is due). Wake detection: when the wall clock advanced more than 90 s beyond the monotonic clock between two ticks → `Poll(Wake)`. After each poll or `Publish` it builds a `Snapshot` and hands it to the main thread.
- *Action thread:* owns a second `Store` (from `open_store`). Runs switch / save current / edit / remove / save settings one at a time, each replying through a one-shot channel, then sends `Poll(AfterSwitch)` or `Publish` to the engine thread. Two stores are safe because every accounts.json change is load-modify-save under the app lock.
- *Watcher thread:* `watcher::watch`; its callback sends `SyncLive` to the action thread (sync_live when `mirrorLive`, else nothing), which then sends `Publish`.
- Tauri commands never block the main thread (`async` commands that wait on the reply with `tauri::async_runtime::spawn_blocking`).

**Snapshot** (serde camelCase; the panel's only data source; event `snapshot`, and command `get_snapshot`):

```rust
pub struct Snapshot {
    pub views: Vec<AccountView>,          // engine::views, active first
    pub active_id: Option<String>,
    pub live: LiveView,                   // { kind: "noLogin"|"ended"|"known"|"unknown", accountId?, email?, orgName? }
    pub suggestion: Option<Suggestion>,   // engine::suggestion (Serialize, camelCase)
    pub warnings: Vec<LoginWarning>,      // engine::login_warnings (Serialize, camelCase, tag "kind")
    pub pending: Option<PendingSwitch>,   // { targetId, targetLabel, email, orgName }: a switch waits for "save or discard the current login?"
    pub secrets_warning: Option<String>,  // from open_store: logins stored unencrypted
    pub last_poll_at: Option<DateTime<Utc>>,
    pub error: Option<String>,            // last engine-wide error (store/cache unreadable)
    pub settings: Option<Settings>,      // None when accounts.json cannot be read: the panel disables Save
}
```

**Tauri commands** (all `Result<T, String>`, the error is the human message of the core error):
`get_snapshot() -> Snapshot` · `refresh_now()` (Poll(Wake)) · `switch_account(key, unsaved: "refuse"|"discard"|"save", label: Option<String>) -> String` (the success sentence; `CurrentNotSaved` sets `pending` and returns Err "not saved") · `cancel_pending()` · `save_current(label: Option<String>)` (sync_live with create) · `edit_account(key, label?, abbr?, color?)` · `remove_account(key)` · `save_settings(settings: Settings)` (validated; applies startAtLogin) · `icon_preview(abbr, color) -> Vec<IconPx { size, rgba: Vec<u8> }>` for 16, 22, 32 (the panel draws them with `ImageData`) · `fit_panel(height: f64)` · `hide_panel()`.
Event `navigate` (payload `"main"|"add"|"settings"`) when the panel is shown for a menu item.

**Tray** (`tray.rs`, redrawn on every snapshot): icon from `icon::render` at 22 px ×2 (44 px image) for the active account — ring = `settings.iconLimit` limit (binding/session/week) with its `LoadLevel` colour, `locked` at ≥ 100 %, grey ring while the usage is unknown, a grey "?" disc when no account is live; tooltip as §8.1; tray label (`set_title`) per `trayText`. Linux backend: tray-icon's `ksni` (StatusNotifierItem over D-Bus), switched on in `crates/app/Cargo.toml` next to Tauri's libappindicator, because libappindicator reports no clicks and drops the tooltip; with ksni the item has `ItemIsMenu = false`, a left click arrives as `TrayIconEvent::Click(Left)` (opens the panel), a right click shows the menu, and the tooltip is published (checked on the session bus, 2026-11-10). Without a StatusNotifierWatcher on the bus (plain GNOME without the AppIndicator extension, or a desktop panel that starts after seatswap at login) tray-icon cannot create the item and has no option to wait; `tray::start` then keeps the app running without an icon, says so on stderr and in a notification (`seatswap panel` still opens the pop-up), and creates the icon when the name `org.kde.StatusNotifierWatcher` appears (`gio::bus_watch_name`, GTK's own gio). Once the item exists, ksni registers it again whenever the watcher restarts. Menu as §8.3: one item per account (`AC  Acme   97 % · 1h12m` + "✓" for the live one, "logged out" when so), clicking switches with `refuse`; then "Save current login (…)…" when live is unknown; "Open panel…", "Add account…", "Settings…", "Quit". Number formatting is shared with the CLI (one module, `crates/app/src/format.rs`).

**Panel** (`panel.rs`): one window label `panel`, created on first open (`index.html#<view>`), kept and hidden afterwards. 380 px wide, height from `fit_panel`, no decorations, always on top, skip taskbar. Placement §9.2: X11 at the mouse, inside the monitor that holds it, above the mouse when the mouse is in the lower half, else below; Wayland (`XDG_SESSION_TYPE=wayland`) centred. `show` takes an `Anchor { cursor, monitor, scale }` once per opening and keeps it with the last requested logical height in `GuiState.panel: Mutex<Placement>`; `fit_panel` resizes and re-places from that anchor (`anchored_position(anchor, height, wayland)` → `place_panel`), so the panel keeps its bottom edge above the mouse, its top edge below it, and stays inside the monitor. Never read the window's position or size back to place it: on X11 they report the old values until the window system has applied a change (the headless run of 2026-11-10 drifted the panel to y = 836 and y = −386 that way). The window is built `resizable(true)`: GTK ignores `set_size` on a non-resizable window. Closes on Esc and the header's × (panel.js → `hide_panel`), on focus loss, and on a left click on the tray icon while open (`panel::toggle`, decision in the pure `tray_click(visible, focus_hidden_ago)`; a click within `REOPEN_GUARD` = 400 ms after a focus-loss hide is the click that caused it and does not reopen). Focus loss hides only if the panel is still unfocused `FOCUS_RECHECK` = 150 ms later and no pop-up of the process holds a GTK grab (`own_popup_open` → `gtk::grab_get_current`): WebKitGTK shows a `<select>` list as its own window with a grab, so the panel is unfocused for as long as the list is open; the list closes on the next click outside and hands the focus back. Focus on X11 (`take_focus`, `focus_x11`): `present_with_time` + `gdk_window.focus` with the X server's current time (`gdkx11::x11_get_server_time`) — Cinnamon's window manager refuses `set_focus`, which asks without a timestamp; and on every map the request is repeated up to `FOCUS_RETRIES` = 5 times, `FOCUS_RETRY_GAP` = 80 ms apart, until the window is active, because a freshly mapped window is not yet managed and the first request is ignored (both seen on Cinnamon 2026-11-10). Built hidden at startup (`panel::preload` in `setup`) so the page has loaded and measured itself before the first opening. A hidden panel does not appear when `show` is called: `show` stores the anchor, sets `PanelState.pending_show` and sends `navigate`; the page answers every navigation with `fit_panel` (setView), and that call sizes, places and only then shows it (`reveal`: move, then `show`), so the window manager maps it once, at its final size and place. If no height arrives within `SHOW_DEADLINE` = 300 ms it appears anyway with a log line. On Linux, nothing may map the window before the move is applied (`present_with_time` maps through GTK at once, and the panel flashed at its old place); focus is asked from the map handler instead. `.panel` has `flex-shrink: 0`: before the first showing the viewport has no width and a shrinking panel measured 2698 px tall. Opening it sends `Poll(PanelOpened)`.

**Notifications** (`notify.rs`, notify-rust): after a switch when `notifyOnSwitch` ("Switched to P — Private"); the switch suggestion (§7.4) once per account and reset window, with a "Switch" action (`wait_for_action` on its own thread; click on the body opens the panel); login warnings once per account and level per run; an unknown live login once per identity per run ("New login found: <email>. Save it as an account?" → opens the panel).

**Single instance and `seatswap panel`** (`ipc.rs`): the tray app takes `acquire_file_lock(gui_lock, 0)`. Busy → another instance runs: send `open-panel main` to `gui_socket` and exit 0. Held → name the socket only now (the lock created the data folder on first start, and the name needs its real path), remove a leftover socket, listen on it (mode 0600); one line per connection: `open-panel <view>` → `ok`, anything else → `error unknown command`.

**Autostart** (`autostart.rs`): at start and on `save_settings`, `startAtLogin` on → write `autostart_file($XDG_CONFIG_HOME or ~/.config)` with `Exec=<current_exe>` via `write_atomic`; off → remove it if present.

**Panel UI** (`ui/`, plain HTML/CSS/JS, no build step, `window.__TAURI__` with `app.withGlobalTauri: true`): §11.1–11.3. Views `main`, `add` (the §6.1 steps; shows the unknown live login with a label field and "Save" when there is one), `settings` (every §10 setting). Every time on the page (reset countdowns, the suggestion's clock time, the header's poll age, a card's "usage 5 min ago") is an element with `data-time` + `data-time-style` whose text comes from the command `time_texts([{at, style}]) → [text]` (`countdown` → `format_reset_at`, `clock` → `format_reset_clock`, `age` → `format_age`), asked after each render and every second, so the page has no copy of these rules. Without `window.__TAURI__` it runs on a built-in fixture snapshot so it can be opened in a browser; there `time_texts` answers with stand-ins (the plain clock time, "a moment ago").
