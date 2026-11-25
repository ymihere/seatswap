//! The rows of the Sessions area (spec §3.8): the running sessions of this host
//! and the closed ones from the history, with their agents, tokens and the two
//! values of those tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::{self, Access};
use crate::burn::tied::thinking_tied;
use crate::codex_sessions::CodexSession;
use crate::harness::{self, HarnessSession};
use crate::history::{HistoryDir, HistoryError, Hosted, Hosts, Range, SessionRecord, TokenRecord};
use crate::money::{self, is_plan_model, Fx, Valued, Valuer};
use crate::project::ProjectKey;
use crate::projects::{ProjectId, Projects};
use crate::sessions::{SessionFile, SessionStatus};
use crate::store::Settings;
use crate::transcripts::{self, AgentMeta, AgentSeen, HarnessRecorded, ScanState, Tokens};

/// A session idle longer than this pays nothing extra for a switch (spec V11).
pub const CACHE_LIFE_MIN: i64 = 60;

/// The report is rewritten after this many seconds when the rows changed.
pub const REPORT_EVERY_SECS: i64 = 30;

/// A report older than this many minutes is stale (spec §3.8).
pub const REPORT_STALE_MIN: i64 = 15;

/// The provider of every Claude Code row (spec §3.8). The status line's feed
/// names a Claude Code session, so its cost belongs to these rows.
pub const CLAUDE_PROVIDER: &str = "Claude Code";

/// The reason both values have while an agent's model is not known.
const NO_AGENT_MODEL: &str = "the agent's model is not known yet";

/// Why a sum has no API value when its parts are in two currencies.
const TWO_CURRENCIES: &str =
    "the dollar rate is missing for some of these days, so the values are in two currencies";

/// Why a sum has no subscription value when its parts are valued in two
/// currencies (each host can set its own settings' currency).
const TWO_SUBSCRIPTION_CURRENCIES: &str = "these sessions are valued in two currencies";

/// What a session is doing, as the Sessions area shows it (spec §3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveStatus {
    Busy,
    Idle,
    Waiting,
    Closed,
    Other(String),
}

/// The two values of some tokens, as a page shows them (spec §1.4, §3.6).
///
/// `api` is in `api_currency`: the settings' currency when the record's day has
/// a dollar rate, and "USD" when a currency is set but its rate is not known
/// yet. `subscription` is always in `currency`, the settings' currency. A
/// missing half names its reason with the text of [`crate::money::Missing`];
/// the two halves are never mixed.
///
/// A sum keeps the known part of a half whose other parts are missing (R3): the
/// amount is the known part, `api_partial` / `subscription_partial` is true and
/// the first missing part's reason is in `api_missing` / `subscription_missing`.
/// A half with no known part at all stays `None` with its reason and
/// `partial = false`, as an empty sum stays a known 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Values {
    pub api: Option<f64>,
    pub api_currency: Option<String>,
    pub api_missing: Option<String>,
    #[serde(default)]
    pub api_partial: bool,
    pub subscription: Option<f64>,
    pub currency: Option<String>,
    pub subscription_missing: Option<String>,
    #[serde(default)]
    pub subscription_partial: bool,
}

impl Values {
    /// The one way a list of [`Valued`] becomes what a page shows: one
    /// [`Values`] per record, then [`Values::sum`]. Nothing used is a known 0,
    /// not an unknown value: an empty list is worth 0 in both halves. The API
    /// half takes the currency [`money::api_in_currency`] gives 0 US dollars at
    /// the view's time: the settings' currency with a stored rate, USD while
    /// no rate is stored, and no currency while none is set.
    pub fn of(valued: &[Valued], settings: &Settings, fx: &Fx, at: DateTime<Utc>) -> Values {
        if valued.is_empty() {
            let (api, api_currency, api_missing) = match settings.currency.as_deref() {
                // No currency is not a rate that is missing: nothing used is a
                // known 0 with no currency of its own.
                None => (Some(0.0), None, None),
                Some(_) => match money::api_in_currency(0.0, settings, fx, at.date_naive()) {
                    Ok((amount, currency)) => (Some(amount), Some(currency), None),
                    Err(missing) => (None, None, Some(missing.to_string())),
                },
            };
            return Values {
                api,
                api_currency,
                api_missing,
                api_partial: false,
                subscription: Some(0.0),
                currency: settings.currency.clone(),
                subscription_missing: None,
                subscription_partial: false,
            };
        }
        let parts: Vec<Values> = valued
            .iter()
            .map(|part| Values {
                api: part.api,
                api_currency: part.api_currency.clone(),
                api_missing: part.api_missing.as_ref().map(ToString::to_string),
                api_partial: false,
                subscription: part.subscription,
                currency: settings.currency.clone(),
                subscription_missing: part.subscription_missing.as_ref().map(ToString::to_string),
                subscription_partial: false,
            })
            .collect();
        let parts: Vec<&Values> = parts.iter().collect();
        let mut sum = Values::sum(&parts);
        // Every part was built with the settings' currency.
        sum.currency = settings.currency.clone();
        sum
    }

    /// The sum of built values per R3: each half adds the parts that have it,
    /// and keeps their sum as the known part while at least one part is
    /// missing. Such a half is `partial` with the first missing part's reason;
    /// a half with no known part is `None` with that reason, and `partial` is
    /// false whenever the half is `None`. A part whose half is `None` because
    /// of two currencies keeps that half `None` with that reason, whatever else
    /// is added. Parts whose `api_currency` or `currency` differ are not added,
    /// as before; a part that is itself partial keeps the sum partial. The
    /// currency is the first part's; the view is built with one settings'
    /// currency, so all parts agree. An empty list is a known 0 in both halves,
    /// with no currency of its own.
    pub fn sum(parts: &[&Values]) -> Values {
        let mut api_sum = 0.0;
        let mut api_currency: Option<String> = None;
        let mut api_missing: Option<String> = None;
        let mut api_known = 0usize;
        let mut api_missing_parts = 0usize;
        let mut two_currencies = false;
        let mut api_two_currency_part = false;
        let mut subscription_sum = 0.0;
        let mut subscription_missing: Option<String> = None;
        let mut subscription_known = 0usize;
        let mut subscription_missing_parts = 0usize;
        let mut currency: Option<String> = None;
        let mut two_subscription_currencies = false;
        let mut subscription_two_currency_part = false;
        for part in parts {
            if let Some(part_currency) = &part.currency {
                match &currency {
                    None => currency = Some(part_currency.clone()),
                    Some(first) if first != part_currency => two_subscription_currencies = true,
                    Some(_) => {}
                }
            }
            match part.api {
                None => {
                    api_missing_parts += 1;
                    if part.api_missing.as_deref() == Some(TWO_CURRENCIES) {
                        api_two_currency_part = true;
                    }
                    if api_missing.is_none() {
                        api_missing = part.api_missing.clone();
                    }
                }
                Some(value) => {
                    api_known += 1;
                    api_sum += value;
                    if part.api_partial {
                        api_missing_parts += 1;
                        if api_missing.is_none() {
                            api_missing = part.api_missing.clone();
                        }
                    }
                    match &api_currency {
                        None => api_currency = part.api_currency.clone(),
                        Some(first) if Some(first) != part.api_currency.as_ref() => {
                            two_currencies = true;
                        }
                        Some(_) => {}
                    }
                }
            }
            match part.subscription {
                None => {
                    subscription_missing_parts += 1;
                    if part.subscription_missing.as_deref() == Some(TWO_SUBSCRIPTION_CURRENCIES) {
                        subscription_two_currency_part = true;
                    }
                    if subscription_missing.is_none() {
                        subscription_missing = part.subscription_missing.clone();
                    }
                }
                Some(value) => {
                    subscription_known += 1;
                    subscription_sum += value;
                    if part.subscription_partial {
                        subscription_missing_parts += 1;
                        if subscription_missing.is_none() {
                            subscription_missing = part.subscription_missing.clone();
                        }
                    }
                }
            }
        }
        // An empty list is a known 0; a half with no known part is unknown.
        let mut api = if api_known == 0 && !parts.is_empty() {
            None
        } else {
            Some(api_sum)
        };
        if two_currencies || api_two_currency_part {
            api = None;
            api_missing = Some(TWO_CURRENCIES.to_string());
            api_missing_parts = 0;
        }
        if api.is_none() {
            api_currency = None;
            api_missing_parts = 0;
        }
        let api_partial = api.is_some() && api_missing_parts > 0 && api_known > 0;
        let mut subscription = if subscription_known == 0 && !parts.is_empty() {
            None
        } else {
            Some(subscription_sum)
        };
        if subscription_two_currency_part {
            subscription = None;
            subscription_missing = Some(TWO_SUBSCRIPTION_CURRENCIES.to_string());
            subscription_missing_parts = 0;
            currency = None;
        }
        if two_subscription_currencies {
            if subscription.is_some() {
                subscription = None;
                subscription_missing = Some(TWO_SUBSCRIPTION_CURRENCIES.to_string());
                subscription_missing_parts = 0;
            }
            currency = None;
        }
        let subscription_partial =
            subscription.is_some() && subscription_missing_parts > 0 && subscription_known > 0;
        Values {
            api,
            api_currency,
            api_missing,
            api_partial,
            subscription,
            currency,
            subscription_missing,
            subscription_partial,
        }
    }

    /// Both halves missing with the same reason, and the settings' currency.
    pub(crate) fn missing(reason: &str, settings: &Settings) -> Values {
        Values {
            api: None,
            api_currency: None,
            api_missing: Some(reason.to_string()),
            api_partial: false,
            subscription: None,
            currency: settings.currency.clone(),
            subscription_missing: Some(reason.to_string()),
            subscription_partial: false,
        }
    }

    /// Both halves missing with the same reason and no currency: the pricing
    /// itself is not known, so there is no settings' currency either.
    fn unknown(reason: &str) -> Values {
        Values {
            api: None,
            api_currency: None,
            api_missing: Some(reason.to_string()),
            api_partial: false,
            subscription: None,
            currency: None,
            subscription_missing: Some(reason.to_string()),
            subscription_partial: false,
        }
    }
}

/// How the rows' values are built (spec §3.8): with a valuer and the settings'
/// currency, or not at all because the history, the prices or the dollar rate
/// cannot be read. `Unknown` gives every row's and agent's [`Values`] both
/// halves missing with `reason` and no currency: the two facts the session
/// files alone cannot provide do not keep the rows from being built.
pub enum Pricing<'a> {
    Known {
        valuer: &'a Valuer,
        settings: &'a Settings,
    },
    Unknown {
        reason: String,
    },
}

impl Pricing<'_> {
    /// The values of a list of records for one row or agent.
    fn of(&self, valued: &[Valued]) -> Values {
        match self {
            Pricing::Known { valuer, settings } => {
                Values::of(valued, settings, valuer.fx(), Utc::now())
            }
            Pricing::Unknown { reason } => Values::unknown(reason),
        }
    }

    /// The settings' currency, for the one `Values` built without a valuer.
    fn missing(&self, reason: &str) -> Values {
        match self {
            Pricing::Known { settings, .. } => Values::missing(reason, settings),
            Pricing::Unknown { reason } => Values::unknown(reason),
        }
    }
}

/// One agent of a running session (spec §3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgent {
    pub agent: String,
    pub parent: Option<String>,
    /// The agent's provider: [`CLAUDE_PROVIDER`] for a Claude Code subagent,
    /// [`HARNESS_PROVIDER`] for a flash job and a Harness worker. A report of
    /// an older seatswap has none; [`other_hosts`] gives such an agent its
    /// session's provider, which is what it meant before the field existed.
    pub provider: String,
    pub kind: Option<String>,
    /// The task text, kept in memory only: a Claude Code subagent's meta
    /// description, a flash job's report label, a Harness worker's title
    /// outside a flash job. [`write_report`] clears it; no file holds it.
    pub task: Option<String>,
    pub model: Option<String>,
    pub running: bool,
    /// The agent's first answer; `None` until it answered.
    pub first: Option<DateTime<Utc>>,
    /// The agent's last answer; `None` until it answered.
    pub last: Option<DateTime<Utc>>,
    pub tokens: Tokens,
    pub values: Values,
}

/// One row of the Sessions area (spec §3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    pub session: String,
    pub host: String,
    /// `atlas-a6`: the session's own label, shown as a small grey tag after
    /// its title.
    pub label: String,
    /// Claude Code's own session title, when it has one; the page names the
    /// row by it and shows the label as a tag. `None` for a Harness or Codex
    /// row, and for a Claude Code row without an `ai-title` line. Absent in a
    /// report from a version before this field means none, so a shared
    /// history stays readable.
    #[serde(default)]
    pub title: Option<String>,
    /// The project the row is counted under after joins (spec §3.6). Its name
    /// is `project`; two projects with the same name have different ids.
    pub project_id: ProjectId,
    /// The project's name after joins (spec §3.6).
    pub project: String,
    pub org: Option<String>,
    pub cwd: String,
    /// The row's provider: [`CLAUDE_PROVIDER`] for a Claude Code row.
    pub provider: String,
    pub model: Option<String>,
    pub tied: bool,
    pub status: LiveStatus,
    /// The status's time; for a closed session, when it closed.
    pub since: DateTime<Utc>,
    pub started: DateTime<Utc>,
    pub agents_running: u32,
    pub agents_done: u32,
    pub agents: Vec<LiveAgent>,
    pub tokens: Tokens,
    /// The last hour's tokens; `None` when the row cannot know them
    /// (a Codex total that went down), so a sum over it is unknown too.
    pub last_hour: Option<Tokens>,
    /// The last hour's tokens valued per model, like `values` (spec §3.8).
    /// Closed rows value the token lines that overlap the last hour: each line
    /// is a 5-minute bucket, and an old line without `min` is an hour.
    #[serde(default = "no_values")]
    pub last_hour_values: Values,
    /// [`Values::sum`] of the values of the agents counted done; the sum of an
    /// empty list when no agent is done (a closed row lists no agents).
    #[serde(default = "no_values")]
    pub agents_done_values: Values,
    pub values: Values,
}

/// The values of a row whose report does not carry them (another host still on
/// an older version): named as missing, never a made-up number.
fn no_values() -> Values {
    Values::unknown("the report has no such value")
}

/// The rows of the Sessions area, and the rows that were left out because
/// something they hold is not a valid value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rows {
    pub sessions: Vec<LiveSession>,
    pub warnings: Vec<String>,
}

/// What a switch costs a running session (spec §2.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchKind {
    Free,
    Resend,
    LosesThinking,
}

/// Closed, idle or waiting for more than [`CACHE_LIFE_MIN`], or without
/// answers → Free.
/// tied and thinking in its current task (`thinking_since_user > 0`) and not
/// Free → LosesThinking. Otherwise Resend.
pub fn switch_kind(
    session: &LiveSession,
    thinking_since_user: u32,
    now: DateTime<Utc>,
) -> SwitchKind {
    if session.status == LiveStatus::Closed {
        return SwitchKind::Free;
    }
    if session.model.is_none() {
        return SwitchKind::Free;
    }
    // A session that waits for the user sends nothing either: its stored copy
    // ages the same way as an idle one's.
    if matches!(session.status, LiveStatus::Idle | LiveStatus::Waiting)
        && now.signed_duration_since(session.since) > Duration::minutes(CACHE_LIFE_MIN)
    {
        return SwitchKind::Free;
    }
    if session.tied && thinking_since_user > 0 {
        return SwitchKind::LosesThinking;
    }
    SwitchKind::Resend
}

/// This host's token records of its running sessions, as the history holds
/// them (spec §3.8): one read of this host's token files from an hour on, kept
/// by the sessions thread, plus every line this host wrote after that read
/// ([`RecordedTokens::add`]). A running row's tokens and values come from these
/// records, as a closed row's come from the history, so the row does not jump
/// when it closes. The default value holds no read: nothing is recorded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordedTokens {
    /// The hour the read starts at; `None` for the default value.
    from: Option<DateTime<Utc>>,
    /// The records of each session id, in the order they were read or added.
    by_session: BTreeMap<String, Vec<TokenRecord>>,
}

impl RecordedTokens {
    /// This host's token records from `from` on, whatever their end, with the
    /// read's warnings. A history that cannot be read is the error.
    pub fn read(
        history: &HistoryDir,
        from: DateTime<Utc>,
    ) -> Result<(RecordedTokens, Vec<String>), HistoryError> {
        let range = Range::between(Some(from), None)?;
        let read = history.read::<TokenRecord>(&range, &Hosts::One(history.host().to_string()))?;
        let mut recorded = RecordedTokens {
            from: Some(from),
            by_session: BTreeMap::new(),
        };
        for hosted in read.records {
            recorded.push(hosted.record);
        }
        Ok((recorded, read.warnings))
    }

    /// Whether the read holds every record of this host from `from` on.
    pub fn covers(&self, from: DateTime<Utc>) -> bool {
        self.from.is_some_and(|start| start <= from)
    }

    /// Add lines this host wrote to the history after the read, so the
    /// records stay what the history holds without reading it again. A line
    /// before the read's start is not kept, as the read would not have it;
    /// the default value keeps none.
    pub fn add(&mut self, lines: &[TokenRecord]) {
        let Some(from) = self.from else {
            return;
        };
        for line in lines.iter().filter(|line| line.t >= from) {
            self.push(line.clone());
        }
    }

    fn push(&mut self, record: TokenRecord) {
        self.by_session
            .entry(record.session.clone())
            .or_default()
            .push(record);
    }

    /// The records of one session.
    fn of(&self, session: &str) -> &[TokenRecord] {
        self.by_session
            .get(session)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The first hour of the running sessions' token records, for
/// [`RecordedTokens::read`]: the hour of the earliest start or first answer of
/// a running session (a closed row reads its records from its first answer's
/// hour). `None` when no session runs.
pub fn records_from(files: &[SessionFile], state: &ScanState) -> Option<DateTime<Utc>> {
    files
        .iter()
        .flat_map(|file| {
            let first = state.sessions.get(&file.session_id).map(|seen| seen.first);
            [time_ms(file.started_at), first]
        })
        .flatten()
        .min()
        .map(transcripts::hour_start)
}

/// Where the running Claude Code rows take their tokens from ([`running`]).
#[derive(Debug, Clone, Copy)]
pub enum RowTokens<'a> {
    /// The rows' token records ([`of_records`], the closed row's way):
    /// `recorded` holds what this host's history has, `pending` the lines this
    /// pass has not written yet.
    Recorded {
        recorded: &'a RecordedTokens,
        pending: &'a [TokenRecord],
    },
    /// The records cannot be had (no history is open, or its token files
    /// cannot be read): a row counts its transcripts' totals, the transcript
    /// lines this pass could not write (`unwritten`) and its linked agents'
    /// tokens, and every value is unknown with `reason`.
    Transcripts {
        unwritten: &'a [TokenRecord],
        reason: &'a str,
    },
}

/// The tokens and the two values of a session's token records (spec §3.8):
/// each record is valued at its own hour by [`Valuer::value`], so a session
/// that runs across days, a weekly reset or an account switch takes the
/// share of each week and each account it used. The one way a running and a
/// closed Claude Code row get their numbers.
fn of_records<'r>(
    records: impl IntoIterator<Item = &'r TokenRecord>,
    host: &str,
    pricing: &Pricing,
) -> (Tokens, Values) {
    let mut tokens = Tokens::default();
    let mut valued = Vec::new();
    for record in records {
        tokens += Tokens::from(record);
        if let Pricing::Known { valuer, .. } = pricing {
            valued.push(valuer.value(host, record));
        }
    }
    (tokens, pricing.of(&valued))
}

