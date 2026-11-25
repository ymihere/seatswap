//! The Subscriptions page's figures (M-2 of 1.3; spec §3.3, R6 and R7): every
//! chosen subscription's factor from the one estimator in [`crate::burn::rates`],
//! the plan sizes it implies, the usage rows, the tokens a week holds and the
//! per-percent points. Nothing here fits a factor or values a record on its own;
//! the period and usage rules are in [`super::subscriptions`].

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::Serialize;

use crate::burn::fit::Factor;
use crate::burn::fit::{self, Pair};
use crate::burn::rates::{self, main_limit, worked_hour, PairLimit, RateAccount, WorkedHour};
use crate::burn::{usd_hours, Rates, RHYTHM_WEEKS};
use crate::history::{AccountRef, Event, Hosted, Hosts, Range, Sample, TokenRecord, WindowRecord};
use crate::live_sessions::Values;
use crate::money::{api_usd, model_price_key, price_key, work_usd, Company, Missing, Valuer};
use crate::transcripts::Tokens;

use super::subscriptions::{self, HistoryReads, Period, UsageInput};
use super::{
    account_chosen, account_id, account_weeks_valued, add_value, full_half, keep_warnings,
    next_week_start, range_of, stored_account, stored_subscription, subscription_months,
    week_columns, week_of_record, AccountWeek, Ctx, MonthValued, Query, StatsError,
};

/// A factor's range is "thin" when half its width is more than this share of
/// its value (decision 4 of M-2): the page marks such a figure.
const THIN_SHARE: f64 = 0.2;

/// The longest window any company has is Ollama's month; a point's window can
/// start this many days before the range and still overlap it.
const LONGEST_WINDOW_DAYS: i64 = 31;

/// How many points before a change the "before" factor pools (decision 6).
const POINTS_BEFORE_CHANGE: usize = 8;

/// Why a subscription has no figure: `rates_in` found no pair whose work is
/// priced in the range, so it has no main-limit factor.
const NO_FACTOR_REASON: &str = "not measured yet: no pair with priced work in this range";

/// Why a subscription of `company` has no factor.
fn no_factor_reason(company: Company) -> String {
    if company.work_recorded() {
        NO_FACTOR_REASON.to_string()
    } else {
        Missing::WorkNotRecorded.to_string()
    }
}

/// Why a subscription with a factor has no model figures: it ran no model with
/// a price in the range.
const NO_PRICED_MODEL_REASON: &str = "no priced model it ran in this range";

/// The whole Subscriptions page (task M-2).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionsPage {
    pub totals: Totals,
    pub plans: Vec<PlanRow>,
    pub sizes: Sizes,
    pub usage: Vec<UsageGroup>,
    #[serde(rename = "usageTotal")]
    pub usage_total: UsageRow,
    #[serde(rename = "perPercent")]
    pub per_percent: Vec<PerPercent>,
    #[serde(rename = "tokensWeek")]
    pub tokens_week: TokensWeek,
    pub currency: Option<String>,
}

/// The page's totals. A sum with a missing part is `None`, never 0
/// (decision 9 of M-2).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub count: u32,
    pub price_month: Option<f64>,
    pub api_month: Option<f64>,
    pub times_paid: Option<f64>,
    pub tokens_used_month: Option<f64>,
    pub weeks: u32,
    pub partial: Vec<Partial>,
}

/// A subscription with fewer finished weeks in the range than the range has.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Partial {
    pub id: String,
    pub weeks: u32,
}

/// One subscription and what a full weekly (Ollama: monthly) limit holds of
/// the owner's actual work with its company's models (1.4.1): hours worked and
/// tokens, both from [`WorkedHour`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRow {
    pub id: String,
    pub company: Company,
    pub plan: Option<String>,
    /// Hours of actual work a full limit holds: the limit's API dollars over
    /// the dollars of an hour worked. `None` with `missing`, or when no hour of
    /// the last 4 weeks holds priced work of the company.
    pub full_hours: Option<f64>,
    /// Tokens a full limit holds at the owner's mix of the company's models:
    /// the limit's API dollars at the dollars per token of the same hours.
    /// `None` exactly when `full_hours` is.
    pub full_tokens: Option<f64>,
    pub trend: Option<Change>,
    /// Why `trend` cannot tell a change ([`too_few_for_change`]): too few
    /// points in the range, so the page says so rather than "steady". `None`
    /// when the points are enough, or the row has no points at all.
    pub trend_missing: Option<String>,
    pub basis_hours: Option<f64>,
    pub thin: bool,
    /// Why the row has no models, no basis and no cells; `None` when it has
    /// them (decision 12 of M-2's fix).
    pub missing: Option<String>,
}

