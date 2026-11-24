//! Every text auto-burn shows (spec §2.5, §2.12): the tray app's line and
//! notifications, the suggestion, its reasons and the regime's "why". The CLI
//! uses the same functions, so the two front ends cannot drift.

#[cfg(feature = "gui")]
use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, Utc};
use serde::Serialize;
#[cfg(feature = "gui")]
use serde::Serializer;

use seatswap_core::burn::decide::RegimeWhy;
use seatswap_core::burn::decide::{Decision, Reason};
#[cfg(feature = "gui")]
use seatswap_core::burn::plan::Fill;
#[cfg(feature = "gui")]
use seatswap_core::burn::plan::Plan;
use seatswap_core::burn::plan::{follow, Inputs, Policy};
use seatswap_core::burn::replay::ReplayResult;
use seatswap_core::engine::AccountView;
use seatswap_core::history::AccountRef;
use seatswap_core::store::Account;

use crate::format;

/// A text that cannot be written from the values at hand. Every variant names
/// the account it is about; the callers put the error into the snapshot or the
/// command instead of guessing a name or a time.
#[derive(Debug, thiserror::Error)]
pub enum TextError {
    #[error("account {account}: no view")]
    ViewMissing { account: String },
    #[error("account {account}: no weekly reset")]
    ResetMissing { account: String },
    #[cfg(feature = "gui")]
    #[error("account {account}: the plan does not say when it fills")]
    FillMissing { account: String },
    #[error("no active account to name")]
    NoActive,
}

/// One piece of the auto-burn line: a text, or an account named by its id and
/// drawn by the pop-up as a chip in the account's colour (spec §2.5).
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BurnPart {
    Text(String),
    Account { id: String, abbr: String },
}

#[cfg(feature = "gui")]
impl Serialize for BurnPart {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // The page reads `{"text":"…"}` or `{"account":"<id>","abbr":"AC"}`;
        // an externally tagged enum would nest the account's fields.
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Wire<'a> {
            Text { text: &'a str },
            Account { account: &'a str, abbr: &'a str },
        }
        let wire = match self {
            BurnPart::Text(text) => Wire::Text { text },
            BurnPart::Account { id, abbr } => Wire::Account { account: id, abbr },
        };
        wire.serialize(serializer)
    }
}

/// The whole line as plain text: every account is its abbreviation, so the
/// text is the sentence the line said before it had parts.
// No front end shows the line as text today (the pop-up draws the parts, the
// tray menu, notifications and the CLI have texts of their own); the tests
// use it to compare the sentence. It is compiled for tests only: the app has
// no production caller.
#[cfg(all(feature = "gui", test))]
pub fn plain(parts: &[BurnPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            BurnPart::Text(piece) => text.push_str(piece),
            BurnPart::Account { abbr, .. } => text.push_str(abbr),
        }
    }
    text
}

/// A suggested switch: the account to switch to and the whole sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchSuggestion {
    pub to: String,
    pub text: String,
}

/// The account's abbreviation. An id without a view is an error, never the id
/// (or anything else) guessed in its place.
pub fn abbr(views: &[AccountView], id: &str) -> Result<String, TextError> {
    view(views, id)
        .map(|view| view.abbr.clone())
        .ok_or_else(|| TextError::ViewMissing {
            account: id.to_string(),
        })
}

fn view<'a>(views: &'a [AccountView], id: &str) -> Option<&'a AccountView> {
    views.iter().find(|view| view.id == id)
}

/// The account as the pop-up draws it: its id and its abbreviation. An id
/// without a view is an error, never the id guessed as its name.
#[cfg(feature = "gui")]
fn account_part(views: &[AccountView], id: &str) -> Result<BurnPart, TextError> {
    Ok(BurnPart::Account {
        id: id.to_string(),
        abbr: abbr(views, id)?,
    })
}

