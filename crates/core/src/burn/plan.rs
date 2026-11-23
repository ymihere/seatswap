//! Slack, choice and the forward plans of auto-burn (spec §2.2–2.5): how much
//! work each account can still take before its reset, which account to use,
//! and what the next 168 hours look like when it is switched only when blocked
//! (when full), also when another account is at risk of losing quota (lazy),
//! or as early as it pays (eager).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, FixedOffset, Utc};

use crate::api::{Limit, LimitKind, Severity};
use crate::transcripts::hour_start;

use super::decide::Regime;
use super::{Rates, Rhythm, EXPIRY_CUSHION_H, HORIZON_H, MIN_DWELL_MIN, SLACK_MARGIN_H, STEP_MIN};

/// A 5 h window lasts this many hours (spec §2.1).
const WINDOW_H: i64 = 5;
/// The work of one step: a quarter of the work of the hour that holds it.
const STEP_FRACTION: f64 = STEP_MIN as f64 / 60.0;
/// Two slacks within this are equal: a smaller difference is rounding noise.
const SLACK_TIE: f64 = 1e-9;

/// An account's 5 h window at the moment of a decision (spec §2.1). A window
/// has a reset time only while it runs; `None` means no window is open.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    pub percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// One account as the plan needs it (spec §2.3).
#[derive(Debug, Clone)]
pub struct AccountState {
    pub id: String,
    pub may_use: bool,
    pub login_ok: bool,
    pub week_percent: f64,
    pub week_resets_at: DateTime<Utc>,
    /// `None`: the account has no 5 h limit at all. It is the only sign of a
    /// capped account: `rates.s` is `None` also while that rate is not learned
    /// yet, and such an account is played without its 5 h limit.
    pub session: Option<SessionState>,
    pub model_blocked: bool,
    pub rates: Option<Rates>,
    /// The answers with thinking that the running sessions hold and this
    /// account produced (spec §2.12): the sum of the minutes it was live for.
    pub thinking_held: u32,
}

/// Everything a plan is computed from. It is pure: no clock, no files, no
/// network (spec §2.4).
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: DateTime<Utc>,
    pub offset: FixedOffset,
    pub accounts: Vec<AccountState>,
    pub active: Option<String>,
    pub rhythm: Rhythm,
    /// The last switch of any kind on this host. No early move (S2) comes
    /// within [`MIN_DWELL_MIN`] of it, in [`crate::burn::decide::decide`] and in
    /// the eager plan alike; `None` counts as long ago.
    pub last_switch_at: Option<DateTime<Utc>>,
    /// The end of the hold a switch not made by auto-burn starts (spec §2.5):
    /// no early move before it.
    pub manual_hold_until: Option<DateTime<Utc>>,
}

/// A value [`Inputs::check`] rejects, because no plan can be computed from it.
/// Every error that concerns an account names it.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(
        "account {account}: its {limit} percent {percent} is not a finite number of at least 0"
    )]
    InvalidPercent {
        account: String,
        limit: &'static str,
        percent: f64,
    },
    #[error("account {account}: its session is at {percent} % without a reset time")]
    SessionWithoutReset { account: String, percent: f64 },
    #[error("the active account {id} is not in accounts")]
    UnknownActive { id: String },
    #[error("two accounts have the id {id}")]
    DuplicateAccount { id: String },
    #[error("last_switch_at {at} is after now {now}")]
    LastSwitchAfterNow {
        at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    #[error("oldest_reading {at} is after now {now}")]
    OldestReadingAfterNow {
        at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
}

impl Inputs {
    /// Checks the values every caller must reject before [`slack`], [`choose`]
    /// or [`follow`] (the tray app's auto-burn calls it first). An error names
    /// the account it concerns: a percent that is NaN, infinite or negative, a
    /// session percent above 0 without a reset time (spec §2.1: a window has a
    /// reset time only while it runs), an `active` id that is not in
    /// `accounts`, two accounts with the same id, or a last switch after `now`
    /// (a clock that went back, not a value to hold or pass).
    pub fn check(&self) -> Result<(), PlanError> {
        for (index, account) in self.accounts.iter().enumerate() {
            if self.accounts[..index]
                .iter()
                .any(|other| other.id == account.id)
            {
                return Err(PlanError::DuplicateAccount {
                    id: account.id.clone(),
                });
            }
        }
        for account in &self.accounts {
            if !valid_percent(account.week_percent) {
                return Err(PlanError::InvalidPercent {
                    account: account.id.clone(),
                    limit: "week",
                    percent: account.week_percent,
                });
            }
            if let Some(session) = &account.session {
                if !valid_percent(session.percent) {
                    return Err(PlanError::InvalidPercent {
                        account: account.id.clone(),
                        limit: "session",
                        percent: session.percent,
                    });
                }
                if session.percent > 0.0 && session.resets_at.is_none() {
                    return Err(PlanError::SessionWithoutReset {
                        account: account.id.clone(),
                        percent: session.percent,
                    });
                }
            }
        }
        if let Some(active) = &self.active {
            if !self.accounts.iter().any(|account| &account.id == active) {
                return Err(PlanError::UnknownActive { id: active.clone() });
            }
        }
        if let Some(at) = self.last_switch_at {
            if at > self.now {
                return Err(PlanError::LastSwitchAfterNow { at, now: self.now });
            }
        }
        Ok(())
    }
}

fn valid_percent(percent: f64) -> bool {
    percent.is_finite() && percent >= 0.0
}

/// The plans of spec §2.5. Auto-burn follows `Lazy` or `Eager`; `WhenFull` is
/// what the Forecast and the suggestion compare them with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Stay on the current account until it is not usable: "only when full".
    WhenFull,
    /// Stay on the current account until it is not usable, or until another
    /// account with clearly less slack has a slack of at most
    /// [`EXPIRY_CUSHION_H`] typical hours: it is at risk of losing quota at its
    /// weekly reset.
    Lazy,
    /// Also move to an account with clearly less slack while its slack is
    /// above that cushion.
    Eager,
}

/// One stretch of the plan played with one account. `account` is `None` while
/// nothing is usable; such a stretch blocks the work inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub account: Option<String>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// Which limit fills an account in the plan: its week or its 5 h window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Window,
    Week,
}

/// The next [`HORIZON_H`] hours played in [`STEP_MIN`] steps (spec §2.4).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub segments: Vec<Segment>,
    pub blocked_usd: f64,
    pub switches: u32,
    /// Week-% unused at each account's first weekly reset inside the horizon.
    pub left_at_reset: BTreeMap<String, f64>,
    /// The first step at which each account is full: its week or its 5 h
    /// window is at 100 %.
    pub full_at: BTreeMap<String, DateTime<Utc>>,
    /// Which limit made the account full at `full_at`.
    pub full_by: BTreeMap<String, Fill>,
}

/// Whether the account is blocked at `at` as far as its login and its limits say
/// (spec §2.3, §2.7): its login has ended, its week is at 100 % or more, its 5 h
/// window is at 100 % and has not ended, or a per-model limit blocks it.
/// `may_use` is not part of this: an account auto-burn may not use is still
/// unblocked while it can work, and is left only when [`blocked`] is true
/// (spec §2.6). This is the one copy of the limits rule; [`usable`] builds on it.
pub fn blocked(account: &AccountState, at: DateTime<Utc>) -> bool {
    View::of(account, at).blocked(at)
}

/// Whether the account can work at `at` (spec §2.3): auto-burn may use it and it
/// is not [`blocked`].
pub fn usable(account: &AccountState, at: DateTime<Utc>) -> bool {
    account.may_use && !blocked(account, at)
}

