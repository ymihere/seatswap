//! The learning side of auto-burn (spec §2.3, R2 of 1.3): the weekly rhythm of
//! work, which account was live when, and the factors an account's limits cost
//! per API dollar of work.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::api::LimitKind;
use crate::history::{
    is_early_reset, same_reset, AccountRef, Event, EventKind, Hosted, Sample, TokenRecord,
};
use crate::money::{
    api_usd, is_plan_model, no_openai_login_warning, ollama_subscription, openai_subscription_at,
    openai_subscriptions, provider_switch_timelines, Company, Prices,
};
use crate::store::{Account, ProviderSubscription};

use crate::stats::{local_hour_of_week, local_hour_start};

use super::fit::{self, Factor};
use super::{
    week_weight, Rates, Rhythm, ASSUMED_WEEK_PERCENT_PER_USD_AT_1X, MIN_LIVE_SHARE, RATES_DAYS,
    RHYTHM_WEEKS,
};

/// Average API dollars of work per hour of the week from the Claude token
/// records of all hosts in `[now - 4 weeks, now)`, in the local time of
/// `zone`. A record of another provider (Codex, Ollama:
/// [`crate::money::is_plan_model`] is false) uses no Claude account, so it is
/// no work here and is left out. A record whose model has no price
/// ([`api_usd`] is `None`) is left out too: work of a model without a price is
/// left out of auto-burn's rhythm until the price list has it (documented
/// rule; [`unpriced_models`] names such models for [`rates`] and
/// [`super::live::learn`]). A clock hour of the week is the weighted
/// mean over the four weeks: the two newer weeks weigh 2, the two older 1. A
/// week without a record in that hour counts as 0 for it. A record's span is
/// spread over the local clock hours it covers, by the minutes each one holds,
/// so an offset that is not a whole hour splits it between two and a
/// daylight-saving change is followed.
pub fn rhythm<Tz: TimeZone + Copy>(
    tokens: &[Hosted<TokenRecord>],
    now: DateTime<Utc>,
    zone: &Tz,
    prices: &Prices,
) -> Rhythm {
    rhythm_of_company(tokens, now, zone, prices, Company::Anthropic)
}

/// [`rhythm`] of one company's models (R4): the hours that hold work of the
/// models [`Company::of_model`] gives the company. `rhythm` is the Anthropic
/// case, so auto-burn keeps counting Claude records only.
pub(crate) fn rhythm_of_company<Tz: TimeZone + Copy>(
    tokens: &[Hosted<TokenRecord>],
    now: DateTime<Utc>,
    zone: &Tz,
    prices: &Prices,
    company: Company,
) -> Rhythm {
    let mut weighted = [0.0f64; super::HOURS];
    let weight_sum: f64 = (0..RHYTHM_WEEKS).map(week_weight).sum();
    let unpriced = unpriced_models(tokens, prices);
    for hosted in tokens {
        if hosted.record.t >= now {
            continue;
        }
        let age = now.signed_duration_since(hosted.record.t);
        if age >= Duration::weeks(RHYTHM_WEEKS) {
            continue;
        }
        let week = age.num_days() / 7;
        let record = &hosted.record;
        if Company::of_model(&record.model) != company || unpriced.contains(&record.model) {
            continue;
        }
        let Some(usd) = api_usd(record, prices) else {
            continue;
        };
        spread(&mut weighted, record, zone, week_weight(week) * usd);
    }
    let mut usd = [0.0f64; super::HOURS];
    for (hour, sum) in usd.iter_mut().zip(weighted) {
        *hour = sum / weight_sum;
    }
    Rhythm { usd }
}

/// One company's work per hour you actually worked (1.4.1): the API dollars and
/// tokens of an average local clock hour that holds work of its models, over
/// `[now - 4 weeks, now)` and every host. An hour counts once however many
/// hosts worked in it. Unlike [`rhythm_of_company`], an hour without work
/// counts nowhere, so a model or a weekday used now and then does not thin the
/// average out. It filters the records as [`rhythm_of_company`] does: another
/// company's model, a model without a price and a record without a price are
/// left out. `None` when no hour holds such work.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WorkedHour {
    /// API dollars per worked hour, always above 0.
    pub usd: f64,
    /// Tokens (input, output, cache writes and reads) per worked hour.
    pub tokens: f64,
}

pub(crate) fn worked_hour<Tz: TimeZone + Copy>(
    tokens: &[Hosted<TokenRecord>],
    now: DateTime<Utc>,
    zone: &Tz,
    prices: &Prices,
    company: Company,
) -> Option<WorkedHour> {
    let unpriced = unpriced_models(tokens, prices);
    let mut hours: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    let mut usd_sum = 0.0;
    let mut token_sum = 0.0;
    for hosted in tokens {
        let record = &hosted.record;
        if record.t >= now || now.signed_duration_since(record.t) >= Duration::weeks(RHYTHM_WEEKS) {
            continue;
        }
        if Company::of_model(&record.model) != company || unpriced.contains(&record.model) {
            continue;
        }
        let Some(usd) = api_usd(record, prices) else {
            continue;
        };
        let counted = crate::transcripts::Tokens::from(record);
        usd_sum += usd;
        token_sum += (counted.input + counted.out + counted.cw + counted.cr) as f64;
        hours.extend(
            hour_shares(record, |at| local_hour_start(at, zone))
                .into_iter()
                .filter(|(_, share)| *share > 0.0)
                .map(|(hour, _)| hour),
        );
    }
    if hours.is_empty() || usd_sum <= 0.0 {
        return None;
    }
    let count = hours.len() as f64;
    Some(WorkedHour {
        usd: usd_sum / count,
        tokens: token_sum / count,
    })
}

/// Add the work of one record's span to the clock hours it covers, by the share
/// of the span each one holds. A whole-hour offset leaves it in one hour. The
/// hour boundaries and the hour-of-week index are the statistics' one rule.
fn spread<Tz: TimeZone + Copy>(
    weighted: &mut [f64; super::HOURS],
    record: &TokenRecord,
    zone: &Tz,
    usd: f64,
) {
    for (hour, share) in hour_shares(record, |at| local_hour_start(at, zone)) {
        weighted[local_hour_of_week(hour, zone)] += usd * share;
    }
}

/// How a record's [`TokenRecord::span`] is shared between the clock hours it
/// covers: each hour's start (as `hour_of` returns it) and the share of the
/// span that lies in it, in order, the shares summing to 1. `hour_of` gives the
/// start of the hour an instant lies in, so the caller chooses local or UTC
/// hours. The one place a token line is spread over hours; `work_in` uses its
/// timeline overlap instead because a line can be split by an account switch.
pub(crate) fn hour_shares(
    record: &TokenRecord,
    hour_of: impl Fn(DateTime<Utc>) -> DateTime<Utc>,
) -> Vec<(DateTime<Utc>, f64)> {
    let start = record.t;
    let end = start + record.span();
    let span = seconds_between(start, end);
    let mut shares: Vec<(DateTime<Utc>, f64)> = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let hour = hour_of(cursor);
        let to = (hour + Duration::hours(1)).min(end);
        let share = seconds_between(cursor, to) / span;
        match shares.last_mut() {
            Some((last, sum)) if *last == hour => *sum += share,
            _ => shares.push((hour, share)),
        }
        cursor = to;
    }
    shares
}

/// Which account was live on `host` at `t`: the last `live` or `switch` event of
/// that host at or before `t`, the one rule of `Timeline::at`. `None` when no
/// such event exists, or when the last one says no account was live.
pub fn account_at(events: &[Hosted<Event>], host: &str, t: DateTime<Utc>) -> Option<AccountRef> {
    Timeline::of(events, host).at(t).cloned()
}

/// One subscription the factors are learned for: how the history names it, the
/// company whose models are its work, the capacity of its plan
/// ([`Account::capacity`]: 5 for Max, 20 for Ultra, 1 for Pro; `None` when its
/// tier names no multiplier) and its plan's name ([`Account::plan`], compared
/// exactly when one subscription takes another's factor, within its company
/// only).
#[derive(Debug, Clone, PartialEq)]
pub struct RateAccount {
    pub account: AccountRef,
    pub company: Company,
    pub capacity: Option<f64>,
    pub plan: Option<String>,
}

impl RateAccount {
    /// A Claude account: its models are Anthropic's.
    pub fn of(account: &Account) -> RateAccount {
        RateAccount {
            account: AccountRef::of(account),
            company: Company::Anthropic,
            capacity: account.capacity(),
            plan: account.plan.clone(),
        }
    }

    /// A provider subscription (decision 1 of M-2): its company is the one its
    /// account's org names, its plan the stored plan's. A provider has no
    /// capacity by name: its week is learned from its own readings only.
    pub fn of_provider(subscription: &ProviderSubscription) -> RateAccount {
        RateAccount {
            account: subscription.account.clone(),
            company: Company::of_account(&subscription.account),
            capacity: None,
            plan: subscription.plan.clone(),
        }
    }
}

/// The limit whose percent is the main factor `c` of a company (decision 1 of
/// M-2): `weekly_all` for Anthropic, OpenAI and Google, Ollama's `monthly_credits`.
pub(crate) fn main_limit(company: Company) -> LimitKind {
    match company {
        Company::Anthropic | Company::OpenAI | Company::Google => LimitKind::WeeklyAll,
        Company::Ollama => LimitKind::Other("monthly_credits".to_string()),
    }
}

/// What [`rates`] learned: every account's rates, and the Claude models among
/// the token records that have no price. Their work is left out of the rates
/// and the rhythm until the price list has them; the caller names each once.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedRates {
    pub rates: BTreeMap<AccountRef, Rates>,
    pub unpriced: BTreeSet<String>,
    /// One sentence per host on which an OpenAI record could belong to no
    /// stored login (several logins and no `providerSwitch`): its work is left
    /// out of every subscription's pairs, and the caller says so once per host.
    pub openai_without_login: BTreeMap<String, String>,
}

impl LearnedRates {
    /// One warning per Claude model without a price, not one per record: the
    /// words [`super::live::learn`] adds to its warnings. Then one per host
    /// whose OpenAI records belong to no stored login.
    pub fn warnings(&self) -> Vec<String> {
        self.unpriced
            .iter()
            .map(|model| unpriced_warning(model))
            .chain(self.openai_without_login.values().cloned())
            .collect()
    }
}

/// The one wording of a model without a price, shared by [`LearnedRates`] and
/// [`super::live::learn`] so the warning is said the same way with and without
/// session reading.
pub(crate) fn unpriced_warning(model: &str) -> String {
    format!("no price for {model}: its work is left out of auto-burn's rates and rhythm")
}

/// Every account's rates (spec §2.3, R2 of 1.3): its week factor `c` and its
/// 5 h factor `s`, each fitted by [`fit::factor`] from the account's pairs of
/// readings with an assumed factor as prior.
///
/// The pairs: the account's consecutive samples in the last [`RATES_DAYS`] days
/// that lie in one weekly window and are at most 6 h apart ([`pairs`]); a pair
/// counts only when the account was live on some host for at least
/// [`MIN_LIVE_SHARE`] of its time (the union over hosts, documented rule): a
/// rise outside that time comes from use seatswap does not see (claude.ai,
/// another machine). A pair's gain is the weekly percent gained, its dollars
/// the work done while the account was live ([`work_in`]: a record spread
/// evenly over its own span, counted for the part its host had the account
/// live). A pair that holds work of a Claude model without a price is left out
/// whole, never valued as 0. The pairs inside one 5 h window give the
/// session's factor the same way.
///
/// The prior of an account and a limit (documented rule), from the factors of
/// the **other** accounts that are measured from their own pairs alone (no
/// circular priors):
/// 1. pooled over those with the same plan name, `Σ gain / Σ usd`;
/// 2. else pooled over those with a capacity, `Σ (gain × capacity) / Σ usd`,
///    divided by this account's capacity;
/// 3. else, for the week only, [`ASSUMED_WEEK_PERCENT_PER_USD_AT_1X`] divided
///    by this account's capacity;
/// 4. else no prior: the account has a factor only once it is measured.
///
/// `s` is learned only for an account whose latest sample has a session limit.
/// An account without `c` has no entry.
pub fn rates(
    accounts: &[RateAccount],
    samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    events: &[Hosted<Event>],
    prices: &Prices,
    now: DateTime<Utc>,
) -> LearnedRates {
    rates_in(
        accounts,
        samples,
        tokens,
        events,
        &[],
        prices,
        now - Duration::days(RATES_DAYS),
        now,
    )
}