/// The plan sizes against the most measured Anthropic plan (decision 5).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sizes {
    /// The id of the reference subscription, `None` when no Anthropic
    /// subscription has a measured factor (fix 3 of M-2).
    pub reference: Option<String>,
    pub rows: Vec<SizeRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SizeRow {
    pub id: String,
    pub plan: Option<String>,
    /// Its factor against the reference's; `None` when its own factor is not
    /// measured: a value that leans on its prior is no measured ratio (fix 3
    /// of M-2).
    pub of_reference: Option<f64>,
    /// The low end of the ratio's range, `None` with `of_reference`.
    pub low: Option<f64>,
    /// The high end of the ratio's range, `None` with `of_reference`.
    pub high: Option<f64>,
    /// The ratio its plan's name gives, against the reference's; `None` when
    /// either name has no capacity or there is no measured reference.
    pub by_name: Option<f64>,
    pub measured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageGroup {
    pub company: Company,
    pub count: u32,
    pub price_month: Option<f64>,
    pub rows: Vec<UsageRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub id: String,
    pub plan: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub price_month: Option<f64>,
    pub entered: Option<EnteredPrice>,
    pub used_percent: Option<f64>,
    pub expired: Option<f64>,
    pub api: Option<f64>,
    pub times_paid: Option<f64>,
    pub missing: Option<String>,
}

/// A stored price entered in a currency other than the display currency.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnteredPrice {
    pub amount: f64,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerPercent {
    pub id: String,
    pub limit: String,
    pub model: String,
    pub points: Vec<Point>,
    pub change: Option<Change>,
    /// Why `change` cannot be told ([`too_few_for_change`]); `None` when the
    /// points are enough or there are none.
    pub change_missing: Option<String>,
    pub now: Vec<Now>,
    /// Minutes of actual work 1 % of this limit holds ([`WorkedHour`]); `None`
    /// when no hour of the last 4 weeks holds priced work of the company.
    pub minutes: Option<f64>,
    pub window_holds_minutes: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub week: DateTime<Utc>,
    pub tokens: f64,
    pub low: f64,
    pub high: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Now {
    pub model: String,
    pub tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub model: String,
    pub since: DateTime<Utc>,
    pub percent: f64,
    pub before: f64,
    pub after: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokensWeek {
    pub models: Vec<TokenModel>,
    pub rows: Vec<TokensWeekRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenModel {
    pub model: String,
    pub company: Company,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokensWeekRow {
    pub id: String,
    /// One cell per model column: the figure, or why the cell has none.
    pub cells: Vec<TokensWeekCell>,
    /// Why the row has no cells; `None` when it has them (decision 12 of
    /// M-2's fix).
    pub missing: Option<String>,
}

/// One cell of the tokens-a-week table: a figure for a model the subscription
/// can use, or the reason the cell is empty. Serialized untagged, a missing
/// cell is `{ "missing": "<reason>" }`; a figure carries `tokens`, `low`,
/// `high`, `estimated`, `thin` and `measured`. `measured` is the factor's own
/// flag: a factor that leans on its plan-size prior has no range worth showing
/// (without pairs its range is the prior itself, so it would read `± 0`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TokensWeekCell {
    Figure {
        tokens: f64,
        low: f64,
        high: Option<f64>,
        estimated: bool,
        thin: bool,
        measured: bool,
    },
    Missing {
        missing: String,
    },
}

/// The page's entry point, for the window command.
pub fn subscriptions_page<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    q: &Query,
) -> Result<SubscriptionsPage, StatsError> {
    let range = range_of(q.range, ctx)?;
    subscriptions_in(ctx, &range, q)
}

/// One subscription's model work in the range: its tokens, its API dollars
/// (USD, the unit the factor uses) and the four token counts, for estimating a
/// model the subscription never ran from its company's main mix.
#[derive(Debug, Clone, Default)]
struct ModelStat {
    tokens: u64,
    /// The API dollars of the priced records; `0.0` while [`ModelStat::unpriced`]
    /// says a record of the model has no price.
    usd: f64,
    /// A record of this model had no price, so the sum is not the model's USD
    /// per token.
    unpriced: bool,
    mix: [u64; 4],
}

impl ModelStat {
    fn add(&mut self, record: &TokenRecord, usd: Option<f64>) {
        let tokens = Tokens::from(record);
        self.tokens += tokens.input + tokens.out + tokens.cw + tokens.cr;
        self.mix[0] += tokens.input;
        self.mix[1] += tokens.out;
        self.mix[2] += tokens.cw;
        self.mix[3] += tokens.cr;
        match usd {
            Some(usd) => self.usd += usd,
            None => self.unpriced = true,
        }
    }

    fn usd_per_token(&self) -> Option<f64> {
        (!self.unpriced && self.tokens > 0).then(|| self.usd / self.tokens as f64)
    }
}

/// A point with the factor it was fitted from, kept for the change rule.
struct ChangePoint {
    point: Point,
    value: f64,
    pairs: Vec<Pair>,
}

/// One subscription's usage row with the figures the totals need (decisions 3
/// and 4 of M-2's fix).
struct UsageEntry {
    company: Company,
    row: UsageRow,
    /// What it paid over its finished periods, in the display currency.
    paid: Option<f64>,
    /// Its finished weeks or months in the range.
    periods: u32,
    /// Whether it counts per month (Ollama) rather than per week.
    ollama: bool,
    values: Values,
    tokens: u64,
}

impl UsageEntry {
    /// Whether the totals count the subscription: it has a finished period of
    /// the range, and its work is recorded, so price and work can be set
    /// against each other.
    fn counted(&self) -> bool {
        self.periods > 0 && self.company.work_recorded()
    }
}

pub(crate) fn subscriptions_in<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
) -> Result<SubscriptionsPage, StatsError> {
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
    let weeks = account_weeks_valued(ctx, range, q, &valuer)?;
    let subjects = chosen_subjects(ctx, q);
    // The fit runs over every stored subscription, whatever chips the page
    // shows, so a chip never changes a factor or a prior (decision 9).
    let every_stored = all_subjects(ctx);

    let events = keep_warnings(ctx.warnings, || {
        ctx.history.read::<Event>(&Range::ALL, &Hosts::All)
    })?;
    let samples = keep_warnings(ctx.warnings, || {
        ctx.history.read::<Sample>(&Range::ALL, &Hosts::All)
    })?;
    let windows = keep_warnings(ctx.warnings, || {
        ctx.history.finished_windows(&Range::ALL, &Hosts::All)
    })?;
    let point_from = range
        .from
        .checked_sub_signed(Duration::days(LONGEST_WINDOW_DAYS))
        .ok_or_else(|| StatsError::Message("the range starts before the calendar".to_string()))?;
    let point_range = Range {
        from: point_from,
        to: range.to,
    };
    let tokens = keep_warnings(ctx.warnings, || {
        ctx.history.read::<TokenRecord>(&point_range, &Hosts::All)
    })?;
    let rhythm_from = ctx
        .now
        .checked_sub_signed(Duration::weeks(RHYTHM_WEEKS))
        .ok_or_else(|| StatsError::Message("now is before the calendar".to_string()))?;
    let rhythm_tokens = keep_warnings(ctx.warnings, || {
        ctx.history.read::<TokenRecord>(
            &Range {
                from: rhythm_from,
                to: ctx.now,
            },
            &Hosts::All,
        )
    })?;

    let periods = subscriptions::periods_for_chosen_from(
        ctx,
        q,
        &valuer,
        HistoryReads {
            events: &events.records,
            samples: &samples.records,
            windows: &windows.records,
        },
    )?;
    let removed = subscriptions::removed_periods(
        ctx,
        range,
        q,
        &valuer,
        HistoryReads {
            events: &events.records,
            samples: &samples.records,
            windows: &windows.records,
        },
    )?;
    // The timelines, the OpenAI logins and each subscription's sorted samples,
    // once for every window and subscription (decision 16).
    let pools = rates::WindowPools::of(&events.records, ctx.provider_subscriptions);
    let lines = rates::sample_lines_by_account(&every_stored, &samples.records);
    let learned = rates::rates_in_with(
        &every_stored,
        &lines,
        &samples.records,
        &tokens.records,
        &pools,
        ctx.prices,
        range.from,
        range.to,
    );

    let months = ollama_months(ctx, range, q, &subjects)?;

    // The model work of every record in the range, by owner and by company.
    let mut owner_models: BTreeMap<AccountRef, BTreeMap<String, ModelStat>> = BTreeMap::new();
    let mut company_models: BTreeMap<Company, BTreeMap<String, ModelStat>> = BTreeMap::new();
    let mut seen_unpriced: BTreeSet<String> = BTreeSet::new();
    let mut first_seen: BTreeMap<AccountRef, DateTime<Utc>> = BTreeMap::new();
    let mut finished_values: BTreeMap<AccountRef, Values> = BTreeMap::new();
    let mut finished_tokens: BTreeMap<AccountRef, u64> = BTreeMap::new();
    let mut warned_unknown_openai = false;
    let empty_values = Values::of(&[], ctx.settings, ctx.fx, range.to);

    for hosted in &tokens.records {
        let record = &hosted.record;
        if record.t < range.from || record.t >= range.to {
            continue;
        }
        let tokens_here = token_count(record);
        if tokens_here == 0 {
            continue;
        }
        let value = valuer.value(&hosted.host, record);
        let company = Company::of_model(&record.model);
        let usd = api_usd(record, ctx.prices);
        if usd.is_none() {
            seen_unpriced.insert(record.model.clone());
        }
        if company == Company::OpenAI
            && !warned_unknown_openai
            && crate::money::openai_subscription_at(
                &pools.provider_timelines,
                &pools.openai,
                &hosted.host,
                record.t,
            )
            .is_err()
        {
            warned_unknown_openai = true;
            ctx.warnings
                .borrow_mut()
                .push(crate::money::no_openai_login_warning(
                    &format!("record of {}", record.model),
                    &hosted.host,
                    record.t,
                ));
        }
        company_models
            .entry(company)
            .or_default()
            .entry(record.model.clone())
            .or_default()
            .add(record, usd);
        let Some(owner) = value.account.clone() else {
            continue;
        };
        first_seen
            .entry(owner.clone())
            .and_modify(|first| *first = (*first).min(record.t))
            .or_insert(record.t);
        owner_models
            .entry(owner.clone())
            .or_default()
            .entry(record.model.clone())
            .or_default()
            .add(record, usd);
        let finished = match company {
            Company::Anthropic | Company::OpenAI | Company::Google => {
                week_of_record(&weeks, &owner, record.t).is_some()
            }
            Company::Ollama => months.get(&owner).is_some_and(|months| {
                months.iter().any(|month| {
                    month.end <= ctx.now && month.start <= record.t && record.t < month.end
                })
            }),
        };
        if finished {
            let values = Values::of(std::slice::from_ref(&value), ctx.settings, ctx.fx, record.t);
            add_value(&mut finished_values, owner.clone(), &values);
            *finished_tokens.entry(owner).or_default() += tokens_here;
        }
    }

    seen_unpriced.extend(learned.unpriced.iter().cloned());
    for model in &seen_unpriced {
        ctx.warnings
            .borrow_mut()
            .push(rates::unpriced_warning(model));
    }

    let mut worked_cache: BTreeMap<Company, Option<WorkedHour>> = BTreeMap::new();

    let range_weeks = week_columns(range, &ctx.zone)
        .iter()
        .filter(|week| next_week_start(**week, &ctx.zone) <= ctx.now)
        .count() as u32;
    let currency = ctx.settings.currency.clone();

    let mut plans = Vec::new();
    let mut usage_rows: Vec<UsageEntry> = Vec::new();
    let mut per_percent = Vec::new();
    let mut weekly: BTreeMap<AccountRef, Rates> = BTreeMap::new();
    let mut priced_main: BTreeSet<AccountRef> = BTreeSet::new();
    let mut partial = Vec::new();

    for subject in &subjects {
        let reference = &subject.account;
        let id = account_id(ctx, reference);
        let subject_weeks: Vec<&AccountWeek> = weeks
            .iter()
            .filter(|week| &week.account == reference)
            .collect();
        let own_weeks = subject_weeks.len() as u32;
        let subject_months = months.get(reference).cloned().unwrap_or_default();
        let own_periods: &[Period] = periods.get(reference).map_or(&[], Vec::as_slice);
        let stored_price = stored_price(ctx, reference);
        let price_attempt = valuer.price_at(reference, range.to);
        let (price, price_missing) = match price_attempt {
            Ok(price) => (Some(price), None),
            Err(missing) => (None, Some(missing.to_string())),
        };
        let since = own_periods.first().map(|period| period.from).or_else(|| {
            first_seen.get(reference).copied().or_else(|| {
                subject_weeks
                    .first()
                    .map(|week| week.week)
                    .or_else(|| subject_months.first().map(|month| month.start))
            })
        });
        let plan = own_periods
            .iter()
            .rev()
            .find(|period| period.from <= ctx.now)
            .and_then(|period| period.plan.clone())
            .or_else(|| match subject.company {
                Company::Anthropic => stored_account(ctx, reference).and_then(|a| a.plan.clone()),
                Company::OpenAI | Company::Ollama | Company::Google => subject.plan.clone(),
            });
        let values = if subject.company.work_recorded() {
            finished_values
                .get(reference)
                .cloned()
                .unwrap_or_else(|| empty_values.clone())
        } else {
            Values::missing(&Missing::WorkNotRecorded.to_string(), ctx.settings)
        };
        let tokens_here = finished_tokens.get(reference).copied().unwrap_or(0);
        let own_months = subject_months
            .iter()
            .filter(|month| month.end <= ctx.now)
            .count() as u32;
        let periods_here = if subject.company == Company::Ollama {
            own_months
        } else {
            own_weeks
        };
        let (usage, paid) = subscriptions::usage_row(UsageInput {
            id: id.clone(),
            plan: plan.clone(),
            since,
            price,
            price_missing,
            stored_price,
            currency: currency.as_deref(),
            weeks: &subject_weeks,
            months: &subject_months,
            values: values.clone(),
            ollama: subject.company == Company::Ollama,
            now: ctx.now,
            finished_periods: periods_here,
        })?;
        let entry = UsageEntry {
            company: subject.company,
            row: usage,
            paid,
            periods: periods_here,
            ollama: subject.company == Company::Ollama,
            values,
            tokens: tokens_here,
        };
        if entry.counted() && subject.company != Company::Ollama && own_weeks < range_weeks {
            partial.push(Partial {
                id: id.clone(),
                weeks: own_weeks,
            });
        }
        usage_rows.push(entry);

        let owner_model_stats = owner_models.get(reference);
        let main = main_model(owner_model_stats, &company_models, subject.company);
        let main_upt = main
            .as_deref()
            .and_then(|model| model_upt(&company_models, subject.company, model));
        let learned_rates = learned.rates.get(reference).copied();
        let usable = learned_rates.is_some() && main.is_some() && main_upt.is_some();
        let worked = worked_of(
            ctx,
            &rhythm_tokens.records,
            subject.company,
            &mut worked_cache,
        );
        let basis_hours = learned_rates
            .zip(worked)
            .and_then(|(rates, hour)| usd_hours(rates.c.basis_usd, hour.usd));
        let lines = lines.get(reference).map(Vec::as_slice).unwrap_or(&[]);
        let main_points = if usable {
            points_for_main(
                ctx,
                range,
                subject,
                main_upt,
                lines,
                &tokens.records,
                &pools,
                &windows.records,
                &subject_months,
            )
        } else {
            Vec::new()
        };
        let trend = main
            .as_deref()
            .and_then(|model| change_of(&main_points, model, main_upt));
        let missing = match (learned_rates, main.as_deref()) {
            (None, _) => Some(no_factor_reason(subject.company)),
            (Some(_), None) => Some(NO_PRICED_MODEL_REASON.to_string()),
            (Some(_), Some(_)) if main_upt.is_none() => Some(NO_PRICED_MODEL_REASON.to_string()),
            (Some(_), Some(_)) => None,
        };
        let (full_hours, full_tokens) = match (missing.is_none(), learned_rates, worked) {
            (true, Some(rates), Some(hour)) => full_limit(&rates.c, hour),
            _ => (None, None),
        };
        plans.push(PlanRow {
            id: id.clone(),
            company: subject.company,
            plan: plan.clone(),
            full_hours,
            full_tokens,
            trend: if usable { trend.clone() } else { None },
            trend_missing: too_few_for_change(main_points.len(), period_word(subject.company)),
            basis_hours,
            thin: learned_rates.is_some_and(|rates| is_thin(&rates.c)),
            missing,
        });
        if let Some(rates) = learned_rates {
            if subject.company != Company::Ollama {
                weekly.insert(reference.clone(), rates);
            }
            if let (true, Some(model)) = (usable, main.as_deref()) {
                priced_main.insert(reference.clone());
                let limit = match subject.company {
                    Company::Ollama => "month",
                    Company::Anthropic | Company::OpenAI | Company::Google => "week",
                };
                per_percent.push(per_percent_row(
                    subject,
                    &id,
                    model,
                    limit,
                    &main_points,
                    trend,
                    &rates.c,
                    owner_model_stats,
                    &company_models,
                    worked,
                ));
                let session_factor = match subject.company {
                    Company::Ollama => None,
                    Company::Anthropic | Company::OpenAI | Company::Google => rates.s,
                };
                if let Some(session_factor) = session_factor {
                    let session = session_points(
                        ctx,
                        range,
                        subject,
                        main_upt,
                        lines,
                        &tokens.records,
                        &pools,
                    );
                    per_percent.push(PerPercent {
                        id: id.clone(),
                        limit: "5h".to_string(),
                        model: model.to_string(),
                        points: session.iter().map(|point| point.point.clone()).collect(),
                        change: change_of(&session, model, main_upt),
                        change_missing: too_few_for_change(session.len(), "weeks"),
                        now: now_of(
                            subject,
                            owner_model_stats,
                            &company_models,
                            session_factor.value,
                        ),
                        minutes: percent_minutes(&session_factor, worked),
                        window_holds_minutes: percent_minutes(&session_factor, worked)
                            .map(|minutes| 100.0 * minutes),
                    });
                }
            }
        }
    }

    // An account no longer stored that has periods in the range keeps its usage
    // row and its money; it gets no plan or figure row (deviation 5).
    for (reference, subject_periods) in &removed {
        let id = account_id(ctx, reference);
        let subject_weeks: Vec<&AccountWeek> = weeks
            .iter()
            .filter(|week| &week.account == reference)
            .collect();
        let own_weeks = subject_weeks.len() as u32;
        let (price, price_missing) = match valuer.price_at(reference, range.to) {
            Ok(price) => (Some(price), None),
            Err(missing) => (None, Some(missing.to_string())),
        };
        let since = subject_periods.first().map(|period| period.from);
        let plan = subject_periods
            .iter()
            .rev()
            .find(|period| period.from <= ctx.now)
            .and_then(|period| period.plan.clone());
        let values = finished_values
            .get(reference)
            .cloned()
            .unwrap_or_else(|| empty_values.clone());
        let tokens_here = finished_tokens.get(reference).copied().unwrap_or(0);
        let (usage, paid) = subscriptions::usage_row(UsageInput {
            id: id.clone(),
            plan,
            since,
            price,
            price_missing,
            stored_price: None,
            currency: currency.as_deref(),
            weeks: &subject_weeks,
            months: &[],
            values: values.clone(),
            ollama: false,
            now: ctx.now,
            finished_periods: own_weeks,
        })?;
        usage_rows.push(UsageEntry {
            company: Company::Anthropic,
            row: usage,
            paid,
            periods: own_weeks,
            ollama: false,
            values,
            tokens: tokens_here,
        });
    }

    let sizes = sizes_of(ctx, &subjects, &learned.rates);
    let times_paid = times_paid_of_counted(&usage_rows);
    let totals = totals_of(
        range_weeks,
        &usage_rows,
        &partial,
        currency.as_deref(),
        times_paid,
    );
    let (usage, usage_total) = usage_groups(&usage_rows, times_paid);
    let tokens_week = tokens_week_of(ctx, &subjects, &weekly, &priced_main, &company_models);

    Ok(SubscriptionsPage {
        totals,
        plans,
        sizes,
        usage,
        usage_total,
        per_percent,
        tokens_week,
        currency,
    })
}

fn chosen_subjects<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, q: &Query) -> Vec<RateAccount> {
    let mut subjects: Vec<RateAccount> = ctx
        .accounts
        .iter()
        .filter(|account| account_chosen(ctx, q, Some(&AccountRef::of(account))))
        .map(RateAccount::of)
        .collect();
    subjects.extend(
        ctx.provider_subscriptions
            .iter()
            .filter(|subscription| account_chosen(ctx, q, Some(&subscription.account)))
            .map(RateAccount::of_provider),
    );
    subjects.sort_by(|a, b| {
        a.company
            .cmp(&b.company)
            .then_with(|| a.account.cmp(&b.account))
    });
    subjects
}

/// Every stored subscription, whatever the query's chips: Claude accounts and
/// provider subscriptions (decision 9).
fn all_subjects<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>) -> Vec<RateAccount> {
    let mut subjects: Vec<RateAccount> = ctx.accounts.iter().map(RateAccount::of).collect();
    subjects.extend(
        ctx.provider_subscriptions
            .iter()
            .map(RateAccount::of_provider),
    );
    subjects.sort_by(|a, b| {
        a.company
            .cmp(&b.company)
            .then_with(|| a.account.cmp(&b.account))
    });
    subjects
}

