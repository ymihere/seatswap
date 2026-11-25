//! The statistics pages (spec §3.1–§3.6, §3.9): what the history, the prices
//! and the projects mean for a chosen range. Everything here only reads.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, MappedLocalTime, NaiveDate, NaiveDateTime, TimeZone,
    Timelike, Utc,
};
use serde::{Deserialize, Serialize};

use crate::api::LimitKind;
use crate::burn::{rates, Rhythm, RHYTHM_WEEKS};
use crate::history::{
    AccountRef, By, Event, EventKind, HistoryDir, HistoryError, Hosted, Hosts, Range, Record,
    Sample, SessionRecord, TokenRecord, UpRecord, WindowRecord,
};
use crate::live_sessions::Values;
use crate::money::{
    no_openai_login_warning, window_money, Company, Fx, Missing, MoneyError, Prices, Valued,
    Valuer, WindowMoney,
};
use crate::projects::{Projects, ProjectsLogRecord};
use crate::store::{Account, ProviderSubscription, Settings};

pub mod activity;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
pub mod forecast;
pub mod overview;
pub mod plans;
pub mod projects;
pub mod subscriptions;
pub mod weeks;

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Money(#[from] MoneyError),
    /// A value the statistics cannot compute; the text says what is missing.
    #[error("{0}")]
    Message(String),
}

pub(crate) use crate::history::keep_warnings;

/// Add `next` to a value that may not exist yet, through [`Values::sum`].
pub(crate) fn add_opt_value(current: &mut Option<Values>, next: &Values) {
    let sum = match current {
        Some(current) => Values::sum(&[current, next]),
        None => next.clone(),
    };
    *current = Some(sum);
}

/// Add `next` to one key's value, through [`Values::sum`].
pub(crate) fn add_value<K: Ord>(values: &mut BTreeMap<K, Values>, key: K, next: &Values) {
    match values.get_mut(&key) {
        Some(current) => {
            let sum = Values::sum(&[current, next]);
            *current = sum;
        }
        None => {
            values.insert(key, next.clone());
        }
    }
}

/// One half's value when it is full, else `None` with the half's reason. A
/// share, ratio or percent is computed only from halves that are not partial
/// (R3): a partial half is a known lower bound, and a quotient of a lower bound
/// is not a figure. An amount on its own may stay partial and is drawn "≥".
///
/// The one exception is a share of an API value: it divides the priced part of
/// a part by the priced part of the whole ([`priced_api`]), so priced work keeps
/// its share when other work has no price.
pub(crate) fn full_half(
    value: Option<f64>,
    partial: bool,
    missing: Option<&str>,
) -> (Option<f64>, Option<String>) {
    if partial || value.is_none() {
        (None, missing.map(str::to_string))
    } else {
        (value, None)
    }
}

/// The priced API part of a value: its known API value, whether or not some of
/// its records have no price (R3). A share of an API value divides this part of
/// a part by this part of the whole, so a missing price hides only the work it
/// belongs to, never the rest. `None` when the value has no priced API part at
/// all. A whole whose priced part is `None` or 0 gives no shares.
pub(crate) fn priced_api(values: &Values) -> Option<f64> {
    values.api
}

/// The span of one account week: from the start of its first window to the end
/// of its last one. A week may hold several windows after a reset.
pub(crate) fn week_span(week: &AccountWeek) -> Range {
    // `window` is the last of `windows`; starting from it keeps the span right
    // without assuming `windows` is filled.
    let from = week
        .windows
        .iter()
        .map(WindowRecord::weekly_start)
        .fold(week.window.weekly_start(), DateTime::min);
    Range {
        from,
        to: week.window.end,
    }
}

/// The range that covers every account week whole, widened from `range` so a
/// ratio divides a window by its own span.
pub(crate) fn windows_span(weeks: &[AccountWeek], range: &Range) -> Range {
    let mut span = *range;
    for week in weeks {
        let week = week_span(week);
        span.from = span.from.min(week.from);
        span.to = span.to.max(week.to);
    }
    span
}

/// The range switch of the statistics area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RangeKey {
    ThisWeek,
    LastWeek,
    Days30,
    Days90,
    Months12,
    All,
}

/// What the statistics pages ask for: a range, account ids and one host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    pub range: RangeKey,
    /// Account ids; empty = all.
    pub accounts: Vec<String>,
    /// One host, or every host when `None`.
    pub host: Option<String>,
}

/// Everything a page reads: the history, the store's accounts and settings, the
/// prices, the projects and the local zone the pages are shown in.
#[derive(Clone, Copy)]
pub struct Ctx<'a, Tz: TimeZone + Copy = FixedOffset> {
    pub history: &'a HistoryDir,
    pub accounts: &'a [Account],
    /// The provider subscriptions `accounts.json` stores beside the accounts
    /// (decision P-2 1): their prices and looks, and the OpenAI logins a record
    /// may belong to.
    pub provider_subscriptions: &'a [ProviderSubscription],
    pub settings: &'a Settings,
    pub prices: &'a Prices,
    pub fx: &'a Fx,
    pub projects: &'a Projects,
    /// The history read warnings of the page's reads (spec §1.2). The app
    /// prints the new ones once after the command; the page shows nothing new.
    pub warnings: &'a RefCell<Vec<String>>,
    pub now: DateTime<Utc>,
    /// The time zone of this computer; every local time is the zone's offset at
    /// that instant. The app passes `chrono::Local`, tests a [`FixedOffset`] or
    /// a zone with a daylight-saving change.
    pub zone: Tz,
}

/// ThisWeek: Monday 00:00 local of this week .. now. LastWeek: the week before. Days30 / Days90 / Months12: from the
/// Monday 00:00 of the week that holds now - 30 d / 90 d / 365 d .. now. All: from the Monday 00:00 local of the
/// week that holds the first record .. now, so every page's first calendar week holds all its windows; when the
/// history holds no record, from this week's Monday 00:00.
pub fn range_of<Tz: TimeZone + Copy>(key: RangeKey, ctx: &Ctx<Tz>) -> Result<Range, StatsError> {
    let this_week = week_start(ctx.now, &ctx.zone);
    let (from, to) = match key {
        RangeKey::ThisWeek => (this_week, ctx.now),
        RangeKey::LastWeek => (previous_week_start(this_week, &ctx.zone), this_week),
        RangeKey::Days30 => (week_start(ctx.now - Duration::days(30), &ctx.zone), ctx.now),
        RangeKey::Days90 => (week_start(ctx.now - Duration::days(90), &ctx.zone), ctx.now),
        RangeKey::Months12 => (
            week_start(ctx.now - Duration::days(365), &ctx.zone),
            ctx.now,
        ),
        RangeKey::All => (
            first_record(ctx)?
                .map(|first| week_start(first, &ctx.zone))
                .unwrap_or(this_week),
            ctx.now,
        ),
    };
    Range::between(Some(from), Some(to)).map_err(StatsError::from)
}

/// The instant a local wall-clock time starts at: one that does not exist (a
/// change at that time) starts at the first instant after the gap, and one that
/// exists twice starts at the earlier instant. The one rule for every local
/// day and week start; [`local_hour_start`] adds the occurrence an instant
/// falls in for an hour that exists twice.
fn local_start<Tz: TimeZone>(zone: &Tz, local: NaiveDateTime) -> DateTime<Utc> {
    match zone.from_local_datetime(&local) {
        MappedLocalTime::Single(at) => at.with_timezone(&Utc),
        MappedLocalTime::Ambiguous(a, b) => fold(a, b).0,
        MappedLocalTime::None => {
            // The gap holds no instant. The first local clock second that
            // exists again is the gap's end, which is the first instant after
            // it. A gap is finite, so this loop ends.
            let mut after = local;
            loop {
                after += Duration::seconds(1);
                match zone.from_local_datetime(&after) {
                    MappedLocalTime::Single(at) => return at.with_timezone(&Utc),
                    MappedLocalTime::Ambiguous(a, b) => return fold(a, b).0,
                    MappedLocalTime::None => {}
                }
            }
        }
    }
}

/// The two instants of a local time that exists twice, earlier first. chrono
/// does not keep one order: its `Local` on Linux names the later instant first
/// (seen with chrono 0.4.45 in Europe/Vienna on 25 Oct 2026), so every caller
/// orders them here instead of trusting the order of `Ambiguous`.
fn fold<Tz: TimeZone>(a: DateTime<Tz>, b: DateTime<Tz>) -> (DateTime<Utc>, DateTime<Utc>) {
    let (a, b) = (a.with_timezone(&Utc), b.with_timezone(&Utc));
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// The start of the local hour `t` falls in, as an instant. Every date has an
/// hour that holds `t`; when the wall-clock hour exists twice, this is the
/// occurrence that holds `t`, so consecutive hours never go backwards.
pub(crate) fn local_hour_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> DateTime<Utc> {
    let local = t.with_timezone(zone);
    let into_hour = Duration::minutes(i64::from(local.minute()))
        + Duration::seconds(i64::from(local.second()))
        + Duration::nanoseconds(i64::from(local.nanosecond()));
    let wall = local.naive_local() - into_hour;
    match zone.from_local_datetime(&wall) {
        MappedLocalTime::Single(start) => start.with_timezone(&Utc),
        MappedLocalTime::Ambiguous(a, b) => {
            let (earlier, later) = fold(a, b);
            if t >= later {
                later
            } else {
                earlier
            }
        }
        MappedLocalTime::None => local_start(zone, wall),
    }
}

/// The instant local midnight of `date` starts at.
pub(crate) fn local_date_start<Tz: TimeZone>(date: NaiveDate, zone: &Tz) -> DateTime<Utc> {
    local_start(zone, date.and_time(chrono::NaiveTime::MIN))
}

/// The start of the local day `t` falls in, as an instant.
pub(crate) fn local_day_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> DateTime<Utc> {
    let local = t.with_timezone(zone);
    local_date_start(local.date_naive(), zone)
}

/// The next local midnight after `t`: a local day lasts 23, 24 or 25 hours, so
/// this is not `t + 24 h`.
pub(crate) fn next_local_day_start<Tz: TimeZone + Copy>(
    t: DateTime<Utc>,
    zone: &Tz,
) -> DateTime<Utc> {
    let date = t.with_timezone(zone).date_naive() + Duration::days(1);
    local_date_start(date, zone)
}

/// Monday 00:00 local time of the week that holds `t`, as an instant.
pub fn week_start<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> DateTime<Utc> {
    let local = t.with_timezone(zone);
    let monday =
        local.date_naive() - Duration::days(i64::from(local.weekday().num_days_from_monday()));
    local_date_start(monday, zone)
}

/// Monday 00:00 of the week after the one holding `week`; a local week lasts
/// 167, 168 or 169 hours, so this is not `week + 7 d`.
pub(crate) fn next_week_start<Tz: TimeZone + Copy>(
    week: DateTime<Utc>,
    zone: &Tz,
) -> DateTime<Utc> {
    let monday = week.with_timezone(zone).date_naive() + Duration::days(7);
    local_date_start(monday, zone)
}

/// Monday 00:00 of the week before the one holding `week`.
pub(crate) fn previous_week_start<Tz: TimeZone + Copy>(
    week: DateTime<Utc>,
    zone: &Tz,
) -> DateTime<Utc> {
    let monday = week.with_timezone(zone).date_naive() - Duration::days(7);
    local_date_start(monday, zone)
}

/// The statistics frame: the hosts, when the history's subscription values
/// begin and how large the whole history is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsFrame {
    pub hosts: Vec<String>,
    /// The earliest usage reading or switch of any host; token and session
    /// records are older and do not set it. `None` for a history with neither.
    pub history_start: Option<DateTime<Utc>>,
    /// The size of every host folder of the history, as Settings → History
    /// counts it ([`HistoryDir::host_bytes`]).
    pub history_bytes: u64,
}

/// The frame's answer: the host list, the history's first reading or switch and
/// its size. Only the month files up to the first reading or switch are read.
pub fn frame(history: &HistoryDir) -> Result<StatsFrame, StatsError> {
    let hosts = history.hosts()?;
    let mut history_start: Option<DateTime<Utc>> = None;
    let mut history_bytes = 0u64;
    for host in &hosts {
        let firsts = [
            history.first_where::<Sample>(host, |_| true)?,
            history.first_where::<Event>(host, |event| {
                matches!(event.kind, EventKind::Switch { .. })
            })?,
        ];
        for first in firsts.into_iter().flatten() {
            history_start = Some(history_start.map_or(first, |at| at.min(first)));
        }
        history_bytes = history_bytes
            .checked_add(history.host_bytes(host)?)
            .ok_or_else(|| {
                StatsError::Message(format!(
                    "the history files add up to more than 2^64 bytes: {}",
                    history.root().display()
                ))
            })?;
    }
    Ok(StatsFrame {
        hosts,
        history_start,
        history_bytes,
    })
}

/// The weekly windows of one account whose midpoints fall in one calendar week.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountWeek {
    pub week: DateTime<Utc>,
    pub account: AccountRef,
    /// Every weekly window of the account whose midpoint falls in this
    /// calendar week, oldest end first: the finished ones, and with
    /// [`account_weeks_so_far`] the running one last.
    pub windows: Vec<WindowRecord>,
    /// The last of `windows` is still running ([`Valuer::running_windows`]):
    /// its use is the use so far and it has expired nothing yet.
    pub running: bool,
    /// The last of `windows`: the window whose end, lost percent and plan the
    /// week shows.
    pub window: WindowRecord,
    /// The sum of the windows' use: each window's `final` minus its `from`
    /// (absent = 0). It may pass 100 after a reset.
    pub used: f64,
    /// The account's monthly price at the last window's midpoint, in the display
    /// currency, from the [`EventKind::PriceChanged`] events of the history;
    /// `None` when `price_missing` says why there is none.
    pub price: Option<f64>,
    /// Why `price` is `None`; `None` when it has a value.
    pub price_missing: Option<Missing>,
}

/// The use of one window: its `final` percent minus its `from` (absent = 0).
/// A reset window starts at 0, so its use is its final percent. `None` when the
/// window ends below its start: such a window adds nothing to the week's use,
/// and every caller counts no use for it (the page names it with the warning
/// [`account_weeks_valued`] adds).
pub(crate) fn window_used(window: &WindowRecord) -> Option<f64> {
    let from = window.from.unwrap_or(0.0);
    if window.final_percent < from {
        return None;
    }
    Some(window.final_percent - from)
}

/// The middle of a weekly window, which names the calendar week it belongs to;
/// `None` only when chrono cannot place it.
pub(crate) fn window_midpoint(window: &WindowRecord) -> Option<DateTime<Utc>> {
    let start = window.weekly_start();
    start.checked_add_signed((window.end - start) / 2)
}

