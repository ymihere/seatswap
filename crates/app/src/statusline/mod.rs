//! Claude Code's status line: the payload it writes to standard input (spec
//! §5.2), the numbers seatswap fills in, and the layout it renders (spec §5.4).
//!
//! `parse` is the one place a malformed payload is tolerated: Claude Code may
//! send anything, and the status line must still print. `facts` gathers what
//! the line shows besides the payload; `render_spans` fits it to the terminal
//! and `ansi` turns the spans into the command's one line.

#[cfg(any(feature = "gui", test))]
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use seatswap_core::api::Usage;
use seatswap_core::atomic::{self, Access};
use seatswap_core::burn::burn_next;
use seatswap_core::engine::UsageCache;
use seatswap_core::feed::{self, Feed, FeedLimit};
use seatswap_core::paths::{self, AppPaths};
use seatswap_core::providers::google;
use seatswap_core::store::{Account, AccountsFile, Settings};
#[cfg(any(feature = "gui", test))]
use seatswap_core::store::{SegmentKind, StatuslineConfig};
use seatswap_core::switcher::LiveState;

use crate::format;
use crate::gui::ipc;

pub(crate) mod render;
pub(crate) mod width;

#[cfg(any(feature = "gui", test))]
use render::Span;
use render::{git_branch, AccountFacts, Facts, Limit2};

/// The status line payload of `specs/plan/README.md` §5, whole. The preview
/// draws [`preview_payload`] instead; this is the tests' copy of README §5.
#[cfg(test)]
pub(crate) const README_PAYLOAD: &str = r#"{"workspace":{"current_dir":"/home/sam/projects/atlas"},"model":{"id":"claude-fable-5-2","display_name":"Fable 5.2"},"effort":{"level":"high"},"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1794490200},"seven_day":{"used_percentage":48,"resets_at":1794790800}}}"#;

/// The branch the preview's made-up session is on. The preview's folder does
/// not exist on the machine, so `.git/HEAD` has no branch to read.
#[cfg(any(feature = "gui", test))]
const PREVIEW_BRANCH: &str = "main";

/// The parts of the payload seatswap reads. Unknown keys are ignored: Claude
/// Code adds keys with new versions. A key that is missing or `null` is `None`.
#[derive(Deserialize, Default)]
pub(crate) struct Payload {
    /// The whole payload as Claude Code sent it, for the fields that are not
    /// typed above. `Null` when standard input was not JSON.
    #[serde(skip)]
    pub raw: serde_json::Value,
    pub workspace: Option<Workspace>,
    pub model: Option<Model>,
    pub effort: Option<Effort>,
    pub rate_limits: Option<RateLimits>,
}

/// `workspace.current_dir`: the session's folder.
#[derive(Deserialize)]
pub(crate) struct Workspace {
    pub current_dir: Option<String>,
}