/// The line while an account auto-burn may use, or the active one, has a login
/// and no reading yet: its abbreviation, in the order `accounts` gives.
#[cfg(feature = "gui")]
pub fn unread_line(views: &[AccountView], accounts: &[String]) -> Result<Vec<BurnPart>, TextError> {
    let mut parts = vec![BurnPart::Text(
        "Waiting for a first reading of ".to_string(),
    )];
    for (index, id) in accounts.iter().enumerate() {
        if index > 0 {
            parts.push(BurnPart::Text(", ".to_string()));
        }
        parts.push(account_part(views, id)?);
    }
    parts.push(BurnPart::Text(".".to_string()));
    Ok(parts)
}

/// The line when no account auto-burn may use (nor the active one) has a
/// working login and a reading. With no stored account at all it asks for the
/// first one; otherwise every such login ended, or auto-burn may use no
/// account.
#[cfg(feature = "gui")]
pub fn no_accounts_line(accounts: &[Account]) -> Vec<BurnPart> {
    let text = if accounts.is_empty() {
        "Save an account to start."
    } else {
        "No account auto-burn may use has a working login."
    };
    vec![BurnPart::Text(text.to_string())]
}

/// The target's weekly reset, in the words a reason needs.
fn weekly_reset(views: &[AccountView], id: &str) -> Result<DateTime<Utc>, TextError> {
    view(views, id)
        .ok_or_else(|| TextError::ViewMissing {
            account: id.to_string(),
        })?
        .usage
        .as_ref()
        .and_then(|usage| usage.weekly_all())
        .and_then(|week| week.resets_at)
        .ok_or_else(|| TextError::ResetMissing {
            account: id.to_string(),
        })
}

/// Why early switching is on or off (spec §2.5).
pub fn why(why: RegimeWhy) -> &'static str {
    match why {
        RegimeWhy::Learning => "learning",
        RegimeWhy::SessionsOff => "reading sessions is off: no rates are learned",
        RegimeWhy::NoCostYet => "no switch measured yet",
        RegimeWhy::NotWorthIt => "switches early only for quota at risk of expiring",
        RegimeWhy::Pays => "switches early whenever it pays",
    }
}

/// One line of `seatswap burn plan`: `HH:MM–HH:MM  AC`, or
/// `HH:MM–HH:MM  nothing usable` for a stretch without an account. The times
/// are local; a time that is not today carries its weekday. An account without
/// a view is an error, never an empty name.
pub fn segment_line(
    account: Option<&str>,
    views: &[AccountView],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<String, TextError> {
    let name = match account {
        Some(id) => abbr(views, id)?,
        None => "nothing usable".to_string(),
    };
    Ok(format!(
        "{}–{}  {name}",
        format::clock(from, now, offset),
        format::clock(to, now, offset)
    ))
}

/// The name of a replay's policy: the words `seatswap burn simulate` prints.
pub fn policy_name(policy: Policy) -> &'static str {
    match policy {
        Policy::WhenFull => "Only when full",
        Policy::Lazy => "Lazy",
        Policy::Eager => "Eager",
    }
}

/// A dollar figure as the duration it takes at `typical_usd` USD per
/// typical working hour, in the one spelling of review item 42 (`2h 30min`). A
/// typical hour of 0 means the hours are not known: they are said so, never
/// divided by zero. A negative or non-finite figure is the callers' error and
/// keeps the old one-decimal text, as before.
fn hours(usd: f64, typical_usd: f64) -> String {
    match seatswap_core::burn::usd_hours(usd, typical_usd) {
        Some(hours) if hours.is_finite() && hours >= 0.0 => {
            seatswap_core::duration::duration_text((hours * 3_600.0).round() as u64)
        }
        Some(hours) => format!("{hours:.1} h"),
        None => "not known".to_string(),
    }
}

/// The report of one replay: its policy, the hours worked and blocked, its
/// switches, and the expired week-percent of every account whose reset the
/// replay passed. A replay account without a stored account is an error.
pub fn replay_report(result: &ReplayResult, accounts: &[Account]) -> Result<String, TextError> {
    let mut lines = vec![format!(
        "{}: worked {}, blocked {}, {} switches",
        policy_name(result.policy),
        hours(result.worked_usd, result.typical_usd),
        hours(result.blocked_usd, result.typical_usd),
        result.switches
    )];
    for (account, expired) in &result.expired {
        let stored = accounts
            .iter()
            .find(|stored| AccountRef::of(stored) == *account)
            .ok_or_else(|| TextError::ViewMissing {
                account: format!("{}/{}", account.acc, account.org),
            })?;
        lines.push(format!(
            "  {}: {} % expired",
            stored.abbr,
            format::integer_percent(*expired)
        ));
    }
    Ok(lines.join("\n"))
}

