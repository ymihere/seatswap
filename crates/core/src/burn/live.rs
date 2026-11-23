//! The reading side of auto-burn (spec §2.3, §2.5, §2.12): one pass over the
//! history and one translation of the tray app's views into the plan's inputs.
//! The CLI and the tray app both call these, so neither builds a `Situation`
//! by hand.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, FixedOffset, TimeZone, Utc};

use crate::api::Limit;
use crate::engine::{AccountView, Blocker};
use crate::history::{
    AccountRef, By, Event, EventKind, HistoryDir, HistoryError, Hosted, Hosts, Range, Sample,
    TokenRecord,
};
use crate::money::Prices;
use crate::store::{Account, LoginState};

use super::cost::switch_cost;
use super::decide::{decide, regime, Decision, Regime, Situation};
use super::plan::{blocked, followed, AccountState, Inputs, Plan, PlanError, SessionState};
use super::rates::{rates, rhythm, unpriced_models, unpriced_warning, RateAccount, Timeline};
use super::{Rates, Rhythm, MANUAL_HOLD_H, RATES_DAYS, RHYTHM_WEEKS};

/// Everything a decision is built from, read once: the rhythm, the accounts'
/// rates, the measured switch cost, the read warnings (and one per Claude model
/// without a price) and the events of the last [`RATES_DAYS`] days.
#[derive(Debug, Clone)]
pub struct Learned {
    pub rhythm: Rhythm,
    pub rates: BTreeMap<AccountRef, Rates>,
    pub cost: Option<super::decide::SwitchCost>,
    pub warnings: Vec<String>,
    /// The events of the last [`RATES_DAYS`] days, every host, sorted by time.
    pub events: Vec<Hosted<Event>>,
    /// This history folder's host. Only its switch events hold auto-burn back.
    pub host: String,
    /// `readSessions` is off: no rates were learned from the sessions
    /// (spec §2.3). The regime says so with [`super::decide::RegimeWhy::SessionsOff`].
    pub sessions_off: bool,
}

/// What [`situation`] found: a checked `Situation`, or the accounts that have a
/// login but no reading yet.
#[allow(clippy::large_enum_variant)]
pub enum Readiness {
    Ready(Situation),
    /// Accounts with a login and no weekly reading. The caller treats this
    /// exactly as [`super::decide::Decision::Stale`]: poll them, do not switch.
    Unread {
        accounts: Vec<String>,
    },
    /// No account at all can go into the inputs (every account is logged out).
    /// The caller decides nothing and shows no view.
    NoAccounts,
}

/// Read the history once: samples and tokens of [`RHYTHM_WEEKS`] weeks, events
/// of all time. The timelines of the rates take every event (R2 of 1.3: an
/// account live for weeks without a switch still has a timeline); the switch
/// cost measures the switches of the last [`RATES_DAYS`] days, and
/// [`Learned::events`] keeps those days only. The rates are computed for
/// `accounts` only; an account with too little work and no prior has no entry.
/// Work is priced with `prices` ([`crate::money::api_usd`]); each Claude model
/// without a price gets one warning, its work being left out of the rates and
/// the rhythm, with or without session reading. When the stored list has no
/// Anthropic model at all, one warning says that auto-burn has no API prices
/// yet instead of one per model. With `read_sessions` false no rates are
/// learned: every account plays as one without rates (spec §2.3).
pub fn learn<Tz: TimeZone + Copy>(
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    accounts: &[Account],
    read_sessions: bool,
    prices: &Prices,
) -> Result<Learned, HistoryError> {
    /// The one sentence for a price list without an Anthropic model.
    const NO_PRICES: &str = "auto-burn has no API prices yet: it measures work in API dollars and needs the price list (Settings → General → Look up prices)";
    let weeks = Range {
        from: now - Duration::weeks(RHYTHM_WEEKS),
        to: now,
    };
    let days = Range {
        from: now - Duration::days(RATES_DAYS),
        to: now,
    };
    let all_time = Range {
        from: DateTime::<Utc>::MIN_UTC,
        to: now,
    };
    // Auto-burn reads Claude's history only (step-d.md Task 4): a provider's
    // samples and events never make a rate, a hold or a switch cost.
    let samples = history.read_claude::<Sample>(&weeks, &Hosts::All)?;
    let tokens = history.read::<TokenRecord>(&weeks, &Hosts::All)?;
    let events = history.read_claude::<Event>(&all_time, &Hosts::All)?;

    let mut warnings = Vec::new();
    warnings.extend(samples.warnings);
    warnings.extend(tokens.warnings);
    warnings.extend(events.warnings);

    let rhythm = rhythm(&tokens.records, now, &zone, prices);
    // The rhythm and the rates meet the same unpriced models; the union is
    // what `learn` names once each, whether or not it read sessions.
    let mut unpriced: BTreeSet<String> = unpriced_models(&tokens.records, prices);
    let learned_rates = if read_sessions {
        let rate_accounts: Vec<RateAccount> = accounts.iter().map(RateAccount::of).collect();
        let learned = rates(
            &rate_accounts,
            &samples.records,
            &tokens.records,
            &events.records,
            prices,
            now,
        );
        unpriced.extend(learned.unpriced.iter().cloned());
        learned.rates
    } else {
        BTreeMap::new()
    };
    if has_anthropic_prices(prices) {
        warnings.extend(unpriced.iter().map(|model| unpriced_warning(model)));
    } else {
        warnings.push(NO_PRICES.to_string());
    }
    let cost = switch_cost(
        &events.records,
        &samples.records,
        &tokens.records,
        &learned_rates,
        prices,
        &days,
    );
    let recent: Vec<Hosted<Event>> = events
        .records
        .into_iter()
        .filter(|hosted| hosted.record.t >= days.from)
        .collect();

    Ok(Learned {
        rhythm,
        rates: learned_rates,
        cost,
        warnings,
        events: recent,
        host: history.host().to_string(),
        sessions_off: !read_sessions,
    })
}