fn ollama_months<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    q: &Query,
    subjects: &[RateAccount],
) -> Result<BTreeMap<AccountRef, Vec<MonthValued>>, StatsError> {
    let mut months = BTreeMap::new();
    for subject in subjects
        .iter()
        .filter(|subject| subject.company == Company::Ollama)
    {
        if !account_chosen(ctx, q, Some(&subject.account)) {
            continue;
        }
        months.insert(
            subject.account.clone(),
            subscription_months(ctx, range, &subject.account)?,
        );
    }
    Ok(months)
}

fn stored_price<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    reference: &AccountRef,
) -> Option<(f64, Option<String>)> {
    if let Some(account) = stored_account(ctx, reference) {
        return account
            .price
            .map(|amount| (amount, account.price_currency.clone()));
    }
    stored_subscription(ctx, reference).and_then(|subscription| {
        subscription
            .price
            .map(|amount| (amount, subscription.price_currency.clone()))
    })
}

fn token_count(record: &TokenRecord) -> u64 {
    let tokens = Tokens::from(record);
    tokens.input + tokens.out + tokens.cw + tokens.cr
}

fn is_thin(factor: &Factor) -> bool {
    (factor.high - factor.low) / 2.0 > THIN_SHARE * factor.value
}

/// The owner's USD per token of `model` in the range, over every subscription
/// and host (decision 8): `None` when a record of it has no price.
fn model_upt(
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
    company: Company,
    model: &str,
) -> Option<f64> {
    company_models.get(&company)?.get(model)?.usd_per_token()
}

/// The model of the subscription's own work in the range with the most API
/// dollars, among the models the owner's mix gives a price (decision 4).
fn main_model(
    stats: Option<&BTreeMap<String, ModelStat>>,
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
    company: Company,
) -> Option<String> {
    let stats = stats?;
    stats
        .iter()
        .filter(|(model, _)| model_upt(company_models, company, model).is_some())
        .max_by(|a, b| a.1.usd.total_cmp(&b.1.usd).then_with(|| b.0.cmp(a.0)))
        .map(|(model, _)| model.clone())
}

/// The owner's worked hour of `company` ([`worked_hour`]), once per company and
/// page.
fn worked_of<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    rhythm_tokens: &[Hosted<TokenRecord>],
    company: Company,
    cache: &mut BTreeMap<Company, Option<WorkedHour>>,
) -> Option<WorkedHour> {
    *cache
        .entry(company)
        .or_insert_with(|| worked_hour(rhythm_tokens, ctx.now, &ctx.zone, ctx.prices, company))
}

/// What a full limit holds of the owner's actual work: `100 / factor` API
/// dollars as hours worked and as tokens, at the worked hour's dollars and
/// tokens (1.4.1).
fn full_limit(factor: &Factor, hour: WorkedHour) -> (Option<f64>, Option<f64>) {
    let usd = 100.0 / factor.value;
    match usd_hours(usd, hour.usd) {
        Some(hours) => (Some(hours), Some(hours * hour.tokens)),
        None => (None, None),
    }
}

/// Minutes of actual work 1 % of a limit holds: `1 / factor` API dollars at
/// the worked hour's dollars.
fn percent_minutes(factor: &Factor, worked: Option<WorkedHour>) -> Option<f64> {
    worked.and_then(|hour| usd_hours(1.0 / factor.value, hour.usd).map(|hours| 60.0 * hours))
}

#[allow(clippy::too_many_arguments)]
fn points_for_main<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    subject: &RateAccount,
    upt: Option<f64>,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &rates::WindowPools,
    windows: &[Hosted<WindowRecord>],
    months: &[MonthValued],
) -> Vec<ChangePoint> {
    // A point needs a price per token, not a typical hour (decision 2 of M-2's
    // fix).
    let Some(upt) = upt else {
        return Vec::new();
    };
    let mut points = Vec::new();
    match subject.company {
        Company::Anthropic | Company::OpenAI | Company::Google => {
            for hosted in windows {
                let record = &hosted.record;
                if record.account != subject.account
                    || record.k != main_limit(subject.company)
                    || record.weekly_start() >= range.to
                    || record.end <= range.from
                {
                    continue;
                }
                push_point(
                    &mut points,
                    subject,
                    lines,
                    tokens,
                    pools,
                    ctx.prices,
                    record.weekly_start(),
                    record.end,
                    upt,
                    PairLimit::Main,
                );
            }
        }
        Company::Ollama => {
            for month in months {
                if month.end > ctx.now || month.start >= range.to || month.end <= range.from {
                    continue;
                }
                push_point(
                    &mut points,
                    subject,
                    lines,
                    tokens,
                    pools,
                    ctx.prices,
                    month.start,
                    month.end,
                    upt,
                    PairLimit::Main,
                );
            }
        }
    }
    points.sort_by_key(|point| point.point.week);
    points
}

#[allow(clippy::too_many_arguments)]
fn push_point(
    points: &mut Vec<ChangePoint>,
    subject: &RateAccount,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &rates::WindowPools,
    prices: &crate::money::Prices,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    upt: f64,
    limit: PairLimit,
) {
    let pairs = rates::window_pairs(subject, lines, tokens, pools, prices, limit, from, to);
    let Some(factor) = fit::factor(&pairs, None) else {
        return;
    };
    if factor.value <= 0.0 {
        return;
    }
    let tokens_here = 1.0 / (factor.value * upt);
    let low = 1.0 / (factor.high * upt);
    let high = (factor.low > 0.0).then(|| 1.0 / (factor.low * upt));
    points.push(ChangePoint {
        point: Point {
            week: from,
            tokens: tokens_here,
            low,
            high,
        },
        value: factor.value,
        pairs,
    });
}

/// Why a line of `points` points cannot tell a change: [`change_of`] needs
/// [`POINTS_BEFORE_CHANGE`] + 2 of them, so with fewer the line is not
/// "steady", it is too short. `None` with enough points, and with none (the
/// page says "no finished week" then). `period` is the points' unit, plural.
fn too_few_for_change(points: usize, period: &str) -> Option<String> {
    let needed = POINTS_BEFORE_CHANGE + 2;
    (points > 0 && points < needed)
        .then(|| format!("too few {period} to tell a change: {points} of the {needed} it needs"))
}

/// The unit of a subscription's points: Ollama counts months, the others
/// weeks.
fn period_word(company: Company) -> &'static str {
    match company {
        Company::Ollama => "months",
        Company::Anthropic | Company::OpenAI | Company::Google => "weeks",
    }
}

fn change_of(points: &[ChangePoint], model: &str, upt: Option<f64>) -> Option<Change> {
    let upt = upt?;
    if points.len() < POINTS_BEFORE_CHANGE + 2 {
        return None;
    }
    for k in (POINTS_BEFORE_CHANGE..points.len() - 1).rev() {
        let before = &points[k - POINTS_BEFORE_CHANGE..k];
        let low = before
            .iter()
            .map(|point| point.value)
            .fold(f64::INFINITY, f64::min);
        let high = before
            .iter()
            .map(|point| point.value)
            .fold(f64::NEG_INFINITY, f64::max);
        let a = points[k].value;
        let b = points[k + 1].value;
        // Two consecutive points are only two consecutive weeks when their
        // calendar dates are one week apart: a week without a point breaks the
        // run (fix 4 of the final review).
        if points[k]
            .point
            .week
            .date_naive()
            .checked_add_signed(Duration::days(7))
            != Some(points[k + 1].point.week.date_naive())
        {
            continue;
        }
        if !((a > high && b > high) || (a < low && b < low)) {
            continue;
        }
        let before_pairs: Vec<Pair> = before
            .iter()
            .flat_map(|point| point.pairs.iter().copied())
            .collect();
        let after_pairs: Vec<Pair> = points[k..]
            .iter()
            .flat_map(|point| point.pairs.iter().copied())
            .collect();
        let before_factor = fit::factor(&before_pairs, None)?;
        let after_factor = fit::factor(&after_pairs, None)?;
        if before_factor.value <= 0.0 || after_factor.value <= 0.0 {
            return None;
        }
        let before_tokens = 1.0 / (before_factor.value * upt);
        let after_tokens = 1.0 / (after_factor.value * upt);
        return Some(Change {
            model: model.to_string(),
            since: points[k].point.week,
            percent: (after_tokens - before_tokens) / before_tokens * 100.0,
            before: before_tokens,
            after: after_tokens,
        });
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn session_points<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    range: &Range,
    subject: &RateAccount,
    upt: Option<f64>,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &rates::WindowPools,
) -> Vec<ChangePoint> {
    let Some(upt) = upt else {
        return Vec::new();
    };
    let mut points = Vec::new();
    for week in week_columns(range, &ctx.zone) {
        let end = next_week_start(week, &ctx.zone);
        if end > ctx.now || week >= range.to {
            continue;
        }
        push_point(
            &mut points,
            subject,
            lines,
            tokens,
            pools,
            ctx.prices,
            week,
            end,
            upt,
            PairLimit::Session,
        );
    }
    points.sort_by_key(|point| point.point.week);
    points
}

#[allow(clippy::too_many_arguments)]
fn per_percent_row(
    subject: &RateAccount,
    id: &str,
    model: &str,
    limit: &str,
    points: &[ChangePoint],
    change: Option<Change>,
    factor: &Factor,
    stats: Option<&BTreeMap<String, ModelStat>>,
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
    worked: Option<WorkedHour>,
) -> PerPercent {
    PerPercent {
        id: id.to_string(),
        limit: limit.to_string(),
        model: model.to_string(),
        points: points.iter().map(|point| point.point.clone()).collect(),
        change,
        change_missing: too_few_for_change(points.len(), period_word(subject.company)),
        now: now_of(subject, stats, company_models, factor.value),
        minutes: percent_minutes(factor, worked),
        // Only the 5 h line holds a window (decision 7 of M-2's fix).
        window_holds_minutes: None,
    }
}

/// The tokens 1 % of a limit buys of each priced model the subscription ran,
/// the model with the most dollars first.
fn now_of(
    subject: &RateAccount,
    stats: Option<&BTreeMap<String, ModelStat>>,
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
    factor_value: f64,
) -> Vec<Now> {
    let Some(stats) = stats else {
        return Vec::new();
    };
    let mut rows: Vec<(String, f64)> = stats
        .iter()
        .filter(|(model, _)| model_upt(company_models, subject.company, model).is_some())
        .map(|(model, stat)| (model.clone(), stat.usd))
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .filter_map(|(model, _)| {
            let upt = model_upt(company_models, subject.company, &model)?;
            Some(Now {
                model,
                tokens: 1.0 / (factor_value * upt),
            })
        })
        .collect()
}

fn sizes_of<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    subjects: &[RateAccount],
    learned: &BTreeMap<AccountRef, Rates>,
) -> Sizes {
    // The reference is the Anthropic subscription with the largest basis among
    // the measured ones: a prior's value is not a measured size, so no plan is
    // measured against it (fix 3 of M-2).
    let reference = subjects
        .iter()
        .filter(|subject| subject.company == Company::Anthropic)
        .filter_map(|subject| learned.get(&subject.account).map(|rates| (subject, rates)))
        .filter(|(_, rates)| rates.c.measured)
        .max_by(|(a, a_rates), (b, b_rates)| {
            a_rates
                .c
                .basis_usd
                .total_cmp(&b_rates.c.basis_usd)
                .then_with(|| b.account.cmp(&a.account))
        });
    let mut rows = Vec::new();
    for subject in subjects {
        let Some(rates) = learned.get(&subject.account) else {
            continue;
        };
        if subject.company == Company::Ollama {
            continue;
        }
        // A factor that leans on its prior is not a measured size: its ratio to
        // the reference says nothing the work measured, so it has none. A row
        // that is not measured keeps its ratio by name (fix 3 of M-2).
        let (of_reference, low, high) = match (rates.c.measured, reference) {
            (true, Some((_, reference_rates))) => {
                let of_reference = reference_rates.c.value / rates.c.value;
                let reference_range = (reference_rates.c.high - reference_rates.c.low)
                    / (2.0 * reference_rates.c.value);
                let range = (rates.c.high - rates.c.low) / (2.0 * rates.c.value);
                let relative = (reference_range * reference_range + range * range).sqrt();
                (
                    Some(of_reference),
                    Some((of_reference * (1.0 - relative)).max(0.0)),
                    Some(of_reference * (1.0 + relative)),
                )
            }
            _ => (None, None, None),
        };
        let by_name = match (
            reference.and_then(|(reference, _)| reference.capacity),
            subject.capacity,
        ) {
            (Some(reference_capacity), Some(capacity)) if reference_capacity > 0.0 => {
                Some(capacity / reference_capacity)
            }
            _ => None,
        };
        rows.push(SizeRow {
            id: account_id(ctx, &subject.account),
            plan: subject.plan.clone(),
            of_reference,
            low,
            high,
            by_name,
            measured: rates.c.measured,
        });
    }
    Sizes {
        reference: reference.map(|(subject, _)| account_id(ctx, &subject.account)),
        rows,
    }
}

