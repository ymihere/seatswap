//! The history folder (spec §1): the record types, appending them, reading a
//! range and merging several hosts. One subfolder per host, one JSON Lines
//! file per kind and month; [`read`](HistoryDir::read) owns the reading side.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::api::LimitKind;
use crate::atomic::{self, Access};
use crate::paths::{self, AppPaths, PathsError};
use crate::project::ProjectKey;
use crate::store::{Account, Settings};

mod carry;
mod read;
mod write;

pub use carry::{bring_along, bring_along_due, BRING_ALONG_LOCK_TIMEOUT};
pub use read::{
    export_csv, export_json, HostConflict, HostInfo, Hosted, Hosts, Imported, Range, Read,
};
pub use write::{closed_windows, sample_of, Recorder};

/// One history read that keeps its records and adds its warnings to `warnings`
/// (spec §1.2). The statistics pages and [`crate::money::Valuer::build`] go
/// through it, so a torn line or an unknown shape is named and not dropped.
/// A warning already in `warnings` is not added again: a page that reads the
/// same files twice names each warning once.
pub(crate) fn keep_warnings<R: Record>(
    warnings: &RefCell<Vec<String>>,
    read: impl FnOnce() -> Result<Read<R>, HistoryError>,
) -> Result<Read<R>, HistoryError> {
    let read = read()?;
    let mut shown = warnings.borrow_mut();
    for warning in &read.warnings {
        if !shown.contains(warning) {
            shown.push(warning.clone());
        }
    }
    drop(shown);
    Ok(read)
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} line {line}: {source}")]
    Json {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "host {host}: {folder}/host.json names another machine id than this machine's ({machine_id}).\n\
         If another computer writes there, set a different hostName in the settings; if this computer lost its id, delete that host.json."
    )]
    HostTaken {
        host: String,
        folder: PathBuf,
        machine_id: PathBuf,
    },
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error(transparent)]
    Lock(#[from] crate::locks::LockError),
    #[error("{} does not exist", path.display())]
    NoFolder { path: PathBuf },
    #[error("from {} is not before until {}", from.to_rfc3339(), to.to_rfc3339())]
    Range {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    #[error("a {kind:?} record is too long: its line with the newline is more than 4096 bytes")]
    LineTooLong { kind: Kind },
    #[error("a {kind:?} record holds a number that is not finite")]
    NotFinite { kind: Kind },
}

/// `LimitKind` as the history writes it: the plain text the server sent
/// (`session`, `weekly_all`, `weekly_scoped`, or an unknown kind as itself),
/// never the object that `api::LimitKind`'s own serde form makes of
/// `LimitKind::Other` for `usage-cache.json`.
mod limit_kind {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::api::LimitKind;

    pub fn serialize<S: Serializer>(kind: &LimitKind, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(kind.as_api())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<LimitKind, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(LimitKind::from_api(&text))
    }
}

/// The eight kinds of history line, each with its own file per month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Samples,
    Windows,
    Events,
    Tokens,
    Sessions,
    Up,
    Projects,
    Ollama,
}

impl Kind {
    fn stem(self) -> &'static str {
        match self {
            Kind::Samples => "samples",
            Kind::Windows => "windows",
            Kind::Events => "events",
            Kind::Tokens => "tokens",
            Kind::Sessions => "sessions",
            Kind::Up => "up",
            Kind::Projects => "projects",
            Kind::Ollama => "ollama",
        }
    }
}

/// One line of one history file.
pub trait Record: Serialize + DeserializeOwned + Send {
    const KIND: Kind;
    fn at(&self) -> DateTime<Utc>;

    /// Whether this record belongs to a Claude account. A record that names no
    /// account answers `true`: it belongs to no provider either.
    fn is_claude(&self) -> bool {
        true
    }
}

/// An account by the ids the server uses, never by seatswap's local id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountRef {
    pub acc: String,
    pub org: String,
}

impl AccountRef {
    /// The one way to name a stored account in the history.
    pub fn of(account: &Account) -> AccountRef {
        AccountRef {
            acc: account.account_uuid.clone(),
            org: account.organization_uuid.clone(),
        }
    }