/// Whether the stored price list holds an Anthropic model at all: a list that
/// was never looked up, or whose look-up is off, cannot price any Claude work.
fn has_anthropic_prices(prices: &Prices) -> bool {
    let prefix = format!("{}/", crate::money::Company::Anthropic.price_list());
    prices.models.keys().any(|key| key.starts_with(&prefix))
}

/// The answers with thinking the running sessions hold, per account that
/// produced them (spec §2.12): each minute's count goes to the account that was
/// live on `host` at the minute's start ([`Timeline::of`]). A minute without a
/// known live account is left out; the map has no entry for an account without
/// thinking.
pub fn thinking_held(
    thinking: &[(DateTime<Utc>, u32)],
    events: &[Hosted<Event>],
    host: &str,
) -> BTreeMap<AccountRef, u32> {
    let timeline = Timeline::of(events, host);
    let mut held: BTreeMap<AccountRef, u32> = BTreeMap::new();
    for (minute, count) in thinking {
        let Some(account) = timeline.at(*minute) else {
            continue;
        };
        held.entry(account.clone())
            .and_modify(|sum| *sum = sum.saturating_add(*count))
            .or_insert(*count);
    }
    held
}

/// Turn the views into [`Inputs`], check them, and derive the two hold times
/// from the learned events (spec §2.5, §2.12).
///
/// A logged-out account is in the inputs when it has a reading, with
/// `login_ok` false: the plan treats it as blocked. An account with a login but
/// no reading makes the whole situation [`Readiness::Unread`]; a logged-out
/// account without a reading is left out. When no account at all can go into
/// the inputs, the situation is [`Readiness::NoAccounts`].
///
/// `thinking` is every running tied session's answers with thinking since its
/// user turn; each account's [`AccountState::thinking_held`] is what it produced
/// of them, 0 when it produced none.
#[allow(clippy::too_many_arguments)]
pub fn situation(
    learned: &Learned,
    views: &[AccountView],
    accounts: &[Account],
    active: Option<&str>,
    now: DateTime<Utc>,
    offset: FixedOffset,
    tied_busy: u32,
    thinking: &[(DateTime<Utc>, u32)],
) -> Result<Readiness, PlanError> {
    let mut unread = Vec::new();
    let mut states = Vec::new();
    let mut oldest_reading: Option<DateTime<Utc>> = None;
    let held = thinking_held(thinking, &learned.events, &learned.host);

    for account in accounts {
        let view = views.iter().find(|view| view.id == account.id);
        let logged_out = view.is_some_and(|view| view.login_state == LoginState::LoggedOut);
        match view.and_then(reading_of) {
            Some((week, week_resets_at, session, fetched_at)) => {
                let login_ok = view.is_some_and(|view| view.login_state == LoginState::Ok);
                states.push(AccountState {
                    id: account.id.clone(),
                    may_use: account.auto_burn,
                    login_ok,
                    week_percent: week.percent,
                    week_resets_at,
                    session,
                    model_blocked: view.is_some_and(model_blocked),
                    rates: learned.rates.get(&AccountRef::of(account)).copied(),
                    thinking_held: held.get(&AccountRef::of(account)).copied().unwrap_or(0),
                });
                if login_ok && (account.auto_burn || active == Some(account.id.as_str())) {
                    oldest_reading = Some(match oldest_reading {
                        Some(oldest) => oldest.min(fetched_at),
                        None => fetched_at,
                    });
                }
            }
            None if !logged_out && (account.auto_burn || active == Some(account.id.as_str())) => {
                unread.push(account.id.clone())
            }
            None => {}
        }
    }

    if !unread.is_empty() {
        return Ok(Readiness::Unread { accounts: unread });
    }
    let Some(oldest_reading) = oldest_reading else {
        // No account with a login has a reading: there is nothing to decide
        // from (every account is logged out, or there are none).
        return Ok(Readiness::NoAccounts);
    };

    let last_switch_at = last_switch_at(learned, now);
    let manual_hold_until = manual_hold_until(learned, accounts, &states, now);

    let situation = Situation {
        inputs: Inputs {
            now,
            offset,
            accounts: states,
            active: active.map(str::to_string),
            rhythm: learned.rhythm.clone(),
            last_switch_at,
            manual_hold_until,
        },
        cost: learned.cost,
        tied_busy,
        oldest_reading,
        sessions_off: learned.sessions_off,
    };
    situation.check()?;
    Ok(Readiness::Ready(situation))
}

/// The account auto-burn would switch to at this reading: the target of a
/// [`Decision::Switch`] or [`Decision::Wait`], otherwise the first account of
/// the plan other than the active one, otherwise `None`. It is the one
/// rule `BurnView` and `status --json`'s `next` use.
pub fn next_account(situation: &Situation, decision: &Decision, plan: &Plan) -> Option<String> {
    match decision {
        Decision::Switch { to, .. } | Decision::Wait { to, .. } => Some(to.clone()),
        Decision::Stay | Decision::Stale | Decision::AllFull { .. } => {
            plan.segments.iter().find_map(|segment| {
                let id = segment.account.as_deref()?;
                if Some(id) == situation.inputs.active.as_deref() {
                    None
                } else {
                    Some(id.to_string())
                }
            })
        }
    }
}

/// The whole shared assembly after [`situation`]: the regime, the decision, the
/// plan auto-burn follows and the account the plan names next. The tray app and
/// both CLI commands use it, so they cannot take the steps in a different order.
pub struct Assessment {
    pub situation: Situation,
    pub regime: Regime,
    pub decision: Decision,
    pub plan: Plan,
    pub next: Option<String>,
}

