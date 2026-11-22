//! Claude Code's transcripts: reading them incrementally, and what they say
//! about answers, user turns and agents (spec §1.6).
//!
//! Only numbers and names leave the parser: time, folder, model, block kinds,
//! token counts, session and agent ids. Message text is never stored, logged,
//! put into a warning or sent anywhere.
//!
//! The state is kept as one file per project folder in
//! [`AppPaths::transcripts_state_dir`], because one machine had 7,908
//! transcripts in 37 folders: a single file would be megabytes, rewritten at
//! every wake.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Timelike, Utc};
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::atomic::{self, Access};
use crate::history::TokenRecord;
use crate::paths::{AppPaths, ClaudePaths};
use crate::project::{self, ProjectKey};

/// The version of the state files this module writes and reads.
///
/// Version 2 keeps the last hour's tokens per model. A version-1 file is
/// migrated: its offsets are kept, so no byte already read is recorded again.
/// A file of any other version is an error, never a silent rebuild.
const STATE_VERSION: u32 = 2;
/// The version 1.1.0 wrote, and the one this module migrates.
const STATE_VERSION_1: u32 = 1;
/// How many message ids a growing transcript keeps, so that an answer is
/// counted once even when its lines arrive far apart.
const RECENT_IDS: usize = 500;
/// A transcript that has not grown for this long drops its ids; it is read to
/// its end then, so nothing can be counted twice.
const KEEP_IDS_FOR_HOURS: i64 = 24;
/// An answer of this model never reached the API: no tokens to count.
const SYNTHETIC: &str = "<synthetic>";
/// How much of a transcript is read at a time.
const CHUNK_BYTES: usize = 64 * 1024;
/// The UTF-8 byte order mark, which some writers put at offset 0 of a file.
const BYTE_ORDER_MARK: [u8; 3] = [0xEF, 0xBB, 0xBF];
/// The two block kinds that mean the answer thought before it spoke.
const THINKING_BLOCKS: [&str; 2] = ["thinking", "redacted_thinking"];
/// The `flash-agents` MCP tools whose tool results name flash jobs.
const FLASH_TOOLS: [&str; 4] = [
    "mcp__plugin_flash-agents_flash__flash_task",
    "mcp__plugin_flash-agents_flash__flash_result",
    "mcp__plugin_flash-agents_flash__flash_batch",
    "mcp__plugin_flash-agents_flash__flash_status",
];
/// The state file that remembers which Harness session tokens were already
/// written to the history, beside the per-folder state files.
const HARNESS_STATE_FILE: &str = "harness.json";
/// The state file that remembers which Codex rollout totals were already
/// written to the history, beside the Harness state file.
const CODEX_STATE_FILE: &str = "codex.json";

/// `file` below `root`, so a saved total finds the file again when the store
/// moves; a file outside `root` is kept as it is. The one relative-path helper
/// of the Harness and Codex stores.
pub(crate) fn file_below(root: &Path, file: &Path) -> PathBuf {
    match file.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => file.to_path_buf(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {source}")]
    State {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A state file of a version this module cannot read. Rebuilding would read
    /// every transcript again and write its records a second time, so this is an
    /// error, not a cache miss.
    #[error(
        "{path}: state version {found} is not supported; this seatswap reads version 1 and version {current}{newer}"
    )]
    UnsupportedStateVersion {
        path: PathBuf,
        found: u32,
        current: u32,
        /// `" (the file was written by a newer seatswap)"` for a higher version.
        newer: &'static str,
    },
}

/// One answer of a transcript, counted once by its message id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub session: String,
    pub agent: Option<String>,
    pub message_id: String,
    pub at: DateTime<Utc>,
    pub cwd: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub thinking: bool,
}

/// A real user message of a main transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurn {
    pub session: String,
    pub at: DateTime<Utc>,
}

/// The four token counts of some answers: input, output, cache write, cache read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input: u64,
    pub out: u64,
    pub cw: u64,
    pub cr: u64,
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, other: Tokens) {
        self.input += other.input;
        self.out += other.out;
        self.cw += other.cw;
        self.cr += other.cr;
    }
}

/// What one Harness session already has in the history: its token totals and
/// the number of assistant messages those totals cover. Both advance together,
/// so a later pass writes only the growth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRecorded {
    pub tokens: Tokens,
    pub messages: u32,
    /// The session's file below the Harness sessions folder. A later save drops
    /// the total only when this file no longer exists; absent in an old state
    /// file means the file is not known, so the total is kept.
    #[serde(default)]
    pub file: Option<PathBuf>,
}

/// What one Codex rollout already has in the history: its newest
/// `info.total_token_usage`, mapped as [`Tokens`] maps Codex's fields, the time
/// of that `token_count` line, and the rollout file below Codex's sessions
/// folder. A later read writes only the growth after `last_at`; a total that is
/// equal records nothing and a total that went down is a warning, never a
/// negative line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRecorded {
    pub total: Tokens,
    pub last_at: DateTime<Utc>,
    /// The rollout file below `<codex home>/sessions`; an absolute path from an
    /// old or hand-written state file is used as it is.
    pub file: PathBuf,
}

/// A flash job link kept after its Claude Code session left the scan state: the
/// job id, the session that named it and that session's `last`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedJob {
    pub session: String,
    pub last: DateTime<Utc>,
}

/// The four counts a history [`TokenRecord`] holds.
impl From<&TokenRecord> for Tokens {
    fn from(record: &TokenRecord) -> Tokens {
        Tokens {
            input: record.input,
            out: record.out,
            cw: record.cw,
            cr: record.cr,
        }
    }
}

/// One `agent-<id>.meta.json` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMeta {
    pub session: String,
    pub agent: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub finished: bool,
    /// The nesting the meta names; `None` when the meta file could not be read.
    pub spawn_depth: Option<u32>,
    /// The `toolUseId` of the call that started the agent.
    pub tool_use_id: Option<String>,
    /// The agent that made the call; `None` when the session made it.
    pub parent: Option<String>,
    /// Whether [`agent_ended`] says the agent's own transcript ends with an
    /// assistant entry whose `stop_reason` is `"end_turn"`. Read for every
    /// listed agent whose meta does not say `finished`, whether or not its meta
    /// file could be read; an agent marked `finished` keeps `false` here
    /// without its transcript being read, so read the done state only through
    /// [`AgentMeta::is_done`].
    pub transcript_ended: bool,
}

impl AgentMeta {
    /// Whether the agent is done under the one rule the Sessions row and the
    /// session record both use: Claude Code marked it `finished`, or its own
    /// transcript ends with the agent's final `end_turn` answer.
    pub fn is_done(&self) -> bool {
        self.finished || self.transcript_ended
    }
}

/// How far a transcript was read, and the ids of the answers it gave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileState {
    /// The offset just after the last whole line that was read.
    pub offset: u64,
    /// The file's modification time when the scan last read new bytes from it.
    pub grew: DateTime<Utc>,
    /// The last [`RECENT_IDS`] message ids, while the file still grows.
    pub recent: Vec<String>,
    /// The time of the newest answer this file has given; absent in a state file
    /// written before this field means `None`.
    pub last_answer: Option<DateTime<Utc>>,
    /// The `tool_use` ids of flash-agents calls in this file, so their tool
    /// results are recognised also when they arrive in a later scan. Absent in
    /// an old state file means none.
    #[serde(default)]
    pub flash_tools: BTreeSet<String>,
    /// A title this file's `ai-title` line gave while its session had no
    /// answer or user turn yet; it reaches the session in the scan that first
    /// sees one. Absent means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The same for a `custom-title` line (the user's `/rename`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
}

/// What a main transcript says about its session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSeen {
    /// The name of the project folder the transcript is in.
    pub folder: String,
    pub cwd: String,
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub last_model: Option<String>,
    pub last_user_turn: Option<DateTime<Utc>>,
    /// The answers with thinking since the last user turn: per minute (its
    /// start in UTC) and oldest first. A user turn empties the list, so every
    /// reader of [`Self::thinking_since_user`] sees the current task's count.
    #[serde(default)]
    pub thinking: Vec<(DateTime<Utc>, u32)>,
    /// The tokens of the session's answers and of its agents'. A state file
    /// written before this field loads with zero: the sums then start at the
    /// answers read from that point on, and the old transcripts are not read
    /// again for them.
    #[serde(default)]
    pub totals: Tokens,
    /// [`Self::totals`], per model.
    #[serde(default)]
    pub by_model: BTreeMap<String, Tokens>,
    /// What the session's agents did, per agent id.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSeen>,
    /// The `tool_use` id of an `Agent` or `Task` call, and the agent that made
    /// it: `None` when the call is in the main transcript.
    #[serde(default)]
    pub spawns: BTreeMap<String, Option<String>>,
    /// The answers of the last 60 minutes: one entry per minute (its start in
    /// UTC) with the minute's tokens per model, oldest first. The key is the
    /// answer's model string, exactly as [`Self::by_model`] keys it.
    #[serde(default)]
    pub recent: Vec<(DateTime<Utc>, BTreeMap<String, Tokens>)>,
    /// The session record last written from this session, so a restart does not
    /// write the same record again. Absent in an old state file means none.
    #[serde(default)]
    pub recorded: Option<RecordedSession>,
    /// The flash job ids named in this session's tool results: every
    /// `flash-task-<uuid>` and every `job-<8 hex>` id. A `job-…` id is resolved
    /// through the job reports when the rows are built. Absent in an old state
    /// file means none.
    #[serde(default)]
    pub flash_jobs: BTreeSet<String>,
    /// The `aiTitle` of the last `ai-title` line of the session's own
    /// transcript, as Claude Code wrote it; absent in a state file written
    /// before this field means none, and no transcript is read again for it.
    #[serde(default)]
    pub title: Option<String>,
    /// The `customTitle` of the last `custom-title` line of the session's own
    /// transcript: the name the user gave it with `/rename`. Absent in a state
    /// file written before this field means none (1.4.4).
    #[serde(default)]
    pub custom_title: Option<String>,
}

impl SessionSeen {
    /// The session's title as Claude Code shows it: the user's own name
    /// (`custom-title`) when it has one, else Claude Code's `ai-title`.
    pub fn shown_title(&self) -> Option<&String> {
        self.custom_title.as_ref().or(self.title.as_ref())
    }

    /// The answers with thinking since the session's last user turn: the sum of
    /// [`Self::thinking`], the one count every reader uses.
    pub fn thinking_since_user(&self) -> u32 {
        self.thinking
            .iter()
            .fold(0u32, |sum, (_, count)| sum.saturating_add(*count))
    }
}

/// What one agent of a session did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSeen {
    /// The agent's first answer.
    pub first: DateTime<Utc>,
    /// The agent's last answer.
    pub last: DateTime<Utc>,
    /// The model of its last answer.
    pub model: Option<String>,
    pub tokens: Tokens,
    pub answers: u32,
}

/// The session record last written from a [`SessionSeen`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedSession {
    /// The `last` answer time the record covers.
    pub last: DateTime<Utc>,
    /// Whether that record was the closing one (`closed: true`).
    pub closed: bool,
}

/// Everything remembered between two scans.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanState {
    pub version: u32,
    /// Keyed by the path below `projects_dir` with `/`: `<folder>/<session>.jsonl`
    /// or `<folder>/<session>/subagents/agent-<id>.jsonl`.
    pub files: BTreeMap<String, FileState>,
    pub sessions: BTreeMap<String, SessionSeen>,
    /// The tokens and assistant messages of each Harness session already
    /// written to the history, so a pass writes only what grew. Kept in its own
    /// `harness.json` beside the project folder files; absent in an old state
    /// folder means none.
    #[serde(default)]
    pub harness_recorded: BTreeMap<String, HarnessRecorded>,
    /// The job links of Claude Code sessions that left `sessions`, kept until
    /// the Harness window has passed after the session's `last`. Saved in
    /// `harness.json` with the Harness totals; absent in an old state file
    /// means none.
    #[serde(default)]
    pub closed_flash_jobs: BTreeMap<String, ClosedJob>,
    /// The newest tokens and line time of each Codex rollout already written to
    /// the history, so a pass writes only what grew. Kept in its own
    /// `codex.json` beside the Harness totals; absent in an old state folder
    /// means none.
    #[serde(default)]
    pub codex_recorded: BTreeMap<String, CodexRecorded>,
    /// The project folders whose state changed since the last save.
    #[serde(skip)]
    changed: BTreeSet<String>,
    /// Whether `harness_recorded` changed since the last save.
    #[serde(skip)]
    harness_changed: bool,
    /// Whether `codex_recorded` changed since the last save.
    #[serde(skip)]
    codex_changed: bool,
}

impl ScanState {
    /// Read the state of every project folder. A missing folder is the default.
    ///
    /// A version-1 file is read with the 1.1.0 shape and migrated: its offsets
    /// stay, so no byte already read is recorded again. The migrated folders are
    /// marked changed, so the next [`save`](Self::save) writes them as version 2.
    /// Any other version is an error: rebuilding a state would append the
    /// records it already wrote to the history a second time.
    pub fn load(app: &AppPaths) -> Result<ScanState, TranscriptError> {
        let dir = app.transcripts_state_dir();
        let mut state = ScanState {
            version: STATE_VERSION,
            ..ScanState::default()
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(state),
            Err(source) => return Err(TranscriptError::Io { path: dir, source }),
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| TranscriptError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(HARNESS_STATE_FILE) | Some(CODEX_STATE_FILE)
            ) {
                // The Harness and Codex token memories are read on their own,
                // and a transcript load must not delete them.
                continue;
            }
            let kind = entry.file_type().map_err(|source| TranscriptError::Io {
                path: path.clone(),
                source,
            })?;
            if kind.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        let harness_path = dir.join(HARNESS_STATE_FILE);
        let mut harness_extra =
            RecordedTotals::of(&mut state.harness_recorded, &mut state.harness_changed)
                .load(&harness_path)?;
        if let Some(value) = harness_extra.remove("closedFlashJobs") {
            state.closed_flash_jobs =
                serde_json::from_value(value).map_err(|source| TranscriptError::State {
                    path: harness_path,
                    source,
                })?;
        }
        let codex_path = dir.join(CODEX_STATE_FILE);
        RecordedTotals::of(&mut state.codex_recorded, &mut state.codex_changed)
            .load(&codex_path)?;

        for path in &paths {
            let bytes = fs::read(path).map_err(|source| TranscriptError::Io {
                path: path.clone(),
                source,
            })?;
            let (file, migrated) = parse_state_file(path, &bytes)?;
            if migrated {
                // A migrated file must reach the disk as version 2, even when
                // this pass reads no new bytes from its transcripts.
                for key in file.files.keys() {
                    if let Some((folder, _)) = key.split_once('/') {
                        state.changed.insert(folder.to_string());
                    }
                }
                for seen in file.sessions.values() {
                    state.changed.insert(seen.folder.clone());
                }
            }
            state.files.extend(file.files);
            state.sessions.extend(file.sessions);
        }
        Ok(state)
    }

    /// The one way to remember the session record written from `session`: it
    /// sets [`SessionSeen::recorded`] and marks the session's project folder
    /// changed, so the record reaches the state file even when the transcript
    /// did not grow. A session not in the state is not changed.
    pub fn set_recorded(&mut self, session: &str, recorded: RecordedSession) {
        let Some(seen) = self.sessions.get_mut(session) else {
            return;
        };
        seen.recorded = Some(recorded);
        let folder = seen.folder.clone();
        self.mark_changed(&folder);
    }

    /// Remember which Harness session tokens were written to the history, so the
    /// next pass writes only what grew. The state is saved with the next
    /// [`save`](Self::save).
    pub fn set_harness_recorded(&mut self, recorded: &BTreeMap<String, HarnessRecorded>) {
        RecordedTotals::of(&mut self.harness_recorded, &mut self.harness_changed).set(recorded);
    }

    /// Drop a recorded Harness total when its session's file no longer exists
    /// below `sessions`. One metadata call per recorded id whose file is known;
    /// a file that cannot be read is still there, so a failed read never drops
    /// a total. A total without a known file (an old state file) is kept.
    pub fn prune_harness_recorded(&mut self, sessions: &Path) {
        RecordedTotals::of(&mut self.harness_recorded, &mut self.harness_changed).prune(
            sessions,
            &BTreeSet::new(),
            |recorded| recorded.file.as_deref(),
        );
    }

    /// Remember which Codex rollout totals were written to the history, so the
    /// next pass writes only what grew. The state is saved with the next
    /// [`save`](Self::save).
    pub fn set_codex_recorded(&mut self, recorded: &BTreeMap<String, CodexRecorded>) {
        RecordedTotals::of(&mut self.codex_recorded, &mut self.codex_changed).set(recorded);
    }

    /// Drop a recorded Codex total when its rollout file no longer exists below
    /// `sessions` **and** no rollout of its session id was read in this pass.
    /// One metadata call per recorded id; a file that cannot be read is still
    /// there, so a failed read never drops a total. An absolute path from an old
    /// state file is checked as it is.
    pub fn prune_codex_recorded(&mut self, sessions: &Path, read_ids: &BTreeSet<String>) {
        RecordedTotals::of(&mut self.codex_recorded, &mut self.codex_changed).prune(
            sessions,
            read_ids,
            |recorded| Some(recorded.file.as_path()),
        );
    }

    /// Drop the job links of Claude Code sessions that last answered more than
    /// `hours` before `now`: the Harness window has passed.
    pub fn prune_closed_flash_jobs(&mut self, now: DateTime<Utc>, hours: i64) {
        let before = self.closed_flash_jobs.len();
        self.closed_flash_jobs
            .retain(|_, link| now.signed_duration_since(link.last) <= Duration::hours(hours));
        if self.closed_flash_jobs.len() != before {
            self.harness_changed = true;
        }
    }

    /// Mark one project folder for the next [`save`](Self::save).
    pub fn mark_changed(&mut self, folder: &str) {
        self.changed.insert(folder.to_string());
    }

    /// Forget a session that is not running and whose last answer (its own or an
    /// agent's) is more than
    /// one hour before `now`: its sums leave `sessions`, while its files'
    /// offsets stay, so its transcript is not read again. Its `flash_jobs` move
    /// into [`Self::closed_flash_jobs`], so a Harness job of the closed session
    /// keeps its owner for the Harness window. Its folder is marked changed, so
    /// the next [`save`](Self::save) writes it. Returns how many sessions left.
    pub fn forget_closed(&mut self, running: &BTreeSet<String>, now: DateTime<Utc>) -> usize {
        let closed: Vec<String> = self
            .sessions
            .iter()
            .filter(|(session, seen)| {
                // The session's last answer is its own or one of its agents',
                // whichever is newer: an agent that still works keeps it.
                let last = seen
                    .agents
                    .values()
                    .map(|agent| agent.last)
                    .fold(seen.last, DateTime::max);
                !running.contains(*session) && now.signed_duration_since(last) > Duration::hours(1)
            })
            .map(|(session, _)| session.clone())
            .collect();
        let mut forgotten = 0;
        let mut links_moved = false;
        for session in closed {
            if let Some(seen) = self.sessions.remove(&session) {
                for job in seen.flash_jobs {
                    let link = ClosedJob {
                        session: session.clone(),
                        last: seen.last,
                    };
                    match self.closed_flash_jobs.get(&job) {
                        Some(previous) if previous.last >= link.last => {}
                        _ => {
                            self.closed_flash_jobs.insert(job, link);
                            links_moved = true;
                        }
                    }
                }
                self.mark_changed(&seen.folder);
                forgotten += 1;
            }
        }
        if links_moved {
            self.harness_changed = true;
        }
        forgotten
    }