/// Name a window that ends below its start in the page's warnings: the one text
/// both the weeks list and the valuer push, added once, so a page that runs both
/// shows it once.
pub(crate) fn warn_window_below_start(warnings: &RefCell<Vec<String>>, window: &WindowRecord) {
    let warning = format!(
        "the weekly window of {} ending {} ends at {} % below its start at {} %; \
         it adds nothing to the week's use",
        window.account.acc,
        window.end,
        window.final_percent,
        window.from.unwrap_or(0.0),
    );
    let mut shown = warnings.borrow_mut();
    if !shown.contains(&warning) {
        shown.push(warning);
    }
}

/// Every finished weekly_all window whose midpoint lies in the range, of the chosen accounts, any host; if several
/// hosts recorded the same window (same account and end), the one with the latest lastSample. One entry per account
/// and calendar week: every window whose midpoint falls in that week is kept in `windows`, oldest first.
pub fn account_weeks<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
) -> Result<Vec<AccountWeek>, StatsError> {
    let valuer = Valuer::build(
        ctx.history,
        range,
        ctx.prices,
        ctx.fx,
        ctx.settings,
        ctx.accounts,
        ctx.provider_subscriptions,
        ctx.warnings,
    )?;
    account_weeks_valued(ctx, range, q, &valuer)
}

/// [`account_weeks`] with a [`Valuer`] the caller already built for the same
/// range, so a page that values token records too builds only one. One entry
/// per account and calendar week: every window whose midpoint falls in that
/// week is kept in `windows`, oldest first; `used` sums their use and `window`
/// is the last. The windows are every company's: a Claude account's week and an
/// OpenAI or Google subscription's `weekly_all` week (an Ollama subscription counts per
/// month, [`subscription_months`]). A page that serves Claude's accounts alone
/// leaves the provider weeks out by [`Company::of_account`].
pub(crate) fn account_weeks_valued<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    valuer: &Valuer,
) -> Result<Vec<AccountWeek>, StatsError> {
    let windows = keep_warnings(ctx.warnings, || {
        ctx.history.finished_windows(&Range::ALL, &Hosts::All)
    })?;
    let finished = windows
        .records
        .into_iter()
        .map(|hosted| (hosted.record, false));
    Ok(weeks_of_windows(ctx, range, q, valuer, finished))
}

/// [`account_weeks_valued`] with the weekly windows still running at
/// `ctx.now` ([`Valuer::running_windows`]): each is placed by its midpoint like
/// a finished window (its end is the known reset, its start seven days
/// before), so it never moves to another week when it finishes. Its use is
/// the use so far (`final − from`), it offers one week's price like any window
/// and it expires nothing ([`week_lost_percent`]); its week is `running`.
pub(crate) fn account_weeks_so_far<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    valuer: &Valuer,
) -> Result<Vec<AccountWeek>, StatsError> {
    let windows = keep_warnings(ctx.warnings, || {
        ctx.history.finished_windows(&Range::ALL, &Hosts::All)
    })?;
    let finished = windows
        .records
        .into_iter()
        .map(|hosted| (hosted.record, false));
    let running = valuer
        .running_windows(ctx.now)
        .into_iter()
        .map(|window| (window, true));
    Ok(weeks_of_windows(
        ctx,
        range,
        q,
        valuer,
        finished.chain(running),
    ))
}

/// The chosen accounts' weekly windows still running at `ctx.now`
/// ([`Valuer::running_windows`]), one account week each, wherever their
/// midpoint lies: the windows Overview's This-week card shows, whatever range
/// the page has. Claude's accounts alone: a provider subscription's running
/// week is that card's provider row, not an account row. Each is priced at its
/// own midpoint like any account week; its use is the use so far (`final −
/// from`) and it has expired nothing yet.
pub(crate) fn running_account_weeks<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    q: &Query,
    valuer: &Valuer,
) -> Vec<AccountWeek> {
    let running = valuer
        .running_windows(ctx.now)
        .into_iter()
        .filter(|window| Company::of_account(&window.account) == Company::Anthropic)
        .map(|window| (window, true));
    weeks_of_windows(ctx, &Range::ALL, q, valuer, running)
}

/// One month of an Ollama subscription (decision P-2 9): the share of the
/// month's credits used, the money of that use and the money the finished month
/// left unused.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonthValued {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The share of the month's credits used, in percent: the newest reading's
    /// percent minus the reading a reset began the month at.
    pub used_percent: f64,
    /// The money of `used_percent` at the subscription's price for the month;
    /// `None` without a price, never 0.
    pub money: Option<f64>,
    /// The money the finished month left unused; `Some(0.0)` while the month
    /// runs, because it has expired nothing yet; `None` without a price.
    pub expired: Option<f64>,
}

/// The finished and running months of the Ollama subscription `account` that
/// overlap `range`, oldest end first (decision P-2 9). An Ollama subscription
/// counts per month, not per week: `used_percent` is the share of the credits
/// used, not a week's use. Like every page function it takes a range, and
/// [`Valuer::build`] reads every month that overlaps it whole (decision P-2 2).
pub fn subscription_months<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    account: &AccountRef,
) -> Result<Vec<MonthValued>, StatsError> {
    let valuer = Valuer::build(
        ctx.history,
        range,
        ctx.prices,
        ctx.fx,
        ctx.settings,
        ctx.accounts,
        ctx.provider_subscriptions,
        ctx.warnings,
    )?;
    Ok(valuer
        .month_windows(account)
        .into_iter()
        .filter(|month| month.end > range.from && month.start < range.to)
        .map(|month| {
            let running = month.end > ctx.now;
            let (money, expired) = match month.money {
                Some(money) => (
                    Some(money.used),
                    Some(if running { 0.0 } else { money.expired }),
                ),
                None => (None, None),
            };
            MonthValued {
                start: month.start,
                end: month.end,
                used_percent: month.used,
                money,
                expired,
            }
        })
        .collect())
}

/// What a range that holds now says about the chosen accounts' running weekly
/// windows that count in a week it does not show: a window counts in the week
/// of its midpoint, and a running window's midpoint lies within three and a
/// half days of now, so it can count in last week or in next week. One
/// sentence per such week, last week first, e.g. `AC's running weekly window
/// counts in last week, the week that holds its middle.`; empty when the range
/// does not hold now or every running window counts in one of its weeks.
pub(crate) fn running_elsewhere<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    valuer: &Valuer,
) -> Vec<String> {
    if ctx.now < range.from || ctx.now > range.to {
        return Vec::new();
    }
    let this_week = week_start(ctx.now, &ctx.zone);
    // Last week's and next week's running windows, by account name.
    let mut before: Vec<String> = Vec::new();
    let mut after: Vec<String> = Vec::new();
    for window in valuer.running_windows(ctx.now) {
        if !account_chosen(ctx, q, Some(&window.account)) {
            continue;
        }
        let Some(midpoint) = window_midpoint(&window) else {
            continue;
        };
        let week = week_start(midpoint, &ctx.zone);
        let name = account_abbr(ctx, &window.account);
        if midpoint < range.from && week < this_week {
            before.push(name);
        } else if week >= range.to && week > this_week {
            after.push(name);
        }
    }
    let sentence = |names: &[String], when: &str| match names {
        [one] => format!(
            "{one}'s running weekly window counts in {when}, the week that holds its middle."
        ),
        _ => format!(
            "The running weekly windows of {} count in {when}, the week that holds their middle.",
            join_names(names)
        ),
    };
    let mut sentences = Vec::new();
    if !before.is_empty() {
        sentences.push(sentence(&before, "last week"));
    }
    if !after.is_empty() {
        sentences.push(sentence(&after, "next week"));
    }
    sentences
}

/// The account weeks of `windows` (each with whether it still runs): the
/// weekly_all windows of the chosen accounts whose midpoint lies in the range,
/// grouped by account and calendar week, oldest end first, each week priced at
/// its last window.
fn weeks_of_windows<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    valuer: &Valuer,
    windows: impl IntoIterator<Item = (WindowRecord, bool)>,
) -> Vec<AccountWeek> {
    let mut windows: Vec<(WindowRecord, bool)> = windows.into_iter().collect();
    // A running window ends after every finished one of its account; the
    // stable sort keeps the finished list's own order (by end, then account).
    windows.sort_by(|a, b| {
        a.0.end
            .cmp(&b.0.end)
            .then_with(|| a.0.account.cmp(&b.0.account))
    });
    let mut weeks: Vec<AccountWeek> = Vec::new();
    for (window, running) in windows {
        if window.k != LimitKind::WeeklyAll {
            continue;
        }
        let Some(midpoint) = window_midpoint(&window) else {
            continue;
        };
        // The range chooses a window whose midpoint's calendar week is one of
        // its weeks. A finished window's midpoint lies before now, so for a
        // range that ends now or on a Monday this is "its midpoint lies in the
        // range"; a running window's midpoint may still lie ahead in the week
        // that holds now.
        let week = week_start(midpoint, &ctx.zone);
        if midpoint < range.from || week >= range.to {
            continue;
        }
        if !account_chosen(ctx, q, Some(&window.account)) {
            continue;
        }
        let used = match window_used(&window) {
            Some(used) => used,
            None => {
                // A window that ends below its start adds nothing. The text is
                // the one `Valuer::build` uses for the same window: a page that
                // runs both shows it once.
                warn_window_below_start(ctx.warnings, &window);
                0.0
            }
        };
        match weeks
            .iter_mut()
            .find(|kept| kept.account == window.account && kept.week == week)
        {
            Some(kept) => {
                kept.used += used;
                kept.running = running;
                kept.window = window.clone();
                kept.windows.push(window);
            }
            None => {
                let account = window.account.clone();
                let last = window.clone();
                weeks.push(AccountWeek {
                    week,
                    account,
                    windows: vec![window],
                    window: last,
                    running,
                    used,
                    price: None,
                    price_missing: None,
                });
            }
        }
    }
    for week in &mut weeks {
        let (price, price_missing) =
            match valuer.window_price(&week.account, week.window.weekly_start(), week.window.end) {
                Ok(price) => (Some(price), None),
                Err(missing) => (None, Some(missing)),
            };
        week.price = price;
        week.price_missing = price_missing;
    }
    weeks
}

/// The ids of the accounts with at least one account week without a price:
/// stored accounts in [`Ctx::accounts`] order, then accounts no longer stored
/// by id. These are the accounts left out of a money sum; a priced week of the
/// same account still counts.
pub(crate) fn accounts_without_price<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    weeks: &[AccountWeek],
) -> Vec<String> {
    let mut ids: Vec<String> = ctx
        .accounts
        .iter()
        .filter(|account| {
            let reference = AccountRef::of(account);
            weeks
                .iter()
                .any(|week| week.account == reference && week.price.is_none())
        })
        .map(|account| account.id.clone())
        .collect();
    let mut removed: Vec<String> = weeks
        .iter()
        .filter(|week| {
            week.price.is_none()
                && Company::of_account(&week.account) == Company::Anthropic
                && stored_account(ctx, &week.account).is_none()
        })
        .map(|week| account_id(ctx, &week.account))
        .collect();
    removed.sort();
    removed.dedup();
    ids.extend(removed);
    ids
}

/// The switches of the range by who made them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchCounts {
    pub user: u32,
    pub cli: u32,
    pub auto: u32,
    pub agent: u32,
    pub external: u32,
}

impl SwitchCounts {
    /// Every kind of switch added up. `u32` is what each kind counts; a range
    /// with more than `u32::MAX` switches of one kind is not a practical case.
    pub fn total(&self) -> u32 {
        self.user + self.cli + self.auto + self.agent + self.external
    }
}

/// The `Switch` events of `range`, only of `host` when it is set, by their
/// `by`.
pub(crate) fn switch_counts(
    events: &[Hosted<Event>],
    range: &Range,
    host: Option<&str>,
) -> SwitchCounts {
    let mut counts = SwitchCounts::default();
    for hosted in events {
        let at = hosted.record.t;
        if at < range.from || at >= range.to {
            continue;
        }
        if host.is_some_and(|host| host != hosted.host) {
            continue;
        }
        let EventKind::Switch { by, .. } = &hosted.record.kind else {
            continue;
        };
        match by {
            By::User => counts.user += 1,
            By::Cli => counts.cli += 1,
            By::Auto => counts.auto += 1,
            By::Agent => counts.agent += 1,
            By::External => counts.external += 1,
        }
    }
    counts
}

/// The index of the local hour of the week that holds `t`: Monday 00:00 local
/// is 0. The one index for the heat map and for burn's rhythm.
pub(crate) fn local_hour_of_week<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> usize {
    let local = t.with_timezone(zone);
    local.weekday().num_days_from_monday() as usize * 24 + local.hour() as usize
}

/// The calendar weeks of `range`: Monday 00:00 local of the week that holds
/// `range.from`, then the following Monday 00:00 local of each day that is
/// still before `range.to`.
pub(crate) fn week_columns<Tz: TimeZone + Copy>(range: &Range, zone: &Tz) -> Vec<DateTime<Utc>> {
    let mut columns = Vec::new();
    let mut week = week_start(range.from, zone);
    while week < range.to {
        columns.push(week);
        // The next Monday 00:00 local, not 168 h later: a week may hold 167 or
        // 169 real hours.
        week = next_week_start(week, zone);
    }
    columns
}

/// The blocked hours of `spans` per calendar week: a span is split at every
/// week start it crosses and each part counts the working hours it has.
pub(crate) fn blocked_by_week<Tz: TimeZone + Copy>(
    spans: &[BlockedSpan],
    rhythm: &Rhythm,
    zone: &Tz,
) -> BTreeMap<DateTime<Utc>, f64> {
    let mut by_week = BTreeMap::new();
    for span in spans {
        let mut cursor = span.from;
        while cursor < span.to {
            let week = week_start(cursor, zone);
            let end = next_week_start(week, zone).min(span.to);
            *by_week.entry(week).or_insert(0.0) += working_hours(cursor, end, rhythm, zone);
            cursor = end;
        }
    }
    by_week
}

/// The blocked hours of `spans` per local weekday, Monday first: a span is
/// split at every local midnight it crosses and each part counts the working
/// hours it has.
pub(crate) fn blocked_by_weekday<Tz: TimeZone + Copy>(
    spans: &[BlockedSpan],
    rhythm: &Rhythm,
    zone: &Tz,
) -> [f64; 7] {
    let mut by_day = [0.0; 7];
    for span in spans {
        let mut cursor = span.from;
        while cursor < span.to {
            let local = cursor.with_timezone(zone);
            let day = local.weekday().num_days_from_monday() as usize;
            let next = next_local_day_start(cursor, zone).min(span.to);
            by_day[day] += working_hours(cursor, next, rhythm, zone);
            cursor = next;
        }
    }
    by_day
}