/// The running sessions of this host (spec §3.8). An agent is running while its
/// meta says `finished == false` and does not say its transcript ended with the
/// agent's final `end_turn` answer (`AgentMeta::transcript_ended`). An agent
/// seen only in the transcripts has no meta and keeps today's rule: done.
///
/// `project_of` turns a session's `cwd` into its project key; the sessions
/// thread already caches that look-up. `metas` returns the `AgentMeta` files of
/// one session, and `host` names the history host whose timeline values these
/// records.
///
/// A row's tokens and values are its token records' ([`RowTokens::Recorded`],
/// [`of_records`], the closed row's way): `recorded` holds what this host's
/// history has, `pending` the lines this pass has not written yet (a linked
/// Harness worker's growth, written after the rows; transcript lines whose
/// write failed). Only the lines of the row's own session id count. They hold
/// the session's own answers, its Claude subagents' and the recorded growth of
/// its linked flash/Harness workers, each in its own hour, so a session that
/// runs across days, a weekly reset or an account switch is valued week by
/// week and account by account. The row's last hour stays the minute-exact
/// one of the transcripts: each model's tokens of the 60 minutes are one
/// record dated at the newest minute of that model in the hour, through the
/// same [`Valuer::value`]. Without records ([`RowTokens::Transcripts`]) the
/// row counts its transcripts and every value is unknown with the reason.
///
/// `linked` holds the flash/Harness agents of each Claude Code session
/// ([`harness_rows`]); they are put under their row by [`add_agents`].
///
/// An agent's tokens are one record with its model, dated at its last answer;
/// an agent without a model has both values missing. An agent that has not
/// answered has no time at all. `agents_done_values` is [`Values::sum`] over
/// the values of the agents counted done.
///
/// A session file whose `startedAt` or `statusUpdatedAt` is not a valid time is
/// not a row: the reason goes into [`Rows::warnings`], never a default time.
///
/// `projects` is `None` when the history cannot be read: a row's project then
/// has its key's default name and no organization, so the session files alone
/// still give every waiting session its label. `pricing` says how the values
/// are built, or why they are missing; it never keeps a row from being built.
#[allow(clippy::too_many_arguments)] // the plan's signature plus `project_of`
pub fn running(
    files: &[SessionFile],
    state: &ScanState,
    metas: &dyn Fn(&str) -> Vec<AgentMeta>,
    linked: &BTreeMap<String, Vec<LiveAgent>>,
    row_tokens: RowTokens,
    projects: Option<&Projects>,
    pricing: &Pricing,
    host: &str,
    now: DateTime<Utc>,
    project_of: &dyn Fn(&str) -> ProjectKey,
) -> Rows {
    // Without records no value is built, not even from part of a row.
    let unrecorded;
    let pricing = match row_tokens {
        RowTokens::Recorded { .. } => pricing,
        RowTokens::Transcripts { reason, .. } => {
            unrecorded = Pricing::Unknown {
                reason: reason.to_string(),
            };
            &unrecorded
        }
    };
    let mut sessions = Vec::with_capacity(files.len());
    let mut warnings = Vec::new();
    for file in files {
        let Some(started) = time_ms(file.started_at) else {
            warnings.push(format!(
                "session {}: startedAt {} is not a valid time",
                file.session_id, file.started_at
            ));
            continue;
        };
        let Some(since) = time_ms(file.status_updated_at) else {
            warnings.push(format!(
                "session {}: statusUpdatedAt {} is not a valid time",
                file.session_id, file.status_updated_at
            ));
            continue;
        };
        let seen = state.sessions.get(&file.session_id);
        let key = project_of(&file.cwd);
        let id = ProjectId::of(&key, host);
        let (project_id, project_name, org) = named_project(projects, &id);

        let mut live_agents = Vec::new();
        let mut agents_running = 0u32;
        let mut agents_done = 0u32;
        let mut done_values: Vec<Values> = Vec::new();
        let mut agents: BTreeMap<String, (Option<AgentMeta>, Option<&AgentSeen>)> = BTreeMap::new();
        for meta in metas(&file.session_id) {
            let agent = meta.agent.clone();
            agents.entry(agent).or_insert((None, None)).0 = Some(meta);
        }
        if let Some(seen) = seen {
            for (agent, agent_seen) in &seen.agents {
                agents.entry(agent.clone()).or_insert((None, None)).1 = Some(agent_seen);
            }
        }
        for (agent, (meta, agent_seen)) in agents {
            let first = agent_seen.map(|agent| agent.first);
            let last = agent_seen.map(|agent| agent.last);
            let model = agent_seen.and_then(|agent| agent.model.clone());
            let tokens = agent_seen.map(|agent| agent.tokens).unwrap_or_default();
            let is_running = meta.as_ref().map(|meta| !meta.is_done()).unwrap_or(false);
            if is_running {
                agents_running += 1;
            } else {
                agents_done += 1;
            }
            let values = match pricing {
                Pricing::Known { valuer, .. } => match (model.as_deref(), last) {
                    (Some(model), Some(last)) => {
                        // A row value, never written: the span keeps today's 60 minutes.
                        let record = token_record(
                            &file.session_id,
                            &file.cwd,
                            &key,
                            model,
                            last,
                            60,
                            tokens,
                        );
                        pricing.of(&[valuer.value(host, &record)])
                    }
                    _ => pricing.missing(NO_AGENT_MODEL),
                },
                Pricing::Unknown { reason } => Values::unknown(reason),
            };
            if !is_running {
                done_values.push(values.clone());
            }
            live_agents.push(LiveAgent {
                agent,
                parent: meta.as_ref().and_then(|meta| meta.parent.clone()),
                provider: CLAUDE_PROVIDER.to_string(),
                kind: meta.as_ref().and_then(|meta| meta.agent_type.clone()),
                task: meta.as_ref().and_then(|meta| meta.description.clone()),
                model,
                running: is_running,
                first,
                last,
                tokens,
                values,
            });
        }
        let done_values: Vec<&Values> = done_values.iter().collect();
        let agents_done_values = Values::sum(&done_values);

        let model = seen.and_then(|seen| seen.last_model.clone());
        let linked_agents = linked.get(&file.session_id).cloned().unwrap_or_default();
        let own = |line: &&TokenRecord| line.session == file.session_id;
        let (tokens, values) = match row_tokens {
            RowTokens::Recorded { recorded, pending } => of_records(
                recorded
                    .of(&file.session_id)
                    .iter()
                    .chain(pending.iter().filter(own)),
                host,
                pricing,
            ),
            // The one place a running row counts its transcripts instead of its
            // token records: without records there is nothing else to count,
            // and the transcripts still show how far the session went. The
            // totals hold the answers whose lines are in the state; the
            // unwritten lines are the answers read past it (the state does not
            // move while they cannot be written); the linked agents' tokens are
            // not in the transcripts. Their sum is not valued: the values need
            // the records, so they stay unknown with the reason.
            RowTokens::Transcripts { unwritten, reason } => {
                let mut tokens = seen.map(|seen| seen.totals).unwrap_or_default();
                for line in unwritten.iter().filter(own) {
                    tokens += Tokens::from(line);
                }
                for agent in &linked_agents {
                    tokens += agent.tokens;
                }
                (tokens, Values::unknown(reason))
            }
        };
        let last_hour = seen
            .map(|seen| transcripts::last_hour(seen, now))
            .unwrap_or_default();
        // Each model's tokens of the last 60 minutes, dated at the newest minute
        // of that model in the hour, so the hour is valued per model; the one
        // window rule is `transcripts::last_hour_by_model`.
        let last_hour_by_model = seen
            .map(|seen| transcripts::last_hour_by_model(seen, now))
            .unwrap_or_default();
        let last_hour_values = match pricing {
            Pricing::Known { valuer, .. } => {
                let valued: Vec<Valued> = last_hour_by_model
                    .iter()
                    .map(|(model, (at, tokens))| {
                        // The last 60 minutes, never written: a 60-minute span.
                        let record = token_record(
                            &file.session_id,
                            &file.cwd,
                            &key,
                            model,
                            *at,
                            60,
                            *tokens,
                        );
                        valuer.value(host, &record)
                    })
                    .collect();
                pricing.of(&valued)
            }
            Pricing::Unknown { reason } => Values::unknown(reason),
        };
        let label = file
            .name
            .clone()
            .unwrap_or_else(|| session_label(&project_name, &file.session_id));
        let mut row = LiveSession {
            session: file.session_id.clone(),
            host: host.to_string(),
            label,
            title: seen.and_then(|seen| seen.shown_title().cloned()),
            project_id,
            project: project_name,
            org,
            cwd: file.cwd.clone(),
            provider: CLAUDE_PROVIDER.to_string(),
            tied: model.as_deref().map(thinking_tied).unwrap_or(false),
            model,
            status: live_status(&file.status),
            since,
            started,
            agents_running,
            agents_done,
            agents: live_agents,
            tokens,
            last_hour: Some(last_hour),
            last_hour_values,
            agents_done_values,
            values,
        };
        add_agents(&mut row, linked_agents);
        sessions.push(row);
    }
    Rows { sessions, warnings }
}

/// The rows and agents one Harness read adds, and the token lines its tokens
/// produce.
#[derive(Debug, Clone)]
pub struct HarnessRows {
    /// The Harness sessions that are their own row: no linked top-level job.
    pub rows: Rows,
    /// The flash agents of each Claude Code session, keyed by that session's id.
    pub agents: BTreeMap<String, Vec<LiveAgent>>,
    /// The token lines this pass must write, and the totals after writing them.
    pub tokens: Vec<TokenRecord>,
    /// `ScanState::harness_recorded` after this pass.
    pub recorded: BTreeMap<String, HarnessRecorded>,
}

/// The provider of a Harness session that is its own row, of a flash job and
/// of a Harness worker (spec §3.8).
pub const HARNESS_PROVIDER: &str = "DeepSeek Harness";

/// Put the Harness sessions into the Sessions area: a job named in a Claude
/// Code transcript becomes a `"flash"` agent of that session, with its workers
/// below it; every other top-level session is its own row. The tokens of every
/// session become token lines for what grew since `recorded`; the returned
/// `recorded` is the total after this pass, and a line is written only when a
/// total grew (a total that went down is a warning, never a negative line).
///
/// A flash job's task text is its report's label (`reports`), so a running
/// job, which has no report yet, shows none; a worker under a flash job shows
/// none either ([`harness_task`]).
#[allow(clippy::too_many_arguments)]
pub fn harness_rows(
    sessions: &[HarnessSession],
    owners: &BTreeMap<String, String>,
    claude_cwd: &BTreeMap<String, String>,
    recorded: &BTreeMap<String, HarnessRecorded>,
    locks: &BTreeSet<String>,
    reports: &harness::JobReports,
    projects: Option<&Projects>,
    pricing: &Pricing,
    host: &str,
    now: DateTime<Utc>,
    project_of: &dyn Fn(&str) -> ProjectKey,
) -> HarnessRows {
    let by_id: BTreeMap<&str, &HarnessSession> = sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect();
    let mut components: BTreeMap<&str, Vec<&HarnessSession>> = BTreeMap::new();
    for session in sessions {
        components
            .entry(top_known_ancestor(session, &by_id))
            .or_default()
            .push(session);
    }

    let mut out = HarnessRows {
        rows: Rows {
            sessions: Vec::new(),
            warnings: Vec::new(),
        },
        agents: BTreeMap::new(),
        tokens: Vec::new(),
        recorded: recorded.clone(),
    };
    // The folders a flash worker's copy may have been made from.
    let roots: Vec<&str> = claude_cwd
        .values()
        .map(String::as_str)
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();
    for (root_id, mut component) in components {
        component.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        let root = by_id[root_id];
        // The owner of the tree: its own id first, then the ids of its sessions
        // and their parent chains as far as they are known.
        let owner = owner_of(root, &component, &by_id, owners);
        match owner {
            Some(owner) => {
                let claude_cwd = claude_cwd.get(&owner).cloned();
                if claude_cwd.is_none() {
                    out.rows.warnings.push(format!(
                        "the Claude Code session {owner} of Harness job {root_id} is not known yet; its folder is not known"
                    ));
                }
                let flash = is_flash_tree(root);
                for session in component {
                    let Some((cwd, project)) = harness_location(
                        session,
                        claude_cwd.as_deref(),
                        &roots,
                        project_of,
                        &mut out.rows.warnings,
                    ) else {
                        continue;
                    };
                    let is_root = session.id.as_str() == root_id;
                    // A worker whose job is out of the slice still hangs under
                    // the job's owner; only a depth-0 job is the flash agent.
                    let (parent, kind) = if is_root && session.depth == 0 {
                        (None, Some("flash"))
                    } else {
                        (session.parent.clone(), None)
                    };
                    let running = harness::is_running(session, locks.contains(&session.id), now);
                    let task = harness_task(session, flash, reports);
                    let agent = harness_agent(
                        session, parent, kind, task, running, &cwd, &project, pricing, host,
                    );
                    out.agents.entry(owner.clone()).or_default().push(agent);
                    harness_token_line(
                        session,
                        &owner,
                        &cwd,
                        &project,
                        &mut out.recorded,
                        &mut out.tokens,
                        &mut out.rows.warnings,
                    );
                }
            }
            None => {
                let Some((cwd, project)) =
                    harness_location(root, None, &roots, project_of, &mut out.rows.warnings)
                else {
                    continue;
                };
                // A tree without its Claude Code session is a row only while
                // it is open, as a Codex row: a flash job whose task ended is
                // done, not idle. Its tokens are recorded either way.
                if harness::is_open(root, locks.contains(&root.id), now) {
                    let row = harness_row(
                        root, &component, &cwd, &project, projects, reports, pricing, host, locks,
                        now,
                    );
                    out.rows.sessions.push(row);
                }
                for session in component {
                    harness_token_line(
                        session,
                        &session.id,
                        &cwd,
                        &project,
                        &mut out.recorded,
                        &mut out.tokens,
                        &mut out.rows.warnings,
                    );
                }
            }
        }
    }
    out.tokens
        .sort_by(|a, b| a.session.cmp(&b.session).then_with(|| a.t.cmp(&b.t)));
    out
}

/// The row of a running Harness tree whose Claude Code session is not known:
/// the root session with its workers as agents. Its tokens and values are the
/// root's and every worker's: a Harness row has no token records of its own
/// to read them from.
#[allow(clippy::too_many_arguments)]
fn harness_row(
    root: &HarnessSession,
    component: &[&HarnessSession],
    cwd: &str,
    project: &ProjectKey,
    projects: Option<&Projects>,
    reports: &harness::JobReports,
    pricing: &Pricing,
    host: &str,
    locks: &BTreeSet<String>,
    now: DateTime<Utc>,
) -> LiveSession {
    let id = ProjectId::of(project, host);
    let (project_id, project_name, org) = named_project(projects, &id);
    let flash = is_flash_tree(root);
    let mut agents = Vec::new();
    for session in component.iter().filter(|session| session.id != root.id) {
        let is_running = harness::is_running(session, locks.contains(&session.id), now);
        let agent = harness_agent(
            session,
            session.parent.clone(),
            None,
            harness_task(session, flash, reports),
            is_running,
            cwd,
            project,
            pricing,
            host,
        );
        agents.push(agent);
    }
    let root_values = harness_values(root, cwd, project, pricing, host);
    let mut tokens = root.tokens;
    let mut parts = vec![&root_values];
    for agent in &agents {
        tokens += agent.tokens;
        parts.push(&agent.values);
    }
    let values = Values::sum(&parts);
    // The last hour of a Harness row is known from its answers'
    // times, never a silent 0 (S-3): the messages inside the window,
    // valued like the row's tokens at the newest of them, or at the
    // row's last event when the window holds none (a known 0).
    let (last_hour, last_hour_at) = harness_last_hour(root, now);
    let last_hour_values = harness_values_at(
        root,
        last_hour,
        last_hour_at.unwrap_or(root.last),
        cwd,
        project,
        pricing,
        host,
    );
    let mut row = LiveSession {
        session: root.id.clone(),
        host: host.to_string(),
        label: session_label(&project_name, &root.id),
        // A Harness session that is its own row is named by its
        // last `session/title`, as a Claude Code row by its
        // `ai-title`. A flash job's title is a worker's task text:
        // it never names a row and never reaches the report.
        title: if harness::is_flash_job(&root.id) {
            None
        } else {
            root.title.clone()
        },
        project_id,
        project: project_name,
        org,
        cwd: cwd.to_string(),
        provider: HARNESS_PROVIDER.to_string(),
        model: root.model.clone(),
        tied: root.model.as_deref().map(thinking_tied).unwrap_or(false),
        status: if harness::is_running(root, locks.contains(&root.id), now) {
            LiveStatus::Busy
        } else {
            LiveStatus::Idle
        },
        since: root.last,
        started: root.created,
        agents_running: 0,
        agents_done: 0,
        agents: Vec::new(),
        tokens,
        last_hour: Some(last_hour),
        last_hour_values,
        agents_done_values: Values::sum(&[]),
        values,
    };
    add_agents(&mut row, agents);
    row
}

/// Whether a Harness tree is a flash job's: its top known session, or the
/// session that one names as its parent, is a `flash-task-…` job.
fn is_flash_tree(root: &HarnessSession) -> bool {
    harness::is_flash_job(&root.id) || root.parent.as_deref().is_some_and(harness::is_flash_job)
}

/// The task text a Harness session shows as an agent (spec §3.8): a flash
/// job's is its report's label, `None` while it has no report; a worker in a
/// flash job's tree shows none, because its title only names its folder; any
/// other Harness session shows its title.
fn harness_task(
    session: &HarnessSession,
    flash_tree: bool,
    reports: &harness::JobReports,
) -> Option<String> {
    if harness::is_flash_job(&session.id) {
        reports.label(&session.id).map(str::to_string)
    } else if flash_tree {
        None
    } else {
        session.title.clone()
    }
}

/// The top known ancestor of a session: a parent that is not in the slice stops
/// the walk but its id is still looked at for an owner.
fn top_known_ancestor<'a>(
    session: &'a HarnessSession,
    by_id: &BTreeMap<&'a str, &'a HarnessSession>,
) -> &'a str {
    let mut root = session.id.as_str();
    let mut seen = BTreeSet::new();
    let mut parent = session.parent.as_deref();
    while let Some(id) = parent {
        if !seen.insert(id) {
            break;
        }
        let Some(next) = by_id.get(id) else {
            break;
        };
        root = next.id.as_str();
        parent = next.parent.as_deref();
    }
    root
}

/// The owner of a tree: the root's own id, then every session's own id and
/// parent chain as far as it is known. A parent outside the slice is still
/// looked at for an owner.
fn owner_of(
    root: &HarnessSession,
    component: &[&HarnessSession],
    by_id: &BTreeMap<&str, &HarnessSession>,
    owners: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(owner) = owners.get(&root.id) {
        return Some(owner.clone());
    }
    for session in component {
        let mut seen = BTreeSet::new();
        let mut current = Some(session.id.as_str());
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            if let Some(owner) = owners.get(id) {
                return Some(owner.clone());
            }
            current = match by_id.get(id) {
                Some(session) => session.parent.as_deref(),
                None => break,
            };
        }
    }
    None
}

/// Put agents under a running row and recount its agents and the values of
/// the done ones. The row's tokens and values are not touched: a Claude Code
/// row's come from its token records ([`running`]), which already hold the
/// recorded growth of its linked flash/Harness workers, so adding a worker's
/// tokens here would count them twice (without records [`running`] adds them
/// itself); a Harness row of its own sums its workers itself ([`harness_row`]).
pub fn add_agents(session: &mut LiveSession, agents: Vec<LiveAgent>) {
    for agent in agents {
        if agent.running {
            session.agents_running += 1;
        } else {
            session.agents_done += 1;
        }
        session.agents.push(agent);
    }
    let done: Vec<&Values> = session
        .agents
        .iter()
        .filter(|agent| !agent.running)
        .map(|agent| &agent.values)
        .collect();
    session.agents_done_values = Values::sum(&done);
}

/// Item 6 (spec §1.4, §3.8): Claude Code's own cost of a session becomes its API
/// value, through [`money::api_value`]: the provider's number when the status
/// line's feed gave one for that session, else models.dev's value of its tokens.
/// The subscription side is not touched.
///
/// `cost_usd` is the feed's `cost.total_cost_usd`, in US dollars, put in the API
/// column's currency by [`money::api_in_currency`]: with the look-ups off the
/// cost stays in USD; a settings currency without a stored dollar rate also
/// stays USD; a stored rate converts it; and no settings currency makes the API
/// value missing with that reason.
///
/// A converted value that is not finite (a huge cost times the rate) is not
/// used: the row keeps the valuer's value and the returned warning says why, so
/// the caller can name the session.
pub fn take_provider_cost(
    values: &mut Values,
    cost_usd: Option<f64>,
    settings: &Settings,
    fx: &Fx,
    at: DateTime<Utc>,
) -> Option<String> {
    let cost_usd = cost_usd?;
    match money::api_in_currency(cost_usd, settings, fx, at.date_naive()) {
        Ok((provider, currency)) => {
            if !provider.is_finite() {
                return Some(format!(
                    "Claude Code's own cost {cost_usd} US dollars is {provider} in {currency} at the stored rate; it is not used, so the valuer's API value stays"
                ));
            }
            if let Some(api) = money::api_value(Some(provider), values.api) {
                values.api = Some(api);
                values.api_currency = Some(currency);
                values.api_missing = None;
                // The provider's cost covers the whole session, so the API half
                // is no longer only partly known.
                values.api_partial = false;
            }
            None
        }
        Err(missing) => {
            values.api = None;
            values.api_currency = None;
            values.api_missing = Some(missing.to_string());
            // A half with no value is never partial.
            values.api_partial = false;
            None
        }
    }
}

/// The folder and project of a Harness session: the Claude Code session's for a
/// linked job, else its own. A folder that is not text leaves the session out
/// with a warning.
fn harness_location(
    session: &HarnessSession,
    claude_cwd: Option<&str>,
    roots: &[&str],
    project_of: &dyn Fn(&str) -> ProjectKey,
    warnings: &mut Vec<String>,
) -> Option<(String, ProjectKey)> {
    let cwd = match claude_cwd {
        Some(cwd) => cwd.to_string(),
        // Without its Claude Code session, a flash worker's copy names the
        // folder it was copied from (`harness::copy_source`); any other
        // session keeps its own folder.
        None => match harness::copy_source(&session.cwd, roots) {
            Some(source) => source,
            None => match session.cwd.to_str() {
                Some(cwd) => cwd.to_string(),
                None => {
                    warnings.push(format!(
                        "Harness session {} has a folder that is not text; it is not shown",
                        session.id
                    ));
                    return None;
                }
            },
        },
    };
    let project = project_of(&cwd);
    Some((cwd, project))
}

/// One Harness session as a live agent; `kind` is `"flash"` for a linked job
/// and `task` is [`harness_task`]'s.
#[allow(clippy::too_many_arguments)]
fn harness_agent(
    session: &HarnessSession,
    parent: Option<String>,
    kind: Option<&str>,
    task: Option<String>,
    running: bool,
    cwd: &str,
    project: &ProjectKey,
    pricing: &Pricing,
    host: &str,
) -> LiveAgent {
    LiveAgent {
        agent: session.id.clone(),
        parent,
        provider: HARNESS_PROVIDER.to_string(),
        kind: kind.map(str::to_string),
        task,
        model: session.model.clone(),
        running,
        first: Some(session.created),
        last: Some(session.last),
        tokens: session.tokens,
        values: harness_values(session, cwd, project, pricing, host),
    }
}

/// The values of a Harness session's tokens at its reported model.
fn harness_values(
    session: &HarnessSession,
    cwd: &str,
    project: &ProjectKey,
    pricing: &Pricing,
    host: &str,
) -> Values {
    harness_values_at(
        session,
        session.tokens,
        session.last,
        cwd,
        project,
        pricing,
        host,
    )
}

/// The values of some of a Harness session's tokens at its reported model,
/// dated at `at` (spec §3.8): the row's own totals at its last event, its last
/// hour at the newest message inside the hour. The one valuer call both use.
#[allow(clippy::too_many_arguments)]
fn harness_values_at(
    session: &HarnessSession,
    tokens: Tokens,
    at: DateTime<Utc>,
    cwd: &str,
    project: &ProjectKey,
    pricing: &Pricing,
    host: &str,
) -> Values {
    let Some(model) = session.model.as_deref() else {
        return pricing.missing(NO_AGENT_MODEL);
    };
    // A Harness row value, never written: the span keeps today's 60 minutes.
    let record = token_record(&session.id, cwd, project, model, at, 60, tokens);
    match pricing {
        Pricing::Known { valuer, .. } => pricing.of(&[valuer.value(host, &record)]),
        Pricing::Unknown { .. } => pricing.of(&[]),
    }
}

/// A Harness session's tokens in the last hour (spec §3.8), and the newest
/// message time they are valued at: every `assistant/message` after
/// `now - 60 min`, the window [`transcripts::last_hour_by_model`] uses. The hour
/// is known from the answers' own times, so an empty window is a known 0, not
/// an unknown hour.
fn harness_last_hour(
    session: &HarnessSession,
    now: DateTime<Utc>,
) -> (Tokens, Option<DateTime<Utc>>) {
    let after = now - Duration::minutes(60);
    let mut tokens = Tokens::default();
    let mut newest: Option<DateTime<Utc>> = None;
    for message in &session.messages_detail {
        if message.at <= after {
            continue;
        }
        tokens += message.usage.unwrap_or_default();
        newest = Some(match newest {
            Some(at) if at >= message.at => at,
            _ => message.at,
        });
    }
    (tokens, newest)
}