    /// Write the state files of the project folders that changed since the last
    /// save, and return how many were written or deleted.
    pub fn save(
        &mut self,
        claude: &ClaudePaths,
        app: &AppPaths,
        now: DateTime<Utc>,
    ) -> Result<u32, TranscriptError> {
        let projects = claude.projects_dir();
        // Check every transcript first, without changing anything: an error
        // leaves the state exactly as it was before the call.
        let mut gone: Vec<String> = Vec::new();
        for key in self.files.keys() {
            match fs::metadata(projects.join(key)) {
                Ok(_) => {}
                // Claude Code removed the transcript; its entry goes.
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    gone.push(key.clone());
                }
                // An unreadable transcript is not a gone one: nothing is pruned.
                Err(source) => {
                    return Err(TranscriptError::Io {
                        path: projects.join(key),
                        source,
                    })
                }
            }
        }
        // Prune: a transcript that has not grown for 24 hours drops its ids, an
        // entry whose transcript is gone is removed, and so is a session without
        // any transcript left.
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for (key, file) in &mut self.files {
            if now.signed_duration_since(file.grew) > Duration::hours(KEEP_IDS_FOR_HOURS)
                && !file.recent.is_empty()
            {
                file.recent.clear();
                touched.insert(folder_of(key).to_string());
            }
        }
        for key in gone {
            self.files.remove(&key);
            touched.insert(folder_of(&key).to_string());
        }
        // The (folder, session) pairs that still have a transcript, built once.
        let mut with_transcript: BTreeSet<(String, String)> = BTreeSet::new();
        for key in self.files.keys() {
            if let Some((folder, rest)) = key.split_once('/') {
                let session = match rest.split_once('/') {
                    Some((session, _)) => session,
                    None => rest.strip_suffix(".jsonl").unwrap_or(rest),
                };
                with_transcript.insert((folder.to_string(), session.to_string()));
            }
        }
        let without_transcript: Vec<String> = self
            .sessions
            .iter()
            .filter(|(session, seen)| {
                !with_transcript.contains(&(seen.folder.clone(), (*session).clone()))
            })
            .map(|(session, _)| session.clone())
            .collect();
        for session in without_transcript {
            if let Some(seen) = self.sessions.remove(&session) {
                touched.insert(seen.folder);
            }
        }
        for folder in touched {
            self.changed.insert(folder);
        }

        let dir = app.transcripts_state_dir();
        if !self.changed.is_empty() || self.harness_changed || self.codex_changed {
            atomic::ensure_private_dir(&dir).map_err(|source| TranscriptError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        let mut written = 0u32;
        for folder in &self.changed {
            let state = self.state_of(folder);
            let path = dir.join(format!("{folder}.json"));
            if state.files.is_empty() && state.sessions.is_empty() {
                match fs::remove_file(&path) {
                    Ok(()) => written += 1,
                    Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => return Err(TranscriptError::Io { path, source }),
                }
            } else {
                let bytes =
                    serde_json::to_vec(&state).map_err(|source| TranscriptError::State {
                        path: path.clone(),
                        source,
                    })?;
                atomic::write_atomic(&path, &bytes, Access::Private).map_err(|source| {
                    TranscriptError::Io {
                        path: path.clone(),
                        source,
                    }
                })?;
                written += 1;
            }
        }
        if self.harness_changed {
            let path = dir.join(HARNESS_STATE_FILE);
            let extra = if self.harness_recorded.is_empty() && self.closed_flash_jobs.is_empty() {
                None
            } else {
                Some(harness_extra(&self.closed_flash_jobs, &path)?)
            };
            written += RecordedTotals::of(&mut self.harness_recorded, &mut self.harness_changed)
                .save(&path, extra)?;
        }
        if self.codex_changed {
            let path = dir.join(CODEX_STATE_FILE);
            let extra = if self.codex_recorded.is_empty() {
                None
            } else {
                Some(BTreeMap::new())
            };
            written += RecordedTotals::of(&mut self.codex_recorded, &mut self.codex_changed)
                .save(&path, extra)?;
        }
        self.version = STATE_VERSION;
        self.changed.clear();
        Ok(written)
    }

    /// The entries of one project folder, as its state file holds them.
    fn state_of(&self, folder: &str) -> StateFile {
        let mut state = StateFile {
            version: STATE_VERSION,
            files: BTreeMap::new(),
            sessions: BTreeMap::new(),
        };
        for (key, file) in &self.files {
            if folder_of(key) == folder {
                state.files.insert(key.clone(), file.clone());
            }
        }
        for (session, seen) in &self.sessions {
            if seen.folder == folder {
                state.sessions.insert(session.clone(), seen.clone());
            }
        }
        state
    }
}

/// Read one project folder's state file, migrating a version-1 file and
/// refusing a version this module does not know. Returns the file and whether
/// it was migrated (and must be written back as version 2).
fn parse_state_file(path: &Path, bytes: &[u8]) -> Result<(StateFile, bool), TranscriptError> {
    let header: StateVersion =
        serde_json::from_slice(bytes).map_err(|source| TranscriptError::State {
            path: path.to_path_buf(),
            source,
        })?;
    match header.version {
        STATE_VERSION_1 => {
            let old: StateFileV1 =
                serde_json::from_slice(bytes).map_err(|source| TranscriptError::State {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok((old.into(), true))
        }
        STATE_VERSION => {
            let file: StateFile =
                serde_json::from_slice(bytes).map_err(|source| TranscriptError::State {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok((file, false))
        }
        found => Err(TranscriptError::UnsupportedStateVersion {
            path: path.to_path_buf(),
            found,
            current: STATE_VERSION,
            newer: newer_version(found),
        }),
    }
}

/// The hint an unsupported version's error carries.
fn newer_version(found: u32) -> &'static str {
    if found > STATE_VERSION {
        " (the file was written by a newer seatswap)"
    } else {
        ""
    }
}

/// What one scan found, including the state it read into. The state is staged:
/// [`commit`](Scan::commit) moves it into the caller's [`ScanState`] once the
/// answers' token lines were written. A scan that is dropped leaves the old
/// state, so the next scan reads the same lines again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub answers: Vec<Answer>,
    pub user_turns: Vec<UserTurn>,
    pub complete: bool,
    pub warnings: Vec<String>,
    /// The state after this scan; [`commit`](Scan::commit) moves it out.
    next: ScanState,
}

impl Scan {
    /// The state after this scan: the sessions it saw and the offsets it read
    /// to. `token_records` needs it to name a session's folder.
    pub fn state(&self) -> &ScanState {
        &self.next
    }

    /// The project folders this scan changed; empty after [`save`](Self::save).
    pub fn changed_folders(&self) -> Vec<String> {
        self.next.changed.iter().cloned().collect()
    }

    /// Write this scan's changed folders, before its answers' token lines.
    /// A failed save leaves the caller's [`ScanState`] as it was; it must not be
    /// committed, or a restart would read the same answers again.
    pub fn save(
        &mut self,
        claude: &ClaudePaths,
        app: &AppPaths,
        now: DateTime<Utc>,
    ) -> Result<u32, TranscriptError> {
        self.next.save(claude, app, now)
    }

    /// Move the scan state past what this scan read. Call it only after the
    /// answers' token lines were written: a scan that is not committed reads
    /// the same lines again next time, and its answers are counted again then.
    pub fn commit(&mut self, state: &mut ScanState) {
        *state = std::mem::take(&mut self.next);
    }
}

/// Read what is new in every transcript under `projects_dir`. The [`ScanState`]
/// is not changed; the result carries the state to [`commit`](Scan::commit)
/// once the answers' token lines are written.
pub fn scan(
    claude: &ClaudePaths,
    state: &ScanState,
    budget_bytes: u64,
) -> Result<Scan, TranscriptError> {
    let mut next = state.clone();
    let projects = claude.projects_dir();
    let mut files = candidates(&projects)?;
    // A file whose length equals its stored offset has nothing new: it is not opened.
    files.retain(|candidate| {
        candidate.len != next.files.get(&candidate.key).map_or(0, |file| file.offset)
    });
    // Running sessions come before a backlog. Within one session its main
    // transcript comes before its agents, so an agent's answers meet the
    // session its transcript makes even when the budget stops the scan early.
    let mut newest: HashMap<String, DateTime<Utc>> = HashMap::new();
    for candidate in &files {
        let entry = newest
            .entry(candidate.session.clone())
            .or_insert(candidate.mtime);
        if candidate.mtime > *entry {
            *entry = candidate.mtime;
        }
    }
    files.sort_by(|a, b| {
        newest[&b.session]
            .cmp(&newest[&a.session])
            .then_with(|| a.agent.is_some().cmp(&b.agent.is_some()))
            .then_with(|| a.key.cmp(&b.key))
    });

    let mut run = ScanRun {
        state: &mut next,
        budget: budget_bytes,
        used: 0,
        answers: Vec::new(),
        user_turns: Vec::new(),
        spawns: Vec::new(),
        flash_jobs: BTreeMap::new(),
        warnings: Vec::new(),
    };
    let mut complete = true;
    for (index, candidate) in files.iter().enumerate() {
        if run.used >= run.budget {
            complete = false;
            break;
        }
        let outcome = run.read(candidate)?;
        run.used = run.used.saturating_add(outcome.consumed);
        if outcome.budget_hit {
            if !outcome.at_eof || index + 1 < files.len() {
                complete = false;
            }
            break;
        }
    }
    let ScanRun {
        answers,
        user_turns,
        spawns,
        flash_jobs,
        warnings,
        ..
    } = run;
    remember_spawns(&mut next, &spawns);
    remember_flash_jobs(&mut next, &flash_jobs);
    sum_answers(&mut next, &answers);
    Ok(Scan {
        answers,
        user_turns,
        complete,
        warnings,
        next,
    })
}

/// Put this scan's `Agent` and `Task` calls into their sessions' `spawns`.
fn remember_spawns(state: &mut ScanState, spawns: &[Spawn]) {
    for spawn in spawns {
        if let Some(seen) = state.sessions.get_mut(&spawn.session) {
            seen.spawns
                .insert(spawn.tool_use_id.clone(), spawn.agent.clone());
        }
    }
}

/// Put this scan's flash job ids into their sessions' `flash_jobs`. The ids
/// outlive the state files as long as their session is in the state; the
/// sessions thread resolves them with the job reports.
fn remember_flash_jobs(state: &mut ScanState, jobs: &BTreeMap<String, BTreeSet<String>>) {
    for (session, ids) in jobs {
        let Some(seen) = state.sessions.get_mut(session) else {
            continue;
        };
        let before = seen.flash_jobs.len();
        seen.flash_jobs.extend(ids.iter().cloned());
        if seen.flash_jobs.len() != before {
            let folder = seen.folder.clone();
            state.mark_changed(&folder);
        }
    }
}

/// Add this scan's answers to the sums of their session. Only the answers this
/// scan counted are here: the message-id rule kept every other one out, so an
/// answer read twice is summed once.
fn sum_answers(state: &mut ScanState, answers: &[Answer]) {
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for answer in answers {
        let Some(seen) = state.sessions.get_mut(&answer.session) else {
            // An agent's transcript without its session's main transcript: there
            // is no session to hold its sums (a main transcript makes one).
            continue;
        };
        let tokens = Tokens {
            input: answer.input,
            out: answer.output,
            cw: answer.cache_write,
            cr: answer.cache_read,
        };
        seen.totals += tokens;
        *seen.by_model.entry(answer.model.clone()).or_default() += tokens;
        if let Some(agent) = &answer.agent {
            let entry = seen
                .agents
                .entry(agent.clone())
                .or_insert_with(|| AgentSeen {
                    first: answer.at,
                    last: answer.at,
                    model: None,
                    tokens: Tokens::default(),
                    answers: 0,
                });
            if answer.at < entry.first {
                entry.first = answer.at;
            }
            if answer.at >= entry.last {
                entry.last = answer.at;
                entry.model = Some(answer.model.clone());
            }
            entry.tokens += tokens;
            entry.answers += 1;
        }
        let minute = minute_start(answer.at);
        match seen
            .recent
            .binary_search_by_key(&minute, |(start, _)| *start)
        {
            Ok(index) => {
                *seen.recent[index]
                    .1
                    .entry(answer.model.clone())
                    .or_default() += tokens
            }
            Err(index) => {
                let mut of_minute = BTreeMap::new();
                of_minute.insert(answer.model.clone(), tokens);
                seen.recent.insert(index, (minute, of_minute));
            }
        }
        touched.insert(answer.session.clone());
    }
    for session in touched {
        if let Some(seen) = state.sessions.get_mut(&session) {
            prune_recent(seen);
        }
    }
}

/// Keep only the minutes within 60 minutes of the newest answer: at most 60.
fn prune_recent(seen: &mut SessionSeen) {
    let Some((newest, _)) = seen.recent.last() else {
        return;
    };
    let oldest = *newest - Duration::minutes(60);
    seen.recent.retain(|(minute, _)| *minute > oldest);
}

/// The tokens of the last 60 minutes per model, with each model's newest minute
/// in the window: the one way the hour is valued per model (spec §3.8). A
/// minute starting exactly at `now - 60 min` is not in the window.
pub fn last_hour_by_model(
    seen: &SessionSeen,
    now: DateTime<Utc>,
) -> BTreeMap<String, (DateTime<Utc>, Tokens)> {
    let after = now - Duration::minutes(60);
    let mut by_model: BTreeMap<String, (DateTime<Utc>, Tokens)> = BTreeMap::new();
    for (minute, of_minute) in &seen.recent {
        if *minute <= after {
            continue;
        }
        for (model, tokens) in of_minute {
            let entry = by_model
                .entry(model.clone())
                .or_insert((*minute, Tokens::default()));
            if *minute > entry.0 {
                entry.0 = *minute;
            }
            entry.1 += *tokens;
        }
    }
    by_model
}

/// The tokens of the answers in the 60 minutes before `now`: the sum over all
/// models of [`last_hour_by_model`], so both readers use the same window.
pub fn last_hour(seen: &SessionSeen, now: DateTime<Utc>) -> Tokens {
    let mut tokens = Tokens::default();
    for (_, (_, of_model)) in last_hour_by_model(seen, now) {
        tokens += of_model;
    }
    tokens
}

/// The agents a session started, and the meta files that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agents {
    pub metas: Vec<AgentMeta>,
    pub warnings: Vec<String>,
}

/// The agents a session started, from their `*.meta.json` files. `spawns` maps
/// the `tool_use` id of an `Agent` or `Task` call to the agent that made it
/// (`None`: the main transcript), and gives each agent its parent.
///
/// A meta file that cannot be read or parsed is a warning naming the file and
/// the reason, and its agent is still listed, as running: a meta without
/// `finished` means a running agent, and an unreadable meta cannot say it
/// finished. An agent whose meta already says `finished` does not read its
/// transcript: the end-turn rule cannot change that answer. [`AgentEnds`]
/// remembers the other agents' transcript ends between passes.
pub fn agents_of(
    claude: &ClaudePaths,
    session: &str,
    spawns: &BTreeMap<String, Option<String>>,
    ends: &mut AgentEnds,
) -> Agents {
    let mut out = Agents {
        metas: Vec::new(),
        warnings: Vec::new(),
    };
    let dir = claude.projects_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut folders: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.path().is_dir() {
            folders.push(entry.path());
        }
    }
    folders.sort();
    for folder in folders {
        let subagents = folder.join(session).join("subagents");
        let Ok(entries) = fs::read_dir(&subagents) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let file_name = entry.file_name();
            let Some(agent) = file_name.to_str().and_then(agent_meta_id) else {
                continue;
            };
            let path = entry.path();
            // The transcript beside the meta file; the end-turn rule is about
            // the agent's own lines, whether or not its meta can be read.
            let transcript = path.with_file_name(format!("agent-{agent}.jsonl"));
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(source) => {
                    out.warnings
                        .push(format!("agent meta {}: {source}", path.display()));
                    out.metas
                        .push(unread_agent(session, agent, ends.ended(&transcript)));
                    continue;
                }
            };
            let meta = match serde_json::from_slice::<AgentMetaFile>(&bytes) {
                Ok(meta) => meta,
                Err(error) => {
                    // Never the parser's message: it can quote the file's text.
                    out.warnings.push(format!(
                        "agent meta {}: not valid JSON at line {} column {}",
                        path.display(),
                        error.line(),
                        error.column()
                    ));
                    out.metas
                        .push(unread_agent(session, agent, ends.ended(&transcript)));
                    continue;
                }
            };
            let parent = meta
                .tool_use_id
                .as_ref()
                .and_then(|id| spawns.get(id))
                .cloned()
                .flatten();
            let finished = meta.finished.unwrap_or(false);
            // A finished meta already says the agent is done: reading its
            // transcript could not change the rule's answer.
            let transcript_ended = if finished {
                false
            } else {
                ends.ended(&transcript)
            };
            out.metas.push(AgentMeta {
                session: session.to_string(),
                agent: agent.to_string(),
                agent_type: meta.agent_type,
                description: meta.description,
                finished,
                spawn_depth: Some(meta.spawn_depth),
                tool_use_id: meta.tool_use_id,
                parent,
                transcript_ended,
            });
        }
    }
    out.metas.sort_by(|a, b| a.agent.cmp(&b.agent));
    out
}

/// The agent entry of a meta file that could not be read: still running (a
/// meta without `finished` is a running agent), with everything else unknown.
fn unread_agent(session: &str, agent: &str, transcript_ended: bool) -> AgentMeta {
    AgentMeta {
        session: session.to_string(),
        agent: agent.to_string(),
        agent_type: None,
        description: None,
        finished: false,
        spawn_depth: None,
        tool_use_id: None,
        parent: None,
        transcript_ended,
    }
}

/// One remembered answer: the file's length and modification time, and
/// whether it ended.
type EndEntry = ((u64, Option<SystemTime>), bool);

/// The answers of [`agent_ended`], remembered per transcript path with the
/// file's length and modification time: a pass that sees an unchanged file does
/// not read it again. The sessions thread keeps one of these between passes and
/// calls [`AgentEnds::start_pass`] at the start of each, so an answer lives only
/// while its agent is still asked about: a closed session's agents are dropped
/// one pass after they were last asked, and a long day does not grow the cache.
#[derive(Debug, Default, Clone)]
pub struct AgentEnds {
    /// The answers asked for in this pass.
    ends: HashMap<PathBuf, EndEntry>,
    /// The answers of the pass before, not asked for again yet.
    previous: HashMap<PathBuf, EndEntry>,
}

impl AgentEnds {
    /// Begin a pass: the answers no one asked for during the last pass are
    /// forgotten.
    pub fn start_pass(&mut self) {
        self.previous = std::mem::take(&mut self.ends);
    }

    /// How many transcript answers are remembered.
    pub fn len(&self) -> usize {
        self.ends.len() + self.previous.len()
    }

    /// Whether no transcript answer is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the agent's transcript at `path` ended, reading it only when it
    /// is new or changed since this cache's last answer.
    pub fn ended(&mut self, path: &Path) -> bool {
        self.ended_by(path, agent_ended)
    }

    /// [`Self::ended`] with the reader passed in, so a test can count the
    /// reads. A path whose metadata cannot be read has no length or time to
    /// remember, so it is read every time.
    fn ended_by<F>(&mut self, path: &Path, read: F) -> bool
    where
        F: FnOnce(&Path) -> bool,
    {
        let Some(key) = fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.len(), metadata.modified().ok()))
        else {
            return read(path);
        };
        if let Some((stored, ended)) = self.ends.get(path) {
            if *stored == key {
                return *ended;
            }
        }
        if let Some((stored, ended)) = self.previous.remove(path) {
            if stored == key {
                self.ends.insert(path.to_path_buf(), (stored, ended));
                return ended;
            }
        }
        let ended = read(path);
        self.ends.insert(path.to_path_buf(), (key, ended));
        ended
    }
}

