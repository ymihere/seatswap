//! Auto-burn (spec §2): the work of a session's tokens in API dollars, the weekly
//! rhythm of work, and each account's rates. The plans, the decision, the cost
//! of a switch and the replay join these types in the tasks after this one.

use chrono::{DateTime, FixedOffset, Utc};

pub mod burn_next;
pub mod cost;
pub mod decide;
pub mod fit;
pub mod live;
pub mod plan;
pub mod rates;
pub mod replay;
pub mod tied;

use fit::Factor;

/// A factor is measured from this much work of its own, in USD (spec R2 of
/// 1.3). While the account's own work is below it, an assumed factor (the
/// prior) weighs as much as this many dollars of its own measurement; from it
/// on the prior is dropped (R7 of 1.4).
pub const LEARN_MIN_USD: f64 = 100.0;
/// The assumed weekly percent per API dollar of a plan with capacity 1 (spec
/// R2 of 1.3, "assumed factor"): a plan of capacity `n` (its tier ends in
/// `_<n>x`, Pro is 1) is assumed at this divided by `n`. From the fit on the
/// owner's history of 17 Nov 2026: Max ≈ 0.15, Team Premium ≈ 0.087, Ultra ≈
/// 0.042 % per USD.
pub const ASSUMED_WEEK_PERCENT_PER_USD_AT_1X: f64 = 0.75;
/// A pair of readings counts for a factor only when the account was live on
/// some seatswap host for at least this share of the pair's time (spec R2 of
/// 1.3, documented rule): a rise outside that time comes from use seatswap
/// does not see.
pub const MIN_LIVE_SHARE: f64 = 0.9;
/// The rhythm averages this many weeks; the newer two count double.
pub const RHYTHM_WEEKS: i64 = 4;
/// Rates are learned from the samples of this many days.
pub const RATES_DAYS: i64 = 14;
/// The plan's step (spec §2.4).
pub const STEP_MIN: i64 = 15;
/// The plan's horizon (spec §2.4).
pub const HORIZON_H: i64 = 168;

/// A plan's switches as switches per day of its [`HORIZON_H`] hours: the
/// "about N a day" of Settings → Auto-burn and of the Forecast page.
pub fn switches_per_day(switches: u32) -> f64 {
    f64::from(switches) / (HORIZON_H as f64 / 24.0)
}
/// The shortest time between two switches (spec §2.5).
pub const MIN_DWELL_MIN: i64 = 30;
/// The margin of S2, in typical working hours (spec §2.5).
pub const SLACK_MARGIN_H: f64 = 2.0;
/// An account whose slack is this many typical working hours or less is at
/// risk of losing quota at its weekly reset: the machines may be off longer
/// than the rhythm expects. S2 moves to it even while early switching is off
/// (spec §7.4).
pub const EXPIRY_CUSHION_H: f64 = 8.0;
/// A switch made by hand holds the early rule back this many hours (spec §2.5).
pub const MANUAL_HOLD_H: i64 = 4;
/// Early switching must save at least this much, in typical working hours
/// (spec §2.5).
pub const MIN_GAIN_H: f64 = 0.5;
/// Early switching must also save at least this many times what its extra
/// switches cost (spec §2.5).
pub const GAIN_FACTOR: f64 = 2.0;
/// A measured switch cost needs at least this many switches (spec §2.5).
pub const MIN_COST_SAMPLES: u32 = 3;
/// A decision needs readings at most this many minutes old for every account
/// auto-burn may use (spec §2.5).
pub const FRESH_MIN: i64 = 20;
/// A switch event that lands after the files it names is learned again after at
/// most this many minutes (spec §5.3: the CLI writes the files before the event).
pub const SWITCH_RELEARN_MIN: i64 = 1;
/// A decision asks for a poll of old readings at most this often (spec §7.4.6).
pub const STALE_ASK_SECS: i64 = 60;
/// A failed history read is tried again after this many minutes (spec §7.4.6).
pub const LEARN_RETRY_MIN: i64 = 5;
/// The same switch decision is sent again after this many minutes (spec §2.5).
pub const SWITCH_RETRY_MIN: i64 = 10;

/// The hours of the week.
const HOURS: usize = 168;

/// How much one of the [`RHYTHM_WEEKS`] weeks weighs in the rhythm: the two
/// newer ones weigh 2, the two older ones 1.
fn week_weight(week: i64) -> f64 {
    if week < 2 {
        2.0
    } else {
        1.0
    }
}

/// API dollars as hours at `typical_usd` dollars per typical working hour;
/// `None` when `typical_usd` is 0.0, because no hours can be computed then. The
/// one dollars-to-hours conversion (spec §2.3, §2.4).
pub fn usd_hours(usd: f64, typical_usd: f64) -> Option<f64> {
    (typical_usd > 0.0).then(|| usd / typical_usd)
}

/// API dollars of work per clock hour of the week, in the local time of the
/// offset the rhythm was learned in; index 0 is Monday 00:00.
#[derive(Debug, Clone, PartialEq)]
pub struct Rhythm {
    pub usd: [f64; HOURS],
}

impl Rhythm {
    /// The dollars of the hour that holds `t`.
    pub fn at(&self, t: DateTime<Utc>, offset: FixedOffset) -> f64 {
        self.usd[crate::stats::local_hour_of_week(t, &offset)]
    }

    /// The mean dollars over the hours that have work, 0.0 when there is none:
    /// the "work of a typical working hour" of spec §2.3.
    pub fn typical_hour(&self) -> f64 {
        mean_over_hours(self.usd.iter().copied())
    }
}

/// The mean over the values that have work, 0.0 when there is none: the "work
/// of a typical working hour" of spec §2.3. The one function behind the learned
/// rhythm's [`Rhythm::typical_hour`] and the recorded hours of a replay.
pub(crate) fn mean_over_hours(hours: impl IntoIterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for usd in hours {
        if usd > 0.0 {
            sum += usd;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// One account's rates (spec §2.3, R2 of 1.3): `c` is the factor of its week,
/// the percent of the week one API dollar of work costs, `s` the factor of one
/// 5 h window. `s` is `None` for an account without a session limit, or until
/// it can be measured or assumed. The plan uses each factor's `value`; neither
/// is ever 0 or below ([`fit::factor`] gives no such factor).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub c: Factor,
    pub s: Option<Factor>,
}

#[cfg(any(test, feature = "fixtures"))]
impl Rates {
    /// Rates given as numbers rather than learned, for tests and fixtures that
    /// play a plan from known rates: each factor's value without a range,
    /// measured on [`LEARN_MIN_USD`] of work.
    pub fn given(c: f64, s: Option<f64>) -> Rates {
        let factor = |value: f64| Factor {
            value,
            low: value,
            high: value,
            basis_usd: LEARN_MIN_USD,
            measured: true,
        };
        Rates {
            c: factor(c),
            s: s.map(factor),
        }
    }
}