/// One token line for what a Harness session gained since `recorded`, or a
/// warning when its totals went down or its model or folder is not known. The
/// growth is split by the 5-minute bucket and model of each new assistant
/// message, exactly as S-1's `token_records` groups its answers.
#[allow(clippy::too_many_arguments)]
fn harness_token_line(
    session: &HarnessSession,
    history_session: &str,
    cwd: &str,
    project: &ProjectKey,
    recorded: &mut BTreeMap<String, HarnessRecorded>,
    lines: &mut Vec<TokenRecord>,
    warnings: &mut Vec<String>,
) {
    let previous = recorded.get(&session.id).cloned().unwrap_or_default();
    let current = HarnessRecorded {
        tokens: session.tokens,
        messages: session.messages,
        file: Some(session.file.clone()),
    };
    if current == previous {
        return;
    }
    let grew = current.tokens.input >= previous.tokens.input
        && current.tokens.out >= previous.tokens.out
        && current.tokens.cw >= previous.tokens.cw
        && current.tokens.cr >= previous.tokens.cr
        && current.messages >= previous.messages;
    if !grew {
        warnings.push(format!(
            "Harness session {} has fewer tokens or messages than already recorded; the old totals are kept",
            session.id
        ));
        return;
    }
    // The answers since the recorded message count: each has its own time,
    // model and usage. A message without usage adds an answer with no tokens.
    let new_count = (current.messages - previous.messages) as usize;
    let Some(messages) = session
        .messages_detail
        .get(previous.messages as usize..)
        .map(|messages| &messages[..messages.len().min(new_count)])
    else {
        warnings.push(format!(
            "Harness session {} counts {} answers but its file holds fewer; the old totals are kept",
            session.id, current.messages
        ));
        return;
    };
    let mut groups: BTreeMap<(DateTime<Utc>, String), (Tokens, u32)> = BTreeMap::new();
    for message in messages {
        let Some(model) = message.model.clone().or_else(|| session.model.clone()) else {
            warnings.push(format!(
                "Harness session {} has an answer without a model; its tokens are not recorded yet",
                session.id
            ));
            return;
        };
        let group = groups
            .entry((transcripts::bucket_start(message.at), model))
            .or_default();
        if let Some(usage) = message.usage {
            group.0 += usage;
        }
        group.1 = group.1.saturating_add(1);
    }
    if groups.is_empty() && current.tokens != previous.tokens {
        warnings.push(format!(
            "Harness session {} has more tokens than recorded but no new answer; the old totals are kept",
            session.id
        ));
        return;
    }
    for ((bucket, model), (tokens, answers)) in groups {
        let mut record = token_record(
            history_session,
            cwd,
            project,
            &model,
            bucket,
            transcripts::TOKEN_BUCKET_MINUTES as u32,
            tokens,
        );
        record.agent_answers = answers;
        lines.push(record);
    }
    recorded.insert(session.id.clone(), current);
}

/// The provider of a Codex row (spec §3.8).
const CODEX_PROVIDER: &str = "Codex";

/// Why a Codex row's last hour has no value: its newest token total is below the
/// total the hour is measured from, which the read warns about and never turns
/// into a number. The row's `last_hour` is `None` and it carries that reason in
/// `last_hour_values`, so the row and every sum over it say why.
const CODEX_LAST_HOUR_UNKNOWN: &str =
    "the Codex session's token total went down, so its last hour is not known";

/// Put the running Codex sessions into the Sessions area. A session is running
/// when a process holds its rollout file open (`open_files`, from the one
/// process walk the Harness links use) or its last event is less than
/// [`harness::HARNESS_QUIET_MINUTES`] old; its `busy` says whether a task is
/// open. The two rules are Harness's running rule without its finished test: a
/// Codex session between two tasks is still open, only idle.
///
/// Only running sessions are rows, and the row keeps the session's live total.
/// The sessions thread writes the token lines of these sessions from their
/// series; a closed Codex session has no session record of its own here, so it
/// has no closed row to read.
///
/// The values use the same [`Pricing`] and [`Valuer`] as the other rows: a
/// session's tokens at its model, dated at its last event, and its `last_hour`
/// at the same model and time with the same valuer. A session without a model
/// has both values missing with the same reason as an agent without one. A
/// last hour that is not known has both values missing with
/// [`CODEX_LAST_HOUR_UNKNOWN`] rather than a value of 0, and the row's
/// `last_hour` is `None`, so every sum over it is unknown too.
pub fn codex_rows(
    sessions: &[CodexSession],
    open_files: &BTreeSet<PathBuf>,
    projects: Option<&Projects>,
    pricing: &Pricing,
    host: &str,
    now: DateTime<Utc>,
    project_of: &dyn Fn(&str) -> ProjectKey,
) -> Rows {
    let mut rows = Rows {
        sessions: Vec::new(),
        warnings: Vec::new(),
    };
    for session in sessions {
        if !codex_running(session, open_files, now) {
            continue;
        }
        let Some(cwd) = session.cwd.to_str() else {
            rows.warnings.push(format!(
                "Codex session {} has a folder that is not text; it is not shown",
                session.id
            ));
            continue;
        };
        let project = project_of(cwd);
        let id = ProjectId::of(&project, host);
        let (project_id, project_name, org) = named_project(projects, &id);
        let last_hour_values = match session.last_hour {
            Some(last_hour) => {
                codex_tokens_values(session, last_hour, cwd, &project, pricing, host)
            }
            None => pricing.missing(CODEX_LAST_HOUR_UNKNOWN),
        };
        rows.sessions.push(LiveSession {
            session: session.id.clone(),
            host: host.to_string(),
            label: session_label(&project_name, &session.id),
            // Codex's own name of the thread, from its session index.
            title: session.title.clone(),
            project_id,
            project: project_name,
            org,
            cwd: cwd.to_string(),
            provider: CODEX_PROVIDER.to_string(),
            model: session.model.clone(),
            // Codex does not run on the Claude account, so no thinking of its is
            // tied to that account.
            tied: false,
            status: if session.busy {
                LiveStatus::Busy
            } else {
                LiveStatus::Idle
            },
            since: session.last,
            started: session.started,
            agents_running: 0,
            agents_done: 0,
            agents: Vec::new(),
            tokens: session.tokens,
            // An unknown hour is `null`, never 0 tokens; its values carry the
            // reason (`CODEX_LAST_HOUR_UNKNOWN`) in the row and the sums.
            last_hour: session.last_hour,
            last_hour_values,
            agents_done_values: Values::sum(&[]),
            values: codex_values(session, cwd, &project, pricing, host),
        });
    }
    rows
}

/// Whether a Codex session is running: a process holds its rollout file open,
/// or its last event is less than [`harness::HARNESS_QUIET_MINUTES`] old.
fn codex_running(
    session: &CodexSession,
    open_files: &BTreeSet<PathBuf>,
    now: DateTime<Utc>,
) -> bool {
    codex_recent(session, now) || holds_open(open_files, session)
}

/// Whether a Codex session's last event is inside the quiet window, so the
/// running rule holds without the process walk.
fn codex_recent(session: &CodexSession, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(session.last) < Duration::minutes(harness::HARNESS_QUIET_MINUTES)
}

/// Whether any Codex session is old enough that only an open file could still
/// make it running: the one condition under which the process walk's open files
/// are needed. The session thread asks this before walking, so a pass with only
/// recent Codex sessions does not walk at all.
pub fn codex_needs_process_walk(sessions: &[CodexSession], now: DateTime<Utc>) -> bool {
    sessions.iter().any(|session| !codex_recent(session, now))
}

/// Whether the process walk found `session`'s file open. The kernel reports an
/// open file by its resolved path, so a store reached through a symlink is
/// matched after resolving the file too.
fn holds_open(open_files: &BTreeSet<PathBuf>, session: &CodexSession) -> bool {
    if open_files.contains(&session.file) {
        return true;
    }
    match fs::canonicalize(&session.file) {
        Ok(resolved) => open_files.contains(&resolved),
        Err(_) => false,
    }
}

/// The values of a Codex session's tokens at its reported model.
fn codex_values(
    session: &CodexSession,
    cwd: &str,
    project: &ProjectKey,
    pricing: &Pricing,
    host: &str,
) -> Values {
    codex_tokens_values(session, session.tokens, cwd, project, pricing, host)
}

/// The values of one token total of a Codex session, at its model and the
/// session's last event: the one valuer both `tokens` and `last_hour` use.
fn codex_tokens_values(
    session: &CodexSession,
    tokens: Tokens,
    cwd: &str,
    project: &ProjectKey,
    pricing: &Pricing,
    host: &str,
) -> Values {
    let Some(model) = session.model.as_deref() else {
        return pricing.missing(NO_AGENT_MODEL);
    };
    // A Codex row value, never written: the span keeps today's 60 minutes.
    let record = token_record(&session.id, cwd, project, model, session.last, 60, tokens);
    match pricing {
        Pricing::Known { valuer, .. } => pricing.of(&[valuer.value(host, &record)]),
        Pricing::Unknown { .. } => pricing.of(&[]),
    }
}

/// The sessions of every host that closed in `[since, now)` (spec §3.8). A row
/// is a session whose last `SessionRecord` has `closed: true` and whose `last`
/// is in the range; its tokens are the sum of every `TokenRecord` of the
/// session from the hour start of the earliest `first` of the rows, because one
/// bucket and model can have several lines; they are valued through
/// [`of_records`], the running row's way. Its last hour sums the records whose
/// span `[t, t + span)` overlaps `[now - 60 min, now)`: a 5-minute line lands
/// wholly in its bucket, and an hourly record from the hour before the window
/// still counts; those overlapping records are also its `last_hour_values`,
/// each valued like `values`. Its agents list is empty,
/// so its `agents_done_values` is the sum of an empty list; the counts come from
/// the record. `label` is `<project>-<first 2 characters of
/// the session id>`.
///
/// The history read's warnings are passed on in [`Rows::warnings`]. `projects`
/// and `pricing` mean what they do in [`running`].
pub fn closed(
    history: &HistoryDir,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    projects: Option<&Projects>,
    pricing: &Pricing,
) -> Result<Rows, HistoryError> {
    let range = Range::between(Some(since), Some(now))?;
    let read = history.read::<SessionRecord>(&range, &Hosts::All)?;
    let mut warnings = read.warnings;
    let mut last_lines: BTreeMap<(String, String), Hosted<SessionRecord>> = BTreeMap::new();
    for hosted in read.records {
        last_lines.insert((hosted.host.clone(), hosted.record.session.clone()), hosted);
    }
    let rows: Vec<&Hosted<SessionRecord>> = last_lines
        .values()
        .filter(|hosted| {
            hosted.record.closed && hosted.record.last >= since && hosted.record.last < now
        })
        .collect();
    let Some(earliest) = rows.iter().map(|hosted| hosted.record.first).min() else {
        return Ok(Rows {
            sessions: Vec::new(),
            warnings,
        });
    };
    let token_range = Range::between(Some(transcripts::hour_start(earliest)), Some(now))?;
    let token_read = history.read::<TokenRecord>(&token_range, &Hosts::All)?;
    let token_records = token_read.records;
    warnings.extend(token_read.warnings);
    let mut tokens_of: BTreeMap<(String, String), Vec<&TokenRecord>> = BTreeMap::new();
    for hosted in &token_records {
        tokens_of
            .entry((hosted.host.clone(), hosted.record.session.clone()))
            .or_default()
            .push(&hosted.record);
    }

    let mut sessions = Vec::with_capacity(rows.len());
    for hosted in rows {
        let record = &hosted.record;
        let of_session = tokens_of
            .get(&(hosted.host.clone(), record.session.clone()))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let (tokens, values) = of_records(of_session.iter().copied(), &hosted.host, pricing);
        // A line whose span overlaps the window counts, even when its start is
        // before the window.
        let hour_ago = now - Duration::minutes(60);
        let (last_hour, last_hour_values) = of_records(
            of_session
                .iter()
                .copied()
                .filter(|token| token.t < now && token.t + token.span() > hour_ago),
            &hosted.host,
            pricing,
        );
        // The Claude Code row keeps the Claude model of its newest plan-model
        // record; a linked worker's model never becomes the row's model.
        let model = of_session
            .iter()
            .filter(|token| is_plan_model(&token.model))
            .max_by_key(|token| token.t)
            .map(|token| token.model.clone());
        let id = ProjectId::of(&record.project, &hosted.host);
        let (project_id, project_name, org) = named_project(projects, &id);
        let label = session_label(&project_name, &record.session);
        sessions.push(LiveSession {
            session: record.session.clone(),
            host: hosted.host.clone(),
            label,
            title: record.title.clone(),
            project_id,
            project: project_name,
            org,
            cwd: record.cwd.clone(),
            provider: CLAUDE_PROVIDER.to_string(),
            tied: model.as_deref().map(thinking_tied).unwrap_or(false),
            model,
            status: LiveStatus::Closed,
            since: record.last,
            started: record.first,
            agents_running: 0,
            agents_done: record.agents_finished,
            agents: Vec::new(),
            tokens,
            last_hour: Some(last_hour),
            last_hour_values,
            agents_done_values: Values::sum(&[]),
            values,
        });
    }
    Ok(Rows { sessions, warnings })
}

/// A project's key, name and organization after the joins. Without a project
/// look-up (the history cannot be read) the row's own key, the key's default
/// name and no organization are used, so a row can always name its project.
fn named_project(
    projects: Option<&Projects>,
    id: &ProjectId,
) -> (ProjectId, String, Option<String>) {
    match projects {
        Some(projects) => {
            let project = projects.of(id);
            (project.key, project.name, project.org)
        }
        None => (id.clone(), crate::projects::default_name(&id.key), None),
    }
}

/// What this host writes for the other hosts (spec §3.8): the rows of the
/// Sessions area at one moment. No agent's `task` is ever in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReport {
    pub at: DateTime<Utc>,
    pub sessions: Vec<LiveSession>,
}

/// One other host's report, as this host reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherHost {
    pub host: String,
    pub at: DateTime<Utc>,
    /// Older than [`REPORT_STALE_MIN`]: its sessions say `Other("no report")`.
    pub stale: bool,
    pub sessions: Vec<LiveSession>,
}

/// Every other host's report, and the reports that could not be read.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherHosts {
    pub hosts: Vec<OtherHost>,
    pub warnings: Vec<String>,
}

/// This host's rows as the report other hosts read (spec §3.8). Every agent's
/// `task` is set to `None` first, so no task text can reach the file. The file
/// lives in this host's own folder, written with [`atomic::write_atomic`] and
/// [`Access::Private`].
pub fn write_report(
    history: &HistoryDir,
    sessions: &[LiveSession],
    at: DateTime<Utc>,
) -> Result<HostReport, HistoryError> {
    let mut sessions = sessions.to_vec();
    for session in &mut sessions {
        for agent in &mut session.agents {
            agent.task = None;
        }
    }
    let report = HostReport { at, sessions };
    let path = history.sessions_now_file(history.host());
    let bytes = serde_json::to_vec_pretty(&report).map_err(|source| HistoryError::Io {
        path: path.clone(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot encode the sessions report: {source}"),
        ),
    })?;
    let folder = history.root().join(history.host());
    atomic::ensure_private_dir(&folder).map_err(|source| HistoryError::Io {
        path: folder.clone(),
        source,
    })?;
    atomic::write_atomic(&path, &bytes, Access::Private)
        .map_err(|source| HistoryError::Io { path, source })?;
    Ok(report)
}

/// The reports of every other host folder (spec §3.8). A folder without
/// `sessions-now.json` is left out: that host has no report. A file that cannot
/// be read or parsed is a warning naming the file and the reason, and its host
/// is left out. Only an error listing the host folders themselves is an error.
///
/// A report whose `at` is more than [`REPORT_STALE_MIN`] before `now` is stale:
/// every session's status becomes `Other("no report")`. A report from the
/// future (another machine's clock is ahead) is fresh.
pub fn other_hosts(history: &HistoryDir, now: DateTime<Utc>) -> Result<OtherHosts, HistoryError> {
    let mut hosts = Vec::new();
    let mut warnings = Vec::new();
    for host in history.hosts()? {
        if host == history.host() {
            continue;
        }
        let path = history.sessions_now_file(&host);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                warnings.push(format!("{}: {source}", path.display()));
                continue;
            }
        };
        let report = match serde_json::from_str::<serde_json::Value>(&text).and_then(|mut value| {
            older_agents_take_their_sessions_provider(&mut value);
            serde_json::from_value::<HostReport>(value)
        }) {
            Ok(report) => report,
            Err(source) => {
                warnings.push(format!("{}: {source}", path.display()));
                continue;
            }
        };
        let stale = now.signed_duration_since(report.at) > Duration::minutes(REPORT_STALE_MIN);
        let mut sessions = report.sessions;
        if stale {
            for session in &mut sessions {
                session.status = LiveStatus::Other("no report".to_string());
            }
        }
        hosts.push(OtherHost {
            host,
            at: report.at,
            stale,
            sessions,
        });
    }
    hosts.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(OtherHosts { hosts, warnings })
}

/// A report written by a seatswap before agents had a `provider` has none on
/// its agents: each such agent gets its session's `provider`, which is what it
/// meant then (a Claude Code row's agents were Claude Code's, a Harness row's
/// workers Harness's). Only the report read does this; everywhere else a
/// missing `provider` is the parse error it is. A session without a
/// `provider` text is left as it is, so its own missing field stays an error.
fn older_agents_take_their_sessions_provider(report: &mut serde_json::Value) {
    let Some(sessions) = report
        .get_mut("sessions")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for session in sessions {
        let Some(provider) = session
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(agents) = session
            .get_mut("agents")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for agent in agents {
            if let Some(agent) = agent.as_object_mut() {
                agent
                    .entry("provider")
                    .or_insert_with(|| serde_json::Value::String(provider.clone()));
            }
        }
    }
}

/// The key with the most output tokens; on a tie, the first in key order.
pub(crate) fn most_output<K: Ord + Clone>(by: &BTreeMap<K, u64>) -> Option<K> {
    let mut best: Option<(&K, u64)> = None;
    for (key, out) in by {
        if best.is_none_or(|(_, best_out)| *out > best_out) {
            best = Some((key, *out));
        }
    }
    best.map(|(key, _)| key.clone())
}

/// One token record for the live view: a model's or an agent's sum dated at its
/// last answer. Only the fields the valuer reads are filled. `minutes` is how
/// long the line lasts; only a written line's span is read, so a record built
/// only to value a row keeps today's 60 minutes.
fn token_record(
    session: &str,
    cwd: &str,
    project: &ProjectKey,
    model: &str,
    at: DateTime<Utc>,
    minutes: u32,
    tokens: Tokens,
) -> TokenRecord {
    TokenRecord {
        t: at,
        minutes,
        session: session.to_string(),
        cwd: cwd.to_string(),
        project: project.clone(),
        model: model.to_string(),
        input: tokens.input,
        out: tokens.out,
        cw: tokens.cw,
        cr: tokens.cr,
        answers: 0,
        agent_answers: 0,
    }
}

/// The `SessionStatus` of a session file as the page shows it.
fn live_status(status: &SessionStatus) -> LiveStatus {
    match status {
        SessionStatus::Busy => LiveStatus::Busy,
        SessionStatus::Idle => LiveStatus::Idle,
        SessionStatus::Waiting => LiveStatus::Waiting,
        SessionStatus::Other(text) => LiveStatus::Other(text.clone()),
    }
}

