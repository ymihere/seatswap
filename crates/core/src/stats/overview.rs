//! The Overview page (spec §3.1): what the chosen range used, paid and let
//! expire, the hours blocked, the switches and what they cost, each week by
//! account and by project, the marks of the range, and the findings. Everything
//! here only reads.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::Serialize;

use crate::burn::cost::switch_cost;
use crate::burn::rates;
use crate::burn::{Rates, RATES_DAYS};
use crate::history::{
    AccountRef, Event, EventKind, Hosted, Hosts, Range, Read, Sample, TokenRecord,
};
use crate::live_sessions::Values;
use crate::money::{Company, Valuer, WindowMoney};
use crate::project::ProjectKey;
use crate::projects::ProjectId;

use super::{
    account_abbr, account_id, account_position, account_weeks_so_far, accounts_without_price,
    add_opt_value, add_value, blocked_by_week, blocked_spans, date_text, finished_windows,
    full_half, hours_text, keep_warnings, money_of, money_text, offered_finished_money,
    offered_money, percent_of_offered, percent_text, range_of, running_account_weeks,
    subscription_shares, switch_counts, top_three, week_columns, week_lost_percent, week_of_record,
    week_span, week_start, window_used, working_rhythm, AccountWeek, Ctx, Query, StatsError,
    SwitchCounts,
};

/// The Overview page's numbers for one range (spec §3.1).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    /// The accounts with at least one week left out of the money sums, in the
    /// order of [`Ctx::accounts`]: a week without a price is left out, while
    /// the account's priced weeks still count. An account no longer stored is
    /// named by its id after the stored ones.
    pub without_price: Vec<String>,
    pub kpis: Kpis,
    pub weeks: Vec<OverviewWeek>,
    pub marks: Vec<Mark>,
    pub findings: Vec<String>,
}

/// The This-week card on top of the Overview page ([`this_week_card`]): each
/// chosen account's weekly window that runs now ([`running_account_weeks`]),
/// the work of every other provider in the current calendar week, what it all
/// went to, and the totals on top. The Overview's history below counts every
/// weekly window of its range, the running ones included
/// ([`account_weeks_so_far`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisWeek {
    /// One per chosen account with a running weekly window, in the order of
    /// [`Ctx::accounts`], an account no longer stored after them by id; empty
    /// until the first usage reading.
    pub accounts: Vec<ThisWeekAccount>,
    /// One per other company (OpenAI, Ollama) with token records in the
    /// current calendar week, in [`Company`]'s order.
    pub providers: Vec<ThisWeekProvider>,
    /// The three totals over every record the card counts.
    pub totals: ThisWeekTotals,
    /// One row per project the counted records belong to, sorted by the known
    /// part of its subscription value, largest first, ties by output tokens,
    /// then by name. All projects, no cut: the page shows the first eight and
    /// folds the rest.
    pub projects: Vec<ThisWeekProject>,
}

/// The three totals on top of the This-week card.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisWeekTotals {
    /// The output tokens of every record the card counts.
    pub tokens_out: u64,
    /// The number of entries in [`ThisWeek::projects`].
    pub projects: u32,
    /// The two values of every counted record, summed with [`Values::sum`]:
    /// the subscription value used and the same work at API prices.
    pub used: Values,
    /// What the running windows' weeks cost: a Claude account's running week
    /// at its week price ([`money_of`]'s `paid`), an OpenAI subscription's
    /// running week at its week price, and an Ollama month as
    /// `price_month × 12 / 52` in the settings' currency (documented rule). A
    /// window without a price adds nothing, and Google's week stays out because
    /// its work is in no record ([`Company::work_recorded`]); `None` while no
    /// counted window has a price.
    pub paid: Option<f64>,
    /// A listed running window (a Claude account's running week, a provider row
    /// with a running window) has no price, so `paid` holds only the others'
    /// sum and is a lower bound (R3). `false` when every listed window has a
    /// price and also when none has one (`paid` is `None` then).
    pub paid_partial: bool,
    /// `used.subscription / paid × 100`; `None` while either is unknown,
    /// `paid` is 0, or `used.subscription` is partial or `paid` is partial: a
    /// share is computed only from halves that are not partial ([`full_half`],
    /// R3).
    pub used_of_paid: Option<f64>,
}

/// One project row of the This-week card.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisWeekProject {
    /// The project's shown name, after the project log's joins.
    pub name: String,
    /// The key Statistics → Projects puts on the project's node; `None` only
    /// for a project no key names.
    pub key: Option<ProjectKey>,
    /// The output tokens of the project's counted records.
    pub tokens_out: u64,
    /// The two values of the project's counted records, summed with
    /// [`Values::sum`].
    pub values: Values,
    /// Each subscription's share of this project's API value, in %, largest
    /// first; `None` is work without a known account (Claude) or without a
    /// named subscription (provider). The id is the chip id: a stored Claude
    /// account's id ([`account_id`]) or a provider subscription's
    /// [`AccountRef::acc`]. The plan's comment calls this "subscription value
    /// per chip id"; the share of the API value is what R2's bars split by on
    /// every page and what Statistics → Projects gives, so both pages draw the
    /// same split. A share of an API value counts the priced work only
    /// (`stats::priced_api`): a part with no priced API value is left out. Empty
    /// while the project's API value is unknown or 0.
    pub by_subscription: Vec<(Option<String>, f64)>,
}

/// One provider subscription's row on the This-week card: its running window's
/// use, money and reset. Its records count from the calendar week's start for
/// Ollama (documented rule, so its work and its paid share cover the same week)
/// and from its running window's start for OpenAI (its `weekly_all` week),
/// exactly as an account row sums from its window's start (decision P-2 6). The
/// values the records make are on the card's project and total rows; this row
/// only names the subscription and its running window.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisWeekProvider {
    /// The subscription's stored abbreviation (`OA`, `OA2`, `OL`), or the
    /// company's name for a record whose subscription cannot be named.
    pub name: String,
    /// The subscription's company: `openai` or `ollama`.
    pub company: Company,
    /// The running window's use so far, in percent: OpenAI's `weekly_all` week
    /// or Ollama's `monthly_credits` month; `None` while the subscription has
    /// no running window.
    pub used: Option<f64>,
    /// The money of `used` at the subscription's price; `None` without a price
    /// or without a running window.
    pub money: Option<f64>,
    /// Why `money` is `None`, in the words [`ThisWeekAccount::money_missing`]
    /// uses; `None` when it has a value.
    pub money_missing: Option<String>,
    /// The running window's reset; `None` without a running window.
    pub resets_at: Option<DateTime<Utc>>,
}

/// One account's running weekly window on the This-week card.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisWeekAccount {
    /// The account's id ([`account_id`]).
    pub id: String,
    /// The use so far in percent: the newest reading minus the reading a
    /// reset began the window at (`stats::window_used`).
    pub used: f64,
    /// The newest reading's percent; above `used` when a reset began the
    /// window.
    pub now: f64,
    /// The money of `used` at the window's price; `None` without a price.
    pub money: Option<f64>,
    /// Why `money` is `None`, in the words the week money uses; `None` when
    /// it has a value.
    pub money_missing: Option<String>,
    /// The window's start ([`crate::history::WindowRecord::weekly_start`]).
    pub started_at: DateTime<Utc>,
    /// The window's reset.
    pub resets_at: DateTime<Utc>,
}

/// The five numbers of the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Kpis {
    /// Money used of the priced account weeks: `Σ week.used × pv`.
    pub used: f64,
    /// Money paid for the priced account weeks: `100 × pv` each.
    pub paid: f64,
    /// `lost × pv` of the priced account weeks.
    pub expired: f64,
    /// The used part of the offered weeks, in percent: every weekly window of
    /// the range — the running ones included — offers one week of its account's
    /// price, so a week that a reset split offers each of its windows.
    /// `used + expired + still open` make 100 % of them, except that a window
    /// whose use passed 100 % counts its whole use, so the two shares can then
    /// pass 100 % together (nothing is cut). `None` when no account week has a
    /// price.
    pub used_percent: Option<f64>,
    /// The expired part of the finished weeks, in percent: every finished
    /// window's `lost`, over the money those finished windows offered (a running
    /// window has expired nothing yet, so it is not in the denominator).
    /// `None` when no finished account week has a price.
    pub expired_percent: Option<f64>,
    /// The expired money per account id, largest first.
    pub expired_by_account: Vec<(String, f64)>,
    /// The working hours blocked in the range, one entry per company that has
    /// working hours in it, Anthropic first (decision P-2 5, 6): no sum across
    /// companies, because a provider's block is not an Anthropic one.
    pub blocked_by_company: Vec<BlockedByCompany>,
    /// The hours blocked in the calendar weeks before the last account added
    /// in the range was added; `None` when the range holds no such account.
    pub blocked_before_added: Option<BlockedBefore>,
    /// The part of Claude's blocked hours in which other quota expired.
    pub avoidable_hours: f64,
    pub switches: SwitchCounts,
    /// The five switch counts added up.
    pub switches_total: u32,
    /// What one switch costs the account in use at the range's end, as a
    /// percent of its 5 h window; `None` when the rates or the cost are missing.
    pub switch_cost_percent_of_window: Option<f64>,
    /// The calendar weeks the page shows: the weeks of the range.
    pub weeks: u32,
}

/// One company's working hours blocked in the range (decision P-2 6).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedByCompany {
    /// [`Company`]'s lowercase JSON name: `anthropic`, `openai`, `ollama`.
    pub company: Company,
    /// The company's blocked hours inside its own working hours.
    pub hours: f64,
}

/// The blocked hours of the weeks before an account was added, and the account.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedBefore {
    /// The added account's short name ([`account_abbr`]), as the chart's
    /// marks name it; the account may be one the page's filter leaves out.
    pub name: String,
    /// The blocked hours of the calendar weeks before the week it was added.
    pub hours: f64,
}

/// One calendar week of the range, with the account weeks that fall in it, the
/// running ones included.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewWeek {
    pub week: DateTime<Utc>,
    pub accounts: Vec<WeekAccount>,
    /// The projects' subscription value, the three largest then `other`.
    pub projects: Vec<(String, f64)>,
    /// The working hours blocked in this week.
    pub blocked_hours: f64,
    /// The expired money of the priced account weeks of this week over the money
    /// their finished windows offered, in percent: the same rule as
    /// [`Kpis::expired_percent`]. `None` when no account week of the week has a
    /// price, or when the week's only windows still run (they have expired
    /// nothing, and a running window's offer is not in the denominator).
    pub expired_share: Option<f64>,
    /// The calendar week that holds `ctx.now`, which the page marks "so far":
    /// its running windows count their use so far and expire nothing.
    pub running: bool,
}

/// One account's week as the Overview shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeekAccount {
    pub id: String,
    pub used_percent: f64,
    /// Money used; `None` without a price for the week.
    pub used: Option<f64>,
    pub expired_percent: f64,
    /// Money expired; `None` without a price for the week.
    pub expired: Option<f64>,
    pub uncertain: bool,
}

/// What the page marks on its chart in one calendar week: the accounts added
/// and the auto-burn switches of that week, in one label, at the first of them
/// ([`week_marks`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub at: DateTime<Utc>,
    pub label: String,
}

pub fn overview<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, q: &Query) -> Result<Overview, StatsError> {
    let range = range_of(q.range, ctx)?;
    overview_in(ctx, &range, q)
}

