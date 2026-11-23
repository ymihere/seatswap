//! The one decision of auto-burn (spec §2.5): whether to stay, switch, wait for
//! a quiet moment, or say that every account is full. It also decides once an
//! hour whether switching before an account is full pays for itself.

use chrono::{DateTime, Duration, Utc};

use super::plan::{
    blocked, choose, early_move_allowed, early_move_due, follow, leave_due, rates_known,
    rhythm_walk, shared_slack, slack, usable_again, Inputs, PlanError, Policy,
};
use super::{usd_hours, FRESH_MIN, GAIN_FACTOR, HORIZON_H, MIN_COST_SAMPLES, MIN_GAIN_H, STEP_MIN};

/// The measured cost of one switch (spec §2.13): the extra API dollars of work and the
/// number of switches the mean came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwitchCost {
    pub usd: f64,
    pub samples: u32,
}

/// Everything one decision is taken from. [`Situation::check`] rejects a
/// situation no decision may be taken from; [`decide`] and [`regime`] require
/// that it has passed.
#[derive(Clone)]
pub struct Situation {
    pub inputs: Inputs,
    pub cost: Option<SwitchCost>,
    pub tied_busy: u32,
    pub oldest_reading: DateTime<Utc>,
    /// `readSessions` is off, so no rates were learned (spec §2.3).
    pub sessions_off: bool,
}

impl Situation {
    /// Checks the values [`decide`] and [`regime`] take: [`Inputs::check`] must
    /// pass, and the oldest reading may not lie after `inputs.now` — that is a
    /// clock that went back, not a value to hold or pass.
    pub fn check(&self) -> Result<(), PlanError> {
        self.inputs.check()?;
        let now = self.inputs.now;
        if self.oldest_reading > now {
            return Err(PlanError::OldestReadingAfterNow {
                at: self.oldest_reading,
                now,
            });
        }
        Ok(())
    }
}

/// Why early switching is on or off (spec §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegimeWhy {
    /// Not every account auto-burn may use has rates, or no typical hour is known.
    Learning,
    /// No rates were learned because `readSessions` is off (spec §2.3).
    SessionsOff,
    /// No switch has been measured, or too few of them.
    NoCostYet,
    /// The eager plan does not save enough for its extra switches.
    NotWorthIt,
    /// The eager plan saves enough.
    Pays,
}

/// The regime of the next hour: whether the eager plan pays, and the numbers it
/// was decided from (spec §2.5).
#[derive(Debug, Clone, PartialEq)]
pub struct Regime {
    pub early: bool,
    pub why: RegimeWhy,
    pub lazy_blocked_h: f64,
    pub eager_blocked_h: f64,
    pub extra_switches: u32,
    pub cost_h: Option<f64>,
}

/// Why the decision switches (spec §2.5, §2.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// No account was in use at the reading.
    NoneActive,
    /// The active account is blocked (or the login ended or a model blocks it).
    ActiveFull { until: Option<DateTime<Utc>> },
    /// The active account's 5 h window is at
    /// [`crate::burn::WINDOW_LEAVE_PERCENT`] or more and another account's
    /// window is below it; `until` is the end of the active account's window.
    WindowNearlyFull { until: Option<DateTime<Utc>> },
    /// The target's 5 h window has room.
    WindowOpen,
    /// The target is about to lose unused quota at its weekly reset.
    ResetNear,
}

/// What auto-burn does at this reading (spec §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Stay,
    Stale,
    Switch {
        to: String,
        reason: Reason,
    },
    Wait {
        to: String,
        latest: DateTime<Utc>,
        tied_busy: u32,
        reason: Reason,
    },
    AllFull {
        next: Option<(String, DateTime<Utc>)>,
    },
}