/// The reason of a switch, in the words the notification and the suggestion use.
pub fn reason(
    reason: &Reason,
    target: &str,
    active: Option<&str>,
    views: &[AccountView],
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<String, TextError> {
    match reason {
        Reason::NoneActive => Ok("no account was in use".to_string()),
        Reason::ActiveFull { until } => {
            let id = active.ok_or(TextError::NoActive)?;
            let who = abbr(views, id)?;
            match until {
                Some(until) => Ok(format!(
                    "{who} is full until {}",
                    format::clock(*until, now, offset)
                )),
                None => Ok(format!("{who} is full")),
            }
        }
        Reason::WindowNearlyFull { until } => {
            let id = active.ok_or(TextError::NoActive)?;
            let who = abbr(views, id)?;
            match until {
                Some(until) => Ok(format!(
                    "{who}'s 5h window is nearly full; it resets at {}",
                    format::clock(*until, now, offset)
                )),
                None => Ok(format!("{who}'s 5h window is nearly full")),
            }
        }
        Reason::WindowOpen => Ok(format!(
            "its 5h window is open and its week resets on {}",
            format::weekday_clock(weekly_reset(views, target)?, offset)
        )),
        Reason::ResetNear => Ok(format!(
            "its week resets on {}",
            format::weekday_clock(weekly_reset(views, target)?, offset)
        )),
    }
}

#[cfg(feature = "gui")]
/// The line under the auto-burn switch: the account in use, when it fills and
/// which account follows. A plan that leaves the account before it fills (an
/// eager plan can) says `Then AC, before PR is full.` instead. The line never
/// mentions learning: an account whose rates are not known yet is simply not
/// planned to fill (spec §2.3).
pub fn line(
    plan: &Plan,
    active: Option<&str>,
    views: &[AccountView],
    now: DateTime<Utc>,
) -> Result<Option<Vec<BurnPart>>, TextError> {
    let current = active
        .filter(|id| {
            plan.segments
                .iter()
                .any(|segment| segment.account.as_deref() == Some(*id))
        })
        .or_else(|| {
            plan.segments
                .iter()
                .find_map(|segment| segment.account.as_deref())
        });
    let Some(current) = current else {
        return Ok(None);
    };
    let next = plan
        .segments
        .iter()
        .find(|segment| segment.account.as_deref().is_some_and(|id| id != current));
    match next {
        Some(next) => {
            let Some(next_id) = next.account.as_deref() else {
                return Ok(None);
            };
            // A plan that fills the current account only after the next one is
            // due leaves it before it is full (an eager plan can do that): the
            // line then says so instead of claiming it stays until full.
            let fills_before_switch = plan
                .full_at
                .get(current)
                .is_some_and(|full_at| *full_at <= next.from);
            if !fills_before_switch {
                return Ok(Some(vec![
                    BurnPart::Text("On ".to_string()),
                    account_part(views, current)?,
                    BurnPart::Text(format!(
                        " for about {}. Then ",
                        format::time_left(next.from, now)
                    )),
                    account_part(views, next_id)?,
                    BurnPart::Text(", before ".to_string()),
                    account_part(views, current)?,
                    BurnPart::Text(" is full.".to_string()),
                ]));
            }
            let fill = plan
                .full_by
                .get(current)
                .ok_or_else(|| TextError::FillMissing {
                    account: current.to_string(),
                })?;
            let limit = match fill {
                Fill::Week => "week",
                Fill::Window => "5h window",
            };
            Ok(Some(vec![
                BurnPart::Text("On ".to_string()),
                account_part(views, current)?,
                BurnPart::Text(format!(
                    " until its {limit} is full, about {}. Then ",
                    format::time_left(next.from, now)
                )),
                account_part(views, next_id)?,
                BurnPart::Text(".".to_string()),
            ]))
        }
        None => Ok(Some(vec![
            BurnPart::Text("On ".to_string()),
            account_part(views, current)?,
            BurnPart::Text(". No switch planned this week.".to_string()),
        ])),
    }
}

#[cfg(feature = "gui")]
/// The first future use of every other account, by account id.
pub fn planned(plan: &Plan, active: Option<&str>) -> BTreeMap<String, DateTime<Utc>> {
    let mut planned = BTreeMap::new();
    for segment in &plan.segments {
        let Some(id) = segment.account.as_deref() else {
            continue;
        };
        if active == Some(id) {
            continue;
        }
        planned.entry(id.to_string()).or_insert(segment.from);
    }
    planned
}

#[cfg(feature = "gui")]
/// The notification after an automatic switch.
pub fn switch_notification(
    decision: &Decision,
    active: Option<&str>,
    views: &[AccountView],
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<Option<String>, TextError> {
    let Decision::Switch { to, reason: why } = decision else {
        return Ok(None);
    };
    let to_name = abbr(views, to)?;
    match (why, active) {
        (Reason::ActiveFull { until: Some(until) }, Some(from)) => Ok(Some(format!(
            "Auto-burn: now on {to_name}. {} is full until {}.",
            abbr(views, from)?,
            format::clock(*until, now, offset)
        ))),
        _ => Ok(Some(format!(
            "Auto-burn: now on {to_name}: {}.",
            reason(why, to, active, views, now, offset)?
        ))),
    }
}

/// The suggestion when auto-burn is off: "Switch to … now: …", plus what the
/// plan that switches only at 100 % ([`Policy::WhenFull`]) expects to leave
/// unused on the target's week. The "Staying on …" sentence is about staying on
/// the active account until it is full, which only that plan models, so this
/// is the one place that plan is chosen: the plan auto-burn follows moves early
/// to an account that would lose quota, and its `left_at_reset` would not
/// answer that sentence.
pub fn suggestion(
    decision: &Decision,
    inputs: &Inputs,
    active: Option<&str>,
    views: &[AccountView],
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<Option<SwitchSuggestion>, TextError> {
    let (to, why) = match decision {
        Decision::Switch { to, reason } => (to, reason),
        Decision::Wait { to, reason, .. } => (to, reason),
        Decision::Stay | Decision::Stale | Decision::AllFull { .. } => return Ok(None),
    };
    let to_name = abbr(views, to)?;
    let mut text = format!(
        "Switch to {to_name} now: {}.",
        reason(why, to, active, views, now, offset)?
    );
    let plan = follow(inputs, Policy::WhenFull);
    if let (Some(left), Some(active_name)) = (
        plan.left_at_reset.get(to),
        active.map(|id| abbr(views, id)).transpose()?,
    ) {
        if *left > 1.0 {
            text.push_str(&format!(
                " Staying on {active_name} is expected to let {} % of {to_name} expire.",
                format::integer_percent(*left)
            ));
        }
    }
    Ok(Some(SwitchSuggestion {
        to: to.clone(),
        text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Duration;
    #[cfg(feature = "gui")]
    use std::collections::BTreeMap;

    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::burn::decide::{Decision, Regime, RegimeWhy, Situation};
    use seatswap_core::burn::live::assess;
    use seatswap_core::burn::plan::AccountState;
    #[cfg(feature = "gui")]
    use seatswap_core::burn::plan::{Fill, Plan, Segment};
    use seatswap_core::burn::{Rates, Rhythm};
    use seatswap_core::store::LoginState;

    const RESET: &str = "2026-11-16T02:00:00Z";

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a test time")
            .with_timezone(&Utc)
    }

    fn offset() -> FixedOffset {
        FixedOffset::east_opt(0).expect("UTC")
    }

    fn view(id: &str, abbr: &str, week: f64, session: bool) -> AccountView {
        let mut limits = vec![Limit {
            kind: LimitKind::WeeklyAll,
            percent: week,
            severity: Severity::Normal,
            resets_at: Some(utc(RESET)),
            model: None,
            is_active: false,
        }];
        if session {
            limits.push(Limit {
                kind: LimitKind::Session,
                percent: 10.0,
                severity: Severity::Normal,
                resets_at: Some(utc(RESET)),
                model: None,
                is_active: false,
            });
        }
        AccountView {
            id: id.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            auto_burn: true,
            is_active: false,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: Some(Usage {
                limits,
                extra_usage_enabled: false,
                warnings: Vec::new(),
            }),
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: Some(utc("2026-11-10T11:59:00Z")),
            last_error: None,
        }
    }

    #[cfg(feature = "gui")]
    fn plan() -> Plan {
        let now = utc("2026-11-10T12:00:00Z");
        Plan {
            segments: vec![
                Segment {
                    account: Some("pr".to_string()),
                    from: now - Duration::hours(2),
                    to: now + Duration::hours(2),
                },
                Segment {
                    account: Some("ac".to_string()),
                    from: now + Duration::hours(2),
                    to: now + Duration::hours(4),
                },
            ],
            blocked_usd: 0.0,
            switches: 1,
            left_at_reset: BTreeMap::new(),
            // The lazy plan's shape: PR fills when its segment ends, so the
            // switch comes at the same time, not before.
            full_at: BTreeMap::from([("pr".to_string(), now + Duration::hours(2))]),
            full_by: BTreeMap::from([
                ("pr".to_string(), Fill::Week),
                ("ac".to_string(), Fill::Window),
            ]),
        }
    }

    /// An uncapped account as the plan needs it for the suggestion's own lazy
    /// plan.
    fn state(id: &str, c: f64, week: f64, reset: &str) -> AccountState {
        AccountState {
            id: id.to_string(),
            may_use: true,
            login_ok: true,
            week_percent: week,
            week_resets_at: utc(reset),
            session: None,
            model_blocked: false,
            rates: Some(Rates::given(c, None)),
            thinking_held: 0,
        }
    }

    /// Two accounts whose weekly resets lie beyond the plan's horizon: no reset
    /// is recorded, so the suggestion adds no "Staying" sentence.
    fn inputs_without_a_reset() -> Inputs {
        Inputs {
            now: utc("2026-11-10T12:00:00Z"),
            offset: offset(),
            accounts: vec![
                state("pr", 2.0, 20.0, "2026-11-18T12:00:00Z"),
                state("ac", 1.0, 10.0, "2026-11-18T12:00:00Z"),
            ],
            active: Some("pr".to_string()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            paused_until: None,
        }
    }

    /// Switching only when full stays on PR for 50 h, after AC's weekly reset at
    /// 24 h, so AC is left untouched (100 % unused); the eager plan uses AC at
    /// once and leaves nothing at that reset.
    fn inputs_that_leave_the_target_unused() -> Inputs {
        Inputs {
            now: utc("2026-11-10T12:00:00Z"),
            offset: offset(),
            accounts: vec![
                state("pr", 2.0, 0.0, "2026-11-18T12:00:00Z"),
                state("ac", 5.0, 0.0, "2026-11-11T12:00:00Z"),
            ],
            active: Some("pr".to_string()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            paused_until: None,
        }
    }

    fn early_regime() -> Regime {
        Regime {
            early: true,
            why: RegimeWhy::Pays,
            lazy_blocked_h: 1.0,
            eager_blocked_h: 0.0,
            extra_switches: 1,
            cost_h: Some(0.1),
        }
    }

    fn situation(inputs: Inputs) -> Situation {
        Situation {
            oldest_reading: inputs.now - Duration::minutes(1),
            inputs,
            cost: None,
            tied_busy: 0,
            sessions_off: false,
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_line_names_the_account_in_use_and_the_next() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let line = line(&plan(), Some("pr"), &views, utc("2026-11-10T12:00:00Z"))
            .expect("a line")
            .expect("the plan has a next account");
        assert_eq!(
            plain(&line),
            "On PR until its week is full, about 2h. Then AC."
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_line_says_week_when_the_week_fills_first() {
        let views = vec![view("pr", "PR", 20.0, true), view("ac", "AC", 10.0, true)];
        let line = line(&plan(), Some("pr"), &views, utc("2026-11-10T12:00:00Z"))
            .expect("a line")
            .expect("the plan has a next account");
        assert_eq!(
            plain(&line),
            "On PR until its week is full, about 2h. Then AC.",
            "PR has a 5h limit, but the plan records its week as what fills first"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_line_says_so_when_the_plan_leaves_before_the_account_is_full() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let now = utc("2026-11-10T12:00:00Z");
        let mut plan = plan();
        plan.full_at
            .insert("pr".to_string(), now + Duration::hours(3));
        let line = line(&plan, Some("pr"), &views, now)
            .expect("a line")
            .expect("the plan has a next account");
        assert_eq!(
            plain(&line),
            "On PR for about 2h. Then AC, before PR is full.",
            "the eager plan leaves PR before its week fills"
        );
    }

    #[test]
    fn a_suggestion_has_the_exact_reason() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let decision = Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::WindowOpen,
        };
        let suggestion = suggestion(
            &decision,
            &inputs_without_a_reset(),
            Some("pr"),
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect("the text is writable")
        .expect("a switch suggests");
        assert_eq!(
            suggestion.text,
            "Switch to AC now: its 5h window is open and its week resets on Mon 02:00."
        );
        assert_eq!(suggestion.to, "ac");
    }

    #[test]
    fn a_wait_suggestion_uses_the_decisions_reason() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, false)];
        let decision = Decision::Wait {
            to: "ac".to_string(),
            latest: utc("2026-11-10T14:00:00Z"),
            tied_busy: 1,
            reason: Reason::WindowOpen,
        };
        let suggestion = suggestion(
            &decision,
            &inputs_without_a_reset(),
            Some("pr"),
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect("the text is writable")
        .expect("a wait suggests");
        assert_eq!(
            suggestion.text,
            "Switch to AC now: its 5h window is open and its week resets on Mon 02:00.",
            "AC has no session limit in its view; the decision's reason wins"
        );
    }

    #[test]
    fn sessions_off_says_why() {
        assert_eq!(
            why(RegimeWhy::SessionsOff),
            "reading sessions is off: no rates are learned"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_unread_line_names_the_accounts() {
        let views = vec![view("lb", "LB", 20.0, false), view("ac", "AC", 10.0, true)];
        let line = unread_line(&views, &["lb".to_string(), "ac".to_string()])
            .expect("both accounts have a view");
        assert_eq!(plain(&line), "Waiting for a first reading of LB, AC.");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn parts_name_every_account_by_id() {
        fn accounts(parts: &[BurnPart]) -> Vec<(&str, &str)> {
            parts
                .iter()
                .filter_map(|part| match part {
                    BurnPart::Account { id, abbr } => Some((id.as_str(), abbr.as_str())),
                    BurnPart::Text(_) => None,
                })
                .collect()
        }

        let views = vec![
            view("pr", "PR", 20.0, false),
            view("ac", "AC", 10.0, true),
            view("lb", "LB", 30.0, false),
        ];
        let now = utc("2026-11-10T12:00:00Z");

        let plan_line = line(&plan(), Some("pr"), &views, now)
            .expect("a line")
            .expect("a next account");
        assert_eq!(
            accounts(&plan_line),
            vec![("pr", "PR"), ("ac", "AC")],
            "each account part carries the account's id, never a guessed name"
        );
        assert_eq!(
            plain(&plan_line),
            "On PR until its week is full, about 2h. Then AC."
        );

        let unread = unread_line(&views, &["ac".to_string(), "lb".to_string()])
            .expect("both accounts have a view");
        assert_eq!(accounts(&unread), vec![("ac", "AC"), ("lb", "LB")]);
        assert_eq!(plain(&unread), "Waiting for a first reading of AC, LB.");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_part_serializes_as_text_or_an_account() {
        assert_eq!(
            serde_json::to_value(BurnPart::Text("On ".to_string())).expect("a text part"),
            serde_json::json!({ "text": "On " })
        );
        assert_eq!(
            serde_json::to_value(BurnPart::Account {
                id: "ac".to_string(),
                abbr: "AC".to_string(),
            })
            .expect("an account part"),
            serde_json::json!({ "account": "ac", "abbr": "AC" })
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn why_uses_the_plans_words() {
        assert_eq!(why(RegimeWhy::Learning), "learning");
        assert_eq!(why(RegimeWhy::NoCostYet), "no switch measured yet");
        assert_eq!(
            why(RegimeWhy::NotWorthIt),
            "switches early only for quota at risk of expiring"
        );
        assert_eq!(why(RegimeWhy::Pays), "switches early whenever it pays");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_switch_notification_names_both_accounts() {
        let views = vec![view("pr", "PR", 100.0, false), view("ac", "AC", 10.0, true)];
        let decision = Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::ActiveFull {
                until: Some(utc("2026-11-10T14:40:00Z")),
            },
        };
        let text = switch_notification(
            &decision,
            Some("pr"),
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect("the text is writable")
        .expect("a switch notifies");
        assert_eq!(text, "Auto-burn: now on AC. PR is full until 14:40.");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_nearly_full_window_says_when_it_resets() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 96.0, true)];
        let decision = Decision::Switch {
            to: "pr".to_string(),
            reason: Reason::WindowNearlyFull {
                until: Some(utc("2026-11-10T14:40:00Z")),
            },
        };
        let text = switch_notification(
            &decision,
            Some("ac"),
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect("the text is writable")
        .expect("a switch notifies");
        assert_eq!(
            text,
            "Auto-burn: now on PR: AC's 5h window is nearly full; it resets at 14:40."
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn no_active_account_has_its_own_reason() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let decision = Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::NoneActive,
        };
        let text = switch_notification(
            &decision,
            None,
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect("the text is writable")
        .expect("a switch notifies");
        assert_eq!(text, "Auto-burn: now on AC: no account was in use.");
    }

    #[test]
    fn an_account_without_a_view_is_an_error() {
        let views = vec![view("pr", "PR", 20.0, false)];
        let decision = Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::WindowOpen,
        };
        let error = suggestion(
            &decision,
            &inputs_without_a_reset(),
            Some("pr"),
            &views,
            utc("2026-11-10T12:00:00Z"),
            offset(),
        )
        .expect_err("AC has no view");
        assert!(
            matches!(error, TextError::ViewMissing { ref account } if account == "ac"),
            "{error}"
        );
    }

    #[test]
    fn an_expiring_target_adds_the_only_when_full_percent() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let inputs = inputs_that_leave_the_target_unused();
        assert_eq!(
            follow(&inputs, Policy::WhenFull)
                .left_at_reset
                .get("ac")
                .copied(),
            Some(100.0),
            "switching only when full never uses AC before its reset"
        );
        assert_ne!(
            follow(&inputs, Policy::Lazy).left_at_reset.get("ac"),
            Some(&100.0),
            "the lazy plan moves to AC, which would lose quota"
        );
        let decision = Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::WindowOpen,
        };
        let suggestion = suggestion(&decision, &inputs, Some("pr"), &views, inputs.now, offset())
            .expect("the text is writable")
            .expect("a switch suggests");
        assert!(
            suggestion
                .text
                .ends_with(" Staying on PR is expected to let 100 % of AC expire."),
            "{}",
            suggestion.text
        );
    }

    #[test]
    fn the_suggestion_uses_only_when_full_even_when_the_assessment_is_eager() {
        let views = vec![view("pr", "PR", 20.0, false), view("ac", "AC", 10.0, true)];
        let inputs = inputs_that_leave_the_target_unused();
        let early = early_regime();
        let assessment = assess(situation(inputs.clone()), Some(&early));
        assert_eq!(
            assessment.plan,
            follow(&inputs, Policy::Eager),
            "early switching makes the assessment follow the eager plan"
        );
        assert_ne!(
            assessment.plan.left_at_reset.get("ac"),
            follow(&inputs, Policy::WhenFull).left_at_reset.get("ac"),
            "the eager plan leaves AC used, only when full leaves it untouched"
        );

        let suggestion = suggestion(
            &assessment.decision,
            &assessment.situation.inputs,
            Some("pr"),
            &views,
            inputs.now,
            offset(),
        )
        .expect("the text is writable")
        .expect("the decision switches");
        assert!(
            suggestion
                .text
                .ends_with(" Staying on PR is expected to let 100 % of AC expire."),
            "{}",
            suggestion.text
        );
    }
}