    /// Whether this is a Claude account's reference. The other companies'
    /// readings are stored beside Claude's with a provider's name as `org`; a
    /// Claude organization id is a 36-character UUID, so those names cannot
    /// clash with one ([`crate::money::Company::of_account`]).
    pub fn is_claude(&self) -> bool {
        crate::money::Company::of_account(self) == crate::money::Company::Anthropic
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Poll,
    Statusline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitSample {
    #[serde(with = "limit_kind")]
    pub k: LimitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    pub p: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub b: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub t: DateTime<Utc>,
    #[serde(flatten)]
    pub account: AccountRef,
    pub src: Source,
    pub l: Vec<LimitSample>,
}

impl Sample {
    /// The sample's limit of `kind`: the one way to find a limit in a sample.
    pub fn limit(&self, kind: &LimitKind) -> Option<&LimitSample> {
        self.l.iter().find(|limit| &limit.k == kind)
    }
}

impl Record for Sample {
    const KIND: Kind = Kind::Samples;

    fn at(&self) -> DateTime<Utc> {
        self.t
    }

    fn is_claude(&self) -> bool {
        self.account.is_claude()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowRecord {
    #[serde(flatten)]
    pub account: AccountRef,
    #[serde(with = "limit_kind")]
    pub k: LimitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    pub end: DateTime<Utc>,
    #[serde(rename = "final")]
    pub final_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost: Option<f64>,
    #[serde(rename = "lastSample")]
    pub last_sample: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub uncertain: bool,
    /// The window ended before its reset time because the limit dropped (a
    /// reset: by hand, of a model, or by the provider). Nothing expired then,
    /// so such a record has `lost: None`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset: bool,
    /// On the window that follows a reset: the percent of its first reading.
    /// The window's own use is `final − from`; absent means 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<f64>,
}

impl WindowRecord {
    /// A weekly window's start: the recorded `start`, or seven days before its
    /// `end` when the record has none. This is the rule the value of a token
    /// record and the pages use to find the window that holds it.
    pub fn weekly_start(&self) -> DateTime<Utc> {
        self.start.unwrap_or(self.end - chrono::Duration::days(7))
    }
}

impl Record for WindowRecord {
    const KIND: Kind = Kind::Windows;

    fn at(&self) -> DateTime<Utc> {
        self.end
    }

    fn is_claude(&self) -> bool {
        self.account.is_claude()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum By {
    User,
    Cli,
    Auto,
    Agent,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCounts {
    pub running: u32,
    pub busy: u32,
    /// How many sessions run on a model whose thinking is tied to the account
    /// (spec §2.11), as the sessions thread counts it; `None` when not known,
    /// and then left out of the file. An old line with `"tied":0` still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tied: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EventKind {
    Switch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<AccountRef>,
        to: AccountRef,
        by: By,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions: Option<SessionCounts>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        waited_secs: Option<u64>,
    },
    /// A switch between two saved Codex logins (R8): the field shapes are the
    /// `switch` event's, the type is its own. It is not a Claude switch: every
    /// reader of switches, timelines and switch counts reads
    /// [`EventKind::Switch`] only, and this event names Codex accounts, so
    /// `read_claude` leaves it out too.
    ProviderSwitch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<AccountRef>,
        to: AccountRef,
        by: By,
    },
    Blocked {
        account: AccountRef,
        #[serde(with = "limit_kind")]
        k: LimitKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        m: Option<String>,
    },
    Unblocked {
        account: AccountRef,
        #[serde(with = "limit_kind")]
        k: LimitKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        m: Option<String>,
    },
    AllBlocked,
    AnyUsable,
    AccountAdded {
        account: AccountRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
    },
    AccountRemoved {
        account: AccountRef,
    },
    PlanChanged {
        account: AccountRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// An account's monthly price changed. `amount` is the new price; `currency`
    /// is the setting's value at the time (spec §1.4).
    PriceChanged {
        account: AccountRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        currency: Option<String>,
    },
    LoginEnded {
        account: AccountRef,
    },
    LoginRenewed {
        account: AccountRef,
    },
    Live {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountRef>,
    },
    AgentCall {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        allowed: bool,
    },
    Waited {
        session: String,
        project: ProjectKey,
        secs: u64,
    },
    /// Auto-burn was paused or resumed. A pause ends at `paused_until`; `None`
    /// clears a pause (spec §2.5).
    AutoBurn {
        on: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paused_until: Option<DateTime<Utc>>,
    },
    /// A settings save changed whether auto-burn is on. It is not a pause or a
    /// resume: a toggle leaves an explicit pause alone (spec §7.4.3).
    AutoBurnSet {
        on: bool,
    },
    /// An account's look was edited on a host: only the fields that changed
    /// (spec §5, "Shared between hosts"). [`crate::shared`] reads these back.
    AccountLook {
        account: AccountRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abbr: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_burn: Option<bool>,
    },
    /// One shared setting ([`crate::store::SHARED_SETTINGS`]) was saved on a
    /// host: its new value, the same JSON the settings file holds for it.
    SettingSet {
        key: String,
        value: serde_json::Value,
    },
}

impl EventKind {
    /// The accounts an event names. A `Live`, a `Switch` or a `ProviderSwitch`
    /// may name two; an event about neither account names none.
    pub fn accounts(&self) -> Vec<&AccountRef> {
        match self {
            EventKind::Switch { from, to, .. } | EventKind::ProviderSwitch { from, to, .. } => {
                from.iter().chain(std::iter::once(to)).collect()
            }
            EventKind::Blocked { account, .. }
            | EventKind::Unblocked { account, .. }
            | EventKind::AccountAdded { account, .. }
            | EventKind::AccountRemoved { account }
            | EventKind::PlanChanged { account, .. }
            | EventKind::PriceChanged { account, .. }
            | EventKind::AccountLook { account, .. }
            | EventKind::LoginEnded { account }
            | EventKind::LoginRenewed { account } => vec![account],
            EventKind::Live { account } => account.iter().collect(),
            // Named one by one, so a new kind must say whether it names an
            // account.
            EventKind::AllBlocked
            | EventKind::AnyUsable
            | EventKind::AgentCall { .. }
            | EventKind::Waited { .. }
            | EventKind::AutoBurn { .. }
            | EventKind::AutoBurnSet { .. }
            | EventKind::SettingSet { .. } => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub t: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Record for Event {
    const KIND: Kind = Kind::Events;

    fn at(&self) -> DateTime<Utc> {
        self.t
    }

    /// True when the event names no account, or at least one Claude account:
    /// an event about a provider alone is not Claude's.
    fn is_claude(&self) -> bool {
        let accounts = self.kind.accounts();
        accounts.is_empty() || accounts.iter().any(|account| account.is_claude())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRecord {
    pub t: DateTime<Utc>,
    /// How long the line lasts, in minutes: 5 for a line written since 1.3, 60
    /// for the hour every older version wrote. A line without `min` is an hour.
    #[serde(
        rename = "min",
        default = "sixty",
        deserialize_with = "deserialize_minutes",
        skip_serializing_if = "is_sixty"
    )]
    pub minutes: u32,
    pub session: String,
    pub cwd: String,
    pub project: ProjectKey,
    pub model: String,
    #[serde(rename = "in")]
    pub input: u64,
    pub out: u64,
    pub cw: u64,
    pub cr: u64,
    pub answers: u32,
    pub agent_answers: u32,
}

impl TokenRecord {
    /// How long this line lasts: its `min` minutes. The one place a token line's
    /// length is read.
    pub fn span(&self) -> chrono::Duration {
        chrono::Duration::minutes(i64::from(self.minutes))
    }
}

impl Record for TokenRecord {
    const KIND: Kind = Kind::Tokens;

    fn at(&self) -> DateTime<Utc> {
        self.t
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub t: DateTime<Utc>,
    pub session: String,
    pub cwd: String,
    pub project: ProjectKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// Claude Code's own title of the session, when it has one. Absent in a
    /// record written before this field means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub agents_started: u32,
    pub agents_finished: u32,
    pub closed: bool,
}

impl Record for SessionRecord {
    const KIND: Kind = Kind::Sessions;

    fn at(&self) -> DateTime<Utc> {
        self.t
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpRecord {
    pub t: DateTime<Utc>,
}

impl Record for UpRecord {
    const KIND: Kind = Kind::Up;

    fn at(&self) -> DateTime<Utc> {
        self.t
    }
}

/// One hour of Ollama's own usage, made from one bucket of `/api/usage?range=24h`.
///
/// A running hour's bucket only grows: a later answer for the same hour has no
/// count ([`requests`](Self::requests), [`input`](Self::input),
/// [`cached_input`](Self::cached_input), [`output`](Self::output),
/// [`usd`](Self::usd)) smaller than the hour's last line and at least one
/// larger. A line is written only when that holds, so an older or overlapping
/// answer with smaller numbers never replaces a newer one. Readers take the
/// **largest line per hour**, over the hosts' lines of that hour and among one
/// host's own lines alike: that is the newest answer's numbers, never a sum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaHourRecord {
    /// The bucket's `from`; a bucket always runs exactly one hour.
    pub hour: DateTime<Utc>,
    pub requests: u64,
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub usd: f64,
}

impl Record for OllamaHourRecord {
    const KIND: Kind = Kind::Ollama;

    fn at(&self) -> DateTime<Utc> {
        self.hour
    }

    /// Ollama's own use is no Claude account's.
    fn is_claude(&self) -> bool {
        false
    }
}

/// One host's history folder: `<root>/<host>/`, shared with other hosts by
/// pointing `historyDir` at one synced folder on every machine.
#[derive(Clone)]
pub struct HistoryDir {
    root: PathBuf,
    host: String,
}

/// The one file in a host folder that is not history: this host's live state,
/// which the other hosts read (spec §3.8). The import never copies it.
pub(crate) const SESSIONS_NOW_FILE: &str = "sessions-now.json";

/// The one rule for "the history folder of these settings": the root and this
/// machine's host folder inside it. `historyDir`/`hostName` are used when set;
/// otherwise the app's history folder and this machine's host name are.
pub fn location(app: &AppPaths, settings: &Settings) -> Result<(PathBuf, String), HistoryError> {
    let root = paths::history_root(settings.history_dir.as_deref(), app)?;
    let host = paths::host_name(settings.host_name.as_deref())?;
    Ok((root, host))
}

impl HistoryDir {
    /// Root = `settings.historyDir` or `app.history_dir()`; host =
    /// `paths::host_name(settings.host_name)`. Creates `<root>/<host>/` and its
    /// `host.json` marker with this machine's id (created on first use) and
    /// refuses a folder marked with another machine's id.
    pub fn open(app: &AppPaths, settings: &Settings) -> Result<HistoryDir, HistoryError> {
        let (root, host) = location(app, settings)?;
        let history = HistoryDir { root, host };

        let host_dir = history.root.join(&history.host);
        atomic::ensure_private_dir(&host_dir).map_err(|source| HistoryError::Io {
            path: host_dir.clone(),
            source,
        })?;
        let machine_id_file = app.machine_id_file();
        let machine_id = read_or_create_machine_id(app)?;
        history.check_or_write_marker(
            &host_dir.join("host.json"),
            &machine_id,
            &machine_id_file,
        )?;
        Ok(history)
    }

    /// A history folder without the marker check (tests).
    pub fn at(root: &Path, host: &str) -> HistoryDir {
        HistoryDir {
            root: root.to_path_buf(),
            host: host.to_string(),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<host>/sessions-now.json`: one host's live state, not a history
    /// record (spec §3.8).
    pub fn sessions_now_file(&self, host: &str) -> PathBuf {
        self.root.join(host).join(SESSIONS_NOW_FILE)
    }

    /// Every subfolder of the root whose name follows `paths::host_name`'s rule,
    /// sorted, whether or not it holds a `host.json`. A name that is not a
    /// valid, already-lower-case host name (a sync service's conflict copy) is
    /// not a host folder.
    fn host_folders(&self) -> Result<Vec<String>, HistoryError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(HistoryError::Io {
                    path: self.root.clone(),
                    source,
                })
            }
        };
        let mut hosts = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| HistoryError::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            let is_dir = match fs::metadata(&path) {
                Ok(metadata) => metadata.is_dir(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(HistoryError::Io {
                        path: path.clone(),
                        source,
                    })
                }
            };
            if !is_dir {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // The name of a host folder follows `paths::host_name`'s rule and is
            // already in the lower case that rule produces; a sync service's
            // "vega (conflicted copy)" folder is not a host.
            if matches!(paths::host_name(Some(name)), Ok(known) if known == name) {
                hosts.push(name.to_string());
            }
        }
        hosts.sort();
        Ok(hosts)
    }

    /// The subfolders that hold a `host.json`, sorted. A name that is not a
    /// valid, already-lower-case host name (a sync service's conflict copy) is
    /// not a host.
    pub fn hosts(&self) -> Result<Vec<String>, HistoryError> {
        let mut hosts = Vec::new();
        for host in self.host_folders()? {
            let marker = self.root.join(&host).join("host.json");
            match fs::metadata(&marker) {
                Ok(metadata) if metadata.is_file() => hosts.push(host),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(HistoryError::Io {
                        path: marker,
                        source,
                    })
                }
            }
        }
        Ok(hosts)
    }

    /// Append one record to `<root>/<own host>/<stem>-YYYY-MM.jsonl`, the month
    /// of `record.at()` in UTC. A line over [`atomic::MAX_LINE_BYTES`] is an
    /// error before the folder or file is made.
    pub fn append<R: Record>(&self, record: &R) -> Result<(), HistoryError> {
        self.append_many(std::slice::from_ref(record))
    }

    /// Append several records, one line each. Every line of one month file is
    /// written with one append, so a failure leaves none of that file's new
    /// lines; every line is checked before any file is opened.
    pub fn append_many<R: Record>(&self, records: &[R]) -> Result<(), HistoryError> {
        if records.is_empty() {
            return Ok(());
        }
        let host_dir = self.root.join(&self.host);
        let mut by_file: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for record in records {
            let name = format!("{}-{}.jsonl", R::KIND.stem(), record.at().format("%Y-%m"));
            let path = host_dir.join(name);
            let line = encode::<R>(record, &path)?;
            by_file.entry(path).or_default().push(line);
        }
        atomic::ensure_private_dir(&host_dir).map_err(|source| HistoryError::Io {
            path: host_dir,
            source,
        })?;
        for (path, lines) in by_file {
            atomic::append_lines(&path, &lines, Access::Private)
                .map_err(|source| HistoryError::Io { path, source })?;
        }
        Ok(())
    }

    fn check_or_write_marker(
        &self,
        marker: &Path,
        machine_id: &str,
        machine_id_file: &Path,
    ) -> Result<(), HistoryError> {
        let Some(held) = read_marker(marker)? else {
            let text = format!("{{\"machineId\":\"{machine_id}\"}}");
            return atomic::write_atomic(marker, text.as_bytes(), Access::Private).map_err(
                |source| HistoryError::Io {
                    path: marker.to_path_buf(),
                    source,
                },
            );
        };
        if held.machine_id != machine_id {
            return Err(HistoryError::HostTaken {
                host: self.host.clone(),
                folder: self.root.join(&self.host),
                machine_id: machine_id_file.to_path_buf(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct HostMarker {
    #[serde(rename = "machineId")]
    machine_id: String,
}

/// The marker of a host folder, or `None` when its `host.json` is not there.
/// The id is trimmed; a file that does not parse is an error naming it.
fn read_marker(path: &Path) -> Result<Option<HostMarker>, HistoryError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(HistoryError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let mut marker: HostMarker =
        serde_json::from_str(&text).map_err(|source| HistoryError::Json {
            path: path.to_path_buf(),
            line: source.line(),
            source,
        })?;
    marker.machine_id = marker.machine_id.trim().to_string();
    Ok(Some(marker))
}

fn read_or_create_machine_id(app: &AppPaths) -> Result<String, HistoryError> {
    let path = app.machine_id_file();
    match fs::read_to_string(&path) {
        Ok(text) => parse_machine_id(&path, text.trim()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            atomic::ensure_private_dir(&app.data_dir).map_err(|source| HistoryError::Io {
                path: app.data_dir.clone(),
                source,
            })?;
            let id = uuid::Uuid::new_v4().to_string();
            atomic::write_atomic(&path, id.as_bytes(), Access::Private).map_err(|source| {
                HistoryError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
            // Two programs starting for the first time can each write an id;
            // the file holds the one that won. Read it back and use that.
            let text = fs::read_to_string(&path).map_err(|source| HistoryError::Io {
                path: path.clone(),
                source,
            })?;
            parse_machine_id(&path, text.trim())
        }
        Err(source) => Err(HistoryError::Io { path, source }),
    }
}

fn parse_machine_id(path: &Path, text: &str) -> Result<String, HistoryError> {
    match uuid::Uuid::parse_str(text) {
        Ok(id) => Ok(id.to_string()),
        Err(_) => Err(HistoryError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{text:?} is not a uuid; delete {} and seatswap makes a new one",
                    path.display()
                ),
            ),
        }),
    }
}

/// A reset time rounded up to the full minute: the server sends a moment just
/// before the boundary (`00:59:59.59`), which would otherwise read `00:59`
/// instead of `01:00`. An exact minute stays as it is.
pub fn reset_minute(resets_at: DateTime<Utc>) -> DateTime<Utc> {
    let into_minute = resets_at.timestamp().rem_euclid(60) * 1_000_000_000
        + i64::from(resets_at.timestamp_subsec_nanos());
    if into_minute == 0 {
        resets_at
    } else {
        resets_at + chrono::Duration::nanoseconds(60_000_000_000 - into_minute)
    }
}

/// Reset times within this many seconds of each other name the same window;
/// the server's reset times move by a little between answers.
pub const RESET_JITTER: i64 = 60;

/// Whether two reset times name the same window: they differ by at most
/// [`RESET_JITTER`] seconds. The one rule for a reset time's jitter; callers
/// that need a one-sided comparison write it with [`RESET_JITTER`].
pub fn same_reset(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    (a - b).abs() <= chrono::Duration::seconds(RESET_JITTER)
}

/// A reading at least this many percentage points below the previous reading
/// of the same account, limit and model, before the previous reading's reset
/// time, is a reset: the window ends there. A smaller drop is kept as a reading
/// and named once in a warning, never smoothed.
pub const RESET_DROP_POINTS: f64 = 1.0;

/// Whether a limit that fell from `previous_percent` to `current_percent` was
/// reset early: the drop is at least [`RESET_DROP_POINTS`] and `at` is more
/// than [`RESET_JITTER`] before `reset_at` (decision 12). A drop at or within
/// the jitter of the reset time is the window's ordinary end, not a reset; a
/// limit without a reset time cannot be seen as reset. This is the one rule
/// the history recorder, the "usable again" notification and auto-burn's
/// learned rates use.
pub fn is_early_reset(
    previous_percent: f64,
    current_percent: f64,
    reset_at: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> bool {
    let Some(reset_at) = reset_at else {
        return false;
    };
    reset_at.signed_duration_since(at) > chrono::Duration::seconds(RESET_JITTER)
        && previous_percent - current_percent >= RESET_DROP_POINTS
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The length of a line written before 1.3: one hour.
fn sixty() -> u32 {
    60
}

fn is_sixty(minutes: &u32) -> bool {
    *minutes == sixty()
}

/// Read a line's `min`: a positive number of minutes that divides one hour. A
/// value that cannot be a length is an error with the value in its text, never
/// a silent clamp.
fn deserialize_minutes<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let minutes = u32::deserialize(deserializer)?;
    if minutes == 0 || 60 % minutes != 0 {
        return Err(serde::de::Error::custom(format!(
            "a token line's min must be a positive number of minutes that divides 60, not {minutes}"
        )));
    }
    Ok(minutes)
}

/// One record as its JSON line, with the checks every history write makes.
fn encode<R: Record>(record: &R, path: &Path) -> Result<String, HistoryError> {
    let line = serde_json::to_string(record).map_err(|e| HistoryError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot encode a {} record: {e}", R::KIND.stem()),
        ),
    })?;
    // Every optional field is left out when it is `None`, so a `null` in the
    // serialized record can only be a non-finite number, which would read
    // back as a wrong value.
    let value: serde_json::Value = serde_json::from_str(&line).map_err(|e| HistoryError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "cannot read back the encoded {} record: {e}",
                R::KIND.stem()
            ),
        ),
    })?;
    if holds_null(&value) {
        return Err(HistoryError::NotFinite { kind: R::KIND });
    }
    if line.len() + 1 > atomic::MAX_LINE_BYTES {
        return Err(HistoryError::LineTooLong { kind: R::KIND });
    }
    Ok(line)
}

/// Whether a serialized record holds a `null` anywhere. All optional fields of
/// a record are omitted when they are `None`, so the only `null` serde_json can
/// put into a record is a non-finite `f64` (`NaN`, infinity).
fn holds_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(items) => items.iter().any(holds_null),
        serde_json::Value::Object(fields) => fields.values().any(holds_null),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::io::Write;

    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use tempfile::tempdir;

    use crate::api::LimitKind;
    use crate::paths::AppPaths;
    use crate::store::Settings;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse::<DateTime<Utc>>().expect("test date")
    }

    fn atlas_account() -> AccountRef {
        AccountRef {
            acc: "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04".to_string(),
            org: "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20".to_string(),
        }
    }

    fn a_sample(t: DateTime<Utc>, percent: f64) -> Sample {
        Sample {
            t,
            account: atlas_account(),
            src: Source::Poll,
            l: vec![LimitSample {
                k: LimitKind::Session,
                m: None,
                p: percent,
                r: Some(utc("2026-11-14T19:30:00Z")),
                b: false,
            }],
        }
    }

    fn wide_range() -> Range {
        Range {
            from: utc("2026-01-01T00:00:00Z"),
            to: utc("2027-01-01T00:00:00Z"),
        }
    }

    fn a_session_record(t: DateTime<Utc>, title: Option<String>) -> SessionRecord {
        SessionRecord {
            t,
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
            entrypoint: Some("cli".to_string()),
            title,
            first: t,
            last: t,
            agents_started: 0,
            agents_finished: 0,
            closed: false,
        }
    }

    #[test]
    fn a_sample_round_trips_with_the_short_keys() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let sample = Sample {
            t: utc("2026-11-14T14:02:11Z"),
            account: atlas_account(),
            src: Source::Poll,
            l: vec![
                LimitSample {
                    k: LimitKind::Session,
                    m: None,
                    p: 38.0,
                    r: Some(utc("2026-11-14T19:30:00Z")),
                    b: false,
                },
                LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 75.0,
                    r: Some(utc("2026-11-16T16:00:00Z")),
                    b: true,
                },
                LimitSample {
                    k: LimitKind::WeeklyScoped,
                    m: Some("Fable".to_string()),
                    p: 21.0,
                    r: Some(utc("2026-11-16T16:00:00Z")),
                    b: false,
                },
            ],
        };
        history.append(&sample).unwrap();

        let text = fs::read_to_string(dir.path().join("atlas/samples-2026-11.jsonl")).unwrap();
        let line = text.lines().next().unwrap();
        let value: Value = serde_json::from_str(line).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["t", "acc", "org", "src", "l"]);
        assert_eq!(value["l"][1]["b"], Value::Bool(true));
        assert!(value["l"][0].get("b").is_none());
        assert!(value["l"][0].get("m").is_none());

        let read = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].record, sample);
    }

    #[test]
    fn a_session_record_round_trips_its_title() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let record = a_session_record(
            utc("2026-11-14T10:00:00Z"),
            Some("Fix the invoice export".to_string()),
        );
        history.append(&record).unwrap();

        let read = history
            .read::<SessionRecord>(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(
            read.records[0].record.title.as_deref(),
            Some("Fix the invoice export")
        );
    }

    #[test]
    fn a_1_1_session_record_reads_without_a_title() {
        // The SessionRecord line 1.1.0 wrote: no `title` key at all.
        let line = serde_json::json!({
            "t": "2026-11-14T10:00:00Z",
            "session": "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34",
            "cwd": "/home/sam/projects/atlas",
            "project": {"kind": "repo", "id": "github.com/sam/atlas"},
            "entrypoint": "cli",
            "first": "2026-11-14T09:00:00Z",
            "last": "2026-11-14T10:00:00Z",
            "agentsStarted": 2,
            "agentsFinished": 1,
            "closed": true
        })
        .to_string();

        let record: SessionRecord = serde_json::from_str(&line).unwrap();

        assert_eq!(record.title, None);
        assert_eq!(record.session, "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34");
    }

    #[test]
    fn an_event_has_its_type_inline() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let event = Event {
            t: utc("2026-11-14T14:02:11Z"),
            kind: EventKind::Switch {
                from: None,
                to: atlas_account(),
                by: By::Agent,
                reason: None,
                cwd: Some("/home/sam/projects/ledger".to_string()),
                sessions: Some(SessionCounts {
                    running: 2,
                    busy: 1,
                    tied: None,
                }),
                waited_secs: None,
            },
        };
        history.append(&event).unwrap();

        let text = fs::read_to_string(dir.path().join("atlas/events-2026-11.jsonl")).unwrap();
        assert!(text.contains("\"type\":\"switch\""));
        assert!(text.contains("\"by\":\"agent\""));

        let read = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
    }

    /// A Codex switch is its own event type (R8): Claude's readers (`read_claude`,
    /// the switch counts, the timelines) never see it, and a 1.2 reader meets an
    /// unknown `type` and skips the line with a warning.
    #[test]
    fn a_provider_switch_is_not_a_claude_switch() {
        const OA: &str = "6a1d4c8e-2f5b-4d3a-9e7c-1b0a5f2d8c64";
        const OA2: &str = "c3e8f1a2-7b4d-4a69-8f2e-5d0c9b1a3e47";
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let claude = atlas_account();
        let oa = AccountRef {
            acc: OA.to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let oa2 = AccountRef {
            acc: OA2.to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };

        history
            .append(&Event {
                t: utc("2026-11-14T10:00:00Z"),
                kind: EventKind::Live {
                    account: Some(claude.clone()),
                },
            })
            .unwrap();
        history
            .append(&Event {
                t: utc("2026-11-14T11:00:00Z"),
                kind: EventKind::ProviderSwitch {
                    from: Some(oa),
                    to: oa2.clone(),
                    by: By::User,
                },
            })
            .unwrap();

        // The line is its own type and reads back.
        let text = fs::read_to_string(dir.path().join("atlas/events-2026-11.jsonl")).unwrap();
        assert!(text.contains("\"type\":\"providerSwitch\""), "{text}");
        assert!(text.contains("\"by\":\"user\""), "{text}");
        let read = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 2);
        assert!(
            read.records.iter().any(|hosted| matches!(
                &hosted.record.kind,
                EventKind::ProviderSwitch { to, by: By::User, .. } if to.acc == OA2
            )),
            "the provider switch does not read back: {read:?}"
        );

        // Claude's readers leave it out: it names no Claude account.
        let claude_only = history
            .read_claude::<Event>(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(claude_only.records.len(), 1);
        assert!(matches!(
            &claude_only.records[0].record.kind,
            EventKind::Live { .. }
        ));

        // The statistics' switch counts count no Codex switch.
        assert_eq!(
            crate::stats::switch_counts(&read.records, &wide_range(), None),
            crate::stats::SwitchCounts::default()
        );

        // A line of a type this version does not know -- what a 1.2 reader
        // meets with this event -- is skipped with a warning, never an error.
        let path = dir.path().join("atlas/events-2026-11.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"2026-11-14T13:00:00Z\",\"type\":\"somethingNew\"}\n")
            .unwrap();
        drop(file);
        let after = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(after.records.len(), 2);
        assert!(
            after
                .warnings
                .iter()
                .any(|warning| warning.contains("unknown variant")),
            "{:?}",
            after.warnings
        );

        // The timelines keep the live account: `Timeline::of` reads `Live` and
        // `Switch` only, and here through its one public door.
        assert_eq!(
            crate::burn::rates::account_at(&read.records, "atlas", utc("2026-11-14T12:00:00Z")),
            Some(claude)
        );
    }

    #[test]
    fn a_tied_count_is_left_out_until_it_is_known() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let event = Event {
            t: utc("2026-11-14T14:02:11Z"),
            kind: EventKind::Switch {
                from: None,
                to: atlas_account(),
                by: By::User,
                reason: None,
                cwd: None,
                sessions: Some(SessionCounts {
                    running: 2,
                    busy: 1,
                    tied: None,
                }),
                waited_secs: None,
            },
        };
        history.append(&event).unwrap();
        let path = dir.path().join("atlas/events-2026-11.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("\"tied\""),
            "a count step B has not measured is not written: {text}"
        );

        // A line written by an older seatswap with `"tied":0` still reads.
        let old = format!(
            "{{\"t\":\"2026-11-14T14:03:11Z\",\"type\":\"switch\",\"to\":{},\"by\":\"user\",\"sessions\":{{\"running\":2,\"busy\":1,\"tied\":0}}}}\n",
            serde_json::to_string(&atlas_account()).unwrap()
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(old.as_bytes()).unwrap();
        drop(file);

        let read = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        let tied: Vec<Option<u32>> = read
            .records
            .iter()
            .filter_map(|hosted| match &hosted.record.kind {
                EventKind::Switch {
                    sessions: Some(counts),
                    ..
                } => Some(counts.tied),
                _ => None,
            })
            .collect();
        assert_eq!(tied, vec![None, Some(0)]);
    }

    #[test]
    fn records_go_to_the_month_of_their_time() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&a_sample(utc("2026-10-31T23:59:59Z"), 10.0))
            .unwrap();
        history
            .append(&a_sample(utc("2026-11-01T00:00:00Z"), 11.0))
            .unwrap();

        let october = dir.path().join("atlas/samples-2026-10.jsonl");
        let november = dir.path().join("atlas/samples-2026-11.jsonl");
        assert_eq!(fs::read_to_string(&october).unwrap().lines().count(), 1);
        assert_eq!(fs::read_to_string(&november).unwrap().lines().count(), 1);
    }

    #[test]
    fn read_returns_only_the_range() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        for hour in 10..=12 {
            history
                .append(&a_sample(
                    utc(&format!("2026-11-14T{hour}:00:00Z")),
                    hour as f64,
                ))
                .unwrap();
        }

        let range = Range {
            from: utc("2026-11-14T10:30:00Z"),
            to: utc("2026-11-14T12:00:00Z"),
        };
        let read = history.read::<Sample>(&range, &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].record.t, utc("2026-11-14T11:00:00Z"));
    }

    fn codex_account() -> AccountRef {
        AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        }
    }

    fn google_account() -> AccountRef {
        AccountRef {
            acc: "sam@mail.example".to_string(),
            org: crate::providers::GOOGLE_ORG.to_string(),
        }
    }

    /// The one place the Claude filter lives: `read_claude` and
    /// `finished_claude_windows` keep a Claude account's records and an event
    /// that names no account, while `read` and `finished_windows` keep every
    /// line (step-d.md Task 4).
    #[test]
    fn read_claude_keeps_claude_records_and_the_account_less_event() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let at = utc("2026-11-14T10:00:00Z");
        let claude = atlas_account();
        let codex = codex_account();
        let google = google_account();
        let sample = |account: &AccountRef| Sample {
            t: at,
            account: account.clone(),
            src: Source::Poll,
            l: vec![LimitSample {
                k: LimitKind::Session,
                m: None,
                p: 12.0,
                r: None,
                b: false,
            }],
        };
        let window = |account: &AccountRef| WindowRecord {
            account: account.clone(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(at - chrono::Duration::days(7)),
            end: at,
            final_percent: 12.0,
            lost: None,
            last_sample: at,
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        };
        let blocked = |account: AccountRef| Event {
            t: at,
            kind: EventKind::Blocked {
                account,
                k: LimitKind::Session,
                m: None,
            },
        };
        history.append(&sample(&claude)).unwrap();
        history.append(&sample(&codex)).unwrap();
        history.append(&sample(&google)).unwrap();
        history.append(&window(&claude)).unwrap();
        history.append(&window(&codex)).unwrap();
        history.append(&window(&google)).unwrap();
        history.append(&blocked(claude.clone())).unwrap();
        history.append(&blocked(codex)).unwrap();
        history.append(&blocked(google)).unwrap();
        history
            .append(&Event {
                t: at,
                kind: EventKind::Live { account: None },
            })
            .unwrap();

        let samples = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(samples.records.len(), 3, "read keeps every line");
        let claude_samples = history
            .read_claude::<Sample>(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(claude_samples.records.len(), 1);
        assert_eq!(claude_samples.records[0].record.account, claude);

        let windows = history
            .finished_windows(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(
            windows.records.len(),
            3,
            "finished_windows keeps every line"
        );
        let claude_windows = history
            .finished_claude_windows(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(claude_windows.records.len(), 1);
        assert_eq!(claude_windows.records[0].record.account, claude);

        let events = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(events.records.len(), 4, "read keeps every line");
        let claude_events = history
            .read_claude::<Event>(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(
            claude_events.records.len(),
            2,
            "the Claude event and the account-less one stay; the providers' go"
        );
        assert!(claude_events.records.iter().any(|hosted| matches!(
            &hosted.record.kind,
            EventKind::Blocked { account, .. } if account == &claude
        )));
        assert!(claude_events
            .records
            .iter()
            .any(|hosted| matches!(&hosted.record.kind, EventKind::Live { account: None })));
    }

    #[test]
    fn read_merges_hosts_in_time_order() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas
            .append(&a_sample(utc("2026-11-14T10:00:00Z"), 10.0))
            .unwrap();
        atlas
            .append(&a_sample(utc("2026-11-14T12:00:00Z"), 12.0))
            .unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        vega.append(&a_sample(utc("2026-11-14T11:00:00Z"), 11.0))
            .unwrap();
        fs::write(
            dir.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .unwrap();

        let all = atlas.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        let hosts: Vec<&str> = all.records.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(hosts, vec!["atlas", "vega", "atlas"]);

        let one = atlas
            .read::<Sample>(&wide_range(), &Hosts::One("vega".to_string()))
            .unwrap();
        assert_eq!(one.records.len(), 1);
        assert_eq!(one.records[0].host, "vega");
    }

    #[test]
    fn a_torn_last_line_is_skipped_with_a_warning() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&a_sample(utc("2026-11-14T10:00:00Z"), 10.0))
            .unwrap();
        history
            .append(&a_sample(utc("2026-11-14T11:00:00Z"), 11.0))
            .unwrap();
        let path = dir.path().join("atlas/samples-2026-11.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"2026-11-14T1").unwrap();
        drop(file);

        let read = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn open_refuses_a_host_folder_of_another_machine() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("atlas")).unwrap();
        fs::write(
            root.path().join("atlas/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .unwrap();

        let settings = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(app_dir.path());
        assert!(matches!(
            HistoryDir::open(&app, &settings),
            Err(HistoryError::HostTaken { .. })
        ));
    }

    #[test]
    fn open_creates_the_marker_once() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        let settings = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(app_dir.path());

        HistoryDir::open(&app, &settings).unwrap();
        let marker = fs::read(root.path().join("atlas/host.json")).unwrap();
        let machine = fs::read(app.machine_id_file()).unwrap();

        HistoryDir::open(&app, &settings).unwrap();
        assert_eq!(
            fs::read(root.path().join("atlas/host.json")).unwrap(),
            marker
        );
        assert_eq!(fs::read(app.machine_id_file()).unwrap(), machine);
    }

    #[test]
    fn location_is_the_folder_open_uses() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        let app = AppPaths::at(app_dir.path());

        let explicit = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let (located_root, located_host) = location(&app, &explicit).unwrap();
        assert_eq!(located_root, root.path());
        assert_eq!(located_host, "atlas");
        let opened = HistoryDir::open(&app, &explicit).unwrap();
        assert_eq!(opened.root(), located_root);
        assert_eq!(opened.host(), located_host);

        let defaults = Settings::default();
        let (located_root, located_host) = location(&app, &defaults).unwrap();
        assert_eq!(located_root, app.history_dir());
        let opened = HistoryDir::open(&app, &defaults).unwrap();
        assert_eq!(opened.root(), located_root);
        assert_eq!(opened.host(), located_host);
    }

    #[test]
    fn a_range_takes_open_ends_and_refuses_a_start_that_is_not_before_its_end() {
        let open = Range::between(None, None).unwrap();
        assert_eq!(open.from, DateTime::<Utc>::MIN_UTC);
        assert_eq!(open.to, DateTime::<Utc>::MAX_UTC);

        let days = Range::between(
            Some(utc("2026-11-09T00:00:00Z")),
            Some(utc("2026-11-16T00:00:00Z")),
        )
        .unwrap();
        assert_eq!(days.from, utc("2026-11-09T00:00:00Z"));
        assert_eq!(days.to, utc("2026-11-16T00:00:00Z"));

        let error = Range::between(
            Some(utc("2026-11-17T00:00:00Z")),
            Some(utc("2026-11-16T00:00:00Z")),
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("2026-11-17T00:00:00+00:00"), "{text}");
        assert!(text.contains("2026-11-16T00:00:00+00:00"), "{text}");
    }

    #[test]
    fn append_rejects_a_record_over_the_line_limit() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let event = Event {
            t: utc("2026-11-14T14:02:11Z"),
            kind: EventKind::AgentCall {
                command: "x".repeat(5000),
                cwd: None,
                allowed: true,
            },
        };
        assert!(matches!(
            history.append(&event),
            Err(HistoryError::LineTooLong { kind: Kind::Events })
        ));
        assert!(!dir.path().join("atlas").exists());
    }

    #[test]
    fn reset_minute_rounds_up() {
        assert_eq!(
            reset_minute(utc("2026-11-14T00:59:59.59Z")),
            utc("2026-11-14T01:00:00Z")
        );
        assert_eq!(
            reset_minute(utc("2026-11-14T01:00:00Z")),
            utc("2026-11-14T01:00:00Z")
        );
    }

    #[test]
    fn same_reset_accepts_a_minute_of_jitter() {
        let reset = utc("2026-11-16T16:00:00Z");
        assert!(same_reset(reset, reset));
        assert!(same_reset(
            reset,
            reset + chrono::Duration::seconds(RESET_JITTER)
        ));
        assert!(same_reset(
            reset,
            reset - chrono::Duration::seconds(RESET_JITTER)
        ));
        assert!(!same_reset(
            reset,
            reset + chrono::Duration::seconds(RESET_JITTER + 1)
        ));
        assert!(!same_reset(
            reset,
            reset - chrono::Duration::seconds(RESET_JITTER + 1)
        ));
    }

    #[test]
    fn a_window_written_by_1_1_0_reads_without_reset_and_from() {
        let line = r#"{"acc":"6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04","org":"a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20","k":"weekly_all","start":"2026-11-09T16:00:00Z","end":"2026-11-16T16:00:00Z","final":87.0,"lost":13.0,"lastSample":"2026-11-16T15:58:10Z"}"#;
        let window: WindowRecord = serde_json::from_str(line).unwrap();
        assert!(!window.reset);
        assert_eq!(window.from, None);
        assert_eq!(serde_json::to_string(&window).unwrap(), line);

        let after_reset = WindowRecord {
            reset: true,
            from: Some(45.0),
            lost: None,
            ..window
        };
        let text = serde_json::to_string(&after_reset).unwrap();
        assert!(text.contains(r#""reset":true"#), "{text}");
        assert!(text.contains(r#""from":45.0"#), "{text}");
    }

    #[test]
    fn a_torn_line_inside_the_file_is_only_a_warning() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        for hour in 10..=12 {
            history
                .append(&a_sample(
                    utc(&format!("2026-11-14T{hour}:00:00Z")),
                    hour as f64,
                ))
                .unwrap();
        }
        let path = dir.path().join("atlas/samples-2026-11.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"2026-11-14T1").unwrap();
        drop(file);
        history
            .append(&a_sample(utc("2026-11-14T13:00:00Z"), 13.0))
            .unwrap();

        let read = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 4);
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn a_line_torn_inside_a_character_is_only_a_warning() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&a_sample(utc("2026-11-14T10:00:00Z"), 10.0))
            .unwrap();
        let path = dir.path().join("atlas/samples-2026-11.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"x\",\"cwd\":\"/home/sam/M\xc3")
            .unwrap();
        drop(file);

        let read = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn an_unknown_event_type_is_only_a_warning() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        fs::create_dir_all(dir.path().join("atlas")).unwrap();
        let lines = [
            "{\"t\":\"2026-11-14T10:00:00Z\",\"type\":\"priceDropped\",\"amount\":180.0}",
            "{\"t\":\"2026-11-14T10:00:00Z\",\"type\":\"priceDropped\",\"amount\":180.0}",
            "{\"t\":\"2026-11-14T10:00:00Z\",\"type\":\"priceDropped\",\"amount\":180.0}",
            "{\"t\":\"2026-11-14T10:00:00Z\",\"type\":\"allBlocked\"}",
        ];
        fs::write(
            dir.path().join("atlas/events-2026-11.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();

        let read = history.read::<Event>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.warnings.len(), 3);
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        fs::create_dir_all(dir.path().join("atlas")).unwrap();
        let good = serde_json::to_string(&a_sample(utc("2026-11-14T10:00:00Z"), 10.0)).unwrap();
        let mut lines = vec![good; 10];
        lines[3] = "{\"t\":]".to_string();
        lines[7] = "{\"t\":]".to_string();
        fs::write(
            dir.path().join("atlas/samples-2026-11.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();

        assert!(matches!(
            history.read::<Sample>(&wide_range(), &Hosts::All),
            Err(HistoryError::Json { line: 4, .. })
        ));
    }

    #[test]
    fn append_refuses_a_number_that_is_not_finite() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");

        let mut sample = a_sample(utc("2026-11-14T10:00:00Z"), 10.0);
        sample.l[0].p = f64::NAN;
        assert!(matches!(
            history.append(&sample),
            Err(HistoryError::NotFinite {
                kind: Kind::Samples
            })
        ));
        assert!(!dir.path().join("atlas").exists());

        let window = WindowRecord {
            account: atlas_account(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc("2026-11-09T16:00:00Z")),
            end: utc("2026-11-16T16:00:00Z"),
            final_percent: 87.0,
            lost: Some(f64::INFINITY),
            last_sample: utc("2026-11-16T15:58:10Z"),
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        };
        assert!(matches!(
            history.append(&window),
            Err(HistoryError::NotFinite {
                kind: Kind::Windows
            })
        ));
        assert!(!dir.path().join("atlas").exists());
    }

    #[test]
    fn an_unknown_limit_kind_is_a_plain_string() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let mut sample = a_sample(utc("2026-11-14T10:00:00Z"), 5.0);
        sample.l[0].k = LimitKind::Other("weekly_opus".to_string());
        history.append(&sample).unwrap();

        let text = fs::read_to_string(dir.path().join("atlas/samples-2026-11.jsonl")).unwrap();
        assert!(text.contains("\"k\":\"weekly_opus\""));

        let read = history.read::<Sample>(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].record, sample);
    }

    #[test]
    fn host_taken_says_how_to_fix_it() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("atlas")).unwrap();
        fs::write(
            root.path().join("atlas/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .unwrap();

        let settings = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(app_dir.path());
        let error = match HistoryDir::open(&app, &settings) {
            Ok(_) => panic!("the host folder belongs to another machine"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(text.contains("hostName"), "{text}");
        assert!(text.contains("host.json"), "{text}");
    }

    #[test]
    fn read_ignores_conflict_copies_and_reads_a_far_range_quickly() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&a_sample(utc("2026-11-14T10:00:00Z"), 10.0))
            .unwrap();

        let conflict_host = dir.path().join("vega (conflicted copy)");
        fs::create_dir_all(&conflict_host).unwrap();
        fs::write(
            conflict_host.join("host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .unwrap();
        fs::write(
            conflict_host.join("samples-2026-11.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&a_sample(utc("2026-11-14T11:00:00Z"), 11.0)).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            dir.path()
                .join("atlas/samples-2026-11 (conflicted copy).jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&a_sample(utc("2026-11-14T12:00:00Z"), 12.0)).unwrap()
            ),
        )
        .unwrap();

        let range = Range {
            from: utc("1970-01-01T00:00:00Z"),
            to: DateTime::<Utc>::MAX_UTC,
        };
        let read = history.read::<Sample>(&range, &Hosts::All).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].record.t, utc("2026-11-14T10:00:00Z"));
        assert!(read.warnings.is_empty());
    }

    #[test]
    fn a_host_name_of_dots_is_refused() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        assert!(matches!(
            history.read::<Sample>(&wide_range(), &Hosts::One("..".to_string())),
            Err(HistoryError::Paths(_))
        ));
    }

    #[test]
    fn a_machine_id_file_without_a_uuid_says_how_to_fix_it() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        let settings = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(app_dir.path());
        fs::write(app.machine_id_file(), "not-a-uuid\n").unwrap();

        let error = match HistoryDir::open(&app, &settings) {
            Ok(_) => panic!("the machine-id file does not hold a uuid"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(text.contains("delete"), "{text}");
        assert!(
            text.contains(&app.machine_id_file().display().to_string()),
            "{text}"
        );
    }

    #[test]
    fn the_machine_id_used_is_the_one_in_the_file() {
        let app_dir = tempdir().unwrap();
        let root = tempdir().unwrap();
        let settings = Settings {
            history_dir: Some(root.path().to_path_buf()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(app_dir.path());
        let id = "0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60";
        fs::write(app.machine_id_file(), format!("{id}\n")).unwrap();

        HistoryDir::open(&app, &settings).unwrap();
        let marker: Value =
            serde_json::from_str(&fs::read_to_string(root.path().join("atlas/host.json")).unwrap())
                .unwrap();
        assert_eq!(marker["machineId"], Value::String(id.to_string()));
    }

    #[test]
    fn sample_limit_finds_the_limit_of_a_kind() {
        let sample = a_sample(utc("2026-11-14T10:00:00Z"), 12.0);
        let session = sample.limit(&LimitKind::Session).expect("a session limit");
        assert!((session.p - 12.0).abs() < 1e-9);
        assert!(sample.limit(&LimitKind::WeeklyAll).is_none());
    }

    // -----------------------------------------------------------------------
    // The hosts of a history folder, the import and the export
    // -----------------------------------------------------------------------

    fn an_up(t: DateTime<Utc>) -> UpRecord {
        UpRecord { t }
    }

    fn a_window(end: DateTime<Utc>, percent: f64) -> WindowRecord {
        WindowRecord {
            account: atlas_account(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(end - chrono::Duration::hours(4)),
            end,
            final_percent: percent,
            lost: None,
            last_sample: end - chrono::Duration::minutes(2),
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        }
    }

    fn a_token(
        t: DateTime<Utc>,
        project: ProjectKey,
        model: &str,
        input: u64,
        out: u64,
    ) -> TokenRecord {
        TokenRecord {
            t,
            minutes: 60,
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project,
            model: model.to_string(),
            input,
            out,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        }
    }

    #[test]
    fn a_line_without_min_is_an_hour() {
        let line = serde_json::to_value(a_token(
            utc("2026-11-11T11:00:00Z"),
            atlas_project(),
            "claude-opus-5-5",
            1_204,
            3_877,
        ))
        .unwrap();
        assert!(line.get("min").is_none(), "{line}");
        let hourly: TokenRecord = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(hourly.span(), chrono::Duration::minutes(60));

        let mut five_minutes = line;
        five_minutes["min"] = Value::from(5);
        let five: TokenRecord = serde_json::from_value(five_minutes).unwrap();
        assert_eq!(five.span(), chrono::Duration::minutes(5));
    }

    #[test]
    fn a_line_with_min_0_is_an_error() {
        let mut line = serde_json::to_value(a_token(
            utc("2026-11-11T11:00:00Z"),
            atlas_project(),
            "claude-opus-5-5",
            1_204,
            3_877,
        ))
        .unwrap();
        line["min"] = Value::from(0);
        let error = serde_json::from_value::<TokenRecord>(line)
            .expect_err("a line of no minutes cannot last")
            .to_string();
        assert!(error.contains("min"), "{error}");
        assert!(error.contains('0'), "{error}");
    }

    /// A host folder a reader recognizes: a `host.json` marker and no records.
    fn mark_host(root: &std::path::Path, host: &str) {
        mark_host_with(root, host, "0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60");
    }

    fn mark_host_with(root: &std::path::Path, host: &str, machine_id: &str) {
        fs::create_dir_all(root.join(host)).unwrap();
        fs::write(
            root.join(host).join("host.json"),
            format!("{{\"machineId\":\"{machine_id}\"}}"),
        )
        .unwrap();
    }

    fn atlas_project() -> ProjectKey {
        ProjectKey::Repo("github.com/sam/atlas".to_string())
    }

    /// Every file of one folder with its bytes, by name: two runs are identical
    /// only when nothing in the folder changed.
    fn folder_files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.metadata().unwrap().is_file())
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn host_infos_list_every_host_with_its_last_record() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas.append(&an_up(utc("2026-11-14T09:58:00Z"))).unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        mark_host(dir.path(), "vega");

        let infos = atlas.host_infos().unwrap();
        let hosts: Vec<&str> = infos.iter().map(|info| info.host.as_str()).collect();
        assert_eq!(hosts, vec!["atlas", "vega"]);
        assert!(infos[0].this_host, "atlas is this host");
        assert!(!infos[1].this_host, "vega is another host");

        // The plan's fixture: each host's only `up` line is its newest record,
        // and also the first record of its oldest file.
        assert_eq!(infos[0].last_record, Some(utc("2026-11-14T09:58:00Z")));
        assert_eq!(infos[1].last_record, Some(utc("2026-11-14T07:00:00Z")));
        assert_eq!(infos[0].first_record, Some(utc("2026-11-14T09:58:00Z")));
        assert_eq!(infos[1].first_record, Some(utc("2026-11-14T07:00:00Z")));

        // Every file of the folder counts, the marker too.
        for info in &infos {
            assert!(info.bytes > 0, "{info:?}");
        }
        let sizes: u64 = ["up-2026-11.jsonl", "samples-2026-11.jsonl", "host.json"]
            .iter()
            .map(|name| {
                fs::metadata(dir.path().join("atlas").join(name))
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(infos[0].bytes, sizes);
    }

    #[test]
    fn a_hosts_size_leaves_out_its_report() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas.append(&an_up(utc("2026-11-14T09:58:00Z"))).unwrap();
        let report = atlas.sessions_now_file("atlas");
        fs::write(&report, "{\"at\":\"2026-11-14T09:58:00Z\",\"sessions\":[]}").unwrap();
        assert!(
            fs::metadata(&report).unwrap().len() > 0,
            "the report is there and has bytes"
        );

        let infos = atlas.host_infos().unwrap();
        let sizes: u64 = ["up-2026-11.jsonl", "samples-2026-11.jsonl", "host.json"]
            .iter()
            .map(|name| {
                fs::metadata(dir.path().join("atlas").join(name))
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(
            infos[0].bytes, sizes,
            "the live report is not history and does not count"
        );
    }

    #[test]
    fn host_infos_carry_this_host_before_it_has_a_folder() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");

        let infos = history.host_infos().unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].host, "atlas");
        assert!(infos[0].this_host);
        assert_eq!(infos[0].first_record, None);
        assert_eq!(infos[0].last_record, None);
        assert_eq!(infos[0].bytes, 0);
    }

    /// The hosts table shows the first and the last record of a host and the
    /// size of its folder. It must not read every record of every kind of every
    /// host to do that: a file it does not need must not decide whether the
    /// table is there at all. Two broken lines among a hundred are more than
    /// 1 % of the file, so the reader calls that file an error; the table does
    /// not read the file.
    #[test]
    fn host_infos_does_not_read_the_records_it_does_not_need() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas.append(&an_up(utc("2026-11-14T09:58:00Z"))).unwrap();
        let mut lines = String::new();
        for index in 0..100 {
            if index == 40 || index == 41 {
                lines.push_str("this is not a record\n");
            } else {
                lines.push_str("{\"t\":\"2026-11-14T09:00:00Z\"}\n");
            }
        }
        fs::write(
            dir.path().join("atlas").join("tokens-2026-11.jsonl"),
            &lines,
        )
        .unwrap();

        let infos = atlas
            .host_infos()
            .expect("the hosts table is there even when a file it does not need is broken");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].host, "atlas");
        assert_eq!(infos[0].last_record, Some(utc("2026-11-14T09:58:00Z")));
    }

    #[test]
    fn import_copies_another_host_and_never_the_own() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");
        own.append(&an_up(utc("2026-11-14T09:58:00Z"))).unwrap();
        mark_host(here.path(), "atlas");
        let before = folder_files(&here.path().join("atlas"));

        // The other folder holds this host's name too, with another line.
        let other = HistoryDir::at(there.path(), "atlas");
        other.append(&an_up(utc("2026-11-14T02:00:00Z"))).unwrap();
        mark_host(there.path(), "atlas");
        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        mark_host(there.path(), "vega");

        let imported = own.import(there.path()).unwrap();
        assert_eq!(imported.copied, vec!["vega"]);
        assert!(imported.conflicts.is_empty());
        assert_eq!(
            folder_files(&here.path().join("atlas")),
            before,
            "this host's own folder is never touched"
        );
        assert!(here.path().join("vega/up-2026-11.jsonl").is_file());
        assert!(here.path().join("vega/host.json").is_file());
        assert_eq!(
            own.host_infos()
                .unwrap()
                .iter()
                .map(|info| info.host.clone())
                .collect::<Vec<_>>(),
            vec!["atlas", "vega"]
        );

        // A second run copies nothing more.
        assert!(own.import(there.path()).unwrap().copied.is_empty());
    }

    #[test]
    fn sessions_now_file_is_in_the_hosts_folder() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        assert_eq!(
            history.sessions_now_file("vega"),
            dir.path().join("vega").join("sessions-now.json")
        );
    }

    #[test]
    fn the_import_does_not_copy_a_hosts_report() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");
        mark_host(here.path(), "atlas");

        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        mark_host(there.path(), "vega");
        fs::write(
            vega.sessions_now_file("vega"),
            "{\"at\":\"2026-11-14T07:00:00Z\",\"sessions\":[]}",
        )
        .unwrap();

        let imported = own.import(there.path()).unwrap();
        assert_eq!(imported.copied, vec!["vega"]);
        assert!(imported.conflicts.is_empty());
        assert!(here.path().join("vega/up-2026-11.jsonl").is_file());
        assert!(
            !here.path().join("vega/sessions-now.json").exists(),
            "one host's live state is not a history record and is never copied"
        );
    }

    #[test]
    fn import_takes_the_longer_file() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");
        mark_host(there.path(), "vega");

        let two = "{\"t\":\"2026-11-14T07:00:00Z\"}\n{\"t\":\"2026-11-14T08:00:00Z\"}\n";
        let five = "{\"t\":\"2026-11-14T07:00:00Z\"}\n{\"t\":\"2026-11-14T08:00:00Z\"}\n\
                    {\"t\":\"2026-11-14T09:00:00Z\"}\n{\"t\":\"2026-11-14T10:00:00Z\"}\n\
                    {\"t\":\"2026-11-14T11:00:00Z\"}\n";
        fs::create_dir_all(here.path().join("vega")).unwrap();
        // Both sides say this is this machine, so the longer file may replace
        // the shorter one; a folder without a marker would be a conflict.
        mark_host(here.path(), "vega");
        fs::create_dir_all(there.path().join("vega")).unwrap();
        fs::write(here.path().join("vega/up-2026-11.jsonl"), two).unwrap();
        fs::write(there.path().join("vega/up-2026-11.jsonl"), five).unwrap();

        assert_eq!(own.import(there.path()).unwrap().copied, vec!["vega"]);
        assert_eq!(
            fs::read_to_string(here.path().join("vega/up-2026-11.jsonl")).unwrap(),
            five
        );

        // The other way round: the longer file here stays.
        fs::write(there.path().join("vega/up-2026-11.jsonl"), two).unwrap();
        assert!(own.import(there.path()).unwrap().copied.is_empty());
        assert_eq!(
            fs::read_to_string(here.path().join("vega/up-2026-11.jsonl")).unwrap(),
            five
        );
    }

    #[test]
    fn import_refuses_this_folder() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        history.append(&an_up(utc("2026-11-14T09:58:00Z"))).unwrap();
        mark_host(dir.path(), "atlas");

        let error = history
            .import(dir.path())
            .expect_err("the same folder cannot be imported");
        assert!(
            error.to_string().contains("that is this history folder"),
            "{error}"
        );

        // A folder with no host folder in it says so.
        let empty = tempdir().unwrap();
        fs::create_dir_all(empty.path().join("not a host")).unwrap();
        let error = history.import(empty.path()).expect_err("nothing to import");
        assert!(
            error.to_string().contains(&format!(
                "no host folder found in {}",
                empty.path().display()
            )),
            "{error}"
        );
    }

    #[test]
    fn import_refuses_a_host_of_another_machine() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");

        // The `vega` folder here belongs to this machine; the source's `vega`
        // belongs to another one.
        mark_host_with(here.path(), "vega", "0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60");
        let before = folder_files(&here.path().join("vega"));
        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        mark_host_with(there.path(), "vega", "5d8a1f30-2c7b-4e91-b6a4-9f0c3d7e2a15");
        // Another host is copied before the conflict is reached.
        let ledger = HistoryDir::at(there.path(), "ledger");
        ledger.append(&an_up(utc("2026-11-14T06:00:00Z"))).unwrap();
        mark_host(there.path(), "ledger");

        let imported = own
            .import(there.path())
            .expect("a conflict is reported, not an error");
        assert_eq!(imported.copied, vec!["ledger"]);
        assert_eq!(imported.conflicts.len(), 1);
        assert_eq!(imported.conflicts[0].host, "vega");
        assert_eq!(
            imported.conflicts[0].reason,
            "two machines use the host name vega; give one of them another name under History and hosts"
        );
        assert_eq!(
            folder_files(&here.path().join("vega")),
            before,
            "nothing of the other machine's vega is copied"
        );
        assert!(
            here.path().join("ledger/up-2026-11.jsonl").is_file(),
            "the other hosts are still copied"
        );
    }

    #[test]
    fn import_does_not_merge_a_host_without_a_marker() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");

        // The `vega` folder is here already, with a line of this machine, but
        // without a marker: nothing says that the source's vega is the same one.
        fs::create_dir_all(here.path().join("vega")).unwrap();
        let mine = "{\"t\":\"2026-11-14T06:00:00Z\"}\n";
        fs::write(here.path().join("vega/up-2026-11.jsonl"), mine).unwrap();
        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        mark_host(there.path(), "vega");

        let imported = own.import(there.path()).unwrap();
        assert!(imported.copied.is_empty());
        assert_eq!(imported.conflicts.len(), 1);
        assert_eq!(imported.conflicts[0].host, "vega");
        assert_eq!(
            imported.conflicts[0].reason,
            format!(
                "cannot tell whether vega here and in {} are the same machine: {} has no host.json",
                there.path().display(),
                here.path().join("vega").display()
            )
        );
        assert_eq!(
            fs::read_to_string(here.path().join("vega/up-2026-11.jsonl")).unwrap(),
            mine,
            "the other machine's vega is never merged in"
        );

        // The other way round: this side has the marker and the source has none.
        fs::write(
            here.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .unwrap();
        fs::remove_file(there.path().join("vega/host.json")).unwrap();
        let imported = own.import(there.path()).unwrap();
        assert!(imported.copied.is_empty());
        assert_eq!(imported.conflicts.len(), 1);
        assert_eq!(
            imported.conflicts[0].reason,
            format!(
                "cannot tell whether vega here and in {} are the same machine: {} has no host.json",
                there.path().display(),
                there.path().join("vega").display()
            )
        );
    }

    #[test]
    fn proof_import_does_not_merge_two_host_folders_with_an_empty_machine_id() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");

        // Both sides carry a marker, but neither names a machine: an empty id
        // proves nothing about which machine wrote the folder.
        fs::create_dir_all(here.path().join("vega")).unwrap();
        fs::write(here.path().join("vega/host.json"), "{\"machineId\":\"\"}").unwrap();
        // One byte shorter than the other side's line, so "the longer file
        // wins" would replace it.
        let mine = "{\"t\":\"2026-11-14T06:00:00Z\"}";
        fs::write(here.path().join("vega/up-2026-11.jsonl"), mine).unwrap();

        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        fs::write(there.path().join("vega/host.json"), "{\"machineId\":\"\"}").unwrap();

        let imported = own.import(there.path()).unwrap();
        assert!(
            imported.copied.is_empty(),
            "an empty machine id proves nothing: {:?}",
            imported.copied
        );
        assert_eq!(imported.conflicts.len(), 1, "the host is a conflict");
        assert_eq!(imported.conflicts[0].host, "vega");
        assert_eq!(
            fs::read_to_string(here.path().join("vega/up-2026-11.jsonl")).unwrap(),
            mine,
            "the other machine's vega is never merged in"
        );
    }

    #[test]
    fn import_does_not_copy_a_host_without_a_machine_id() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");

        // vega is not here yet, and the source's vega has no marker: a copy
        // would never be listed by `hosts`, so its files would never be read.
        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();

        let imported = own.import(there.path()).unwrap();
        assert!(imported.copied.is_empty(), "{:?}", imported.copied);
        assert_eq!(imported.conflicts.len(), 1);
        assert_eq!(imported.conflicts[0].host, "vega");
        assert!(
            imported.conflicts[0]
                .reason
                .contains("has no host.json with a machine id"),
            "{}",
            imported.conflicts[0].reason
        );
        assert!(!here.path().join("vega").exists(), "nothing is copied");
    }

    #[test]
    fn import_matches_a_marker_with_a_trailing_newline() {
        let here = tempdir().unwrap();
        let there = tempdir().unwrap();
        let own = HistoryDir::at(here.path(), "atlas");
        let id = "0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60";
        mark_host_with(here.path(), "vega", id);
        let vega = HistoryDir::at(there.path(), "vega");
        vega.append(&an_up(utc("2026-11-14T07:00:00Z"))).unwrap();
        // The source's marker holds the same id with a newline at its end.
        fs::create_dir_all(there.path().join("vega")).unwrap();
        fs::write(
            there.path().join("vega/host.json"),
            format!("{{\"machineId\":\"{id}\\n\"}}"),
        )
        .unwrap();

        let imported = own
            .import(there.path())
            .expect("the same machine id, with whitespace");
        assert_eq!(imported.copied, vec!["vega"]);
        assert!(imported.conflicts.is_empty());
        assert!(here.path().join("vega/up-2026-11.jsonl").is_file());
    }

    #[test]
    fn import_says_when_the_source_folder_is_not_there() {
        let dir = tempdir().unwrap();
        let history = HistoryDir::at(dir.path(), "atlas");
        let missing = dir.path().join("no-such-folder");

        let error = history
            .import(&missing)
            .expect_err("the folder is not there");
        assert_eq!(
            error.to_string(),
            format!("{} does not exist", missing.display())
        );
    }

    #[test]
    fn a_window_of_two_hosts_is_read_once_with_the_latest_last_sample() {
        let dir = tempdir().unwrap();
        let end = utc("2026-11-16T16:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let mut from_atlas = a_window(end, 61.0);
        from_atlas.last_sample = utc("2026-11-16T09:58:00Z");
        atlas.append(&from_atlas).unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        let mut from_vega = a_window(end, 63.0);
        from_vega.last_sample = utc("2026-11-16T10:02:00Z");
        vega.append(&from_vega).unwrap();
        mark_host(dir.path(), "vega");

        let windows = atlas.finished_windows(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(windows.records.len(), 1);
        assert_eq!(windows.records[0].host, "vega");
        assert_eq!(windows.records[0].record.final_percent, 63.0);
    }

    #[test]
    fn two_models_ending_together_read_as_two_windows() {
        let dir = tempdir().unwrap();
        let end = utc("2026-11-16T16:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        for (model, percent) in [("Fable", 61.0), ("Sonnet", 40.0)] {
            let mut window = a_window(end, percent);
            window.k = LimitKind::WeeklyScoped;
            window.m = Some(model.to_string());
            atlas.append(&window).unwrap();
        }

        let windows = atlas.finished_windows(&wide_range(), &Hosts::All).unwrap();
        let models: Vec<Option<&str>> = windows
            .records
            .iter()
            .map(|hosted| hosted.record.m.as_deref())
            .collect();
        assert_eq!(windows.records.len(), 2, "{models:?}");
        assert!(models.contains(&Some("Fable")));
        assert!(models.contains(&Some("Sonnet")));
    }

    #[test]
    fn two_resets_of_one_host_stay_two_windows() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        for (last_sample, end) in [
            ("2026-11-17T12:00:00Z", "2026-11-18T08:30:00Z"),
            ("2026-11-18T08:30:00Z", "2026-11-18T14:00:00Z"),
        ] {
            let mut window = a_window(utc(end), 50.0);
            window.last_sample = utc(last_sample);
            window.reset = true;
            atlas.append(&window).unwrap();
        }

        let windows = atlas.finished_windows(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(windows.records.len(), 2);
    }

    #[test]
    fn a_window_of_two_hosts_on_a_tie_uses_the_first_host_name() {
        let dir = tempdir().unwrap();
        let end = utc("2026-11-16T16:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let mut from_atlas = a_window(end, 61.0);
        from_atlas.last_sample = utc("2026-11-16T10:00:00Z");
        atlas.append(&from_atlas).unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        let mut from_vega = a_window(end, 63.0);
        from_vega.last_sample = utc("2026-11-16T10:00:00Z");
        vega.append(&from_vega).unwrap();
        mark_host(dir.path(), "vega");

        let windows = atlas.finished_windows(&wide_range(), &Hosts::All).unwrap();
        assert_eq!(windows.records.len(), 1);
        assert_eq!(windows.records[0].host, "atlas");
        assert_eq!(windows.records[0].record.final_percent, 61.0);
    }

    #[test]
    fn export_writes_a_window_of_two_hosts_once() {
        let dir = tempdir().unwrap();
        let end = utc("2026-11-16T16:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let mut from_atlas = a_window(end, 61.0);
        from_atlas.last_sample = utc("2026-11-16T09:58:00Z");
        atlas.append(&from_atlas).unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        let mut from_vega = a_window(end, 63.0);
        from_vega.last_sample = utc("2026-11-16T10:02:00Z");
        vega.append(&from_vega).unwrap();
        mark_host(dir.path(), "vega");

        let mut csv = Vec::new();
        export_csv(&atlas, &wide_range(), &mut csv).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        let windows: Vec<&str> = csv
            .lines()
            .filter(|line| line.starts_with("window,"))
            .collect();
        assert_eq!(windows.len(), 1, "{csv}");
        assert!(windows[0].contains(",63,"), "{csv}");

        let mut json = Vec::new();
        export_json(&atlas, &wide_range(), &mut json).unwrap();
        let json: Value = serde_json::from_slice(&json).unwrap();
        let windows = json["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 1, "{json}");
        assert_eq!(windows[0]["percent"], 63.0);
    }

    #[test]
    fn export_csv_has_a_header_and_one_line_per_window() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas
            .append(&a_window(utc("2026-11-14T19:30:00Z"), 87.0))
            .unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        vega.append(&a_window(utc("2026-11-15T02:00:00Z"), 64.5))
            .unwrap();
        mark_host(dir.path(), "vega");

        let mut out = Vec::new();
        export_csv(&atlas, &wide_range(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "kind,account,org,limit,model,start,end,percent,day_utc,project,input,output,cache_write,cache_read"
        );
        let windows: Vec<&&str> = lines[1..]
            .iter()
            .filter(|line| line.starts_with("window,"))
            .collect();
        assert_eq!(windows.len(), 2, "{text}");
        let first: Vec<&str> = windows[0].split(',').collect();
        assert_eq!(first.len(), 14);
        assert_eq!(first[1], atlas_account().acc);
        assert_eq!(first[2], atlas_account().org);
        assert_eq!(first[3], "weekly_all");
        assert_eq!(first[4], "", "a window without a model");
        assert_eq!(first[5], "2026-11-14T15:30:00Z");
        assert_eq!(first[6], "2026-11-14T19:30:00Z");
        assert_eq!(first[7], "87");
        assert_eq!(&first[8..], ["", "", "", "", "", ""]);
        assert!(windows[1].contains(",64.5,"), "{text}");
    }

    #[test]
    fn export_json_has_the_same_rows() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas
            .append(&a_window(utc("2026-11-14T19:30:00Z"), 87.0))
            .unwrap();
        atlas
            .append(&a_token(
                utc("2026-11-14T09:00:00Z"),
                atlas_project(),
                "claude-opus-5-5",
                1_284_000,
                96_500,
            ))
            .unwrap();
        atlas
            .append(&a_token(
                utc("2026-11-14T18:20:00Z"),
                atlas_project(),
                "claude-opus-5-5",
                731_000,
                41_200,
            ))
            .unwrap();
        let vega = HistoryDir::at(dir.path(), "vega");
        vega.append(&a_window(utc("2026-11-15T02:00:00Z"), 64.5))
            .unwrap();
        // A window whose record holds neither a model nor a start: both are
        // still in the JSON, as `null`.
        let mut bare = a_window(utc("2026-11-16T03:00:00Z"), 12.0);
        bare.start = None;
        atlas.append(&bare).unwrap();
        vega.append(&a_token(
            utc("2026-11-14T20:10:00Z"),
            atlas_project(),
            "claude-opus-5-5",
            405_000,
            12_400,
        ))
        .unwrap();
        vega.append(&a_token(
            utc("2026-11-15T08:00:00Z"),
            ProjectKey::Folder("ledger".to_string()),
            "claude-opus-5-5",
            88_000,
            3_100,
        ))
        .unwrap();
        mark_host(dir.path(), "vega");

        let mut csv = Vec::new();
        export_csv(&atlas, &wide_range(), &mut csv).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        let mut json = Vec::new();
        export_json(&atlas, &wide_range(), &mut json).unwrap();
        let json: Value = serde_json::from_slice(&json).unwrap();

        // The same number of rows in both formats.
        let window_lines = csv
            .lines()
            .filter(|line| line.starts_with("window,"))
            .count();
        let token_lines = csv
            .lines()
            .filter(|line| line.starts_with("tokens,"))
            .count();
        assert_eq!(json["windows"].as_array().unwrap().len(), window_lines);
        assert_eq!(json["dailyTokens"].as_array().unwrap().len(), token_lines);

        // Two lines of one day and project on different hosts are one row; a
        // third day and folder is a second one.
        assert_eq!(token_lines, 2, "{csv}");
        let days = json["dailyTokens"].as_array().unwrap();
        assert_eq!(days[0]["dayUtc"], "2026-11-14");
        assert_eq!(days[0]["project"], "repo:github.com/sam/atlas");
        assert_eq!(days[0]["input"], 1_284_000 + 731_000 + 405_000);
        assert_eq!(days[0]["output"], 96_500 + 41_200 + 12_400);
        assert_eq!(days[0]["cacheWrite"], 0);
        assert_eq!(days[0]["cacheRead"], 0);
        assert_eq!(days[1]["dayUtc"], "2026-11-15");
        assert_eq!(days[1]["project"], "folder:ledger");
        assert_eq!(days[1]["input"], 88_000);

        // The window rows carry the same window ends and percentages, and the
        // JSON holds every column: an empty model or start is `null`, not left
        // out.
        let windows = json["windows"].as_array().unwrap();
        assert_eq!(windows[0].as_object().unwrap().len(), 8);
        assert_eq!(windows[0]["account"], atlas_account().acc);
        assert_eq!(windows[0]["limit"], "weekly_all");
        assert_eq!(windows[0]["percent"], 87.0);
        assert!(windows[0].get("model").is_some());
        assert!(windows[0]["model"].is_null());
        assert!(windows[0].get("start").is_some());
        let first: Vec<&str> = csv
            .lines()
            .find(|line| line.starts_with("window,"))
            .unwrap()
            .split(',')
            .collect();
        assert_eq!(windows[0]["end"], first[6]);
        assert_eq!(windows[0]["start"], first[5]);
        assert_eq!(windows[1]["percent"], 64.5);
        assert!(windows[2].get("model").is_some());
        assert!(windows[2]["model"].is_null());
        assert!(windows[2].get("start").is_some());
        assert!(windows[2]["start"].is_null());
    }

    #[test]
    fn export_csv_quotes_a_field_with_a_comma_or_a_quote() {
        let dir = tempdir().unwrap();
        let atlas = HistoryDir::at(dir.path(), "atlas");
        atlas
            .append(&a_token(
                utc("2026-11-14T09:00:00Z"),
                ProjectKey::Folder("atlas, \"old\"".to_string()),
                "claude-opus-5-5",
                10,
                2,
            ))
            .unwrap();
        let mut out = Vec::new();
        export_csv(&atlas, &wide_range(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(",\"folder:atlas, \"\"old\"\"\","),
            "a folder name with a comma and quotes is quoted:\n{text}"
        );
    }
}
