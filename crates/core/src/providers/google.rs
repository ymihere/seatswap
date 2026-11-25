//! Google's usage, as the Antigravity CLI (`agy`) hands it to its status line
//! command (spec §7.9). seatswap asks Google nothing and reads no Google login:
//! `seatswap statusline` gets the payload on standard input and keeps the
//! account's numbers in `<data>/google.json`. The pop-up's card shows them, and
//! the Gemini limits are recorded in the history like the other providers'
//! readings ([`GoogleReading::recorded`]).
//!
//! Before the first reading, [`agy_link`] tells whether `agy` is installed and
//! whether its status line already runs seatswap; [`connect_agy`] writes that
//! one key when the person clicks Connect on the card.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::api::{mark_highest_active, Limit, LimitKind, Severity, Usage};
use crate::claude_settings;
use crate::feed;
use crate::history::{same_reset, AccountRef};
use crate::paths::{AgyPaths, AppPaths};

use super::{ProviderError, ProviderReading, GOOGLE_ORG};

/// The payload's `product` when `agy` runs the status line command.
const PRODUCT: &str = "antigravity";

/// One quota bucket of the payload, under its own id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleBucket {
    pub id: String,
    /// The percent used; `None` when `problem` says why there is none.
    pub percent: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// What `agy` last said about the account it is signed in to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFeed {
    pub at: DateTime<Utc>,
    pub email: String,
    pub plan: Option<String>,
    pub buckets: Vec<GoogleBucket>,
}

impl GoogleBucket {
    /// Whether the two say the same. An unused limit's reset moves with the
    /// clock, so it says nothing while nothing is used.
    fn same(&self, other: &GoogleBucket) -> bool {
        self.id == other.id
            && self.percent == other.percent
            && self.problem == other.problem
            && (self.resets_at == other.resets_at || self.percent == Some(0.0))
    }

    /// Whether `said`, read at `at`, is an older number of the limit than this
    /// stored one: less use of the same window, no use while a used window
    /// still runs, or a window that ends earlier with no more use or that had
    /// already ended. An open `agy` session keeps saying the numbers of its own
    /// last answer while another one works on.
    fn outdates(&self, said: &GoogleBucket, at: DateTime<Utc>) -> bool {
        let (Some(used), Some(said_used)) = (self.percent, said.percent) else {
            return false;
        };
        let (Some(reset), Some(said_reset)) = (self.resets_at, said.resets_at) else {
            return false;
        };
        if same_reset(reset, said_reset) {
            return said_used < used;
        }
        if said_used == 0.0 && used > 0.0 && reset > at {
            return true;
        }
        said_reset < reset && (said_used <= used || said_reset <= at)
    }
}

impl GoogleFeed {
    /// Keep the numbers of `stored` that this feed only has older ones of.
    /// Another account's or another plan's numbers are never compared.
    fn keep_newer(&mut self, stored: &GoogleFeed) {
        if self.email != stored.email || self.plan != stored.plan {
            return;
        }
        let at = self.at;
        for bucket in &mut self.buckets {
            let kept = stored
                .buckets
                .iter()
                .find(|kept| kept.id == bucket.id && kept.outdates(bucket, at));
            if let Some(kept) = kept {
                *bucket = kept.clone();
            }
        }
    }

    /// Whether the two say the same of the same account.
    fn same(&self, other: &GoogleFeed) -> bool {
        self.email == other.email
            && self.plan == other.plan
            && self.buckets.len() == other.buckets.len()
            && self
                .buckets
                .iter()
                .zip(&other.buckets)
                .all(|(one, two)| one.same(two))
    }
}

/// A stored feed as a reading: when `agy` said it, and what of it is not a
/// limit row (a window that ended since, a bucket that could not be read).
#[derive(Debug, Clone, PartialEq)]
pub struct GoogleReading {
    pub reading: ProviderReading,
    /// What the history records: the account's Gemini session and week as they
    /// were at `at`, whatever has ended since. A limit nobody has used has no
    /// window that could end, so it has no reset here. No usage without either.
    pub recorded: ProviderReading,
    pub at: DateTime<Utc>,
    pub notes: Vec<String>,
}