fn usage_groups(rows: &[UsageEntry], times_paid: Option<f64>) -> (Vec<UsageGroup>, UsageRow) {
    let mut groups = Vec::new();
    for company in Company::ALL {
        let company_rows: Vec<UsageRow> = rows
            .iter()
            .filter(|entry| entry.company == company)
            .map(|entry| entry.row.clone())
            .collect();
        let price_month = sum_opt(company_rows.iter().map(|row| row.price_month));
        groups.push(UsageGroup {
            company,
            count: company_rows.len() as u32,
            price_month,
            rows: company_rows,
        });
    }
    // The total row counts only the subscriptions with a finished period of
    // the range and recorded work, as `Totals` does (decision 4 of M-2's fix).
    let counted: Vec<&UsageEntry> = rows.iter().filter(|entry| entry.counted()).collect();
    let price_month = sum_opt(counted.iter().map(|entry| entry.row.price_month));
    let expired = sum_opt(counted.iter().map(|entry| entry.row.expired));
    let api = sum_opt(counted.iter().map(|entry| {
        full_half(
            entry.row.api,
            entry.values.api_partial,
            entry.values.api_missing.as_deref(),
        )
        .0
    }));
    let used_percent = used_share(&counted);
    let missing = total_missing(
        rows,
        &counted,
        price_month,
        api,
        used_percent,
        expired,
        times_paid,
    );
    (
        groups,
        UsageRow {
            id: "all".to_string(),
            plan: None,
            since: None,
            price_month,
            entered: None,
            used_percent,
            expired,
            api,
            times_paid,
            missing,
        },
    )
}

/// The used share of the counted rows, each weighted by its monthly price; a
/// counted row without a use or a price gives no share (decision 5 of M-2's
/// fix: a missing value is never 0).
fn used_share(counted: &[&UsageEntry]) -> Option<f64> {
    let mut weighted = 0.0;
    let mut total = 0.0;
    for entry in counted {
        let used = entry.row.used_percent?;
        let price = entry.row.price_month?;
        weighted += used * price;
        total += price;
    }
    (total > 0.0).then(|| weighted / total)
}

/// Why the total row has a figure missing: the reason of a counted row whose
/// same figure is missing. A row left out of the totals for having no finished
/// period of the range never marks the total, and a total whose figures are all
/// there has no reason (decision 2 of M-2's fix 2).
fn total_missing(
    rows: &[UsageEntry],
    counted: &[&UsageEntry],
    price_month: Option<f64>,
    api: Option<f64>,
    used_percent: Option<f64>,
    expired: Option<f64>,
    times_paid: Option<f64>,
) -> Option<String> {
    // Nothing counted: every figure above is `None` for the same reason, so the
    // total names that reason rather than a row's (fix 3 of M-2): finished
    // periods of work that is not recorded, or no finished period at all.
    if counted.is_empty() {
        return Some(if rows.iter().any(|entry| entry.periods > 0) {
            Missing::WorkNotRecorded.to_string()
        } else {
            subscriptions::NO_FINISHED_WEEK.to_string()
        });
    }
    let reason = |figure: fn(&UsageRow) -> Option<f64>| {
        counted
            .iter()
            .find(|entry| figure(&entry.row).is_none())
            .and_then(|entry| entry.row.missing.clone())
    };
    if price_month.is_none() {
        reason(|row| row.price_month)
    } else if api.is_none() {
        reason(|row| row.api)
    } else if used_percent.is_none() {
        reason(|row| row.used_percent)
    } else if expired.is_none() {
        reason(|row| row.expired)
    } else if times_paid.is_none() {
        reason(|row| row.times_paid).or_else(|| {
            counted
                .iter()
                .find(|entry| entry.paid.is_none_or(|paid| paid <= 0.0))
                .and_then(|entry| entry.row.missing.clone())
        })
    } else {
        None
    }
}

/// The API dollars over what was paid, over the rows the totals count: the
/// same figure for `Totals.timesPaid` and `usageTotal.timesPaid` (decision 3 of
/// M-2's fix). `None` when a counted row has no API value or no paid money, or
/// nothing was paid. A share, ratio or percent is computed only from halves
/// that are not partial (R3): a partial API half is a lower bound, so its row
/// gives no times-paid figure.
fn times_paid_of_counted(entries: &[UsageEntry]) -> Option<f64> {
    let mut api = 0.0;
    let mut paid = 0.0;
    let mut counted = false;
    for entry in entries.iter().filter(|entry| entry.counted()) {
        counted = true;
        let (row_api, _) = full_half(
            entry.row.api,
            entry.values.api_partial,
            entry.values.api_missing.as_deref(),
        );
        api += row_api?;
        paid += entry.paid?;
    }
    (counted && paid > 0.0).then(|| api / paid)
}

fn totals_of(
    range_weeks: u32,
    usage: &[UsageEntry],
    partial: &[Partial],
    currency: Option<&str>,
    times_paid: Option<f64>,
) -> Totals {
    let counted: Vec<&UsageEntry> = usage.iter().filter(|entry| entry.counted()).collect();
    let count = counted.len() as u32;
    let price_month = sum_opt(counted.iter().map(|entry| entry.row.price_month));
    let api_month = sum_opt(counted.iter().map(|entry| api_month_of(entry, currency)));
    // A counted row always knows its tokens; the sum is still `None` over no
    // row, as every figure of the totals is (fix 3 of M-2).
    let tokens_used_month = sum_opt(counted.iter().map(|entry| Some(tokens_month_of(entry))));
    Totals {
        count,
        price_month,
        api_month,
        times_paid,
        tokens_used_month,
        weeks: range_weeks,
        partial: partial.to_vec(),
    }
}

/// One counted subscription's API dollars a month, in the display currency:
/// `None` when it has no API value, its value is in another currency, or its
/// API half is partial. A share, ratio or percent is computed only from halves
/// that are not partial (R3): a month's share of a lower bound is no figure.
fn api_month_of(entry: &UsageEntry, currency: Option<&str>) -> Option<f64> {
    let (api, _) = full_half(
        entry.values.api,
        entry.values.api_partial,
        entry.values.api_missing.as_deref(),
    );
    let api = match api {
        Some(api) if entry.values.api_currency.as_deref() == currency => api,
        _ => return None,
    };
    let periods = entry.periods as f64;
    Some(if entry.ollama {
        api / periods
    } else {
        api / periods * 52.0 / 12.0
    })
}

/// One counted subscription's tokens a month: a month's share for Ollama, a
/// week's share of the year for the companies that count weeks.
fn tokens_month_of(entry: &UsageEntry) -> f64 {
    let periods = entry.periods as f64;
    if entry.ollama {
        entry.tokens as f64 / periods
    } else {
        entry.tokens as f64 / periods * 52.0 / 12.0
    }
}

/// The sum of a list that has no missing part: `None` when one value is
/// missing (decision 9 of M-2) and `None` over no value at all: a sum over
/// nothing is no figure, not 0 (fix 3 of M-2).
pub(super) fn sum_opt(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut seen = false;
    for value in values {
        sum += value?;
        seen = true;
    }
    seen.then_some(sum)
}

fn tokens_week_of<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    subjects: &[RateAccount],
    weekly: &BTreeMap<AccountRef, Rates>,
    priced_main: &BTreeSet<AccountRef>,
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
) -> TokensWeek {
    // Only a company with a row has columns. Ollama has a month line, not a
    // week row, so its models get no column (fix 3 of the final review).
    let row_companies: BTreeSet<Company> = subjects
        .iter()
        .filter(|subject| subject.company != Company::Ollama)
        .map(|subject| subject.company)
        .collect();
    let mut models: Vec<TokenModel> = Vec::new();
    for company in Company::ALL {
        if !row_companies.contains(&company) {
            continue;
        }
        let stats = company_models.get(&company);
        let mut seen: Vec<(String, u64)> = stats
            .into_iter()
            .flat_map(|stats| stats.iter())
            .filter(|(_, stat)| stat.tokens > 0)
            .map(|(model, stat)| (model.clone(), stat.tokens))
            .collect();
        seen.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (model, _) in &seen {
            models.push(TokenModel {
                model: model.clone(),
                company,
            });
        }
        if company == Company::Anthropic {
            for family in ["opus", "sonnet", "haiku", "fable"] {
                if let Some(model) = lineup_model(ctx, family) {
                    if !models.iter().any(|entry| entry.model == model) {
                        models.push(TokenModel { model, company });
                    }
                }
            }
        }
    }
    let missing_cells = |reason: &str| -> Vec<TokensWeekCell> {
        (0..models.len())
            .map(|_| TokensWeekCell::Missing {
                missing: reason.to_string(),
            })
            .collect()
    };
    let mut rows = Vec::new();
    for subject in subjects {
        if subject.company == Company::Ollama {
            continue;
        }
        let id = account_id(ctx, &subject.account);
        let Some(rates) = weekly.get(&subject.account) else {
            let reason = no_factor_reason(subject.company);
            rows.push(TokensWeekRow {
                id,
                cells: missing_cells(&reason),
                missing: Some(reason),
            });
            continue;
        };
        if !priced_main.contains(&subject.account) {
            rows.push(TokensWeekRow {
                id,
                cells: missing_cells(NO_PRICED_MODEL_REASON),
                missing: Some(NO_PRICED_MODEL_REASON.to_string()),
            });
            continue;
        }
        let figure = |upt: f64, estimated: bool| TokensWeekCell::Figure {
            tokens: 100.0 / (rates.c.value * upt),
            low: 100.0 / (rates.c.high * upt),
            high: (rates.c.low > 0.0).then(|| 100.0 / (rates.c.low * upt)),
            estimated,
            thin: is_thin(&rates.c),
            measured: rates.c.measured,
        };
        let mut cells = Vec::new();
        for model_entry in &models {
            let model_company = Company::of_model(&model_entry.model);
            if model_company != subject.company {
                cells.push(TokensWeekCell::Missing {
                    missing: format!("not offered by {}", subject.company.name()),
                });
                continue;
            }
            let seen = company_models
                .get(&subject.company)
                .and_then(|stats| stats.get(&model_entry.model))
                .is_some_and(|stat| stat.tokens > 0);
            let cell = if seen {
                // A model somebody ran is never estimated: its owner's mix
                // gives its price, or it has none (decision 8).
                match model_upt(company_models, subject.company, &model_entry.model) {
                    Some(upt) => figure(upt, false),
                    None => TokensWeekCell::Missing {
                        missing: format!("no price for {}", model_entry.model),
                    },
                }
            } else {
                match estimated_upt(ctx, &model_entry.model, subject.company, company_models) {
                    Ok(upt) => figure(upt, true),
                    Err(missing) => TokensWeekCell::Missing { missing },
                }
            };
            cells.push(cell);
        }
        rows.push(TokensWeekRow {
            id,
            cells,
            missing: None,
        });
    }
    TokensWeek { models, rows }
}