/// Whether an agent's transcript ends with the agent's final answer: its last
/// whole non-blank line is an `assistant` entry whose `message.stop_reason` is
/// `"end_turn"`.
///
/// Only the end of the file is read. A torn last line (no newline yet, because
/// the file is still being written) is ignored, and the line before it decides.
/// Blank lines at the end are not entries either, so the last non-blank whole
/// line decides. A missing or unreadable file is `false`.
pub fn agent_ended(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let Ok(mut end) = last_whole_line_end(&mut file, path) else {
        return false;
    };
    loop {
        // `end` is just after a '\n'; the line before it ends at that '\n'.
        let Some(line_end) = end.checked_sub(1) else {
            return false;
        };
        let start = match newline_before(&mut file, line_end, path) {
            Ok(Some(index)) => index + 1,
            Ok(None) => 0,
            Err(_) => return false,
        };
        let Ok(len) = usize::try_from(line_end - start) else {
            return false;
        };
        let Ok(mut bytes) = read_at(&mut file, start, len, path) else {
            return false;
        };
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            // A blank line is not an entry: the line before it decides.
            end = start;
            continue;
        }
        let Ok(line) = serde_json::from_slice::<EndLine>(&bytes) else {
            return false;
        };
        return line.kind.as_deref() == Some("assistant")
            && line
                .message
                .and_then(|message| message.stop_reason)
                .as_deref()
                == Some("end_turn");
    }
}

/// The last line's `type` and `message.stop_reason`; every other field is
/// ignored, and message text is never read into a value.
#[derive(Deserialize)]
struct EndLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<EndMessage>,
}

#[derive(Deserialize)]
struct EndMessage {
    stop_reason: Option<String>,
}

/// One [`TokenRecord`] per (session, 5-minute bucket, model).
pub fn token_records(answers: &[Answer], state: &ScanState, home: &Path) -> Vec<TokenRecord> {
    // The earliest answer of a group names its folder, so the answers are taken
    // in time order.
    let mut ordered: Vec<&Answer> = answers.iter().collect();
    ordered.sort_by_key(|answer| answer.at);

    let mut groups: BTreeMap<GroupKey, TokenGroup> = BTreeMap::new();
    let mut projects: HashMap<String, ProjectKey> = HashMap::new();
    for answer in ordered {
        let cwd = answer_cwd(answer, state);
        let key = (
            bucket_start(answer.at),
            answer.session.clone(),
            answer.model.clone(),
        );
        let group = groups.entry(key).or_insert_with(|| {
            let project = projects
                .entry(cwd.clone())
                .or_insert_with(|| project::identify(Path::new(&cwd), home))
                .clone();
            TokenGroup {
                cwd: cwd.clone(),
                project,
                input: 0,
                output: 0,
                cache_write: 0,
                cache_read: 0,
                answers: 0,
                agent_answers: 0,
            }
        });
        group.input += answer.input;
        group.output += answer.output;
        group.cache_write += answer.cache_write;
        group.cache_read += answer.cache_read;
        group.answers += 1;
        if answer.agent.is_some() {
            group.agent_answers += 1;
        }
    }
    groups
        .into_iter()
        .map(|((t, session, model), group)| TokenRecord {
            t,
            minutes: TOKEN_BUCKET_MINUTES as u32,
            session,
            cwd: group.cwd,
            project: group.project,
            model,
            input: group.input,
            out: group.output,
            cw: group.cache_write,
            cr: group.cache_read,
            answers: group.answers,
            agent_answers: group.agent_answers,
        })
        .collect()
}

// ---- the implementation ----

/// The first field of a state file, read before the rest so a version-1 file
/// can use the 1.1.0 shape and any other unsupported version is an error.
#[derive(Debug, Deserialize)]
struct StateVersion {
    version: u32,
}

/// The state file of one project folder: `<folder name>.json`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    version: u32,
    files: BTreeMap<String, FileState>,
    sessions: BTreeMap<String, SessionSeen>,
}

/// The 1.1.0 shape of a project folder's state file (version 1), read only to
/// migrate it. Its `recent` entries have no model, so they cannot become the
/// version-2 per-model entries; [`From`] drops them, which leaves only the last
/// hour unknown.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateFileV1 {
    files: BTreeMap<String, FileStateV1>,
    sessions: BTreeMap<String, SessionSeenV1>,
}

/// The 1.1.0 shape of one transcript's state.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileStateV1 {
    offset: u64,
    grew: DateTime<Utc>,
    recent: Vec<String>,
    #[serde(default)]
    last_answer: Option<DateTime<Utc>>,
}

/// The 1.1.0 shape of one session's state.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSeenV1 {
    folder: String,
    cwd: String,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    last_model: Option<String>,
    last_user_turn: Option<DateTime<Utc>>,
    #[serde(default)]
    thinking: Vec<(DateTime<Utc>, u32)>,
    #[serde(default)]
    totals: Tokens,
    #[serde(default)]
    by_model: BTreeMap<String, Tokens>,
    #[serde(default)]
    agents: BTreeMap<String, AgentSeen>,
    #[serde(default)]
    spawns: BTreeMap<String, Option<String>>,
    #[serde(default)]
    recent: Vec<(DateTime<Utc>, Tokens)>,
    #[serde(default)]
    recorded: Option<RecordedSession>,
}

impl From<StateFileV1> for StateFile {
    /// Keep every offset and every field the version-2 state still has. The
    /// fields version 1 lacks start empty; they only concern flash links and
    /// the per-model last hour, never which bytes were already recorded.
    fn from(old: StateFileV1) -> StateFile {
        StateFile {
            version: STATE_VERSION,
            files: old
                .files
                .into_iter()
                .map(|(key, file)| {
                    (
                        key,
                        FileState {
                            offset: file.offset,
                            grew: file.grew,
                            recent: file.recent,
                            last_answer: file.last_answer,
                            flash_tools: BTreeSet::new(),
                            title: None,
                            custom_title: None,
                        },
                    )
                })
                .collect(),
            sessions: old
                .sessions
                .into_iter()
                .map(|(session, seen)| {
                    // Version 1's per-minute counts have no model: read them so
                    // the 1.1.0 shape parses, then drop them on purpose.
                    let _no_model = seen.recent;
                    (
                        session,
                        SessionSeen {
                            folder: seen.folder,
                            cwd: seen.cwd,
                            first: seen.first,
                            last: seen.last,
                            last_model: seen.last_model,
                            last_user_turn: seen.last_user_turn,
                            thinking: seen.thinking,
                            totals: seen.totals,
                            by_model: seen.by_model,
                            agents: seen.agents,
                            spawns: seen.spawns,
                            recent: Vec::new(),
                            recorded: seen.recorded,
                            flash_jobs: BTreeSet::new(),
                            title: None,
                            custom_title: None,
                        },
                    )
                })
                .collect(),
        }
    }
}

/// One recorded-totals state file: `harness.json` or `codex.json`. Both hold
/// the version, the recorded totals and, for the Harness store, the job links
/// of Claude Code sessions that left the scan state. The extra keys come after
/// `recorded`, as `harness.json` has always written them.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedFile<T> {
    version: u32,
    recorded: BTreeMap<String, T>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// One store's recorded token totals, seen through the map and changed flag it
/// lives in. The loader, saver, setter and pruner are written once here for
/// both `harness.json` and `codex.json`.
struct RecordedTotals<'a, T> {
    recorded: &'a mut BTreeMap<String, T>,
    /// Whether `recorded` changed since the last save.
    changed: &'a mut bool,
}

impl<'a, T> RecordedTotals<'a, T>
where
    T: Clone + PartialEq + Serialize + serde::de::DeserializeOwned,
{
    fn of(recorded: &'a mut BTreeMap<String, T>, changed: &'a mut bool) -> Self {
        RecordedTotals { recorded, changed }
    }

    /// Read `path` into `recorded` and return its extra top-level keys. A
    /// missing file is no totals and no warning. A file of another version is
    /// an error, never a silent rebuild: rebuilding writes the history it
    /// already holds a second time.
    fn load(self, path: &Path) -> Result<BTreeMap<String, serde_json::Value>, TranscriptError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(source) => {
                return Err(TranscriptError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        let header: StateVersion =
            serde_json::from_slice(&bytes).map_err(|source| TranscriptError::State {
                path: path.to_path_buf(),
                source,
            })?;
        if header.version != STATE_VERSION {
            return Err(TranscriptError::UnsupportedStateVersion {
                path: path.to_path_buf(),
                found: header.version,
                current: STATE_VERSION,
                newer: newer_version(header.version),
            });
        }
        let file: RecordedFile<T> =
            serde_json::from_slice(&bytes).map_err(|source| TranscriptError::State {
                path: path.to_path_buf(),
                source,
            })?;
        *self.recorded = file.recorded;
        Ok(file.extra)
    }

    /// Remember a store's totals for the next [`save`](ScanState::save).
    fn set(&mut self, recorded: &BTreeMap<String, T>) {
        if *self.recorded != *recorded {
            *self.recorded = recorded.clone();
            *self.changed = true;
        }
    }

    /// Drop a recorded total when its file no longer exists below `root` and no
    /// rollout of its session id was read in this pass. One metadata call per
    /// recorded id; a file that cannot be read is still there, so a failed read
    /// never drops a total.
    fn prune(
        &mut self,
        root: &Path,
        read_ids: &BTreeSet<String>,
        file_of: impl Fn(&T) -> Option<&Path>,
    ) {
        let mut dropped = false;
        self.recorded.retain(|id, recorded| {
            if read_ids.contains(id) {
                return true;
            }
            let Some(file) = file_of(recorded) else {
                return true;
            };
            let path = if file.is_absolute() {
                file.to_path_buf()
            } else {
                root.join(file)
            };
            match fs::metadata(&path) {
                Ok(_) => true,
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    dropped = true;
                    false
                }
                // The file cannot be checked (a permission or an I/O error):
                // it is not known to be gone, so its total stays.
                Err(_) => true,
            }
        });
        if dropped {
            *self.changed = true;
        }
    }

    /// Write the store's file when it changed, or remove it when it holds
    /// nothing. `extra` is `None` when the store has no other value to keep, so
    /// an empty file is removed as it always was; `Some` writes those keys after
    /// `recorded`, even when the map is empty. Returns 1 when the file was
    /// written or removed.
    fn save(
        &mut self,
        path: &Path,
        extra: Option<BTreeMap<String, serde_json::Value>>,
    ) -> Result<u32, TranscriptError> {
        if !*self.changed {
            return Ok(0);
        }
        let written = match extra {
            None if self.recorded.is_empty() => match fs::remove_file(path) {
                Ok(()) => 1,
                Err(source) if source.kind() == io::ErrorKind::NotFound => 0,
                Err(source) => {
                    return Err(TranscriptError::Io {
                        path: path.to_path_buf(),
                        source,
                    })
                }
            },
            extra => {
                let state = RecordedFile {
                    version: STATE_VERSION,
                    recorded: self.recorded.clone(),
                    extra: extra.unwrap_or_default(),
                };
                let bytes =
                    serde_json::to_vec(&state).map_err(|source| TranscriptError::State {
                        path: path.to_path_buf(),
                        source,
                    })?;
                atomic::write_atomic(path, &bytes, Access::Private).map_err(|source| {
                    TranscriptError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                })?;
                1
            }
        };
        *self.changed = false;
        Ok(written)
    }
}

/// The extra top-level keys of `harness.json` after `recorded`: the links of
/// code sessions that left the scan state.
fn harness_extra(
    closed: &BTreeMap<String, ClosedJob>,
    path: &Path,
) -> Result<BTreeMap<String, serde_json::Value>, TranscriptError> {
    let value = serde_json::to_value(closed).map_err(|source| TranscriptError::State {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(BTreeMap::from([("closedFlashJobs".to_string(), value)]))
}

/// A transcript that may have new bytes.
struct Candidate {
    key: String,
    session: String,
    agent: Option<String>,
    folder: String,
    path: PathBuf,
    len: u64,
    mtime: DateTime<Utc>,
}

/// What reading one transcript did.
struct Outcome {
    /// The bytes of whole lines that were read in this scan.
    consumed: u64,
    /// The budget was reached after a whole line.
    budget_hit: bool,
    /// No whole line is left in the file.
    at_eof: bool,
}

/// What the path of a transcript says about it; a line of the file never
/// changes these.
struct Reading<'a> {
    session: &'a str,
    agent: Option<&'a str>,
    folder: &'a str,
    /// The file was read again from 0 because it shrank.
    reread: bool,
    /// `last_answer` when this reading began: the reread guard compares against
    /// it, while the file state's own copy moves up as answers are counted.
    last_answer: Option<DateTime<Utc>>,
}

/// One line at a time: a line of a session with its own order of answers and
/// user turns.
enum Event {
    Answer(usize),
    User(DateTime<Utc>),
}

/// The part of a scan that belongs to one file.
struct FileScan {
    state: FileState,
    events: Vec<Event>,
    /// The message ids of this scan, with the index of their answer.
    ids: HashMap<String, usize>,
    unreadable: usize,
    /// The `ai-title` of this scan: `Some` when the file had such a line, and
    /// its `None` when that title was empty. Applied after the lines, so a
    /// title line before the session's first answer still reaches it.
    title: Option<Option<String>>,
    /// The `custom-title` of this scan, the same way.
    custom_title: Option<Option<String>>,
}

/// An `Agent` or `Task` call of a `tool_use` block: the id its meta file names,
/// and the agent that made the call (`None`: the main transcript).
struct Spawn {
    session: String,
    agent: Option<String>,
    tool_use_id: String,
}

/// The scan as it runs.
struct ScanRun<'a> {
    state: &'a mut ScanState,
    budget: u64,
    used: u64,
    answers: Vec<Answer>,
    user_turns: Vec<UserTurn>,
    spawns: Vec<Spawn>,
    /// The flash job ids each session's tool results named.
    flash_jobs: BTreeMap<String, BTreeSet<String>>,
    warnings: Vec<String>,
}