/// Whether a status line payload comes from `agy`.
pub fn is_payload(raw: &Value) -> bool {
    raw.get("product").and_then(Value::as_str) == Some(PRODUCT)
}

/// The account's numbers in `agy`'s payload at `now`. `None` without an email
/// or without quota (signed out, an API key, before the first answer): there is
/// no account whose usage this would be, and the last reading stays.
pub fn feed_from_payload(raw: &Value, now: DateTime<Utc>) -> Option<GoogleFeed> {
    let email = non_empty(raw.get("email"))?;
    let quota = raw.get("quota").and_then(Value::as_object)?;
    if quota.is_empty() {
        return None;
    }
    let mut buckets: Vec<GoogleBucket> =
        quota.iter().map(|(id, value)| bucket(id, value)).collect();
    // One order whatever order the payload has, so an unchanged quota is an
    // unchanged feed.
    buckets.sort_by(|a, b| a.id.cmp(&b.id));
    Some(GoogleFeed {
        at: now,
        email,
        plan: non_empty(raw.get("plan_tier")),
        buckets,
    })
}

/// Write `<data>/google.json` under the feed's own rule, without going back to
/// an older number. Returns whether it wrote.
pub fn write_if_due(app: &AppPaths, feed: &GoogleFeed) -> io::Result<bool> {
    let path = app.google_file();
    feed::write_file_if_due(app, &path, feed.clone(), feed.at, |stored, feed| {
        feed.keep_newer(stored);
        (stored.at, stored.same(feed))
    })
}

/// The last reading `agy` left, at `now`; `None` when there is no account yet.
pub fn read(app: &AppPaths, now: DateTime<Utc>) -> Result<Option<GoogleReading>, ProviderError> {
    let path = app.google_file();
    match feed::read_file::<GoogleFeed>(&path) {
        Ok(feed) => Ok(feed.map(|feed| reading(feed, now))),
        Err(source) => Err(ProviderError::Io { path, source }),
    }
}

/// Where `agy` is and the program its status line must run: the console
/// `seatswap` program beside the running one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agy {
    pub paths: AgyPaths,
    pub program: PathBuf,
}

/// How `agy` stands before seatswap has a reading from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgyLink {
    /// `agy`'s folder does not exist.
    NotInstalled,
    /// Installed, with no `statusLine`: Connect can add seatswap's.
    NotConnected,
    /// The status line is seatswap's for this program.
    Connected,
    /// The status line runs something else (the command, or the whole value when
    /// it has no command text); seatswap never replaces it.
    Other(String),
}

/// The `statusLine` value seatswap writes in `agy`'s settings: its command for
/// `program`, shown under `agy`'s own status line.
pub fn agy_status_line(program: &Path) -> Result<Value, ProviderError> {
    Ok(json!({
        "type": "command",
        "command": claude_settings::status_line_command(program)?,
        "stack_with_default": true,
    }))
}

/// How `agy` stands: [`AgyLink`]. A settings file that cannot be read is an
/// error with its path.
pub fn agy_link(agy: &Agy) -> Result<AgyLink, ProviderError> {
    if !agy.paths.dir.is_dir() {
        return Ok(AgyLink::NotInstalled);
    }
    let Some(current) = claude_settings::status_line_in(&agy.paths.settings_file())? else {
        return Ok(AgyLink::NotConnected);
    };
    let ours = agy_status_line(&agy.program)?;
    if current.get("command") == ours.get("command") {
        return Ok(AgyLink::Connected);
    }
    let command = match current.get("command").and_then(Value::as_str) {
        Some(command) => command.to_string(),
        None => current.to_string(),
    };
    Ok(AgyLink::Other(command))
}

/// Connect `agy`: write seatswap's `statusLine` into its settings (backed up
/// once, every other key kept in its place). Already connected writes nothing.
/// Not installed, or a status line that runs another command, is an error;
/// another command is never replaced.
pub fn connect_agy(agy: &Agy) -> Result<(), ProviderError> {
    match agy_link(agy)? {
        AgyLink::Connected => Ok(()),
        AgyLink::NotInstalled => Err(ProviderError::NoAgy {
            dir: agy.paths.dir.clone(),
        }),
        AgyLink::Other(command) => Err(ProviderError::AgyStatusLineTaken {
            command,
            path: agy.paths.settings_file(),
        }),
        AgyLink::NotConnected => {
            let ours = agy_status_line(&agy.program)?;
            claude_settings::set_status_line_in(&agy.paths.settings_file(), Some(&ours))?;
            Ok(())
        }
    }
}