/// The money of `used` and `expired` percent of one week's `price`.
pub(crate) fn money_at(price: f64, used: f64, expired: f64) -> WindowMoney {
    let paid = window_money(price, 100.0).paid;
    let percent_value = paid / 100.0;
    WindowMoney {
        used: used * percent_value,
        paid,
        expired: expired * percent_value,
    }
}

/// The money of one account week from its price and its windows: `paid` is one
/// week's price, `used` is `week.used` percent of it (which may pass 100), and
/// `expired` is every window's `lost` percent added ([`week_lost_percent`]).
/// A reset window's absent `lost` means nothing expired. `None` without a price.
pub(crate) fn money_of(week: &AccountWeek) -> Result<Option<WindowMoney>, StatsError> {
    let Some(price) = week.price else {
        return Ok(None);
    };
    let expired = week_lost_percent(week)?;
    Ok(Some(money_at(price, week.used, expired)))
}

/// The `lost` percent of a weekly window. A reset window has no `lost` because
/// nothing expired then; any other window without it is an error, because
/// seatswap writes it.
pub(crate) fn lost_percent(window: &WindowRecord) -> Result<f64, StatsError> {
    match window.lost {
        Some(lost) => Ok(lost),
        None if window.reset => Ok(0.0),
        None => Err(StatsError::Message(format!(
            "the weekly window of {} ending {} has no lost percent; seatswap always writes it",
            window.account.acc, window.end
        ))),
    }
}

/// The share of one account week that expired: every window's `lost` added, so
/// a week with two ordinary windows counts both. A reset window counts nothing
/// ([`lost_percent`]), and neither does a running window (the last one of a
/// `running` week): it has expired nothing yet.
pub(crate) fn week_lost_percent(week: &AccountWeek) -> Result<f64, StatsError> {
    let mut expired = 0.0;
    for window in &week.windows[..finished_windows(week)] {
        expired += lost_percent(window)?;
    }
    Ok(expired)
}

/// How many of an account week's windows have finished: all of them, or all
/// but the last of a `running` week.
pub(crate) fn finished_windows(week: &AccountWeek) -> usize {
    if week.running {
        week.windows.len().saturating_sub(1)
    } else {
        week.windows.len()
    }
}

/// The money an account week's windows offered: one week's price for each
/// window, so a week a reset split offers two. Every share of the offered weeks
/// divides by this; a week's `paid` itself stays one week's price.
pub(crate) fn offered_money(week: &AccountWeek, money: &WindowMoney) -> f64 {
    money.paid * week.windows.len() as f64
}

/// The money an account week's finished windows offered: [`offered_money`]
/// without a running window. The Expired share divides by this: a running
/// window has expired nothing yet, and counting its offer would make the share
/// shrink while it runs.
pub(crate) fn offered_finished_money(week: &AccountWeek, money: &WindowMoney) -> f64 {
    money.paid * finished_windows(week) as f64
}

/// `part` as a percent of `offered`: the one rule of the Used and Expired
/// shares (`None` when nothing was offered).
pub(crate) fn percent_of_offered(part: f64, offered: f64) -> Option<f64> {
    (offered > 0.0).then_some(part / offered * 100.0)
}

/// The account week of `account` whose windows hold `t`: a record belongs to
/// the week when it lies in any of its windows, not only in the last one.
pub(crate) fn week_of_record<'a>(
    weeks: &'a [AccountWeek],
    account: &AccountRef,
    t: DateTime<Utc>,
) -> Option<&'a AccountWeek> {
    weeks.iter().find(|week| {
        &week.account == account
            && week
                .windows
                .iter()
                .any(|window| window.weekly_start() <= t && t < window.end)
    })
}

/// The output tokens of each account's records whose window is one of its
/// account weeks, keyed by the account and the calendar week of that window.
pub(crate) fn output_by_account_week<'a>(
    weeks: &[AccountWeek],
    records: impl IntoIterator<Item = (&'a AccountRef, &'a TokenRecord)>,
) -> BTreeMap<(AccountRef, DateTime<Utc>), u64> {
    let mut by_week: BTreeMap<(AccountRef, DateTime<Utc>), u64> = BTreeMap::new();
    for (account, record) in records {
        let Some(week) = week_of_record(weeks, account, record.t) else {
            continue;
        };
        *by_week.entry((account.clone(), week.week)).or_insert(0) += record.out;
    }
    by_week
}

/// The id a page writes for `reference`: the stored account's id, or the
/// reference's account id when it is no longer stored.
pub(crate) fn account_id<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, reference: &AccountRef) -> String {
    stored_account(ctx, reference)
        .map(|account| account.id.clone())
        .unwrap_or_else(|| reference.acc.clone())
}

/// Whether the query chooses `account`: every account when `q.accounts` is
/// empty, else only the one whose [`account_id`] is named. A record without a
/// known account is never chosen by a filter.
pub(crate) fn account_chosen<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    q: &Query,
    account: Option<&AccountRef>,
) -> bool {
    if q.accounts.is_empty() {
        return true;
    }
    let Some(account) = account else {
        return false;
    };
    let id = account_id(ctx, account);
    q.accounts.iter().any(|chosen| chosen == &id)
}

/// Whether the query chooses a valued token record on a page of project work,
/// by its [`Valued::account`] ([`account_chosen`]): a Claude record's account,
/// or a provider record's subscription (decision P-2 8). A record without a
/// known account is chosen only while no chip is taken out; a provider record
/// whose subscription is not chosen is left out like an account's.
pub(crate) fn record_chosen<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, q: &Query, value: &Valued) -> bool {
    account_chosen(ctx, q, value.account.as_ref())
}

/// The abbreviation a page writes for `reference`: the stored account's, a
/// stored provider subscription's, or the first 8 characters of the id
/// `account_id` gives it when neither is stored (the rule
/// `crates/app/ui/common.js` uses for an unknown account).
pub(crate) fn account_abbr<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, reference: &AccountRef) -> String {
    if let Some(account) = stored_account(ctx, reference) {
        return account.abbr.clone();
    }
    if let Some(subscription) = stored_subscription(ctx, reference) {
        return subscription.abbr.clone();
    }
    reference.acc.chars().take(8).collect()
}

/// Where an account id stands in the stored accounts' order, then the provider
/// subscriptions' order; an id that is no longer stored sorts after all of
/// them.
pub(crate) fn account_position<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, id: &str) -> usize {
    if let Some(position) = ctx.accounts.iter().position(|account| account.id == id) {
        return position;
    }
    ctx.provider_subscriptions
        .iter()
        .position(|subscription| subscription.account.acc == id)
        .map(|position| ctx.accounts.len() + position)
        .unwrap_or(usize::MAX)
}

/// The three largest of `items`, then `("other", rest)` only when a rest is
/// left; ties by name.
pub(crate) fn top_three(mut items: Vec<(String, f64)>) -> Vec<(String, f64)> {
    items.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if items.len() <= 3 {
        return items;
    }
    let rest = items.split_off(3);
    let other = rest.into_iter().fold(0.0, |sum, (_, value)| sum + value);
    items.push(("other".to_string(), other));
    items
}

/// Each project's share of the API value of `total`, in percent, the three
/// largest then `other` ([`top_three`]). A share of an API value counts the
/// priced work only ([`priced_api`]): it divides a project's known API part by
/// the total's known API part, so a missing price in one project leaves the
/// others' shares. A project without a priced API part is left out, as is a
/// total whose API value is unknown or 0: a share of nothing is no share.
/// `by_project` and `total` are sums of [`Values::of`].
pub(crate) fn api_shares(
    by_project: BTreeMap<String, Values>,
    total: &Values,
) -> Vec<(String, f64)> {
    match priced_api(total) {
        Some(total) if total > 0.0 => top_three(
            by_project
                .into_iter()
                .filter_map(|(name, values)| {
                    priced_api(&values).map(|value| (name, value / total * 100.0))
                })
                .collect(),
        ),
        _ => Vec::new(),
    }
}

/// Each subscription's share of the API value of `total`, in percent, largest
/// first; ties in the chips' order ([`account_position`]), an id no longer
/// stored after the stored ones. `parts` holds each chip id's API sum: a stored
/// Claude account's id ([`account_id`]), a provider subscription's
/// `account.acc`, or `None` for work without a known account (Claude) or
/// without a named subscription (provider). This is the one rule both pages
/// use: the This-week card's project rows split their bars by it and
/// Statistics → Projects' `Node::by_subscription` splits a project's API value
/// by it, so both pages draw the same split. A share of an API value counts the
/// priced work only ([`priced_api`]): it divides each part's known API value by
/// the total's known API value, so a part without a price is left out and the
/// others keep their shares. A total whose API value is unknown or 0 gives an
/// empty list.
pub(crate) fn subscription_shares<'a, Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    total: &Values,
    parts: impl IntoIterator<Item = (Option<String>, &'a Values)>,
) -> Vec<(Option<String>, f64)> {
    let Some(api) = priced_api(total) else {
        return Vec::new();
    };
    if api <= 0.0 {
        return Vec::new();
    }
    let mut shares: Vec<(Option<String>, f64)> = parts
        .into_iter()
        .filter_map(|(id, values)| priced_api(values).map(|value| (id, value / api * 100.0)))
        .collect();
    shares.sort_by(|a, b| {
        b.1.total_cmp(&a.1).then_with(|| match (&a.0, &b.0) {
            (Some(a), Some(b)) => account_position(ctx, a)
                .cmp(&account_position(ctx, b))
                .then_with(|| a.cmp(b)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
    });
    shares
}

/// Money as the page writes it (`crates/app/ui/common.js`'s `money`): two
/// decimals below 10 (`8.31 EUR`), else whole units with thousands commas
/// (`19 EUR`, `1,204 EUR`).
pub(crate) fn money_text(amount: f64, currency: &str) -> String {
    let value = (amount * 100.0).round() / 100.0;
    if value < 10.0 {
        format!("{value:.2} {currency}")
    } else {
        format!("{} {currency}", thousands(value.round() as i64))
    }
}

/// Names joined with ", " and a last " and ": `a`, `a and b`, `a, b and c`.
pub(crate) fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A whole number with a comma every three digits: `1204` → `1,204`.
pub(crate) fn thousands(whole: i64) -> String {
    let digits = whole.unsigned_abs().to_string();
    let mut text = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    if whole < 0 {
        format!("-{text}")
    } else {
        text
    }
}

/// An hour count as a duration, rounded to the whole minute: `2h`, `30min`,
/// `0.5 h` becomes `30min` (review item 42); none at all is `0h`, not `0s`, as
/// the window's `hoursText` writes it. A negative or non-finite count is the
/// callers' error, as before: it keeps the old one-decimal `h` text instead of
/// being silently turned into a duration.
pub(crate) fn hours_text(hours: f64) -> String {
    if hours.is_finite() && hours >= 0.0 {
        let minutes = (hours * 60.0).round() as u64;
        if minutes == 0 {
            return "0h".to_string();
        }
        return crate::duration::duration_text(minutes * 60);
    }
    if (hours * 10.0).round() % 10.0 == 0.0 {
        format!("{} h", hours.round() as i64)
    } else {
        format!("{hours:.1} h")
    }
}

/// A percent, whole: `27 %`.
pub(crate) fn percent_text(percent: f64) -> String {
    format!("{} %", percent.round() as i64)
}

/// A date in local time: `12 Nov`.
pub(crate) fn date_text<Tz: TimeZone + Copy>(t: DateTime<Utc>, zone: &Tz) -> String {
    let local = t.with_timezone(zone).naive_local();
    format!("{} {}", local.day(), local.format("%b"))
}

/// A stretch in which the account in use on a host was blocked, cut at every
/// change of the account in use, at the range's ends and at its `Unblocked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedSpan {
    pub host: String,
    pub account: AccountRef,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// The working hours of the span (the rhythm's hours count).
    pub hours: f64,
    /// The part of `hours` in which another auto-burn account was usable and
    /// had a finished weekly window running that ended with something expired.
    pub avoidable_hours: f64,
}

/// `None` when the range holds a reading; else what to show instead of a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Empty {
    pub recording_since: Option<DateTime<Utc>>,
}

/// The blocked time of one company in the range (decision P-2 5): the
/// company's own blocks inside that company's working hours. Claude's accounts
/// count only while the blocked account was the live one, any limit kind; their
/// avoidable hours are auto-burn's. OpenAI's `session` and `weekly_all` blocks
/// count only while the blocked subscription was the one in use on the block's
/// host (the `providerSwitch` rule of decision P-2 2); Ollama has one
/// subscription, so its `monthly_credits` block always counts, and Google's
/// readings are of the account agy was signed in to, so its `session` and
/// `weekly_all` blocks count too; no caller asks for Google's yet, because a
/// company without recorded work has no blocked hours. No sum across companies.
pub fn blocked_spans<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    company: Company,
) -> Result<Vec<BlockedSpan>, StatsError> {
    let events = keep_warnings(ctx.warnings, || {
        ctx.history.read::<Event>(&Range::ALL, &Hosts::All)
    })?;
    let block_events: Vec<Hosted<Event>> = events
        .records
        .iter()
        .filter(|hosted| match &hosted.record.kind {
            EventKind::Blocked { account, k, .. } | EventKind::Unblocked { account, k, .. } => {
                Company::of_account(account) == company && blocks_whole_account(company, k)
            }
            _ => false,
        })
        .cloned()
        .collect();
    let intervals = block_intervals(&block_events, range.to);
    let mut weekly = Vec::new();
    let windows = keep_warnings(ctx.warnings, || {
        ctx.history.finished_windows(&Range::ALL, &Hosts::All)
    })?;
    for hosted in windows.records {
        if hosted.record.k == LimitKind::WeeklyAll
            && Company::of_account(&hosted.record.account) == Company::Anthropic
        {
            weekly.push(hosted);
        }
    }
    let rhythm = working_rhythm(ctx, range, company)?;
    let claude_timelines = rates::timelines(&events.records);
    // OpenAI's blocks follow the one rule that names a record's subscription.
    let provider_timelines = crate::money::provider_switch_timelines(&events.records);
    let openai_stored = crate::money::openai_subscriptions(ctx.provider_subscriptions);

    let mut spans = Vec::new();
    let mut warned_openai_hosts: BTreeSet<String> = BTreeSet::new();
    for host in hosts_of(q, &block_events) {
        if company == Company::Anthropic && !claude_timelines.contains_key(&host) {
            continue;
        }
        for (interval_host, account, blocked_from, blocked_to) in &intervals {
            if interval_host != &host {
                continue;
            }
            if !account_chosen(ctx, q, Some(account)) {
                continue;
            }
            let start = (*blocked_from).max(range.from);
            let end = (*blocked_to).min(range.to);
            if start >= end {
                continue;
            }
            let mut points = vec![start];
            match company {
                Company::Anthropic => {
                    let Some(timeline) = claude_timelines.get(&host) else {
                        continue;
                    };
                    points.extend(timeline.changes_between(start, end));
                }
                Company::OpenAI => {
                    if let Some(timeline) = provider_timelines.get(&host) {
                        points.extend(timeline.changes_between(start, end));
                    }
                }
                Company::Ollama | Company::Google => {}
            }
            points.push(end);
            points.sort_unstable();
            points.dedup();
            for pair in points.windows(2) {
                let (from, to) = (pair[0], pair[1]);
                if from >= to {
                    continue;
                }
                let in_use = match company {
                    Company::Anthropic => claude_timelines
                        .get(&host)
                        .is_some_and(|timeline| timeline.at(from) == Some(account)),
                    Company::OpenAI => {
                        match crate::money::openai_subscription_at(
                            &provider_timelines,
                            &openai_stored,
                            &host,
                            from,
                        ) {
                            Ok(subscription) => &subscription == account,
                            Err(_) => {
                                if warned_openai_hosts.insert(host.clone()) {
                                    ctx.warnings.borrow_mut().push(no_openai_login_warning(
                                        &format!("block of {}", account_abbr(ctx, account)),
                                        &host,
                                        from,
                                    ));
                                }
                                false
                            }
                        }
                    }
                    Company::Ollama | Company::Google => true,
                };
                if !in_use {
                    continue;
                }
                spans.push(BlockedSpan {
                    host: host.clone(),
                    account: account.clone(),
                    from,
                    to,
                    hours: working_hours(from, to, &rhythm, &ctx.zone),
                    // Auto-burn's avoidable hours are Claude accounts' alone.
                    avoidable_hours: if company == Company::Anthropic {
                        avoidable_hours(
                            ctx,
                            &host,
                            account,
                            Range { from, to },
                            &intervals,
                            &weekly,
                            &rhythm,
                        )
                    } else {
                        0.0
                    },
                });
            }
        }
    }
    spans.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.account.cmp(&b.account))
    });
    Ok(spans)
}