/// [`rates`] with an explicit span and provider subscriptions (decision 1 of
/// M-2): the one estimator of every subscription's factors. Auto-burn calls
/// [`rates`] with Claude accounts and no provider subscriptions, so its rates
/// are exactly what they were; the Subscriptions page calls this over its range
/// for every stored subscription, whatever chips the page shows, so a chip
/// never changes a factor or a prior.
///
/// A provider subscription's work is its company's records that belong to it:
/// an OpenAI record belongs to the subscription
/// [`openai_subscription_at`] gives it, an Ollama record to
/// [`ollama_subscription`]. A provider pair counts only when every record of
/// that company inside the pair's span belongs to this subscription (the
/// providers' version of [`MIN_LIVE_SHARE`], documented rule). A record of
/// another company never counts.
///
/// The priors stay within a company: rule 1 (same plan name) pools only
/// subscriptions of the same company, and rules 2 and 3 (capacity, assumed
/// factor) exist for Anthropic only. A provider without a same-plan measured
/// subscription gets a factor only once it is measured.
/// The timelines and stored OpenAI logins a page builds once for all its
/// windows and subscriptions (decision 16 of M-2's fix): [`window_pairs`] and
/// [`rates_in_with`] take them instead of rebuilding them per window.
pub(crate) struct WindowPools {
    pub(crate) timelines: HashMap<String, Timeline>,
    pub(crate) provider_timelines: HashMap<String, Timeline>,
    pub(crate) openai: BTreeSet<AccountRef>,
}

impl WindowPools {
    pub(crate) fn of(
        events: &[Hosted<Event>],
        provider_subscriptions: &[ProviderSubscription],
    ) -> WindowPools {
        WindowPools {
            timelines: timelines(events),
            provider_timelines: provider_switch_timelines(events),
            openai: openai_subscriptions(provider_subscriptions),
        }
    }
}

/// One subscription's samples, sorted by time then host, so a page builds the
/// list once and every window cuts it (decision 16 of M-2's fix).
pub(crate) fn sample_lines<'a>(
    account: &AccountRef,
    samples: &'a [Hosted<Sample>],
) -> Vec<&'a Hosted<Sample>> {
    let mut lines: Vec<&Hosted<Sample>> = samples
        .iter()
        .filter(|hosted| &hosted.record.account == account)
        .collect();
    lines.sort_by(|a, b| {
        a.record
            .t
            .cmp(&b.record.t)
            .then_with(|| a.host.cmp(&b.host))
    });
    lines
}