impl ScanRun<'_> {
    fn read(&mut self, candidate: &Candidate) -> Result<Outcome, TranscriptError> {
        let mut stored = match self.state.files.get(&candidate.key) {
            Some(file) => file.clone(),
            None => FileState {
                offset: 0,
                grew: candidate.mtime,
                recent: Vec::new(),
                last_answer: None,
                flash_tools: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        };
        let mut file = match File::open(&candidate.path) {
            Ok(file) => file,
            // Claude Code removed the transcript between listing and opening it.
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(Outcome {
                    consumed: 0,
                    budget_hit: false,
                    at_eof: true,
                })
            }
            Err(source) => {
                return Err(TranscriptError::Io {
                    path: candidate.path.clone(),
                    source,
                })
            }
        };
        let mut reread = false;
        let shorter = candidate.len < stored.offset;
        if shorter {
            if stored.recent.is_empty() {
                // It had stopped growing, so it cannot be counted twice: leave it.
                return self.not_counted(candidate, &mut file, stored);
            }
            self.warnings.push(format!(
                "{} shrank from {} to {} bytes; reading it again from the start",
                candidate.key, stored.offset, candidate.len
            ));
            stored.offset = 0;
            reread = true;
        }
        let mut start = stored.offset;
        if !shorter && start > 0 {
            // The byte before a stored offset is the '\n' that ended the last line
            // read. If it is not, the file was replaced by one whose lines fall
            // elsewhere and the offset points into the middle of a line.
            let last = byte_before(&mut file, start, &candidate.path)?;
            if last != Some(b'\n') {
                if stored.recent.is_empty() {
                    return self.not_counted(candidate, &mut file, stored);
                }
                self.warnings.push(format!(
                    "{} was replaced after it had been read; reading it again from the start",
                    candidate.key
                ));
                stored.offset = 0;
                start = 0;
                reread = true;
            }
        }
        if let Err(source) = file.seek(SeekFrom::Start(start)) {
            return Err(TranscriptError::Io {
                path: candidate.path.clone(),
                source,
            });
        }
        let reading = Reading {
            session: &candidate.session,
            agent: candidate.agent.as_deref(),
            folder: &candidate.folder,
            reread,
            last_answer: stored.last_answer,
        };
        let mut file_scan = FileScan {
            state: stored,
            events: Vec::new(),
            ids: HashMap::new(),
            unreadable: 0,
            title: None,
            custom_title: None,
        };
        let mut pending: Vec<u8> = Vec::new();
        // The file offset of pending[0].
        let mut pending_start = start;
        // The start of the next line in pending; everything before it was read.
        let mut line_from = 0usize;
        // How much of pending was searched for '\n'; nothing before it holds one.
        let mut searched = 0usize;
        let mut chunk = vec![0u8; CHUNK_BYTES];
        let mut offset = start;
        let mut budget_hit = false;
        let mut at_eof = false;
        'lines: loop {
            while let Some(found) = pending[searched..].iter().position(|byte| *byte == b'\n') {
                let end = searched + found;
                let mut line = &pending[line_from..end];
                if pending_start + line_from as u64 == 0 && line.starts_with(&BYTE_ORDER_MARK) {
                    line = &line[BYTE_ORDER_MARK.len()..];
                }
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                offset = pending_start + end as u64 + 1;
                self.line(&reading, line, &mut file_scan);
                line_from = end + 1;
                searched = end + 1;
                if self.used.saturating_add(offset - start) >= self.budget {
                    budget_hit = true;
                    break 'lines;
                }
            }
            // The bytes not searched yet hold no '\n'; the next chunk is searched from there.
            searched = pending.len();
            // The consumed part is removed once per chunk, not once per line.
            if line_from > 0 {
                pending.drain(..line_from);
                pending_start += line_from as u64;
                searched -= line_from;
                line_from = 0;
            }
            let read = match file.read(&mut chunk) {
                Ok(0) => {
                    at_eof = true;
                    break;
                }
                Ok(read) => read,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(TranscriptError::Io {
                        path: candidate.path.clone(),
                        source,
                    })
                }
            };
            pending.extend_from_slice(&chunk[..read]);
        }
        // The bytes after the last '\n' are not read; the offset stays before them.
        let consumed = offset - start;
        let mut file_state = file_scan.state;
        file_state.offset = offset;
        if consumed > 0 {
            file_state.grew = candidate.mtime;
        }
        if file_scan.unreadable > 0 {
            self.warnings.push(format!(
                "{}: {} lines could not be read",
                candidate.key, file_scan.unreadable
            ));
        }
        let title = file_scan.title.take();
        let custom_title = file_scan.custom_title.take();
        self.apply_events(&candidate.session, file_scan.events);
        let seen = self.state.sessions.get_mut(&candidate.session);
        let changed = match seen {
            Some(seen) => {
                let ai = apply_title(&mut file_state.title, title, Some(&mut seen.title));
                let custom = apply_title(
                    &mut file_state.custom_title,
                    custom_title,
                    Some(&mut seen.custom_title),
                );
                ai || custom
            }
            None => {
                apply_title(&mut file_state.title, title, None);
                apply_title(&mut file_state.custom_title, custom_title, None);
                false
            }
        };
        if changed {
            self.state.mark_changed(&candidate.folder);
        }
        if self.state.files.get(&candidate.key) != Some(&file_state) {
            self.state.files.insert(candidate.key.clone(), file_state);
            self.state.mark_changed(&candidate.folder);
        }
        Ok(Outcome {
            consumed,
            budget_hit,
            at_eof: at_eof || offset >= candidate.len,
        })
    }

    /// A transcript that had stopped growing was replaced: warn once, move the
    /// offset to the end of its last whole line, and count nothing.
    fn not_counted(
        &mut self,
        candidate: &Candidate,
        file: &mut File,
        mut stored: FileState,
    ) -> Result<Outcome, TranscriptError> {
        self.warnings.push(format!(
            "{} was replaced after it had been read; its new content is not counted",
            candidate.key
        ));
        stored.offset = last_whole_line_end(file, &candidate.path)?;
        self.state.files.insert(candidate.key.clone(), stored);
        self.state.mark_changed(&candidate.folder);
        Ok(Outcome {
            consumed: 0,
            budget_hit: false,
            at_eof: true,
        })
    }

    /// One line: a narrow look at it, nothing of its text kept.
    fn line(&mut self, reading: &Reading, bytes: &[u8], file: &mut FileScan) {
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            // A blank line, or one of only spaces and tabs, says nothing.
            return;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            file.unreadable += 1;
            return;
        };
        // The type decides the shape; every other key is looked at only on an
        // answer or a user line, whatever shape it has elsewhere.
        let Ok(head) = serde_json::from_str::<LineType>(text) else {
            file.unreadable += 1;
            return;
        };
        if head.kind == "ai-title" || head.kind == "custom-title" {
            self.title(reading, text, &head.kind, file);
            return;
        }
        if head.kind != "assistant" && head.kind != "user" {
            // attachment, system, api-request, queue-operation, …: skipped quietly.
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(text) else {
            file.unreadable += 1;
            return;
        };
        if head.kind == "assistant" {
            self.assistant(reading, &line, file);
        } else {
            self.user(reading, &line, file);
        }
    }

    /// An `ai-title` line (Claude Code's own title of the session) or a
    /// `custom-title` line (the name the user gave it with `/rename`), as
    /// written. An agent's transcript never names its session; a line of
    /// either type without its string (`aiTitle`, `customTitle`) is unreadable
    /// like a broken answer. An empty or all-space title is no title. No other
    /// text of the line is kept.
    fn title(&mut self, reading: &Reading, text: &str, kind: &str, file: &mut FileScan) {
        if reading.agent.is_some() {
            return;
        }
        let Ok(line) = serde_json::from_str::<TitleLine>(text) else {
            file.unreadable += 1;
            return;
        };
        let (title, slot) = if kind == "custom-title" {
            (line.custom_title, &mut file.custom_title)
        } else {
            (line.ai_title, &mut file.title)
        };
        let Some(title) = title else {
            file.unreadable += 1;
            return;
        };
        *slot = Some(session_title(title));
    }

    fn assistant(&mut self, reading: &Reading, line: &Line, file: &mut FileScan) {
        let (Some(at), Some(cwd), Some(message)) =
            (line.timestamp, line.cwd.as_deref(), line.message.as_ref())
        else {
            file.unreadable += 1;
            return;
        };
        self.record_spawns(reading, message, file);
        let (Some(id), Some(model), Some(usage)) = (
            message.id.as_deref(),
            message.model.as_deref(),
            message.usage.as_ref(),
        ) else {
            file.unreadable += 1;
            return;
        };
        let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) else {
            file.unreadable += 1;
            return;
        };
        if model == SYNTHETIC {
            return;
        }
        // The model the session asked for names the answer: a gateway such as
        // Ollama answers with its own spelling (`deepseek-v4.1-flash` for
        // `deepseek-v4.1-flash:cloud`), while prices, providers and the
        // Harness rows use the asked-for name. Claude's own models are spelled
        // the same in both fields.
        let model = line.requested_model.as_deref().unwrap_or(model);
        let thinking = message.content.as_ref().is_some_and(Content::thinking);
        if let Some(&index) = file.ids.get(id) {
            // A further line of an answer this scan already counted.
            if thinking {
                if let Some(answer) = self.answers.get_mut(index) {
                    answer.thinking = true;
                }
            }
            return;
        }
        if file.state.recent.iter().any(|known| known == id) {
            // Counted before this scan, or by the scan before a reread.
            return;
        }
        // On a reread, an answer from before the file was read again is not
        // counted twice even when its id has left `recent`.
        if reading.reread && reading.last_answer.is_some_and(|last| at <= last) {
            return;
        }
        let cwd = project::canonical_cwd(cwd);
        let index = self.answers.len();
        file.ids.insert(id.to_string(), index);
        if file.state.recent.len() >= RECENT_IDS {
            file.state.recent.remove(0);
        }
        file.state.recent.push(id.to_string());
        file.state.last_answer = Some(file.state.last_answer.map_or(at, |last| last.max(at)));
        self.answers.push(Answer {
            session: reading.session.to_string(),
            agent: reading.agent.map(str::to_string),
            message_id: id.to_string(),
            at,
            cwd: cwd.clone(),
            model: model.to_string(),
            input,
            output,
            cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            thinking,
        });
        if reading.agent.is_none() {
            self.observe(reading, at, Some(&cwd), Some(model));
            file.events.push(Event::Answer(index));
        }
    }

    /// The `Agent` and `Task` calls of an assistant line, also when the line
    /// repeats an answer already counted: only the block's `type`, `id` and
    /// `name` are read. A `flash-agents` call's id is remembered with the file,
    /// so its tool result is recognised in a later scan.
    fn record_spawns(&mut self, reading: &Reading, message: &Message, file: &mut FileScan) {
        let Some(content) = message.content.as_ref() else {
            return;
        };
        for block in &content.blocks {
            if block.kind.as_deref() != Some("tool_use") {
                continue;
            }
            let Some(id) = block.id.as_deref() else {
                continue;
            };
            if let Some(name) = block.name.as_deref() {
                if FLASH_TOOLS.contains(&name) {
                    file.state.flash_tools.insert(id.to_string());
                }
            }
            if !matches!(block.name.as_deref(), Some("Agent") | Some("Task")) {
                continue;
            }
            self.spawns.push(Spawn {
                session: reading.session.to_string(),
                agent: reading.agent.map(str::to_string),
                tool_use_id: id.to_string(),
            });
        }
    }

    /// The flash job ids of a user line's tool results: only a result whose
    /// `tool_use` id belongs to one of [`FLASH_TOOLS`] is read, and only the ids
    /// its content holds leave this function.
    fn record_flash_results(&mut self, reading: &Reading, line: &Line, file: &FileScan) {
        let Some(content) = line
            .message
            .as_ref()
            .and_then(|message| message.content.as_ref())
        else {
            return;
        };
        for block in &content.blocks {
            if block.kind.as_deref() != Some("tool_result") {
                continue;
            }
            let Some(tool) = block.tool_use_id.as_deref() else {
                continue;
            };
            if !file.state.flash_tools.contains(tool) || block.content.0.is_empty() {
                continue;
            }
            self.flash_jobs
                .entry(reading.session.to_string())
                .or_default()
                .extend(block.content.0.iter().cloned());
        }
    }

    fn user(&mut self, reading: &Reading, line: &Line, file: &mut FileScan) {
        let Some(at) = line.timestamp else {
            file.unreadable += 1;
            return;
        };
        // A flash result can be in a worker's transcript too; the job still
        // hangs under the session that started the worker.
        self.record_flash_results(reading, line, file);
        if reading.agent.is_some() {
            // A user turn comes from a main transcript only.
            return;
        }
        let cwd = line.cwd.as_deref().map(project::canonical_cwd);
        let real = !line.is_meta.unwrap_or(false)
            && match &line.origin {
                None => true,
                Some(origin) => origin.kind.as_deref() == Some("human"),
            }
            && line
                .message
                .as_ref()
                .and_then(|message| message.content.as_ref())
                .is_some_and(Content::is_real_user_message);
        let suppressed = reading.reread
            && self
                .state
                .sessions
                .get(reading.session)
                .is_some_and(|seen| seen.last_user_turn.is_some_and(|last| at <= last));
        if suppressed {
            return;
        }
        self.observe(reading, at, cwd.as_deref(), None);
        if real {
            self.user_turns.push(UserTurn {
                session: reading.session.to_string(),
                at,
            });
            file.events.push(Event::User(at));
        }
    }

    /// Note a line of a main transcript in its session.
    fn observe(
        &mut self,
        reading: &Reading,
        at: DateTime<Utc>,
        cwd: Option<&str>,
        model: Option<&str>,
    ) {
        let seen = self
            .state
            .sessions
            .entry(reading.session.to_string())
            .or_insert_with(|| SessionSeen {
                folder: reading.folder.to_string(),
                cwd: cwd.unwrap_or_default().to_string(),
                first: at,
                last: at,
                last_model: model.map(str::to_string),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Tokens::default(),
                by_model: BTreeMap::new(),
                agents: BTreeMap::new(),
                spawns: BTreeMap::new(),
                recent: Vec::new(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            });
        if at >= seen.last {
            if let Some(cwd) = cwd {
                seen.cwd = cwd.to_string();
            }
            if let Some(model) = model {
                seen.last_model = Some(model.to_string());
            }
        }
        if at < seen.first {
            seen.first = at;
        }
        if at > seen.last {
            seen.last = at;
        }
        self.state.mark_changed(reading.folder);
    }

    /// Apply a file's answers and user turns to its session, in the order they
    /// were read: a user turn resets the thinking count.
    fn apply_events(&mut self, session: &str, events: Vec<Event>) {
        let answers = &self.answers;
        let Some(seen) = self.state.sessions.get_mut(session) else {
            return;
        };
        for event in events {
            match event {
                Event::Answer(index) => {
                    let Some(answer) = answers.get(index) else {
                        continue;
                    };
                    if !answer.thinking {
                        continue;
                    }
                    let minute = minute_start(answer.at);
                    match seen
                        .thinking
                        .binary_search_by_key(&minute, |(start, _)| *start)
                    {
                        Ok(found) => {
                            seen.thinking[found].1 = seen.thinking[found].1.saturating_add(1)
                        }
                        Err(found) => seen.thinking.insert(found, (minute, 1)),
                    }
                }
                Event::User(at) => {
                    seen.last_user_turn = Some(seen.last_user_turn.map_or(at, |last| last.max(at)));
                    seen.thinking.clear();
                }
            }
        }
    }
}

/// Every transcript that may be read: `<folder>/<session>.jsonl` and
/// `<folder>/<session>/subagents/agent-<id>.jsonl`. Nothing else under
/// `projects` is looked at.
fn candidates(dir: &Path) -> Result<Vec<Candidate>, TranscriptError> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(source) => {
            return Err(TranscriptError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    let mut folders: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| TranscriptError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if entry.path().is_dir() {
            folders.push((name.to_string(), entry.path()));
        }
    }
    folders.sort();
    for (folder, path) in folders {
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(TranscriptError::Io {
                    path: path.clone(),
                    source,
                })
            }
        };
        let mut inner: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| TranscriptError::Io {
                path: path.clone(),
                source,
            })?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            inner.push((name.to_string(), entry.path()));
        }
        inner.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry_path) in inner {
            if !entry_path.is_dir() {
                let Some(session) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if session.is_empty() {
                    continue;
                }
                push_candidate(&mut out, &folder, &entry_path, session, None, &name)?;
                continue;
            }
            // <folder>/<session>/subagents/agent-<id>.jsonl
            let subagents = entry_path.join("subagents");
            let entries = match fs::read_dir(&subagents) {
                Ok(entries) => entries,
                Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(TranscriptError::Io {
                        path: subagents.clone(),
                        source,
                    })
                }
            };
            for entry in entries {
                let entry = entry.map_err(|source| TranscriptError::Io {
                    path: subagents.clone(),
                    source,
                })?;
                let file_name = entry.file_name();
                let Some(agent_name) = file_name.to_str() else {
                    continue;
                };
                let Some(agent) = agent_id(agent_name) else {
                    continue;
                };
                if entry.path().is_dir() {
                    continue;
                }
                push_candidate(
                    &mut out,
                    &folder,
                    &entry.path(),
                    &name,
                    Some(agent),
                    agent_name,
                )?;
            }
        }
    }
    Ok(out)
}