/// The model's id (coloured by family) and display name (printed).
#[derive(Deserialize)]
pub(crate) struct Model {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

/// The current effort level (`low`, `medium`, `high`, `xhigh`, `max`).
#[derive(Deserialize)]
pub(crate) struct Effort {
    pub level: Option<String>,
}

/// The live limits of the session's account; absent until its first answer.
#[derive(Deserialize)]
pub(crate) struct RateLimits {
    pub five_hour: Option<RateLimit>,
    pub seven_day: Option<RateLimit>,
}

/// One limit: a whole percent and the reset in seconds since the epoch.
#[derive(Deserialize)]
pub(crate) struct RateLimit {
    pub used_percentage: f64,
    pub resets_at: i64,
}

/// Claude Code's JSON to a [`Payload`]. Anything that is not a JSON object of
/// this shape gives the default typed fields: the line must print whatever the
/// input was. `raw` keeps what was parsed, or `null` when it was not JSON. A
/// present key of the wrong type is an error for the whole typed payload (the
/// `RateLimit` fields are not optional), so it is defaulted too.
pub(crate) fn parse(stdin: &[u8]) -> Payload {
    let raw: serde_json::Value = serde_json::from_slice(stdin).unwrap_or_default();
    let mut payload: Payload = serde_json::from_value(raw.clone()).unwrap_or_default();
    payload.raw = raw;
    payload
}

/// The preview's made-up session: README §5's payload, with the folder
/// `home/projects/atlas` and the keys Claude Code sends after a session's first
/// answer (its context window, cost, lines and prompt). The numbers are the
/// fixture's samples; only the account's numbers on the preview line are live.
#[cfg(any(feature = "gui", test))]
pub(crate) fn preview_payload(home: &std::path::Path) -> Payload {
    let folder = home.join("projects").join("atlas");
    let raw = serde_json::json!({
        "workspace": {"current_dir": folder.display().to_string()},
        "model": {"id": "claude-fable-5-2", "display_name": "Fable 5.2"},
        "effort": {"level": "high"},
        "rate_limits": {
            "five_hour": {"used_percentage": 12, "resets_at": 1794490200},
            "seven_day": {"used_percentage": 48, "resets_at": 1794790800}
        },
        "context_window": {
            "context_window_size": 200000,
            "total_input_tokens": 82000,
            "total_output_tokens": 4100,
            "used_percentage": 41,
            "remaining_percentage": 59,
            "current_usage": {
                "input_tokens": 82000,
                "output_tokens": 4100,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        },
        "cost": {
            "total_cost_usd": 3.2,
            "total_duration_ms": 300000,
            "total_api_duration_ms": 210000,
            "total_lines_added": 120,
            "total_lines_removed": 8
        },
        "session_name": "atlas-a6",
        "prompt_id": "0b6f3a1c-5d7e-4f2a-9c8b-7e6d5c4b3a21",
        "prompt_cache": {}
    });
    parse(raw.to_string().as_bytes())
}

/// The feed a payload carries: its 5 h and weekly numbers, its model, its
/// session and that session's own cost, at `now`. `None` when the payload has no
/// `rate_limits` at all (before a session's first answer there is nothing to
/// feed), or when it has no `seven_day`: a feed without a week cannot be matched
/// to an account and would replace a complete `feed.json` with a partial one.
///
/// A cost that cannot be stored does not cost the status line: the feed is still
/// written with `cost_usd` `None` and a `cost_problem` naming the session and the
/// reason (item 6), so `publish_feed` and `facts_from_source` read the one feed
/// and the window's preview sees the same text.
pub(crate) fn feed_from_payload(payload: &Payload, now: DateTime<Utc>) -> Option<Feed> {
    let limits = payload.rate_limits.as_ref()?;
    let seven_day = limits.seven_day.as_ref().and_then(feed_limit)?;
    let five_hour = limits.five_hour.as_ref().and_then(feed_limit);
    let session = payload_session(payload);
    let (cost_usd, cost_problem) = payload_cost(payload, session.as_deref());
    Some(Feed {
        at: now,
        five_hour,
        seven_day: Some(seven_day),
        model: payload.model.as_ref().and_then(|model| model.id.clone()),
        session,
        cost_usd,
        cost_problem,
    })
}

/// Claude Code's `session_id`: the session the payload's numbers come from.
/// Absent, `null` or not a text is `None`; a cost without a session has no row
/// to belong to and is reported by [`payload_cost`], never guessed.
fn payload_session(payload: &Payload) -> Option<String> {
    match payload.raw.get("session_id") {
        Some(serde_json::Value::String(session)) => Some(session.clone()),
        _ => None,
    }
}

/// Claude Code's `cost.total_cost_usd`, in US dollars. Absent or `null` is no
/// cost and no problem. A cost that is not a number, negative or not finite is
/// no cost either, but its reason is returned for the feed's `cost_problem`; a
/// cost without a session id is not stored, since no row could own it.
fn payload_cost(payload: &Payload, session: Option<&str>) -> (Option<f64>, Option<String>) {
    let Some(cost) = payload.raw.get("cost").filter(|cost| !cost.is_null()) else {
        return (None, None);
    };
    let Some(value) = cost.get("total_cost_usd") else {
        return (None, None);
    };
    let named = match session {
        Some(session) => format!(" for session {session}"),
        None => String::new(),
    };
    match value.as_f64() {
        Some(usd) if usd.is_finite() && usd >= 0.0 && session.is_some() => (Some(usd), None),
        Some(usd) if usd.is_finite() && usd >= 0.0 => (
            None,
            Some(format!(
                "Claude Code's cost.total_cost_usd {usd} has no session_id, so no session can own it; it is not stored"
            )),
        ),
        Some(usd) => (
            None,
            Some(format!(
                "Claude Code's cost.total_cost_usd{named} is {usd}; only a finite cost of 0 or more is stored"
            )),
        ),
        None => (
            None,
            Some(format!(
                "Claude Code's cost.total_cost_usd{named} is not a number: {value}; it is not stored"
            )),
        ),
    }
}

/// One payload limit as a feed limit. A reset time chrono cannot represent is
/// left out, never replaced by another time.
fn feed_limit(limit: &RateLimit) -> Option<FeedLimit> {
    Some(FeedLimit {
        percent: limit.used_percentage,
        resets_at: DateTime::<Utc>::from_timestamp(limit.resets_at, 0)?,
    })
}

/// Write the payload's live numbers to `feed.json` when the setting is on, and
/// tell a running tray app when it wrote. The status line calls this before it
/// renders, so a failure becomes the line's `feed: <reason>`.
pub(crate) fn publish_feed(
    app: &AppPaths,
    settings: &Settings,
    payload: &Payload,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if !settings.statusline_feed {
        return Ok(());
    }
    let Some(feed) = feed_from_payload(payload, now) else {
        return Ok(());
    };
    if !feed::write_if_due(app, &feed)
        .map_err(|error| format!("cannot write {}: {error}", app.feed_file().display()))?
    {
        return Ok(());
    }
    ipc::send_feed(app).map_err(|error| error.to_string())
}

/// `agy`'s payload (spec §7.9): keep the Google account's numbers in
/// `google.json`, and answer with the line `agy` shows: the plan as `agy` names
/// it ("Google AI Pro") and each limit's use. `None` when the payload is not `agy`'s.
/// Without an account the line is empty.
pub(crate) fn google_line(
    app: &AppPaths,
    payload: &Payload,
    now: DateTime<Utc>,
) -> Result<Option<String>, String> {
    if !google::is_payload(&payload.raw) {
        return Ok(None);
    }
    let Some(feed) = google::feed_from_payload(&payload.raw, now) else {
        return Ok(Some(String::new()));
    };
    google::write_if_due(app, &feed)
        .map_err(|error| format!("cannot write {}: {error}", app.google_file().display()))?;
    let read = google::reading(feed, now);
    let mut parts = vec![read.reading.plan.unwrap_or_else(|| "Google".to_string())];
    for limit in read.reading.usage.iter().flat_map(|usage| &usage.limits) {
        parts.push(format!(
            "{} {}",
            google::limit_name(&limit.kind),
            format::percent(limit.percent)
        ));
    }
    Ok(Some(parts.join(" · ")))
}

/// One key path of the payload and the JSON kind of the value found there.
/// The value itself is never kept: the recorder exists to learn which fields
/// Claude Code sends, not what they hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeyInfo {
    pub path: String,
    /// `string`, `number`, `boolean`, `null`, `object` or `array`.
    pub kind: String,
}

/// The recorder's file (`statusline-keys.json`): the paths recorded so far
/// and when they were last written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordedKeys {
    pub at: DateTime<Utc>,
    pub keys: Vec<KeyInfo>,
}

/// The JSON kind of a value, as it is written in the file.
fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::String(_) => "string",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Null => "null",
        serde_json::Value::Object(_) => "object",
        serde_json::Value::Array(_) => "array",
    }
}

/// Every path of `raw` with the kind at that path, sorted by path. An array
/// adds `path[]` for its elements (one entry per kind when they differ) and an
/// array of objects continues below `path[].key`. `raw` that is not a JSON
/// object has no payload paths. No value is read into the result.
pub(crate) fn key_paths(raw: &serde_json::Value) -> Vec<KeyInfo> {
    let mut found = Vec::new();
    if raw.is_object() {
        visit(raw, "", false, &mut found);
    }
    sorted_unique(found)
}

/// Collect the paths below `value`. `emit_self` is false for the root object
/// (it has no path) and for an array's element, whose kind was already written
/// as `path[]` by the caller.
fn visit(value: &serde_json::Value, path: &str, emit_self: bool, found: &mut Vec<KeyInfo>) {
    if emit_self {
        found.push(KeyInfo {
            path: path.to_string(),
            kind: kind_of(value).to_string(),
        });
    }
    match value {
        serde_json::Value::Object(entries) => {
            for (key, child) in entries {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                visit(child, &child_path, true, found);
            }
        }
        serde_json::Value::Array(items) => {
            let item_path = format!("{path}[]");
            for item in items {
                match item {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        // The element's kind belongs to `path[]`; its own
                        // children continue below that same prefix.
                        found.push(KeyInfo {
                            path: item_path.clone(),
                            kind: kind_of(item).to_string(),
                        });
                        visit(item, &item_path, false, found);
                    }
                    scalar => visit(scalar, &item_path, true, found),
                }
            }
        }
        _ => {}
    }
}

/// Sort by path (then kind) and drop repeats.
fn sorted_unique(mut keys: Vec<KeyInfo>) -> Vec<KeyInfo> {
    keys.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
    keys.dedup();
    keys
}