/// The Overview of `range`; the page's [`overview`] takes the range from its
/// [`Query`], the tests name one.
pub(crate) fn overview_in<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
) -> Result<Overview, StatsError> {
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
    let weeks_so_far = account_weeks_so_far(ctx, range, q, &valuer)?;
    let spans = blocked_spans(ctx, range, q, Company::Anthropic)?;
    let rhythm = working_rhythm(ctx, range, Company::Anthropic)?;
    let currency = ctx.settings.currency.clone();

    let events = keep_warnings(ctx.warnings, || {
        ctx.history.read_claude::<Event>(&Range::ALL, &Hosts::All)
    })?;
    let reading = reading_range(range, &weeks_so_far);
    let samples = keep_warnings(ctx.warnings, || {
        ctx.history.read_claude::<Sample>(&reading, &Hosts::All)
    })?;
    let tokens = keep_warnings(ctx.warnings, || {
        ctx.history.read::<TokenRecord>(&reading, &Hosts::All)
    })?;

    let without_price = accounts_without_price(ctx, &weeks_so_far);

    // The page shows every calendar week of the range, with or without an
    // account week; the blocked hours of all of them add up to the KPI.
    let this_week = week_start(ctx.now, &ctx.zone);
    let mut weeks_by_start: BTreeMap<DateTime<Utc>, OverviewWeek> = week_columns(range, &ctx.zone)
        .into_iter()
        .map(|week| (week, empty_week(week, this_week)))
        .collect();

    // The money of every priced account week; a week without a price is left
    // out of the sums and its account is named in `without_price`.
    let mut used = 0.0;
    let mut paid = 0.0;
    let mut expired = 0.0;
    // The shares of the offer: the Used share divides the use by the price of
    // every window (a week a reset split offers each of its windows, item 33,
    // R-1); the Expired share divides by the finished windows' price only,
    // because a running window has expired nothing yet.
    let mut offered = 0.0;
    let mut offered_finished = 0.0;
    let mut used_part = 0.0;
    let mut expired_part = 0.0;
    let mut priced_weeks: BTreeMap<AccountRef, u32> = BTreeMap::new();
    let mut expired_by_account: BTreeMap<AccountRef, f64> = BTreeMap::new();
    let mut money: BTreeMap<DateTime<Utc>, WindowMoney> = BTreeMap::new();
    let mut offered_finished_by_week: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
    for week in &weeks_so_far {
        let week_money = money_of(week)?;
        if let Some(week_money) = week_money {
            used += week_money.used;
            paid += week_money.paid;
            expired += week_money.expired;
            offered += offered_money(week, &week_money);
            let week_offered_finished = offered_finished_money(week, &week_money);
            offered_finished += week_offered_finished;
            *offered_finished_by_week.entry(week.week).or_insert(0.0) += week_offered_finished;
            used_part += week_money.used;
            expired_part += week_money.expired;
            if finished_windows(week) > 0 {
                *priced_weeks.entry(week.account.clone()).or_insert(0) += 1;
            }
            *expired_by_account
                .entry(week.account.clone())
                .or_insert(0.0) += week_money.expired;
            let week_sum = money.entry(week.week).or_insert(WindowMoney {
                used: 0.0,
                paid: 0.0,
                expired: 0.0,
            });
            week_sum.used += week_money.used;
            week_sum.paid += week_money.paid;
            week_sum.expired += week_money.expired;
        }
        weeks_by_start
            .entry(week.week)
            .or_insert_with(|| empty_week(week.week, this_week))
            .accounts
            .push(WeekAccount {
                id: account_id(ctx, &week.account),
                used_percent: week.used,
                used: week_money.map(|money| money.used),
                expired_percent: week_lost_percent(week)?,
                expired: week_money.map(|money| money.expired),
                uncertain: week.window.uncertain,
            });
    }

    let blocked_weeks = blocked_by_week(&spans, &rhythm, &ctx.zone);
    // One entry per company that has working hours in the range, Anthropic
    // first: a company's blocked hours count in its own working hours only,
    // never summed with another's (decision P-2 5, 6).
    let mut blocked_by_company = Vec::new();
    for company in Company::ALL {
        let company_rhythm = working_rhythm(ctx, range, company)?;
        if !company_rhythm.usd.iter().any(|usd| *usd > 0.0) {
            continue;
        }
        let hours = if company == Company::Anthropic {
            // The page's own spans, which its weeks and findings count too.
            spans.iter().fold(0.0, |sum, span| sum + span.hours)
        } else {
            blocked_spans(ctx, range, q, company)?
                .iter()
                .fold(0.0, |sum, span| sum + span.hours)
        };
        blocked_by_company.push(BlockedByCompany { company, hours });
    }
    let switches = switch_counts(&events.records, range, q.host.as_deref());
    let switches_total = switches.total();
    let mut kpis = Kpis {
        used,
        paid,
        expired,
        used_percent: percent_of_offered(used_part, offered),
        expired_percent: percent_of_offered(expired_part, offered_finished),
        expired_by_account: expired_by_account
            .into_iter()
            .map(|(reference, expired)| (account_id(ctx, &reference), expired))
            .collect(),
        blocked_by_company,
        blocked_before_added: blocked_before_added(ctx, range, &events.records, &blocked_weeks),
        avoidable_hours: spans
            .iter()
            .fold(0.0, |sum, span| sum + span.avoidable_hours),
        switches,
        switches_total,
        switch_cost_percent_of_window: switch_cost_percent(ctx, range, &events, &samples, &tokens),
        weeks: weeks_by_start.len() as u32,
    };
    kpis.expired_by_account
        .sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // The week's account rows follow the stored accounts' order; an account
    // that is no longer stored comes after them.
    for week in weeks_by_start.values_mut() {
        week.accounts.sort_by(|a, b| {
            account_position(ctx, &a.id)
                .cmp(&account_position(ctx, &b.id))
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    for (week, hours) in &blocked_weeks {
        weeks_by_start
            .entry(*week)
            .or_insert_with(|| empty_week(*week, this_week))
            .blocked_hours += *hours;
    }

    for (start, week) in weeks_by_start.iter_mut() {
        if let (Some(week_money), Some(offered)) =
            (money.get(start), offered_finished_by_week.get(start))
        {
            week.expired_share = percent_of_offered(week_money.expired, *offered);
        }
    }

    let mut project_sums: BTreeMap<DateTime<Utc>, BTreeMap<String, f64>> = BTreeMap::new();
    for hosted in &tokens.records {
        if q.host.as_ref().is_some_and(|host| host != &hosted.host) {
            continue;
        }
        let value = valuer.value(&hosted.host, &hosted.record);
        let (Some(account), Some(subscription)) = (value.account, value.subscription) else {
            continue;
        };
        let Some(week) = week_of_record(&weeks_so_far, &account, hosted.record.t) else {
            continue;
        };
        let name = ctx
            .projects
            .of(&ProjectId::of(&hosted.record.project, &hosted.host))
            .name;
        *project_sums
            .entry(week.week)
            .or_default()
            .entry(name)
            .or_insert(0.0) += subscription;
    }
    for (start, sums) in project_sums {
        weeks_by_start
            .entry(start)
            .or_insert_with(|| empty_week(start, this_week))
            .projects = top_three(sums.into_iter().collect());
    }

    let marks = week_marks(ctx, range, &events.records);

    let weeks: Vec<OverviewWeek> = weeks_by_start.into_values().collect();
    let mut findings = Vec::new();
    if let Some(text) = finding_expired(ctx, &kpis, &priced_weeks, currency.as_deref()) {
        findings.push(text);
    }
    if let Some(text) = finding_blocked(&kpis, &weeks) {
        findings.push(text);
    }
    if let Some(text) = finding_auto_burn(
        ctx,
        range,
        &events.records,
        &weeks,
        &money,
        &offered_finished_by_week,
    ) {
        findings.push(text);
    }
    if let Some(text) = finding_full(ctx, &weeks_so_far) {
        findings.push(text);
    }

    Ok(Overview {
        without_price,
        kpis,
        weeks,
        marks,
        findings,
    })
}

/// The This-week card: the chosen accounts' running weekly windows, each with
/// its use so far and its money, from the window's start to now; the work of
/// every other provider (Codex, Ollama) in the current calendar week, valued at
/// API prices; and the same records by project, with the three totals on top.
///
/// The host filter applies to the token records, as the Weeks summary filters
/// them; the range in `q` does not apply, so a range that holds no record still
/// has the card. A Claude record counts in its account's running window, or,
/// without a known account, from the calendar week's Monday 00:00; an OpenAI
/// record counts in its subscription's running week, and an Ollama record from
/// the calendar week's start (documented rule, so its work and its paid share
/// of the month cover the same week). A record of another provider uses no
/// account's week and is not filtered by the account chips.
pub fn this_week_card<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    q: &Query,
) -> Result<ThisWeek, StatsError> {
    // A running weekly window began at most seven days back, and so did this
    // calendar week's Monday.
    let valuer = Valuer::build(
        ctx.history,
        &Range {
            from: ctx.now - Duration::days(7),
            to: ctx.now,
        },
        ctx.prices,
        ctx.fx,
        ctx.settings,
        ctx.accounts,
        ctx.provider_subscriptions,
        ctx.warnings,
    )?;
    let running = running_account_weeks(ctx, q, &valuer);
    let calendar_week = week_start(ctx.now, &ctx.zone);
    // Every provider subscription the page can show: the stored ones, the
    // OpenAI weeks and Ollama month running now, and (once the records are
    // read) every subscription a record names. `None` means no running window.
    let mut provider_runs: BTreeMap<AccountRef, (Company, Option<ProviderRun>)> = BTreeMap::new();
    for subscription in ctx.provider_subscriptions {
        let company = Company::of_account(&subscription.account);
        if company != Company::Anthropic {
            provider_runs.insert(subscription.account.clone(), (company, None));
        }
    }
    for window in valuer.running_windows(ctx.now) {
        let company = Company::of_account(&window.account);
        if company == Company::Anthropic {
            continue;
        }
        let run = ProviderRun {
            start: window.weekly_start(),
            end: window.end,
            used: window_used(&window),
        };
        provider_runs.insert(window.account.clone(), (company, Some(run)));
    }
    let ollama = crate::money::ollama_subscription();
    if let Some(month) = valuer.running_month(&ollama, ctx.now) {
        provider_runs.insert(
            ollama,
            (
                Company::Ollama,
                Some(ProviderRun {
                    start: month.start,
                    end: month.end,
                    used: Some(month.used),
                }),
            ),
        );
    }
    let from = running
        .iter()
        .map(|week| week.window.weekly_start())
        .chain(
            provider_runs
                .values()
                .filter_map(|(_, run)| run.as_ref().map(|run| run.start)),
        )
        .fold(calendar_week, DateTime::min);
    let tokens = keep_warnings(ctx.warnings, || {
        ctx.history.read::<TokenRecord>(
            &Range {
                from: from.min(ctx.now),
                to: ctx.now,
            },
            &Hosts::All,
        )
    })?;

    // One pass over the records: the records no subscription can be named for
    // by company, and every counted record by project. Everything is a `Values`
    // sum, so a missing part keeps its reason (R3).
    let mut unassigned: BTreeSet<Company> = BTreeSet::new();
    let mut unassigned_missing: BTreeMap<Company, crate::money::Missing> = BTreeMap::new();
    let mut total: Option<Values> = None;
    let mut tokens_out = 0u64;
    let mut by_project: BTreeMap<ProjectId, ProjectWeek> = BTreeMap::new();
    for hosted in &tokens.records {
        if q.host.as_ref().is_some_and(|host| host != &hosted.host) {
            continue;
        }
        let record = &hosted.record;
        if record.t > ctx.now {
            continue;
        }
        let value = valuer.value(&hosted.host, record);
        match value.company {
            Company::Anthropic => match value.account.as_ref() {
                None => {
                    if record.t < calendar_week {
                        continue;
                    }
                }
                Some(account) => {
                    let Some(week) = running.iter().find(|week| &week.account == account) else {
                        continue;
                    };
                    if record.t < week.window.weekly_start() {
                        continue;
                    }
                }
            },
            company => match value.account.as_ref() {
                None => {
                    if record.t < calendar_week {
                        continue;
                    }
                    if let Some(missing) = &value.subscription_missing {
                        unassigned_missing
                            .entry(company)
                            .or_insert_with(|| missing.clone());
                    }
                    unassigned.insert(company);
                }
                Some(account) => {
                    // An Ollama record counts from the calendar week's start,
                    // the same start as Claude work without a known account, so
                    // its work and its 12/52 paid share of the month cover the
                    // same week (documented rule). An OpenAI record keeps its
                    // running week's start.
                    let start = match company {
                        Company::Ollama => calendar_week,
                        _ => provider_runs
                            .get(account)
                            .and_then(|(_, run)| run.as_ref())
                            .map(|run| run.start)
                            .unwrap_or(calendar_week),
                    };
                    if record.t < start {
                        continue;
                    }
                    provider_runs
                        .entry(account.clone())
                        .or_insert((company, None));
                }
            },
        }
        let project = ctx
            .projects
            .of(&ProjectId::of(&record.project, &hosted.host));
        let record_values = Values::of(std::slice::from_ref(&value), ctx.settings, ctx.fx, ctx.now);
        add_opt_value(&mut total, &record_values);
        tokens_out += record.out;
        let project_row = by_project
            .entry(project.key.clone())
            .or_insert_with(|| ProjectWeek {
                name: project.name.clone(),
                key: project.key.key.clone(),
                tokens_out: 0,
                values: None,
                api_by_subscription: BTreeMap::new(),
            });
        project_row.tokens_out += record.out;
        add_opt_value(&mut project_row.values, &record_values);
        add_value(
            &mut project_row.api_by_subscription,
            subscription_chip(ctx, &value),
            &record_values,
        );
    }

    let mut paid: Option<f64> = None;
    // Whether a listed running window has no price. While at least one window
    // has one, `paid` is a lower bound and `paid_partial` says so (R3); while
    // none has one, `paid` is `None` and there is no partial sum to mark.
    let mut unpriced_window = false;
    let mut accounts = Vec::with_capacity(running.len());
    for week in &running {
        let money = money_of(week)?;
        match money {
            Some(money) => paid = Some(paid.unwrap_or(0.0) + money.paid),
            None => unpriced_window = true,
        }
        accounts.push(ThisWeekAccount {
            id: account_id(ctx, &week.account),
            used: week.used,
            now: week.window.final_percent,
            money: money.map(|money| money.used),
            money_missing: week.price_missing.as_ref().map(ToString::to_string),
            started_at: week.window.weekly_start(),
            resets_at: week.window.end,
        });
    }
    accounts.sort_by(|a, b| {
        account_position(ctx, &a.id)
            .cmp(&account_position(ctx, &b.id))
            .then_with(|| a.id.cmp(&b.id))
    });

    // The rows in the stored subscriptions' order, then the unnamed companies'.
    let mut providers: Vec<(usize, String, ThisWeekProvider)> =
        Vec::with_capacity(provider_runs.len());
    for (account, (company, run)) in &provider_runs {
        let row = provider_row(&valuer, *company, account, run.as_ref());
        match row.week_paid {
            // `paid` is what the recorded work is set against.
            _ if !company.work_recorded() => {}
            Some(week_paid) => paid = Some(paid.unwrap_or(0.0) + week_paid),
            // A provider row without a running window has no week to price.
            None if run.is_some() => unpriced_window = true,
            None => {}
        }
        let name = super::stored_subscription(ctx, account)
            .map(|subscription| subscription.abbr.clone())
            .unwrap_or_else(|| company.name().to_string());
        providers.push((
            account_position(ctx, &account.acc),
            name.clone(),
            ThisWeekProvider {
                name,
                company: *company,
                used: row.used,
                money: row.money,
                money_missing: row.money_missing,
                resets_at: row.resets_at,
            },
        ));
    }
    // A record whose subscription cannot be named is one row per company,
    // named by the company, with the reason its window is unknown (decision
    // P-2 6). It has no running window, so the page's subscription lines skip
    // it; the record still counts in its project and in the totals.
    for company in unassigned {
        let money_missing = unassigned_missing
            .get(&company)
            .map(ToString::to_string)
            .or_else(|| Some(crate::money::Missing::NoAccount.to_string()));
        let name = company.name().to_string();
        providers.push((
            usize::MAX,
            name.clone(),
            ThisWeekProvider {
                name,
                company,
                used: None,
                money: None,
                money_missing,
                resets_at: None,
            },
        ));
    }
    providers.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let providers: Vec<ThisWeekProvider> = providers.into_iter().map(|(_, _, row)| row).collect();

    let used = total.unwrap_or_else(|| Values::sum(&[]));
    let mut projects: Vec<ThisWeekProject> = by_project
        .into_values()
        .map(|project| {
            let values = project.values.unwrap_or_else(|| Values::sum(&[]));
            let by_subscription = subscription_shares(
                ctx,
                &values,
                project
                    .api_by_subscription
                    .iter()
                    .map(|(id, values)| (id.clone(), values)),
            );
            ThisWeekProject {
                name: project.name,
                key: Some(project.key),
                tokens_out: project.tokens_out,
                values,
                by_subscription,
            }
        })
        .collect();
    projects.sort_by(|a, b| {
        match (a.values.subscription, b.values.subscription) {
            (Some(a), Some(b)) => b.total_cmp(&a),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        .then_with(|| b.tokens_out.cmp(&a.tokens_out))
        .then_with(|| a.name.cmp(&b.name))
    });
    // A share is computed only from halves that are not partial (R3): a
    // partial `used.subscription` or a partial `paid` gives no percent, and the
    // page shows the "≥" amount instead.
    let (used_subscription, _) = full_half(
        used.subscription,
        used.subscription_partial,
        used.subscription_missing.as_deref(),
    );
    let paid_partial = paid.is_some() && unpriced_window;
    let used_of_paid = match (used_subscription, paid) {
        (Some(used), Some(paid)) if paid > 0.0 && !paid_partial => Some(used / paid * 100.0),
        _ => None,
    };
    Ok(ThisWeek {
        accounts,
        providers,
        totals: ThisWeekTotals {
            tokens_out,
            projects: projects.len() as u32,
            used,
            paid,
            paid_partial,
            used_of_paid,
        },
        projects,
    })
}

/// One project's sums on the This-week card, built in the card's one pass.
struct ProjectWeek {
    name: String,
    key: ProjectKey,
    tokens_out: u64,
    values: Option<Values>,
    /// Each chip id's API value among the project's records; `None` is work
    /// without a known account or named subscription.
    api_by_subscription: BTreeMap<Option<String>, Values>,
}

/// The chip id a record splits by: a stored Claude account's id
/// ([`account_id`]) or a provider subscription's [`AccountRef::acc`]; `None`
/// for work without a known account or named subscription.
fn subscription_chip<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    value: &crate::money::Valued,
) -> Option<String> {
    let account = value.account.as_ref()?;
    Some(match value.company {
        Company::Anthropic => account_id(ctx, account),
        Company::OpenAI | Company::Ollama | Company::Google => account.acc.clone(),
    })
}

/// A provider subscription's running window on the This-week card.
struct ProviderRun {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    /// The window's use so far; `None` when the readings cannot compute it.
    used: Option<f64>,
}

/// A provider row's four subscription fields and its week price (decision
/// P-2 6, 7): filled like an account row's from the subscription's own running
/// window, and `money_missing` with the reason when it has no price.
/// `week_paid` is what the window's week costs: OpenAI's week price and
/// Ollama's month price × 12 / 52 (the documented rule); `None` without a
/// price. A subscription with no running window gets its own reason, never
/// four `None` fields without one: OpenAI's missing week, Ollama's missing
/// month, or Ollama's reading without a monthly allowance. Never a 0 for an
/// unknown.
struct ProviderRow {
    used: Option<f64>,
    money: Option<f64>,
    money_missing: Option<String>,
    resets_at: Option<DateTime<Utc>>,
    week_paid: Option<f64>,
}

fn provider_row(
    valuer: &Valuer,
    company: Company,
    account: &AccountRef,
    run: Option<&ProviderRun>,
) -> ProviderRow {
    let Some(run) = run else {
        let missing = valuer.no_reading(account);
        return ProviderRow {
            used: None,
            money: None,
            money_missing: Some(missing.to_string()),
            resets_at: None,
            week_paid: None,
        };
    };
    match valuer.window_price(account, run.start, run.end) {
        Ok(price) => {
            let money = match (company, run.used) {
                // One percent of Ollama's month is a hundredth of the month's
                // price; one percent of a week is a week's share of it.
                (_, None) => None,
                (Company::Ollama, Some(used)) => Some(crate::money::month_money(price, used).used),
                (_, Some(used)) => Some(crate::money::window_money(price, used).used),
            };
            // The `paid` of a week is its monthly price × 12 / 52; an Ollama
            // month counts the same share of its month price.
            let week_paid = crate::money::window_money(price, 100.0).paid;
            ProviderRow {
                used: run.used,
                money,
                money_missing: None,
                resets_at: Some(run.end),
                week_paid: Some(week_paid),
            }
        }
        Err(missing) => ProviderRow {
            used: run.used,
            money: None,
            money_missing: Some(missing.to_string()),
            resets_at: Some(run.end),
            week_paid: None,
        },
    }
}

/// The range the token and sample reads cover: the range itself, the 14 days
/// the rates are learned over before its end, and every account week's windows.
fn reading_range(range: &Range, weeks_so_far: &[AccountWeek]) -> Range {
    let mut span = Range {
        from: range.from.min(range.to - Duration::days(RATES_DAYS)),
        to: range.to,
    };
    for week in weeks_so_far {
        let week = week_span(week);
        span.from = span.from.min(week.from);
        span.to = span.to.max(week.to);
    }
    span
}

/// The hours blocked in the calendar weeks before the week the last account
/// added in the range was added, and that account's short name. The week it was added
/// is not counted: the page's hours are counted per calendar week, not split at
/// the moment of the addition. `None` when the range holds no `AccountAdded`
/// of a stored account.
fn blocked_before_added<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    events: &[Hosted<Event>],
    blocked_weeks: &BTreeMap<DateTime<Utc>, f64>,
) -> Option<BlockedBefore> {
    let (at, reference) = last_account_added(ctx, range, events)?;
    let added_week = week_start(at, &ctx.zone);
    let hours = blocked_weeks
        .iter()
        .filter(|(week, _)| **week < added_week)
        .map(|(_, hours)| *hours)
        .sum();
    Some(BlockedBefore {
        name: account_abbr(ctx, &reference),
        hours,
    })
}

/// The latest `AccountAdded` event in the range of an account the settings
/// still store; ties go to the larger [`AccountRef`], so the choice does not
/// depend on the order of the history's files.
fn last_account_added<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    events: &[Hosted<Event>],
) -> Option<(DateTime<Utc>, AccountRef)> {
    let mut latest: Option<(DateTime<Utc>, AccountRef)> = None;
    for hosted in events {
        let at = hosted.record.t;
        if at < range.from || at >= range.to {
            continue;
        }
        let EventKind::AccountAdded { account, .. } = &hosted.record.kind else {
            continue;
        };
        if !ctx
            .accounts
            .iter()
            .any(|stored| AccountRef::of(stored) == *account)
        {
            continue;
        }
        let candidate = (at, account.clone());
        if latest.as_ref().is_none_or(|current| candidate > *current) {
            latest = Some(candidate);
        }
    }
    latest
}