/// The feed as a reading at `now`, and as what the history records of it.
pub fn reading(feed: GoogleFeed, now: DateTime<Utc>) -> GoogleReading {
    let (limits, notes) = limits_at(&feed.buckets, now);
    let (mut recorded, _) = limits_at(&feed.buckets, feed.at);
    recorded.retain(|limit| matches!(limit.kind, LimitKind::Session | LimitKind::WeeklyAll));
    for limit in recorded.iter_mut().filter(|limit| limit.percent == 0.0) {
        limit.resets_at = None;
    }
    let recorded = (!recorded.is_empty()).then(|| usage_of(recorded));
    let of = |usage: Option<Usage>| ProviderReading {
        account: AccountRef {
            acc: feed.email.clone(),
            org: GOOGLE_ORG.to_string(),
        },
        plan: feed.plan.clone(),
        usage,
        credits: None,
        spend_24h: None,
        hours: None,
    };
    GoogleReading {
        reading: of(Some(usage_of(limits))),
        recorded: of(recorded),
        at: feed.at,
        notes,
    }
}

fn usage_of(mut limits: Vec<Limit>) -> Usage {
    mark_highest_active(&mut limits);
    Usage {
        limits,
        extra_usage_enabled: false,
        warnings: Vec::new(),
    }
}

/// The buckets as limits at `now`. A window whose reset has passed holds no
/// usage any more and nobody has said what the next one holds, so it is a note,
/// not a row.
fn limits_at(buckets: &[GoogleBucket], now: DateTime<Utc>) -> (Vec<Limit>, Vec<String>) {
    let mut limits = Vec::new();
    let mut notes = Vec::new();
    for bucket in buckets {
        let kind = limit_of(&bucket.id);
        let Some(percent) = bucket.percent else {
            let reason = bucket.problem.as_deref().unwrap_or("no percent");
            notes.push(format!("{} not read: {reason}", bucket.id));
            continue;
        };
        if bucket.resets_at.is_some_and(|reset| reset <= now) {
            notes.push(format!(
                "{} was reset since the last reading",
                limit_name(&kind)
            ));
            continue;
        }
        limits.push(Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at: bucket.resets_at,
            model: None,
            is_active: false,
        });
    }
    (limits, notes)
}

/// A limit's name in a note and in the status line.
pub fn limit_name(kind: &LimitKind) -> &str {
    match kind {
        LimitKind::Session => "5h",
        LimitKind::WeeklyAll | LimitKind::WeeklyScoped => "week",
        LimitKind::Other(name) => name,
    }
}

/// The limit a bucket id names. Google's Gemini models share one 5h and one
/// weekly limit, which are the account's session and week; the Claude and GPT
/// models share another pair, named window first because the pop-up cuts a long
/// name. An id seatswap does not know keeps its own name.
fn limit_of(id: &str) -> LimitKind {
    match id {
        "gemini-5h" => LimitKind::Session,
        "gemini-weekly" => LimitKind::WeeklyAll,
        "3p-5h" => LimitKind::Other("5h Claude/GPT".to_string()),
        "3p-weekly" => LimitKind::Other("week Claude/GPT".to_string()),
        _ => LimitKind::Other(id.to_string()),
    }
}

fn bucket(id: &str, value: &Value) -> GoogleBucket {
    let (percent, resets_at, problem) = match bucket_values(value) {
        Ok((percent, resets_at)) => (Some(percent), resets_at, None),
        Err(problem) => (None, None, Some(problem)),
    };
    GoogleBucket {
        id: id.to_string(),
        percent,
        resets_at,
        problem,
    }
}