/// The newest priced model of a family. Its version is compared part by part
/// as numbers, not as a string; a dated key (`…-20260115`) and its alias of the
/// same version are one model, and the alias is the one kept (decision 15 of
/// M-2's fix).
fn lineup_model<Tz: TimeZone + Copy>(ctx: &Ctx<Tz>, family: &str) -> Option<String> {
    let provider = Company::Anthropic.price_list();
    let family_prefix = format!("claude-{family}-");
    let prefix = price_key(provider, &family_prefix);
    let provider_prefix = price_key(provider, "");
    let mut best: Option<(Vec<u64>, bool, String)> = None;
    for key in ctx
        .prices
        .models
        .keys()
        .filter(|key| key.starts_with(&prefix))
    {
        let Some(model) = key.strip_prefix(&provider_prefix) else {
            continue;
        };
        // A key that is not the one `model_price_key` gives its model is not
        // that model's price.
        if model_price_key(model) != *key {
            continue;
        }
        let Some(rest) = model.strip_prefix(&family_prefix) else {
            continue;
        };
        let (version, dated) = match rest.rsplit_once('-') {
            Some((version, last))
                if last.len() == 8 && last.chars().all(|c| c.is_ascii_digit()) =>
            {
                (version, true)
            }
            _ => (rest, false),
        };
        let Some(parts) = version
            .split('-')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
        else {
            continue;
        };
        let candidate = (parts, dated, model.to_string());
        best = match best {
            None => Some(candidate),
            Some((best_parts, best_dated, best_model)) => {
                if candidate.0 > best_parts
                    || (candidate.0 == best_parts && best_dated && !candidate.1)
                {
                    Some(candidate)
                } else {
                    Some((best_parts, best_dated, best_model))
                }
            }
        };
    }
    best.map(|(_, _, model)| model)
}