/// The recorded keys in `<data>/statusline-keys.json`: `None` when the file is
/// missing. A file that cannot be read or does not parse is an error naming
/// the path and the reason; it is never replaced by an empty recording.
pub(crate) fn read_recorded_keys(app: &AppPaths) -> Result<Option<RecordedKeys>, String> {
    let path = app.statusline_keys_file();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

/// Write the recorded keys through [`atomic`], private like the feed file.
pub(crate) fn write_recorded_keys(app: &AppPaths, recorded: &RecordedKeys) -> Result<(), String> {
    let path = app.statusline_keys_file();
    atomic::ensure_private_dir(&app.data_dir)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    let bytes = serde_json::to_vec_pretty(recorded)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    atomic::write_atomic(&path, &bytes, Access::Private)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// While the setting's time is after `now`, merge the file's key paths with
/// this payload's and write the union with `at = now`. Outside a recording, or
/// for a payload that is not a JSON object, this neither reads nor writes the
/// keys file. A file that does not parse is an error, never overwritten.
pub(crate) fn record_keys(
    app: &AppPaths,
    settings: &Settings,
    payload: &Payload,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let Some(until) = settings.statusline_record_until else {
        return Ok(());
    };
    if until <= now {
        return Ok(());
    }
    if !payload.raw.is_object() {
        return Ok(());
    }
    let mut keys = match read_recorded_keys(app)? {
        Some(stored) => stored.keys,
        None => Vec::new(),
    };
    keys.extend(key_paths(&payload.raw));
    write_recorded_keys(
        app,
        &RecordedKeys {
            at: now,
            keys: sorted_unique(keys),
        },
    )
}

/// How long a recording lasts: 1 to 60 minutes from `now`. 0 and more than 60
/// are refused with the reason; the page only asks for 1.
#[cfg(any(feature = "gui", test))]
pub(crate) fn record_until(minutes: u32, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    if !(1..=60).contains(&minutes) {
        return Err(format!("a recording lasts 1 to 60 minutes, got {minutes}"));
    }
    Ok(now + chrono::Duration::minutes(i64::from(minutes)))
}

fn cached_limit(limit: &seatswap_core::api::Limit) -> Limit2 {
    Limit2 {
        percent: limit.percent,
        resets_at: limit.resets_at,
    }
}

/// The next account the tray app wrote to `burn-next.json`, named through the
/// caller's lookup: the command looks in its accounts file, the preview in the
/// snapshot's views. The tray app writes the file only while auto-burn is on
/// and unpaused; a missing or stale file is no value. An id the lookup does not
/// know is no value either, never a guessed abbreviation.
pub(crate) fn burn_next_account(
    app: &AppPaths,
    now: DateTime<Utc>,
    abbr_of: impl Fn(&str) -> Option<String>,
) -> Result<Option<(String, DateTime<Utc>)>, String> {
    match burn_next::read(app, now) {
        Ok(Some(next)) => Ok(abbr_of(&next.account).map(|abbr| (abbr, next.at))),
        Ok(None) => Ok(None),
        Err(error) => Err(format!(
            "cannot read {}: {error}",
            app.burn_next_file().display()
        )),
    }
}

/// Where the numbers besides the payload come from: the status line command
/// reads `accounts.json` and `usage-cache.json`; the window preview reads the
/// current snapshot. Both go through [`facts_from_source`].
pub(crate) trait FactsSource {
    fn host_name(&self) -> Option<&str>;
    fn account(&self, account_id: &str) -> Option<AccountFacts>;
    fn usage(&self, account_id: &str, feed: Option<&Feed>) -> Option<Usage>;
    fn model_limits(&self, account_id: &str) -> Vec<(String, f64)>;
    fn burn_next(&self, now: DateTime<Utc>) -> Result<Option<(String, DateTime<Utc>)>, String>;
}

/// The per-model weekly limits of `usage`: each scoped limit's model and
/// percent. The one body both [`FactsSource`]s use for
/// [`FactsSource::model_limits`], so the command and the preview cannot drift
/// apart.
pub(crate) fn model_limits_of(usage: &Usage) -> Vec<(String, f64)> {
    usage
        .weekly_scoped()
        .filter_map(|limit| {
            limit
                .model
                .as_ref()
                .map(|model| (model.clone(), limit.percent))
        })
        .collect()
}

/// The command's source: the files it read without a lock.
struct AccountsFacts<'a> {
    app: &'a AppPaths,
    file: &'a AccountsFile,
    cache: &'a UsageCache,
}

impl FactsSource for AccountsFacts<'_> {
    fn host_name(&self) -> Option<&str> {
        self.file.settings.host_name.as_deref()
    }

    fn account(&self, account_id: &str) -> Option<AccountFacts> {
        let account: &Account = self.file.find(account_id)?;
        Some(AccountFacts {
            abbr: account.abbr.clone(),
            color: account.color.clone(),
            plan: account.plan.clone(),
        })
    }

    fn usage(&self, account_id: &str, feed: Option<&Feed>) -> Option<Usage> {
        let cached = self
            .cache
            .entries
            .get(account_id)
            .and_then(|entry| entry.usage.as_ref())?;
        // The payload's numbers are shown only when they belong to the live
        // account. A fresh session that has not sent a request since a switch
        // carries the old account's numbers; they are never printed under the
        // new account's letters. Without a cache entry there is nothing to
        // compare.
        let belongs_to_live = feed.is_some_and(|feed| {
            feed::account_for(feed, self.cache, &self.file.accounts).as_deref() == Some(account_id)
        });
        Some(match (feed, belongs_to_live) {
            (Some(feed), true) => feed::merged(cached, feed),
            _ => cached.clone(),
        })
    }

    fn model_limits(&self, account_id: &str) -> Vec<(String, f64)> {
        self.cache
            .entries
            .get(account_id)
            .and_then(|entry| entry.usage.as_ref())
            .map(model_limits_of)
            .unwrap_or_default()
    }

    fn burn_next(&self, now: DateTime<Utc>) -> Result<Option<(String, DateTime<Utc>)>, String> {
        // A file the tray app wrote names an account in this same accounts.json;
        // an id that is no longer there is no value, never a guessed
        // abbreviation.
        burn_next_account(self.app, now, |account_id| {
            self.file
                .accounts
                .iter()
                .find(|account| account.id == account_id)
                .map(|account| account.abbr.clone())
        })
    }
}

/// What the line shows besides the payload: who and where, the live account,
/// its limits, and the branch of the session's folder. A live login that
/// matches no stored account shows no account (and no limits from the cache);
/// no live login at all is an error the command prints as its one line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn facts(
    app: &AppPaths,
    payload: &Payload,
    file: &AccountsFile,
    live: &LiveState,
    cache: &UsageCache,
    home: &std::path::Path,
    user: String,
    now: DateTime<Utc>,
) -> Result<Facts, String> {
    let source = AccountsFacts { app, file, cache };
    facts_from_source(&source, payload, live, home, user, now)
}