/// Assemble one decision from a checked situation: regime → decide →
/// [`followed`] → next. `cached_regime` is a regime an earlier call computed;
/// `None` computes a new one. Keeping that cache is the tray app's own job.
pub fn assess(situation: Situation, cached_regime: Option<&Regime>) -> Assessment {
    let regime = match cached_regime {
        Some(regime) => regime.clone(),
        None => regime(&situation),
    };
    let decision = decide(&situation, &regime);
    let plan = followed(&situation.inputs, &regime);
    let next = next_account(&situation, &decision, &plan);
    Assessment {
        situation,
        regime,
        decision,
        plan,
        next,
    }
}

/// The weekly limit and the session state of a view, plus when the reading was
/// fetched. `None` when there is no usage, no weekly limit with a reset, or no
/// fetch time: a value auto-burn cannot plan from.
type Reading<'a> = (
    &'a Limit,
    DateTime<Utc>,
    Option<SessionState>,
    DateTime<Utc>,
);

fn reading_of(view: &AccountView) -> Option<Reading<'_>> {
    let fetched_at = view.fetched_at?;
    let usage = view.usage.as_ref()?;
    let week = usage.weekly_all()?;
    let week_resets_at = week.resets_at?;
    let session = usage.session().map(|session| SessionState {
        percent: session.percent,
        resets_at: session.resets_at,
    });
    Some((week, week_resets_at, session, fetched_at))
}

fn model_blocked(view: &AccountView) -> bool {
    matches!(&view.blocker, Some(Blocker::Model(_)))
}

/// The last `Switch` event of this host, of any `by`.
fn last_switch_at(learned: &Learned, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    learned
        .events
        .iter()
        .filter(|hosted| hosted.host == learned.host && hosted.record.t <= now)
        .filter(|hosted| matches!(hosted.record.kind, EventKind::Switch { .. }))
        .map(|hosted| hosted.record.t)
        .max()
}