/// Which of the two plans pays for the next hour (spec §2.5). `Pays` only when
/// both rules hold: the eager plan blocks at least [`MIN_GAIN_H`] typical hours
/// less than the lazy one, and that gain is at least [`GAIN_FACTOR`] times what
/// its extra switches cost. `Learning` while an account auto-burn may use has no
/// rates, or while the rhythm's typical hour is 0.0; `NoCostYet` until the switch
/// cost has [`MIN_COST_SAMPLES`] samples. `early` is `why == Pays`.
///
/// The plans are [`follow`] with [`Policy::Lazy`] and [`Policy::Eager`];
/// `extra_switches` is eager's switches minus lazy's when eager has more, else 0
/// (fewer switches cost nothing extra). `cost_h` is the switch cost divided by
/// the typical hour. When that hour is 0.0 no hours can be computed: every `_h`
/// field is 0.0 and `cost_h` is `None` (it is also `None` while no cost is
/// known).
///
/// `situation` must have passed [`Situation::check`]; this function does not
/// check it again.
pub fn regime(situation: &Situation) -> Regime {
    let inputs = &situation.inputs;
    let typical = inputs.rhythm.typical_hour();
    let lazy = follow(inputs, Policy::Lazy);
    let eager = follow(inputs, Policy::Eager);
    let lazy_blocked_h = usd_hours(lazy.blocked_usd, typical).unwrap_or(0.0);
    let eager_blocked_h = usd_hours(eager.blocked_usd, typical).unwrap_or(0.0);
    let extra_switches = eager.switches.saturating_sub(lazy.switches);
    let cost_h = situation.cost.and_then(|cost| usd_hours(cost.usd, typical));
    let learning = typical == 0.0
        || inputs
            .accounts
            .iter()
            .any(|account| account.may_use && !rates_known(account));
    let why = if learning && situation.sessions_off {
        RegimeWhy::SessionsOff
    } else if learning {
        RegimeWhy::Learning
    } else {
        match (situation.cost, cost_h) {
            (Some(cost), Some(cost_h)) if cost.samples >= MIN_COST_SAMPLES => {
                let gain = lazy_blocked_h - eager_blocked_h;
                let extra_cost = GAIN_FACTOR * extra_switches as f64 * cost_h;
                if gain >= MIN_GAIN_H && gain >= extra_cost {
                    RegimeWhy::Pays
                } else {
                    RegimeWhy::NotWorthIt
                }
            }
            _ => RegimeWhy::NoCostYet,
        }
    };
    Regime {
        early: why == RegimeWhy::Pays,
        why,
        lazy_blocked_h,
        eager_blocked_h,
        extra_switches,
        cost_h,
    }
}

/// The one decision of auto-burn (spec §2.5, §2.6), in this order:
/// 1. a reading older than [`FRESH_MIN`] minutes → [`Decision::Stale`];
/// 2. and 3. the active account is [`blocked`] or there is none: [`choose`] names
///    a target → [`Decision::Switch`] with [`Reason::ActiveFull`] (or
///    [`Reason::NoneActive`] when there is no active account), else
///    [`Decision::AllFull`]; when [`leave_due`] says the active account's
///    5 h window is nearly full and another account's has room →
///    [`Decision::Switch`] with [`Reason::WindowNearlyFull`] to the account
///    [`choose`] names (it prefers windows with room);
/// 4. a pause that ends in the future, or the last switch less than
///    [`MIN_DWELL_MIN`] ago ([`early_move_allowed`], the rule both plans
///    play) → [`Decision::Stay`];
/// 5. [`choose`] names another account and [`early_move_due`] says yes: its
///    slack is at least [`crate::burn::SLACK_MARGIN_H`] × the typical hour
///    below the active one's, and early switching is on ([`Regime::early`]) or
///    the target's [`shared_slack`] is at most [`crate::burn::EXPIRY_CUSHION_H`]
///    × the typical hour (it is at risk of losing quota, alone or with the
///    accounts that reset before it): with no busy tied session →
///    [`Decision::Switch`] (`WindowOpen` when the target has a 5 h limit, else
///    `ResetNear`); with one → [`Decision::Wait`] until the rhythm's work from
///    now reaches the target's shared slack. A shared slack of 0 or less
///    switches at once;
/// 6. otherwise [`Decision::Stay`].
///
/// Step 3 tests the active account for `blocked` and [`leave_due`], not for
/// `usable`: an account auto-burn may not use is left only when it is blocked
/// or its 5 h window is nearly full (spec §2.6), and it never gives `AllFull`
/// while it is not blocked. Step 5 needs the active
/// account's slack and an active account auto-burn may use; when either is
/// missing there is no early switch.
///
/// `situation` must have passed [`Situation::check`]; this function does not
/// check it again.
pub fn decide(situation: &Situation, regime: &Regime) -> Decision {
    let inputs = &situation.inputs;
    let now = inputs.now;
    if now.signed_duration_since(situation.oldest_reading) > Duration::minutes(FRESH_MIN) {
        return Decision::Stale;
    }

    let active_index = inputs
        .active
        .as_ref()
        .and_then(|id| inputs.accounts.iter().position(|account| &account.id == id));
    let Some(active_index) = active_index else {
        return match choose(inputs, now) {
            Some(to) => Decision::Switch {
                to,
                reason: Reason::NoneActive,
            },
            None => Decision::AllFull {
                next: next_free(inputs),
            },
        };
    };
    let active = &inputs.accounts[active_index];
    if blocked(active, now) {
        return match choose(inputs, now) {
            Some(to) => Decision::Switch {
                to,
                reason: Reason::ActiveFull {
                    until: usable_again(active, now),
                },
            },
            None => Decision::AllFull {
                next: next_free(inputs),
            },
        };
    }

    if leave_due(inputs, active_index) {
        // `leave_due` found another usable account whose window has room, so
        // `choose` names one.
        let Some(to) = choose(inputs, now) else {
            return Decision::Stay;
        };
        return Decision::Switch {
            to,
            reason: Reason::WindowNearlyFull {
                until: active.session.as_ref().and_then(|state| state.resets_at),
            },
        };
    }

    if !early_move_allowed(inputs.last_switch_at, inputs.paused_until, now) {
        return Decision::Stay;
    }

    if !active.may_use {
        return Decision::Stay;
    }
    let Some(to) = choose(inputs, now) else {
        return Decision::Stay;
    };
    if to == active.id {
        return Decision::Stay;
    }
    let Some(target) = inputs.accounts.iter().find(|account| account.id == to) else {
        return Decision::Stay;
    };
    let (Some(target_slack), Some(target_shared), Some(active_slack)) = (
        slack(target, inputs),
        shared_slack(target, inputs),
        slack(active, inputs),
    ) else {
        return Decision::Stay;
    };
    if !early_move_due(
        regime.early,
        target_slack,
        target_shared,
        active_slack,
        inputs.rhythm.typical_hour(),
    ) {
        return Decision::Stay;
    }
    let reason = if target.session.is_some() {
        Reason::WindowOpen
    } else {
        Reason::ResetNear
    };
    if situation.tied_busy == 0 || target_shared <= 0.0 {
        Decision::Switch { to, reason }
    } else {
        Decision::Wait {
            to,
            latest: latest_reaching(inputs, target_shared),
            tied_busy: situation.tied_busy,
            reason,
        }
    }
}