/// Whether a limit of `company` blocks the whole account when it is at 100 %
/// (decision P-2 5): every kind for Claude (today's rule), the 5 h session and
/// the week of all models for OpenAI and Google, Ollama's credits month.
fn blocks_whole_account(company: Company, kind: &LimitKind) -> bool {
    match company {
        Company::Anthropic => true,
        Company::OpenAI | Company::Google => {
            matches!(kind, LimitKind::Session | LimitKind::WeeklyAll)
        }
        Company::Ollama => matches!(
            kind,
            LimitKind::Other(name) if name == "monthly_credits"
        ),
    }
}

/// Whether the history holds no record in the range; then the page says since
/// when seatswap has been recording.
pub fn empty<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
) -> Result<Option<Empty>, StatsError> {
    if has_record(ctx, range)? {
        return Ok(None);
    }
    Ok(Some(Empty {
        recording_since: first_record(ctx)?,
    }))
}

fn earliest<R: Record>(
    warnings: &RefCell<Vec<String>>,
    history: &HistoryDir,
    range: &Range,
) -> Result<Option<DateTime<Utc>>, HistoryError> {
    let read = keep_warnings(warnings, || history.read_claude::<R>(range, &Hosts::All))?;
    Ok(read.records.first().map(|hosted| hosted.record.at()))
}

fn first_record<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>) -> Result<Option<DateTime<Utc>>, StatsError> {
    let range = Range::ALL;
    let found = [
        earliest::<Sample>(ctx.warnings, ctx.history, &range)?,
        earliest::<WindowRecord>(ctx.warnings, ctx.history, &range)?,
        earliest::<Event>(ctx.warnings, ctx.history, &range)?,
        earliest::<TokenRecord>(ctx.warnings, ctx.history, &range)?,
        earliest::<SessionRecord>(ctx.warnings, ctx.history, &range)?,
        earliest::<UpRecord>(ctx.warnings, ctx.history, &range)?,
        earliest::<ProjectsLogRecord>(ctx.warnings, ctx.history, &range)?,
    ];
    Ok(found.into_iter().flatten().min())
}

fn any_record<R: Record>(
    warnings: &RefCell<Vec<String>>,
    history: &HistoryDir,
    range: &Range,
) -> Result<bool, HistoryError> {
    Ok(
        !keep_warnings(warnings, || history.read_claude::<R>(range, &Hosts::All))?
            .records
            .is_empty(),
    )
}

/// The kinds with the fewest lines are asked first: a range with records
/// mostly has a window or an event, and then the many samples are not read.
fn has_record<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, range: &Range) -> Result<bool, StatsError> {
    Ok(
        any_record::<WindowRecord>(ctx.warnings, ctx.history, range)?
            || any_record::<Event>(ctx.warnings, ctx.history, range)?
            || any_record::<ProjectsLogRecord>(ctx.warnings, ctx.history, range)?
            || any_record::<TokenRecord>(ctx.warnings, ctx.history, range)?
            || any_record::<SessionRecord>(ctx.warnings, ctx.history, range)?
            || any_record::<UpRecord>(ctx.warnings, ctx.history, range)?
            || any_record::<Sample>(ctx.warnings, ctx.history, range)?,
    )
}

pub(crate) fn stored_account<'a, Tz: TimeZone + Copy>(
    ctx: &Ctx<'a, Tz>,
    reference: &AccountRef,
) -> Option<&'a Account> {
    ctx.accounts.iter().find(|account| {
        account.account_uuid == reference.acc && account.organization_uuid == reference.org
    })
}

/// The stored provider subscription `reference` names, if any (decision P-2 1):
/// its look is the one a page writes for the subscription's own work.
pub(crate) fn stored_subscription<'a, Tz: TimeZone + Copy>(
    ctx: &Ctx<'a, Tz>,
    reference: &AccountRef,
) -> Option<&'a ProviderSubscription> {
    ctx.provider_subscriptions
        .iter()
        .find(|subscription| &subscription.account == reference)
}

fn hosts_of(q: &Query, events: &[Hosted<Event>]) -> Vec<String> {
    match &q.host {
        Some(host) => vec![host.clone()],
        None => {
            let mut hosts: Vec<String> = events.iter().map(|hosted| hosted.host.clone()).collect();
            hosts.sort();
            hosts.dedup();
            hosts
        }
    }
}

/// The working hours of one company in the range (decision P-2 5): the hours of
/// the week whose value in the rhythm of the four weeks before `range.to` is
/// above 0, learned from the token records of that company's models alone. No
/// sum across companies: a company's blocked time counts in its own working
/// hours only.
pub(crate) fn working_rhythm<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    company: Company,
) -> Result<Rhythm, StatsError> {
    let from = range.to - Duration::weeks(RHYTHM_WEEKS);
    let read = keep_warnings(ctx.warnings, || {
        ctx.history
            .read::<TokenRecord>(&Range { from, to: range.to }, &Hosts::All)
    })?;
    Ok(rates::rhythm_of_company(
        &read.records,
        range.to,
        &ctx.zone,
        ctx.prices,
        company,
    ))
}

/// The hours of `[from, to)` that fall in a working hour.
fn working_hours<Tz: TimeZone + Copy>(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    rhythm: &Rhythm,
    zone: &Tz,
) -> f64 {
    let mut hours = 0.0;
    let mut cursor = from;
    while cursor < to {
        let next = (local_hour_start(cursor, zone) + Duration::hours(1)).min(to);
        if rhythm.usd[local_hour_of_week(cursor, zone)] > 0.0 {
            hours += seconds_between(cursor, next) / 3600.0;
        }
        cursor = next;
    }
    hours
}

fn seconds_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    let span = to.signed_duration_since(from);
    span.num_seconds() as f64 + span.subsec_nanos() as f64 / 1e9
}

type BlockedInterval = (String, AccountRef, DateTime<Utc>, DateTime<Utc>);
/// A block belongs to one host and one account.
type BlockKey = (String, AccountRef);
/// The limits a block holds open, by API name and model.
type OpenLimits = BTreeSet<(String, Option<String>)>;