/// The time the account is usable again as far as its limits say (spec §2.5):
/// the latest reset among the limits that block at `at` — its weekly reset when
/// its week is at 100 %, its 5 h reset when its window is at 100 % and has not
/// ended. `None` when its login ended, a per-model limit blocks it, or nothing
/// blocks at `at`. This is the one copy of the rule; the decision uses it.
pub fn usable_again(account: &AccountState, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    View::of(account, at).usable_again(at)
}

/// Whether the account's rates are known (spec §2.3): its `c`, measured or
/// assumed. A capped account whose `s` is not known yet is planned without its
/// 5 h limit (`s` plays as 0); at 100 % it is still left. This is the one rule
/// [`View::rates`] and the decision's regime use.
pub fn rates_known(account: &AccountState) -> bool {
    account.rates.is_some()
}

/// The weekly percent the plan plays an account from at `at`: 0 when the
/// account's weekly reset is at or before `at`, because that week is over; else
/// the reading's percent. This is the one copy of the rule [`View::of`] and the
/// Forecast page use.
pub(crate) fn week_percent_at(account: &AccountState, at: DateTime<Utc>) -> f64 {
    if account.week_resets_at <= at {
        0.0
    } else {
        account.week_percent
    }
}

/// The USD this account can still take before its weekly reset, minus
/// the USD needed to empty it (spec §2.3). The walk runs in [`STEP_MIN`]
/// steps from [`Inputs::now`]; a capped account takes at most
/// `(100 - session-%) / s` USD per window, a window opens at the first
/// step with work after the previous one ended and lasts 5 h. A weekly reset at
/// or before [`Inputs::now`] means that week is over: the account is played
/// from 0 % with its next reset, as the 5 h window already is when its reset
/// has passed. `None` while the rates are unknown, which includes a capped
/// account without `s`. Task 4 returns no rate of 0 or below (it returns
/// `None`), so neither division here can be by zero from a rate.
pub fn slack(account: &AccountState, inputs: &Inputs) -> Option<f64> {
    let walk = rhythm_walk(inputs);
    View::of(account, inputs.now).slack(inputs.now, &walk)
}

/// The walk of the learned rhythm: every step carries a quarter of the work of
/// the clock hour that holds its start, over the plan's [`HORIZON_H`] hours.
/// [`crate::burn::decide`] uses it for the waiting switch's latest time too.
pub(super) fn rhythm_walk(inputs: &Inputs) -> Walk<'_> {
    Walk::new(
        Work::Rhythm {
            rhythm: &inputs.rhythm,
            offset: inputs.offset,
        },
        inputs.now + Duration::hours(HORIZON_H),
    )
}

/// Where the work of a walk's steps comes from.
#[derive(Clone, Copy)]
pub(super) enum Work<'a> {
    /// The learned rhythm: the hour that holds the step's start, a quarter of it.
    Rhythm {
        rhythm: &'a Rhythm,
        offset: FixedOffset,
    },
    /// Work recorded in the history, by the UTC hour that holds it; each hour is
    /// spread evenly over the four steps of that hour.
    Recorded(&'a BTreeMap<DateTime<Utc>, f64>),
}

impl Work<'_> {
    fn at(&self, t: DateTime<Utc>) -> f64 {
        match self {
            Work::Rhythm { rhythm, offset } => rhythm.at(t, *offset) * STEP_FRACTION,
            Work::Recorded(hours) => {
                hours.get(&hour_start(t)).copied().unwrap_or(0.0) / STEPS_PER_HOUR as f64
            }
        }
    }
}

/// The steps of an hour: the four a recorded hour is spread over.
const STEPS_PER_HOUR: i64 = 60 / STEP_MIN;

/// A walk of [`STEP_MIN`] steps: every step from a start up to the exclusive
/// `end`, with the work `work` gives that step. The plan's one step walk:
/// [`follow`] and [`slack`] give it the rhythm, a replay the recorded work.
pub(super) struct Walk<'a> {
    work: Work<'a>,
    end: DateTime<Utc>,
}

impl<'a> Walk<'a> {
    pub(super) fn new(work: Work<'a>, end: DateTime<Utc>) -> Walk<'a> {
        Walk { work, end }
    }

    pub(super) fn steps_from(
        &self,
        from: DateTime<Utc>,
    ) -> impl Iterator<Item = (DateTime<Utc>, f64)> + '_ {
        self.steps(from, self.end)
    }

    /// The walk from `from` over [`HORIZON_H`] hours, whatever this walk's own
    /// end is: a slack always looks a full horizon ahead from the moment of the
    /// decision, as it did before the recorded walk had an end of its own.
    fn horizon_from(&self, from: DateTime<Utc>) -> impl Iterator<Item = (DateTime<Utc>, f64)> + '_ {
        self.steps(from, from + Duration::hours(HORIZON_H))
    }

    fn steps(
        &self,
        from: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> impl Iterator<Item = (DateTime<Utc>, f64)> + '_ {
        let count = if end <= from {
            0
        } else {
            let span = end.signed_duration_since(from);
            let minutes = span.num_minutes() + if span.subsec_nanos() > 0 { 1 } else { 0 };
            (minutes + STEP_MIN - 1) / STEP_MIN
        };
        (0..count).map(move |index| {
            let at = from + Duration::minutes(STEP_MIN * index);
            (at, self.work.at(at))
        })
    }

    fn end(&self) -> DateTime<Utc> {
        self.end
    }
}

/// The usable account with the least slack (spec §2.3), ties broken by the
/// earlier weekly reset and then by the account id, so the result never depends
/// on the order of [`Inputs::accounts`]. Two slacks within 1e-9 are a tie: a
/// difference that small is rounding noise, not a reason to skip the reset and
/// id rule. The percents are those of [`Inputs::now`], so an `at` other than
/// `now` only moves the rhythm and the resets. If any usable account has no
/// rates, the fixed order of spec §2.3 is used instead: capped accounts whose
/// window has room first, then the others, each group by the earlier weekly
/// reset and then the id.
pub fn choose(inputs: &Inputs, at: DateTime<Utc>) -> Option<String> {
    let views: Vec<View<'_>> = inputs
        .accounts
        .iter()
        .map(|account| View::of(account, at))
        .collect();
    let index = pick(
        &views,
        at,
        &rhythm_walk(inputs),
        inputs.rhythm.typical_hour(),
    )?;
    inputs.accounts.get(index).map(|account| account.id.clone())
}

/// Play the next [`HORIZON_H`] hours in [`STEP_MIN`] steps (spec §2.4). Every
/// step has the work of its rhythm hour divided by four. Every policy leaves
/// the current account when it is not usable and then calls [`choose`].
/// WhenFull makes no other move; Lazy and Eager make the early moves
/// [`early_move_due`] allows: Lazy only to an account at risk of losing quota,
/// Eager to any account [`choose`] names whose slack is at least
/// [`SLACK_MARGIN_H`] × [`Rhythm::typical_hour`] below the current one's, each
/// when [`early_move_allowed`] says yes: the plan starts from
/// [`Inputs::last_switch_at`] and [`Inputs::manual_hold_until`], so its first
/// early move is the one [`crate::burn::decide::decide`] will make.
///
/// An account without rates is played with `c = 0` and `s = 0`: it never fills
/// in the plan and it is never the reason for an eager move, because its slack
/// is unknown. The same holds for a capped account without a session rate. When
/// [`Rhythm::typical_hour`] is 0.0 the margin has no unit, so there is no early
/// move at all and the three plans are the same. Weekly resets repeat every 7
/// days from the account's reset time; a weekly reset at or before
/// [`Inputs::now`] means that week is over: the account is played from 0 % with
/// its next reset. A 5 h window opens at the first step with work after the
/// previous one ended and lasts 5 h.
///
/// An active account auto-burn may not use (`may_use` false) stays `current`
/// under every policy while it is not [`blocked`]: no plan leaves it by an
/// early move. Once it is blocked it is left, and it is never chosen again,
/// because [`choose`] only takes usable accounts (spec §2.6: S1 still leaves it
/// when it gets blocked).
pub fn follow(inputs: &Inputs, policy: Policy) -> Plan {
    play(
        inputs,
        policy,
        &rhythm_walk(inputs),
        inputs.rhythm.typical_hour(),
    )
    .plan
}