fn push_candidate(
    out: &mut Vec<Candidate>,
    folder: &str,
    path: &Path,
    session: &str,
    agent: Option<&str>,
    file_name: &str,
) -> Result<(), TranscriptError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(TranscriptError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let modified = metadata.modified().map_err(|source| TranscriptError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let key = match agent {
        Some(_) => format!("{folder}/{session}/subagents/{file_name}"),
        None => format!("{folder}/{file_name}"),
    };
    out.push(Candidate {
        key,
        session: session.to_string(),
        agent: agent.map(str::to_string),
        folder: folder.to_string(),
        path: path.to_path_buf(),
        len: metadata.len(),
        mtime: DateTime::<Utc>::from(modified),
    });
    Ok(())
}

/// `<id>` of `agent-<id>.jsonl`.
fn agent_id(file_name: &str) -> Option<&str> {
    let id = file_name.strip_prefix("agent-")?.strip_suffix(".jsonl")?;
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// `<id>` of `agent-<id>.meta.json`.
fn agent_meta_id(file_name: &str) -> Option<&str> {
    let id = file_name
        .strip_prefix("agent-")?
        .strip_suffix(".meta.json")?;
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// The project folder a `files` key belongs to.
fn folder_of(key: &str) -> &str {
    match key.split_once('/') {
        Some((folder, _)) => folder,
        None => key,
    }
}

/// The byte at `offset - 1` of the open file, or `None` when the file ends
/// before it.
fn byte_before(file: &mut File, offset: u64, path: &Path) -> Result<Option<u8>, TranscriptError> {
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(offset - 1))
        .map_err(|source| TranscriptError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    loop {
        match file.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(TranscriptError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
    }
}

/// The offset just after the last '\n' of the open file, or 0 when it has none.
fn last_whole_line_end(file: &mut File, path: &Path) -> Result<u64, TranscriptError> {
    let mut end = file
        .seek(SeekFrom::End(0))
        .map_err(|source| TranscriptError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    while end > 0 {
        let take = end.min(CHUNK_BYTES as u64) as usize;
        let from = end - take as u64;
        file.seek(SeekFrom::Start(from))
            .map_err(|source| TranscriptError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let mut filled = 0usize;
        while filled < take {
            match file.read(&mut buffer[filled..take]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(TranscriptError::Io {
                        path: path.to_path_buf(),
                        source,
                    })
                }
            }
        }
        if filled < take {
            // The file shrank while it was read: start again from its new end.
            end = file
                .seek(SeekFrom::End(0))
                .map_err(|source| TranscriptError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            continue;
        }
        if let Some(index) = buffer[..take].iter().rposition(|byte| *byte == b'\n') {
            return Ok(from + index as u64 + 1);
        }
        end = from;
    }
    Ok(0)
}

/// The offset of the last '\n' before `before`, or `None` when there is none.
/// Only the bytes before `before` are read.
fn newline_before(
    file: &mut File,
    before: u64,
    path: &Path,
) -> Result<Option<u64>, TranscriptError> {
    let mut end = before;
    while end > 0 {
        let take = end.min(CHUNK_BYTES as u64) as usize;
        let from = end - take as u64;
        let buffer = read_at(file, from, take, path)?;
        if let Some(index) = buffer.iter().rposition(|byte| *byte == b'\n') {
            return Ok(Some(from + index as u64));
        }
        end = from;
    }
    Ok(None)
}

/// Read exactly `len` bytes at `from`.
fn read_at(
    file: &mut File,
    from: u64,
    len: usize,
    path: &Path,
) -> Result<Vec<u8>, TranscriptError> {
    let mut buffer = vec![0u8; len];
    file.seek(SeekFrom::Start(from))
        .map_err(|source| TranscriptError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.read_exact(&mut buffer)
        .map_err(|source| TranscriptError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(buffer)
}

/// The length of a token line's bucket, in minutes (spec R3).
pub const TOKEN_BUCKET_MINUTES: i64 = 5;

/// The start of the 5-minute bucket `at` falls in, in UTC.
pub fn bucket_start(at: DateTime<Utc>) -> DateTime<Utc> {
    at - Duration::minutes(i64::from(at.minute()) % TOKEN_BUCKET_MINUTES)
        - Duration::seconds(i64::from(at.second()))
        - Duration::nanoseconds(i64::from(at.nanosecond()))
}

/// The start of the hour `at` falls in, in UTC.
pub(crate) fn hour_start(at: DateTime<Utc>) -> DateTime<Utc> {
    at - Duration::minutes(i64::from(at.minute()))
        - Duration::seconds(i64::from(at.second()))
        - Duration::nanoseconds(i64::from(at.nanosecond()))
}

/// The start of the minute `at` falls in, in UTC.
fn minute_start(at: DateTime<Utc>) -> DateTime<Utc> {
    at - Duration::seconds(i64::from(at.second()))
        - Duration::nanoseconds(i64::from(at.nanosecond()))
}

/// A group of [`TokenRecord`], keyed by session, 5-minute bucket and model.
type GroupKey = (DateTime<Utc>, String, String);

struct TokenGroup {
    /// The cwd of the group's earliest answer.
    cwd: String,
    project: ProjectKey,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    answers: u32,
    agent_answers: u32,
}

/// The folder an answer's tokens belong to: its own for a main transcript, its
/// session's for an agent whose parent session is known.
fn answer_cwd(answer: &Answer, state: &ScanState) -> String {
    if answer.agent.is_some() {
        if let Some(seen) = state.sessions.get(&answer.session) {
            if !seen.cwd.is_empty() {
                return seen.cwd.clone();
            }
        }
    }
    answer.cwd.clone()
}

/// The `type` of a transcript line, the only key read before the rest: every
/// other line type has its own shape.
#[derive(Deserialize)]
struct LineType {
    #[serde(rename = "type")]
    kind: String,
}

/// A session's title as written, or none when it is empty or all-space: the
/// one rule for Claude Code's `ai-title` and DeepSeek Harness's
/// `session/title`.
pub fn session_title(title: String) -> Option<String> {
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

/// An `ai-title` or `custom-title` line: only its `aiTitle` or `customTitle`
/// is read; every other field and text of the line is dropped as it is read.
#[derive(Deserialize)]
struct TitleLine {
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
}

/// One kind of title after a file's scan: this scan's title (`Some(None)` for
/// an empty one), else one that waited in the file's state for its session to
/// appear. With the session (`seen`) it is set there and leaves the file's
/// state; without, it waits in `pending`. True when the session's title
/// changed.
fn apply_title(
    pending: &mut Option<String>,
    scanned: Option<Option<String>>,
    seen: Option<&mut Option<String>>,
) -> bool {
    let Some(title) = scanned.or_else(|| pending.take().map(Some)) else {
        return false;
    };
    match seen {
        Some(seen) => {
            *pending = None;
            if *seen == title {
                return false;
            }
            *seen = title;
            true
        }
        None => {
            *pending = title;
            false
        }
    }
}

/// A transcript line, as far as seatswap looks at it: never a message text.
/// Only an `assistant` or a `user` line is read into it.
#[derive(Deserialize)]
struct Line {
    timestamp: Option<DateTime<Utc>>,
    cwd: Option<String>,
    /// The model the session asked for (an assistant line's `requestedModel`).
    #[serde(rename = "requestedModel")]
    requested_model: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    origin: Option<Origin>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Origin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    content: Option<Content>,
    usage: Option<Usage>,
}

/// The four token counts. The cache counts are absent when no cache was used;
/// a cache count that is present but not a number makes the line unreadable.
#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// A content block, as far as seatswap looks at it: its `type`, for a
/// `tool_use` block its `id` and `name`, and for a `tool_result` block its
/// `tool_use_id` and the flash job ids in its content. Its text and its input
/// are never kept.
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    /// The flash job ids of a tool result's `content`; every other text is
    /// dropped by [`FlashIds`] as it is read.
    #[serde(default)]
    content: FlashIds,
}

/// `message.content` as far as seatswap looks at it: whether it is a string,
/// and each block. Of a text block nothing but its type is kept.
struct Content {
    is_string: bool,
    blocks: Vec<Block>,
}

impl Content {
    /// A real user message: a string, or blocks that include a `text` block.
    fn is_real_user_message(&self) -> bool {
        self.is_string
            || self
                .blocks
                .iter()
                .any(|block| block_kind(block) == Some("text"))
    }

    fn thinking(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block_kind(block), Some(kind) if THINKING_BLOCKS.contains(&kind)))
    }
}

fn block_kind(block: &Block) -> Option<&str> {
    block.kind.as_deref()
}

/// The flash job ids in a tool result's content. Deserialization visits the
/// content's strings and keeps only ids matching `flash-task-<uuid>` or
/// `job-<8 hex>`: the text itself is never stored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FlashIds(BTreeSet<String>);

impl<'de> Deserialize<'de> for FlashIds {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<FlashIds, D::Error> {
        struct FlashVisitor;

        impl<'de> Visitor<'de> for FlashVisitor {
            type Value = FlashIds;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tool result: a string, a list or an object")
            }

            fn visit_str<E>(self, text: &str) -> Result<FlashIds, E> {
                Ok(FlashIds(flash_ids_in(text)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FlashIds, A::Error> {
                let mut ids = BTreeSet::new();
                while let Some(part) = seq.next_element::<FlashIds>()? {
                    ids.extend(part.0);
                }
                Ok(FlashIds(ids))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<FlashIds, A::Error> {
                let mut ids = BTreeSet::new();
                while let Some((key, value)) = map.next_entry::<String, FlashIds>()? {
                    ids.extend(flash_ids_in(&key));
                    ids.extend(value.0);
                }
                Ok(FlashIds(ids))
            }

            fn visit_bool<E>(self, _value: bool) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }

            fn visit_i64<E>(self, _value: i64) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }

            fn visit_u64<E>(self, _value: u64) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }

            fn visit_f64<E>(self, _value: f64) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }

            fn visit_none<E>(self) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }

            fn visit_unit<E>(self) -> Result<FlashIds, E> {
                Ok(FlashIds::default())
            }
        }

        deserializer.deserialize_any(FlashVisitor)
    }
}

/// The `flash-task-<uuid>` and `job-<8 hex>` ids in `text`, with the boundaries
/// the shapes imply: a UUID is exactly 36 characters, a job id exactly 8 hex
/// characters, and neither starts inside a longer word.
fn flash_ids_in(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut ids = BTreeSet::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if let Some(id) = pattern_at(bytes, index, b"flash-task-", |bytes, start| {
            uuid_at(bytes, start)
        }) {
            ids.insert(id);
            index += "flash-task-".len() + 36;
            continue;
        }
        if let Some(id) = pattern_at(bytes, index, b"job-", |bytes, start| {
            job_id_at(bytes, start)
        }) {
            ids.insert(id);
            index += "job-".len() + 8;
            continue;
        }
        index += 1;
    }
    ids
}

/// Whether a word boundary is before `index`: the start of the text or a
/// character that cannot be part of a longer id.
fn is_boundary_before(bytes: &[u8], index: usize) -> bool {
    match index.checked_sub(1).and_then(|before| bytes.get(before)) {
        None => true,
        Some(byte) => !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'-',
    }
}

/// The id beginning at `index` if `prefix` is there, `tail` accepts what follows
/// it, and a word boundary is before it.
fn pattern_at(
    bytes: &[u8],
    index: usize,
    prefix: &[u8],
    tail: impl Fn(&[u8], usize) -> Option<usize>,
) -> Option<String> {
    if !is_boundary_before(bytes, index) || !bytes[index..].starts_with(prefix) {
        return None;
    }
    let start = index + prefix.len();
    let end = tail(bytes, start)?;
    std::str::from_utf8(&bytes[index..end])
        .ok()
        .map(str::to_string)
}

/// The end of a 36-character UUID at `start`, when its shape is exact.
fn uuid_at(bytes: &[u8], start: usize) -> Option<usize> {
    const DASHES: [usize; 4] = [8, 13, 18, 23];
    let end = start.checked_add(36)?;
    if end > bytes.len() {
        return None;
    }
    for (offset, byte) in bytes[start..end].iter().enumerate() {
        let is_dash = DASHES.contains(&offset);
        if is_dash {
            if *byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    // A longer hex run after the UUID is not this UUID.
    if bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
    {
        return None;
    }
    Some(end)
}

/// The end of an 8-hex-digit job id at `start`, when its shape is exact.
fn job_id_at(bytes: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(8)?;
    if end > bytes.len() || !bytes[start..end].iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    if bytes.get(end).is_some_and(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(end)
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Content, D::Error> {
        struct ContentVisitor;

        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = Content;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string or a list of content blocks")
            }

            fn visit_str<E>(self, _text: &str) -> Result<Content, E> {
                Ok(Content {
                    is_string: true,
                    blocks: Vec::new(),
                })
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element::<Block>()? {
                    blocks.push(block);
                }
                Ok(Content {
                    is_string: false,
                    blocks,
                })
            }
        }

        deserializer.deserialize_any(ContentVisitor)
    }
}

/// One `agent-<id>.meta.json`: `agentType` and `description` are optional,
/// `spawnDepth` and `toolUseId` are in every file.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentMetaFile {
    agent_type: Option<String>,
    description: Option<String>,
    /// Absent while the agent runs; `false` was never seen.
    finished: Option<bool>,
    spawn_depth: u32,
    tool_use_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const SESSION: &str = "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34";
    const SESSION_B: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";
    const FOLDER: &str = "-home-sam-projects-atlas";
    const FOLDER_B: &str = "-home-sam-projects-ledger";
    const KEY: &str = "-home-sam-projects-atlas/7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34.jsonl";
    const AT: &str = "2026-11-12T09:41:07.412Z";
    const ENGINE: &str = "/home/sam/projects/atlas/engine";
    const OPUS: &str = "claude-opus-5-5";
    const SONNET: &str = "claude-sonnet-5-2";
    /// input, output, cache write, cache read of the README §5 answer.
    const USAGE: (u64, u64, u64, u64) = (2, 166, 26173, 28126);
    const BUDGET: u64 = 64 * 1024 * 1024;

    fn fixture() -> (tempfile::TempDir, ClaudePaths, AppPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let claude = ClaudePaths::for_config_dir(tmp.path(), tmp.path());
        let app = AppPaths::at(&tmp.path().join("app"));
        (tmp, claude, app)
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn blocks(kinds: &[&str]) -> Vec<serde_json::Value> {
        kinds
            .iter()
            .map(|kind| match *kind {
                "thinking" => {
                    serde_json::json!({"type": "thinking", "thinking": "", "signature": "EqQ"})
                }
                "redacted_thinking" => {
                    serde_json::json!({"type": "redacted_thinking", "data": "EqQ"})
                }
                "text" => serde_json::json!({"type": "text", "text": "fix the failing tests"}),
                other => serde_json::json!({"type": other}),
            })
            .collect()
    }

    fn assistant_line(
        id: &str,
        at: &str,
        cwd: &str,
        model: &str,
        kinds: &[&str],
        usage: (u64, u64, u64, u64),
    ) -> String {
        assistant_with_content(id, at, cwd, model, blocks(kinds), usage)
    }

    fn assistant_with_content(
        id: &str,
        at: &str,
        cwd: &str,
        model: &str,
        content: Vec<serde_json::Value>,
        usage: (u64, u64, u64, u64),
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": "0b6f4c1a-2d3e-4f50-8a9b-0c1d2e3f4a5b",
            "parentUuid": "91c2a7b3-4c5d-4e6f-8a90-1b2c3d4e5f60",
            "sessionId": SESSION,
            "timestamp": at,
            "cwd": cwd,
            "gitBranch": "main",
            "isSidechain": false,
            "requestId": "req_011CZk3",
            "version": "2.1.322",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": "tool_use",
                "usage": {
                    "input_tokens": usage.0,
                    "output_tokens": usage.1,
                    "cache_creation_input_tokens": usage.2,
                    "cache_read_input_tokens": usage.3,
                    "service_tier": "standard",
                }
            }
        })
        .to_string()
    }

    /// An answer line with one `tool_use` block that started an agent.
    fn agent_call(id: &str, at: &str, tool_use_id: &str, name: &str) -> String {
        assistant_with_content(
            id,
            at,
            ENGINE,
            OPUS,
            vec![serde_json::json!({
                "type": "tool_use",
                "id": tool_use_id,
                "name": name,
                "input": {}
            })],
            USAGE,
        )
    }

    fn answer(id: &str, at: &str) -> String {
        assistant_line(id, at, ENGINE, OPUS, &["thinking"], USAGE)
    }

    /// An answer that is the agent's final one: `stop_reason` is `end_turn`.
    fn assistant_ending(id: &str, at: &str) -> String {
        assistant_line(id, at, ENGINE, OPUS, &["text"], USAGE).replace(
            "\"stop_reason\":\"tool_use\"",
            "\"stop_reason\":\"end_turn\"",
        )
    }

    /// An answer whose `requestId` is padded so that the line is `len` bytes.
    fn answer_of_len(id: &str, at: &str, len: usize) -> String {
        let base = serde_json::json!({
            "type": "assistant",
            "timestamp": at,
            "cwd": "/home/sam/atlas",
            "requestId": "",
            "message": {
                "id": id,
                "model": OPUS,
                "content": [],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string();
        assert!(
            base.len() <= len,
            "an answer line is at least {} bytes, not {len}",
            base.len()
        );
        let pad = "x".repeat(len - base.len());
        base.replace("\"requestId\":\"\"", &format!("\"requestId\":\"{pad}\""))
    }

    fn user_string(text: &str, at: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": at,
            "sessionId": SESSION,
            "isSidechain": false,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn user_tool_result(at: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": at,
            "sessionId": SESSION,
            "isSidechain": false,
            "toolUseResult": {},
            "sourceToolAssistantUUID": "0b6f4c1a-2d3e-4f50-8a9b-0c1d2e3f4a5b",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "ok"}
            ]}
        })
        .to_string()
    }

    fn user_tool_result_with(tool_use_id: &str, content: &str, at: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": at,
            "sessionId": SESSION,
            "isSidechain": false,
            "toolUseResult": {},
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": content}
            ]}
        })
        .to_string()
    }

    fn user_meta(at: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": at,
            "sessionId": SESSION,
            "isMeta": true,
            "message": {"role": "user", "content": [{"type": "text", "text": "..."}]}
        })
        .to_string()
    }

    fn user_with_origin(kind: &str, at: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": at,
            "sessionId": SESSION,
            "origin": {"kind": kind},
            "message": {"role": "user", "content": "fix the failing tests"}
        })
        .to_string()
    }

    fn ai_title(title: &str) -> String {
        serde_json::json!({
            "type": "ai-title",
            "aiTitle": title,
            "sessionId": SESSION,
        })
        .to_string()
    }

    fn custom_title(title: &str) -> String {
        serde_json::json!({
            "type": "custom-title",
            "customTitle": title,
            "sessionId": SESSION,
        })
        .to_string()
    }

    fn meta_of(finished: Option<bool>, tool_use_id: &str, spawn_depth: u32) -> String {
        let mut value = serde_json::json!({
            "agentType": "general-purpose",
            "description": "fix the failing tests",
            "isBuiltIn": true,
            "spawnDepth": spawn_depth,
            "toolUseId": tool_use_id
        });
        if let Some(finished) = finished {
            value["finished"] = serde_json::json!(finished);
        }
        value.to_string()
    }

    fn folder(claude: &ClaudePaths, name: &str) -> PathBuf {
        claude.projects_dir().join(name)
    }

    fn write_lines(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn append(path: &Path, line: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(format!("{line}\n").as_bytes()).unwrap();
    }

    fn scan_run(claude: &ClaudePaths) -> (ScanState, Scan) {
        let mut state = ScanState::default();
        let scan = scan_commit(claude, &mut state, BUDGET);
        (state, scan)
    }

    /// One scan and its commit, as a pass that wrote its token lines does.
    fn scan_commit(claude: &ClaudePaths, state: &mut ScanState, budget: u64) -> Scan {
        let mut scan = scan(claude, state, budget).unwrap();
        scan.commit(state);
        scan
    }

    /// Claude Code on an Ollama model (seen on 16 November 2026, Claude Code
    /// 2.1.326): the answer says `deepseek-v4.1-flash`, the line's
    /// `requestedModel` says `deepseek-v4.1-flash:cloud`. The asked-for name
    /// counts, so the tokens match Ollama's and the Harness rows' spelling;
    /// a line without `requestedModel` keeps the answer's model.
    #[test]
    fn the_requested_model_names_the_answer() {
        let (_tmp, claude, _app) = fixture();
        let mut asked: serde_json::Value = serde_json::from_str(&assistant_line(
            "msg_01Ollama1",
            AT,
            ENGINE,
            "deepseek-v4.1-flash",
            &["text"],
            USAGE,
        ))
        .unwrap();
        asked["requestedModel"] = serde_json::json!("deepseek-v4.1-flash:cloud");
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                asked.to_string(),
                assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["text"], USAGE),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        let models: Vec<&str> = scan
            .answers
            .iter()
            .map(|answer| answer.model.as_str())
            .collect();
        assert_eq!(models, vec!["deepseek-v4.1-flash:cloud", OPUS]);
    }

    #[test]
    fn an_answer_over_two_lines_counts_once() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["thinking"], USAGE),
                assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["tool_use"], USAGE),
            ],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        let answer = &scan.answers[0];
        assert_eq!(answer.session, SESSION);
        assert_eq!(answer.agent, None);
        assert_eq!(answer.message_id, "msg_01QmB7xT");
        assert_eq!(answer.at, at(AT));
        assert_eq!(answer.cwd, ENGINE);
        assert_eq!(answer.model, OPUS);
        assert_eq!(answer.input, 2);
        assert_eq!(answer.output, 166);
        assert_eq!(answer.cache_write, 26173);
        assert_eq!(answer.cache_read, 28126);
        assert!(answer.thinking);
        assert_eq!(
            state.files[KEY].offset,
            fs::metadata(folder(&claude, FOLDER).join(format!("{SESSION}.jsonl")))
                .unwrap()
                .len()
        );
    }

    #[test]
    fn the_same_id_far_apart_counts_once() {
        let (_tmp, claude, _app) = fixture();
        let mut lines = vec![
            assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["thinking"], USAGE),
            assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["tool_use"], USAGE),
        ];
        for minute in 0..10 {
            lines.push(user_tool_result(&format!(
                "2026-11-12T09:4{minute}:30.000Z"
            )));
        }
        lines.push(assistant_line(
            "msg_01QmB7xT",
            "2026-11-12T09:50:00.000Z",
            ENGINE,
            OPUS,
            &["text"],
            USAGE,
        ));
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &lines,
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        assert_eq!(scan.answers[0].message_id, "msg_01QmB7xT");
        assert_eq!(scan.answers[0].at, at(AT), "the first line's time");
    }

    #[test]
    fn a_second_scan_reads_only_what_is_new() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 1);

        append(
            &path,
            &assistant_line(
                "msg_b",
                "2026-11-12T09:42:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
        );
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert_eq!(second.answers.len(), 1, "{:?}", second.answers);
        assert_eq!(second.answers[0].message_id, "msg_b");
        assert!(second.complete);
    }

    #[test]
    fn a_half_written_last_line_waits() {
        let (_tmp, claude, _app) = fixture();
        let first_line = answer("msg_a", AT);
        let second_line = assistant_line(
            "msg_b",
            "2026-11-12T09:42:00.000Z",
            ENGINE,
            OPUS,
            &["text"],
            USAGE,
        );
        let split = second_line.len() / 2;
        let path = folder(&claude, FOLDER).join(format!("{SESSION}.jsonl"));
        fs::create_dir_all(folder(&claude, FOLDER)).unwrap();
        fs::write(&path, format!("{first_line}\n{}", &second_line[..split])).unwrap();

        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 1, "{:?}", first.answers);
        assert_eq!(first.answers[0].message_id, "msg_a");

        fs::write(&path, format!("{first_line}\n{second_line}\n")).unwrap();
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert_eq!(second.answers.len(), 1, "{:?}", second.answers);
        assert_eq!(second.answers[0].message_id, "msg_b");
    }

    #[test]
    fn a_shrunk_file_restarts_without_double_counting() {
        let (_tmp, claude, _app) = fixture();
        let first_line = answer("msg_a", AT);
        let second_line = assistant_line(
            "msg_b",
            "2026-11-12T09:42:00.000Z",
            ENGINE,
            OPUS,
            &["text"],
            USAGE,
        );
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[first_line.clone(), second_line.clone()],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 2);

        let bytes = fs::read(&path).unwrap();
        let cut = first_line.len() + 1 + second_line.len() / 2;
        fs::write(&path, &bytes[..cut]).unwrap();
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert_eq!(
            state.files[KEY].offset,
            first_line.len() as u64 + 1,
            "the offset stops after the last whole line of the reread"
        );
    }

    #[test]
    fn tool_results_and_meta_lines_are_not_user_turns() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                user_tool_result("2026-11-12T09:41:09.730Z"),
                user_meta("2026-11-12T09:40:57.000Z"),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.user_turns.len(), 1, "{:?}", scan.user_turns);
        assert_eq!(scan.user_turns[0].at, at("2026-11-12T09:40:58.101Z"));
        assert_eq!(scan.user_turns[0].session, SESSION);
        assert!(scan.answers.is_empty());
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn flash_job_ids_are_collected_from_their_tool_results() {
        let (_tmp, claude, _app) = fixture();
        let flash_tool = "mcp__plugin_flash-agents_flash__flash_task";
        let job = "flash-task-5c974a53-178a-4254-93b2-b804a111414a";
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                agent_call("msg_flash", AT, "toolu_flash", flash_tool),
                agent_call("msg_other", AT, "toolu_other", "Bash"),
                user_tool_result_with(
                    "toolu_flash",
                    &format!(
                        r#"{{"sessionId":"{job}","jobId":"job-3e32c2fc","childSessionId":"a25abffd-ed93-4296-868f-dbd77aca32ac"}}"#
                    ),
                    "2026-11-12T09:41:09.730Z",
                ),
                user_tool_result_with(
                    "toolu_other",
                    r#"{"sessionId":"flash-task-00000000-0000-4000-8000-000000000000"}"#,
                    "2026-11-12T09:41:10.730Z",
                ),
            ],
        );

        let (state, scan) = scan_run(&claude);

        let seen = state.sessions.get(SESSION).expect("the session");
        assert_eq!(
            seen.flash_jobs,
            BTreeSet::from([job.to_string(), "job-3e32c2fc".to_string()])
        );
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn notifications_and_messages_from_other_sessions_are_not_user_turns() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_with_origin("task-notification", "2026-11-12T09:40:58.101Z"),
                user_with_origin("peer", "2026-11-12T09:40:59.101Z"),
                user_with_origin("human", "2026-11-12T09:41:00.101Z"),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.user_turns.len(), 1, "{:?}", scan.user_turns);
        assert_eq!(scan.user_turns[0].at, at("2026-11-12T09:41:00.101Z"));
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn other_line_types_are_skipped_quietly() {
        let (_tmp, claude, _app) = fixture();
        let lines: Vec<String> = ["attachment", "system", "api-request", "queue-operation"]
            .iter()
            .map(|kind| {
                serde_json::json!({"type": kind, "timestamp": AT, "sessionId": SESSION}).to_string()
            })
            .collect();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &lines,
        );

        let (_state, scan) = scan_run(&claude);

        assert!(scan.answers.is_empty());
        assert!(scan.user_turns.is_empty());
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert!(scan.complete);
    }

    #[test]
    fn the_last_ai_title_names_the_session() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                // A title line before the session's first user turn in the same
                // scan is applied after the scan's lines; the last title wins.
                ai_title("Fix the invoice export"),
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                ai_title("Ledger import speed-up"),
            ],
        );

        let (state, scan) = scan_run(&claude);

        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert_eq!(
            state.sessions[SESSION].title.as_deref(),
            Some("Ledger import speed-up")
        );
    }

    /// Review S-4: a title line read before the session has any answer or user
    /// line waits in the file's state, so the title is not lost when the file
    /// offset moves past it.
    #[test]
    fn a_title_before_the_first_answer_is_kept_after_a_later_scan() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[ai_title("Fix the invoice export")],
        );
        let mut state = ScanState::default();
        scan_commit(&claude, &mut state, BUDGET);
        append(
            &path,
            &user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
        );
        append(&path, &answer("msg_a", AT));
        scan_commit(&claude, &mut state, BUDGET);

        assert_eq!(
            state.sessions[SESSION].title.as_deref(),
            Some("Fix the invoice export")
        );
    }

    /// 1.4.4: a `/rename` writes a `custom-title` line. It names the session
    /// over any `ai-title`, before or after it, as Claude Code shows it; a
    /// later rename is picked up by the next scan, and an empty one gives the
    /// name back to the `ai-title`.
    #[test]
    fn a_rename_names_the_session_over_the_ai_title() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                ai_title("Fix the invoice export"),
                custom_title("tripwire"),
                ai_title("Ledger import speed-up"),
            ],
        );
        let (mut state, scan) = scan_run(&claude);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        let seen = &state.sessions[SESSION];
        assert_eq!(seen.shown_title().map(String::as_str), Some("tripwire"));
        assert_eq!(seen.title.as_deref(), Some("Ledger import speed-up"));

        append(&path, &custom_title("tripwire-guard"));
        scan_commit(&claude, &mut state, BUDGET);
        assert_eq!(
            state.sessions[SESSION].shown_title().map(String::as_str),
            Some("tripwire-guard"),
            "a later rename is picked up"
        );

        append(&path, &custom_title("  "));
        scan_commit(&claude, &mut state, BUDGET);
        assert_eq!(state.sessions[SESSION].custom_title, None);
        assert_eq!(
            state.sessions[SESSION].shown_title().map(String::as_str),
            Some("Ledger import speed-up"),
            "an empty name gives the name back to the ai-title"
        );
    }

    #[test]
    fn a_rename_before_the_first_answer_is_kept_after_a_later_scan() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[custom_title("tripwire")],
        );
        let mut state = ScanState::default();
        scan_commit(&claude, &mut state, BUDGET);
        append(
            &path,
            &user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
        );
        append(&path, &answer("msg_a", AT));
        scan_commit(&claude, &mut state, BUDGET);

        assert_eq!(
            state.sessions[SESSION].custom_title.as_deref(),
            Some("tripwire")
        );
        assert_eq!(state.sessions[SESSION].title, None);
    }

    #[test]
    fn a_custom_title_without_a_title_is_unreadable() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                serde_json::json!({"type": "custom-title", "sessionId": SESSION}).to_string(),
            ],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(state.sessions[SESSION].custom_title, None);
        assert_eq!(
            scan.warnings,
            vec![format!("{KEY}: 1 lines could not be read")]
        );
    }

    #[test]
    fn an_ai_title_without_a_title_is_unreadable() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                serde_json::json!({"type": "ai-title", "sessionId": SESSION}).to_string(),
            ],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(state.sessions[SESSION].title, None);
        assert_eq!(
            scan.warnings,
            vec![format!("{KEY}: 1 lines could not be read")]
        );
    }

    #[test]
    fn a_title_is_stored_as_written_and_an_empty_one_is_no_title() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                ai_title("  Fix the invoice export  "),
            ],
        );
        let (mut state, scan) = scan_run(&claude);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert_eq!(
            state.sessions[SESSION].title.as_deref(),
            Some("  Fix the invoice export  "),
            "the title is stored as written"
        );

        append(&path, &ai_title("   "));
        let scan = scan_commit(&claude, &mut state, BUDGET);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert_eq!(
            state.sessions[SESSION].title, None,
            "an all-space title is no title"
        );
    }

    #[test]
    fn an_agents_transcript_does_not_set_the_title() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                answer("msg_a", AT),
            ],
        );
        write_lines(
            &folder(&claude, FOLDER).join(SESSION).join("subagents"),
            "agent-a875.jsonl",
            &[ai_title("Fix the invoice export")],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(state.sessions[SESSION].title, None);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn thinking_since_the_user_is_counted_and_reset() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:58.101Z"),
                assistant_line(
                    "msg_a",
                    "2026-11-12T09:41:07.412Z",
                    ENGINE,
                    OPUS,
                    &["thinking"],
                    USAGE,
                ),
                assistant_line(
                    "msg_b",
                    "2026-11-12T09:41:20.000Z",
                    ENGINE,
                    OPUS,
                    &["tool_use"],
                    USAGE,
                ),
                user_string("and the linter", "2026-11-12T09:42:00.000Z"),
                assistant_line(
                    "msg_c",
                    "2026-11-12T09:42:30.000Z",
                    ENGINE,
                    OPUS,
                    &["redacted_thinking"],
                    USAGE,
                ),
            ],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.user_turns.len(), 2, "{:?}", scan.user_turns);
        let seen = &state.sessions[SESSION];
        assert_eq!(seen.thinking_since_user(), 1);
        assert_eq!(seen.last_user_turn, Some(at("2026-11-12T09:42:00.000Z")));
        assert_eq!(seen.folder, FOLDER);
        assert_eq!(seen.cwd, ENGINE);
        assert_eq!(seen.last_model.as_deref(), Some(OPUS));
        assert_eq!(seen.first, at("2026-11-12T09:40:58.101Z"));
        assert_eq!(seen.last, at("2026-11-12T09:42:30.000Z"));
    }

    #[test]
    fn thinking_is_kept_per_minute() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER);
        write_lines(
            &dir,
            &format!("{SESSION}.jsonl"),
            &[
                user_string("fix the failing tests", "2026-11-12T09:40:57.000Z"),
                assistant_line(
                    "msg_a",
                    "2026-11-12T09:41:07.412Z",
                    ENGINE,
                    OPUS,
                    &["thinking"],
                    USAGE,
                ),
                assistant_line(
                    "msg_b",
                    "2026-11-12T09:41:40.000Z",
                    ENGINE,
                    OPUS,
                    &["thinking"],
                    USAGE,
                ),
                assistant_line(
                    "msg_c",
                    "2026-11-12T09:42:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
                assistant_line(
                    "msg_d",
                    "2026-11-12T09:43:02.000Z",
                    ENGINE,
                    OPUS,
                    &["thinking"],
                    USAGE,
                ),
            ],
        );

        let (mut state, _scan) = scan_run(&claude);
        let seen = &state.sessions[SESSION];
        assert_eq!(
            seen.thinking,
            vec![
                (at("2026-11-12T09:41:00.000Z"), 2),
                (at("2026-11-12T09:43:00.000Z"), 1),
            ]
        );
        assert_eq!(seen.thinking_since_user(), 3);

        append(
            &dir.join(format!("{SESSION}.jsonl")),
            &user_string("and the linter", "2026-11-12T09:44:00.000Z"),
        );
        scan_commit(&claude, &mut state, BUDGET);
        assert!(state.sessions[SESSION].thinking.is_empty());
        assert_eq!(state.sessions[SESSION].thinking_since_user(), 0);
    }

    #[test]
    fn synthetic_answers_are_skipped() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[assistant_line(
                "msg_synthetic",
                AT,
                ENGINE,
                SYNTHETIC,
                &["text"],
                USAGE,
            )],
        );

        let (_state, scan) = scan_run(&claude);

        assert!(scan.answers.is_empty());
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn an_agent_answer_belongs_to_its_session() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        write_lines(&dir, "agent-a875.jsonl", &[answer("msg_agent", AT)]);

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        assert_eq!(scan.answers[0].session, SESSION);
        assert_eq!(scan.answers[0].agent.as_deref(), Some("a875"));
        assert!(
            !state.sessions.contains_key(SESSION),
            "an agent's line never makes a session"
        );
    }

    #[test]
    fn agents_of_reads_the_meta_files() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("agent-a875.meta.json"),
            meta_of(Some(true), "toolu_01Nkn4", 1),
        )
        .unwrap();

        // No scan state here, so no call is known: no agent has a parent.
        let agents = agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut AgentEnds::default(),
        );

        assert!(agents.warnings.is_empty(), "{:?}", agents.warnings);
        assert_eq!(
            agents.metas,
            vec![AgentMeta {
                session: SESSION.to_string(),
                agent: "a875".to_string(),
                agent_type: Some("general-purpose".to_string()),
                description: Some("fix the failing tests".to_string()),
                finished: true,
                spawn_depth: Some(1),
                tool_use_id: Some("toolu_01Nkn4".to_string()),
                parent: None,
                transcript_ended: false,
            }]
        );
    }

    #[test]
    fn an_unreadable_meta_file_is_a_warning_and_the_agent_is_running() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-a875.meta.json"), "{\"agentType\": ").unwrap();

        let agents = agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut AgentEnds::default(),
        );

        assert_eq!(agents.metas.len(), 1, "{:?}", agents.warnings);
        assert!(
            !agents.metas[0].finished,
            "an unreadable meta cannot say the agent finished"
        );
        assert_eq!(agents.metas[0].spawn_depth, None);
        assert_eq!(agents.warnings.len(), 1);
        assert!(
            agents.warnings[0].contains("agent-a875.meta.json"),
            "the warning names the file: {:?}",
            agents.warnings
        );
    }

    #[test]
    fn an_agent_without_finished_is_still_running() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("agent-a875.meta.json"),
            meta_of(None, "toolu_01Nkn4", 1),
        )
        .unwrap();

        // No scan state here, so no call is known: no agent has a parent.
        let agents = agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut AgentEnds::default(),
        );

        assert_eq!(agents.metas.len(), 1);
        assert!(!agents.metas[0].finished);
    }

    /// An unreadable meta still cannot say the agent finished; the end flag
    /// comes from the transcript beside it.
    #[test]
    fn an_unreadable_meta_gets_the_transcript_end_flag() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-a875.meta.json"), "{\"agentType\": ").unwrap();
        write_lines(
            &dir,
            "agent-a875.jsonl",
            &[user_string("fix the failing tests", AT)],
        );

        let agents = agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut AgentEnds::default(),
        );

        assert_eq!(agents.metas.len(), 1, "{:?}", agents.warnings);
        assert!(!agents.metas[0].finished);
        assert!(
            !agents.metas[0].transcript_ended,
            "the last entry is a user turn, not an end_turn answer"
        );
        assert_eq!(agents.warnings.len(), 1, "the meta is still a warning");
    }

    #[test]
    fn agent_ended_ignores_a_torn_last_line() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a875.jsonl");
        let mut text = format!("{}\n", assistant_ending("msg_agent", AT));
        fs::write(&path, &text).unwrap();
        assert!(
            agent_ended(&path),
            "the last whole line is the end_turn one"
        );

        // The file is being written: its last line has no newline yet, so the
        // end_turn line before it decides.
        text.push_str("{\"type\":\"assistant\"");
        fs::write(&path, &text).unwrap();
        assert!(
            agent_ended(&path),
            "a torn last line is ignored, the line before it decides"
        );

        // The torn line becomes whole: it is not a complete `end_turn` line, so
        // the agent did not end.
        text.push('\n');
        fs::write(&path, &text).unwrap();
        assert!(!agent_ended(&path));

        write_lines(
            &dir,
            "agent-a875.jsonl",
            &[
                assistant_ending("msg_agent", AT),
                user_string("fix the failing tests", AT),
            ],
        );
        assert!(
            !agent_ended(&path),
            "a new user turn after the answer means the agent runs again"
        );
        assert!(
            !agent_ended(&dir.join("agent-c222.jsonl")),
            "a missing file"
        );
    }

    #[test]
    fn agent_ended_ignores_blank_lines_at_the_end() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a875.jsonl");
        fs::write(&path, format!("{}\n\n", assistant_ending("msg_agent", AT))).unwrap();

        assert!(
            agent_ended(&path),
            "a blank line at the end is not an entry; the end_turn line decides"
        );
    }

    #[test]
    fn a_finished_meta_does_not_read_the_transcript() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("agent-a875.meta.json"),
            meta_of(Some(true), "toolu_01Nkn4", 1),
        )
        .unwrap();
        // The transcript ends with an end_turn answer, so reading it would set
        // the flag; the meta's `finished` already answers the rule, so it is not
        // read and the flag stays false.
        write_lines(
            &dir,
            "agent-a875.jsonl",
            &[assistant_ending("msg_agent", AT)],
        );

        let agents = agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut AgentEnds::default(),
        );

        assert!(agents.metas[0].finished);
        assert!(
            !agents.metas[0].transcript_ended,
            "a finished meta is not read for its transcript's end"
        );
        assert!(agents.metas[0].is_done());
    }

    #[test]
    fn an_unchanged_transcript_end_is_not_read_again() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        let path = write_lines(
            &dir,
            "agent-a875.jsonl",
            &[assistant_ending("msg_agent", AT)],
        );
        let mut ends = AgentEnds::default();
        let mut reads = 0;

        assert!(ends.ended_by(&path, |path| {
            reads += 1;
            agent_ended(path)
        }));
        assert_eq!(reads, 1);
        assert!(
            ends.ended_by(&path, |path| {
                reads += 1;
                agent_ended(path)
            }),
            "the cached answer is the same"
        );
        assert_eq!(reads, 1, "an unchanged transcript is not read again");

        append(&path, &user_string("fix the failing tests", AT));
        assert!(!ends.ended_by(&path, |path| {
            reads += 1;
            agent_ended(path)
        }));
        assert_eq!(reads, 2, "a changed transcript is read again");
    }

    /// An answer asked for in a pass is kept into the next one; an answer no
    /// one asked for during a whole pass is forgotten, so the cache holds only
    /// the agents of the sessions still looked at.
    #[test]
    fn a_transcript_end_not_asked_for_in_a_pass_is_forgotten() {
        let (_tmp, claude, _app) = fixture();
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        let kept = write_lines(
            &dir,
            "agent-a875.jsonl",
            &[assistant_ending("msg_agent", AT)],
        );
        let dropped = write_lines(
            &dir,
            "agent-b412.jsonl",
            &[assistant_ending("msg_other", AT)],
        );
        let mut ends = AgentEnds::default();
        let mut reads = 0;
        let mut ask = |ends: &mut AgentEnds, path: &Path| {
            ends.ended_by(path, |path| {
                reads += 1;
                agent_ended(path)
            })
        };

        ends.start_pass();
        assert!(ask(&mut ends, &kept));
        assert!(ask(&mut ends, &dropped));
        assert_eq!(ends.len(), 2);

        ends.start_pass();
        assert!(ask(&mut ends, &kept), "kept from the pass before");
        assert_eq!(ends.len(), 2, "the other answer waits one pass");

        ends.start_pass();
        assert_eq!(ends.len(), 1, "only the answer asked for in the last pass");
        assert!(ask(&mut ends, &kept));
        assert_eq!(reads, 2, "the kept transcript was read once");
    }

    #[test]
    fn an_agent_is_done_when_finished_or_its_transcript_ended() {
        let mut meta = AgentMeta {
            session: SESSION.to_string(),
            agent: "a875".to_string(),
            agent_type: None,
            description: None,
            finished: true,
            spawn_depth: None,
            tool_use_id: None,
            parent: None,
            transcript_ended: false,
        };
        assert!(meta.is_done());
        meta.finished = false;
        meta.transcript_ended = true;
        assert!(meta.is_done());
        meta.transcript_ended = false;
        assert!(!meta.is_done(), "neither mark says the agent is done");
    }

    #[test]
    fn the_budget_stops_and_the_next_scan_continues() {
        let (_tmp, claude, _app) = fixture();
        let mut ids = Vec::new();
        for session in [SESSION, SESSION_B] {
            let lines: Vec<String> = (0..4)
                .map(|n| {
                    let id = format!("msg_{}", &session[..8]);
                    let id = format!("{id}_{n}");
                    ids.push(id.clone());
                    answer_of_len(&id, AT, 249)
                })
                .collect();
            let path = write_lines(
                &folder(&claude, FOLDER),
                &format!("{session}.jsonl"),
                &lines,
            );
            assert_eq!(fs::metadata(&path).unwrap().len(), 1000);
        }

        let mut state = ScanState::default();
        let first = scan_commit(&claude, &mut state, 1200);
        assert!(!first.complete, "the budget stops the first scan");
        let second = scan_commit(&claude, &mut state, 1200);
        assert!(second.complete, "the second scan reads the rest");

        let mut seen: Vec<String> = first
            .answers
            .iter()
            .chain(second.answers.iter())
            .map(|answer| answer.message_id.clone())
            .collect();
        seen.sort();
        let mut expected = ids;
        expected.sort();
        assert_eq!(seen, expected, "every answer exactly once");
    }

    #[test]
    fn only_the_folders_that_changed_are_saved() {
        let (_tmp, claude, app) = fixture();
        let atlas_path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        write_lines(
            &folder(&claude, FOLDER_B),
            &format!("{SESSION_B}.jsonl"),
            &[assistant_line(
                "msg_b",
                "2026-11-12T10:00:00.000Z",
                "/home/sam/projects/ledger",
                OPUS,
                &["text"],
                USAGE,
            )],
        );
        let mut state = ScanState::default();
        scan_commit(&claude, &mut state, BUDGET);
        assert_eq!(state.save(&claude, &app, Utc::now()).unwrap(), 2);

        let state_dir = app.transcripts_state_dir();
        let atlas_file = state_dir.join(format!("{FOLDER}.json"));
        let other_file = state_dir.join(format!("{FOLDER_B}.json"));
        assert!(atlas_file.exists() && other_file.exists());
        let other_bytes = fs::read(&other_file).unwrap();
        let other_mtime = fs::metadata(&other_file).unwrap().modified().unwrap();

        // One file per project folder, with the short shape the plan gives.
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&atlas_file).unwrap()).unwrap();
        assert_eq!(written["version"], 2);
        assert!(written["files"][KEY]["offset"].is_u64());
        assert!(written["files"][KEY]["grew"].is_string());
        assert!(written["files"][KEY]["recent"].is_array());
        let seen = &written["sessions"][SESSION];
        assert!(seen["folder"].is_string());
        assert!(seen["lastModel"].is_string());
        assert!(seen["lastUserTurn"].is_null());
        assert_eq!(
            seen["thinking"],
            serde_json::json!([["2026-11-12T09:41:00Z", 1]])
        );

        append(
            &atlas_path,
            &assistant_line(
                "msg_c",
                "2026-11-12T09:42:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
        );
        scan_commit(&claude, &mut state, BUDGET);
        assert_eq!(state.save(&claude, &app, Utc::now()).unwrap(), 1);

        assert_eq!(fs::read(&other_file).unwrap(), other_bytes);
        assert_eq!(
            fs::metadata(&other_file).unwrap().modified().unwrap(),
            other_mtime,
            "a folder that did not change is not written"
        );
        let loaded = ScanState::load(&app).unwrap();
        assert_eq!(loaded.files, state.files);
        assert_eq!(loaded.sessions, state.sessions);
    }

    #[test]
    fn a_file_that_stopped_growing_drops_its_ids() {
        let (_tmp, claude, app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, _scan) = scan_run(&claude);
        let offset = state.files[KEY].offset;
        assert!(!state.files[KEY].recent.is_empty());

        let wrote = state
            .save(&claude, &app, Utc::now() + Duration::hours(25))
            .unwrap();

        assert_eq!(wrote, 1);
        assert!(state.files[KEY].recent.is_empty(), "the ids are dropped");
        assert_eq!(state.files[KEY].offset, offset, "the offset does not move");
    }

    #[test]
    fn a_replaced_old_file_is_not_counted_again() {
        let (_tmp, claude, app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, _scan) = scan_run(&claude);
        state
            .save(&claude, &app, Utc::now() + Duration::hours(25))
            .unwrap();
        assert!(state.files[KEY].recent.is_empty());

        let bytes = fs::read(&path).unwrap();
        let half = bytes.len() / 2;
        fs::write(&path, &bytes[..half]).unwrap();
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(
            second.warnings,
            vec![format!(
                "{KEY} was replaced after it had been read; its new content is not counted"
            )]
        );
        let last_newline = bytes[..half].iter().rposition(|byte| *byte == b'\n');
        assert_eq!(
            state.files[KEY].offset,
            last_newline.map_or(0, |index| index as u64 + 1),
            "the offset is the end of the last whole line within the first half"
        );
    }

    #[test]
    fn entries_of_deleted_transcripts_are_dropped() {
        let (_tmp, claude, app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, _scan) = scan_run(&claude);
        assert!(state.files.contains_key(KEY));
        assert!(state.sessions.contains_key(SESSION));

        fs::remove_file(&path).unwrap();
        state.save(&claude, &app, Utc::now()).unwrap();

        assert!(!state.files.contains_key(KEY), "{:?}", state.files);
        assert!(
            !state.sessions.contains_key(SESSION),
            "{:?}",
            state.sessions
        );
    }

    #[test]
    fn a_windows_cwd_gets_one_spelling() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line("msg_a", AT, r"c:\Users\sam\atlas", OPUS, &["text"], USAGE),
                assistant_line(
                    "msg_b",
                    "2026-11-12T09:42:00.000Z",
                    r"C:\Users\sam\atlas",
                    OPUS,
                    &["text"],
                    USAGE,
                ),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 2);
        for answer in &scan.answers {
            assert_eq!(answer.cwd, r"C:\Users\sam\atlas");
        }
    }

    #[test]
    fn token_records_group_by_session_hour_and_model() {
        let (tmp, claude, _app) = fixture();
        let home = tmp.path();
        let repo = home.join("projects/atlas");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(
            repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/sam/atlas.git\n",
        )
        .unwrap();
        let engine = repo.join("engine");
        fs::create_dir_all(&engine).unwrap();
        let engine = engine.to_str().unwrap().to_string();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line(
                    "msg_1",
                    "2026-11-12T09:41:07.412Z",
                    &engine,
                    OPUS,
                    &["text"],
                    (2, 100, 26173, 28126),
                ),
                assistant_line(
                    "msg_2",
                    "2026-11-12T09:55:00.000Z",
                    &engine,
                    OPUS,
                    &["text"],
                    (2, 66, 26173, 28126),
                ),
                assistant_line(
                    "msg_3",
                    "2026-11-12T10:05:00.000Z",
                    &engine,
                    OPUS,
                    &["text"],
                    (2, 50, 26173, 28126),
                ),
            ],
        );
        write_lines(
            &folder(&claude, FOLDER).join(SESSION).join("subagents"),
            "agent-a875.jsonl",
            &[assistant_line(
                "msg_4",
                "2026-11-12T09:50:00.000Z",
                &engine,
                SONNET,
                &["text"],
                (2, 20, 26173, 28126),
            )],
        );

        let (state, scan) = scan_run(&claude);
        let records = token_records(&scan.answers, &state, home);

        assert_eq!(records.len(), 4, "{records:?}");
        let nine_forty = at("2026-11-12T09:40:00Z");
        let nine_fifty = at("2026-11-12T09:50:00Z");
        let nine_fifty_five = at("2026-11-12T09:55:00Z");
        let ten_five = at("2026-11-12T10:05:00Z");
        let opus = records
            .iter()
            .find(|record| record.t == nine_forty && record.model == OPUS)
            .unwrap();
        assert_eq!(opus.session, SESSION);
        assert_eq!(opus.cwd, engine);
        assert_eq!(opus.out, 100);
        assert_eq!(opus.answers, 1);
        assert_eq!(opus.agent_answers, 0);
        assert_eq!(opus.minutes, TOKEN_BUCKET_MINUTES as u32);
        assert_eq!(
            opus.project,
            ProjectKey::Repo("github.com/sam/atlas".to_string())
        );
        let sonnet = records
            .iter()
            .find(|record| record.t == nine_fifty && record.model == SONNET)
            .unwrap();
        assert_eq!(sonnet.answers, 1);
        assert_eq!(sonnet.agent_answers, 1);
        assert_eq!(sonnet.out, 20);
        assert_eq!(sonnet.cwd, engine, "an agent uses its session's folder");
        let later = records.iter().find(|record| record.t == ten_five).unwrap();
        assert_eq!(later.answers, 1);
        assert_eq!(later.out, 50);
        let nine_fifty_five = records
            .iter()
            .find(|record| record.t == nine_fifty_five)
            .unwrap();
        assert_eq!(nine_fifty_five.answers, 1);
        assert_eq!(nine_fifty_five.out, 66);
        assert_eq!(records[0].t, nine_forty);
        assert_eq!(records[3].t, ten_five);
    }

    #[test]
    fn five_minute_buckets_group_answers_by_session_model_and_bucket() {
        let (tmp, claude, _app) = fixture();
        let home = tmp.path();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line(
                    "msg_1",
                    "2026-11-12T10:01:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
                assistant_line(
                    "msg_2",
                    "2026-11-12T10:04:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
                assistant_line(
                    "msg_3",
                    "2026-11-12T10:06:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
            ],
        );

        let (state, scan) = scan_run(&claude);
        let records = token_records(&scan.answers, &state, home);

        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[0].t, at("2026-11-12T10:00:00Z"));
        assert_eq!(records[0].minutes, TOKEN_BUCKET_MINUTES as u32);
        assert_eq!(records[0].answers, 2);
        assert_eq!(records[1].t, at("2026-11-12T10:05:00Z"));
        assert_eq!(records[1].minutes, TOKEN_BUCKET_MINUTES as u32);
        assert_eq!(records[1].answers, 1);

        let line = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(line["min"], serde_json::json!(TOKEN_BUCKET_MINUTES));
    }

    #[test]
    fn nothing_but_numbers_and_names_leaves_the_parser() {
        let (_tmp, claude, _app) = fixture();
        let line = assistant_line("msg_a", AT, ENGINE, OPUS, &["text"], USAGE)
            .replace("fix the failing tests", "SECRET-TEXT");
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[line],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1);
        let printed = format!("{scan:?} {state:?}");
        assert!(
            !printed.contains("SECRET-TEXT"),
            "message text never leaves the parser"
        );
    }

    #[test]
    fn a_reread_file_does_not_repeat_user_turns() {
        let (_tmp, claude, _app) = fixture();
        let user = user_string("fix the failing tests", "2026-11-12T09:40:58.101Z");
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[user.clone(), answer("msg_a", AT)],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.user_turns.len(), 1);
        assert_eq!(first.answers.len(), 1);

        fs::write(&path, format!("{user}\n")).unwrap();
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert!(second.user_turns.is_empty(), "{:?}", second.user_turns);
        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert!(second.warnings[0].contains("shrank"));
    }

    #[test]
    fn a_line_longer_than_the_budget_is_still_read() {
        let (_tmp, claude, _app) = fixture();
        let line = answer_of_len("msg_long", AT, 3000);
        assert_eq!(line.len(), 3000);
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[line],
        );

        let mut state = ScanState::default();
        let scan = scan_commit(&claude, &mut state, 1000);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        assert!(scan.complete, "one long line is read whole");
    }

    #[test]
    fn a_file_of_another_session_is_its_own_session() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION_B}.jsonl"),
            &[answer("msg_b", AT)],
        );

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 2);
        assert_eq!(state.sessions.len(), 2);
    }

    #[test]
    fn a_broken_assistant_line_is_counted_in_one_warning() {
        let (_tmp, claude, _app) = fixture();
        let broken = serde_json::json!({"type": "assistant", "timestamp": AT}).to_string();
        let not_json = "{\"type\": \"assistant\",".to_string();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[broken.clone(), not_json, answer("msg_a", AT)],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1);
        assert_eq!(
            scan.warnings,
            vec![format!("{KEY}: 2 lines could not be read")]
        );
    }

    #[test]
    fn lines_of_other_types_with_other_shapes_are_quiet() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                serde_json::json!({"type": "system", "message": "compacted"}).to_string(),
                serde_json::json!({"type": "attachment", "timestamp": 1794612960}).to_string(),
                serde_json::json!({"type": "summary", "cwd": {"x": 1}}).to_string(),
                answer("msg_a", AT),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    #[test]
    fn blank_lines_and_a_byte_order_mark_are_quiet() {
        let (_tmp, claude, _app) = fixture();
        let first = answer("msg_a", AT);
        let second = assistant_line(
            "msg_b",
            "2026-11-12T09:42:00.000Z",
            ENGINE,
            OPUS,
            &["text"],
            USAGE,
        );
        let text = format!("\u{feff}{first}\n\n\r\n{second}\n");
        assert!(
            text.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]),
            "the file starts with a byte order mark"
        );
        let path = folder(&claude, FOLDER).join(format!("{SESSION}.jsonl"));
        fs::create_dir_all(folder(&claude, FOLDER)).unwrap();
        fs::write(&path, text.as_bytes()).unwrap();

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 2, "{:?}", scan.answers);
        assert_eq!(scan.answers[1].message_id, "msg_b");
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert_eq!(
            state.files[KEY].offset,
            text.len() as u64,
            "the mark counts for the offset"
        );
    }

    #[test]
    fn a_shrunk_file_with_many_answers_counts_none_again() {
        let (_tmp, claude, _app) = fixture();
        let base = at(AT);
        let lines: Vec<String> = (0..800u32)
            .map(|n| {
                let at = (base + Duration::seconds(i64::from(n)))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                answer(&format!("msg_01QmB7xT{n:014}"), &at)
            })
            .collect();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &lines,
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 800);
        let old_offset = state.files[KEY].offset;

        let mut text = String::new();
        for line in &lines[..lines.len() - 1] {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&path, text.as_bytes()).unwrap();
        assert!(
            (text.len() as u64) < old_offset,
            "the file is shorter than its offset"
        );
        let second = scan_commit(&claude, &mut state, BUDGET);

        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert!(
            second.warnings[0].contains("shrank"),
            "{:?}",
            second.warnings
        );
    }

    #[test]
    fn a_file_replaced_by_a_longer_one_is_read_again() {
        let (_tmp, claude, _app) = fixture();
        let first = vec![
            assistant_line("msg_a", AT, ENGINE, OPUS, &["thinking"], USAGE),
            assistant_line(
                "msg_b",
                "2026-11-12T09:42:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
            assistant_line(
                "msg_c",
                "2026-11-12T09:43:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
        ];
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &first,
        );
        let (mut state, first_scan) = scan_run(&claude);
        assert_eq!(first_scan.answers.len(), 3);
        let old_offset = state.files[KEY].offset as usize;

        // The same three answers, the first line a little longer, and three new ones.
        let longer = first[0].replace(
            "\"requestId\":\"req_011CZk3\"",
            &format!("\"requestId\":\"req_011CZk3{}\"", "x".repeat(40)),
        );
        let lines = [
            longer,
            first[1].clone(),
            first[2].clone(),
            assistant_line(
                "msg_d",
                "2026-11-12T09:44:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
            assistant_line(
                "msg_e",
                "2026-11-12T09:45:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
            assistant_line(
                "msg_f",
                "2026-11-12T09:46:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
        ];
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&path, text.as_bytes()).unwrap();
        assert!(
            text.len() > old_offset,
            "the file is longer than its offset"
        );
        assert_ne!(
            text.as_bytes()[old_offset - 1],
            b'\n',
            "the old offset falls inside a line"
        );

        let second = scan_commit(&claude, &mut state, BUDGET);

        let mut ids: Vec<&str> = second
            .answers
            .iter()
            .map(|answer| answer.message_id.as_str())
            .collect();
        ids.sort();
        assert_eq!(ids, ["msg_d", "msg_e", "msg_f"], "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert!(
            second.warnings[0].contains("replaced"),
            "the file was replaced, not read from the middle: {:?}",
            second.warnings
        );
    }

    #[test]
    fn a_long_line_is_read_whole() {
        let (_tmp, claude, _app) = fixture();
        let long = answer_of_len("msg_long", "2026-11-12T09:42:00.000Z", 5 * 1024 * 1024);
        assert_eq!(long.len(), 5 * 1024 * 1024);
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                answer("msg_a", AT),
                long,
                assistant_line(
                    "msg_b",
                    "2026-11-12T09:43:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 3, "{:?}", scan.answers);
        assert_eq!(scan.answers[1].message_id, "msg_long");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_transcript_folder_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, claude, app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, scan) = scan_run(&claude);
        assert_eq!(scan.answers.len(), 1);
        let offset = state.files[KEY].offset;

        let dir = folder(&claude, FOLDER);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let result = state.save(&claude, &app, Utc::now());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(&result, Err(TranscriptError::Io { .. })),
            "{result:?}"
        );
        assert_eq!(state.files[KEY].offset, offset, "the entry is still there");
    }

    #[test]
    fn a_reread_counts_two_new_answers_with_the_same_time() {
        let (_tmp, claude, _app) = fixture();
        let old_b = "2026-11-12T09:42:00.000Z";
        let new_at = "2026-11-12T09:43:00.000Z";
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                answer_of_len("msg_old_a", AT, 3000),
                answer_of_len("msg_old_b", old_b, 3000),
            ],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 2);
        let old_offset = state.files[KEY].offset;

        // The file shrinks, keeping the old answers and adding two new ones that
        // share a time later than the old last_answer.
        let text: String = [
            answer("msg_old_a", AT),
            answer("msg_old_b", old_b),
            answer("msg_new_a", new_at),
            answer("msg_new_b", new_at),
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
        assert!(
            (text.len() as u64) < old_offset,
            "the file is shorter than its offset"
        );
        fs::write(&path, text.as_bytes()).unwrap();
        let second = scan_commit(&claude, &mut state, BUDGET);

        let mut ids: Vec<&str> = second
            .answers
            .iter()
            .map(|answer| answer.message_id.as_str())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            ["msg_new_a", "msg_new_b"],
            "both new answers, none of the old: {:?}",
            second.answers
        );
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert!(
            second.warnings[0].contains("shrank"),
            "{:?}",
            second.warnings
        );
    }

    #[test]
    fn a_replaced_file_ending_inside_a_line_warns_once() {
        let (_tmp, claude, app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 1);
        state
            .save(&claude, &app, Utc::now() + Duration::hours(25))
            .unwrap();
        assert!(state.files[KEY].recent.is_empty());

        let bytes = fs::read(&path).unwrap();
        let half = bytes.len() / 2;
        fs::write(&path, &bytes[..half]).unwrap();
        assert_ne!(bytes[half - 1], b'\n', "the new file ends inside a line");

        let second = scan_commit(&claude, &mut state, BUDGET);
        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert_eq!(state.files[KEY].offset, 0, "no whole line was read");

        let third = scan_commit(&claude, &mut state, BUDGET);
        assert!(third.answers.is_empty(), "{:?}", third.answers);
        assert!(third.warnings.is_empty(), "{:?}", third.warnings);
    }

    #[test]
    fn a_file_replaced_by_a_longer_one_without_ids_is_not_counted() {
        let (_tmp, claude, app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 1);
        state
            .save(&claude, &app, Utc::now() + Duration::hours(25))
            .unwrap();
        assert!(state.files[KEY].recent.is_empty());
        let old_offset = state.files[KEY].offset as usize;

        let longer = answer("msg_a", AT).replace(
            "\"requestId\":\"req_011CZk3\"",
            &format!("\"requestId\":\"req_011CZk3{}\"", "x".repeat(100)),
        );
        let text: String = [
            longer,
            assistant_line(
                "msg_b",
                "2026-11-12T09:42:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                USAGE,
            ),
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
        assert!(
            text.len() > old_offset,
            "the file is longer than its offset"
        );
        assert_ne!(
            text.as_bytes()[old_offset - 1],
            b'\n',
            "the old offset falls inside a line"
        );
        fs::write(&path, text.as_bytes()).unwrap();

        let second = scan_commit(&claude, &mut state, BUDGET);
        assert!(second.answers.is_empty(), "{:?}", second.answers);
        assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
        assert!(
            second.warnings[0].contains("is not counted"),
            "{:?}",
            second.warnings
        );
        assert_eq!(
            state.files[KEY].offset,
            text.len() as u64,
            "the offset is the end of the last whole line"
        );

        let third = scan_commit(&claude, &mut state, BUDGET);
        assert!(third.answers.is_empty(), "{:?}", third.answers);
        assert!(third.warnings.is_empty(), "{:?}", third.warnings);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, claude, app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[answer("msg_a", AT)],
        );
        write_lines(
            &folder(&claude, FOLDER_B),
            &format!("{SESSION_B}.jsonl"),
            &[answer("msg_b", AT)],
        );
        let (mut state, scan) = scan_run(&claude);
        assert_eq!(scan.answers.len(), 2);
        assert!(!state.files[KEY].recent.is_empty());

        let unreadable = folder(&claude, FOLDER_B);
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let result = state.save(&claude, &app, Utc::now() + Duration::hours(25));
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(&result, Err(TranscriptError::Io { .. })),
            "{result:?}"
        );
        assert!(
            !state.files[KEY].recent.is_empty(),
            "a failed save changed a folder before it failed"
        );
    }

    #[test]
    fn a_crlf_split_across_two_chunks_is_one_line() {
        let (_tmp, claude, _app) = fixture();
        let json = answer_of_len("msg_crlf", AT, CHUNK_BYTES - 1);
        assert_eq!(json.len(), CHUNK_BYTES - 1);
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[format!("{json}\r")],
        );
        let len = fs::metadata(&path).unwrap().len();
        assert_eq!(len, CHUNK_BYTES as u64 + 1);
        assert_eq!(fs::read(&path).unwrap()[CHUNK_BYTES - 1], b'\r');
        assert_eq!(fs::read(&path).unwrap()[CHUNK_BYTES], b'\n');

        let (state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 1, "{:?}", scan.answers);
        assert_eq!(scan.answers[0].message_id, "msg_crlf");
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
        assert_eq!(
            state.files[KEY].offset, len,
            "the offset is the file length"
        );
    }

    #[test]
    fn a_line_of_spaces_is_quiet() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                answer("msg_a", AT),
                "   \t".to_string(),
                assistant_line(
                    "msg_b",
                    "2026-11-12T09:42:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    USAGE,
                ),
            ],
        );

        let (_state, scan) = scan_run(&claude);

        assert_eq!(scan.answers.len(), 2, "{:?}", scan.answers);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    }

    /// Two answers of one agent: theirs is the agent's count and part of the
    /// session's.
    #[test]
    fn agent_sums_are_kept_per_agent() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[assistant_line(
                "msg_main",
                AT,
                ENGINE,
                OPUS,
                &["text"],
                (2, 100, 26173, 28126),
            )],
        );
        write_lines(
            &folder(&claude, FOLDER).join(SESSION).join("subagents"),
            "agent-a875.jsonl",
            &[
                assistant_line(
                    "msg_agent_a",
                    "2026-11-12T09:42:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    (2, 300, 0, 0),
                ),
                assistant_line(
                    "msg_agent_b",
                    "2026-11-12T09:43:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    (2, 500, 0, 0),
                ),
            ],
        );

        let (state, _scan) = scan_run(&claude);

        let seen = &state.sessions[SESSION];
        let agent = &seen.agents["a875"];
        assert_eq!(agent.tokens.out, 800);
        assert_eq!(agent.answers, 2);
        assert_eq!(agent.first, at("2026-11-12T09:42:00.000Z"));
        assert_eq!(agent.last, at("2026-11-12T09:43:00.000Z"));
        assert_eq!(agent.model.as_deref(), Some(OPUS));
        assert_eq!(
            seen.totals.out, 900,
            "the session's answers and its agent's"
        );
        assert_eq!(seen.by_model[OPUS].out, 900);
    }

    #[test]
    fn an_agent_started_by_an_agent_has_it_as_parent() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[agent_call("msg_main", AT, "toolu_01Nkn4", "Agent")],
        );
        let dir = folder(&claude, FOLDER).join(SESSION).join("subagents");
        write_lines(
            &dir,
            "agent-a875.jsonl",
            &[agent_call(
                "msg_agent",
                "2026-11-12T09:42:00.000Z",
                "toolu_02",
                "Agent",
            )],
        );
        fs::write(
            dir.join("agent-b111.meta.json"),
            meta_of(Some(true), "toolu_02", 2),
        )
        .unwrap();
        fs::write(
            dir.join("agent-c222.meta.json"),
            meta_of(Some(true), "toolu_01Nkn4", 1),
        )
        .unwrap();

        let (state, _scan) = scan_run(&claude);
        let agents = agents_of(
            &claude,
            SESSION,
            &state.sessions[SESSION].spawns,
            &mut AgentEnds::default(),
        );
        let metas = agents.metas;

        let started_by_agent = metas
            .iter()
            .find(|meta| meta.agent == "b111")
            .expect("the meta of b111");
        assert_eq!(started_by_agent.tool_use_id.as_deref(), Some("toolu_02"));
        assert_eq!(started_by_agent.parent.as_deref(), Some("a875"));
        let started_by_session = metas
            .iter()
            .find(|meta| meta.agent == "c222")
            .expect("the meta of c222");
        assert_eq!(
            started_by_session.parent, None,
            "its call is in the main transcript"
        );
    }

    #[test]
    fn the_last_hour_forgets_older_minutes() {
        let (_tmp, claude, _app) = fixture();
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line(
                    "msg_early",
                    "2026-11-12T09:05:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    (0, 100, 0, 0),
                ),
                assistant_line(
                    "msg_late",
                    "2026-11-12T09:50:00.000Z",
                    ENGINE,
                    OPUS,
                    &["text"],
                    (0, 200, 0, 0),
                ),
            ],
        );

        let (mut state, _scan) = scan_run(&claude);
        let seen = &state.sessions[SESSION];
        assert_eq!(last_hour(seen, at("2026-11-12T10:10:00Z")).out, 200);
        assert_eq!(last_hour(seen, at("2026-11-12T10:55:00Z")).out, 0);
        assert_eq!(seen.recent.len(), 2, "09:05 and 09:50");

        // A hundred more minutes: every minute older than an hour is dropped.
        let first = at("2026-11-12T11:00:00Z");
        for minute in 0..100 {
            let when = first + Duration::minutes(minute);
            append(
                &path,
                &assistant_line(
                    &format!("msg_minute_{minute}"),
                    &when.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    ENGINE,
                    OPUS,
                    &["text"],
                    (0, 1, 0, 0),
                ),
            );
        }
        scan_commit(&claude, &mut state, BUDGET);

        assert!(
            state.sessions[SESSION].recent.len() <= 60,
            "the last hour holds at most 60 minutes, not {}",
            state.sessions[SESSION].recent.len()
        );
        assert_eq!(state.sessions[SESSION].recent.len(), 60);
    }

    #[test]
    fn a_closed_session_leaves_the_state_after_an_hour() {
        let (_tmp, claude, app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[assistant_line(
                "msg_a",
                AT,
                ENGINE,
                OPUS,
                &["text"],
                (0, 100, 0, 0),
            )],
        );
        let (mut state, _scan) = scan_run(&claude);
        let last = state.sessions[SESSION].last;
        state.save(&claude, &app, last).unwrap();
        assert!(state.changed.is_empty(), "the scan's change was written");

        let running = BTreeSet::from([SESSION.to_string()]);
        assert_eq!(state.forget_closed(&running, last + Duration::hours(2)), 0);
        assert!(
            state.sessions.contains_key(SESSION),
            "a running session stays"
        );

        let none = BTreeSet::new();
        assert_eq!(state.forget_closed(&none, last + Duration::minutes(60)), 0);
        assert!(
            state.sessions.contains_key(SESSION),
            "one hour is not more than one hour"
        );

        assert_eq!(state.forget_closed(&none, last + Duration::minutes(61)), 1);
        assert!(!state.sessions.contains_key(SESSION));
        assert!(state.files.contains_key(KEY), "its offset stays");
        assert!(
            state.changed.contains(FOLDER),
            "the next save writes the folder without it"
        );
    }

    #[test]
    fn a_closed_sessions_flash_jobs_are_kept_for_the_harness_window() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[assistant_line(
                "msg_a",
                AT,
                ENGINE,
                OPUS,
                &["text"],
                (0, 100, 0, 0),
            )],
        );
        let (mut state, _scan) = scan_run(&claude);
        let last = state.sessions[SESSION].last;
        let job = "flash-task-5c974a53-178a-4254-93b2-b804a111414a";
        state
            .sessions
            .get_mut(SESSION)
            .expect("the session")
            .flash_jobs
            .insert(job.to_string());

        assert_eq!(
            state.forget_closed(&BTreeSet::new(), last + Duration::hours(2)),
            1
        );

        let link = state.closed_flash_jobs.get(job).expect("the kept job link");
        assert_eq!(link.session, SESSION);
        assert_eq!(link.last, last);

        state.prune_closed_flash_jobs(last + Duration::hours(24), 24);
        assert!(
            state.closed_flash_jobs.contains_key(job),
            "the link is kept for the whole Harness window"
        );
        state.prune_closed_flash_jobs(last + Duration::hours(24) + Duration::seconds(1), 24);
        assert!(
            !state.closed_flash_jobs.contains_key(job),
            "the link goes after the Harness window"
        );
    }

    #[test]
    fn an_old_state_file_without_the_sums_loads() {
        let (_tmp, _claude, app) = fixture();
        let dir = app.transcripts_state_dir();
        fs::create_dir_all(&dir).unwrap();
        // `version: 2` is the current version: this tests a missing field of the
        // current shape (the sums), not a state file of an older version.
        let old = serde_json::json!({
            "version": 2,
            "files": {},
            "sessions": {
                SESSION: {
                    "folder": FOLDER,
                    "cwd": ENGINE,
                    "first": "2026-11-12T09:41:07.412Z",
                    "last": "2026-11-12T09:41:07.412Z",
                    "lastModel": OPUS,
                    "lastUserTurn": null,
                    "thinkingSinceUser": 1
                }
            }
        });
        fs::write(dir.join(format!("{FOLDER}.json")), old.to_string()).unwrap();

        let state = ScanState::load(&app).unwrap();

        let seen = &state.sessions[SESSION];
        assert_eq!(seen.totals, Tokens::default());
        assert!(seen.by_model.is_empty());
        assert!(seen.agents.is_empty());
        assert!(seen.spawns.is_empty());
        assert!(seen.recent.is_empty());
    }

    /// REVIEW PROOF (Z-2, finding 1): a real 1.1.0 state file has per-minute
    /// counts without a model in `recent`. It is migrated: the offsets stay, so
    /// the answers it already recorded are not read again, and only the last
    /// hour (the one thing version 2 cannot derive) starts empty.
    #[test]
    fn review_a_version_1_state_with_minutes_is_migrated() {
        let (_tmp, _claude, app) = fixture();
        let dir = app.transcripts_state_dir();
        fs::create_dir_all(&dir).unwrap();
        let old = serde_json::json!({
            "version": 1,
            "files": {
                KEY: {
                    "offset": 987_u64,
                    "grew": "2026-11-12T09:41:07.412Z",
                    "recent": ["msg_01QmB7xT"],
                    "lastAnswer": "2026-11-12T09:41:07.412Z"
                }
            },
            "sessions": {
                SESSION: {
                    "folder": FOLDER,
                    "cwd": ENGINE,
                    "first": "2026-11-12T09:41:07.412Z",
                    "last": "2026-11-12T09:41:07.412Z",
                    "lastModel": OPUS,
                    "lastUserTurn": null,
                    "thinking": [["2026-11-12T09:41:00Z", 1]],
                    "totals": {"input": 2, "out": 166, "cw": 26173, "cr": 28126},
                    "byModel": {OPUS: {"input": 2, "out": 166, "cw": 26173, "cr": 28126}},
                    "agents": {},
                    "spawns": {},
                    "recent": [["2026-11-12T09:41:00Z", {"input": 2, "out": 166, "cw": 26173, "cr": 28126}]],
                    "recorded": {"last": "2026-11-12T09:41:07.412Z", "closed": true}
                }
            }
        });
        let path = dir.join(format!("{FOLDER}.json"));
        fs::write(&path, old.to_string()).unwrap();

        let state = ScanState::load(&app).expect("version 1 is migrated, not an error");

        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.files[KEY].offset, 987, "the read offset is kept");
        assert_eq!(state.files[KEY].recent, vec!["msg_01QmB7xT".to_string()]);
        assert!(
            state.files[KEY].flash_tools.is_empty(),
            "version 1 named no flash tools"
        );
        let seen = &state.sessions[SESSION];
        assert_eq!(seen.totals.out, 166, "the sums are kept");
        assert_eq!(seen.by_model[OPUS].out, 166);
        assert_eq!(seen.thinking_since_user(), 1);
        assert_eq!(
            seen.recorded,
            Some(RecordedSession {
                last: "2026-11-12T09:41:07.412Z".parse().unwrap(),
                closed: true,
            }),
            "the record already written is still remembered"
        );
        assert!(
            seen.recent.is_empty(),
            "the per-minute tokens have no model and cannot become version 2's"
        );
        assert!(seen.flash_jobs.is_empty(), "version 1 named no flash jobs");
        assert!(
            state.changed.contains(FOLDER),
            "the migrated folder is written back as version 2"
        );
    }

    /// A state file of a version this build does not read is an error naming the
    /// file and both versions, never a silent rebuild: a rebuild is what writes
    /// the history a second time.
    #[test]
    fn a_state_file_of_a_newer_version_is_an_error() {
        let (_tmp, _claude, app) = fixture();
        let dir = app.transcripts_state_dir();
        fs::create_dir_all(&dir).unwrap();
        let newer = serde_json::json!({
            "version": 3,
            "files": {},
            "sessions": {}
        });
        let path = dir.join(format!("{FOLDER}.json"));
        fs::write(&path, newer.to_string()).unwrap();

        let error = ScanState::load(&app).expect_err("version 3 is not read");

        let text = error.to_string();
        assert!(
            text.contains(path.to_str().unwrap()),
            "the file is named: {text}"
        );
        assert!(
            text.contains("version 3"),
            "the found version is named: {text}"
        );
        assert!(
            text.contains("version 2"),
            "this build's version is named: {text}"
        );
        assert!(
            text.contains("newer seatswap"),
            "a higher version says why: {text}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            newer.to_string(),
            "the unsupported file is left as it was"
        );
    }

    /// A transcript state of version 1 is migrated beside the Harness totals,
    /// which live in their own file and must survive the migration.
    #[test]
    fn a_version_1_migration_keeps_the_harness_totals() {
        let (_tmp, _claude, app) = fixture();
        let dir = app.transcripts_state_dir();
        fs::create_dir_all(&dir).unwrap();
        let old = serde_json::json!({
            "version": 1,
            "files": {},
            "sessions": {
                SESSION: {
                    "folder": FOLDER,
                    "cwd": ENGINE,
                    "first": "2026-11-12T09:41:07.412Z",
                    "last": "2026-11-12T09:41:07.412Z",
                    "lastModel": OPUS,
                    "lastUserTurn": null,
                    "totals": {"input": 2, "out": 166, "cw": 26173, "cr": 28126}
                }
            }
        });
        fs::write(dir.join(format!("{FOLDER}.json")), old.to_string()).unwrap();
        let harness = serde_json::json!({
            "version": 2,
            "recorded": {
                SESSION: {
                    "tokens": {"input": 10, "out": 5, "cw": 0, "cr": 0},
                    "messages": 2
                }
            }
        });
        fs::write(dir.join("harness.json"), harness.to_string()).unwrap();

        let state = ScanState::load(&app).expect("a version-1 state is migrated");

        assert_eq!(
            state.sessions[SESSION].totals.out, 166,
            "the version-1 session is migrated too"
        );
        let recorded = state
            .harness_recorded
            .get(SESSION)
            .expect("the Harness totals are kept");
        assert_eq!(recorded.messages, 2);
        assert_eq!(recorded.tokens.input, 10);
        assert!(
            dir.join("harness.json").exists(),
            "a transcript migration must not delete the Harness totals"
        );
    }

    #[test]
    fn a_state_file_without_thinking_holds_none() {
        let (_tmp, _claude, app) = fixture();
        let dir = app.transcripts_state_dir();
        fs::create_dir_all(&dir).unwrap();
        // `version: 2` is the current version: this tests a missing field of the
        // current shape (thinking), not a state file of an older version.
        let saved = serde_json::json!({
            "version": 2,
            "files": {},
            "sessions": {
                SESSION: {
                    "folder": FOLDER,
                    "cwd": ENGINE,
                    "first": "2026-11-12T09:41:07.412Z",
                    "last": "2026-11-12T09:41:07.412Z",
                    "lastModel": OPUS,
                    "lastUserTurn": null
                }
            }
        });
        fs::write(dir.join(format!("{FOLDER}.json")), saved.to_string()).unwrap();

        let state = ScanState::load(&app).unwrap();

        let seen = &state.sessions[SESSION];
        assert!(seen.thinking.is_empty());
        assert_eq!(seen.thinking_since_user(), 0);
    }

    #[test]
    fn an_answer_read_twice_is_summed_once() {
        let (_tmp, claude, _app) = fixture();
        // One answer, two lines with the same message id and the same usage.
        let path = write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[
                assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["thinking"], USAGE),
                assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["tool_use"], USAGE),
            ],
        );

        let (mut state, first) = scan_run(&claude);
        assert_eq!(first.answers.len(), 1, "{:?}", first.answers);
        assert_eq!(state.sessions[SESSION].totals.out, USAGE.1);

        // Its line is read again by the next scan: still counted once.
        append(
            &path,
            &assistant_line("msg_01QmB7xT", AT, ENGINE, OPUS, &["text"], USAGE),
        );
        scan_commit(&claude, &mut state, BUDGET);

        assert_eq!(state.sessions[SESSION].totals.out, USAGE.1);
        assert_eq!(state.sessions[SESSION].by_model[OPUS].out, USAGE.1);
        assert_eq!(state.sessions[SESSION].recent.len(), 1);
    }

    /// The last answer of a session is the newest answer of its agents too:
    /// the sums of a session hold the answers of its agents. A session whose
    /// agent still works must therefore not be forgotten. When it is, the
    /// agent's next answers have no session to be summed into (`sum_answers`
    /// skips them) and their offset moves on: the tokens are gone for good.
    #[test]
    fn a_session_whose_agent_answers_is_not_forgotten() {
        let (_tmp, claude, _app) = fixture();
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &[assistant_line(
                "msg_main",
                AT,
                ENGINE,
                OPUS,
                &["text"],
                (0, 100, 0, 0),
            )],
        );
        let path = write_lines(
            &folder(&claude, FOLDER).join(SESSION).join("subagents"),
            "agent-a875.jsonl",
            &[assistant_line(
                "msg_agent_a",
                AT,
                ENGINE,
                OPUS,
                &["text"],
                (0, 300, 0, 0),
            )],
        );

        let (mut state, _scan) = scan_run(&claude);
        assert_eq!(
            state.sessions[SESSION].totals.out, 400,
            "the agent's answer is part of the session's sums"
        );

        // Two hours later the agent answers again; the main transcript is quiet.
        append(
            &path,
            &assistant_line(
                "msg_agent_b",
                "2026-11-12T11:41:00.000Z",
                ENGINE,
                OPUS,
                &["text"],
                (0, 500, 0, 0),
            ),
        );
        scan_commit(&claude, &mut state, BUDGET);
        assert_eq!(
            state.sessions[SESSION].totals.out, 900,
            "the agent's second answer is summed, too"
        );
        assert_eq!(state.sessions[SESSION].agents["a875"].answers, 2);

        // The session's last answer is the agent's: it is not a closed session.
        assert_eq!(
            state.forget_closed(&BTreeSet::new(), at("2026-11-12T11:42:00Z")),
            0,
            "the session's agent answered a minute ago"
        );
        assert!(
            state.sessions.contains_key(SESSION),
            "a session whose agent answers stays in the state"
        );
    }

    #[test]
    fn proof_flash_batch_and_status_results_name_their_jobs() {
        let (_tmp, claude, _app) = fixture();
        let jobs = [
            (
                "toolu_task",
                "mcp__plugin_flash-agents_flash__flash_task",
                "flash-task-11111111-1111-4111-8111-111111111111",
            ),
            (
                "toolu_result",
                "mcp__plugin_flash-agents_flash__flash_result",
                "flash-task-22222222-2222-4222-8222-222222222222",
            ),
            (
                "toolu_batch",
                "mcp__plugin_flash-agents_flash__flash_batch",
                "flash-task-33333333-3333-4333-8333-333333333333",
            ),
            (
                "toolu_status",
                "mcp__plugin_flash-agents_flash__flash_status",
                "flash-task-44444444-4444-4444-8444-444444444444",
            ),
        ];
        let mut lines = Vec::new();
        for (tool, name, job) in jobs {
            lines.push(agent_call(&format!("msg_{tool}"), AT, tool, name));
            lines.push(user_tool_result_with(
                tool,
                &format!(r#"{{"sessionId":"{job}"}}"#),
                "2026-11-12T09:41:09.730Z",
            ));
        }
        write_lines(
            &folder(&claude, FOLDER),
            &format!("{SESSION}.jsonl"),
            &lines,
        );

        let (state, _scan) = scan_run(&claude);

        let seen = state.sessions.get(SESSION).expect("the session");
        for (_, _, job) in jobs {
            assert!(
                seen.flash_jobs.contains(job),
                "{job}: {:?}",
                seen.flash_jobs
            );
        }
    }

    /// `harness.json` keeps the shape it has always had, byte for byte: the
    /// version, the recorded totals and then the closed job links.
    #[test]
    fn harness_json_is_unchanged() {
        let (_tmp, claude, app) = fixture();
        let mut state = ScanState::default();
        state.set_harness_recorded(&BTreeMap::from([(
            SESSION.to_string(),
            HarnessRecorded {
                tokens: Tokens {
                    input: 2,
                    out: 166,
                    cw: 26173,
                    cr: 28126,
                },
                messages: 2,
                file: Some(PathBuf::from("session.jsonl")),
            },
        )]));
        state.closed_flash_jobs.insert(
            "flash-task-5c974a53-178a-4254-93b2-b804a111414a".to_string(),
            ClosedJob {
                session: SESSION.to_string(),
                last: at(AT),
            },
        );

        let written = state
            .save(&claude, &app, at("2026-11-12T10:00:00Z"))
            .expect("the Harness state is written");

        assert_eq!(written, 1);
        let text = fs::read_to_string(app.transcripts_state_dir().join("harness.json"))
            .expect("the written Harness state");
        assert_eq!(
            text,
            format!(
                r#"{{"version":2,"recorded":{{"{SESSION}":{{"tokens":{{"input":2,"out":166,"cw":26173,"cr":28126}},"messages":2,"file":"session.jsonl"}}}},"closedFlashJobs":{{"flash-task-5c974a53-178a-4254-93b2-b804a111414a":{{"session":"{SESSION}","last":"{AT}"}}}}}}"#
            ),
            "harness.json's bytes are the shape it has always had"
        );
    }

    /// `codex.json` holds the Codex rollouts' totals and reads back what it
    /// wrote.
    #[test]
    fn codex_json_round_trips() {
        let (_tmp, claude, app) = fixture();
        let recorded = BTreeMap::from([(
            SESSION.to_string(),
            CodexRecorded {
                total: Tokens {
                    input: 3500,
                    out: 300,
                    cw: 0,
                    cr: 800,
                },
                last_at: at("2026-11-14T10:07:00Z"),
                file: PathBuf::from("2026/11/14/rollout-2026-11-14T09-55-00-session.jsonl"),
            },
        )]);
        let mut state = ScanState::default();
        state.set_codex_recorded(&recorded);
        state
            .save(&claude, &app, at("2026-11-14T10:08:00Z"))
            .expect("the Codex state is written");

        let loaded = ScanState::load(&app).expect("the Codex state is read");

        assert_eq!(loaded.codex_recorded, recorded);
    }

    /// A recorded total is pruned only when its file is gone and no rollout of
    /// its session id was read in this pass.
    #[test]
    fn a_total_stays_while_another_file_of_its_session_exists() {
        let (_tmp, _claude, app) = fixture();
        let sessions = app.transcripts_state_dir().join("sessions");
        let day = sessions.join("2026").join("11").join("14");
        fs::create_dir_all(&day).expect("the Codex day folder");
        fs::write(
            day.join(format!("rollout-2026-11-14T09-55-00-{SESSION}.jsonl")),
            "",
        )
        .expect("another rollout of the session");
        let mut state = ScanState::default();
        state.codex_recorded.insert(
            SESSION.to_string(),
            CodexRecorded {
                total: Tokens::default(),
                last_at: at("2026-11-14T10:07:00Z"),
                file: PathBuf::from("2026/11/14/gone.jsonl"),
            },
        );
        state.codex_recorded.insert(
            SESSION_B.to_string(),
            CodexRecorded {
                total: Tokens::default(),
                last_at: at("2026-11-14T10:07:00Z"),
                file: PathBuf::from("2026/11/14/also-gone.jsonl"),
            },
        );

        state.prune_codex_recorded(&sessions, &BTreeSet::from([SESSION.to_string()]));

        assert!(
            state.codex_recorded.contains_key(SESSION),
            "another file of the session still exists"
        );
        assert!(
            !state.codex_recorded.contains_key(SESSION_B),
            "a total whose file is gone and whose session was not read goes"
        );
    }
}