/// The chart's marks: one per calendar week that holds an `AccountAdded` or
/// `AutoBurnSet` event of the range, at the week's first such event. The chart
/// puts a mark at its week's column, so two marks of one week would print on
/// top of each other; one mark names them all instead: the accounts added, in
/// time order, as `PR, AC added`, then each auto-burn switch, joined by ` · `.
fn week_marks<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    events: &[Hosted<Event>],
) -> Vec<Mark> {
    let mut chosen: Vec<&Event> = events
        .iter()
        .map(|hosted| &hosted.record)
        .filter(|event| range.from <= event.t && event.t < range.to)
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::AccountAdded { .. } | EventKind::AutoBurnSet { .. }
            )
        })
        .collect();
    chosen.sort_by_key(|event| event.t);
    /// One week's marked events: its first event's time, the accounts added
    /// and the auto-burn switches, each in time order.
    struct WeekEvents {
        at: DateTime<Utc>,
        added: Vec<String>,
        auto_burn: Vec<&'static str>,
    }
    let mut weeks: BTreeMap<DateTime<Utc>, WeekEvents> = BTreeMap::new();
    for event in chosen {
        let week = weeks
            .entry(week_start(event.t, &ctx.zone))
            .or_insert_with(|| WeekEvents {
                at: event.t,
                added: Vec::new(),
                auto_burn: Vec::new(),
            });
        match &event.kind {
            EventKind::AccountAdded { account, .. } => {
                let name = account_abbr(ctx, account);
                if !week.added.contains(&name) {
                    week.added.push(name);
                }
            }
            EventKind::AutoBurnSet { on } => {
                week.auto_burn
                    .push(if *on { "auto-burn on" } else { "auto-burn off" });
            }
            _ => {}
        }
    }
    weeks
        .into_values()
        .map(|week| {
            let mut parts: Vec<String> = Vec::new();
            if !week.added.is_empty() {
                parts.push(format!("{} added", week.added.join(", ")));
            }
            parts.extend(week.auto_burn.into_iter().map(str::to_string));
            Mark {
                at: week.at,
                label: parts.join(" · "),
            }
        })
        .collect()
}

/// A week with no accounts yet; `running` is true for the calendar week that
/// holds now.
fn empty_week(week: DateTime<Utc>, this_week: DateTime<Utc>) -> OverviewWeek {
    OverviewWeek {
        week,
        accounts: Vec::new(),
        projects: Vec::new(),
        blocked_hours: 0.0,
        expired_share: None,
        running: week == this_week,
    }
}

/// The measured cost of one switch at the range's end (spec §2.13): the mean
/// over the switches of the range, times `s` of the account in use at the end
/// on this host. The rule is the Settings page's (`cost.usd × s`, the 5 h
/// factor's value).
fn switch_cost_percent<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    events: &Read<Event>,
    samples: &Read<Sample>,
    tokens: &Read<TokenRecord>,
) -> Option<f64> {
    let rate_accounts: Vec<rates::RateAccount> =
        ctx.accounts.iter().map(rates::RateAccount::of).collect();
    let learned: BTreeMap<AccountRef, Rates> = rates::rates(
        &rate_accounts,
        &samples.records,
        &tokens.records,
        &events.records,
        ctx.prices,
        range.to,
    )
    .rates;
    let cost = switch_cost(
        &events.records,
        &samples.records,
        &tokens.records,
        &learned,
        ctx.prices,
        range,
    )?;
    let timelines = rates::timelines(&events.records);
    let active = timelines.get(ctx.history.host())?.at(range.to)?;
    let window = learned.get(active)?.s?.value;
    Some(cost.usd * window)
}

/// The first finding: the account with the most expired money, if above zero;
/// no money text without a currency.
fn finding_expired<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    kpis: &Kpis,
    priced_weeks: &BTreeMap<AccountRef, u32>,
    currency: Option<&str>,
) -> Option<String> {
    let currency = currency?;
    let (id, expired) = kpis.expired_by_account.first()?;
    if *expired <= 0.0 {
        return None;
    }
    let account = ctx.accounts.iter().find(|account| &account.id == id)?;
    let weeks = priced_weeks
        .get(&AccountRef::of(account))
        .copied()
        .unwrap_or(0);
    let noun = if weeks == 1 { "week" } else { "weeks" };
    Some(format!(
        "{} is where most money expired: {} in {weeks} {noun}.",
        account.abbr,
        money_text(*expired, currency)
    ))
}

/// The second finding: the blocked time, over the weeks the page shows. It
/// speaks of Claude's accounts alone: auto-burn and the finding are
/// Anthropic's, and no company's hours are added to another's (decision P-2 5).
fn finding_blocked(kpis: &Kpis, weeks: &[OverviewWeek]) -> Option<String> {
    // No Anthropic entry means no Anthropic blocked hours in the range: there
    // is no finding, not a finding of 0 h.
    let blocked = kpis
        .blocked_by_company
        .iter()
        .find(|entry| entry.company == Company::Anthropic)
        .map(|entry| entry.hours)?;
    if blocked <= 0.0 || weeks.is_empty() {
        return None;
    }
    let noun = if weeks.len() == 1 { "week" } else { "weeks" };
    Some(format!(
        "You were blocked {} in {} {noun}; {} of them while other quota expired.",
        hours_text(blocked),
        weeks.len(),
        hours_text(kpis.avoidable_hours)
    ))
}

/// The third finding: since auto-burn was switched on and stayed on, with a
/// full week on each side. Its expired share needs priced weeks, not a
/// currency of its own.
fn finding_auto_burn<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    events: &[Hosted<Event>],
    weeks: &[OverviewWeek],
    money: &BTreeMap<DateTime<Utc>, WindowMoney>,
    offered_finished_by_week: &BTreeMap<DateTime<Utc>, f64>,
) -> Option<String> {
    let at = auto_burn_start(range, events)?;
    let before: Vec<&OverviewWeek> = weeks
        .iter()
        .filter(|week| {
            week.week
                .checked_add_signed(Duration::weeks(1))
                .is_some_and(|end| end <= at)
        })
        .collect();
    let after: Vec<&OverviewWeek> = weeks.iter().filter(|week| week.week >= at).collect();
    let (blocked_before, expired_before) = week_averages(&before, money, offered_finished_by_week)?;
    let (blocked_after, expired_after) = week_averages(&after, money, offered_finished_by_week)?;
    Some(format!(
        "Since auto-burn ({}): {} blocked a week, {} expired. Before: {}, {}.",
        date_text(at, &ctx.zone),
        hours_text(blocked_after),
        percent_text(expired_after),
        hours_text(blocked_before),
        percent_text(expired_before)
    ))
}

/// The latest `AutoBurnSet { on: true }` in the range that has a full week
/// before and after it and no `AutoBurnSet { on: false }` after it.
fn auto_burn_start(range: &Range, events: &[Hosted<Event>]) -> Option<DateTime<Utc>> {
    events.iter().rev().find_map(|hosted| {
        let at = hosted.record.t;
        if at < range.from || at >= range.to {
            return None;
        }
        let EventKind::AutoBurnSet { on: true } = &hosted.record.kind else {
            return None;
        };
        let stays_on = !events.iter().any(|later| {
            later.record.t > at
                && later.record.t < range.to
                && matches!(later.record.kind, EventKind::AutoBurnSet { on: false })
        });
        let week_before = at
            .checked_sub_signed(Duration::weeks(1))
            .is_some_and(|start| start >= range.from);
        let week_after = at
            .checked_add_signed(Duration::weeks(1))
            .is_some_and(|end| end <= range.to);
        (stays_on && week_before && week_after).then_some(at)
    })
}