/// The owner's estimate of `model`'s dollars per token from the company's most
/// used model's token mix, at `model`'s own price. The main model needs a mix,
/// not a price of its own: a new model before models.dev lists it still gives
/// the mix (fix 2 of the final review). `Err` carries the reason a cell has no
/// estimate.
fn estimated_upt<Tz: TimeZone + Copy>(
    ctx: &Ctx<Tz>,
    model: &str,
    company: Company,
    company_models: &BTreeMap<Company, BTreeMap<String, ModelStat>>,
) -> Result<f64, String> {
    let no_mix = || format!("no token mix of {} in this range", company.name());
    let stats = company_models.get(&company).ok_or_else(no_mix)?;
    let (_, main_stat) = stats
        .iter()
        .max_by(|a, b| a.1.tokens.cmp(&b.1.tokens).then_with(|| b.0.cmp(a.0)))
        .ok_or_else(no_mix)?;
    let tokens: u64 = main_stat.mix.iter().sum();
    if tokens == 0 {
        return Err(format!(
            "the token mix of {}'s main model is empty",
            company.name()
        ));
    }
    let estimated = work_usd(
        model,
        main_stat.mix[0],
        main_stat.mix[1],
        main_stat.mix[2],
        main_stat.mix[3],
        ctx.prices,
        ctx.now,
    )
    .ok_or_else(|| format!("no price for {model}"))?;
    if estimated <= 0.0 {
        return Err(format!("{model}'s price gives no estimate"));
    }
    Ok(estimated / tokens as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::api::LimitKind;
    use crate::history::{EventKind, LimitSample, Source};
    use crate::money::{price_key, ModelPrice};
    use crate::project::ProjectKey;
    use crate::stats::tests::{all, provider_subscription, tiny, utc};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.001
    }

    fn factor(value: f64, low: f64, high: f64, basis_usd: f64) -> Factor {
        Factor {
            value,
            low,
            high,
            basis_usd,
            measured: basis_usd >= crate::burn::LEARN_MIN_USD,
        }
    }

    fn point(week: DateTime<Utc>, value: f64, gain: f64) -> ChangePoint {
        ChangePoint {
            point: Point {
                week,
                tokens: 0.0,
                low: 0.0,
                high: None,
            },
            value,
            pairs: vec![Pair { gain, usd: 100.0 }],
        }
    }

    /// A full limit at 0.5 % per dollar is 200 USD; with 50 USD and 2.5M
    /// tokens an hour worked, that is 4 hours and 10M tokens, and 1 % is 2.4
    /// minutes (1.4.1).
    #[test]
    fn a_full_limit_holds_its_dollars_in_hours_worked_and_tokens() {
        let hour = WorkedHour {
            usd: 50.0,
            tokens: 2_500_000.0,
        };
        let (hours, tokens) = full_limit(&factor(0.5, 0.4, 0.6, 100.0), hour);
        assert!(close(hours.expect("hours"), 4.0), "{hours:?}");
        assert!(close(tokens.expect("tokens"), 10_000_000.0), "{tokens:?}");
        let minutes = percent_minutes(&factor(0.5, 0.4, 0.6, 100.0), Some(hour));
        assert!(close(minutes.expect("minutes"), 2.4), "{minutes:?}");
        assert_eq!(percent_minutes(&factor(0.5, 0.4, 0.6, 100.0), None), None);
    }

    #[test]
    fn thin_when_the_range_is_wider_than_20_percent() {
        assert!(is_thin(&factor(1.0, 0.5, 1.6, 100.0)));
        assert!(!is_thin(&factor(1.0, 0.9, 1.1, 100.0)));
    }

    #[test]
    fn plan_sizes_are_against_the_most_measured_anthropic_plan() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let lb = RateAccount::of(tiny.lb());
        let ac = RateAccount::of(tiny.ac());
        let mut learned = BTreeMap::new();
        learned.insert(
            lb.account.clone(),
            Rates {
                c: factor(0.5, 0.4, 0.6, 300.0),
                s: None,
            },
        );
        learned.insert(
            ac.account.clone(),
            Rates {
                c: factor(0.25, 0.2, 0.3, 100.0),
                s: None,
            },
        );
        let sizes = sizes_of(&ctx, &[lb.clone(), ac.clone()], &learned);
        assert_eq!(sizes.reference.as_deref(), Some(tiny.lb().id.as_str()));
        assert_eq!(sizes.rows.len(), 2, "{:?}", sizes.rows);
        let ac_row = sizes
            .rows
            .iter()
            .find(|row| row.id == tiny.ac().id)
            .expect("AC's row");
        assert!(close(ac_row.of_reference.expect("a ratio"), 2.0));
        assert!(ac_row.measured);
    }

    #[test]
    fn an_unmeasured_plan_has_no_measured_size() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let measured = RateAccount {
            account: tiny.lb_ref(),
            company: Company::Anthropic,
            capacity: Some(5.0),
            plan: Some("Max".to_string()),
        };
        let unmeasured = RateAccount {
            account: tiny.ac_ref(),
            company: Company::Anthropic,
            capacity: Some(1.0),
            plan: Some("Pro".to_string()),
        };
        let mut learned = BTreeMap::new();
        learned.insert(
            measured.account.clone(),
            Rates {
                c: factor(0.5, 0.4, 0.6, 300.0),
                s: None,
            },
        );
        // 40 USD of priced work is below `LEARN_MIN_USD`: its factor leans on
        // its prior, so its ratio is no measured size.
        learned.insert(
            unmeasured.account.clone(),
            Rates {
                c: factor(0.25, 0.2, 0.3, 40.0),
                s: None,
            },
        );
        let sizes = sizes_of(&ctx, &[measured, unmeasured.clone()], &learned);
        assert_eq!(sizes.reference.as_deref(), Some(tiny.lb().id.as_str()));
        let row = sizes
            .rows
            .iter()
            .find(|row| row.id == tiny.ac().id)
            .expect("AC's row");
        assert!(!row.measured);
        assert_eq!(row.of_reference, None, "{row:?}");
        assert_eq!(row.low, None, "{row:?}");
        assert_eq!(row.high, None, "{row:?}");
        assert_eq!(row.by_name, Some(0.2), "the ratio by name stays: {row:?}");

        // With no measured Anthropic subscription there is no reference, and no
        // row has a ratio at all.
        let sizes = sizes_of(&ctx, &[unmeasured], &learned);
        assert_eq!(sizes.reference, None);
        assert_eq!(sizes.rows.len(), 1, "{:?}", sizes.rows);
        let row = &sizes.rows[0];
        assert!(!row.measured);
        assert_eq!(row.of_reference, None, "{row:?}");
        assert_eq!(row.low, None, "{row:?}");
        assert_eq!(row.high, None, "{row:?}");
        assert_eq!(row.by_name, None, "no reference, no size by name: {row:?}");
    }

    #[test]
    fn two_weeks_outside_the_range_of_the_8_before_is_a_change() {
        let start = utc("2026-09-07T00:00:00Z");
        let mut points: Vec<ChangePoint> = (0..8)
            .map(|index| point(start + Duration::weeks(index), 1.0, 1.0))
            .collect();
        points.push(point(start + Duration::weeks(8), 2.0, 2.0));
        points.push(point(start + Duration::weeks(9), 2.0, 2.0));
        let change = change_of(&points, "claude-opus-5-5", Some(1.0)).expect("a change");
        assert_eq!(change.since, start + Duration::weeks(8));
        assert!(close(change.before, 100.0));
        assert!(close(change.after, 50.0));
        assert!(close(change.percent, -50.0));
    }

    #[test]
    fn one_week_outside_is_no_change() {
        let start = utc("2026-09-07T00:00:00Z");
        let mut points: Vec<ChangePoint> = (0..8)
            .map(|index| point(start + Duration::weeks(index), 1.0, 1.0))
            .collect();
        points.push(point(start + Duration::weeks(8), 2.0, 2.0));
        points.push(point(start + Duration::weeks(9), 1.0, 1.0));
        assert!(change_of(&points, "claude-opus-5-5", Some(1.0)).is_none());
    }

    #[test]
    fn a_gap_between_two_weeks_is_no_change() {
        let start = utc("2026-09-07T00:00:00Z");
        let mut points: Vec<ChangePoint> = (0..8)
            .map(|index| point(start + Duration::weeks(index), 1.0, 1.0))
            .collect();
        points.push(point(start + Duration::weeks(8), 2.0, 2.0));
        // Week 9 has no point, so weeks 8 and 10 are not consecutive.
        points.push(point(start + Duration::weeks(10), 2.0, 2.0));
        assert!(change_of(&points, "claude-opus-5-5", Some(1.0)).is_none());
    }

    #[test]
    fn an_unpriced_new_model_does_not_make_the_lineup_not_offered() {
        let mut tiny = tiny();
        tiny.prices.take(
            BTreeMap::from([(
                price_key("anthropic", "claude-sonnet-5-5"),
                ModelPrice {
                    input: 3.0,
                    output: 15.0,
                    cache_write: 3.75,
                    cache_read: 0.3,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let ctx = tiny.ctx();
        let lb = RateAccount::of(tiny.lb());
        let rates = Rates::given(0.5, None);
        let weekly = BTreeMap::from([(lb.account.clone(), rates)]);
        let priced_main = BTreeSet::from([lb.account.clone()]);
        // The company's most used model is new and has no price; the older
        // opus is priced. The estimate for sonnet still uses the new model's
        // token mix and sonnet's own price.
        let company = BTreeMap::from([(
            Company::Anthropic,
            BTreeMap::from([
                (
                    "claude-opus-6-0".to_string(),
                    ModelStat {
                        tokens: 2_000_000,
                        usd: 0.0,
                        unpriced: true,
                        mix: [2_000_000, 0, 0, 0],
                    },
                ),
                (
                    "claude-opus-5-5".to_string(),
                    ModelStat {
                        tokens: 100_000,
                        usd: 2.0,
                        unpriced: false,
                        mix: [0, 100_000, 0, 0],
                    },
                ),
            ]),
        )]);
        let week = tokens_week_of(
            &ctx,
            std::slice::from_ref(&lb),
            &weekly,
            &priced_main,
            &company,
        );
        let row = week
            .rows
            .iter()
            .find(|row| row.id == account_id(&ctx, &lb.account))
            .expect("LB's week row");
        let index = week
            .models
            .iter()
            .position(|model| model.model == "claude-sonnet-5-5")
            .expect("sonnet's line-up column");
        match &row.cells[index] {
            TokensWeekCell::Figure { estimated, .. } => assert!(*estimated),
            TokensWeekCell::Missing { missing } => {
                panic!("sonnet is offered but its cell says {missing}")
            }
        }
    }

    #[test]
    fn a_cell_on_a_factor_that_leans_on_its_prior_is_not_measured() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let lb = RateAccount::of(tiny.lb());
        // No pair yet: the factor is the plan-size prior itself, with no range.
        let mut rates = Rates::given(0.5, None);
        rates.c = Factor {
            value: 0.5,
            low: 0.5,
            high: 0.5,
            basis_usd: 0.0,
            measured: false,
        };
        let weekly = BTreeMap::from([(lb.account.clone(), rates)]);
        let priced_main = BTreeSet::from([lb.account.clone()]);
        let company = BTreeMap::from([(
            Company::Anthropic,
            BTreeMap::from([(
                "claude-opus-5-5".to_string(),
                ModelStat {
                    tokens: 100_000,
                    usd: 2.0,
                    unpriced: false,
                    mix: [0, 100_000, 0, 0],
                },
            )]),
        )]);
        let week = tokens_week_of(
            &ctx,
            std::slice::from_ref(&lb),
            &weekly,
            &priced_main,
            &company,
        );
        let index = week
            .models
            .iter()
            .position(|model| model.model == "claude-opus-5-5")
            .expect("opus's column");
        match &week.rows[0].cells[index] {
            TokensWeekCell::Figure {
                measured,
                estimated,
                ..
            } => {
                assert!(!*measured, "a prior's factor is not measured");
                assert!(!*estimated, "opus ran, so its cell is not estimated");
            }
            TokensWeekCell::Missing { missing } => panic!("opus ran but its cell says {missing}"),
        }
        let json = serde_json::to_value(&week.rows[0].cells[index]).expect("cell json");
        assert_eq!(json["measured"], serde_json::json!(false));
    }

    #[test]
    fn an_empty_cell_says_why_and_a_company_without_a_row_has_no_column() {
        let tiny = tiny();
        let ctx = tiny.ctx();
        let lb = RateAccount::of(tiny.lb());
        let codex = RateAccount {
            account: crate::stats::tests::codex_ref(),
            company: Company::OpenAI,
            capacity: Some(1.0),
            plan: Some("Plus".to_string()),
        };
        let ollama = RateAccount {
            account: crate::money::ollama_subscription(),
            company: Company::Ollama,
            capacity: Some(1.0),
            plan: Some("Plus".to_string()),
        };
        let rates = Rates::given(0.5, None);
        let weekly = BTreeMap::from([
            (lb.account.clone(), rates),
            (codex.account.clone(), rates),
            (ollama.account.clone(), rates),
        ]);
        let priced_main = BTreeSet::from([
            lb.account.clone(),
            codex.account.clone(),
            ollama.account.clone(),
        ]);
        let company = BTreeMap::from([
            (
                Company::Anthropic,
                BTreeMap::from([
                    (
                        "claude-opus-5-5".to_string(),
                        ModelStat {
                            tokens: 1_000_000,
                            usd: 2.0,
                            unpriced: false,
                            mix: [0, 1_000_000, 0, 0],
                        },
                    ),
                    (
                        "claude-opus-6-0".to_string(),
                        ModelStat {
                            tokens: 500_000,
                            usd: 0.0,
                            unpriced: true,
                            mix: [500_000, 0, 0, 0],
                        },
                    ),
                ]),
            ),
            (
                Company::OpenAI,
                BTreeMap::from([(
                    "gpt-6.2-codex".to_string(),
                    ModelStat {
                        tokens: 1_000_000,
                        usd: 5.0,
                        unpriced: false,
                        mix: [1_000_000, 0, 0, 0],
                    },
                )]),
            ),
            (
                Company::Ollama,
                BTreeMap::from([(
                    "llama-4:cloud".to_string(),
                    ModelStat {
                        tokens: 1_000_000,
                        usd: 5.0,
                        unpriced: false,
                        mix: [1_000_000, 0, 0, 0],
                    },
                )]),
            ),
        ]);
        let week = tokens_week_of(
            &ctx,
            &[lb.clone(), codex.clone(), ollama],
            &weekly,
            &priced_main,
            &company,
        );
        assert!(
            !week
                .models
                .iter()
                .any(|model| model.company == Company::Ollama),
            "Ollama has no week row, so no Ollama column: {:?}",
            week.models
        );
        let index = |model: &str| {
            week.models
                .iter()
                .position(|entry| entry.model == model)
                .unwrap_or_else(|| panic!("{model}'s column"))
        };
        let lb_row = week
            .rows
            .iter()
            .find(|row| row.id == account_id(&ctx, &lb.account))
            .expect("LB's week row");
        assert_eq!(
            lb_row.cells[index("gpt-6.2-codex")],
            TokensWeekCell::Missing {
                missing: "not offered by Anthropic".to_string()
            }
        );
        assert_eq!(
            lb_row.cells[index("claude-opus-6-0")],
            TokensWeekCell::Missing {
                missing: "no price for claude-opus-6-0".to_string()
            }
        );
        let codex_row = week
            .rows
            .iter()
            .find(|row| row.id == account_id(&ctx, &codex.account))
            .expect("the OpenAI week row");
        assert_eq!(
            codex_row.cells[index("claude-opus-5-5")],
            TokensWeekCell::Missing {
                missing: "not offered by OpenAI".to_string()
            }
        );
        let json = serde_json::to_value(&week).expect("the week's JSON");
        assert_eq!(
            json["rows"][0]["cells"][index("claude-opus-6-0")]["missing"],
            "no price for claude-opus-6-0"
        );
    }

    #[test]
    fn an_unused_model_is_estimated_from_the_companys_main_mix() {
        let mut tiny = tiny();
        tiny.prices.take(
            BTreeMap::from([(
                price_key("anthropic", "claude-sonnet-5-5"),
                ModelPrice {
                    input: 3.0,
                    output: 15.0,
                    cache_write: 3.75,
                    cache_read: 0.3,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let ctx = tiny.ctx();
        let mut company = BTreeMap::new();
        company.insert(
            Company::Anthropic,
            BTreeMap::from([(
                "claude-opus-5-5".to_string(),
                ModelStat {
                    tokens: 1_000_000,
                    usd: 15.0,
                    unpriced: false,
                    mix: [1_000_000, 0, 0, 0],
                },
            )]),
        );
        let upt = estimated_upt(&ctx, "claude-sonnet-5-5", Company::Anthropic, &company)
            .expect("an estimate");
        assert!(close(upt, 3.0e-6), "{upt}");
    }

    fn values(api: f64) -> Values {
        Values {
            api: Some(api),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        }
    }

    fn usage_row(id: &str, price: f64, api: f64) -> UsageRow {
        UsageRow {
            id: id.to_string(),
            plan: None,
            since: None,
            price_month: Some(price),
            entered: None,
            used_percent: Some(50.0),
            expired: Some(price * 0.5),
            api: Some(api),
            times_paid: Some(api / price),
            missing: None,
        }
    }

    fn usage_entry(id: &str, price: f64, api: f64, paid: f64, periods: u32) -> UsageEntry {
        UsageEntry {
            company: Company::Anthropic,
            row: usage_row(id, price, api),
            paid: Some(paid),
            periods,
            ollama: false,
            values: values(api),
            tokens: 1_000_000,
        }
    }

    #[test]
    fn totals_scale_each_subscription_over_its_own_weeks() {
        let tiny = tiny();
        let rows = vec![
            usage_entry(&tiny.lb().id, 120.0, 40.0, 240.0, 2),
            usage_entry(&tiny.ac().id, 200.0, 60.0, 200.0, 1),
        ];
        let partial = vec![Partial {
            id: tiny.ac().id.clone(),
            weeks: 1,
        }];
        let times_paid = times_paid_of_counted(&rows);
        let totals = totals_of(2, &rows, &partial, Some("EUR"), times_paid);
        assert_eq!(totals.count, 2);
        assert_eq!(totals.weeks, 2);
        assert!(close(totals.price_month.expect("a price"), 320.0));
        assert!(close(
            totals.api_month.expect("an API month"),
            40.0 / 2.0 * 52.0 / 12.0 + 60.0 / 1.0 * 52.0 / 12.0
        ));
        assert!(close(
            totals.tokens_used_month.expect("tokens a month"),
            1_000_000.0 / 2.0 * 52.0 / 12.0 + 1_000_000.0 * 52.0 / 12.0
        ));
        // What was paid over the range's finished periods, not one month's price.
        assert!(close(times_paid.expect("times paid"), 100.0 / 440.0));
        assert_eq!(totals.partial, partial);
    }

    /// A partial API half is a lower bound, not a figure a ratio may divide: its
    /// times paid, its API month and the total's API value are all unknown with
    /// the half's reason (R3).
    #[test]
    fn a_partial_api_value_gives_no_times_paid_and_no_api_month() {
        let mut entry = usage_entry(
            "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34",
            100.0,
            40.0,
            100.0,
            2,
        );
        entry.values.api_partial = true;
        entry.values.api_missing = Some("no price for claude-x".to_string());

        assert_eq!(times_paid_of_counted(std::slice::from_ref(&entry)), None);
        assert_eq!(api_month_of(&entry, Some("EUR")), None);

        let totals = totals_of(2, std::slice::from_ref(&entry), &[], Some("EUR"), None);
        assert_eq!(totals.api_month, None);
        let (_, total) = usage_groups(std::slice::from_ref(&entry), None);
        assert_eq!(total.api, None);
    }

    #[test]
    fn the_total_used_share_counts_only_rows_with_a_finished_week() {
        let mut started = usage_entry("7c1e4b2a9d3f4e6ab8c52f0a1d7e9b34", 100.0, 30.0, 100.0, 2);
        started.row.used_percent = Some(60.0);
        let mut running = usage_entry("2b7d9c1e5a4f4c3b9e8d7f6a5b4c3d2e", 100.0, 0.0, 0.0, 0);
        // A running row has no use and no paid money: it must not count as 0.
        running.row.used_percent = None;
        running.row.api = None;
        running.paid = None;
        running.row.missing = Some("no finished week yet in this range".to_string());
        let rows = vec![started, running];
        let times_paid = times_paid_of_counted(&rows);
        let totals = totals_of(1, &rows, &[], Some("EUR"), times_paid);
        let (groups, total) = usage_groups(&rows, times_paid);
        assert_eq!(groups[0].rows.len(), 2, "the running row stays a row");
        assert_eq!(total.price_month, totals.price_month);
        assert_eq!(total.used_percent, Some(60.0));
        assert_eq!(total.price_month, Some(100.0));
        assert_eq!(total.times_paid, totals.times_paid);
        assert_eq!(total.times_paid, Some(30.0 / 100.0));
        assert_eq!(
            total.missing, None,
            "the total does not take the running row's reason"
        );
    }

    #[test]
    fn model_stat_sums_its_dollars() {
        let record = TokenRecord {
            t: utc("2026-11-18T09:00:00Z"),
            minutes: 60,
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
            model: "claude-opus-5-5".to_string(),
            input: 1_000_000,
            out: 0,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        };
        let mut stat = ModelStat::default();
        assert_eq!(stat.usd_per_token(), None);
        stat.add(&record, Some(4.0));
        stat.add(&record, Some(6.0));
        assert_eq!(stat.usd_per_token(), Some(5.0e-6));
        stat.add(&record, None);
        assert_eq!(stat.usd_per_token(), None);
    }

    #[test]
    fn the_chosen_chips_do_not_change_a_factor() {
        // AC is measured on "Pro"; LB, on the same plan, has no pair. Its
        // factor must take AC's measured one whether or not AC is a chip.
        let mut tiny = tiny();
        tiny.accounts[0].plan = Some("Pro".to_string());
        tiny.accounts[1].plan = Some("Pro".to_string());
        tiny.history
            .append(&Sample {
                t: utc("2026-11-10T08:00:00Z"),
                account: tiny.ac_ref(),
                src: Source::Poll,
                l: vec![LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 0.0,
                    r: Some(utc("2026-11-16T02:00:00Z")),
                    b: false,
                }],
            })
            .expect("AC's first sample");
        tiny.history
            .append(&Sample {
                t: utc("2026-11-10T09:00:00Z"),
                account: tiny.ac_ref(),
                src: Source::Poll,
                l: vec![LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 10.0,
                    r: Some(utc("2026-11-16T02:00:00Z")),
                    b: false,
                }],
            })
            .expect("AC's second sample");
        tiny.history
            .append(&TokenRecord {
                t: utc("2026-11-10T08:00:00Z"),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 0,
                out: 6_000_000,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            })
            .expect("AC's work");
        let range = Range {
            from: utc("2026-11-10T07:00:00Z"),
            to: tiny.now,
        };
        let with_ac = subscriptions_in(&tiny.ctx(), &range, &all()).expect("the page");
        let only_lb = Query {
            range: crate::stats::RangeKey::All,
            accounts: vec![tiny.lb().id.clone()],
            host: None,
        };
        let without_ac = subscriptions_in(&tiny.ctx(), &range, &only_lb).expect("the page");
        let lb_with = with_ac
            .plans
            .iter()
            .find(|row| row.id == tiny.lb().id)
            .expect("LB's row");
        let lb_without = without_ac
            .plans
            .iter()
            .find(|row| row.id == tiny.lb().id)
            .expect("LB's row alone");
        assert!(lb_with.missing.is_none(), "{:?}", lb_with);
        assert_eq!(lb_with, lb_without);
        assert!(!without_ac.plans.iter().any(|row| row.id == tiny.ac().id));
    }

    #[test]
    fn a_subscription_without_a_factor_is_a_row_with_its_reason() {
        let mut tiny = tiny();
        // A plan name no other subscription shares and no capacity: no prior.
        tiny.accounts[1].plan = Some("Team Standard".to_string());
        let range = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: tiny.now,
        };
        let page = subscriptions_in(&tiny.ctx(), &range, &all()).expect("the page");
        let ac_id = tiny.ac().id.clone();
        let row = page
            .plans
            .iter()
            .find(|row| row.id == ac_id)
            .expect("AC's row");
        assert_eq!(row.missing.as_deref(), Some(NO_FACTOR_REASON));
        assert_eq!((row.full_hours, row.full_tokens), (None, None));
        assert_eq!(row.basis_hours, None);
        assert_eq!(row.trend, None);
        assert_eq!(row.trend_missing, None);
        let week_row = page
            .tokens_week
            .rows
            .iter()
            .find(|row| row.id == ac_id)
            .expect("AC's week row");
        assert_eq!(week_row.missing.as_deref(), Some(NO_FACTOR_REASON));
        assert!(week_row.cells.iter().all(|cell| matches!(
            cell,
            TokensWeekCell::Missing { missing } if missing == NO_FACTOR_REASON
        )));
        assert!(!page.per_percent.iter().any(|line| line.id == ac_id));
    }

    #[test]
    fn a_removed_account_keeps_its_money_in_the_range() {
        let mut tiny = tiny();
        let removed = tiny.ac_ref();
        let removed_id = removed.acc.clone();
        let removed_store_id = tiny.ac().id.clone();
        tiny.history
            .append(&Event {
                t: utc("2026-11-01T00:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: removed.clone(),
                    amount: Some(140.0),
                    currency: Some("EUR".to_string()),
                },
            })
            .expect("AC's price");
        tiny.accounts
            .retain(|account| account.id != removed_store_id);
        let range = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        };
        let page = subscriptions_in(&tiny.ctx(), &range, &all()).expect("the page");
        let group = page
            .usage
            .iter()
            .find(|group| group.company == Company::Anthropic)
            .expect("Anthropic's group");
        let row = group
            .rows
            .iter()
            .find(|row| row.id == removed_id)
            .expect("the removed account's usage row");
        assert_eq!(row.plan.as_deref(), Some("Team Premium"));
        assert_eq!(row.price_month, Some(140.0));
        assert!(row.used_percent.is_some());
        assert!(row.expired.is_some());
        assert!(row.api.is_some());
        assert_eq!(row.missing, None);
        assert_eq!(page.totals.count, 2, "LB and the removed account");
        assert!(page.totals.price_month.expect("a price") >= 140.0);
        assert!(!page.plans.iter().any(|plan| plan.id == removed_id));
        assert!(!page.sizes.rows.iter().any(|size| size.id == removed_id));
        assert!(!page.per_percent.iter().any(|line| line.id == removed_id));
        assert!(!page
            .tokens_week
            .rows
            .iter()
            .any(|week| week.id == removed_id));
    }

    #[test]
    fn the_newest_model_is_by_version() {
        let mut tiny = tiny();
        let price = ModelPrice {
            input: 1.0,
            output: 1.0,
            cache_write: 0.0,
            cache_read: 0.0,
            tiered: false,
        };
        tiny.prices.take(
            BTreeMap::from([
                (price_key("anthropic", "claude-opus-9-9"), price.clone()),
                (price_key("anthropic", "claude-opus-10-1"), price.clone()),
                (
                    price_key("anthropic", "claude-opus-10-1-20260115"),
                    price.clone(),
                ),
                (price_key("anthropic", "claude-sonnet-4-5-20250929"), price),
            ]),
            utc("2026-01-01T00:00:00Z"),
        );
        let ctx = tiny.ctx();
        // 10-1 is newer than 9-9 though "9" sorts after "1"; the dated key of
        // the same version gives way to its alias.
        assert_eq!(
            lineup_model(&ctx, "opus").as_deref(),
            Some("claude-opus-10-1")
        );
        assert_eq!(
            lineup_model(&ctx, "sonnet").as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    #[test]
    fn a_full_subscription_end_to_end() {
        use chrono::NaiveDate;
        use tempfile::tempdir;

        use crate::history::HistoryDir;
        use crate::money::{Fx, PricePoint, Prices};
        use crate::paths::AppPaths;
        use crate::store::{AccountsFile, FileSecrets, LoginState, Settings, Store};
        use crate::types::OAuthAccount;

        let dir = tempdir().expect("a temp history");
        let now = utc("2026-11-19T10:00:00Z");
        let settings = Settings {
            currency: Some("EUR".to_string()),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        };
        let app = AppPaths::at(dir.path());
        let history = HistoryDir::open(&app, &settings).expect("the history");
        let account = crate::store::Account {
            id: "7c1e4b2a9d3f4e6ab8c52f0a1d7e9b34".to_string(),
            account_uuid: "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04".to_string(),
            organization_uuid: "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20".to_string(),
            label: "PR".to_string(),
            abbr: "PR".to_string(),
            color: "#3b82f6".to_string(),
            email: Some("pr@mail.example".to_string()),
            org_name: None,
            plan: Some("Max".to_string()),
            price: Some(180.0),
            price_currency: Some("EUR".to_string()),
            oauth_account: OAuthAccount(serde_json::Map::new()),
            added_at: utc("2026-11-01T00:00:00Z"),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            auto_burn: true,
        };
        let reference = AccountRef::of(&account);
        let accounts = vec![account.clone()];

        let mut prices = Prices::default();
        for (model, input, output) in [
            ("claude-opus-5-5", 4.0, 20.0),
            ("claude-sonnet-5-5", 3.0, 15.0),
        ] {
            prices.models.insert(
                price_key("anthropic", model),
                vec![PricePoint {
                    from: utc("2026-01-01T00:00:00Z"),
                    price: ModelPrice {
                        input,
                        output,
                        cache_write: input,
                        cache_read: input / 10.0,
                        tiered: false,
                    },
                }],
            );
        }
        let mut fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        fx.take(NaiveDate::from_ymd_opt(2026, 1, 1).expect("a date"), 1.0);

        history
            .append(&Event {
                t: utc("2026-11-02T02:00:00Z"),
                kind: EventKind::Live {
                    account: Some(reference.clone()),
                },
            })
            .expect("the live event");
        let weeks = [
            (
                utc("2026-11-02T02:00:00Z"),
                utc("2026-11-09T02:00:00Z"),
                50.0,
            ),
            (
                utc("2026-11-09T02:00:00Z"),
                utc("2026-11-16T02:00:00Z"),
                40.0,
            ),
        ];
        for (start, end, final_percent) in weeks {
            history
                .append(&WindowRecord {
                    account: reference.clone(),
                    k: LimitKind::WeeklyAll,
                    m: None,
                    start: Some(start),
                    end,
                    final_percent,
                    lost: Some(100.0 - final_percent),
                    last_sample: end - Duration::minutes(2),
                    plan: Some("Max".to_string()),
                    uncertain: false,
                    reset: false,
                    from: None,
                })
                .expect("the window");
        }
        // Samples every 5 minutes, the running week included.
        let mut samples = Vec::new();
        let mut running_start = None;
        for (index, (start, end, final_percent)) in weeks.iter().copied().enumerate() {
            let first = if index == 0 {
                start
            } else {
                start + Duration::minutes(5)
            };
            let mut at = first;
            while at <= end {
                let share = (at - start).num_seconds() as f64 / (end - start).num_seconds() as f64;
                samples.push(Sample {
                    t: at,
                    account: reference.clone(),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: final_percent * share,
                        r: Some(end),
                        b: false,
                    }],
                });
                at += Duration::minutes(5);
            }
            running_start = Some(end + Duration::minutes(5));
        }
        let week3_end = utc("2026-11-23T02:00:00Z");
        let mut at = running_start.expect("the running week");
        while at <= now {
            let start = utc("2026-11-16T02:00:00Z");
            let share =
                (at - start).num_seconds() as f64 / (week3_end - start).num_seconds() as f64;
            samples.push(Sample {
                t: at,
                account: reference.clone(),
                src: Source::Poll,
                l: vec![LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 15.0 * share,
                    r: Some(week3_end),
                    b: false,
                }],
            });
            at += Duration::minutes(5);
        }
        history.append_many(&samples).expect("the samples");

        let records = [
            (
                "2026-11-03T09:00:00Z",
                "claude-opus-5-5",
                1_000_000,
                3_000_000,
            ),
            (
                "2026-11-04T09:00:00Z",
                "claude-sonnet-5-5",
                2_000_000,
                2_000_000,
            ),
            ("2026-11-05T09:00:00Z", "claude-opus-5-5", 0, 3_000_000),
            ("2026-11-10T09:00:00Z", "claude-opus-5-5", 0, 2_000_000),
            (
                "2026-11-12T09:00:00Z",
                "claude-sonnet-5-5",
                1_000_000,
                1_000_000,
            ),
        ];
        let mut work = Vec::new();
        for (at, model, input, out) in records {
            let record = TokenRecord {
                t: utc(at),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: model.to_string(),
                input,
                out,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            };
            work.push(crate::money::api_usd(&record, &prices).expect("a price"));
            history.append(&record).expect("a token record");
        }
        let opus_usd = work[0] + work[2] + work[3];
        let opus_tokens = 4_000_000.0 + 3_000_000.0 + 2_000_000.0;
        let upt = opus_usd / opus_tokens;
        let week1_usd = work[0] + work[1] + work[2];
        let week2_usd = work[3] + work[4];
        let first_week2 = 40.0 * 5.0 / 10_080.0;
        let expected = [
            (utc("2026-11-02T02:00:00Z"), 50.0 / week1_usd),
            (
                utc("2026-11-09T02:00:00Z"),
                (40.0 - first_week2) / week2_usd,
            ),
        ];

        let file = AccountsFile {
            accounts: accounts.clone(),
            settings: settings.clone(),
            ..AccountsFile::default()
        };
        Store::new(app.clone(), Box::new(FileSecrets::new(app.secrets_dir())))
            .save(&file)
            .expect("accounts.json");
        prices.save(&app).expect("prices.json");
        fx.save(&app).expect("fx.json");
        let (projects, _) = crate::projects::load(&history, &accounts).expect("projects");

        let warnings = std::cell::RefCell::new(Vec::new());
        let ctx = Ctx {
            history: &history,
            accounts: &accounts,
            provider_subscriptions: &[],
            settings: &settings,
            prices: &prices,
            fx: &fx,
            projects: &projects,
            warnings: &warnings,
            now,
            zone: chrono::FixedOffset::east_opt(0).expect("UTC"),
        };
        let range = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: now,
        };
        let page = subscriptions_in(&ctx, &range, &all()).expect("the page");

        let plan = page.plans.first().expect("one plan row");
        assert_eq!(plan.company, Company::Anthropic);
        assert!(plan.full_hours.is_some_and(|hours| hours > 0.0), "{plan:?}");
        assert!(
            plan.full_tokens.is_some_and(|tokens| tokens > 0.0),
            "{plan:?}"
        );
        assert!(plan.basis_hours.is_some(), "{plan:?}");
        assert_eq!(plan.missing, None);
        assert!(page.totals.partial.is_empty(), "{:?}", page.totals.partial);
        assert_eq!(page.totals.times_paid, page.usage_total.times_paid);
        assert!(page.totals.times_paid.is_some());

        let week = page
            .per_percent
            .iter()
            .find(|line| line.limit == "week")
            .expect("the week line");
        assert_eq!(week.window_holds_minutes, None);
        assert_eq!(week.model, "claude-opus-5-5");
        assert_eq!(week.points.len(), expected.len());
        assert_eq!(week.points.len(), expected.len());
        let needed = POINTS_BEFORE_CHANGE + 2;
        let missing = (week.points.len() < needed).then(|| {
            format!(
                "too few weeks to tell a change: {} of the {needed} it needs",
                week.points.len()
            )
        });
        assert_eq!(plan.trend_missing, missing);
        assert_eq!(week.change_missing, missing);
        for point in &week.points {
            let (_, value) = expected
                .iter()
                .find(|(start, _)| *start == point.week)
                .expect("a point's window");
            let tokens = 1.0 / (value * upt);
            let relative = (point.tokens - tokens).abs() / tokens;
            assert!(relative < 1e-6, "{point:?} vs {tokens}");
        }

        let json = serde_json::to_value(&page).expect("the page's JSON");
        assert_eq!(json["plans"][0]["company"], "anthropic");
        assert_eq!(json["usage"][0]["company"], "anthropic");
        assert_eq!(json["tokensWeek"]["models"][0]["company"], "anthropic");
    }

    #[test]
    fn ollama_has_a_month_line_and_no_week_row() {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions
            .push(provider_subscription(&ollama, "OL", Some(20.0)));
        tiny.prices.take(
            BTreeMap::from([(
                price_key("ollama-cloud", "llama-4"),
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-01-01T00:00:00Z"),
        );
        let month = LimitKind::Other("monthly_credits".to_string());
        for (at, percent) in [
            ("2026-10-01T00:00:00Z", 0.0),
            ("2026-10-01T05:00:00Z", 50.0),
        ] {
            tiny.history
                .append(&Sample {
                    t: utc(at),
                    account: ollama.clone(),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: month.clone(),
                        m: None,
                        p: percent,
                        r: Some(utc("2026-11-01T00:00:00Z")),
                        b: false,
                    }],
                })
                .expect("the Ollama sample");
        }
        tiny.history
            .append(&crate::history::TokenRecord {
                t: utc("2026-10-01T02:00:00Z"),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: "llama-4:cloud".to_string(),
                input: 12_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            })
            .expect("the Ollama token");
        tiny.history
            .append(&WindowRecord {
                account: ollama.clone(),
                k: month,
                m: None,
                start: Some(utc("2026-10-01T00:00:00Z")),
                end: utc("2026-11-01T00:00:00Z"),
                final_percent: 50.0,
                lost: Some(50.0),
                last_sample: utc("2026-10-01T05:00:00Z"),
                plan: Some("Plus".to_string()),
                uncertain: false,
                reset: false,
                from: None,
            })
            .expect("the Ollama month");

        let range = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: utc("2026-11-19T10:00:00Z"),
        };
        let page = subscriptions_in(&tiny.ctx(), &range, &all()).expect("the page");
        let group = page
            .usage
            .iter()
            .find(|group| group.company == Company::Ollama)
            .expect("Ollama's group");
        assert_eq!(group.count, 1);
        assert!(page
            .per_percent
            .iter()
            .any(|line| line.limit == "month" && line.id == "ollama"));
        assert!(!page.per_percent.iter().any(|line| line.limit == "week"));
        // Ollama has no week, so no row of its own (the Anthropic rows stay,
        // each with its reason when it has no factor).
        assert!(
            !page.tokens_week.rows.iter().any(|row| row.id == "ollama"),
            "{:?}",
            page.tokens_week.rows
        );
    }

    /// The fixture plus one Ollama subscription whose only month still runs
    /// (1 Nov .. 1 Dec; "now" is 19 Nov), so no month of the range is finished.
    fn tiny_with_a_running_ollama_month() -> crate::stats::fixture::Tiny {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions
            .push(provider_subscription(&ollama, "OL", Some(20.0)));
        let month = LimitKind::Other("monthly_credits".to_string());
        for (at, percent) in [
            ("2026-11-01T00:00:00Z", 0.0),
            ("2026-11-15T05:00:00Z", 21.09),
        ] {
            tiny.history
                .append(&Sample {
                    t: utc(at),
                    account: ollama.clone(),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: month.clone(),
                        m: None,
                        p: percent,
                        r: Some(utc("2026-12-01T00:00:00Z")),
                        b: false,
                    }],
                })
                .expect("the Ollama sample");
        }
        tiny
    }

    /// The fixture plus one Ollama subscription with a finished October month at
    /// 50 % and a running November month at 21.09 %; "now" is 19 Nov.
    fn tiny_with_a_finished_and_a_running_ollama_month() -> crate::stats::fixture::Tiny {
        let mut tiny = tiny();
        let ollama = crate::money::ollama_subscription();
        tiny.provider_subscriptions
            .push(provider_subscription(&ollama, "OL", Some(20.0)));
        let month = LimitKind::Other("monthly_credits".to_string());
        for (at, percent, reset) in [
            ("2026-10-01T00:00:00Z", 0.0, "2026-11-01T00:00:00Z"),
            ("2026-10-01T05:00:00Z", 50.0, "2026-11-01T00:00:00Z"),
            ("2026-11-01T00:00:00Z", 0.0, "2026-12-01T00:00:00Z"),
            ("2026-11-15T05:00:00Z", 21.09, "2026-12-01T00:00:00Z"),
        ] {
            tiny.history
                .append(&Sample {
                    t: utc(at),
                    account: ollama.clone(),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: month.clone(),
                        m: None,
                        p: percent,
                        r: Some(utc(reset)),
                        b: false,
                    }],
                })
                .expect("the Ollama sample");
        }
        tiny.history
            .append(&WindowRecord {
                account: ollama,
                k: month,
                m: None,
                start: Some(utc("2026-10-01T00:00:00Z")),
                end: utc("2026-11-01T00:00:00Z"),
                final_percent: 50.0,
                lost: Some(50.0),
                last_sample: utc("2026-10-01T05:00:00Z"),
                plan: Some("Plus".to_string()),
                uncertain: false,
                reset: false,
                from: None,
            })
            .expect("the Ollama month");
        tiny
    }

    #[test]
    fn ollama_used_share_counts_finished_months_only() {
        let tiny = tiny_with_a_finished_and_a_running_ollama_month();
        let range = Range {
            from: utc("2026-10-01T00:00:00Z"),
            to: tiny.now,
        };
        let page = subscriptions_in(&tiny.ctx(), &range, &all()).expect("the page");
        let group = page
            .usage
            .iter()
            .find(|group| group.company == Company::Ollama)
            .expect("Ollama's group");
        let row = group.rows.first().expect("Ollama's row");
        assert_eq!(row.used_percent, Some(50.0), "{row:?}");
    }

    /// The range the lead's app used: the last 30 days.
    fn last_30_days(tiny: &crate::stats::fixture::Tiny) -> Range {
        Range {
            from: utc("2026-10-19T00:00:00Z"),
            to: tiny.now,
        }
    }

    #[test]
    fn a_row_without_a_finished_period_has_no_figures() {
        let tiny = tiny_with_a_running_ollama_month();
        let page = subscriptions_in(&tiny.ctx(), &last_30_days(&tiny), &all()).expect("the page");
        let group = page
            .usage
            .iter()
            .find(|group| group.company == Company::Ollama)
            .expect("Ollama's group");
        assert_eq!(group.count, 1);
        let row = group.rows.first().expect("Ollama's row");
        assert_eq!(row.used_percent, None, "{row:?}");
        assert_eq!(row.expired, None, "{row:?}");
        assert_eq!(row.api, None, "{row:?}");
        assert_eq!(row.times_paid, None, "{row:?}");
        assert_eq!(
            row.missing.as_deref(),
            Some("no finished month yet in this range"),
            "{row:?}"
        );
    }

    /// The range with no finished week yet: this week, Thursday 19 Nov.
    fn this_week(tiny: &crate::stats::fixture::Tiny) -> Range {
        Range {
            from: utc("2026-11-16T00:00:00Z"),
            to: tiny.now,
        }
    }

    #[test]
    fn nothing_counted_is_no_figure_not_zero() {
        let tiny = tiny();
        let page = subscriptions_in(&tiny.ctx(), &this_week(&tiny), &all()).expect("the page");
        assert_eq!(page.totals.count, 0);
        assert_eq!(page.totals.weeks, 0);
        assert_eq!(page.totals.price_month, None, "{:?}", page.totals);
        assert_eq!(page.totals.api_month, None, "{:?}", page.totals);
        assert_eq!(page.totals.times_paid, None, "{:?}", page.totals);
        assert_eq!(page.totals.tokens_used_month, None, "{:?}", page.totals);

        let total = &page.usage_total;
        assert_eq!(total.price_month, None, "{total:?}");
        assert_eq!(total.used_percent, None, "{total:?}");
        assert_eq!(total.expired, None, "{total:?}");
        assert_eq!(total.api, None, "{total:?}");
        assert_eq!(total.times_paid, None, "{total:?}");
        assert_eq!(
            total.missing.as_deref(),
            Some("no finished week yet in this range"),
            "{total:?}"
        );

        let openai = page
            .usage
            .iter()
            .find(|group| group.company == Company::OpenAI)
            .expect("OpenAI's group");
        assert_eq!(openai.count, 0);
        assert!(openai.rows.is_empty(), "{:?}", openai.rows);
        assert_eq!(openai.price_month, None, "no row is no sum: {openai:?}");

        // A group that has rows but no counted one keeps the sum of its rows'
        // prices.
        let anthropic = page
            .usage
            .iter()
            .find(|group| group.company == Company::Anthropic)
            .expect("Anthropic's group");
        assert_eq!(anthropic.count, 2);
        assert_eq!(anthropic.price_month, Some(320.0), "{anthropic:?}");
    }

    #[test]
    fn a_row_left_out_of_the_totals_does_not_mark_the_total() {
        let tiny = tiny_with_a_running_ollama_month();
        // One Claude account with finished weeks and the running Ollama
        // subscription: only the Claude row counts in the totals.
        let q = Query {
            range: crate::stats::RangeKey::Days30,
            accounts: vec![tiny.lb().id.clone(), "ollama".to_string()],
            host: None,
        };
        let page = subscriptions_in(&tiny.ctx(), &last_30_days(&tiny), &q).expect("the page");
        let anthropic = page
            .usage
            .iter()
            .find(|group| group.company == Company::Anthropic)
            .expect("the Anthropic group");
        assert_eq!(anthropic.rows.len(), 1, "one Claude account is chosen");
        let lb = &anthropic.rows[0];
        assert!(lb.missing.is_none(), "{lb:?}");
        assert_eq!(
            page.usage_total.missing, None,
            "the Ollama row's reason does not mark the total: {:?}",
            page.usage_total
        );
        assert_eq!(page.usage_total.price_month, lb.price_month);
        assert_eq!(page.usage_total.used_percent, lb.used_percent);
        assert_eq!(page.usage_total.expired, lb.expired);
        assert_eq!(page.usage_total.api, lb.api);
        assert_eq!(page.usage_total.times_paid, lb.times_paid);
        assert!(page.usage_total.api.is_some(), "{:?}", page.usage_total);
    }

    /// Google's work is not recorded, so its row has its price, use and unused
    /// money and an unknown work value, never a 0; the totals, which set work
    /// against price, leave it out.
    #[test]
    fn a_google_week_shows_its_use_and_stays_out_of_the_work_totals() {
        let without = tiny();
        let before =
            subscriptions_in(&without.ctx(), &last_30_days(&without), &all()).expect("the page");

        let mut tiny = tiny();
        let google = crate::stats::tests::google_ref();
        tiny.provider_subscriptions
            .push(provider_subscription(&google, "GO", Some(22.0)));
        tiny.history
            .append(&WindowRecord {
                account: google.clone(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-02T10:00:00Z")),
                end: utc("2026-11-09T10:00:00Z"),
                final_percent: 40.0,
                lost: Some(60.0),
                last_sample: utc("2026-11-08T10:00:00Z"),
                plan: None,
                uncertain: false,
                reset: false,
                from: None,
            })
            .expect("Google's week");
        let page = subscriptions_in(&tiny.ctx(), &last_30_days(&tiny), &all()).expect("the page");
        let group = page
            .usage
            .iter()
            .find(|group| group.company == Company::Google)
            .expect("Google's group");
        assert_eq!(group.count, 1);
        assert_eq!(group.price_month, Some(22.0));
        let row = group.rows.first().expect("Google's row");
        assert_eq!(row.used_percent, Some(40.0), "{row:?}");
        assert!(row.expired.is_some_and(|money| money > 0.0), "{row:?}");
        assert_eq!(row.api, None, "an unknown value is never 0: {row:?}");
        assert_eq!(row.times_paid, None, "{row:?}");
        let reason = "seatswap does not record the work agy does";
        assert_eq!(row.missing.as_deref(), Some(reason), "{row:?}");

        assert_eq!(page.usage_total, before.usage_total);
        assert_eq!(page.totals, before.totals);

        let plan = page
            .plans
            .iter()
            .find(|plan| plan.company == Company::Google)
            .expect("Google's plan card");
        assert_eq!(plan.missing.as_deref(), Some(reason), "{plan:?}");
        let week = page
            .tokens_week
            .rows
            .iter()
            .find(|row| row.id == "sam@mail.example")
            .expect("Google's week row");
        assert_eq!(week.missing.as_deref(), Some(reason), "{week:?}");

        // Google alone: the total names Google's reason, not a missing week.
        let q = Query {
            range: crate::stats::RangeKey::Days30,
            accounts: vec!["sam@mail.example".to_string()],
            host: None,
        };
        let alone = subscriptions_in(&tiny.ctx(), &last_30_days(&tiny), &q).expect("the page");
        assert_eq!(alone.usage_total.missing.as_deref(), Some(reason));
        assert_eq!(alone.usage_total.times_paid, None);
    }
}