/// The account auto-burn may use that is usable again first (spec §2.5, S3),
/// ties by account id; `None` when no such account has a time.
fn next_free(inputs: &Inputs) -> Option<(String, DateTime<Utc>)> {
    let mut best: Option<(String, DateTime<Utc>)> = None;
    for account in &inputs.accounts {
        if !account.may_use {
            continue;
        }
        let Some(at) = usable_again(account, inputs.now) else {
            continue;
        };
        let earlier = match &best {
            None => true,
            Some((id, when)) => at < *when || (at == *when && account.id < *id),
        };
        if earlier {
            best = Some((account.id.clone(), at));
        }
    }
    best
}

/// The end of the first [`STEP_MIN`] step at which the rhythm's work summed from
/// [`Inputs::now`] reaches `target_usd`; the horizon's end when it never does
/// (the decision is taken again at every reading).
fn latest_reaching(inputs: &Inputs, target_usd: f64) -> DateTime<Utc> {
    let step = Duration::minutes(STEP_MIN);
    let mut sum = 0.0;
    for (at, work) in rhythm_walk(inputs).steps_from(inputs.now) {
        sum += work;
        if sum >= target_usd {
            return at + step;
        }
    }
    inputs.now + Duration::hours(HORIZON_H)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::plan::testkit::{account, calm, crowded, inputs, narrow, now, utc, week, EPS};
    use crate::burn::plan::SessionState;
    use crate::burn::{Rates, SLACK_MARGIN_H};

    fn situation(inputs: Inputs) -> Situation {
        Situation {
            inputs,
            cost: Some(SwitchCost {
                usd: 0.05,
                samples: 3,
            }),
            tied_busy: 0,
            oldest_reading: now() - Duration::minutes(1),
            sessions_off: false,
        }
    }

    /// An early regime, for the cases that test `decide` alone.
    fn pays() -> Regime {
        Regime {
            early: true,
            why: RegimeWhy::Pays,
            lazy_blocked_h: 1.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: Some(0.0),
        }
    }

    #[test]
    fn stale_readings_decide_nothing() {
        let mut situation = situation(inputs());
        situation.oldest_reading = now() - Duration::minutes(21);
        let state = regime(&situation);
        assert_eq!(decide(&situation, &state), Decision::Stale);
    }

    #[test]
    fn a_full_active_account_switches_at_once() {
        let mut inputs = inputs();
        inputs.accounts[0].week_percent = 100.0;
        let mut situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::ActiveFull {
                    until: Some(now() + Duration::days(5) + Duration::hours(11)),
                },
            }
        );

        // Neither a busy tied session nor a pause holds S1 back.
        situation.tied_busy = 3;
        situation.inputs.paused_until = Some(now() + Duration::hours(3));
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::ActiveFull {
                    until: Some(now() + Duration::days(5) + Duration::hours(11)),
                },
            }
        );
    }

    #[test]
    fn a_nearly_full_window_switches_to_an_account_with_room() {
        let mut inputs = inputs();
        inputs.active = Some("AC".to_string());
        let window_end = now() + Duration::hours(2);
        inputs.accounts[1].session = Some(SessionState {
            percent: 96.0,
            resets_at: Some(window_end),
        });
        let mut situation = situation(inputs);
        // Lazy regime, a busy tied session: neither holds the move back.
        situation.tied_busy = 2;
        let state = Regime {
            early: false,
            why: RegimeWhy::NotWorthIt,
            ..pays()
        };
        let Decision::Switch { to, reason } = decide(&situation, &state) else {
            panic!("expected a switch");
        };
        assert_ne!(to, "AC");
        assert_eq!(
            reason,
            Reason::WindowNearlyFull {
                until: Some(window_end)
            }
        );

        // With no other window that has room it stays, and it is not full.
        situation.inputs.accounts[0].may_use = false;
        situation.inputs.accounts[2].may_use = false;
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn everything_full_names_the_next_free_account() {
        let mut inputs = inputs();
        // PR and LB have their week at 100 %. AC's week has room; only its 5 h
        // window is full, until 14:40. That is a time at which AC can really
        // work again, so it is the next account.
        inputs.accounts[0].week_percent = 100.0;
        inputs.accounts[2].week_percent = 100.0;
        let session_reset = now() + Duration::hours(6) + Duration::minutes(40);
        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(session_reset),
        });
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::AllFull {
                next: Some(("AC".to_string(), session_reset)),
            }
        );
    }

    #[test]
    fn the_active_account_blocked_by_week_and_window_is_free_at_the_later_reset() {
        let mut inputs = inputs();
        inputs.active = Some("AC".to_string());
        inputs.accounts[1].week_percent = 100.0;
        // The window resets at 14:40, long before AC's week resets on Monday
        // 02:00, so 14:40 is still a blocked time.
        let session_reset = now() + Duration::hours(6) + Duration::minutes(40);
        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(session_reset),
        });
        let weekly_reset = inputs.accounts[1].week_resets_at;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "LB".to_string(),
                reason: Reason::ActiveFull {
                    until: Some(weekly_reset),
                },
            }
        );
    }

    #[test]
    fn all_full_names_the_time_an_account_can_really_work() {
        let mut inputs = inputs();
        for account in &mut inputs.accounts {
            account.week_percent = 100.0;
        }
        // AC's window resets at 14:40, but its week is full until Monday 02:00;
        // PR's weekly reset on Saturday 19:00 comes first, so PR is the next
        // account that can really work.
        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() + Duration::hours(6) + Duration::minutes(40)),
        });
        let pr_reset = inputs.accounts[0].week_resets_at;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::AllFull {
                next: Some(("PR".to_string(), pr_reset)),
            }
        );
    }

    #[test]
    fn without_rates_there_is_no_early_switch() {
        // The calm setup: AC, which the fixed order names, has slack above the
        // cushion.
        let mut inputs = calm();
        inputs.accounts[2].rates = None;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::Learning);
        assert!(!state.early);
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn sessions_off_is_the_regime_why() {
        let mut inputs = inputs();
        for account in &mut inputs.accounts {
            account.rates = None;
        }
        let mut situation = situation(inputs);
        situation.sessions_off = true;
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::SessionsOff);
        assert!(!state.early);
    }

    #[test]
    fn without_a_measured_cost_there_is_no_early_switch() {
        let mut situation = situation(calm());
        situation.cost = None;
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::NoCostYet);
        assert_eq!(decide(&situation, &state), Decision::Stay);

        situation.cost = Some(SwitchCost {
            usd: 0.05,
            samples: 2,
        });
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::NoCostYet);
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn a_narrow_window_makes_early_switching_pay() {
        // The narrow setup: eager is blocked 9.5 h less than lazy, with 18
        // more switches at 0.05 h each, so the gain is above both rules.
        let situation = situation(narrow());
        let state = regime(&situation);
        assert!(state.early);
        assert_eq!(state.why, RegimeWhy::Pays);
        assert!((state.lazy_blocked_h - 9.5).abs() < EPS, "{state:?}");
        assert!(state.eager_blocked_h.abs() < EPS, "{state:?}");
        assert_eq!(state.extra_switches, 18);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::ResetNear,
            }
        );
    }

    #[test]
    fn accounts_that_reset_together_are_at_risk_together() {
        // The crowded setup: AC and LB have 17.5 USD of slack each, above the
        // cushion, but a shared slack of 0. Early switching does not pay, and
        // auto-burn still leaves PR at once, even past a busy tied session.
        let mut situation = situation(crowded());
        let state = regime(&situation);
        assert!(!state.early, "{state:?}");
        situation.tied_busy = 1;
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::ResetNear,
            }
        );
    }

    #[test]
    fn two_accounts_at_risk_together_do_not_take_turns() {
        // On LB, choose() names AC (the same slack and reset, the lower id),
        // whose shared slack is 0. The margin compares own slacks, which are
        // equal, so the shared deficit is no reason to move to AC and back.
        let mut inputs = crowded();
        inputs.active = Some("LB".to_string());
        assert_eq!(choose(&inputs, now()), Some("AC".to_string()));
        let situation = situation(inputs);
        let state = regime(&situation);
        assert!(!state.early, "{state:?}");
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn a_target_within_the_cushion_is_switched_to_without_early_switching() {
        // AC at 40 % and LB at 20 % have slacks of 4.0 and 4.75 USD: above 0,
        // but within the cushion of 8 typical hours, so a few hours with the
        // machines off would cost quota. Early switching does not pay here.
        let mut inputs = calm();
        inputs.accounts[1].week_percent = 40.0;
        inputs.accounts[2].week_percent = 20.0;
        let ac_slack = slack(&inputs.accounts[1], &inputs).expect("AC has rates");
        assert!((ac_slack - 4.0).abs() < EPS, "{ac_slack}");
        let situation = situation(inputs);
        let state = regime(&situation);
        assert!(!state.early, "{state:?}");
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn large_windows_make_it_not_worth_it() {
        let mut inputs = inputs();
        inputs.accounts[1].rates = Some(Rates::given(4.0, Some(25.0)));
        inputs.accounts[2].rates = Some(Rates::given(4.0, Some(20.0)));
        let mut situation = situation(inputs);
        situation.cost = Some(SwitchCost {
            usd: 2.0,
            samples: 3,
        });
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::NotWorthIt);
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn a_pause_holds_early_switches() {
        let mut situation = situation(inputs());
        situation.inputs.paused_until = Some(now() + Duration::hours(3));
        let state = regime(&situation);
        assert_eq!(decide(&situation, &state), Decision::Stay);

        situation.inputs.paused_until = Some(now() - Duration::minutes(1));
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn a_recent_switch_holds_for_five_minutes() {
        let mut situation = situation(inputs());
        situation.inputs.last_switch_at = Some(now() - Duration::minutes(4));
        let state = regime(&situation);
        assert_eq!(decide(&situation, &state), Decision::Stay);

        situation.inputs.last_switch_at = Some(now() - Duration::minutes(5));
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn a_tied_busy_session_makes_it_wait() {
        // The narrow setup: AC's shared slack is 10 USD, 10 working hours at
        // the test rhythm's 1.0 USD per working hour (Monday 08:00 to 17:00,
        // then 18:00 to 19:00), and choose() names it.
        let inputs = narrow();
        let ac_shared = shared_slack(&inputs.accounts[1], &inputs).expect("AC has rates");
        assert!((ac_shared - 10.0).abs() < EPS, "{ac_shared}");
        assert_eq!(choose(&inputs, now()), Some("AC".to_string()));

        let mut situation = situation(inputs);
        situation.tied_busy = 1;
        let state = regime(&situation);
        assert!(state.early);
        assert_eq!(
            decide(&situation, &state),
            Decision::Wait {
                to: "AC".to_string(),
                latest: utc("2026-11-16T19:00:00Z"),
                tied_busy: 1,
                reason: Reason::ResetNear,
            }
        );
    }

    #[test]
    fn waiting_ends_when_the_target_has_no_slack_left() {
        // The test setup's AC has slack -6.0 (19 windows take 19 USD, emptying it
        // needs 25), which is all this case needs: a slack at or below 0.
        let mut situation = situation(inputs());
        situation.tied_busy = 1;
        let ac_slack =
            slack(&situation.inputs.accounts[1], &situation.inputs).expect("AC has rates");
        assert!(ac_slack <= 0.0, "{ac_slack}");
        for state in [regime(&situation), pays()] {
            assert_eq!(
                decide(&situation, &state),
                Decision::Switch {
                    to: "AC".to_string(),
                    reason: Reason::WindowOpen,
                },
                "{state:?}"
            );
        }
    }

    #[test]
    fn an_account_that_loses_quota_is_switched_to_without_early_switching() {
        // AC's slack is -6.0: it loses quota at its reset for every hour it does
        // not work. Whatever keeps early switching off, auto-burn moves to it.
        let mut situation = situation(inputs());
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::NotWorthIt, "{state:?}");
        let to_ac = Decision::Switch {
            to: "AC".to_string(),
            reason: Reason::WindowOpen,
        };
        assert_eq!(decide(&situation, &state), to_ac);

        situation.cost = None;
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::NoCostYet);
        assert_eq!(decide(&situation, &state), to_ac);

        // While LB has no rates the fixed order names AC, whose slack is known.
        let mut learning = situation;
        learning.cost = Some(SwitchCost {
            usd: 0.05,
            samples: 3,
        });
        learning.inputs.accounts[2].rates = None;
        let state = regime(&learning);
        assert_eq!(state.why, RegimeWhy::Learning);
        assert_eq!(decide(&learning, &state), to_ac);
    }

    #[test]
    fn a_move_to_an_account_that_loses_quota_waits_for_the_dwell_and_a_pause() {
        let mut situation = situation(inputs());
        let state = regime(&situation);
        assert!(!state.early, "{state:?}");
        situation.inputs.last_switch_at = Some(now() - Duration::minutes(4));
        assert_eq!(decide(&situation, &state), Decision::Stay);
        situation.inputs.last_switch_at = None;
        situation.inputs.paused_until = Some(now() + Duration::hours(3));
        assert_eq!(decide(&situation, &state), Decision::Stay);
    }

    #[test]
    fn going_back_to_an_account_that_loses_quota_after_its_window_resets() {
        // AC loses quota and filled its 5 h window; auto-burn moved to LB,
        // whose slack (15.75 at 64 %) is above the cushion. The window ended five minutes
        // ago. AC's slack is still below 0, so auto-burn goes back to AC, an
        // hour after the switch to LB, though early switching is off.
        let mut inputs = inputs();
        inputs.active = Some("LB".to_string());
        inputs.accounts[2].week_percent = 64.0;
        inputs.accounts[1].session = Some(SessionState {
            percent: 100.0,
            resets_at: Some(now() - Duration::minutes(5)),
        });
        inputs.last_switch_at = Some(now() - Duration::hours(1));
        let ac_slack = slack(&inputs.accounts[1], &inputs).expect("AC has rates");
        assert!(ac_slack <= 0.0, "{ac_slack}");
        let situation = situation(inputs);
        let state = regime(&situation);
        assert!(!state.early, "{state:?}");
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn an_account_auto_burn_may_not_use_is_never_a_target() {
        // AC has the least slack, but auto-burn may not use it; LB's slack of
        // -0.25 is within the cushion, so it is the target.
        let mut inputs = inputs();
        inputs.accounts[1].may_use = false;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "LB".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn an_active_account_auto_burn_may_not_use_is_left_only_when_blocked() {
        let mut inputs = inputs();
        inputs.active = Some("AC".to_string());
        inputs.accounts[1].may_use = false;
        // AC's c of 100 means its remaining 100 % needs only 1 USD; its 19 windows
        // take 19 USD, so its slack is 18.0 USD and LB (-1.25) lies more than the
        // 2 USD margin below it. The regime is stated directly: this case is
        // about decide, not about whether the eager plan pays.
        inputs.accounts[1].rates = Some(Rates::given(100.0, Some(100.0)));
        let fresh = situation(inputs);
        let early = Regime {
            early: true,
            why: RegimeWhy::Pays,
            lazy_blocked_h: 1.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: Some(0.0),
        };
        assert_eq!(decide(&fresh, &early), Decision::Stay);

        // Blocked, S1 still leaves it: the target is another account.
        let mut blocked = fresh;
        blocked.inputs.accounts[1].week_percent = 100.0;
        assert_eq!(
            decide(&blocked, &early),
            Decision::Switch {
                to: "LB".to_string(),
                reason: Reason::ActiveFull {
                    until: Some(blocked.inputs.accounts[1].week_resets_at),
                },
            }
        );
    }

    #[test]
    fn no_typical_hour_means_learning() {
        let mut inputs = inputs();
        inputs.rhythm = week(0);
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(state.why, RegimeWhy::Learning);
        assert!(!state.early);
        assert!(state.lazy_blocked_h.abs() < EPS);
        assert!(state.eager_blocked_h.abs() < EPS);
        assert_eq!(state.extra_switches, 0);
        assert_eq!(state.cost_h, None);
    }

    #[test]
    fn a_capped_account_without_a_session_rate_is_not_learning() {
        // A capped account with a known `c` and no `s` yet is planned without
        // its 5 h limit; the regime uses the same rule as the plan.
        let mut inputs = inputs();
        assert!(
            inputs.accounts[2].session.is_some(),
            "the account is capped"
        );
        inputs.accounts[2].rates = Some(Rates::given(4.0, None));
        assert_ne!(regime(&situation(inputs)).why, RegimeWhy::Learning);
        // Without any `c` it is still learning.
        let mut inputs = super::super::plan::testkit::inputs();
        inputs.accounts[2].rates = None;
        assert_eq!(regime(&situation(inputs)).why, RegimeWhy::Learning);
    }

    #[test]
    fn situation_check_rejects_a_clock_that_went_back() {
        let good = situation(inputs());
        assert!(good.check().is_ok());

        let mut future_switch = good.clone();
        future_switch.inputs.last_switch_at = Some(now() + Duration::minutes(1));
        let err = future_switch
            .check()
            .expect_err("a last switch after now is rejected");
        assert!(matches!(err, PlanError::LastSwitchAfterNow { .. }), "{err}");

        let mut future_reading = good;
        future_reading.oldest_reading = now() + Duration::minutes(1);
        let err = future_reading
            .check()
            .expect_err("a reading after now is rejected");
        assert!(
            matches!(err, PlanError::OldestReadingAfterNow { .. }),
            "{err}"
        );
    }

    #[test]
    fn situation_check_runs_the_inputs_check() {
        let mut situation = situation(inputs());
        situation
            .inputs
            .accounts
            .push(situation.inputs.accounts[0].clone());
        let err = situation
            .check()
            .expect_err("the inputs must still pass their own check");
        assert!(
            matches!(err, PlanError::DuplicateAccount { ref id } if id == "PR"),
            "{err}"
        );
    }

    #[test]
    fn a_target_whose_slack_is_within_the_margin_is_not_switched_to() {
        let mut inputs = inputs();
        // LB is full. PR's slack is 43 USD (92 USD before its reset minus
        // 49 needed); AC is played uncapped with c = 2 and the next Monday's
        // reset, so its slack is 92 - 50 = 42 USD. That is one USD below
        // PR and inside the 2-hour margin, so there is no early switch.
        inputs.accounts[2].week_percent = 100.0;
        inputs.accounts[0].week_percent = 2.0;
        inputs.accounts[1].session = None;
        inputs.accounts[1].rates = Some(Rates::given(2.0, None));
        let pr_slack = slack(&inputs.accounts[0], &inputs).expect("PR has rates");
        let ac_slack = slack(&inputs.accounts[1], &inputs).expect("AC has rates");
        assert!((pr_slack - 43.0).abs() < EPS, "{pr_slack}");
        assert!((ac_slack - 42.0).abs() < EPS, "{ac_slack}");
        assert_eq!(choose(&inputs, now()), Some("AC".to_string()));

        let situation = situation(inputs);
        assert_eq!(decide(&situation, &pays()), Decision::Stay);
    }

    #[test]
    fn an_uncapped_target_switches_for_its_reset_near() {
        let mut inputs = inputs();
        inputs.active = Some("AC".to_string());
        // LB is full. AC's slack is 19 - (100 - 84) / 4 = 15 USD; PR's is
        // 92 - (100 - 13) / 1 = 5 USD, so choose() names the uncapped PR.
        inputs.accounts[2].week_percent = 100.0;
        inputs.accounts[1].week_percent = 84.0;
        inputs.accounts[0].week_percent = 13.0;
        inputs.accounts[0].rates = Some(Rates::given(1.0, None));
        assert_eq!(choose(&inputs, now()), Some("PR".to_string()));
        let situation = situation(inputs);
        assert_eq!(
            decide(&situation, &pays()),
            Decision::Switch {
                to: "PR".to_string(),
                reason: Reason::ResetNear,
            }
        );
    }

    #[test]
    fn a_gain_below_the_minimum_is_not_worth_it() {
        let mut inputs = inputs();
        // One account only: both plans are the same and gain nothing, so
        // extra_switches is 0 and the gain is below MIN_GAIN_H.
        inputs.accounts.truncate(1);
        inputs.active = Some("PR".to_string());
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(state.extra_switches, 0);
        assert!(
            (state.lazy_blocked_h - state.eager_blocked_h).abs() < EPS,
            "{} vs {}",
            state.lazy_blocked_h,
            state.eager_blocked_h
        );
        assert_eq!(state.why, RegimeWhy::NotWorthIt);
        assert!(!state.early);
    }

    #[test]
    fn all_full_skips_an_account_auto_burn_may_not_use() {
        let mut inputs = inputs();
        for account in &mut inputs.accounts {
            account.week_percent = 100.0;
        }
        // PR would be usable again first (Saturday 19:00), but auto-burn may not
        // use it, so AC (Monday 02:00) is next.
        inputs.accounts[0].may_use = false;
        let ac_reset = inputs.accounts[1].week_resets_at;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::AllFull {
                next: Some(("AC".to_string(), ac_reset)),
            }
        );
    }

    #[test]
    fn all_full_breaks_an_equal_time_by_account_id() {
        let reset = "2026-11-21T19:00:00Z";
        let mut lb = account("LB", 4.0, Some(80.0), true, reset);
        lb.week_percent = 100.0;
        let mut ac = account("AC", 4.0, Some(100.0), true, reset);
        ac.week_percent = 100.0;
        let mut inputs = inputs();
        // LB comes first and has the same time as AC; the id rule takes AC.
        inputs.accounts = vec![lb, ac];
        inputs.active = Some("LB".to_string());
        let when = inputs.accounts[1].week_resets_at;
        let situation = situation(inputs);
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::AllFull {
                next: Some(("AC".to_string(), when)),
            }
        );
    }

    #[test]
    fn a_manual_hold_that_ends_now_is_no_hold() {
        let mut situation = situation(inputs());
        situation.inputs.paused_until = Some(now());
        let state = regime(&situation);
        assert_eq!(
            decide(&situation, &state),
            Decision::Switch {
                to: "AC".to_string(),
                reason: Reason::WindowOpen,
            }
        );
    }

    #[test]
    fn a_full_active_account_goes_back_to_the_account_with_the_thinking() {
        let reset = "2026-11-21T19:00:00Z";
        let mut z = account("Z", 2.0, None, false, reset);
        z.week_percent = 100.0;
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 2.0;
        x2.thinking_held = 5;
        let inputs = Inputs {
            now: now(),
            offset: crate::burn::plan::testkit::offset(),
            accounts: vec![z, x1, x2],
            active: Some("Z".to_string()),
            rhythm: week(15),
            last_switch_at: None,
            paused_until: None,
        };
        let situation = situation(inputs);
        let state = regime(&situation);

        match decide(&situation, &state) {
            Decision::Switch {
                to,
                reason: Reason::ActiveFull { .. },
            } => assert_eq!(to, "X2"),
            other => panic!("{other:?}"),
        }
        // The pop-up's plan plays through the same `pick`: it names the account
        // the decision switches to.
        let plan = follow(&situation.inputs, Policy::Lazy);
        let planned = plan
            .segments
            .iter()
            .find(|segment| segment.account.as_deref() != Some("Z"))
            .and_then(|segment| segment.account.as_deref());
        assert_eq!(planned, Some("X2"));
    }

    /// Review Focus 6, the other half: the account with the thinking is outside
    /// the margin (slack 45.0 against 42.0, margin 2.0), so the clearly more
    /// urgent third account is taken, by the decision and by the plan.
    #[test]
    fn a_full_active_account_goes_to_the_clearly_more_urgent_third_one() {
        let reset = "2026-11-21T19:00:00Z";
        let mut z = account("Z", 2.0, None, false, reset);
        z.week_percent = 100.0;
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 6.0;
        x2.thinking_held = 5;
        let inputs = Inputs {
            now: now(),
            offset: crate::burn::plan::testkit::offset(),
            accounts: vec![z, x1, x2],
            active: Some("Z".to_string()),
            rhythm: week(15),
            last_switch_at: None,
            paused_until: None,
        };
        let situation = situation(inputs);
        let state = regime(&situation);

        match decide(&situation, &state) {
            Decision::Switch { to, .. } => assert_eq!(to, "X1"),
            other => panic!("{other:?}"),
        }
        let plan = follow(&situation.inputs, Policy::Lazy);
        let planned = plan
            .segments
            .iter()
            .find(|segment| segment.account.as_deref() != Some("Z"))
            .and_then(|segment| segment.account.as_deref());
        assert_eq!(planned, Some("X1"));
    }

    #[test]
    fn an_early_switch_waits_for_the_account_with_the_thinking() {
        let reset = "2026-11-21T19:00:00Z";
        let mut z = account("Z", 2.0, None, false, reset);
        z.week_percent = 7.0;
        let x1 = account("X1", 2.0, None, false, reset);
        let mut x2 = account("X2", 2.0, None, false, reset);
        x2.week_percent = 3.5;
        x2.thinking_held = 5;
        let inputs = Inputs {
            now: now(),
            offset: crate::burn::plan::testkit::offset(),
            accounts: vec![z, x1, x2],
            active: Some("Z".to_string()),
            rhythm: week(15),
            last_switch_at: None,
            paused_until: None,
        };
        let z_slack = slack(&inputs.accounts[0], &inputs).expect("Z has rates");
        let x1_slack = slack(&inputs.accounts[1], &inputs).expect("X1 has rates");
        let x2_slack = slack(&inputs.accounts[2], &inputs).expect("X2 has rates");
        assert!((z_slack - 45.5).abs() < EPS, "{z_slack}");
        assert!((x1_slack - 42.0).abs() < EPS, "{x1_slack}");
        assert!((x2_slack - 43.75).abs() < EPS, "{x2_slack}");
        let margin = SLACK_MARGIN_H * inputs.rhythm.typical_hour();
        assert!((margin - 2.0).abs() < EPS, "{margin}");

        let mut situation = situation(inputs);
        assert_eq!(decide(&situation, &pays()), Decision::Stay);

        situation.inputs.accounts[2].thinking_held = 0;
        assert!(matches!(
            decide(&situation, &pays()),
            Decision::Switch { ref to, .. } if to == "X1"
        ));
    }
}