/// The latest of a manual switch's [`MANUAL_HOLD_H`] hold and an `AutoBurn`
/// pause, less the manual hold that ended because its account was blocked.
/// Any switch auto-burn did not make starts the hold (`by` other than `Auto`).
/// A hold ends at the first time its account is blocked after the switch — the
/// history's [`EventKind::Blocked`] of that account whose kind
/// `LimitKind::blocks_whole_account` names (the 5 h session or the week of all
/// models; a per-model weekly limit at 100 % is not an account block, spec §7.2),
/// or its state now when that is already past — and stays ended. A resume
/// (`paused_until: None`) ends a
/// manual hold that came before it. An `AutoBurnSet` (a settings toggle) is not
/// a pause or a resume and changes neither.
fn manual_hold_until(
    learned: &Learned,
    accounts: &[Account],
    states: &[AccountState],
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut manual: Option<(DateTime<Utc>, AccountRef)> = None;
    let mut paused: Option<DateTime<Utc>> = None;
    for hosted in &learned.events {
        if hosted.host != learned.host || hosted.record.t > now {
            continue;
        }
        match &hosted.record.kind {
            EventKind::Switch { to, by, .. } if *by != By::Auto => {
                manual = Some((hosted.record.t, to.clone()));
            }
            EventKind::AutoBurn { paused_until, .. } => {
                paused = *paused_until;
                if paused_until.is_none() {
                    manual = None;
                }
            }
            _ => {}
        }
    }

    let manual_until = manual.and_then(|(at, to)| {
        let until = at + Duration::hours(MANUAL_HOLD_H);
        if until <= now {
            return None;
        }
        let id = accounts
            .iter()
            .find(|account| AccountRef::of(account) == to)
            .map(|account| account.id.as_str());
        let blocked_event = learned
            .events
            .iter()
            .filter(|hosted| hosted.host == learned.host && hosted.record.t > at)
            .filter(|hosted| hosted.record.t <= now)
            .filter_map(|hosted| match &hosted.record.kind {
                EventKind::Blocked { account, k, .. }
                    if *account == to && k.blocks_whole_account() =>
                {
                    Some(hosted.record.t)
                }
                _ => None,
            })
            .min();
        let blocked_now = id
            .and_then(|id| states.iter().find(|state| state.id == id))
            .is_some_and(|state| blocked(state, now))
            .then_some(now);
        let ended = match (blocked_event, blocked_now) {
            (Some(event), Some(state)) => Some(event.min(state)),
            (Some(event), None) => Some(event),
            (None, Some(state)) => Some(state),
            (None, None) => None,
        };
        let until = ended.map_or(until, |ended| until.min(ended));
        (until > now).then_some(until)
    });
    let paused_until = paused.filter(|until| *until > now);
    manual_until.max(paused_until)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::FixedOffset;

    use crate::api::{Limit, LimitKind, Severity, Usage};
    use crate::burn::decide::{Reason, RegimeWhy};
    use crate::burn::plan::{follow, followed, usable, usable_again, Policy};
    use crate::history::{Recorder, Source, TokenRecord};
    use crate::project::ProjectKey;
    use crate::stats::tests::{append_codex_records, tiny};
    use crate::types::OAuthAccount;

    const ORG: &str = "e92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20";
    const PR_ACC: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const AC_ACC: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const LB_ACC: &str = "1c4a8e2f-7b3d-4a6c-8e0f-2b4d6a8c0e1f";
    const RESET: &str = "2026-11-16T02:00:00Z";

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a test time")
            .with_timezone(&Utc)
    }

    fn offset() -> FixedOffset {
        FixedOffset::east_opt(0).expect("UTC")
    }

    fn account(id: &str, abbr: &str, acc: &str, auto_burn: bool) -> Account {
        Account {
            id: id.to_string(),
            account_uuid: acc.to_string(),
            organization_uuid: ORG.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            org_name: None,
            plan: Some("Team".to_string()),
            oauth_account: OAuthAccount(serde_json::Map::new()),
            added_at: utc("2026-11-01T00:00:00Z"),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            auto_burn,
            price: None,
            price_currency: None,
        }
    }

    fn limit(kind: LimitKind, percent: f64, resets_at: Option<DateTime<Utc>>) -> Limit {
        Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at,
            model: None,
            is_active: false,
        }
    }

    fn usage(week: f64, session: Option<f64>, reset: DateTime<Utc>) -> Usage {
        let mut limits = vec![limit(LimitKind::WeeklyAll, week, Some(reset))];
        if let Some(session) = session {
            limits.push(limit(LimitKind::Session, session, Some(utc(RESET))));
        }
        Usage {
            limits,
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn view(
        id: &str,
        abbr: &str,
        week: f64,
        session: Option<f64>,
        fetched_at: DateTime<Utc>,
    ) -> AccountView {
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
            usage: Some(usage(week, session, utc(RESET))),
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: Some(fetched_at),
            last_error: None,
        }
    }

    fn learned(host: &str, events: Vec<Hosted<Event>>) -> Learned {
        Learned {
            rhythm: Rhythm { usd: [0.0; 168] },
            rates: BTreeMap::new(),
            cost: None,
            warnings: Vec::new(),
            events,
            host: host.to_string(),
            sessions_off: false,
        }
    }

    fn switch_event(host: &str, at: DateTime<Utc>, to: AccountRef, by: By) -> Hosted<Event> {
        Hosted {
            host: host.to_string(),
            record: Event {
                t: at,
                kind: EventKind::Switch {
                    from: None,
                    to,
                    by,
                    reason: None,
                    cwd: None,
                    sessions: None,
                    waited_secs: None,
                },
            },
        }
    }

    fn blocked_event(
        host: &str,
        at: DateTime<Utc>,
        account: AccountRef,
        kind: LimitKind,
        model: Option<&str>,
    ) -> Hosted<Event> {
        Hosted {
            host: host.to_string(),
            record: Event {
                t: at,
                kind: EventKind::Blocked {
                    account,
                    k: kind,
                    m: model.map(str::to_string),
                },
            },
        }
    }

    fn ready(
        learned: &Learned,
        views: &[AccountView],
        accounts: &[Account],
        active: Option<&str>,
        now: DateTime<Utc>,
    ) -> Situation {
        match situation(learned, views, accounts, active, now, offset(), 0, &[])
            .expect("the views are valid")
        {
            Readiness::Ready(situation) => situation,
            Readiness::Unread { accounts } => panic!("unread accounts: {accounts:?}"),
            Readiness::NoAccounts => panic!("no account can go into the inputs"),
        }
    }

    #[test]
    fn thinking_goes_to_the_account_live_in_its_minute() {
        let lb = account("lb", "LB", LB_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let events = vec![
            switch_event(
                "atlas",
                utc("2026-11-10T09:00:00Z"),
                AccountRef::of(&lb),
                By::Auto,
            ),
            switch_event(
                "atlas",
                utc("2026-11-10T10:03:40Z"),
                AccountRef::of(&ac),
                By::Auto,
            ),
        ];
        let thinking = vec![
            (utc("2026-11-10T09:50:00Z"), 4),
            (utc("2026-11-10T10:03:00Z"), 2),
            (utc("2026-11-10T10:04:00Z"), 3),
        ];

        let held = thinking_held(&thinking, &events, "atlas");

        assert_eq!(held.get(&AccountRef::of(&lb)), Some(&6));
        assert_eq!(held.get(&AccountRef::of(&ac)), Some(&3));
    }

    #[test]
    fn thinking_without_a_known_account_is_left_out() {
        let lb = account("lb", "LB", LB_ACC, true);
        let events = vec![switch_event(
            "atlas",
            utc("2026-11-10T09:00:00Z"),
            AccountRef::of(&lb),
            By::Auto,
        )];

        // 08:00 lies before the first event of the host.
        let before = thinking_held(&[(utc("2026-11-10T08:00:00Z"), 4)], &events, "atlas");
        assert!(before.is_empty(), "{before:?}");

        // The same switches, but reported by another host.
        let thinking = vec![(utc("2026-11-10T09:50:00Z"), 4)];
        let other_host = thinking_held(&thinking, &events, "vega");
        assert!(other_host.is_empty(), "{other_host:?}");
    }

    #[test]
    fn situation_fills_thinking_held() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let lb = account("lb", "LB", LB_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let events = vec![
            switch_event(
                "atlas",
                utc("2026-11-10T09:00:00Z"),
                AccountRef::of(&lb),
                By::Auto,
            ),
            switch_event(
                "atlas",
                utc("2026-11-10T10:03:40Z"),
                AccountRef::of(&ac),
                By::Auto,
            ),
        ];
        let learned = learned("atlas", events);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("lb", "LB", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, None, now - Duration::minutes(1)),
        ];
        let thinking = vec![
            (utc("2026-11-10T09:50:00Z"), 4),
            (utc("2026-11-10T10:03:00Z"), 2),
            (utc("2026-11-10T10:04:00Z"), 3),
        ];

        let readiness = situation(
            &learned,
            &views,
            &[pr, lb, ac],
            Some("pr"),
            now,
            offset(),
            0,
            &thinking,
        )
        .expect("the views are valid");
        let Readiness::Ready(situation) = readiness else {
            panic!("the accounts have readings");
        };
        let held = |id: &str| {
            situation
                .inputs
                .accounts
                .iter()
                .find(|state| state.id == id)
                .map(|state| state.thinking_held)
                .expect("the account is in the inputs")
        };
        assert_eq!(held("lb"), 6);
        assert_eq!(held("ac"), 3);
        assert_eq!(held("pr"), 0);
    }

    #[test]
    fn a_manual_hold_ends_when_its_account_is_blocked() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        // AC was switched to by hand an hour ago and is now at 100 % of its week.
        let event = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::User,
        );
        let learned = learned("atlas", vec![event]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 100.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until, None,
            "the blocked account ends the hold early"
        );
        assert_eq!(
            situation.inputs.last_switch_at,
            Some(now - Duration::hours(1))
        );
    }

    /// AC's per-model Sonnet week crosses 100 % while all work runs on Opus: the
    /// account is not blocked, so a manual switch to AC keeps its hold (spec
    /// §7.2, "Usable").
    #[test]
    fn a_per_model_weekly_block_keeps_the_manual_hold() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::User,
        );
        let blocked = blocked_event(
            "atlas",
            now - Duration::minutes(30),
            AccountRef::of(&ac),
            LimitKind::WeeklyScoped,
            Some("sonnet"),
        );
        let learned = learned("atlas", vec![switch, blocked]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(3)),
            "a per-model weekly limit is not an account block"
        );
    }

    /// AC's 5 h session is full after the manual switch: that is an account
    /// block, so the hold ends at the event.
    #[test]
    fn a_session_block_ends_the_manual_hold() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::User,
        );
        let blocked = blocked_event(
            "atlas",
            now - Duration::minutes(30),
            AccountRef::of(&ac),
            LimitKind::Session,
            None,
        );
        let learned = learned("atlas", vec![switch, blocked]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until, None,
            "a full session ends the hold"
        );
    }

    #[test]
    fn a_hold_ended_by_a_full_account_does_not_come_back() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        // AC was switched to by hand two hours ago, was blocked an hour ago,
        // and its window has reset since: the old rule would start the hold
        // again because AC is not blocked at this reading.
        let switch = switch_event(
            "atlas",
            now - Duration::hours(2),
            AccountRef::of(&ac),
            By::User,
        );
        let blocked = blocked_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            LimitKind::WeeklyAll,
            None,
        );
        let learned = learned("atlas", vec![switch, blocked]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until, None,
            "the hold ended at the blocked event and stays ended"
        );
    }

    #[test]
    fn an_external_switch_starts_the_hold() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::External,
        );
        let learned = learned("atlas", vec![switch]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(3)),
            "a /login outside seatswap holds the early rule back"
        );
    }

    #[test]
    fn without_reading_sessions_no_rates_are_learned() {
        let now = utc("2026-11-14T18:00:00Z");
        let ac = account("ac", "AC", AC_ACC, true);
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let mut recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        let account_ref = AccountRef::of(&ac);
        recorder
            .event(
                utc("2026-11-14T08:00:00Z"),
                EventKind::Live {
                    account: Some(account_ref.clone()),
                },
            )
            .expect("the live event");
        recorder
            .reading(
                &account_ref,
                None,
                &usage(40.0, None, utc("2026-11-16T02:00:00Z")),
                utc("2026-11-14T09:00:00Z"),
                Source::Poll,
            )
            .expect("the first reading");
        recorder
            .reading(
                &account_ref,
                None,
                &usage(48.0, None, utc("2026-11-16T02:00:00Z")),
                utc("2026-11-14T13:00:00Z"),
                Source::Poll,
            )
            .expect("the second reading");
        recorder
            .tokens(&[TokenRecord {
                t: utc("2026-11-14T10:00:00Z"),
                minutes: 60,
                session: "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string(),
                cwd: "/home/sam/projects/seatswap".to_string(),
                project: ProjectKey::Folder("seatswap".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 30_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            }])
            .expect("the tokens");

        // 30 million input tokens of claude-opus-5-5 are 120 USD of work.
        let prices = crate::money::tests::priced_prices(now);
        let with_sessions = learn(
            &history,
            now,
            offset(),
            std::slice::from_ref(&ac),
            true,
            &prices,
        )
        .expect("the history reads");
        assert!(
            with_sessions.rates.contains_key(&account_ref),
            "the readings are enough rates when sessions are read"
        );
        let without_sessions = learn(
            &history,
            now,
            offset(),
            std::slice::from_ref(&ac),
            false,
            &prices,
        )
        .expect("the history reads");
        assert!(
            without_sessions.rates.is_empty(),
            "readSessions off learns no rates"
        );
    }

    /// The rhythm leaves unpriced work out whether or not sessions are read,
    /// so its model is named in both cases: the warning cannot depend on the
    /// `rates` pass alone.
    #[test]
    fn an_unpriced_model_is_named_even_without_reading_sessions() {
        let now = utc("2026-11-14T18:00:00Z");
        let ac = account("ac", "AC", AC_ACC, true);
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        recorder
            .tokens(&[TokenRecord {
                t: utc("2026-11-14T10:00:00Z"),
                minutes: 60,
                session: "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string(),
                cwd: "/home/sam/projects/seatswap".to_string(),
                project: ProjectKey::Folder("seatswap".to_string()),
                model: "claude-new-9".to_string(),
                input: 30_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            }])
            .expect("the tokens");
        let prices = crate::money::tests::priced_prices(now);

        let learned = learn(
            &history,
            now,
            offset(),
            std::slice::from_ref(&ac),
            false,
            &prices,
        )
        .expect("the history reads");
        assert!(learned.rates.is_empty(), "no rates are learned");
        assert_eq!(
            learned.warnings,
            vec![
                "no price for claude-new-9: its work is left out of auto-burn's rates and rhythm"
                    .to_string()
            ]
        );
    }

    /// A list that was never looked up has no Anthropic model at all: one
    /// sentence says so, instead of one warning for each model met.
    #[test]
    fn a_learn_without_prices_says_so_once() {
        let now = utc("2026-11-14T18:00:00Z");
        let ac = account("ac", "AC", AC_ACC, true);
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        recorder
            .tokens(&[TokenRecord {
                t: utc("2026-11-14T10:00:00Z"),
                minutes: 60,
                session: "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d".to_string(),
                cwd: "/home/sam/projects/seatswap".to_string(),
                project: ProjectKey::Folder("seatswap".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 30_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            }])
            .expect("the priced model's tokens");
        recorder
            .tokens(&[TokenRecord {
                t: utc("2026-11-14T11:00:00Z"),
                minutes: 60,
                session: "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e".to_string(),
                cwd: "/home/sam/projects/seatswap".to_string(),
                project: ProjectKey::Folder("seatswap".to_string()),
                model: "claude-new-9".to_string(),
                input: 30_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            }])
            .expect("the unpriced model's tokens");

        let learned = learn(
            &history,
            now,
            offset(),
            std::slice::from_ref(&ac),
            true,
            &Prices::default(),
        )
        .expect("the history reads");
        assert_eq!(
            learned.warnings,
            vec![
                "auto-burn has no API prices yet: it measures work in API dollars and needs the price list (Settings → General → Look up prices)"
                    .to_string()
            ],
            "one sentence for the missing list, not one per model"
        );
    }

    #[test]
    fn an_account_without_a_reading_makes_the_situation_stale() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let learned = learned("atlas", Vec::new());
        let mut no_reading = view("ac", "AC", 10.0, None, now - Duration::minutes(1));
        no_reading.usage = None;
        no_reading.fetched_at = None;
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            no_reading,
        ];
        let readiness = situation(
            &learned,
            &views,
            &[pr, ac],
            Some("pr"),
            now,
            offset(),
            0,
            &[],
        )
        .expect("the accounts are valid");
        match readiness {
            Readiness::Unread { accounts } => assert_eq!(accounts, vec!["ac".to_string()]),
            Readiness::Ready(_) => panic!("AC has no reading"),
            Readiness::NoAccounts => panic!("PR still goes into the inputs"),
        }
    }

    #[test]
    fn a_manual_hold_lasts_four_hours() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::Cli,
        );
        let learned = learned("atlas", vec![switch]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(3))
        );
    }

    #[test]
    fn an_unread_account_auto_burn_may_not_use_is_left_out() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, false);
        let learned = learned("atlas", Vec::new());
        let mut no_reading = view("ac", "AC", 10.0, None, now - Duration::minutes(1));
        no_reading.usage = None;
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            no_reading,
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("pr"), now);
        assert_eq!(situation.inputs.accounts.len(), 1);
        assert_eq!(situation.inputs.accounts[0].id, "pr");
    }

    #[test]
    fn the_plan_starts_on_the_active_account() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let learned = learned("atlas", Vec::new());
        let views = vec![view("pr", "PR", 10.0, None, now - Duration::minutes(1))];
        let situation = ready(&learned, &views, &[pr], Some("pr"), now);
        let plan = follow(&situation.inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("pr"));
    }

    #[test]
    fn a_logged_out_account_does_not_make_the_situation_stale() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let mut logged_out = view("pr", "PR", 20.0, None, now - Duration::hours(3));
        logged_out.login_state = LoginState::LoggedOut;
        let fresh = view("ac", "AC", 10.0, None, now - Duration::minutes(2));
        let learned = learned("atlas", Vec::new());
        let views = vec![logged_out, fresh];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.oldest_reading,
            now - Duration::minutes(2),
            "the logged-out account's old reading is not counted"
        );
        let pr_state = situation
            .inputs
            .accounts
            .iter()
            .find(|state| state.id == "pr")
            .expect("the logged-out account with a reading is in the inputs");
        assert!(!pr_state.login_ok);
    }

    #[test]
    fn the_oldest_reading_counts_the_usable_and_the_active_accounts() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        // Auto-burn may not use AC and it is not active: its old reading does
        // not make every decision stale.
        let ac = account("ac", "AC", AC_ACC, false);
        let learned = learned("atlas", Vec::new());
        let views = vec![
            view("pr", "PR", 20.0, None, now - Duration::minutes(5)),
            view("ac", "AC", 10.0, None, now - Duration::minutes(30)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("pr"), now);
        assert_eq!(situation.oldest_reading, now - Duration::minutes(5));
    }

    #[test]
    fn no_account_is_no_accounts_not_unread() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let mut no_pr = view("pr", "PR", 10.0, None, now - Duration::minutes(1));
        no_pr.login_state = LoginState::LoggedOut;
        no_pr.usage = None;
        no_pr.fetched_at = None;
        let mut no_ac = view("ac", "AC", 10.0, None, now - Duration::minutes(1));
        no_ac.login_state = LoginState::LoggedOut;
        no_ac.usage = None;
        no_ac.fetched_at = None;
        let learned = learned("atlas", Vec::new());
        let readiness = situation(
            &learned,
            &[no_pr, no_ac],
            &[pr, ac],
            None,
            now,
            offset(),
            0,
            &[],
        )
        .expect("the accounts are valid");
        match readiness {
            Readiness::NoAccounts => {}
            Readiness::Ready(_) => panic!("no account can go into the inputs"),
            Readiness::Unread { accounts } => {
                panic!("logged-out accounts are not unread: {accounts:?}")
            }
        }
    }

    #[test]
    fn resume_ends_a_manual_hold() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::User,
        );
        let resume = Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: now - Duration::minutes(30),
                kind: EventKind::AutoBurn {
                    on: true,
                    paused_until: None,
                },
            },
        };
        let learned = learned("atlas", vec![switch, resume]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, None, now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(situation.inputs.manual_hold_until, None);
    }

    #[test]
    fn a_toggle_does_not_end_a_manual_hold_or_a_pause() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, None, now - Duration::minutes(1)),
        ];
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::User,
        );
        let toggle = Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: now - Duration::minutes(30),
                kind: EventKind::AutoBurnSet { on: true },
            },
        };
        let first_learned = learned("atlas", vec![switch, toggle]);
        let situation = ready(
            &first_learned,
            &views,
            &[pr.clone(), ac.clone()],
            Some("ac"),
            now,
        );
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(3)),
            "a settings toggle is not a resume of the manual switch's hold"
        );

        let pause = Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: now - Duration::minutes(45),
                kind: EventKind::AutoBurn {
                    on: true,
                    paused_until: Some(now + Duration::hours(2)),
                },
            },
        };
        let toggle = Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: now - Duration::minutes(30),
                kind: EventKind::AutoBurnSet { on: false },
            },
        };
        let learned = learned("atlas", vec![pause, toggle]);
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(2)),
            "a settings toggle is not a resume of the explicit pause"
        );
    }

    #[test]
    fn next_account_is_the_decisions_target_then_the_plans() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let mut learned = learned("atlas", Vec::new());
        // Every hour has work, so PR at 99.5 % fills in the plan's first step
        // and the lazy plan moves to AC. AC at 99 % has no 5 h window here and
        // needs 0.25 USD of the work before its reset, so its slack is far above
        // the cushion and the lazy plan does not move to it before PR is full.
        learned.rhythm = Rhythm { usd: [1.0; 168] };
        learned.rates = BTreeMap::from([
            (AccountRef::of(&pr), Rates::given(2.0, None)),
            (AccountRef::of(&ac), Rates::given(4.0, Some(100.0))),
        ]);
        let views = vec![
            view("pr", "PR", 99.5, None, now - Duration::minutes(1)),
            view("ac", "AC", 99.0, None, now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("pr"), now);
        let plan = follow(&situation.inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("pr"));
        assert_eq!(plan.segments[1].account.as_deref(), Some("ac"));

        let switch = Decision::Switch {
            to: "lb".to_string(),
            reason: Reason::WindowOpen,
        };
        assert_eq!(
            next_account(&situation, &switch, &plan).as_deref(),
            Some("lb")
        );
        let wait = Decision::Wait {
            to: "lb".to_string(),
            latest: now + Duration::hours(1),
            tied_busy: 1,
            reason: Reason::WindowOpen,
        };
        assert_eq!(
            next_account(&situation, &wait, &plan).as_deref(),
            Some("lb")
        );
        assert_eq!(
            next_account(&situation, &Decision::Stay, &plan).as_deref(),
            Some("ac"),
            "without a target, the first other account of the lazy plan is next"
        );
        assert_eq!(
            next_account(&situation, &Decision::AllFull { next: None }, &plan).as_deref(),
            Some("ac")
        );
    }

    #[test]
    fn paused_until_is_the_situations_hold() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let switch = switch_event(
            "atlas",
            now - Duration::hours(1),
            AccountRef::of(&ac),
            By::Cli,
        );
        let learned = learned("atlas", vec![switch]);
        let views = vec![
            view("pr", "PR", 10.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, Some(10.0), now - Duration::minutes(1)),
        ];
        let situation = ready(&learned, &views, &[pr, ac], Some("ac"), now);
        assert_eq!(
            situation.inputs.manual_hold_until,
            Some(now + Duration::hours(3)),
            "a CLI switch one hour ago holds for four hours"
        );
    }

    fn early_regime(early: bool) -> Regime {
        Regime {
            early,
            why: if early {
                RegimeWhy::Pays
            } else {
                RegimeWhy::NoCostYet
            },
            lazy_blocked_h: 0.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: None,
        }
    }

    fn assess_situation(inputs: Inputs) -> Situation {
        Situation {
            oldest_reading: inputs.now - Duration::minutes(1),
            inputs,
            cost: None,
            tied_busy: 0,
            sessions_off: false,
        }
    }

    /// The stored accounts the plan's ids name: PR, AC and LB, as the
    /// Forecast's own tests build them.
    fn stored_plan_accounts(tiny: &crate::stats::tests::Tiny) -> Vec<Account> {
        let mut pr = tiny.lb().clone();
        pr.id = "PR".to_string();
        pr.label = "PR".to_string();
        pr.abbr = "PR".to_string();
        pr.account_uuid = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d".to_string();
        pr.organization_uuid = "1b2c3d4e-5f6a-4b7c-9d0e-1f2a3b4c5d6e".to_string();
        pr.price = Some(100.0);
        pr.price_currency = Some("EUR".to_string());

        let mut ac = tiny.ac().clone();
        ac.id = "AC".to_string();
        ac.label = "AC".to_string();
        ac.abbr = "AC".to_string();

        let mut lb = tiny.lb().clone();
        lb.id = "LB".to_string();
        lb.label = "LB".to_string();
        lb.abbr = "LB".to_string();

        vec![pr, ac, lb]
    }

    /// The plan's next account and the start of its first switch away from the
    /// active account.
    fn next_and_first_switch(
        plan: &Plan,
        active: Option<&str>,
    ) -> (Option<String>, Option<DateTime<Utc>>) {
        let next = plan.segments.iter().find_map(|segment| {
            let id = segment.account.as_deref()?;
            (Some(id) != active).then(|| id.to_string())
        });
        let first = plan
            .segments
            .iter()
            .find(|segment| segment.account.as_deref() != active)
            .map(|segment| segment.from);
        (next, first)
    }

    #[test]
    fn with_early_switching_assess_follows_the_forecasts_plan() {
        let tiny = crate::stats::tests::tiny();
        let accounts = stored_plan_accounts(&tiny);
        let ctx = crate::stats::Ctx {
            accounts: &accounts,
            ..tiny.ctx()
        };
        let inputs = crate::burn::plan::testkit::calm();
        let early = early_regime(true);
        let active = inputs.active.as_deref();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert_ne!(
            next_and_first_switch(&lazy, active),
            next_and_first_switch(&eager, active),
            "the lazy and eager plans name a different next account or first switch time"
        );

        let assessment = assess(assess_situation(inputs.clone()), Some(&early));
        assert_eq!(
            assessment.plan,
            followed(&assessment.situation.inputs, &early)
        );
        assert_eq!(assessment.plan, eager);
        assert_eq!(assessment.next, next_and_first_switch(&eager, active).0);

        let forecast =
            crate::stats::forecast::forecast(&ctx, &inputs, &early).expect("the forecast is built");
        let mut blocks: Vec<(Option<String>, DateTime<Utc>, DateTime<Utc>)> = forecast
            .lanes
            .iter()
            .flat_map(|lane| {
                lane.blocks
                    .iter()
                    .map(move |(from, to)| (Some(lane.id.clone()), *from, *to))
            })
            .collect();
        let mut segments: Vec<(Option<String>, DateTime<Utc>, DateTime<Utc>)> = assessment
            .plan
            .segments
            .iter()
            .filter(|segment| segment.account.is_some())
            .map(|segment| (segment.account.clone(), segment.from, segment.to))
            .collect();
        blocks.sort();
        segments.sort();
        assert_eq!(
            blocks, segments,
            "the Forecast's lanes follow the assessed plan"
        );
        assert_eq!(forecast.kpis.switches, assessment.plan.switches);
        let blocked: Vec<(DateTime<Utc>, DateTime<Utc>)> = assessment
            .plan
            .segments
            .iter()
            .filter(|segment| segment.account.is_none())
            .map(|segment| (segment.from, segment.to))
            .collect();
        assert_eq!(forecast.blocked, blocked);
    }

    #[test]
    fn without_early_switching_assess_follows_the_lazy_plan() {
        let inputs = crate::burn::plan::testkit::inputs();
        let late = early_regime(false);
        let assessment = assess(assess_situation(inputs.clone()), Some(&late));
        assert_eq!(
            assessment.plan,
            follow(&assessment.situation.inputs, Policy::Lazy)
        );
    }

    #[test]
    fn provider_records_do_not_change_what_auto_burn_learns() {
        let without = tiny();
        let with = tiny();
        append_codex_records(&with.history, utc("2026-11-14T09:00:00Z"));

        let before = learn(
            &without.history,
            without.now,
            without.offset,
            &without.accounts,
            true,
            &without.prices,
        )
        .expect("the history reads");
        let after = learn(
            &with.history,
            with.now,
            with.offset,
            &with.accounts,
            true,
            &with.prices,
        )
        .expect("the history reads");

        assert_eq!(
            after.rhythm, before.rhythm,
            "a Codex sample changed the rhythm"
        );
        assert_eq!(after.cost, before.cost, "a Codex record changed the cost");
        assert_eq!(
            after.events.len(),
            before.events.len(),
            "a Codex event reached auto-burn"
        );
        assert_eq!(after.rates.len(), before.rates.len());
        for (account, rates) in &before.rates {
            let learned = after.rates.get(account).expect("the account's rates");
            assert_eq!(learned.c, rates.c);
            assert_eq!(learned.s, rates.s);
        }
    }

    /// AC's week is reset by hand from 100 % to 0 % days before its reset time
    /// (a reset event, decision 12 of the 1.2.0 plan): the next poll's reading
    /// is all the plan and the "usable again" notification need. The
    /// notification reads the engine's `usable`, which is [`crate::engine::blocker`].
    #[test]
    fn a_reset_account_is_usable_at_once() {
        let now = utc("2026-11-10T12:00:00Z");
        let pr = account("pr", "PR", PR_ACC, true);
        let ac = account("ac", "AC", AC_ACC, true);
        let learned = learned("atlas", Vec::new());
        let state = |situation: &Situation, id: &str| {
            situation
                .inputs
                .accounts
                .iter()
                .find(|state| state.id == id)
                .cloned()
                .expect("the account is in the inputs")
        };

        let before = vec![
            view("pr", "PR", 100.0, None, now - Duration::minutes(1)),
            view("ac", "AC", 100.0, None, now - Duration::minutes(1)),
        ];
        let full = ready(
            &learned,
            &before,
            &[pr.clone(), ac.clone()],
            Some("pr"),
            now,
        );
        let ac_full = state(&full, "ac");
        assert!(!usable(&ac_full, now));
        assert_eq!(usable_again(&ac_full, now), Some(utc(RESET)));
        assert_eq!(follow(&full.inputs, Policy::Lazy).segments[0].account, None);
        let usage = before[1].usage.as_ref().expect("a reading");
        assert!(crate::engine::blocker(usage, None, now).is_some());

        // The next poll: AC reads 0 %, its reset time unchanged.
        let next = now + Duration::minutes(1);
        let after = vec![
            view("pr", "PR", 100.0, None, next),
            view("ac", "AC", 0.0, None, next),
        ];
        let reset = ready(&learned, &after, &[pr, ac], Some("pr"), next);
        let ac_reset = state(&reset, "ac");
        assert_eq!(ac_reset.week_percent, 0.0);
        assert!(usable(&ac_reset, next));
        assert_eq!(usable_again(&ac_reset, next), None);
        assert_eq!(
            follow(&reset.inputs, Policy::Lazy).segments[0]
                .account
                .as_deref(),
            Some("ac")
        );
        let usage = after[1].usage.as_ref().expect("a reading");
        assert!(crate::engine::blocker(usage, None, next).is_none());
    }
}