fn bucket_values(value: &Value) -> Result<(f64, Option<DateTime<Utc>>), String> {
    let object = value.as_object().ok_or("it is not an object")?;
    if object.get("disabled") == Some(&Value::Bool(true)) {
        return Err("it is disabled".to_string());
    }
    let left = present(object, "remaining_fraction")
        .ok_or("it has no remaining_fraction")?
        .as_f64()
        .filter(|left| (0.0..=1.0).contains(left))
        .ok_or("its remaining_fraction is not a number from 0 to 1")?;
    let resets_at = match present(object, "reset_time") {
        None => None,
        Some(time) => Some(
            time.as_str()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .ok_or("its reset_time is not a date")?
                .with_timezone(&Utc),
        ),
    };
    Ok(((1.0 - left) * 100.0, resets_at))
}

fn present<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).filter(|value| !value.is_null())
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    /// The documented payload (antigravity.google/docs/cli/statusline), with the
    /// four buckets a signed-in account can have.
    const PAYLOAD: &str = r#"{"cwd":"/home/sam/projects/atlas","session_id":"4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30","model":{"id":"Gemini 4 Argon (Medium)","display_name":"Gemini 4 Argon (Medium)","effort":"medium"},"version":"1.5.17","product":"antigravity","agent_state":"idle","plan_tier":"Pro","email":"sam@mail.example","quota":{"gemini-weekly":{"remaining_fraction":0.75,"reset_time":"2026-11-30T07:50:32Z","reset_in_seconds":431432},"gemini-5h":{"remaining_fraction":0.5,"reset_time":"2026-11-25T14:10:00Z","reset_in_seconds":7800},"3p-weekly":{"remaining_fraction":1,"reset_time":"2026-12-02T12:00:00Z"},"3p-5h":{"remaining_fraction":0.25,"reset_time":"2026-11-25T15:00:00Z"}}}"#;

    fn now() -> DateTime<Utc> {
        "2026-11-25T12:00:00Z".parse().unwrap()
    }

    /// An installed `agy` with the given settings file (none for `None`), and
    /// a program path with a space, so the quoting is part of every test.
    fn agy_with(tmp: &tempfile::TempDir, settings: Option<&str>) -> Agy {
        let dir = tmp.path().join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&dir).expect("agy's folder");
        if let Some(text) = settings {
            std::fs::write(dir.join("settings.json"), text).expect("agy's settings");
        }
        Agy {
            paths: AgyPaths::at(&dir),
            program: tmp.path().join("my apps").join("seatswap"),
        }
    }

    fn settings_of(agy: &Agy) -> Value {
        let text = std::fs::read_to_string(agy.paths.settings_file()).expect("the settings");
        serde_json::from_str(&text).expect("JSON")
    }

    #[test]
    fn agy_is_not_installed_without_its_folder() {
        let tmp = tempdir().expect("tempdir");
        let agy = Agy {
            paths: AgyPaths::at(&tmp.path().join(".gemini").join("antigravity-cli")),
            program: tmp.path().join("seatswap"),
        };
        assert_eq!(agy_link(&agy).expect("the link"), AgyLink::NotInstalled);
        let error = connect_agy(&agy).expect_err("nothing to connect");
        assert!(matches!(error, ProviderError::NoAgy { .. }), "{error}");
        assert!(!agy.paths.dir.exists(), "connect created no folder");
    }

    #[test]
    fn connect_adds_the_status_line_and_keeps_every_other_key() {
        let tmp = tempdir().expect("tempdir");
        let before = "{\n  \"model\": \"Gemini 4 Neon (High)\",\n  \"trustedWorkspaces\": [\"/home/sam/projects/atlas\"]\n}";
        let agy = agy_with(&tmp, Some(before));
        assert_eq!(agy_link(&agy).expect("the link"), AgyLink::NotConnected);

        connect_agy(&agy).expect("connected");

        let after = settings_of(&agy);
        let keys: Vec<&String> = after.as_object().expect("an object").keys().collect();
        assert_eq!(keys, ["model", "trustedWorkspaces", "statusLine"]);
        assert_eq!(after["model"], "Gemini 4 Neon (High)");
        let program = agy.program.to_str().expect("unicode");
        assert_eq!(
            after["statusLine"],
            json!({"type": "command", "command": format!("\"{program}\" statusline"), "stack_with_default": true})
        );
        let backup = agy.paths.dir.join("settings.json.seatswap-backup");
        assert_eq!(std::fs::read_to_string(backup).expect("the backup"), before);
        assert_eq!(agy_link(&agy).expect("the link"), AgyLink::Connected);
    }

    #[test]
    fn connect_creates_a_missing_settings_file_and_twice_writes_once() {
        let tmp = tempdir().expect("tempdir");
        let agy = agy_with(&tmp, None);
        assert_eq!(agy_link(&agy).expect("the link"), AgyLink::NotConnected);
        connect_agy(&agy).expect("connected");
        let first = std::fs::read(agy.paths.settings_file()).expect("the settings");
        connect_agy(&agy).expect("already connected");
        assert_eq!(
            std::fs::read(agy.paths.settings_file()).expect("the settings"),
            first
        );
        assert_eq!(settings_of(&agy).as_object().expect("an object").len(), 1);
    }

    #[test]
    fn another_status_line_is_named_and_never_replaced() {
        let tmp = tempdir().expect("tempdir");
        let before = r#"{"statusLine":{"type":"command","command":"bash ~/bin/agy-line.sh"}}"#;
        let agy = agy_with(&tmp, Some(before));
        assert_eq!(
            agy_link(&agy).expect("the link"),
            AgyLink::Other("bash ~/bin/agy-line.sh".to_string())
        );
        let error = connect_agy(&agy).expect_err("taken");
        assert!(
            error.to_string().contains("bash ~/bin/agy-line.sh"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(agy.paths.settings_file()).expect("the settings"),
            before
        );
    }

    #[test]
    fn broken_agy_settings_are_an_error_with_the_path() {
        let tmp = tempdir().expect("tempdir");
        let agy = agy_with(&tmp, Some("{\"model\": "));
        let error = agy_link(&agy).expect_err("broken JSON");
        assert!(error.to_string().contains("settings.json"), "{error}");
        assert!(connect_agy(&agy).is_err());
        assert_eq!(
            std::fs::read_to_string(agy.paths.settings_file()).expect("the settings"),
            "{\"model\": "
        );
    }

    fn payload() -> Value {
        serde_json::from_str(PAYLOAD).unwrap()
    }

    #[test]
    fn only_agys_payload_is_googles() {
        assert!(is_payload(&payload()));
        assert!(!is_payload(
            &serde_json::json!({"model": {"id": "claude-fable-5-2"}})
        ));
        assert!(!is_payload(&Value::Null));
    }

    #[test]
    fn the_buckets_become_limits() {
        let read = reading(feed_from_payload(&payload(), now()).unwrap(), now());
        assert_eq!(read.reading.account.acc, "sam@mail.example");
        assert_eq!(read.reading.account.org, GOOGLE_ORG);
        assert_eq!(read.reading.plan.as_deref(), Some("Pro"));
        assert!(read.notes.is_empty(), "{:?}", read.notes);
        let usage = read.reading.usage.unwrap();
        let rows: Vec<(String, f64, bool)> = usage
            .limits
            .iter()
            .map(|limit| {
                (
                    limit_name(&limit.kind).to_string(),
                    limit.percent,
                    limit.is_active,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("5h Claude/GPT".to_string(), 75.0, true),
                ("week Claude/GPT".to_string(), 0.0, false),
                ("5h".to_string(), 50.0, false),
                ("week".to_string(), 25.0, false),
            ]
        );
    }

    #[test]
    fn no_email_or_no_quota_is_no_account() {
        for (key, value) in [
            ("email", Value::Null),
            ("email", Value::from("")),
            ("quota", Value::Null),
            ("quota", serde_json::json!({})),
        ] {
            let mut raw = payload();
            raw[key] = value;
            assert_eq!(feed_from_payload(&raw, now()), None, "{key}");
        }
    }

    #[test]
    fn a_bucket_that_cannot_be_read_is_a_note() {
        let mut raw = payload();
        raw["quota"] = serde_json::json!({
            "a": {"remaining_amount": 12},
            "b": {"remaining_fraction": 1.5},
            "c": {"remaining_fraction": 0.5, "reset_time": "tomorrow"},
            "d": {"remaining_fraction": 0.5, "disabled": true},
            "e": {"remaining_fraction": 0.5}
        });
        let read = reading(feed_from_payload(&raw, now()).unwrap(), now());
        assert_eq!(
            read.notes,
            vec![
                "a not read: it has no remaining_fraction",
                "b not read: its remaining_fraction is not a number from 0 to 1",
                "c not read: its reset_time is not a date",
                "d not read: it is disabled",
            ]
        );
        let limits = read.reading.usage.unwrap().limits;
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].kind, LimitKind::Other("e".to_string()));
        assert_eq!(limits[0].resets_at, None);
    }

    #[test]
    fn a_window_that_ended_is_a_note_not_a_row() {
        let feed = feed_from_payload(&payload(), now()).unwrap();
        let read = reading(feed, "2026-11-25T14:10:00Z".parse().unwrap());
        assert_eq!(read.notes, vec!["5h was reset since the last reading"]);
        assert_eq!(read.reading.usage.unwrap().limits.len(), 3);
        assert_eq!(read.at, now());
    }

    #[test]
    fn the_file_is_written_when_it_changes_or_is_ten_seconds_old() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(&dir.path().join("data"));
        assert_eq!(read(&app, now()).unwrap(), None);

        let feed = feed_from_payload(&payload(), now()).unwrap();
        assert!(write_if_due(&app, &feed).unwrap());
        let later = GoogleFeed {
            at: now() + chrono::Duration::seconds(9),
            ..feed.clone()
        };
        assert!(!write_if_due(&app, &later).unwrap());
        let old = GoogleFeed {
            at: now() + chrono::Duration::seconds(10),
            ..feed.clone()
        };
        assert!(write_if_due(&app, &old).unwrap());
        let other = GoogleFeed {
            email: "lab@mail.example".to_string(),
            ..old
        };
        assert!(write_if_due(&app, &other).unwrap());
        let read_back = read(&app, now()).unwrap().unwrap();
        assert_eq!(read_back.reading.account.acc, "lab@mail.example");
    }

    /// A feed of one bucket, `seconds` after [`now`].
    fn one_bucket(seconds: i64, id: &str, percent: f64, reset: &str) -> GoogleFeed {
        GoogleFeed {
            at: now() + chrono::Duration::seconds(seconds),
            email: "sam@mail.example".to_string(),
            plan: Some("Google AI Pro".to_string()),
            buckets: vec![GoogleBucket {
                id: id.to_string(),
                percent: Some(percent),
                resets_at: Some(reset.parse().unwrap()),
                problem: None,
            }],
        }
    }

    fn stored_percent(app: &AppPaths) -> Option<f64> {
        let stored = feed::read_file::<GoogleFeed>(&app.google_file())
            .unwrap()
            .unwrap();
        stored.buckets[0].percent
    }

    #[test]
    fn an_unused_limits_moving_reset_is_no_change() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(&dir.path().join("data"));
        for (seconds, reset, wrote) in [
            (0, "2026-11-25T17:00:00Z", true),
            (1, "2026-11-25T17:00:01Z", false),
            (9, "2026-11-25T17:00:09Z", false),
            (10, "2026-11-25T17:00:10Z", true),
        ] {
            let feed = one_bucket(seconds, "3p-5h", 0.0, reset);
            assert_eq!(write_if_due(&app, &feed).unwrap(), wrote, "{seconds} s");
        }
        // Once it is used, its reset is a change like any other.
        let used = one_bucket(11, "3p-5h", 0.4, "2026-11-25T17:00:11Z");
        assert!(write_if_due(&app, &used).unwrap());
    }

    #[test]
    fn an_older_sessions_numbers_never_replace_newer_ones() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(&dir.path().join("data"));
        let week = "2026-11-30T07:50:32Z";
        for (seconds, percent, reset, stored) in [
            (0, 30.0, week, 30.0),
            // The same window, said by a session that has not asked since.
            (20, 12.0, week, 30.0),
            (40, 31.0, "2026-11-30T07:50:50Z", 31.0),
            // A window that starts later is the newer one.
            (60, 2.0, "2026-12-07T07:50:32Z", 2.0),
            // The one before it, and an unused one from before its start.
            (80, 31.0, "2026-11-25T12:01:10Z", 2.0),
            (100, 0.0, "2026-12-02T12:00:00Z", 2.0),
            // An unused limit's reset moves with the clock, so it can lie after
            // the used window's while that window runs.
            (110, 0.0, "2026-12-08T12:00:00Z", 2.0),
            // More use before an earlier reset is news, not an old number.
            (120, 5.0, "2026-12-03T12:00:00Z", 5.0),
        ] {
            write_if_due(&app, &one_bucket(seconds, "gemini-weekly", percent, reset)).unwrap();
            assert_eq!(stored_percent(&app), Some(stored), "at {seconds} s");
        }
        // A held-back number does not hold back the time it was said at.
        let stored = read(&app, now()).unwrap().unwrap();
        assert_eq!(stored.at, now() + chrono::Duration::seconds(120));

        // Another plan's and another account's numbers are taken whole.
        let mut ultra = one_bucket(140, "gemini-weekly", 1.0, "2026-12-03T12:00:00Z");
        ultra.plan = Some("Google AI Ultra".to_string());
        write_if_due(&app, &ultra).unwrap();
        assert_eq!(stored_percent(&app), Some(1.0));
        let mut other = one_bucket(160, "gemini-weekly", 0.5, "2026-12-03T12:00:00Z");
        other.email = "lab@mail.example".to_string();
        write_if_due(&app, &other).unwrap();
        assert_eq!(stored_percent(&app), Some(0.5));
    }

    #[test]
    fn an_unused_number_is_news_once_the_used_window_has_ended() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(&dir.path().join("data"));
        let reset = "2026-11-25T14:10:00Z";
        for (seconds, percent, reset, stored) in [
            (0, 5.0, reset, 5.0),
            (1_200, 0.0, "2026-11-25T17:20:00Z", 5.0),
            (7_800, 0.0, "2026-11-25T19:10:00Z", 0.0),
        ] {
            write_if_due(&app, &one_bucket(seconds, "gemini-5h", percent, reset)).unwrap();
            assert_eq!(stored_percent(&app), Some(stored), "at {seconds} s");
        }
    }

    #[test]
    fn an_unused_limit_is_recorded_without_a_window() {
        let feed = one_bucket(0, "gemini-weekly", 0.0, "2026-12-02T12:00:00Z");
        let found = reading(feed, now());
        let limits = |reading: &ProviderReading| {
            let usage = reading.usage.as_ref().expect("usage");
            (usage.limits[0].percent, usage.limits[0].resets_at)
        };
        assert_eq!(limits(&found.recorded), (0.0, None));
        let card = limits(&found.reading);
        assert_eq!(card, (0.0, Some("2026-12-02T12:00:00Z".parse().unwrap())));
    }

    #[test]
    fn what_is_recorded_is_what_agy_said_when_it_said_it() {
        let feed = feed_from_payload(&payload(), now()).unwrap();
        // Three hours later the 5h window has ended: the card has a note, the
        // record still has the number of the time it was said.
        let read = reading(feed, now() + chrono::Duration::hours(3));
        assert_eq!(
            read.notes,
            vec![
                "5h Claude/GPT was reset since the last reading",
                "5h was reset since the last reading"
            ]
        );
        let recorded = read.recorded;
        assert_eq!(recorded.account, read.reading.account);
        assert_eq!(recorded.plan.as_deref(), Some("Pro"));
        let limits: Vec<(LimitKind, f64, bool)> = recorded
            .usage
            .unwrap()
            .limits
            .into_iter()
            .map(|limit| (limit.kind, limit.percent, limit.is_active))
            .collect();
        assert_eq!(
            limits,
            vec![
                (LimitKind::Session, 50.0, true),
                (LimitKind::WeeklyAll, 25.0, false)
            ]
        );

        // A window that had ended when agy said it is not a reading of it, and
        // a feed without a Gemini limit records no usage.
        let ended = one_bucket(0, "gemini-5h", 40.0, "2026-11-25T11:00:00Z");
        assert_eq!(reading(ended, now()).recorded.usage, None);
        let other = one_bucket(0, "3p-weekly", 40.0, "2026-11-30T11:00:00Z");
        assert_eq!(reading(other, now()).recorded.usage, None);
    }

    #[test]
    fn a_broken_file_is_an_error_with_its_path() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(dir.path());
        std::fs::write(app.google_file(), b"{").unwrap();
        let error = read(&app, now()).unwrap_err().to_string();
        assert!(error.contains("google.json"), "{error}");
    }
}
