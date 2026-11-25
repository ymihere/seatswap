# seatswap — implementation notes

The spec (`docs/spec.md`) says what the program does and why. This file fixes the module contracts, so slices built in parallel fit together. Signatures here are binding; bodies are free. Section numbers like §6.2 refer to the spec.

Done already (do not rewrite): `types.rs` (`Login`, `OAuthAccount`, `LOGIN_KEYS`), `paths.rs` (`ClaudePaths`, `AppPaths`), `atomic.rs` (`write_atomic`, `backup_once`).

Linux and macOS share the Unix code. The Windows port and its contracts are in `docs/windows.md`; the macOS Keychain login is in "live.rs on macOS" below.

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

## Files 1.2.0 reads (real shapes)

These are the other programs' files the Sessions area and the history read. seatswap only reads them;
the Windows paths are in `docs/windows.md` §14.

**A DeepSeek Harness session** is `<sessions>/<cwd-key>/<id>/session.v4.jsonl.zstd`, a zstd-compressed
JSON-lines file with an empty `session.lock` beside it. `<cwd-key>` is the cwd with `/` as `-`, wrapped
in `--`; the first line is the header and the rest are events, and `time` and `createdAt` are epoch
milliseconds. A worker has its own file and header, with `parentSession` and `delegationDepth` 1.
Values are made up:

```json
{"type":"session","version":4,"id":"flash-task-5c974a53-178a-4254-93b2-b804a111414a","createdAt":1794696054070,"cwd":"/home/sam/projects/atlas","isSeeded":false,"delegationDepth":0}
{"type":"turn/start","seq":3,"time":1794696054100,"data":{"turn":1}}
{"type":"assistant/message","seq":9,"time":1794696060000,"data":{"turn":1,"step":1,"message":{"role":"assistant","source":{"kind":"model","provider":"ollama","model":"deepseek-v4.1-flash:cloud"},"id":"m1"},"usage":{"inputTokens":10516,"outputTokens":3585,"totalTokens":14101}}}
{"type":"session/title","seq":4,"time":1794696054200,"data":{"title":"atlas link test"}}
{"type":"turn/end","seq":20,"time":1794696062000,"data":{"turn":1,"reason":{"kind":"completed"}}}
```

Other event types (`user/message`, `system/message`, `tool/call`, `tool/result`, `step/start`,
`step/end`, `request/header`, `request/context`, `subagent/catalog`, `agent/inbox/spliced`) are
skipped unread except for their `time`.

**A flash job report** is `<state>/flash-agents/<key>/patches/job-<id>.report.json`. Only its
`sessionId` (the job's `flash-task-…` id) is read; the keys `createdAt, jobId, key, kind, label,
report, sessionId, status, stopReason, timing` may all be present, and `label` and `report` are never
stored.

```json
{"createdAt":"2026-11-14T17:53:10.000Z","jobId":"job-3e32c2fc","key":"atlas","kind":"flash","label":"atlas link test","report":"…","sessionId":"flash-task-5c974a53-178a-4254-93b2-b804a111414a","status":"done","stopReason":"completed","timing":{"ms":17930}}
```

**A flash result in a Claude Code transcript** is the tool result of one of
`mcp__plugin_flash-agents_flash__flash_task`, `…__flash_result`, `…__flash_batch` or
`…__flash_status`. As JSON text it holds `"sessionId":"flash-task-<uuid>"`,
`"childSessionId":"<uuid>"`, `"route":{"provider":"ollama","model":"deepseek-v4.1-flash:cloud"}` and
`"usage":{"inputTokens":…,"outputTokens":…,"totalTokens":…,"modelCalls":…,"cacheReadTokens":…}`. A
background start returns only `{"jobId":"job-3e32c2fc",…}`. The reader looks for the id strings in
the result's text, not for its keys:

```json
{"sessionId":"flash-task-5c974a53-178a-4254-93b2-b804a111414a","childSessionId":"a25abffd-ed93-4296-868f-dbd77aca32ac","route":{"provider":"ollama","model":"deepseek-v4.1-flash:cloud"},"usage":{"inputTokens":10516,"outputTokens":3585,"totalTokens":14101,"modelCalls":2,"cacheReadTokens":0}}
```

**A Codex rollout** is `<codex_home>/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl`, one JSON object
per line with `timestamp`, `ordinal`, `type` and `payload`. Values are made up:

```json
{"timestamp":"2026-11-14T17:53:10.120Z","ordinal":0,"type":"session_meta","payload":{"id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","session_id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","cwd":"/home/sam/projects/atlas","originator":"codex-tui","source":"cli","cli_version":"1.4.0","model_provider":"openai"}}
{"timestamp":"2026-11-14T17:53:11.000Z","ordinal":1,"type":"turn_context","payload":{"turn_id":"t1","cwd":"/home/sam/projects/atlas","model":"gpt-6.1-sol","effort":"high"}}
{"timestamp":"2026-11-14T17:54:00.000Z","ordinal":2,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12500,"cached_input_tokens":4000,"cache_write_input_tokens":0,"output_tokens":900,"reasoning_output_tokens":200,"total_tokens":13400},"last_token_usage":{"input_tokens":12500,"cached_input_tokens":4000,"cache_write_input_tokens":0,"output_tokens":900,"reasoning_output_tokens":200,"total_tokens":13400}},"rate_limits":{"primary":{"used_percent":12,"window_minutes":300,"resets_at":1794699600}}}}
{"timestamp":"2026-11-14T17:54:01.000Z","ordinal":3,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1","started_at":1794696060}}
{"timestamp":"2026-11-14T17:55:00.000Z","ordinal":4,"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","completed_at":1794696120,"duration_ms":60000}}
```

`token_usage_record` lines carry `payload.usage`, `payload.turn_token_usage` and
`payload.thread_token_usage` with the same token fields; they are not read. Only the `event_msg`
line whose `payload.type` is `token_count` is a token record.

**Ollama's `/api/usage?range=24h`** answer: `totals.{request_count, usage_usd, input_tokens,
cached_input_tokens, output_tokens}`, and one bucket per whole UTC hour in
`buckets[].{from, until, request_count, usage_usd, input_tokens, cached_input_tokens, output_tokens}`.

**Dropbox's `info.json`**: `personal.path` is the Dropbox folder, `business.path` its business one,
and either may be absent; `personal.path` wins.

```json
{"personal":{"path":"/home/sam/Dropbox","host":123456789,"is_team":false,"subscription_type":"Basic"}}
```

---

## atomic.rs — files, modes and private folders

The one way seatswap writes a file, and the only module that knows about file modes. No other module imports `std::os::unix` outside tests.

```rust
/// Who may read a file seatswap creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access { Private /* Unix 0600 */, Shared /* Unix 0644 */ }

/// Temp file in the same folder, write, fsync, rename over `path`, fsync the folder (Unix).
/// Unix: an existing file keeps its mode, a new one gets the mode of `access_if_new`.
/// Windows: no modes (a file gets its folder's permissions), no folder fsync (Windows cannot
/// open a folder as a file), and the rename is retried while it fails with OS error 5 or 32
/// (another program holds the target open without delete sharing): 10 ms, doubling up to
/// 200 ms, for at most 2 s, then the last OS error. Any error removes the temp file.
pub fn write_atomic(path: &Path, bytes: &[u8], access_if_new: Access) -> io::Result<()>;

/// Append `line` plus '\n' as one write to `path` (created with `access_if_new` when
/// missing), then fsync. `line` must not contain '\n' and, with the newline, must fit
/// in MAX_LINE_BYTES: otherwise io::ErrorKind::InvalidInput.
pub const MAX_LINE_BYTES: usize = 4096;
pub fn append_line(path: &Path, line: &str, access_if_new: Access) -> io::Result<()>;
/// Append every line plus '\n' as one write, after checking every line; a rejected line leaves the
/// file unchanged. `append_line` is this with one line.
pub fn append_lines<S: AsRef<str>>(path: &Path, lines: &[S], access_if_new: Access) -> io::Result<()>;

/// Copy `path` to `<path>.seatswap-backup` (Access::Private) unless that backup exists.
pub fn backup_once(path: &Path) -> io::Result<bool>;

/// Missing: create with its parents (Unix: mode 0700, set explicitly). Existing folder: left
/// as it is. Something else in its place: io::ErrorKind::AlreadyExists, "exists but is not a folder".
pub fn ensure_private_dir(dir: &Path) -> io::Result<()>;

/// Unix: chmod 0700. Windows: nothing to set, Ok(()).
pub fn restrict_dir(dir: &Path) -> io::Result<()>;

/// Read + write, create when missing, never truncate; a new file is private (Unix: 0600).
pub fn open_private(path: &Path) -> io::Result<File>;
```

Every file seatswap writes passes `Access::Private`, except the XDG autostart entry (`Access::Shared`). The data folder (store, usage cache, lock files) is made with `ensure_private_dir`; the fallback secrets folder also gets `restrict_dir`.

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

/// The six specs in the order Claude Code takes them: the renewal locks first, because
/// Claude Code saves a renewed login inside its renewal lock:
/// refresh_lock 60 s, legacy_refresh_lock 60 s, design_refresh_lock 60 s (designOauth),
/// cloud_sessions_refresh_lock 60 s (cloudSessionsOauth), storage_write_lock 15 s,
/// identity_lock 10 s.
pub fn claude_lock_specs(paths: &ClaudePaths) -> [LockSpec; 6];

/// All six, in that order, sharing one deadline (now + timeout). On failure the ones
/// already taken are released before returning the error. Dropping ClaudeLocks releases
/// them in reverse order.
pub struct ClaudeLocks { /* Vec<DirLock> */ }
pub fn acquire_claude_locks(paths: &ClaudePaths, timeout: Duration) -> Result<ClaudeLocks, LockError>;

/// True when any of the six exists and is not stale. The watcher reads only when false.
pub fn claude_locks_busy(paths: &ClaudePaths) -> bool;

/// seatswap's own exclusive lock on <data_dir>/switch.lock (fs4 file lock, the file
/// is created if missing with atomic::open_private, the data folder too with
/// atomic::ensure_private_dir). Poll try_lock until timeout, then Busy.
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
/// via atomic::write_atomic (Access::Private if new). Call atomic::backup_once first.
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
    pub auto_burn: bool,                // "Auto-burn may use this account", default true
}

#[derive(… Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum LoginState { Ok, LoggedOut }

/// Settings §10 (no clearOrgCaches). Defaults: tray_text Percent, icon_limit Binding,
/// icon_text Letters,
/// poll_active_secs 120, poll_inactive_secs 900, auto_burn false, suggest_switches true,
/// notify_on_switch true, mirror_live true, refresh_inactive true, hotkey "Ctrl+Alt+C",
/// start_at_login true, theme System, claude_config_dir None, history_dir None,
/// host_name None, read_sessions true, notify_usable_again true, notify_waiting true,
/// notify_waiting_hosts ThisHost, agents_read true, agents_switch false,
/// statusline_feed true, statusline StatuslineConfig::current_line().
/// `#[serde(default)]` per field so an older file without a new setting still loads; the
/// removed suggest_at and suggest_target_below are unknown keys and are ignored.
pub struct Settings { … }  // camelCase in the file
// step A's fields:
pub history_dir: Option<PathBuf>,       // None = <data>/history; a relative path is refused
pub host_name: Option<String>,          // None = the machine's host name; validated by paths::host_name
pub read_sessions: bool,
pub notify_usable_again: bool,
pub notify_waiting: bool,
pub notify_waiting_hosts: WaitingHosts,
pub agents_read: bool,
pub agents_switch: bool,
// step B's fields:
pub auto_burn: bool,                    // default false; on: switch by itself
pub suggest_switches: bool,             // default true; used only while auto_burn is off
pub statusline_feed: bool,              // default true; off: statusline writes no feed.json
pub statusline: StatuslineConfig,       // default current_line(); see "statusline — layout types"
#[serde(rename_all = "camelCase")] pub enum WaitingHosts { ThisHost, AllHosts }
pub enum TrayText { Percent, PercentReset, Off }
pub enum IconLimit { Binding, Session, Week }
pub enum IconText { Letters, Percent }  // what the icon's disc shows, spec §8.1
pub enum Theme { System, Light, Dark }
impl Settings { pub fn validate(&self) -> Result<(), StoreError>; } // percents 1..=100, polls >= 30 s,
// an absolute claudeConfigDir, an absolute historyDir, a hostName that passes paths::host_name,
// and validate_statusline (below)

pub struct AccountsFile { pub version: u32, pub accounts: Vec<Account>,
    #[serde(default)] pub provider_subscriptions: Vec<ProviderSubscription>,
    pub settings: Settings,
    #[serde(default)] pub secrets_backend: Option<SecretsBackend> }
// version must be 1; anything else is UnsupportedVersion, never silently upgraded.

/// An OpenAI, Ollama or Google subscription (R4). `account` is the provider's `AccountRef`; its look and
/// price are the account's own fields. Serde camelCase; company is derived, never stored.
pub struct ProviderSubscription { pub account: AccountRef, pub label: String, pub abbr: String,
    pub color: String, #[serde(default)] pub plan: Option<String>,
    #[serde(default)] pub price: Option<f64>, #[serde(default)] pub price_currency: Option<String> }
impl ProviderSubscription { pub fn company(&self) -> Company; } // Company::of_account

pub trait SecretStore: Send + Sync {
    fn get_value(&self, id: &str) -> Result<Option<Value>, StoreError>;
    fn set_value(&self, id: &str, value: &Value) -> Result<(), StoreError>;
    fn delete(&self, id: &str) -> Result<(), StoreError>;   // missing is fine
    fn encrypted(&self) -> bool;                             // false → UI warns (§5)
    // Provided, through the value methods:
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError>;
    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError>;
    fn get_codex(&self, id: &str) -> Result<Option<CodexLogin>, StoreError>;
    fn set_codex(&self, id: &str, login: &CodexLogin) -> Result<(), StoreError>;
}
pub struct KeyringSecrets;            // keyring crate, service "seatswap", user = account id, JSON value
pub struct FileSecrets { dir: PathBuf } // <data>/secrets/<id>.json; folder 0700, files 0600, via write_atomic
impl FileSecrets { pub fn new(dir: PathBuf) -> FileSecrets; }
/// Windows only (W-1): `get_value`/`set_value` and every other method go to `main`, the recorded
/// backend, but `get_codex`/`set_codex` go to `codex`, a `FileSecrets` in the same `secrets`
/// folder. A Codex login is larger than one Credential Manager entry (2,560 bytes), so its copies
/// are private files even while Claude logins stay in the keyring; `delete` tries both. On other
/// systems `keyring_store` returns `KeyringSecrets` unchanged.
pub struct RoutedSecrets { pub main: Box<dyn SecretStore>, pub codex: FileSecrets }
impl SecretStore for RoutedSecrets { /* value methods → main; codex methods → codex */ }
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
/// ("logins are stored unencrypted in <dir>") — shown by the UI every time (§5). On Windows the
/// keyring backend is wrapped in `RoutedSecrets`, so Codex logins are files either way (W-1).
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
    pub fn provider_subscription(&self, account: &AccountRef) -> Option<&ProviderSubscription>;
    /// Create or update a provider subscription; a new one's look is "OpenAI"/`OA`/`#db2777`
    /// or "Ollama"/`OL`/`#ea580c`, its abbr the base or the base's first letter with the first
    /// free digit 2–9 (`O2`, `O3`, …) when the base is taken, all through `validate_abbr`.
    /// Returns whether it was created, and the warnings (a plan that changed).
    pub fn ensure_provider_subscription(&mut self, account: &AccountRef, plan: Option<&str>)
        -> (bool, Vec<String>);
}

pub const PALETTE: [&str; 10] = ["#3b82f6", "#f59e0b", "#10b981", "#ef4444", "#8b5cf6",
                                 "#ec4899", "#14b8a6", "#f97316", "#6366f1", "#84cc16"];
/// "Acme Team" → "AT", "Acme" → "AC", "Private" → "PR", "x" → "X".
/// Letters/digits only; words split on whitespace and punctuation. No letter or digit → InvalidLabel.
pub fn default_abbr(label: &str) -> Result<String, StoreError>;
/// Trim, upper-case, 1–2 of A–Z0–9, not used by another account or provider subscription
/// (`others` = their abbrs).
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
pub struct Usage { pub limits: Vec<Limit>, pub extra_usage_enabled: bool,
    #[serde(skip)] pub warnings: Vec<String> }   // one sentence per unreadable reset time; the engine puts them in last_error
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
impl Limit {
    /// Whether this limit blocks at `now` (spec §2.7, §7.2): at 100 % or more and, for a session
    /// limit, its reset is after `now`. A week is read as 0 % after its reset by the callers.
    pub fn is_full_at(&self, now: DateTime<Utc>) -> bool;   // reaches_at(100.0, now)
    /// At `percent` or more and, for a session limit, its reset is after `now`: auto-burn's leave
    /// rule asks it with WINDOW_LEAVE_PERCENT.
    pub fn reaches_at(&self, percent: f64, now: DateTime<Utc>) -> bool;
    pub fn level(&self) -> LoadLevel;
}
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
/// and Synced::{Unchanged, Updated, Added}: Added only when this call created the account (the
/// caller that writes the history uses it for `accountAdded`; every other caller tests
/// `!= Synced::Unchanged`).
pub enum Synced { Unchanged, Updated, Added }
pub fn sync_live(claude: &ClaudePaths, store: &Store, create_unknown: Option<Option<String>>,
                 now: DateTime<Utc>) -> Result<(LiveState, Synced), SwitchError>;
// create_unknown: None = never create; Some(label) = create with that label (None = default label).

/// Remove an account from the list and its login from the secrets. Never touches Claude's files.
/// The CLI and the GUI call ops::remove_account instead, which adds the `accountRemoved` line.
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
    /// The login state the history last has for this account (loginEnded / loginRenewed).
    /// Serde default: an old cache loads with None and the first poll seeds it without a line.
    pub recorded_login: Option<LoginState>,
}
/// The allBlocked / anyUsable state the history last has. Serde default: an old cache loads with
/// None, so the first poll may write AllBlocked. It moves only when its event was written.
pub struct RecordedUsable { pub all_blocked: bool, pub any_usable: bool }
#[derive(… Serialize, Deserialize)] pub struct UsageCache { pub version: u32 /* 1 */, pub entries: BTreeMap<String, CacheEntry>,
    #[serde(default)] pub recorded_usable: Option<RecordedUsable> }
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
    pub auto_burn: bool,                      // "Auto-burn may use this account"
    pub is_active: bool,
    pub login_state: LoginState,
    pub login_ends_at_ms: Option<i64>,
    pub login_days_left: Option<i64>,         // floor((ends - now) / 1 day), may be negative
    pub usage: Option<Usage>,
    pub usable: bool,                         // login ok, usage known, no blocking limit
    pub blocked_until: Option<DateTime<Utc>>,
    #[serde(skip)] pub blocker: Option<Blocker>,
    pub fetched_at: Option<DateTime<Utc>>,    // the later of the poll and the status line's feed
    pub last_error: Option<String>,
}

// Step B's suggestion is `burn_text::SwitchSuggestion { to, text }`, built from the decision and the
// plan it computes itself (`follow(inputs, Policy::WhenFull)`). It replaces the fixed `Suggestion`
// struct and `engine::suggestion`, which are gone.

/// §6.3: accounts whose login ends within 5 days (days_left 5..=2 → Soon, ≤ 1 → Tomorrow, < 0 → Ended).
pub enum LoginWarning { Soon { id: String, days_left: i64 }, Tomorrow { id: String }, Ended { id: String } }
pub fn login_warnings(views: &[AccountView]) -> Vec<LoginWarning>;
```

## live.rs on macOS: the Keychain login (§2.2)

On macOS `ClaudePaths::resolve` sets `login_keychain: Some(KeychainItem)`, and `read_login` /
`write_login` then read and write that item instead of `credentials_file`, with the same JSON
merge (unknown keys kept, `LOGIN_KEYS` removed then inserted, corrupt data never overwritten).
`for_config_dir` and `resolve_with` always give `None`, so every test keeps the file.

```rust
// paths.rs
pub struct KeychainItem { pub service: String, pub account: String, pub keychain: Option<PathBuf> }
impl KeychainItem {
    /// "Claude Code-credentials" for home/.claude, else "Claude Code-credentials-" + the first
    /// 8 hex digits of SHA-256(config_dir as given). keychain None = the default keychain.
    pub fn for_config_dir(config_dir: &Path, home: &Path, user: &str) -> KeychainItem;
}
pub const LOGIN_STORE_VARIABLE: &str = "SEATSWAP_CLAUDE_LOGIN"; // unset | "file" | "keychain" (macOS only)
// PathsError::NoUser ($USER missing), PathsError::LoginStore(String) (any other value)
```

- `live/keychain.rs` runs `/usr/bin/security` (Claude Code's own program, so its access list
  covers seatswap): `find-generic-password -a -s -w` to read (exit 44 = no item; hex output is
  decoded), `add-generic-password -U -a -s -X <hex>` to write. The write line goes to
  `security -i` on stdin when it is at most `INTERACTIVE_LINE_LIMIT` (4032) bytes; `security -i`
  splits longer lines (measured: 4096), so a longer value is passed on argv, as Claude Code does.
  Names holding `"`, `\` or a line break are refused; a named keychain must be an existing absolute
  file, because `security` quietly uses the default keychain for a missing one.
- The backup is the item `<service>.seatswap-backup`, written once before the first write.
- Tests never reach the default keychain: core tests name a throw-away keychain made with
  `security create-keychain` in a tempdir; the app's `cli::resolve_paths` drops the item under
  `cfg(test)`; the CLI integration tests run the program with `SEATSWAP_CLAUDE_LOGIN=file`.

## watcher.rs (§6.4) — wave 3

Tells the GUI that the live login may have changed. It does not read or interpret the files: the
callback calls `switcher::sync_live` (mirrorLive on) or `switcher::live_state` (off).

```rust
pub const DEBOUNCE: Duration = Duration::from_millis(500);
/// A burst in a `watch_folder` folder fires at the latest this long after its first change, even
/// while changes keep arriving; `watch` has no such bound (a login written many times is one change).
pub const DEBOUNCE_MAX: Duration = Duration::from_secs(1);
pub const BUSY_RECHECK: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum WatchError { Notify(#[from] notify::Error), Io { path: PathBuf, source: std::io::Error } }

/// Watches the folder holding credentials_file and the folder holding identity_file (non-recursive:
/// Claude Code and seatswap replace the files by rename, so the folders are watched, not the
/// files; one watch when both are the same folder), plus the parent of each existing folder, where
/// its removal shows up (on Windows a removed folder's own watch ends without an event). Events
/// for other paths are ignored (a file of the same name in a parent is not the login). After a
/// relevant event: wait until DEBOUNCE passes without another one, then until
/// locks::claude_locks_busy() is false (re-checked every BUSY_RECHECK), then call `on_change` once.
/// The callback runs on the watcher's own thread, never concurrently with itself. A missing folder
/// is not an error: Claude Code has not been set up (WatchState::NotSetUp); its nearest existing
/// parent is watched, and when the folder appears the watch moves to it and `on_change` is called;
/// a removed folder moves the watch back up. No folder is created. A path that exists but is not a
/// folder, or cannot be read, is an error (at run time WatchState::Failed(text), shown as
/// "file watcher: …"). Drop stops the watching and joins the thread.
pub enum WatchState { Watching, NotSetUp, Failed(String) }
pub struct Watcher { /* … */ }
impl Watcher { pub fn state(&self) -> WatchState; }
// gui: apply_snapshot(Shown::New(snapshot) | Shown::Again) adds the start-up errors, the watch
// state and `claudeNotSetUp` only to a new snapshot, never to the stored one shown again.
pub fn watch(claude: &ClaudePaths, on_change: impl Fn() + Send + 'static) -> Result<Watcher, WatchError>;
/// One folder, non-recursive, read access ignored: `on_change` after DEBOUNCE quiet, or at the
/// latest DEBOUNCE_MAX after a burst's first change (a transcript folder written every 200 ms
/// still wakes the sessions thread). No lock wait.
pub struct FolderWatch { /* … */ }
pub fn watch_folder(folder: &Path, on_change: impl Fn() + Send + 'static) -> Result<FolderWatch, WatchError>;
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

## paths.rs additions — step A

```rust
impl ClaudePaths {
    pub fn sessions_dir(&self) -> PathBuf;    // <config_dir>/sessions
    pub fn projects_dir(&self) -> PathBuf;    // <config_dir>/projects
    pub fn skills_dir(&self) -> PathBuf;      // <config_dir>/skills
}
impl AppPaths {
    pub fn history_dir(&self) -> PathBuf;          // <data_dir>/history
    pub fn machine_id_file(&self) -> PathBuf;      // <data_dir>/machine-id
    pub fn transcripts_state_dir(&self) -> PathBuf; // <data_dir>/transcripts-state
    pub fn notification_icon(&self) -> PathBuf;    // <data_dir>/notification-icon.png
}
/// The one resolver for the Codex folder, the Harness sessions folder and the flash-agents state
/// folder: a named folder (Codex's setting, or an environment variable) must be absolute;
/// a relative one is an error naming the variable and its value, never resolved against the
/// current folder. Codex: `codexHome` or `$CODEX_HOME`, else `~/.codex`. Harness:
/// `$DSH_HOME/sessions`, else `~/.dsh/sessions`; flash-agents (its own rule, `durableBaseDir`):
/// `$FLASH_DATA_DIR` as it is, else `%LOCALAPPDATA%\flash-agents` (or `<home>\AppData\Local\
/// flash-agents`) on Windows, `<home>/Library/Application Support/flash-agents` on macOS,
/// `$XDG_STATE_HOME/flash-agents` or `~/.local/state/flash-agents` elsewhere. The platform's rule
/// is chosen by a private `Platform::CURRENT`, so every platform's rule is tested on any machine.
pub struct HarnessEnv { pub dsh_home: Option<OsString>, pub flash_data_dir: Option<OsString>,
    pub xdg_state_home: Option<OsString>, pub local_app_data: Option<OsString> }  // Default
pub fn harness_folders(env: &HarnessEnv, home: &Path) -> Result<(PathBuf, PathBuf), PathsError>;
// PathsError gains RelativeHarnessFolder { variable, value }: "${variable} must be an absolute
// path, got {value}". Codex keeps RelativeCodexHome for codexHome and $CODEX_HOME.
/// A `historyDir` must be absolute; a relative one is PathsError::HistoryDir, never resolved
/// against the current folder.
pub fn check_history_dir(path: &Path) -> Result<(), PathsError>;
/// The explicit absolute historyDir, else app.history_dir().
pub fn history_root(explicit: Option<&Path>, app: &AppPaths) -> Result<PathBuf, PathsError>;
/// The name this host writes its history under: `explicit` if set, else the machine's host name
/// (Linux: /proc/sys/kernel/hostname; Windows: %COMPUTERNAME%). 1..=63 characters of
/// [A-Za-z0-9._-], starting with a letter or a digit; anything else is PathsError::HostName.
/// The result is always in lower case.
pub fn host_name(explicit: Option<&str>) -> Result<String, PathsError>;
/// The first `<folder>/<name>` in `path_var` that is a program: a file on Windows, a file with an
/// execute bit on Unix. Only the value passed in is read, never the process environment; an empty
/// or relative `PATH` entry is skipped, and no shell or `which` is called. The provider thread
/// reads `PATH` once at start and passes it here (R6); `ollama.exe` is looked for on Windows.
pub fn program_on_path(name: &str, path_var: &OsStr) -> Option<PathBuf>;
/// The Linux menu entry and its icon (D-1): `$XDG_DATA_HOME/applications/seatswap.desktop` and
/// `$XDG_DATA_HOME/icons/hicolor/256x256/apps/seatswap.png`.
pub fn applications_entry(xdg_data_home: &Path) -> PathBuf;
pub fn app_icon(xdg_data_home: &Path) -> PathBuf;
```

## history — records, folder, append, read (step A)

One subfolder per host, one JSON Lines file per kind and month; nothing else opens these files.

```rust
pub enum HistoryError { Io { path, source }, Json { path, line, source }, HostTaken { host, folder, machine_id },
                        Paths(PathsError), LineTooLong { kind }, NotFinite { kind } }
pub enum Kind { Samples, Windows, Events, Tokens, Sessions, Up, Projects, Ollama }
// stems: samples windows events tokens sessions up projects ollama
pub trait Record: Serialize + DeserializeOwned { const KIND: Kind; fn at(&self) -> DateTime<Utc>;
    fn is_claude(&self) -> bool { true } }                  // see step D

pub struct AccountRef { pub acc: String, pub org: String }        // accountUuid and organizationUuid
impl AccountRef { pub fn of(account: &Account) -> AccountRef;
                  pub fn is_claude(&self) -> bool; }              // its company is Anthropic
pub enum Source { Poll, Statusline }
pub struct LimitSample { pub k: LimitKind, pub m: Option<String>, pub p: f64, pub r: Option<DateTime<Utc>>, pub b: bool }
pub struct Sample { pub t: DateTime<Utc>, #[serde(flatten)] pub account: AccountRef, pub src: Source, pub l: Vec<LimitSample> }
pub struct WindowRecord { #[serde(flatten)] pub account: AccountRef, pub k: LimitKind, pub m: Option<String>,
    pub start: Option<DateTime<Utc>>, pub end: DateTime<Utc>, #[serde(rename = "final")] pub final_percent: f64,
    pub lost: Option<f64>, #[serde(rename = "lastSample")] pub last_sample: DateTime<Utc>,
    pub plan: Option<String>, pub uncertain: bool, pub reset: bool, pub from: Option<f64> }
impl WindowRecord { pub fn weekly_start(&self) -> DateTime<Utc>; } // start, else end − 7 d
pub enum By { User, Cli, Auto, Agent, External }
pub struct SessionCounts { pub running: u32, pub busy: u32, pub tied: Option<u32> }
// `tied` (sessions on a model whose thinking is tied to the account, spec §7.4.2) is the count the
// sessions thread reports; it is written only when known and left out of the line when None. An old
// line with "tied":0 still reads.
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum EventKind {
    Switch { from, to, by, reason, cwd, sessions, waited_secs },
    /// A switch between two saved Codex logins (1.3, R8): the `switch` event's field shapes, its
    /// own type. A `switch` stays Claude-only; every timeline, rate and switch count reads
    /// `Switch` only, and `read_claude` leaves a `providerSwitch` out too.
    ProviderSwitch { from: Option<AccountRef>, to: AccountRef, by: By },
    Blocked { account, k, m }, Unblocked { account, k, m }, AllBlocked, AnyUsable,
    AccountAdded { account, plan }, AccountRemoved { account }, PlanChanged { account, from, to },
    PriceChanged { account, amount, currency },
    LoginEnded { account }, LoginRenewed { account }, Live { account },
    AgentCall { command, cwd, allowed },
    Waited { session, project, secs },
    AutoBurn { on, paused_until }, AutoBurnSet { on },
    AccountLook { account, label, abbr, color, auto_burn },  // absent fields skipped (1.2.0, S-5)
    SettingSet { key, value: serde_json::Value },              // one shared setting (1.2.0, S-5)
}
pub struct Event { pub t: DateTime<Utc>, #[serde(flatten)] pub kind: EventKind }
#[serde(rename_all = "camelCase")]
pub struct TokenRecord { pub t: DateTime<Utc>,
    #[serde(rename = "min", default = "sixty", skip_serializing_if = "is_sixty")] pub minutes: u32,
    pub session: String, pub cwd: String, pub project: ProjectKey,
    pub model: String, #[serde(rename = "in")] pub input: u64, pub out: u64, pub cw: u64, pub cr: u64,
    pub answers: u32, pub agent_answers: u32 }                // agentAnswers in the file
impl TokenRecord { pub fn span(&self) -> chrono::Duration; }    // its minutes; a line without `min` is 60
#[serde(rename_all = "camelCase")]
pub struct SessionRecord { pub t: DateTime<Utc>, pub session: String, pub cwd: String, pub project: ProjectKey,
    pub entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub title: Option<String>,
    pub first: DateTime<Utc>, pub last: DateTime<Utc>,
    pub agents_started: u32, pub agents_finished: u32, pub closed: bool }  // agentsStarted, agentsFinished
// `title` is Claude Code's own title of the session (`SessionSeen.title`); absent in a 1.1.0 record
// means none. A changed title makes the record due like a changed `last` (`same_summary`).
pub struct UpRecord { pub t: DateTime<Utc> }
/// One hour of Ollama's own usage, one record per `/api/usage?range=24h` bucket. `hour` is the
/// bucket's `from`, always on a whole UTC hour one hour before its `until`.
#[serde(rename_all = "camelCase")]
pub struct OllamaHourRecord { pub hour: DateTime<Utc>, pub requests: u64, pub input: u64,
    pub cached_input: u64, pub output: u64, pub usd: f64 }
impl Record for OllamaHourRecord { const KIND: Kind = Kind::Ollama; }   // is_claude() = false

pub struct HistoryDir { /* root, host */ }
impl HistoryDir {
    /// Root = settings.history_dir or app.history_dir(); host = paths::host_name(settings.host_name).
    /// Creates <root>/<host>/ and its host.json {"machineId": "<uuid>"} (the id of app.machine_id_file(),
    /// created on first use); a host.json naming another machine is HostTaken.
    pub fn open(app: &AppPaths, settings: &Settings) -> Result<HistoryDir, HistoryError>;
    pub fn at(root: &Path, host: &str) -> HistoryDir;            // tests: no marker check
    pub fn host(&self) -> &str;
    pub fn root(&self) -> &Path;
    pub fn hosts(&self) -> Result<Vec<String>, HistoryError>;    // subfolders holding host.json, sorted
    pub fn append<R: Record>(&self, record: &R) -> Result<(), HistoryError>;
    /// Several records; all lines of one month file with one append, every line checked first.
    pub fn append_many<R: Record>(&self, records: &[R]) -> Result<(), HistoryError>;
    pub fn read<R: Record>(&self, range: &Range, hosts: &Hosts) -> Result<Read<R>, HistoryError>;
}
pub struct Range { pub from: DateTime<Utc>, pub to: DateTime<Utc> }   // from inclusive, to exclusive
pub enum Hosts { All, One(String) }
pub struct Hosted<R> { pub host: String, pub record: R }
pub struct Read<R> { pub records: Vec<Hosted<R>>, pub warnings: Vec<String> }
pub fn reset_minute(resets_at: DateTime<Utc>) -> DateTime<Utc>;       // round up to the full minute
```

Rules: a record goes to `<root>/<own host>/<stem>-YYYY-MM.jsonl` by its `at()` in UTC. A line,
with its newline, over `atomic::MAX_LINE_BYTES` is `LineTooLong` before the folder or file is made;
a record holding a non-finite number is `NotFinite`. Optional fields are omitted, never `null`.
`read` opens only the month files that overlap the range, skips a torn line or a line of a shape
this version does not know with a warning, and makes other broken lines an error only when they are
more than 1 % of a file's lines. `Hosts::All` is every folder with a `host.json` plus this dir's own
host, merged by `at()` and then host.

**Who writes which record.** The CLI writes the records that depend only on the command it just ran:
`switch`, `agentCall`, `accountAdded` and `accountRemoved`. Records that depend on what was written
before — samples, windows, blocked/unblocked, allBlocked/anyUsable, loginEnded/loginRenewed — are
written only by the tray app's engine. A second process's recorder does not see the other's lines and
would write a window or a blocked event twice. So `status --refresh` and `list --refresh` build their
engine without a recorder, and a CLI engine never moves `CacheEntry.recorded_login`.

```rust
pub struct Recorder { /* dir, per-account last line and remembered limits, written windows, last up,
                          read warnings */ }
impl Recorder {
    pub fn new(dir: HistoryDir) -> Recorder;
    pub fn dir(&self) -> &HistoryDir;
    /// One usage answer: appends a Sample when a limit or a reset time differs from the account's
    /// last written line, or that line is at least 1 h old. Before it, writes every window that
    /// ended and every Blocked / Unblocked crossing; returns the events written. The account's
    /// state is read from this host's files on first use.
    pub fn reading(&mut self, account: &AccountRef, plan: Option<&str>, usage: &Usage,
                   at: DateTime<Utc>, src: Source) -> Result<Vec<EventKind>, HistoryError>;
    pub fn event(&self, at: DateTime<Utc>, kind: EventKind) -> Result<(), HistoryError>;
    pub fn up(&mut self, at: DateTime<Utc>) -> Result<(), HistoryError>;   // at most once per 5 min
    pub fn tokens(&self, records: &[TokenRecord]) -> Result<(), HistoryError>;
                                    // all lines of one month file with one append
    pub fn session(&self, record: &SessionRecord) -> Result<(), HistoryError>;
    /// Ollama's buckets: one line per hour only when every count of the new answer is at least
    /// the hour's last line and at least one is larger. An answer with a smaller count is an
    /// older answer: it is not written and is named once per hour through `take_warnings`. The
    /// first use reads this host's `ollama` lines of the buckets' months and remembers the last
    /// line per hour, so an old bucket is not written again. Returns how many lines it appended.
    pub fn ollama_hours(&mut self, records: &[OllamaHourRecord]) -> Result<usize, HistoryError>;
    /// The warnings of the lines `reading` read back (a torn last line, a line of a shape this
    /// version does not know), each returned once; the app prints them, core never does.
    pub fn take_warnings(&mut self) -> Vec<String>;
}
pub fn sample_of(account: &AccountRef, usage: &Usage, at: DateTime<Utc>, src: Source) -> Sample;
/// A limit (matched by k and m) has ended when prev.r is Some(end) and next.t >= end, or next has
/// the limit with next.r > end + RESET_JITTER and next.r >= end + the kind's window length
/// - RESET_JITTER (7 d, 5 h; any later reset for a kind of unknown length): a reset that moves
/// later by less, as an unused Codex limit's does at every poll, is the same window going on.
/// A limit missing from next, or with r None, ends nothing.
/// end = prev.r, final = prev.p, lastSample = prev.t, uncertain = end - prev.t > 1 h;
/// start = end - 7 d for weekly kinds, end - 5 h for session, None otherwise;
/// lost = max(0, 100 - final) for weekly kinds, None otherwise. The drop at the reset time is
/// measured against the limit's last poll reading when the recorder has one.
pub fn closed_windows(prev: &Sample, next: &Sample, plan: Option<&str>) -> Vec<WindowRecord>;
/// The one early-reset rule, used by the recorder, the "usable again" notification and
/// auto-burn's learned rates: `previous - current >= RESET_DROP_POINTS` (1) and `at` is more than
/// `RESET_JITTER` (60 s) before `reset_at`. Only a poll's reading is the recorder's `previous`; a
/// status-line reading never moves a percent down. `reset_at` None is never a reset.
pub fn is_early_reset(previous: f64, current: f64, reset_at: Option<DateTime<Utc>>,
                      at: DateTime<Utc>) -> bool;
/// The one reset-jitter rule: |a - b| <= RESET_JITTER (60 s). The two one-sided comparisons in
/// `write.rs` use RESET_JITTER directly to keep their one-sided meaning.
pub const RESET_JITTER: i64;
pub fn same_reset(a: DateTime<Utc>, b: DateTime<Utc>) -> bool;
```

**A week with a reset.** A weekly window normally runs seven days from its reset time. When a reading
drops by the one rule `history::is_early_reset` — at least `RESET_DROP_POINTS` (1) more than
`RESET_JITTER` before that reset time — the window ended early: its record has `reset: true`, `end`
at the lower reading, and `lost: None`, because a reset gave the quota back and nothing expired. The
drop is measured against the newest **poll** reading of the limit, never a status-line reading: the
feed's merged percent only moves up, so it cannot show a reset. The next window begins at that
reading: its record has `start` at the reading and `from` = the reading's percent (absent when a
restart could not find that sample on this host, which makes the record `uncertain`). A window's own
use is `final − from`, with `from` 0 when absent; a week that holds two windows sums both, so its use
can pass 100 % and its expired money can be more than one week's price. A window that ends below its
start has no use, adds 0 to a week's total and is named once in the read's warnings. A pair of
readings that `is_early_reset` calls a reset is left out of `burn::rates`' gains and session gains:
the drop is not use.

**Ollama's hours.** An hour's bucket only grows while the hour runs, so one line per hour is enough:
every count (`requests`, `input`, `cached_input`, `output`, `usd`) of a later answer is at least the
stored line's and at least one is larger, and only then is a line written. An answer with a smaller
count anywhere is an older answer: the whole answer for that hour is dropped and the hour is named
once, through `take_warnings`; the hour updates again when every count is at least the stored one.
Readers take the **largest line per hour**, over all hosts and among one host's own lines alike, and
never sum two lines for one hour. An empty or unreadable `buckets` list is the reader's error, which
the app reports once per text; one bucket that is not on a whole hour, not exactly one hour long (the
running hour, `"partial": true`, ends at the answer's moment within its hour), or missing a whole
number is an error naming the bucket, and no hour is skipped and none becomes 0.

`HistoryDir::location(app, settings)` is the one rule for the folder and host name; `open` uses it
and creates the host folder. `first_where`, `finished_windows`, `read_claude` and
`finished_claude_windows` are the reads the pages and the valuer use; the last two keep only records
whose `is_claude()` is true, so Codex's and Ollama's readings, recorded beside Claude's with `org`
`"codex"` or `"ollama"`, never reach a Claude-only reader.

## project.rs — from a folder to the project's identity (step A)

```rust
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub enum ProjectKey {
    Repo(String),     // "github.com/acme/acme-kb": host/path of remote origin, lower-case host, no protocol/user/.git
    Folder(String),   // no remote: the repository folder name; outside a repository: first folder under home, or "~"
}
/// Walk up from cwd to the first ancestor with .git (a folder, or a file "gitdir: <path>" as in
/// worktrees and submodules: follow it to the config). Read [remote "origin"] url. Nothing is run.
pub fn identify(cwd: &Path, home: &Path) -> ProjectKey;
/// The same project for https, ssh:// and scp-like remotes; credentials before @ are dropped;
/// file:// or a plain path gives None.
pub fn normalize_remote(url: &str) -> Option<String>;
/// A Windows path starting with a lower-case drive letter gets it in upper case; everything else
/// comes back unchanged. sessions and transcripts pass every cwd they read through this.
pub fn canonical_cwd(cwd: &str) -> String;
```

## sessions.rs — Claude Code's session files (step A)

```rust
pub enum SessionsError { Io { path, source } }
pub enum SessionStatus { Busy, Idle, Waiting, Other(String) }
#[serde(rename_all = "camelCase")]
pub struct SessionFile { pub pid: u32, pub session_id: String, pub cwd: String, pub started_at: i64,
    pub proc_start: String, pub version: Option<String>, pub kind: Option<String>,
    pub entrypoint: Option<String>, pub name: Option<String>,
    pub messaging_socket_path: Option<String>, pub status: SessionStatus, pub status_updated_at: i64 }
pub trait Liveness { fn alive(&self, file: &SessionFile) -> bool; }
/// Linux: pid exists and field 22 of /proc/<pid>/stat equals proc_start. Windows: the messaging
/// pipe named by messaging_socket_path is in the \\.\pipe\ listing read once by new(); nothing is
/// opened or connected to.
pub struct SystemProbe { /* Windows: the pipe names */ }
impl SystemProbe { pub fn new() -> std::io::Result<SystemProbe>; }
pub fn pipe_name(messaging_socket_path: &str) -> Option<&str>;
pub const TESTED_CLAUDE_CODE: &str = "2.1.322";
/// Every session file whose process is alive, sorted by started_at then pid. A missing folder is
/// an empty list. Only <digits>.json is read; an empty file and a file without status are ignored
/// quietly; any other file that does not parse is named in skipped.
pub fn running(claude: &ClaudePaths, probe: &dyn Liveness) -> Result<Running, SessionsError>;
pub struct Running { pub sessions: Vec<SessionFile>, pub skipped: Vec<String> }
pub fn newer_than_tested(version: &str) -> bool;
```

## transcripts.rs — incremental reading, answers, token records (step A)

```rust
pub enum TranscriptError { Io { path, source }, State { path, source }, Version { path, found } }
pub struct Answer { pub session: String, pub agent: Option<String>, pub message_id: String,
    pub at: DateTime<Utc>, pub cwd: String, pub model: String, pub input: u64, pub output: u64,
    pub cache_write: u64, pub cache_read: u64, pub thinking: bool }
pub struct UserTurn { pub session: String, pub at: DateTime<Utc> }
pub struct AgentMeta { pub session: String, pub agent: String, pub agent_type: Option<String>,
    pub description: Option<String>, pub finished: bool, pub spawn_depth: Option<u32> }
#[derive(Default, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct ScanState { pub version: u32, pub files: BTreeMap<String, FileState>,
    pub sessions: BTreeMap<String, SessionSeen> /* private: changed folders */ }
pub struct FileState { pub offset: u64, pub grew: DateTime<Utc>, pub recent: Vec<String>,
    pub last_answer: Option<DateTime<Utc>> }
pub struct SessionSeen { pub folder: String, pub cwd: String, pub first: DateTime<Utc>,
    pub last: DateTime<Utc>, pub last_model: Option<String>, pub last_user_turn: Option<DateTime<Utc>>,
    pub thinking: Vec<(DateTime<Utc>, u32)>, pub recorded: Option<RecordedSession>,
    #[serde(default)] pub title: Option<String>, #[serde(default)] pub custom_title: Option<String> }
// `title` is the `aiTitle` of the last `ai-title` line of the session's own transcript (never of an
// agent's), stored as written; an empty or all-space one is no title, a line without a string
// `aiTitle` is an unreadable line. It is applied after the file's other lines, so a title line before
// the first answer still names the session. No other text of the line is read. Absent in a state
// file written before it means none: no transcript is read again for it, so an older session gets a
// title only from its next `ai-title` line. The state version stays 2.
// `custom_title` is the `customTitle` of the last `custom-title` line (the user's `/rename`, 1.4.4),
// by the same rules; a pending one waits in `FileState.custom_title` as `title` does in `title`.
impl SessionSeen { pub fn thinking_since_user(&self) -> u32; }  // the sum of `thinking`
impl SessionSeen { pub fn shown_title(&self) -> Option<&String>; }
// The name Claude Code shows (`customTitle ?? aiTitle`): the live row's `title` and the session
// record's `title` are this, never `title` alone.
// `thinking` is the answers with thinking since the last user turn: one entry per minute (its start
// in UTC) and oldest first. A user turn empties it; every reader of the old number calls the sum.
pub struct RecordedSession { pub last: DateTime<Utc>, pub closed: bool }
// `recorded` is the last session record written from this session: the answer time it covers and
// whether it was the closing one. It keeps a restart from writing the same record again; absent in
// an old state file means none.
impl ScanState {
    pub fn load(app: &AppPaths) -> Result<ScanState, TranscriptError>;  // missing folder = default
    /// The one way to remember the session record written from `session`: sets `recorded` and marks
    /// the session's project folder changed, so it reaches the state file even when its transcript
    /// did not grow. A session not in the state is not changed.
    pub fn set_recorded(&mut self, session: &str, recorded: RecordedSession);
    /// Mark one project folder for the next save.
    pub fn mark_changed(&mut self, folder: &str);
    /// Writes only the project folders that changed, as <folder>.json; before writing, drops the
    /// recent ids of a file that has not grown for 24 h, and removes entries whose transcript is gone.
    pub fn save(&mut self, claude: &ClaudePaths, app: &AppPaths, now: DateTime<Utc>)
        -> Result<u32, TranscriptError>;
}
pub struct Scan { pub answers: Vec<Answer>, pub user_turns: Vec<UserTurn>, pub complete: bool,
    pub warnings: Vec<String> /* private: the state after this scan */ }
impl Scan {
    pub fn state(&self) -> &ScanState;              // for token_records
    /// The project folders this scan changed; empty after save.
    pub fn changed_folders(&self) -> Vec<String>;
    /// Write this scan's changed folders, before its answers' token lines.
    pub fn save(&mut self, claude: &ClaudePaths, app: &AppPaths, now: DateTime<Utc>)
        -> Result<u32, TranscriptError>;
    /// Moves the state past what this scan read; call it only after the answers' token lines were
    /// written. A scan that is not committed leaves the old offsets, so the next pass reads the same
    /// lines again and nothing is counted twice.
    pub fn commit(&mut self, state: &mut ScanState);
}
/// Reads what is new in <folder>/<session>.jsonl and <folder>/<session>/subagents/agent-<id>.jsonl,
/// at most budget_bytes; only whole lines, and the offset stops after the last '\n'. A shorter file
/// is read again from 0 (warning); its recent ids keep answers from counting twice. A file that had
/// stopped growing and was replaced is not counted again, and the warning says so. Other line
/// types are skipped quietly. A user line counts only as a real user message whose origin is
/// absent or "human". An answer counts once per file by message id; "<synthetic>" is skipped.
/// An answer's model is the line's `requestedModel` when it has one (a gateway such as Ollama
/// answers `deepseek-v4.1-flash` for `deepseek-v4.1-flash:cloud`), else `message.model`.
/// The caller's state is not changed; the result carries the state to `commit`.
pub fn scan(claude: &ClaudePaths, state: &ScanState, budget_bytes: u64)
    -> Result<Scan, TranscriptError>;
pub struct Agents { pub metas: Vec<AgentMeta>, pub warnings: Vec<String> }
pub fn agents_of(claude: &ClaudePaths, session: &str,
    spawns: &BTreeMap<String, Option<String>>) -> Agents;
/// One TokenRecord per (session, 5-minute bucket, model); an agent's answers go to its parent session
/// and use the parent's folder when known. The group's cwd is that of its earliest answer.
pub fn token_records(answers: &[Answer], state: &ScanState, home: &Path) -> Vec<TokenRecord>;
/// The length of a token line's bucket (spec R3); its `min` field is this.
pub const TOKEN_BUCKET_MINUTES: i64 = 5;
/// The start of the 5-minute bucket `at` falls in, in UTC.
pub fn bucket_start(at: DateTime<Utc>) -> DateTime<Utc>;
```

Only numbers and names leave the parser: time, folder, model, block kinds, token counts, session
and agent ids. Message text is never stored, logged, put into a warning or sent anywhere.

## transcripts additions — per-agent sums, who started whom, the last hour (step C2)

```rust
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct Tokens { pub input: u64, pub out: u64, pub cw: u64, pub cr: u64 }
impl std::ops::AddAssign for Tokens;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct AgentSeen { pub first: DateTime<Utc>, pub last: DateTime<Utc>, pub model: Option<String>,
    pub tokens: Tokens, pub answers: u32 }
// SessionSeen gains, all `#[serde(default)]` so a state file from before C2 loads:
pub totals: Tokens,                                  // the session and its agents
pub by_model: BTreeMap<String, Tokens>,
pub agents: BTreeMap<String, AgentSeen>,
pub spawns: BTreeMap<String, Option<String>>,        // tool_use id -> the agent that made it; None: the session
pub recent: Vec<(DateTime<Utc>, Tokens)>,            // one entry per minute, the last 60 minutes
// AgentMeta gains:
pub tool_use_id: Option<String>, pub parent: Option<String>   // the agent that started it; None: the session
pub fn last_hour(seen: &SessionSeen, now: DateTime<Utc>) -> Tokens;
pub fn agents_of(claude: &ClaudePaths, session: &str, spawns: &BTreeMap<String, Option<String>>)
    -> Agents;
impl ScanState { pub fn forget_closed(&mut self, running: &BTreeSet<String>, now: DateTime<Utc>) -> usize; }
```

`scan` adds every answer this scan counted to its session's `totals`, `by_model` and (for an agent
answer) `agents`; the same message id rule keeps an answer read twice from being summed twice. An
answer only goes to a session whose main transcript has made one: an agent transcript alone has
nowhere to put it. A `tool_use` block named `Agent` or `Task` adds its id and the agent that made the
call to `spawns` (only the id and the name are read from the block); `agents_of` looks each meta's
`toolUseId` up there and sets `parent`. A meta file that cannot be read or parsed is a warning
naming the file and the reason, and its agent is still listed as running with `spawn_depth: None`
(a meta without `finished` is a running agent, and an unreadable one cannot say it finished).
`recent` is one entry per minute; it is pruned to the
60 minutes before the newest answer, and `last_hour` adds the entries whose minute starts after
`now - 60 min`. `forget_closed` drops a session that is not running and whose latest answer — its own
or one of its agents', whichever is newer — is more than an hour old; its files' offsets stay, so the
bytes already read are not read again, and the folder is marked changed for the next save.

## engine.rs additions — usable, models in use, due at a reset (step A)

```rust
impl<R: Remote> Engine<R> {
    pub fn with_recorder(self, recorder: Recorder) -> Self;   // every record_success also calls recorder.reading
    pub fn recorder(&mut self) -> Option<&mut Recorder>;
    /// The models of the running sessions (lower-case ids), or None when they are not known
    /// (readSessions off, or the CLI).
    pub fn set_models_in_use(&mut self, models: Option<BTreeSet<String>>);
}
pub enum Blocker { Week, Session, Model(String) }
/// Why this usage blocks the account, or None. Week: weekly_all >= 100. Session: session >= 100.
/// Model(m): a weekly_scoped limit of m >= 100 and a model in use contains m lower-cased; with
/// models_in_use None, only while the server marks that limit active.
pub fn blocker(usage: &Usage, models_in_use: Option<&BTreeSet<String>>) -> Option<Blocker>;
/// At least one view has a working login, and every view with a working login has a Blocker.
pub fn all_blocked(views: &[AccountView]) -> bool;
// AccountView gains:  pub usable: bool, pub blocked_until: Option<DateTime<Utc>>
```

When several limits are at 100 %, the blocking one is chosen by the latest reset time (a limit with
no reset wins over one with a time); on a tie Week comes before Model, Model before Session.
`blocked_until` is that limit's reset time. `record_success` passes the account's `AccountRef` and
plan to the recorder; a history error becomes the account's `last_error`, never a failed poll. An
account blocked by a limit is due once 30 s after that reset whatever the interval, if its last
success and last attempt are both before that moment. After a poll the engine writes `AllBlocked` when
`all_blocked` becomes true, and `AnyUsable` when some view becomes usable after none was; it remembers
both in `UsageCache.recorded_usable`, which moves only when its event was written, so a restart during
a block writes no second `AllBlocked`. An account
whose usage is unknown is not usable but not blocked either, so it never makes `all_blocked` true and
losing the last working login is not "usable again".

The same poll compares every stored account's login state with `CacheEntry.recorded_login`: `None`
is seeded without a line (an old cache or a new account); `Ok` that is now `LoggedOut` writes
`loginEnded`; `LoggedOut` that is now `Ok` writes `loginRenewed`. The event carries the poll's `now`.
The field moves only when its event was written, so a failed write is retried by the next poll and
an engine without a recorder moves nothing (the next tray-app engine writes it). A failed login-event
write becomes a `last_error` through the same helper as a failed `AllBlocked`/`AnyUsable` write, so it
never hides another error and never fails the poll; it carries its own prefix, so a successful write
of one event kind never clears the other kind's failure.

## app: CLI (§4)

`crates/app/src/cli.rs`, clap derive. `main.rs` parses; no subcommand → tray app when built with `gui`, else help; from an agent (§6.6 of the spec) it is refused instead (`no_subcommand(caller)`). Global options: `--claude-config-dir <DIR>` (else the `claudeConfigDir` setting, else `$CLAUDE_CONFIG_DIR`, else `~/.claude`), `--data-dir <DIR>` (else the platform folder). Every command prints errors as `seatswap: <message>` to stderr and exits 1 (2: an unsaved current login; 3: refused for an agent); success exits 0.

| Command | Does |
|---|---|
| `list` | One line per account: `*` for the live one, abbr, label, email, plan, session %, week %, "login ends today" / "login ends in 3d", "LOGGED OUT" when so. Usage from the cache, with its age ("5min ago"); `--refresh` polls first (PollReason::PanelOpened). |
| `use <KEY>` | `switch_to`. KEY = id or abbr. When the live login is not saved: `--save-current [LABEL]` saves it first, `--discard-current` drops it; without either, exit 2 with the message "the current login (<email>) is not saved; rerun with --save-current or --discard-current". After the switch: "Switched to P — Private". |
| `next` | Switch to the next Ok account after the live one in list order (wraps), same unsaved handling as `use`. With `autoBurn` on, an agent's call prints `seatswap: auto-burn is on and decides` and switches nothing. |
| `add [--label L]` | `ops::save_live_login` (`sync_live` with create): saves the live login as a new account or updates the stored copy; a new account writes `accountAdded` with its plan. Prints the account. |
| `rm <KEY> [--yes]` | `ops::remove_account` (writes `accountRemoved`); without `--yes` asks "Remove AC — Acme? [y/N]" on a terminal, refuses when stdin is not a terminal. |
| `edit <KEY> [--label L] [--abbr A] [--color #rrggbb]` | `edit_account`. |
| `panel [main\|add\|settings]` | Asks the running tray app to open its pop-up (§ app: GUI, single instance); `settings` sends `window settings` and opens the large window instead, because the pop-up has no settings view (step C1). Not running → exit 1 "seatswap is not running". This is the hotkey on Linux v1: the person binds it in the desktop's keyboard settings (no global-shortcut crate is available). |
| `agent-skill install\|uninstall [--force]` | `agent_skill`: `<claude config dir>/skills/seatswap/SKILL.md` from the bundled text, through `atomic::ensure_private_dir` and `atomic::write_atomic` (0600). Install prints "already installed: <path>" when the file is identical, refuses a differing file without `--force`, else prints the path. Uninstall prints "not installed: <path>" when absent, refuses a differing file without `--force`, else prints "removed <path>" and removes the folder when it is empty. |
| `status` | The live account in one line: `AC 97% · 1h 12min` (binding limit, time to its reset; format §11: `35min`, `1h 5min`, `1d 18h`, beyond 48 h `Sun 02:00` — `format::format_reset_at`; reset times are rounded up to the full minute first, `format::reset_minute`). `--json` prints `views` + `active` + `suggestion` + `autoBurn` + `next` + `burnError` (§7.8 of the spec); each view carries `usable`, the one definition of §7.2, and `slackHours` when known. `--waybar` prints one JSON line `{"text":"AC 97%","tooltip":"…","class":"fine|close|critical|locked|none","percentage":97}`; `--i3blocks` prints three lines (full text, short text, colour). `--refresh` polls first (PollReason::Timer). Only `--json` reads the history. |
| `statusline` | Claude Code's status line: reads its JSON from stdin, prints the one line and exits 0 (even when it prints `seatswap: <reason>`). Never gated, never recorded, no lock, no network. |
| `burn plan [--json]` | The regime and the plan auto-burn follows (eager while early switching is on; §7.7 of the spec). |
| `burn simulate --from D --to D [--without KEY]…` | Replays the two local days for `Lazy` and `Eager`, Anthropic work only (R7). |
| `codex list [--json]` / `codex switch <ABBR>` | `codex list` reads the saved Codex logins and prints one line each, live first then by abbreviation: `* OA  Plus  live` (`*` and `live` on the live one, abbr, plan or `—`; a saved login has neither mark); `--json` prints `{account, abbr, label, color, plan, live}` per login, no usage and no poll. `codex switch` switches Codex's live login to the saved login with that abbreviation through `ops::switch_codex`, prints its note and warnings, and records `providerSwitch`; an unknown abbr is a usage error, and a Claude account's abbr is named as such. `codex list` follows `agentsRead`; `codex switch` follows `agentsSwitch`. |

### Agent gate and skill (step A)

```rust
pub enum Caller { Person, Agent }
/// Agent when `CLAUDECODE` is exactly "1", or `AI_AGENT` is set and not empty. `env` is passed in,
/// never the process's own environment.
pub fn caller(env: &dyn Fn(&str) -> Option<String>) -> Caller;
pub enum Gate { Allowed, Refused(&'static str) }
pub fn gate(command: &Command, caller: Caller, settings: &Settings) -> Gate;
pub enum SkillAction { Install { force: bool }, Uninstall { force: bool } }
pub enum CliError { … , AgentRefused(&'static str) }   // exit code 3
```

Gate, when `caller` is `Person`: everything is `Allowed`. When `Agent`: `list`, `status` and `burn`
need `agents_read`; `statusline` is always allowed; `use` and `next` need
`agents_switch`, and never with `--save-current` or
`--discard-current`; `add`, `rm`, `edit`, `agent-skill` and `panel` are always refused. The three
texts are `agents may not read usage (seatswap settings → Agents)`, `agents may not switch accounts
(seatswap settings → Agents)` and `agents may not change seatswap's accounts or settings`; a
refusal prints `seatswap: <text>` and exits 3. The store is opened first, so the call can be
recorded: a refused call changes no account, setting or login, and writes only its `agentCall`
event.

`command_text` names the subcommand and its positional values and flags, never free text
(`--label`, `--abbr`, `--color`, `--save-current`'s value); every call by an agent writes
`EventKind::AgentCall { command, cwd, allowed }`. `current_cwd` records the folder only when
`std::env::current_dir()` and its UTF-8 name can be read; otherwise it prints a warning and records
None, never a replaced name. A switch made by a caller `Agent` is recorded with `By::Agent` and the
same folder; the tray always announces it with `An agent switched to {abbr} (from {cwd with ~}).` or
`An agent switched to {abbr}.` when the folder is unknown, whatever `notifyOnSwitch` says. The
CLI tests' `command()` removes `CLAUDECODE` and `AI_AGENT` so a developer's terminal cannot change
a test's result.

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

Files: `crates/app/src/gui/{mod.rs, worker.rs, tray.rs, panel.rs, notify.rs, autostart.rs, desktop_entry.rs, ipc.rs, registry.rs}` (`desktop_entry.rs` and the Linux entry on every non-Windows system, `registry.rs` on Windows only), `crates/app/ui/{index.html, panel.css, panel.js}`, `crates/app/capabilities/default.json`. The code is the library `seatswap` (`src/lib.rs`, `pub fn run() -> ExitCode`); two programs call it: `seatswap` (`src/main.rs`) and `seatswap-tray` (`src/bin/seatswap-tray.rs`, built without a console window on Windows for autostart and the Start menu; Linux builds it but does not install it). No subcommand → `gui::run(app: AppPaths, claude_override: Option<PathBuf>, data_override: Option<PathBuf>) -> ExitCode` (the same path resolution as the CLI, one function; the overrides are passed on so `seatswap panel` and autostart start the app with the same flags). Actions both front ends must take the same way live in `crates/app/src/ops.rs`: `save_live_login(claude, store, recorder: Option<&Recorder>, label, now) -> Result<(Account, Option<String>), String>` (a missing, ended or unknown login saved nothing and is an error; the `Option<String>` is the history error, never a failed add, and `accountAdded` is written only when `sync_live` returned `Synced::Added`), `remove_account(store, key, recorder: Option<&Recorder>, now) -> Result<(Account, Option<String>), String>` (the same shape for `accountRemoved`), `switch_message(store, account_id) -> Result<String, String>` ("Switched to AC — Acme"; an unreadable store or unknown id is an error, never the raw id) and `switched_without_list(error) -> String`. The action thread's external-switch check keeps the history read warnings it printed, so a torn month-file line is printed once per run, not at every live-account change.

**Threads.**
- *Main thread:* Tauri event loop, tray, menu, panel window.
- *Engine thread:* owns `Engine<Api>` (its own `Store` from `store::open_store`). Loop on a command channel with a 30 s tick: `Poll(PollReason)`, `Publish`, `Feed`, `HistoryChanged`, `Shutdown`. Each tick polls with `Timer` (the engine decides what is due). Wake detection: when the wall clock advanced more than 90 s beyond the monotonic clock between two ticks → `Poll(Wake)`. After each poll or `Publish` it builds a `Snapshot` and hands it to the main thread.
- *Action thread:* owns a second `Store` (from `open_store`). Runs switch / save current / edit / remove / save settings / pause-burn one at a time, each replying through a one-shot channel, then sends `Poll(AfterSwitch)` or `Publish` to the engine thread. Two stores are safe because every accounts.json change is load-modify-save under the app lock.
- *Watcher thread:* `watcher::watch`; its callback sends `SyncLive` to the action thread (sync_live when `mirrorLive`, else nothing), which then sends `Publish`.
- Tauri commands never block the main thread (`async` commands that wait on the reply with `tauri::async_runtime::spawn_blocking`).

**Snapshot** (serde camelCase; the panel's only data source; event `snapshot`, and command `get_snapshot`). Every snapshot reaches the pages through `send_snapshot` in `gui/mod.rs`, which sends it only when its JSON differs from the last one sent (`SentSnapshot`); while sessions work, the sessions thread can finish several passes a second, mostly without a change. A page that loads later reads the stored one with `get_snapshot`:

```rust
pub struct Snapshot {
    pub views: Vec<AccountView>,          // engine::views, active first
    pub accounts: Vec<AccountRow>,        // the stored fields the cards do not need
    pub provider_subscriptions: Vec<ProviderSubscriptionRow>,
    pub active_id: Option<String>,
    pub live: LiveView,                   // { kind: "noLogin"|"ended"|"known"|"unknown", accountId?, email?, orgName? }
    pub burn: Option<BurnView>,           // step B: auto-burn's line, plan, waiting switch, suggestion
    pub warnings: Vec<LoginWarning>,      // engine::login_warnings (Serialize, camelCase, tag "kind")
    pub pending: Option<PendingSwitch>,   // { targetId, targetLabel, email, orgName }: a switch waits for "save or discard the current login?"
    pub secrets_warning: Option<String>,  // from open_store: logins stored unencrypted
    pub last_poll_at: Option<DateTime<Utc>>,
    pub error: Option<String>,            // last engine-wide error (store/cache unreadable)
    pub sessions: Option<SessionsSummary>, // None = readSessions off
    pub providers: Vec<ProviderView>,    // switched-on cards, filled by the provider thread
    pub codex_logins: Vec<CodexLoginView>, // saved Codex logins, live first; empty while the card is off
    pub settings: Option<Settings>,      // None when accounts.json cannot be read: the panel disables Save
}
```

**Tauri commands** (all `Result<T, String>`, the error is the human message of the core error):
`get_snapshot() -> Snapshot` · `app_version() -> String` (`env!("CARGO_PKG_VERSION")`, for Settings → General's *Version* line) · `refresh_now()` (Poll(Wake)) · `switch_account(key, unsaved: "refuse"|"discard"|"save", label: Option<String>) -> String` (the success sentence; `CurrentNotSaved` sets `pending` and returns Err "not saved") · `cancel_pending()` · `save_current(label: Option<String>)` (sync_live with create) · `edit_account(key, label?, abbr?, color?)` · `remove_account(key)` · `save_setting(field: String, value: serde_json::Value) -> SavedSetting` (one field merged under the app lock, validated, applies startAtLogin) · `set_auto_burn(on)` (`SaveSetting { field: "autoBurn", value: on }`: the same save path, writes `autoBurnSet` only on a change) · `pause_burn(hours: Option<i64>)` (Some = pause, None = resume) · `switch_codex(account: AccountRef) -> Result<String, String>` (the action thread's `SwitchCodex`, the note; the provider thread is poked through `codex_changed`) · `remove_codex_login(account: AccountRef) -> Result<(), String>` (the action thread's `RemoveCodexLogin`; the live copy is refused) · `icon_preview(abbr, color) -> Vec<IconPx { size, rgba: Vec<u8> }>` for 16, 22, 32 (the panel draws them with `ImageData`) · `fit_panel(height: f64)` · `hide_panel()`.
Event `navigate` (payload `"main"|"add"`) when the panel is shown for a menu item. The pop-up has no
settings view; its gear, `,` and the CLI's `panel settings` open the large window at Settings.

**Tray** (`tray.rs`, redrawn on every snapshot): icon from `icon::render` at 22 px ×2 (44 px image) for the active account — ring = `settings.iconLimit` limit (binding/session/week) with its `LoadLevel` colour, disc text = the abbreviation or, with `iconText` `percent`, that limit's whole percent with 100 as `99` (`IconSpec.text`; the letters while it has no reading), `locked` at ≥ 100 %, grey ring while the usage is unknown, a grey "?" disc when no account is live; tooltip as §8.1; tray label (`set_title`) per `trayText`. Linux backend: tray-icon's `ksni` (StatusNotifierItem over D-Bus), switched on in `crates/app/Cargo.toml` next to Tauri's libappindicator, because libappindicator reports no clicks and drops the tooltip; with ksni the item has `ItemIsMenu = false`, a left click arrives as `TrayIconEvent::Click(Left)` (opens the panel), a right click shows the menu, and the tooltip is published (checked on the session bus, 2026-11-10). Without a StatusNotifierWatcher on the bus (plain GNOME without the AppIndicator extension, or a desktop panel that starts after seatswap at login) tray-icon cannot create the item and has no option to wait; `tray::start` then keeps the app running without an icon, says so on stderr at once and in a notification only if no tray has appeared after 30 s (`TRAY_GRACE`: at login Cinnamon starts its tray in the same second as seatswap, so the first try often comes too early) (`seatswap panel` still opens the pop-up), and creates the icon when the name `org.kde.StatusNotifierWatcher` appears (`gio::bus_watch_name`, GTK's own gio). Once the item exists, ksni registers it again whenever the watcher restarts. Menu as §8.3: one item per account (`AC  Acme   97 % · 1h 12min` + "✓" for the live one, "logged out" when so), clicking switches with `refuse`; then "Save current login (…)…" when live is unknown; "Open panel…", "Add account…", "Sessions…",
"Statistics…", "Settings…", "Quit seatswap". Number formatting is shared with the CLI (one module, `crates/app/src/format.rs`); durations are `seatswap_core::duration`, and `format::login_ends_text(days_left)` is the one "login ends" phrase for the CLI and notifications.

**Panel** (`panel.rs`): one window label `panel`, created on first open (`index.html#<view>`), kept and hidden afterwards. 380 px wide, height from `fit_panel`, no decorations, always on top, skip taskbar. Placement §9.2: X11 at the mouse, inside the monitor that holds it, above the mouse when the mouse is in the lower half, else below; Wayland (`XDG_SESSION_TYPE=wayland`) centred. `show` takes an `Anchor { cursor, monitor, scale }` once per opening and keeps it with the last requested logical height in `GuiState.panel: Mutex<Placement>`; `fit_panel` resizes and re-places from that anchor (`anchored_position(anchor, height, wayland)` → `place_panel`), so the panel keeps its bottom edge above the mouse, its top edge below it, and stays inside the monitor. Never read the window's position or size back to place it: on X11 they report the old values until the window system has applied a change (the headless run of 2026-11-10 drifted the panel to y = 836 and y = −386 that way). The window is built `resizable(true)`: GTK ignores `set_size` on a non-resizable window. Closes on Esc and the header's × (panel.js → `hide_panel`), on focus loss, and on a left click on the tray icon while open (`panel::toggle`, decision in the pure `tray_click(visible, focus_hidden_ago)`; a click within `REOPEN_GUARD` = 400 ms after a focus-loss hide is the click that caused it and does not reopen). Focus loss hides only if the panel is still unfocused `FOCUS_RECHECK` = 150 ms later and no pop-up of the process holds a GTK grab (`own_popup_open` → `gtk::grab_get_current`): WebKitGTK shows a `<select>` list as its own window with a grab, so the panel is unfocused for as long as the list is open; the list closes on the next click outside and hands the focus back. Focus on X11 (`take_focus`, `focus_x11`): `present_with_time` + `gdk_window.focus` with the X server's current time (`gdkx11::x11_get_server_time`) — Cinnamon's window manager refuses `set_focus`, which asks without a timestamp; and on every map the request is repeated up to `FOCUS_RETRIES` = 5 times, `FOCUS_RETRY_GAP` = 80 ms apart, until the window is active, because a freshly mapped window is not yet managed and the first request is ignored (both seen on Cinnamon 2026-11-10). Built hidden at startup (`panel::preload` in `setup`) so the page has loaded and measured itself before the first opening. While the panel is hidden (`document.hidden`), `panel.js` only keeps the newest snapshot and does not draw it, and the one-second age timer is still: WebKit runs no animation frame in a hidden page, so each drawing waited in memory for its ring and card animations until the next opening (1.4.2, gigabytes a day). Every opening draws through `navigate` → `setView`; a snapshot that came while the page was still hidden is drawn on `visibilitychange`. A hidden panel does not appear when `show` is called: `show` stores the anchor, sets `PanelState.pending_show` and sends `navigate`; the page answers every navigation with `fit_panel` (setView), and that call sizes, places and only then shows it (`reveal`: move, then `show`), so the window manager maps it once, at its final size and place. If no height arrives within `SHOW_DEADLINE` = 300 ms it appears anyway with a log line. On Linux, nothing may map the window before the move is applied (`present_with_time` maps through GTK at once, and the panel flashed at its old place); focus is asked from the map handler instead. `.panel` has `flex-shrink: 0`: before the first showing the viewport has no width and a shrinking panel measured 2698 px tall. Opening it sends `Poll(PanelOpened)`.

**Notifications** (`notify.rs`, notify-rust): every notification is built from `base()` (summary "seatswap"; on Windows `.app_id("dev.seatswap.app")`), the only caller of `Notification::new()`. It shows the picture at `<data folder>/notification-icon.png` (`AppPaths::notification_icon`), written from the embedded icon when the file is missing or holds different bytes; a failed write is a warning on stderr and the notification still goes out. On Windows `register_app_id()` writes the display name "seatswap" under `HKCU\Software\Classes\AppUserModelId\dev.seatswap.app` once at startup, before the single-instance listener; a failure is a startup error `notification name: …`. After a switch when `notifyOnSwitch` ("Switched to P — Private"); auto-burn's suggestion (§7.4) once per target and hour, with a "Switch" action (`wait_for_response` on its own thread; click on the body opens the panel); login warnings once per account and level per run; an unknown live login once per identity per run ("New login found: <email>. Save it as an account?" → a click on its button or body opens the add view). What counts as a click is decided in one pure function, `click(response, button)`: the offered button (`NotificationResponse::Action`) or the body (`Default`); a notification that timed out, was dismissed or closed (`Closed`) does nothing. On Linux the body click only arrives when the notification announces the action `default` (`clickable()`); the notification service shows no button for it. Windows reports a body click by itself, so `default` is announced on Linux only.

**Single instance and `seatswap panel`** (`ipc.rs`): the tray app takes `acquire_file_lock(gui_lock, 0)`. Busy → another instance runs: send `open-panel main` to `gui_socket` and exit 0. Held → name the socket only now (the lock created the data folder on first start, and the name needs its real path), remove a leftover socket, listen on it (mode 0600 on Unix; on Windows the file has the data folder's permissions). Windows uses the same Unix socket through `uds_windows` (`std::os::windows::net` is not stable yet); there is no runtime folder, so the socket is `<data>\gui.sock`. One line per connection: `open-panel <view>` → `ok`, anything else → `error unknown command`.

**Autostart** (`autostart.rs`, one entry point `apply_autostart(enabled, arguments)`): at start and on every `save_setting`. The entry is text: a program path or folder argument that is not valid Unicode is `AutostartError::NotUnicode` on both platforms (the one `path_text`), never a path with replaced characters. Linux: `startAtLogin` on → write `autostart_file($XDG_CONFIG_HOME or ~/.config)` with `Exec=<current_exe>` via `write_atomic` (`Access::Shared`); off → remove it if present. Windows: on → the string value `seatswap` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` = `"<folder of current_exe>\seatswap-tray.exe" "<arg>" …` (each part in double quotes, `"` written `\"`, a run of backslashes before a `"` or at the end doubled); a missing `seatswap-tray.exe` (`NoTrayProgram`), a path that is not valid Unicode (`NotUnicode`) or a command over 260 UTF-16 units (`CommandTooLong`) is an error, never shortened; off → delete the value (missing is fine). `gui/registry.rs` (Windows only) is the one module that names `winreg`: `set_string`, `get_string` (tests only), `delete_value`, all under `HKEY_CURRENT_USER`. `autostart::data_home() -> Result<PathBuf, AutostartError>` is `$XDG_DATA_HOME`, else `~/.local/share`, read once at start for the menu entry; `data_home_from(xdg, home)` is the test form and never reads the environment.

**The Linux menu entry** (`gui/desktop_entry.rs`, `gui` feature, D-1): at every start of the tray app,
`apply_at_start(arguments: &[String]) -> Result<Outcome, String>` writes
`paths::applications_entry(data_home)` and `paths::app_icon(data_home)` through `write_atomic` when
they are missing or differ. The entry's `Exec` is the same command and flags as the autostart entry;
it carries `X-seatswap-managed=true`. `Outcome { written: Vec<PathBuf>, left_alone: Vec<PathBuf> }`
names what was written and what was left alone, one stderr line per left-alone path. A symbolic link
at either path is left alone, and so is an entry that does not carry the managed line; the icon is
seatswap's own file and is still written when the entry is left alone. A line break in a path or
argument is refused in both entries; the icon path is escaped.
One embedded icon constant (`APP_ICON_PNG`) serves the tray, the notifications and the entry. The
whole module returns no effect on Windows.

**Panel UI** (`ui/`, plain HTML/CSS/JS, no build step, `window.__TAURI__` with `app.withGlobalTauri: true`): §11.1–11.2. Views `main` and `add` (the §6.1 steps; shows the unknown live login with a label field and "Save" when there is one); a `settings` request now opens the large window at Settings (step C1), because the pop-up has no settings view. The main view's footer carries the sessions line of §11.1 from `snapshot.sessions`; the line's "N sessions" opens Sessions, "N waiting for you" opens that session's `sessions?<id>` (Sessions itself when no id is there), and "see the plan" opens `statistics/forecast`. A waiting id the window's `parse_target` refuses is shown as the banner's error, never dropped. Every time on the page (reset countdowns, the header's poll age, a card's "usage 5min ago") is an element with `data-time` + `data-time-style` whose text comes from the command `time_texts([{at, style}]) → [text]` (`countdown` → `format_reset_at`, `clock` → `format_reset_clock`, `age` → `format_age`, `day`, `date`, `dayClock`, `dayYear` → `format::day`, `format::date`, `format::day_clock`, `format::day_year`, and `left` → `format::time_left`, the card header's time left until the weekly reset), asked after each render and every second, so the page has no copy of these rules. Without `window.__TAURI__` it runs on a built-in fixture snapshot so it can be opened in a browser; there `time_texts` answers with stand-ins (the plain clock time, "a moment ago").

## app: sessions thread (`gui/sessions_rt.rs`) — step A

It runs only when `readSessions` is on: every wake asks which sessions are alive, reads the new
parts of their transcripts, writes token and session lines to the history, then hands a
`SessionsSummary` to the main thread and the models in use to the engine thread.

```rust
pub(crate) struct PassOutcome { pub summary: SessionsSummary, pub models: Option<BTreeSet<String>>,
                                pub tied_busy: u32, pub thinking: Vec<(DateTime<Utc>, u32)>,
                                pub wake_soon: bool }
pub(crate) struct SessionMemory { /* home, project per cwd, waits, last session record, closed,
    feed costs, Harness links, the last other hosts read */ }
impl SessionMemory { pub(crate) fn new(home: PathBuf) -> SessionMemory; }
pub(crate) struct SessionsHandle { /* stop flag, command sender, join handle, latest summary */ }
impl SessionsHandle {
    pub(crate) fn latest(&self) -> Arc<Mutex<Option<SessionsSummary>>>;
    pub(crate) fn client(&self) -> SessionsClient;
    pub(crate) fn stop(&mut self);           // also joins the thread; Drop stops it
}
pub(crate) fn spawn(app: AppPaths, claude: ClaudePaths, history: Result<HistoryDir, String>,
                    harness: Result<HarnessPaths, String>, codex: Result<CodexPaths, String>,
                    lookup: Option<LookupClient>, ui: Sender<UiEvent>,
                    engine: Sender<EngineCommand>) -> Result<SessionsHandle, String>;

/// One wake, without the thread: the sessions, new transcript answers, token lines, session lines
/// and waits, then the Harness and Codex rows. `procs` is the process table (`harness::RealProc`
/// in the app; a fake in tests). `state_error` is the load error that keeps this pass from reading
/// a transcript, writing a token or session record, or saving the state; it is shown in the summary.
/// A pass that did not finish asks for a wake in one second.
pub(crate) fn run_pass(app: &AppPaths, claude: &ClaudePaths, probe: &dyn Liveness,
                       procs: &dyn harness::ProcSource, recorder: Option<&Recorder>,
                       state: &mut ScanState, memory: &mut SessionMemory, now: DateTime<Utc>,
                       state_error: Option<&str>) -> PassOutcome;
/// run_pass, then state.save; a save failure is a warning on the summary. The thread's start is
/// load_state(app) + SessionMemory::new(home) + one_pass, which is what the backfill test does.
fn one_pass(app: &AppPaths, claude: &ClaudePaths, probe: &dyn Liveness,
            procs: &dyn harness::ProcSource, recorder: Option<&Recorder>,
            state: &mut ScanState, memory: &mut SessionMemory, now: DateTime<Utc>,
            state_error: Option<&str>) -> PassOutcome;
```

`WAKE` = 60 s, `AFTER_INCOMPLETE` = 1 s, `SCAN_BUDGET` = 64 MiB per pass, `RECORD_EVERY_SECS` =
10 min: a running session's record is written when its summary changes and at most that often, and
one closing record when it is gone. A session seen in a transcript whose session file is not running
gets one closed record too (a short `claude -p` session, every session the backfill read); a later
scan that adds answers writes a new closed record, and `SessionSeen.recorded` keeps a restart from
writing the same one again. `SessionMemory.closed` remembers only a session already pruned from the
state, which `recorded` cannot cover. Token records are written on every pass from the answers that
pass read (`transcripts::token_records`). When there are token lines, the scanned state is saved
first; if that save fails no token line is written and nothing is committed (the next pass tries
again). If the token write fails, the old state's folders are saved back and nothing is committed.
A crash between the two steps loses those answers but never counts them twice. Without a recorder
the offsets stay, so the next pass with a working history reads the same answers. Waits are kept in
memory: a wait that ended writes
`EventKind::Waited { session, project, secs }` with the whole seconds (never 0 from a backwards
clock). Only "the session files cannot be read" is `SessionsSummary.unavailable` (the counts are
then 0); a history error, a state that cannot be read or written, an unknown `statusUpdatedAt`, a
count that does not fit u32 and a transcript read error are `SessionsSummary.warning`, with the
history error first and then the state. The thread keeps running. The app prints the recorder's read
warnings once on stderr as `seatswap: history: <warning>`; the action thread does the same for the
warnings of the month file `record_external_switch` reads.

## app: worker, ops and notifications additions — step A

```rust
// worker.rs
pub(crate) struct SessionsSummary { pub running: u32, pub busy: u32, pub tied: u32, pub tied_busy: u32,
    pub waiting: Vec<Waiting>, pub unavailable: Option<String>, pub warning: Option<String> }
pub(crate) struct Waiting { pub session: String, pub label: String,
    pub title: Option<String> /* the waiting notification names the session by it, else by label */,
    pub project: String,
    pub since: DateTime<Utc>, pub host: Option<String> /* None = this host */ }
pub(crate) struct Snapshot { … pub burn: Option<BurnView>, pub sessions: Option<SessionsSummary> /* None = readSessions off */ … }
pub(crate) enum EngineCommand { Poll(PollReason), Publish,
    ModelsInUse { models: Option<BTreeSet<String>>, tied_busy: u32,
                  thinking: Vec<(DateTime<Utc>, u32)> }, Feed, HistoryChanged, Shutdown }
pub(crate) enum UiEvent { Snapshot(Snapshot), Sessions(SessionsSummary) }

/// The timer poll as a fixed deadline: a command never moves it; `advance` jumps over `now`.
struct TickDeadline { next: Instant }
impl TickDeadline { fn new(now: Instant) -> TickDeadline; fn wait(&self, now: Instant) -> Duration;
                    fn due(&self, now: Instant) -> bool; fn advance(&mut self, now: Instant); }

/// History-read warnings already printed this run. The tray app keeps one set
/// (`GuiState::printed_warnings`, an `Arc<Mutex<…>>`) that the engine, the action thread, the
/// auto-burn report, the statistics commands and the projects commands all print through.
pub(crate) struct PrintedWarnings { seen: BTreeSet<String> }
impl PrintedWarnings { pub(crate) fn new() -> PrintedWarnings;
                       pub(crate) fn new_texts(&mut self, warnings: &[String]) -> Vec<String>;
                       /// `seatswap: history: <warning>` for each new text.
                       pub(crate) fn print(&mut self, warnings: &[String]); }

// gui/mod.rs: the one app error reporter (`GuiState::reported_errors`): prints `seatswap: <text>`
// and notifies once per text per run. The snapshot's error, a failed history write of an action
// and the provider thread's recording errors all go through it.
pub(crate) struct ReportedErrors { notified_errors: Mutex<HashSet<String>>, send: Box<dyn Fn(&str) + Send + Sync> }
impl ReportedErrors { pub(crate) fn new(send: Box<dyn Fn(&str) + Send + Sync>) -> ReportedErrors;
                      pub(crate) fn report(&self, text: &str); }

// ops.rs
pub(crate) fn open_history(app: &AppPaths, settings: &Settings) -> Result<Recorder, String>;
pub(crate) fn history_unwritable(error: &dyn std::fmt::Display) -> String;
pub(crate) fn record_switch(recorder: &Recorder, from: Option<AccountRef>, to: AccountRef,
    saved_new: Option<&Account>, by: By, reason: Option<String>, cwd: Option<String>,
    sessions: Option<SessionCounts>, at: DateTime<Utc>) -> Result<(), HistoryError>;
// saved_new = the account SwitchOutcome.saved_new created: its accountAdded is written before the switch.
// Both lines are tried even when the first fails; the first error is returned.

/// Create or update the provider subscription for an OpenAI or Ollama account seen in a reading.
/// OpenAI keeps its plan current at every reading; Ollama's plan is typed. Returns the warnings to
/// print (a store write failure is one, never a failed reading).
pub(crate) fn ensure_provider_subscription(app: &AppPaths, recorder: Option<&Recorder>,
    account: &AccountRef, plan: Option<&str>, now: DateTime<Utc>) -> Vec<String>;

pub(crate) struct CodexSwitch { pub note: String, pub warnings: Vec<String> }
/// Switch Codex's live login and record the `providerSwitch` event. The event follows a successful
/// write, with the old live login as `from` (None when auth.json was missing) and `by` the action's
/// `By`. `note` is `Codex now uses {abbr}. Codex sessions already running keep {from} until they
/// restart.` (without the second sentence when nothing was live); a history failure is a warning,
/// never a failed switch.
pub(crate) fn switch_codex(app: &AppPaths, codex_paths: &CodexPaths, store: &Store,
    recorder: Option<&Recorder>, to: &AccountRef, now: DateTime<Utc>)
    -> Result<CodexSwitch, String>;

// notify.rs
pub(crate) enum Notification { Suggestion(SwitchSuggestion), LoginWarning(..), UnknownLogin { .. },
    UsableAgain { id: String, reason: String }, AllFull { next: Option<(String, DateTime<Utc>)> },
    Waiting(Waiting), WaitingOver { session: String } }
impl Deduper {
    /// All notifications that are due now: suggestions and warnings as before, plus usable again,
    /// all full, and waits.
    pub(crate) fn due(&mut self, snapshot: &Snapshot, now: DateTime<Utc>) -> Vec<Notification>;
    /// When the next waiting session becomes 15 s old; `None` when none is still waiting.
    pub(crate) fn recheck_at(&self, snapshot: &Snapshot, now: DateTime<Utc>)
        -> Option<DateTime<Utc>>;
}
pub(crate) fn send_usable_again(account: &str, reason: &str, on_switch: impl FnOnce() + Send + 'static,
    on_open: impl FnOnce() + Send + 'static);
pub(crate) fn send_all_full(next: Option<(&str, &str)>);
pub(crate) fn send_waiting(waiting: &Waiting, host: Option<&str>,
    on_open: impl FnOnce() + Send + 'static) -> WaitingHandle;
impl WaitingHandle { pub(crate) fn withdraw(self); }
```

`Notification::UsableAgain` needs `notifyUsableAgain` on, the account now usable, not active, and
the previous snapshot naming a blocker whose `blocked_until` is at or before `now` (the block ended
because its reset passed), or whose blocking limit fell early by `history::is_early_reset` (the
blocker's percent in the previous snapshot is at least 1 point above its percent now, and now is more
than 60 s before its reset time). It is remembered by (account id, previous reset minute), so one per
account and reset. A block that ends earlier without that drop sends nothing and does not use that key. `AllFull` only
when the previous snapshot had a usable account and `engine::all_blocked` says every account whose
login still works is now blocked; the next is the earliest `blocked_until`. `Waiting`
only after 15 s (`waiting.since` to `now`), once per session and wait; turning `notifyWaiting` off
withdraws the shown ones and sends nothing new; a session that stops waiting gets `WaitingOver`,
which withdraws its notification. The texts come from one place each: `usable_again_text` =
`{abbr} is usable again: {reason}.`, with reason `its 5h window reset`, `its week reset`,
`its {model} week reset`, or the early-reset forms `its 5h window was reset early`,
`its week was reset early` and `its {model} week was reset early`; `all_full_text` =
`All accounts are full. Next: {abbr} at {time}.` or
`All accounts are full.`; `waiting_text` = `{label} is waiting for you ({project}, on {host}).` or,
without a host name, `{label} is waiting for you ({project}).` (the host is `waiting.host` when set,
this host's history name otherwise; a click opens the large window at `sessions?<session id>`).
`notifyWaitingHosts` decides which waits notify: `ThisHost` only `host: None`, `AllHosts` all. On
Linux a waiting notification never times out and can be withdrawn; on Windows it follows the
system's rules and the pop-up keeps the count.

---

## Step B: status line, feed and auto-burn

The new modules, each with one job:

| Module | Job |
|---|---|
| `core::feed` | the status line's live numbers: the feed file, its account, merging into the cache |
| `core::burn::{rates, plan, decide, tied, cost, replay, live, burn_next}` | auto-burn: rhythm and rates, slack and the two plans, the decision, thinking across a switch, switch cost, replay, one read of the history into the plan's inputs, and the status line's next-account file |
| `app::statusline` (`mod`, `render`, `width`) | Claude Code's payload, the line's facts, rendering and terminal width |
| `app::burn_text` | every word auto-burn shows, for the tray app and the CLI |
| `app::gui::burn_rt` | the tray app's auto-burn loop: learn, decide, switch, view |

## statusline — layout types in store.rs (step B)

`Settings.statusline` and the account's `auto_burn` live in `store.rs`; the layout types are the
same file (spec §7.6).

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct StatuslineConfig {
    #[serde(default = "default_statusline_left")]  pub left: Vec<Segment>,
    #[serde(default = "default_statusline_right")] pub right: Vec<Segment>,
    #[serde(default = "default_statusline_margin")] pub margin: u16,
}
#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct Segment {
    pub kind: SegmentKind,
    #[serde(default = "default_color_rule")] pub color: ColorRule,
    #[serde(default, skip_serializing_if = "String::is_empty")] pub prefix: String,
    #[serde(default, skip_serializing_if = "String::is_empty")] pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub drop: Option<u8>, // 1 dropped first; None never
    #[serde(default, skip_serializing_if = "Option::is_none")] pub time_format: Option<TimeFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub separator: Option<SeparatorChar>,
}
/// A separator segment's character. `text()` is the character alone: `│`, `·`, `•`, `/`, `›` or one
/// space (blank); the spaces around it come from the segment's prefix and suffix.
#[serde(rename_all = "camelCase")]
pub enum SeparatorChar { Pipe, Dot, Bullet, Slash, Chevron, Blank }
#[serde(rename_all = "camelCase")]
pub enum SegmentKind { Account, Plan, FiveHourPercent, FiveHourReset, WeekPercent, WeekReset,
    ModelPercent, AutoBurn, Model, Effort, Folder, GitBranch, UserHost, Clock, Text, Separator }
#[serde(tag = "rule", rename_all = "camelCase")]
pub enum ColorRule { None, Fixed { color: String }, ByLoad { warn: u32, bad: u32 },
    ByModelFamily, ByEffort, Account }
#[serde(rename_all = "camelCase")]
pub enum TimeFormat { CompactLeft, Seatswap, Clock }
impl StatuslineConfig { pub fn current_line() -> StatuslineConfig; } // the preset "My current line"
fn validate_statusline(config: &StatuslineConfig) -> Result<(), StoreError>;
// Fixed colour is #rrggbb; ByLoad needs warn < bad <= 100; drop is never 0; a Text segment has text;
// a SeparatorChar is allowed on a Separator segment only, and a 1.1.0 saved segment without the key
// is the pipe; 1.1.0 printed ASCII `|` there, this version prints `│`.
```

## feed.rs — the status line's live numbers (step B)

```rust
// account_for matches with history::same_reset, whose RESET_JITTER is 60 seconds.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct FeedLimit { pub percent: f64, pub resets_at: DateTime<Utc> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct Feed { pub at: DateTime<Utc>, pub five_hour: Option<FeedLimit>,
    pub seven_day: Option<FeedLimit>, pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cost_problem: Option<String> }

/// Write <data>/feed.json when its numbers differ from the file's — the 5 h and week limits, the
/// session, the cost or the cost problem — or the file is at least 10 s older than feed.at. A file
/// that does not parse is overwritten by this call.
pub fn write_if_due(app: &AppPaths, feed: &Feed) -> io::Result<bool>;
/// Missing file → None; a file that does not parse → InvalidData.
pub fn read(app: &AppPaths) -> io::Result<Option<Feed>>;
/// The one stored account whose cached weekly_all reset is within RESET_JITTER of
/// feed.seven_day.resets_at. None without a seven_day, or when none or more than one matches.
pub fn account_for(feed: &Feed, cache: &UsageCache, accounts: &[Account]) -> Option<String>;
/// usage with the session and weekly-all numbers taken from the feed when it is not older: the
/// session when its reset is new or the same window's percent is not lower, and never when its
/// reset is at or before feed.at; the weekly percent when it is not lower. The week's reset,
/// per-model limits and flags stay.
pub fn merged(usage: &Usage, feed: &Feed) -> Usage;
```

`feed.json` is the shape above. `AppPaths::feed_file()` is `<data_dir>/feed.json`. The status line
writes it; the tray app only reads it.

`session` is the payload's `session_id` and `cost_usd` its `cost.total_cost_usd`, in US dollars. A
cost that is `null` or absent is no cost. A cost that is not a finite number of 0 or more is not
stored: `cost_usd` is `None` and `cost_problem` carries the session and the reason, and the status
line itself is never broken by it. Both keys are absent from a feed written by 1.1.0 and from the file
while they are `None`. The sessions thread keeps the last cost it read per session while that session
is a row, because the feed holds only its last writer; `cost_problem` is a warning on every pass while
it lasts. A converted cost that is not finite is not used: the row keeps the valuer's API value and
the thread names the session. With a currency and no stored rate the cost is not converted: it stays
in USD and is labelled USD, and `take_provider_cost` returns no warning for that.

## burn/mod.rs — constants and shared types (step B, 1.3)

```rust
pub const LEARN_MIN_USD: f64 = 100.0;   // a factor is measured from this much of its own work,
                                        // in API dollars; a prior weighs as much
pub const ASSUMED_WEEK_PERCENT_PER_USD_AT_1X: f64 = 0.75; // a plan of capacity n assumes this / n
pub const MIN_LIVE_SHARE: f64 = 0.9;    // a Claude pair needs the account live for this share
pub const RHYTHM_WEEKS: i64 = 4;        // the rhythm averages 4 weeks; the newer two count double
pub const RATES_DAYS: i64 = 14;         // rates are learned from this many days of samples
pub const STEP_MIN: i64 = 15;           // the plan's step
pub const HORIZON_H: i64 = 168;         // the plan's horizon
pub const MIN_DWELL_MIN: i64 = 5;       // shortest time before an early switch: a runaway guard
pub const WINDOW_LEAVE_PERCENT: f64 = 95.0; // S1 leaves a 5 h window from here for one with room
pub const SLACK_MARGIN_H: f64 = 2.0;    // S2's margin, in typical working hours
pub const MIN_GAIN_H: f64 = 0.5;        // early switching must save at least this
pub const GAIN_FACTOR: f64 = 2.0;       // and at least this many times its extra switches' cost
pub const MIN_COST_SAMPLES: u32 = 3;    // a measured switch cost needs this many switches
pub const FRESH_MIN: i64 = 20;          // a decision needs readings at most this old
pub const SWITCH_RELEARN_MIN: i64 = 1;  // a switch event that lands late is learned at the next tick
pub const STALE_ASK_SECS: i64 = 60;     // stale readings ask for a poll at most this often
pub const LEARN_RETRY_MIN: i64 = 5;     // a failed history read is tried again this much later
pub const SWITCH_RETRY_MIN: i64 = 10;   // the same switch decision is sent again this much later

pub const HOURS: usize = 168;            // private

pub fn switches_per_day(switches: u32) -> f64;   // switches / (HORIZON_H / 24)
/// API dollars as hours at `typical_usd`; None when that is 0.0. The one conversion
/// used by the regime, `burn status`, `burn plan` and `burn_text::hours`.
pub fn usd_hours(usd: f64, typical_usd: f64) -> Option<f64>;
#[derive(Debug, Clone, PartialEq)] pub struct Rhythm { pub usd: [f64; HOURS] }
impl Rhythm { pub fn at(&self, t: DateTime<Utc>, offset: FixedOffset) -> f64;
              pub fn typical_hour(&self) -> f64; } // mean over hours with work; 0.0 if none
#[derive(Debug, Clone, Copy, PartialEq)] pub struct Rates { pub c: Factor, pub s: Option<Factor> }
```

`Rates` holds the fitted factors of the account's week (`c`) and 5 h window (`s`), each a
`burn::fit::Factor`; `s` is `None` without a session limit or before it can be measured or
assumed. The plan uses each factor's `value`; neither is ever 0 or below. In tests and fixtures
`Rates::given(c, s)` makes factors with no range, measured on `LEARN_MIN_USD`.

## burn/fit.rs — the one estimator of a factor and its range (1.3, R2)

```rust
const Z_80: f64 = 1.2816;               // the two-sided 80 % range of a normal distribution
#[derive(Debug, Clone, Copy, PartialEq)] pub struct Pair { pub gain: f64, pub usd: f64 }
#[derive(Debug, Clone, Copy, PartialEq)] pub struct Factor {
    pub value: f64, pub low: f64, pub high: f64, pub basis_usd: f64, pub measured: bool,
}
pub fn factor(pairs: &[Pair], prior: Option<f64>) -> Option<Factor>;
```

`value = (Σ gain + prior × LEARN_MIN_USD) / (Σ usd + LEARN_MIN_USD)` while the account's own dollars
are below `LEARN_MIN_USD` and a prior exists, and `Σ gain / Σ usd` once they reach it or without a
prior: a measured factor drops its prior completely (F-1). `se = sqrt(Σ (gain_i − value · usd_i)² +
(prior · w − value · w)²) / (Σ usd + w)` while the prior counts, with `w` the prior's weight;
`low = max(0, value − 1.2816 · se)`, `high = value + 1.2816 · se`. Once `measured`, value, range and
standard error are computed as if there were no prior. `basis_usd = Σ usd`;
`measured = Σ usd ≥ LEARN_MIN_USD`. `None` without pairs and
without a prior, when the dollars are 0 and there is no prior, or when the value is not above 0: a
measured account whose own gain is ≤ 0 has no factor, never a prior-lifted one. Whether a factor
that is neither measured nor has a prior may be used is the caller's rule. Every
factor in auto-burn and the statistics comes from here.

## burn/rates.rs — rhythm, live account, rates (step B, 1.3)

```rust
pub fn rhythm<Tz: TimeZone + Copy>(tokens: &[Hosted<TokenRecord>], now: DateTime<Utc>,
                                   zone: &Tz, prices: &Prices) -> Rhythm;
pub(crate) fn rhythm_of_company<Tz: TimeZone + Copy>(tokens: &[Hosted<TokenRecord>],
    now: DateTime<Utc>, zone: &Tz, prices: &Prices, company: Company) -> Rhythm;
/// One company's work per hour actually worked (1.4.1): API dollars and tokens of the average
/// local clock hour of [now - 4 weeks, now) that holds its priced work, on any host (an hour
/// counts once). Filters as rhythm_of_company; an hour without work counts nowhere. None
/// without such an hour. The Subscriptions page's hours of work; auto-burn keeps the rhythm.
pub(crate) struct WorkedHour { pub usd: f64, pub tokens: f64 }
pub(crate) fn worked_hour<Tz: TimeZone + Copy>(tokens: &[Hosted<TokenRecord>],
    now: DateTime<Utc>, zone: &Tz, prices: &Prices, company: Company) -> Option<WorkedHour>;
pub(crate) fn hour_shares(record: &TokenRecord,
    hour_of: impl Fn(DateTime<Utc>) -> DateTime<Utc>) -> Vec<(DateTime<Utc>, f64)>;
pub fn account_at(events: &[Hosted<Event>], host: &str, t: DateTime<Utc>) -> Option<AccountRef>;
#[derive(Debug, Clone, PartialEq)] pub struct RateAccount {
    pub account: AccountRef, pub company: Company,
    pub capacity: Option<f64>, pub plan: Option<String> }
impl RateAccount { pub fn of(account: &Account) -> RateAccount;
                   pub fn of_provider(subscription: &ProviderSubscription) -> RateAccount; }
pub(crate) fn main_limit(company: Company) -> LimitKind;
#[derive(Debug, Clone, PartialEq)] pub struct LearnedRates {
    pub rates: BTreeMap<AccountRef, Rates>, pub unpriced: BTreeSet<String> }
impl LearnedRates { pub fn warnings(&self) -> Vec<String>; }
pub fn rates(accounts: &[RateAccount], samples: &[Hosted<Sample>],
             tokens: &[Hosted<TokenRecord>], events: &[Hosted<Event>],
             prices: &Prices, now: DateTime<Utc>) -> LearnedRates;
#[allow(clippy::too_many_arguments)]
pub(crate) fn rates_in(subjects: &[RateAccount], samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>], events: &[Hosted<Event>],
    provider_subscriptions: &[ProviderSubscription], prices: &Prices,
    from: DateTime<Utc>, to: DateTime<Utc>) -> LearnedRates;
pub(crate) struct WindowPools {
    pub(crate) timelines: HashMap<String, Timeline>,
    pub(crate) provider_timelines: HashMap<String, Timeline>,
    pub(crate) openai: BTreeSet<AccountRef> }
impl WindowPools { pub(crate) fn of(events: &[Hosted<Event>],
    provider_subscriptions: &[ProviderSubscription]) -> WindowPools; }
pub(crate) fn sample_lines<'a>(account: &AccountRef, samples: &'a [Hosted<Sample>])
    -> Vec<&'a Hosted<Sample>>;
pub(crate) fn sample_lines_by_account<'a>(subjects: &[RateAccount],
    samples: &'a [Hosted<Sample>]) -> BTreeMap<AccountRef, Vec<&'a Hosted<Sample>>>;
#[allow(clippy::too_many_arguments)]
pub(crate) fn rates_in_with(subjects: &[RateAccount],
    lines: &BTreeMap<AccountRef, Vec<&Hosted<Sample>>>, samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>], pools: &WindowPools, prices: &Prices,
    from: DateTime<Utc>, to: DateTime<Utc>) -> LearnedRates;
pub(crate) enum PairLimit { Main, Session }
pub(crate) fn window_pairs(subject: &RateAccount, lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>], pools: &WindowPools, prices: &Prices,
    limit: PairLimit, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<fit::Pair>;
pub(crate) struct Timeline { /* changes, before */ }
impl Timeline { pub(crate) fn provider_switches(events: &[Hosted<Event>], host: &str) -> Timeline; }
```

`rhythm` is `rhythm_of_company` with `Company::Anthropic`, so auto-burn counts Anthropic records
only. A record's span is spread over the local clock hours it covers, by the minutes each holds
(`hour_shares`), so an offset that is not a whole hour splits it and a daylight-saving change is
followed. `rates` is `rates_in` over the last `RATES_DAYS` days with no provider subscriptions, so
auto-burn's factors are unchanged. `rates_in` fits every subscription over an explicit range, with
provider subscriptions and an explicit `from`/`to`; the Subscriptions page calls it over its range
for every stored subscription whatever chips it shows. `WindowPools` builds the timelines, the
`providerSwitch` timelines and the stored OpenAI subscriptions once per page, and `rates_in_with`
takes them with the sorted `sample_lines`. `RateAccount.company` is `Anthropic` for a Claude
account, and the company of the subscription's org for a provider; a provider has no capacity, so
its week is learned from its own readings. `main_limit` is `weekly_all` for Anthropic and OpenAI,
and `monthly_credits` for Ollama.

`rates` pairs consecutive samples of the same weekly window at most 6 h apart in the last 14 days,
keeps a pair only when the account was live for at least `MIN_LIVE_SHARE` of it (union over hosts),
and counts the work of token records whose host had the account live. A pair that holds an
Anthropic record without a price is left out whole; `LearnedRates::unpriced` names each such model
once and `warnings()` gives the words. A pair whose earlier sample is at 100 % is left out. The
prior of an account and a limit, from the factors of the other accounts measured from their own
pairs alone:
1. pooled over those with the same plan name, `Σ gain / Σ usd`;
2. else pooled over those with a capacity, `Σ (gain × capacity) / Σ usd`, divided by this account's
   capacity;
3. else, week only, `ASSUMED_WEEK_PERCENT_PER_USD_AT_1X` divided by this account's capacity;
4. else no prior: the account has a factor only once it is measured.
A provider pair counts only when every record of its company inside the pair belongs to that
subscription and at least one does (through `openai_subscription_at` or Ollama's one
subscription); a pair no record touches says nothing. Priors stay within a company; rules 2 and 3
are Anthropic's only. `Account::capacity()` reads
`oauthAccount.userRateLimitTier`, else `organizationRateLimitTier` (`store::tier_capacity`:
`_<n>x` → n); plan "Pro" gives 1. `rates_known` needs only `c`: an unknown `s` plays as 0.

## burn/plan.rs — slack, choice and the two plans (step B)

```rust
#[derive(Debug, Clone, PartialEq)] pub struct SessionState { pub percent: f64, pub resets_at: Option<DateTime<Utc>> }
#[derive(Clone)] pub struct AccountState { pub id: String, pub may_use: bool, pub login_ok: bool,
    pub week_percent: f64, pub week_resets_at: DateTime<Utc>, pub session: Option<SessionState>,
    pub model_blocked: bool, pub rates: Option<Rates>, pub thinking_held: u32 }
#[derive(Clone)] pub struct Inputs { pub now: DateTime<Utc>, pub offset: FixedOffset,
    pub accounts: Vec<AccountState>, pub active: Option<String>, pub rhythm: Rhythm,
    pub last_switch_at: Option<DateTime<Utc>>, pub paused_until: Option<DateTime<Utc>> }
impl Inputs { pub fn check(&self) -> Result<(), PlanError>; }   // also LastSwitchAfterNow
pub(super) fn early_move_allowed(last_switch: Option<DateTime<Utc>>,
    paused_until: Option<DateTime<Utc>>, at: DateTime<Utc>) -> bool; // decide and both plans
pub(super) fn early_move_due(early: bool, target_slack: f64, target_shared: f64,
    current_slack: f64, typical: f64) -> bool; // decide and both plans
pub enum PlanError { InvalidPercent{account,limit,percent}, SessionWithoutReset{account,percent},
    UnknownActive{id}, DuplicateAccount{id}, LastSwitchAfterNow{at,now}, OldestReadingAfterNow{at,now} }
pub fn blocked(account: &AccountState, at: DateTime<Utc>) -> bool;
pub fn usable(account: &AccountState, at: DateTime<Utc>) -> bool;       // may_use && !blocked
pub fn usable_again(account: &AccountState, at: DateTime<Utc>) -> Option<DateTime<Utc>>;
pub fn leave_due(inputs: &Inputs, index: usize) -> bool; // S1: blocked, or the window nearly full
pub fn rates_known(account: &AccountState) -> bool;               // c known; s may be missing
pub fn slack(account: &AccountState, inputs: &Inputs) -> Option<f64>;
pub fn shared_slack(account: &AccountState, inputs: &Inputs) -> Option<f64>; // ≤ slack
pub fn choose(inputs: &Inputs, at: DateTime<Utc>) -> Option<String>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Policy { WhenFull, Lazy, Eager }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct Segment { pub account: Option<String>,
    pub from: DateTime<Utc>, pub to: DateTime<Utc> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Fill { Window, Week }
#[derive(Debug, Clone, PartialEq)] pub struct Plan { pub segments: Vec<Segment>,
    pub blocked_usd: f64, pub switches: u32, pub left_at_reset: BTreeMap<String, f64>,
    pub full_at: BTreeMap<String, DateTime<Utc>>, pub full_by: BTreeMap<String, Fill> }
pub fn follow(inputs: &Inputs, policy: Policy) -> Plan;
pub fn followed(inputs: &Inputs, regime: &Regime) -> Plan; // eager while `regime.early`, else lazy
pub(super) fn rhythm_walk(inputs: &Inputs) -> Walk<'_>; // the plan and `decide::latest_reaching`
```

`follow` plays 168 h in 15-minute steps. Every policy leaves the current account when `leave_due`
says so, then calls `choose`. `leave_due` (one copy, `leave_due_of` on the plan's views; `decide`
uses it too) is true when the account is blocked, or when its 5 h window is at
`WINDOW_LEAVE_PERCENT` (95) or more and not over (`View::window_nearly_full`, through
`Limit::reaches_at`), another account is `View::roomy` (usable, window below 95 %) and
`early_move_allowed` says yes. `pick` keeps only the roomy accounts when there is one, so `choose`
passes over a nearly full window; with none it takes every usable account as before. WhenFull ("only when full", for the Forecast and the suggestion's
"Staying on …" sentence) makes no other move. Lazy and eager also move when `choose` names another
usable account whose slack is at least `SLACK_MARGIN_H` × the typical hour below the current one's,
`early_move_allowed` says yes (the last switch at least `MIN_DWELL_MIN` ago and no pause
running) and `early_move_due` says yes: eager always, lazy only when the target's shared slack is at
most `EXPIRY_CUSHION_H` (8) × the typical hour, so it is at risk of losing quota at its reset, alone
or with the accounts that reset before it. `shared_slack` (one copy, `shared_slack_of` on the
plan's views) is, for each weekly reset at or after the account's own, the walk's work until that
reset capped by what the accounts resetting by then can take (`View::room`), minus what they need
to be emptied; the least of these and the account's own slack. It counts the accounts that
`View::shares_work`: auto-burn may use them, login live, no per-model block, week below 100 %, and
known rates. The margin compares the accounts' own slacks, so two accounts at risk together never
take turns. The walk starts from
`Inputs::last_switch_at` and `Inputs::paused_until`, so the plan's first early move is the one
`decide` will make; a replay passes `None` for both. An account without rates is played with `c = 0` and `s = 0`: it never fills
and never causes an early move. An active account with `may_use` false stays until `leave_due`.
`choose` is the usable account with the least slack, ties by the earlier weekly reset and then id;
when any usable account has no rates it uses the fixed order instead (capped accounts with an open
window first, then the rest, each by earlier reset and id). When every usable account has a slack,
the candidates are the accounts within `SLACK_MARGIN_H` × the typical hour of the least slack; among
them the highest `thinking_held` wins, and equal counts (including all 0) fall back to the slack,
reset and id order. So with no thinking held `choose` names exactly the least-slack account as before.
`followed` is the one place the rule "eager while early switching is on, else lazy" lives: it returns
`follow(inputs, Policy::Eager)` when `regime.early`, else `follow(inputs, Policy::Lazy)`. The
Forecast's planned plan, `live::assess` and everything that reads its plan take it from there.

## burn/decide.rs — the regime and the decision (step B)

```rust
#[derive(Debug, Clone, Copy, PartialEq)] pub struct SwitchCost { pub usd: f64, pub samples: u32 }
#[derive(Clone)] pub struct Situation { pub inputs: Inputs, pub cost: Option<SwitchCost>,
    pub tied_busy: u32, pub oldest_reading: DateTime<Utc>, pub sessions_off: bool }
impl Situation { pub fn check(&self) -> Result<(), PlanError>; }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum RegimeWhy { Learning, SessionsOff, NoCostYet, NotWorthIt, Pays }
#[derive(Debug, Clone, PartialEq)] pub struct Regime { pub early: bool, pub why: RegimeWhy,
    pub lazy_blocked_h: f64, pub eager_blocked_h: f64, pub extra_switches: u32, pub cost_h: Option<f64> }
pub fn regime(situation: &Situation) -> Regime;
#[derive(Debug, Clone, PartialEq, Eq)] pub enum Reason { NoneActive,
    ActiveFull { until: Option<DateTime<Utc>> },
    WindowNearlyFull { until: Option<DateTime<Utc>> },  // the active account's window end
    WindowOpen, ResetNear }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum Decision { Stay, Stale,
    Switch { to: String, reason: Reason },
    Wait { to: String, latest: DateTime<Utc>, tied_busy: u32, reason: Reason },
    AllFull { next: Option<(String, DateTime<Utc>)> } }
pub fn decide(situation: &Situation, regime: &Regime) -> Decision;
```

`decide` in this order: a reading older than `FRESH_MIN` → `Stale`; the active account blocked or no
active account → `choose` names a target → `Switch` (`NoneActive` when there was none, otherwise
`ActiveFull`), else `AllFull`; `leave_due` on the active account (its window nearly full, another
roomy account, no pause, the dwell over) → `Switch` to `choose`'s account with `WindowNearlyFull`,
never `Wait` and never `AllFull`; `early_move_allowed` false (a pause that ends in the future, or the last
switch less than `MIN_DWELL_MIN` ago) → `Stay`; an active account auto-burn may use, `choose` names
another account with enough less slack, and early switching is on or that account's shared slack is
at most `EXPIRY_CUSHION_H` × the typical hour (`early_move_due`) → `Switch` (`WindowOpen` when the
target has a 5 h limit, else `ResetNear`), or `Wait` until the work reaches the target's shared
slack when a thinking-tied session is busy; otherwise `Stay`. A target shared slack of 0 or less
switches at once. `regime.early` is `why == Pays`; it needs rates for every account
auto-burn may use, a typical hour, at least `MIN_COST_SAMPLES`, and both gain rules of
`docs/spec.md` §7.4.1. When no rates were learned because `readSessions` is off, `why` is
`SessionsOff`, not `Learning`; the fact travels in `Situation.sessions_off`. `latest_reaching`
walks `plan::rhythm_walk`, so the waiting time and the plan cannot count work differently.

## burn/tied.rs — thinking across a switch (step B)

```rust
pub fn thinking_tied(model_id: &str) -> bool;
```

The ids are: tied `claude-fable-5-2`, `claude-mythos-5-2`,
`claude-sonnet-5-5`, `claude-haiku-5-5`; not tied `claude-opus-5-5`, `claude-opus-5`,
`claude-fable-5-1`, `claude-mythos-5-1`, `claude-fable-5`, `claude-mythos-5`, `claude-sonnet-5`,
and every id starting with `claude-opus-4`, `claude-sonnet-4`, `claude-haiku-4`, `claude-3`. One
trailing `[...]` suffix such as `[1m]` is stripped. A model off the Claude plan (`money::is_plan_model`
false: Harness, Codex, Ollama) is never tied, since a switch does not touch it; any other `claude-` id
is tied.

## burn/cost.rs — what a switch costs (step B)

```rust
pub fn switch_cost(events: &[Hosted<Event>], samples: &[Hosted<Sample>],
                   tokens: &[Hosted<TokenRecord>], rates: &BTreeMap<AccountRef, Rates>,
                   prices: &Prices, range: &Range) -> Option<SwitchCost>;
```

For each `Switch` event in `range`: the target's weekly percent gained between its first and last
sample in the 10 minutes after the switch, minus `c` × the API dollars of work done between those
samples, divided by `c` (the extra dollars the switch cost). A switch whose window holds a record
without a price is left out whole (documented rule), never measured with that work missing; switches
without two samples or without the target's rates are left out too, and a negative result counts as
0. The result is the mean and its sample count, `None` when none remains.

## burn/replay.rs — replay recorded work (step B)

```rust
#[derive(Debug, Clone, PartialEq)] pub struct ReplayResult { pub policy: Policy,
    pub worked_usd: f64, pub blocked_usd: f64, pub switches: u32,
    pub expired: BTreeMap<AccountRef, f64>, pub typical_usd: f64, pub warnings: Vec<String> }
#[derive(Debug, thiserror::Error)] pub enum ReplayError {
    History(#[from] HistoryError), Plan(#[from] PlanError),
    Unplayable { account: String, at: DateTime<Utc>, missing: &'static str } }
pub fn replay(history: &HistoryDir, range: &Range, rates: &BTreeMap<AccountRef, Rates>,
              without: &[AccountRef], policy: Policy, prices: &Prices)
    -> Result<ReplayResult, ReplayError>;
```

The replay reads the token and sample records of `range` from all hosts. Each account starts from
its first sample in the range; a sample without a weekly limit or a weekly reset is
`ReplayError::Unplayable`. Work is read as API dollars of Anthropic records only (R7): an OpenAI or
Ollama line is left out, and a Claude line without a price makes its hour unknown, never 0. Each
line is spread over the UTC hours its `span` covers by `hour_shares`, then over the play's steps.
`without` leaves accounts out; the replay starts with no active account. Ids inside the play are
`"<acc>/<org>"`; `expired` names the full `AccountRef` again. `typical_usd` is the mean of the
recorded hours with work (0.0 when none); the reads' warnings are kept for the caller.

## burn/live.rs — one history read into one situation (step B)

```rust
#[derive(Debug, Clone)] pub struct Learned { pub rhythm: Rhythm,
    pub rates: BTreeMap<AccountRef, Rates>, pub cost: Option<SwitchCost>,
    pub warnings: Vec<String>, pub events: Vec<Hosted<Event>>, pub host: String,
    pub sessions_off: bool }
pub enum Readiness { Ready(Situation), Unread { accounts: Vec<String> }, NoAccounts }
pub fn learn<Tz: TimeZone + Copy>(history: &HistoryDir, now: DateTime<Utc>, zone: Tz,
             accounts: &[Account], read_sessions: bool) -> Result<Learned, HistoryError>;
pub fn situation(learned: &Learned, views: &[AccountView], accounts: &[Account],
                 active: Option<&str>, now: DateTime<Utc>, offset: FixedOffset,
                 tied_busy: u32, thinking: &[(DateTime<Utc>, u32)])
    -> Result<Readiness, PlanError>;
/// `thinking` is the running tied sessions' answers with thinking, minute by minute; every
/// `AccountState.thinking_held` is what the account produced of them (`thinking_held`).
pub fn thinking_held(thinking: &[(DateTime<Utc>, u32)], events: &[Hosted<Event>], host: &str)
    -> BTreeMap<AccountRef, u32>;
pub fn next_account(situation: &Situation, decision: &Decision, plan: &Plan) -> Option<String>;
pub struct Assessment { pub situation: Situation, pub regime: Regime, pub decision: Decision,
                        pub plan: Plan, pub next: Option<String> }
/// regime → decide → followed → next. `cached_regime` is an earlier regime; None computes one.
pub fn assess(situation: Situation, cached_regime: Option<&Regime>) -> Assessment;
```

`learn` reads 4 weeks of samples and tokens and 14 days of events from all hosts, computes the
rhythm, the rates of `accounts` and the switch cost; `read_sessions` off means no rates are learned
and `sessions_off` is set. `situation` puts every account with a reading
into `Inputs` (`may_use` = the account's `auto_burn`, `login_ok` = `loginState == "ok"`), and returns
`Unread` when an account auto-burn may use, or the active one, has no reading and is not logged
out. `paused_until` is the end of this host's latest `autoBurn` pause while it is still ahead; a
resume clears it, and a switch by hand starts none. `assess` takes its plan from `plan::followed`, so the pop-up,
`burn-next.json`, the status line and `status --json` follow the same plan the Forecast shows.
`next_account` is the target of a
`Switch`/`Wait`, otherwise the first account of the plan other than the active one.

## burn changes — going back restores (step C2)

```rust
// burn/plan.rs
pub struct AccountState { /* … */ pub thinking_held: u32 }
fn pick(views: &[View<'_>], at: DateTime<Utc>, walk: &Walk<'_>, typical: f64) -> Option<usize>;

// burn/live.rs
pub fn thinking_held(thinking: &[(DateTime<Utc>, u32)], events: &[Hosted<Event>], host: &str)
    -> BTreeMap<AccountRef, u32>;
pub fn situation(learned: &Learned, views: &[AccountView], accounts: &[Account],
    active: Option<&str>, now: DateTime<Utc>, offset: FixedOffset, tied_busy: u32,
    thinking: &[(DateTime<Utc>, u32)]) -> Result<Readiness, PlanError>;
```

`thinking_held` builds `Timeline::of(events, host)` and gives each minute's count to
`Timeline::at(minute)`; a minute without a known live account is left out, and the map has no entry
for an account without thinking. `situation` looks each account's `AccountRef::of` up there and
fills `AccountState.thinking_held` (0 when none), then passes `tied_busy` through unchanged.

`pick`, the one choice `choose` and the plan's steps use, keeps its old branch when any usable
account has no slack: the fixed order. In the branch where every usable account has one, the
candidates are the accounts whose slack is at most `SLACK_MARGIN_H × typical` (plus the `1e-9`
tie noise) above the least; the highest `thinking_held` wins, and equal counts — including all 0 —
fall back to slack, weekly reset and id, exactly as before. `decide`'s early switch (step 5) still
requires the chosen target to be at least `SLACK_MARGIN_H × typical` below the active account, so a
thinking account inside that margin keeps auto-burn on the active one until it is that far below or
another account is clearly more urgent. With `readSessions` off no rates are learned, `pick` takes
the fixed-order branch, and nothing is held back.

The list reaches the decision from the sessions thread: `PassOutcome.thinking` →
`EngineCommand::ModelsInUse { models, tied_busy, thinking }` → `EngineRuntime.thinking` (emptied
with `tied_busy` when `models` is `None`) → `BurnTick.thinking` → `live::situation`. The CLI's
`burn plan` and `status` pass `&[]` because they do not know the running sessions, and the replay
sets every `thinking_held` to 0, so both plan as before. `Timeline::of` is `pub(super)` and
`Timeline::at` is `pub(crate)` for this one reader.

## engine.rs additions — feed, stale poll, 30 s near full (step B)

```rust
pub(crate) const NEAR_FULL_POLL_SECS: u64 = 30;
pub enum PollReason { Timer, PanelOpened, AfterSwitch, Wake, Stale }
pub struct CacheEntry { …, #[serde(default)] pub fed_at: Option<DateTime<Utc>> }
impl<R: Remote> Engine<R> {
    pub fn apply_feed(&mut self, feed: &Feed, now: DateTime<Utc>) -> Result<Option<String>, EngineError>;
}
```

`apply_feed` matches the feed's account with `feed::account_for`, ignores a feed at or before the
entry's `fetched_at` or `fed_at`, merges with `feed::merged`, writes the cache and, through the
recorder, one `Sample` with `src: statusline`. `AccountView.fetched_at` is the later of
`fetched_at` and `fed_at`. `PollReason::Stale` polls only an account whose last reading or attempt
is older than `FRESH_MIN`; the interval timer is not used for it. `NEAR_FULL_POLL_SECS` replaces
`pollActive` while the active account's binding limit is at least 90 % and below 100 %.

## app: statusline (step B)

```rust
pub(crate) struct Payload { workspace: Option<Workspace>, model: Option<Model>, effort: Option<Effort>,
    rate_limits: Option<RateLimits> }
pub(crate) struct Workspace { current_dir: Option<String> }
pub(crate) struct Model { id: Option<String>, display_name: Option<String> }
pub(crate) struct Effort { level: Option<String> }
pub(crate) struct RateLimits { five_hour: Option<RateLimit>, seven_day: Option<RateLimit> }
pub(crate) struct RateLimit { used_percentage: f64, resets_at: i64 }
pub(crate) fn parse(stdin: &[u8]) -> Payload;                 // any parse error → Payload::default()
pub(crate) fn feed_from_payload(payload: &Payload, now: DateTime<Utc>) -> Option<Feed>;
pub(crate) fn publish_feed(app: &AppPaths, settings: &Settings, payload: &Payload,
                           now: DateTime<Utc>) -> Result<(), String>;
pub(crate) fn facts(app: &AppPaths, payload: &Payload, file: &AccountsFile, live: &LiveState,
                    cache: &UsageCache, home: &Path, user: String,
                    now: DateTime<Utc>) -> Result<Facts, String>;

pub(crate) struct Facts { pub user: String, pub host: String, pub home: String,
    pub now: DateTime<Utc>, pub account: Option<AccountFacts>, pub five_hour: Option<Limit2>,
    pub week: Option<Limit2>, pub model_limits: Vec<(String, f64)>,
    pub burn_next: Option<(String, DateTime<Utc>)>, pub git_branch: Option<String> }
pub(crate) struct AccountFacts { pub abbr: String, pub color: String, pub plan: Option<String> }
pub(crate) struct Limit2 { pub percent: f64, pub resets_at: Option<DateTime<Utc>> }

pub(crate) fn render_spans<Tz: TimeZone>(config: &StatuslineConfig, payload: &Payload, facts: &Facts,
    width: Option<u16>, tz: &Tz) -> (Vec<Span>, Vec<u8>) where Tz::Offset: Display; // step C4
pub(crate) fn ansi(spans: &[Span]) -> String; // step C4: the old `render`, byte for byte
pub(crate) fn visible_width(text: &str) -> usize;
pub(crate) fn compact_left(seconds: i64, week: bool) -> String; // also used by burn_text::duration
pub(crate) fn clock(at: DateTime<Utc>, now: DateTime<Utc>, offset: FixedOffset) -> String; // §7.4.5
pub(crate) fn weekday_clock(at: DateTime<Utc>, offset: FixedOffset) -> String;            // §7.4.5
pub(crate) fn git_branch(cwd: &Path) -> Option<String>;
pub(crate) fn terminal_width() -> Option<u16>;
```

`facts` shows the payload's numbers only when the feed belongs to the live account; without a
matching feed it shows the cache. `burn_next` comes from `burn_next::read(app, now)` mapped through
the stored accounts to their abbreviations: a missing file, an unknown account id or an old `written`
is `None`, and an unreadable file is the line's error. `publish_feed` does nothing when
`statusline_feed` is off or the payload
has no `rate_limits`/`seven_day`; when `write_if_due` wrote, it sends `feed` to the tray app (no
socket is not an error). `render_spans` is pure; `facts` is the one part that reads `.git/HEAD`.
A terminal cell count treats East Asian ambiguous characters as one cell; a terminal that draws them
wide (some CJK settings) therefore makes the fitted line a little too long. That is a known limit,
not a bug to fix with a second table.

`width` walks the `/proc` ancestors' pseudo-terminals (Linux, `rustix::termios::tcgetwinsize`) and
falls back to `$COLUMNS`, else `None`. `format::clock` is the local `HH:MM` today and `Mon HH:MM`
when not today; `weekday_clock` is always `Mon HH:MM`; both are the §7.4.5 texts and moved here from
`burn_text` so the line and the auto-burn words share them. `compact_left` rounds minutes up and
calls `duration_text` (`2h 31min`, `3d 16h`); `week` chooses days/hours, otherwise hours/minutes.

## app: burn_text (step B)

```rust
#[derive(Debug, thiserror::Error)] pub enum TextError { ViewMissing { account: String },
    ResetMissing { account: String }, FillMissing { account: String }, NoActive }
#[derive(Debug, Clone, PartialEq, Eq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct SwitchSuggestion { pub to: String, pub text: String }
pub fn abbr(views: &[AccountView], id: &str) -> Result<String, TextError>;
pub fn unread_line(views: &[AccountView], accounts: &[String]) -> Result<String, TextError>;
pub fn no_accounts_line(accounts: &[Account]) -> Vec<BurnPart>;
pub fn why(why: RegimeWhy) -> &'static str;
pub fn segment_line(account: Option<&str>, views: &[AccountView], from: DateTime<Utc>,
                    to: DateTime<Utc>, now: DateTime<Utc>, offset: FixedOffset) -> Result<String, TextError>;
pub fn policy_name(policy: Policy) -> &'static str; // "Only when full" | "Lazy" | "Eager"
pub fn replay_report(result: &ReplayResult, accounts: &[Account]) -> Result<String, TextError>;
pub fn reason(reason: &Reason, target: &str, active: Option<&str>, views: &[AccountView],
              now: DateTime<Utc>, offset: FixedOffset) -> Result<String, TextError>;
pub fn line(plan: &Plan, active: Option<&str>, views: &[AccountView],
            now: DateTime<Utc>) -> Result<Option<Vec<BurnPart>>, TextError>; // feature "gui"
pub fn planned(plan: &Plan, active: Option<&str>) -> BTreeMap<String, DateTime<Utc>>; // feature "gui"
pub fn switch_notification(decision: &Decision, active: Option<&str>, views: &[AccountView],
    now: DateTime<Utc>, offset: FixedOffset) -> Result<Option<String>, TextError>; // feature "gui"
pub fn suggestion(decision: &Decision, inputs: &Inputs, active: Option<&str>, views: &[AccountView],
    now: DateTime<Utc>, offset: FixedOffset) -> Result<Option<SwitchSuggestion>, TextError>;
```

Every text is here; the tray app and the CLI do not keep a second copy. Missing values are
`TextError`, never a guessed abbreviation or time. `unread_line` is
`Waiting for a first reading of AC, LB.` in account order; `no_accounts_line` is
`Save an account to start.` without a stored account, else `No account auto-burn may use has a working login.` `why(SessionsOff)` is
`reading sessions is off: no rates are learned`. `suggestion` returns `Some` only for `Switch`
and `Wait`, and chooses the plan for its "Staying on … is expected to let … expire" sentence
itself: `follow(inputs, Policy::WhenFull)`, the plan that switches only when full (`leave_due`), never the plan
auto-burn follows (which moves early to an account at risk of losing quota). It adds that sentence only when
that plan leaves more than 1 % of the target's week at its reset.

## app: auto-burn in the tray app (`gui/burn_rt.rs`, step B)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct WaitingSwitch { pub to: String, pub latest: DateTime<Utc>, pub tied_busy: u32 }
#[derive(Debug, Clone, Default, Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct BurnView { pub on: bool, pub paused_until: Option<DateTime<Utc>>,
    pub early: bool, pub why: String, pub line: Option<Vec<BurnPart>>,
    pub planned: BTreeMap<String, DateTime<Utc>>, pub waiting: Option<WaitingSwitch>,
    pub suggestion: Option<SwitchSuggestion>, pub measured: Option<Measured> }
pub(crate) struct BurnTick<'a> { pub app: &'a AppPaths, pub history: Option<&'a HistoryDir>,
    pub views: &'a [AccountView], pub accounts: &'a [Account], pub settings: &'a Settings,
    pub active: Option<&'a str>, pub now: DateTime<Utc>, pub offset: FixedOffset,
    pub tied_busy: u32, pub thinking: &'a [(DateTime<Utc>, u32)] }
#[derive(Debug, Default)] pub(crate) struct BurnReport { pub view: Option<BurnView>,
    pub error: Option<String>, pub warnings: Vec<String>, pub notification: Option<String> }
pub(crate) struct BurnRuntime { /* learned, last learn/regime, last sent switch, view */ }
impl BurnRuntime {
    pub(crate) fn new(action: Sender<ActionCommand>, engine: Sender<EngineCommand>) -> BurnRuntime;
    pub(crate) fn force_learn(&mut self);
    pub(crate) fn after_snapshot(&mut self, tick: &BurnTick<'_>) -> BurnReport;
}
```

`after_snapshot` learns at the first tick, once an hour, after a switch, after a pause, when the
active account changes, and `LEARN_RETRY_MIN` minutes after a failed read. It then builds the
situation and calls `live::assess` with the cached regime when the regime is not due, so the tray
app, `burn status` and `burn plan` take the same steps in the same order. Only `auto_burn` on sends
`ActionCommand::Switch { by: By::Auto, reason: Some(text) }`;
`Wait` fills `view.waiting`; `AllFull` is left to the step A notification; `Stale` asks
`EngineCommand::Poll(PollReason::Stale)`, at most once per `STALE_ASK_SECS`. An `Unread` situation
keeps the strip with `burn_text::unread_line` as its line and still asks the stale poll;
`NoAccounts` keeps it with `burn_text::no_accounts_line`. With `auto_burn` off and
`suggest_switches` on, `Switch` and `Wait` fill `view.suggestion`. The switch is retried after
`SWITCH_RETRY_MIN` minutes; the notification is shown only after the reply says the switch landed.
`publish_next` writes `<data>/burn-next.json` (`Switch`: `to` and now; `Wait`: `to` and `latest`;
otherwise the next account of the plan auto-burn follows and its first use), removes it when
auto-burn is off, paused or nothing is planned, and rewrites at least every
`burn_next::REWRITE_AFTER_MIN` minutes.
The runtime hands its learned warnings to the worker, which prints them through the app's one
`worker::PrintedWarnings`. `BurnView` is part of `Snapshot.burn`; the
settings toggle calls `worker::save_setting` and writes `EventKind::AutoBurnSet`, the pause calls
`ActionCommand::PauseBurn { hours }` and writes `EventKind::AutoBurn`.

## app: CLI — `burn` and `status` (step B)

`seatswap statusline` is handled before the context is opened: it never opens the store's secret
backend, takes no lock and writes no history. The other additions:

- `seatswap status --json` reads the history once through `live::learn` and `live::situation` and
  assembles through `live::assess`; a history or planner error becomes `burnError`, never a failed
  command. The plain, waybar and i3blocks forms do not read the history.
- `seatswap burn plan [--json]` uses the same read and `live::assess` with `tied_busy = 0` and no
  thinking (the CLI has no session count and does not know the running sessions). Blocked hours come
  from `usd_hours` on the lazy and eager `blocked_usd`. An
  account auto-burn may use (or the active one) with a login and no reading is an error
  `no reading for …`; no account at all is `no account has a login with a reading`.
- `seatswap burn simulate --from D --to D [--without ABBR]…` maps the two local days to a range
  ending at the local midnight after `--to`, learns the rates from the history as of now, and
  replays `Policy::Lazy` and `Policy::Eager`. Both replays read the same files; each warning is
  printed once on standard error. Only Anthropic records are replayed: `replay_records` reads the
  work of `Company::Anthropic` and leaves an OpenAI or Ollama line out (R7).
- `seatswap next` by an agent with `auto_burn` on returns
  `seatswap: auto-burn is on and decides` and exit code 0 without switching or touching a login;
  the agent call is recorded like any other.
- The gate: `statusline` is allowed for every caller and never recorded; `burn plan` and
  `burn simulate` follow `agents_read` like `list` and `status`.
- The bundled `agent-skill` text adds the `views[].slackHours`, `autoBurn` and `next` rows and
  rule 1: "If `autoBurn.on` is true, do not switch: auto-burn decides."

### `status --json`

```json
{"views":[{"id":"…","label":"…","abbr":"AC","color":"#3b82f6","email":null,"plan":"Team",
  "autoBurn":true,"isActive":true,"loginState":"ok","loginEndsAtMs":null,"loginDaysLeft":null,
  "usage":{…},"usable":true,"blockedUntil":null,"fetchedAt":"2026-11-15T09:41:12Z",
  "lastError":null,"slackHours":3.5}],
 "active":"…","suggestion":{"toId":"…","text":"Switch to AC now: …"},
 "autoBurn":{"on":false,"pausedUntil":null},"next":"…","burnError":null}
```

`views[]` is `AccountView` serialized flat with the same fields as `list --json`, plus
`slackHours` only when known. `blocker` is `#[serde(skip)]` and never appears. `suggestion` is
`{toId, text}` or null and only when auto-burn is off and `suggestSwitches` is on. `next` is the
`live::next_account` answer or null. `burnError` is null unless the history read failed; then
`next` is null and no view has `slackHours`.

### `burn plan --json`

```json
{"regime":{"early":true,"why":"switches early whenever it pays","blockedLazyH":2.0,
  "blockedEagerH":0.0,"extraSwitches":4,"costH":0.1},
 "segments":[{"account":"…","from":"2026-11-15T07:00:00Z","to":"2026-11-15T09:00:00Z"},
             {"account":null,"from":"…","to":"…"}],
 "leftAtReset":{"…":12.0}}
```

`blockedLazyH` and `blockedEagerH` are null when the rhythm has no typical hour (no hours can be
computed); `costH` is also null while no switch cost is known. `account` is null for a stretch with
nothing usable. The text form is the `why` words followed by one `segment_line` per segment.

## app: worker and panel additions (step B)

```rust
pub(crate) enum EngineCommand { …, Feed, HistoryChanged } // Feed: apply feed.json; HistoryChanged: learn again
pub(crate) enum ActionCommand { …, PauseBurn { hours: Option<i64> } } // Some: pause; None: resume
#[tauri::command] pub(crate) async fn set_auto_burn(state: State<GuiState>, on: bool) -> Result<(), String>;
#[tauri::command] pub(crate) fn pause_burn(state: State<GuiState>, hours: Option<i64>) -> Result<(), String>;
```

`SaveSetting` merges one field into the stored settings; only a change of `auto_burn` writes
`EventKind::AutoBurnSet`. `PauseBurn` refuses `hours <= 0`, writes `EventKind::AutoBurn { on, paused_until }` and sends
`EngineCommand::HistoryChanged`. The engine reads `feed.json` on `Feed` and calls `apply_feed`; it
does the same once at start, so a feed written while no tray app ran is not lost. The pop-up's
`set_auto_burn` sends `SaveSetting { field: "autoBurn", value: on }` (one write path), and `pause_burn`
is the Pause 4h / Resume button.

## Files step B writes

- **`<data folder>/feed.json`** (§7.6): atomic, mode 0600, written by `seatswap statusline` only
  when `statuslineFeed` is on, the payload has a week, and the numbers changed or the file is 10 s
  old. The tray app only reads it.
- **`<data folder>/burn-next.json`** (§7.6): atomic, mode 0600, written by the tray app's auto-burn
  loop with `{"account","at","written"}`; rewritten at least every 10 minutes while a switch is
  planned, and removed when auto-burn is off, paused or nothing is planned. `seatswap statusline`
  only reads it.
- **History events** in `<root>/<host>/events-YYYY-MM.jsonl`:
  - `{"t":"…","type":"autoBurn","on":true,"pausedUntil":"2026-11-15T13:10:00Z"}` for a pause;
    `pausedUntil` is absent (not null) for a resume. `on` is the setting at that moment.
  - `{"t":"…","type":"autoBurnSet","on":true}` when a settings save turns auto-burn on or off.
    It is not a pause or a resume.
  - every switch, including an automatic one, is a `switch` event with `by` (`"user"`, `"cli"`,
    `"auto"`, `"agent"` or `"external"`) and, for an automatic switch, the reason text in
    `reason`, plus `sessions` when known: `{"t":"…","type":"switch","to":{"acc":"…","org":"…"},
    "by":"auto","reason":"AC is full until 14:40","sessions":{"running":3,"busy":1,
    "tied":1}}`. `from`, `cwd`, `waitedSecs` and `tied` are omitted when unknown.
- **History samples**: a merged feed writes a `sample` line with `"src":"statusline"`; its limits,
  account and time are the same shape a poll writes.


## Step C1: the large window, Settings, money and projects

The new modules, each with one job:

| Module | Job |
|---|---|
| `app::gui::window` | the large window's label, areas, target grammar and opener |
| `app::gui::lookup_rt` | the look-up thread: models.dev and Frankfurter, the money view, the price rows |
| `core::money` | API prices with dates, the dollar rate, the look-ups, the value of token records |
| `core::projects` | projects, joins and organizations from the session records and the shared project log |
| `app::ui::common.js` | the page helpers both windows use |
| `app::ui::window.js`, `areas/settings.js`, `areas/sessions.js` | the window's tabs and areas, the Settings and Sessions pages |

## gui/window.rs and the page helpers (step C1)

```rust
#[cfg(feature = "gui")]
pub(crate) const WINDOW_LABEL: &str = "main";
pub(crate) const AREAS: [&str; 4] = ["sessions", "statistics", "statusline", "settings"];

/// "<area>" or "<area>/<sub>", the sub optionally "<name>?<item>"; a-z, 0-9 and '-' only.
pub(crate) fn parse_target(target: &str) -> Result<(&str, Option<&str>), String>;

/// Create (1280 × 800, resizable, decorated, in the taskbar, not always on top) or focus
/// the window, and send it the target as `open-area` (an existing window) or as its
/// address `window.html#<target>` (a new one). The new window's address is set by an
/// `initialization_script` before the page's scripts run, so it never draws Sessions
/// first and jumps. A window that cannot be built or shown is an error the caller shows.
#[cfg(feature = "gui")]
pub(crate) fn show(app: &AppHandle, target: &str) -> Result<(), String>;
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn open_window(app: AppHandle, target: String) -> Result<(), String>;
// every target goes to `show`; the pop-up has its own command:
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn open_panel(app: AppHandle);
// calls `gui::show_panel(&app, "main")`, the one function the tray uses.
```

`parse_target` accepts `settings`, `settings/accounts`, `sessions?<session-id>` (and a sub-area with
`?<item>`); it refuses an unknown area, an item directly after an area other than `sessions`,
an empty sub
(`settings/`), a second `/`, a character outside `a-z 0-9 -` (also in the item after `?`), and an
empty item. `show` refuses the same targets before it touches a window. `gui::panel::open_panel` is
the page's way to open the pop-up: it calls `gui::show_panel(&app, "main")` (the function the tray
and the notifications use), and the Sessions page's "Switch…" calls it; `open_window` has no
special target.
The pop-up keeps the event name `navigate`; the large window has `open-area`, because `emit`
reaches every window.

The CLI and the IPC use the same grammar: `ipc::window_line(target)` is `window <target>`;
`ipc::send_window` writes that line and `parse_command` accepts it only when `parse_target` does.
`Command::Window { target: Option<String> }` defaults to `sessions` and validates before sending.
`seatswap panel settings` sends `window settings` (`cli::panel_settings_line`); the tray menu's
"Settings…" and "Sessions…" call `show(app, "settings")` and `show(app, "sessions")`, and
"Statistics…" calls `show(app, "statistics")`.

`crates/app/ui/common.js` is the pop-up's helpers moved out of `panel.js`, plus the controls both
pages use. Exported: `PALETTE`, `TRAY_GREY`, `LEVEL_COLORS`, `TAURI_GLOBAL`, `HAS_TAURI`, `isObject`,
`clamp`, `clone`, `errorMessage`, `isColor`, `hexToRgb`, `rgba`, `rgbToHex`, `mixHex`,
`relativeLuminance`, `contrastText`, `limitLevel`, `shortPercent`, `clear`, `setStyles`, `applyProps`,
`appendChildren`, `h`, `banner`, `applyTheme`, `followTheme`, `invoke`, `listen`, `money`, `tokens`, `textInput`,
`selectInput`, `switchControl`, `sanitizeAbbrChar`, `abbrLetterBoxes`, `colorSwatches`, `hexField`,
`iconPreview`, `foldMemory`, `valueNode`, `valueText`, `subscriptionSplit`. `foldMemory()` is one
page's fold memory (R7): `isOpen(key, defaultOpen)` and `set(key, open)`, a key the user never
folded answers its default and a fold lasts while the window is open; Sessions' folds and Projects'
folders use it. `valueNode(values, unit, viewCurrency)` draws one `Values` half: the amount in its
own currency, `—` with the reason when missing, and `≥ amount` with the reason when the half is
partial. `valueText` returns `{ text, title, partial }` for the same half, and `valueNode` wraps it in
one `span`. `subscriptionSplit(parts, views, tip)` draws a
bar split by subscription in the chips' colours, the `null` part grey and striped (`stats-split-unknown`)
with the tooltip the caller gives, the one split for the Overview's project rows and Statistics →
Projects.
On/Off word (or `{ words: false }` without it) and, with `{ popup: true }`, the pop-up's `.switch`;
the caller saves and reverts it through `set`. `followTheme(getSetting)` is the one system light/dark follower both pages call; it
applies `applyTheme("system")` on a change while `getSetting()` is "system". `money(amount, currency)` gives `"1,234 EUR"` for `(1234, "EUR")` and two decimals
below 10, `"6.47 EUR"`, and throws on a negative or non-finite amount or a currency that is not
three capital letters; `tokens(n)` returns `"1.54M"`, `"683K"`, `"845"` or `"0"` (three significant
digits, units K/M/B, a rounding carry moves to the next unit) and throws on an invalid count;
`showTokens(node, n)` writes that text into a cell and sets `tokens-small` (common.css
`--tokens-small`, `--tokens-small-accent`) below a million; `modelName(null)` is "no answers yet" and
any other value that is not a model id throws; `plural(count, one, many)` is the one "1 host / 2
hosts". `window.html`, `window.css` and `window.js` are the second page; `window.js` reads the area
from `location.hash`, draws the tabs, imports `./areas/<area>.js` on demand and calls its
`render(container, { sub, snapshot, invoke, listen, fromOpen })`. A module that fails to import
shows "This page could not be loaded: <the error's message>" in the area and keeps the reason in
the console; a module that throws while rendering shows its error in the area. A `snapshot`
calls the current area's `update(snapshot)` when it has one and has rendered, and `render` only for
an area without `update`. The old content stays until the area replaces it. `fromOpen` is true when
an `open-area` request caused the render, so an item it names is applied even when the address
already names the same item. An area module may also export `unmount()`; the window calls it before
switching to another area, and `areas/sessions.js` uses it to remove its `sessions-view` listener and
send `sessions_unsubscribe`. `crates/app/capabilities/default.json` names both windows:
`"windows": ["panel", "main"]`.

`areas/sessions.js` exports `render(container, context)`, `update(snapshot)` and `unmount()`. Its
three kept choices are `seatswap.sessions.closed`, `seatswap.sessions.host` and
`seatswap.sessions.grouping`; an unknown stored value is removed and the page opens at `None`, all
hosts and By project. It calls `sessions_view` then `sessions_subscribe` on entry and
`sessions_subscribe` then `sessions_view` on a choice change; `sessions-view` events update the
page in place, keeping open details and the scroll. `sessions?<session-id>` opens and marks that
session, and an id that is not listed says "that session is no longer listed". A running row that
leaves the live list is held grey for 60 s, then removed; closed rows are not held. Numbers come
from `SessionsView.sums` by key, values follow `values`' missing reasons and currency, and times and
ages come from `time_texts`; the page never adds values. One function, common.js's `sessionName(item)`, names a
session wherever a page names one (the Sessions rows, Activity's largest sessions): its `title`, or
the `label` when it has none; the title is cut by CSS, and its hover text is the whole title with the
label on the next line. One fold memory (`folds`, common.js's `foldMemory`) holds the user's folds of
sessions, agents and completed lines; an agent is open by default, a completed line folded. An item without a label is an error. The row signature holds the
title, so a new title redraws the row.

## The Settings page and its save queue (step C1)

`areas/settings.js` exports `render(container, context)` and `update(snapshot)`; the eight groups
are `general`, `accounts`, `auto-burn`, `notifications`, `projects`, `agents`, `providers`,
`history`. The address is `settings/<group>[?<item>]`, with `groupFrom` accepting the aliases `gen`,
`acc`, `autoburn`/`burn`, `noti`, `proj`, `prov`, `hist`; an item opens that account's editor (and
an `open-area` request applies it even when the address already names it). A group click writes
`settings/<group>` and the `hashchange` redraws it.

`update(snapshot)` stores the snapshot and its settings the way `takeSettings` does, then redraws
the open group in place from the stored views. It sends no save. While an input, select or textarea
inside the area has focus it draws nothing; the next snapshot after that field lost focus draws.
The only view it reads is the projects view, when the snapshot's list of account ids changed and
the Accounts group is open. `render` reads money (`loadMoney`), the
price list, projects (`loadProjects`), history (`loadHistory`), the agent skill and the agent calls
only when their group is opened. A save or edit that changes a view (the money rows, a price,
a project edit, a saved host setting, the agent skill) reads that view again; `listenMoney` handles
`UiEvent::Money`, and the price list is also read then while it is open. Every view keeps a request
token so a slow reply cannot overwrite a newer one (`moneyToken`, `priceToken`, `projectsToken`,
`historyToken`, `skillToken`, `callsToken`). A render or update keeps the old content until the new
one replaces it, so the area is never blank while a view loads.

**Money and the price cell.** In General → Money, `rateRow` shows the rate in effect with its ECB
date, its source ("European Central Bank via Frankfurter") and when it was fetched; for USD it says
"USD itself"; without a rate it says "no rate yet: API values are shown in USD", and
`state.money.rateError` is shown under that row. There is no own rate and no own price: the row is
read-only, and with the look-ups off the API column stays in US dollars and says so.
`apiPricesRow` shows how many models have a price, when they were checked and the last change; the
model list itself is read-only under Providers and its `leftOut` line names the models models.dev's
list left out in its `title`. In the Accounts group the price cell shows the amount with
`row.priceCurrency`; when that differs from the settings' currency it shows a warning mark whose
tooltip is "in EUR: enter it again for CHF", and entering the same amount again saves it in the new
currency.

The page does not repeat the core's checks. A number field sends its number, an empty field `null`,
and text that is not a number as the text; a text setting sends its text or `null`; the core's
`Settings::validate`, `store::validate_price`, `validate_abbr` and `validate_color` refuse a bad
value, and its error stays under the row like any refused save. `textField(field, label, { placeholder, note,
restart, width })` is the one text-setting field: `restart` shows "takes effect after the tray app
restarts" instead of `note` and reads the history view again after a save. In a list a project's
name comes from `projectLabel(name, key)`: a folder id with a host adds " · <host>", so the two home
folders read "~ (home folder) · atlas" and "~ (home folder) · vega". The General card's foot carries
the pop-up's hotkey hint: "To open the pop-up with a key, bind `seatswap panel` to a key in your
desktop's keyboard settings."

The save queue is the page's one write path, in this order:

```js
function queuedSave(run)            // every save runs on `state.saving`, one after another
function saveField(field, value)    // a setting: invokes save_setting with one field and its value
function saveAccount(account, patch, field) // invokes edit_account { key, ...patch }; the patch is one of label, abbr, color, autoBurn, price
async function saveNow(field, value) // refuses without state.settings; keeps the server's error text
```

`state.pendingSaves` counts the queued and running saves. `takeSettings(snapshot)` ignores the
snapshot's settings while it is above 0, so a poll cannot undo a save that has not answered yet.
`saveNow` sends `save_setting(field, value)` and takes `settings` from the reply as the page's copy
(`state.settings` and `state.snapshot.settings`); the reply's `warning` ("settings saved; start at
login could not be set: …") keeps the saved values and is shown under the row, because only the
autostart entry failed. A refused save leaves the error under its row, and `setting()` builds that
error line from `state.errors[field]`, so the next redraw keeps it until that field saves
successfully; the caller puts the control back to `savedValue(field)`. The worker merges one field
under the app lock (`merge_setting`): the field must be a key of the settings' camelCase JSON
object, the value replaces only it, the result is deserialised and validated
(`Settings::with_setting`, since 1.2.0 the one way to change a setting by its key), and a typed `~`
or `~/` in `claudeConfigDir` or `historyDir` is expanded with `format::expand_tilde`. `label`, the
chip's letters and colour, `autoBurn` and `price` save through `edit_account`; `price` is sent as a
number, the typed text when it is not a number, `null` to clear it, or left out to keep it.

## store.rs additions — plan, price and the money settings (step C1)

```rust
pub struct Account {
    // …
    pub plan: Option<String>,   // plan_label(login, identity); serde default missing = None
    #[serde(default)]
    pub price: Option<f64>,     // per month, in Settings.currency; missing = None
    #[serde(default)]
    pub price_currency: Option<String>, // the currency `price` was entered in; missing = None
}
pub struct Settings {
    // …
    #[serde(default = "default_notify_login_ends")] pub notify_login_ends: bool, // true
    #[serde(default)] pub currency: Option<String>,                             // None
    #[serde(default = "default_look_up_prices")] pub look_up_prices: bool,      // true
}

pub struct LoginNote { pub changed: bool, pub plan_changed: bool, pub previous_plan: Option<String> }
impl Account { pub fn note_stored_login(&mut self, login: &Login) -> LoginNote; }

/// The identity's planDisplayName, else Claude's current name from PLAN_NAMES
/// (max+5x "Max", max+20x "Ultra", team+5x "Team Premium"), else subscriptionType
/// capitalised plus its level: "_<n>x" → " <n>x" ("Enterprise 20x"); another tier →
/// " (<tier without default_claude_>)"; no tier → nothing.
pub fn plan_label(login: &Login, identity: &OAuthAccount) -> Option<String>;
/// A price is None or a finite number above 0; anything else is StoreError::InvalidPrice.
pub fn validate_price(price: Option<f64>) -> Result<Option<f64>, StoreError>;
```

`Settings::validate` refuses a currency that is not three capital letters; there is no own rate and
no stored dollar rate in the settings. An older `accounts.json` without `price`, `priceCurrency`,
`notifyLoginEnds`, `currency` or `lookUpPrices` loads with `None`, `None`, `true`, `None`, `true`;
the removed `usdRate` key in `accounts.json` and `overrides` key in `prices.json` are ignored
silently, and the next write leaves them out.

`switcher::edit_account` takes the price as `Option<Option<f64>>` (`None` keeps it, `Some(None)`
clears it, `Some(Some(v))` sets it after `validate_price`) and returns
`Edited { before, account }` (1.2.0; `Edited::price_changed()` compares the two). Under the same app lock and from the same load of
`accounts.json`, setting a price stores `account.price_currency = settings.currency`; clearing it
clears the currency too. Setting a price while no currency is set is refused with
`SwitchError::PriceWithoutCurrency` ("set a currency in Settings → General before a price"), and
nothing is written. A save of the same amount counts as a change when `price_currency` differs from
the settings' currency, so a `PriceChanged` with the new currency is recorded. The panel sends
`price` as JSON: absent, `null`, or a number, because `Option<Option<f64>>` cannot tell absent from
null over JSON. Since 1.2.0 the worker and `seatswap edit` write an edit's events through
`ops::record_edit`, which writes `shared::edit_events(&edited)`: `AccountLook` with the changed look
fields and `EventKind::PriceChanged { account, amount, currency }` only when `price_changed()`, with
`currency` taken from `edited.account.price_currency`. `ops::record_plan_change`
writes `EventKind::PlanChanged` from a `switcher::PlanChange { account, from, to }` after a switch
(CLI or app), a live sync or the mirror.

## api.rs additions (step C1)

```rust
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
pub const FRANKFURTER_URL: &str = "https://api.frankfurter.dev/v1/latest";
const MAX_MODELS_DEV_BYTES: usize = 32 * 1024 * 1024;

pub enum Fetched<T> { NotModified, New { etag: Option<String>, body: T } }

impl Api {
    /// GET the list with `If-None-Match: <etag>` when there is one; 200 → New with the reply's
    /// ETag; 304 → NotModified; a body over 32 MiB (Content-Length or read) is ApiError::Decode.
    pub fn models_dev(&self, etag: Option<&str>) -> Result<Fetched<serde_json::Value>, ApiError>;
    /// GET "<base>?base=USD&symbols=<currency>" and read {date, rates:{<currency>: rate}}.
    /// A currency that is not three capital letters, a missing date or rate is ApiError::Decode.
    pub fn usd_rate(&self, currency: &str) -> Result<(NaiveDate, f64), ApiError>;
    /// Tests only: point the two look-up URLs at a local server.
    pub fn with_lookup_urls(mut self, models_dev: &str, frankfurter: &str) -> Api;
}
```

Both requests carry `User-Agent: seatswap/<version>` and no other header of the account's; the
look-ups never send a token or any user data. All HTTP still goes through `Api`.

## money (step C1)

```rust
pub const PRICE_PROVIDERS: [&str; 3] = ["anthropic", "openai", "ollama-cloud"];
const MONEY_VERSION: u32 = 1;

pub enum MoneyError { Io { path, source }, Json { path, source }, UnsupportedVersion { path, version },
    ModelsDev(String), Api(ApiError), Lock(LockError), BadRate { currency: String, rate: f64 },
    MonthStart { end: DateTime<Utc> }, History(HistoryError) }

/// USD per million tokens.
pub struct ModelPrice { pub input: f64, pub output: f64, pub cache_write: f64,
    pub cache_read: f64, pub tiered: bool }
impl ModelPrice { pub fn validate(&self) -> Result<(), String>; } // four finite numbers of 0 or more
pub struct PricePoint { pub from: DateTime<Utc>, pub price: ModelPrice }
pub struct PriceChange { pub at: DateTime<Utc>, pub added: Vec<String>,
    pub changed: Vec<String>, pub removed: Vec<String> }
pub struct CurrentPrice { pub key: String, pub price: ModelPrice,
    pub since: Option<DateTime<Utc>> }
pub struct ModelsDevList { pub prices: BTreeMap<String, ModelPrice>, pub left_out: Vec<String> }
/// Why a value is missing; Display is the text a page shows.
pub enum Missing { NoCurrency, NoPrice { model: String }, NoAccount, NoAccountPrice,
    PriceInOtherCurrency { currency: String }, PriceWithoutCurrency, NoReading,
    UnpricedInWeek { model: String }, NoMonthlyAllowance, NoWeekReading, NoMonthReading }
// Display is the page's text: "no monthly allowance in Ollama's reading", "no reading of this
// week's usage", "no reading of this month's usage".

pub struct Prices { pub version: u32, pub checked_at: Option<DateTime<Utc>>,
    pub etag: Option<String>, pub last_change: Option<PriceChange>,
    pub models: BTreeMap<String, Vec<PricePoint>>,           // key → points in time order
    pub missing: BTreeSet<String> }                          // reported removed once
impl Prices {
    pub fn load(app: &AppPaths) -> Result<Prices, MoneyError>; // missing = empty; bad/other version = error
    pub fn save(&self, app: &AppPaths) -> Result<(), MoneyError>; // pretty, atomic, mode 0600
    /// The price of `key` at `at`: the last point at or before `at`, else the first point.
    pub fn at(&self, key: &str, at: DateTime<Utc>) -> Option<&ModelPrice>;
    /// The price of a model under its company's key ([`model_price_key`]), then [`Prices::at`]:
    /// `anthropic/<id>` for a Claude model, `openai/<id>` for any other (Codex's), and
    /// `ollama-cloud/<id>` for an Ollama cloud id. A provider's own cost replaces a Claude Code
    /// row's API value only through `take_provider_cost`, and only from the feed.
    pub fn of_model(&self, model: &str, at: DateTime<Utc>) -> Option<&ModelPrice>;
    /// The current price of every key: its last point, sorted by key.
    pub fn current(&self) -> Vec<CurrentPrice>;
    pub fn take(&mut self, list: BTreeMap<String, ModelPrice>, at: DateTime<Utc>) -> Option<PriceChange>;
}

pub struct FxPoint { pub date: NaiveDate, pub rate: f64,
    pub fetched_at: Option<DateTime<Utc>> } // absent in an old fx.json: "fetched: not recorded"
pub struct Fx { pub version: u32, pub currency: String, pub points: Vec<FxPoint> }
impl Fx { pub fn load(app) -> Result<Fx, MoneyError>; pub fn save(&self, app) -> Result<(), MoneyError>;
          pub fn take(&mut self, date: NaiveDate, rate: f64); // replace that day, else insert in order
          pub fn take_fetched(&mut self, date: NaiveDate, rate: f64, fetched_at: Option<DateTime<Utc>>);
          pub fn at(&self, date: NaiveDate) -> Option<f64>;   // last point ≤ date, else first
          pub fn fetched_at(&self, date: NaiveDate) -> Option<DateTime<Utc>>; }

pub struct Rate { pub value: f64, pub date: Option<NaiveDate> }
/// "USD" is 1.0 with no date; else a point of `fx` for the settings' currency. None without a
/// currency, with `lookUpPrices` off, or when `fx` is for another currency; a rate of another
/// currency is never used.
pub fn usd_rate(settings: &Settings, fx: &Fx, date: NaiveDate) -> Option<Rate>;
/// The one conversion of every API value into the API column's currency: with `lookUpPrices` off
/// it stays US dollars and says "USD"; without a currency the value is missing
/// (`Missing::NoCurrency`); a currency without a stored rate yet also stays US dollars.
pub fn api_in_currency(usd: f64, settings: &Settings, fx: &Fx, date: NaiveDate)
    -> Result<(f64, String), Missing>;
/// The provider's own number when it gives one, else models.dev's; None when neither has one.
pub fn api_value(provider_usd: Option<f64>, models_dev_usd: Option<f64>) -> Option<f64>;

pub enum Looked {
    NotModified,
    Taken { change: Option<PriceChange>, left_out: Vec<String> },
}
/// Models.dev → prices.json. Load for the ETag, request, take the app lock, load again, take the
/// list, store the ETag and checkedAt. NotModified moves checkedAt only. Any error saves nothing.
/// `left_out` names each model the list skipped and why; a later list replaces it.
pub fn look_up_prices(api: &Api, app: &AppPaths, now: DateTime<Utc>) -> Result<Looked, MoneyError>;
/// Whether the dollar rate is due at `now`: once per Frankfurt day while the stored point is older
/// than today, and on Mon–Fri 16:00–18:00 Frankfurt time every 30 minutes after the last answer
/// until an answer carries today's date. A time chrono cannot place is an error, never a guess.
pub fn rate_due(now: DateTime<Utc>, stored: Option<NaiveDate>, last_ask: Option<DateTime<Utc>>)
    -> Result<bool, String>;
/// Frankfurter → fx.json under the app lock, stamped with `now`. "USD" sends nothing and returns
/// None; a rate that is not finite and above 0 is BadRate; a file for another currency starts
/// empty for this one.
pub fn look_up_rate(api: &Api, app: &AppPaths, currency: &str, now: DateTime<Utc>)
    -> Result<Option<FxPoint>, MoneyError>;

/// models.dev's body → the three providers' prices and the models it left out. No provider of
/// the three = error; a model without id is skipped; input and output are required;
/// cache_write/cache_read default to 0; a cost that is negative, non-finite or not a number
/// leaves that model out of the list, named in `left_out`; tiers = tiered.
pub fn prices_from_models_dev(body: &serde_json::Value) -> Result<ModelsDevList, MoneyError>;
pub fn price_key(provider: &str, model_id: &str) -> String; // "<provider>/<model id>"

/// The company a record's model or an account belongs to (serialised as its lowercase name:
/// `anthropic`, `openai`, `ollama`, `google`). `of_model` is the
/// one rule: `claude-*` → Anthropic, `<name>:cloud` / `<name>:<tag>-cloud` (name and tag not
/// empty) → Ollama, any other id → OpenAI; `of_account` maps org `codex` → OpenAI, `ollama` →
/// Ollama, `google` → Google, every other org (a Claude organization id) → Anthropic. `model_price_key` and
/// `is_plan_model` both follow it (one private decision, `priced_as`, gives the company and its id
/// in the company's models.dev list). `name()` is "Anthropic", "OpenAI", "Ollama" or "Google"; `price_list()`
/// is `anthropic`, `openai`, `ollama-cloud` or `google` (not read). `work_recorded()` is false for
/// Google alone: no model id is Google's, its work's worth is `Missing::WorkNotRecorded`, and
/// `UsageEntry::counted` and the This-week `paid` leave it out (`paid_without` names the rows).
#[derive(Serialize)] #[serde(rename_all = "lowercase")]
pub enum Company { Anthropic, OpenAI, Ollama, Google }
impl Company { pub const ALL: [Company; 4]; pub fn work_recorded(&self) -> bool;
               pub fn of_model(model: &str) -> Company; pub fn of_account(account: &AccountRef) -> Company;
               pub fn name(&self) -> &'static str; pub fn price_list(&self) -> &'static str; }
/// The one plan-model rule: `Company::of_model(model) == Company::Anthropic`. Only such a record
/// enters a Claude account's week; any other model id (OpenAI's, Ollama's) gets its own
/// subscription's value instead, adds nothing to the Claude week's split and never makes the
/// Claude week's subscription value unknown. Its API value still follows the price rule.
pub fn is_plan_model(model: &str) -> bool;
/// One record at API prices, in USD: the work of auto-burn and the statistics. None when its model
/// has no price. Of_model, so the model is priced under its company's key at `at`; the base price
/// of a tiered model, with the long-context premium left out (documented rule). A record with four
/// zero counts costs 0 even with no price.
pub fn work_usd(model: &str, input: u64, output: u64, cache_write: u64, cache_read: u64,
                prices: &Prices, at: DateTime<Utc>) -> Option<f64>;
/// [`work_usd`] of a token record at its own time.
pub fn api_usd(record: &TokenRecord, prices: &Prices) -> Option<f64>;
pub fn percent_value(monthly_price: f64) -> f64; // monthly × 12 / 52 / 100
pub(crate) fn month_money(price: f64, percent: f64) -> WindowMoney; // used = percent / 100 × price

/// One weekly window's money in the display currency.
pub(crate) struct WindowMoney { pub(crate) used: f64, pub(crate) paid: f64, pub(crate) expired: f64 }
/// The money of a weekly window from its price and the percent it used (a finished window's final
/// percent, or a running window's latest percent − `from`): paid = 100 × pv, expired = max(0, 100 −
/// percent) × pv, used = paid − expired.
pub(crate) fn window_money(price: f64, percent: f64) -> WindowMoney;

/// `account` is the Claude account live on the record's host at its time, else the one account the
/// usage readings name around the record's span (`claude_account`, G-1: both readings within
/// `READING_REACH_MINUTES` of the span, same weekly reset, a rise, and no other stored Anthropic
/// account risen around it), or the record's provider
/// subscription (an OpenAI record by `openai_subscription_at`, every Ollama record by
/// `ollama_subscription`); `company` is `Company::of_model`. A record whose OpenAI subscription
/// cannot be named has `account: None`. `account_from_readings` says the account came from the
/// readings rather than the live timeline.
pub struct Valued { pub api: Option<f64>, pub subscription: Option<f64>,
    pub account: Option<AccountRef>, pub company: Company, pub api_currency: Option<String>,
    pub api_missing: Option<Missing>, pub subscription_missing: Option<Missing> }
pub(crate) fn openai_subscription_at(timelines: &HashMap<String, Timeline>,
    stored: &BTreeSet<AccountRef>, host: &str, t: DateTime<Utc>)
    -> Result<AccountRef, Missing>;
pub(crate) fn ollama_subscription() -> AccountRef;   // acc and org both "ollama"

pub struct Valuer { … }
impl Valuer {
    /// Reads samples, windows and tokens of the range widened by 7 days, and every event over all
    /// history (a Claude record's account is the last Live/Switch at or before it, an OpenAI
    /// record's subscription the last providerSwitch; a window's price is the last PriceChanged at
    /// or before its midpoint). It also takes the stored provider subscriptions.
    pub fn build(history: &HistoryDir, range: &Range, prices: &Prices, fx: &Fx,
                 settings: &Settings, accounts: &[Account],
                 provider_subscriptions: &[ProviderSubscription],
                 warnings: &RefCell<Vec<String>>) -> Result<Valuer, MoneyError>;
    /// The price of the window from `start` to `end`: the account's price at its midpoint
    /// `start + (end − start) / 2`, through price_at. A window a reset shortened or began is
    /// never assumed to be seven days long.
    pub(crate) fn window_price(&self, account: &AccountRef, start: DateTime<Utc>,
        end: DateTime<Utc>) -> Result<f64, Missing>;
    pub fn price_at(&self, account: &AccountRef, at: DateTime<Utc>) -> Result<f64, Missing>;
    /// api = work_usd × rate of the record's day (USD, labelled, when there is no rate; None with
    /// its reason when the model has no price or no currency is set);
    /// subscription = usd / the window's api_sum × the window's used money (at most what was paid)
    /// at the window's price. A Claude record uses its weekly window; an OpenAI record its
    /// subscription's `weekly_all` week; an Ollama record its `monthly_credits` month, whose used
    /// money is `month_money(price, used)`. A record whose window has no reading, no price, an
    /// unpriced model or no API value gives None with the reason (`NoWeekReading`,
    /// `NoMonthReading`, `NoMonthlyAllowance`); a provider subscription without a price has no
    /// subscription value while its API value stays. A running window's use is its latest reading's
    /// percent − `from` (`from` 0 when absent), never the latest percent alone.
    pub fn value(&self, host: &str, record: &TokenRecord) -> Valued;
}
```

`Prices::take` adds a point only for a new model or one whose price differs from its last point; a
model missing from the list keeps its points and enters `missing` once; a model that comes back
leaves it. `Prices::at` never looks into the future: before the first point it uses the first point,
so the oldest price counts for the past. A window's price is the last `PriceChanged` at or before
the window's midpoint; a stored price without an event uses `Account.price_currency` like an event's
currency: it counts only while that currency is the settings' currency, and a price whose currency
is `None` does not count (`Missing::PriceWithoutCurrency`). There are no own prices and no own rate
any more (Removed): every price comes from models.dev, and every rate from Frankfurter. An Ollama
month starts one calendar month before its reset (`Months::new(1)`); a month that cannot be computed
that far back is `MoneyError::MonthStart`, never 30 days. A stored provider subscription's price
counts exactly like an account's, in the currency it was entered in.

## lookup_rt (step C1)

```rust
const PRICE_EVERY: Duration = Duration::from_secs(60 * 60);   // the price look-up's period
const RATE_EVERY: Duration = Duration::from_secs(30 * 60);    // a failed rate ask is retried this soon

#[derive(Serialize)] pub(crate) struct MoneyView {
    pub currency: Option<String>, pub rate: Option<Rate>,
    pub rate_fetched_at: Option<DateTime<Utc>>, pub rate_source: &'static str,
    pub prices_source: &'static str, pub look_up: bool,
    pub models_with_price: u32, pub prices_checked_at: Option<DateTime<Utc>>,
    pub last_change: Option<PriceChange>, pub left_out: Vec<String>,
    pub rate_error: Option<String>, pub last_error: Option<String> }

#[derive(Serialize)] pub(crate) struct PriceRow {
    pub key: String, pub price: ModelPrice, pub tiered: bool,
    pub since: Option<DateTime<Utc>> }

pub(crate) struct LookupClient { … }
impl LookupClient {
    pub(crate) fn view(&self) -> Result<MoneyView, String>;   // the last round's view
    pub(crate) fn now(&self) -> Result<MoneyView, String>;    // one round now, answered by the thread
    /// A session met a Claude plan model (its id starts with `claude-`) without a price: ask
    /// models.dev at once. A model outside the plan is never asked for. At most one ask per model
    /// per hour; the sessions thread calls it only for a listed row or agent on an unpriced plan
    /// model.
    pub(crate) fn want_price(&self, model: &str);
}
pub(crate) fn spawn(app: AppPaths, api: Api, ui: Sender<UiEvent>) -> Result<LookupHandle, String>;
pub(crate) struct LookupRuntime { … }
impl LookupRuntime {
    pub(crate) fn round(&mut self, settings: &Settings, now: DateTime<Utc>) -> MoneyView;
    pub(crate) fn rate_round(&mut self, settings: &Settings, now: DateTime<Utc>)
        -> Option<MoneyView>;   // None when no rate was asked for
    pub(crate) fn settings_failed(&mut self, error: String) -> MoneyView; // no request
    pub(crate) fn view(&self) -> MoneyView;
}
pub(crate) fn price_rows(app: &AppPaths) -> Result<Vec<PriceRow>, MoneyError>;
```

The thread reads `accounts.json` at every round, so `lookUpPrices` counts without a restart. With
it on, a round asks models.dev every hour and asks Frankfurter when `money::rate_due` says so:
once per Frankfurt day while the stored point is older than today, and on Mon–Fri 16:00–18:00
Frankfurt time every 30 minutes after the last answer until an answer carries today's date. A
failure is tried again at the next look. With `lookUpPrices` off no request is made and the view
still shows what is stored; the API column then stays in US dollars and its header says so. The
runtime's constructor fills the view from the files before the first round, so the page never shows
zero prices while `prices.json` holds some. Every changed view is sent as `UiEvent::Money(view)`;
the main thread emits it to both windows. An unpriced model on the Claude plan (its id starts with `claude-`)
is asked for at once through `want_price`, at most once per model per hour, when the sessions thread
lists a row or agent that uses it; a model outside the plan is never asked for. A failed rate
look-up goes into `rate_error`, so the rate row shows it; `last_error` keeps the price look-up's
errors and the `prices.json` / `fx.json` read errors. `left_out` is the last list's skipped models
and is kept until a later list names others. There is no own price and no own rate: no command sets
one.

## projects (step C1)

```rust
#[serde(tag = "edit", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ProjectEdit {
    Rename { key: ProjectId, name: String },
    Join { key: ProjectId, into: ProjectId },
    Unjoin { key: ProjectId },
    Assign { key: ProjectId, org: Option<String> },
    OrgRename { org: String, name: String },
    OrgJoin { org: String, into: String },
    OrgUnjoin { org: String },
    AccountOrg { account: AccountRef, org: Option<String> },
    OrgAdd { name: String },
    OrgRemove { org: String },
}
impl ProjectEdit { pub fn validate(&self) -> Result<(), ProjectsError>; }
impl Projects { pub fn validate_edit(&self, edit: &ProjectEdit) -> Result<(), ProjectsError>;
                pub fn with_tokens(&mut self, history: &HistoryDir) -> Result<Vec<String>, HistoryError>; }
// project and organization shown names: 1..=40 characters, not only spaces, no leading or trailing
// space for a new organization; an organization identity (Assign.org, AccountOrg.org,
// OrgJoin.org/into, OrgRemove.org, OrgUnjoin.org) only non-empty, because it comes from a login's
// orgName or a repository owner, not from typing; a project or organization cannot be joined into
// itself; a folder id names its host, a repository id does not. `validate_edit` runs `validate`
// first, then refuses `OrgAdd` for an existing name (`OrganizationExists`), `OrgRemove` for an
// unknown organization (`UnknownOrganization`) or one whose `owner_repositories` is not empty
// (`OwnerOrganization`, naming the first repository), and `OrgUnjoin` for an organization that is
// not merged now (`NotMerged`).

#[serde(rename_all = "camelCase")]
pub struct ProjectId {
    #[serde(flatten)] pub key: ProjectKey,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub host: Option<String>,
}
impl ProjectId { pub fn of(key: &ProjectKey, host: &str) -> ProjectId; } // host kept for a folder key only

pub enum ProjectsError { NameLength { what, length }, NameBlank { what }, EmptyOrg,
    FolderWithoutHost, RepoHasHost { host }, JoinIntoItself { what },
    OrganizationExists { name }, UnknownOrganization { org },
    OwnerOrganization { name, repository }, NotMerged { org }, OrganizationNameSpace,
    History(HistoryError) }

pub struct ProjectsLogRecord { pub t: DateTime<Utc>, #[serde(flatten)] pub edit: ProjectEdit }
impl Record for ProjectsLogRecord { const KIND: Kind = Kind::Projects; } // "projects"

pub struct Project { pub key: ProjectId, pub name: String, pub repo: Option<String>,
    pub org: Option<String>, pub joined: Vec<JoinedKey>, pub folders: BTreeMap<String, Vec<String>>,
    pub sessions: u32,
    /// Output tokens of all this project's history, all hosts and all time; filled by `with_tokens`.
    pub tokens: u64 }
pub struct JoinedKey { pub key: ProjectId, pub name: String } // the key's default name
pub enum OrgSource { Owner, Login, Added } // serde lowercase
pub struct MergedOrg { pub id: String, pub name: String, pub source: OrgSource }
pub struct Org { pub id: String, pub name: String, pub accounts: Vec<AccountRef>, pub projects: u32,
    pub owner_repositories: Vec<String>, pub source: OrgSource, pub merged: Vec<MergedOrg>,
    /// No project and no account: Settings lists it, Statistics leaves it out.
    pub empty: bool,
    /// Output tokens of all its projects, all time (`with_tokens`), and their distinct sessions
    /// (`resolve`).
    pub tokens: u64, pub sessions: u32 }
pub struct AccountEntry { pub id: String, pub account: AccountRef } // the store's id and its AccountRef
pub struct Projects { pub projects: Vec<Project>, pub orgs: Vec<Org>,
    pub account_refs: Vec<AccountEntry>, /* names, assigned, joins, org_join: private */ }

impl Projects {
    pub fn resolve(log: &[Hosted<ProjectsLogRecord>], sessions: &[Hosted<SessionRecord>],
                   accounts: &[Account]) -> Projects;
    pub fn of(&self, id: &ProjectId) -> Project; // an id that is not a row, with the log's names/joins
}
```

`resolve` first collects each session's `ProjectId::of(key, host)`: a repository id has no host and
is the same project everywhere, a folder id carries its host, so the same folder name on two hosts is
two projects until a `Join` makes one. It then applies the log in time order, ties by host name and
then by line order: `Rename` and `Assign` for the same id replace the previous; `Join` follows the
chain and, when it would close a cycle, drops the earliest record of that cycle; `Unjoin` removes
the id's join; `OrgRename`, `OrgJoin` and `OrgUnjoin` work the same for organizations: `OrgJoin`
merges `org` into `into` and `OrgUnjoin` stops that latest merge, so the organization is a root of
its own again (R1). An `Assign` made after a merge keeps the organization it names. Ids are grouped
under their join root; a group with no session is not a row. A project's name is the last rename of
its root or the root key's default name; its organization is the last assignment of the root or the
root key's default (the repository owner); its `joined` ids carry their default names, because a
joined id's own rename is not shown. Organization identities come from every project's
organization, every account's `org_name`, every `OrgRename` and every `AccountOrg` organization,
and both ends of every `OrgJoin`, joined by `OrgJoin`. An organization that comes from a repository
owner of this or another host stays as an empty row when its last project moves away (decision 30);
its `empty` is true when it has no project and no account. An organization is `source` `Owner`,
`Login` or `Added`; a merged organization is a `MergedOrg` of its target, not a row of its own.
Organization names that differ only in case are
one organization, in a 1.1.0 history too: an owner spelled `Acme` and a login organization named
`acme` are the same organization, because GitHub owners are case-insensitive. The key is the
Unicode lower case of the name; the first spelling in the history's order gives the id and the shown
name, and a stored account's `org_name` spells an organization only when no session or log record
names it. `AccountOrg` records are applied in log order:
the latest record for an account puts that one account in that organization's root, or in none.
Nothing else moves an account, so an edit for one account leaves every other account where it is.
`with_tokens` fills every project's output tokens over all time and every organization's tokens with
one `ProjectId → row` map, so an edit does not read the token lines per project; `resolve` counts an
organization's sessions from its projects.
`Projects::of` uses the same maps for an id without a row.

## app: history additions — hosts, import and export (step C1)

```rust
pub struct HostInfo { pub host: String, pub this_host: bool, pub first_record: Option<DateTime<Utc>>,
    pub last_record: Option<DateTime<Utc>>, pub bytes: u64 }
pub struct Imported { pub copied: Vec<String>, pub conflicts: Vec<HostConflict> }
pub struct HostConflict { pub host: String, pub reason: String }
impl HistoryDir {
    /// One entry per host folder under the root, this host's own included when it is still
    /// empty, sorted by host. `first_record` is the first record of the oldest file of any kind;
    /// `last_record` the newest `up` or `samples` line; `bytes` the size of the history files in
    /// the folder (`sessions-now.json` is live state, not history, and does not count).
    pub fn host_infos(&self) -> Result<Vec<HostInfo>, HistoryError>;
    /// The window records of the range, one per window: `read::same_window` decides, not just
    /// the end — two ordinary records' ends are within `same_reset`; two hosts' reset records
    /// are one when their spans `(lastSample, end]` overlap; a reset and an ordinary record are
    /// one when the ordinary end is the reset's start + 7 days and the ordinary began before the
    /// reset's end. The reset record wins; otherwise the record with the latest `lastSample`
    /// counts, a tie goes to the host name that sorts first, so read order does not decide.
    /// Sorted by end, then account. An ordinary record whose next window of the same account,
    /// kind and model ends more than RESET_JITTER later but was read more than RESET_JITTER
    /// before its end went on and is left out (1.5.2 wrote one per poll for an unused Codex
    /// limit); their count is one warning of the read. `keep_warnings` adds each warning once.
    pub fn finished_windows(&self, range: &Range, hosts: &Hosts)
        -> Result<Read<WindowRecord>, HistoryError>;
    /// Copy every file of `from`'s host folders that is missing here or longer there; keep a
    /// longer file; never read or write this host's folder; refuse this folder and a folder
    /// without a host folder. A host folder that is not here yet is copied as it is when its
    /// `host.json` names a machine; without one `hosts` would never list the copy, so it is a
    /// conflict ("… has no host.json with a machine id …"). One that is here is copied only
    /// when both sides' `host.json` name the same machine; another machine id, or no
    /// `host.json` (or an empty id) on either side, is a conflict ("cannot tell whether …"), and
    /// nothing of that folder is copied. A read or write error stops the whole import; a
    /// conflict does not. Returns what was copied and the conflicts, both sorted by host.
    pub fn import(&self, from: &Path) -> Result<Imported, HistoryError>;
}
pub fn export_csv(history: &HistoryDir, range: &Range, out: &mut dyn Write) -> Result<(), HistoryError>;
pub fn export_json(history: &HistoryDir, range: &Range, out: &mut dyn Write) -> Result<(), HistoryError>;
```

The export has one row per window — windows that several hosts recorded are one row, the
record with the latest `lastSample` winning (from `finished_windows`) — and one per UTC day and
project key (`kind: "tokens"`, `dayUtc`, `project` as `repo:<id>`/`folder:<id>`, input, output,
cacheWrite, cacheRead). Both formats make their rows once through the same function: every
window first, then the token rows month by month, so memory holds one month of token records
plus the rows. CSV writes the header
`kind,account,org,limit,model,start,end,percent,day_utc,project,input,output,cache_write,cache_read`
and then the rows, quoting per RFC 4180; JSON writes `{"windows":[…],"dailyTokens":[…]}` with
camelCase fields. A project's shown name is never exported; a name changes, the key does not.

## app: panel commands (step C1)

New and changed `#[tauri::command]`s; all return the error text as `String`.

```rust
// money
async fn money_view(state) -> Result<MoneyView, String>;
async fn price_list(state) -> Result<Vec<PriceRow>, String>;
async fn lookup_now(state) -> Result<MoneyView, String>;
// accounts: changed, `price` is absent / null / a number
async fn edit_account(state, key: String, label: Option<String>, abbr: Option<String>,
    color: Option<String>, auto_burn: Option<bool>, price: Option<serde_json::Value>) -> Result<(), String>;
// projects
async fn projects_view(state) -> Result<Projects, String>;
async fn projects_edit(state, edit: ProjectEdit) -> Result<Projects, String>;
// agents
struct AgentCallRow { at: DateTime<Utc>, command: String, cwd: Option<String>, allowed: bool }
struct SkillRow { path: String, installed: bool, differs: bool }
async fn agent_calls(state, limit: u32) -> Result<Vec<AgentCallRow>, String>;
async fn skill_status(state) -> Result<SkillRow, String>;
async fn skill_install(state, force: bool) -> Result<SkillRow, String>;
// history
struct HistoryView { host: String, dir: String, hosts: Vec<HostInfo>, bytes: u64,
    saved_host: Option<String>, saved_dir: Option<String>,
    suggested_folder: Option<String>,  // JSON suggestedFolder; null when not set
    suggested_error: Option<String> }  // JSON suggestedError; null when not set
struct ImportResult { copied: Vec<String>, conflicts: Vec<HostConflict>, view: HistoryView }
async fn history_view(state) -> Result<HistoryView, String>;
async fn history_import(state, from: String) -> Result<ImportResult, String>;
async fn history_export(state, format: String, to: String,
    from: Option<DateTime<Utc>>, until: Option<DateTime<Utc>>) -> Result<(), String>;
```

The commands use the history folder the tray app opened at startup (`state.history`), never one from
a newer snapshot, until a restart; `history_view`'s `saved_host` and `saved_dir` are the saved values
that a restart would use. `suggested_folder` is `<dropbox>/seatswap` from `paths::dropbox_folder`,
only while the saved settings name no history folder; a missing `info.json` is no suggestion, and a
file that cannot be read gives `suggested_error` instead. The page shows the suggestion above the
host list and the error under it, joined with the history error when there is one. It is only a
suggestion: nothing is created or written until the user presses Use, and Use saves the folder
through the same `save_setting` command the field uses. Paths from the page are expanded with `~` and
must be absolute.
`history_export` refuses an existing file and writes through `atomic` with mode 0600.
`projects_from` reads all hosts' `ProjectsLogRecord` and `SessionRecord` over all time, prints torn
lines' warnings on stderr and writes each folder with `~`; `edit_projects` validates first, appends
to this host's own log only, then returns the new resolution.

`worker.rs`: `Snapshot.accounts: Vec<AccountRow>` carries the stored fields the cards do not
(`plan`, `price`, `orgName`, `autoBurn`, login end); `UiEvent` gains `Money(MoneyView)`;
`Worker.lookup: Option<LookupHandle>` is the thread; `Worker.history` is the one `HistoryDir`.
`AccountRow { id, label, abbr, color, email, orgName, plan, price, autoBurn, loginEndsAtMs,
loginDaysLeft, isActive }`. `save_setting(field, value)` takes the app lock, loads
`accounts.json`, replaces that one field with `merge_setting`, validates the whole `Settings` and
writes the file, then writes the history events of a change (`settings_change_events`:
`AutoBurnSet` for `autoBurn`, and since 1.2.0 a `SettingSet` per changed shared setting) and applies
the autostart entry; it answers `SavedSetting { settings, warning }`, where an
autostart failure is `warning` = `settings saved; start at login could not be set: <error>`, not a
failed save. `remember_settings` puts the reply's merged settings into the snapshot.

## app: CLI — `window`, `prices` and `history` (step C1)

```rust
pub(crate) enum Command {
    // …
    Window { target: Option<String> },              // default "sessions"
    Prices { refresh: bool },
    History { #[command(subcommand)] action: HistoryAction },
}
pub(crate) enum HistoryAction {
    Export { #[arg(long, value_enum)] format: ExportFormat,
             #[arg(long)] from: Option<String>, #[arg(long)] to: Option<String> },
    Import { folder: String },
}
pub(crate) enum ExportFormat { Csv, Json }
```

- `seatswap window [AREA]`: validates the target with `window::parse_target`, then sends
  `window <target>` to the running app; no app running is the IPC module's error.
- `seatswap prices [--refresh]`: with `--refresh`, refuses when `lookUpPrices` is off
  ("look-ups are off (lookUpPrices); turn them on in the settings to refresh"), otherwise calls
  `money::look_up_prices`; then prints the stored prices as a table: `USD per million tokens`,
  then `key`, `input`, `output`, `cache write`, `cache read`; a tiered key has ` *` and a closing
  note that the base price is shown. No prices prints "no prices yet: run `seatswap prices
  --refresh` while online.".
- `seatswap history export --format csv|json [--from YYYY-MM-DD] [--to YYYY-MM-DD]`: writes to
  standard output; the days are UTC days, `--to` inclusive (the range ends at the next midnight),
  either end open when absent, `--from` after `--to` is a usage error.
- `seatswap history import <folder>`: `~` is expanded, the folder must be absolute; prints
  `copied <host>` per host at least one file was copied for, or `nothing new`, then
  `conflict <host>: <reason>` per conflict, and exits with an error when there was one (after
  printing what was copied).

The gate for an agent: `window` is refused like `panel` (`AGENTS_MAY_NOT_CHANGE`); `prices` follows
`agentsRead`, and `--refresh` is refused even with it on (it writes `prices.json`); `history export`
follows `agentsRead`, `history import` is refused (`AGENTS_MAY_NOT_CHANGE`); `codex list` follows
`agentsRead` and `codex switch` follows `agentsSwitch`. `seatswap panel
settings` sends `window settings` instead of a pop-up view.

## Files step C1 writes

- **`<data folder>/prices.json`** (§11.5): pretty JSON, atomic, mode 0600, written under the app
  lock by the look-up thread and by `seatswap prices --refresh`. `version`, optional `checkedAt`,
  `etag`, `lastChange`, `models` (key → dated points) and `missing` (keys reported removed once).
  There is no `overrides`: a price is never entered by hand. A file of another version is refused,
  never replaced.
- **`<data folder>/fx.json`** (§11.5): `{"version":1,"currency":"EUR","points":[{"date":"…",
  "rate":0.90481}]}`, the same way, only while the currency is set and not `USD`.
- **`<history root>/<host>/projects-YYYY-MM.jsonl`** (§11.6): one line per project or organization
  edit, `{"t":"…","edit":"join","key":{…},"into":{…}}`; append-only, this host's own file only,
  atomic through `atomic::append_lines`.
- **History events**: `{"t":"…","type":"priceChanged","account":{"acc":"…","org":"…"},
  "amount":180.0,"currency":"EUR"}` when an account's price changes (amount and currency omitted
  when null); `{"t":"…","type":"planChanged","account":{…},"from":"Team","to":"Team Premium"}` when a
  stored login changes the plan label.

## Step C2: the Sessions area and the other hosts (spec §3.8, §4.1)

### live_sessions (step C2)

`core::live_sessions` owns the rows and the small report another host reads.

```rust
// live_sessions.rs
pub const CACHE_LIFE_MIN: i64 = 60;      // idle/waiting longer: a switch pays nothing extra
pub const REPORT_EVERY_SECS: i64 = 30;
pub const REPORT_STALE_MIN: i64 = 15;
#[serde(rename_all = "camelCase")] pub enum LiveStatus { Busy, Idle, Waiting, Closed, Other(String) }
#[serde(rename_all = "camelCase")] pub struct Values { pub api: Option<f64>,
    pub api_currency: Option<String>, pub api_missing: Option<String>,
    #[serde(default)] pub api_partial: bool,
    pub subscription: Option<f64>, pub currency: Option<String>,
    pub subscription_missing: Option<String>,
    #[serde(default)] pub subscription_partial: bool }
impl Values { pub fn of(valued: &[Valued], settings: &Settings, fx: &Fx, at: DateTime<Utc>,
                        ) -> Values;
              pub fn sum(parts: &[&Values]) -> Values; }
#[serde(rename_all = "camelCase")] pub struct LiveAgent { pub agent: String,
    pub parent: Option<String>, pub kind: Option<String>, pub task: Option<String>,
    pub model: Option<String>, pub running: bool, pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>, pub tokens: Tokens, pub values: Values }
#[serde(rename_all = "camelCase")] pub struct LiveSession { pub session: String, pub host: String,
    pub label: String, #[serde(default)] pub title: Option<String>,
    pub project_id: ProjectId, pub project: String, pub org: Option<String>,
    pub cwd: String, pub provider: String, pub model: Option<String>, pub tied: bool,
    pub status: LiveStatus, pub since: DateTime<Utc>, pub started: DateTime<Utc>,
    pub agents_running: u32, pub agents_done: u32, pub agents: Vec<LiveAgent>,
    pub tokens: Tokens, pub last_hour: Option<Tokens>,
    #[serde(default = "no_values")] pub last_hour_values: Values,
    #[serde(default = "no_values")] pub agents_done_values: Values,
    pub values: Values }
pub struct Rows { pub sessions: Vec<LiveSession>, pub warnings: Vec<String> }
pub enum SwitchKind { Free, Resend, LosesThinking }
pub fn switch_kind(session: &LiveSession, thinking_since_user: u32, now: DateTime<Utc>) -> SwitchKind;
pub enum Pricing<'a> { Known { valuer: &'a Valuer, settings: &'a Settings },
    Unknown { reason: String } }
pub fn running(files: &[SessionFile], state: &ScanState,
    metas: &dyn Fn(&str) -> Vec<AgentMeta>, projects: Option<&Projects>,
    pricing: &Pricing, host: &str, now: DateTime<Utc>,
    project_of: &dyn Fn(&str) -> ProjectKey) -> Rows;
pub fn closed(history: &HistoryDir, since: DateTime<Utc>, now: DateTime<Utc>,
    projects: Option<&Projects>, pricing: &Pricing) -> Result<Rows, HistoryError>;

/// The Harness rows and the flash agents they add to Claude Code rows.
pub struct HarnessRows { pub rows: Rows,
    pub agents: BTreeMap<String, Vec<LiveAgent>>,      // Claude Code session id → its flash agents
    pub tokens: Vec<TokenRecord>,                      // the lines this pass writes
    pub recorded: BTreeMap<String, HarnessRecorded> }  // the totals after writing
pub const HARNESS_PROVIDER: &str = "DeepSeek Harness";
pub fn harness_rows(sessions: &[HarnessSession], owners: &BTreeMap<String, String>,
    claude_cwd: &BTreeMap<String, String>, recorded: &BTreeMap<String, HarnessRecorded>,
    locks: &BTreeSet<String>, projects: Option<&Projects>, pricing: &Pricing, host: &str,
    now: DateTime<Utc>, project_of: &dyn Fn(&str) -> ProjectKey) -> HarnessRows;
pub const CODEX_PROVIDER: &str = "Codex";
pub const CODEX_LAST_HOUR_UNKNOWN: &str =
    "the Codex session's token total went down, so its last hour is not known";
pub fn codex_rows(sessions: &[CodexSession], open_files: &BTreeSet<PathBuf>,
    projects: Option<&Projects>, pricing: &Pricing, host: &str, now: DateTime<Utc>,
    project_of: &dyn Fn(&str) -> ProjectKey) -> Rows;
/// Whether a pass must walk the processes for Codex's open rollouts: one of `sessions` is not
/// recent.
pub fn codex_needs_process_walk(sessions: &[CodexSession], now: DateTime<Utc>) -> bool;
/// Claude Code's own cost from the status line's feed becomes the API value, converted once with
/// `money::api_in_currency`; returns the warning when the converted number is not finite and the
/// row keeps the valuer's value. With a currency and no stored rate the cost stays in USD, labelled
/// USD, and this returns `None`: that is not a problem. Without a currency the cost has no value
/// (`api_missing`), also without a warning. `None` when the cost is used.
pub fn take_provider_cost(values: &mut Values, cost_usd: Option<f64>, settings: &Settings,
    fx: &Fx, at: DateTime<Utc>) -> Option<String>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct HostReport { pub at: DateTime<Utc>, pub sessions: Vec<LiveSession> }
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct OtherHost { pub host: String, pub at: DateTime<Utc>, pub stale: bool,
    pub sessions: Vec<LiveSession> }
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct OtherHosts { pub hosts: Vec<OtherHost>, pub warnings: Vec<String> }
pub fn write_report(history: &HistoryDir, sessions: &[LiveSession], at: DateTime<Utc>)
    -> Result<HostReport, HistoryError>;
pub fn other_hosts(history: &HistoryDir, now: DateTime<Utc>)
    -> Result<OtherHosts, HistoryError>;

// history/mod.rs
pub(crate) const SESSIONS_NOW_FILE: &str = "sessions-now.json";  // one host's live state, not history
impl HistoryDir { pub fn sessions_now_file(&self, host: &str) -> PathBuf; }

// projects.rs: the one project read the pop-up and the sessions thread share
pub fn load(history: &HistoryDir, accounts: &[Account])
    -> Result<(Projects, Vec<String>), HistoryError>;
```

`running` builds one row per session file. A session file whose `startedAt` or `statusUpdatedAt` is
not a valid time is not a row; its reason is a `Rows::warnings` line. A row's tokens are
`SessionSeen.by_model` valued as one record per model, dated at the session's last answer; an agent's
tokens are one record with its model at its last answer, and an agent without a model has both values
missing. `switch_kind` answers what a switch costs a row in the "If you switched now" card: `Free`
for a closed one, one without a model, or an idle or waiting one older than `CACHE_LIFE_MIN`; else
`LosesThinking` when the row is tied and `thinking_since_user > 0`; else `Resend`. `closed` reads
the last `SessionRecord` of each session and keeps the closed ones whose `last` is in `[since, now)`,
sums every `TokenRecord` of the session (several lines for one bucket and model) from the hour start
of the earliest `first` — a record's `t` is its bucket's start — and gives the row the
label `<project>-<first two characters of the session id>` and no agents. A closed row's last hour
sums the records whose span `[t, t + span())` overlaps `[now - 60 min, now)`: a 5-minute line, or an
older hourly line from the hour before the window, counts when it reaches into it.

`live_sessions::Values::sum(parts: &[&Values]) -> Values` is the one sum rule (R3): each half adds
its known parts, even when another part is missing. The half is then the known part and
`api_partial` / `subscription_partial` is true, with the first missing part's reason in
`api_missing` / `subscription_missing`; a half with no known part stays `None` with its reason and
`partial = false`. An empty list is a known 0 in both halves with no currency of its own. API parts in two
`api_currency` values say `TWO_CURRENCIES`, and both `api` and `api_currency` are then `None`;
subscription parts in two `currency` values say "these sessions are valued in two currencies", and
both `subscription` and `currency` are then `None` (each host can set its own settings' currency).
`Values::of` over no records is a known 0 too: the API 0 has the settings' currency when a rate is
stored, `USD` when the currency is set without a rate or look-ups are off, and no currency when no
currency is set; the subscription 0 is in the settings' currency.
`Values::of(valued, settings, fx, at)` builds one `Values` per record with the settings' currency and
calls `Values::sum`; a group in the Sessions view uses the same function, never a second addition.
A page draws a partial half as `≥ amount` with its reason; a share, ratio or percent takes only full
halves through `stats::full_half`, except a share of an API value, which uses `stats::priced_api`
(V-2, below).
`LiveSession` carries `project_id: ProjectId` after the project joins, so the shown `project` name
and the grouping key stay apart. `last_hour` is `Option<Tokens>`: `None` when the row's total went
down (Codex), and the reason is the row's warning; a known 0 is an empty last hour, not `—`.

**The Harness and Codex rows.** `harness_rows` adds the DeepSeek Harness's sessions (spec §3.8): a
flash job named in a Claude Code transcript — its `flash-task-…` id or its `job-<8hex>` report id —
becomes a `"flash"` agent of that Claude Code session, with its workers below it and its tokens under
the Claude Code session in the history. A job first seen unlinked is recorded under its own Harness
id and folder; once it is linked its tokens go under the Claude Code session, and the total recorded
so far is kept while its session file exists (`transcripts::prune_harness_recorded` drops it only
when the file is gone). An unlinked session's folder is `harness_location`'s: the Claude Code
session's folder when linked, else `harness::copy_source(cwd, roots)` — a flash worker's copy
`<base>/<key>/slots/<n>/…` mapped to the known Claude Code folder whose `copy_key` (first 12 hex
digits of its path's SHA-256, flash-agents' naming) is `<key>`, subfolder kept — else its own folder.
Another top-level Harness session is a row of its own while `harness::is_open` (a lock holder or a
last event younger than the quiet time; a flash job only while `is_running`), with the provider
`DeepSeek Harness` and the status Busy (`is_running`) or Idle, named by its `HarnessSession::title` (`LiveSession.title`,
which the live report keeps) unless it is a flash job (`harness::is_flash_job`, a `flash-task-` id):
an unlinked job's row has no title, because its title is a worker's task text; its agents are its
workers. A worker's title is shown
live as that agent's `task`; it is never written to the history, and `write_report` sets every
agent's `task` to `None` before the host report is written, so other hosts never see it. A session, agent or hour without any
token line is a known 0; a sum is unknown only when a part of it is unknown, and a missing value is
shown as "—" with its reason in the tooltip. A model that runs under two providers is one sum per
provider, never one mixed sum.

`codex_rows` adds Codex's running sessions with the provider `Codex`: a session is running when its
rollout file is held open by any process or its last event is younger than
`harness::HARNESS_QUIET_MINUTES` (30 min), and only running sessions are rows. A session id whose
canonical path is in `open_files` counts, whatever the process is called. Codex writes no history
records of its own, so a Codex row lives
while it runs and leaves the list afterwards. Several rollout files of one session id are one
session: its newest total in time over all the files counts. Its last hour is the newest total minus
the newest total at or before `now - 60 min`, or the whole newest total when there is none; when any
token field went down, the last hour is `None` and the row carries `CODEX_LAST_HOUR_UNKNOWN` as the
reason, never a made-up 0. `token_count` with `info: null` is normal and changes nothing; an object
without `total_token_usage` is a warning. A missing `cache_write_input_tokens` counts as 0.

**The process link.** `harness::RealProc` reads `/proc` on Linux (pids, open files, parent) and is
empty on every other system, so `proc_links` and `proc_links_in` return no owner there and
`codex_needs_process_walk` still decides whether the walk is worth it. `ProcLinks { owners, locks,
files, warnings }` matches a job's process to a session through its parent chain and its open session
file; `warnings` carries the per-process read failures, which the caller shows once. A
job whose result is in a transcript is linked by the transcript alone, on every system. A Codex
session is running through its open rollout file only where the link exists; elsewhere its last event
decides. `harness::is_running(session, lock_held, now)` is `!finished` and either the lock is held or
the last event is younger than the quiet time.

`write_report` sets every agent's `task` to `None` before it writes, so no task text ever leaves
memory; the file is pretty JSON, written with `atomic::write_atomic` and `Access::Private` in this
host's folder. `other_hosts` lists the host folders with `HistoryDir::hosts`, skips this host and a
folder without the file, and turns every unreadable or unparseable file into a warning naming the
file and the reason while the other reports are still returned; only failing to list the folders is
an error. A report more than `REPORT_STALE_MIN` before `now` is stale: its sessions say
`Other("no report")`; a future `at` is fresh. The import skips `SESSIONS_NOW_FILE`, and the month
file listing and the export never see it.

### the sessions thread: passes, the report and the commands (step C2)

The sessions thread (`gui/sessions_rt.rs`) keeps a rows context of two parts built apart: the
`Projects` and a `Valuer` for the day up to `now` with the settings from `load_accounts`. A part
that was built is kept for `ROWS_CONTEXT_MAX_AGE_SECS = 300`; a part that could not be built keeps
its error and is tried again after `ROWS_CONTEXT_RETRY_SECS = 60`, not on every pass. Each failure
is `SessionsSummary.unavailable`, shown once, and the rows are built with what is there. Without a
history (`recorder` is `None`) the projects are `None` (the keys' default names, no organization)
and the pricing is `Pricing::Unknown` with the history's error, so this host's waiting sessions
still notify; no report is written and no other host is read. The report is written also when the
pricing is unknown, because its rows say why the values are missing. Every "is it time again" rule
of the thread (the report's 30 s and heartbeat, the other hosts' read, the context's ages) counts a
time in the future of `now` as due, so a clock that moved back silences nothing. The rows are the
one source of a waiting session's `label` and `project`; their warnings are the one place an
invalid session time is said. This host's report is written when the rows changed and the last
write is at least `REPORT_EVERY_SECS` old, at once when a running session left the list, and as a
heartbeat when the last write is older than `REPORT_HEARTBEAT_SECS = 300`; an empty list is a
report too. The same thread reads `other_hosts` at most every `REPORT_EVERY_SECS` and keeps the
last read for the passes in between, so its waiting sessions and warnings do not flicker; a read
that fails stores an empty one with its error. Every waiting session of a fresh report goes into
`SessionsSummary.waiting` with its host. `waiting: Vec<Waiting>` carries
`Waiting.host: Option<String>` (`None` = this host); this host's name is `GuiState.history.host()`.
`Deduper` shows another host's wait only with `notifyWaitingHosts: AllHosts`; a click opens the
large window at `sessions?<session id>` (`window::parse_target` accepts the id and refuses a bare
item after any other area).

One pass is `run_pass` (the tests' entry) inside `one_pass`, which also saves the transcript state:

```rust
// gui/sessions_rt.rs
pub(crate) struct PassOutcome { pub summary: SessionsSummary, pub models: Option<BTreeSet<String>>,
    pub tied_busy: u32, pub thinking: Vec<(DateTime<Utc>, u32)>, pub wake_soon: bool }
pub(crate) fn run_pass(app: &AppPaths, claude: &ClaudePaths, probe: &dyn Liveness,
    recorder: Option<&Recorder>, state: &mut ScanState, memory: &mut SessionMemory,
    now: DateTime<Utc>, state_error: Option<&str>) -> PassOutcome;
impl SessionsHandle { pub(crate) fn latest(&self) -> Arc<Mutex<Option<SessionsSummary>>>;
                      pub(crate) fn client(&self) -> SessionsClient;
                      pub(crate) fn stop(&mut self); }
```

A pass reads the session files, scans what is new in the transcripts (at most
`SCAN_BUDGET = 64 MiB`), writes the token and session records and the wait records, builds the rows,
writes this host's report and reads the other hosts. A session record is rewritten at most every
`RECORD_EVERY_SECS = 600` while its summary changes; a closed session gets exactly one closing
record. The thread's regular wake is `WAKE = 60 s`, and `AFTER_INCOMPLETE = 1 s` follows a scan that
did not finish; the session folder is watched all the time, the transcript folders only while the
window is subscribed. `PassOutcome.thinking` is the running tied sessions' answers with thinking,
minute by minute, and `tied_busy` of them the busy ones; both go to the burn tick (below). The same
counts fill `SessionsSummary.tied` and `.tied_busy`, the one source for the pop-up's sentence and the
switch event's history line. `agents_of` runs once per running session per pass; the session record
and the rows both use that one list, so the meta files are read once whether or not the window is
open, and the list's warnings go to `SessionsSummary.warning`.

### window.rs: the sessions view, its sums and the commands (step C2, Task 4)

The sessions thread owns the facts the view is made of and answers the window over one
`SessionsClient`, the same pattern as the look-up's client:

```rust
// gui/sessions_rt.rs
pub(crate) const SESSIONS_OFF: &str =
    "reading sessions is off: turn on \"Read Claude Code's session files\" in Settings → General and restart seatswap";
enum SessionsCommand { View { options: SessionsOptions, reply: Sender<Result<SessionsView, String>> },
                      Subscribe(SessionsOptions), Unsubscribe, Stop, Wake }
#[derive(Clone)] pub(crate) struct SessionsClient { /* sender */ }
impl SessionsClient {
    pub(crate) fn view(&self, options: SessionsOptions) -> Result<SessionsView, String>;
    pub(crate) fn subscribe(&self, options: SessionsOptions) -> Result<(), String>;
    pub(crate) fn unsubscribe(&self) -> Result<(), String>;
}
impl SessionsHandle { pub(crate) fn client(&self) -> SessionsClient; }
```

`Wake` is the folder watchers' plain wake. The command channel is also the thread's wake channel.
When `readSessions` is off no thread runs, `GuiState.sessions_client` is `None`, and
`GuiState::sessions_commands()` answers every command with `SESSIONS_OFF`; turning the setting on
takes effect after restarting seatswap, because the thread starts only with the app.

After every pass `SessionMemory` keeps this host's `Rows`, each running session's
`thinking_since_user()` (the sum of its minutes), the last `OtherHosts` read, the summary's
`unavailable` text, the settings' currency, the last pass time and the history's host (`None`
without a history). It keeps the
subscription's options (`Option<SessionsOptions>`, `None` while nothing is subscribed) apart from a
command's; `publish_view` always uses the subscription's. A `View` command is answered from those
facts over `window::view_of`, without a pass, for the options it sends; another closed choice reads
that choice's closed rows once and keeps them beside the subscription's.

```rust
// gui/window.rs
pub(crate) enum ClosedSince { None, Day, Week, Month }        // 24 h, 7 d, 30 d before now
impl ClosedSince { pub(crate) fn since(self, now: DateTime<Utc>) -> Option<DateTime<Utc>>; }
pub(crate) struct SessionsOptions { pub closed: ClosedSince, pub host: Option<String> }
pub(crate) struct HostState { pub host: String, pub at: DateTime<Utc>, pub stale: bool, pub this: bool }
pub(crate) struct SwitchEffect { pub loses_thinking: u32, pub loses_thinking_busy: u32,
                                 pub loses_thinking_models: Vec<String>,
                                 pub resend: u32, pub free: u32 }
/// `kind` in JSON: all/provider/project/providerModel/model, with `project`,
/// `provider` and `model` as their fields.
pub(crate) enum SumOf { All, Provider(String), Project(ProjectId),
    ProviderModel(String, Option<String>), Model(Option<String>) }
pub(crate) struct Sum { pub of: SumOf, pub sessions: u32, pub agents_running: u32,
    pub agents_done: u32, pub tokens: Tokens, pub last_hour: Option<Tokens>,
    pub last_hour_values: Values, pub values: Values }
pub(crate) struct SessionsView { pub at: DateTime<Utc>, pub this_host: Option<String>,
    pub hosts: Vec<HostState>, pub sessions: Vec<LiveSession>, pub sums: Vec<Sum>,
    pub switch_effect: SwitchEffect,
    pub currency: Option<String>, pub unavailable: Option<String> }
pub(crate) fn view_of(at: DateTime<Utc>, this_host: Option<String>, running: &[LiveSession],
    closed: &[LiveSession], other_hosts: &[OtherHost], thinking: &BTreeMap<String, u32>,
    unavailable: Option<&str>, currency: Option<&str>, options: &SessionsOptions) -> SessionsView;
```

`view_of` is pure. This host's running rows come first in their order, then the other hosts' by
name, then the closed rows newest first; a closed row whose `(host, session)` is already listed as
running is dropped, because the running row is the newer fact. `options.host` keeps one host and a
host that is not in
`hosts` gives an empty list. `hosts` starts with this host (`this: true`, `at` the pass time, never
stale) when there is a history, then the other hosts by name. `switch_effect` counts this host's
running rows only, whatever `options.host` and `closed` say; `loses_thinking_busy` is the busy part
of `loses_thinking`, and `loses_thinking_models` holds the distinct model ids of those sessions in
the order their first session is listed. `add_agents` is the one rule that puts a session's linked
flash/Harness workers on a row: it runs for a running row and the closed record counts the same
workers, so Agents, Tokens and both values do not change when the session closes. The account in use
is not part of the view: the page reads it from the snapshot, like the pop-up does.

`sums` is built over exactly the sessions the view lists, by `sums_of`, so a group's total and the
rows agree and the page never adds values. It has `All` (even for an empty list) and one key for
each provider, project, provider's model and model that has a session. (1.4.5 removed `ProjectModel`:
By project lists sessions directly under the project.) `ProviderModel` is one
provider's model (the "By model" total row), while `Model` is the Models card's total over every
provider. `Sum.tokens` adds the token counts; `Sum.last_hour` is `None` when any row of the group has
an unknown last hour, never a sum over the rest; `Sum.last_hour_values` and `Sum.values` are
`Values::sum` over the group's row values: each half adds its known parts and stays partial with the
first missing part's reason when another part is missing (R3); two API currencies are not added;
`agents_running` and `agents_done` add the row counts, and a total row with no agent data
shows `—`. A model total row carries the `tied` bubble when its thinking is tied to the account.
`switch_effect` counts only rows whose provider is `Claude Code` and whose model is absent or a plan
model (`money::is_plan_model`, the id starts with `claude-`); OpenAI, Ollama, Harness, flash and
other models are left out.
`LiveSession` carries `project_id: ProjectId` beside `project`: the post-join key groups two
projects with the same name apart, while `project` is only the shown name.

The closed rows are valued with a `Valuer` built for exactly `[since, now]` (`build_pricing` now
takes the range), with the rows context's projects; a part that is missing is `None` /
`Pricing::Unknown` as for the running rows. Each choice's result is kept with its `ClosedSince`, its
read time, its failure and the time it was last asked for; the passes refresh only the
subscription's choice, while a `View` command's choice is read and kept beside it. A choice is read
when it is not held, a running session of this host left the list, or `CLOSED_EVERY_SECS` (60 s)
passed; a choice nobody asked for in `CLOSED_KEEP_SECS` (300 s) is dropped. `ClosedSince::None`
reads nothing.

The view's currency is the settings' currency, read with the accounts file whether or not the prices
and the dollar rate could be read; with an accounts file that cannot be read it is `None` and that
failure is in `unavailable`.

A pass that cannot read the running sessions still leaves a view: it records the pass time, the
summary's `unavailable` text, the settings' currency, the other hosts' last read and the closed rows,
and pushes a view through the throttle. This host's running rows are then empty (they are not known,
never the old ones), while the other hosts and the closed rows are listed; `sessions_view` answers
that view, not "no pass yet". A `SystemProbe` that cannot be built takes the same path.

While subscribed, the thread watches the transcript folders of the running sessions and their
agents. The folders come from the keys `ScanState` already holds (`<folder>/<session>.jsonl`,
`<folder>/<session>/subagents/agent-<id>.jsonl`), then `projects_dir` is prefixed; the watches
follow the running list and are dropped on `Unsubscribe`. A pass then sends
`UiEvent::SessionsView(view)` through a `ViewThrottle`, which takes the time as an argument: at most
one view per `VIEW_EVERY_MS` (500 ms) with a trailing one, and one every 60 s even when nothing
changed. Without a subscription no view is sent and no transcript folder is watched.

The commands never starve the passes: after a pass the thread computes one deadline for its next
pass (the 60 s wake, the 1 s after an incomplete scan, or the pending trailing view, whichever is
first) and every `recv_timeout` waits only for the rest of it. Handling a `View`, `Subscribe` or
`Unsubscribe` never moves the deadline later; a `Subscribe` or a watcher wake starts the next pass
at once.

The three commands are registered with the others and are `async`, like `money_view`; each runs its
client call on `tauri::async_runtime::spawn_blocking`. The large window's `Destroyed` event sends
`Unsubscribe`. `gui::handle_ui_event` emits `sessions-view` with
`emit_to(WINDOW_LABEL, …)`, so only the large window receives it.

## Files step C2 writes

- `<history root>/<host>/sessions-now.json` — this host's live report for the other hosts' Sessions
  area (step C2's one new file). Pretty JSON, atomic, `Access::Private`, written in this host's own
  folder: `HostReport { at, sessions: Vec<LiveSession> }`, where every agent's `task` is `null` and
  no message text is in it. It is written when the rows changed and the last write is at least 30 s
  old, at once when a running session left the list, and as a 5-minute heartbeat; a failed write is
  the history warning on the summary. `HistoryDir::sessions_now_file` is the only path to it. The
  import never copies it and `folder_bytes` leaves it out of a host folder's size; no reader of the
  history sees it as a record.
- `<data folder>/transcripts-state/<folder>.json` — the scan state, with `totals`, `byModel`,
  `agents`, `spawns`, `recent` and `thinking` on `SessionSeen`, plus 1.2.0's `recorded` (the Harness
  totals kept while a session file exists), `flash_jobs` (the job ids a transcript names) and
  `FileState.flash_tools` (the tool-result ids already read). All are `#[serde(default)]`. The state
  is a cache, and its version is `STATE_VERSION = 2`: version 2 keeps the last hour's tokens per
  model. `ScanState::load` migrates a version 1 (1.1.0) file to version 2, keeping every offset, and
  marks it changed so the next save writes it back, so no transcript is read again. A file of any
  other version is an error, never a rebuild: `<path>: state version <found> is not supported; this seatswap reads
  version 1 and version 2` (a higher version adds ` (the file was written by a newer seatswap)`).
  The pass then reads no transcript and writes no token or session line; the window shows `the
  transcript state cannot be read: <error>; no transcript is read until it can be. Deleting
  <folder> reads every transcript again and writes its tokens to the history a second time`.
  Deleting the folder makes the next pass read the transcripts from the start, and the records
  already written are written again (the history appends without a dedupe).
- `<data folder>/transcripts-state/harness.json` — the Harness half of the scan state: the recorded
  totals per Harness id and file (`HarnessRecorded { tokens, messages, file }`), the closed flash
  jobs (`ClosedJob { session, last }`) and the flags that say whether either changed. It sits beside
  the per-folder files and is written atomically and private like them. `prune_harness_recorded`
  drops a total only when its session file is gone; `prune_closed_flash_jobs` drops a closed job
  after 24 h.
- The history files keep the shapes step A gave them, plus `ollama-YYYY-MM.jsonl` (the Ollama hourly
  lines; `Kind::Ollama`) and the `reset`/`from` fields of a weekly `WindowRecord`. The sessions
  thread writes the same `tokens-YYYY-MM.jsonl`, `sessions-YYYY-MM.jsonl` and wait event lines; a
  linked flash job's tokens are written under its Claude Code session, and an unlinked one under its
  Harness id.
- The history files keep the shapes step A gave them. The sessions thread writes the same
  `tokens-YYYY-MM.jsonl`, `sessions-YYYY-MM.jsonl` and wait event lines; step C2 adds no file kind
  there.

## `stats` (step C3)

`crates/core/src/stats/` holds everything the statistics pages show (spec §3.1–§3.6, §3.9),
computed from the history, the prices and the project log. **Statistics only read:** no function of
this module opens a file for writing. `stats` is public from `lib.rs`; `fixture` is compiled only
for tests or the core feature `fixtures`.

```rust
pub enum StatsError { History(HistoryError), Money(MoneyError), Message(String) }
pub enum RangeKey { ThisWeek, LastWeek, Days30, Days90, Months12, All }
pub struct Query { pub range: RangeKey, pub accounts: Vec<String>, pub host: Option<String> }
// Every function below that takes a `Ctx` declares `Tz: TimeZone + Copy`; the listings
// write the short `ctx: &Ctx<Tz>` for space. `Ctx`'s zone defaults to `FixedOffset` for
// the fixture; the app passes `chrono::Local`.
pub struct Ctx<'a, Tz: TimeZone + Copy = FixedOffset> { pub history: &'a HistoryDir,
    pub accounts: &'a [Account], pub provider_subscriptions: &'a [ProviderSubscription],
    pub settings: &'a Settings, pub prices: &'a Prices,
    pub fx: &'a Fx, pub projects: &'a Projects,
    pub warnings: &'a RefCell<Vec<String>>, pub now: DateTime<Utc>, pub zone: Tz }
// The provider subscriptions come from `accounts.json` and are inputs of every page: the
// subscriptions page, This week's provider rows, the blocked hours and the Sessions values.

/// ThisWeek: this week's Monday .. now. LastWeek: the Monday before .. this Monday.
/// Days30/Days90/Months12: the Monday of the week holding now − 30/90/365 days .. now.
/// All: the Monday of the week holding the first record .. now; this week's Monday when
/// the history is empty.
pub fn range_of<Tz: TimeZone + Copy>(key: RangeKey, ctx: &Ctx<Tz>)
    -> Result<Range, StatsError>;
pub fn week_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> DateTime<Utc>;
pub(crate) fn local_hour_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz)
    -> DateTime<Utc>;
pub(crate) fn local_day_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz)
    -> DateTime<Utc>;
pub(crate) fn next_local_day_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz)
    -> DateTime<Utc>;
pub(crate) fn next_week_start<Tz: TimeZone + Copy>(week: DateTime<Utc>, zone: &Tz)
    -> DateTime<Utc>;
pub(crate) fn previous_week_start<Tz: TimeZone + Copy>(week: DateTime<Utc>, zone: &Tz)
    -> DateTime<Utc>;

pub struct AccountWeek { pub week: DateTime<Utc>, pub account: AccountRef,
    /// Every weekly window of the account whose midpoint falls in this calendar week, oldest end
    /// first: the finished ones, and with `account_weeks_so_far` the running one last. A week with
    /// a reset holds both windows.
    pub windows: Vec<WindowRecord>,
    /// The last of `windows` still runs (`Valuer::running_windows`): its use is the use so far
    /// and it has expired nothing yet.
    pub running: bool,
    /// The last of `windows`: the one whose end, lost percent and plan the week shows.
    pub window: WindowRecord,
    /// The sum of the windows' use, each `final − from` (`from` 0 when absent); it may pass 100.
    pub used: f64,
    /// The account's monthly price at the last window's midpoint, in the display currency.
    pub price: Option<f64>, pub price_missing: Option<Missing> }
pub fn account_weeks(ctx: &Ctx<Tz>, range: &Range, q: &Query)
    -> Result<Vec<AccountWeek>, StatsError>;
pub struct MonthValued { pub start: DateTime<Utc>, pub end: DateTime<Utc>,
    /// The share of the credits used: the newest reading's percent − the reading the month
    /// began at.
    pub used_percent: f64,
    /// `used_percent / 100 × the month's price`; `None` without a price, never 0.
    pub money: Option<f64>,
    /// What the finished month left unused; `Some(0.0)` while it runs; `None` without a price.
    pub expired: Option<f64> }
/// The finished and running months of the Ollama subscription `account` overlapping `range`,
/// oldest end first.
pub fn subscription_months(ctx: &Ctx<Tz>, range: &Range, account: &AccountRef)
    -> Result<Vec<MonthValued>, StatsError>;
/// The working rhythm of one company in the range, learned from that company's records alone.
pub(crate) fn working_rhythm(ctx: &Ctx<Tz>, range: &Range, company: Company)
    -> Result<Rhythm, StatsError>;
/// The spans one company was blocked in the range, inside that company's own working hours.
pub fn blocked_spans(ctx: &Ctx<Tz>, range: &Range, q: &Query, company: Company)
    -> Result<Vec<BlockedSpan>, StatsError>;

pub(crate) fn account_weeks_valued(ctx: &Ctx<Tz>, range: &Range, q: &Query, valuer: &Valuer)
    -> Result<Vec<AccountWeek>, StatsError>;
/// `account_weeks_valued` with the windows running at `ctx.now`, each placed by its midpoint.
pub(crate) fn account_weeks_so_far(ctx: &Ctx<Tz>, range: &Range, q: &Query, valuer: &Valuer)
    -> Result<Vec<AccountWeek>, StatsError>;
/// Only the chosen accounts' running windows, one account week each, wherever their midpoint lies.
pub(crate) fn running_account_weeks(ctx: &Ctx<Tz>, q: &Query, valuer: &Valuer) -> Vec<AccountWeek>;
/// For a range that holds now: one sentence per week outside the range that a running window
/// counts in, e.g. `The running weekly windows of AC and LB count in last week, the week that holds
/// their middle.`; empty otherwise.
pub(crate) fn running_elsewhere(ctx: &Ctx<Tz>, range: &Range, q: &Query, valuer: &Valuer)
    -> Vec<String>;

/// The statistics frame: every host, when the history's subscription values begin and its size.
pub struct StatsFrame { pub hosts: Vec<String>, pub history_start: Option<DateTime<Utc>>,
    pub history_bytes: u64 }
/// The earliest usage reading or switch of any host; token and session records are older and do not
/// set it. `None` for a history with neither. Codex's and Ollama's readings are usage readings, so
/// they set it too. `history_bytes` is every host folder's size, as Settings → History counts it.
pub fn frame(history: &HistoryDir) -> Result<StatsFrame, StatsError>;

pub struct BlockedSpan { pub host: String, pub account: AccountRef,
    pub from: DateTime<Utc>, pub to: DateTime<Utc>, pub hours: f64, pub avoidable_hours: f64 }
pub fn blocked_spans(ctx: &Ctx<Tz>, range: &Range, q: &Query)
    -> Result<Vec<BlockedSpan>, StatsError>;
pub struct Empty { pub recording_since: Option<DateTime<Utc>> }
pub fn empty(ctx: &Ctx<Tz>, range: &Range) -> Result<Option<Empty>, StatsError>;
```

- `range_of` starts every range at a local Monday (spec §3.1's ranges); `All` uses the week of the
  first record, not the record's instant. A range whose start and end coincide is `Range::between`'s
  error, not an empty page.
- `account_weeks` is the one shared weeks list every statistics page reads. It reads every finished
  `weekly_all` window, puts it in the calendar week of its midpoint, keeps the window whose midpoint
  lies in `range`, filters with `account_chosen`, and prices the week with
  `Valuer::window_price(account, weekly_start, end)`, the price at the week's last window's
  midpoint. One entry per account and calendar week keeps **every** window of that week in
  `windows`; `used` sums their use; `window` is the last. A reset week therefore offers two windows
  and its `used` can pass 100 (spec §1.4). The cross-host rule (same account and end: the latest
  `lastSample`) is `HistoryDir::finished_windows`'s. A window whose end is below its start has no
  use: it adds 0 and is named once in the read's warnings.
- `account_weeks_so_far` adds `Valuer::running_windows(ctx.now)` to the finished windows. A running
  window goes into the week of its midpoint like a finished one (its start is seven days before its
  reset), so it never moves when it ends; its week is `running`, it offers one week's price and it
  expires nothing (`week_lost_percent` leaves the running window out). The Weeks page's summary,
  Overview's figures and weekly chart, and the full-weeks finding use it; the Expired share and the
  expired finding leave the running windows out. `running_account_weeks` builds the account weeks of
  the running windows alone, over every week (Overview's This-week card). `running_elsewhere` names
  the running windows whose midpoint lies in last or next week when the range holds now and does not
  show that week.
- `blocked_spans` reads all events, builds each host's blocked intervals for every limit kind and
  each host's account timeline (the last `Live` or `Switch`), and cuts every interval at the range,
  at every change of the account in use and at an `Unblocked`. Only a piece with
  `timeline.at(from) == account` becomes a span. `hours` counts only the rhythm's working hours;
  `avoidable_hours` is the part in which an account with `auto_burn` other than the blocked one is
  not blocked on that host and has a finished weekly window running (`start ≤ t < end`) whose final
  percent left something unused.
- `empty` asks whether any of the seven record kinds (sample, window, event, token, session, up,
  project log) has a record in the range. `recording_since` is the earliest record of any kind in
  the whole history, or `None` for a history with nothing.
- `working_rhythm(ctx, range)` is `burn::rates::rhythm` over this host's token records of the four
  weeks before `range.to`: the weighted work of each clock hour of the week. `working_hours` counts
  the seconds of an interval in hours whose rhythm value is above 0.

The shared helpers, used by more than one page:

| Helper | Job |
|---|---|
| `accounts_without_price` | the account ids with at least one account week that has no price: stored accounts in `Ctx::accounts` order, then removed accounts by id |
| `money_of` | the week's money: `paid = 100 × pv` for one week, `expired = the sum of every window's lost × pv`, `used = the week's summed use × pv`; `None` without a price. `offered_money` is `paid × the number of windows`, `offered_finished_money` the same without a running window (the Expired share's denominator), and `percent_of_offered` is the one Used/Expired share rule |
| `full_half` | `pub(crate) fn full_half(value: Option<f64>, partial: bool, missing: Option<&str>) -> (Option<f64>, Option<String>)`: a half's value when it is full, else `None` with the half's reason. A share, ratio or percent uses only full halves (R3); an amount alone may stay partial and is drawn `≥` |
| `priced_api` | `pub(crate) fn priced_api(values: &Values) -> Option<f64>`: the value's known API part, partial or not. A share of an API value divides a part's `priced_api` by the whole's, so priced work keeps its share when other work has no price (V-2); `None` when there is no priced part, and a whole whose priced part is `None` or 0 gives no shares |
| `api_shares` | `pub(crate) fn api_shares(by_project: BTreeMap<String, Values>, total: &Values) -> Vec<(String, f64)>`: each project's share of a total's priced API value in percent, through `top_three`; empty when the total's priced API value is unknown or 0. The Weeks summary's top projects use it |
| `subscription_shares` | `pub(crate) fn subscription_shares<'a, Tz>(ctx: &Ctx<Tz>, total: &Values, parts: impl IntoIterator<Item = (Option<String>, &'a Values)>) -> Vec<(Option<String>, f64)>`: each subscription's share of the priced API value of `total`, in percent, largest first, ties in the chips' order (`account_position`). A part without a priced API value is left out. The one split rule of the This-week card's project bars and `Node::by_subscription` / `ProjectDetail::by_subscription` |
| `lost_percent` | the window's `lost`; a reset window's absent `lost` is 0, and any other window without it is an error, because seatswap always writes it |
| `SwitchCounts`, `switch_counts` | `Switch` events of a range and host by `By::{User, Cli, Auto, Agent, External}` |
| `week_columns` | Monday 00:00 local of the week holding `range.from`, then each following Monday 00:00 local while the start is before `range.to` |
| `blocked_by_week`, `blocked_by_weekday` | spans split at every week start or local midnight; each part counts its working hours |
| `week_of_record`, `output_by_account_week` | the account week whose window holds an instant; output tokens per `(account, calendar week)` for the records inside those windows |
| `account_chosen` | empty `q.accounts` = every account and provider subscription; otherwise only a named one; a Claude record without a known account is never chosen by a filter |
| `stored_account`, `account_id`, `account_abbr`, `account_position` | the stored account behind a reference or its id/abbr when it is no longer stored; the store's order, removed accounts last |
| `top_three` | the three largest `(name, value)` by value, ties by name, then `("other", rest)` only when a rest is left |
| `money_text`, `hours_text`, `percent_text`, `thousands`, `join_names`, `date_text` | the pages' exact formats; `money_text` matches `ui/common.js`'s `money` |

Every page module has a public entry point taking the `Query` (the forecast takes the burn inputs
and regime; `window_detail` takes the account and end) and most also have a `pub(crate) ..._in`
taking a `Range` for their tests; each produces one serde camelCase struct:

```rust
// overview.rs
/// `kpis`, `weeks`, `marks`, the weekly chart and the full-weeks finding count every weekly window
/// of the range, the running ones included (`account_weeks_so_far`); the Expired share and the
/// expired finding use finished windows only. The card on top is `this_week_card`.
pub struct Overview { pub without_price: Vec<String>,
    pub kpis: Kpis, pub weeks: Vec<OverviewWeek>, pub marks: Vec<Mark>, pub findings: Vec<String> }
/// The three totals over every record the card counts, one row per project in the counted
/// records, and one row per running subscription window.
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct ThisWeek { pub accounts: Vec<ThisWeekAccount>, pub providers: Vec<ThisWeekProvider>,
    pub totals: ThisWeekTotals, pub projects: Vec<ThisWeekProject> }
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct ThisWeekTotals {
    /// The output tokens of every record the card counts.
    pub tokens_out: u64,
    /// The number of entries in `ThisWeek::projects`.
    pub projects: u32,
    /// The two values of every counted record, summed with `Values::sum`.
    pub used: Values,
    /// What the running windows' weeks cost: a Claude account's running week at its week price
    /// (`money_of`'s `paid`), an OpenAI subscription's running week at its week price, and an
    /// Ollama month as `price_month × 12 / 52` (documented rule). A window without a price adds
    /// nothing; `None` while no listed window has a price.
    pub paid: Option<f64>,
    /// A listed running window has no price, so `paid` holds only the others' sum (R3).
    pub paid_partial: bool,
    /// The rows with a running window that `paid` leaves out: their work is in no record.
    pub paid_without: Vec<String>,
    /// `used.subscription / paid × 100`; `None` while either is unknown, `paid` is 0, or either
    /// is partial ([`full_half`], R3).
    pub used_of_paid: Option<f64> }
/// One project row: name after the project log's joins, key, output tokens, both `values`, and
/// each subscription's share of its priced API value (`subscription_shares`, V-2), largest first;
/// `None` is work without a known account or named subscription. Empty when the project has no
/// priced API value or it is 0. Sorted by the known subscription part, then tokens, then name; the
/// page shows the first eight and folds the rest.
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub struct ThisWeekProject { pub name: String, pub key: Option<ProjectKey>,
    pub tokens_out: u64, pub values: Values,
    pub by_subscription: Vec<(Option<String>, f64)> }
/// One entry per chosen account with a running weekly window, stored accounts' order then others
/// by id; one per stored provider subscription and one per unnamed company with records this week,
/// `Company` order then stored order. A provider row without a running window has `used: None` and
/// the page does not draw it.
/// A provider subscription's row: its records from its running window's start to now — OpenAI:
/// its `weekly_all` week; Ollama: `week_start(now)`, the calendar week's start, like Claude work
/// without a known account — host filter applied, account chips ignored, summed with `Values::of`
/// / `Values::sum`. `used` is its running window's use (`None` with `resets_at` absent for a
/// finished month); an Ollama row's percent and `money` are its current month's, so its work and
/// its paid share cover the same week. `money_missing` is the reason when it has no price.
/// `name` is the subscription's abbr.
pub struct ThisWeekProvider { pub name: String, pub company: Company,
    pub used: Option<f64>,
    pub money: Option<f64>, pub money_missing: Option<String>,
    pub resets_at: Option<DateTime<Utc>> }
pub struct ThisWeekAccount { pub id: String,
    /// Use so far, `final − from` (`window_used`); `now` is the newest reading's percent.
    pub used: f64, pub now: f64,
    /// `money_of` of the running account week: `used × pv` at the window's price; `None` with
    /// the price's `Missing` text in `money_missing`.
    pub money: Option<f64>, pub money_missing: Option<String>,
    pub started_at: DateTime<Utc>, pub resets_at: DateTime<Utc> }
pub struct Kpis { pub used: f64, pub paid: f64, pub expired: f64,
    /// The used part of the offered weeks, in percent: every weekly window of the range — the
    /// running ones included — offers one week of its account's price, so a week that a reset
    /// split offers each of its windows. `used + expired + still open` make 100 % of them, except
    /// that a window whose use passed 100 % counts its whole use, so the two shares can then pass
    /// 100 % together (nothing is cut). `None` when no account week has a price.
    pub used_percent: Option<f64>,
    /// The expired part of the finished weeks, in percent: every finished window's `lost`, over
    /// the money those finished windows offered (a running window has expired nothing yet, so it
    /// is not in the denominator). `None` when no finished account week has a price.
    pub expired_percent: Option<f64>,
    pub expired_by_account: Vec<(String, f64)>,
    /// One entry per company with working hours in the range, Anthropic first: a provider's
    /// blocked hours are not Anthropic's and are never added into them.
    pub blocked_by_company: Vec<BlockedByCompany>,
    pub blocked_before_added: Option<BlockedBefore>,
    pub avoidable_hours: f64, pub switches: SwitchCounts, pub switches_total: u32,
    pub switch_cost_percent_of_window: Option<f64>, pub weeks: u32 }
pub struct BlockedBefore { pub name: String, pub hours: f64 }
pub struct BlockedByCompany { pub company: Company, pub hours: f64 }
pub struct OverviewWeek { pub week: DateTime<Utc>, pub accounts: Vec<WeekAccount>,
    pub projects: Vec<(String, f64)>, pub blocked_hours: f64, pub expired_share: Option<f64> }
pub struct WeekAccount { pub id: String, pub used_percent: f64, pub used: Option<f64>,
    pub expired_percent: f64, pub expired: Option<f64>, pub uncertain: bool }
pub struct Mark { pub at: DateTime<Utc>, pub label: String }
pub fn overview(ctx: &Ctx<Tz>, q: &Query) -> Result<Overview, StatsError>;
/// The This-week card on top of the Overview: ignores `q.range`, keeps its accounts and host.
pub fn this_week_card(ctx: &Ctx<Tz>, q: &Query) -> Result<ThisWeek, StatsError>;

// weeks.rs
pub struct WeeksPage { pub week_starts: Vec<DateTime<Utc>>, pub running: Vec<bool>,
    pub rows: Vec<WeeksRow>, pub blocked: Vec<f64>, pub recent: Vec<Curve>,
    pub now: DateTime<Utc> }
pub struct WeeksRow { pub id: String, pub plan: Option<String>, pub cells: Vec<Option<WeekCell>>,
    /// The running window's newest percent, and the calendar week it counts in (its midpoint's
    /// week, maybe last or next week): the row's "now" cell opens that week's summary.
    pub now_percent: Option<f64>, pub now_week: Option<DateTime<Utc>> }
pub struct WeekCell { pub end: DateTime<Utc>, pub used: f64,
    /// One part per window of the account week, oldest first; their sum is `used`, and the page
    /// draws one stack per part (a reset week has two). A part that ended below its start is 0.
    pub parts: Vec<f64>,
    /// The end of each window of `parts`, in the same order: a click on a part opens that
    /// window's detail, also when the part shares its week.
    pub window_ends: Vec<DateTime<Utc>>, pub expired: f64,
    pub expired_money: Option<f64>, pub full: bool, pub uncertain: bool }
pub struct Curve { pub id: String, pub points: Vec<(DateTime<Utc>, f64)> }
pub struct WeekSummary { pub week: DateTime<Utc>,
    /// `used / offered × 100`, where the week offered one week's price per window (so a reset
    /// week offers two); never divided by the one-week `paid`.
    pub used_percent_of_paid: Option<f64>, pub used: Option<f64>, pub expired: Option<f64>,
    /// For the week that holds now without a window: the `running_elsewhere` sentences.
    pub money_missing: Option<String>,
    /// A window of the week still runs (`account_weeks_so_far`): its use is so far, and
    /// `used_change_percent` is `None`.
    pub running: bool,
    pub blocked_hours: f64, pub switches: SwitchCounts,
    pub switches_total: u64, pub per_account_percent: Vec<(String, f64)>,
    /// `expired / offered × 100` over the finished windows' offer only (`offered_finished_money`).
    pub expired_percent_of_paid: Option<f64>,
    pub top_projects: Vec<(String, f64)>, pub tokens: Tokens, pub values: Values,
    pub used_change_percent: Option<f64>, pub conclusion: Vec<String> }
pub struct WindowDetail { pub id: String, pub start: DateTime<Utc>, pub end: DateTime<Utc>,
    pub plan: Option<String>, pub weekly_price: Option<f64>, pub used: f64,
    pub used_money: Option<f64>, pub expired: f64, pub expired_money: Option<f64>,
    pub reached_100_at: Option<DateTime<Utc>>, pub blocked_hours_after: f64,
    pub session_windows_used: Option<u32>, pub possible_session_windows: Option<u32>,
    pub used_more_than_possible: bool, pub note: Option<String>,
    pub switches_in_out: u32, pub switches_auto: u32,
    pub last_sample_before_end_secs: i64, pub uncertain: bool,
    pub curve: Vec<(DateTime<Utc>, f64)> }
pub fn weeks(ctx: &Ctx<Tz>, q: &Query) -> Result<WeeksPage, StatsError>;
pub fn week_summary(ctx: &Ctx<Tz>, q: &Query, week: DateTime<Utc>)
    -> Result<WeekSummary, StatsError>;
pub fn window_detail(ctx: &Ctx<Tz>, account_id: &str, end: DateTime<Utc>)
    -> Result<WindowDetail, StatsError>;

// projects.rs
pub enum NodeKind { Org, Project, Folder }
pub struct Node { pub kind: NodeKind, pub level: u8, pub name: String,
    pub key: Option<ProjectKey>, pub org_accounts: Vec<String>, pub tokens: Tokens,
    /// This node's API value as a share of the page's, in %; `None` when the node has no priced
    /// API value or the page's total has none, or the total is zero.
    pub share: Option<f64>, pub values: Values,
    /// Each subscription's share of this node's priced API value in % (`subscription_shares`,
    /// V-2), largest first, ties in the chips' order; `None` is work without a known account or
    /// named subscription. Empty without a priced API value or at 0.
    pub by_subscription: Vec<(Option<String>, f64)>,
    pub sessions: u32 }
pub struct ProjectsPage { pub nodes: Vec<Node>, pub matrix: Matrix,
    pub tokens_per_percent: Vec<(String, f64)>, pub unknown_account_tokens: Tokens,
    pub without_price: Vec<String>, pub notes: Vec<String>, pub usd_rate: Option<UsdRate> }
pub struct UsdRate { pub value: f64, pub date: Option<NaiveDate>, pub currency: String,
    pub varies: bool }
pub struct Matrix { pub orgs: Vec<String>,
    /// The chosen Claude accounts in `Ctx::accounts` order, then the chosen provider
    /// subscriptions in the chips' order; the ids `Node::by_subscription` writes.
    pub subscriptions: Vec<String>,
    /// Subscription value per organization and subscription; `None` when a record there has no
    /// subscription value, `Some(0.0)` when it has no record there.
    pub cells: Vec<Vec<Option<f64>>>,
    /// The cells' `Values::sum` per organization, per subscription, and over everything; a
    /// missing cell leaves the known part and a partial total (R3), never unknown.
    pub row_totals: Vec<Values>, pub column_totals: Vec<Values>, pub total: Values,
    pub sentences: Vec<String> }
pub struct ProjectDetail { pub name: String, pub tokens: Tokens, pub agent_share: f64,
    /// Each host's share of the priced API value, largest first.
    pub hosts: Vec<(String, f64)>, pub values: Values,
    pub by_subscription: Vec<(Option<String>, f64)>,
    /// The project's API value as a share of the page's, `None` without priced work.
    pub share: Option<f64>,
    pub weekly_by_model: Vec<(DateTime<Utc>, Vec<(String, u64)>)>, pub sessions: u32 }
pub fn projects_page(ctx: &Ctx<Tz>, q: &Query) -> Result<ProjectsPage, StatsError>;
pub fn project_detail(ctx: &Ctx<Tz>, q: &Query, key: &ProjectKey)
    -> Result<ProjectDetail, StatsError>;

// plans.rs (the Subscriptions page, R6 of 1.3)
pub struct SubscriptionsPage { pub totals: Totals, pub plans: Vec<PlanRow>, pub sizes: Sizes,
    pub usage: Vec<UsageGroup>, #[serde(rename = "usageTotal")] pub usage_total: UsageRow,
    #[serde(rename = "perPercent")] pub per_percent: Vec<PerPercent>,
    #[serde(rename = "tokensWeek")] pub tokens_week: TokensWeek,
    pub currency: Option<String> }
/// A sum with a missing part is None, never 0 (M-2 decision 9).
pub struct Totals { pub count: u32, pub price_month: Option<f64>, pub api_month: Option<f64>,
    pub times_paid: Option<f64>, pub tokens_used_month: Option<f64>, pub weeks: u32,
    pub partial: Vec<Partial> }
pub struct Partial { pub id: String, pub weeks: u32 }
pub struct PlanRow { pub id: String, pub company: Company, pub plan: Option<String>,
    /// A full limit (Ollama: a month's) as hours of actual work and as tokens: 100 / factor
    /// API dollars over the company's WorkedHour (1.4.1). None with `missing`, or without a
    /// worked hour; both or neither.
    pub full_hours: Option<f64>, pub full_tokens: Option<f64>,
    pub trend: Option<Change>,
    /// too_few_for_change: "too few weeks to tell a change: 7 of the 10 it needs" (Ollama:
    /// months) while the points are fewer than POINTS_BEFORE_CHANGE + 2; the page shows "—"
    /// with it instead of "steady". PerPercent carries the same as `change_missing`.
    pub trend_missing: Option<String>,
    pub basis_hours: Option<f64>,  // basis_usd in worked hours
    pub thin: bool,
    /// Why the row has no figures, no basis and no cells; None when it has them.
    pub missing: Option<String> }
/// Against the most measured Anthropic plan; `reference` is None when none is measured.
pub struct Sizes { pub reference: Option<String>, pub rows: Vec<SizeRow> }
pub struct SizeRow { pub id: String, pub plan: Option<String>,
    pub of_reference: Option<f64>, pub low: Option<f64>, pub high: Option<f64>,
    /// The ratio its name gives; None when either name has no capacity or no reference.
    pub by_name: Option<f64>, pub measured: bool }
pub struct UsageGroup { pub company: Company, pub count: u32,
    pub price_month: Option<f64>, pub rows: Vec<UsageRow> }
pub struct UsageRow { pub id: String, pub plan: Option<String>, pub since: Option<DateTime<Utc>>,
    pub price_month: Option<f64>, pub entered: Option<EnteredPrice>,
    pub used_percent: Option<f64>, pub expired: Option<f64>, pub api: Option<f64>,
    pub times_paid: Option<f64>, pub missing: Option<String> }
/// A stored price entered in a currency other than the display currency.
pub struct EnteredPrice { pub amount: f64, pub currency: Option<String> }
pub struct PerPercent { pub id: String, pub limit: String, pub model: String,
    pub points: Vec<Point>, pub change: Option<Change>, pub now: Vec<Now>,
    /// Minutes of actual work 1 % of the limit holds (1 / factor over the worked hour).
    pub minutes: Option<f64>,
    pub window_holds_minutes: Option<f64> }  // 5h only: 100 × minutes
pub struct Point { pub week: DateTime<Utc>, pub tokens: f64, pub low: f64,
    pub high: Option<f64> }
pub struct Now { pub model: String, pub tokens: f64 }  // tokens 1 % buys of the model
pub struct Change { pub model: String, pub since: DateTime<Utc>, pub percent: f64,
    pub before: f64, pub after: f64 }
pub struct TokensWeek { pub models: Vec<TokenModel>, pub rows: Vec<TokensWeekRow> }
pub struct TokenModel { pub model: String, pub company: Company }
pub struct TokensWeekRow { pub id: String, pub cells: Vec<TokensWeekCell>,
    pub missing: Option<String> }
#[serde(untagged)] // a figure, or {"missing": "<reason>"}: "not offered by {Company}",
pub enum TokensWeekCell {  // "no price for {model}", or why the estimate failed
    Figure { tokens: f64, low: f64, high: Option<f64>, estimated: bool, thin: bool,
        measured: bool },  // measured: the factor's own flag (false: leans on its prior)
    Missing { missing: String } }
const THIN_SHARE: f64 = 0.2;          // a thin figure: half its range over 20 % of its value
const POINTS_BEFORE_CHANGE: usize = 8; // the weeks before a change that set its range
const NO_FACTOR_REASON: &str = "not measured yet: no pair with priced work in this range";
const NO_PRICED_MODEL_REASON: &str = "no priced model it ran in this range";
pub fn subscriptions_page(ctx: &Ctx<Tz>, q: &Query) -> Result<SubscriptionsPage, StatsError>;
pub(crate) fn subscriptions_in(ctx: &Ctx<Tz>, range: &Range, q: &Query)
    -> Result<SubscriptionsPage, StatsError>;

/// A stored price typed close to a plan change belongs to the new plan (documented rule).
pub const PRICE_BEFORE_PLAN_MIN: i64 = 15;
pub const NO_FINISHED_WEEK: &str = "no finished week yet in this range";
pub const NO_FINISHED_MONTH: &str = "no finished month yet in this range";
pub struct Period { pub id: String, pub plan: Option<String>, pub price: Option<f64>,
    pub from: DateTime<Utc>, pub to: Option<DateTime<Utc>> }
pub(crate) struct HistoryReads<'a> { pub events: &'a [Hosted<Event>],
    pub samples: &'a [Hosted<Sample>], pub windows: &'a [Hosted<WindowRecord>] }
pub(crate) fn periods_for_chosen_from(ctx: &Ctx<Tz>, q: &Query, valuer: &Valuer,
    reads: HistoryReads<'_>) -> Result<BTreeMap<AccountRef, Vec<Period>>, StatsError>;
pub(crate) fn removed_periods(ctx: &Ctx<Tz>, range: &Range, q: &Query, valuer: &Valuer,
    reads: HistoryReads<'_>) -> Result<BTreeMap<AccountRef, Vec<Period>>, StatsError>;
#[cfg(test)] pub(crate) fn periods_for_chosen(ctx: &Ctx<Tz>, q: &Query, valuer: &Valuer)
    -> Result<BTreeMap<AccountRef, Vec<Period>>, StatsError>;
pub(crate) struct UsageInput<'a> { pub id: String, pub plan: Option<String>,
    pub since: Option<DateTime<Utc>>, pub price: Option<f64>, pub price_missing: Option<String>,
    pub stored_price: Option<(f64, Option<String>)>, pub currency: Option<&'a str>,
    pub weeks: &'a [&'a AccountWeek], pub months: &'a [MonthValued], pub values: Values,
    pub ollama: bool, pub now: DateTime<Utc>, pub finished_periods: u32 }
pub(crate) fn usage_row(input: UsageInput<'_>)
    -> Result<(UsageRow, Option<f64>), StatsError>;

// forecast.rs
// forecast.rs
pub struct Forecast { pub basis_weeks: u32,
    /// The accounts auto-burn may use whose week factor exists but is not measured (assumed
    /// from the plan), and those with no factor yet, in account order.
    pub assumed: Vec<String>, pub unknown: Vec<String>,
    pub kpis: ForecastKpis, pub accounts: Vec<ForecastAccount>, pub lanes: Vec<Lane>,
    pub blocked: Vec<(DateTime<Utc>, DateTime<Utc>)>, pub from: DateTime<Utc>,
    pub to: DateTime<Utc>, pub sentence: String }
pub struct ForecastKpis { pub expire_planned: Option<f64>,
    pub expire_only_when_full: Option<f64>, pub blocked_hours: f64, pub switches: u32,
    pub switches_per_day: f64, pub switches_only_when_full: u32 }
pub struct ForecastAccount { pub id: String, pub now: f64, pub expected: Option<f64>,
    pub low: Option<f64>, pub full_at: Option<DateTime<Utc>>, pub resets_at: DateTime<Utc>,
    pub only_when_full_expected: Option<f64>, pub only_when_full_expires: Option<f64> }
pub struct Lane { pub id: String, pub blocks: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub resets: Vec<DateTime<Utc>> }
pub fn forecast(ctx: &Ctx<Tz>, inputs: &Inputs, regime: &Regime) -> Result<Forecast, StatsError>;

`ForecastAccount.low` is this week's percent now plus the second-lowest gain of the last 8 finished
windows, at most 100, or `None` with fewer than four gains. A window's gain is its final percent
minus the newest weekly reading at or before `end − d` that names its reset (`d` is the time from
now to the reset the plan plays the account to). A reset window names its reset by its start + 7
days, an ordinary window by its end. A week with a reset counts every one of its windows; a window
without such a reading and a negative gain (a model reset's drop is not use) are left out.

// activity.rs
pub struct RhythmPage { pub heat: Vec<f64>, pub per_day: Vec<DayRow> }
pub struct DayRow { pub weekday: u8, pub working_hours: f64, pub blocked_hours: f64 }
pub enum HourState { Off, Idle, Work, Unknown }
pub struct HourCell { pub t: DateTime<Utc>, pub state: HourState,
    pub by_model: Vec<(String, u64)>, pub sessions: u32, pub top_project: Option<String>,
    pub values: Values }
pub struct HostLane { pub host: String, pub hours: Vec<HourCell>, pub working: u32,
    pub per_working_hour: Option<f64>, pub idle: u32, pub off: u32, pub unknown: u32,
    pub tokens: Tokens, pub values: Values }
pub struct DayTokens { pub date: NaiveDate, pub out: Vec<(String, u64)> }
pub struct ActivityWeek { pub week: DateTime<Utc>, pub working_hours: Vec<(String, u32)>,
    pub tokens: Tokens, pub values: Values, pub sessions: u32,
    pub top: Option<(String, String)> }
pub struct BigSession { pub label: String, pub title: Option<String>, pub project: String, pub host: String,
    pub model: Option<String>, pub worked_hours: u32, pub tokens: Tokens,
    pub values: Values, pub agents: Option<u32> }
pub struct ActivityPage { pub lanes: Vec<HostLane>, pub days: Vec<DayTokens>,
    pub weeks: Vec<ActivityWeek>, pub largest: Vec<BigSession>,
    pub waited: Vec<(NaiveDate, f64)>, pub peak_hour: Tokens,
    pub total_tokens: Tokens, pub total_values: Values,
    pub per_working_hour: Option<f64>, pub what_it_shows: String,
    pub ollama: OllamaUse }
/// What Ollama counted beyond the known sessions on Ollama models, for the one-line note under
/// the hour chart; serde `{"otherTokens": 1234567}`.
pub struct OllamaUse { pub other_tokens: u64 }
pub fn rhythm(ctx: &Ctx<Tz>, q: &Query) -> Result<RhythmPage, StatsError>;
pub fn activity(ctx: &Ctx<Tz>, q: &Query) -> Result<ActivityPage, StatsError>;
```

Page details that are easy to miss:

- `Overview::expired_share` is `Option` (a week without a priced account week has none); the four
  findings are the only sentences and are built in the fixed order above, each `None` when its data
  is missing. `Kpis::blocked_before_added` names the added account by its short name, also when the
  page's account filter leaves that account out. `this_week_card` ignores the range: it builds its
  own `Valuer` over `[now − 7 d, now]`, which holds the running windows and this calendar week; the
  Overview's `Valuer` covers only the range. The card's project split reads the token records of `[min(earliest running start, this week's
  Monday), now]`, keeps the query's host, values each with `Valuer::value` and `Values::of` as the
  Weeks summary does, and shares them with `subscription_shares`: a Claude record in its account's
  running window; an OpenAI record in its subscription's running `weekly_all` week; an Ollama record
  from `week_start(now)` on. Each provider record belongs to its subscription's `ThisWeekProvider`,
  whose own `used`, `money`, `money_missing` and `resets_at` are the running window's (for Ollama,
  the current month's). The card's totals `Values::sum` the same records, and its project rows use
  `subscription_shares` on each project's records.
- Pages of project work (Projects, project detail, Weeks summary, Activity) choose a record with
  `stats::record_chosen(ctx, q, &Valued)`: a Claude record by `account_chosen`, a provider record by
  its subscription. Pages about a subscription's own window (the Subscriptions page, tokens per
  percent, the matrix, the Overview's weekly project chart) need `Valued.account` and so leave a
  record whose subscription cannot be named out. `burn::rates::rhythm` and `work_in` (rates, switch
  cost) count Anthropic records only.
- `WeeksRow.cells` is one entry per `week_starts`; `None` means no finished window that week.
  A cell's `used` is the sum of its `parts`, one per window of the account week; a reset week has
  two parts and the page draws one bar with both. `WeeksPage.running` has one flag per column:
  `true` for the calendar week `Ctx::now` falls in; the page draws the "now" cell there and needs
  no browser clock. `now_percent` is the `final` of the account's window in
  `Valuer::running_windows(ctx.now)`, and `now_week` the week of that window's midpoint. `recent`
  covers the last 14 days. `session_windows_used` counts finished `session` windows with `final > 0` that ended inside the weekly window. `blocked_hours_after` uses the
  rhythm of the four weeks before `Ctx::now`, because a single window has no range of its own. The
  detail card's 5h windows line says "N used, more than the about M your usual working hours hold"
  when `used_more_than_possible` is true, and "of about M possible" otherwise.
- `projects_page` only adds folder rows when a project has more than one folder (`total_folders >
  1`); its `by_subscription` shares divide each subscription's `priced_api` by the node's, and
  `share` is the node's priced API value over the page's (`priced_api`; V-2). `tokens_per_percent`
  sums an account's output tokens over all its finished
  weeks and divides by the sum of those weeks' final percents; an account without a week, or whose
  weeks total 0 %, is left out. `notes` always carries the "use that leaves no transcript" text and
  adds the unknown-account and no-subscription-value sentences when they have counts, the
  `{tokens} tokens: account taken from the usage readings.` note when `Valuer` booked work from the
  readings (G-1), and `No price for {subscriptions}: their tokens add no value; shares count priced
  work only.` when the page has an unpriced subscription. `unknown_account_tokens` counts Claude
  records only. `project_detail` filters by `q.host`, so a folder key's detail counts every host's
  folder of that name only with all hosts chosen; its `by_subscription` uses the same share rule.
- `subscriptions`' `since` is the first period of the whole history, not of the range; `plan` and
  `price` are the latest visible period's.
- `forecast` uses `regime.early` to choose the planned policy (`Eager`/`Lazy`), always computes the
  `WhenFull` plan for "only when full", takes `HORIZON_H = 168` hours from `inputs.now`, reads the last eight finished windows
  and the samples that name their resets, and returns an error if a plan account is not stored.
  `low` is `state.week_percent + gains[1]` capped at 100, with fewer than four gains → `None`.
- `rhythm` divides the heat map and both per-day numbers by the range's calendar weeks (a partial
  week counts as one). `activity` lanes are the 168 hours ending with the hour holding `range.to`,
  the day chart the 14 local dates ending with the date holding `range.to`, and the largest sessions
  are the 20 with the most output tokens. `HostLane::per_working_hour` is the lane's own output
  tokens over its working hours (`None` without one); `BigSession::agents` is `None` while the
  session has no `SessionRecord`; `BigSession::title` is the newest record's title. The "What it shows" sentence says "night(s) with work on <host>";
  the hour lanes carry no agent data.
- `ActivityPage::ollama.other_tokens` sums, over the UTC hours whose start is inside the page's
  range, Ollama's hourly line (input + output; the largest line of that UTC hour over every host,
  never a sum) minus the input + output of every known session on an Ollama model (`…:cloud`) in
  that same UTC hour, over every host whatever the page's host filter, because the Ollama line is
  the whole account's. An hour where the known sessions used more than Ollama reported adds
  nothing, never a negative; an hour without an Ollama line adds nothing. The chart has no Ollama
  lane: the page shows the number only as a muted note under the hour lanes, and none when it is 0.

## `stats_*` commands in `gui/window.rs` (step C3)

```rust
pub(crate) enum Page<T> { Empty(Empty), Data(T) }        // serde: {"empty":{…}} | {"data":{…}}
pub(crate) struct StatsParts { accounts: Vec<Account>,
    provider_subscriptions: Vec<ProviderSubscription>, settings: Settings, prices: Prices,
    fx: Fx, projects: projects::Projects }
impl StatsParts {
    pub(crate) fn ctx<'a, Tz: TimeZone + Copy>(&'a self, history: &'a HistoryDir,
        now: DateTime<Utc>, zone: Tz) -> Ctx<'a, Tz>;
}
pub(crate) fn load_prices_and_fx(app: &AppPaths) -> Result<(Prices, Fx), String>;
pub(crate) fn stats_ctx_parts(app: &AppPaths, history: &HistoryDir)
    -> Result<StatsParts, String>;
fn stats_page<T, Tz: TimeZone + Copy>(parts: &StatsParts, history: &HistoryDir,
    now: DateTime<Utc>, zone: Tz, query: &Query,
    compute: impl FnOnce(&Ctx<'_, Tz>) -> Result<T, stats::StatsError>)
    -> Result<Page<T>, String>;
```

`stats_ctx_parts` loads `accounts.json` (with its provider subscriptions), the prices and the dollar rate through the one
`load_prices_and_fx` that `sessions_rt::build_pricing` also uses, and the project log. Every history read of a statistics page goes through
`history::keep_warnings`, which adds the read's warnings to `Ctx::warnings` (`StatsParts` holds the
`RefCell`, seeded with the projects' warnings); after the command the app prints the new ones
through the shared `PrintedWarnings` as `seatswap: history: {warning}`. `money::Valuer::build` takes
the same sink. The error texts are `the accounts cannot be
read: …`, `the prices cannot be read: …`, `the dollar rate cannot be read: …` and `the projects
cannot be read: …`. `stats_page` is the one place a page becomes a `Page`: it computes
`range_of(query.range)` and `stats::empty`; an empty range is `Page::Empty` with the recording date,
otherwise it calls the page function and wraps `Page::Data`.

Each page has a plain testable function and one async command. Every page command but
`stats_forecast` takes `query: Query`; the detail commands take the item they open, and
`stats_frame` is the separate frame (the hosts, the history's start and its size):

```rust
pub(crate) fn stats_overview_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<overview::Overview>, String>;
// The card ignores the range, so it never says "nothing recorded": it does not go through
// `stats_page` and answers the card itself, empty lists without a running window.
pub(crate) fn stats_this_week_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<overview::ThisWeek, String>;
pub(crate) fn stats_weeks_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<weeks::WeeksPage>, String>;
pub(crate) fn stats_week_summary_at<Tz: TimeZone + Copy>(parts, history, now, zone, query, week: DateTime<Utc>)
    -> Result<Page<weeks::WeekSummary>, String>;
pub(crate) fn stats_window_detail_at<Tz: TimeZone + Copy>(parts, history, now, zone, query,
    account_id: &str, end: DateTime<Utc>) -> Result<Page<weeks::WindowDetail>, String>;
pub(crate) fn stats_projects_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<stats_projects::ProjectsPage>, String>;
pub(crate) fn stats_project_detail_at<Tz: TimeZone + Copy>(parts, history, now, zone, query,
    key: &ProjectKey) -> Result<Page<stats_projects::ProjectDetail>, String>;
pub(crate) fn stats_subscriptions_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<plans::SubscriptionsPage>, String>;
pub(crate) fn stats_forecast_at<Tz: TimeZone + Copy>(parts, history, now, zone,
    kept: Option<KeptForecast>, no_plan: String) -> Result<Page<forecast::Forecast>, String>;
// no_plan = no_plan_text(burn, views): NO_PLAN, plus `: {abbrs} has/have none yet; it comes/they
// come with the next usage poll` while the burn view's `unread` (the accounts auto-burn waits for a
// first reading of) is not empty. forecast.js shows NO_PLAN, and every error starting `NO_PLAN: `,
// as a plain line.
pub(crate) fn stats_rhythm_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<activity::RhythmPage>, String>;
pub(crate) fn stats_activity_at<Tz: TimeZone + Copy>(parts, history, now, zone, query)
    -> Result<Page<activity::ActivityPage>, String>;
pub(crate) fn stats_frame_at(history: &HistoryDir) -> Result<stats::StatsFrame, String>;
```

`stats_forecast_at` takes no `Query`: the forecast is the latest plan kept from the auto-burn
runtime, for every account and host (spec §3.4), and `stats_forecast` asks only the state. It
returns the one error `the forecast needs a reading of every account first` both when the kept
inputs are `None` and when the oldest reading they were built from is more than `burn::FRESH_MIN`
(20 min) before the command's `now`. `stats_frame_at` is `stats::frame`, so a broken prices file
cannot hide the host list, the history's start or its size; `frame` reads no prices and no rate.

One private helper is the body of every page command (`stats_frame` reads only the frame):

```rust
async fn stats_command<T, F>(state: State<'_, super::GuiState>, run: F) -> Result<T, String>
where T: Send + 'static,
      F: FnOnce(&StatsParts, &HistoryDir, DateTime<Utc>, Local) -> Result<T, String>
          + Send + 'static;
```

It clones the app paths and the history handle, and inside `spawn_blocking` loads `StatsParts`,
takes `Utc::now()` and `Local`, the computer's time zone, and runs the page's function. The files are thus
read on the blocking thread, not on the async one. The async commands `stats_overview`,
`stats_this_week`, `stats_weeks`, `stats_week_summary`, `stats_window_detail`, `stats_projects`,
`stats_project_detail`, `stats_subscriptions`, `stats_forecast`, `stats_rhythm`,
`stats_activity` and `stats_frame` are registered in `gui/mod.rs`'s handler list; `stats_forecast`
reads the kept `(Inputs, Regime)` from `GuiState` before it enters the helper; `stats_frame` needs
only `state.history`.

## The forecast's inputs kept from the burn runtime (step C3)

```rust
// gui/burn_rt.rs
pub(crate) type KeptForecast = (Inputs, Regime, DateTime<Utc>);
pub(crate) struct BurnReport { …, pub forecast: Option<KeptForecast> }

// gui/worker.rs and gui/mod.rs
pub(crate) forecast: Arc<Mutex<Option<KeptForecast>>>
```

`BurnRuntime::tick`, after `live::assess` produced the situation, the regime and the plan, does
`report.forecast = Some((situation.inputs.clone(), regime.clone(), situation.oldest_reading))`. A tick that has no situation
(sessions off, no account, an unread account, or no learned rates) leaves it `None`; the worker
stores `report.forecast` on every report, `None` included, through `gui::lock`, so a tick without a
situation clears the kept forecast. `stats_forecast` reads that one field and hands it to
`stats_forecast_at`; it never recomputes a situation. The values are the plan's own `Inputs`
(each account's id, weekly percent and reset, session state and rates, `may_use` from the account's
`auto_burn`) and `Regime` (the early-switching verdict and the lazy/eager blocked hours), so the
Forecast page and auto-burn are computed from the same values; the page refuses the kept plan when
the oldest reading it was built from (`situation.oldest_reading`) is older than `FRESH_MIN`, with the
same text as no plan.

## The statistics fixture and `export_tiny` (step C3)

`crates/core/Cargo.toml` declares the empty feature `fixtures = []` and an example that requires it:

```toml
[[example]]
name = "export_tiny"
required-features = ["fixtures"]
```

Under `#[cfg(any(test, feature = "fixtures"))]`, `stats/fixture.rs` writes the fixture every
statistics test uses, as seatswap's own files:

```rust
pub struct TinyDir { /* path, optional guard */ }
impl TinyDir { pub fn path(&self) -> &Path; #[cfg(test)] pub(crate) fn keep(&mut self,
    guard: Box<dyn Any + Send>); }
pub struct Tiny { pub dir: TinyDir, pub history: HistoryDir, pub accounts: Vec<Account>,
    pub settings: Settings, pub prices: Prices, pub fx: Fx, pub projects: Projects,
    pub now: DateTime<Utc>, pub offset: FixedOffset, pub windows: Vec<WindowRecord> }
pub fn write(root: &Path) -> Result<Tiny, String>;
```

`write` refuses a root that exists and is not empty (`{root} is not empty`). It writes the history
under `AppPaths::at(root)`'s history folder for host `atlas`, `accounts.json`, `prices.json` and
`fx.json`, each through the module that owns that file (`history`, `store`, `money`; never a second
writer). The fixture is the plan's: host `atlas`, local offset +00:00, "now" Thu 19 Nov 2026 10:00,
accounts LB (Max, 180 EUR) and AC (Team Premium, 140 EUR), their four finished windows and the events,
token and session records of `step-c3.md`. `Tiny.dir` is the history root. The test helper
`stats::tests::tiny()` writes the fixture into a `tempfile::tempdir()` and keeps the `TempDir` in
`TinyDir`'s optional guard, so the folder is removed with the fixture; `fixture.rs` itself does not
depend on `tempfile`.

`crates/core/examples/export_tiny.rs` is a thin wrapper: `export_tiny <empty folder>` writes the
same fixture there and prints the folder; a missing folder, a second argument or a folder that
cannot be written is an error with its text. The tray bench starts the app on a throw-away data
folder (`--data-dir`); `export_tiny` fills such a folder with the fixture's history, accounts,
prices and dollar rate, so the Statistics pages open on a real history without touching any of the
person's files. The browser page bench does not need it: the pages fall back to
`crates/app/ui/fixture-stats.js`, whose answers have the same JSON shape as the Rust structs.

## `app: ui/charts.js` (step C3)

The statistics pages' charts are plain elements and SVG, no chart library, in
`crates/app/ui/charts.js`. Every colour is a CSS token or the colour the caller hands in (an
account's colour is its snapshot view's colour), so the charts follow the theme. One tooltip lives
here; a mark with a `tip` shows it on hover and on keyboard focus.

```js
export function stackedColumns({ columns, marks } = {});
export function lines({ series, yMax, marks, ySuffix, yDecimals, yFormat } = {});
export function bars({ categories, series, yMax, labels, yLabel, yTicks } = {});
export function grid({ rows, headers } = {});
export function heat({ rows = 7, cols = 24, values, tip } = {});
export function lanes({ lanes, from, to, ticks } = {});
export function hours({ lanes, days } = {});
export { attachTip, hideTip, showTip };
```

- `stackedColumns`: `columns: [{ label, sub?, at?, stacks: [[{ value, color, hatch?, tip? }]] }]`.
  Every stack is scaled to the largest stack total; a segment with `hatch` is drawn as a hatched
  fill. `marks: [{ at, label }]` are placed at the left edge of the first column that starts after
  the mark (a mark on a week's Monday sits at the next column); without `at` on the columns a mark
  is left out, never guessed.
- `lines`: `series: [{ name, color?, area?, points: [[time, value]], tip(time, value)? }]`,
  `yMax` (else the largest value), `ySuffix`, `yDecimals`, `yFormat(value)`. A point is the pair
  `[time, value]` the Rust structs serialise; points become focusable buttons, a `null` value
  breaks the line, and the x axis is the first, middle and last valid time.
- `bars`: `categories: [label]`, `series: [{ name, color, values: [number], unit?, tip(category, value)? }]`,
  optional `yMax` (a finite number above 0), `yLabel(value)`, `yTicks` (strictly rising from 0 to
  `yMax`) and `labels(value)`; a `null` value is a missing bar, written `—` with `labels`. A value
  that is neither a finite number nor `null` is an error naming the category, never a skipped point;
  a value above the fixed `yMax` is an error too. The grid lines and their labels sit in the bars'
  own box, so a line at value v is at the height a bar of v reaches.
- `grid`: `rows: [{ label, cells: [{ fill?, hatch?, full?, uncertain?, current?, color?,
  textColor?, text?, tip?, onClick? }] }]`, `headers: [label | { label, tip?, onClick?, selected?,
  current? }]`. A cell without `fill` is plain; the fill is its height in percent.
- `heat`: `values` is row-major, 7 rows of 24 columns, Monday first; the colour strength is the
  value over the largest; `tip(row, col, value)` names the weekday and hour.
- `lanes`: `lanes: [{ label, color?, className?, blocks: [[from, to]], blockTips?, marks: [time],
  markTips? }]`, `from`, `to`, `ticks: [{ at, label } | time]`; a block shorter than the axis still
  gets a sliver.
- `hours`: `lanes: [{ label, cells: [{ state: "work"|"idle"|"off"|"unknown", stack:
  [{ value, color }], strip?, tip? }] }]`, `days: [{ label, span }]`. The stack is scaled to the
  tallest hour; the strip under each lane is a heatmap of `strip`, the number of sessions working in
  that hour, scaled to the largest number in any lane and coloured by the one `chart-heat-tone`
  rule Rhythm's cells use; a missing count is 0, any other non-number or a negative is an error.
- **Fitting.** Every chart fits its text to the width it gets, once its width is known and again
  when it changes; fitting only shows, hides or rewords text, never resizes the chart. Axis labels
  show all, else every second counted from the newest, else (on a dated axis) month starts (`Oct`,
  `Nov`, the year at January), dropping touching ones oldest first; an edge label turns inwards. A
  group of numbers under or inside columns shows all or none (a short form first, `11h blocked` →
  `11h`); the tooltip always has them. A mark label takes the first of two lines where it touches
  nothing, else becomes a dot on its line with the text in its tooltip. Line charts draw their dots
  only on hover or focus when points are closer than 14 px. The Weeks grid narrows its columns to
  12 px and then scrolls only itself, keeping the row labels in view.
- Every builder throws on a missing required list and on a value it cannot read; a `null` is a
  missing point or bar, never a skipped or zero value (except `lanes`' `from`/`to`, which is an
  error). Nothing is clamped silently
  except a percent into 0..100 for a CSS position.

## `app: the Statistics area (step C3)`

```text
crates/app/ui/areas/statistics.js          the area: frame, range, chips, host, tabs, calls
crates/app/ui/areas/statistics/overview.js
crates/app/ui/areas/statistics/weeks.js
crates/app/ui/areas/statistics/projects.js
crates/app/ui/areas/statistics/subscriptions.js
crates/app/ui/areas/statistics/forecast.js
crates/app/ui/areas/statistics/rhythm.js
crates/app/ui/areas/statistics/activity.js
crates/app/ui/charts.js                    the seven chart builders above
crates/app/ui/fixture-stats.js             the browser fixture, the Rust structs' JSON shape
```

`statistics.js` exports `render(container, context)`, `update(snapshot)` and `unmount()`. `render`
reads the area's sub-page from the address, builds the frame and loads one page module at a time
(`import("./statistics/<page>.js")`), each with `show(container, env)`. `update(snapshot)` redraws
the frame and chips and reloads the page when the stored account set changes, not on every
snapshot. `env` is:

```js
{ query, call, ask, accounts, open, currency }
```

`query` is `{ range, accounts, host }`; the account list is empty while no chip is excluded, else
the remaining stored ids; `host` is `null` for all hosts. `call(command, extra?)` invokes a
`stats_*` command, throws an `EmptyRange` error for a `{"empty": …}` reply (carrying `recordingSince`)
and returns `reply.data` otherwise; `ask(command, extra?)` returns the reply as it comes, for
`stats_this_week`, which never answers "empty". The Overview asks `stats_this_week` and
`stats_overview` together and draws the card first; when `stats_overview` throws `EmptyRange`, the
card stays and the same "Nothing recorded yet…" line stands below it. `accounts` are the chosen snapshot views, `open(page)` asks
the area to switch page, `currency` is the snapshot's settings currency. A page draws into its own
stage and is swapped in only when its load still wins, so a slow page cannot overwrite a newer one.
An `EmptyRange` becomes the plain "Nothing recorded yet…" line; any other error becomes the area's
banner.

The frame has the range tabs, the account chips (at least one stays chosen; clicking one off writes
the exclusion list), the host `<select>` only when `stats_frame` answered more than one host, and the
seven page tabs. The range, excluded accounts, host and page are kept in
`localStorage` under `seatswap.stats.range`, `seatswap.stats.accounts`, `seatswap.stats.host` and
`seatswap.stats.tab`; the host choice falls back to all hosts when its remembered host is gone. A
page tab writes `statistics/<page>` to the address (`open(page)` does the same). `stats_forecast`
takes no query parameter (the area's generic call still adds the frame's query to the request, which
the command does not read): that page follows neither the range nor the chips nor the host.

The seven page modules only format and draw the struct; they add no sum, share, verdict or sentence
of their own:

- **Overview** draws the five numbers, the two stacks per week with the project legend, the
  blocked hours, the marks, "What the numbers say" and the expired-share line. Project colours are
  taken from a five-token cycle, *other* in the muted token.
- **Weeks** draws the grid (a date opens the week summary, a cell the window detail), the last-14-days
  curves and the detail card; it opens the newest finished week's summary.
- **Projects** draws the tree (By organization · Plain folder tree, rows open and close), the
  organization × subscription matrix and its sentences, the project detail and the notes; it opens
  the first project's detail.
- **Subscriptions** draws the totals line and the four cards in their order: *What does each plan
  give me?*, *Am I using what I pay for?*, *What 1 % of a limit buys, over the weeks* and *Tokens a
  week, per subscription and model*. There is no what-if replay and no row's period bars any more.
- **Forecast** draws the four numbers, the sentence, the per-account rows and the seven-day lanes.
  It shows `the forecast needs a reading of every account first` as a plain line, every other
  failure as the area's banner.
- **Rhythm** draws the 7 × 24 heat map and the working and blocked hours per weekday.
- **Activity** draws the per-host numbers, the 168-hour lanes with the unit switch (Tokens ·
  Subscription value · API value), the 14-day token bars, the per-week table and the largest
  sessions.

Kept choices: the Forecast page is the latest kept plan of every account and host (no range or
chip), up to 20 minutes old; the Activity waits follow the host but no account chip, because a
`Waited` event names no account; the account chips are the stored accounts of the snapshot, while
an empty query still lets core count the history of accounts no longer stored; page detail is
always fetched by its own command, never computed in the page; and the browser opens on
`fixture-stats.js` with no tray app, which is for pictures only and is not the app's data.

## Step C4: the status line builder

The renderer of step B is split into "segments → coloured spans" and "spans → ANSI text", so the
builder's preview and Claude Code's line come from one implementation. One core module owns the one
key seatswap writes in Claude Code's settings; one page edits `settings.statusline`.

| Module | Job |
|---|---|
| `core::claude_settings` | the `statusLine` key of Claude Code's `settings.json`: read, set, remove |
| `core::atomic` (extended) | `write_atomic` writes through a symbolic link and keeps the link |
| `app::statusline::render` (extended) | the three new segments, their field path table, spans, the one fitting |
| `app::statusline` (extended) | the preview's lines and scenarios, the recorder of field names |
| `app::gui::window` (extended) | the nine `statusline_*` commands, the install record's two decisions |
| `app::gui::autostart` (extended) | `running_program` and `program_beside` (`EXE_SUFFIX`): the console and tray programs beside the running one |
| `app::ui/areas/statusline.js` | the Status line area: palette, zones, options, preview, install card, recorder |

## store.rs additions (step C4)

```rust
#[serde(rename_all = "camelCase")]
pub enum SegmentKind { …, Separator, ContextUsed, SessionCost, LinesChanged }
impl StatuslineConfig {
    pub fn presets() -> Vec<(String, StatuslineConfig)>; // "My current line", "Limits only", "Everything"
    pub fn limits_only() -> StatuslineConfig;           // no left zone, the current line's right zone
    pub fn everything() -> StatuslineConfig;            // every kind once; the drop numbers 1, 2, 3 on
                                                        // the two resets, the 5h percent and the branch
}
pub struct StatuslineInstall {
    pub installed: serde_json::Value,                  // the exact line seatswap wrote last
    #[serde(default, deserialize_with = "deserialize_present_value",
            skip_serializing_if = "Option::is_none")]
    pub previous: Option<serde_json::Value>,           // absent = no key; Some(null) = the value null
}
// Settings gains:
pub statusline_record_until: Option<DateTime<Utc>>,    // serde default None
pub statusline_install: Option<StatuslineInstall>,     // serde default None
```

`deserialize_present_value` reads a present field as `Some`, also when it is `null`; a missing field
takes the default `None`. That is what keeps "the file had no `statusLine` key" and "the file had
`statusLine: null`" apart through `accounts.json`, and the install proof test round-trips a `null`
through serde to check it. An older `accounts.json` loads with both settings `None`: no recording
runs and there is nothing to restore.

## spans, `ansi` and the one fitting (step C4)

```rust
// statusline/render.rs
pub(crate) enum NamedColor { Green, Yellow, Red, Magenta, Cyan, BrightYellow, BrightRed }
const NAMED_COLORS: [(&str, &str, &str, &str); 7]; // name, SGR foreground, SGR background, #rrggbb
pub(crate) enum Color { Named(NamedColor), Rgb([u8; 3]) }   // Serialize writes #rrggbb
#[derive(Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct Span { pub text: String, pub fg: Option<Color>, pub bg: Option<Color>, pub bold: bool }
pub(crate) struct FittedLine { pub spans: Vec<Span>, pub dropped: Vec<u8>,
                               pub dropped_kinds: Vec<SegmentKind> }
pub(crate) fn render_spans<Tz: TimeZone>(config: &StatuslineConfig, payload: &Payload, facts: &Facts,
    width: Option<u16>, tz: &Tz) -> (Vec<Span>, Vec<u8>) where Tz::Offset: Display;
pub(crate) fn fitted_line<Tz: TimeZone>(config: &StatuslineConfig, payload: &Payload, facts: &Facts,
    width: Option<u16>, tz: &Tz) -> FittedLine where Tz::Offset: Display;
pub(crate) fn ansi(spans: &[Span]) -> String;
fn zone_gap(left_width: usize, right_width: usize) -> usize;
pub(crate) fn at_path<'a>(raw: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value>;
pub(crate) fn visible_width(text: &str) -> usize;   // ANSI stripped, each code point by char_width
```

`fitted_line` is the only fitting rule; `render_spans` is `fitted_line(..)`'s spans and drop numbers,
so the command and the preview cannot drift. `Span.fg`/`.bg` are the `Color` enum, whose `Serialize`
writes the `#rrggbb` the page paints (a named colour uses the xterm default of `NAMED_COLORS`), so
the preview receives a colour string or `null`; `ansi` keeps writing the same SGR bytes as
step B: `\x1b[1m` for bold, then the background, then the foreground, the text and `\x1b[0m`; a span
with no colour and no bold is written as plain text. An empty list of spans is the empty line.

The fitting itself is step B's rule: the drop numbers of the pieces that have a value, sorted and
deduplicated, are raised one at a time until `fits` holds — left width + `zone_gap` + right width +
margin ≤ width — and every number at or below the last level is dropped. Without a width nothing is
dropped and the right zone follows the left after the same gap. `zone_gap` is 2 when both zones
print something and 0 when one of them is empty, in `fits` and in the drawn span alike, so an empty
zone leaves the other flush with `width - margin` (or with the line's start without a width).
Otherwise the gap is `width - margin - left - right`, at least 2. `visible_width` strips ANSI CSI
sequences and counts a code point as 2 columns in the East Asian wide/fullwidth and emoji ranges, 0
for a combining mark or a zero-width format character, else 1 — an approximation kept without a
dependency, so a code point outside those tables counts as one column; an East Asian ambiguous
character counts one column even where a terminal draws it two.

A `Separator` segment prints its own `separator.unwrap_or(Pipe).text()`; it has no payload value and
never goes missing. Its prefix and suffix are printed like any other segment's. The one `edge_fg`
rule gives the prefix and the suffix the value's foreground when the colour rule paints no
background (so a unit or a bracket stays the value's colour and `ansi` writes it as one run with the
value); when the rule paints a background both are plain, so the colour stops at the value.

## the field path table and the three new segments (step C4)

```rust
const FIELD_PATHS: [(SegmentKind, &[&str]); 3] = [
    (SegmentKind::ContextUsed,  &["context_window.used_percentage"]),
    (SegmentKind::SessionCost,  &["cost.total_cost_usd"]),
    (SegmentKind::LinesChanged, &["cost.total_lines_added", "cost.total_lines_removed"]),
];
#[derive(Clone, Copy, Debug, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct FieldPaths { pub kind: SegmentKind, pub paths: &'static [&'static str] }
pub(crate) fn field_paths(kind: SegmentKind) -> &'static [&'static str]; // [] for every other kind
pub(crate) fn all_field_paths() -> Vec<FieldPaths>;                      // feature "gui" / tests
```

The table is the one place a payload path is written; the renderer reads it and so does the
window's `statusline_fields`, which the page uses to mark a segment whose field a recording did not
see. There are no separate path constants: a fourth reader would be a fourth place to change.

| Kind | Value | Missing |
|---|---|---|
| `ContextUsed` | `format!("{}%", whole(n))` | the path is absent, `null` or not a number |
| `SessionCost` | `format!("${n:.2}")` | the path is absent, `null` or not a number |
| `LinesChanged` | `format!("+{} -{}", whole(added), whole(removed))` | either path is absent, `null` or not a number |

`number_at` is `at_path(raw, path).and_then(Value::as_f64)`, so a string, a boolean, an object and
`null` all print nothing and the segment's prefix and suffix are not printed either. `whole` prints
a whole number exactly (`120`) and otherwise the `integer_percent` rounding. The numbers come from
the payload's raw JSON (`Payload.raw`), which is the whole payload Claude Code sent (`Null` when
standard input was not JSON): the typed `Payload` fields did not grow a `context_window` or a `cost`
struct, because the path table and `at_path` already read any shape. Before a session's first answer
`used_percentage` is `null` (nothing printed) and `total_cost_usd` is a whole `0` (`$0.00`). A `Text`
segment whose `text` is absent or the empty string is no value either: it prints nothing, prefix and
suffix included.

## the preview (step C4)

```rust
// statusline/mod.rs
#[derive(Debug, Clone, Copy, Deserialize)] #[serde(rename_all = "camelCase")]
pub(crate) enum Scenario { Live, NearFull, Blocked, NoSessionLimit }
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct PreviewLine { pub width: u16, pub spans: Vec<Span>,
                                pub dropped: Vec<u8>, pub note: String }
pub(crate) fn preview_lines<Tz: TimeZone>(config: &StatuslineConfig, payload: &Payload, facts: &Facts,
    widths: &[u16], scenario: Scenario, tz: &Tz) -> Result<Vec<PreviewLine>, String>;
pub(crate) fn preview_payload(home: &std::path::Path) -> Payload;
const PREVIEW_BRANCH: &str = "main";
```

`preview_lines` renders the given config — never the stored one — once per width, in the width
order it was given. An empty list is `the preview needs at least one width`, and a width of 0 is
`a preview width must be at least 1 column; 0 cannot show a line`; the window command does not
repeat those checks. A scenario changes one value of the facts and nothing else: `Live` takes them
as they are, `NearFull` sets the week to 97 %, `Blocked` to 100 % (each keeps the week's reset),
`NoSessionLimit` drops the 5 h values. The preview's session is made up: `preview_lines` sets the
branch to `PREVIEW_BRANCH` (`main`) before the scenario's change, because the payload's folder does
not exist on the machine and `.git/HEAD` has no branch to read. `dropped` is the drop numbers the
fitting removed; `note` is built from the kinds those numbers removed, one label per kind from the
store's one kind table (`kind.spec().label`), in drop order then line order: `everything fits`, or
`{names joined with ", " and " and "} {is|are} dropped`
(`5h reset, week reset and 5h percent are dropped`). A segment without a value is not a piece the
fitting can drop, so the note never names it. The note and the page therefore use the same word for
a kind (`git branch`, not `branch`).

`preview_payload(home)` is README §5's payload with the folder `<home>/projects/atlas` and the keys
Claude Code sends after a session's first answer: `context_window.used_percentage` 41,
`cost.total_cost_usd` 3.2, `total_lines_added` 120 and `total_lines_removed` 8. The window's
`statusline_preview` builds the facts from the snapshot (`SnapshotFacts`: the views' abbr, colour and
plan, the cache's usage, the per-model limits, `burn-next.json` through the snapshot's views, the
live state, `claude.home()` and `$USER`/`%USERNAME%`) and passes that payload, so every scenario has
the same folder, model and effort while the limits and the account are the live account's (the
snapshot's usage is already the engine's merged view, so the payload's own limits are not used). It
is the same `facts_from_source` the command uses, with a different `FactsSource`; the facts builder,
not the page, decides what a segment shows. When there is no live login, or it has ended,
`facts_from_source` returns `no live login; run seatswap add` or `the live login has ended; log in
again` and `statusline_preview` returns that error: the page shows the error instead of a line.

## the recorder of field names (step C4)

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct KeyInfo { pub path: String, pub kind: String } // string|number|boolean|null|object|array
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct RecordedKeys { pub at: DateTime<Utc>, pub keys: Vec<KeyInfo> }
pub(crate) fn key_paths(raw: &serde_json::Value) -> Vec<KeyInfo>;
pub(crate) fn read_recorded_keys(app: &AppPaths) -> Result<Option<RecordedKeys>, String>;
pub(crate) fn write_recorded_keys(app: &AppPaths, recorded: &RecordedKeys) -> Result<(), String>;
pub(crate) fn record_keys(app: &AppPaths, settings: &Settings, payload: &Payload,
                          now: DateTime<Utc>) -> Result<(), String>;
pub(crate) fn record_until(minutes: u32, now: DateTime<Utc>) -> Result<DateTime<Utc>, String>;
```

`key_paths` walks the raw payload and keeps the path and the JSON kind only — never a value. An
array adds `path[]` for its elements (one entry per kind when they differ) and an array of objects
continues below `path[].key`; the result is sorted by path then kind and deduplicated. A key that
holds a dot is written as it is, so it cannot be told apart from a nested key of the same name; a
key with an empty name is written as the empty path, and its children lose that level (`c`, not
`.c`). A payload that is not a JSON object has no paths.

`record_keys` runs from `seatswap statusline` last: after `publish_feed` and after the line is
rendered, and its result is discarded. A broken keys file therefore does not change the printed
line and does not stop the feed; the window's `statusline_keys` command reads the same file and
shows its error on the page. Outside a recording (`statuslineRecordUntil` absent or not in the
future) or for a payload that is not an object it neither reads nor writes the file; inside one it
merges the file's paths with this payload's, writes the union through `atomic::write_atomic` with
mode 0600 (making the data folder private first) and stamps it with `now`. A keys file that does not
parse is an error, never replaced. `record_until` accepts 1 to 60 minutes
and refuses 0 and more with `a recording lasts 1 to 60 minutes, got N`; the page asks for 1.

`AppPaths::statusline_keys_file()` is `<data_dir>/statusline-keys.json`. The page's *Record for a
minute* calls `statusline_record(1)`, which saves `statuslineRecordUntil` through the settings
page's one writer (`save_setting`) and nothing else; the page then reads `statusline_keys` every
5 s while the minute runs and once more after it. `statusline_keys` returns the file's content or
`null` when the file is missing, and an error naming the path when it cannot be read or parsed.

## claude_settings — the one key in Claude Code's settings (step C4)

```rust
pub enum ClaudeSettingsError {
    Io { path: PathBuf, source: std::io::Error },
    NotAnObject { path: PathBuf },              // an empty file, a list, a string, a number
    Json { path: PathBuf, source: serde_json::Error },
    NotUnicode { path: PathBuf },               // the exe path, when the line is built
}
pub fn status_line(claude: &ClaudePaths) -> Result<Option<Value>, ClaudeSettingsError>;
pub fn set_status_line(claude: &ClaudePaths, value: Option<&Value>)
    -> Result<Option<Value>, ClaudeSettingsError>;   // the previous value
pub fn seatswap_status_line(exe: &Path) -> Result<Value, ClaudeSettingsError>;
fn read_object(path: &Path) -> Result<Option<Map<String, Value>>, ClaudeSettingsError>;  // missing = None
fn write_object(path: &Path, object: &Map<String, Value>) -> Result<(), ClaudeSettingsError>;
```

`ClaudePaths::settings_file()` is `<config_dir>/settings.json`. `status_line` reads the object and
clones `statusLine`; a missing file or a missing key is `None`. `set_status_line` reads the file
once, inserts or removes the one key (`serde_json`'s `preserve_order` keeps every other key and its
position), calls `atomic::backup_once` before the first write of an existing file, and writes the
object through `write_atomic` with `Access::Shared`. Removing a key that is not there writes
nothing. A missing file is created holding only `statusLine` (there is nothing to back up). An empty
file, broken JSON and a JSON value that is not an object are errors with the path and the file is
left untouched. `write_object` is `serde_json::to_vec_pretty`, so two spaces per level and no
trailing newline.

`seatswap_status_line` builds `{"type":"command","command":"\"<exe>\" statusline",
"refreshInterval":2}`. The path is **always** inside double quotes, not only when it has a space, so
bash and Git Bash on Windows read it as one word; inside the quotes `"`, `$` and `` ` `` get a
leading backslash, a backslash is doubled when it ends the path, precedes one of those three or
another backslash, or is part of a run (so a UNC path's leading `\\` becomes `\\\\`). A path that is
not valid Unicode is `NotUnicode`, never a lossy replacement. The tests cover `/opt/my apps/seatswap`,
a plain path, an inner quote, `$`, a backtick, `C:\Program Files\seatswap\seatswap.exe` and
`\\vega\tools\seatswap.exe`.

## write_atomic through a symbolic link (step C4)

```rust
fn write_target(path: &Path) -> io::Result<PathBuf>;   // private to atomic.rs
```

`write_atomic` first resolves the file it must replace: `path` itself when it is not a symbolic link
(also when it does not exist yet), else the file the chain of links finally points to (a relative
link target is resolved against the link's own folder). The temp file is made beside that file, the
file keeps its mode (or gets `access_if_new` when it is new), and the rename replaces the target, so
every link in the chain stays a link. A link whose target (or later link) does not exist is
`NotFound` with a text naming the link, and no file is created beside it; more than 40 links in a row
is `InvalidInput`. The tests are `a_symlink_to_a_file_is_written_through_and_stays_a_link`,
`a_chain_of_two_symlinks_is_written_through` and `a_dangling_symlink_is_refused_and_creates_nothing`
on Unix; `claude_settings` adds the same proof for Claude Code's `settings.json`. The resolving code
is not `cfg`-gated, so Windows takes the same path; only the link tests are Unix.

## the install record and the two decisions (step C4)

```rust
// gui/window.rs
#[derive(Debug, Clone, PartialEq, Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct InstallState {
    pub current: Option<serde_json::Value>,   // Claude Code's statusLine now (null = no key)
    pub ours: serde_json::Value,              // the line seatswap would write
    pub installed: bool,                      // current == ours
    pub restorable: bool,                     // the restore rule allows it
    pub previous: Option<serde_json::Value>,  // the record's previous value
    pub settings_file: String,                // <config>/settings.json, as the page shows it
}
struct InstallDecision { record: Option<StatuslineInstall>, write: Option<Value>, state: InstallState }
struct RestoreDecision { restore: Option<Value>, before: Option<Value>, state: InstallState }
struct StatusLineReading { ours: Value, current: Option<Value>, settings_file: String }
impl StatusLineReading { fn with_current(&self, current: Option<Value>) -> Self; }
fn read_status_lines(claude: &ClaudePaths, exe: &Path) -> Result<StatusLineReading, String>;
fn install_state_of(reading: &StatusLineReading, record: Option<&StatuslineInstall>) -> InstallState;
fn restorable(record: Option<&StatuslineInstall>, current: Option<&Value>, ours: &Value) -> bool;
fn install_decision(claude: &ClaudePaths, exe: &Path, record: Option<StatuslineInstall>)
    -> Result<InstallDecision, String>;
fn restore_decision(claude: &ClaudePaths, exe: &Path, record: Option<StatuslineInstall>)
    -> Result<RestoreDecision, String>;
async fn install_status_line<F, Fut>(claude: &ClaudePaths, exe: &Path,
    record: Option<StatuslineInstall>, save: F) -> Result<InstallState, String>
    where F: FnOnce(StatuslineInstall) -> Fut, Fut: Future<Output = Result<(), String>>;
async fn restore_status_line<F, Fut>(claude: &ClaudePaths, exe: &Path,
    record: Option<StatuslineInstall>, save: F) -> Result<InstallState, String>
    where F: FnOnce() -> Fut, Fut: Future<Output = Result<(), String>>;
fn console_program() -> Result<PathBuf, String>;   // running_program, then console_program
fn saved_install(state: &GuiState) -> Result<Option<StatuslineInstall>, String>;
```

Both decisions are taken from one `read_status_lines` reading of Claude Code's file and the saved
record, and `install_state_of` is the one place an `InstallState` is built; after a change the
decisions build the state from the same reading through `StatusLineReading::with_current`. The
decision functions take plain paths and the record, so the tests run them without Tauri. The
commands' bodies are `install_status_line` and `restore_status_line`, which take the save as a
parameter: `statusline_install` and `statusline_restore` pass a closure around `save_setting`, and
the tests pass a fake, so the order the commands really use is the one under test.

- **Install.** (a) The current line is seatswap's: write nothing, keep the record, `installed` and
  `restorable` true. (b) A record exists and the current line is the record's `installed` line (an
  older seatswap path): keep the record's `previous` and write today's line. (c) Otherwise — the
  user's own line, or no record: the current value (a present `null` kept as `Some(null)`, an absent
  key as `None`) becomes the new record's `previous`. `install_status_line` saves the record through
  `save` (the command's `save_setting("statuslineInstall", …)`) **before** it writes the line, so a
  refused save leaves Claude Code's file untouched; a line it could not write leaves a record whose
  `installed` value is not in the file: an install started again then falls into (c) and keeps the
  user's line as `previous`, and a restore refuses because the file does not hold seatswap's line.
- **Restore.** Without a record: `seatswap has no record of the line before its own: nothing to
  restore`. With a current line that is neither seatswap's nor the record's `installed` line:
  `the status line in Claude Code is not seatswap's: nothing to restore`; both leave the file
  untouched. Otherwise it writes the record's `previous` exactly (`None` removes the key), then
  clears the record with `save` (the command's `save_setting("statuslineInstall", null)`); if clearing
  fails, the line that was there before the restore is written back and the error is returned, with
  the rollback's own error appended when that also fails.
- `console_program` reads the running program through `autostart::running_program()` and then calls
  `autostart::console_program(&current)`: `seatswap.exe` (`seatswap` on Linux) beside the running
  program, an error naming the path when it is not a file. `running_program()` is the one read of
  `std::env::current_exe()`, with `cannot find the running seatswap program: …`; `apply_autostart`
  uses it too. `program_beside(current_exe, console)` builds the sibling's name as `seatswap` or
  `seatswap-tray` plus `std::env::consts::EXE_SUFFIX`, so the Windows and Linux difference needs no
  `cfg`. On Windows the tray program makes the change too, so the line never names
  `seatswap-tray.exe`.

## the status line commands and their JSON shapes (step C4)

All nine are `#[cfg(feature = "gui")]` and are registered in `gui/mod.rs`'s handler list.

| Command | Arguments | Reply |
|---|---|---|
| `statusline_preview` | `{config, widths: [u16], scenario}` | `[{width, spans: [{text, fg, bg, bold}], dropped: [u8], note}]` |
| `statusline_presets` | — | `[["My current line", config], ["Limits only", config], ["Everything", config]]` |
| `statusline_kinds` | — | `[{kind, label, group, colors: [rule], time, text, repeatable}]`, the one kind table in `SegmentKind::ALL`'s order |
| `statusline_record` | `{minutes: u32}` | `null`; saves `statuslineRecordUntil` |
| `statusline_keys` | — | `{at, keys: [{path, kind}]}`, or `null` when no recording wrote the file |
| `statusline_fields` | — | `[{kind, paths: [path]}]`, the field path table in its order |
| `statusline_install_state` | — | `InstallState` |
| `statusline_install` | — | `InstallState` after the install |
| `statusline_restore` | — | `InstallState` after the restore |

`scenario` is `"live"`, `"nearFull"`, `"blocked"` or `"noSessionLimit"` (serde camelCase). `fg` and
`bg` are `#rrggbb` or `null`; `bold` is always present. A width of 0 or an empty list is an error
with its text; `statusline_preview` also refuses when `accounts.json` cannot be read ("accounts.json
cannot be read; the preview has no data"). A command's error is the `String` Tauri shows the page,
and the page puts it in a banner.

## the Status line area page (step C4)

```text
crates/app/ui/areas/statusline.js   the area: palette, zones, options, preview, install, recorder
crates/app/ui/window.js             the area list: the title "Status line" for id "statusline"
crates/app/ui/common.js             panel, banner, selectInput, switchControl, clone, isObject, …
crates/app/ui/window.css            the .sl-* rules
crates/app/ui/fixture.js            the browser fixture: the commands' replies and a config
```

`statusline.js` exports `render(container, context)`, `update(snapshot)` and `unmount()`. `render`
reads `settings.statusline` and `settings.statuslineFeed` (naming every value it cannot read, never
replacing one), then loads the kinds table and the presets, the install state, the recorded keys and
the field paths once, and draws: the head (preset, Restore previous, Use in Claude Code), the
palette, the line, the options, the preview and the fields card. `update(snapshot)` takes the
snapshot's settings, clears the preview's cached key so the next preview request is not skipped, and
reads the install state again before the head and the card are drawn; it redraws, except while a save
is pending (the page's copy wins) or while focus is inside the page (only the settings are taken and
the preview is re-requested, so a redraw does not take a field away mid-typing). Every change is
saved through
`save_setting` at once, queued one after another each built from the settings the one before left;
a refused save restores the stored layout and shows its text, and changes queued behind it are
counted as dropped rather than saved stale.

The kinds and the options each kind offers are one table in Rust: `SegmentKind::spec()` returns a
`KindSpec` (label, group, the colour rules it takes, and whether it takes a time format, text or
repeats), in `SegmentKind::ALL`'s palette order. `statusline_kinds` sends it to the page, and
`store.rs`'s `validate_statusline` uses the same table to refuse a colour rule, a time format or text
that the kind does not take (`statusline.left[2].color: folder does not take byLoad`,
`….timeFormat: … does not take a time format`, `….text: … does not take text`). The page keeps no
copy; it reads the reply into `state.kinds`:

| Kind (`id`) | Group | Label | Colour rules | Time format | Text | Repeatable |
|---|---|---|---|---|---|---|
| `account` | seatswap | account | none · the account's colour · fixed | | | |
| `plan` | seatswap | plan | none · fixed | | | |
| `fiveHourPercent` | seatswap | 5h percent | none · by load · fixed | | | |
| `fiveHourReset` | seatswap | 5h reset | none · fixed | ✓ | | |
| `weekPercent` | seatswap | week percent | none · by load · fixed | | | |
| `weekReset` | seatswap | week reset | none · fixed | ✓ | | |
| `modelPercent` | seatswap | per-model % | none · by load · fixed | | | |
| `autoBurn` | seatswap | auto-burn: next | none · fixed | | | |
| `model` | Claude Code | model | none · by model family · fixed | | | |
| `effort` | Claude Code | effort | none · by effort · fixed | | | |
| `folder` | Claude Code | folder | none · fixed | | | |
| `gitBranch` | Claude Code | git branch | none · fixed | | | |
| `contextUsed` | Claude Code | context used | none · fixed | | | |
| `sessionCost` | Claude Code | session cost | none · fixed | | | |
| `linesChanged` | Claude Code | lines changed | none · fixed | | | |
| `userHost` | System | user@host | none · fixed | | | |
| `clock` | System | clock | none · fixed | | | |
| `text` | System | free text | none · fixed | | ✓ | ✓ |
| `separator` | System | separator | none · fixed | | | ✓ |

A `separator` does not take text: it takes its own character, one of `│ · • / ›` or blank, and the
page offers those six under the segment. The palette filters `repeatable`; a kind already in a zone is
not offered again. "Drop when narrow"
offers never · 1 · 2 · 3 only. A colour rule whose values are not typed yet is held in
`state.pendingColorRule` and is neither in `state.config` nor saved, so the page never writes a
threshold or a hex it made up; an unknown kind, a colour rule its kind does not offer, a time format
or a drop number outside the table are shown as stored with "not offered" and left alone.

The preview asks `statusline_preview` with the whole config, the three parsed widths and the chosen
scenario, 150 ms after the last change, and drops a reply that a newer request has overtaken. The
width fields are read as typed: the page checks that each is a whole number of columns, and the
command refuses 0 (`a preview width must be at least 1 column; 0 cannot show a line`), an empty list
and a value `u16` cannot hold; the error is shown, not replaced by a number. The terminal tab
recolours the boxes only; the spans' colours come
from the reply. A segment whose kind is in the field path table and whose path is not in a recorded
list carries `Claude Code did not send this field (<time of the recording>)` as a warning banner in
its options and in its chip's tooltip; the note exists only when a recorded list exists.

The head's *Use in Claude Code* reads the install state again when the card opens (the file may have
changed by hand or in another window) and then opens the card (the file, `now` and `after` as JSON,
the sentence `seatswap changes only this key. The value it replaces is kept and can be put back.`,
Install and Cancel); it never writes before Install is pressed. The button reads *In use in Claude
Code* and is disabled while `installed`. *Restore previous* is drawn only when `restorable`. Both
show the command's error as a banner and their success as a notice. `fixture.js` has a stand-in for
every `statusline_*` reply with the same shape (including the kinds table) and its own small preview
drawing, so the browser page works with no tray app; that drawing is for pictures only and is not
the app's renderer.

---

## Step D: OpenAI (Codex) and Ollama cards

Two more companies. One reader per company turns the answer into the `Usage` and `Limit` types an
account already uses, so a card, its colours and the history need no second implementation. A
provider thread in the tray app polls each switched-on card and every provider found by itself (R6).
It reads Codex's `auth.json` at every poll, saves the live login in the secrets store and
writes `auth.json` only when switching between saved logins; a saved inactive login may renew itself.
No request generates text.

| Module | Job |
|---|---|
| `core::paths` (extended) | `CodexPaths`: the Codex folder and its `auth.json` |
| `core::store` (extended) | the four settings and `validate_ollama_url` |
| `core::providers` | one reader per company into `ProviderReading`; Codex's saved logins, renewal and switch |
| `core::api` (extended) | Codex's usage and token renewal, Ollama's balance and usage; `NoConnection` |
| `core::history` (extended) | `is_claude`, `read_claude`, `finished_claude_windows`: provider records out of every Claude reader |
| `app::gui::providers_rt` | the provider thread, its client, the snapshot's `providers` and `codexLogins` |
| `app::ops` | `ensure_provider_subscription`, `switch_codex` |
| `app::ui/panel.js` | the pop-up's cards and the saved-login switch |
| `app::ui/areas/settings.js` | the Providers group and the Accounts subscription rows |

## paths.rs additions — `CodexPaths` (step D)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPaths {
    pub home: PathBuf,          // the Codex folder; seatswap writes only auth.json, only on a switch
    pub auth_file: PathBuf,     // <home>/auth.json
    pub auth_lock: PathBuf,     // <home>/auth.lock: Codex's first write lock
    pub auth_json_lock: PathBuf, // <home>/auth.json.lock: taken after auth_lock
}
impl CodexPaths {
    pub fn resolve(explicit: Option<&Path>) -> Result<CodexPaths, PathsError>;
    pub fn resolve_with(explicit: Option<&Path>, env_codex_home: Option<OsString>,
                        home: &Path) -> Result<CodexPaths, PathsError>; // tests
    pub fn at(home: &Path) -> CodexPaths;
}
```

`resolve` takes the `codexHome` setting first, then a non-empty `$CODEX_HOME`, then `~/.codex`.
`resolve_with` does the same without reading the process environment, so a test passes its own
folder. A relative explicit folder or `$CODEX_HOME` is `PathsError::RelativeCodexHome` and is never
resolved against the current folder: `the Codex folder (codexHome or $CODEX_HOME) must be an absolute
path, got {path}`. `at` is the test and caller shorthand for a given folder; `auth_file` is always
`<home>/auth.json`.

## paths.rs additions — Dropbox (step G-3)

```rust
/// The first existing info.json's folder: `personal.path`, else `business.path`, else None. A missing
/// file is Ok(None), and an absent or `null` section has no path. A section of another kind, a file
/// that is not JSON or not a JSON object, a path that is not a string and an empty path are
/// `PathsError::DropboxInfo` naming the file; it never creates anything and never guesses a folder.
pub fn dropbox_folder() -> Result<Option<PathBuf>, PathsError>;
/// The same rule for a given info.json, so a test passes its own file and never reads the user's.
pub fn dropbox_folder_in(info_json: &Path) -> Result<Option<PathBuf>, PathsError>;
```

On Windows the two candidate files are `%APPDATA%\Dropbox\info.json` then
`%LOCALAPPDATA%\Dropbox\info.json`; on macOS and Linux `~/.dropbox/info.json`. The settings' history
page offers `<folder>/seatswap` as `HistoryView.suggested_folder` only while the saved settings name
no history folder, shows a broken `info.json`'s `suggested_error` under the host list, and writes
nothing itself.

## store.rs additions — the four settings (step D)

```rust
pub codex_card: bool,                    // serde default: default_codex_card() = false
pub codex_home: Option<PathBuf>,        // serde default: None
pub ollama_card: bool,                  // serde default: default_ollama_card() = false
pub ollama_url: String,                 // serde default: "http://127.0.0.1:11434"
pub fn validate_ollama_url(url: &str) -> Result<(), StoreError>;
```

An older `accounts.json` without the four fields loads with both cards off, no Codex folder and the
local Ollama URL: each field has a `#[serde(default = "…")]` or `#[serde(default)]`, so nothing has to
be rewritten. `Settings::validate` checks `codexHome` with the other paths (it must be absolute when
`Some`) and always calls `validate_ollama_url`. The URL rule is: no whitespace or control character
anywhere; it must start with `http://` or `https://`; the text after the scheme up to the first `/`,
`?` or `#` is the authority and must start with a real host — a DNS name (dot-separated labels of
letters, digits and inner hyphens, each 1 to 63 bytes, 253 bytes in all), an IPv4 address (accepted by
that same name rule) or a bracketed IPv6 address followed by nothing or `:port`; no `?`, no `#`, and
no `//` after the authority. User info (an `@` in the authority) is refused before the host check. A
port, a path, an IPv6 host and one trailing `/` are accepted. Every refusal is
`StoreError::InvalidSetting`; all but the user-info refusal name `ollamaUrl` and quote the value — the
user-info text is fixed (`ollamaUrl must not contain a user name or password`) because the URL may
hold a password. There is no fallback. `providers::ollama::read` calls the same function again before
it asks the server.

## providers — the one reading type and its errors (step D, 1.3)

```rust
pub const CODEX_ORG: &str = "codex";
pub const OLLAMA_ORG: &str = "ollama";

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("No Codex login found in {path}. If Codex keeps its login in the system keyring, seatswap cannot read it.")]
    NoLogin { path: PathBuf },
    #[error("Codex keeps its login in the system keyring. To switch Codex logins, set cli_auth_credentials_store = \"file\" in Codex's config.toml and sign in again.")]
    KeyringLogin,
    #[error("Codex is signed in with an API key. An API key is no subscription; seatswap neither saves nor switches it.")]
    ApiKeyLogin,
    #[error("No saved Codex login for {acc}. Sign in with codex login once while seatswap runs.")]
    NotSaved { acc: String },
    #[error("{0}")] RenewFailed(String),       // "Sign in again with codex login."
    #[error(transparent)] Lock(#[from] LockError),
    #[error("the Codex login store: {0}")] Store(#[from] crate::store::StoreError),
    #[error("Codex's login has expired. Run codex once to renew it.")]
    LoginExpired,
    #[error("Ollama is not running on {url}.")]
    NotRunning { url: String },
    #[error("Ollama on {url} refused the request (HTTP {status}). Is it signed in?")]
    OllamaRefused { url: String, status: u16 },
    #[error("{0}")] Setting(String),
    #[error(transparent)] Api(ApiError),
    #[error("Unexpected answer: {0}")] Shape(String),
    #[error("cannot read {path}: {source}")] Io { path: PathBuf, #[source] source: io::Error },
}

pub struct Credits { pub left: f64, pub allowance: Option<f64>,
    pub until: Option<DateTime<Utc>>, pub purchased: Option<f64> }
pub struct ProviderReading { pub account: AccountRef, pub plan: Option<String>,
    pub usage: Option<Usage>, pub credits: Option<Credits>,
    pub spend_24h: Option<Result<f64, String>>,
    pub hours: Option<Result<Vec<OllamaHourRecord>, String>> }
```

`api::mark_highest_active` (in the api.rs section below) sets `is_active` on exactly one limit: the
one with the highest percent, the first of equals. `ProviderError::Setting` carries the text of the
setting or path that refused the reading; it is not a second setting error. The `Api` variant is
transparent, so the card prints the API error's own text. `NotRunning { url }` is a refused
connection to the Ollama server (`Ollama is not running on {url}.`); the poll uses it to report the
change once, no reading is recorded, and the card writes its own quiet line `Ollama is not
running.` The keyring and API-key sentences are
exact: the card shows them as its error, and neither login is saved or switched.

## codex — `auth.json`, its saved logins and the usage answer (step D, 1.3 R8)

```rust
pub const CODEX_LOCK_TIMEOUT_SECS: u64 = 10;      // Codex's two locks together
const CODEX_ISSUER: &str = "https://auth.openai.com"; // the only issuer a saved login is renewed at
const SIGN_IN_AGAIN: &str = "Sign in again with codex login.";

pub fn secret_id(account: &AccountRef) -> String; // first 32 hex of SHA-256 of "codex:<acc>"
pub struct LiveCodex { pub account: AccountRef, pub saved: bool }
pub fn sync_live(app: &AppPaths, paths: &CodexPaths, secrets: &dyn SecretStore)
    -> Result<LiveCodex, ProviderError>;
pub fn read(paths: &CodexPaths, api: &Api) -> Result<ProviderReading, ProviderError>;
pub fn live_account(paths: &CodexPaths) -> Result<Option<AccountRef>, ProviderError>;
pub fn read_saved(app: &AppPaths, paths: &CodexPaths, secrets: &dyn SecretStore,
                  account: &AccountRef, api: &Api) -> Result<ProviderReading, ProviderError>;
pub const LIVE_LOGIN_REMOVE: &str; // the one refusal sentence for removing the live login
pub fn forget(app: &AppPaths, secrets: &dyn SecretStore, account: &AccountRef)
    -> Result<(), ProviderError>;
pub struct CodexSwitched { pub from: Option<AccountRef>, pub to: AccountRef }
pub fn switch_to(app: &AppPaths, paths: &CodexPaths, secrets: &dyn SecretStore,
                 to: &AccountRef) -> Result<CodexSwitched, ProviderError>;
pub fn usage_from(answer: &Value) -> Result<(Usage, Option<String>, AccountRef), ProviderError>;
fn window_kind(seconds: i64, additional: bool) -> LimitKind;
fn plan_type(value: Option<&Value>) -> Result<Option<String>, ProviderError>;
```

`read` reads `paths.auth_file` once and never writes it. A missing file is `NoLogin`; a file that
is not a JSON object, or whose `tokens` is not an object, is `Shape` with a sentence that never
quotes a token. `tokens` absent or `null` with `auth_mode == "chatgpt"` is `KeyringLogin`, with
`auth_mode == "apikey"` is `ApiKeyLogin`, otherwise `NoLogin`. `tokens.access_token` and
`tokens.account_id` must be non-empty strings (`Shape` with the field's name only). It then calls
`api.codex_usage(access_token, account_id)`; HTTP 401 is `LoginExpired`, every other API error stays
`Api`. The live login is Codex's own; seatswap never renews it.

`sync_live` takes seatswap's app lock, reads the live file and saves its whole object under
`secret_id`, when there is no copy or its `tokens` differ, so a login Codex renewed or the user just
signed in with is saved at the next poll and an unchanged poll writes nothing. A live ChatGPT login
is saved only with non-empty `account_id`, `access_token` and `refresh_token`; otherwise `Shape`
names the first missing field and the copy stays. `live_account` is the one read-only way to know
which saved login is live (`codex list`, and the guard against removing the live copy).

`read_saved` asks the stored copy's usage. A 401 renews it once at `CODEX_TOKEN_URL`, its own
issuer only (`api.codex_refresh`, through the no-redirect agent), then asks again; a failed renewal
or a second 401 is `RenewFailed(SIGN_IN_AGAIN)`, and the copy stays, so `codex login` is the next
step. The renewal (the token POST and storing its answer) runs under the app lock, after the live
file was read again under it: a login that became live during the round is not renewed
(`BecameLive`, an `error` row for that round), because Codex renews the live login itself. Under the
same lock the stored copy is read again: a copy someone renewed meanwhile is asked with as it is
(no second POST with a spent refresh token); the second usage ask runs after the lock is released.
`store_renewed` re-reads the copy too: the renewal replaces
the copy only when the copy still holds the tokens the renewal started from
(`CodexLogin::same_tokens`), and the second ask uses the newer copy. There is no second renewal. A
login whose stored `tokens.oauth_provenance.issuer` is not `CODEX_ISSUER` is not renewed at all (`RenewFailed`, same sentence:
`Sign in again with codex login.`). `forget` deletes the stored copy under the app lock; Codex's
`auth.json` is not touched, so a live login keeps working and a missing copy is not an error.

`switch_to` writes `auth.json` from the stored copy of `to`, and writes nothing when the live login
already is `to` (`from == Some(to)`). Locks are taken in this order: seatswap's app lock
(`APP_LOCK_TIMEOUT`), Codex's `auth.lock`, `auth.json.lock`, the last two within one
`CODEX_LOCK_TIMEOUT_SECS` deadline; a busy lock is `Lock` naming the file. Only after all locks does
it read the target copy, then reads and saves the live login first (so a login seatswap cannot save
stops the switch, never overwrites it), then writes the stored object whole, key order included and
mode 0600, atomic. A missing `auth.json` is no `from`. It writes no history; the caller records
`ProviderSwitch`.

`usage_from` reads `rate_limit.primary_window` and `.secondary_window`: `used_percent` is the
percent, `limit_window_seconds` picks the kind, `reset_at` is the reset when present. An entry of
`additional_rate_limits` has its own `rate_limit` (a `null` one skipped) and a required
`limit_name` that becomes the limit's `model`. `window_kind` is the one mapping: 18000 no → `Session`,
604800 no → `WeeklyAll`, 604800 yes → `WeeklyScoped`, a multiple of 3600 → `Other("{h}h limit")`, a
multiple of 60 → `Other("{m}min limit")`, else `Other("{s}s limit")`. `plan_type` is `None` when
absent, `null` or the empty string, otherwise its first character is uppercased; any other type is a
shape error. Every limit's `severity` is `Normal`; `api::mark_highest_active` runs last. Codex leaves
`credits`, `spend_24h` and `hours` `None`.

## ollama — the balance and the last 24 h (step D)

```rust
pub fn read(base_url: &str, api: &Api) -> Result<ProviderReading, ProviderError>;
pub fn amount(value: &Value) -> Option<f64>; // a JSON number, whole or fractional; else None
```

`read` first runs `store::validate_ollama_url(base_url)`; a URL the settings refuse is
`ProviderError::Setting` and no request is made. It then calls `api.ollama_balance(base_url)`; a
refused connection (`ApiError::NoConnection`) becomes `NotRunning { url }`, a balance answer of 401
becomes `OllamaRefused { url, status: 401 }`, a 403 becomes `OllamaRefused { url, status: 403 }`, and
every other API error stays `Api`. The balance answer is `balance_reading`:

- `included.balance_usd` present selects the **new plan**: `balance_usd` (required, not negative),
  `allowance_usd` (optional, not negative, and `left > allowance` is refused),
  `period.until` (optional RFC 3339), and `purchased.balance_usd` (optional). It returns
  `Some(Credits)`. When an allowance is present and not zero it also returns one
  `Usage { limits: [Other("monthly_credits")] }` with `percent = (allowance − left) / allowance × 100`,
  `resets_at = period.until` and `is_active = true`; without an allowance, or with a zero one, the
  usage is `None` and the credits are still returned.
- `included.session` or `included.weekly` without `balance_usd` selects the **old plan**: each
  non-null window becomes `Session` or `WeeklyAll` with `percent = 100 − remaining_percent` (the
  remaining must be within 0..=100) and `resets_at` from `resets_at`; both null is a shape error and
  no credits are returned. `api::mark_highest_active` runs over the two.
- An `included` with `balance_usd` and also `session`/`weekly` is refused as both plan shapes at
  once; an `included` with neither is refused. A non-object answer, a missing `included`, a number
  given as a string and a bad time are `Shape` errors naming the field path.

The usage call is made after a successful balance. `spend_24h` is always `Some`: `Ok(totals.usage_usd)`
through `amount` when the answer carries that number, else `Err({the API error's text})` for an HTTP,
network or decode failure, or `Err("the usage answer has no number at totals.usage_usd")` when the
field is absent or not a number. A failed usage call never fails the reading; the balance is kept.

The same answer's `buckets` become `hours`: one `OllamaHourRecord` per bucket, in the answer's
order. A bucket that is not an object, starts at something other than a whole UTC hour, does not run
exactly one hour (the running hour is the exception: marked `"partial": true`, it ends at the
answer's moment, after its start and at most one hour later; a `partial` that is not a boolean is an
error), or lacks a whole non-negative `request_count`, `input_tokens`,
`cached_input_tokens` or `output_tokens` (and a finite non-negative `usage_usd`) is an error whose
text names the bucket, never a skipped hour and never a 0. An answer without `buckets` is `Err("the
usage answer has no buckets")`, and one whose `buckets` is not an array says so. A usage call that
failed has no buckets to read: `hours` is `None`, the card's spend error already shows it, and no
second warning repeats it.

The provider thread writes a reading's sample and then its hour lines (`ProviderReading::hours`) to
`ollama-YYYY-MM.jsonl`. The hour lines only grow while an hour runs: a line is written when every
count of the answer is at least the hour's stored line and at least one is larger; an answer with a
smaller count is an older answer, dropped whole and named once per hour through the recorder's
warnings, which the thread prints once per text. An answer without `buckets` is a warning once per
text too. The account is `AccountRef { acc: "ollama", org: "ollama" }`, the plan `None`.

## api.rs additions (step D)

```rust
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
impl Api {
    pub fn with_codex_url(mut self, url: &str) -> Api;                                  // tests
    pub fn with_codex_token_url(mut self, url: &str) -> Api;                           // tests
    pub fn codex_usage(&self, access_token: &str, account_id: &str)
        -> Result<serde_json::Value, ApiError>;
    /// POST CODEX_TOKEN_URL with `grant_type=refresh_token`, the client id and the copy's
    /// refresh token; the no-redirect agent keeps it at Codex's issuer. A 401 or 400 is the
    /// caller's to map; no token enters an error text.
    pub fn codex_refresh(&self, client_id: &str, refresh_token: &str)
        -> Result<serde_json::Value, ApiError>;
    pub fn ollama_balance(&self, base_url: &str) -> Result<serde_json::Value, ApiError>;
    pub fn ollama_usage_24h(&self, base_url: &str) -> Result<serde_json::Value, ApiError>;
}
fn get_json(request: ureq::Request) -> Result<serde_json::Value, ApiError>;
fn get_json_no_redirect(request: ureq::Request) -> Result<serde_json::Value, ApiError>;
fn call(request: ureq::Request) -> Result<ureq::Response, ApiError>;
fn read_json(response: ureq::Response) -> Result<serde_json::Value, ApiError>;
fn ollama_url(base_url: &str, path: &str) -> String;
fn transport_error(error: ureq::Transport) -> ApiError;
fn refused_connection(error: &ureq::Transport) -> bool;
pub(crate) fn mark_highest_active(limits: &mut [Limit]);
fn highest_limit_index(limits: &[Limit]) -> Option<usize>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    // …
    #[error("cannot connect: {0}")]
    NoConnection(String),
    // …
}
```

`codex_usage` sends `GET CODEX_USAGE_URL` with `User-Agent: seatswap/<version>`,
`Authorization: Bearer <access token>` and `chatgpt-account-id: <account id>`; it returns the parsed
JSON and knows nothing about `auth.json`. `codex_refresh` posts the refresh grant to
`CODEX_TOKEN_URL` only, with no `Authorization` and no redirect; it is the one call that may renew a
saved Codex login. `ollama_balance` asks `<base>/api/balance` and
`ollama_usage_24h` asks `<base>/api/usage?range=24h`, both with only the User-Agent. All three use
the `lookup_agent` (the agent of the two look-ups too): `redirects(0)` plus `no_redirect` makes a 3xx
answer an `ApiError::Http` naming where it pointed, and nothing is asked there. `get_json_no_redirect`
is the status/body/decode path for these three GETs and for Frankfurter's rate look-up; models.dev
has its own reader (the 304 answer and the size cap) on the same no-redirect agent, and Claude's
`usage` GET goes through `get_json`. `get_json` and `get_json_no_redirect` share `call` (status and
transport mapping) and `read_json` (body and JSON): a non-2xx answer is `ApiError::Http` (or
`Unauthorized`, `RateLimited`, `Server` by status), a redirect in the no-redirect path is an
`ApiError::Http` whose body says `redirected to {location}; seatswap asks no other host`, an
unreadable body is `Network`, and unparsable JSON is `Decode`. `ollama_url` removes one trailing `/`
from the base and appends the endpoint's own path, so a base with a path keeps it.

The one highest-limit rule lives here too: `highest_limit_index` is the index of the highest percent,
the first of equals. `mark_highest_active` sets that limit's `is_active` for a provider reading, and
`Usage::binding` uses the same index for its fallback when no limit is active, so a Claude reply and a
a Claude reply and an OpenAI or Ollama reading choose the same limit when two are equal.

`transport_error` maps a transport failure: `ConnectionFailed` whose source chain holds an
`io::Error` of kind `ConnectionRefused` becomes `ApiError::NoConnection` (displayed as `cannot
connect: {reason}`); `BadHeader` becomes the fixed `network error: a request header holds a character
that is not allowed`, because ureq's own text is the whole header line and an `Authorization` line
contains the token; every other kind is `Network`. `ApiError::is_transient` counts `RateLimited`,
`Server`, `Network` and `NoConnection`; the provider thread simply tries again at its next round.

## history — provider readings stay out of every Claude reader (step D)

```rust
pub trait Record {
    // The default: a record that names no account belongs to no provider either.
    fn is_claude(&self) -> bool { true }
}
impl AccountRef {
    pub fn is_claude(&self) -> bool; // Company::of_account is Anthropic
}
impl Record for Event {
    fn is_claude(&self) -> bool;     // no account, or at least one Claude account
}
impl HistoryDir {
    pub fn read_claude<R: Record>(&self, range: &Range, hosts: &Hosts) -> Result<Read<R>, HistoryError>;
    pub fn finished_claude_windows(&self, range: &Range, hosts: &Hosts)
        -> Result<Read<WindowRecord>, HistoryError>;
}
```

`Sample` and `WindowRecord` implement `Record::is_claude` through their `AccountRef`; the other
record kinds keep the default `true`. `Event` is Claude's when the event names no account or at
least one Claude account; an event about a provider alone is not, and a `providerSwitch` is never
Claude's (both of its accounts are OpenAI logins). `read_claude` is `read` followed by
`retain(record.is_claude())` and keeps the warnings; `finished_claude_windows` is
`finished_windows` followed by the same filter. Every reader that serves Claude's accounts uses
these — auto-burn's live situation and replay, money, and all `stats` pages. `read` and
`finished_windows` remain for the recorder (which loads a provider's own last line), the export, the
import and the tests.

The provider thread records a reading through the existing `Recorder::reading(account, plan, usage,
at, Source::Poll)`; it writes a `Sample` (a `WindowRecord` when one of the reading's windows ends, and
`Blocked`/`Unblocked` events on a 100 % crossing) exactly as for an account. The `AccountRef` is
`{acc: <the provider's id>, org: "codex" | "ollama"}`, so a Claude organization UUID can never equal
it. A reading with `usage == None` (Ollama's new plan without an allowance) records nothing. The
export is raw history: it writes finished windows and daily token rows through `read` and
`finished_windows`, so a provider's finished window can appear in it while its samples do not (the
export has no sample rows); an import copies a host folder as it is, provider lines included.

## providers_rt — the thread, its client and the snapshot (step D)

```rust
pub(crate) const PROVIDER_POLL_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderView {
    pub provider: String,  // "codex" | "ollama"
    pub title: String,     // "Codex" | "Ollama"
    pub plan: Option<String>, pub usage: Option<Usage>, pub credits: Option<Credits>,
    pub spend_24h: Option<f64>, pub spend_24h_error: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>, pub error: Option<String> }
/// One saved Codex login as the pop-up lists it (R8).
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub(crate) struct CodexLoginView {
    pub account: AccountRef, pub abbr: String, pub label: String, pub color: String,
    pub plan: Option<String>, pub live: bool, pub usage: Option<Usage>,
    pub state: CodexLoginState, pub message: Option<String>,
    pub remove_note: Option<String> } // codex::LIVE_LOGIN_REMOVE on the live row, else None
#[derive(Serialize)] #[serde(rename_all = "lowercase")]
pub(crate) enum CodexLoginState { Ok, Expired, Error }
/// The cards and the saved logins, handed to the UI in one event.
pub(crate) struct ProviderLists { pub cards: Vec<ProviderView>,
    pub codex_logins: Vec<CodexLoginView> }

enum ProvidersCommand { Opened, SettingsSaved, CodexChanged, Stop }
#[derive(Clone)]
pub(crate) struct ProvidersClient { commands: Sender<ProvidersCommand> }
impl ProvidersClient {
    pub(crate) fn opened(&self) -> Result<(), String>;
    pub(crate) fn settings_saved(&self) -> Result<(), String>;
    pub(crate) fn codex_changed(&self) -> Result<(), String>;
}
pub(crate) struct ProvidersHandle { /* client, stop flag, JoinHandle */ }
impl ProvidersHandle { pub(crate) fn client(&self) -> ProvidersClient; pub(crate) fn stop(&mut self); }

pub(crate) type CodexPathsLookup =
    Box<dyn Fn(Option<&Path>) -> Result<CodexPaths, PathsError> + Send>;
/// The way to read the settings after a request; the app loads `accounts.json`.
pub(crate) type SettingsLookup = Box<dyn Fn(&Settings) -> Result<Settings, String> + Send>;
pub(crate) type ReportError = Box<dyn Fn(&str) + Send>;

pub(crate) fn spawn(app: AppPaths, history: Result<HistoryDir, String>, api: Api,
                    ui: Sender<UiEvent>, reported_errors: Arc<ReportedErrors>,
                    printed_warnings: Arc<Mutex<PrintedWarnings>>)
    -> Result<ProvidersHandle, String>;

pub(crate) struct ProvidersRuntime { /* api, ui, app, store_opener, codex_paths, recorder, … */ }
impl ProvidersRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(history: Result<HistoryDir, String>, api: Api, ui: Option<Sender<UiEvent>>,
                      app: AppPaths, store_opener: StoreOpener, codex_paths: CodexPathsLookup,
                      settings_lookup: SettingsLookup, report: ReportError,
                      printed_warnings: Arc<Mutex<PrintedWarnings>>) -> ProvidersRuntime;
    pub(crate) fn views(&self) -> Vec<ProviderView>;
    pub(crate) fn codex_logins(&self) -> Vec<CodexLoginView>;   // #[cfg(test)]
    pub(crate) fn poll_round(&mut self, settings: &Settings, now: DateTime<Utc>) -> Vec<ProviderView>;
    pub(crate) fn opened(&mut self, settings: &Settings, now: DateTime<Utc>) -> Vec<ProviderView>;
    pub(crate) fn settings_saved(&mut self, settings: &Settings, now: DateTime<Utc>)
        -> Vec<ProviderView>;
    pub(crate) fn codex_changed(&mut self, settings: &Settings, now: DateTime<Utc>)
        -> Vec<ProviderView>;
    pub(crate) fn report_error(&mut self, text: &str);
}
```

`spawn` always starts one named thread (`seatswap-providers`), whatever the two switches say. `run`
sets `next = now`, so the first round happens at once, then waits on the command channel until
`next`; a timeout is `poll_round` and the next wait is 300 s. `Opened`, `SettingsSaved`,
`CodexChanged` and a save do not move the five-minute instant. `settings_of` reads `accounts.json` again for every command and
round, and `poll` uses the same load through `SettingsLookup` after each request. So a switch, a Codex
folder or an Ollama URL counts without a restart; a load failure is reported through `ReportedErrors` (once per text per
run) and the round is skipped.

`ProvidersRuntime` keeps one `Option<ProviderView>` and one last-attempt time for each provider, in
the order Codex, Ollama. `poll_round` calls `settle(settings, previous)`: a card that is now off drops
its view and its last attempt; a card whose `codexHome`/`ollamaUrl` changed drops its old view,
because those numbers belong to another login or server, and is polled. `opened` and `settings_saved`
run the same check, so a folder or URL changed by hand in `accounts.json` drops the old numbers before
the next round just like a save. `poll_round` polls every switched-on card and every provider found
by itself (R6): Codex when `paths.auth_file.is_file()`, Ollama when this run's `PATH` value holds an
`ollama` program or this host stores an Ollama subscription (`found`). `spawn` reads `PATH` once and
the runtime keeps that value; an Ollama subscription only another host stores does not count. A
found provider's reading is recorded and its subscription ensured like a switched-on one's; with the
card off its errors go to `PrintedWarnings` (one line on stderr, once per text) through
`report_provider_error(false, …)`, never to `ReportedErrors`, which would notify.
`ProviderError::NotRunning { url }` is the Ollama poll's refused connection: it sets
`ollama_not_running` and is reported only on a change from running to not running; the card shows the
quiet line `Ollama is not running.`, and the next answering poll clears the flag. `opened` polls a
switched-on card whose place changed, or whose last *attempt* is
more than 60 s old (exactly 60 s is not older). `settings_saved` polls a card that was just switched
on, or whose `codexHome`/`ollamaUrl` changed; a card that is now off is dropped at once, and any
other save does not poll. `codex_changed` polls Codex at once after a switch or a removal, so the
live mark and the rows follow it before the next round.

`poll` marks the attempt time before the reading. Codex runs one round: `live_account` (a live
login that cannot be read fails the card), the live `read`, then `sync_live` as a separate step (a
failed save is reported and the live row's message says `This login is not saved: {error}`; the card
keeps its reading; a copy the save created or replaced gets its subscription at once, plan `None`),
then `read_saved` for each stored login in `accounts.json`, and
`ops::ensure_provider_subscription` for each answer's account so an OpenAI plan stays current. The
round remembers the live account; when it changes and this host's last `providerSwitch` does not
already name it as `to`, the poll writes one `providerSwitch {from: the last round's account, to,
by: "user"}` through `ops::record_provider_switch`, the one writer `switch_codex` uses too (at the
app's first round the last round's account is that event's `to`; with no event, nothing is written); a
failed renewal keeps its copy and is listed as `expired` with `SIGN_IN_AGAIN`, and any other failure
as `error`. Ollama runs `providers::ollama::read`. It then reads the settings again through the
`SettingsLookup`:
then reads the settings again through the `SettingsLookup`: an answer for a card that is now off, or
whose folder/URL changed while the request ran, is neither recorded nor shown. A failed settings read
is reported once and drops the answer too. Otherwise a success becomes `ProviderView { fetched_at:
Some(now), error: None, … }`, with `spend_24h`/`spend_24h_error` from the reading's
`Some(Ok)`/`Some(Err)`; a failure becomes a view that copies the last view's numbers, plan, 24 h
spend, its error and `fetched_at`, and sets `error` to the `ProviderError`'s text. A success with a
`usage` is recorded through `Recorder::reading`; a history write failure is turned into the app's
history text (`ops::history_unwritable`) and reported through the app's one `ReportedErrors` (once per text per run), and never
changes the card. A recorder that could not be opened at start records nothing and reports nothing: that error is
already the snapshot's error and was notified there.
`publish` compares the list with the one sent last and sends `UiEvent::Providers(list)` only when it
changes.

`Snapshot.providers: Vec<ProviderView>` is always present (empty when no card is on), and
`Snapshot.codexLogins: Vec<CodexLoginView>` lists the saved logins (empty while the card is off).
`Snapshot.providerSubscriptions: Vec<ProviderSubscriptionRow>` is the stored provider subscriptions
with their look and price (`ProviderSubscriptionRow`: `{account, company, label, abbr, color,
plan, price, priceCurrency}`); its JSON company key is lowercase, from `Company::of_account`. The main thread keeps them in
`GuiState`, fills them on `UiEvent::Providers`, and `apply_snapshot` copies them into every snapshot,
so the pop-up and the large window see the same lists. The JSON is camelCase:
`provider`, `title`, `plan`, `usage`, `credits`, `spend24h`, `spend24hError`, `fetchedAt`, `error`.
`Usage` is `{limits: [{kind, percent, severity, resetsAt, model, isActive}], extraUsageEnabled}`; a
`LimitKind::Other("monthly_credits")` is `{"other":"monthly_credits"}`; `Credits` is
`{left, allowance, until, purchased}`. A `CodexLoginView` is
`{account, abbr, label, color, plan, live, usage, state, message, removeNote}`, with `state` one of `ok`,
`expired`, `error`. Example:

```json
"providers": [
  {"provider":"codex","title":"Codex","plan":"Plus",
   "usage":{"limits":[
     {"kind":"session","percent":12.0,"severity":"normal","resetsAt":null,"model":null,"isActive":false},
     {"kind":"weekly_all","percent":48.0,"severity":"normal","resetsAt":null,"model":null,"isActive":true}],
    "extraUsageEnabled":false},
   "credits":null,"spend24h":null,"spend24hError":null,"fetchedAt":"2026-11-14T09:00:00Z","error":null},
  {"provider":"ollama","title":"Ollama","plan":null,
   "usage":{"limits":[
     {"kind":{"other":"monthly_credits"},"percent":29.2,"severity":"normal",
      "resetsAt":"2026-12-03T00:00:00Z","model":null,"isActive":true}],
    "extraUsageEnabled":false},
   "credits":{"left":212.4,"allowance":300.0,"until":"2026-12-03T00:00:00Z","purchased":0.0},
   "spend24h":3.18,"spend24hError":null,"fetchedAt":"2026-11-14T09:00:00Z","error":null}
]
```

## app: the pop-up's provider card (step D)

`panel.js` reads `state.snapshot.providers` through `providerViews()` (only objects), and
`renderProviders()` fills `#provider-cards` under `#providers`; with no switched-on card it clears
the list and hides the section, so neither heading nor gap remains. The heading is the static
`Other providers` in `index.html`. The keyboard cursor and the card-order animation stay on account
cards.

`renderProviderCard(view)` uses `credits = isObject(view.credits)` and `numbers = credits || usage`.
A credits card takes its gauge from the `monthly_credits` limit when that exists (an allowance was
known) and gets the `no-gauge` class otherwise; a Codex or old-plan card takes the usual gauge from
`weekly_all` and `session`. The title is `view.title`. The plan and the age are shown only with
numbers: the plan first, then `usage {age}` from `usageAge(view)`, which returns nothing unless
`usage` or `credits` and `fetchedAt` exist and the reading is older than 120000 ms. A failed poll adds
the `stale` class (the CSS greys the gauge and both row lists, opacity 0.55) and the error text in a
`card-note error` under the rows; a first failure with no numbers leaves the title and the error
text.

The limit rows are the account card's `renderLimitRows`: `Week`, `Session`, the scoped limits by
their model (or `Scoped`) and every other limit by its kind name, in that order, each with its reset
when it has one. An `Other` limit that names a model is labelled `{model} · {name}`
(`gpt-6-astra · 5h limit`), so two per-model limits of the same length do not become two identical
rows. With no rows the card says `No usage limits in the reply.` A credits card instead uses
`renderCreditRows`:

| Row | Value |
|---|---|
| `Credits` | `{left} of {allowance} USD`, or `{left} USD` without an allowance; with the reset date, `  ·  resets {day}` |
| `Last 24h` | `{spend24h} USD` when it is a finite number; when `spend24hError` is a non-empty string, `not read: {spend24hError}` in the muted `not-read` row, with the text as its title |
| `Purchased` | `{purchased} USD`, only when it is a number above zero |

Amounts use `money`, the page's one rule (`1,234 USD` from 10 on, `6.47 USD` below) with `USD` after
each amount, so a full allowance reads `212 USD of 300 USD`; `twoDecimalAmount` is gone. A provider
card has no accent bar and no Switch button.

`fixture.js` keeps every provider view in `providerCardViews` and computes `providers` from its own
`codexCard`/`ollamaCard` settings (`fixtureOpenProviders`), so the browser bench shows only the
switched-on cards in the Rust order; `fixtureSaveSetting` recomputes the list, the fixture's settings
default both cards on, and its `providersOff` hook switches both off through the settings save.
`providerError(text)` keeps the last numbers and sets the error; `spendError(text)` sets `spend24h`
to null and `spend24hError` to the text.

## app: the Providers settings group (step D)

`settings.js`'s `GROUPS` gains `{ id: "providers", label: "Providers" }` after Agents and before
History and hosts; the alias `prov` is accepted, and `#providers` in the address opens the group.
`providersCard()` is the panel `Providers` with the subtitle `cards for OpenAI (Codex) and Ollama's
cloud plan` and two sections:

- **OpenAI (Codex)**: `Show a Codex card` (toggle `codexCard`, note `every 5 minutes`), the note
  `seatswap reads Codex's login file and asks chatgpt.com for your usage every 5 minutes. It writes
  auth.json only when you switch between saved logins.`, and `Codex folder` (text field `codexHome`,
  note `empty: $CODEX_HOME or ~/.codex`). The OpenAI card lists the saved logins, each with *Switch*
  and *Remove saved login*; the live one's removal is refused.
- **Ollama**: `Show an Ollama card` (toggle `ollamaCard`, note `asks the Ollama server on this machine
  for your cloud balance every 5 minutes`) and `Ollama server` (text field `ollamaUrl`).

- **Google (Antigravity)**: `Show a Google card` (toggle `googleCard`) and the note that Connect on the
  card sets agy's `statusLine` and never replaces one the person set.

The fields save through the settings page's one writer; the backend's `Settings::validate` refuses a
relative `codexHome` or a bad `ollamaUrl` and the page shows that text under the row and puts the
control back. After every successful save the worker calls `ProvidersClient::settings_saved`, which
is what makes a newly switched-on card or a changed folder/URL poll at once. The pop-up calls
`ProvidersClient::opened` when it opens (`panel.rs`), which is the 60 s rule above.

## google — the numbers `agy` hands to the status line (spec §7.9)

```rust
// core::providers::google
pub struct GoogleBucket { pub id: String, pub percent: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>, pub problem: Option<String> }
pub struct GoogleFeed { pub at: DateTime<Utc>, pub email: String, pub plan: Option<String>,
    pub buckets: Vec<GoogleBucket> }                       // <data>/google.json
pub struct GoogleReading { pub reading: ProviderReading,
    pub recorded: ProviderReading,   // Session and WeeklyAll as they were at `at`; no usage without either
    pub at: DateTime<Utc>, pub notes: Vec<String> }
pub fn is_payload(raw: &Value) -> bool;
pub fn feed_from_payload(raw: &Value, now: DateTime<Utc>) -> Option<GoogleFeed>;
pub fn write_if_due(app: &AppPaths, feed: &GoogleFeed) -> io::Result<bool>; // never back to an older number
pub fn read(app: &AppPaths, now: DateTime<Utc>) -> Result<Option<GoogleReading>, ProviderError>;
pub fn reading(feed: GoogleFeed, now: DateTime<Utc>) -> GoogleReading;
pub fn limit_name(kind: &LimitKind) -> &str;
pub struct Agy { pub paths: AgyPaths, pub program: PathBuf }   // program: the console seatswap
pub enum AgyLink { NotInstalled, NotConnected, Connected, Other(String) }
pub fn agy_status_line(program: &Path) -> Result<Value, ProviderError>;
pub fn agy_link(agy: &Agy) -> Result<AgyLink, ProviderError>;
pub fn connect_agy(agy: &Agy) -> Result<(), ProviderError>;   // refuses NotInstalled and Other
// core::paths
pub struct AgyPaths { pub dir: PathBuf }                    // ~/.gemini/antigravity-cli
impl AgyPaths { pub fn resolve() -> Result<AgyPaths, PathsError>; pub fn at(dir: &Path) -> AgyPaths;
    pub fn settings_file(&self) -> PathBuf; }
// core::claude_settings: the one statusLine reader and writer, for Claude Code's and agy's file
pub fn status_line_in(path: &Path) -> Result<Option<Value>, ClaudeSettingsError>;
pub fn set_status_line_in(path: &Path, value: Option<&Value>) -> Result<Option<Value>, ClaudeSettingsError>;
pub fn status_line_command(exe: &Path) -> Result<String, ClaudeSettingsError>;
// core::feed: the one write and read rule of a status line's file
pub(crate) fn write_file_if_due<T>(app, path, value: T, at, stored_state: FnOnce(&T, &mut T) -> (at, same)) -> io::Result<bool>;
pub(crate) fn read_file<T>(path: &Path) -> io::Result<Option<T>>;
// app::statusline: None when the payload is not agy's
pub(crate) fn google_line(app, payload, now) -> Result<Option<String>, String>;
```

`cli::statusline_line` asks `google_line` first. The provider thread's third provider reads the file at
each round and when the pop-up opens; `ProviderView` carries `email` and `notes` for it, and `fetchedAt`
is the time `agy` said the numbers. `found` is whether the file exists: the reading is recorded with the
card off too, through `record(reading, at, src, on)` (`Source::Statusline`, at the feed's `at`) and
`ensure_subscription`, once per `at` (`google_recorded`). Without the file it asks `agy_link` (the runtime holds `agy: Result<Agy, String>`, built once by
`providers_rt::real_agy`; tests pass an `Agy` in their tempdir) and `ProviderView.can_connect` tells
the pop-up to show **Connect agy**. The pop-up's `connect_agy` command goes through the action thread
(`ActionCommand::ConnectAgy`), which calls `google::connect_agy` and then
`ProvidersClient::changed(Provider::Google)`; Codex's switch uses the same `changed(Provider::Codex)`.

## Step E: the Harness, Codex sessions and durations (1.2.0)

### harness.rs

`core::harness` reads DeepSeek Harness's session store and flash-agents' job reports, and links a
running job's process to the Claude Code session that started it.

```rust
// harness.rs
pub const HARNESS_QUIET_MINUTES: i64 = 30;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessPaths { pub sessions: PathBuf,   // $DSH_HOME/sessions, else ~/.dsh/sessions
                          pub reports: PathBuf }   // flash-agents' state folder, by
                                                   // paths::harness_folders' rule
impl HarnessPaths {
    pub fn resolve() -> Result<HarnessPaths, PathsError>;           // the app only
    pub fn resolve_with(env: &HarnessEnv, home: &Path)
        -> Result<HarnessPaths, PathsError>;                        // tests, one resolver
    pub fn at(sessions: PathBuf, reports: PathBuf) -> HarnessPaths; // tests
}
pub struct HarnessSession { pub id: String, pub file: PathBuf, pub parent: Option<String>,
    pub depth: u32, pub cwd: PathBuf, pub created: DateTime<Utc>, pub last: DateTime<Utc>,
    pub last_message: DateTime<Utc>, pub model: Option<String>, pub tokens: Tokens,
    pub messages: u32, pub messages_detail: Vec<HarnessMessage>, pub finished: bool,
    pub title: Option<String> }
pub struct HarnessMessage { pub at: DateTime<Utc>, pub model: Option<String>,
    pub usage: Option<Tokens> }
pub struct HarnessRead { pub sessions: Vec<HarnessSession>, pub warnings: Vec<String> }
pub struct HarnessLinks { pub owners: BTreeMap<String, String>,  // Harness id → Claude Code session
                          pub warnings: Vec<String> }
pub fn sessions(paths: &HarnessPaths, since: DateTime<Utc>) -> HarnessRead;
pub fn links(paths: &HarnessPaths, seen: &BTreeMap<String, SessionSeen>,
    closed: &BTreeMap<String, ClosedJob>) -> HarnessLinks;

pub trait ProcSource { fn pids(&self) -> io::Result<Vec<u32>>;
                       fn open_files(&self, pid: u32) -> io::Result<Vec<PathBuf>>;
                       fn parent(&self, pid: u32) -> Option<u32>; }
pub struct RealProc;                                    // /proc on Linux; empty elsewhere
pub struct ProcLinks { pub owners: BTreeMap<String, String>,
    pub locks: BTreeSet<String>, pub files: BTreeSet<PathBuf>, pub warnings: Vec<String> }
pub fn proc_links(paths: &HarnessPaths, procs: &dyn ProcSource,
    claude_pids: &BTreeMap<u32, String>) -> ProcLinks;
pub fn proc_links_in(sessions: Option<&Path>, procs: &dyn ProcSource,
    claude_pids: &BTreeMap<u32, String>) -> ProcLinks;   // Linux only; default elsewhere
pub fn is_running(session: &HarnessSession, lock_held: bool, now: DateTime<Utc>) -> bool;
pub fn is_open(session: &HarnessSession, lock_held: bool, now: DateTime<Utc>) -> bool;
pub fn copy_source(cwd: &Path, roots: &[&str]) -> Option<String>;
```

A session is `<sessions>/<cwd-key>/<id>/session.v4.jsonl.zstd` with an empty `session.lock` beside
it; `sessions` lists every cwd key, opens only files whose last write is at or after `since`, and
reads the header and the events. A file still being written ends mid-frame: the bytes decoded so far
are used up to the last whole line, and that is not a warning. A store that does not exist is no
session and no warning. A missing `assistant/message` usage is an answer with no tokens; a message's
`time` names the hour its tokens belong to. The last `session/title` is the session's `title`; an empty or all-space one is no title. It names a Harness row that is not a flash job (and goes into the live report with it), or is a flash job's or worker's task (shown live, cleared before the report); it never reaches a history record. The empty-title rule is `transcripts::session_title`, shared with `ai-title`; a last `session/title` without a title clears it.

`links` puts a job id named by a transcript under that transcript's Claude Code session. A
`flash-task-<uuid>` names itself; a `job-<8hex>` is resolved through its report's `sessionId`. A
resumed session carries the old transcript's tool results, so one job can be named twice: the
session with the later `last` owns it. A report that cannot be read is a warning, never a failed
read. `proc_links` walks the process table once and returns the held locks, their owners through the
parent chain, and every open file. On a system without `/proc`, `RealProc` has no pids and
`proc_links_in` returns `ProcLinks::default()`, so there is no process link there; a job is linked
once its result is in a transcript.

### codex_sessions.rs

```rust
// codex_sessions.rs
pub struct CodexSession { pub id: String, pub cwd: PathBuf, pub model: Option<String>,
    pub started: DateTime<Utc>, pub last: DateTime<Utc>, pub tokens: Tokens,
    pub last_hour: Option<Tokens>, pub busy: bool,
    pub title: Option<String>,   // newest `thread_name` for the id in `session_index.jsonl`
    pub(crate) file: PathBuf,    // not part of the page's shape
    pub(crate) series: Vec<CodexTokenPoint> }            // every token_count total, in time order
pub struct CodexRead { pub sessions: Vec<CodexSession>, pub warnings: Vec<String> }
/// Every Codex session whose rollout file was last written at or after `since`.
pub fn sessions(codex_home: &Path, since: DateTime<Utc>, now: DateTime<Utc>) -> CodexRead;
#[derive(Default)] pub struct CodexLines { pub lines: Vec<TokenRecord>, pub warnings: Vec<String> }
/// The token lines of the Codex sessions, and what could not be recorded. `recorded` is the scan
/// state's `codex_recorded`, keyed by session id; on return it holds the high-water mark, the
/// time the pass reached and the rollout file (`transcripts::CodexRecorded`).
pub fn codex_token_lines(sessions: &[CodexSession],
    recorded: &mut BTreeMap<String, CodexRecorded>, home: &Path, since: DateTime<Utc>) -> CodexLines;
```

A rollout is `<codex_home>/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl`; every day folder is
listed whatever its date, and a rollout is opened only when its metadata's last write is at or after
`since`. The file's last write decides, not its day folder, so a session resumed in an old folder is
read. A folder that does not exist is no session and no warning; a symlink to something that is not
a folder is a warning. A torn last line is ignored; a broken whole line is a warning naming the file
and the line, and the rest is still read; a file without `session_meta` is a warning and no session.

A `token_count` with `info: null` is Codex's normal line before the first answer: it changes nothing
and is no warning. One whose `info` is an object without `total_token_usage`, and one with no `info`
key, are warnings. `token_usage_record` lines are not read at all. `cache_write_input_tokens`
missing counts as 0. Two rollout files with the same
session id are one session: the file whose last event is newer gives the model, `busy` and `last`,
`started` is the earlier, and `tokens` is the newest total in time over all files, so a newer file
without a total changes no token. `last_hour` is the newest total minus the newest total at or before
`now - 60 min`, or the whole newest total when there is none; when the newest total is lower than
that baseline in any field, `last_hour` is `None` and the read warns, naming the session.

`codex_token_lines` writes the growth of `info.total_token_usage` between `token_count` lines, per
`transcripts::bucket_start` bucket and the `turn_context` model in force at each line:
`input = input_tokens − cached_input_tokens`, `cr = cached_input_tokens`, `out = output_tokens`,
`cw = cache_write_input_tokens` or 0 when absent (documented rule), and `answers` the number of grown
lines. Growth counts only above the highest total the session has reached, per kind, so a repeated
total records nothing and a total below the mark is a warning, never a negative line. A session not
recorded before starts at the newest total before `since`; one that started inside the window starts
at zero. A grown line without a model is a warning and keeps its total back for a later pass; a
session not seen within 24 h of its end is not recorded. `ScanState::set_codex_recorded` saves the
map, `prune_codex_recorded(sessions, read_ids)` removes an entry whose file is gone, and the state
file is `codex.json` (`codex_recorded`, `codex_changed`). The sessions thread writes them in the
Harness order: save the recorded totals, `recorder.tokens`, roll back on error, then count the lines
as written.

### duration.rs — the one spelling of a duration

```rust
// duration.rs
/// `12s`, `12min`, `4h`, `2h 31min`, `3d`, `3d 4h`.
pub fn duration_text(seconds: u64) -> String;
```

The rule: below a minute `12s`; below an hour `12min`; below a day `4h` or `2h 31min`; from a day on
`3d` or `3d 4h`. No space between a number and its unit, one space between two units, never more than
two units, no leading zero (`2h 5min`, not `2h 05min`). The minutes of a duration of a day or more
are not shown. A fractional hour count (`2.5 h`) is `2h 30min`; a time that has passed reads `0s`.
`about 2h` keeps its word.

The caller rounds, the function never does: `format.rs`'s `format_age` floors an age to its largest
unit and prints `12s ago`, `5min ago`, `2h ago`, `3d ago`; the reset text and `compact_left` round up
to the minute (`compact_left`'s week form to the hour, so `16h` and `3d 16h`); `stats::hours_text`
rounds to the minute and writes no hours at all as `0h`, not `0s` (an hour figure such as the
blocked hours counts hours); `burn_text.rs`'s hour figures and `providers/codex.rs`'s limit names go through
it too. A negative or non-finite hour count keeps the old `h` text (`-2 h`); callers never pass one.
The pages have
`durationText(seconds)` in `common.js` with the same rule, exported, and `hoursText(hours,
options)` beside it for every hour figure (blocked, avoidable, working, idle hours): the seconds of
`Math.round(hours * 3600)` through `durationText`, with none at all as `0h` like `stats::hours_text`
(1.3.0). Subscriptions' `workHoursText` rounds its hours of work first and calls it; `sessions.js`'s local `durationText(since, now)` becomes
`sinceText`, and the fixtures use common's function.

### the sessions thread's 1.2.0 rules

`HARNESS_SINCE_HOURS = 24`: Harness and Codex sessions older than a day are not live rows, but a
Harness total already recorded is kept while its session file exists. Every pass reads
`harness::sessions` and `codex_sessions::sessions`, walks the process table only when
`codex_needs_process_walk` says a Codex rollout is not recent (and once for the Harness locks), and
feeds `harness_rows` and `codex_rows`. `apply_provider_costs` reads `feed.json` every pass and keeps
the last cost per session in `memory.feed_costs`, because the feed holds only its last writer; a
`cost_problem` becomes a warning on every pass while it lasts. A row's own cost is converted once,
through `money::api_in_currency`, and a converted value that is not finite is not used: the row keeps
the valuer's API value and the thread names the session.

## shared.rs — account looks and settings shared between hosts (1.2.0, S-5)

Spec §5 "Shared between hosts", decision N5. The history folder is the medium; no second file.

```rust
// store.rs
pub const SHARED_SETTINGS: [&str; 8] = ["currency", "notifyOnSwitch", "notifyUsableAgain",
    "notifyWaiting", "notifyWaitingHosts", "notifyLoginEnds", "statusline", "statuslineFeed"];
impl Settings {
    /// One key replaced by its JSON value, checked as a whole: UnknownSetting, SettingValue
    /// {key, message} (serde's text, shown as "<key>: <message>") or validate()'s error.
    pub fn with_setting(&self, key: &str, value: serde_json::Value) -> Result<Settings, StoreError>;
    pub fn value_of(&self, key: &str) -> Result<serde_json::Value, StoreError>;
}
pub fn validate_currency(currency: &str) -> Result<(), StoreError>; // now pub

// switcher.rs
pub struct Edited { pub before: Account, pub account: Account }
impl Edited { pub fn price_changed(&self) -> bool; } // price or price_currency differ

// shared.rs
pub struct Stamped<T> { pub value: T, pub at: DateTime<Utc>, pub host: String }
pub struct SharedPrice { pub amount: Option<f64>, pub currency: Option<String> }
pub struct SharedLook { pub label, pub abbr, pub color: Option<Stamped<String>>,
    pub auto_burn: Option<Stamped<bool>>, pub price: Option<Stamped<SharedPrice>> }
pub struct Shared { pub accounts: BTreeMap<AccountRef, SharedLook>,
    pub settings: BTreeMap<String, Stamped<serde_json::Value>>, pub warnings: Vec<String> }
pub struct Applied { pub accounts: Vec<String> /* abbrs */, pub settings: Vec<String> /* keys */,
    pub warnings: Vec<String> }                    // Applied::changed()
pub struct Seeded { pub written: usize, pub warnings: Vec<String> }
pub enum SharedError { History(HistoryError), Store(StoreError), Lock(LockError) }

pub fn edit_events(edited: &Edited) -> Vec<EventKind>;           // AccountLook (changed fields), PriceChanged
pub fn setting_events(previous: &Settings, next: &Settings) -> Result<Vec<EventKind>, StoreError>;
pub fn latest(events: &[Hosted<Event>]) -> Shared;
pub fn apply(store: &Store, shared: &Shared) -> Result<Applied, SharedError>;
pub fn seed(store: &Store, shared: &Shared, recorder: &Recorder, now: DateTime<Utc>)
    -> Result<Seeded, SharedError>;
pub fn sync(store: &Store, recorder: &Recorder, now: DateTime<Utc>) -> Result<Applied, SharedError>;
```

Lines: `{"t":"…","type":"accountLook","account":{"acc":"…","org":"…"},"label":"Lab","autoBurn":false}`
(only the changed fields) and `{"t":"…","type":"settingSet","key":"currency","value":"EUR"}`. The
price is not part of `accountLook`: `priceChanged` stays its one event.

- `latest`: per account and field (label, abbr, color, autoBurn, price from `priceChanged`) and per
  setting key, the value with the greatest `(t, host)`; the same time and host is the later line. A
  `settingSet` for a key outside `SHARED_SETTINGS` is left out with a warning naming key and host.
- `apply`: under the app lock, one load of `accounts.json`; for each local account by
  `AccountRef::of`: `validate_label`, `validate_color`, the price through `validate_price` and
  `validate_currency` (a cleared price has no currency); abbreviations are decided together
  (`validate_abbr` against the abbreviations the other accounts will have; a clash refuses one
  account at a time until none is left, so a swap applies). Settings through `with_setting`. Every
  refusal is a warning `account AC (Acme): the shared colour from host vega is not applied: …` or
  `setting currency: the shared value from host vega is not applied: …`. Writes the file only when
  something changed, and writes no event.
- `seed`: for each local account one `AccountLook` with the fields no host shared, a `PriceChanged`
  for a stored price no host shared, and a `SettingSet` per unshared key whose value differs from
  `Settings::default()`. A failed append (a status line over 4096 bytes) is a warning.
- `sync`: `read::<Event>(Range::ALL, Hosts::All)`, `latest`, `seed`, `apply`; warnings in that order.

Who runs it:
- `gui/worker.rs`: `ActionCommand::Share` runs `ActionRuntime::share()` (`shared::sync`; warnings
  through `PrintedWarnings`, an error through `ReportedErrors`, `EngineCommand::Publish` when
  something changed). The engine thread sends it from `run()` at start and from `poll()` when
  `share_due` says so (`SHARE_EVERY_MIN = 5`, on the existing 30 s tick, no own timer). It runs on
  the action thread so it never lands between an edit and its event. `save_current` and a switch
  that saved a new account run `share()` at once. Without a history folder nothing is shared.
- `cli.rs`: `add`, `next --save` and a `use`/`next` that saved the unsaved login run
  `share_new_account` (warnings printed, never a failed command); `add` prints the account as stored
  after it. `seatswap edit` writes `ops::record_edit`.
- The pages need no command: the snapshot after `Publish` carries the new settings and accounts.

Known limits: an edit made with the CLI at the very moment the tray app applies can be undone until
the next round (at most 5 minutes later), because the CLI writes its event after its app lock. A
host that adds an account before another host's events reached it through the sync seeds its own
look; the newer one wins everywhere.

## 1.2.0 release-candidate fixes (2026-11-17)

**money.** `pub fn model_price_key(model: &str) -> String` is the one map from a model id to its
price key (spec §11.5): `claude-*` → `anthropic/<id>`, `<name>:cloud` → `ollama-cloud/<name>`,
`<name>:<tag>-cloud` → `ollama-cloud/<name>:<tag>`, else `openai/<id>`. `Prices::of_model` uses it.

**harness.** `pub struct JobReports` caches flash-agents' job reports by file name:
`refresh(&HarnessPaths) -> Vec<String>` parses only reports it has not read (a broken one is a warning
and is tried again), `label(id) -> Option<&str>` by the job's `sessionId` (a blank label is none).
`links(reports: &JobReports, seen, closed)`. `copy_source` knows the three worker-copy layouts and
slot names of spec §3.8 and matches a root as recorded and with either case of its drive letter. An
empty or all-space Harness model is no model.

**live_sessions.** `LiveAgent.provider: String` (`CLAUDE_PROVIDER` or `pub const HARNESS_PROVIDER`);
an older host report's agents take their session's provider while the report is read.
`pub struct RecordedTokens` holds this host's TokenRecords from an hour on, by session:
`read(history, from) -> Result<(RecordedTokens, Vec<String>), HistoryError>`, `covers(from) -> bool`,
`add(&[TokenRecord])`; `pub fn records_from(files, state) -> Option<DateTime<Utc>>`.
`running(files, state, metas, linked: &BTreeMap<String, Vec<LiveAgent>>, row_tokens: RowTokens,
projects, pricing, host, now, project_of)`: with `RowTokens::Recorded { recorded, pending }` a row is
valued from its recorded lines plus the pending ones through the same private `of_records` as
`closed`; with `RowTokens::Transcripts { unwritten, reason }` (no history, or the records cannot be
read) its tokens are `seen.totals` + its unwritten lines + its linked agents', and every value is
unknown with the reason. `running` attaches the linked agents itself (`add_agents`, which adds no
tokens or values). A scan that hits its budget while nothing can be recorded is a warning.
`harness_rows(…, reports: &harness::JobReports, …)`: a flash job's task is its label, a worker inside
a flash tree has none.

**The sessions thread.** `RowsContext.recorded` is rebuilt every 300 s (retry after 60 s), or when it
does not reach back to `records_from`; lines written in a pass are added in memory, `unwritten` and
the Harness lines are passed as pending. The pricing rebuild for an earlier `from` has the same retry
guard. `SessionMemory.job_reports` is refreshed every pass before `links`.

**history.** New `history/carry.rs`: `pub fn bring_along_due(app, settings) -> Result<bool,
HistoryError>` (no lock, no write), `pub fn bring_along(app, settings) -> Result<Vec<String>,
HistoryError>` under the app lock (`BRING_ALONG_LOCK_TIMEOUT` 120 s), and `pub(super) fn
merge_folder(from_dir, to_dir) -> Result<Merged, HistoryError>`, used by the move and the import.
Lines are only appended: every reader sorts by `Record::at` (stable), so file order does not matter
and a concurrent append is never lost. A prefix keeps the longer file (a file torn inside a line that
the other completes is replaced through `write_atomic`); otherwise a multiset: a line k times in the
source and j < k times in the target is appended k − j times. Not-`.jsonl` files are copied only when
missing; `sessions-now.json` never. The root is remembered in `AppPaths::history_root_file()`
(`<data_dir>/history-root.json`, `{"historyRoot": …}`) only after a complete move. `Imported` gains
`warnings`, which the Settings page shows. `HistoryError::Lock(LockError)`.

**ops.** `pub(crate) enum Bringer { TrayApp, Command }`, `pub(crate) fn bring_history_along(app,
settings, by) -> Result<Vec<String>, String>`. `worker::spawn` calls it (TrayApp) before
`open_history`, its warnings and errors through `ReportedErrors`; `cli::open_context` (Command), through
`warn()`. `Command` asks the tray app's socket (`ipc::send_line(app, "feed")`); only `NotRunning` lets
it move, and it never takes `gui.lock`.

**gui/window.rs.** `show` validates the target and does the rest on a thread of its own
(`show_or_create`, one at a time): building a web view on the main thread or in a synchronous
command deadlocks on Windows (Tauri's `WebviewWindowBuilder::new`, "Known issues"). An error from
that thread goes through `gui::report_error` (the one `ReportedErrors`; printed only before it is
managed). The window is built hidden and shown and focused once its page reports
`PageLoadEvent::Finished` (once; a failed load ends with Finished too), so it never shows an empty
frame. On Linux `build` runs on the main thread (`run_on_main_thread`; the window thread waits on a
channel, so one build at a time still holds), makes sure the pop-up exists (`panel::preload`) and
builds the window `with_related_view` of the pop-up's web view: both share one WebKit web process.
A new web process cost about 0.6 s, and on a hybrid laptop its graphics set-up waited about 2 s for
a suspended NVIDIA card to wake (strace on the bench, 2026-11-17). The cost: the shared process keeps
about 180 MB after the window closes, and a long script in one page delays the other. Other systems
build on the window thread as before.

**gui/tray.rs.** The menu is a list of `MenuEntry` (`menu_entries(snapshot, now)`); `apply_menu`
replaces the tray's menu only when items come, go or move (`menu_change`) and otherwise sets changed
texts on the open menu's items, because replacing the menu closes it on Windows and a snapshot comes
with every sessions pass.