/// [`sample_lines`] for every subscription, built once per page (decision 16
/// of M-2's fix).
pub(crate) fn sample_lines_by_account<'a>(
    subjects: &[RateAccount],
    samples: &'a [Hosted<Sample>],
) -> BTreeMap<AccountRef, Vec<&'a Hosted<Sample>>> {
    subjects
        .iter()
        .map(|subject| {
            (
                subject.account.clone(),
                sample_lines(&subject.account, samples),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rates_in(
    subjects: &[RateAccount],
    samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    events: &[Hosted<Event>],
    provider_subscriptions: &[ProviderSubscription],
    prices: &Prices,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> LearnedRates {
    let pools = WindowPools::of(events, provider_subscriptions);
    let lines = sample_lines_by_account(subjects, samples);
    rates_in_with(subjects, &lines, samples, tokens, &pools, prices, from, to)
}

/// [`rates_in`] with the timelines, logins and sorted samples a page already
/// built (decision 16 of M-2's fix).
#[allow(clippy::too_many_arguments)]
pub(crate) fn rates_in_with(
    subjects: &[RateAccount],
    lines: &BTreeMap<AccountRef, Vec<&Hosted<Sample>>>,
    samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &WindowPools,
    prices: &Prices,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> LearnedRates {
    let measured: Vec<(&RateAccount, Measured)> = subjects
        .iter()
        .map(|entry| {
            let empty = Vec::new();
            let lines = lines.get(&entry.account).unwrap_or(&empty);
            let has_session = samples
                .iter()
                .filter(|hosted| hosted.record.account == entry.account)
                .filter(|hosted| hosted.record.t >= from && hosted.record.t <= to)
                .max_by_key(|hosted| hosted.record.t)
                .is_some_and(|hosted| hosted.record.limit(&LimitKind::Session).is_some());
            let measured = measure(entry, lines, tokens, pools, prices, from, to, has_session);
            (entry, measured)
        })
        .collect();

    let mut openai_without_login = BTreeMap::new();
    for (_, own) in &measured {
        openai_without_login.extend(own.openai_without_login.clone());
    }
    let mut learned = BTreeMap::new();
    for (entry, own) in &measured {
        let week_prior = prior(
            entry,
            &measured,
            |m| &m.week,
            Some(ASSUMED_WEEK_PERCENT_PER_USD_AT_1X),
        );
        let Some(c) = usable(&own.week, week_prior) else {
            continue;
        };
        let s = if own.has_session {
            usable(&own.session, prior(entry, &measured, |m| &m.session, None))
        } else {
            None
        };
        learned.insert(entry.account.clone(), Rates { c, s });
    }
    LearnedRates {
        rates: learned,
        unpriced: unpriced_models(tokens, prices),
        openai_without_login,
    }
}

/// The factor of an account's own pairs and its prior, when it may be used:
/// with a prior while it is not measured, and once it is measured on its own
/// pairs alone (the prior dropped). A measured factor that is not positive is
/// unknown, prior or not.
fn usable(pairs: &[fit::Pair], prior: Option<f64>) -> Option<Factor> {
    fit::factor(pairs, prior).filter(|factor| prior.is_some() || factor.measured)
}

/// Whether pairs give a measured factor on their own, without a prior: the
/// accounts another account's prior may come from.
fn measured_alone(pairs: &[fit::Pair]) -> bool {
    fit::factor(pairs, None).is_some_and(|factor| factor.measured)
}

/// The prior of `entry` for one limit (see [`rates`]): `pairs_of` picks the
/// limit's pairs, `assumed_at_1x` is the built-in factor at capacity 1 (the
/// week's; the 5 h limit has none).
fn prior(
    entry: &RateAccount,
    all: &[(&RateAccount, Measured)],
    pairs_of: fn(&Measured) -> &Vec<fit::Pair>,
    assumed_at_1x: Option<f64>,
) -> Option<f64> {
    let others: Vec<(&RateAccount, &[fit::Pair])> = all
        .iter()
        .filter(|(other, _)| other.account != entry.account)
        .map(|(other, measured)| (*other, pairs_of(measured).as_slice()))
        .filter(|(_, pairs)| measured_alone(pairs))
        .collect();

    if let Some(plan) = &entry.plan {
        let (mut gain, mut usd) = (0.0, 0.0);
        for (_, pairs) in others.iter().filter(|(other, _)| {
            other.company == entry.company && other.plan.as_deref() == Some(plan.as_str())
        }) {
            for pair in *pairs {
                gain += pair.gain;
                usd += pair.usd;
            }
        }
        if usd > 0.0 {
            return Some(gain / usd);
        }
    }

    // The capacity rules are Anthropic's only: an OpenAI or Ollama
    // subscription has no capacity by name, so it waits to be measured.
    if entry.company != Company::Anthropic {
        return None;
    }
    let capacity = entry.capacity?;
    let (mut gain, mut usd) = (0.0, 0.0);
    for (other, pairs) in &others {
        let Some(other_capacity) = other.capacity else {
            continue;
        };
        for pair in *pairs {
            gain += pair.gain * other_capacity;
            usd += pair.usd;
        }
    }
    if usd > 0.0 {
        return Some(gain / usd / capacity);
    }
    assumed_at_1x.map(|assumed| assumed / capacity)
}

/// The Claude models among `tokens` that have no price: [`api_usd`] is `None`
/// for one of their records. The one source of "a model without a price" for
/// [`rhythm`], [`rates`] and [`super::live::learn`].
pub(super) fn unpriced_models(tokens: &[Hosted<TokenRecord>], prices: &Prices) -> BTreeSet<String> {
    tokens
        .iter()
        .map(|hosted| &hosted.record)
        .filter(|record| is_plan_model(&record.model) && api_usd(record, prices).is_none())
        .map(|record| record.model.clone())
        .collect()
}

/// One subscription's usable pairs (see [`rates_in`]), as the fit takes them.
struct Measured {
    week: Vec<fit::Pair>,
    session: Vec<fit::Pair>,
    /// The subscription's latest sample in `[from, to]` has a session limit.
    has_session: bool,
    /// One sentence per host whose OpenAI records no stored login can be named
    /// for: see [`LearnedRates::openai_without_login`].
    openai_without_login: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
fn measure(
    subject: &RateAccount,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &WindowPools,
    prices: &Prices,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    has_session: bool,
) -> Measured {
    let (owned, openai_without_login) =
        owned_pairs(subject, lines, tokens, pools, prices, from, to);
    let mut week = Vec::new();
    let mut session = Vec::new();
    for (pair, work) in owned {
        if work.unpriced {
            continue;
        }
        week.push(fit::Pair {
            gain: pair.week,
            usd: work.usd,
        });
        if let Some(gain) = pair.session {
            session.push(fit::Pair {
                gain,
                usd: work.usd,
            });
        }
    }
    Measured {
        week,
        session,
        has_session,
        openai_without_login,
    }
}

/// The limits [`window_pairs`] can pool a window's pairs for (decision 6 of
/// M-2): the main limit's percent, or the 5 h session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairLimit {
    Main,
    Session,
}

/// The pairs of one subscription inside one window, through the same ownership
/// and work rules as [`rates_in`] (decisions 1 and 6 of M-2): the fit is
/// [`fit::factor`] with no prior. The page's per-window points and their pooled
/// "before"/"after" factors come from here, never from a second pair or work
/// function. `lines` are [`sample_lines`] of the subscription and `pools` the
/// page's [`WindowPools`], both built once (decision 16 of M-2's fix).
#[allow(clippy::too_many_arguments)]
pub(crate) fn window_pairs(
    subject: &RateAccount,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &WindowPools,
    prices: &Prices,
    limit: PairLimit,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<fit::Pair> {
    let (owned, _) = owned_pairs(subject, lines, tokens, pools, prices, from, to);
    owned
        .into_iter()
        .filter_map(|(pair, work)| {
            if work.unpriced {
                return None;
            }
            match limit {
                PairLimit::Main => Some(fit::Pair {
                    gain: pair.week,
                    usd: work.usd,
                }),
                PairLimit::Session => pair.session.map(|gain| fit::Pair {
                    gain,
                    usd: work.usd,
                }),
            }
        })
        .collect()
}

/// The pairs of one subscription whose work belongs to it, with that work.
/// Claude pairs keep auto-burn's rule (live for [`MIN_LIVE_SHARE`] of the
/// pair); a provider pair is kept only when a record of its company inside the
/// pair's span belongs to this subscription and none belongs to another.
#[allow(clippy::too_many_arguments)]
fn owned_pairs(
    subject: &RateAccount,
    lines: &[&Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    pools: &WindowPools,
    prices: &Prices,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> (Vec<(SamplePair, SpanWork)>, BTreeMap<String, String>) {
    let raw = pairs(lines, from, to, &main_limit(subject.company));
    let spans: Vec<(DateTime<Utc>, DateTime<Utc>)> =
        raw.iter().map(|pair| (pair.from, pair.to)).collect();
    let ollama = ollama_subscription();
    let provider = ProviderWork {
        company: subject.company,
        subscription: match subject.company {
            Company::Ollama => &ollama,
            Company::Anthropic | Company::OpenAI | Company::Google => &subject.account,
        },
        timelines: &pools.provider_timelines,
        openai: &pools.openai,
    };
    let owner = match subject.company {
        Company::Anthropic => Ownership::Claude(&subject.account),
        Company::OpenAI | Company::Ollama | Company::Google => Ownership::Provider(&provider),
    };
    let mut openai_without_login = BTreeMap::new();
    let work = work_for(
        &pools.timelines,
        tokens,
        owner,
        &spans,
        prices,
        &mut openai_without_login,
    );
    let owned = raw
        .into_iter()
        .zip(work)
        .filter(|(pair, work)| {
            if work.foreign {
                return false;
            }
            match subject.company {
                Company::Anthropic => {
                    live_seconds(&pools.timelines, &subject.account, pair.from, pair.to)
                        >= MIN_LIVE_SHARE * seconds_between(pair.from, pair.to)
                }
                Company::OpenAI | Company::Ollama | Company::Google => work.own,
            }
        })
        .collect();
    (owned, openai_without_login)
}

/// The timeline of every host that has an event. `pub(crate)` so `money` reads
/// a record's account through the same timelines, without a second copy.
pub(crate) fn timelines(events: &[Hosted<Event>]) -> HashMap<String, Timeline> {
    let mut timelines: HashMap<String, Timeline> = HashMap::new();
    for hosted in events {
        if !timelines.contains_key(&hosted.host) {
            timelines.insert(hosted.host.clone(), Timeline::of(events, &hosted.host));
        }
    }
    timelines
}

/// Two consecutive samples of one account that lie in one weekly window.
struct SamplePair {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    /// The weekly percent gained between the two samples.
    week: f64,
    /// The session percent gained, when both samples lie in one 5 h window.
    session: Option<f64>,
}

/// The pairs of the subscription's sorted `lines` in `[from, to]`: consecutive,
/// at most 6 h apart, the same `main` limit's reset ([`same_reset`]). A pair
/// whose earlier sample is already at 100 % cannot rise and is left out. A pair
/// whose main percent fell by the reset rule ([`is_early_reset`]) is a reset and
/// is left out too, as a changed reset time already is. A pair that goes down by
/// less stays in, so the sum of the differences is the last percent minus the
/// first.
fn pairs(
    lines: &[&Hosted<Sample>],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    main: &LimitKind,
) -> Vec<SamplePair> {
    let lines: Vec<&Hosted<Sample>> = lines
        .iter()
        .copied()
        .filter(|hosted| hosted.record.t >= from && hosted.record.t <= to)
        .collect();

    let mut pairs = Vec::new();
    for pair in lines.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        let span = after.record.t.signed_duration_since(before.record.t);
        if span <= Duration::zero() || span > Duration::hours(6) {
            continue;
        }
        let (Some(main_before), Some(main_after)) =
            (before.record.limit(main), after.record.limit(main))
        else {
            continue;
        };
        let (Some(reset_before), Some(reset_after)) = (main_before.r, main_after.r) else {
            continue;
        };
        if !same_reset(reset_before, reset_after)
            || main_before.p >= 100.0
            || is_early_reset(
                main_before.p,
                main_after.p,
                Some(reset_before),
                after.record.t,
            )
        {
            continue;
        }
        pairs.push(SamplePair {
            from: before.record.t,
            to: after.record.t,
            week: main_after.p - main_before.p,
            session: session_gain(&before.record, &after.record),
        });
    }
    pairs
}

/// The session percent a pair gained, when both samples carry a session limit in
/// one 5 h window (the same reset, [`same_reset`]) and the earlier one is below
/// 100 %; otherwise no session pair. A session that fell by the reset rule
/// ([`is_early_reset`]) was reset early and gives no session gain either.
fn session_gain(before: &Sample, after: &Sample) -> Option<f64> {
    let limit_before = before.limit(&LimitKind::Session)?;
    let limit_after = after.limit(&LimitKind::Session)?;
    let (reset_before, reset_after) = (limit_before.r?, limit_after.r?);
    if !same_reset(reset_before, reset_after)
        || limit_before.p >= 100.0
        || is_early_reset(limit_before.p, limit_after.p, Some(reset_before), after.t)
    {
        return None;
    }
    Some(limit_after.p - limit_before.p)
}

/// When an account was live on one host. Every `live` and `switch` event changes
/// it, so the account between two changes is the one of the earlier change.
pub(crate) struct Timeline {
    changes: Vec<(DateTime<Utc>, Option<AccountRef>)>,
    /// The account in use before the first change, when the constructor knows
    /// it (only [`Timeline::provider_switches`] does: the first
    /// `providerSwitch`'s `from`).
    before: Option<Option<AccountRef>>,
}

impl Timeline {
    pub(super) fn of(events: &[Hosted<Event>], host: &str) -> Timeline {
        let mut changes: Vec<(DateTime<Utc>, Option<AccountRef>)> = events
            .iter()
            .filter(|hosted| hosted.host == host)
            .filter_map(|hosted| match &hosted.record.kind {
                EventKind::Live { account } => Some((hosted.record.t, account.clone())),
                EventKind::Switch { to, .. } => Some((hosted.record.t, Some(to.clone()))),
                _ => None,
            })
            .collect();
        changes.sort_by_key(|change| change.0);
        Timeline {
            changes,
            before: None,
        }
    }

    /// The same timeline for a provider subscription (decision P-2 2): every
    /// `providerSwitch` event of `host` changes which OpenAI login is in use.
    /// A `switch` event stays Claude-only and changes nothing here, and
    /// [`Timeline::of`] ignores a `providerSwitch` in the same way.
    pub(crate) fn provider_switches(events: &[Hosted<Event>], host: &str) -> Timeline {
        let mut switches: Vec<(DateTime<Utc>, Option<AccountRef>, AccountRef)> = events
            .iter()
            .filter(|hosted| hosted.host == host)
            .filter_map(|hosted| match &hosted.record.kind {
                EventKind::ProviderSwitch { from, to, .. } => {
                    Some((hosted.record.t, from.clone(), to.clone()))
                }
                _ => None,
            })
            .collect();
        switches.sort_by_key(|change| change.0);
        // Before the first switch the login in use is that event's `from`.
        let before = switches.first().map(|(_, from, _)| from.clone());
        Timeline {
            changes: switches
                .into_iter()
                .map(|(at, _, to)| (at, Some(to)))
                .collect(),
            before,
        }
    }

    /// The account in use before the first change, when the constructor knows
    /// it: the first `providerSwitch`'s `from` for
    /// [`Timeline::provider_switches`], and `None` for a live timeline or a
    /// timeline with no event.
    pub(crate) fn before_first(&self) -> Option<&AccountRef> {
        self.before.as_ref().and_then(Option::as_ref)
    }

    /// Whether the timeline holds no change: no event of its kind on the host.
    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub(crate) fn at(&self, t: DateTime<Utc>) -> Option<&AccountRef> {
        let after = self.changes.partition_point(|change| change.0 <= t);
        if after == 0 {
            None
        } else {
            self.changes[after - 1].1.as_ref()
        }
    }

    /// The moments in `(from, to)` at which the account in use changes.
    pub(crate) fn changes_between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        // The changes are sorted by time: start after the last one at or
        // before `from`, stop at the first one at or after `to`.
        let first = self.changes.partition_point(|change| change.0 <= from);
        self.changes[first..]
            .iter()
            .map(|change| change.0)
            .take_while(move |at| *at < to)
    }

    /// Call `each` with every stretch of `[from, to)` in which this timeline
    /// had `account` live, in time order. The one walk behind
    /// [`Timeline::seconds`] and [`live_seconds`].
    fn live_spans(
        &self,
        account: &AccountRef,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        mut each: impl FnMut(DateTime<Utc>, DateTime<Utc>),
    ) {
        let mut cursor = from;
        let mut live = self.at(from) == Some(account);
        let first = self.changes.partition_point(|change| change.0 <= from);
        for (at, target) in &self.changes[first..] {
            if *at >= to {
                break;
            }
            if live && cursor < *at {
                each(cursor, *at);
            }
            cursor = *at;
            live = target.as_ref() == Some(account);
        }
        if live && cursor < to {
            each(cursor, to);
        }
    }

    /// The seconds of `[from, to)` in which this timeline had `account` live.
    fn seconds(&self, account: &AccountRef, from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
        let mut total = 0.0;
        self.live_spans(account, from, to, |start, end| {
            total += seconds_between(start, end);
        });
        total
    }
}

/// The seconds of `[from, to)` in which at least one host had `account` live:
/// the union of every host's live stretches, so two hosts live at the same
/// time count once (the [`MIN_LIVE_SHARE`] rule of [`rates`]).
fn live_seconds(
    timelines: &HashMap<String, Timeline>,
    account: &AccountRef,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> f64 {
    let mut stretches: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for timeline in timelines.values() {
        timeline.live_spans(account, from, to, |start, end| stretches.push((start, end)));
    }
    stretches.sort();
    let mut total = 0.0;
    let mut open: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
    for (start, end) in stretches {
        open = match open {
            Some((open_start, open_end)) if start <= open_end => {
                Some((open_start, open_end.max(end)))
            }
            Some((open_start, open_end)) => {
                total += seconds_between(open_start, open_end);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((start, end)) = open {
        total += seconds_between(start, end);
    }
    total
}

fn seconds_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    let span = to.signed_duration_since(from);
    span.num_seconds() as f64 + span.subsec_nanos() as f64 / 1e9
}

/// The work of one span of [`work_in`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct SpanWork {
    /// The API dollars of the priced records.
    pub(super) usd: f64,
    /// A record of the subscription's company without a price lay in the span:
    /// its work is not in `usd`.
    pub(super) unpriced: bool,
    /// A provider record of the span's company belongs to another
    /// subscription (or to none): the span is not this subscription's work.
    pub(super) foreign: bool,
    /// A provider record of the span's company belongs to this subscription:
    /// a provider pair needs one, so a pair no record touches says nothing
    /// about the plan (decision 10 of M-2's fix).
    pub(super) own: bool,
}

/// Whose records count as work in [`work_for`]: a Claude account's live time,
/// or a provider subscription's records.
enum Ownership<'a> {
    Claude(&'a AccountRef),
    Provider(&'a ProviderWork<'a>),
}

/// The provider side of [`work_for`]: the company's records that belong to one
/// subscription.
struct ProviderWork<'a> {
    company: Company,
    subscription: &'a AccountRef,
    /// The `providerSwitch` timelines, per host: which OpenAI login a record
    /// belongs to.
    timelines: &'a HashMap<String, Timeline>,
    openai: &'a BTreeSet<AccountRef>,
}

/// The work of one subscription in each of the given disjoint, time-sorted
/// spans, one [`SpanWork`] per span in the same order. A Claude record (a
/// Codex or Ollama record uses no Claude account and is left out) counts only
/// while `account` was live on the record's host, spread evenly over its own
/// [`TokenRecord::span`], at its [`api_usd`]. A provider record counts when
/// its company is the subscription's and it belongs to it, spread the same way
/// and without a live rule; a record of that company inside the span that
/// belongs elsewhere marks the span `foreign`. A record without a price is not
/// valued: it marks the spans it counts for as `unpriced`. An OpenAI record the
/// stored logins cannot be named for marks its host in `openai_without_login`,
/// once per host, and counts as foreign. `tokens` are sorted by time, as a
/// history read returns them, so only the records whose longest possible span
/// reaches into the spans are looked at.
fn work_for(
    timelines: &HashMap<String, Timeline>,
    tokens: &[Hosted<TokenRecord>],
    owner: Ownership<'_>,
    spans: &[(DateTime<Utc>, DateTime<Utc>)],
    prices: &Prices,
    openai_without_login: &mut BTreeMap<String, String>,
) -> Vec<SpanWork> {
    let mut work = vec![SpanWork::default(); spans.len()];
    let (Some(first_span), Some(last_span)) = (spans.first(), spans.last()) else {
        return work;
    };
    debug_assert!(
        tokens.is_sorted_by_key(|hosted| hosted.record.t),
        "work_for needs the token records sorted by time"
    );
    // A record whose longest span ends at or before the first span, or that
    // starts at or after the last span's end, has no part inside a span. The
    // bound uses the longest span (an hour) so a 5-minute line next to an
    // hourly one is never skipped; the overlap below uses the line's own span.
    let from =
        tokens.partition_point(|hosted| hosted.record.t + Duration::minutes(60) <= first_span.0);
    let to = tokens.partition_point(|hosted| hosted.record.t < last_span.1);
    for hosted in &tokens[from..to.max(from)] {
        let record = &hosted.record;
        let company = Company::of_model(&record.model);
        match &owner {
            Ownership::Claude(account) => {
                // Another provider's work uses no Claude account.
                if company != Company::Anthropic {
                    continue;
                }
                let start = record.t;
                let end = start + record.span();
                let span_seconds = seconds_between(start, end);
                let usd = api_usd(record, prices);
                let Some(timeline) = timelines.get(&hosted.host) else {
                    continue;
                };
                let first = spans.partition_point(|span| span.1 <= start);
                for (index, span) in spans.iter().enumerate().skip(first) {
                    if span.0 >= end {
                        break;
                    }
                    let from = span.0.max(start);
                    let to = span.1.min(end);
                    if from >= to {
                        continue;
                    }
                    let live = timeline.seconds(account, from, to);
                    if live <= 0.0 {
                        continue;
                    }
                    match usd {
                        Some(usd) => work[index].usd += usd * live / span_seconds,
                        None => work[index].unpriced = true,
                    }
                }
            }
            Ownership::Provider(provider) => {
                if company != provider.company {
                    continue;
                }
                let start = record.t;
                let end = start + record.span();
                let span_seconds = seconds_between(start, end);
                let usd = api_usd(record, prices);
                let belongs = match company {
                    Company::Ollama => provider.subscription == &ollama_subscription(),
                    // No model id is Google's.
                    Company::Google => false,
                    Company::Anthropic | Company::OpenAI => {
                        match openai_subscription_at(
                            provider.timelines,
                            provider.openai,
                            &hosted.host,
                            record.t,
                        ) {
                            Ok(account) => &account == provider.subscription,
                            Err(_) => {
                                openai_without_login
                                    .entry(hosted.host.clone())
                                    .or_insert_with(|| {
                                        no_openai_login_warning(
                                            &format!("record of {}", record.model),
                                            &hosted.host,
                                            record.t,
                                        )
                                    });
                                false
                            }
                        }
                    }
                };
                let first = spans.partition_point(|span| span.1 <= start);
                for (index, span) in spans.iter().enumerate().skip(first) {
                    if span.0 >= end {
                        break;
                    }
                    let from = span.0.max(start);
                    let to = span.1.min(end);
                    if from >= to {
                        continue;
                    }
                    if !belongs {
                        work[index].foreign = true;
                        continue;
                    }
                    work[index].own = true;
                    match usd {
                        Some(usd) => {
                            work[index].usd += usd * seconds_between(from, to) / span_seconds;
                        }
                        None => work[index].unpriced = true,
                    }
                }
            }
        }
    }
    work
}

/// [`work_for`] for a Claude account: auto-burn's one work function.
pub(super) fn work_in(
    timelines: &HashMap<String, Timeline>,
    tokens: &[Hosted<TokenRecord>],
    account: &AccountRef,
    spans: &[(DateTime<Utc>, DateTime<Utc>)],
    prices: &Prices,
) -> Vec<SpanWork> {
    // A Claude account reads no OpenAI record as work: there is no OpenAI
    // subscription in its owner to warn about.
    let mut openai_without_login = BTreeMap::new();
    work_for(
        timelines,
        tokens,
        Ownership::Claude(account),
        spans,
        prices,
        &mut openai_without_login,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::FixedOffset;
    use serde_json::json;

    use crate::history::{By, HistoryDir, LimitSample, Source};
    use crate::project::ProjectKey;

    const EPS: f64 = 1e-9;

    const AC_SESSION: &str = "2026-11-14T19:30:00.336716Z";
    const WEEK_RESET: &str = "2026-11-16T16:00:00.336716Z";
    /// Input tokens of `claude-opus-5-5` (4 USD per million) worth one dollar.
    const TOKENS_PER_USD: f64 = 250_000.0;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("test date")
    }

    /// The models.dev fixture's prices: `claude-opus-5-5` has one,
    /// `claude-new-9` none.
    fn prices() -> Prices {
        crate::money::tests::priced_prices(utc("2026-01-01T00:00:00Z"))
    }

    /// One account's rates without a plan capacity or name: its measurement
    /// alone.
    fn rates_of(
        account: &AccountRef,
        samples: &[Hosted<Sample>],
        tokens: &[Hosted<TokenRecord>],
        events: &[Hosted<Event>],
        now: DateTime<Utc>,
    ) -> Option<Rates> {
        let accounts = [with_capacity(account.clone(), None)];
        rates(&accounts, samples, tokens, events, &prices(), now)
            .rates
            .remove(account)
    }

    fn east_one() -> FixedOffset {
        FixedOffset::east_opt(3600).expect("+01:00")
    }

    /// A zone that is not a whole hour off UTC.
    fn half_hour() -> FixedOffset {
        FixedOffset::east_opt(5 * 3600 + 1800).expect("+05:30")
    }

    fn ac() -> AccountRef {
        AccountRef {
            acc: "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04".to_string(),
            org: "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20".to_string(),
        }
    }

    fn lb() -> AccountRef {
        AccountRef {
            acc: "3b7d1f04-9c2e-4a61-8d5b-0e7f2a4c9b13".to_string(),
            org: "c41e6b82-7d3a-4f95-b2c8-5a1d9e3f7b46".to_string(),
        }
    }

    fn pr() -> AccountRef {
        AccountRef {
            acc: "8e2c5a17-4b9d-4f30-a6e1-2d7b9c0f5a84".to_string(),
            org: "f0a3d6c9-2e8b-4c71-9b54-7a1e3d6f8c20".to_string(),
        }
    }

    fn or() -> AccountRef {
        AccountRef {
            acc: "2d9f4b61-8a3c-4e7d-b5f0-9c1e6a3d8b27".to_string(),
            org: "7b4e1c93-5f2a-4d8b-a6c0-3e9d2f7a1b54".to_string(),
        }
    }

    fn weekly(percent: f64, reset: &str) -> LimitSample {
        LimitSample {
            k: LimitKind::WeeklyAll,
            m: None,
            p: percent,
            r: Some(utc(reset)),
            b: false,
        }
    }

    fn session(percent: f64, reset: &str) -> LimitSample {
        LimitSample {
            k: LimitKind::Session,
            m: None,
            p: percent,
            r: Some(utc(reset)),
            b: false,
        }
    }

    fn sample(host: &str, at: &str, account: AccountRef, l: Vec<LimitSample>) -> Hosted<Sample> {
        Hosted {
            host: host.to_string(),
            record: Sample {
                t: utc(at),
                account,
                src: Source::Poll,
                l,
            },
        }
    }

    /// An hourly token line of `claude-opus-5-5` worth `usd` API dollars (its
    /// input tokens at 4 USD per million).
    fn token(host: &str, at: &str, usd: f64) -> Hosted<TokenRecord> {
        Hosted {
            host: host.to_string(),
            record: TokenRecord {
                t: utc(at),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: (usd * TOKENS_PER_USD).round() as u64,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            },
        }
    }

    fn live(host: &str, at: &str, account: Option<AccountRef>) -> Hosted<Event> {
        Hosted {
            host: host.to_string(),
            record: Event {
                t: utc(at),
                kind: EventKind::Live { account },
            },
        }
    }

    fn switched(host: &str, at: &str, to: AccountRef) -> Hosted<Event> {
        Hosted {
            host: host.to_string(),
            record: Event {
                t: utc(at),
                kind: EventKind::Switch {
                    from: None,
                    to,
                    by: By::User,
                    reason: None,
                    cwd: None,
                    sessions: None,
                    waited_secs: None,
                },
            },
        }
    }

    /// A stored account as `accounts.json` holds it: its plan's name and its
    /// identity's rate-limit tier.
    fn stored(account: &AccountRef, abbr: &str, plan: &str, tier: Option<&str>) -> Account {
        let mut identity = json!({
            "accountUuid": account.acc,
            "organizationUuid": account.org,
            "emailAddress": "sam@example.com"
        });
        if let Some(tier) = tier {
            identity["organizationRateLimitTier"] = json!(tier);
        }
        serde_json::from_value(json!({
            "id": format!("{}{}", &account.acc[..8], &account.org[..8]),
            "accountUuid": account.acc,
            "organizationUuid": account.org,
            "label": abbr,
            "abbr": abbr,
            "color": "#3b82f6",
            "email": "sam@example.com",
            "orgName": null,
            "plan": plan,
            "oauthAccount": identity,
            "addedAt": "2026-09-01T00:00:00Z",
            "loginState": "ok",
            "autoBurn": true
        }))
        .expect("a test account")
    }

    #[test]
    fn the_rhythm_prices_each_token_kind_at_its_models_price() {
        // One million tokens of each kind of claude-opus-5-5: 4 + 20 + 5 + 0.2
        // USD, in the newest week, which weighs 2 of the four weeks' 6.
        let mut line = token("atlas", "2026-11-23T08:00:00Z", 0.0);
        line.record.input = 1_000_000;
        line.record.out = 1_000_000;
        line.record.cw = 1_000_000;
        line.record.cr = 1_000_000;
        let rhythm = rhythm(&[line], utc("2026-11-23T12:00:00Z"), &east_one(), &prices());
        assert!(
            (rhythm.usd[9] - 29.2 * 2.0 / 6.0).abs() < EPS,
            "{}",
            rhythm.usd[9]
        );
    }

    #[test]
    fn rhythm_averages_per_hour_of_the_week() {
        // The last four Mondays, 09:00 local (+01:00), 2 USD each; two lines
        // from another host count the same as this host's.
        let tokens = vec![
            token("atlas", "2026-11-23T08:00:00Z", 2.0),
            token("vega", "2026-11-16T08:00:00Z", 2.0),
            token("atlas", "2026-11-09T08:00:00Z", 2.0),
            token("vega", "2026-11-02T08:00:00Z", 2.0),
        ];
        let now = utc("2026-11-23T12:00:00Z");
        let rhythm = rhythm(&tokens, now, &east_one(), &prices());

        assert!((rhythm.usd[9] - 2.0).abs() < EPS, "{:?}", rhythm.usd[9]);
        for (hour, usd) in rhythm.usd.iter().enumerate() {
            if hour != 9 {
                assert!((*usd - 0.0).abs() < EPS, "hour {hour}: {usd}");
            }
        }
        assert!((rhythm.typical_hour() - 2.0).abs() < EPS);
        // Monday 09:30 local is that hour; Tuesday 09:00 local is another one.
        assert!((rhythm.at(utc("2026-11-23T08:30:00Z"), east_one()) - 2.0).abs() < EPS);
        assert!((rhythm.at(utc("2026-11-24T08:00:00Z"), east_one()) - 0.0).abs() < EPS);
    }

    /// The 1.4.1 fix: work in one of the four weeks only, 20 USD in each of two
    /// hours. The rhythm spreads it over all four weeks, so its typical hour is
    /// 20 × 2 / 6 (the newer two weeks weigh 2 of 6) and a full limit looked
    /// three times as long; the worked hour is the 20 USD the hours held.
    #[test]
    fn a_worked_hour_is_not_thinned_by_weeks_without_work() {
        let tokens = vec![
            token("atlas", "2026-11-23T08:00:00Z", 20.0),
            token("atlas", "2026-11-23T09:00:00Z", 20.0),
        ];
        let now = utc("2026-11-23T12:00:00Z");
        let rhythm_hour = rhythm(&tokens, now, &east_one(), &prices()).typical_hour();
        assert!(
            (rhythm_hour - 20.0 * 2.0 / 6.0).abs() < 1e-6,
            "{rhythm_hour}"
        );

        let hour = worked_hour(&tokens, now, &east_one(), &prices(), Company::Anthropic)
            .expect("two worked hours");
        assert!((hour.usd - 20.0).abs() < 1e-6, "{hour:?}");
        assert!(
            (hour.tokens - 20.0 * TOKENS_PER_USD).abs() < 1.0,
            "{hour:?}"
        );
    }

    /// Two hosts in the same hour are one hour of work; a record of a model
    /// without a price, of another company, from the future or older than four
    /// weeks is left out.
    #[test]
    fn a_worked_hour_counts_each_clock_hour_once_and_only_priced_work() {
        let now = utc("2026-11-23T12:00:00Z");
        let tokens = vec![
            token("atlas", "2026-11-23T08:00:00Z", 6.0),
            token("vega", "2026-11-23T08:00:00Z", 4.0),
            token_of("claude-new-9", "atlas", "2026-11-23T10:00:00Z", 30.0),
            token_of("gpt-6-astra", "atlas", "2026-11-23T11:00:00Z", 30.0),
            token("atlas", "2026-11-23T13:00:00Z", 30.0),
            token("atlas", "2026-10-25T08:00:00Z", 30.0),
        ];
        let hour = worked_hour(&tokens, now, &east_one(), &prices(), Company::Anthropic)
            .expect("one worked hour");
        assert!((hour.usd - 10.0).abs() < 1e-6, "{hour:?}");
        assert!(
            (hour.tokens - 10.0 * TOKENS_PER_USD).abs() < 1.0,
            "{hour:?}"
        );
    }

    #[test]
    fn no_worked_hour_without_priced_work() {
        let now = utc("2026-11-23T12:00:00Z");
        assert_eq!(
            worked_hour(&[], now, &east_one(), &prices(), Company::Anthropic),
            None
        );
        let unpriced = vec![token_of(
            "claude-new-9",
            "atlas",
            "2026-11-23T08:00:00Z",
            3.0,
        )];
        assert_eq!(
            worked_hour(&unpriced, now, &east_one(), &prices(), Company::Anthropic),
            None
        );
    }

    #[test]
    fn newer_weeks_count_double() {
        let tokens = vec![
            token("atlas", "2026-11-23T08:00:00Z", 4.0),
            token("atlas", "2026-11-16T08:00:00Z", 4.0),
            token("atlas", "2026-11-09T08:00:00Z", 1.0),
            token("atlas", "2026-11-02T08:00:00Z", 1.0),
        ];
        let rhythm = rhythm(&tokens, utc("2026-11-23T12:00:00Z"), &east_one(), &prices());
        assert!((rhythm.usd[9] - 3.0).abs() < EPS, "{:?}", rhythm.usd[9]);
    }

    #[test]
    fn account_at_follows_live_and_switch_events() {
        let events = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(lb())),
            switched("atlas", "2026-11-14T10:00:00Z", ac()),
        ];
        assert_eq!(
            account_at(&events, "atlas", utc("2026-11-14T09:00:00Z")),
            Some(lb())
        );
        assert_eq!(
            account_at(&events, "atlas", utc("2026-11-14T10:00:00Z")),
            Some(ac())
        );
        assert_eq!(
            account_at(&events, "atlas", utc("2026-11-14T07:00:00Z")),
            None
        );
        assert_eq!(
            account_at(&events, "vega", utc("2026-11-14T09:00:00Z")),
            None
        );
    }

    /// AC live all day: weekly 40 → 48 and session 0 → 100 over 125 USD.
    fn ac_day() -> (Vec<Hosted<Sample>>, Vec<Hosted<TokenRecord>>) {
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET), session(0.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET), session(100.0, AC_SESSION)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 50.0),
            token("atlas", "2026-11-14T11:00:00Z", 50.0),
            token("atlas", "2026-11-14T12:00:00Z", 25.0),
        ];
        (samples, tokens)
    }

    #[test]
    fn rates_divide_percent_by_work() {
        let (samples, tokens) = ac_day();
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("125 USD is enough work");
        assert!((rates.c.value - 0.064).abs() < EPS, "{:?}", rates.c);
        assert!(rates.c.measured);
        assert!((rates.c.basis_usd - 125.0).abs() < EPS);
        let s = rates.s.expect("a session limit");
        assert!((s.value - 0.8).abs() < EPS, "{s:?}");
    }

    #[test]
    fn rates_ignore_accounts_from_readings() {
        // AC's readings rise 41 → 43 between 09:20 and 09:50, and a 5-minute
        // record 09:25–09:30 lies before the first `live` event at 10:00: R4
        // books it to AC in the valuer. Auto-burn's rates learn AC's factor
        // from its live pairs only, 4 % over the 100 USD of 10:00–11:00; a rate
        // that counted the record from the readings would pool 6 % over 200 USD.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:20:00Z",
                ac(),
                vec![weekly(41.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T09:50:00Z",
                ac(),
                vec![weekly(43.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T10:00:00Z",
                ac(),
                vec![weekly(43.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T11:00:00Z",
                ac(),
                vec![weekly(47.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            five_minute_token("atlas", "2026-11-14T09:25:00Z", 100.0),
            token("atlas", "2026-11-14T10:00:00Z", 100.0),
        ];
        let events = vec![live("atlas", "2026-11-14T10:00:00Z", Some(ac()))];
        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T12:00:00Z"),
        )
        .expect("100 USD is enough work");

        assert!((rates.c.value - 0.04).abs() < EPS, "{:?}", rates.c);
        assert!((rates.c.basis_usd - 100.0).abs() < EPS, "{:?}", rates.c);
    }

    /// The same line as [`token`], but one 5-minute bucket long.
    fn five_minute_token(host: &str, at: &str, usd: f64) -> Hosted<TokenRecord> {
        let mut hosted = token(host, at, usd);
        hosted.record.minutes = 5;
        hosted
    }

    #[test]
    fn hourly_and_five_minute_lines_of_one_hour_add_up_once() {
        // One hourly line of 300 USD and twelve 5-minute lines of 5 USD each
        // over the hour 10:00-11:00; the pair 10:00-10:30 holds half of the
        // hourly line and six of the 5-minute ones: 150 + 30 USD. A 90 % gain
        // over that pair is 0.5 % per USD.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T10:00:00Z",
                ac(),
                vec![weekly(0.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T10:30:00Z",
                ac(),
                vec![weekly(90.0, WEEK_RESET)],
            ),
        ];
        let mut tokens = vec![token("atlas", "2026-11-14T10:00:00Z", 300.0)];
        for minute in (0..60).step_by(5) {
            tokens.push(five_minute_token(
                "atlas",
                &format!("2026-11-14T10:{minute:02}:00Z"),
                5.0,
            ));
        }
        let events = vec![live("atlas", "2026-11-14T09:00:00Z", Some(ac()))];
        let timelines = timelines(&events);
        let work = work_in(
            &timelines,
            &tokens,
            &ac(),
            &[(utc("2026-11-14T10:00:00Z"), utc("2026-11-14T10:30:00Z"))],
            &prices(),
        );
        assert_eq!(work.len(), 1);
        assert!((work[0].usd - 180.0).abs() < EPS, "{:?}", work[0]);
        assert!(!work[0].unpriced);

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("180 USD is enough work");
        assert!((rates.c.value - 0.5).abs() < EPS, "{:?}", rates.c);
    }

    /// The same token line on another model; `usd` sets its token count as
    /// [`token`] does.
    fn token_of(model: &str, host: &str, at: &str, usd: f64) -> Hosted<TokenRecord> {
        let mut hosted = token(host, at, usd);
        hosted.record.model = model.to_string();
        hosted
    }

    /// Codex and flash work done while AC was live uses no Claude account: it
    /// changes neither AC's rates, nor the switch cost's work, nor the rhythm.
    #[test]
    fn another_providers_work_leaves_the_rates_alone() {
        let (samples, claude) = ac_day();
        let mut mixed = claude.clone();
        mixed.push(token_of(
            "gpt-6.1-sol",
            "atlas",
            "2026-11-14T10:00:00Z",
            150.0,
        ));
        mixed.push(token_of(
            "deepseek-v4.1-flash:cloud",
            "atlas",
            "2026-11-14T11:30:00Z",
            200.0,
        ));
        mixed.sort_by_key(|hosted| hosted.record.t);
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];
        let now = utc("2026-11-14T18:00:00Z");

        let alone = rates_of(&ac(), &samples, &claude, &events, now).expect("125 USD of Claude");
        let with_others =
            rates_of(&ac(), &samples, &mixed, &events, now).expect("the same 125 USD");
        assert_eq!(with_others, alone);
        assert!(
            (with_others.c.value - 0.064).abs() < EPS,
            "{:?}",
            with_others.c
        );

        let timelines = timelines(&events);
        let spans = [(utc("2026-11-14T09:00:00Z"), utc("2026-11-14T13:00:00Z"))];
        let work = work_in(&timelines, &mixed, &ac(), &spans, &prices());
        assert!((work[0].usd - 125.0).abs() < EPS, "{:?}", work[0]);
        assert!(
            !work[0].unpriced,
            "a model of another company is no Claude work"
        );
        assert_eq!(
            rhythm(&mixed, now, &east_one(), &prices()),
            rhythm(&claude, now, &east_one(), &prices()),
            "the rhythm counts Claude work only"
        );
        let learned = rates(
            &[with_capacity(ac(), None)],
            &samples,
            &mixed,
            &events,
            &prices(),
            now,
        );
        assert!(
            learned.unpriced.is_empty(),
            "gpt-6.1-sol has no price but is no Claude model: {:?}",
            learned.unpriced
        );
    }

    #[test]
    fn work_on_another_account_does_not_count() {
        // AC is live on atlas the whole time and does its 125 USD there; on
        // vega LB is live and 125 USD more fall into the pair. With LB's work
        // counted, c would be 8 / 250 = 0.032.
        let (samples, _) = ac_day();
        let tokens = vec![
            token("atlas", "2026-11-14T09:00:00Z", 125.0),
            token("vega", "2026-11-14T10:00:00Z", 125.0),
        ];
        let events = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(ac())),
            live("vega", "2026-11-14T08:00:00Z", Some(lb())),
        ];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("AC's 125 USD is enough work");
        assert!((rates.c.value - 0.064).abs() < EPS, "{:?}", rates.c);
        let s = rates.s.expect("a session limit");
        assert!((s.value - 0.8).abs() < EPS, "{s:?}");
    }

    #[test]
    fn less_than_100_dollars_is_unknown() {
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![token("atlas", "2026-11-14T10:00:00Z", 99.6)];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        assert!(rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z")
        )
        .is_none());
    }

    #[test]
    fn a_reset_between_two_samples_is_not_a_drop() {
        // A valid pair gains 8 % of the week over 125 USD: c = 8 / 125 = 0.064.
        // The 96 % → 3 % pair crosses the Monday reset; keeping it would add
        // -93 % over 125 USD more, so c would be (8 - 93) / 250 < 0.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-16T15:00:00Z",
                ac(),
                vec![weekly(96.0, "2026-11-16T16:00:00.336716Z")],
            ),
            sample(
                "atlas",
                "2026-11-16T17:00:00Z",
                ac(),
                vec![weekly(3.0, "2026-11-23T16:00:00.336716Z")],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 125.0),
            token("atlas", "2026-11-16T16:00:00Z", 125.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-16T18:00:00Z"),
        )
        .expect("the valid pair carries 125 USD");
        assert!((rates.c.value - 0.064).abs() < EPS, "c = {:?}", rates.c);
    }

    #[test]
    fn a_reset_that_keeps_its_reset_time_is_left_out_of_the_rates() {
        // The review's case: 40 → 48 → 0 at 09:00, 13:00 and 15:00 with the
        // same reset time. The 13:00 → 15:00 pair is an early reset, not -48
        // points of usage; keeping it would make c (8 - 48) / 250 < 0 and
        // `rates` None. The rising pair's 125 USD gives c = 8 / 125 = 0.064.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T15:00:00Z",
                ac(),
                vec![weekly(0.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 125.0),
            token("atlas", "2026-11-14T14:00:00Z", 125.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("the rising pair carries 125 USD");
        assert!((rates.c.value - 0.064).abs() < EPS, "c = {:?}", rates.c);
    }

    #[test]
    fn a_model_reset_that_keeps_its_reset_time_is_left_out() {
        // A model reset takes the week from 70 % to 45 % with the reset time
        // unchanged. The pair is a reset: with it c would be (30 - 25) / 250 =
        // 0.02; without it the rising pair's 30 points over 125 USD give 0.24.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(70.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T15:00:00Z",
                ac(),
                vec![weekly(45.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 125.0),
            token("atlas", "2026-11-14T14:00:00Z", 125.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("the rising pair carries 125 USD");
        assert!((rates.c.value - 0.24).abs() < EPS, "c = {:?}", rates.c);
    }

    #[test]
    fn an_account_without_a_session_limit_has_no_s() {
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![token("atlas", "2026-11-14T10:00:00Z", 125.0)];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("125 USD is enough work");
        assert!((rates.c.value - 0.064).abs() < EPS, "{:?}", rates.c);
        assert!(rates.s.is_none());
    }

    #[test]
    fn a_switch_inside_an_hour_splits_its_work() {
        // AC stays live on atlas, so the pair counts. On vega, where the work
        // is done, a switch at 11:20 splits the 11:00 token line's hour 20/40:
        // AC gets one third of its 50 USD, so 116 2/3 USD carry 42 % of the
        // week.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(82.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("vega", "2026-11-14T09:00:00Z", 50.0),
            token("vega", "2026-11-14T10:00:00Z", 50.0),
            token("vega", "2026-11-14T11:00:00Z", 50.0),
        ];
        let now = utc("2026-11-14T18:00:00Z");

        let splitting = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(ac())),
            live("vega", "2026-11-14T08:00:00Z", Some(ac())),
            switched("vega", "2026-11-14T11:20:00Z", lb()),
        ];
        let split = rates_of(&ac(), &samples, &tokens, &splitting, now)
            .expect("116 2/3 USD is enough work");
        assert!((split.c.value - 0.36).abs() < EPS, "{:?}", split.c);

        // AC live the whole hour: the same lines are worth 150 USD, and the
        // factor is lower.
        let whole_hour = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(ac())),
            live("vega", "2026-11-14T08:00:00Z", Some(ac())),
        ];
        let unsplit =
            rates_of(&ac(), &samples, &tokens, &whole_hour, now).expect("150 USD is enough work");
        assert!(
            (unsplit.c.value - 42.0 / 150.0).abs() < EPS,
            "{:?}",
            unsplit.c
        );
    }

    #[test]
    fn two_token_lines_of_one_hour_add_up() {
        // Three lines of one session, hour and model (one per scan), and one line
        // from another host: all four are work, 100 USD over the pair.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 25.0),
            token("atlas", "2026-11-14T10:00:00Z", 25.0),
            token("atlas", "2026-11-14T10:00:00Z", 25.0),
            token("vega", "2026-11-14T10:00:00Z", 25.0),
        ];
        let events = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(ac())),
            live("vega", "2026-11-14T08:00:00Z", Some(ac())),
        ];

        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-14T18:00:00Z"),
        )
        .expect("100 USD is enough work");
        assert!((rates.c.value - 0.08).abs() < EPS, "{:?}", rates.c);
    }

    // ---- proof tests of the review ----

    #[test]
    fn a_half_hour_offset_splits_a_record_over_two_local_hours() {
        // 10:00Z is 15:30 in +05:30, so the hour of the record (10:00Z to
        // 11:00Z) is 15:30 to 16:30 local: half of its work is in local hour
        // 15 and half in local hour 16.
        let tokens = vec![
            token("atlas", "2026-11-23T10:00:00Z", 2.0),
            token("atlas", "2026-11-16T10:00:00Z", 2.0),
            token("atlas", "2026-11-09T10:00:00Z", 2.0),
            token("atlas", "2026-11-02T10:00:00Z", 2.0),
        ];
        let rhythm = rhythm(
            &tokens,
            utc("2026-11-23T12:00:00Z"),
            &half_hour(),
            &prices(),
        );

        assert!(
            (rhythm.usd[15] - 1.0).abs() < EPS,
            "hour 15: {}",
            rhythm.usd[15]
        );
        assert!(
            (rhythm.usd[16] - 1.0).abs() < EPS,
            "hour 16: {}",
            rhythm.usd[16]
        );
        let total: f64 = rhythm.usd.iter().sum();
        assert!((total - 2.0).abs() < EPS, "total: {total}");
    }

    #[test]
    fn hour_shares_split_a_line_over_the_local_hours_it_covers() {
        // 08:00Z is 13:30 in +05:30, so an hourly line runs 13:30-14:30 local:
        // half of its span in each of local hours 13 and 14. A 5-minute line
        // lies wholly in one of them.
        let hourly = token("atlas", "2026-11-23T08:00:00Z", 12.0);
        let shares = hour_shares(&hourly.record, |at| {
            crate::stats::local_hour_start(at, &half_hour())
        });
        assert_eq!(shares.len(), 2, "{shares:?}");
        assert_eq!(shares[0].0, utc("2026-11-23T07:30:00Z"));
        assert_eq!(shares[1].0, utc("2026-11-23T08:30:00Z"));
        assert!((shares[0].1 - 0.5).abs() < EPS, "{}", shares[0].1);
        assert!((shares[1].1 - 0.5).abs() < EPS, "{}", shares[1].1);

        let five = five_minute_token("atlas", "2026-11-23T08:00:00Z", 1.0);
        let shares = hour_shares(&five.record, |at| {
            crate::stats::local_hour_start(at, &half_hour())
        });
        assert_eq!(shares.len(), 1, "{shares:?}");
        assert_eq!(shares[0].0, utc("2026-11-23T07:30:00Z"));
        assert!((shares[0].1 - 1.0).abs() < EPS, "{}", shares[0].1);
    }

    #[test]
    fn rhythm_counts_five_minute_lines_like_the_hour_they_make_up() {
        let hourly = vec![token("atlas", "2026-11-23T08:00:00Z", 12.0)];
        let mut five = Vec::new();
        for minute in (0..60).step_by(5) {
            five.push(five_minute_token(
                "atlas",
                &format!("2026-11-23T08:{minute:02}:00Z"),
                1.0,
            ));
        }
        let now = utc("2026-11-23T12:00:00Z");
        let a = rhythm(&hourly, now, &east_one(), &prices());
        let b = rhythm(&five, now, &east_one(), &prices());
        assert!(
            (b.usd[9] - a.usd[9]).abs() < EPS,
            "hourly {} vs 5-minute {}",
            a.usd[9],
            b.usd[9]
        );
    }

    #[test]
    fn a_record_at_now_is_outside_the_four_weeks() {
        // The window is [now - 4 weeks, now): the hour of a record that starts
        // at now lies in the future and is not work of the past four weeks.
        let tokens = vec![token("atlas", "2026-11-23T08:00:00Z", 2.0)];
        let rhythm = rhythm(&tokens, utc("2026-11-23T08:00:00Z"), &east_one(), &prices());

        assert!(
            (rhythm.usd[9] - 0.0).abs() < EPS,
            "hour 9: {}",
            rhythm.usd[9]
        );
    }

    #[test]
    fn a_percent_that_cannot_rise_gives_no_rate() {
        // The account is at 100 % of its week and of its window: neither
        // percent can rise, so the pair and its 125 USD are left out. With no
        // pair left there is not enough work and no factor at all; a factor of
        // 0 % per USD is not a factor: the plan divides by it (rem / c,
        // (100 - session%) / s and 100 * c / s).
        let full = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(100.0, WEEK_RESET), session(100.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(100.0, WEEK_RESET), session(100.0, AC_SESSION)],
            ),
        ];
        let tokens = vec![token("atlas", "2026-11-14T16:00:00Z", 125.0)];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];
        assert!(rates_of(&ac(), &full, &tokens, &events, utc("2026-11-14T18:00:00Z")).is_none());

        // A pair that can rise beside a 100 % one keeps its positive factor;
        // the 100 % pair's 125 USD stay out (with them c would be 8 / 250 =
        // 0.032 and s 50 / 250 = 0.2).
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET), session(0.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(48.0, WEEK_RESET), session(50.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-16T09:00:00Z",
                ac(),
                vec![weekly(100.0, WEEK_RESET), session(100.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-16T13:00:00Z",
                ac(),
                vec![weekly(100.0, WEEK_RESET), session(100.0, AC_SESSION)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T10:00:00Z", 125.0),
            token("atlas", "2026-11-16T10:00:00Z", 125.0),
        ];
        let rates = rates_of(
            &ac(),
            &samples,
            &tokens,
            &events,
            utc("2026-11-16T18:00:00Z"),
        )
        .expect("the 125 USD of the pair that can rise is enough work");
        assert!((rates.c.value - 0.064).abs() < EPS, "c = {:?}", rates.c);
        let s = rates.s.expect("a session limit");
        assert!((s.value - 0.4).abs() < EPS, "s = {s:?}");
    }

    /// A record at 09:00 in +02:00 stays in hour 9 of the week even when the
    /// four weeks start in +01:00: the rhythm uses the offset of each instant.
    #[test]
    fn a_summer_record_is_in_its_own_rhythm_hour() {
        let tokens = vec![token("atlas", "2026-03-30T07:00:00Z", 2.0)];
        let rhythm = rhythm(
            &tokens,
            utc("2026-03-30T12:00:00Z"),
            &crate::stats::tests::SummerZone,
            &prices(),
        );

        assert!(
            (rhythm.usd[9] - 2.0 / 3.0).abs() < EPS,
            "hour 9: {}",
            rhythm.usd[9]
        );
        assert!(
            (rhythm.usd[8] - 0.0).abs() < EPS,
            "hour 8: {}",
            rhythm.usd[8]
        );
    }

    fn with_capacity(account: AccountRef, capacity: Option<f64>) -> RateAccount {
        RateAccount {
            account,
            company: Company::Anthropic,
            capacity,
            plan: None,
        }
    }

    fn on_plan(account: AccountRef, capacity: Option<f64>, plan: &str) -> RateAccount {
        RateAccount {
            account,
            company: Company::Anthropic,
            capacity,
            plan: Some(plan.to_string()),
        }
    }

    #[test]
    fn an_openai_pair_counts_only_when_all_its_work_is_that_subscriptions() {
        let openai = Company::OpenAI;
        let first = AccountRef {
            acc: "3c1e7a94-6d2b-4f08-9a51-2e7c4b6d8f10".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let second = AccountRef {
            acc: "9b4d2e61-7c3a-4f85-8d09-1a6e5c3b7f24".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let subscriptions = [
            crate::stats::tests::provider_subscription(&first, "OA", None),
            crate::stats::tests::provider_subscription(&second, "OB", None),
        ];
        let samples = vec![
            sample(
                "atlas",
                "2026-11-10T08:00:00Z",
                first.clone(),
                vec![weekly(0.0, "2026-11-16T00:00:00Z")],
            ),
            sample(
                "atlas",
                "2026-11-10T09:00:00Z",
                first.clone(),
                vec![weekly(10.0, "2026-11-16T00:00:00Z")],
            ),
            sample(
                "atlas",
                "2026-11-10T10:00:00Z",
                first.clone(),
                vec![weekly(50.0, "2026-11-16T00:00:00Z")],
            ),
        ];
        let events = vec![Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: utc("2026-11-10T09:30:00Z"),
                kind: EventKind::ProviderSwitch {
                    from: Some(first.clone()),
                    to: second.clone(),
                    by: By::User,
                },
            },
        }];
        let mut prices = prices();
        crate::stats::tests::codex_prices(&mut prices);
        let mut first_token =
            crate::stats::tests::codex_token("2026-11-10T08:10:00Z", "gpt-6.2-codex", 12_000_000);
        first_token.t = utc("2026-11-10T08:10:00Z");
        let mut second_token =
            crate::stats::tests::codex_token("2026-11-10T09:40:00Z", "gpt-6.2-codex", 12_000_000);
        second_token.t = utc("2026-11-10T09:40:00Z");
        let tokens = vec![
            Hosted {
                host: "atlas".to_string(),
                record: first_token,
            },
            Hosted {
                host: "atlas".to_string(),
                record: second_token,
            },
        ];
        let subject = RateAccount {
            account: first.clone(),
            company: openai,
            capacity: None,
            plan: None,
        };
        let pools = WindowPools::of(&events, &subscriptions);
        let pairs = window_pairs(
            &subject,
            &sample_lines(&first, &samples),
            &tokens,
            &pools,
            &prices,
            PairLimit::Main,
            utc("2026-11-10T08:00:00Z"),
            utc("2026-11-10T10:00:00Z"),
        );
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(pairs[0].gain, 10.0);
        assert!((pairs[0].usd - 100.0).abs() < 0.001, "{}", pairs[0].usd);
    }

    /// Two stored OpenAI logins and no `providerSwitch` on the host: the pair
    /// belongs to neither subscription, so it is not booked, but the rates
    /// reader says so once for the host (review, 1.3.0).
    #[test]
    fn two_openai_logins_without_a_switch_are_named_once() {
        let first = AccountRef {
            acc: "3c1e7a94-6d2b-4f08-9a51-2e7c4b6d8f10".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let second = AccountRef {
            acc: "9b4d2e61-7c3a-4f85-8d09-1a6e5c3b7f24".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let subscriptions = [
            crate::stats::tests::provider_subscription(&first, "OA", None),
            crate::stats::tests::provider_subscription(&second, "OB", None),
        ];
        let samples = vec![
            sample(
                "atlas",
                "2026-11-10T08:00:00Z",
                first.clone(),
                vec![weekly(0.0, "2026-11-16T00:00:00Z")],
            ),
            sample(
                "atlas",
                "2026-11-10T09:00:00Z",
                first.clone(),
                vec![weekly(10.0, "2026-11-16T00:00:00Z")],
            ),
        ];
        let mut prices = prices();
        crate::stats::tests::codex_prices(&mut prices);
        let tokens = vec![
            Hosted {
                host: "atlas".to_string(),
                record: crate::stats::tests::codex_token(
                    "2026-11-10T08:10:00Z",
                    "gpt-6.2-codex",
                    12_000_000,
                ),
            },
            Hosted {
                host: "atlas".to_string(),
                record: crate::stats::tests::codex_token(
                    "2026-11-10T08:20:00Z",
                    "gpt-6.2-codex",
                    12_000_000,
                ),
            },
        ];
        let subject = RateAccount::of_provider(&subscriptions[0]);

        let learned = rates_in(
            std::slice::from_ref(&subject),
            &samples,
            &tokens,
            &[],
            &subscriptions,
            &prices,
            utc("2026-11-10T08:00:00Z"),
            utc("2026-11-10T10:00:00Z"),
        );
        assert!(
            learned.rates.is_empty(),
            "no pair can be booked for either login: {:?}",
            learned.rates
        );
        let warnings = learned.warnings();
        assert_eq!(warnings.len(), 1, "one warning per host: {warnings:?}");
        assert!(
            warnings[0].starts_with("the OpenAI record of gpt-6.2-codex on atlas at "),
            "{warnings:?}"
        );
        assert!(
            warnings[0].contains(
                "belongs to no stored OpenAI login (several logins and no providerSwitch)"
            ),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_plan_name_prior_stays_within_its_company() {
        // An OpenAI subscription on "Pro" with 120 USD of measured work, and an
        // Anthropic account on "Pro" with no capacity and no samples. Without
        // the company guard the Anthropic account would take the OpenAI plan's
        // measured pairs as its prior; it has no learned rate of its own.
        let openai = AccountRef {
            acc: "3c1e7a94-6d2b-4f08-9a51-2e7c4b6d8f10".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let mut subscription = crate::stats::tests::provider_subscription(&openai, "OA", None);
        subscription.plan = Some("Pro".to_string());
        let provider = RateAccount::of_provider(&subscription);
        let subjects = [on_plan(ac(), None, "Pro"), provider];
        let samples = vec![
            sample(
                "atlas",
                "2026-11-10T08:00:00Z",
                openai.clone(),
                vec![weekly(0.0, "2026-11-16T00:00:00Z")],
            ),
            sample(
                "atlas",
                "2026-11-10T09:00:00Z",
                openai.clone(),
                vec![weekly(10.0, "2026-11-16T00:00:00Z")],
            ),
        ];
        let mut prices = prices();
        crate::stats::tests::codex_prices(&mut prices);
        let tokens = vec![Hosted {
            host: "atlas".to_string(),
            record: crate::stats::tests::codex_token(
                "2026-11-10T08:00:00Z",
                "gpt-6.2-codex",
                12_000_000,
            ),
        }];
        let learned = rates_in(
            &subjects,
            &samples,
            &tokens,
            &[],
            std::slice::from_ref(&subscription),
            &prices,
            utc("2026-11-10T08:00:00Z"),
            utc("2026-11-10T10:00:00Z"),
        );
        let openai_rates = learned.rates.get(&openai).expect("OA's measured factor");
        assert!(openai_rates.c.measured);
        assert!(
            !learned.rates.contains_key(&ac()),
            "an OpenAI Pro says nothing about an Anthropic Pro: {:?}",
            learned.rates
        );
    }

    #[test]
    fn a_provider_pair_with_no_record_of_its_own_is_left_out() {
        // A pair of OA readings with no OpenAI record in its span says nothing
        // about the plan (Codex on another machine, the web).
        let openai = AccountRef {
            acc: "3c1e7a94-6d2b-4f08-9a51-2e7c4b6d8f10".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        let subscription = crate::stats::tests::provider_subscription(&openai, "OA", None);
        let subject = RateAccount::of_provider(&subscription);
        let samples = vec![
            sample(
                "atlas",
                "2026-11-10T08:00:00Z",
                openai.clone(),
                vec![weekly(0.0, "2026-11-16T00:00:00Z")],
            ),
            sample(
                "atlas",
                "2026-11-10T09:00:00Z",
                openai.clone(),
                vec![weekly(10.0, "2026-11-16T00:00:00Z")],
            ),
        ];
        let mut prices = prices();
        crate::stats::tests::codex_prices(&mut prices);
        let pools = WindowPools::of(&[], std::slice::from_ref(&subscription));
        let without = window_pairs(
            &subject,
            &sample_lines(&openai, &samples),
            &[],
            &pools,
            &prices,
            PairLimit::Main,
            utc("2026-11-10T08:00:00Z"),
            utc("2026-11-10T10:00:00Z"),
        );
        assert!(without.is_empty(), "{without:?}");

        let tokens = vec![Hosted {
            host: "atlas".to_string(),
            record: crate::stats::tests::codex_token(
                "2026-11-10T08:00:00Z",
                "gpt-6.2-codex",
                12_000_000,
            ),
        }];
        let with = window_pairs(
            &subject,
            &sample_lines(&openai, &samples),
            &tokens,
            &pools,
            &prices,
            PairLimit::Main,
            utc("2026-11-10T08:00:00Z"),
            utc("2026-11-10T10:00:00Z"),
        );
        assert_eq!(with.len(), 1, "{with:?}");
    }

    #[test]
    fn a_plan_without_any_measurement_starts_from_the_assumption() {
        // No sample at all: a 5x plan assumes the built-in factor over 5, and a
        // plan without a capacity has no rates.
        let accounts = [with_capacity(ac(), Some(5.0)), with_capacity(lb(), None)];
        let learned = rates(
            &accounts,
            &[],
            &[],
            &[],
            &prices(),
            utc("2026-11-14T18:00:00Z"),
        )
        .rates;
        let ac_rates = learned.get(&ac()).expect("AC has a capacity");
        assert!(
            (ac_rates.c.value - ASSUMED_WEEK_PERCENT_PER_USD_AT_1X / 5.0).abs() < EPS,
            "{:?}",
            ac_rates.c
        );
        assert!(!ac_rates.c.measured);
        assert!(ac_rates.s.is_none(), "no session was ever measured");
        assert!(!learned.contains_key(&lb()), "no capacity, no measurement");
    }

    #[test]
    fn a_measured_plan_moves_the_assumption_of_the_others() {
        // LB (capacity 20) gained 10 % of its week over 200 USD while live: at
        // capacity 1 that is 10 × 20 / 200 = 1 % per USD, and AC (capacity 5,
        // never measured) assumes a fifth of it. LB's own 200 USD are measured,
        // so its prior is dropped: its factor is 10 / 200.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                lb(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                lb(),
                vec![weekly(50.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T09:00:00Z", 100.0),
            token("atlas", "2026-11-14T10:00:00Z", 100.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(lb()))];
        let accounts = [
            with_capacity(lb(), Some(20.0)),
            with_capacity(ac(), Some(5.0)),
        ];
        let learned = rates(
            &accounts,
            &samples,
            &tokens,
            &events,
            &prices(),
            utc("2026-11-14T18:00:00Z"),
        )
        .rates;

        let ac_c = learned.get(&ac()).expect("AC").c;
        assert!((ac_c.value - 1.0 / 5.0).abs() < EPS, "{ac_c:?}");
        assert!(!ac_c.measured);
        let lb_c = learned.get(&lb()).expect("LB").c;
        let lb_expected = 10.0 / 200.0;
        assert!((lb_c.value - lb_expected).abs() < EPS, "{lb_c:?}");
        assert!(lb_c.measured);
    }

    #[test]
    fn a_rise_while_the_account_is_live_nowhere_is_left_out() {
        // AC rose 30 % while LB was live on the only host: the rise came from
        // use seatswap does not see. AC keeps its assumption unchanged; without
        // a capacity it has no rates at all.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(10.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![token("atlas", "2026-11-14T09:00:00Z", 400.0)];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(lb()))];
        let now = utc("2026-11-14T18:00:00Z");

        let learned = rates(
            &[with_capacity(ac(), Some(5.0))],
            &samples,
            &tokens,
            &events,
            &prices(),
            now,
        )
        .rates;
        let c = learned.get(&ac()).expect("AC has a capacity").c;
        assert!(
            (c.value - ASSUMED_WEEK_PERCENT_PER_USD_AT_1X / 5.0).abs() < EPS,
            "{c:?}"
        );
        assert!(rates_of(&ac(), &samples, &tokens, &events, now).is_none());
    }

    #[test]
    fn a_session_rate_is_assumed_from_another_measured_session() {
        // LB (capacity 5) filled 50 % of a 5 h window with 125 USD: 0.4 % per
        // USD, 2 % at capacity 1. AC (capacity 20, capped, no work) assumes
        // 0.1 % per USD; PR (capacity 20, no session limit) gets no s.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                lb(),
                vec![weekly(40.0, WEEK_RESET), session(0.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                lb(),
                vec![weekly(48.0, WEEK_RESET), session(50.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(1.0, WEEK_RESET), session(0.0, AC_SESSION)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                pr(),
                vec![weekly(1.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![token("atlas", "2026-11-14T10:00:00Z", 125.0)];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(lb()))];
        let accounts = [
            with_capacity(lb(), Some(5.0)),
            with_capacity(ac(), Some(20.0)),
            with_capacity(pr(), Some(20.0)),
        ];
        let learned = rates(
            &accounts,
            &samples,
            &tokens,
            &events,
            &prices(),
            utc("2026-11-14T18:00:00Z"),
        )
        .rates;
        let s = learned.get(&ac()).expect("AC").s.expect("AC is capped");
        assert!((s.value - 50.0 * 5.0 / 125.0 / 20.0).abs() < EPS, "{s:?}");
        assert!(learned.get(&pr()).expect("PR").s.is_none());
    }

    // ---- 1.3: one measure of work ----

    /// Review Focus 2: a stretch of work with a model that has no price. The
    /// pair that holds it is left out of the fit; the factor comes from the
    /// other pairs, and the model is named once, not once per record.
    #[test]
    fn a_pair_with_an_unpriced_model_is_left_out_not_zeroed() {
        // Three pairs of two hours: 10 % over 100 USD, 20 % over 50 USD and
        // two lines of `claude-new-9`, 10 % over 100 USD. Without the middle
        // pair c = 20 / 200 = 0.1; with its unpriced lines valued as 0 it
        // would be 40 / 250 = 0.16.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(10.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T11:00:00Z",
                ac(),
                vec![weekly(20.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T15:00:00Z",
                ac(),
                vec![weekly(50.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T09:00:00Z", 50.0),
            token("atlas", "2026-11-14T10:00:00Z", 50.0),
            token("atlas", "2026-11-14T11:00:00Z", 50.0),
            token_of("claude-new-9", "atlas", "2026-11-14T11:00:00Z", 30.0),
            token_of("claude-new-9", "atlas", "2026-11-14T12:00:00Z", 30.0),
            token("atlas", "2026-11-14T13:00:00Z", 50.0),
            token("atlas", "2026-11-14T14:00:00Z", 50.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(ac()))];
        let now = utc("2026-11-14T18:00:00Z");

        let learned = rates(
            &[with_capacity(ac(), None)],
            &samples,
            &tokens,
            &events,
            &prices(),
            now,
        );
        let c = learned.rates.get(&ac()).expect("200 USD of priced pairs").c;
        assert!((c.value - 0.1).abs() < EPS, "{c:?}");
        assert!((c.basis_usd - 200.0).abs() < EPS, "{c:?}");
        assert_eq!(
            learned.unpriced,
            BTreeSet::from(["claude-new-9".to_string()])
        );
        assert_eq!(
            learned.warnings(),
            vec![
                "no price for claude-new-9: its work is left out of auto-burn's rates and rhythm"
                    .to_string()
            ]
        );

        // The rhythm leaves the unpriced lines out as well: never as 0, never
        // guessed.
        let priced: Vec<Hosted<TokenRecord>> = tokens
            .iter()
            .filter(|hosted| hosted.record.model == "claude-opus-5-5")
            .cloned()
            .collect();
        assert_eq!(
            rhythm(&tokens, now, &east_one(), &prices()),
            rhythm(&priced, now, &east_one(), &prices())
        );
    }

    #[test]
    fn a_pair_live_for_less_than_90_percent_is_left_out() {
        // Two pairs of two hours, 100 USD each: 10 % and 30 %. AC is live on
        // atlas until 12:45, so the second pair has AC live for 105 of its 120
        // minutes (87.5 %): it is left out, c = 10 / 100.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                ac(),
                vec![weekly(10.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T11:00:00Z",
                ac(),
                vec![weekly(20.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                ac(),
                vec![weekly(50.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T09:00:00Z", 100.0),
            token("atlas", "2026-11-14T11:00:00Z", 100.0),
        ];
        let now = utc("2026-11-14T18:00:00Z");
        let mut events = vec![
            live("atlas", "2026-11-14T08:00:00Z", Some(ac())),
            switched("atlas", "2026-11-14T12:45:00Z", lb()),
        ];
        let short = rates_of(&ac(), &samples, &tokens, &events, now).expect("the first pair");
        assert!((short.c.value - 0.1).abs() < EPS, "{:?}", short.c);
        assert!((short.c.basis_usd - 100.0).abs() < EPS, "{:?}", short.c);

        // AC live on vega from 12:30 closes the gap: the union of both hosts
        // covers the whole pair, which now counts: c = 40 / 200.
        events.push(live("vega", "2026-11-14T12:30:00Z", Some(ac())));
        let union = rates_of(&ac(), &samples, &tokens, &events, now).expect("both pairs");
        assert!((union.c.value - 0.2).abs() < EPS, "{:?}", union.c);
    }

    /// Review Focus 4: a new install or a new account nobody has measured
    /// starts from its plan at once; nothing waits a week.
    #[test]
    fn an_unmeasured_pro_account_starts_from_its_plan_name() {
        // Pro's tier names no multiplier: its plan name makes it capacity 1.
        // Max's tier is `_5x`.
        let pro = stored(&pr(), "PR", "Pro", Some("default_claude_ai"));
        let max = stored(&lb(), "LB", "Max", Some("default_claude_max_5x"));
        let accounts = [RateAccount::of(&pro), RateAccount::of(&max)];
        assert_eq!(accounts[0].capacity, Some(1.0));
        let learned = rates(
            &accounts,
            &[],
            &[],
            &[],
            &prices(),
            utc("2026-11-14T18:00:00Z"),
        )
        .rates;
        let pro_c = learned.get(&pr()).expect("Pro has a capacity").c;
        assert!((pro_c.value - 0.75).abs() < EPS, "{pro_c:?}");
        assert!(!pro_c.measured);
        let max_c = learned.get(&lb()).expect("Max has a capacity").c;
        assert!((max_c.value - 0.15).abs() < EPS, "{max_c:?}");
        assert!(!max_c.measured);
    }

    #[test]
    fn an_account_takes_the_factor_of_a_measured_account_on_the_same_plan() {
        // LB (Team Premium, capacity 5) gained 10 % over 200 USD: 0.05 % per
        // USD. AC is Team Premium too but its tier is not known: it takes LB's
        // factor by the plan's name. PR (Ultra, capacity 20) takes the pooled
        // factor per capacity: 10 × 5 / 200 / 20. OR (Team Standard, no
        // capacity) has no prior and no rates.
        let samples = vec![
            sample(
                "atlas",
                "2026-11-14T09:00:00Z",
                lb(),
                vec![weekly(40.0, WEEK_RESET)],
            ),
            sample(
                "atlas",
                "2026-11-14T13:00:00Z",
                lb(),
                vec![weekly(50.0, WEEK_RESET)],
            ),
        ];
        let tokens = vec![
            token("atlas", "2026-11-14T09:00:00Z", 100.0),
            token("atlas", "2026-11-14T10:00:00Z", 100.0),
        ];
        let events = vec![live("atlas", "2026-11-14T08:00:00Z", Some(lb()))];
        let accounts = [
            on_plan(lb(), Some(5.0), "Team Premium"),
            on_plan(ac(), None, "Team Premium"),
            on_plan(pr(), Some(20.0), "Ultra"),
            on_plan(or(), None, "Team Standard"),
        ];
        let learned = rates(
            &accounts,
            &samples,
            &tokens,
            &events,
            &prices(),
            utc("2026-11-14T18:00:00Z"),
        )
        .rates;

        let ac_c = learned.get(&ac()).expect("AC takes LB's factor").c;
        assert!((ac_c.value - 0.05).abs() < EPS, "{ac_c:?}");
        assert!(!ac_c.measured);
        let pr_c = learned.get(&pr()).expect("PR takes the pooled factor").c;
        assert!(
            (pr_c.value - 10.0 * 5.0 / 200.0 / 20.0).abs() < EPS,
            "{pr_c:?}"
        );
        assert!(!learned.contains_key(&or()), "no plan peer, no capacity");
        // LB's own 200 USD are measured, so its prior is dropped: 10 / 200.
        let lb_c = learned.get(&lb()).expect("LB").c;
        let lb_expected = 10.0 / 200.0;
        assert!((lb_c.value - lb_expected).abs() < EPS, "{lb_c:?}");
    }

    /// R2 of 1.3: the events of the timelines are read for all time, so an
    /// account live for weeks without a switch still has a timeline.
    #[test]
    fn timelines_reach_back_past_rates_days() {
        let dir = tempfile::tempdir().expect("temp dir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let now = utc("2026-11-14T18:00:00Z");
        let (samples, tokens) = ac_day();
        history
            .append(&Event {
                t: now - Duration::days(20),
                kind: EventKind::Live {
                    account: Some(ac()),
                },
            })
            .expect("the live event");
        for hosted in &samples {
            history.append(&hosted.record).expect("a sample");
        }
        for hosted in &tokens {
            history.append(&hosted.record).expect("a token line");
        }
        // Team Standard: no capacity, so only a measurement gives rates.
        let accounts = [stored(&ac(), "AC", "Team Standard", None)];

        let learned = super::super::live::learn(
            &history,
            now,
            FixedOffset::east_opt(0).expect("UTC"),
            &accounts,
            true,
            &prices(),
        )
        .expect("the history reads");
        let c = learned.rates.get(&ac()).expect("AC was live 20 days ago").c;
        assert!((c.value - 0.064).abs() < EPS, "{c:?}");
        assert!(c.measured);
        assert!(learned.events.is_empty(), "no event of the last 14 days");
    }
}