/// The plan auto-burn follows (spec §7.4): eager while early switching is on,
/// else lazy. This is the one place the rule lives; the Forecast, the pop-up,
/// `burn-next.json`, the status line's auto-burn segment and `status --json`'s
/// `next` all take their plan from here.
pub fn followed(inputs: &Inputs, regime: &Regime) -> Plan {
    let policy = if regime.early {
        Policy::Eager
    } else {
        Policy::Lazy
    };
    follow(inputs, policy)
}

/// One played walk: the plan, the work that was done on an account, and the
/// week-% unused at every weekly reset the walk passed, per account.
pub(super) struct Played {
    pub(super) plan: Plan,
    pub(super) worked_usd: f64,
    pub(super) expired: BTreeMap<String, f64>,
}

/// Play `inputs` over `walk` in [`STEP_MIN`] steps. This is the plan's one step
/// loop: [`follow`] gives it the rhythm over [`HORIZON_H`] hours, a replay the
/// recorded work up to the range's end and the recorded hours' typical hour.
/// `worked_usd` is the work of the steps with an account, `plan.blocked_usd` the
/// work of those without one, so they add up to the work of the walk.
pub(super) fn play(inputs: &Inputs, policy: Policy, walk: &Walk<'_>, typical: f64) -> Played {
    let step = Duration::minutes(STEP_MIN);
    let end = walk.end();
    let mut views: Vec<View<'_>> = inputs
        .accounts
        .iter()
        .map(|account| View::of(account, inputs.now))
        .collect();
    for view in &mut views {
        view.clear_resets(inputs.now, end);
        if view.is_full(inputs.now) {
            view.full_at = Some(inputs.now);
            view.full_by = Some(view.fill());
        }
    }
    let mut current: Option<usize> = inputs
        .active
        .as_ref()
        .and_then(|id| views.iter().position(|view| &view.account.id == id))
        .filter(|&index| !views[index].blocked(inputs.now));
    let mut last_switch = inputs.last_switch_at;
    let mut worked_usd = 0.0;
    let mut blocked_usd = 0.0;
    let mut switches = 0u32;
    let mut segments: Vec<Segment> = Vec::new();

    for (t, work) in walk.steps_from(inputs.now) {
        for view in &mut views {
            view.clear_resets(t, end);
        }

        let choice = match current {
            Some(index) if !views[index].blocked(t) => {
                if views[index].usable(t) {
                    early_move(
                        &views,
                        t,
                        index,
                        policy,
                        typical,
                        last_switch,
                        inputs.manual_hold_until,
                        walk,
                    )
                    .or(Some(index))
                } else {
                    Some(index)
                }
            }
            _ => pick(&views, t, walk, typical),
        };

        match choice {
            Some(index) => {
                if current != Some(index) {
                    if current.is_some() {
                        switches += 1;
                    }
                    last_switch = Some(t);
                }
                current = Some(index);
                if work > 0.0 {
                    worked_usd += work;
                    views[index].apply(t, work);
                    if views[index].full_at.is_none() && views[index].is_full(t) {
                        let fill = views[index].fill();
                        views[index].full_at = Some(t);
                        views[index].full_by = Some(fill);
                    }
                }
            }
            None => {
                if work > 0.0 {
                    blocked_usd += work;
                }
            }
        }

        let name = choice.map(|index| views[index].account.id.clone());
        match segments.last_mut() {
            Some(last) if last.account == name => last.to = t + step,
            _ => segments.push(Segment {
                account: name,
                from: t,
                to: t + step,
            }),
        }
    }

    for view in &mut views {
        view.clear_resets(end, end);
    }

    Played {
        plan: Plan {
            segments,
            blocked_usd,
            switches,
            left_at_reset: views
                .iter()
                .filter_map(|view| {
                    view.left_at_reset
                        .map(|left| (view.account.id.clone(), left))
                })
                .collect(),
            full_at: views
                .iter()
                .filter_map(|view| view.full_at.map(|at| (view.account.id.clone(), at)))
                .collect(),
            full_by: views
                .iter()
                .filter_map(|view| view.full_by.map(|fill| (view.account.id.clone(), fill)))
                .collect(),
        },
        worked_usd,
        expired: views
            .iter()
            .filter(|view| view.resets_passed > 0)
            .map(|view| (view.account.id.clone(), view.expired))
            .collect(),
    }
}

/// One account's state while a plan is played. The public functions build one
/// per account; [`follow`] keeps a vector and moves it forward.
#[derive(Clone, Copy)]
struct View<'a> {
    account: &'a AccountState,
    week: f64,
    session: f64,
    window_end: Option<DateTime<Utc>>,
    reset: DateTime<Utc>,
    full_at: Option<DateTime<Utc>>,
    full_by: Option<Fill>,
    left_at_reset: Option<f64>,
    /// The week-% unused at every weekly reset this view passed.
    expired: f64,
    /// How many weekly resets this view passed.
    resets_passed: u32,
}