/// Milliseconds since the epoch as a time; `None` when it is not a valid time.
fn time_ms(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// A session's label: the project's name, a dash, and the first two characters
/// of the session id.
pub(crate) fn session_label(project: &str, session: &str) -> String {
    format!("{}-{}", project, short_id(session))
}

/// The first two characters of a session id, the part a label adds to a
/// project's name.
fn short_id(session: &str) -> String {
    session.chars().take(2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tempfile::tempdir;

    use crate::api::LimitKind;
    use crate::history::{AccountRef, Event, EventKind, WindowRecord};
    use crate::money::{Fx, ModelPrice, Prices};
    use crate::store::{Account, Settings};
    use crate::transcripts::SessionSeen;

    const SESSION: &str = "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34";
    const SESSION_11: &str = "11a2b3c4-d5e6-4f70-8a9b-0c1d2e3f4a5b";
    const AGENT: &str = "a8751b20660a1de6b";
    const ATLAS_CWD: &str = "/home/sam/projects/atlas";
    const AC_ACC: &str = "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04";
    const AC_ORG: &str = "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20";

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("a test time")
    }

    fn project_key() -> ProjectKey {
        ProjectKey::Repo("github.com/acme/atlas".to_string())
    }

    fn ac() -> AccountRef {
        AccountRef {
            acc: AC_ACC.to_string(),
            org: AC_ORG.to_string(),
        }
    }

    fn account(price: Option<f64>) -> Account {
        serde_json::from_value(json!({
            "id": "0b4f6a2c8d1e4f7a9c3b5d7e9f1a2b4c",
            "accountUuid": AC_ACC,
            "organizationUuid": AC_ORG,
            "label": "AC",
            "abbr": "AC",
            "color": "#3b82f6",
            "email": "sam@example.com",
            "orgName": "Acme",
            "plan": "Team Premium",
            "price": price,
            "priceCurrency": "EUR",
            "oauthAccount": {
                "accountUuid": AC_ACC,
                "organizationUuid": AC_ORG,
                "emailAddress": "sam@example.com"
            },
            "addedAt": "2026-09-01T00:00:00Z",
            "loginState": "ok",
            "autoBurn": true
        }))
        .expect("a test account")
    }

    fn settings_eur() -> Settings {
        Settings {
            currency: Some("EUR".to_string()),
            ..Settings::default()
        }
    }

    fn settings_chf() -> Settings {
        Settings {
            currency: Some("CHF".to_string()),
            ..Settings::default()
        }
    }

    fn prices() -> Prices {
        let mut prices = Prices::default();
        let list = ["claude-opus-5-5", "claude-fable-5-2"]
            .into_iter()
            .map(|model| {
                (
                    format!("anthropic/{model}"),
                    ModelPrice {
                        input: 4.0,
                        output: 20.0,
                        cache_write: 5.0,
                        cache_read: 0.2,
                        tiered: false,
                    },
                )
            })
            .collect();
        prices.take(list, utc("2026-01-01T00:00:00Z"));
        prices
    }

    fn token(session: &str, at: DateTime<Utc>, model: &str, input: u64, out: u64) -> TokenRecord {
        TokenRecord {
            t: at,
            minutes: 60,
            session: session.to_string(),
            cwd: ATLAS_CWD.to_string(),
            project: project_key(),
            model: model.to_string(),
            input,
            out,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        }
    }

    /// A token line of one 5-minute bucket.
    fn five_minute_token(
        session: &str,
        at: DateTime<Utc>,
        model: &str,
        input: u64,
        out: u64,
    ) -> TokenRecord {
        TokenRecord {
            minutes: transcripts::TOKEN_BUCKET_MINUTES as u32,
            ..token(session, at, model, input, out)
        }
    }

    fn live_event(at: DateTime<Utc>, account: Option<AccountRef>) -> Event {
        Event {
            t: at,
            kind: EventKind::Live { account },
        }
    }

    fn price_changed_event(
        at: DateTime<Utc>,
        amount: Option<f64>,
        currency: Option<&str>,
    ) -> Event {
        Event {
            t: at,
            kind: EventKind::PriceChanged {
                account: ac(),
                amount,
                currency: currency.map(str::to_string),
            },
        }
    }

    fn session_file() -> SessionFile {
        SessionFile {
            pid: 41822,
            session_id: SESSION.to_string(),
            cwd: ATLAS_CWD.to_string(),
            started_at: 1794612960232,
            proc_start: "19664719".to_string(),
            version: Some("2.1.322".to_string()),
            kind: Some("interactive".to_string()),
            entrypoint: Some("cli".to_string()),
            name: Some("atlas-a6".to_string()),
            messaging_socket_path: Some("/run/user/1000/cc-socks/41822.sock".to_string()),
            status: SessionStatus::Busy,
            status_updated_at: 1794681295320,
        }
    }

    fn seen(model: &str, out: u64) -> SessionSeen {
        let tokens = Tokens {
            input: 2_000_000,
            out,
            cw: 26_173,
            cr: 28_126,
        };
        let mut by_model = BTreeMap::new();
        by_model.insert(model.to_string(), tokens);
        SessionSeen {
            folder: "home-sam-projects-atlas".to_string(),
            cwd: ATLAS_CWD.to_string(),
            first: utc("2026-11-13T23:40:00Z"),
            last: utc("2026-11-14T18:30:00Z"),
            last_model: Some(model.to_string()),
            last_user_turn: None,
            thinking: Vec::new(),
            totals: tokens,
            by_model,
            agents: BTreeMap::new(),
            spawns: BTreeMap::new(),
            recent: Vec::new(),
            recorded: None,
            flash_jobs: BTreeSet::new(),
            title: None,
            custom_title: None,
        }
    }

    fn state_with(seen: SessionSeen) -> ScanState {
        let mut state = ScanState::default();
        state.sessions.insert(SESSION.to_string(), seen);
        state
    }

    fn session_record(
        session: &str,
        t: DateTime<Utc>,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
        closed: bool,
        agents_finished: u32,
    ) -> SessionRecord {
        SessionRecord {
            t,
            session: session.to_string(),
            cwd: ATLAS_CWD.to_string(),
            project: project_key(),
            entrypoint: Some("cli".to_string()),
            title: None,
            first,
            last,
            agents_started: agents_finished,
            agents_finished,
            closed,
        }
    }

    fn projects() -> Projects {
        let hosted = Hosted {
            host: "atlas".to_string(),
            record: session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ),
        };
        Projects::resolve(&[], &[hosted], &[])
    }

    fn known_pricing<'a>(valuer: &'a Valuer, settings: &'a Settings) -> Pricing<'a> {
        Pricing::Known { valuer, settings }
    }

    fn weekly_window() -> WindowRecord {
        let end = utc("2026-11-14T00:00:00Z");
        WindowRecord {
            account: ac(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(end - Duration::days(7)),
            end,
            final_percent: 80.0,
            lost: None,
            last_sample: end,
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        }
    }

    fn valuer(history: &HistoryDir, settings: &Settings, accounts: &[Account]) -> Valuer {
        valuer_with(history, settings, accounts, &prices())
    }

    /// A looked-up EUR rate of 1 so the fixture's EUR values stay EUR.
    fn fx_eur() -> Fx {
        let mut fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        fx.take(utc("2026-01-01T00:00:00Z").date_naive(), 1.0);
        fx
    }

    fn valuer_with(
        history: &HistoryDir,
        settings: &Settings,
        accounts: &[Account],
        prices: &Prices,
    ) -> Valuer {
        let range = Range::between(
            Some(utc("2026-11-10T00:00:00Z")),
            Some(utc("2026-11-15T00:00:00Z")),
        )
        .expect("a range");
        Valuer::build(
            history,
            &range,
            prices,
            &fx_eur(),
            settings,
            accounts,
            &[],
            &std::cell::RefCell::new(Vec::new()),
        )
        .expect("a valuer")
    }

    fn row(status: LiveStatus, model: Option<&str>, since: DateTime<Utc>) -> LiveSession {
        LiveSession {
            session: SESSION.to_string(),
            host: "atlas".to_string(),
            label: "atlas-7c".to_string(),
            title: None,
            project_id: ProjectId::of(&project_key(), "atlas"),
            project: "atlas".to_string(),
            org: Some("acme".to_string()),
            cwd: ATLAS_CWD.to_string(),
            provider: CLAUDE_PROVIDER.to_string(),
            model: model.map(str::to_string),
            tied: model.map(thinking_tied).unwrap_or(false),
            status,
            since,
            started: since,
            agents_running: 0,
            agents_done: 0,
            agents: Vec::new(),
            tokens: Tokens::default(),
            last_hour: Some(Tokens::default()),
            last_hour_values: Values::sum(&[]),
            agents_done_values: Values::sum(&[]),
            values: Values::sum(&[]),
        }
    }

    #[test]
    fn a_running_session_becomes_a_row() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-opus-5-5", 12_400_000));
        let rows = running(
            &[session_file()],
            &state,
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                // The session's one token line, not written yet.
                pending: &[token(
                    SESSION,
                    utc("2026-11-14T18:00:00Z"),
                    "claude-opus-5-5",
                    2_000_000,
                    12_400_000,
                )],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert!(rows.warnings.is_empty(), "{:?}", rows.warnings);
        assert_eq!(rows.sessions.len(), 1);
        let row = &rows.sessions[0];
        assert_eq!(row.label, "atlas-a6");
        assert_eq!(row.provider, "Claude Code");
        assert!(!row.tied);
        assert_eq!(row.status, LiveStatus::Busy);
        assert_eq!(row.tokens.out, 12_400_000);
        assert_eq!(row.project, "atlas");
        assert_eq!(row.org.as_deref(), Some("acme"));
        assert_eq!(row.started, utc("2026-11-13T23:36:00.232Z"));
        assert_eq!(row.since, utc("2026-11-14T18:34:55.320Z"));
    }

    #[test]
    fn a_live_row_carries_its_transcripts_title() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let mut seen = seen("claude-opus-5-5", 12_400_000);
        seen.title = Some("Fix the invoice export".to_string());
        let state = state_with(seen);
        let rows = running(
            &[session_file()],
            &state,
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.title.as_deref(), Some("Fix the invoice export"));
        assert_eq!(row.label, "atlas-a6", "the label keeps its meaning");
    }

    #[test]
    fn a_renamed_session_is_named_by_its_new_name() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let mut seen = seen("claude-opus-5-5", 12_400_000);
        seen.title = Some("Fix the invoice export".to_string());
        // `/rename` in Claude Code: the user's name wins, as Claude Code shows it.
        seen.custom_title = Some("tripwire".to_string());
        let state = state_with(seen);
        let rows = running(
            &[session_file()],
            &state,
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.title.as_deref(), Some("tripwire"));
        assert_eq!(row.label, "atlas-a6", "the label keeps its meaning");
    }

    #[test]
    fn without_a_title_the_row_has_only_its_label() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-opus-5-5", 12_400_000));
        let rows = running(
            &[session_file()],
            &state,
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.title, None);
        assert_eq!(row.label, "atlas-a6");
    }

    #[test]
    fn agents_are_counted_running_and_done() {
        let metas: Vec<AgentMeta> = (0..41)
            .map(|i| AgentMeta {
                session: SESSION.to_string(),
                agent: format!("a8751b20660a1de{i:02x}"),
                agent_type: Some("general-purpose".to_string()),
                description: Some("fix the failing tests".to_string()),
                finished: i >= 3,
                spawn_depth: Some(1),
                tool_use_id: Some(format!("toolu_{i:02}")),
                parent: None,
                transcript_ended: false,
            })
            .collect();
        let metas_of = |_: &str| metas.clone();
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-opus-5-5", 1_000_000));
        let rows = running(
            &[session_file()],
            &state,
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.agents_running, 3);
        assert_eq!(row.agents_done, 38);
        assert_eq!(row.agents.len(), 41);
    }

    #[test]
    fn last_hour_is_valued_per_model() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        // Two prices, so a sum of both models cannot be mistaken for one model's.
        let mut prices = prices();
        prices
            .models
            .get_mut("anthropic/claude-fable-5-2")
            .and_then(|points| points.last_mut())
            .expect("the fable price")
            .price
            .output = 40.0;
        let valuer = valuer_with(&history, &settings, &[], &prices);
        let minute = utc("2026-11-14T18:30:00Z");
        let mut seen = seen("claude-opus-5-5", 1_000_000);
        let mut of_minute = BTreeMap::new();
        of_minute.insert(
            "claude-opus-5-5".to_string(),
            Tokens {
                out: 1_000_000,
                ..Tokens::default()
            },
        );
        of_minute.insert(
            "claude-fable-5-2".to_string(),
            Tokens {
                out: 500_000,
                ..Tokens::default()
            },
        );
        seen.recent = vec![
            // A minute starting exactly at `now - 60 min` is not in the hour.
            (utc("2026-11-14T18:00:00Z"), {
                let mut outside = BTreeMap::new();
                outside.insert(
                    "claude-opus-5-5".to_string(),
                    Tokens {
                        out: 9_000_000,
                        ..Tokens::default()
                    },
                );
                outside.insert(
                    "claude-fable-5-2".to_string(),
                    Tokens {
                        out: 9_000_000,
                        ..Tokens::default()
                    },
                );
                outside
            }),
            (minute, of_minute),
        ];
        let now = utc("2026-11-14T19:00:00Z");

        // Each model's own part: its tokens of the hour and its newest minute.
        let by_model = transcripts::last_hour_by_model(&seen, now);
        assert_eq!(by_model.len(), 2, "one part per model");
        assert_eq!(
            by_model["claude-opus-5-5"],
            (
                minute,
                Tokens {
                    out: 1_000_000,
                    ..Tokens::default()
                }
            ),
            "opus's own part"
        );
        assert_eq!(
            by_model["claude-fable-5-2"],
            (
                minute,
                Tokens {
                    out: 500_000,
                    ..Tokens::default()
                }
            ),
            "fable's own part"
        );

        let rows = running(
            &[session_file()],
            &state_with(seen),
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            now,
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(
            row.last_hour.expect("a known hour").out,
            1_500_000,
            "both models' minutes; the boundary minute is not counted"
        );
        assert_eq!(
            row.last_hour_values.api,
            Some(40.0),
            "20 for opus and 20 for fable, in two parts"
        );
        assert_eq!(row.last_hour_values.api_currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn completed_agents_have_a_summed_value() {
        let mut seen = seen("claude-opus-5-5", 1_000_000);
        let priced = AgentSeen {
            first: utc("2026-11-14T18:10:00Z"),
            last: utc("2026-11-14T18:21:00Z"),
            model: Some("claude-opus-5-5".to_string()),
            tokens: Tokens {
                out: 1_000_000,
                ..Tokens::default()
            },
            answers: 1,
        };
        let unpriced = AgentSeen {
            first: utc("2026-11-14T18:00:00Z"),
            last: utc("2026-11-14T18:20:00Z"),
            model: Some("claude-x".to_string()),
            tokens: Tokens {
                out: 1_000_000,
                ..Tokens::default()
            },
            answers: 1,
        };
        // Sorted by id, the unpriced agent's reason comes first in the sum.
        seen.agents.insert("a1111b20660a1de6".to_string(), unpriced);
        seen.agents.insert("a2221b20660a1de6".to_string(), priced);
        let metas = vec![
            AgentMeta {
                session: SESSION.to_string(),
                agent: "a1111b20660a1de6".to_string(),
                agent_type: Some("general-purpose".to_string()),
                description: Some("fix the failing tests".to_string()),
                finished: true,
                spawn_depth: Some(1),
                tool_use_id: None,
                parent: None,
                transcript_ended: true,
            },
            AgentMeta {
                session: SESSION.to_string(),
                agent: "a2221b20660a1de6".to_string(),
                agent_type: Some("general-purpose".to_string()),
                description: Some("fix the failing tests".to_string()),
                finished: true,
                spawn_depth: Some(1),
                tool_use_id: None,
                parent: None,
                transcript_ended: true,
            },
        ];
        let metas_of = |_: &str| metas.clone();
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &state_with(seen),
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.agents_done, 2);
        assert!(
            row.agents_done_values.api.is_some(),
            "the priced agent's API value is the known part: {:?}",
            row.agents_done_values
        );
        assert!(row.agents_done_values.api_partial);
        assert_eq!(
            row.agents_done_values.api_missing.as_deref(),
            Some("no price for claude-x"),
            "the unpriced agent's reason stays in the sum"
        );
        assert!(
            row.agents_done_values.subscription.is_none(),
            "no agent's subscription share is known here"
        );
        assert!(!row.agents_done_values.subscription_partial);
        assert_eq!(
            row.agents_done_values.subscription_missing.as_deref(),
            Some("no price for claude-x")
        );
    }

    #[test]
    fn an_agent_without_finished_whose_last_entry_is_end_turn_is_done() {
        let meta = AgentMeta {
            session: SESSION.to_string(),
            agent: AGENT.to_string(),
            agent_type: Some("general-purpose".to_string()),
            description: Some("fix the failing tests".to_string()),
            finished: false,
            spawn_depth: Some(1),
            tool_use_id: None,
            parent: None,
            transcript_ended: true,
        };
        let metas_of = |_: &str| vec![meta.clone()];
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.agents_running, 0, "the transcript ended with end_turn");
        assert_eq!(row.agents_done, 1);
        assert!(!row.agents[0].running);
    }

    #[test]
    fn a_resumed_agent_runs_again() {
        let dir = tempdir().expect("tempdir");
        let claude = crate::paths::ClaudePaths::for_config_dir(dir.path(), dir.path());
        let subagents = claude
            .projects_dir()
            .join("-home-sam-projects-atlas")
            .join(SESSION)
            .join("subagents");
        std::fs::create_dir_all(&subagents).expect("the subagents folder");
        std::fs::write(
            subagents.join(format!("agent-{AGENT}.meta.json")),
            json!({
                "agentType": "general-purpose",
                "description": "fix the failing tests",
                "spawnDepth": 1,
                "toolUseId": null
            })
            .to_string(),
        )
        .expect("the meta file");
        let transcript = subagents.join(format!("agent-{AGENT}.jsonl"));
        let end_turn = json!({
            "type": "assistant",
            "timestamp": "2026-11-14T18:30:00Z",
            "cwd": ATLAS_CWD,
            "message": {
                "id": "msg_agent_a",
                "model": "claude-opus-5-5",
                "content": [],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 166,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        })
        .to_string();
        std::fs::write(&transcript, format!("{end_turn}\n")).expect("the transcript");

        let first = crate::transcripts::agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut crate::transcripts::AgentEnds::default(),
        );
        assert_eq!(first.metas.len(), 1);
        assert!(
            first.metas[0].transcript_ended,
            "the last entry is end_turn"
        );
        let metas = first.metas.clone();
        let settings = settings_eur();
        let history = HistoryDir::at(dir.path(), "atlas");
        let valuer = valuer(&history, &settings, &[]);
        let first_rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &|_: &str| metas.clone(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );
        assert_eq!(first_rows.sessions[0].agents_done, 1);
        assert_eq!(first_rows.sessions[0].agents_running, 0);

        // The agent is resumed: a new user turn is the transcript's last entry.
        let mut text = std::fs::read_to_string(&transcript).expect("the transcript");
        text.push_str(
            &json!({
                "type": "user",
                "timestamp": "2026-11-14T18:40:00Z",
                "sessionId": SESSION,
                "message": {"role": "user", "content": "fix the failing tests"}
            })
            .to_string(),
        );
        text.push('\n');
        std::fs::write(&transcript, text).expect("the resumed transcript");

        let again = crate::transcripts::agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut crate::transcripts::AgentEnds::default(),
        );
        assert_eq!(again.metas.len(), 1);
        assert!(
            !again.metas[0].transcript_ended,
            "a new user turn after end_turn means the agent runs again"
        );
        let metas = again.metas.clone();
        let second_rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &|_: &str| metas.clone(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );
        assert_eq!(second_rows.sessions[0].agents_running, 1);
        assert_eq!(second_rows.sessions[0].agents_done, 0);
        assert!(second_rows.sessions[0].agents[0].running);
    }

    #[test]
    fn an_unreadable_meta_still_counts_as_running() {
        let dir = tempdir().expect("tempdir");
        let claude = crate::paths::ClaudePaths::for_config_dir(dir.path(), dir.path());
        let subagents = claude
            .projects_dir()
            .join("-home-sam-projects-atlas")
            .join(SESSION)
            .join("subagents");
        std::fs::create_dir_all(&subagents).expect("the subagents folder");
        std::fs::write(
            subagents.join(format!("agent-{AGENT}.meta.json")),
            "{\"agentType\": ",
        )
        .expect("the unreadable meta");
        std::fs::write(
            subagents.join(format!("agent-{AGENT}.jsonl")),
            format!(
                "{}\n",
                json!({
                    "type": "user",
                    "timestamp": "2026-11-14T18:40:00Z",
                    "sessionId": SESSION,
                    "message": {"role": "user", "content": "fix the failing tests"}
                })
            ),
        )
        .expect("the transcript");

        let agents = crate::transcripts::agents_of(
            &claude,
            SESSION,
            &BTreeMap::new(),
            &mut crate::transcripts::AgentEnds::default(),
        );
        assert_eq!(agents.metas.len(), 1);
        assert_eq!(agents.warnings.len(), 1, "the unreadable meta is a warning");
        assert!(!agents.metas[0].finished);
        assert!(
            !agents.metas[0].transcript_ended,
            "the last entry is not an end_turn answer"
        );
        let metas = agents.metas.clone();
        let settings = settings_eur();
        let history = HistoryDir::at(dir.path(), "atlas");
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &|_: &str| metas.clone(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert_eq!(rows.sessions[0].agents_running, 1);
        assert_eq!(rows.sessions[0].agents_done, 0);
        assert!(rows.sessions[0].agents[0].running);
    }

    #[test]
    fn a_fable_session_is_tied() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-fable-5-2", 1_000_000));
        let rows = running(
            &[session_file()],
            &state,
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert!(rows.sessions[0].tied);
    }

    #[test]
    fn switch_kinds() {
        let now = utc("2026-11-14T19:00:00Z");
        let idle = row(
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now - Duration::minutes(61),
        );
        assert_eq!(switch_kind(&idle, 0, now), SwitchKind::Free);
        let idle = row(
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now - Duration::minutes(10),
        );
        assert_eq!(switch_kind(&idle, 0, now), SwitchKind::Resend);
        let busy = row(LiveStatus::Busy, Some("claude-fable-5-2"), now);
        assert_eq!(switch_kind(&busy, 3, now), SwitchKind::LosesThinking);
        assert_eq!(switch_kind(&busy, 0, now), SwitchKind::Resend);
        let closed = row(LiveStatus::Closed, Some("claude-fable-5-2"), now);
        assert_eq!(switch_kind(&closed, 3, now), SwitchKind::Free);
    }

    #[test]
    fn values_come_from_the_valuer_and_say_why_when_missing() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&Event {
                t: utc("2026-11-06T00:00:00Z"),
                kind: EventKind::Live {
                    account: Some(ac()),
                },
            })
            .expect("a live event");
        history.append(&weekly_window()).expect("a window");
        let accounts = [account(Some(140.0))];
        let opus = token(
            SESSION,
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            0,
        );
        let unknown = token(
            SESSION,
            utc("2026-11-10T10:30:00Z"),
            "claude-x",
            1_000_000,
            0,
        );

        let no_currency_settings = Settings::default();
        let no_currency = valuer(&history, &no_currency_settings, &accounts);
        let no_currency = Values::of(
            &[no_currency.value("atlas", &opus)],
            &no_currency_settings,
            &Fx::default(),
            Utc::now(),
        );
        assert_eq!(no_currency.api, None);
        assert_eq!(
            no_currency.api_missing.as_deref(),
            Some("set a currency in Settings → General")
        );
        assert_eq!(no_currency.subscription, None);
        assert_eq!(
            no_currency.subscription_missing.as_deref(),
            Some("set a currency in Settings → General")
        );
        assert_eq!(no_currency.currency, None);

        let eur = settings_eur();
        let priced = valuer(&history, &eur, &accounts);
        let no_price = Values::of(
            &[priced.value("atlas", &unknown)],
            &eur,
            &Fx::default(),
            Utc::now(),
        );
        assert_eq!(no_price.api, None);
        assert_eq!(
            no_price.api_missing.as_deref(),
            Some("no price for claude-x")
        );
        assert_eq!(
            no_price.subscription_missing.as_deref(),
            Some("no price for claude-x")
        );

        let chf = settings_chf();
        let in_usd = valuer(&history, &chf, &accounts);
        let in_usd = Values::of(
            &[in_usd.value("atlas", &opus)],
            &chf,
            &Fx::default(),
            Utc::now(),
        );
        assert_eq!(in_usd.api, Some(4.0));
        assert_eq!(in_usd.api_currency.as_deref(), Some("USD"));
        assert_eq!(in_usd.api_missing, None);
        assert_eq!(in_usd.currency.as_deref(), Some("CHF"));
    }

    #[test]
    fn closed_sessions_come_from_the_history() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:05:00Z"),
                "claude-opus-5-5",
                0,
                4_000_000,
            ))
            .expect("a token record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:10:00Z"),
                "claude-opus-5-5",
                0,
                3_900_000,
            ))
            .expect("a token record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert!(rows.warnings.is_empty(), "{:?}", rows.warnings);
        assert_eq!(rows.sessions.len(), 1);
        let row = &rows.sessions[0];
        assert_eq!(row.status, LiveStatus::Closed);
        assert_eq!(row.since, utc("2026-11-14T09:12:00Z"));
        assert_eq!(row.started, utc("2026-11-14T08:00:00Z"));
        assert_eq!(row.agents_done, 17);
        assert!(row.agents.is_empty());
        assert_eq!(row.label, "atlas-11");
        assert_eq!(row.project, "atlas");
        assert_eq!(row.tokens.out, 7_900_000);
    }

    #[test]
    fn a_closed_row_carries_its_records_title() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let mut record = session_record(
            SESSION_11,
            utc("2026-11-14T09:12:00Z"),
            utc("2026-11-14T08:00:00Z"),
            utc("2026-11-14T09:12:00Z"),
            true,
            0,
        );
        record.title = Some("Fix the invoice export".to_string());
        history.append(&record).expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        let row = &rows.sessions[0];
        assert_eq!(row.title.as_deref(), Some("Fix the invoice export"));
        assert_eq!(row.label, "atlas-11");
    }

    #[test]
    fn a_closed_row_keeps_the_hour_of_its_first_answer() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:40:00Z"),
                utc("2026-11-14T08:12:00Z"),
                utc("2026-11-14T09:40:00Z"),
                true,
                0,
            ))
            .expect("a session record");
        // `token_records` writes the start of the hour, so the first answer at
        // 08:12 has its record at 08:00.
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T08:00:00Z"),
                "claude-opus-5-5",
                0,
                4_000_000,
            ))
            .expect("a token record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:00:00Z"),
                "claude-opus-5-5",
                0,
                3_000_000,
            ))
            .expect("a token record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T07:00:00Z"),
            utc("2026-11-14T10:10:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(rows.sessions[0].tokens.out, 7_000_000);
    }

    #[test]
    fn a_closed_rows_last_hour_counts_the_hour_that_overlaps() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:58:00Z"),
                utc("2026-11-14T09:50:00Z"),
                utc("2026-11-14T09:58:00Z"),
                true,
                0,
            ))
            .expect("a session record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:00:00Z"),
                "claude-opus-5-5",
                0,
                3_000_000,
            ))
            .expect("a token record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:20:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(
            rows.sessions[0].last_hour.expect("a known hour").out,
            3_000_000,
            "the 09:00 hour overlaps [09:20, 10:20)"
        );
    }

    #[test]
    fn closed_rows_last_hour_counts_five_minute_lines_inside_the_hour() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T10:16:00Z"),
                utc("2026-11-14T09:40:00Z"),
                utc("2026-11-14T10:15:00Z"),
                true,
                0,
            ))
            .expect("a session record");
        // One line before the window, whose 5 minutes end at 09:15, and two
        // inside it.
        for (at, out) in [
            ("2026-11-14T09:10:00Z", 4_000_000),
            ("2026-11-14T09:55:00Z", 1_000_000),
            ("2026-11-14T10:05:00Z", 2_000_000),
        ] {
            history
                .append(&five_minute_token(
                    SESSION_11,
                    utc(at),
                    "claude-opus-5-5",
                    0,
                    out,
                ))
                .expect("a token record");
        }
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:20:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(
            rows.sessions[0].last_hour.expect("a known hour").out,
            3_000_000,
            "the lines of 09:55 and 10:05 overlap [09:20, 10:20), not the one at 09:10"
        );
        assert_eq!(rows.sessions[0].tokens.out, 7_000_000);
    }

    #[test]
    fn a_session_closed_before_since_is_left_out() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:30:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T08:30:00Z"),
                true,
                3,
            ))
            .expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert!(rows.sessions.is_empty());
    }

    #[test]
    fn an_agent_without_an_answer_has_no_time() {
        let meta = AgentMeta {
            session: SESSION.to_string(),
            agent: AGENT.to_string(),
            agent_type: Some("general-purpose".to_string()),
            description: Some("fix the failing tests".to_string()),
            finished: false,
            spawn_depth: Some(1),
            tool_use_id: Some("toolu_01Nkn4".to_string()),
            parent: None,
            transcript_ended: false,
        };
        let metas_of = |_: &str| vec![meta.clone()];
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-opus-5-5", 1_000_000));
        let rows = running(
            &[session_file()],
            &state,
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert_eq!(rows.sessions[0].agents.len(), 1);
        let agent = &rows.sessions[0].agents[0];
        assert_eq!(agent.first, None);
        assert_eq!(agent.last, None);
        assert!(agent.running);
        assert_eq!(agent.kind.as_deref(), Some("general-purpose"));
        assert_eq!(agent.task.as_deref(), Some("fix the failing tests"));
        assert_eq!(agent.model, None);
        assert_eq!(
            agent.values.api_missing.as_deref(),
            Some("the agent's model is not known yet")
        );
        assert_eq!(
            agent.values.subscription_missing.as_deref(),
            Some("the agent's model is not known yet")
        );
    }

    #[test]
    fn values_of_two_currencies_are_not_added() {
        let eur = Valued {
            api: Some(1.5),
            subscription: Some(0.5),
            account: None,
            account_from_readings: false,
            company: money::Company::Anthropic,
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            subscription_missing: None,
        };
        let usd = Valued {
            api: Some(2.0),
            subscription: Some(0.5),
            account: None,
            account_from_readings: false,
            company: money::Company::Anthropic,
            api_currency: Some("USD".to_string()),
            api_missing: None,
            subscription_missing: None,
        };
        let values = Values::of(&[eur, usd], &settings_eur(), &Fx::default(), Utc::now());

        assert_eq!(values.api, None);
        assert_eq!(values.api_currency, None);
        assert_eq!(values.api_missing.as_deref(), Some(TWO_CURRENCIES));
        assert_eq!(values.subscription, Some(1.0));
        assert_eq!(values.currency.as_deref(), Some("EUR"));

        let empty = Values::of(&[], &settings_eur(), &Fx::default(), Utc::now());
        assert_eq!(empty.api, Some(0.0), "nothing used is a known 0");
        assert_eq!(
            empty.api_currency.as_deref(),
            Some("USD"),
            "no rate is stored, so the API column is in USD"
        );
        assert_eq!(empty.api_missing, None);
        assert_eq!(empty.subscription, Some(0.0));
        assert_eq!(empty.currency.as_deref(), Some("EUR"));
        assert_eq!(empty.subscription_missing, None);
    }

    #[test]
    fn a_closed_session_sums_every_token_line_of_an_hour() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                0,
            ))
            .expect("a session record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:10:00Z"),
                "claude-opus-5-5",
                0,
                1_000_000,
            ))
            .expect("a token record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                "claude-opus-5-5",
                0,
                2_000_000,
            ))
            .expect("a token record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        let row = &rows.sessions[0];
        assert_eq!(row.tokens.out, 3_000_000);
        assert_eq!(row.values.api, Some(60.0));
    }

    #[test]
    fn a_closed_claude_row_keeps_its_claude_model_over_a_worker() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:20:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:20:00Z"),
                true,
                0,
            ))
            .expect("a session record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:10:00Z"),
                "claude-opus-5-5",
                0,
                100,
            ))
            .expect("the Claude record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:20:00Z"),
                "deepseek-v4.1-flash:cloud",
                1_000_000,
                5_000_000,
            ))
            .expect("the worker's record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(
            rows.sessions[0].model.as_deref(),
            Some("claude-opus-5-5"),
            "the worker's newer model never becomes the Claude Code row's model"
        );
    }

    /// [`prices`] with the flash workers' model priced under models.dev's
    /// `ollama-cloud` list, where [`money::model_price_key`] looks it up.
    fn prices_with_flash() -> Prices {
        let mut prices = prices();
        prices.take(
            BTreeMap::from([
                (
                    "anthropic/claude-opus-5-5".to_string(),
                    ModelPrice {
                        input: 4.0,
                        output: 20.0,
                        cache_write: 5.0,
                        cache_read: 0.2,
                        tiered: false,
                    },
                ),
                (
                    "anthropic/claude-fable-5-2".to_string(),
                    ModelPrice {
                        input: 4.0,
                        output: 20.0,
                        cache_write: 5.0,
                        cache_read: 0.2,
                        tiered: false,
                    },
                ),
                (
                    money::model_price_key(FLASH_MODEL),
                    ModelPrice {
                        input: 0.3,
                        output: 1.2,
                        cache_write: 0.0,
                        cache_read: 0.0,
                        tiered: false,
                    },
                ),
            ]),
            utc("2026-01-02T00:00:00Z"),
        );
        prices
    }

    /// A running Claude Code row and the same closed row with a linked worker
    /// show the same tokens and values: they do not jump when the session
    /// closes. The running row reads the session's records (its own answers
    /// and the worker's recorded growth) plus this pass's worker growth that is
    /// written only after the rows; the worker put under it is not added again.
    #[test]
    fn a_running_and_closed_row_with_a_worker_have_the_same_numbers() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        history
            .append(&live_event(utc("2026-11-14T00:00:00Z"), Some(ac())))
            .expect("a live event");
        history
            .append(&price_changed_event(
                utc("2026-11-14T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let mut window = weekly_window();
        window.start = Some(utc("2026-11-14T00:00:00Z"));
        window.end = utc("2026-11-21T00:00:00Z");
        window.last_sample = utc("2026-11-20T00:00:00Z");
        history.append(&window).expect("a finished window");
        let claude = TokenRecord {
            t: utc("2026-11-14T18:00:00Z"),
            minutes: 60,
            session: SESSION.to_string(),
            cwd: ATLAS_CWD.to_string(),
            project: project_key(),
            model: "claude-opus-5-5".to_string(),
            input: 2_000_000,
            out: 12_400_000,
            cw: 26_173,
            cr: 28_126,
            answers: 41,
            agent_answers: 0,
        };
        history.append(&claude).expect("the Claude record");
        let mut recorded_worker = token(
            SESSION,
            utc("2026-11-14T18:00:00Z"),
            FLASH_MODEL,
            2_200,
            1_100,
        );
        recorded_worker.answers = 0;
        recorded_worker.agent_answers = 4;
        history
            .append(&recorded_worker)
            .expect("the worker's recorded line");
        // This pass's growth of the worker, written after the rows are built,
        // and another session's line that is not this row's.
        let mut growth = token(
            SESSION,
            utc("2026-11-14T18:00:00Z"),
            FLASH_MODEL,
            1_000,
            500,
        );
        growth.answers = 0;
        growth.agent_answers = 2;
        let elsewhere = token(
            OWN_SESSION,
            utc("2026-11-14T18:00:00Z"),
            FLASH_MODEL,
            9_000,
            9_000,
        );
        let valuer = valuer_with(&history, &settings, &[], &prices_with_flash());
        let pricing = known_pricing(&valuer, &settings);
        let state = state_with(seen("claude-opus-5-5", claude.out));
        let from = records_from(&[session_file()], &state).expect("a running session");
        assert_eq!(from, utc("2026-11-13T23:00:00Z"), "the hour of its start");
        let (recorded, warnings) = RecordedTokens::read(&history, from).expect("the records");
        assert!(warnings.is_empty(), "{warnings:?}");

        let rows = running(
            &[session_file()],
            &state,
            &|_| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &recorded,
                pending: &[growth.clone(), elsewhere],
            },
            Some(&projects()),
            &pricing,
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );
        let mut running_row = rows.sessions[0].clone();
        let worker_record = token(
            SESSION,
            utc("2026-11-14T18:00:00Z"),
            FLASH_MODEL,
            3_200,
            1_600,
        );
        add_agents(
            &mut running_row,
            vec![LiveAgent {
                agent: WORKER_A.to_string(),
                parent: None,
                provider: HARNESS_PROVIDER.to_string(),
                kind: Some("flash".to_string()),
                task: None,
                model: Some(FLASH_MODEL.to_string()),
                running: true,
                first: Some(utc("2026-11-14T18:30:00Z")),
                last: Some(utc("2026-11-14T18:35:00Z")),
                tokens: Tokens {
                    input: 3_200,
                    out: 1_600,
                    cw: 0,
                    cr: 0,
                },
                values: pricing.of(&[valuer.value("atlas", &worker_record)]),
            }],
        );
        assert_eq!(
            running_row.tokens.out,
            12_400_000 + 1_100 + 500,
            "the Claude answers, the worker's recorded line and its growth, once"
        );
        assert_eq!(running_row.agents_running, 1);

        // The pass writes the growth; the session then closes.
        history.append(&growth).expect("the growth is written");
        history
            .append(&session_record(
                SESSION,
                utc("2026-11-14T18:40:00Z"),
                utc("2026-11-13T23:40:00Z"),
                utc("2026-11-14T18:40:00Z"),
                true,
                1,
            ))
            .expect("the closed session");
        let closed_rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T19:00:00Z"),
            Some(&projects()),
            &pricing,
        )
        .expect("rows");
        let closed_row = &closed_rows.sessions[0];

        assert_eq!(running_row.tokens, closed_row.tokens);
        assert_eq!(running_row.values, closed_row.values);
        assert!(
            running_row.values.api.is_some(),
            "the API value is known: {:?}",
            running_row.values
        );
        assert!(
            running_row.values.subscription.is_some(),
            "the priced part's share is kept: {:?}",
            running_row.values
        );
        assert!(
            running_row.values.subscription_partial,
            "the worker's Ollama history names no month window here, so its share is unknown"
        );
        assert_eq!(
            running_row.values.subscription_missing.as_deref(),
            Some("no monthly allowance in Ollama's reading")
        );
    }

    /// Fault B: a running session whose answers lie in two weekly windows is
    /// valued record by record, each in its own week, exactly as the closed
    /// row will be; not as one record dated at its last answer, which put the
    /// first week's tokens into the second week's share.
    #[test]
    fn a_running_session_is_valued_hour_by_hour() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        history
            .append(&live_event(utc("2026-11-06T00:00:00Z"), Some(ac())))
            .expect("a live event");
        history
            .append(&price_changed_event(
                utc("2026-11-01T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        // The week up to the reset at 11-14 used 80 %, the next one 40 %.
        history.append(&weekly_window()).expect("the first week");
        let mut second = weekly_window();
        second.start = Some(utc("2026-11-14T00:00:00Z"));
        second.end = utc("2026-11-21T00:00:00Z");
        second.final_percent = 40.0;
        second.last_sample = utc("2026-11-20T00:00:00Z");
        history.append(&second).expect("the second week");
        let before = token(
            SESSION,
            utc("2026-11-13T10:00:00Z"),
            "claude-opus-5-5",
            0,
            1_000_000,
        );
        let after = token(
            SESSION,
            utc("2026-11-14T18:00:00Z"),
            "claude-opus-5-5",
            0,
            1_000_000,
        );
        history.append(&before).expect("before the reset");
        history.append(&after).expect("after the reset");
        let accounts = [account(Some(140.0))];
        let valuer = valuer(&history, &settings, &accounts);
        let pricing = known_pricing(&valuer, &settings);
        let mut seen = seen("claude-opus-5-5", 2_000_000);
        seen.first = utc("2026-11-13T09:50:00Z");
        let state = state_with(seen);
        let from = records_from(&[session_file()], &state).expect("a running session");
        let (recorded, _) = RecordedTokens::read(&history, from).expect("the records");

        let rows = running(
            &[session_file()],
            &state,
            &|_| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &recorded,
                pending: &[],
            },
            Some(&projects()),
            &pricing,
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.tokens.out, 2_000_000);
        assert_eq!(row.values.api, Some(40.0), "20 EUR in each hour");
        // Each record is the whole API value of its week, so it takes the
        // week's whole used money: 80 % of the first week and 40 % of the
        // second, at 140 EUR a month.
        let expected = money::percent_value(140.0) * (80.0 + 40.0);
        let subscription = row.values.subscription.expect("a subscription value");
        assert!(
            (subscription - expected).abs() < 1e-9,
            "{subscription} is not {expected}"
        );
        let per_record = Values::of(
            &[
                valuer.value("atlas", &before),
                valuer.value("atlas", &after),
            ],
            &settings,
            valuer.fx(),
            Utc::now(),
        );
        assert_eq!(row.values, per_record, "the closed row's way");
    }

    #[test]
    fn recorded_tokens_keep_the_lines_written_after_the_read() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        let early = token(
            SESSION,
            utc("2026-11-14T07:00:00Z"),
            "claude-opus-5-5",
            0,
            100,
        );
        let read_line = token(
            SESSION,
            utc("2026-11-14T08:00:00Z"),
            "claude-opus-5-5",
            0,
            200,
        );
        history.append(&early).expect("an earlier line");
        history.append(&read_line).expect("a line of the read");
        vega.append(&token(
            SESSION,
            utc("2026-11-14T08:00:00Z"),
            "claude-opus-5-5",
            0,
            9_000,
        ))
        .expect("another host's line of the same id");

        assert!(!RecordedTokens::default().covers(utc("2026-11-14T08:00:00Z")));
        let (mut recorded, _) =
            RecordedTokens::read(&history, utc("2026-11-14T08:00:00Z")).expect("the records");
        assert!(recorded.covers(utc("2026-11-14T09:00:00Z")));
        assert!(!recorded.covers(utc("2026-11-14T07:00:00Z")));
        assert_eq!(
            recorded.of(SESSION),
            std::slice::from_ref(&read_line),
            "only this host's lines from the read's start"
        );

        let written = token(
            SESSION,
            utc("2026-11-14T09:00:00Z"),
            "claude-opus-5-5",
            0,
            300,
        );
        recorded.add(&[early, written.clone()]);
        assert_eq!(
            recorded.of(SESSION),
            [read_line, written.clone()],
            "a line before the start is not kept"
        );
        let mut nothing = RecordedTokens::default();
        nothing.add(std::slice::from_ref(&written));
        assert!(nothing.of(SESSION).is_empty(), "no read, nothing kept");
    }

    /// A Codex row's subscription is a known 0, so it never blanks the Claude
    /// totals; its API half still says why it has no price.
    #[test]
    fn a_codex_row_without_a_price_has_no_known_subscription() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        history
            .append(&live_event(utc("2026-11-14T00:00:00Z"), Some(ac())))
            .expect("a live event");
        history
            .append(&price_changed_event(
                utc("2026-11-14T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let mut window = weekly_window();
        window.start = Some(utc("2026-11-14T00:00:00Z"));
        window.end = utc("2026-11-21T00:00:00Z");
        window.last_sample = utc("2026-11-20T00:00:00Z");
        history.append(&window).expect("a finished window");
        let claude_record = token(
            SESSION,
            utc("2026-11-14T18:30:00Z"),
            "claude-opus-5-5",
            0,
            1_000_000,
        );
        history.append(&claude_record).expect("the Claude record");
        let valuer = valuer(&history, &settings, &[]);
        let pricing = known_pricing(&valuer, &settings);

        let mut claude = row(
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            utc("2026-11-14T18:30:00Z"),
        );
        claude.values = pricing.of(&[valuer.value("atlas", &claude_record)]);
        let now = utc("2026-11-14T19:00:00Z");
        let session = crate::codex_sessions::CodexSession {
            id: "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30".to_string(),
            cwd: PathBuf::from(ATLAS_CWD),
            model: Some("gpt-6.1-sol".to_string()),
            started: now - Duration::minutes(30),
            last: now - Duration::minutes(5),
            tokens: Tokens {
                input: 1000,
                out: 100,
                cw: 0,
                cr: 500,
            },
            series: Vec::new(),
            last_hour: Some(Tokens::default()),
            busy: false,
            title: Some("raspberry".to_string()),
            file: PathBuf::from("/home/sam/.codex/sessions/2026/11/14/rollout-now.jsonl"),
            recorded_file: PathBuf::from("2026/11/14/rollout-now.jsonl"),
        };
        let codex = codex_rows(
            &[session],
            &BTreeSet::new(),
            None,
            &pricing,
            "atlas",
            now,
            &|_| project_key(),
        );
        let codex = &codex.sessions[0];
        assert_eq!(
            codex.title.as_deref(),
            Some("raspberry"),
            "Codex's thread name names the row"
        );
        assert_eq!(
            codex.values.subscription, None,
            "Codex's share of its own subscription is unknown here, never a 0"
        );
        assert_eq!(
            codex.values.subscription_missing.as_deref(),
            Some("no price for gpt-6.1-sol"),
            "the subscription half names why it cannot be split"
        );
        assert_eq!(
            codex.values.api_missing.as_deref(),
            Some("no price for gpt-6.1-sol"),
            "the API half keeps the price rule's reason"
        );

        // P-2: a provider row's subscription is its own; a row without one keeps
        // the known part of the total and makes the half partial (R3).
        let all = Values::sum(&[&claude.values, &codex.values]);
        assert_eq!(all.subscription, claude.values.subscription);
        assert!(all.subscription_partial);
        assert_eq!(
            all.subscription_missing.as_deref(),
            Some("no price for gpt-6.1-sol")
        );
        assert_eq!(all.api, claude.values.api);
        assert!(all.api_partial);
        assert_eq!(all.api_missing.as_deref(), Some("no price for gpt-6.1-sol"));
    }

    /// A session without tokens beside one priced in USD adds no second
    /// currency: its known 0 is in the API column's currency, USD without a
    /// stored rate.
    #[test]
    fn a_session_without_tokens_sums_beside_a_usd_value() {
        let settings = settings_eur();
        let priced = Values::of(
            &[Valued {
                api: Some(20.0),
                subscription: Some(1.25),
                account: Some(ac()),
                account_from_readings: false,
                company: money::Company::Anthropic,
                api_currency: Some("USD".to_string()),
                api_missing: None,
                subscription_missing: None,
            }],
            &settings,
            &Fx::default(),
            Utc::now(),
        );
        let silent = Values::of(&[], &settings, &Fx::default(), Utc::now());
        assert_eq!(silent.api_currency.as_deref(), Some("USD"));
        let sum = Values::sum(&[&priced, &silent]);
        assert_eq!(sum.api, Some(20.0));
        assert_eq!(sum.api_currency.as_deref(), Some("USD"));
        assert_eq!(sum.api_missing, None);
    }

    #[test]
    fn a_row_with_a_bad_start_time_is_a_warning() {
        let mut file = session_file();
        file.started_at = i64::MAX;
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[file],
            &ScanState::default(),
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert!(rows.sessions.is_empty());
        assert_eq!(rows.warnings.len(), 1);
        assert_eq!(
            rows.warnings[0],
            format!(
                "session {SESSION}: startedAt {} is not a valid time",
                i64::MAX
            )
        );
    }

    // ---- the edges the review looked at ----

    /// The marker that makes a folder a host folder for the history reader.
    fn host_marker(root: &std::path::Path, host: &str) {
        let dir = root.join(host);
        std::fs::create_dir_all(&dir).expect("the host folder");
        std::fs::write(dir.join("host.json"), "{\"machineId\":\"made-up\"}").expect("the marker");
    }

    #[test]
    /// Two hosts with the same session id are two rows: the key is host and session.
    fn two_hosts_with_the_same_session_id_are_two_rows() {
        let dir = tempdir().expect("tempdir");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        host_marker(dir.path(), "vega");
        atlas
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        vega.append(&session_record(
            SESSION_11,
            utc("2026-11-14T09:20:00Z"),
            utc("2026-11-14T08:30:00Z"),
            utc("2026-11-14T09:20:00Z"),
            true,
            3,
        ))
        .expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&atlas, &settings, &[]);
        let rows = closed(
            &atlas,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 2, "{:?}", rows.sessions);
        let hosts: Vec<&str> = rows.sessions.iter().map(|row| row.host.as_str()).collect();
        assert_eq!(hosts, vec!["atlas", "vega"]);
    }

    #[test]
    /// A session that was closed and continued has a later line with `closed: false`: it is not a row.
    fn a_session_that_was_resumed_is_not_a_closed_row() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:40:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:40:00Z"),
                false,
                17,
            ))
            .expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert!(rows.sessions.is_empty(), "{:?}", rows.sessions);
    }

    #[test]
    /// The last `SessionRecord` of a session wins: its counts and its time.
    fn the_last_line_of_a_session_wins() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T08:30:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T08:30:00Z"),
                false,
                2,
            ))
            .expect("a session record");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(rows.sessions[0].agents_done, 17);
        assert_eq!(rows.sessions[0].since, utc("2026-11-14T09:12:00Z"));
    }

    #[test]
    /// A session that has waited for the user for hours has no stored copy left.
    fn a_session_waiting_for_hours_is_free() {
        let now = utc("2026-11-14T19:00:00Z");
        let waiting = row(
            LiveStatus::Waiting,
            Some("claude-fable-5-2"),
            now - Duration::minutes(61),
        );
        assert_eq!(switch_kind(&waiting, 3, now), SwitchKind::Free);
    }

    #[test]
    /// Exactly `CACHE_LIFE_MIN` of idleness is not yet 'longer than' it.
    fn idle_exactly_sixty_minutes_is_not_free() {
        let now = utc("2026-11-14T19:00:00Z");
        let idle = row(
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now - Duration::minutes(60),
        );
        assert_eq!(switch_kind(&idle, 0, now), SwitchKind::Resend);
    }

    #[test]
    /// A session without a model has no answers: a switch costs it nothing.
    fn a_session_without_a_model_is_free() {
        let now = utc("2026-11-14T19:00:00Z");
        let busy = row(LiveStatus::Busy, None, now);
        assert_eq!(switch_kind(&busy, 3, now), SwitchKind::Free);
    }

    #[test]
    /// An agent with answers but no meta file is listed as done.
    fn an_agent_with_answers_but_no_meta_is_listed() {
        let mut seen = seen("claude-opus-5-5", 1_000_000);
        seen.agents.insert(
            AGENT.to_string(),
            AgentSeen {
                first: utc("2026-11-14T18:00:00Z"),
                last: utc("2026-11-14T18:30:00Z"),
                model: Some("claude-opus-5-5".to_string()),
                tokens: Tokens {
                    input: 1_000,
                    out: 2_000,
                    cw: 0,
                    cr: 0,
                },
                answers: 1,
            },
        );
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &state_with(seen),
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.agents.len(), 1);
        let agent = &row.agents[0];
        assert!(!agent.running);
        assert_eq!(agent.kind, None);
        assert_eq!(agent.task, None);
        assert_eq!(agent.parent, None);
        assert_eq!(row.agents_done, 1);
        assert_eq!(row.agents_running, 0);
    }

    #[test]
    /// An agent started by an agent keeps its parent.
    fn an_agent_started_by_an_agent_keeps_its_parent() {
        let meta = AgentMeta {
            session: SESSION.to_string(),
            agent: AGENT.to_string(),
            agent_type: Some("general-purpose".to_string()),
            description: Some("fix the failing tests".to_string()),
            finished: true,
            spawn_depth: Some(2),
            tool_use_id: Some("toolu_01Nkn4".to_string()),
            parent: Some("a8751b20660a1de6b".to_string()),
            transcript_ended: false,
        };
        let metas_of = |_: &str| vec![meta.clone()];
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert_eq!(
            rows.sessions[0].agents[0].parent.as_deref(),
            Some("a8751b20660a1de6b")
        );
    }

    #[test]
    /// A session the scan has not seen yet is a row with no tokens.
    fn a_session_without_a_scan_state_is_a_row_with_no_tokens() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = running(
            &[session_file()],
            &ScanState::default(),
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.tokens, Tokens::default());
        assert_eq!(row.values.api, Some(0.0), "nothing used is a known 0");
        assert_eq!(row.values.api_currency.as_deref(), Some("EUR"));
        assert_eq!(row.values.api_missing, None);
        assert_eq!(row.values.subscription, Some(0.0));
        assert_eq!(row.model, None);
        assert!(!row.tied);
    }

    #[test]
    /// The name and the label of a joined project are the joined ones.
    fn the_label_and_project_name_after_a_join() {
        let old = ProjectId::of(&ProjectKey::Folder("atlas-old".to_string()), "atlas");
        let atlas = ProjectId::of(&project_key(), "atlas");
        let log = vec![Hosted {
            host: "atlas".to_string(),
            record: crate::projects::ProjectsLogRecord {
                t: utc("2026-11-14T12:00:00Z"),
                edit: crate::projects::ProjectEdit::Join {
                    key: old.clone(),
                    into: atlas.clone(),
                },
            },
        }];
        let sessions = vec![Hosted {
            host: "atlas".to_string(),
            record: session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ),
        }];
        let projects = Projects::resolve(&log, &sessions, &[]);
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let mut file = session_file();
        file.name = None;
        let rows = running(
            &[file],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &|_: &str| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| ProjectKey::Folder("atlas-old".to_string()),
        );

        let row = &rows.sessions[0];
        assert_eq!(row.project, "atlas", "the name after the join");
        assert_eq!(row.label, "atlas-7c", "the label after the join");
    }

    #[test]
    /// A folder project is one project per host.
    fn a_folder_project_is_one_project_per_host() {
        let dir = tempdir().expect("tempdir");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        host_marker(dir.path(), "vega");
        let mut record = session_record(
            SESSION_11,
            utc("2026-11-14T09:12:00Z"),
            utc("2026-11-14T08:00:00Z"),
            utc("2026-11-14T09:12:00Z"),
            true,
            17,
        );
        record.project = ProjectKey::Folder("atlas".to_string());
        atlas.append(&record).expect("a session record");
        vega.append(&record).expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&atlas, &settings, &[]);
        let rows = closed(
            &atlas,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        assert_eq!(rows.sessions.len(), 2, "{:?}", rows.sessions);
        let labels: Vec<&str> = rows.sessions.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, vec!["atlas-11", "atlas-11"]);
    }

    #[test]
    /// A closed session without token records has no tokens and says so.
    fn a_closed_session_without_token_records() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T10:00:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        let row = &rows.sessions[0];
        assert_eq!(row.tokens, Tokens::default());
        assert_eq!(row.values.api, Some(0.0), "nothing used is a known 0");
        assert_eq!(row.values.api_currency.as_deref(), Some("EUR"));
        assert_eq!(row.values.api_missing, None);
        assert_eq!(row.values.subscription, Some(0.0));
        assert_eq!(row.model, None);
        assert!(!row.tied);
    }

    #[test]
    /// A closed session that closed less than an hour ago shows its tokens of the last hour.
    fn a_closed_session_shows_its_tokens_of_the_last_hour() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&session_record(
                SESSION_11,
                utc("2026-11-14T09:12:00Z"),
                utc("2026-11-14T08:00:00Z"),
                utc("2026-11-14T09:12:00Z"),
                true,
                17,
            ))
            .expect("a session record");
        history
            .append(&token(
                SESSION_11,
                utc("2026-11-14T09:10:00Z"),
                "claude-opus-5-5",
                0,
                1_000_000,
            ))
            .expect("a token record");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let rows = closed(
            &history,
            utc("2026-11-14T09:00:00Z"),
            utc("2026-11-14T09:20:00Z"),
            Some(&projects()),
            &known_pricing(&valuer, &settings),
        )
        .expect("rows");

        let row = &rows.sessions[0];
        assert_eq!(
            row.last_hour.expect("a known hour").out,
            1_000_000,
            "the row was closed ten minutes ago: its tokens are in the last hour"
        );
    }

    #[test]
    /// A session with 400 agents is built in well under a second.
    fn the_cost_of_a_row_with_four_hundred_agents() {
        let metas: Vec<AgentMeta> = (0..400)
            .map(|i| AgentMeta {
                session: SESSION.to_string(),
                agent: format!("a8751b20660a1de{i:04x}"),
                agent_type: Some("general-purpose".to_string()),
                description: Some("fix the failing tests".to_string()),
                finished: i >= 3,
                spawn_depth: Some(1),
                tool_use_id: Some(format!("toolu_{i:04}")),
                parent: None,
                transcript_ended: false,
            })
            .collect();
        let metas_of = |_: &str| metas.clone();
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let state = state_with(seen("claude-opus-5-5", 1_000_000));
        let start = std::time::Instant::now();
        let rows = running(
            &[session_file()],
            &state,
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );
        let elapsed = start.elapsed();
        let text = serde_json::to_string(&rows.sessions[0]).expect("the row as json");
        println!(
            "400 agents: {:?} for the row, {} bytes of json",
            elapsed,
            text.len()
        );
        assert_eq!(rows.sessions[0].agents.len(), 400);
    }

    #[test]
    /// A list with one unpriced part and one priced part keeps the priced
    /// part as the known sum, partial with the unpriced part's reason (R3).
    fn values_of_a_list_with_a_missing_part() {
        let missing = Valued {
            api: None,
            subscription: None,
            account: None,
            account_from_readings: false,
            company: money::Company::Anthropic,
            api_currency: None,
            api_missing: Some(crate::money::Missing::NoPrice {
                model: "claude-x".to_string(),
            }),
            subscription_missing: Some(crate::money::Missing::NoPrice {
                model: "claude-x".to_string(),
            }),
        };
        let priced = Valued {
            api: Some(2.0),
            subscription: Some(0.5),
            account: None,
            account_from_readings: false,
            company: money::Company::Anthropic,
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            subscription_missing: None,
        };
        let values = Values::of(
            &[missing, priced],
            &settings_eur(),
            &Fx::default(),
            Utc::now(),
        );

        assert_eq!(values.api, Some(2.0));
        assert!(values.api_partial);
        assert_eq!(values.api_missing.as_deref(), Some("no price for claude-x"));
        assert_eq!(values.subscription, Some(0.5));
        assert!(values.subscription_partial);
        assert_eq!(
            values.subscription_missing.as_deref(),
            Some("no price for claude-x")
        );
    }

    #[test]
    /// The sum never adds two API currencies and keeps the first missing part's
    /// reason. A mixed half keeps its known part and is partial (R3); an empty
    /// sum is a known 0.
    fn a_sum_never_adds_two_currencies_and_keeps_a_known_part() {
        let eur = Values {
            api: Some(1.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(0.5),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let usd = Values {
            api: Some(2.0),
            api_currency: Some("USD".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(0.75),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let sum = Values::sum(&[&eur, &usd]);
        assert_eq!(sum.api, None);
        assert_eq!(sum.api_currency, None);
        assert_eq!(sum.api_missing.as_deref(), Some(TWO_CURRENCIES));
        assert!(!sum.api_partial, "a half that is None is never partial");
        assert_eq!(sum.subscription, Some(1.25));
        assert_eq!(sum.subscription_missing, None);
        assert!(!sum.subscription_partial);
        assert_eq!(sum.currency.as_deref(), Some("EUR"));

        let unpriced = Values {
            api: None,
            api_currency: None,
            api_missing: Some("no price for claude-x".to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no price for claude-x".to_string()),
            subscription_partial: false,
        };
        let sum = Values::sum(&[&eur, &unpriced]);
        assert_eq!(sum.api, Some(1.0), "the known API part is kept");
        assert!(sum.api_partial);
        assert_eq!(sum.api_missing.as_deref(), Some("no price for claude-x"));
        assert_eq!(sum.subscription, Some(0.5), "the known part is kept");
        assert!(sum.subscription_partial);
        assert_eq!(
            sum.subscription_missing.as_deref(),
            Some("no price for claude-x")
        );
        assert_eq!(sum.currency.as_deref(), Some("EUR"));

        let empty = Values::sum(&[]);
        assert_eq!(empty.api, Some(0.0), "nothing used is a known 0");
        assert_eq!(empty.api_currency, None);
        assert_eq!(empty.api_missing, None);
        assert!(!empty.api_partial);
        assert_eq!(empty.subscription, Some(0.0));
        assert_eq!(empty.subscription_missing, None);
        assert!(!empty.subscription_partial);
        assert_eq!(empty.currency, None);
    }

    #[test]
    /// A half with one known part and one missing part is the known part,
    /// marked partial with the first missing part's reason (R3).
    fn a_partly_valued_sum_keeps_its_known_part() {
        let priced = |subscription: f64| Values {
            api: Some(0.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(subscription),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let missing = Values {
            api: Some(0.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no price entered".to_string()),
            subscription_partial: false,
        };

        let sum = Values::sum(&[&priced(0.60), &missing, &priced(0.37)]);

        assert_eq!(sum.subscription, Some(0.97));
        assert!(sum.subscription_partial);
        assert_eq!(
            sum.subscription_missing.as_deref(),
            Some("no price entered")
        );
        assert_eq!(sum.currency.as_deref(), Some("EUR"));
        assert_eq!(
            sum.api,
            Some(0.0),
            "every API part is known, so that half is not partial"
        );
        assert!(!sum.api_partial);
    }

    #[test]
    /// No known part in a half keeps it unknown with the first reason, and a
    /// half that is None is never partial.
    fn a_sum_of_only_unknown_parts_is_unknown() {
        let no_price = Values {
            api: None,
            api_currency: None,
            api_missing: Some("no price entered".to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no price entered".to_string()),
            subscription_partial: false,
        };
        let no_account = Values {
            api: None,
            api_currency: None,
            api_missing: Some("no account was in use at that time".to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no account was in use at that time".to_string()),
            subscription_partial: false,
        };

        let sum = Values::sum(&[&no_price, &no_account]);

        assert_eq!(sum.api, None);
        assert_eq!(sum.api_missing.as_deref(), Some("no price entered"));
        assert!(!sum.api_partial);
        assert_eq!(sum.subscription, None);
        assert_eq!(
            sum.subscription_missing.as_deref(),
            Some("no price entered")
        );
        assert!(!sum.subscription_partial);
    }

    #[test]
    /// Nothing used is a known 0 in both halves, never an unknown value.
    fn an_empty_sum_is_a_known_zero() {
        let sum = Values::sum(&[]);

        assert_eq!(sum.api, Some(0.0));
        assert_eq!(sum.subscription, Some(0.0));
        assert_eq!(sum.api_currency, None);
        assert_eq!(sum.currency, None);
        assert_eq!(sum.api_missing, None);
        assert_eq!(sum.subscription_missing, None);
        assert!(!sum.api_partial);
        assert!(!sum.subscription_partial);
    }

    #[test]
    /// A partial half stays partial when it is added to a full value: the known
    /// parts add up and the first missing reason stays (R3).
    fn a_partial_sum_stays_partial_when_added_again() {
        let known = Values {
            api: Some(1.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(0.97),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let missing = Values {
            api: None,
            api_currency: None,
            api_missing: Some("no price entered".to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no price entered".to_string()),
            subscription_partial: false,
        };
        let partial = Values::sum(&[&known, &missing]);
        assert!(partial.subscription_partial);

        let full = Values {
            api: Some(2.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(1.0),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let sum = Values::sum(&[&partial, &full]);

        assert_eq!(sum.subscription, Some(1.97));
        assert!(sum.subscription_partial);
        assert_eq!(
            sum.subscription_missing.as_deref(),
            Some("no price entered")
        );
        assert_eq!(sum.api, Some(3.0));
        assert!(sum.api_partial);
        assert_eq!(sum.api_missing.as_deref(), Some("no price entered"));
    }

    #[test]
    /// Two hosts can set different currencies; their subscriptions are then not
    /// added, and neither is their API value.
    fn a_sum_of_two_hosts_in_two_currencies_is_not_added() {
        let atlas = Values {
            api: Some(1.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(2.0),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let vega = Values {
            api: Some(3.0),
            api_currency: Some("USD".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(4.0),
            currency: Some("USD".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let sum = Values::sum(&[&atlas, &vega]);
        assert_eq!(sum.subscription, None);
        assert_eq!(sum.currency, None);
        assert_eq!(
            sum.subscription_missing.as_deref(),
            Some(TWO_SUBSCRIPTION_CURRENCIES)
        );
        assert!(!sum.subscription_partial);
        assert_eq!(sum.api, None);
        assert_eq!(sum.api_currency, None);
        assert_eq!(sum.api_missing.as_deref(), Some(TWO_CURRENCIES));
        assert!(!sum.api_partial);
    }

    #[test]
    /// `Values::of` is exactly the sum of the one-record values it builds.
    fn values_of_is_the_sum_of_its_records() {
        let records = [
            Valued {
                api: Some(2.0),
                subscription: Some(0.5),
                account: None,
                account_from_readings: false,
                company: money::Company::Anthropic,
                api_currency: Some("EUR".to_string()),
                api_missing: None,
                subscription_missing: None,
            },
            Valued {
                api: Some(3.0),
                subscription: Some(0.25),
                account: None,
                account_from_readings: false,
                company: money::Company::Anthropic,
                api_currency: Some("EUR".to_string()),
                api_missing: None,
                subscription_missing: None,
            },
            Valued {
                api: None,
                subscription: None,
                account: None,
                account_from_readings: false,
                company: money::Company::Anthropic,
                api_currency: None,
                api_missing: Some(crate::money::Missing::NoPrice {
                    model: "claude-x".to_string(),
                }),
                subscription_missing: Some(crate::money::Missing::NoPrice {
                    model: "claude-x".to_string(),
                }),
            },
        ];
        let settings = settings_eur();
        let of = Values::of(&records, &settings, &Fx::default(), Utc::now());
        let parts: Vec<Values> = records
            .iter()
            .map(|record| {
                Values::of(
                    std::slice::from_ref(record),
                    &settings,
                    &Fx::default(),
                    Utc::now(),
                )
            })
            .collect();
        let parts: Vec<&Values> = parts.iter().collect();
        assert_eq!(of, Values::sum(&parts));
        assert_eq!(of.api, Some(5.0), "the two priced records are kept");
        assert!(of.api_partial);
        assert_eq!(of.api_missing.as_deref(), Some("no price for claude-x"));
        assert_eq!(of.subscription, Some(0.75));
        assert!(of.subscription_partial);
        assert_eq!(
            of.subscription_missing.as_deref(),
            Some("no price for claude-x")
        );
    }

    #[test]
    /// The agent metas of a session are read once per session file.
    fn metas_is_called_once_per_session() {
        let calls = std::cell::RefCell::new(0);
        let metas_of = |_: &str| {
            *calls.borrow_mut() += 1;
            Vec::new()
        };
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let valuer = valuer(&history, &settings, &[]);
        let mut second = session_file();
        second.session_id = SESSION_11.to_string();
        second.pid = 41823;
        let rows = running(
            &[session_file(), second],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &metas_of,
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &known_pricing(&valuer, &settings),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );

        assert_eq!(rows.sessions.len(), 2);
        assert_eq!(*calls.borrow(), 2, "one call per session file");
    }

    // ---- the report other hosts read (spec §3.8) ----

    fn report_session(host: &str, status: LiveStatus, at: DateTime<Utc>) -> LiveSession {
        let mut session = row(status, Some("claude-opus-5-5"), at);
        session.host = host.to_string();
        session
    }

    #[test]
    fn the_report_has_no_task_text() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let at = utc("2026-11-14T19:00:00Z");
        let mut session = report_session("atlas", LiveStatus::Busy, at);
        session.agents.push(LiveAgent {
            agent: "a8751b20660a1de6b".to_string(),
            parent: None,
            provider: CLAUDE_PROVIDER.to_string(),
            kind: Some("general-purpose".to_string()),
            task: Some("fix the failing tests".to_string()),
            model: Some("claude-opus-5-5".to_string()),
            running: false,
            first: Some(at),
            last: Some(at),
            tokens: Tokens::default(),
            values: Values::sum(&[]),
        });

        let report = write_report(&history, &[session], at).expect("the report");
        assert!(
            report.sessions[0].agents[0].task.is_none(),
            "the returned report has no task either"
        );
        let text =
            std::fs::read_to_string(history.sessions_now_file("atlas")).expect("the report file");
        assert!(
            !text.contains("fix the failing tests"),
            "an agent's task is never written: {text}"
        );
        assert!(text.contains("general-purpose"));
        assert!(text.contains("claude-opus-5-5"));
    }

    #[test]
    fn other_hosts_reads_every_other_report() {
        let dir = tempdir().expect("tempdir");
        let now = utc("2026-11-14T19:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        host_marker(dir.path(), "atlas");
        host_marker(dir.path(), "vega");
        let at = now - Duration::minutes(2);
        write_report(&atlas, &[report_session("atlas", LiveStatus::Busy, at)], at)
            .expect("this host's report");
        write_report(
            &vega,
            &[report_session("vega", LiveStatus::Waiting, at)],
            at,
        )
        .expect("vega's report");

        let other = other_hosts(&atlas, now).expect("the other hosts");
        assert!(other.warnings.is_empty(), "{:?}", other.warnings);
        assert_eq!(other.hosts.len(), 1);
        assert_eq!(other.hosts[0].host, "vega");
        assert_eq!(other.hosts[0].at, at);
        assert!(!other.hosts[0].stale, "two minutes old is fresh");
        assert_eq!(other.hosts[0].sessions[0].status, LiveStatus::Waiting);
    }

    #[test]
    fn an_old_report_is_stale() {
        let dir = tempdir().expect("tempdir");
        let now = utc("2026-11-14T19:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        host_marker(dir.path(), "vega");
        let at = now - Duration::minutes(REPORT_STALE_MIN + 1);
        write_report(
            &vega,
            &[report_session("vega", LiveStatus::Waiting, at)],
            at,
        )
        .expect("vega's report");

        let other = other_hosts(&atlas, now).expect("the other hosts");
        assert_eq!(other.hosts.len(), 1);
        assert!(other.hosts[0].stale);
        assert_eq!(
            other.hosts[0].sessions[0].status,
            LiveStatus::Other("no report".to_string())
        );
    }

    #[test]
    fn a_broken_report_is_a_warning_and_hides_nothing_else() {
        let dir = tempdir().expect("tempdir");
        let now = utc("2026-11-14T19:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        let ledger = HistoryDir::at(dir.path(), "ledger");
        host_marker(dir.path(), "vega");
        host_marker(dir.path(), "ledger");
        host_marker(dir.path(), "pr");
        write_report(&vega, &[report_session("vega", LiveStatus::Busy, now)], now)
            .expect("vega's report");
        std::fs::write(ledger.sessions_now_file("ledger"), "{not json").expect("a broken report");

        let other = other_hosts(&atlas, now).expect("the folder can be listed");
        assert_eq!(other.hosts.len(), 1, "vega is still read");
        assert_eq!(other.hosts[0].host, "vega");
        assert_eq!(other.warnings.len(), 1, "{:?}", other.warnings);
        assert!(
            other.warnings[0].contains(&ledger.sessions_now_file("ledger").display().to_string()),
            "the warning names the file: {}",
            other.warnings[0]
        );
        assert!(
            other.warnings[0].contains("key must be a string"),
            "the warning names the reason: {}",
            other.warnings[0]
        );
    }

    const JOB_A: &str = "flash-task-5c974a53-178a-4254-93b2-b804a111414a";
    const JOB_B: &str = "flash-task-8d2f0e7c-6b1a-4c3d-9e0f-1a2b3c4d5e6f";
    const OWN_SESSION: &str = "3f9a1c2e-7b4d-4e8a-9c1f-5d2e6a7b8c90";
    const WORKER_A: &str = "a25abffd-ed93-4296-868f-dbd77aca32ac";
    const HARNESS_KEY: &str = "--home-sam-projects-atlas--";
    const FLASH_MODEL: &str = "deepseek-v4.1-flash:cloud";

    fn harness_store() -> (tempfile::TempDir, crate::harness::HarnessPaths) {
        let tmp = tempdir().expect("tempdir");
        let paths = crate::harness::HarnessPaths::at(
            tmp.path().join("sessions"),
            tmp.path().join("flash-agents"),
        );
        (tmp, paths)
    }

    fn harness_event(kind: &str, time: i64, data: serde_json::Value) -> String {
        serde_json::json!({"type": kind, "seq": 1, "time": time, "data": data}).to_string()
    }

    /// Write one Harness session file; the times are 2026-11-14 09:40 UTC and
    /// later.
    #[allow(clippy::too_many_arguments)]
    fn write_harness_session(
        paths: &crate::harness::HarnessPaths,
        id: &str,
        parent: Option<&str>,
        depth: u32,
        finished: bool,
        input: u64,
        out: u64,
        title: Option<&str>,
    ) {
        let dir = paths.sessions.join(HARNESS_KEY).join(id);
        std::fs::create_dir_all(&dir).expect("a session folder");
        let mut header = serde_json::json!({
            "type": "session",
            "version": 4,
            "id": id,
            "createdAt": 1794696000000_i64,
            "cwd": ATLAS_CWD,
            "isSeeded": false,
            "delegationDepth": depth,
        });
        if let Some(parent) = parent {
            header["parentSession"] = serde_json::json!(parent);
            header["origin"] = serde_json::json!("subagent");
        }
        let mut lines = vec![
            header.to_string(),
            harness_event("turn/start", 1794696001000, serde_json::json!({"turn": 1})),
            harness_event(
                "assistant/message",
                1794696002000,
                serde_json::json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "role": "assistant",
                        "source": {"kind": "model", "provider": "ollama", "model": FLASH_MODEL},
                        "id": "m1"
                    },
                    "usage": {
                        "inputTokens": input,
                        "outputTokens": out,
                        "totalTokens": input + out
                    }
                }),
            ),
        ];
        if let Some(title) = title {
            lines.push(harness_event(
                "session/title",
                1794696003000,
                serde_json::json!({"title": title}),
            ));
        }
        if finished {
            lines.push(harness_event(
                "turn/end",
                1794696004000,
                serde_json::json!({"turn": 1, "reason": {"kind": "completed"}}),
            ));
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(
            dir.join("session.v4.jsonl.zstd"),
            zstd::encode_all(text.as_bytes(), 3).expect("zstd"),
        )
        .expect("a session file");
        std::fs::write(dir.join("session.lock"), []).expect("a lock");
    }

    /// Write a Harness session with `messages` assistant events and a final
    /// `turn/end` when `finished`; every message is 100 input and 50 output.
    fn write_harness_messages(
        paths: &crate::harness::HarnessPaths,
        id: &str,
        parent: Option<&str>,
        depth: u32,
        messages: u32,
        finished: bool,
        last: i64,
    ) {
        let dir = paths.sessions.join(HARNESS_KEY).join(id);
        std::fs::create_dir_all(&dir).expect("a session folder");
        let mut header = serde_json::json!({
            "type": "session",
            "version": 4,
            "id": id,
            "createdAt": last - 3_600_000,
            "cwd": ATLAS_CWD,
            "isSeeded": false,
            "delegationDepth": depth,
        });
        if let Some(parent) = parent {
            header["parentSession"] = serde_json::json!(parent);
            header["origin"] = serde_json::json!("subagent");
        }
        let mut lines = vec![
            header.to_string(),
            harness_event("turn/start", last - 60_000, serde_json::json!({"turn": 1})),
        ];
        for number in 0..messages {
            lines.push(harness_event(
                "assistant/message",
                last - 3_600_000 + i64::from(number) * 1_000,
                serde_json::json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "role": "assistant",
                        "source": {"kind": "model", "provider": "ollama", "model": FLASH_MODEL},
                        "id": format!("m{number}")
                    },
                    "usage": {
                        "inputTokens": 100,
                        "outputTokens": 50,
                        "totalTokens": 150
                    }
                }),
            ));
        }
        if finished {
            lines.push(harness_event(
                "turn/end",
                last,
                serde_json::json!({"turn": 1, "reason": {"kind": "completed"}}),
            ));
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(
            dir.join("session.v4.jsonl.zstd"),
            zstd::encode_all(text.as_bytes(), 3).expect("zstd"),
        )
        .expect("a session file");
        std::fs::write(dir.join("session.lock"), []).expect("a lock");
    }

    fn unknown_pricing() -> Pricing<'static> {
        Pricing::Unknown {
            reason: "no prices in the test".to_string(),
        }
    }

    fn harness_links(
        paths: &crate::harness::HarnessPaths,
        state: &ScanState,
    ) -> BTreeMap<String, String> {
        let mut reports = crate::harness::JobReports::default();
        let warnings = reports.refresh(paths);
        assert!(warnings.is_empty(), "{warnings:?}");
        crate::harness::links(&reports, &state.sessions, &state.closed_flash_jobs).owners
    }

    #[test]
    fn a_job_named_in_a_transcript_hangs_under_its_session() {
        let (_dir, paths) = harness_store();
        write_harness_session(&paths, JOB_A, None, 0, false, 10516, 3585, None);
        write_harness_session(&paths, WORKER_A, Some(JOB_A), 1, false, 2200, 1100, None);
        let report = serde_json::json!({"sessionId": JOB_A});
        std::fs::create_dir_all(paths.reports.join("acme").join("patches")).expect("reports");
        std::fs::write(
            paths
                .reports
                .join("acme")
                .join("patches")
                .join("job-3e32c2fc.report.json"),
            report.to_string(),
        )
        .expect("a report");
        let mut state = state_with(seen(FLASH_MODEL, 3585));
        state
            .sessions
            .get_mut(SESSION)
            .expect("the session")
            .flash_jobs = BTreeSet::from(["job-3e32c2fc".to_string()]);
        let now = utc("2026-11-14T19:00:00Z");
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let claude_cwd = BTreeMap::from([(SESSION.to_string(), ATLAS_CWD.to_string())]);

        let mut rows = running(
            &[session_file()],
            &state,
            &|_| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &unknown_pricing(),
            "atlas",
            now,
            &|_| project_key(),
        );
        let out = harness_rows(
            &harness.sessions,
            &harness_links(&paths, &state),
            &claude_cwd,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );
        let agents = out.agents.get(SESSION).expect("the session's flash agents");
        assert_eq!(agents.len(), 2);
        let job = agents
            .iter()
            .find(|agent| agent.agent == JOB_A)
            .expect("the job");
        assert_eq!(job.kind.as_deref(), Some("flash"));
        assert_eq!(job.parent, None);
        assert!(job.running);
        assert_eq!(job.model.as_deref(), Some(FLASH_MODEL));
        let worker = agents
            .iter()
            .find(|agent| agent.agent == WORKER_A)
            .expect("the worker");
        assert_eq!(worker.parent.as_deref(), Some(JOB_A));
        add_agents(
            rows.sessions
                .iter_mut()
                .find(|row| row.session == SESSION)
                .expect("the Claude row"),
            agents.clone(),
        );
        let row = rows
            .sessions
            .iter()
            .find(|row| row.session == SESSION)
            .expect("the Claude row");
        assert_eq!(row.agents_running, 2);
        assert_eq!(row.agents_done, 0);
    }

    #[test]
    fn two_sessions_in_one_repository_keep_their_own_jobs() {
        let (_dir, paths) = harness_store();
        write_harness_session(&paths, JOB_A, None, 0, true, 100, 50, None);
        write_harness_session(&paths, JOB_B, None, 0, true, 200, 70, None);
        let mut state = state_with(seen(FLASH_MODEL, 3585));
        state
            .sessions
            .get_mut(SESSION)
            .expect("the session")
            .flash_jobs = BTreeSet::from([JOB_A.to_string()]);
        let mut other = seen(FLASH_MODEL, 3585);
        other.flash_jobs = BTreeSet::from([JOB_B.to_string()]);
        state.sessions.insert(SESSION_11.to_string(), other);
        let mut other_file = session_file();
        other_file.pid = 41823;
        other_file.session_id = SESSION_11.to_string();
        let now = utc("2026-11-14T19:00:00Z");
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let claude_cwd = BTreeMap::from([
            (SESSION.to_string(), ATLAS_CWD.to_string()),
            (SESSION_11.to_string(), ATLAS_CWD.to_string()),
        ]);
        let mut rows = running(
            &[session_file(), other_file],
            &state,
            &|_| Vec::new(),
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &unknown_pricing(),
            "atlas",
            now,
            &|_| project_key(),
        );

        let out = harness_rows(
            &harness.sessions,
            &harness_links(&paths, &state),
            &claude_cwd,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );
        for (session, agents) in &out.agents {
            add_agents(
                rows.sessions
                    .iter_mut()
                    .find(|row| &row.session == session)
                    .expect("the session's row"),
                agents.clone(),
            );
        }

        assert_eq!(rows.sessions.len(), 2);
        let first = rows
            .sessions
            .iter()
            .find(|row| row.session == SESSION)
            .expect("the first row");
        let second = rows
            .sessions
            .iter()
            .find(|row| row.session == SESSION_11)
            .expect("the second row");
        assert_eq!(
            first
                .agents
                .iter()
                .map(|agent| agent.agent.as_str())
                .collect::<Vec<_>>(),
            vec![JOB_A]
        );
        assert_eq!(
            second
                .agents
                .iter()
                .map(|agent| agent.agent.as_str())
                .collect::<Vec<_>>(),
            vec![JOB_B]
        );
    }

    #[test]
    fn a_session_without_a_parent_is_its_own_row() {
        let (_dir, paths) = harness_store();
        write_harness_session(
            &paths,
            JOB_A,
            None,
            0,
            false,
            10516,
            3585,
            Some("atlas link test"),
        );
        write_harness_session(&paths, WORKER_A, Some(JOB_A), 1, true, 2200, 1100, None);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert!(out.agents.is_empty());
        assert_eq!(out.rows.sessions.len(), 1);
        let row = &out.rows.sessions[0];
        assert_eq!(row.session, JOB_A);
        assert_eq!(row.provider, "DeepSeek Harness");
        assert_eq!(row.model.as_deref(), Some(FLASH_MODEL));
        assert_eq!(row.tokens.input, 12_716, "the root and its worker");
        assert_eq!(row.tokens.out, 4_685, "the root and its worker");
        assert_eq!(row.agents.len(), 1);
        assert_eq!(row.agents[0].agent, WORKER_A);
        assert_eq!(row.agents[0].parent.as_deref(), Some(JOB_A));
        assert_eq!(row.agents_running, 0);
        assert_eq!(row.agents_done, 1);
    }

    #[test]
    fn a_codex_session_is_a_row_while_held_or_recent() {
        let now = utc("2026-11-14T19:00:00Z");
        let old = crate::codex_sessions::CodexSession {
            id: "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30".to_string(),
            cwd: PathBuf::from(ATLAS_CWD),
            model: Some("gpt-6.1-sol".to_string()),
            started: utc("2026-11-14T16:00:00Z"),
            last: utc("2026-11-14T16:30:00Z"),
            tokens: Tokens {
                input: 12000,
                out: 900,
                cw: 300,
                cr: 8000,
            },
            series: Vec::new(),
            last_hour: Some(Tokens {
                input: 1200,
                out: 90,
                cw: 0,
                cr: 800,
            }),
            busy: true,
            title: None,
            file: PathBuf::from("/home/sam/.codex/sessions/2026/11/14/rollout-old.jsonl"),
            recorded_file: PathBuf::from("2026/11/14/rollout-old.jsonl"),
        };

        let quiet = codex_rows(
            std::slice::from_ref(&old),
            &BTreeSet::new(),
            None,
            &unknown_pricing(),
            "atlas",
            now,
            &|_| project_key(),
        );
        assert!(
            quiet.sessions.is_empty(),
            "a quiet file that no process holds is not a row"
        );

        let open = BTreeSet::from([old.file.clone()]);
        let rows = codex_rows(
            std::slice::from_ref(&old),
            &open,
            None,
            &unknown_pricing(),
            "atlas",
            now,
            &|_| project_key(),
        );
        assert_eq!(rows.sessions.len(), 1);
        let row = &rows.sessions[0];
        assert_eq!(row.session, old.id);
        assert_eq!(row.provider, "Codex");
        assert_eq!(row.status, LiveStatus::Busy);
        assert_eq!(row.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(row.tokens, old.tokens);
        assert_eq!(row.started, old.started);
        assert_eq!(row.since, old.last);
        assert!(row.agents.is_empty());
        assert_eq!(row.agents_running, 0);

        let mut recent = old.clone();
        recent.last = now - Duration::minutes(5);
        recent.busy = false;
        let rows = codex_rows(
            &[recent],
            &BTreeSet::new(),
            None,
            &unknown_pricing(),
            "atlas",
            now,
            &|_| project_key(),
        );
        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(rows.sessions[0].status, LiveStatus::Idle);
    }

    #[test]
    fn a_codex_row_values_its_last_hour_and_says_when_it_is_not_known() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                crate::money::price_key("openai", "gpt-6.1-sol"),
                ModelPrice {
                    input: 1.0,
                    output: 2.0,
                    cache_write: 1.0,
                    cache_read: 0.1,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let valuer = valuer_with(&history, &settings, &[], &prices);
        let now = utc("2026-11-14T19:00:00Z");
        let session = crate::codex_sessions::CodexSession {
            id: "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30".to_string(),
            cwd: PathBuf::from(ATLAS_CWD),
            model: Some("gpt-6.1-sol".to_string()),
            started: now - Duration::minutes(30),
            last: now - Duration::minutes(5),
            tokens: Tokens {
                input: 1000,
                out: 100,
                cw: 0,
                cr: 500,
            },
            series: Vec::new(),
            last_hour: Some(Tokens {
                input: 400,
                out: 40,
                cw: 0,
                cr: 200,
            }),
            busy: false,
            title: None,
            file: PathBuf::from("/home/sam/.codex/sessions/2026/11/14/rollout-now.jsonl"),
            recorded_file: PathBuf::from("2026/11/14/rollout-now.jsonl"),
        };

        let rows = codex_rows(
            std::slice::from_ref(&session),
            &BTreeSet::new(),
            None,
            &known_pricing(&valuer, &settings),
            "atlas",
            now,
            &|_| project_key(),
        );
        assert_eq!(rows.sessions.len(), 1);
        let row = &rows.sessions[0];
        assert_eq!(row.last_hour, session.last_hour);
        assert!(
            row.last_hour_values.api.is_some(),
            "the last hour is valued like the tokens: {:?}",
            row.last_hour_values
        );

        let mut unknown = session.clone();
        unknown.last_hour = None;
        let rows = codex_rows(
            &[unknown],
            &BTreeSet::new(),
            None,
            &known_pricing(&valuer, &settings),
            "atlas",
            now,
            &|_| project_key(),
        );
        let row = &rows.sessions[0];
        assert_eq!(row.last_hour, None);
        assert_eq!(
            row.last_hour_values.api_missing.as_deref(),
            Some(CODEX_LAST_HOUR_UNKNOWN),
            "an unknown last hour says so, never 0: {:?}",
            row.last_hour_values
        );
        assert_eq!(
            row.last_hour_values.subscription_missing.as_deref(),
            Some(CODEX_LAST_HOUR_UNKNOWN)
        );
    }

    #[test]
    fn no_title_reaches_the_history() {
        let (dir, paths) = harness_store();
        let title = "atlas link test";
        // A DeepSeek Harness session of its own, not a flash job: its worker
        // shows its title as its task.
        write_harness_session(&paths, OWN_SESSION, None, 0, false, 10516, 3585, None);
        write_harness_session(
            &paths,
            WORKER_A,
            Some(OWN_SESSION),
            1,
            false,
            2200,
            1100,
            Some(title),
        );
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::from([OWN_SESSION.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert_eq!(
            out.rows.sessions[0].agents[0].task.as_deref(),
            Some(title),
            "the title is shown live"
        );
        let history = HistoryDir::at(&dir.path().join("history"), "atlas");
        write_report(&history, &out.rows.sessions, utc("2026-11-14T19:00:00Z"))
            .expect("the sessions report");
        let recorder =
            crate::history::Recorder::new(HistoryDir::at(&dir.path().join("history"), "atlas"));
        recorder.tokens(&out.tokens).expect("the token lines");

        let mut found = String::new();
        let mut files = 0;
        collect_text(&dir.path().join("history"), &mut found, &mut files);
        assert!(files > 0, "a token line was written");
        assert!(!found.contains(title), "no history file holds the title");
    }

    #[test]
    fn a_harness_row_is_named_by_its_session_title() {
        let (dir, paths) = harness_store();
        // A DeepSeek Harness session of its own (a plain UUID), one with an
        // all-space title, and an unlinked flash job, whose title is a
        // worker's task text.
        let own = "3f9a1c2e-7b4d-4e8a-9c1f-5d2e6a7b8c90";
        let blank = "6c2d8e1f-4a3b-4c5d-8e9f-0a1b2c3d4e5f";
        write_harness_session(
            &paths,
            own,
            None,
            0,
            false,
            10516,
            3585,
            Some("atlas link test"),
        );
        write_harness_session(&paths, blank, None, 0, true, 2200, 1100, Some("   "));
        write_harness_session(
            &paths,
            JOB_A,
            None,
            0,
            false,
            900,
            400,
            Some("Fix the invoice export"),
        );
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::from([own.to_string(), blank.to_string(), JOB_A.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        let row = |id: &str| {
            out.rows
                .sessions
                .iter()
                .find(|row| row.session == id)
                .expect("the Harness row")
        };
        assert_eq!(row(own).title.as_deref(), Some("atlas link test"));
        assert_eq!(row(blank).title, None, "an all-space title is no title");
        assert_eq!(
            row(blank).status,
            LiveStatus::Idle,
            "a session of its own between two turns, its lock held, is open and idle"
        );
        assert_eq!(row(JOB_A).title, None, "a flash job's title names no row");

        let history = HistoryDir::at(&dir.path().join("history"), "atlas");
        write_report(&history, &out.rows.sessions, utc("2026-11-14T19:00:00Z"))
            .expect("the sessions report");
        let mut found = String::new();
        let mut files = 0;
        collect_text(&dir.path().join("history"), &mut found, &mut files);
        assert!(files > 0, "the report was written");
        assert!(
            found.contains("atlas link test"),
            "a Harness row's title is in the report"
        );
        assert!(
            !found.contains("Fix the invoice export"),
            "a flash job's task text never reaches the report"
        );
    }

    /// One finished agent of `provider` with no tokens, for the report tests.
    fn report_agent(agent: &str, provider: &str, at: DateTime<Utc>) -> LiveAgent {
        LiveAgent {
            agent: agent.to_string(),
            parent: None,
            provider: provider.to_string(),
            kind: None,
            task: None,
            model: Some(FLASH_MODEL.to_string()),
            running: false,
            first: Some(at),
            last: Some(at),
            tokens: Tokens::default(),
            values: Values::sum(&[]),
        }
    }

    /// A flash job is named by its report's label, and nothing else: while it
    /// runs it has no report and no task text, its own title (flash-agents'
    /// dispatcher prompt) is never shown, and a worker under it shows no task
    /// (its title names its folder). The label never reaches a file.
    #[test]
    fn a_flash_job_is_named_by_its_report_label() {
        let (dir, paths) = harness_store();
        let dispatcher = "You are a dispatcher. Do the task below and report what changed.";
        let folder = "Working directory: /home/sam/fm/8279d42859b9/slots/3";
        let label = "Fix the invoice export";
        write_harness_session(&paths, JOB_A, None, 0, false, 10516, 3585, Some(dispatcher));
        write_harness_session(
            &paths,
            WORKER_A,
            Some(JOB_A),
            1,
            false,
            2200,
            1100,
            Some(folder),
        );
        let now = utc("2026-11-14T19:00:00Z");
        let owners = BTreeMap::from([(JOB_A.to_string(), SESSION.to_string())]);
        let claude_cwd = BTreeMap::from([(SESSION.to_string(), ATLAS_CWD.to_string())]);
        let mut reports = crate::harness::JobReports::default();
        assert!(reports.refresh(&paths).is_empty());
        let rows_of = |owners: &BTreeMap<String, String>,
                       locks: &BTreeSet<String>,
                       reports: &crate::harness::JobReports| {
            let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
            harness_rows(
                &harness.sessions,
                owners,
                &claude_cwd,
                &BTreeMap::new(),
                locks,
                reports,
                None,
                &unknown_pricing(),
                "atlas",
                now,
                &|_| project_key(),
            )
        };
        let task_of = |out: &HarnessRows, id: &str| {
            out.agents[SESSION]
                .iter()
                .find(|agent| agent.agent == id)
                .expect("the agent")
                .task
                .clone()
        };

        let running_job = rows_of(&owners, &BTreeSet::new(), &reports);
        assert_eq!(task_of(&running_job, JOB_A), None, "no report yet");
        assert_eq!(task_of(&running_job, WORKER_A), None);
        for agent in &running_job.agents[SESSION] {
            assert_eq!(agent.provider, HARNESS_PROVIDER);
        }
        // Not linked yet, the running job is its own row: its worker shows no
        // task either, and the row is not named by the dispatcher prompt.
        let own = rows_of(
            &BTreeMap::new(),
            &BTreeSet::from([JOB_A.to_string()]),
            &reports,
        );
        assert_eq!(own.rows.sessions.len(), 1);
        assert_eq!(own.rows.sessions[0].title, None);
        assert_eq!(own.rows.sessions[0].agents[0].task, None);
        assert_eq!(own.rows.sessions[0].agents[0].provider, HARNESS_PROVIDER);

        // The job ends and flash-agents writes its report (a background call).
        write_harness_session(&paths, JOB_A, None, 0, true, 10516, 3585, Some(dispatcher));
        let patches = paths.reports.join("8279d42859b9").join("patches");
        std::fs::create_dir_all(&patches).expect("the reports folder");
        std::fs::write(
            patches.join("job-3e32c2fc.report.json"),
            serde_json::json!({
                "key": "8279d42859b9",
                "kind": "task",
                "sessionId": JOB_A,
                "label": label,
                "status": "done",
                "stopReason": "end_turn",
                "timing": {"startedAt": "2026-11-14T09:40:00.000Z"},
                "createdAt": "2026-11-14T09:40:00.000Z",
                "report": {"text": "Done."}
            })
            .to_string(),
        )
        .expect("the job's report");
        assert!(reports.refresh(&paths).is_empty());
        let done = rows_of(&owners, &BTreeSet::new(), &reports);
        assert_eq!(task_of(&done, JOB_A).as_deref(), Some(label));
        assert_eq!(task_of(&done, WORKER_A), None);

        let mut claude = row(LiveStatus::Busy, Some("claude-opus-5-5"), now);
        add_agents(&mut claude, done.agents[SESSION].clone());
        let history = HistoryDir::at(&dir.path().join("history"), "atlas");
        write_report(&history, &[claude], now).expect("the sessions report");
        crate::history::Recorder::new(HistoryDir::at(&dir.path().join("history"), "atlas"))
            .tokens(&done.tokens)
            .expect("the token lines");
        let mut found = String::new();
        let mut files = 0;
        collect_text(&dir.path().join("history"), &mut found, &mut files);
        assert!(files > 1, "the report and a token line were written");
        for text in [label, dispatcher, folder] {
            assert!(!found.contains(text), "{text} reached a file");
        }
    }

    /// A report of an older seatswap has no `provider` on its agents: each
    /// takes its session's provider when the report is read. An agent that
    /// has its own keeps it.
    #[test]
    fn an_older_reports_agents_take_their_sessions_provider() {
        let dir = tempdir().expect("tempdir");
        let now = utc("2026-11-14T19:00:00Z");
        let atlas = HistoryDir::at(dir.path(), "atlas");
        let vega = HistoryDir::at(dir.path(), "vega");
        let ledger = HistoryDir::at(dir.path(), "ledger");
        host_marker(dir.path(), "vega");
        host_marker(dir.path(), "ledger");
        let at = now - Duration::minutes(2);

        let mut claude = report_session("vega", LiveStatus::Busy, at);
        claude.agents.push(report_agent(AGENT, CLAUDE_PROVIDER, at));
        let mut own = report_session("vega", LiveStatus::Busy, at);
        own.session = OWN_SESSION.to_string();
        own.provider = HARNESS_PROVIDER.to_string();
        own.agents
            .push(report_agent(WORKER_A, HARNESS_PROVIDER, at));
        let mut older = serde_json::to_value(HostReport {
            at,
            sessions: vec![claude, own],
        })
        .expect("a report");
        for session in older["sessions"].as_array_mut().expect("sessions") {
            for agent in session["agents"].as_array_mut().expect("agents") {
                agent
                    .as_object_mut()
                    .expect("an agent")
                    .remove("provider")
                    .expect("the field the older report lacks");
            }
        }
        std::fs::write(vega.sessions_now_file("vega"), older.to_string())
            .expect("vega's older report");

        // A current report: a flash job under a Claude Code row keeps its own.
        let mut current = report_session("ledger", LiveStatus::Busy, at);
        current
            .agents
            .push(report_agent(JOB_A, HARNESS_PROVIDER, at));
        write_report(&ledger, &[current], at).expect("ledger's report");

        let other = other_hosts(&atlas, now).expect("the other hosts");
        assert!(other.warnings.is_empty(), "{:?}", other.warnings);
        let host = |name: &str| {
            other
                .hosts
                .iter()
                .find(|host| host.host == name)
                .expect("the host")
        };
        let vega = host("vega");
        assert_eq!(vega.sessions[0].agents[0].provider, CLAUDE_PROVIDER);
        assert_eq!(vega.sessions[1].agents[0].provider, HARNESS_PROVIDER);
        assert_eq!(
            host("ledger").sessions[0].agents[0].provider,
            HARNESS_PROVIDER
        );
    }

    /// Every agent names its provider: a Claude Code subagent Claude Code's.
    #[test]
    fn a_claude_subagent_names_its_provider() {
        let meta = AgentMeta {
            session: SESSION.to_string(),
            agent: AGENT.to_string(),
            agent_type: Some("general-purpose".to_string()),
            description: Some("fix the failing tests".to_string()),
            finished: false,
            spawn_depth: Some(1),
            tool_use_id: Some("toolu_01Nkn4".to_string()),
            parent: None,
            transcript_ended: false,
        };
        let rows = running(
            &[session_file()],
            &state_with(seen("claude-opus-5-5", 1_000_000)),
            &|_: &str| vec![meta.clone()],
            &BTreeMap::new(),
            RowTokens::Recorded {
                recorded: &RecordedTokens::default(),
                pending: &[],
            },
            Some(&projects()),
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_: &str| project_key(),
        );
        assert_eq!(rows.sessions[0].agents[0].provider, CLAUDE_PROVIDER);
        let json = serde_json::to_value(&rows.sessions[0].agents[0]).expect("an agent");
        assert_eq!(json["provider"], CLAUDE_PROVIDER, "camelCase `provider`");
    }

    fn collect_text(dir: &std::path::Path, text: &mut String, files: &mut usize) {
        for entry in std::fs::read_dir(dir).expect("a folder") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                collect_text(&path, text, files);
            } else {
                *files += 1;
                text.push_str(&std::fs::read_to_string(&path).expect("a history file"));
            }
        }
    }

    #[test]
    fn proof_a_worker_whose_job_is_out_of_range_still_hangs_under_the_owner() {
        let (_dir, paths) = harness_store();
        // Only the worker is in the range (its job's file is older than `since`).
        write_harness_session(&paths, WORKER_A, Some(JOB_A), 1, false, 2200, 1100, None);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let owners = BTreeMap::from([(JOB_A.to_string(), SESSION.to_string())]);
        let claude_cwd = BTreeMap::from([(SESSION.to_string(), ATLAS_CWD.to_string())]);

        let out = harness_rows(
            &harness.sessions,
            &owners,
            &claude_cwd,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert!(
            out.rows.sessions.is_empty(),
            "the worker became its own DeepSeek Harness row"
        );
        assert_eq!(out.agents.get(SESSION).map(Vec::len), Some(1));
    }

    /// A flash job whose Claude Code session is not known and whose process
    /// has ended is done: no row (a Codex session is a row only while it runs
    /// too), but its tokens are still recorded.
    #[test]
    fn a_finished_job_without_its_session_is_no_row_but_keeps_its_tokens() {
        let (_dir, paths) = harness_store();
        write_harness_session(&paths, JOB_A, None, 0, true, 10516, 3585, None);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert!(out.rows.sessions.is_empty(), "{:?}", out.rows.sessions);
        let output: u64 = out.tokens.iter().map(|line| line.out).sum();
        assert_eq!(output, 3585, "the job's tokens are recorded");
    }

    /// A running flash job without its Claude Code session ran in a worker
    /// copy (`<base>/<key>/slots/<n>`): its row takes the folder the copy was
    /// made from, found among the known Claude Code sessions' folders.
    #[test]
    fn a_job_without_its_session_takes_the_folder_it_was_copied_from() {
        let (_dir, paths) = harness_store();
        // `8279d42859b9` is flash-agents' key of /home/sam/projects/atlas.
        let copy = "/home/sam/fm/8279d42859b9/slots/10";
        let dir = paths
            .sessions
            .join("--home-sam-fm-8279d42859b9-slots-10--")
            .join(JOB_B);
        std::fs::create_dir_all(&dir).expect("a session folder");
        let header = serde_json::json!({
            "type": "session", "version": 4, "id": JOB_B, "createdAt": 1794696000000_i64,
            "cwd": copy, "isSeeded": false, "delegationDepth": 0,
        });
        let text = format!(
            "{header}\n{}\n",
            harness_event("turn/start", 1794696001000, serde_json::json!({"turn": 1}))
        );
        std::fs::write(
            dir.join("session.v4.jsonl.zstd"),
            zstd::encode_all(text.as_bytes(), 3).expect("zstd"),
        )
        .expect("a session file");
        std::fs::write(dir.join("session.lock"), []).expect("a lock");
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        // An earlier Claude Code session of atlas, not the job's owner.
        let claude_cwd = BTreeMap::from([(SESSION.to_string(), ATLAS_CWD.to_string())]);
        let asked = std::cell::RefCell::new(Vec::new());

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &claude_cwd,
            &BTreeMap::new(),
            &BTreeSet::from([JOB_B.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|cwd| {
                asked.borrow_mut().push(cwd.to_string());
                project_key()
            },
        );

        assert_eq!(out.rows.sessions.len(), 1);
        assert_eq!(out.rows.sessions[0].cwd, ATLAS_CWD);
        assert_eq!(asked.borrow().as_slice(), [ATLAS_CWD.to_string()]);
    }

    #[test]
    fn a_worker_without_its_job_and_owner_is_its_own_row() {
        let (_dir, paths) = harness_store();
        write_harness_session(&paths, WORKER_A, Some(JOB_A), 1, false, 2200, 1100, None);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::from([WORKER_A.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert!(out.agents.is_empty());
        assert_eq!(out.rows.sessions.len(), 1);
        let row = &out.rows.sessions[0];
        assert_eq!(row.session, WORKER_A);
        assert_eq!(row.provider, "DeepSeek Harness");
    }

    /// Write a Harness session whose assistant messages are `(time, usage)`;
    /// `None` is a message without `usage`, an answer with no tokens.
    fn write_harness_answers(
        paths: &crate::harness::HarnessPaths,
        id: &str,
        answers: &[(i64, Option<(u64, u64)>)],
    ) {
        let dir = paths.sessions.join(HARNESS_KEY).join(id);
        std::fs::create_dir_all(&dir).expect("a session folder");
        let first = answers.first().map(|(at, _)| *at).unwrap_or(0);
        let header = serde_json::json!({
            "type": "session",
            "version": 4,
            "id": id,
            "createdAt": first - 3_600_000,
            "cwd": ATLAS_CWD,
            "isSeeded": false,
            "delegationDepth": 0,
        });
        let mut lines = vec![
            header.to_string(),
            harness_event("turn/start", first - 60_000, serde_json::json!({"turn": 1})),
        ];
        for (number, (at, usage)) in answers.iter().enumerate() {
            let mut data = serde_json::json!({
                "turn": 1,
                "step": 1,
                "message": {
                    "role": "assistant",
                    "source": {"kind": "model", "provider": "ollama", "model": FLASH_MODEL},
                    "id": format!("m{number}")
                },
            });
            if let Some((input, out)) = usage {
                data["usage"] = serde_json::json!({
                    "inputTokens": input,
                    "outputTokens": out,
                    "totalTokens": input + out
                });
            }
            lines.push(harness_event("assistant/message", *at, data));
        }
        let last = answers.last().map(|(at, _)| *at).unwrap_or(first);
        lines.push(harness_event(
            "turn/end",
            last + 1_000,
            serde_json::json!({"turn": 1, "reason": {"kind": "completed"}}),
        ));
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(
            dir.join("session.v4.jsonl.zstd"),
            zstd::encode_all(text.as_bytes(), 3).expect("zstd"),
        )
        .expect("a session file");
        std::fs::write(dir.join("session.lock"), []).expect("a lock");
    }

    #[test]
    fn harness_token_lines_are_per_hour_of_the_answers() {
        let (_dir, paths) = harness_store();
        let first = 1_794_696_000_000_i64;
        let second = first + 3_600_000;
        write_harness_answers(
            &paths,
            JOB_A,
            &[(first, Some((100, 50))), (second, Some((200, 70)))],
        );
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let session = &harness.sessions[0];

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        let bucket_of = |ms: i64| {
            transcripts::bucket_start(Utc.timestamp_millis_opt(ms).single().expect("a time"))
        };
        assert_eq!(
            out.tokens.len(),
            2,
            "one line per bucket of the new answers: {:?}",
            out.tokens
        );
        let mut lines: Vec<(DateTime<Utc>, u64, u64, u32)> = out
            .tokens
            .iter()
            .map(|record| (record.t, record.input, record.out, record.agent_answers))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            vec![
                (bucket_of(first), 100, 50, 1),
                (bucket_of(second), 200, 70, 1),
            ]
        );
        assert!(
            out.tokens
                .iter()
                .all(|record| record.minutes == transcripts::TOKEN_BUCKET_MINUTES as u32),
            "{:?}",
            out.tokens
        );
        assert_eq!(
            out.tokens.iter().map(|record| record.input).sum::<u64>(),
            session.tokens.input,
            "the lines' sums are the delta since the recorded total"
        );
        assert_eq!(
            out.tokens.iter().map(|record| record.out).sum::<u64>(),
            session.tokens.out
        );
    }

    #[test]
    fn harness_lines_use_five_minute_buckets() {
        let (_dir, paths) = harness_store();
        let first = 1_794_696_000_000_i64;
        write_harness_answers(
            &paths,
            JOB_A,
            &[
                (first, Some((100, 50))),
                (first + 60_000, Some((200, 70))),
                (first + 360_000, Some((300, 90))),
            ],
        );
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        let bucket_of = |ms: i64| {
            transcripts::bucket_start(Utc.timestamp_millis_opt(ms).single().expect("a time"))
        };
        let mut lines: Vec<(DateTime<Utc>, u64, u64, u32)> = out
            .tokens
            .iter()
            .map(|record| (record.t, record.input, record.out, record.agent_answers))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            vec![
                (bucket_of(first), 300, 120, 2),
                (bucket_of(first + 360_000), 300, 90, 1),
            ],
            "the two answers of the first bucket share one line"
        );
        assert!(
            out.tokens
                .iter()
                .all(|record| record.minutes == transcripts::TOKEN_BUCKET_MINUTES as u32),
            "{:?}",
            out.tokens
        );
    }

    #[test]
    fn an_answer_without_usage_adds_a_line_with_no_tokens() {
        let (_dir, paths) = harness_store();
        let first = 1_794_696_000_000_i64;
        write_harness_answers(
            &paths,
            JOB_A,
            &[(first, Some((100, 50))), (first + 1_000, None)],
        );
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert_eq!(out.tokens.len(), 1, "the two answers share their hour");
        assert_eq!(out.tokens[0].input, 100);
        assert_eq!(out.tokens[0].out, 50);
        assert_eq!(
            out.tokens[0].agent_answers, 2,
            "a message without usage is still an answer"
        );
    }

    #[test]
    fn harness_agent_answers_count_the_new_messages() {
        let (_dir, paths) = harness_store();
        // Two assistant messages, then a third one in a later pass.
        write_harness_messages(&paths, JOB_A, None, 0, 2, true, 1_794_696_004_000);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let session = &harness.sessions[0];

        let first = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert_eq!(first.tokens.len(), 1);
        assert_eq!(first.tokens[0].agent_answers, 2);
        assert_eq!(first.tokens[0].input, 200);
        assert_eq!(
            first.tokens[0].t,
            transcripts::bucket_start(session.last_message),
            "the line is dated at the bucket of the last answer S-1 uses"
        );
        assert_ne!(
            first.tokens[0].t,
            transcripts::bucket_start(session.last),
            "the last event (turn/end) is not the answer's bucket"
        );

        write_harness_messages(&paths, JOB_A, None, 0, 3, true, 1_794_696_005_000);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));
        let second = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &first.recorded,
            &BTreeSet::new(),
            &crate::harness::JobReports::default(),
            None,
            &unknown_pricing(),
            "atlas",
            utc("2026-11-14T19:00:00Z"),
            &|_| project_key(),
        );

        assert_eq!(second.tokens.len(), 1);
        assert_eq!(second.tokens[0].agent_answers, 1);
        assert_eq!(second.tokens[0].input, 100);
    }

    #[test]
    fn a_harness_rows_last_hour_counts_the_last_sixty_minutes() {
        // A DeepSeek Harness session of its own between two turns.
        let (dir, paths) = harness_store();
        let now = utc("2026-11-14T19:00:00Z");
        write_harness_answers(
            &paths,
            OWN_SESSION,
            &[
                (
                    (now - Duration::minutes(90)).timestamp_millis(),
                    Some((1_000, 100)),
                ),
                (
                    (now - Duration::minutes(30)).timestamp_millis(),
                    Some((20, 2)),
                ),
                (
                    (now - Duration::minutes(5)).timestamp_millis(),
                    Some((30, 3)),
                ),
            ],
        );
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let mut priced = prices();
        priced.take(
            std::collections::BTreeMap::from([(
                money::model_price_key(FLASH_MODEL),
                ModelPrice {
                    input: 4.0,
                    output: 20.0,
                    cache_write: 5.0,
                    cache_read: 0.2,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let valuer = valuer_with(&history, &settings, &[], &priced);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::from([OWN_SESSION.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &known_pricing(&valuer, &settings),
            "atlas",
            now,
            &|_| project_key(),
        );

        assert_eq!(out.rows.sessions.len(), 1);
        let row = &out.rows.sessions[0];
        assert_eq!(
            row.last_hour,
            Some(Tokens {
                input: 50,
                out: 5,
                cw: 0,
                cr: 0,
            }),
            "the two answers inside the hour, not the one at −90min"
        );
        assert_eq!(row.tokens.out, 105, "the row's own total is unchanged");
        assert!(
            row.last_hour_values.api.is_some(),
            "the known hour is valued, never left missing: {:?}",
            row.last_hour_values
        );
        assert_eq!(row.last_hour_values.api_missing, None);
    }

    #[test]
    fn a_last_hour_without_tokens_is_a_known_zero() {
        // A DeepSeek Harness session of its own between two turns.
        let (dir, paths) = harness_store();
        let now = utc("2026-11-14T19:00:00Z");
        write_harness_answers(
            &paths,
            OWN_SESSION,
            &[(
                (now - Duration::minutes(90)).timestamp_millis(),
                Some((1_000, 100)),
            )],
        );
        let history = HistoryDir::at(dir.path(), "atlas");
        let settings = settings_eur();
        let mut priced = prices();
        priced.take(
            std::collections::BTreeMap::from([(
                money::model_price_key(FLASH_MODEL),
                ModelPrice {
                    input: 4.0,
                    output: 20.0,
                    cache_write: 5.0,
                    cache_read: 0.2,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let valuer = valuer_with(&history, &settings, &[], &priced);
        let harness = crate::harness::sessions(&paths, utc("2026-11-01T00:00:00Z"));

        let out = harness_rows(
            &harness.sessions,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::from([OWN_SESSION.to_string()]),
            &crate::harness::JobReports::default(),
            None,
            &known_pricing(&valuer, &settings),
            "atlas",
            now,
            &|_| project_key(),
        );

        let row = &out.rows.sessions[0];
        assert_eq!(
            row.last_hour,
            Some(Tokens::default()),
            "the only answer is outside the hour"
        );
        assert_eq!(
            row.last_hour_values.api,
            Some(0.0),
            "an hour without tokens is a known 0, not an unknown value: {:?}",
            row.last_hour_values
        );
        assert_eq!(row.last_hour_values.api_currency.as_deref(), Some("EUR"));
        assert_eq!(row.last_hour_values.api_missing, None);
        assert_eq!(row.last_hour_values.subscription, Some(0.0));
        assert_eq!(row.last_hour_values.subscription_missing, None);
    }

    #[test]
    fn the_provider_cost_becomes_the_api_value() {
        let now = utc("2026-11-14T10:00:00Z");
        let settings = settings_eur();
        let mut fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        fx.take(now.date_naive(), 2.0);
        let mut values = Values {
            api: Some(9.0),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(2.0),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        assert_eq!(
            take_provider_cost(&mut values, Some(1.5), &settings, &fx, now),
            None
        );
        assert_eq!(
            values.api,
            Some(3.0),
            "the provider's dollars are converted at the stored rate and beat models.dev's value"
        );
        assert_eq!(values.api_currency.as_deref(), Some("EUR"));
        assert_eq!(
            values.subscription,
            Some(2.0),
            "the subscription side is not touched"
        );
        assert_eq!(values.api_missing, None);

        let mut unpriced = Values::missing("no price for claude-x", &settings);
        assert_eq!(
            take_provider_cost(&mut unpriced, Some(1.5), &settings, &fx, now),
            None
        );
        assert_eq!(unpriced.api, Some(3.0), "a provider cost needs no price");
        assert_eq!(unpriced.api_missing, None);
        assert_eq!(
            unpriced.subscription, None,
            "models.dev's missing subscription stays"
        );

        let mut kept = Values::missing("no price for claude-x", &settings);
        assert_eq!(
            take_provider_cost(&mut kept, None, &settings, &fx, now),
            None
        );
        assert_eq!(
            kept.api_missing.as_deref(),
            Some("no price for claude-x"),
            "without a cost the reason stays"
        );

        // With the look-ups off the cost stays in US dollars, as models.dev's
        // values do.
        let mut off = settings.clone();
        off.look_up_prices = false;
        let mut usd = values.clone();
        assert_eq!(
            take_provider_cost(&mut usd, Some(1.5), &off, &fx, now),
            None
        );
        assert_eq!(usd.api, Some(1.5));
        assert_eq!(usd.api_currency.as_deref(), Some("USD"));

        // No currency in the settings: the API value is missing with that
        // reason, never a number in an unnamed currency.
        let mut bare = settings.clone();
        bare.currency = None;
        let mut none = values.clone();
        assert_eq!(
            take_provider_cost(&mut none, Some(1.5), &bare, &fx, now),
            None
        );
        assert_eq!(none.api, None);
        assert_eq!(none.api_currency, None);
        assert_eq!(
            none.api_missing.as_deref(),
            Some("set a currency in Settings → General")
        );
        assert_eq!(
            none.subscription,
            Some(2.0),
            "the subscription half is not touched either"
        );

        // A huge cost times the rate overflows: the conversion is not used and
        // the row keeps the valuer's value; the caller names the session.
        let mut huge_fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        huge_fx.take(now.date_naive(), f64::MAX);
        let before = values.clone();
        let warning = take_provider_cost(&mut values, Some(f64::MAX), &settings, &huge_fx, now)
            .expect("a non-finite conversion says why");
        assert!(warning.contains("not used"), "{warning}");
        assert_eq!(values, before, "the valuer's value stays");
    }

    #[test]
    fn a_provider_cost_makes_a_partial_api_half_full() {
        let now = utc("2026-11-14T10:00:00Z");
        let settings = settings_eur();
        let mut fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        fx.take(now.date_naive(), 2.0);

        let mut partial = Values {
            api: Some(1.25),
            api_currency: Some("EUR".to_string()),
            api_missing: Some("no price for claude-x".to_string()),
            api_partial: true,
            subscription: Some(2.0),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        assert_eq!(
            take_provider_cost(&mut partial, Some(1.5), &settings, &fx, now),
            None
        );
        assert_eq!(partial.api, Some(3.0));
        assert!(
            !partial.api_partial,
            "the provider's cost covers the whole session, so the half is no longer partial"
        );
        assert_eq!(partial.api_missing, None);
        assert_eq!(
            partial.subscription,
            Some(2.0),
            "the subscription half is not touched"
        );

        // No currency in the settings: the cost cannot be named, so the API
        // half becomes unknown with that reason and is never partial.
        let mut bare = settings.clone();
        bare.currency = None;
        let mut missing = Values {
            api: Some(1.25),
            api_currency: Some("EUR".to_string()),
            api_missing: Some("no price for claude-x".to_string()),
            api_partial: true,
            subscription: Some(2.0),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        assert_eq!(
            take_provider_cost(&mut missing, Some(1.5), &bare, &fx, now),
            None
        );
        assert_eq!(missing.api, None);
        assert!(
            !missing.api_partial,
            "a half with no value is never partial"
        );
        assert_eq!(
            missing.api_missing.as_deref(),
            Some("set a currency in Settings → General")
        );
    }
}