/// The blocked hours a week and the expired share of some weeks; `None` when a
/// week list is empty or none of its weeks has a finished, priced window. The
/// share divides by the finished windows' offer, as the page's Expired KPI and
/// the Weeks page do; a running window has expired nothing yet, so its offer is
/// not in the denominator either.
fn week_averages(
    weeks: &[&OverviewWeek],
    money: &BTreeMap<DateTime<Utc>, WindowMoney>,
    offered_finished_by_week: &BTreeMap<DateTime<Utc>, f64>,
) -> Option<(f64, f64)> {
    if weeks.is_empty() {
        return None;
    }
    let mut blocked = 0.0;
    let mut offered = 0.0;
    let mut expired = 0.0;
    for week in weeks {
        blocked += week.blocked_hours;
        if let Some(week_money) = money.get(&week.week) {
            expired += week_money.expired;
        }
        if let Some(value) = offered_finished_by_week.get(&week.week) {
            offered += value;
        }
    }
    if offered <= 0.0 {
        return None;
    }
    Some((blocked / weeks.len() as f64, expired / offered * 100.0))
}

/// 4. The account with the most full weeks.
fn finding_full<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, finished: &[AccountWeek]) -> Option<String> {
    let mut fullest: Option<(&crate::store::Account, u32, u32)> = None;
    for account in ctx.accounts {
        let reference = AccountRef::of(account);
        let total = finished
            .iter()
            .filter(|week| week.account == reference)
            .count() as u32;
        let full = finished
            .iter()
            .filter(|week| week.account == reference && week.used >= 100.0)
            .count() as u32;
        if full == 0 {
            continue;
        }
        if fullest.is_none_or(|(_, best, _)| full > best) {
            fullest = Some((account, full, total));
        }
    }
    let (account, full, total) = fullest?;
    let noun = if total == 1 { "week" } else { "weeks" };
    Some(format!(
        "{} was full in {full} of {total} {noun}.",
        account.abbr
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::history::{HistoryDir, Source};
    use crate::money::percent_value;
    use crate::stats::tests::{
        all, append_codex_records, codex_prices, codex_ref, codex_token, provider_subscription,
        tiny, utc,
    };
    use crate::stats::RangeKey;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.001
    }

    /// The plan's range: 2 Nov .. 16 Nov.
    fn two_weeks() -> Range {
        Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        }
    }

    fn week<'a>(page: &'a Overview, start: &str) -> &'a OverviewWeek {
        page.weeks
            .iter()
            .find(|week| week.week == utc(start))
            .unwrap_or_else(|| panic!("the week of {start}"))
    }

    /// One company's blocked hours as the page's KPI shows them.
    fn blocked(page: &Overview, company: Company) -> f64 {
        page.kpis
            .blocked_by_company
            .iter()
            .find(|entry| entry.company == company)
            .map(|entry| entry.hours)
            .unwrap_or(0.0)
    }

    #[test]
    fn the_totals() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        assert!(close(page.kpis.used, 120.000), "used {}", page.kpis.used);
        assert!(close(page.kpis.paid, 147.692), "paid {}", page.kpis.paid);
        assert!(
            close(page.kpis.expired, 27.692),
            "expired {}",
            page.kpis.expired
        );
        assert_eq!(
            page.kpis.blocked_by_company,
            vec![BlockedByCompany {
                company: Company::Anthropic,
                hours: 2.0,
            }],
            "one entry per company with working hours, Anthropic first"
        );
        assert_eq!(page.kpis.avoidable_hours, 2.0);
        assert_eq!(page.kpis.switches.user, 1);
        assert_eq!(page.kpis.switches.cli, 0);
        assert_eq!(page.kpis.switches.auto, 1);
        assert_eq!(page.kpis.switches.agent, 1);
        assert_eq!(page.kpis.switches.external, 0);
        assert!(page.without_price.is_empty());
    }

    #[test]
    fn expired_by_account() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        let split = &page.kpis.expired_by_account;
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].0, tiny.ac().id);
        assert!(close(split[0].1, 19.385), "AC {}", split[0].1);
        assert_eq!(split[1].0, tiny.lb().id);
        assert!(close(split[1].1, 8.308), "LB {}", split[1].1);
    }

    #[test]
    fn per_week() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        let first = week(&page, "2026-11-02T00:00:00Z");
        assert!(
            close(first.expired_share.unwrap(), 21.875),
            "W1 share {:?}",
            first.expired_share
        );
        let lb = first
            .accounts
            .iter()
            .find(|account| account.id == tiny.lb().id)
            .expect("LB's W1");
        assert_eq!(lb.used_percent, 100.0);
        let ac = first
            .accounts
            .iter()
            .find(|account| account.id == tiny.ac().id)
            .expect("AC's W1");
        assert_eq!(ac.used_percent, 50.0);

        let second = week(&page, "2026-11-09T00:00:00Z");
        assert!(
            close(second.expired_share.unwrap(), 15.625),
            "W2 share {:?}",
            second.expired_share
        );
        assert_eq!(first.blocked_hours, 2.0);
        assert_eq!(second.blocked_hours, 0.0);
    }

    #[test]
    fn projects_per_week() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        let first = &week(&page, "2026-11-02T00:00:00Z").projects;
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].0, "atlas");
        assert!(close(first[0].1, 41.538), "atlas W1 {}", first[0].1);
        assert_eq!(first[1].0, "acme-kb");
        assert!(close(first[1].1, 16.154), "acme-kb W1 {}", first[1].1);

        let second = &week(&page, "2026-11-09T00:00:00Z").projects;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].0, "atlas");
        assert!(close(second[0].1, 62.308), "atlas W2 {}", second[0].1);
    }

    /// A token in the first window of a week that a reset split in two is
    /// attributed to that week: the token of 3 Nov lies in the 2 Nov .. 5 Nov
    /// 12:00 window, and its subscription value is the week's Atlas bar.
    #[test]
    fn a_token_in_the_first_window_of_a_reset_week_is_counted() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&Event {
                t: utc("2026-11-02T08:00:00Z"),
                kind: EventKind::Live {
                    account: Some(tiny.lb_ref()),
                },
            })
            .expect("LB live");
        history
            .append_many(&[
                WindowRecord {
                    account: tiny.lb_ref(),
                    k: crate::api::LimitKind::WeeklyAll,
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
                },
                WindowRecord {
                    account: tiny.lb_ref(),
                    k: crate::api::LimitKind::WeeklyAll,
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
                },
            ])
            .expect("the two windows of the reset week");
        history
            .append(&TokenRecord {
                t: utc("2026-11-03T10:00:00Z"),
                minutes: 60,
                session: "s0".to_string(),
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
            .expect("the token in the first window");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        // The range starts after the token and before the first window's
        // midpoint; the whole week counts, so the token counts too.
        let page = overview_in(
            &ctx,
            &Range {
                from: utc("2026-11-03T12:00:00Z"),
                to: utc("2026-11-21T00:00:00Z"),
            },
            &all(),
        )
        .unwrap();
        let first = week(&page, "2026-11-02T00:00:00Z");
        assert_eq!(
            first.projects.len(),
            1,
            "the token of 3 Nov is attributed to the week: {:?}",
            first.projects
        );
        assert_eq!(first.projects[0].0, "atlas");
        assert!(
            close(first.projects[0].1, 41.538_461_5 * 0.62),
            "the token's share of the first window's 62 %: {}",
            first.projects[0].1
        );
    }

    /// The week's project bars follow the host filter: vega has none of atlas's
    /// records, so its week has no project bar.
    #[test]
    fn projects_per_week_follow_the_host_filter() {
        let tiny = tiny();
        let query = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: Some("vega".to_string()),
        };

        let page = overview_in(&tiny.ctx(), &two_weeks(), &query).unwrap();

        for week in &page.weeks {
            assert!(
                week.projects.is_empty(),
                "vega has no project bar: {:#?}",
                week.projects
            );
        }
    }

    #[test]
    fn findings_are_worded_exactly() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        assert!(
            page.findings
                .iter()
                .any(|text| text == "AC is where most money expired: 19 EUR in 2 weeks."),
            "findings: {:#?}",
            page.findings
        );
        assert!(
            page.findings
                .iter()
                .any(|text| text == "LB was full in 1 of 2 weeks."),
            "findings: {:#?}",
            page.findings
        );
        assert!(
            page.findings.iter().any(|text| text
                == "You were blocked 2h in 2 weeks; 2h of them while other quota expired."),
            "findings: {:#?}",
            page.findings
        );
        assert_eq!(page.findings.len(), 3, "findings: {:#?}", page.findings);
    }

    /// A sentence that counts one week says "week", not "weeks".
    #[test]
    fn a_sentence_counting_one_week_is_singular() {
        let tiny = tiny();
        let one_week = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-09T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &one_week, &all()).unwrap();

        assert!(
            page.findings
                .iter()
                .any(|text| text == "AC is where most money expired: 16 EUR in 1 week."),
            "findings: {:#?}",
            page.findings
        );
        assert!(
            page.findings
                .iter()
                .any(|text| text == "LB was full in 1 of 1 week."),
            "findings: {:#?}",
            page.findings
        );
    }

    #[test]
    fn an_account_without_a_price_is_left_out_and_named() {
        let mut tiny = tiny();
        let ac_id = tiny.ac().id.clone();
        tiny.accounts[1].price = None;

        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        assert!(close(page.kpis.used, 74.769), "used {}", page.kpis.used);
        assert_eq!(page.without_price, vec![ac_id]);
        let first = week(&page, "2026-11-02T00:00:00Z");
        let ac = first
            .accounts
            .iter()
            .find(|account| account.id == tiny.ac().id)
            .expect("AC's W1");
        assert_eq!(ac.used, None);
        assert_eq!(ac.expired, None);
    }

    #[test]
    fn marks_name_added_accounts() {
        let tiny = tiny();
        tiny.history
            .append(&Event {
                t: utc("2026-11-05T00:00:00Z"),
                kind: EventKind::AccountAdded {
                    account: tiny.ac_ref(),
                    plan: Some("Team Premium".to_string()),
                },
            })
            .expect("AC added");

        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        let mark = page
            .marks
            .iter()
            .find(|mark| mark.label == "AC added")
            .expect("AC's mark");
        assert_eq!(mark.at, utc("2026-11-05T00:00:00Z"));
    }

    /// The fixture's running weekly windows on the This-week card: LB at 30 %
    /// and AC at 40 %, each from its start to its reset, with the money of its
    /// use so far at its price. No token record lies in them yet, so the card
    /// has no project row.
    #[test]
    fn the_card_lists_the_running_windows() {
        let tiny = tiny();
        let card = this_week_card(&tiny.ctx(), &all()).unwrap();

        let money: Vec<f64> = card
            .accounts
            .iter()
            .map(|account| account.money.expect("a priced window"))
            .collect();
        assert_eq!(money.len(), 2);
        assert!(
            close(money[0], 0.30 * 180.0 * 12.0 / 52.0),
            "LB {}",
            money[0]
        );
        assert!(
            close(money[1], 0.40 * 140.0 * 12.0 / 52.0),
            "AC {}",
            money[1]
        );
        let without_money: Vec<ThisWeekAccount> = card
            .accounts
            .iter()
            .map(|account| ThisWeekAccount {
                money: None,
                ..account.clone()
            })
            .collect();
        assert_eq!(
            without_money,
            vec![
                ThisWeekAccount {
                    id: tiny.lb().id.clone(),
                    used: 30.0,
                    now: 30.0,
                    money: None,
                    money_missing: None,
                    started_at: utc("2026-11-16T17:00:00Z"),
                    resets_at: utc("2026-11-23T17:00:00Z"),
                },
                ThisWeekAccount {
                    id: tiny.ac().id.clone(),
                    used: 40.0,
                    now: 40.0,
                    money: None,
                    money_missing: None,
                    started_at: utc("2026-11-16T02:00:00Z"),
                    resets_at: utc("2026-11-23T02:00:00Z"),
                },
            ]
        );
        assert!(card.projects.is_empty());
        assert_eq!(card.totals.tokens_out, 0);
        assert_eq!(card.totals.projects, 0);
    }

    /// The card ignores the range: a query for last week or for the last
    /// twelve months still lists the windows running now.
    #[test]
    fn the_card_ignores_the_range() {
        let tiny = tiny();
        for range in [RangeKey::LastWeek, RangeKey::Months12] {
            let query = Query { range, ..all() };
            let card = this_week_card(&tiny.ctx(), &query).unwrap();
            let listed: Vec<(&str, f64)> = card
                .accounts
                .iter()
                .map(|account| (account.id.as_str(), account.used))
                .collect();
            assert_eq!(
                listed,
                vec![(tiny.lb().id.as_str(), 30.0), (tiny.ac().id.as_str(), 40.0)],
                "the card of the range {range:?}"
            );
        }
    }

    /// The history counts the running weeks: the calendar week that holds now
    /// has LB's and AC's running windows as account weeks, with the use so far
    /// and nothing expired (R5). This is the 1.3 test
    /// `the_history_does_not_count_a_running_window` under the new rule.
    #[test]
    fn the_history_counts_a_running_window() {
        let tiny = tiny();
        let this_week = Range {
            from: utc("2026-11-16T00:00:00Z"),
            to: tiny.now,
        };
        let page = overview_in(&tiny.ctx(), &this_week, &all()).unwrap();

        let lb_week = 180.0 * 12.0 / 52.0;
        let ac_week = 140.0 * 12.0 / 52.0;
        assert!(
            close(page.kpis.used, 0.30 * lb_week + 0.40 * ac_week),
            "the use so far of both windows: {}",
            page.kpis.used
        );
        assert!(
            close(page.kpis.paid, lb_week + ac_week),
            "each running window offers its week: {}",
            page.kpis.paid
        );
        assert_eq!(page.kpis.expired, 0.0, "a running window expires nothing");
        assert!(
            close(page.kpis.used_percent.expect("priced weeks"), 34.375),
            "used share {:?}",
            page.kpis.used_percent
        );
        // Both windows run and neither has expired anything, so no finished
        // window has offered money: the Expired share is `None`, not 0 (the
        // Weeks page's rule). The Used share still divides by both running
        // windows' week price, as `34.375` above shows.
        assert_eq!(
            page.kpis.expired_percent, None,
            "no finished window offers anything: {:?}",
            page.kpis.expired_percent
        );
        assert_eq!(page.weeks.len(), 1);
        let holding = week(&page, "2026-11-16T00:00:00Z");
        assert!(holding.running, "the week that holds now is 'so far'");
        assert_eq!(
            holding.expired_share, None,
            "the week's only windows run, so it has no expired share: {:?}",
            holding.expired_share
        );
        assert_eq!(holding.accounts.len(), 2, "{:?}", holding.accounts);
        assert!(page.without_price.is_empty());
        assert!(
            page.findings
                .iter()
                .all(|text| !text.contains("full in") && !text.contains("money expired")),
            "no finding calls the running use expired: {:#?}",
            page.findings
        );
    }

    /// The figures row counts the running windows (R5): LB's and AC's weekly
    /// windows still running at `ctx.now` are the holding week's account weeks
    /// with their use so far; Expired is the same as it was without them,
    /// because a running window expires nothing. This test names the fixture's
    /// two accounts, LB and AC.
    #[test]
    fn the_figures_row_counts_running_windows() {
        let tiny = tiny();
        let this_week = Range {
            from: utc("2026-11-16T00:00:00Z"),
            to: tiny.now,
        };
        // The same finished windows without today's readings: no window runs.
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append_many(&tiny.windows)
            .expect("the finished windows");
        let without_running = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let before = overview_in(&without_running, &this_week, &all()).unwrap();
        let page = overview_in(&tiny.ctx(), &this_week, &all()).unwrap();

        let lb_week = 180.0 * 12.0 / 52.0;
        let ac_week = 140.0 * 12.0 / 52.0;
        assert_eq!(before.kpis.used, 0.0, "no window runs in that history");
        assert!(
            close(page.kpis.used, 0.30 * lb_week + 0.40 * ac_week),
            "the running windows' use so far: {}",
            page.kpis.used
        );
        assert!(
            close(page.kpis.paid, lb_week + ac_week),
            "each running window offers its week: {}",
            page.kpis.paid
        );
        assert!(
            close(page.kpis.expired, before.kpis.expired),
            "Expired does not change because of a running window: {} vs {}",
            page.kpis.expired,
            before.kpis.expired
        );
        let holding = week(&page, "2026-11-16T00:00:00Z");
        assert!(holding.running, "the week that holds now is 'so far'");
        assert_eq!(holding.accounts.len(), 2, "{:?}", holding.accounts);
    }

    /// The card's project rows sort by the known part of the subscription
    /// value, largest first, ties by output tokens, then by name. In AC's
    /// running window atlas uses 60 USD of the 100 USD of API value, ledger and
    /// acme-kb 20 USD each: the tie goes to ledger's 4 M output tokens before
    /// acme-kb's 2 M, though `acme-kb` sorts first by name.
    #[test]
    fn this_week_projects_are_sorted_by_subscription_value_then_tokens() {
        let mut tiny = tiny();
        for (model, output) in [
            ("claude-opus-5-5", 20.0),
            ("claude-haiku-5-5", 5.0),
            ("claude-sonnet-5-5", 10.0),
        ] {
            tiny.prices.models.insert(
                crate::money::price_key("anthropic", model),
                vec![crate::money::PricePoint {
                    from: utc("2026-01-01T00:00:00Z"),
                    price: crate::money::ModelPrice {
                        input: 0.0,
                        output,
                        cache_write: 0.0,
                        cache_read: 0.0,
                        tiered: false,
                    },
                }],
            );
        }
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        let ledger = crate::project::ProjectKey::Repo("github.com/sam/ledger".to_string());
        let acme_kb = crate::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string());
        tiny.history
            .append_many(&[
                week_token("2026-11-17T09:00:00Z", &atlas, "claude-opus-5-5", 3_000_000),
                week_token(
                    "2026-11-17T09:10:00Z",
                    &ledger,
                    "claude-haiku-5-5",
                    4_000_000,
                ),
                week_token(
                    "2026-11-17T09:20:00Z",
                    &acme_kb,
                    "claude-sonnet-5-5",
                    2_000_000,
                ),
            ])
            .expect("AC's records of this week");

        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        let names: Vec<&str> = card
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["atlas", "ledger", "acme-kb"],
            "the card's order: {:?}",
            card.projects
        );
        let ac_used = 0.40 * 140.0 * 12.0 / 52.0;
        let subscriptions: Vec<f64> = card
            .projects
            .iter()
            .map(|project| project.values.subscription.expect("a known value"))
            .collect();
        assert!(
            close(subscriptions[0], 0.60 * ac_used),
            "atlas {:?}",
            subscriptions[0]
        );
        assert!(
            close(subscriptions[1], 0.20 * ac_used),
            "ledger {:?}",
            subscriptions[1]
        );
        assert!(
            close(subscriptions[2], 0.20 * ac_used),
            "acme-kb {:?}",
            subscriptions[2]
        );
        assert_eq!(card.projects[1].tokens_out, 4_000_000);
        assert_eq!(card.projects[2].tokens_out, 2_000_000);
        assert_eq!(card.totals.projects, 3);
    }

    /// An Ollama month of 20 EUR counts as 20 × 12 / 52 = 4.6154 EUR in `paid`
    /// (the documented rule), even though the month has not ended.
    #[test]
    fn this_week_paid_counts_a_month_price_as_twelve_fifty_seconds() {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![provider_subscription(&ollama, "OL", Some(20.0))];
        tiny.history
            .append(&Sample {
                t: utc("2026-11-10T00:00:00Z"),
                account: ollama,
                src: crate::history::Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::Other("monthly_credits".to_string()),
                    m: None,
                    p: 60.0,
                    r: Some(utc("2026-12-01T00:00:00Z")),
                    b: false,
                }],
            })
            .expect("November's reading");
        // No Claude account is chosen, so the card lists the Ollama month alone.
        let query = Query {
            range: RangeKey::All,
            accounts: vec!["no such account".to_string()],
            host: None,
        };

        let card = this_week_card(&tiny.ctx(), &query).unwrap();
        assert!(card.accounts.is_empty(), "{:?}", card.accounts);
        let paid = card.totals.paid.expect("the month has a price");
        assert_eq!(
            (paid * 10_000.0).round() / 10_000.0,
            4.6154,
            "20 EUR a month is 4.6154 EUR a week: {paid}"
        );
    }

    /// An Ollama record counts on the This-week card from the calendar week's
    /// start, the same start as Claude work without a known account, so its work
    /// and its paid share (12/52 of the month price) cover the same week. A
    /// record of an earlier calendar week in the same Ollama month is not on the
    /// card.
    #[test]
    fn proof_z2_this_week_counts_an_ollama_month_against_one_week_paid() {
        let mut tiny = tiny();
        price_flash_and_codex(&mut tiny);
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![provider_subscription(&ollama, "OL", Some(20.0))];
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        tiny.history
            .append(&Sample {
                t: utc("2026-11-18T00:00:00Z"),
                account: ollama,
                src: Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::Other("monthly_credits".to_string()),
                    m: None,
                    p: 60.0,
                    r: Some(utc("2026-12-01T00:00:00Z")),
                    b: false,
                }],
            })
            .expect("November's reading");
        tiny.history
            .append_many(&[week_token(
                "2026-11-05T09:00:00Z",
                &atlas,
                "deepseek-v4.1-flash:cloud",
                1_000_000,
            )])
            .expect("a record of 5 Nov");
        let query = Query {
            range: RangeKey::All,
            accounts: vec!["no such account".to_string()],
            host: None,
        };
        let card = this_week_card(&tiny.ctx(), &query).unwrap();
        assert_eq!(
            card.totals.tokens_out, 0,
            "a record of 5 Nov is on this week's card: {:#?}",
            card.totals
        );
    }

    /// The positive half of the proof above: the same Ollama subscription counts
    /// a record inside the current calendar week, with its month percent and
    /// money on the subscription line as before.
    #[test]
    fn this_week_counts_an_ollama_record_inside_the_calendar_week() {
        let mut tiny = tiny();
        price_flash_and_codex(&mut tiny);
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![provider_subscription(&ollama, "OL", Some(20.0))];
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        tiny.history
            .append(&Sample {
                t: utc("2026-11-18T00:00:00Z"),
                account: ollama,
                src: Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::Other("monthly_credits".to_string()),
                    m: None,
                    p: 60.0,
                    r: Some(utc("2026-12-01T00:00:00Z")),
                    b: false,
                }],
            })
            .expect("November's reading");
        tiny.history
            .append_many(&[week_token(
                "2026-11-18T09:00:00Z",
                &atlas,
                "deepseek-v4.1-flash:cloud",
                1_000_000,
            )])
            .expect("a record of this week");
        let query = Query {
            range: RangeKey::All,
            accounts: vec!["no such account".to_string()],
            host: None,
        };
        let card = this_week_card(&tiny.ctx(), &query).unwrap();
        assert_eq!(card.totals.tokens_out, 1_000_000);
        assert_eq!(card.projects.len(), 1, "{:#?}", card.projects);
        assert_eq!(card.projects[0].name, "atlas");
        assert_eq!(card.projects[0].values.api, Some(2.0));
        let row = card
            .providers
            .iter()
            .find(|row| row.name == "OL")
            .expect("the Ollama subscription line");
        assert_eq!(row.used, Some(60.0), "the month's use so far");
        assert!(close(row.money.expect("60 % of 20 EUR"), 12.0));
        assert!(close(
            card.totals.paid.expect("a week's share"),
            4.615_384_6
        ));
    }

    /// A running window without a price makes `paid` a lower bound: AC's
    /// running week is priced and OA's running week is not, so `paid` holds AC's
    /// week only and `paid_partial` says one listed window is missing. A percent
    /// over a partial denominator is no figure, so `used_of_paid` is `None`
    /// (R3).
    #[test]
    fn this_week_paid_is_partial_when_a_listed_window_has_no_price() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        for (account, percent, reset) in [
            (tiny.ac_ref(), 40.0, "2026-11-23T02:00:00Z"),
            (codex_ref(), 12.0, "2026-11-24T10:00:00Z"),
        ] {
            history
                .append(&Sample {
                    t: tiny.now,
                    account,
                    src: Source::Poll,
                    l: vec![crate::history::LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: percent,
                        r: Some(utc(reset)),
                        b: true,
                    }],
                })
                .expect("a running week");
        }
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let card = this_week_card(&ctx, &all()).unwrap();
        assert_eq!(
            card.accounts.len(),
            1,
            "AC alone has a Claude window: {:?}",
            card.accounts
        );
        assert_eq!(card.providers.len(), 1, "the OA row: {:?}", card.providers);
        assert_eq!(card.providers[0].name, "OpenAI");
        let ac_week = 140.0 * 12.0 / 52.0;
        assert!(
            close(card.totals.paid.expect("AC's week"), ac_week),
            "paid holds the priced window only: {:?}",
            card.totals.paid
        );
        assert!(card.totals.paid_partial, "OA's running week has no price");
        assert_eq!(
            card.totals.used_of_paid, None,
            "a percent over a partial denominator is no figure"
        );
    }

    /// A project with one priced record and one record whose model has no price
    /// has a partial API half, but a share of an API value counts the priced work
    /// only: the project's `by_subscription` is its priced record's split, AC 100 %
    /// here, not the empty list a partial half used to give.
    #[test]
    fn a_project_with_an_unpriced_record_splits_its_priced_work() {
        let tiny = tiny();
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        tiny.history
            .append_many(&[
                week_token("2026-11-17T09:00:00Z", &atlas, "claude-opus-5-5", 1_000_000),
                week_token(
                    "2026-11-17T10:00:00Z",
                    &atlas,
                    "claude-unpriced-5",
                    1_000_000,
                ),
            ])
            .expect("AC's priced and unpriced records of this week");

        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        assert_eq!(card.projects.len(), 1, "all: {:?}", card.projects);
        let project = &card.projects[0];
        assert_eq!(project.name, "atlas");
        assert_eq!(
            project.values.api,
            Some(20.0),
            "the priced record's known part"
        );
        assert!(
            project.values.api_partial,
            "the unpriced record leaves the API half a lower bound"
        );
        assert_eq!(
            project.by_subscription,
            vec![(Some(tiny.ac().id.clone()), 100.0)],
            "the priced record's split is all of the priced work: {:?}",
            project.by_subscription
        );
    }

    /// Every counted record is in exactly one project row, so the projects'
    /// two halves add up to the card's `used` total.
    #[test]
    fn this_week_project_values_add_up_to_the_used_total() {
        let tiny = tiny();
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        let acme_kb = crate::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string());
        tiny.history
            .append_many(&[
                week_token("2026-11-17T09:00:00Z", &atlas, "claude-opus-5-5", 3_000_000),
                week_token(
                    "2026-11-17T10:00:00Z",
                    &acme_kb,
                    "claude-opus-5-5",
                    1_000_000,
                ),
            ])
            .expect("AC's records of this week");

        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        let parts: Vec<&Values> = card
            .projects
            .iter()
            .map(|project| &project.values)
            .collect();
        let sum = Values::sum(&parts);
        assert_eq!(sum.api, card.totals.used.api, "the API halves");
        assert_eq!(
            sum.subscription, card.totals.used.subscription,
            "the subscription halves"
        );
        assert_eq!(sum, card.totals.used, "the whole values");
        assert_eq!(card.totals.tokens_out, 4_000_000);
    }

    /// A Claude record without a known account counts from Monday 00:00 of the
    /// current calendar week, since 1.4 (R5): after it, the record is its
    /// project's work with no subscription; before it, the record is not
    /// counted.
    #[test]
    fn claude_work_without_an_account_counts_from_the_week_start() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        let ledger = crate::project::ProjectKey::Repo("github.com/sam/ledger".to_string());
        history
            .append_many(&[
                // Sunday 15 Nov, before the calendar week's Monday 00:00.
                week_token(
                    "2026-11-15T20:00:00Z",
                    &ledger,
                    "claude-opus-5-5",
                    2_000_000,
                ),
                // Tuesday 17 Nov, after it.
                week_token("2026-11-17T09:00:00Z", &atlas, "claude-opus-5-5", 1_000_000),
            ])
            .expect("Claude records without a live account");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let card = this_week_card(&ctx, &all()).unwrap();
        assert_eq!(card.projects.len(), 1, "all: {:?}", card.projects);
        let project = &card.projects[0];
        assert_eq!(project.name, "atlas");
        assert_eq!(project.key, Some(atlas.clone()));
        assert_eq!(project.tokens_out, 1_000_000);
        assert_eq!(project.values.api, Some(20.0));
        assert_eq!(
            project.values.subscription, None,
            "no known account, so no subscription"
        );
        assert_eq!(
            project.by_subscription,
            vec![(None, 100.0)],
            "the unknown account is the whole API value"
        );
        assert_eq!(
            card.totals.tokens_out, 1_000_000,
            "the record before Monday is not counted"
        );
        assert!(
            card.projects.iter().all(|project| project.name != "ledger"),
            "a record before Monday 00:00 is not counted: {:?}",
            card.projects
        );
    }

    /// A reset began AC's running window: the card counts its use from the
    /// reading the reset began it at (60 − 45 = 15 %), not the newest
    /// reading, and starts it at the reset. LB has no reading in this
    /// history and is not listed.
    #[test]
    fn the_card_counts_the_use_since_a_reset() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let reset_at = utc("2026-11-18T08:30:00Z");
        let next_reset = utc("2026-11-25T08:30:00Z");
        let weekly = |t: DateTime<Utc>, p: f64| Sample {
            t,
            account: tiny.ac_ref(),
            src: crate::history::Source::Poll,
            l: vec![crate::history::LimitSample {
                k: LimitKind::WeeklyAll,
                m: None,
                p,
                r: Some(next_reset),
                b: true,
            }],
        };
        history
            .append(&WindowRecord {
                account: tiny.ac_ref(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-16T02:00:00Z")),
                end: reset_at,
                final_percent: 70.0,
                lost: None,
                last_sample: utc("2026-11-18T08:00:00Z"),
                plan: Some("Team Premium".to_string()),
                uncertain: false,
                reset: true,
                from: None,
            })
            .expect("the reset window");
        history
            .append_many(&[weekly(reset_at, 45.0), weekly(tiny.now, 60.0)])
            .expect("AC's readings after the reset");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let card = this_week_card(&ctx, &all()).unwrap();
        assert_eq!(card.accounts.len(), 1, "only AC has a reading");
        let ac = &card.accounts[0];
        assert_eq!(ac.id, tiny.ac().id);
        assert_eq!(ac.used, 15.0, "60 % now minus the 45 % the reset began at");
        assert_eq!(ac.now, 60.0);
        assert_eq!(ac.started_at, reset_at);
        assert_eq!(ac.resets_at, next_reset);
        assert!(
            close(ac.money.expect("AC's price"), 0.15 * 140.0 * 12.0 / 52.0),
            "money {:?}",
            ac.money
        );
    }

    /// An account without any usage reading has no running window: the card
    /// does not list it, and a history without any reading has an empty card.
    #[test]
    fn an_account_without_a_reading_is_not_listed() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };
        let card = this_week_card(&ctx, &all()).unwrap();
        assert!(card.accounts.is_empty());
        assert!(card.projects.is_empty());

        let only_lb = Query {
            range: RangeKey::All,
            accounts: vec![tiny.lb().id.clone()],
            host: None,
        };
        let card = this_week_card(&tiny.ctx(), &only_lb).unwrap();
        let listed: Vec<&str> = card
            .accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect();
        assert_eq!(
            listed,
            vec![tiny.lb().id.as_str()],
            "the chosen account only"
        );
    }

    /// Each project's API value and the subscriptions it split by. AC worked
    /// 3 M output tokens on atlas and 1 M on acme-kb (60 and 20 EUR), LB 2 M on
    /// atlas (40 EUR). A flash worker's 9 M on acme-kb (18 EUR) is the Ollama
    /// row and half of acme-kb's API value, its subscription without a month
    /// window. A record before AC's window and one of another host's filter are
    /// left out.
    ///
    /// Until 1.2.1 this test asserted that the flash record was left out of
    /// the card altogether; it is now the Ollama row. Until 1.4 the split was
    /// per account and provider row (`ThisWeekAccount.projects` /
    /// `ThisWeekProvider.projects`); it is now the card's `projects`, with the
    /// same data.
    #[test]
    fn the_card_splits_each_account_by_project() {
        let mut tiny = tiny();
        price_flash_and_codex(&mut tiny);
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        let acme_kb = crate::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string());
        let token =
            |t: &str, project: &crate::project::ProjectKey, model: &str, out: u64| TokenRecord {
                t: utc(t),
                minutes: 60,
                session: "4a1f9c2e-6b3d-4e8a-9f7c-1d2e3b4a5c6d".to_string(),
                cwd: "/home/sam/projects/atlas/engine".to_string(),
                project: project.clone(),
                model: model.to_string(),
                input: 0,
                out,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            };
        tiny.history
            .append_many(&[
                // In AC's finished window, before its running one began.
                token("2026-11-16T01:00:00Z", &atlas, "claude-opus-5-5", 5_000_000),
                token("2026-11-17T10:00:00Z", &atlas, "claude-opus-5-5", 3_000_000),
                token(
                    "2026-11-17T12:00:00Z",
                    &acme_kb,
                    "claude-opus-5-5",
                    1_000_000,
                ),
                token(
                    "2026-11-17T13:00:00Z",
                    &acme_kb,
                    "deepseek-v4.1-flash:cloud",
                    9_000_000,
                ),
                token("2026-11-18T10:00:00Z", &atlas, "claude-opus-5-5", 2_000_000),
            ])
            .expect("the tokens of this week");
        tiny.history
            .append(&Event {
                t: utc("2026-11-18T09:00:00Z"),
                kind: EventKind::Switch {
                    from: Some(tiny.ac_ref()),
                    to: tiny.lb_ref(),
                    by: crate::history::By::User,
                    reason: None,
                    cwd: None,
                    sessions: None,
                    waited_secs: None,
                },
            })
            .expect("the switch to LB");

        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        assert_eq!(card.projects.len(), 2, "all: {:?}", card.projects);
        let row = |name: &str| -> &ThisWeekProject {
            card.projects
                .iter()
                .find(|project| project.name == name)
                .unwrap_or_else(|| panic!("{name} on the card"))
        };

        let atlas_row = row("atlas");
        assert_eq!(atlas_row.key, Some(atlas.clone()));
        assert_eq!(atlas_row.tokens_out, 5_000_000);
        assert_eq!(atlas_row.values.api, Some(100.0));
        assert!(!atlas_row.values.api_partial);
        assert!(
            close(
                atlas_row.values.subscription.expect("a subscription"),
                22.153_846
            ),
            "atlas' subscription: {:?}",
            atlas_row.values.subscription
        );
        assert_eq!(
            atlas_row.by_subscription,
            vec![
                (Some(tiny.ac().id.clone()), 60.0),
                (Some(tiny.lb().id.clone()), 40.0),
            ]
        );

        let acme_row = row("acme-kb");
        assert_eq!(acme_row.key, Some(acme_kb.clone()));
        assert_eq!(acme_row.tokens_out, 10_000_000);
        assert_eq!(acme_row.values.api, Some(38.0));
        assert!(
            close(
                acme_row.values.subscription.expect("a known part"),
                3.230_769
            ),
            "acme-kb's known subscription: {:?}",
            acme_row.values.subscription
        );
        assert!(acme_row.values.subscription_partial);
        let split = &acme_row.by_subscription;
        assert_eq!(split.len(), 2, "acme-kb's split: {split:?}");
        assert_eq!(split[0].0.as_deref(), Some(tiny.ac().id.as_str()));
        assert!(close(split[0].1, 20.0 / 38.0 * 100.0));
        assert_eq!(
            split[1].0.as_deref(),
            Some(crate::money::ollama_subscription().acc.as_str())
        );
        assert!(close(split[1].1, 18.0 / 38.0 * 100.0));

        assert_eq!(card.providers.len(), 1, "providers: {:?}", card.providers);
        let ollama = &card.providers[0];
        assert_eq!(ollama.name, "Ollama");

        assert_eq!(card.totals.tokens_out, 15_000_000);
        assert_eq!(card.totals.projects, 2);
        assert_eq!(card.totals.used.api, Some(138.0));
        assert!(
            close(
                card.totals.used.subscription.expect("a known part"),
                22.153_846 + 3.230_769
            ),
            "the total subscription: {:?}",
            card.totals.used.subscription
        );
        assert!(card.totals.used.subscription_partial);
        assert!(
            close(
                card.totals.paid.expect("the running weeks' price"),
                73.846_154
            ),
            "the two running weeks: {:?}",
            card.totals.paid
        );
        assert_eq!(
            card.totals.used_of_paid, None,
            "a partial used gives no percent"
        );

        // vega has none of these records: the windows stay, the projects go.
        let vega = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: Some("vega".to_string()),
        };
        let card = this_week_card(&tiny.ctx(), &vega).unwrap();
        assert_eq!(card.accounts.len(), 2);
        assert!(card.providers.is_empty());
        assert!(card.projects.is_empty());
        assert_eq!(card.totals.tokens_out, 0);
        assert_eq!(card.totals.projects, 0);
    }

    /// The flash model's and Codex's prices, so their records have an API value:
    /// 2 and 10 USD per million output tokens, at the fixture's rate of 1.
    fn price_flash_and_codex(tiny: &mut crate::stats::tests::Tiny) {
        tiny.prices.models.insert(
            crate::money::price_key("ollama-cloud", "deepseek-v4.1-flash"),
            vec![crate::money::PricePoint {
                from: utc("2026-01-01T00:00:00Z"),
                price: crate::money::ModelPrice {
                    input: 0.5,
                    output: 2.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            }],
        );
        tiny.prices.models.insert(
            crate::money::price_key("openai", "gpt-6.1-sol"),
            vec![crate::money::PricePoint {
                from: utc("2026-01-01T00:00:00Z"),
                price: crate::money::ModelPrice {
                    input: 1.25,
                    output: 10.0,
                    cache_write: 0.0,
                    cache_read: 0.125,
                    tiered: false,
                },
            }],
        );
    }

    /// A token line of this week for the provider tests.
    fn week_token(
        t: &str,
        project: &crate::project::ProjectKey,
        model: &str,
        out: u64,
    ) -> TokenRecord {
        TokenRecord {
            t: utc(t),
            minutes: 60,
            session: "019a7c3e-4b2a-7d3f-9e6a-b8c52f0a1d7e".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: project.clone(),
            model: model.to_string(),
            input: 0,
            out,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        }
    }

    /// A Codex record done while AC was live is the Codex row, valued at API
    /// prices from Monday 00:00 of this calendar week: a record of last week
    /// is not in it. The account chips do not filter it out, the host filter
    /// does. AC's own share of atlas stays without it.
    #[test]
    fn the_card_lists_a_codex_row() {
        let mut tiny = tiny();
        price_flash_and_codex(&mut tiny);
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        let ledger = crate::project::ProjectKey::Repo("github.com/sam/ledger".to_string());
        tiny.history
            .append_many(&[
                // Last week (Sun 15 Nov): before this calendar week.
                week_token("2026-11-15T20:00:00Z", &atlas, "gpt-6.1-sol", 4_000_000),
                week_token("2026-11-17T09:00:00Z", &atlas, "gpt-6.1-sol", 3_000_000),
                week_token("2026-11-18T09:00:00Z", &ledger, "gpt-6.1-sol", 1_000_000),
                week_token("2026-11-17T11:00:00Z", &atlas, "claude-opus-5-5", 1_000_000),
            ])
            .expect("this week's tokens");
        let only_lb = Query {
            range: RangeKey::All,
            accounts: vec![tiny.lb().id.clone()],
            host: None,
        };

        for query in [all(), only_lb] {
            let card = this_week_card(&tiny.ctx(), &query).unwrap();
            assert_eq!(card.providers.len(), 1, "providers: {:?}", card.providers);
            let codex = &card.providers[0];
            assert_eq!(codex.name, "OpenAI");
            let ledger_row = card
                .projects
                .iter()
                .find(|project| project.name == "ledger")
                .expect("ledger on the card");
            assert_eq!(
                ledger_row.by_subscription,
                vec![(None, 100.0)],
                "the Codex work is all of ledger's API value: {:?}",
                card.projects
            );
        }

        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        let atlas_row = card
            .projects
            .iter()
            .find(|project| project.name == "atlas")
            .expect("atlas on the card");
        assert_eq!(
            atlas_row.values.api,
            Some(50.0),
            "30 EUR Codex and 20 EUR AC"
        );
        assert_eq!(
            atlas_row.by_subscription,
            vec![(None, 60.0), (Some(tiny.ac().id.clone()), 40.0),],
            "the Codex work is the None part; AC's is 40 of atlas' 50"
        );
        // The old test checked the Codex row's own split by project (atlas
        // 75 %, ledger 25 %). The card's rows now split each project by
        // subscription, so the Codex part is the `None` chip: 30 of atlas' 50
        // USD and 10 of ledger's 10 USD.
        let codex_part = |name: &str| {
            card.projects
                .iter()
                .find(|project| project.name == name)
                .and_then(|project| {
                    project
                        .by_subscription
                        .iter()
                        .find(|(id, _)| id.is_none())
                        .map(|(_, share)| *share)
                })
        };
        assert_eq!(
            codex_part("atlas"),
            Some(60.0),
            "the Codex part of atlas: {:?}",
            atlas_row.by_subscription
        );
        assert_eq!(
            codex_part("ledger"),
            Some(100.0),
            "the Codex part of ledger: {:?}",
            card.projects
        );

        let vega = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: Some("vega".to_string()),
        };
        let card = this_week_card(&tiny.ctx(), &vega).unwrap();
        assert!(card.providers.is_empty(), "the host filter applies");
    }

    /// A provider row carries its subscription and running window only: the
    /// removed API sum is not in its JSON (no page read it; the project and
    /// total rows carry the values).
    #[test]
    fn a_provider_row_carries_no_api_value() {
        let tiny = tiny();
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        tiny.history
            .append(&week_token(
                "2026-11-18T09:00:00Z",
                &atlas,
                "glm-5:cloud",
                1_000_000,
            ))
            .expect("an Ollama record");
        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        assert_eq!(card.providers.len(), 1);
        let provider = serde_json::to_value(&card.providers[0]).expect("the provider's JSON");
        for field in ["api", "apiCurrency", "apiMissing"] {
            assert!(
                provider.get(field).is_none(),
                "the provider row still has {field}: {provider}"
            );
        }
    }

    /// A provider's model without a price leaves its project's API value
    /// unknown, with the model named, and the project has no subscription
    /// share. The provider row still names the company; the API value is on the
    /// project and total rows now.
    #[test]
    fn an_unpriced_provider_record_leaves_its_project_api_unknown() {
        let tiny = tiny();
        let atlas = crate::project::ProjectKey::Repo("github.com/sam/atlas".to_string());
        tiny.history
            .append(&week_token(
                "2026-11-18T09:00:00Z",
                &atlas,
                "glm-5:cloud",
                1_000_000,
            ))
            .expect("an Ollama record");
        let card = this_week_card(&tiny.ctx(), &all()).unwrap();
        assert_eq!(card.providers.len(), 1);
        let ollama = &card.providers[0];
        assert_eq!(ollama.name, "Ollama");
        assert_eq!(
            card.projects.len(),
            1,
            "the project stays: {:?}",
            card.projects
        );
        let project = &card.projects[0];
        assert_eq!(project.name, "atlas");
        assert_eq!(project.values.api, None);
        assert_eq!(
            project.values.api_missing.as_deref(),
            Some("no price for glm-5:cloud")
        );
        assert!(
            project.by_subscription.is_empty(),
            "an unknown API value makes the share unknown"
        );
    }

    /// Two accounts added and auto-burn switched on in one calendar week are
    /// one mark at the first of them, so their labels never print on top of
    /// each other; a mark of another week stays its own.
    #[test]
    fn marks_of_one_week_are_one_label() {
        let tiny = tiny();
        tiny.history
            .append_many(&[
                Event {
                    t: utc("2026-11-04T09:00:00Z"),
                    kind: EventKind::AccountAdded {
                        account: tiny.lb_ref(),
                        plan: Some("Max".to_string()),
                    },
                },
                Event {
                    t: utc("2026-11-04T09:03:00Z"),
                    kind: EventKind::AccountAdded {
                        account: tiny.ac_ref(),
                        plan: Some("Team Premium".to_string()),
                    },
                },
                Event {
                    t: utc("2026-11-06T17:00:00Z"),
                    kind: EventKind::AutoBurnSet { on: true },
                },
                Event {
                    t: utc("2026-11-10T08:00:00Z"),
                    kind: EventKind::AutoBurnSet { on: false },
                },
            ])
            .expect("the marked events");

        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        assert_eq!(
            page.marks,
            vec![
                Mark {
                    at: utc("2026-11-04T09:00:00Z"),
                    label: "LB, AC added · auto-burn on".to_string(),
                },
                Mark {
                    at: utc("2026-11-10T08:00:00Z"),
                    label: "auto-burn off".to_string(),
                },
            ]
        );
    }

    /// An October range has history in none of its calendar weeks: every week
    /// is shown with no account and no blocked hour.
    #[test]
    fn a_range_without_history_has_weeks_without_accounts() {
        let tiny = tiny();
        let october = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: utc("2026-11-01T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &october, &all()).unwrap();

        assert_eq!(page.weeks.len(), 5, "the calendar weeks of October");
        for week in &page.weeks {
            assert!(week.accounts.is_empty(), "{} has accounts", week.week);
            assert_eq!(week.blocked_hours, 0.0);
            assert_eq!(week.expired_share, None);
        }
        assert_eq!(page.kpis.used, 0.0);
        assert_eq!(page.kpis.paid, 0.0);
        assert_eq!(page.kpis.expired, 0.0);
        assert!(page.kpis.expired_by_account.is_empty());
        assert!(
            page.kpis.blocked_by_company.is_empty(),
            "a range with no work has no working hours and no company entry"
        );
        assert_eq!(page.kpis.avoidable_hours, 0.0);
        assert_eq!(page.kpis.switches, SwitchCounts::default());
        assert_eq!(page.kpis.switch_cost_percent_of_window, None);
        assert!(page.without_price.is_empty());
        assert!(page.marks.is_empty());
        assert!(page.findings.is_empty());
    }

    #[test]
    fn switches_of_one_host() {
        let tiny = tiny();
        let query = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: Some("vega".to_string()),
        };
        let page = overview_in(&tiny.ctx(), &two_weeks(), &query).unwrap();

        assert_eq!(page.kpis.switches, SwitchCounts::default());
    }

    /// Money is written as `crates/app/ui/common.js`'s `money` writes it.
    #[test]
    fn money_text_follows_the_page() {
        assert_eq!(money_text(8.308, "EUR"), "8.31 EUR");
        assert_eq!(money_text(19.385, "EUR"), "19 EUR");
        assert_eq!(money_text(1204.4, "EUR"), "1,204 EUR");
    }

    /// Auto-burn that was switched off again inside the range has no sentence:
    /// only a switch that stayed on for a full week on each side does.
    #[test]
    fn auto_burn_off_inside_the_range_removes_the_sentence() {
        let tiny = tiny();
        tiny.history
            .append_many(&[
                Event {
                    t: utc("2026-11-09T12:00:00Z"),
                    kind: EventKind::AutoBurnSet { on: true },
                },
                Event {
                    t: utc("2026-11-11T12:00:00Z"),
                    kind: EventKind::AutoBurnSet { on: false },
                },
            ])
            .expect("auto-burn on and off");
        let three_weeks = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-23T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &three_weeks, &all()).unwrap();

        assert!(
            page.findings
                .iter()
                .all(|text| !text.starts_with("Since auto-burn")),
            "findings: {:#?}",
            page.findings
        );
    }

    // ---- proof tests of the review ----

    use crate::api::LimitKind;
    use crate::history::WindowRecord;

    /// One finished weekly window of an account.
    fn window(
        account: &crate::history::AccountRef,
        start: &str,
        end: &str,
        final_percent: f64,
    ) -> WindowRecord {
        WindowRecord {
            account: account.clone(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(utc(start)),
            end: utc(end),
            final_percent,
            lost: Some((100.0 - final_percent).max(0.0)),
            last_sample: utc(end) - Duration::minutes(1),
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        }
    }

    /// A window that ended above 100 %: nothing expired in it, so the page
    /// must not show a negative expired money, percent or share.
    #[test]
    fn a_window_above_100_percent_expired_nothing() {
        let tiny = tiny();
        tiny.history
            .append(&window(
                &tiny.lb_ref(),
                "2026-11-02T17:00:00Z",
                "2026-11-09T17:00:00Z",
                105.0,
            ))
            .expect("LB's window at 105 %");
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        for week in &page.weeks {
            assert!(
                week.expired_share.is_none_or(|share| share >= 0.0),
                "the share {:?} of the week",
                week.expired_share
            );
            for account in &week.accounts {
                assert!(
                    account.expired_percent >= 0.0,
                    "{} expired {} %",
                    account.id,
                    account.expired_percent
                );
                if let Some(expired) = account.expired {
                    assert!(expired >= 0.0, "{} expired {expired}", account.id);
                }
            }
        }
    }

    /// A week that a reset split into two windows counts both windows' use:
    /// 62 % from 0, a reset, then 30 % from 0 → the week used 92 %. Its expired
    /// part is the last window's `lost` (70 %), never the reset window's unused
    /// part (the reset gave that quota back). The page's percentages count each
    /// offered window (see `the_overview_percentages_are_shares_of_the_offered_weeks`),
    /// so this week is 46 % used and 35 % expired of the two weeks it offered.
    /// Every window's `lost` counts; the reset window has none.
    #[test]
    fn used_counts_every_window_of_a_week_with_a_reset() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append_many(&[
                WindowRecord {
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
                },
                WindowRecord {
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
                },
            ])
            .expect("the two windows of the reset week");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };

        let page = overview_in(&ctx, &two_weeks(), &all()).unwrap();
        let first = week(&page, "2026-11-02T00:00:00Z");
        let lb = first
            .accounts
            .iter()
            .find(|account| account.id == tiny.lb().id)
            .expect("LB's week");
        assert_eq!(lb.used_percent, 92.0, "62 % then 30 % from 0");
        assert_eq!(
            lb.expired_percent, 70.0,
            "every window's lost; the reset window has none"
        );
        let weekly = tiny.lb().price.expect("LB's price") * 12.0 / 52.0;
        assert!(
            close(lb.used.expect("LB's price"), 0.92 * weekly),
            "used money {}",
            lb.used.unwrap()
        );
        assert!(
            close(lb.expired.expect("LB's price"), 0.70 * weekly),
            "expired money {}",
            lb.expired.unwrap()
        );
        assert!(
            close(page.kpis.used, 0.92 * weekly),
            "used {}",
            page.kpis.used
        );
        assert!(
            close(page.kpis.paid, weekly),
            "paid {}: one week's price, not two",
            page.kpis.paid
        );
        assert!(
            close(page.kpis.used_percent.expect("a priced week"), 46.0),
            "the two offered windows: {:?}",
            page.kpis.used_percent
        );
        assert!(
            close(page.kpis.expired_percent.expect("a priced week"), 35.0),
            "the two offered windows: {:?}",
            page.kpis.expired_percent
        );
    }

    /// Used and Expired are shares of the offered weeks. Every finished weekly
    /// window offers one week of its account's price; a week that a reset split
    /// offers each of its windows. Used is the weeks' use (Σ final − from),
    /// expired is every window's lost, and the rest is still open, so
    /// used + expired + still open make 100 % of the windows counted. With one
    /// window per week this is the mock-up's `used / paid` and `expired / paid`.
    #[test]
    fn the_overview_percentages_are_shares_of_the_offered_weeks() {
        let tiny = tiny();
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        let used = page.kpis.used_percent.expect("the weeks have prices");
        let expired = page.kpis.expired_percent.expect("the weeks have prices");
        assert!(close(used, 81.25), "used {used}");
        assert!(close(expired, 18.75), "expired {expired}");
        assert!(
            close(used + expired, 100.0),
            "used + expired = {}",
            used + expired
        );
        assert_eq!(page.kpis.weeks, 2, "the range's two calendar weeks");
        assert_eq!(
            page.kpis.switches_total, 3,
            "you 1 · cli 0 · auto-burn 1 · agents 1 · external 0"
        );
    }

    /// The Blocked sub-line's number: the hours of the calendar weeks before the
    /// week the last account added in the range was added. AC was added on
    /// 12 Nov; the 2 h block of 7 Nov lies in the week before it.
    #[test]
    fn the_blocked_hours_before_the_last_added_account() {
        let tiny = tiny();
        tiny.history
            .append(&Event {
                t: utc("2026-11-12T00:00:00Z"),
                kind: EventKind::AccountAdded {
                    account: tiny.ac_ref(),
                    plan: Some("Team Premium".to_string()),
                },
            })
            .expect("AC added");

        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();
        let before = page
            .kpis
            .blocked_before_added
            .expect("AC was added in the range");
        assert_eq!(before.name, tiny.ac().abbr);
        assert!(
            close(before.hours, 2.0),
            "the hours before AC: {}",
            before.hours
        );
    }

    /// The hours the page shows are the hours its weeks show: every calendar
    /// week of the range is on the page, so the blocked hours of all of them
    /// add up to the KPI.
    #[test]
    fn the_blocked_hours_of_the_page_add_up() {
        let tiny = tiny();
        // A range that starts inside the week of the block: that week holds
        // no finished window of the range, but it is on the page.
        let range = Range {
            from: utc("2026-11-07T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &range, &all()).unwrap();

        let shown: f64 = page.weeks.iter().map(|week| week.blocked_hours).sum();
        assert!(
            close(shown, blocked(&page, Company::Anthropic)),
            "the weeks show {shown} hours, the page says {} hours",
            blocked(&page, Company::Anthropic)
        );
    }

    /// A range without a block has no hours; the page must not show a
    /// negative zero.
    #[test]
    fn the_hours_of_a_range_without_a_block_are_not_negative() {
        let tiny = tiny();
        let october = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: utc("2026-11-01T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &october, &all()).unwrap();

        assert!(
            page.kpis
                .blocked_by_company
                .iter()
                .all(|entry| entry.hours.is_sign_positive()),
            "blocked hours {:?}",
            page.kpis.blocked_by_company
        );
        assert!(
            page.kpis.avoidable_hours.is_sign_positive(),
            "avoidable hours {}",
            page.kpis.avoidable_hours
        );
    }

    /// No Anthropic entry in `blocked_by_company` means there is no blocked
    /// finding to show, not a finding of 0 h (review, 1.3.0).
    #[test]
    fn a_missing_anthropic_blocked_entry_has_no_finding() {
        let tiny = tiny();
        let october = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: utc("2026-11-01T00:00:00Z"),
        };
        let page = overview_in(&tiny.ctx(), &october, &all()).unwrap();
        assert!(
            page.kpis
                .blocked_by_company
                .iter()
                .all(|entry| entry.company != Company::Anthropic),
            "the range has no Anthropic blocked hours: {:?}",
            page.kpis.blocked_by_company
        );
        assert_eq!(finding_blocked(&page.kpis, &page.weeks), None);
    }

    /// P-2 counts an OpenAI subscription's finished week as an account week, so
    /// the Codex week joins the page as a row without a price; every Claude
    /// figure stays what it was.
    #[test]
    fn provider_records_add_a_provider_week_but_change_no_claude_figure() {
        let without = tiny();
        let with = tiny();
        append_codex_records(&with.history, utc("2026-11-14T09:00:00Z"));

        let before = overview_in(&without.ctx(), &two_weeks(), &all()).expect("the overview");
        let mut after = overview_in(&with.ctx(), &two_weeks(), &all()).expect("the overview");
        let provider_row = after
            .weeks
            .iter_mut()
            .find_map(|week| {
                let index = week
                    .accounts
                    .iter()
                    .position(|row| row.id == "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13")?;
                Some(week.accounts.remove(index))
            })
            .expect("the Codex week is an account week on the page");
        assert_eq!(provider_row.used_percent, 12.0);
        assert_eq!(provider_row.used, None, "the subscription has no price");
        assert_eq!(provider_row.expired, None);

        assert_eq!(
            after, before,
            "a Codex record changed a Claude figure of the overview"
        );
    }

    /// A priced provider subscription fills its This-week row like an account's
    /// row (decision P-2 7): the running week's use, its money, its reset. The
    /// API value stays as it was.
    #[test]
    fn this_week_rows_for_providers_carry_subscription_money() {
        let mut tiny = tiny();
        let codex = codex_ref();
        tiny.provider_subscriptions = vec![provider_subscription(&codex, "OA", None)];
        codex_prices(&mut tiny.prices);
        tiny.history
            .append(&Sample {
                t: utc("2026-11-17T09:00:00Z"),
                account: codex.clone(),
                src: Source::Poll,
                l: vec![crate::history::LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 50.0,
                    r: Some(utc("2026-11-23T10:00:00Z")),
                    b: true,
                }],
            })
            .expect("the Codex reading");
        tiny.history
            .append(&Event {
                t: utc("2026-11-01T00:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: codex,
                    amount: Some(23.0),
                    currency: Some("EUR".to_string()),
                },
            })
            .expect("the subscription's price");
        tiny.history
            .append(&codex_token(
                "2026-11-17T09:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("a Codex record of this week");
        let card = this_week_card(&tiny.ctx(), &all()).expect("the This-week card");
        let openai = card
            .providers
            .iter()
            .find(|row| row.name == "OA")
            .expect("the OA row");
        assert_eq!(openai.used, Some(50.0), "the running week's use so far");
        assert_eq!(openai.resets_at, Some(utc("2026-11-23T10:00:00Z")));
        assert_eq!(openai.money_missing, None);
        assert!(
            close(openai.money.expect("money"), 50.0 * percent_value(23.0)),
            "the used share of the week's 23 EUR: {:?}",
            openai.money
        );
    }

    /// Google's running week has its row with use and money from its readings.
    /// Its work is not recorded, so its week stays out of the sums that set the
    /// recorded work against what was paid.
    #[test]
    fn a_google_row_shows_its_use_and_leaves_the_paid_share_alone() {
        let before = {
            let tiny = tiny();
            this_week_card(&tiny.ctx(), &all()).expect("the This-week card")
        };
        let mut tiny = tiny();
        let google = crate::stats::tests::google_ref();
        tiny.provider_subscriptions
            .push(provider_subscription(&google, "GO", Some(22.0)));
        let q = all();
        for (now_percent, found) in [(None, None), (Some(40.0), Some(40.0))] {
            if let Some(percent) = now_percent {
                tiny.history
                    .append(&Sample {
                        t: utc("2026-11-17T09:00:00Z"),
                        account: google.clone(),
                        src: Source::Statusline,
                        l: vec![crate::history::LimitSample {
                            k: LimitKind::WeeklyAll,
                            m: None,
                            p: percent,
                            r: Some(utc("2026-11-23T10:00:00Z")),
                            b: true,
                        }],
                    })
                    .expect("Google's reading");
            }
            let card = this_week_card(&tiny.ctx(), &q).expect("the This-week card");
            let row = card
                .providers
                .iter()
                .find(|row| row.name == "GO")
                .expect("the GO row");
            assert_eq!(row.company, Company::Google);
            assert_eq!(row.used, found);
            match found {
                Some(_) => assert!(close(row.money.expect("money"), 40.0 * percent_value(22.0))),
                None => assert_eq!(
                    row.money_missing.as_deref(),
                    Some("no reading of this week's usage")
                ),
            }
            assert_eq!(card.totals.paid, before.totals.paid);
            assert_eq!(card.totals.paid_partial, before.totals.paid_partial);
            assert_eq!(card.totals.used_of_paid, before.totals.used_of_paid);
        }
    }

    /// Each OpenAI login gets its own This-week row, summing its own records
    /// from its own running week's start, with that week's use and money
    /// (decision P-2 6, review F): OA and OA2, never one OpenAI row.
    #[test]
    fn two_openai_logins_have_one_this_week_row_each() {
        let mut tiny = tiny();
        let oa = codex_ref();
        let oa2 = AccountRef {
            acc: "5d2e7a1c-3b4f-4a6e-9c8d-1f2a3b4c5d6e".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        tiny.provider_subscriptions = vec![
            provider_subscription(&oa, "OA", Some(23.0)),
            provider_subscription(&oa2, "OA2", Some(30.0)),
        ];
        codex_prices(&mut tiny.prices);
        for (account, percent, reset) in [
            (&oa, 50.0, "2026-11-23T10:00:00Z"),
            (&oa2, 20.0, "2026-11-24T10:00:00Z"),
        ] {
            tiny.history
                .append(&Sample {
                    t: utc("2026-11-17T09:00:00Z"),
                    account: account.clone(),
                    src: Source::Poll,
                    l: vec![crate::history::LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: percent,
                        r: Some(utc(reset)),
                        b: true,
                    }],
                })
                .expect("a running week");
        }
        tiny.history
            .append(&Event {
                t: utc("2026-11-17T13:00:00Z"),
                kind: EventKind::ProviderSwitch {
                    from: Some(oa.clone()),
                    to: oa2.clone(),
                    by: crate::history::By::User,
                },
            })
            .expect("the switch to OA2");
        tiny.history
            .append(&codex_token(
                "2026-11-17T12:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("OA's record");
        tiny.history
            .append(&codex_token(
                "2026-11-17T14:00:00Z",
                "gpt-6.2-codex",
                1_000_000,
            ))
            .expect("OA2's record");

        let card = this_week_card(&tiny.ctx(), &all()).expect("the This-week card");
        assert_eq!(card.providers.len(), 2, "providers: {:?}", card.providers);
        let oa_row = card
            .providers
            .iter()
            .find(|row| row.name == "OA")
            .expect("the OA row");
        assert_eq!(oa_row.used, Some(50.0));
        assert!(
            close(oa_row.money.expect("money"), 50.0 * percent_value(23.0)),
            "OA's used share: {:?}",
            oa_row.money
        );
        let oa2_row = card
            .providers
            .iter()
            .find(|row| row.name == "OA2")
            .expect("the OA2 row");
        assert_eq!(oa2_row.used, Some(20.0));
        assert!(
            close(oa2_row.money.expect("money"), 20.0 * percent_value(30.0)),
            "OA2's used share: {:?}",
            oa2_row.money
        );
    }

    /// A provider row whose subscription has no running window still says why
    /// its money is missing, in words for that subscription (decision P-2 7,
    /// review E): never four `None` fields without a reason.
    #[test]
    fn a_provider_row_without_a_running_window_says_why() {
        let mut tiny = tiny();
        let oa = codex_ref();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![
            provider_subscription(&oa, "OA", Some(23.0)),
            provider_subscription(&ollama, "OL", Some(20.0)),
        ];

        let card = this_week_card(&tiny.ctx(), &all()).expect("the This-week card");
        let oa_row = card
            .providers
            .iter()
            .find(|row| row.name == "OA")
            .expect("the OA row");
        assert_eq!(oa_row.used, None);
        assert_eq!(
            oa_row.money_missing.as_deref(),
            Some("no reading of this week's usage")
        );
        let ollama_row = card
            .providers
            .iter()
            .find(|row| row.name == "OL")
            .expect("the OL row");
        assert_eq!(ollama_row.used, None);
        assert_eq!(
            ollama_row.money_missing.as_deref(),
            Some("no monthly allowance in Ollama's reading")
        );
    }

    /// RF5: Ollama's missing month has one reason, decided by the Valuer. A
    /// subscription with a price and no `monthly_credits` reading says "no
    /// monthly allowance" on the Overview and through `Valuer::subscription`.
    #[test]
    fn ollama_without_a_month_says_the_same_reason_everywhere() {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions = vec![provider_subscription(&ollama, "OL", Some(20.0))];

        let card = this_week_card(&tiny.ctx(), &all()).expect("the This-week card");
        let row = card
            .providers
            .iter()
            .find(|row| row.name == "OL")
            .expect("the OL row");
        let valuer = Valuer::build(
            &tiny.history,
            &Range {
                from: tiny.now - Duration::days(7),
                to: tiny.now,
            },
            &tiny.prices,
            &tiny.fx,
            &tiny.settings,
            &tiny.accounts,
            &tiny.provider_subscriptions,
            &tiny.warnings,
        )
        .expect("a valuer");
        let from_valuer = valuer
            .subscription(
                &ollama,
                &LimitKind::Other("monthly_credits".to_string()),
                tiny.now,
                1.0,
            )
            .expect_err("no month window");
        assert_eq!(row.money_missing, Some(from_valuer.to_string()));
        assert_eq!(
            row.money_missing.as_deref(),
            Some("no monthly allowance in Ollama's reading")
        );
    }

    /// A window that ended above 100 % used more than it was offered. Its whole
    /// use counts in the used share, so the two shares the page shows pass
    /// 100 % together (here 82.7 % + 18.8 %): nothing is cut, and the rule on
    /// `Kpis::used_percent` says so.
    #[test]
    fn a_window_above_100_percent_counts_its_whole_use_in_the_used_share() {
        let tiny = tiny();
        tiny.history
            .append(&window(
                &tiny.lb_ref(),
                "2026-11-02T17:00:00Z",
                "2026-11-09T17:00:00Z",
                105.0,
            ))
            .expect("LB's window at 105 %");
        let page = overview_in(&tiny.ctx(), &two_weeks(), &all()).unwrap();

        let used = page.kpis.used_percent.expect("the weeks have prices");
        let expired = page.kpis.expired_percent.expect("the weeks have prices");
        assert!(close(used, 82.65625), "the used share: {used} %");
        assert!(close(expired, 18.75), "the expired share: {expired} %");
        assert!(
            used + expired > 100.0,
            "the whole use counts, nothing is cut"
        );
    }

    /// The KPI's expired share and the "Share that expired, per week" chart's
    /// share of the same week follow one rule: the expired money over the money
    /// the week's windows offered. A week a reset split offers two windows, so
    /// both say 35 % (70 % of one window over two).
    #[test]
    fn the_overview_share_and_the_weeks_own_share_are_the_same_number() {
        let tiny = tiny();
        let dir = tempdir().expect("a temp history");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append_many(&[
                WindowRecord {
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
                },
                WindowRecord {
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
                },
            ])
            .expect("the two windows of the reset week");
        let ctx = Ctx {
            history: &history,
            ..tiny.ctx()
        };
        let one_week = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-09T00:00:00Z"),
        };

        let page = overview_in(&ctx, &one_week, &all()).unwrap();
        let first = week(&page, "2026-11-02T00:00:00Z");
        let kpi = page.kpis.expired_percent.expect("the week has a price");
        let own = first.expired_share.expect("the week has a price");
        assert!(close(kpi, 35.0), "the KPI's expired share: {kpi} %");
        assert!(
            close(kpi, own),
            "the KPI says {kpi} %, the week's own share says {own} %"
        );
    }

    /// The Expired share divides by the finished windows' offer, as the Weeks
    /// page's own summary does, also when the week holds a running window: LB's
    /// finished window of 16..18 Nov offers one week, LB's running window and
    /// AC's running one offer none of the Expired denominator. So the Overview's
    /// KPI, the week's own share and `weeks.rs`'s summary all say 30 %; before
    /// this fix the Overview divided by all three windows and said 10 %.
    #[test]
    fn the_overview_and_the_weeks_page_agree_on_the_expired_share_of_a_running_week() {
        let tiny = tiny();
        tiny.history
            .append(&window(
                &tiny.lb_ref(),
                "2026-11-16T12:00:00Z",
                "2026-11-18T12:00:00Z",
                70.0,
            ))
            .expect("LB's finished window in the running week");
        let this_week = Range {
            from: utc("2026-11-16T00:00:00Z"),
            to: tiny.now,
        };

        let page = overview_in(&tiny.ctx(), &this_week, &all()).unwrap();
        let holding = week(&page, "2026-11-16T00:00:00Z");
        assert!(holding.running, "the week holds now");
        // The same calendar week from the Weeks page's own summary function on
        // the same context.
        let summary =
            crate::stats::weeks::week_summary(&tiny.ctx(), &all(), utc("2026-11-16T00:00:00Z"))
                .expect("the week's summary");

        let expired = page
            .kpis
            .expired_percent
            .expect("a finished window has a price");
        let own = holding
            .expired_share
            .expect("a finished window has a price");
        let weeks = summary
            .expired_percent_of_paid
            .expect("a finished window has a price");
        assert!(close(expired, 30.0), "the Overview's share: {expired} %");
        assert!(
            close(expired, own),
            "the KPI says {expired} %, the week's own share says {own} %"
        );
        assert!(
            close(expired, weeks),
            "the Overview says {expired} %, the Weeks page says {weeks} %"
        );
    }
}