/// The one builder of [`Facts`]. The command and the preview differ only in
/// their [`FactsSource`].
pub(crate) fn facts_from_source<S: FactsSource>(
    source: &S,
    payload: &Payload,
    live: &LiveState,
    home: &std::path::Path,
    user: String,
    now: DateTime<Utc>,
) -> Result<Facts, String> {
    let host = paths::host_name(source.host_name()).map_err(|error| error.to_string())?;
    // The feed's numbers for the account. A cost that cannot be stored is the
    // feed's own `cost_problem`, so the preview reads the same feed the status
    // line writes instead of dropping the reason.
    let feed = feed_from_payload(payload, now);
    let (account, shown, live_id) = match live {
        LiveState::Known { account_id } => {
            let account = source
                .account(account_id)
                .ok_or_else(|| format!("the live account {account_id} is not in accounts.json"))?;
            let shown = source.usage(account_id, feed.as_ref());
            (Some(account), shown, Some(account_id.as_str()))
        }
        LiveState::Unknown { .. } => (None, None, None),
        LiveState::NoLogin => return Err("no live login; run seatswap add".to_string()),
        LiveState::Ended { .. } => return Err("the live login has ended; log in again".to_string()),
    };
    let five_hour = shown.as_ref().and_then(Usage::session).map(cached_limit);
    let week = shown.as_ref().and_then(Usage::weekly_all).map(cached_limit);
    // The per-model limits only exist in the cache.
    let model_limits = match live_id {
        Some(account_id) => source.model_limits(account_id),
        None => Vec::new(),
    };
    let git_branch = payload
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.current_dir.as_deref())
        .and_then(|folder| git_branch(std::path::Path::new(folder), home));
    let burn_next = source.burn_next(now)?;
    Ok(Facts {
        user,
        host,
        home: home.display().to_string(),
        now,
        account,
        five_hour,
        week,
        model_limits,
        burn_next,
        git_branch,
    })
}

/// One test case of the preview: the live data, or one number changed.
#[cfg(any(feature = "gui", test))]
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Scenario {
    Live,
    NearFull,
    Blocked,
    NoSessionLimit,
}

#[cfg(any(feature = "gui", test))]
impl Scenario {
    /// This scenario's numbers. Each case changes only the one value it names.
    fn facts(self, mut facts: Facts) -> Facts {
        match self {
            Scenario::Live => facts,
            Scenario::NearFull => {
                let resets_at = facts.week.as_ref().and_then(|week| week.resets_at);
                facts.week = Some(Limit2 {
                    percent: 97.0,
                    resets_at,
                });
                facts
            }
            Scenario::Blocked => {
                let resets_at = facts.week.as_ref().and_then(|week| week.resets_at);
                facts.week = Some(Limit2 {
                    percent: 100.0,
                    resets_at,
                });
                facts
            }
            Scenario::NoSessionLimit => {
                facts.five_hour = None;
                facts
            }
        }
    }
}

/// One width of the preview: the fitted spans, the drop numbers removed, and
/// the sentence about them.
#[cfg(any(feature = "gui", test))]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewLine {
    pub width: u16,
    pub spans: Vec<Span>,
    pub dropped: Vec<u8>,
    pub note: String,
}

/// Render `config` (never the stored one) once per width. The preview's session
/// is made up: its folder is the payload's, its branch is [`PREVIEW_BRANCH`] and
/// only the account's numbers come from `facts`. A width of 0 or an empty list
/// is an error: the preview must show a line, not silently skip it.
#[cfg(any(feature = "gui", test))]
pub(crate) fn preview_lines<Tz: TimeZone>(
    config: &StatuslineConfig,
    payload: &Payload,
    facts: &Facts,
    widths: &[u16],
    scenario: Scenario,
    tz: &Tz,
) -> Result<Vec<PreviewLine>, String>
where
    Tz::Offset: std::fmt::Display,
{
    if widths.is_empty() {
        return Err("the preview needs at least one width".to_string());
    }
    if widths.contains(&0) {
        return Err("a preview width must be at least 1 column; 0 cannot show a line".to_string());
    }
    let mut facts = facts.clone();
    facts.git_branch = Some(PREVIEW_BRANCH.to_string());
    let facts = scenario.facts(facts);
    let mut lines = Vec::with_capacity(widths.len());
    for &width in widths {
        let fitted = render::fitted_line(config, payload, &facts, Some(width), tz);
        lines.push(PreviewLine {
            width,
            note: dropped_note(&fitted.dropped_kinds),
            spans: fitted.spans,
            dropped: fitted.dropped,
        });
    }
    Ok(lines)
}