impl<'a> View<'a> {
    fn of(account: &'a AccountState, at: DateTime<Utc>) -> View<'a> {
        let (session, window_end) = match &account.session {
            Some(state) => (state.percent, state.resets_at),
            None => (0.0, None),
        };
        View {
            account,
            week: week_percent_at(account, at),
            session,
            window_end,
            reset: next_reset(account.week_resets_at, at),
            full_at: None,
            full_by: None,
            left_at_reset: None,
            expired: 0.0,
            resets_passed: 0,
        }
    }

    fn capped(&self) -> bool {
        self.account.session.is_some()
    }

    /// The rates the account can be planned with. `None` while `c` is
    /// unknown.
    fn rates(&self) -> Option<Rates> {
        if rates_known(self.account) {
            self.account.rates
        } else {
            None
        }
    }

    /// The rates work is applied with: `c = 0`, `s = 0` when they are unknown,
    /// so such an account never fills (the rule the plan states).
    fn play_rates(&self) -> (f64, f64) {
        match self.rates() {
            Some(rates) => (rates.c.value, rates.s.map_or(0.0, |s| s.value)),
            None => (0.0, 0.0),
        }
    }

    /// Clear what has ended at `at`: a weekly reset that has passed (the week
    /// is over and starts again at 0 %, its unused percent recorded if the
    /// reset is inside the walk) and a 5 h window whose end has passed.
    fn clear_resets(&mut self, at: DateTime<Utc>, end: DateTime<Utc>) {
        while self.reset <= at {
            if self.reset < end {
                let unused = 100.0 - self.week;
                if self.left_at_reset.is_none() {
                    self.left_at_reset = Some(unused);
                }
                self.expired += unused;
                self.resets_passed += 1;
            }
            self.week = 0.0;
            self.reset += Duration::weeks(1);
        }
        if self.window_end.is_some_and(|end| at >= end) {
            self.window_end = None;
            self.session = 0.0;
        }
    }

    fn window_blocked(&self, at: DateTime<Utc>) -> bool {
        if !self.capped() {
            return false;
        }
        // The same rule `engine::blocking_limit` uses: a session limit at 100 %
        // whose reset is not at or before `at`.
        Limit {
            kind: LimitKind::Session,
            percent: self.session,
            severity: Severity::Normal,
            resets_at: self.window_end,
            model: None,
            is_active: false,
        }
        .is_full_at(at)
    }

    /// An account is full when its week is at 100 % or its 5 h window is at
    /// 100 % and not over: the one rule [`usable`] uses too.
    fn is_full(&self, at: DateTime<Utc>) -> bool {
        self.week >= 100.0 || self.window_blocked(at)
    }

    /// The limit that is full at this state: the week when both are full, the
    /// one the account is blocked by first.
    fn fill(&self) -> Fill {
        if self.week >= 100.0 {
            Fill::Week
        } else {
            Fill::Window
        }
    }

    /// An account is blocked when its login has ended, its week is at 100 % or
    /// its 5 h window is at 100 % and not over, or a per-model limit blocks it.
    fn blocked(&self, at: DateTime<Utc>) -> bool {
        !self.account.login_ok || self.account.model_blocked || self.is_full(at)
    }

    /// The latest reset among the limits that block at `at`: the weekly reset
    /// when the week is at 100 %, the 5 h reset when the window is at 100 % and
    /// not over. `None` when the login ended, a per-model limit blocks, or
    /// nothing blocks.
    fn usable_again(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.account.login_ok || self.account.model_blocked {
            return None;
        }
        let mut latest: Option<DateTime<Utc>> = None;
        if self.week >= 100.0 {
            latest = Some(self.reset);
        }
        if self.window_blocked(at) {
            if let Some(reset) = self
                .account
                .session
                .as_ref()
                .and_then(|state| state.resets_at)
            {
                latest = Some(match latest {
                    Some(current) => current.max(reset),
                    None => reset,
                });
            }
        }
        latest
    }

    fn usable(&self, at: DateTime<Utc>) -> bool {
        self.account.may_use && !self.blocked(at)
    }

    fn slack(&self, at: DateTime<Utc>, walk: &Walk<'_>) -> Option<f64> {
        let rates = self.rates()?;
        let week = self.week;
        let mut view = *self;
        let mut usable_usd = 0.0;
        for (cursor, work) in walk.horizon_from(at) {
            if cursor >= view.reset {
                break;
            }
            view.clear_resets(cursor, walk.end());
            if work > 0.0 {
                usable_usd += view.apply(cursor, work);
            }
        }
        Some(usable_usd - (100.0 - week) / rates.c.value)
    }

    /// Apply one step's work to the account: its week grows by `c`, its open
    /// 5 h window by `s`, and a window opens when none runs. Unknown rates are
    /// 0. Returns the USD the account can take in this step: all of it,
    /// or the part the open window still has room for.
    fn apply(&mut self, at: DateTime<Utc>, work: f64) -> f64 {
        let (c, s) = self.play_rates();
        let mut taken = work;
        if self.capped() {
            if self.window_end.is_none_or(|end| at >= end) {
                self.window_end = Some(at + Duration::hours(WINDOW_H));
                self.session = 0.0;
            }
            if s > 0.0 {
                taken = work.min((100.0 - self.session) / s);
            }
            self.session = (self.session + s * work).min(100.0);
        }
        self.week = (self.week + c * work).min(100.0);
        taken
    }
}

/// The index of the account [`choose`] would name: the usable account with the
/// least slack, or the fixed order when any usable account has no rates.
///
/// Only in the branch where every usable account has a slack: the candidates
/// are the accounts whose slack is at most `SLACK_MARGIN_H × typical` above the
/// least one (two slacks within [`SLACK_TIE`] count as equal). Among them the
/// highest [`AccountState::thinking_held`] wins; equal counts, which includes
/// all 0, fall back to the order of the least slack: slack, reset, id.
fn pick(views: &[View<'_>], at: DateTime<Utc>, walk: &Walk<'_>, typical: f64) -> Option<usize> {
    let mut usable: Vec<usize> = (0..views.len())
        .filter(|&index| views[index].usable(at))
        .collect();
    if usable.is_empty() {
        return None;
    }
    if usable
        .iter()
        .any(|&index| views[index].slack(at, walk).is_none())
    {
        let group = |index: usize| u8::from(!views[index].capped());
        usable.sort_by(|&a, &b| {
            group(a)
                .cmp(&group(b))
                .then_with(|| {
                    next_reset(views[a].account.week_resets_at, at)
                        .cmp(&next_reset(views[b].account.week_resets_at, at))
                })
                .then_with(|| views[a].account.id.cmp(&views[b].account.id))
        });
        return usable.first().copied();
    }
    let slacks: Vec<(usize, f64)> = usable
        .iter()
        .filter_map(|&index| views[index].slack(at, walk).map(|slack| (index, slack)))
        .collect();
    let least = slacks
        .iter()
        .map(|(_, slack)| *slack)
        .fold(f64::INFINITY, f64::min);
    let margin = SLACK_MARGIN_H * typical;
    let mut best: Option<(u32, f64, DateTime<Utc>, usize)> = None;
    for (index, slack) in slacks {
        // A slack beyond the margin is not a candidate; the tie is the same
        // rounding noise the order below treats as equal.
        if slack - least > margin + SLACK_TIE {
            continue;
        }
        let reset = next_reset(views[index].account.week_resets_at, at);
        let thinking = views[index].account.thinking_held;
        let better = match best {
            None => true,
            Some((best_thinking, best_slack, best_reset, best_index)) => {
                let slack_order = if (slack - best_slack).abs() <= SLACK_TIE {
                    Ordering::Equal
                } else {
                    slack.total_cmp(&best_slack)
                };
                thinking
                    .cmp(&best_thinking)
                    .reverse()
                    .then(slack_order)
                    .then_with(|| reset.cmp(&best_reset))
                    .then_with(|| views[index].account.id.cmp(&views[best_index].account.id))
                    == Ordering::Less
            }
        };
        if better {
            best = Some((thinking, slack, reset, index));
        }
    }
    best.map(|(_, _, _, index)| index)
}

/// Whether an early move (S2) may come at `at` (spec §2.5): the last switch is
/// at least [`MIN_DWELL_MIN`] before it (no known switch counts as long ago)
/// and no manual hold runs past it. The one copy of the rule:
/// [`crate::burn::decide::decide`] and the eager plan both use it.
pub(super) fn early_move_allowed(
    last_switch: Option<DateTime<Utc>>,
    manual_hold_until: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> bool {
    last_switch.is_none_or(|last| at - last >= Duration::minutes(MIN_DWELL_MIN))
        && manual_hold_until.is_none_or(|until| until <= at)
}

/// Whether an early move (S2) to a target with `target_slack` is due while the
/// active account has `current_slack` (spec §2.5): the target's slack is at
/// least [`SLACK_MARGIN_H`] × `typical` below the active one's, and either
/// early switching is on (`early`) or the target's slack is at most
/// [`EXPIRY_CUSHION_H`] × `typical`. Then it is at risk of losing quota at its
/// weekly reset: the slack counts only the usual working hours, and every hour
/// the machines are off beyond them costs quota, while a switch costs only the
/// switch. Never while `typical` is 0.0: the margin has no unit then. The one copy of the rule:
/// [`crate::burn::decide::decide`] and both plans use it.
pub(super) fn early_move_due(
    early: bool,
    target_slack: f64,
    current_slack: f64,
    typical: f64,
) -> bool {
    typical > 0.0
        && target_slack <= current_slack - SLACK_MARGIN_H * typical
        && (early || target_slack <= EXPIRY_CUSHION_H * typical)
}

/// The early move of a plan (spec §2.5, S2): the account [`choose`] names,
/// when it is another one, [`early_move_due`] says yes for `policy` (eager:
/// early switching on) and [`early_move_allowed`] says yes. A target without
/// known rates is never a reason to move, and WhenFull never moves early.
#[allow(clippy::too_many_arguments)]
fn early_move(
    views: &[View<'_>],
    at: DateTime<Utc>,
    current: usize,
    policy: Policy,
    typical: f64,
    last_switch: Option<DateTime<Utc>>,
    manual_hold_until: Option<DateTime<Utc>>,
    walk: &Walk<'_>,
) -> Option<usize> {
    let early = match policy {
        Policy::WhenFull => return None,
        Policy::Lazy => false,
        Policy::Eager => true,
    };
    if !early_move_allowed(last_switch, manual_hold_until, at) {
        return None;
    }
    let target = pick(views, at, walk, typical)?;
    if target == current {
        return None;
    }
    let target_slack = views[target].slack(at, walk)?;
    let current_slack = views[current].slack(at, walk)?;
    early_move_due(early, target_slack, current_slack, typical).then_some(target)
}

/// The first weekly reset after `at`: the account's reset time repeated every
/// 7 days. This is the reset the plan plays the account to.
pub(crate) fn next_reset(reset: DateTime<Utc>, at: DateTime<Utc>) -> DateTime<Utc> {
    let mut next = reset;
    while next <= at {
        next += Duration::weeks(1);
    }
    next
}

/// The setup the plan's tests share with the later auto-burn tests (Tasks 6
/// and 7 use it; they do not copy it).
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    pub(crate) const EPS: f64 = 1e-9;

    /// The plan's `now`: Monday 08:00 UTC.
    pub(crate) const NOW: &str = "2026-11-16T08:00:00Z";

    pub(crate) fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("test date")
    }

    pub(crate) fn now() -> DateTime<Utc> {
        utc(NOW)
    }

    pub(crate) fn offset() -> FixedOffset {
        FixedOffset::east_opt(0).expect("UTC")
    }

    /// The rhythm of the tests: six working days, one USD per working
    /// hour, a free Sunday.
    ///
    /// Monday to Friday work `hours_per_weekday` hours in three 5 h blocks —
    /// 07:00–12:00, 12:00–17:00 and 18:00–23:00 for 15 — and Saturday works 18
    /// hours in four blocks, 00:00–05:00, 05:00–10:00, 10:00–15:00 and
    /// 15:00–18:00 for 15. Monday's 07:00–08:00 lies before `now`, so the work
    /// before PR's reset on Saturday 19:00 is 14 + 4 × 15 + 18 = 92 USD.
    /// Each block opens one 5 h window; the five weekdays give three windows
    /// each and Saturday four, so AC can use 19 USD in its windows
    /// before its reset, not the 18 the plan expected: 18 windows hold at most
    /// 18 × 5 = 90 USD of work. The tests assert those numbers.
    ///
    /// `hours_per_weekday` is 0 (an empty week, the "no rhythm" case) or
    /// 10..=16: those are the values the block arithmetic below yields valid
    /// hours of the week for.
    pub(crate) fn week(hours_per_weekday: i64) -> Rhythm {
        assert!(
            hours_per_weekday == 0 || (10..=16).contains(&hours_per_weekday),
            "week: hours_per_weekday must be 0 or 10..=16, got {hours_per_weekday}"
        );
        let mut usd = [0.0f64; 168];
        if hours_per_weekday == 0 {
            return Rhythm { usd };
        }
        let third_end = 18 + (hours_per_weekday - 10) as u32;
        let saturday_end = 15 + (hours_per_weekday - 12) as u32;
        let weekday = [(7u32, 12u32), (12, 17), (18, third_end)];
        let saturday = [(0, 5), (5, 10), (10, 15), (15, saturday_end)];
        for day in 0..7u32 {
            let blocks: &[(u32, u32)] = match day {
                0..=4 => &weekday,
                5 => &saturday,
                _ => &[],
            };
            for &(from, to) in blocks {
                for hour in from..to {
                    usd[(day * 24 + hour) as usize] = 1.0;
                }
            }
        }
        Rhythm { usd }
    }

    pub(crate) fn account(
        id: &str,
        c: f64,
        s: Option<f64>,
        capped: bool,
        reset: &str,
    ) -> AccountState {
        AccountState {
            id: id.to_string(),
            may_use: true,
            login_ok: true,
            week_percent: 0.0,
            week_resets_at: utc(reset),
            session: capped.then_some(SessionState {
                percent: 0.0,
                resets_at: None,
            }),
            model_blocked: false,
            rates: Some(Rates::given(c, s)),
            thinking_held: 0,
        }
    }

    /// The three accounts of `specs/sim/burn_sim.py`: PR uncapped, AC and LB
    /// capped, at their resets.
    pub(crate) fn inputs() -> Inputs {
        Inputs {
            now: now(),
            offset: offset(),
            accounts: vec![
                account("PR", 2.0, None, false, "2026-11-21T19:00:00Z"),
                account("AC", 4.0, Some(100.0), true, "2026-11-23T02:00:00Z"),
                account("LB", 4.0, Some(80.0), true, "2026-11-23T17:00:00Z"),
            ],
            active: Some("PR".to_string()),
            rhythm: week(15),
            last_switch_at: None,
            manual_hold_until: None,
        }
    }

    /// `inputs()` with no account at risk of losing quota: AC at 72 % needs
    /// 28 / 4 = 7 USD of its 19, LB at 64 % needs 36 / 4 = 9 USD of its 24.75,
    /// so their slacks are 12.0 and 15.75 USD, above the cushion of 8 typical
    /// hours, and PR's is 42. The eager plan moves to AC at once; the lazy plan
    /// stays on PR until AC's slack is down to the cushion.
    pub(crate) fn calm() -> Inputs {
        let mut inputs = inputs();
        inputs.accounts[1].week_percent = 72.0;
        inputs.accounts[2].week_percent = 64.0;
        inputs
    }

    /// Two accounts that reset together, where early switching pays: AC and
    /// LB uncapped (`c` = 4), both at 30 % and both resetting on Wednesday
    /// 13:00, so each has a slack of 17.5 USD; PR at 80 %. The lazy plan waits
    /// on PR until AC is down to the cushion and then cannot empty both before
    /// their reset; the eager plan starts at once. Lazy blocks 6.5 USD with 10
    /// switches, eager none with 24.
    pub(crate) fn crowded() -> Inputs {
        let mut inputs = inputs();
        inputs.accounts[0].week_percent = 80.0;
        for account in &mut inputs.accounts[1..] {
            account.rates = Some(Rates::given(4.0, None));
            account.session = None;
            account.week_percent = 30.0;
            account.week_resets_at = utc("2026-11-18T13:00:00Z");
        }
        inputs
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn usable_follows_the_limits() {
        let base = inputs().accounts[1].clone();
        assert!(usable(&base, now()));

        let mut full_week = base.clone();
        full_week.week_percent = 100.0;
        assert!(!usable(&full_week, now()));

        let mut full_window = base.clone();
        full_window.session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() + Duration::hours(1)),
        });
        assert!(!usable(&full_window, now()));

        let mut window_over = base.clone();
        window_over.session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() - Duration::minutes(1)),
        });
        assert!(usable(&window_over, now()));

        let mut off = base.clone();
        off.may_use = false;
        assert!(!usable(&off, now()));

        let mut no_login = base.clone();
        no_login.login_ok = false;
        assert!(!usable(&no_login, now()));

        let mut model_blocked = base;
        model_blocked.model_blocked = true;
        assert!(!usable(&model_blocked, now()));
    }

    #[test]
    fn slack_of_an_uncapped_account_is_work_left_minus_work_needed() {
        let inputs = inputs();
        let slack = slack(&inputs.accounts[0], &inputs).expect("PR has rates");
        assert!((slack - 42.0).abs() < EPS, "{slack}");
    }

    #[test]
    fn slack_of_a_capped_account_counts_windows() {
        // AC needs 100 / 4 = 25 USD. Its 19 windows (three on each of the five
        // weekdays, four on Saturday) take 1.0 USD each, so 19 - 25 = -6.0.
        let inputs = inputs();
        let slack = slack(&inputs.accounts[1], &inputs).expect("AC has c and s");
        assert!((slack - (-6.0)).abs() < EPS, "{slack}");
    }

    #[test]
    fn choose_takes_the_least_slack() {
        assert_eq!(choose(&inputs(), now()), Some("AC".to_string()));
    }

    #[test]
    fn choose_without_rates_prefers_capped_accounts_with_room() {
        let mut inputs = inputs();
        for account in &mut inputs.accounts {
            account.rates = None;
        }
        assert_eq!(choose(&inputs, now()), Some("AC".to_string()));

        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() + Duration::hours(1)),
        });
        assert_eq!(choose(&inputs, now()), Some("LB".to_string()));

        inputs.accounts[2].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() + Duration::hours(1)),
        });
        assert_eq!(choose(&inputs, now()), Some("PR".to_string()));
    }

    #[test]
    fn choose_takes_the_account_that_holds_the_thinking_within_the_margin() {
        let reset = "2026-11-21T19:00:00Z";
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 2.0;
        let mut inputs = Inputs {
            now: now(),
            offset: offset(),
            accounts: vec![x1, x2],
            active: None,
            rhythm: week(15),
            last_switch_at: None,
            manual_hold_until: None,
        };
        let x1_slack = slack(&inputs.accounts[0], &inputs).expect("X1 has rates");
        let x2_slack = slack(&inputs.accounts[1], &inputs).expect("X2 has rates");
        assert!((x1_slack - 42.0).abs() < EPS, "{x1_slack}");
        assert!((x2_slack - 43.0).abs() < EPS, "{x2_slack}");
        let margin = SLACK_MARGIN_H * inputs.rhythm.typical_hour();
        assert!((margin - 2.0).abs() < EPS, "{margin}");

        assert_eq!(choose(&inputs, now()), Some("X1".to_string()));
        inputs.accounts[1].thinking_held = 5;
        assert_eq!(choose(&inputs, now()), Some("X2".to_string()));
        inputs.accounts[0].thinking_held = 7;
        assert_eq!(choose(&inputs, now()), Some("X1".to_string()));
    }

    #[test]
    fn thinking_does_not_decide_outside_the_margin() {
        let reset = "2026-11-21T19:00:00Z";
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 6.0;
        x2.thinking_held = 5;
        let inputs = Inputs {
            now: now(),
            offset: offset(),
            accounts: vec![x1, x2],
            active: None,
            rhythm: week(15),
            last_switch_at: None,
            manual_hold_until: None,
        };
        let x1_slack = slack(&inputs.accounts[0], &inputs).expect("X1 has rates");
        let x2_slack = slack(&inputs.accounts[1], &inputs).expect("X2 has rates");
        assert!((x1_slack - 42.0).abs() < EPS, "{x1_slack}");
        assert!((x2_slack - 45.0).abs() < EPS, "{x2_slack}");
        let margin = SLACK_MARGIN_H * inputs.rhythm.typical_hour();
        assert!((margin - 2.0).abs() < EPS, "{margin}");

        assert_eq!(choose(&inputs, now()), Some("X1".to_string()));
    }

    #[test]
    fn thinking_does_not_decide_without_rates() {
        let reset = "2026-11-21T19:00:00Z";
        let mut x1 = account("X1", 2.0, None, false, reset);
        x1.rates = None;
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.thinking_held = 5;
        let mut inputs = Inputs {
            now: now(),
            offset: offset(),
            accounts: vec![x1, x2],
            active: None,
            rhythm: week(15),
            last_switch_at: None,
            manual_hold_until: None,
        };

        let with_thinking = choose(&inputs, now());
        inputs.accounts[1].thinking_held = 0;
        let without_thinking = choose(&inputs, now());
        assert_eq!(with_thinking, without_thinking);
        assert_eq!(with_thinking, Some("X1".to_string()));
    }

    #[test]
    fn the_plan_goes_back_to_the_account_with_the_thinking() {
        let reset = "2026-11-21T19:00:00Z";
        let mut z = account("Z", 2.0, None, false, reset);
        z.week_percent = 100.0;
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 2.0;
        x2.thinking_held = 5;
        let inputs = Inputs {
            now: now(),
            offset: offset(),
            accounts: vec![z, x1, x2],
            active: Some("Z".to_string()),
            rhythm: week(15),
            last_switch_at: None,
            manual_hold_until: None,
        };
        let x1_slack = slack(&inputs.accounts[1], &inputs).expect("X1 has rates");
        let x2_slack = slack(&inputs.accounts[2], &inputs).expect("X2 has rates");
        assert!((x1_slack - 42.0).abs() < EPS, "{x1_slack}");
        assert!((x2_slack - 43.0).abs() < EPS, "{x2_slack}");
        let margin = SLACK_MARGIN_H * inputs.rhythm.typical_hour();
        assert!((margin - 2.0).abs() < EPS, "{margin}");

        let plan = follow(&inputs, Policy::Lazy);
        let first = plan
            .segments
            .iter()
            .find(|segment| segment.account.as_deref() != Some("Z"))
            .expect("the plan uses the usable accounts");
        assert_eq!(first.account.as_deref(), Some("X2"));
    }

    #[test]
    fn the_calm_setup_has_no_account_at_risk() {
        let inputs = calm();
        let slacks: Vec<f64> = inputs
            .accounts
            .iter()
            .map(|account| slack(account, &inputs).expect("the setup's rates"))
            .collect();
        for (slack, expected) in slacks.iter().zip([42.0, 12.0, 15.75]) {
            assert!((slack - expected).abs() < EPS, "{slacks:?}");
        }
    }

    #[test]
    fn lazy_stays_until_blocked() {
        // PR at 95 % fills after 2.5 USD, at 10:30, before AC's slack of 12.0
        // is down to the cushion of 8; the eager plan leaves PR at once.
        let mut inputs = calm();
        inputs.accounts[0].week_percent = 95.0;
        let plan = follow(&inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("PR"));
        assert_eq!(plan.segments[0].from, now());
        let full = plan.full_at.get("PR").copied().expect("PR fills");
        assert_eq!(full, utc("2026-11-16T10:15:00Z"));
        assert_eq!(plan.segments[0].to, full + Duration::minutes(STEP_MIN));
        assert_ne!(plan.segments[1].account.as_deref(), Some("PR"));

        let eager = follow(&inputs, Policy::Eager);
        assert_eq!(eager.segments[0].account.as_deref(), Some("AC"));
    }

    #[test]
    fn lazy_moves_at_once_to_an_account_at_risk() {
        // AC's slack is -6.0: every hour it does not work loses quota at its
        // reset, so the lazy plan leaves PR at once, as the eager one does.
        let plan = follow(&inputs(), Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("AC"));
        assert_eq!(plan.segments[0].from, now());
    }

    #[test]
    fn lazy_moves_when_the_slack_reaches_the_cushion() {
        // In the calm setup the lazy plan stays on PR while AC's slack is above
        // the cushion and moves to AC at the first step at which it is not.
        let inputs = calm();
        let plan = follow(&inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("PR"));
        assert_eq!(plan.segments[1].account.as_deref(), Some("AC"));
        let moved = plan.segments[1].from;
        let ac_slack_at = |at: DateTime<Utc>| {
            let mut later = inputs.clone();
            later.now = at;
            slack(&later.accounts[1], &later).expect("AC has rates")
        };
        let cushion = EXPIRY_CUSHION_H * inputs.rhythm.typical_hour();
        assert!(ac_slack_at(moved) <= cushion, "{}", ac_slack_at(moved));
        let before = moved - Duration::minutes(STEP_MIN);
        assert!(ac_slack_at(before) > cushion, "{}", ac_slack_at(before));
    }

    #[test]
    fn lazy_respects_the_dwell_and_the_manual_hold() {
        // AC loses quota, but a switch 10 minutes ago or a manual hold for the
        // next hour keeps the lazy plan on PR until they end.
        let mut inputs = inputs();
        inputs.last_switch_at = Some(now() - Duration::minutes(10));
        let plan = follow(&inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("PR"));
        assert_eq!(plan.segments[1].account.as_deref(), Some("AC"));
        assert_eq!(plan.segments[1].from, now() + Duration::minutes(30));

        let mut inputs = super::testkit::inputs();
        inputs.manual_hold_until = Some(now() + Duration::hours(1));
        let plan = follow(&inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account.as_deref(), Some("PR"));
        assert_eq!(plan.segments[1].account.as_deref(), Some("AC"));
        assert_eq!(plan.segments[1].from, now() + Duration::hours(1));
    }

    #[test]
    fn eager_is_blocked_less_than_lazy_when_two_accounts_reset_together() {
        let inputs = crowded();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert!((lazy.blocked_usd - 6.5).abs() < EPS, "{}", lazy.blocked_usd);
        assert!(
            (eager.blocked_usd - 0.0).abs() < EPS,
            "{}",
            eager.blocked_usd
        );
        assert_eq!(lazy.switches, 10);
        assert_eq!(eager.switches, 24);
    }

    #[test]
    fn in_the_calm_setup_the_cushion_leaves_eager_nothing_to_gain() {
        let inputs = calm();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert!(
            (lazy.blocked_usd - eager.blocked_usd).abs() < EPS,
            "{} vs {}",
            lazy.blocked_usd,
            eager.blocked_usd
        );
    }

    #[test]
    fn follow_is_unchanged_by_the_split() {
        // The values of Task 5's setup before the one player replaced the loop;
        // since 1.5.1 the lazy plan also moves to AC and LB while they are at
        // risk of losing quota, and in this setup that is every early move the
        // eager plan makes.
        let inputs = inputs();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert!((lazy.blocked_usd - 0.0).abs() < EPS, "{}", lazy.blocked_usd);
        assert_eq!(lazy.switches, 58);
        assert!(
            (eager.blocked_usd - 0.0).abs() < EPS,
            "{}",
            eager.blocked_usd
        );
        assert_eq!(eager.switches, 58);
    }

    fn regime(early: bool) -> Regime {
        Regime {
            early,
            why: crate::burn::decide::RegimeWhy::Pays,
            lazy_blocked_h: 0.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: None,
        }
    }

    #[test]
    fn followed_is_eager_only_when_early() {
        let inputs = crowded();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert_ne!(eager.segments, lazy.segments, "the two policies differ");

        assert_eq!(followed(&inputs, &regime(true)), eager);
        assert_eq!(followed(&inputs, &regime(false)), lazy);
    }

    /// The eager move of `inputs()` away from PR, `minutes` after a switch.
    fn eager_target_after(minutes: i64) -> Option<usize> {
        let inputs = inputs();
        let views: Vec<View<'_>> = inputs
            .accounts
            .iter()
            .map(|account| View::of(account, now()))
            .collect();
        early_move(
            &views,
            now(),
            0, // PR
            Policy::Eager,
            inputs.rhythm.typical_hour(),
            Some(now() - Duration::minutes(minutes)),
            None,
            &rhythm_walk(&inputs),
        )
    }

    /// The account and the start of the eager plan's first segment that is
    /// not on PR, the active account of `inputs()`.
    fn first_move(inputs: &Inputs) -> (String, DateTime<Utc>) {
        let plan = follow(inputs, Policy::Eager);
        let segment = plan
            .segments
            .iter()
            .find(|segment| segment.account.as_deref() != Some("PR"))
            .expect("the eager plan leaves PR");
        (
            segment.account.clone().expect("a usable account"),
            segment.from,
        )
    }

    #[test]
    fn the_eager_plan_waits_out_the_last_switch_and_the_manual_hold() {
        // Without a switch or a hold, the eager plan leaves PR for AC at once.
        let mut inputs = inputs();
        assert_eq!(first_move(&inputs), ("AC".to_string(), now()));

        // A switch 10 minutes ago: the first step at least 30 minutes after it
        // (steps are 15 minutes) is now + 30 min, so the plan does not name
        // "now" while decide stays for the dwell.
        inputs.last_switch_at = Some(now() - Duration::minutes(10));
        assert!(!early_move_allowed(
            inputs.last_switch_at,
            None,
            now() + Duration::minutes(15)
        ));
        assert_eq!(
            first_move(&inputs),
            ("AC".to_string(), now() + Duration::minutes(30))
        );

        // A manual hold for 2 hours: the first early move is at its end.
        inputs.last_switch_at = Some(now() - Duration::hours(1));
        inputs.manual_hold_until = Some(now() + Duration::hours(2));
        assert_eq!(
            first_move(&inputs),
            ("AC".to_string(), now() + Duration::hours(2))
        );
    }

    #[test]
    fn a_last_switch_after_now_is_refused() {
        let mut inputs = inputs();
        inputs.last_switch_at = Some(now() + Duration::minutes(1));
        assert!(matches!(
            inputs.check(),
            Err(PlanError::LastSwitchAfterNow { .. })
        ));
    }

    #[test]
    fn eager_respects_the_dwell() {
        let inputs = inputs();
        let plan = follow(&inputs, Policy::Eager);
        for pair in plan.segments.windows(2) {
            let (before, after) = (&pair[0], &pair[1]);
            let (Some(from), Some(to)) = (&before.account, &after.account) else {
                continue;
            };
            if from == to {
                continue;
            }
            let gap = after.from.signed_duration_since(before.from);
            if gap < Duration::minutes(MIN_DWELL_MIN) {
                // Allowed only where the account in use is not usable at the
                // later start: being full somewhere later in the week is not
                // the rule.
                let index = inputs
                    .accounts
                    .iter()
                    .position(|account| &account.id == from)
                    .expect("the segment names an account");
                assert!(
                    !usable(&inputs.accounts[index], after.from),
                    "{from} gave way after {gap:?} while still usable at {:?}",
                    after.from
                );
            }
        }
        // The move itself waits for the dwell: 29 minutes is too soon.
        assert_eq!(eager_target_after(29), None);
        assert_eq!(eager_target_after(30), Some(1)); // AC
    }

    #[test]
    fn eager_move_waits_for_the_dwell() {
        assert_eq!(eager_target_after(29), None);
        assert_eq!(eager_target_after(30), Some(1)); // AC
    }

    #[test]
    fn a_week_that_resets_inside_the_plan_starts_at_zero() {
        // The calm setup, so that the lazy plan fills PR before its reset.
        let mut inputs = calm();
        let reset = inputs.now + Duration::hours(2);
        inputs.accounts[0].week_percent = 99.0;
        inputs.accounts[0].week_resets_at = reset;

        let plan = follow(&inputs, Policy::Lazy);
        let left = plan
            .left_at_reset
            .get("PR")
            .copied()
            .expect("PR resets inside the plan");
        assert!(left < 1.0, "{left}");
        assert!(
            plan.segments
                .iter()
                .any(|segment| segment.account.as_deref() == Some("PR") && segment.from >= reset),
            "{:?}",
            plan.segments
        );
    }

    #[test]
    fn nothing_usable_is_a_segment_with_no_account() {
        let mut inputs = inputs();
        for account in &mut inputs.accounts {
            account.week_percent = 100.0;
        }
        let plan = follow(&inputs, Policy::Lazy);
        assert_eq!(plan.segments[0].account, None);
        assert_eq!(plan.segments[0].from, now());
        // PR's weekly reset is the earliest one in the horizon.
        assert_eq!(plan.segments[0].to, utc("2026-11-21T19:00:00Z"));
        assert_eq!(plan.segments[1].account.as_deref(), Some("PR"));
    }

    #[test]
    fn an_account_without_rates_never_fills() {
        let mut inputs = inputs();
        inputs.accounts[2].rates = None;
        let plan = follow(&inputs, Policy::Lazy);
        assert!(!plan.full_at.contains_key("LB"), "{:?}", plan.full_at);
    }

    #[test]
    fn a_capped_account_without_a_session_rate_is_planned_by_its_week() {
        // Without `s` the 5 h limit plays as 0: the account never fills its
        // window in the plan, but its week fills by `c` like any other.
        let mut inputs = inputs();
        assert!(inputs.accounts[1].session.is_some(), "AC is capped");
        inputs.accounts[1].rates = Some(Rates::given(4.0, None));
        assert!(slack(&inputs.accounts[1], &inputs).is_some());

        let mut without_window = inputs.clone();
        without_window.accounts[1].session = None;
        assert_eq!(
            follow(&inputs, Policy::Lazy).full_at,
            follow(&without_window, Policy::Lazy).full_at,
            "an unknown s plans the account as one without a 5 h limit"
        );
    }

    #[test]
    fn no_rhythm_means_no_eager_move() {
        let mut inputs = inputs();
        // AC active: with a margin of 0 the eager plan would want PR, whose
        // slack is smaller, so the typical > 0.0 guard is what stops it.
        inputs.active = Some("AC".to_string());
        inputs.rhythm = week(0);
        assert!((inputs.rhythm.typical_hour() - 0.0).abs() < EPS);

        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert_eq!(eager.segments, lazy.segments);
        assert_eq!(eager.switches, 0);
        assert!((eager.blocked_usd - 0.0).abs() < EPS);
    }

    #[test]
    fn an_active_account_auto_burn_may_not_use_stays_until_blocked() {
        let mut inputs = inputs();
        inputs.active = Some("AC".to_string());
        inputs.accounts[1].may_use = false;
        // AC's c of 100 fills its week after 1 USD, one hour of the test rhythm.
        // Both policies keep it until then instead of starting on LB, and it is
        // never chosen again after it is left.
        inputs.accounts[1].rates = Some(Rates::given(100.0, Some(100.0)));
        for policy in [Policy::Lazy, Policy::Eager] {
            let plan = follow(&inputs, policy);
            assert_eq!(
                plan.segments[0].account.as_deref(),
                Some("AC"),
                "{policy:?}"
            );
            assert_eq!(plan.segments[0].from, now());
            assert_eq!(
                plan.segments[0].to,
                now() + Duration::hours(1),
                "{policy:?}"
            );
            assert!(
                plan.segments
                    .iter()
                    .skip(1)
                    .all(|segment| segment.account.as_deref() != Some("AC")),
                "{policy:?}: {:?}",
                plan.segments
            );
        }
    }

    #[test]
    fn a_window_whose_reset_has_passed_is_not_full() {
        let mut inputs = inputs();
        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(inputs.now - Duration::minutes(1)),
        });
        assert!(usable(&inputs.accounts[1], inputs.now));

        let plan = follow(&inputs, Policy::Lazy);
        assert_ne!(
            plan.full_at.get("AC"),
            Some(&inputs.now),
            "{:?}",
            plan.full_at
        );
    }

    #[test]
    fn check_rejects_a_percent_that_is_not_finite_or_negative() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
            let mut week_inputs = inputs();
            week_inputs.accounts[1].week_percent = bad;
            let err = week_inputs.check().expect_err("a bad percent is rejected");
            assert!(
                matches!(err, PlanError::InvalidPercent { ref account, limit: "week", .. } if account == "AC"),
                "{err}"
            );

            let mut session_inputs = inputs();
            session_inputs.accounts[1].session = Some(SessionState {
                percent: bad,
                resets_at: Some(session_inputs.now + Duration::hours(1)),
            });
            let err = session_inputs
                .check()
                .expect_err("a bad percent is rejected");
            assert!(
                matches!(err, PlanError::InvalidPercent { ref account, limit: "session", .. } if account == "AC"),
                "{err}"
            );
        }
    }

    #[test]
    fn check_rejects_a_session_percent_without_a_reset_time() {
        let mut inputs = inputs();
        inputs.accounts[1].session = Some(SessionState {
            percent: 5.0,
            resets_at: None,
        });
        let err = inputs.check().expect_err("a running window needs a reset");
        assert!(
            matches!(err, PlanError::SessionWithoutReset { ref account, .. } if account == "AC"),
            "{err}"
        );
        // An idle capped account is at 0 % and has no reset time, which is fine.
        assert!(super::testkit::inputs().check().is_ok());
    }

    #[test]
    fn check_rejects_an_active_account_that_is_not_known() {
        let mut inputs = inputs();
        inputs.active = Some("XX".to_string());
        let err = inputs.check().expect_err("the active account must exist");
        assert!(
            matches!(err, PlanError::UnknownActive { ref id } if id == "XX"),
            "{err}"
        );
    }

    #[test]
    fn check_rejects_two_accounts_with_the_same_id() {
        let mut inputs = inputs();
        inputs.accounts.push(account(
            "AC",
            4.0,
            Some(100.0),
            true,
            "2026-11-23T02:00:00Z",
        ));
        let err = inputs.check().expect_err("ids must be unique");
        assert!(
            matches!(err, PlanError::DuplicateAccount { ref id } if id == "AC"),
            "{err}"
        );
    }

    #[test]
    fn a_weekly_reset_that_has_passed_means_the_week_is_over() {
        let mut inputs = inputs();
        inputs.accounts[0].week_percent = 90.0;
        inputs.accounts[0].week_resets_at = inputs.now - Duration::hours(3);
        let passed = slack(&inputs.accounts[0], &inputs).expect("PR has rates");

        let mut fresh = inputs.clone();
        fresh.accounts[0].week_percent = 0.0;
        fresh.accounts[0].week_resets_at = inputs.now + Duration::days(7) - Duration::hours(3);
        let zero = slack(&fresh.accounts[0], &fresh).expect("PR has rates");
        assert!((passed - zero).abs() < EPS, "{passed} vs {zero}");
    }

    #[test]
    fn a_reset_inside_the_last_step_is_recorded() {
        let mut inputs = inputs();
        // PR stays far from full, so the reset leaves work unused.
        inputs.accounts[0].rates = Some(Rates::given(0.5, None));
        inputs.accounts[0].week_resets_at =
            inputs.now + Duration::hours(167) + Duration::minutes(55);

        let plan = follow(&inputs, Policy::Lazy);
        let left = plan
            .left_at_reset
            .get("PR")
            .copied()
            .expect("PR's reset in the last step is in the horizon");
        assert!(left > 0.0 && left <= 100.0, "{left}");
    }

    #[test]
    fn slacks_within_a_rounding_noise_are_a_tie() {
        let reset = "2026-11-21T19:00:00Z";
        let mut lb = account("LB", 2.0, None, false, reset);
        lb.week_percent = 1e-9;
        let pr = account("PR", 2.0, None, false, reset);
        let mut inputs = inputs();
        inputs.accounts = vec![lb, pr];

        let lb_slack = slack(&inputs.accounts[0], &inputs).expect("LB has rates");
        let pr_slack = slack(&inputs.accounts[1], &inputs).expect("PR has rates");
        assert!(
            (lb_slack - pr_slack).abs() < 1e-9 && lb_slack > pr_slack,
            "{lb_slack} vs {pr_slack}"
        );
        // The raw slacks prefer PR; within 1e-9 the id rule prefers LB.
        assert_eq!(choose(&inputs, now()), Some("LB".to_string()));
    }

    #[test]
    fn recorded_work_uses_the_transcripts_hour_start() {
        let at = utc("2026-11-16T10:45:30Z");
        let hour = hour_start(at);
        assert_eq!(hour, utc("2026-11-16T10:00:00Z"));
        let hours = BTreeMap::from([(hour, 8.0)]);
        let work = Work::Recorded(&hours);
        assert!((work.at(hour) - 2.0).abs() < EPS);
        assert!((work.at(at) - 2.0).abs() < EPS);
        assert!((work.at(utc("2026-11-16T11:00:00Z")) - 0.0).abs() < EPS);
    }
}