/// The stretches in which an account was blocked on one host, any limit kind. A
/// block that is still open at the last event counts up to `open_to` (the
/// range's end).
fn block_intervals(events: &[Hosted<Event>], open_to: DateTime<Utc>) -> Vec<BlockedInterval> {
    let mut active: BTreeMap<BlockKey, OpenLimits> = BTreeMap::new();
    let mut opened: BTreeMap<BlockKey, DateTime<Utc>> = BTreeMap::new();
    let mut intervals = Vec::new();
    for hosted in events {
        match &hosted.record.kind {
            EventKind::Blocked { account, k, m } => {
                let key = (hosted.host.clone(), account.clone());
                let blocked = active.entry(key.clone()).or_default();
                let was_empty = blocked.is_empty();
                blocked.insert((k.as_api().to_string(), m.clone()));
                if was_empty {
                    opened.insert(key, hosted.record.t);
                }
            }
            EventKind::Unblocked { account, k, m } => {
                let key = (hosted.host.clone(), account.clone());
                let Some(blocked) = active.get_mut(&key) else {
                    continue;
                };
                blocked.remove(&(k.as_api().to_string(), m.clone()));
                if blocked.is_empty() {
                    active.remove(&key);
                    if let Some(start) = opened.remove(&key) {
                        if start < hosted.record.t {
                            intervals.push((key.0, key.1, start, hosted.record.t));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (key, blocked) in active {
        if blocked.is_empty() {
            continue;
        }
        if let Some(start) = opened.remove(&key) {
            if start < open_to {
                intervals.push((key.0, key.1, start, open_to));
            }
        }
    }
    intervals.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    intervals
}

fn blocked_at(
    intervals: &[BlockedInterval],
    host: &str,
    account: &AccountRef,
    t: DateTime<Utc>,
) -> bool {
    intervals.iter().any(|(blocked_host, blocked, from, to)| {
        blocked_host == host && blocked == account && *from <= t && t < *to
    })
}

/// The working hours of `[from, to)` in which another account auto-burn may use
/// was not blocked on `host` and had a finished weekly window running that ended
/// with something expired.
fn avoidable_hours<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    host: &str,
    blocked: &AccountRef,
    span: Range,
    intervals: &[BlockedInterval],
    weekly: &[Hosted<WindowRecord>],
    rhythm: &Rhythm,
) -> f64 {
    let (from, to) = (span.from, span.to);
    let has_other = ctx
        .accounts
        .iter()
        .any(|account| account.auto_burn && AccountRef::of(account) != *blocked);
    if !has_other {
        return 0.0;
    }

    let mut points = vec![from, to];
    for (blocked_host, account, start, end) in intervals {
        if blocked_host != host || account == blocked {
            continue;
        }
        for at in [*start, *end] {
            if at > from && at < to {
                points.push(at);
            }
        }
    }
    for hosted in weekly {
        if hosted.record.account == *blocked {
            continue;
        }
        if let Some(start) = hosted.record.start {
            for at in [start, hosted.record.end] {
                if at > from && at < to {
                    points.push(at);
                }
            }
        }
    }
    points.sort_unstable();
    points.dedup();

    let mut hours = 0.0;
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        if avoidable_at(ctx, host, blocked, start, intervals, weekly) {
            hours += working_hours(start, end, rhythm, &ctx.zone);
        }
    }
    hours
}

fn avoidable_at<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    host: &str,
    blocked: &AccountRef,
    t: DateTime<Utc>,
    intervals: &[BlockedInterval],
    weekly: &[Hosted<WindowRecord>],
) -> bool {
    ctx.accounts
        .iter()
        .filter(|account| account.auto_burn)
        .any(|account| {
            let other = AccountRef::of(account);
            other != *blocked
                && !blocked_at(intervals, host, &other, t)
                && weekly.iter().any(|hosted| {
                    hosted.record.account == other
                        && hosted
                            .record
                            .start
                            .is_some_and(|start| start <= t && t < hosted.record.end)
                        && 100.0 - hosted.record.final_percent > 0.0
                })
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::fs;

    use chrono::{NaiveDate, NaiveTime};
    use tempfile::tempdir;

    use crate::project::ProjectKey;

    pub(crate) use crate::stats::fixture::Tiny;

    pub(crate) fn utc(text: &str) -> DateTime<Utc> {
        text.parse::<DateTime<Utc>>().expect("test date")
    }

    /// A stored provider subscription with `abbr` and an optional monthly price,
    /// as `accounts.json` holds it (decision P-2 1).
    pub(crate) fn provider_subscription(
        account: &AccountRef,
        abbr: &str,
        price: Option<f64>,
    ) -> crate::store::ProviderSubscription {
        crate::store::ProviderSubscription {
            account: account.clone(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#4a6f8c".to_string(),
            plan: Some("Plus".to_string()),
            price,
            price_currency: price.map(|_| "EUR".to_string()),
        }
    }

    /// A Codex sample, window and event beside the Claude records. A reader
    /// that serves every company now sees them (P-2); a Claude-only reader must
    /// give the same answer with and without them (step-d.md Task 4).
    pub(crate) fn append_codex_records(history: &HistoryDir, at: DateTime<Utc>) {
        let account = codex_ref();
        history
            .append(&Sample {
                t: at,
                account: account.clone(),
                src: crate::history::Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 12.0,
                    r: Some(at + Duration::days(7)),
                    b: false,
                }],
            })
            .expect("the Codex sample");
        history
            .append(&WindowRecord {
                account: account.clone(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(at - Duration::days(7)),
                end: at,
                final_percent: 12.0,
                // A Codex week record carries the unused share, as `window_of`
                // writes it; a month has none.
                lost: Some(88.0),
                last_sample: at,
                plan: Some("Plus".to_string()),
                uncertain: false,
                reset: false,
                from: None,
            })
            .expect("the Codex window");
        history
            .append(&Event {
                t: at,
                kind: EventKind::Blocked {
                    account,
                    k: LimitKind::WeeklyAll,
                    m: None,
                },
            })
            .expect("the Codex event");
    }

    fn winter() -> FixedOffset {
        FixedOffset::east_opt(3600).expect("+01:00")
    }

    fn summer() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).expect("+02:00")
    }

    /// The offset either side of one change, and the local times that the
    /// change makes nonexistent or ambiguous.
    fn changing_offset(
        local: &NaiveDateTime,
        change: NaiveDateTime,
        before: FixedOffset,
        after: FixedOffset,
    ) -> MappedLocalTime<FixedOffset> {
        let offset_at = |utc: &NaiveDateTime| if *utc < change { before } else { after };
        let mut found: Vec<(NaiveDateTime, FixedOffset)> = [before, after]
            .into_iter()
            .filter_map(|offset| {
                let utc = *local - Duration::seconds(i64::from(offset.local_minus_utc()));
                (offset_at(&utc) == offset).then_some((utc, offset))
            })
            .collect();
        found.sort_by_key(|(utc, _)| *utc);
        match found.as_slice() {
            [] => MappedLocalTime::None,
            [(_, offset)] => MappedLocalTime::Single(*offset),
            // The later instant first, as chrono's `Local` answers on Linux
            // (see `fold`): the order the code must not depend on.
            [(_, earlier), (_, later)] => MappedLocalTime::Ambiguous(*later, *earlier),
            _ => MappedLocalTime::None,
        }
    }

    /// Central Europe's change on the last Sunday of March: +01:00 before
    /// 2026-03-29T01:00:00Z and +02:00 from then on. A local time in the gap
    /// has no offset and one in the fold has two, through
    /// [`TimeZone::offset_from_local_datetime`].
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct SummerZone;

    fn summer_change() -> NaiveDateTime {
        utc("2026-03-29T01:00:00Z").naive_utc()
    }

    impl TimeZone for SummerZone {
        type Offset = FixedOffset;

        fn from_offset(_offset: &FixedOffset) -> Self {
            SummerZone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            changing_offset(local, summer_change(), winter(), summer())
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if *utc < summer_change() {
                winter()
            } else {
                summer()
            }
        }
    }

    /// Central Europe's change on the last Sunday of October: +02:00 before
    /// 2026-10-25T01:00:00Z and +01:00 from then on. The week and the day that
    /// hold it are 169 and 25 hours long.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct FallZone;

    fn fall_change() -> NaiveDateTime {
        utc("2026-10-25T01:00:00Z").naive_utc()
    }

    impl TimeZone for FallZone {
        type Offset = FixedOffset;

        fn from_offset(_offset: &FixedOffset) -> Self {
            FallZone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            changing_offset(local, fall_change(), summer(), winter())
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if *utc < fall_change() {
                summer()
            } else {
                winter()
            }
        }
    }

    pub(crate) fn all() -> Query {
        Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: None,
        }
    }

    fn two_weeks() -> Range {
        Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.001
    }

    /// The fixture every test of this plan uses: host `atlas`, local time UTC,
    /// "now" = Thu 19 Nov 2026 10:00 (step-c3.md, "The fixture every test of
    /// this plan uses").
    pub(crate) fn tiny() -> Tiny {
        let dir = tempdir().expect("a temp history");
        let mut tiny = crate::stats::fixture::write(dir.path()).expect("the tiny fixture");
        tiny.dir.keep(Box::new(dir));
        tiny
    }

    /// The fixture's context with another zone; the tests use it to put a
    /// daylight-saving change under a page.
    pub(crate) fn ctx_with<Tz: TimeZone + Copy>(tiny: &Tiny, zone: Tz) -> Ctx<'_, Tz> {
        Ctx {
            history: &tiny.history,
            accounts: &tiny.accounts,
            provider_subscriptions: &tiny.provider_subscriptions,
            settings: &tiny.settings,
            prices: &tiny.prices,
            fx: &tiny.fx,
            projects: &tiny.projects,
            warnings: &tiny.warnings,
            now: tiny.now,
            zone,
        }
    }

    /// A statistics page keeps a torn line's warning in the context's warnings
    /// instead of dropping it (spec §1.2).
    #[test]
    fn a_statistics_page_keeps_a_torn_line_warning() {
        use std::io::Write;

        let tiny = tiny();
        let events = tiny
            .history
            .root()
            .join("atlas")
            .join("events-2026-11.jsonl");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&events)
            .expect("the events file");
        file.write_all(b"{\"t\":").expect("the torn last line");
        drop(file);

        let ctx = tiny.ctx();
        overview::overview(&ctx, &all()).expect("the overview");

        let warnings = tiny.warnings.borrow();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("events-2026-11.jsonl line")),
            "the torn line's warning is in Ctx: {warnings:?}"
        );
    }

    #[test]
    fn range_keys() {
        let tiny = tiny();
        let ctx = tiny.ctx();

        let this_week = range_of(RangeKey::ThisWeek, &ctx).unwrap();
        assert_eq!(this_week.from, utc("2026-11-16T00:00:00Z"));
        assert_eq!(this_week.to, tiny.now);

        let last_week = range_of(RangeKey::LastWeek, &ctx).unwrap();
        assert_eq!(last_week.from, utc("2026-11-09T00:00:00Z"));
        assert_eq!(last_week.to, utc("2026-11-16T00:00:00Z"));

        let days30 = range_of(RangeKey::Days30, &ctx).unwrap();
        assert_eq!(days30.from, utc("2026-10-19T00:00:00Z"));
        assert_eq!(days30.to, tiny.now);
    }

    #[test]
    fn a_windows_price_is_the_price_at_its_midpoint() {
        let tiny = tiny();
        tiny.history
            .append_many(&[
                Event {
                    t: utc("2026-11-06T00:00:00Z"),
                    kind: EventKind::PriceChanged {
                        account: tiny.lb_ref(),
                        amount: Some(180.0),
                        currency: Some("EUR".to_string()),
                    },
                },
                Event {
                    t: utc("2026-11-06T10:00:00Z"),
                    kind: EventKind::PriceChanged {
                        account: tiny.lb_ref(),
                        amount: Some(200.0),
                        currency: Some("EUR".to_string()),
                    },
                },
            ])
            .expect("LB's price changes");

        let weeks = account_weeks(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        let price_of = |end: &str| {
            weeks
                .iter()
                .find(|week| week.account == tiny.lb_ref() && week.window.end == utc(end))
                .expect("LB's window")
                .price
        };
        assert_eq!(price_of("2026-11-09T17:00:00Z"), Some(180.0));
        assert_eq!(price_of("2026-11-16T17:00:00Z"), Some(200.0));
    }

    /// A price change inside a week does not change the price of that week's
    /// money: the window is priced at its midpoint, and its records share its
    /// used money. LB's 80 % week of 200 EUR is worth 80 × pv(200) both on the
    /// Overview and in the sum of its records' subscription values.
    #[test]
    fn a_price_change_inside_a_week_values_the_projects_as_the_week() {
        let tiny = tiny();
        tiny.history
            .append(&Event {
                t: utc("2026-11-12T00:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: tiny.lb_ref(),
                    amount: Some(200.0),
                    currency: Some("EUR".to_string()),
                },
            })
            .expect("LB's price change");

        let page = overview::overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        let week = page
            .weeks
            .iter()
            .find(|week| week.week == utc("2026-11-09T00:00:00Z"))
            .expect("the week of 9 Nov");
        let used = week
            .accounts
            .iter()
            .find(|account| account.id == tiny.lb().id)
            .expect("LB's week")
            .used
            .expect("LB's week has a price");

        let window = tiny
            .windows
            .iter()
            .find(|window| {
                window.account == tiny.lb_ref() && window.end == utc("2026-11-16T17:00:00Z")
            })
            .expect("LB's window ending 16 Nov");
        let valuer = Valuer::build(
            &tiny.history,
            &two_weeks(),
            &tiny.prices,
            &tiny.fx,
            &tiny.settings,
            &tiny.accounts,
            &[],
            &tiny.warnings,
        )
        .expect("a valuer");
        let tokens = tiny
            .history
            .read::<TokenRecord>(&two_weeks(), &Hosts::All)
            .unwrap();
        let mut sum = 0.0;
        let mut count = 0;
        for hosted in &tokens.records {
            if hosted.record.t < window.weekly_start() || hosted.record.t >= window.end {
                continue;
            }
            let value = valuer.value(&hosted.host, &hosted.record);
            if value.account.as_ref() != Some(&tiny.lb_ref()) {
                continue;
            }
            sum += value
                .subscription
                .expect("LB's priced records of the week have a value");
            count += 1;
        }
        assert_eq!(count, 1, "the fixture has one LB record in this window");
        assert!(
            (sum - used).abs() < 1e-9,
            "LB's records are worth {sum}, its week's used money is {used}"
        );
    }

    #[test]
    fn a_price_in_another_currency_is_missing() {
        let mut tiny = tiny();
        tiny.accounts[0].price_currency = Some("USD".to_string());

        let weeks = account_weeks(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        let lb = weeks
            .iter()
            .find(|week| {
                week.account == tiny.lb_ref() && week.window.end == utc("2026-11-09T17:00:00Z")
            })
            .expect("LB's window ending 9 Nov");
        assert_eq!(lb.price, None);
        assert_eq!(
            lb.price_missing,
            Some(Missing::PriceInOtherCurrency {
                currency: "USD".to_string()
            })
        );
    }

    /// Two ordinary windows in one calendar week (a model reset kept the reset
    /// time, or the reset moved) count both windows' expired share: 20 % and
    /// 10 % make 30 % of the week's price, not only the last window's 10 %.
    #[test]
    fn two_ordinary_windows_count_both_lost_percents() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let window = |end: &str, final_percent: f64, lost: f64| WindowRecord {
            account: tiny.lb_ref(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc(end) - Duration::days(7)),
            end: utc(end),
            final_percent,
            lost: Some(lost),
            last_sample: utc(end) - Duration::minutes(2),
            plan: Some("Max".to_string()),
            uncertain: false,
            reset: false,
            from: None,
        };
        history
            .append_many(&[
                window("2026-11-06T12:00:00Z", 80.0, 20.0),
                window("2026-11-09T12:00:00Z", 90.0, 10.0),
            ])
            .expect("the two ordinary windows");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };
        let weeks = account_weeks(&ctx, &two_weeks(), &all()).unwrap();
        assert_eq!(weeks.len(), 1, "both windows' midpoints fall in one week");
        let week = &weeks[0];
        assert_eq!(week.windows.len(), 2, "both ordinary windows are kept");
        assert_eq!(week_lost_percent(week).expect("both lost percents"), 30.0);
        let money = money_of(week)
            .expect("both lost percents are written")
            .expect("LB's price is known");
        assert!(
            close(money.expired, 0.30 * money.paid),
            "expired money is both windows' lost: {} of {}",
            money.expired,
            money.paid
        );
    }

    #[test]
    fn windows_go_to_the_week_of_their_midpoint() {
        let tiny = tiny();
        let ctx = tiny.ctx();

        let weeks = account_weeks(&ctx, &two_weeks(), &all()).unwrap();
        assert_eq!(weeks.len(), 4);
        let first = weeks
            .iter()
            .find(|week| {
                week.account == tiny.lb_ref() && week.window.end == utc("2026-11-09T17:00:00Z")
            })
            .expect("LB's window ending 9 Nov");
        assert_eq!(first.week, utc("2026-11-02T00:00:00Z"));
    }

    /// The running windows join the finished ones only through
    /// `account_weeks_so_far`: placed by their midpoint (LB's, Fri 20 Nov 05:00,
    /// still ahead of now, counts in the week that holds now), `running`, with
    /// their newest percent as the use so far and nothing expired.
    #[test]
    fn running_windows_are_weeks_so_far() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let range = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: tiny.now,
        };
        let valuer = Valuer::build(
            ctx.history,
            &range,
            ctx.prices,
            ctx.fx,
            ctx.settings,
            ctx.accounts,
            &[],
            ctx.warnings,
        )
        .expect("the valuer");

        let finished = account_weeks_valued(&ctx, &range, &all(), &valuer).unwrap();
        assert!(finished.iter().all(|week| !week.running));
        let so_far = account_weeks_so_far(&ctx, &range, &all(), &valuer).unwrap();
        assert_eq!(so_far.len(), finished.len() + 2);
        for (reference, percent) in [(tiny.lb_ref(), 30.0), (tiny.ac_ref(), 40.0)] {
            let week = so_far
                .iter()
                .find(|week| week.account == reference && week.running)
                .expect("the running week");
            assert_eq!(week.week, utc("2026-11-16T00:00:00Z"));
            assert_eq!(week.used, percent);
            assert_eq!(week_lost_percent(week).unwrap(), 0.0);
            assert!(week.price.is_some(), "a running week is priced");
        }
    }

    /// A week a reset split, whose second window still runs: the finished
    /// window's `lost` is the week's expired share, over the one week's price
    /// the finished window offered; the running window offers its week to the
    /// Used share only.
    #[test]
    fn a_running_window_offers_to_the_used_share_only() {
        let tiny = tiny();
        let finished = WindowRecord {
            account: tiny.lb_ref(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc("2026-11-09T17:00:00Z")),
            end: utc("2026-11-17T09:00:00Z"),
            final_percent: 80.0,
            lost: Some(20.0),
            last_sample: utc("2026-11-17T08:58:00Z"),
            plan: Some("Max".to_string()),
            uncertain: false,
            reset: false,
            from: None,
        };
        let running = WindowRecord {
            start: Some(utc("2026-11-16T17:00:00Z")),
            end: utc("2026-11-23T17:00:00Z"),
            final_percent: 30.0,
            lost: None,
            last_sample: utc("2026-11-19T09:59:00Z"),
            from: None,
            ..finished.clone()
        };
        let week = AccountWeek {
            week: utc("2026-11-16T00:00:00Z"),
            account: tiny.lb_ref(),
            windows: vec![finished, running.clone()],
            window: running,
            running: true,
            used: 110.0,
            price: Some(180.0),
            price_missing: None,
        };

        let money = money_of(&week).unwrap().expect("a priced week");
        assert_eq!(week_lost_percent(&week).unwrap(), 20.0);
        assert!(close(offered_money(&week, &money), 2.0 * money.paid));
        assert!(close(offered_finished_money(&week, &money), money.paid));
        assert!(close(money.expired, 0.20 * money.paid));
    }

    #[test]
    fn a_week_with_a_reset_sums_its_windows_use() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let first = WindowRecord {
            account: tiny.lb_ref(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc("2026-11-02T00:00:00Z")),
            end: utc("2026-11-05T12:00:00Z"),
            final_percent: 62.0,
            lost: None,
            last_sample: utc("2026-11-05T11:58:00Z"),
            plan: Some("Max".to_string()),
            uncertain: false,
            reset: true,
            from: None,
        };
        let second = WindowRecord {
            account: tiny.lb_ref(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc("2026-11-05T12:00:00Z")),
            end: utc("2026-11-09T12:00:00Z"),
            final_percent: 30.0,
            lost: Some(70.0),
            last_sample: utc("2026-11-09T11:58:00Z"),
            plan: Some("Max".to_string()),
            uncertain: false,
            reset: false,
            from: Some(0.0),
        };
        history
            .append_many(&[first.clone(), second.clone()])
            .expect("the two windows");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let weeks = account_weeks(&ctx, &two_weeks(), &all()).unwrap();
        assert_eq!(weeks.len(), 1, "the two windows are one calendar week");
        let week = &weeks[0];
        assert_eq!(week.windows, vec![first, second.clone()], "oldest first");
        assert_eq!(
            week.window, second,
            "the last window's end and lost are shown"
        );
        assert_eq!(week.used, 92.0, "62 % then 30 % from 0");
        let money = money_of(week)
            .expect("the week's lost percent is written")
            .expect("LB's price is known");
        assert!(
            close(money.paid, 180.0 * 12.0 / 52.0),
            "one week's price, not two: {}",
            money.paid
        );
        assert!(
            close(money.expired, 0.70 * money.paid),
            "the expired part is every window's lost (the second's 70): {}",
            money.expired
        );
        assert!(
            close(money.used, 0.92 * money.paid),
            "used money counts both windows: {}",
            money.used
        );
    }

    /// Review of T-1: a record in the first window of a week with a reset
    /// belongs to that week; the week's `used` (92 %) counts that window.
    #[test]
    fn proof_a_token_in_the_first_window_of_a_reset_week_is_counted() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let window = |start: &str, end: &str, final_percent: f64, lost, reset, from| WindowRecord {
            account: tiny.lb_ref(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc(start)),
            end: utc(end),
            final_percent,
            lost,
            last_sample: utc(end) - Duration::minutes(2),
            plan: Some("Max".to_string()),
            uncertain: false,
            reset,
            from,
        };
        history
            .append_many(&[
                window(
                    "2026-11-02T00:00:00Z",
                    "2026-11-05T12:00:00Z",
                    62.0,
                    None,
                    true,
                    None,
                ),
                window(
                    "2026-11-05T12:00:00Z",
                    "2026-11-09T12:00:00Z",
                    30.0,
                    Some(70.0),
                    false,
                    Some(0.0),
                ),
            ])
            .expect("the two windows");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };
        let weeks = account_weeks(&ctx, &two_weeks(), &all()).unwrap();
        assert_eq!(weeks.len(), 1);
        assert_eq!(weeks[0].used, 92.0);
        let record = TokenRecord {
            t: utc("2026-11-03T10:00:00Z"),
            minutes: 60,
            session: "s0".to_string(),
            cwd: "/home/sam/ledger".to_string(),
            project: ProjectKey::Folder("/home/sam/ledger".to_string()),
            model: "claude-opus-5-5".to_string(),
            input: 0,
            out: 1_000_000,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        };
        let account = tiny.lb_ref();
        let by_week = output_by_account_week(&weeks, [(&account, &record)]);
        assert_eq!(
            by_week.get(&(account.clone(), weeks[0].week)),
            Some(&1_000_000),
            "the record lies in the first window (2 Nov to 5 Nov 12:00) of the week, \
             which `used` counts, but `week_of_record` looks only at the last window"
        );
    }

    #[test]
    fn a_window_is_placed_by_its_own_midpoint() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&WindowRecord {
                account: tiny.lb_ref(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-02T00:00:00Z")),
                end: utc("2026-11-05T00:00:00Z"),
                final_percent: 60.0,
                lost: Some(40.0),
                last_sample: utc("2026-11-04T23:58:00Z"),
                plan: Some("Max".to_string()),
                uncertain: false,
                reset: false,
                from: None,
            })
            .expect("the window");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };
        let range = Range {
            from: utc("2026-10-26T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        };

        let weeks = account_weeks(&ctx, &range, &all()).unwrap();
        assert_eq!(weeks.len(), 1);
        assert_eq!(
            weeks[0].week,
            utc("2026-11-02T00:00:00Z"),
            "the midpoint (3 Nov) is in the week of 2 Nov, not the week of end - 84h (1 Nov)"
        );
        assert_eq!(weeks[0].window.end, utc("2026-11-05T00:00:00Z"));
    }

    #[test]
    fn history_start_is_the_first_reading_or_switch() {
        let tiny = tiny();
        assert_eq!(
            frame(&tiny.history).expect("the frame").history_start,
            Some(utc("2026-11-07T12:00:00Z")),
            "the fixture's first switch is earlier than its first sample"
        );

        tiny.history
            .append(&TokenRecord {
                t: utc("2026-10-01T09:00:00Z"),
                minutes: 60,
                session: "s0".to_string(),
                cwd: "/home/sam/ledger".to_string(),
                project: ProjectKey::Folder("/home/sam/ledger".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 0,
                out: 1_000_000,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            })
            .expect("an old token record");
        tiny.history
            .append(&SessionRecord {
                t: utc("2026-10-02T09:00:00Z"),
                session: "s0".to_string(),
                cwd: "/home/sam/ledger".to_string(),
                project: ProjectKey::Folder("/home/sam/ledger".to_string()),
                entrypoint: Some("cli".to_string()),
                title: None,
                first: utc("2026-10-02T08:00:00Z"),
                last: utc("2026-10-02T09:00:00Z"),
                agents_started: 0,
                agents_finished: 0,
                closed: true,
            })
            .expect("an old session record");
        let frame_with_old_records = frame(&tiny.history).expect("the frame");
        assert_eq!(
            frame_with_old_records.history_start,
            Some(utc("2026-11-07T12:00:00Z")),
            "token and session records do not set the history start"
        );

        tiny.history
            .append(&Sample {
                t: utc("2026-11-04T09:00:00Z"),
                account: tiny.lb_ref(),
                src: crate::history::Source::Poll,
                l: Vec::new(),
            })
            .expect("an earlier usage reading");
        let frame_with_reading = frame(&tiny.history).expect("the frame");
        assert_eq!(
            frame_with_reading.history_start,
            Some(utc("2026-11-04T09:00:00Z")),
            "an earlier sample sets the start"
        );

        let expected_bytes: u64 = fs::read_dir(tiny.dir.path().join("atlas"))
            .expect("LB's host folder")
            .map(|entry| {
                entry
                    .expect("a history file")
                    .metadata()
                    .expect("the file's size")
                    .len()
            })
            .sum();
        assert_eq!(frame_with_reading.history_bytes, expected_bytes);
        assert_eq!(frame_with_reading.hosts, vec!["atlas".to_string()]);
    }

    #[test]
    fn a_window_recorded_by_two_hosts_counts_once() {
        let tiny = tiny();
        let mut older = tiny
            .windows
            .iter()
            .find(|window| {
                window.account == tiny.lb_ref() && window.end == utc("2026-11-09T17:00:00Z")
            })
            .expect("LB's window ending 9 Nov")
            .clone();
        older.last_sample = utc("2026-11-09T16:00:00Z");
        older.final_percent = 77.0;
        let vega = HistoryDir::at(tiny.dir.path(), "vega");
        vega.append(&older).expect("the vega copy");
        fs::write(
            tiny.dir.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .expect("the vega marker");

        let weeks = account_weeks(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        assert_eq!(weeks.len(), 4);
        let first = weeks
            .iter()
            .find(|week| {
                week.account == tiny.lb_ref() && week.window.end == utc("2026-11-09T17:00:00Z")
            })
            .expect("LB's window ending 9 Nov");
        assert_eq!(first.window.final_percent, 100.0);
        assert_eq!(first.window.last_sample, utc("2026-11-09T16:58:00Z"));
    }

    #[test]
    fn blocked_time_counts_only_working_hours_of_the_active_account() {
        let tiny = tiny();
        let spans = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::Anthropic).unwrap();
        assert_eq!(spans.len(), 2);

        let lb = spans
            .iter()
            .find(|span| span.account == tiny.lb_ref())
            .expect("LB's blocked span");
        assert_eq!(lb.from, utc("2026-11-07T10:00:00Z"));
        assert_eq!(lb.to, utc("2026-11-07T12:00:00Z"));
        assert!(close(lb.hours, 2.0), "hours {}", lb.hours);
        assert!(
            close(lb.avoidable_hours, 2.0),
            "avoidable {}",
            lb.avoidable_hours
        );

        let ac = spans
            .iter()
            .find(|span| span.account == tiny.ac_ref())
            .expect("AC's blocked span");
        assert_eq!(ac.from, utc("2026-11-13T02:00:00Z"));
        assert_eq!(ac.to, utc("2026-11-13T03:00:00Z"));
        assert!(close(ac.hours, 0.0), "hours {}", ac.hours);
    }

    #[test]
    fn blocked_while_not_the_active_account_does_not_count() {
        let tiny = tiny();
        let spans = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::Anthropic).unwrap();
        let lb: Vec<&BlockedSpan> = spans
            .iter()
            .filter(|span| span.account == tiny.lb_ref())
            .collect();
        assert_eq!(lb.len(), 1);
        assert_eq!(lb[0].to, utc("2026-11-07T12:00:00Z"));
        assert!(close(lb[0].hours, 2.0), "hours {}", lb[0].hours);
    }

    #[test]
    fn an_empty_range_says_since_when() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let october = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: utc("2026-11-01T00:00:00Z"),
        };
        assert_eq!(
            empty(&ctx, &october).unwrap(),
            Some(Empty {
                recording_since: Some(utc("2026-11-02T08:00:00Z")),
            })
        );

        let dir = tempdir().expect("an empty history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let empty_ctx = Ctx {
            history: &history,
            ..ctx
        };
        assert_eq!(
            empty(&empty_ctx, &october).unwrap(),
            Some(Empty {
                recording_since: None,
            })
        );
    }

    #[test]
    fn all_on_a_new_history_is_this_week() {
        let tiny = tiny();
        let dir = tempdir().expect("an empty history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let range = range_of(RangeKey::All, &ctx).unwrap();
        assert_eq!(range.from, utc("2026-11-16T00:00:00Z"));
        assert_eq!(range.to, tiny.now);
        assert_eq!(
            empty(&ctx, &range).unwrap(),
            Some(Empty {
                recording_since: None,
            })
        );
    }

    #[test]
    fn the_account_filter_applies() {
        let tiny = tiny();
        let query = Query {
            range: RangeKey::All,
            accounts: vec![tiny.lb().id.clone()],
            host: None,
        };
        let weeks = account_weeks(&tiny.ctx(), &two_weeks(), &query).unwrap();
        assert_eq!(weeks.len(), 2);
        assert!(weeks.iter().all(|week| week.account == tiny.lb_ref()));
    }

    /// An account no longer in the store is chosen by the id `account_id`
    /// gives it: its account uuid, on every page.
    #[test]
    fn a_removed_account_is_chosen_by_its_id() {
        let tiny = tiny();
        let accounts = vec![tiny.accounts[0].clone()];
        let ctx = Ctx {
            accounts: &accounts,
            ..tiny.ctx()
        };
        let query = Query {
            range: RangeKey::All,
            accounts: vec![tiny.ac().account_uuid.clone()],
            host: None,
        };

        let weeks = account_weeks(&ctx, &two_weeks(), &query).unwrap();
        assert_eq!(weeks.len(), 2, "AC's two finished weeks");
        assert!(weeks.iter().all(|week| week.account == tiny.ac_ref()));
    }

    /// A block is a record of one host: atlas blocked LB at 10:00 and
    /// unblocked it on 9 Nov, vega blocked it at 11:00 and unblocked it at
    /// 13:00. Every span must lie inside the block of its own host.
    #[test]
    fn a_block_belongs_to_the_host_that_recorded_it() {
        let tiny = tiny();
        let lb = tiny.lb_ref();
        let vega = HistoryDir::at(tiny.dir.path(), "vega");
        vega.append_many(&[
            Event {
                t: utc("2026-11-02T08:00:00Z"),
                kind: EventKind::Live {
                    account: Some(lb.clone()),
                },
            },
            Event {
                t: utc("2026-11-07T11:00:00Z"),
                kind: EventKind::Blocked {
                    account: lb.clone(),
                    k: LimitKind::WeeklyAll,
                    m: None,
                },
            },
            Event {
                t: utc("2026-11-07T13:00:00Z"),
                kind: EventKind::Unblocked {
                    account: lb.clone(),
                    k: LimitKind::WeeklyAll,
                    m: None,
                },
            },
        ])
        .expect("the vega records");
        fs::write(
            tiny.dir.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .expect("the vega marker");

        let spans = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::Anthropic).unwrap();
        let vega_spans: Vec<&BlockedSpan> =
            spans.iter().filter(|span| span.host == "vega").collect();
        assert_eq!(vega_spans.len(), 1, "vega's spans: {vega_spans:#?}");
        assert_eq!(vega_spans[0].from, utc("2026-11-07T11:00:00Z"));
        assert_eq!(vega_spans[0].to, utc("2026-11-07T13:00:00Z"));
        assert!(
            close(vega_spans[0].hours, 1.0),
            "hours {}",
            vega_spans[0].hours
        );
    }

    /// The week starts at Monday 00:00 in the local time of the host, also
    /// when that is not midnight in UTC.
    #[test]
    fn the_range_starts_at_the_local_monday() {
        let tiny = tiny();
        let ctx = Ctx {
            zone: FixedOffset::east_opt(3600).expect("+01:00"),
            ..tiny.ctx()
        };
        let this_week = range_of(RangeKey::ThisWeek, &ctx).unwrap();
        assert_eq!(this_week.from, utc("2026-11-15T23:00:00Z"));
        assert_eq!(this_week.to, tiny.now);

        let last_week = range_of(RangeKey::LastWeek, &ctx).unwrap();
        assert_eq!(last_week.from, utc("2026-11-08T23:00:00Z"));
        assert_eq!(last_week.to, utc("2026-11-15T23:00:00Z"));

        let days30 = range_of(RangeKey::Days30, &ctx).unwrap();
        assert_eq!(days30.from, utc("2026-10-18T23:00:00Z"));
    }

    /// The hours are counted by the minute: 30 working minutes are half an
    /// hour.
    #[test]
    fn blocked_hours_are_counted_by_the_minute() {
        let tiny = tiny();
        // A range that starts inside LB's block, half an hour before the
        // switch that ended it.
        let half_hour = Range {
            from: utc("2026-11-07T11:30:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        };
        let spans = blocked_spans(&tiny.ctx(), &half_hour, &all(), Company::Anthropic).unwrap();
        let lb: Vec<&BlockedSpan> = spans
            .iter()
            .filter(|span| span.account == tiny.lb_ref())
            .collect();
        assert_eq!(lb.len(), 1, "LB's spans: {lb:#?}");
        assert_eq!(lb[0].from, utc("2026-11-07T11:30:00Z"));
        assert_eq!(lb[0].to, utc("2026-11-07T12:00:00Z"));
        assert!(close(lb[0].hours, 0.5), "hours {}", lb[0].hours);
    }
    /// A record at 09:00 in +02:00 stays in the heat map's hour 9 even when the
    /// range starts in +01:00: every local hour uses its own instant's offset.
    /// Monday 30 March ends at 22:00 UTC (00:00 Tuesday in +02:00), so a range
    /// that stops at 12:00 UTC leaves the day unfinished.
    #[test]
    fn a_summer_record_is_in_its_own_local_hour() {
        let tiny = tiny();
        tiny.history
            .append(&TokenRecord {
                t: utc("2026-03-30T07:00:00Z"),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas/engine".to_string(),
                project: crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 0,
                out: 1_000_000,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            })
            .expect("the summer record");
        // The range starts in +01:00 and ends after the change; it touches two
        // calendar weeks, and Monday 30 March has fully ended at its end.
        let range = Range {
            from: utc("2026-03-23T00:00:00Z"),
            to: utc("2026-03-30T22:00:00Z"),
        };
        let ctx = ctx_with(&tiny, SummerZone);
        let page = activity::rhythm_in(&ctx, &range, &all()).expect("the Rhythm page");

        assert!(close(page.heat[9], 500_000.0), "heat 9: {}", page.heat[9]);
        assert!(close(page.heat[8], 0.0), "heat 8: {}", page.heat[8]);
        assert!(
            close(
                page.per_day[0].working_hours.expect("one whole Monday"),
                1.0
            ),
            "Monday: {:?}",
            page.per_day[0].working_hours
        );
        // The same record with the range ending at 12:00 UTC: Monday 30 March
        // has not ended there (it ends at 22:00 UTC), so its average is `None`,
        // never 0.
        let mid_day = Range {
            from: utc("2026-03-23T00:00:00Z"),
            to: utc("2026-03-30T12:00:00Z"),
        };
        let mid_day_page = activity::rhythm_in(&ctx, &mid_day, &all()).expect("the Rhythm page");
        assert_eq!(mid_day_page.per_day[0].working_hours, None);
    }

    /// Every column of a range across the change starts at Monday 00:00 local:
    /// 23:00 UTC in +01:00 and 22:00 UTC in +02:00.
    #[test]
    fn a_week_starts_on_monday_in_both_offsets() {
        let range = Range {
            from: utc("2026-03-01T00:00:00Z"),
            to: utc("2026-04-10T00:00:00Z"),
        };
        let columns = week_columns(&range, &SummerZone);
        assert!(columns.len() >= 5, "columns: {columns:?}");
        let mut saw_winter = false;
        let mut saw_summer = false;
        for column in &columns {
            let local = column.with_timezone(&SummerZone);
            assert_eq!(local.weekday().num_days_from_monday(), 0, "{column}");
            assert_eq!(local.hour(), 0, "{column}");
            assert_eq!(local.minute(), 0, "{column}");
            match local.offset().local_minus_utc() {
                3600 => saw_winter = true,
                7200 => saw_summer = true,
                other => panic!("unexpected offset {other} at {column}"),
            }
        }
        assert!(saw_winter, "no week started in +01:00");
        assert!(saw_summer, "no week started in +02:00");
    }

    /// A fall-back week lasts 169 hours: adding 168 h to its Monday would land
    /// on Sunday 23:00 local and find the same week again. The columns must
    /// advance a local Monday at a time.
    #[test]
    fn a_fall_back_week_still_starts_on_monday() {
        let range = Range {
            from: utc("2026-10-19T00:00:00Z"),
            to: utc("2026-11-08T23:00:00Z"),
        };
        let columns = week_columns(&range, &FallZone);
        assert_eq!(
            columns,
            vec![
                utc("2026-10-18T22:00:00Z"),
                utc("2026-10-25T23:00:00Z"),
                utc("2026-11-01T23:00:00Z"),
            ]
        );
        for column in &columns {
            let local = column.with_timezone(&FallZone);
            assert_eq!(local.weekday().num_days_from_monday(), 0, "{column}");
            assert_eq!(local.time(), NaiveTime::MIN, "{column}");
        }
        assert_eq!(previous_week_start(columns[1], &FallZone), columns[0]);
        assert_eq!(next_week_start(columns[0], &FallZone), columns[1]);
    }

    /// The fall-back day lasts 25 hours; its end is the next local midnight,
    /// not the instant 24 h later.
    #[test]
    fn a_fall_back_day_ends_at_the_next_local_midnight() {
        let start = utc("2026-10-24T22:00:00Z"); // Sunday 00:00 +02:00
        let end = next_local_day_start(start, &FallZone);
        assert_eq!(end, utc("2026-10-25T23:00:00Z")); // Monday 00:00 +01:00
        assert_eq!(end - start, Duration::hours(25));
    }

    /// The repeated hour of the fall-back night: an instant in the second
    /// 02:00-03:00 starts its own hour (01:00Z), never the first one (00:00Z),
    /// whatever order the zone names the two instants in. With the first one,
    /// the hour loops of the working hours and the rates never got past
    /// 01:00Z and Statistics hung (bench world `year`, 2026-11-17).
    #[test]
    fn the_repeated_hour_of_a_fall_back_night_moves_on() {
        assert_eq!(
            local_hour_start(utc("2026-10-25T01:30:00Z"), &FallZone),
            utc("2026-10-25T01:00:00Z")
        );
        assert_eq!(
            local_hour_start(utc("2026-10-25T00:30:00Z"), &FallZone),
            utc("2026-10-25T00:00:00Z")
        );
        let start = utc("2026-10-24T22:00:00Z");
        let rhythm = crate::burn::Rhythm { usd: [1.0; 168] };
        let hours = working_hours(
            start,
            next_local_day_start(start, &FallZone),
            &rhythm,
            &FallZone,
        );
        assert!((hours - 25.0).abs() < 1e-9, "{hours}");
    }

    /// The fixture's Codex account: the one OpenAI subscription of the P-2
    /// tests.
    pub(crate) fn codex_ref() -> AccountRef {
        AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        }
    }

    /// The Google account agy is signed in to.
    pub(crate) fn google_ref() -> AccountRef {
        AccountRef {
            acc: "sam@mail.example".to_string(),
            org: crate::providers::GOOGLE_ORG.to_string(),
        }
    }

    /// A Codex token record of `in` input tokens and no output.
    pub(crate) fn codex_token(t: &str, model: &str, input: u64) -> TokenRecord {
        TokenRecord {
            t: utc(t),
            minutes: 60,
            session: "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Repo("github.com/acme/atlas".to_string()),
            model: model.to_string(),
            input,
            out: 0,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        }
    }

    /// Price the Codex model `gpt-6.2-codex` at 10 USD a million input tokens.
    pub(crate) fn codex_prices(prices: &mut Prices) {
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.2-codex".to_string(),
                crate::money::ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
    }

    /// P-2 counts an OpenAI subscription's finished week as an account week; the
    /// Claude accounts' weeks and blocked spans stay what they were.
    #[test]
    fn provider_records_do_not_change_the_claude_account_weeks() {
        let without = tiny();
        let with = tiny();
        append_codex_records(&with.history, utc("2026-11-14T09:00:00Z"));

        let before = account_weeks(&without.ctx(), &two_weeks(), &all()).expect("the weeks");
        let after = account_weeks(&with.ctx(), &two_weeks(), &all()).expect("the weeks");
        let claude = |weeks: &[AccountWeek]| {
            weeks
                .iter()
                .filter(|week| Company::of_account(&week.account) == Company::Anthropic)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            claude(&after),
            claude(&before),
            "a Codex window changed the Claude weeks"
        );
        assert!(
            after
                .iter()
                .any(|week| Company::of_account(&week.account) == Company::OpenAI),
            "the Codex week is an account week (P-2)"
        );

        let before = blocked_spans(&without.ctx(), &two_weeks(), &all(), Company::Anthropic)
            .expect("the spans");
        let after = blocked_spans(&with.ctx(), &two_weeks(), &all(), Company::Anthropic)
            .expect("the spans");
        assert_eq!(
            after, before,
            "a Codex event changed the Claude blocked spans"
        );
    }

    /// An OpenAI block counts in OpenAI's working hours only: a block at an hour
    /// where only Claude worked has no OpenAI hours, one at an hour of Codex work
    /// has. The Claude side never counts an OpenAI block (decision P-2 5).
    #[test]
    fn openai_blocked_hours_count_in_openai_working_hours_only() {
        let mut tiny = tiny();
        let codex = codex_ref();
        tiny.provider_subscriptions = vec![provider_subscription(&codex, "OA", None)];
        codex_prices(&mut tiny.prices);
        // Codex works at 14:00, when no Claude record is; the Claude accounts
        // work at 09:00 that day.
        tiny.history
            .append(&codex_token(
                "2026-11-11T14:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("the Codex record");
        for at in ["2026-11-11T09:00:00Z", "2026-11-11T14:00:00Z"] {
            let from = utc(at);
            tiny.history
                .append(&Event {
                    t: from,
                    kind: EventKind::Blocked {
                        account: codex.clone(),
                        k: LimitKind::Session,
                        m: None,
                    },
                })
                .expect("the OpenAI block");
            tiny.history
                .append(&Event {
                    t: from + Duration::hours(1),
                    kind: EventKind::Unblocked {
                        account: codex.clone(),
                        k: LimitKind::Session,
                        m: None,
                    },
                })
                .expect("the OpenAI unblock");
        }

        let openai = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::OpenAI)
            .expect("the OpenAI spans");
        assert_eq!(openai.len(), 2, "both blocks are OpenAI spans");
        let hours: f64 = openai.iter().map(|span| span.hours).sum();
        assert!(
            close(hours, 1.0),
            "only the hour of Codex work counts: {hours}"
        );
        let claude_hour = openai
            .iter()
            .find(|span| span.from == utc("2026-11-11T09:00:00Z"))
            .expect("the 09:00 block is an OpenAI span");
        assert_eq!(
            claude_hour.hours, 0.0,
            "09:00 is Claude working time, not OpenAI's"
        );

        let claude = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::Anthropic)
            .expect("the Claude spans");
        assert!(
            claude.iter().all(|span| span.account != codex),
            "a Claude span holds an OpenAI block"
        );
    }

    /// Claude's blocked hours ignore Codex work: a Claude block in an hour where
    /// only Codex worked has no Claude hours, and an OpenAI span never holds a
    /// Claude block (decision P-2 5).
    #[test]
    fn claude_blocked_hours_ignore_codex_work() {
        let mut tiny = tiny();
        codex_prices(&mut tiny.prices);
        // Codex works at 14:00; no Claude record is there, LB is live.
        tiny.history
            .append(&codex_token(
                "2026-11-11T14:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("the Codex record");
        let live = tiny.lb_ref();
        tiny.history
            .append(&Event {
                t: utc("2026-11-11T14:00:00Z"),
                kind: EventKind::Blocked {
                    account: live.clone(),
                    k: LimitKind::Session,
                    m: None,
                },
            })
            .expect("the Claude block");
        tiny.history
            .append(&Event {
                t: utc("2026-11-11T15:00:00Z"),
                kind: EventKind::Unblocked {
                    account: live.clone(),
                    k: LimitKind::Session,
                    m: None,
                },
            })
            .expect("the Claude unblock");

        let claude = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::Anthropic)
            .expect("the Claude spans");
        let block = claude
            .iter()
            .find(|span| span.from == utc("2026-11-11T14:00:00Z"))
            .expect("the Claude block is a span");
        assert_eq!(
            block.hours, 0.0,
            "14:00 is Codex working time, not Claude's"
        );

        let openai = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::OpenAI)
            .expect("the OpenAI spans");
        assert!(
            openai.iter().all(|span| span.account != live),
            "an OpenAI span holds a Claude block"
        );
    }

    /// An Ollama subscription counts per month: `used_percent` is the share of
    /// the month's credits used, its money a hundredth of the month's price per
    /// percent, and a finished month's unused credits are its expired money
    /// (decision P-2 3, 9).
    #[test]
    fn ollama_month_use_is_the_share_of_credits_used() {
        let tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        let month = |t: &str, percent: f64, until: &str| Sample {
            t: utc(t),
            account: ollama.clone(),
            src: crate::history::Source::Poll,
            l: vec![crate::history::LimitSample {
                k: LimitKind::Other("monthly_credits".to_string()),
                m: None,
                p: percent,
                r: Some(utc(until)),
                b: false,
            }],
        };
        // October ended with 40 % of its credits used; November has used 60 % so
        // far, with no finished window record.
        tiny.history
            .append(&month("2026-10-20T00:00:00Z", 40.0, "2026-11-01T00:00:00Z"))
            .expect("October's reading");
        tiny.history
            .append(&month("2026-11-10T00:00:00Z", 25.0, "2026-12-01T00:00:00Z"))
            .expect("November's first reading");
        tiny.history
            .append(&month("2026-11-15T00:00:00Z", 60.0, "2026-12-01T00:00:00Z"))
            .expect("November's newest reading");
        tiny.history
            .append(&Event {
                t: utc("2026-10-01T00:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: ollama.clone(),
                    amount: Some(20.0),
                    currency: Some("EUR".to_string()),
                },
            })
            .expect("the subscription's price");

        let months = subscription_months(
            &tiny.ctx(),
            &Range {
                from: utc("2026-10-01T00:00:00Z"),
                to: utc("2026-12-02T00:00:00Z"),
            },
            &ollama,
        )
        .expect("the months");
        assert_eq!(months.len(), 2, "{months:?}");
        assert_eq!(months[0].end, utc("2026-11-01T00:00:00Z"));
        assert_eq!(months[0].used_percent, 40.0);
        assert_eq!(months[0].money, Some(8.0), "40 % of 20 EUR");
        assert_eq!(months[0].expired, Some(12.0), "the unused 60 %");
        assert_eq!(months[1].start, utc("2026-11-01T00:00:00Z"));
        assert_eq!(months[1].end, utc("2026-12-01T00:00:00Z"));
        assert_eq!(months[1].used_percent, 60.0);
        assert_eq!(months[1].money, Some(12.0), "60 % of 20 EUR");
        assert_eq!(
            months[1].expired,
            Some(0.0),
            "a running month has expired nothing yet"
        );
    }

    /// An Ollama month's value is the same whether the page's range covers the
    /// whole month or only its last days: the valuer reads every overlapping
    /// month by the longest window it values (decision P-2 2, review A).
    #[test]
    fn ollama_month_value_does_not_depend_on_the_range() {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![provider_subscription(&ollama, "OL", Some(20.0))];
        tiny.prices.take(
            BTreeMap::from([(
                "ollama-cloud/deepseek-v4.1-flash".to_string(),
                crate::money::ModelPrice {
                    input: 1.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let month = |t: &str, percent: f64| Sample {
            t: utc(t),
            account: ollama.clone(),
            src: crate::history::Source::Poll,
            l: vec![crate::history::LimitSample {
                k: LimitKind::Other("monthly_credits".to_string()),
                m: None,
                p: percent,
                r: Some(utc("2026-12-01T00:00:00Z")),
                b: false,
            }],
        };
        tiny.history
            .append(&month("2026-11-02T00:00:00Z", 10.0))
            .expect("the first reading");
        tiny.history
            .append(&month("2026-11-28T00:00:00Z", 60.0))
            .expect("the newest reading");
        let first = codex_token(
            "2026-11-02T10:00:00Z",
            "deepseek-v4.1-flash:cloud",
            1_000_000,
        );
        let second = codex_token(
            "2026-11-28T10:00:00Z",
            "deepseek-v4.1-flash:cloud",
            1_000_000,
        );
        tiny.history.append(&first).expect("a record on 2 Nov");
        tiny.history.append(&second).expect("a record on 28 Nov");

        for range in [
            Range {
                from: utc("2026-11-01T00:00:00Z"),
                to: utc("2026-12-01T00:00:00Z"),
            },
            Range {
                from: utc("2026-11-25T00:00:00Z"),
                to: utc("2026-12-01T00:00:00Z"),
            },
        ] {
            let valuer = Valuer::build(
                &tiny.history,
                &range,
                &tiny.prices,
                &tiny.fx,
                &tiny.settings,
                &tiny.accounts,
                &tiny.provider_subscriptions,
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer");
            for record in [&first, &second] {
                let value = valuer.value("atlas", record);
                assert_eq!(value.account, Some(ollama.clone()));
                assert!(
                    (value.subscription.expect("a value") - 6.0).abs() < 1e-9,
                    "each equal record is half of 60 % of 20 EUR: {:?}",
                    value.subscription
                );
            }
        }
    }

    /// An Ollama month starts one calendar month before its reset, never 30
    /// days before it: a 31-day month keeps its day 1 (decision P-2 3, review
    /// H).
    #[test]
    fn an_ollama_month_starts_one_calendar_month_before_its_reset() {
        let tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.history
            .append(&Sample {
                t: utc("2026-12-15T10:00:00Z"),
                account: ollama.clone(),
                src: crate::history::Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::Other("monthly_credits".to_string()),
                    m: None,
                    p: 30.0,
                    r: Some(utc("2027-01-01T00:00:00Z")),
                    b: false,
                }],
            })
            .expect("December's reading");

        let months = subscription_months(
            &tiny.ctx(),
            &Range {
                from: utc("2026-12-01T00:00:00Z"),
                to: utc("2027-01-02T00:00:00Z"),
            },
            &ollama,
        )
        .expect("the months");
        assert_eq!(months.len(), 1, "{months:?}");
        assert_eq!(
            months[0].start,
            utc("2026-12-01T00:00:00Z"),
            "December has 31 days: the month starts on day 1, not 2 December"
        );
        assert_eq!(months[0].end, utc("2027-01-01T00:00:00Z"));
        assert_eq!(months[0].used_percent, 30.0);
    }

    /// A provider's block counts only while that subscription was the one in
    /// use: after the switch to OA2 at 13:00, OA's 13:00-16:00 block has no
    /// OpenAI hours (decision P-2 5, review D).
    #[test]
    fn an_openai_block_counts_only_while_its_subscription_was_in_use() {
        let mut tiny = tiny();
        let oa = codex_ref();
        let oa2 = AccountRef {
            acc: "5d2e7a1c-3b4f-4a6e-9c8d-1f2a3b4c5d6e".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        tiny.provider_subscriptions = vec![
            provider_subscription(&oa, "OA", None),
            provider_subscription(&oa2, "OA2", None),
        ];
        codex_prices(&mut tiny.prices);
        // Codex works at 14:00, inside the block.
        tiny.history
            .append(&codex_token(
                "2026-11-12T14:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("the Codex record");
        tiny.history
            .append(&Event {
                t: utc("2026-11-12T13:00:00Z"),
                kind: EventKind::ProviderSwitch {
                    from: Some(oa.clone()),
                    to: oa2,
                    by: By::User,
                },
            })
            .expect("the switch to OA2");
        tiny.history
            .append(&Event {
                t: utc("2026-11-12T13:00:00Z"),
                kind: EventKind::Blocked {
                    account: oa,
                    k: LimitKind::Session,
                    m: None,
                },
            })
            .expect("OA's block");
        tiny.history
            .append(&Event {
                t: utc("2026-11-12T16:00:00Z"),
                kind: EventKind::Unblocked {
                    account: codex_ref(),
                    k: LimitKind::Session,
                    m: None,
                },
            })
            .expect("OA's unblock");

        let spans = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::OpenAI)
            .expect("the OpenAI spans");
        let hours: f64 = spans.iter().map(|span| span.hours).sum();
        assert_eq!(hours, 0.0, "OA2 was in use during OA's block: {spans:?}");
    }

    /// Two stored OpenAI logins and no `providerSwitch`: the block belongs to
    /// neither, so it is not counted, but the statistics say why once for the
    /// host (review, 1.3.0).
    #[test]
    fn two_openai_logins_without_a_switch_name_the_dropped_block_once() {
        let mut tiny = tiny();
        let oa = codex_ref();
        let oa2 = AccountRef {
            acc: "5d2e7a1c-3b4f-4a6e-9c8d-1f2a3b4c5d6e".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        tiny.provider_subscriptions = vec![
            provider_subscription(&oa, "OA", Some(23.0)),
            provider_subscription(&oa2, "OA2", Some(23.0)),
        ];
        for (from, to) in [
            ("2026-11-12T13:00:00Z", "2026-11-12T16:00:00Z"),
            ("2026-11-13T13:00:00Z", "2026-11-13T16:00:00Z"),
        ] {
            tiny.history
                .append(&Event {
                    t: utc(from),
                    kind: EventKind::Blocked {
                        account: oa.clone(),
                        k: LimitKind::Session,
                        m: None,
                    },
                })
                .expect("OA's block");
            tiny.history
                .append(&Event {
                    t: utc(to),
                    kind: EventKind::Unblocked {
                        account: oa.clone(),
                        k: LimitKind::Session,
                        m: None,
                    },
                })
                .expect("OA's unblock");
        }

        let spans = blocked_spans(&tiny.ctx(), &two_weeks(), &all(), Company::OpenAI)
            .expect("the OpenAI spans");
        assert!(
            spans.is_empty(),
            "no stored login can be named for the block: {spans:?}"
        );
        let warnings = tiny.warnings.borrow();
        let named = warnings
            .iter()
            .filter(|warning| warning.contains("belongs to no stored OpenAI login"))
            .count();
        assert_eq!(named, 1, "one warning per host: {warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("the OpenAI block of OA on atlas at ")),
            "{warnings:?}"
        );
    }

    // ---- proof tests of the review ----

    /// The statistics serve Claude's accounts, but they read the same files
    /// the provider readings are written to. A history that holds only a
    /// provider's sample, window and event holds no Claude record at all: the
    /// page must say so, and the "All" range must start at this week, not at
    /// the provider's reading.
    #[test]
    fn a_provider_record_is_not_the_statistics_first_record() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        append_codex_records(&history, utc("2026-11-14T09:00:00Z"));

        let tiny = tiny();
        let ctx = Ctx {
            history: &history,
            accounts: &tiny.accounts,
            provider_subscriptions: &tiny.provider_subscriptions,
            settings: &tiny.settings,
            prices: &tiny.prices,
            fx: &tiny.fx,
            projects: &tiny.projects,
            warnings: &tiny.warnings,
            now: tiny.now,
            zone: tiny.offset,
        };

        assert_eq!(
            empty(&ctx, &two_weeks()).expect("the empty page"),
            Some(Empty {
                recording_since: None
            }),
            "a Codex record made the statistics page think it holds records"
        );
    }

    /// The "All" range starts at the week of the first Claude record. A
    /// provider's reading is not one, so it must not move the range's start.
    #[test]
    fn a_provider_record_does_not_move_the_all_range() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        append_codex_records(&history, utc("2026-11-14T09:00:00Z"));

        let tiny = tiny();
        let ctx = Ctx {
            history: &history,
            accounts: &tiny.accounts,
            provider_subscriptions: &tiny.provider_subscriptions,
            settings: &tiny.settings,
            prices: &tiny.prices,
            fx: &tiny.fx,
            projects: &tiny.projects,
            warnings: &tiny.warnings,
            now: tiny.now,
            zone: tiny.offset,
        };

        assert_eq!(
            range_of(RangeKey::All, &ctx).expect("the range"),
            Range {
                from: utc("2026-11-16T00:00:00Z"),
                to: tiny.now
            },
            "a Codex record moved the start of the All range"
        );
    }

    /// An hour count is a duration of the one spelling, rounded to the minute;
    /// a negative or non-finite count keeps the old one-decimal text, as before
    /// review item 42 (callers never pass one).
    #[test]
    fn an_hour_count_is_the_one_duration_spelling() {
        assert_eq!(hours_text(2.0), "2h");
        assert_eq!(hours_text(0.5), "30min");
        assert_eq!(hours_text(0.0), "0h");
        assert_eq!(hours_text(0.004), "0h");
        assert_eq!(hours_text(2.5), "2h 30min");
        assert_eq!(hours_text(141.0), "5d 21h");
        assert_eq!(hours_text(-2.0), "-2 h");
        assert_eq!(hours_text(f64::INFINITY), "inf h");
        assert_eq!(hours_text(f64::NAN), "NaN h");
    }

    /// Every kind of switch counts once in the total.
    #[test]
    fn switch_counts_total_adds_every_kind() {
        let counts = SwitchCounts {
            user: 1,
            cli: 2,
            auto: 3,
            agent: 4,
            external: 5,
        };
        assert_eq!(counts.total(), 15);
    }

    /// A value whose halves are in `currency`, both known.
    fn values_in(currency: &str, api: f64, subscription: f64) -> Values {
        Values {
            api: Some(api),
            api_currency: Some(currency.to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(subscription),
            currency: Some(currency.to_string()),
            subscription_missing: None,
            subscription_partial: false,
        }
    }

    /// Folding values one by one goes through `Values::sum`, so a half that two
    /// currencies made unknown stays unknown with that reason when a third
    /// value of one of the currencies is added (R3, one way per job).
    #[test]
    fn folding_values_one_by_one_is_one_sum_when_two_currencies_meet() {
        let eur1 = values_in("EUR", 1.0, 0.5);
        let usd = values_in("USD", 2.0, 0.75);
        let eur2 = values_in("EUR", 3.0, 1.5);
        let flat = Values::sum(&[&eur1, &usd, &eur2]);

        let mut folded: Option<Values> = None;
        add_opt_value(&mut folded, &eur1);
        add_opt_value(&mut folded, &usd);
        add_opt_value(&mut folded, &eur2);
        let folded = folded.expect("the three folded values");

        assert_eq!(flat.api, None, "two API currencies are never added");
        assert_eq!(
            flat.api_missing.as_deref(),
            Some(
                "the dollar rate is missing for some of these days, so the values are in two currencies"
            )
        );
        assert_eq!(folded.api, flat.api, "folding changed the API half");
        assert_eq!(folded.api_missing, flat.api_missing);
        assert_eq!(folded.api_partial, flat.api_partial);
        assert_eq!(flat.subscription, None);
        assert_eq!(
            flat.subscription_missing.as_deref(),
            Some("these sessions are valued in two currencies")
        );
        assert_eq!(
            folded.subscription, flat.subscription,
            "folding changed the subscription half"
        );
        assert_eq!(folded.subscription_missing, flat.subscription_missing);
        assert_eq!(folded.subscription_partial, flat.subscription_partial);
    }

    /// A share of an API value counts the priced work only (the one named
    /// exception of [`full_half`]): it divides a part's known API value by the
    /// whole's known API value, so a partial whole still gives shares and a
    /// partial part keeps its share. A part with no priced API value at all is
    /// left out, and a whole without one, or a known 0, gives no shares.
    #[test]
    fn an_api_share_counts_the_priced_work_only() {
        let full = |api: f64| values_in("EUR", api, 0.0);
        let partial = Values {
            api: Some(40.0),
            api_currency: Some("EUR".to_string()),
            api_missing: Some("no price for claude-x".to_string()),
            api_partial: true,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some("no price for claude-x".to_string()),
            subscription_partial: false,
        };
        let unknown = Values {
            api: None,
            api_currency: None,
            api_missing: Some("no price for claude-y".to_string()),
            api_partial: false,
            subscription: None,
            currency: None,
            subscription_missing: Some("no price for claude-y".to_string()),
            subscription_partial: false,
        };
        let by_project = BTreeMap::from([
            ("atlas".to_string(), full(60.0)),
            ("ledger".to_string(), partial.clone()),
            ("notes".to_string(), unknown.clone()),
        ]);

        let partial_total = Values::sum(&[&full(60.0), &partial]);
        assert!(partial_total.api_partial, "{partial_total:?}");
        assert_eq!(
            api_shares(by_project.clone(), &partial_total),
            vec![("atlas".to_string(), 60.0), ("ledger".to_string(), 40.0)],
            "the priced parts share the priced total; the unpriced project is left out"
        );

        let full_total = Values::sum(&[&full(60.0), &full(60.0)]);
        assert_eq!(
            api_shares(by_project.clone(), &full_total),
            vec![
                ("atlas".to_string(), 50.0),
                ("ledger".to_string(), 40.0 / 120.0 * 100.0)
            ],
            "the unpriced project is left out, the priced parts share the full total"
        );

        let unknown_total = Values::sum(&[&unknown]);
        assert_eq!(unknown_total.api, None);
        assert!(
            api_shares(by_project.clone(), &unknown_total).is_empty(),
            "a whole without a priced part gives no shares"
        );

        let zero_total = Values::sum(&[]);
        assert_eq!(zero_total.api, Some(0.0));
        assert!(
            api_shares(by_project, &zero_total).is_empty(),
            "a share of a known 0 is no share"
        );
    }
}