/// What a preview says about the segments the fitting dropped, one label per
/// kind, from the store's one kind table.
#[cfg(any(feature = "gui", test))]
fn dropped_note(dropped: &[SegmentKind]) -> String {
    let mut names: Vec<&'static str> = Vec::new();
    for kind in dropped {
        let name = kind.spec().label;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let Some((last, rest)) = names.split_last() else {
        return "everything fits".to_string();
    };
    let list = if rest.is_empty() {
        (*last).to_string()
    } else {
        format!("{} and {last}", rest.join(", "))
    };
    let is = if names.len() == 1 { "is" } else { "are" };
    format!("{list} {is} dropped")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_of_the_readme_parses() {
        let payload = parse(README_PAYLOAD.as_bytes());
        assert_eq!(
            payload
                .workspace
                .and_then(|workspace| workspace.current_dir),
            Some("/home/sam/projects/atlas".to_string())
        );
        let model = payload.model.expect("the payload has a model");
        assert_eq!(model.id.as_deref(), Some("claude-fable-5-2"));
        assert_eq!(model.display_name.as_deref(), Some("Fable 5.2"));
        assert_eq!(
            payload.effort.and_then(|effort| effort.level),
            Some("high".to_string())
        );
        let rate_limits = payload.rate_limits.expect("the payload has rate limits");
        let five_hour = rate_limits.five_hour.expect("the payload has a 5h limit");
        assert_eq!(five_hour.used_percentage, 12.0);
        assert_eq!(five_hour.resets_at, 1794490200);
        let seven_day = rate_limits.seven_day.expect("the payload has a week limit");
        assert_eq!(seven_day.used_percentage, 48.0);
        assert_eq!(seven_day.resets_at, 1794790800);
    }

    #[test]
    fn garbage_is_an_empty_payload() {
        for stdin in [&b"not json"[..], &b""[..]] {
            let payload = parse(stdin);
            assert!(payload.workspace.is_none());
            assert!(payload.model.is_none());
            assert!(payload.effort.is_none());
            assert!(payload.rate_limits.is_none());
        }
    }

    /// A field of the wrong type is an error for the whole payload: the
    /// fields of `RateLimit` are not optional, so serde cannot make only that
    /// one `None`. The payload of the task itself has a whole number for
    /// `used_percentage`; that is a `f64` of 12.0.
    #[test]
    fn a_payload_that_does_not_parse_gives_the_default_payload() {
        for stdin in [
            &br#"{"workspace":{"current_dir":"/home/sam/projects/atlas"},"model":{"id":"claude-fable-5-2"},"rate_limits":{"five_hour":{"used_percentage":"12","resets_at":1794490200}}}"#[..],
            &br#"{"model":{"id":"claude-fable-5-2"},"rate_limits":{"seven_day":{"used_percentage":48,"resets_at":99999999999999999999}}}"#[..],
            &br#"{"rate_limits":{"five_hour":{"used_percentage":48}}}"#[..],
        ] {
            let payload = parse(stdin);
            assert!(payload.workspace.is_none());
            assert!(payload.model.is_none());
            assert!(payload.effort.is_none());
            assert!(payload.rate_limits.is_none());
        }
    }

    use std::fs;

    use seatswap_core::api::{Limit, LimitKind, Severity};
    use seatswap_core::engine::CacheEntry;
    use seatswap_core::store::{
        ColorRule, LoginState, Segment, SegmentKind, StatuslineConfig, TimeFormat,
    };
    use seatswap_core::types::OAuthAccount;

    const LB_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const AC_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const LB_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const AC_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const ORG: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("an RFC 3339 test time")
            .with_timezone(&Utc)
    }

    fn account(id: &str, abbr: &str, uuid: &str) -> Account {
        Account {
            id: id.to_string(),
            account_uuid: uuid.to_string(),
            organization_uuid: ORG.to_string(),
            label: format!("{abbr} team"),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            org_name: None,
            plan: Some("Team".to_string()),
            oauth_account: OAuthAccount(serde_json::Map::new()),
            added_at: at("2026-11-01T00:00:00Z"),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            auto_burn: true,
            price: None,
            price_currency: None,
        }
    }

    fn usage(session: f64, week: f64, reset: DateTime<Utc>) -> Usage {
        Usage {
            limits: vec![
                Limit {
                    kind: LimitKind::Session,
                    percent: session,
                    severity: Severity::Normal,
                    resets_at: Some(at("2026-11-14T19:30:00.336716Z")),
                    model: None,
                    is_active: true,
                },
                Limit {
                    kind: LimitKind::WeeklyAll,
                    percent: week,
                    severity: Severity::Normal,
                    resets_at: Some(reset),
                    model: None,
                    is_active: false,
                },
            ],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_numbers_of_another_account_are_not_shown() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        let app = AppPaths::at(&data);
        let lb_reset = at("2026-11-16T17:00:00.336716Z");
        let ac_reset = at("2026-11-16T02:00:00.336716Z");
        let mut file = AccountsFile::default();
        file.accounts.push(account(LB_ID, "LB", LB_UUID));
        file.accounts.push(account(AC_ID, "AC", AC_UUID));
        let mut cache = UsageCache::default();
        cache.entries.insert(
            LB_ID.to_string(),
            CacheEntry {
                usage: Some(usage(10.0, 40.0, lb_reset)),
                fetched_at: Some(now),
                ..CacheEntry::default()
            },
        );
        cache.entries.insert(
            AC_ID.to_string(),
            CacheEntry {
                usage: Some(usage(30.0, 70.0, ac_reset)),
                fetched_at: Some(now),
                ..CacheEntry::default()
            },
        );
        // The session still carries AC's numbers although LB is live.
        let payload = parse(
            format!(
                r#"{{"workspace":{{"current_dir":"/home/sam/projects/atlas"}},"model":{{"id":"claude-fable-5-2","display_name":"Fable 5.2"}},"effort":{{"level":"high"}},"rate_limits":{{"five_hour":{{"used_percentage":99,"resets_at":{}}},"seven_day":{{"used_percentage":99,"resets_at":{}}}}}}}"#,
                ac_reset.timestamp(),
                ac_reset.timestamp()
            )
            .as_bytes(),
        );

        let facts = facts(
            &app,
            &payload,
            &file,
            &LiveState::Known {
                account_id: LB_ID.to_string(),
            },
            &cache,
            std::path::Path::new("/home/sam"),
            "sam".to_string(),
            now,
        )
        .expect("the facts of the live account");

        assert_eq!(facts.account.expect("the live account").abbr, "LB");
        assert_eq!(facts.five_hour.expect("LB's session").percent, 10.0);
        assert_eq!(facts.week.expect("LB's week").percent, 40.0);
    }

    /// The per-model limits are the scoped ones, and only they: the command
    /// and the preview ask this one function for them.
    #[test]
    fn the_model_limits_are_the_scoped_ones() {
        let mut usage = usage(10.0, 40.0, at("2026-11-16T02:00:00.336716Z"));
        usage.limits.push(Limit {
            kind: LimitKind::WeeklyScoped,
            percent: 62.0,
            severity: Severity::Normal,
            resets_at: Some(at("2026-11-16T02:00:00.336716Z")),
            model: Some("claude-fable-5-2".to_string()),
            is_active: false,
        });

        assert_eq!(
            model_limits_of(&usage),
            vec![("claude-fable-5-2".to_string(), 62.0)]
        );
    }

    #[test]
    fn a_payload_without_a_week_is_not_a_feed() {
        let payload = parse(
            br#"{"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1794490200}}}"#,
        );

        assert!(
            feed_from_payload(&payload, at("2026-11-14T14:00:00.336716Z")).is_none(),
            "a feed without a week cannot be placed on an account"
        );
    }

    #[test]
    fn the_feed_carries_the_session_and_its_cost() {
        let payload = parse(
            br#"{"session_id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","model":{"id":"claude-fable-5-2"},"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1794490200},"seven_day":{"used_percentage":48,"resets_at":1794790800}},"cost":{"total_cost_usd":1.47}}"#,
        );

        let feed = feed_from_payload(&payload, at("2026-11-14T14:00:00.336716Z")).expect("a feed");
        assert_eq!(
            feed.session.as_deref(),
            Some("7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34")
        );
        assert_eq!(feed.cost_usd, Some(1.47));
        assert_eq!(feed.cost_problem, None);
    }

    /// A cost that cannot be stored still writes the feed, with its reason, so
    /// the tray app and the status line's preview can say it (item 6).
    #[test]
    fn a_cost_problem_is_written_to_the_feed() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let payload = parse(
            br#"{"session_id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","rate_limits":{"seven_day":{"used_percentage":48,"resets_at":1794790800}},"cost":{"total_cost_usd":-0.5}}"#,
        );

        publish_feed(&app, &Settings::default(), &payload, now)
            .expect("a bad cost does not cost the status line");
        let stored = feed::read(&app)
            .expect("the feed file")
            .expect("the feed is written");
        assert_eq!(stored.cost_usd, None);
        let problem = stored.cost_problem.expect("the reason is written");
        assert!(
            problem.contains("7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34"),
            "{problem}"
        );
    }

    #[test]
    fn a_cost_that_is_negative_or_not_a_number_is_a_cost_problem() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let negative = parse(
            br#"{"session_id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","rate_limits":{"seven_day":{"used_percentage":48,"resets_at":1794790800}},"cost":{"total_cost_usd":-0.5}}"#,
        );
        let feed = feed_from_payload(&negative, now).expect("the feed is still written");
        assert_eq!(feed.cost_usd, None, "a negative cost is never stored");
        let problem = feed.cost_problem.expect("a cost problem says why");
        assert!(problem.contains("total_cost_usd"), "{problem}");
        assert!(
            problem.contains("7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34"),
            "the problem names the session: {problem}"
        );

        let text = parse(
            br#"{"rate_limits":{"seven_day":{"used_percentage":48,"resets_at":1794790800}},"cost":{"total_cost_usd":"1.47"}}"#,
        );
        let feed = feed_from_payload(&text, now).expect("the feed is still written");
        assert_eq!(feed.cost_usd, None);
        let problem = feed.cost_problem.expect("a cost problem says why");
        assert!(problem.contains("total_cost_usd"), "{problem}");
        assert!(problem.contains("not a number"), "{problem}");
    }

    /// A cost that is not there is no cost, not an error: the status line's own
    /// segment reads a null cost as "nothing to print" (`render.rs`'s
    /// `number_at`), and the feed must not turn that shape into a failed status
    /// line.
    #[test]
    fn a_null_cost_is_no_cost_and_not_an_error() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let payload = parse(
            br#"{"rate_limits":{"seven_day":{"used_percentage":48,"resets_at":1794790800}},"cost":{"total_cost_usd":null}}"#,
        );

        let feed = feed_from_payload(&payload, now).expect("a null cost is no cost, not an error");
        assert_eq!(feed.cost_usd, None);
    }

    #[test]
    fn the_live_accounts_feed_is_shown_merged() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        let app = AppPaths::at(&data);
        let ac_reset = at("2026-11-16T02:00:00.336716Z");
        let session_reset = at("2026-11-14T19:30:00.336716Z");
        let mut file = AccountsFile::default();
        file.accounts.push(account(AC_ID, "AC", AC_UUID));
        let mut cache = UsageCache::default();
        cache.entries.insert(
            AC_ID.to_string(),
            CacheEntry {
                usage: Some(usage(10.0, 40.0, ac_reset)),
                fetched_at: Some(now),
                ..CacheEntry::default()
            },
        );
        let payload = parse(
            format!(
                r#"{{"workspace":{{"current_dir":"/home/sam/projects/atlas"}},"model":{{"id":"claude-fable-5-2","display_name":"Fable 5.2"}},"effort":{{"level":"high"}},"rate_limits":{{"five_hour":{{"used_percentage":12,"resets_at":{}}},"seven_day":{{"used_percentage":48,"resets_at":{}}}}}}}"#,
                session_reset.timestamp(),
                ac_reset.timestamp()
            )
            .as_bytes(),
        );

        let facts = facts(
            &app,
            &payload,
            &file,
            &LiveState::Known {
                account_id: AC_ID.to_string(),
            },
            &cache,
            std::path::Path::new("/home/sam"),
            "sam".to_string(),
            now,
        )
        .expect("the facts of the live account");

        assert_eq!(facts.week.as_ref().expect("AC's week").percent, 48.0);
        let line = render::ansi(
            &render::render_spans(
                &StatuslineConfig::current_line(),
                &payload,
                &facts,
                None,
                &Utc,
            )
            .0,
        );
        assert!(line.contains("AC"), "{line}");
        assert!(
            line.contains("48%"),
            "the newer week is on the line: {line}"
        );
        assert!(!line.contains("40%"), "the cached week is replaced: {line}");
    }

    #[test]
    fn the_auto_burn_segment_shows_the_next_account() {
        let now = at("2026-11-14T14:00:00Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        let app = AppPaths::at(&data);
        let mut file = AccountsFile::default();
        file.accounts.push(account(AC_ID, "AC", AC_UUID));
        let switch_at = at("2026-11-14T16:05:00Z");
        burn_next::write(
            &app,
            &burn_next::BurnNext {
                account: AC_ID.to_string(),
                at: switch_at,
                written: now,
            },
        )
        .expect("burn-next.json");

        let payload = parse(README_PAYLOAD.as_bytes());
        let facts = facts(
            &app,
            &payload,
            &file,
            &LiveState::Known {
                account_id: AC_ID.to_string(),
            },
            &UsageCache::default(),
            std::path::Path::new("/home/sam"),
            "sam".to_string(),
            now,
        )
        .expect("the facts of the live account");
        assert_eq!(facts.burn_next, Some(("AC".to_string(), switch_at)));

        let config = StatuslineConfig {
            left: Vec::new(),
            right: vec![Segment {
                kind: SegmentKind::AutoBurn,
                color: ColorRule::None,
                prefix: String::new(),
                suffix: String::new(),
                drop: None,
                time_format: Some(TimeFormat::Seatswap),
                text: None,
                separator: None,
            }],
            margin: 0,
        };
        assert_eq!(
            render::ansi(&render::render_spans(&config, &payload, &facts, None, &Utc).0),
            "AC 2h 5min"
        );
    }

    #[test]
    fn an_old_next_file_shows_nothing() {
        let now = at("2026-11-14T14:00:00Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        let app = AppPaths::at(&data);
        let mut file = AccountsFile::default();
        file.accounts.push(account(AC_ID, "AC", AC_UUID));
        burn_next::write(
            &app,
            &burn_next::BurnNext {
                account: AC_ID.to_string(),
                at: now + chrono::Duration::hours(2),
                written: now - chrono::Duration::minutes(21),
            },
        )
        .expect("burn-next.json");

        let payload = parse(README_PAYLOAD.as_bytes());
        let facts = facts(
            &app,
            &payload,
            &file,
            &LiveState::Known {
                account_id: AC_ID.to_string(),
            },
            &UsageCache::default(),
            std::path::Path::new("/home/sam"),
            "sam".to_string(),
            now,
        )
        .expect("the facts of the live account");
        assert_eq!(facts.burn_next, None, "the tray app is not running");
    }

    #[test]
    fn no_feed_file_while_the_setting_is_off() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let settings = Settings {
            statusline_feed: false,
            ..Settings::default()
        };

        publish_feed(
            &app,
            &settings,
            &parse(README_PAYLOAD.as_bytes()),
            at("2026-11-14T14:00:00.336716Z"),
        )
        .expect("the setting simply writes nothing");

        assert!(!app.feed_file().exists(), "no feed file was written");
    }

    #[test]
    fn a_feed_that_cannot_be_written_is_the_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        fs::create_dir_all(&data).expect("the data folder");
        fs::write(
            data.join("accounts.json"),
            serde_json::to_vec(&AccountsFile::default()).expect("accounts.json"),
        )
        .expect("the account file");
        // A folder where feed.json should be: the write must be reported, not
        // skipped and not hidden behind a rendered line.
        fs::create_dir_all(data.join("feed.json")).expect("the blocking folder");
        let app = AppPaths::at(&data);
        let claude =
            seatswap_core::paths::ClaudePaths::for_config_dir(&tmp.path().join("cfg"), tmp.path());

        let line = crate::cli::tests::statusline_output_at(
            &app,
            &claude,
            README_PAYLOAD.as_bytes(),
            at("2026-11-14T14:00:00.336716Z"),
        );

        assert!(line.starts_with("seatswap: feed: "), "{line}");
    }

    /// The paths and kinds as a comparable list.
    fn pairs(keys: &[KeyInfo]) -> Vec<(&str, &str)> {
        keys.iter()
            .map(|key| (key.path.as_str(), key.kind.as_str()))
            .collect()
    }

    #[test]
    fn key_paths_list_paths_and_kinds_only() {
        let raw: serde_json::Value = serde_json::from_str(
            r#"{"model":{"id":"claude-fable-5-2"},"rate_limits":{"five_hour":{"used_percentage":12}},"tags":["a"]}"#,
        )
        .expect("the test payload");
        let keys = key_paths(&raw);

        assert_eq!(
            pairs(&keys),
            vec![
                ("model", "object"),
                ("model.id", "string"),
                ("rate_limits", "object"),
                ("rate_limits.five_hour", "object"),
                ("rate_limits.five_hour.used_percentage", "number"),
                ("tags", "array"),
                ("tags[]", "string"),
            ]
        );
        let text = serde_json::to_string(&keys).expect("the keys as JSON");
        assert!(
            !text.contains("claude-fable-5-2"),
            "no value is kept: {text}"
        );
    }

    #[test]
    fn recording_stops_at_its_time() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let settings = Settings {
            statusline_record_until: Some(now - chrono::Duration::seconds(1)),
            ..Settings::default()
        };
        let payload = parse(README_PAYLOAD.as_bytes());

        record_keys(&app, &settings, &payload, now)
            .expect("outside a recording nothing is written");

        assert!(!app.statusline_keys_file().exists(), "no keys file");
    }

    #[test]
    fn recording_adds_to_what_is_there() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let settings = Settings {
            statusline_record_until: Some(now + chrono::Duration::minutes(1)),
            ..Settings::default()
        };
        let first = parse(br#"{"model":{"id":"claude-fable-5-2"}}"#);
        let second = parse(br#"{"effort":{"level":"high"}}"#);

        record_keys(&app, &settings, &first, now).expect("the first payload is recorded");
        record_keys(&app, &settings, &second, now + chrono::Duration::seconds(2))
            .expect("the second payload is added");

        let recorded = read_recorded_keys(&app)
            .expect("the keys file is read")
            .expect("a recording exists");
        assert_eq!(
            pairs(&recorded.keys),
            vec![
                ("effort", "object"),
                ("effort.level", "string"),
                ("model", "object"),
                ("model.id", "string"),
            ]
        );
        assert_eq!(recorded.at, now + chrono::Duration::seconds(2));
    }

    #[test]
    fn an_array_of_objects_lists_its_keys() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"items":[{"a":1},{"b":"x"}]}"#).expect("the test payload");

        assert_eq!(
            pairs(&key_paths(&raw)),
            vec![
                ("items", "array"),
                ("items[]", "object"),
                ("items[].a", "number"),
                ("items[].b", "string"),
            ]
        );
    }

    #[test]
    fn a_keys_file_that_does_not_parse_is_not_overwritten() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        fs::create_dir_all(&data).expect("the data folder");
        let app = AppPaths::at(&data);
        let now = at("2026-11-14T14:00:00.336716Z");
        let mut file = AccountsFile::default();
        file.settings.statusline_record_until = Some(now + chrono::Duration::minutes(1));
        // The feed is not part of this test.
        file.settings.statusline_feed = false;
        fs::write(
            data.join("accounts.json"),
            serde_json::to_vec(&file).expect("accounts.json"),
        )
        .expect("the accounts file");
        let broken = b"not a recording";
        fs::write(app.statusline_keys_file(), broken).expect("the broken keys file");
        let claude =
            seatswap_core::paths::ClaudePaths::for_config_dir(&tmp.path().join("cfg"), tmp.path());

        let line =
            crate::cli::tests::statusline_output_at(&app, &claude, README_PAYLOAD.as_bytes(), now);

        assert!(
            !line.starts_with("seatswap: keys: "),
            "the broken file does not cost the line: {line}"
        );
        assert_eq!(
            fs::read(app.statusline_keys_file()).expect("the keys file is still there"),
            broken,
            "the broken file is unchanged"
        );
    }

    #[test]
    fn record_minutes_out_of_range_is_refused() {
        let now = at("2026-11-14T14:00:00.336716Z");
        for minutes in [0u32, 61] {
            let error = record_until(minutes, now).expect_err("out of range");
            assert!(
                error.contains("1 to 60") && error.contains(&format!("got {minutes}")),
                "{error}"
            );
        }
        assert_eq!(
            record_until(1, now).expect("one minute"),
            now + chrono::Duration::minutes(1)
        );
    }

    #[test]
    fn the_preview_shows_the_optional_segments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let payload = preview_payload(home);
        let facts = render::Facts {
            user: "sam".to_string(),
            host: "atlas".to_string(),
            home: home.display().to_string(),
            now: at("2026-11-14T14:00:00.336716Z"),
            account: Some(render::AccountFacts {
                abbr: "AC".to_string(),
                color: "#3b82f6".to_string(),
                plan: None,
            }),
            five_hour: Some(render::Limit2 {
                percent: 12.0,
                resets_at: Some(at("2026-11-14T19:30:00.336716Z")),
            }),
            week: Some(render::Limit2 {
                percent: 48.0,
                resets_at: Some(at("2026-11-16T02:00:00.336716Z")),
            }),
            model_limits: Vec::new(),
            burn_next: None,
            git_branch: None,
        };
        let part = |kind| Segment {
            kind,
            color: ColorRule::None,
            prefix: " ".to_string(),
            suffix: String::new(),
            drop: None,
            time_format: None,
            text: None,
            separator: None,
        };
        let mut folder = part(SegmentKind::Folder);
        folder.prefix = String::new();
        let config = StatuslineConfig {
            left: vec![
                part(SegmentKind::ContextUsed),
                part(SegmentKind::SessionCost),
                part(SegmentKind::LinesChanged),
                part(SegmentKind::GitBranch),
                folder,
            ],
            right: Vec::new(),
            margin: 0,
        };

        let preview = preview_lines(&config, &payload, &facts, &[120], Scenario::Live, &Utc)
            .expect("a preview at 120 columns");
        let text: String = preview[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect();

        assert!(text.contains("41%"), "{text}");
        assert!(text.contains("$3.20"), "{text}");
        assert!(text.contains("+120 -8"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(
            text.contains(&crate::test_support::tilde("projects/atlas")),
            "{text}"
        );
        assert_eq!(preview[0].note, "everything fits");
    }

    /// The preview's note calls a dropped kind by the table's label, so the page
    /// and the note use the same word (`git branch`, not `branch`).
    #[test]
    fn the_preview_note_names_a_dropped_git_branch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let payload = preview_payload(home);
        let facts = render::Facts {
            user: "sam".to_string(),
            host: "atlas".to_string(),
            home: home.display().to_string(),
            now: at("2026-11-14T14:00:00.336716Z"),
            account: None,
            five_hour: None,
            week: None,
            model_limits: Vec::new(),
            burn_next: None,
            git_branch: None,
        };
        let config = StatuslineConfig {
            left: vec![Segment {
                kind: SegmentKind::GitBranch,
                color: ColorRule::None,
                prefix: String::new(),
                suffix: String::new(),
                drop: Some(1),
                time_format: None,
                text: None,
                separator: None,
            }],
            right: Vec::new(),
            margin: 0,
        };

        let preview = preview_lines(&config, &payload, &facts, &[1], Scenario::Live, &Utc)
            .expect("a preview at one column");

        assert_eq!(preview[0].note, "git branch is dropped");
    }

    /// The browser's table is the one `statusline_kinds` sends: every kind, in
    /// the same order, with the same labels, groups and options.
    #[test]
    fn the_statusline_fixture_has_the_kinds_table() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/fixture/statusline.js");
        let source = fs::read_to_string(path).expect("the page fixture");
        let start = source
            .find("const FIXTURE_KINDS =")
            .expect("the fixture has a kinds table");
        let rest = &source[start..];
        let open = rest.find('[').expect("the table starts with a bracket");
        let close = rest.find("\n];").expect("the table ends on its own line");
        let table = &rest[open..close + 2];
        let fixture: serde_json::Value =
            serde_json::from_str(table).expect("the fixture table is JSON-shaped");
        let expected: Vec<serde_json::Value> = SegmentKind::ALL
            .iter()
            .map(|kind| serde_json::to_value(kind.spec()).expect("a kind spec as JSON"))
            .collect();
        assert_eq!(fixture, serde_json::Value::Array(expected));
    }

    // ---- proof tests of the review ----

    /// The recorder's file holds the names of the fields and their kinds only.
    /// No value of the payload (a folder, a model id, a session id, a
    /// transcript path) is in it.
    #[test]
    fn the_recorded_file_holds_no_value_of_the_payload() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let settings = Settings {
            statusline_record_until: Some(now + chrono::Duration::minutes(1)),
            ..Settings::default()
        };
        let payload = parse(
            br#"{"session_id":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","transcript_path":"/home/sam/.claude/projects/atlas/7c1e4b2a.jsonl","cwd":"/home/sam/projects/atlas","model":{"id":"claude-fable-5-2","display_name":"Fable 5.2"},"workspace":{"current_dir":"/home/sam/projects/atlas","added_dirs":["/home/sam/other"]}}"#,
        );

        record_keys(&app, &settings, &payload, now).expect("the payload is recorded");

        let text = fs::read_to_string(app.statusline_keys_file()).expect("the keys file");
        for value in [
            "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34",
            "/home/sam/.claude/projects/atlas/7c1e4b2a.jsonl",
            "/home/sam/projects/atlas",
            "claude-fable-5-2",
            "Fable 5.2",
            "/home/sam/other",
        ] {
            assert!(
                !text.contains(value),
                "the value {value} is in the file: {text}"
            );
        }
        // The names are there, so the test is not vacuous.
        for path in [
            "session_id",
            "transcript_path",
            "cwd",
            "model.id",
            "workspace.added_dirs[]",
        ] {
            assert!(text.contains(path), "the path {path} is missing: {text}");
        }
    }

    /// A data folder that is not there yet: the recording makes it.
    #[test]
    fn a_recording_works_in_a_data_folder_that_does_not_exist_yet() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        let settings = Settings {
            statusline_record_until: Some(now + chrono::Duration::minutes(1)),
            ..Settings::default()
        };

        assert!(!app.data_dir.exists(), "the data folder is not there");
        record_keys(&app, &settings, &parse(README_PAYLOAD.as_bytes()), now)
            .expect("the payload is recorded");

        assert!(
            app.statusline_keys_file().is_file(),
            "the keys file is there"
        );
        let recorded = read_recorded_keys(&app)
            .expect("the keys file is read")
            .expect("a recording exists");
        assert!(!recorded.keys.is_empty(), "the paths are recorded");
    }

    /// An accounts.json written before the setting existed, and one with the
    /// setting set to `null`: neither records anything.
    #[test]
    fn an_older_accounts_file_and_a_null_setting_record_nothing() {
        let now = at("2026-11-14T14:00:00.336716Z");
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));

        let older: Settings =
            serde_json::from_value(serde_json::json!({})).expect("an older settings file");
        let null: Settings = serde_json::from_value(serde_json::json!({
            "statuslineRecordUntil": null
        }))
        .expect("a settings file with a null setting");
        for settings in [older, null] {
            assert!(
                settings.statusline_record_until.is_none(),
                "no recording is set"
            );
            record_keys(&app, &settings, &parse(README_PAYLOAD.as_bytes()), now)
                .expect("nothing is recorded");
            assert!(
                !app.statusline_keys_file().exists(),
                "no keys file is written"
            );
        }
    }

    /// A recording lasts one minute or an hour.
    #[test]
    fn a_recording_lasts_one_and_sixty_minutes() {
        let now = at("2026-11-14T14:00:00.336716Z");
        assert_eq!(
            record_until(1, now).expect("one minute"),
            now + chrono::Duration::minutes(1)
        );
        assert_eq!(
            record_until(60, now).expect("an hour"),
            now + chrono::Duration::minutes(60)
        );
    }

    /// A key that is user text, a key with a dot in it and a key without a
    /// name: the recorder writes the key as the path, so a dot in a key and a
    /// nested key of the same name cannot be told apart, and a key without a
    /// name has no path of its own.
    #[test]
    fn a_key_is_written_as_the_path_it_has() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"a.b":1,"":{"c":2},"my secret is hunter2":3}"#)
                .expect("the test payload");

        assert_eq!(
            pairs(&key_paths(&raw)),
            vec![
                ("", "object"),
                ("a.b", "number"),
                ("c", "number"),
                ("my secret is hunter2", "number"),
            ]
        );
    }

    /// Arrays in arrays: every level adds its own `[]`, and the kinds of the
    /// elements of one level are one entry each.
    #[test]
    fn an_array_of_arrays_lists_its_paths() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"a":[[1,"x"],[{"b":2}]]}"#).expect("the test payload");

        assert_eq!(
            pairs(&key_paths(&raw)),
            vec![
                ("a", "array"),
                ("a[]", "array"),
                ("a[][]", "number"),
                ("a[][]", "object"),
                ("a[][]", "string"),
                ("a[][].b", "number"),
            ]
        );
    }

    /// A setting that is not a time is refused, never silently replaced: the
    /// settings path reads the whole object again after the change. The
    /// settings page's writer names the field in front of the reason
    /// (`statuslineRecordUntil: …`).
    #[test]
    fn a_setting_that_is_not_a_time_is_refused() {
        let error = serde_json::from_value::<Settings>(
            serde_json::json!({"statuslineRecordUntil": "noon"}),
        )
        .expect_err("a setting that is not a time");
        assert!(
            !error.to_string().is_empty(),
            "the error says that the value is not a time"
        );
    }
}
