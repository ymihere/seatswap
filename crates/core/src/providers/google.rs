//! Google's usage, as the Antigravity CLI (`agy`) hands it to its status line
//! command (spec §7.9). seatswap asks Google nothing and reads no Google login:
//! `seatswap statusline` gets the payload on standard input and keeps the
//! account's numbers in `<data>/google.json`, and the pop-up's card shows them.
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
use crate::history::AccountRef;
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

/// A stored feed as a reading: when `agy` said it, and what of it is not a
/// limit row (a window that ended since, a bucket that could not be read).
#[derive(Debug, Clone, PartialEq)]
pub struct GoogleReading {
    pub reading: ProviderReading,
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

/// Write `<data>/google.json` under the feed's own rule. Returns whether it wrote.
pub fn write_if_due(app: &AppPaths, feed: &GoogleFeed) -> io::Result<bool> {
    feed::write_file_if_due(
        app,
        &app.google_file(),
        feed,
        feed.at,
        |stored: &GoogleFeed| {
            let same = stored.email == feed.email
                && stored.plan == feed.plan
                && stored.buckets == feed.buckets;
            (stored.at, same)
        },
    )
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

/// The feed as limits. A window whose reset has passed holds no usage any more
/// and nobody has said what the next one holds, so it is a note, not a row.
pub fn reading(feed: GoogleFeed, now: DateTime<Utc>) -> GoogleReading {
    let mut limits = Vec::new();
    let mut notes = Vec::new();
    for bucket in feed.buckets {
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
    mark_highest_active(&mut limits);
    GoogleReading {
        reading: ProviderReading {
            account: AccountRef {
                acc: feed.email,
                org: GOOGLE_ORG.to_string(),
            },
            plan: feed.plan,
            usage: Some(Usage {
                limits,
                extra_usage_enabled: false,
                warnings: Vec::new(),
            }),
            credits: None,
            spend_24h: None,
            hours: None,
        },
        at: feed.at,
        notes,
    }
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

    #[test]
    fn a_broken_file_is_an_error_with_its_path() {
        let dir = tempdir().unwrap();
        let app = AppPaths::at(dir.path());
        std::fs::write(app.google_file(), b"{").unwrap();
        let error = read(&app, now()).unwrap_err().to_string();
        assert!(error.contains("google.json"), "{error}");
    }
}
