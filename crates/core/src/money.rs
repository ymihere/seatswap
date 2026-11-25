//! Money (spec §1.4, §1.4.1, §3.6): the API prices of models with their dates,
//! the dollar rate with its dates, and the value of token records. Everything is
//! kept in two files seatswap owns (`prices.json`, `fx.json`); a missing value is
//! `None`, never a number.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;

use chrono::{
    DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday,
};
use serde::{Deserialize, Serialize};

use crate::api::{Api, ApiError, Fetched, LimitKind};
use crate::atomic::{self, Access};
use crate::burn::rates::{timelines, Timeline};
use crate::history::{
    keep_warnings, same_reset, AccountRef, Event, EventKind, HistoryDir, HistoryError, Hosted,
    Hosts, Range, Sample, TokenRecord, WindowRecord,
};
use crate::locks::{self, LockError};
use crate::paths::AppPaths;
use crate::stats::{warn_window_below_start, window_midpoint, window_used};
use crate::store::{Account, ProviderSubscription, Settings};
use crate::switcher::APP_LOCK_TIMEOUT;

/// The models.dev providers seatswap reads prices from (spec §1.4.1).
pub const PRICE_PROVIDERS: [&str; 3] = ["anthropic", "openai", "ollama-cloud"];

/// The version of `prices.json` and `fx.json` this build reads and writes.
const MONEY_VERSION: u32 = 1;

/// The longest window the [`Valuer`] values: Ollama's credits month, one
/// calendar month of at most 31 days. A week is shorter. A window that overlaps
/// a range begins at most this many days before it and ends at most this many
/// days after it, so [`Valuer::build`] reads the range widened by them on both
/// sides and every window it values is read whole.
const LONGEST_WINDOW_DAYS: i64 = 31;

/// How far a usage reading may lie from a record's span and still book its
/// work to the reading's account (R4, documented rule): 30 minutes before the
/// record's start for the last reading, and 30 minutes after its end for the
/// first.
const READING_REACH_MINUTES: i64 = 30;

const MICRO: f64 = 1_000_000.0;

#[derive(Debug, thiserror::Error)]
pub enum MoneyError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} has version {version}, but this build reads version 1")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("models.dev's answer is not the expected model list: {0}")]
    ModelsDev(String),
    #[error("the look-up failed: {0}")]
    Api(#[from] ApiError),
    #[error("cannot take the app lock: {0}")]
    Lock(#[from] LockError),
    #[error("{currency}'s dollar rate {rate} is not a number above 0")]
    BadRate { currency: String, rate: f64 },
    #[error("the range {from} .. {to} lies too close to the end of time to read its windows")]
    RangeTooWide {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    #[error("the month ending {end} has no start one calendar month before it")]
    MonthStart { end: DateTime<Utc> },
    #[error(transparent)]
    History(#[from] HistoryError),
}

/// What one model costs at API prices, in USD per million tokens (spec §1.4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// The model has `cost.tiers` (a price that depends on the prompt size);
    /// this step counts its base price.
    pub tiered: bool,
}

impl ModelPrice {
    /// The USD cost of these four token counts at this price.
    fn usd(&self, input: u64, output: u64, cache_write: u64, cache_read: u64) -> f64 {
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_write as f64 * self.cache_write
            + cache_read as f64 * self.cache_read)
            / MICRO
    }

    /// The one price-validity rule (spec §1.4.1): each of the four numbers is
    /// finite and 0 or more. The text names the field.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("input", self.input),
            ("output", self.output),
            ("cache write", self.cache_write),
            ("cache read", self.cache_read),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!(
                    "the {name} price must be a number of 0 or more, got {value}"
                ));
            }
        }
        Ok(())
    }
}

/// One model's price from one moment on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricePoint {
    pub from: DateTime<Utc>,
    pub price: ModelPrice,
}

/// What a look-up changed, for the settings to show (spec §1.4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceChange {
    pub at: DateTime<Utc>,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

/// The price of one key in effect now: its last looked-up point (spec §1.4).
/// `since` is when that point started.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentPrice {
    pub key: String,
    pub price: ModelPrice,
    pub since: Option<DateTime<Utc>>,
}

/// `prices.json`: the looked-up prices with their dates and the last look-up's
/// state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prices {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change: Option<PriceChange>,
    /// Key [`price_key`], points in time order.
    #[serde(default)]
    pub models: BTreeMap<String, Vec<PricePoint>>,
    /// The keys whose last looked-up list did not have them. A key is reported
    /// as removed once, not on every later list that still lacks it; an older
    /// `prices.json` loads with this empty.
    #[serde(default)]
    pub missing: BTreeSet<String>,
}

impl Default for Prices {
    fn default() -> Self {
        Prices {
            version: MONEY_VERSION,
            checked_at: None,
            etag: None,
            last_change: None,
            models: BTreeMap::new(),
            missing: BTreeSet::new(),
        }
    }
}

impl Prices {
    /// `prices.json`; a missing file is empty. A file that does not parse or
    /// names another version is an error naming the path, never replaced.
    pub fn load(app: &AppPaths) -> Result<Prices, MoneyError> {
        let path = app.prices_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Prices::default()),
            Err(source) => return Err(MoneyError::Io { path, source }),
        };
        let prices: Prices = serde_json::from_str(&text).map_err(|source| MoneyError::Json {
            path: path.clone(),
            source,
        })?;
        if prices.version != MONEY_VERSION {
            return Err(MoneyError::UnsupportedVersion {
                path,
                version: prices.version,
            });
        }
        Ok(prices)
    }

    /// Pretty JSON, mode 0600, through [`atomic::write_atomic`].
    pub fn save(&self, app: &AppPaths) -> Result<(), MoneyError> {
        let path = app.prices_file();
        save_json(app, &path, self.version, self)
    }

    /// The price of a model as the valuer looks it up: under the key
    /// [`model_price_key`] gives its id, at `at`. `None` when that key has no
    /// price; it is never looked up under another provider.
    pub fn of_model(&self, model: &str, at: DateTime<Utc>) -> Option<&ModelPrice> {
        self.at(&model_price_key(model), at)
    }

    /// The price of `key` at `at`: the last point at or before `at`, else the
    /// first point. `None` when there is no price at all.
    pub fn at(&self, key: &str, at: DateTime<Utc>) -> Option<&ModelPrice> {
        let points = self.models.get(key)?;
        let after = points.partition_point(|point| point.from <= at);
        if after == 0 {
            points.first().map(|point| &point.price)
        } else {
            points.get(after - 1).map(|point| &point.price)
        }
    }

    /// The current price of every key: its last looked-up point, sorted by key.
    /// The one place the CLI table, the price list and the count get "the price
    /// of every model" from.
    pub fn current(&self) -> Vec<CurrentPrice> {
        self.models
            .iter()
            .filter_map(|(key, points)| {
                points.last().map(|point| CurrentPrice {
                    key: key.clone(),
                    price: point.price.clone(),
                    since: Some(point.from),
                })
            })
            .collect()
    }

    /// Take a new list: add a point for every model that is new or whose price
    /// differs from its last point. A model missing from the list keeps its
    /// points and is reported as removed only the first time it is missing. A
    /// model that comes back leaves [`Prices::missing`]. Returns what changed,
    /// or `None` when nothing did; `last_change` keeps its value then.
    pub fn take(
        &mut self,
        list: BTreeMap<String, ModelPrice>,
        at: DateTime<Utc>,
    ) -> Option<PriceChange> {
        let mut added = Vec::new();
        let mut changed = Vec::new();
        for (key, price) in &list {
            self.missing.remove(key);
            match self.models.get_mut(key) {
                Some(points) => {
                    let differs = points
                        .last()
                        .map(|point| point.price != *price)
                        .unwrap_or(true);
                    if differs {
                        insert_point(
                            points,
                            PricePoint {
                                from: at,
                                price: price.clone(),
                            },
                        );
                        changed.push(key.clone());
                    }
                }
                None => {
                    self.models.insert(
                        key.clone(),
                        vec![PricePoint {
                            from: at,
                            price: price.clone(),
                        }],
                    );
                    added.push(key.clone());
                }
            }
        }
        let mut removed = Vec::new();
        for key in self.models.keys() {
            if !list.contains_key(key) && self.missing.insert(key.clone()) {
                removed.push(key.clone());
            }
        }
        if added.is_empty() && changed.is_empty() && removed.is_empty() {
            return None;
        }
        let change = PriceChange {
            at,
            added,
            changed,
            removed,
        };
        self.last_change = Some(change.clone());
        Some(change)
    }
}

/// Insert a point keeping `points` sorted by `from`; a point at the same time
/// replaces the one there. A look-up whose clock moved back then still leaves
/// the list [`Prices::at`] searches in order.
fn insert_point(points: &mut Vec<PricePoint>, point: PricePoint) {
    match points.binary_search_by(|existing| existing.from.cmp(&point.from)) {
        Ok(index) => points[index] = point,
        Err(index) => points.insert(index, point),
    }
}

/// One day's dollar rate: units of the file's currency per 1 USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FxPoint {
    pub date: NaiveDate,
    pub rate: f64,
    /// When the point was fetched. An old `fx.json` has no such field, so its
    /// points keep `None` and the page says "fetched: not recorded".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
}

/// `fx.json`: one currency and its rates with their dates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fx {
    pub version: u32,
    /// The currency these rates are for; empty when nothing was looked up yet.
    pub currency: String,
    /// In date order.
    #[serde(default)]
    pub points: Vec<FxPoint>,
}

impl Default for Fx {
    fn default() -> Self {
        Fx {
            version: MONEY_VERSION,
            currency: String::new(),
            points: Vec::new(),
        }
    }
}

impl Fx {
    /// `fx.json`; a missing file is empty. A file that does not parse or names
    /// another version is an error naming the path.
    pub fn load(app: &AppPaths) -> Result<Fx, MoneyError> {
        let path = app.fx_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Fx::default()),
            Err(source) => return Err(MoneyError::Io { path, source }),
        };
        let fx: Fx = serde_json::from_str(&text).map_err(|source| MoneyError::Json {
            path: path.clone(),
            source,
        })?;
        if fx.version != MONEY_VERSION {
            return Err(MoneyError::UnsupportedVersion {
                path,
                version: fx.version,
            });
        }
        Ok(fx)
    }

    /// Pretty JSON, mode 0600, through [`atomic::write_atomic`].
    pub fn save(&self, app: &AppPaths) -> Result<(), MoneyError> {
        let path = app.fx_file();
        save_json(app, &path, self.version, self)
    }

    /// Replace the point of `date`, or add one, keeping the points in date order.
    /// The point keeps no fetch time; [`Fx::take_fetched`] records one.
    pub fn take(&mut self, date: NaiveDate, rate: f64) {
        self.take_fetched(date, rate, None);
    }

    /// [`Fx::take`] with the time the rate was fetched; a looked-up point has
    /// one, so the page can say when it arrived.
    pub fn take_fetched(&mut self, date: NaiveDate, rate: f64, fetched_at: Option<DateTime<Utc>>) {
        match self.points.binary_search_by_key(&date, |point| point.date) {
            Ok(index) => {
                self.points[index].rate = rate;
                self.points[index].fetched_at = fetched_at;
            }
            Err(index) => self.points.insert(
                index,
                FxPoint {
                    date,
                    rate,
                    fetched_at,
                },
            ),
        }
    }

    /// The rate at `date`: the point at or before it, else the first point.
    pub fn at(&self, date: NaiveDate) -> Option<f64> {
        self.point_at(date).map(|point| point.rate)
    }

    /// When the point in effect at `date` was fetched; `None` for a point
    /// written by an older build (the documented "fetched: not recorded").
    pub fn fetched_at(&self, date: NaiveDate) -> Option<DateTime<Utc>> {
        self.point_at(date).and_then(|point| point.fetched_at)
    }

    fn point_at(&self, date: NaiveDate) -> Option<&FxPoint> {
        let after = self.points.partition_point(|point| point.date <= date);
        if after == 0 {
            self.points.first()
        } else {
            self.points.get(after - 1)
        }
    }
}

fn save_json<T: Serialize>(
    app: &AppPaths,
    path: &std::path::Path,
    version: u32,
    value: &T,
) -> Result<(), MoneyError> {
    if version != MONEY_VERSION {
        return Err(MoneyError::UnsupportedVersion {
            path: path.to_path_buf(),
            version,
        });
    }
    atomic::ensure_private_dir(&app.data_dir).map_err(|source| MoneyError::Io {
        path: app.data_dir.clone(),
        source,
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| MoneyError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    atomic::write_atomic(path, &bytes, Access::Private).map_err(|source| MoneyError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// models.dev's list reduced to [`PRICE_PROVIDERS`]: the prices it could read,
/// and the models it left out with the reason (`"<key>: <reason>"`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelsDevList {
    pub prices: BTreeMap<String, ModelPrice>,
    pub left_out: Vec<String>,
}

/// models.dev's `api.json` → the prices of [`PRICE_PROVIDERS`]. A body with none
/// of those providers is an error, not an empty list. A model without
/// `cost.input` and `cost.output` is left out; a missing `cache_read` /
/// `cache_write` is 0.0; a cost that is not a number, negative or not finite
/// leaves that model out of the list and names it in [`ModelsDevList::left_out`],
/// never failing the whole list; `tiered` is set when it has `cost.tiers`.
pub fn prices_from_models_dev(body: &serde_json::Value) -> Result<ModelsDevList, MoneyError> {
    let providers = body
        .as_object()
        .ok_or_else(|| MoneyError::ModelsDev("not a JSON object of providers".to_string()))?;
    let mut prices = BTreeMap::new();
    let mut left_out = Vec::new();
    let mut found = 0;
    for provider in PRICE_PROVIDERS {
        let Some(entry) = providers.get(provider) else {
            continue;
        };
        let models = entry
            .get("models")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| MoneyError::ModelsDev(format!("{provider} has no list of models")))?;
        found += 1;
        for model in models.values() {
            let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let key = price_key(provider, id);
            let cost = model.get("cost");
            let number = |name: &str| -> Result<Option<f64>, String> {
                let Some(value) = cost.and_then(|cost| cost.get(name)) else {
                    return Ok(None);
                };
                value
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| format!("the {name} cost is not a number: {value}"))
            };
            let parse = || -> Result<Option<ModelPrice>, String> {
                let (Some(input), Some(output)) = (number("input")?, number("output")?) else {
                    return Ok(None);
                };
                let price = ModelPrice {
                    input,
                    output,
                    cache_write: number("cache_write")?.unwrap_or(0.0),
                    cache_read: number("cache_read")?.unwrap_or(0.0),
                    tiered: cost
                        .and_then(|cost| cost.get("tiers"))
                        .is_some_and(|tiers| !tiers.is_null()),
                };
                price.validate()?;
                Ok(Some(price))
            };
            match parse() {
                Ok(Some(price)) => {
                    prices.insert(key, price);
                }
                Ok(None) => {}
                Err(reason) => left_out.push(format!("{key}: {reason}")),
            }
        }
    }
    if found == 0 {
        return Err(MoneyError::ModelsDev(format!(
            "the body has none of the providers {}",
            PRICE_PROVIDERS.join(", ")
        )));
    }
    Ok(ModelsDevList { prices, left_out })
}

/// A model's key in [`Prices`]: `"<provider>/<model id>"`.
pub fn price_key(provider: &str, model_id: &str) -> String {
    format!("{provider}/{model_id}")
}

/// The company a subscription or a token record's model belongs to (spec
/// §11.5): Anthropic's plan, OpenAI (Codex), Ollama's cloud (DeepSeek Harness
/// and flash workers), or Google (the Antigravity CLI `agy`). An OpenAI or
/// Ollama record belongs to its company and to no Claude account; no record is
/// Google's ([`Company::work_recorded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Company {
    Anthropic,
    OpenAI,
    Ollama,
    Google,
}

impl Company {
    /// Every company, in the order the pages list them.
    pub const ALL: [Company; 4] = [
        Company::Anthropic,
        Company::OpenAI,
        Company::Ollama,
        Company::Google,
    ];

    /// The company of a model id: the one rule, which [`model_price_key`]
    /// and [`is_plan_model`] follow.
    ///
    /// - An id that starts with `claude-` is Anthropic's.
    /// - An Ollama cloud id, as DeepSeek Harness and flash name it, is
    ///   Ollama's: `<name>:cloud`, or `<name>:<tag>-cloud`
    ///   (`deepseek-v4.1-flash:1017-cloud`), name and tag not empty.
    /// - Any other id is OpenAI's: Codex is the only other source of model ids.
    pub fn of_model(model: &str) -> Company {
        priced_as(model).0
    }

    /// The company of an account reference: the org `codex` is OpenAI's, the
    /// org `ollama` is Ollama's, the org `google` is Google's, and every other
    /// org (a Claude organization id) is Anthropic's.
    pub fn of_account(account: &AccountRef) -> Company {
        match account.org.as_str() {
            crate::providers::CODEX_ORG => Company::OpenAI,
            crate::providers::OLLAMA_ORG => Company::Ollama,
            crate::providers::GOOGLE_ORG => Company::Google,
            // A Claude organization id is a 36-character UUID.
            _ => Company::Anthropic,
        }
    }

    /// The name a page writes for the company.
    pub fn name(&self) -> &'static str {
        match self {
            Company::Anthropic => "Anthropic",
            Company::OpenAI => "OpenAI",
            Company::Ollama => "Ollama",
            Company::Google => "Google",
        }
    }

    /// Whether seatswap records the work done on the company's subscriptions as
    /// token records. It has none of agy's, so what a Google subscription's use
    /// was worth is unknown ([`Missing::WorkNotRecorded`]), never 0, and a
    /// figure that sets work against its price leaves Google out.
    pub fn work_recorded(&self) -> bool {
        match self {
            Company::Anthropic | Company::OpenAI | Company::Ollama => true,
            Company::Google => false,
        }
    }

    /// The name of the company's price list on models.dev. Google's is not
    /// read ([`PRICE_PROVIDERS`]): no record is priced from it.
    pub fn price_list(&self) -> &'static str {
        match self {
            Company::Anthropic => "anthropic",
            Company::OpenAI => "openai",
            Company::Ollama => "ollama-cloud",
            Company::Google => "google",
        }
    }
}

/// The one Ollama subscription's account, as `providers_rt` creates it (R4,
/// decision P-2 2): Ollama has one local server and one credit balance, so its
/// readings all belong to this reference.
pub(crate) fn ollama_subscription() -> AccountRef {
    AccountRef {
        acc: crate::providers::OLLAMA_ORG.to_string(),
        org: crate::providers::OLLAMA_ORG.to_string(),
    }
}

/// The OpenAI logins `accounts.json` stores (decision P-2 2): every provider
/// subscription whose org is `codex`. This is what the no-`providerSwitch` rule
/// counts, never a window or a sample of a page's read range.
pub(crate) fn openai_subscriptions(subscriptions: &[ProviderSubscription]) -> BTreeSet<AccountRef> {
    subscriptions
        .iter()
        .filter(|subscription| Company::of_account(&subscription.account) == Company::OpenAI)
        .map(|subscription| subscription.account.clone())
        .collect()
}

/// The `providerSwitch` timelines of every host the events name. A `switch`
/// stays Claude-only, and [`Timeline::of`] ignores a `providerSwitch` in the
/// same way.
pub(crate) fn provider_switch_timelines(events: &[Hosted<Event>]) -> HashMap<String, Timeline> {
    let mut hosts: BTreeSet<&str> = BTreeSet::new();
    for hosted in events {
        hosts.insert(hosted.host.as_str());
    }
    hosts
        .into_iter()
        .map(|host| (host.to_string(), Timeline::provider_switches(events, host)))
        .collect()
}

/// Which OpenAI subscription a record on `host` at `t` belongs to (decision
/// P-2 2): the `to` of the newest `providerSwitch` at or before it; before that
/// host's first switch, that event's `from`; on a host with no switch at all,
/// the only stored OpenAI subscription when there is exactly one, and
/// [`Missing::NoAccount`] when there are several or none. Never a guess. The
/// one rule [`Valuer::openai_subscription`] and the blocked spans use.
pub(crate) fn openai_subscription_at(
    timelines: &HashMap<String, Timeline>,
    stored: &BTreeSet<AccountRef>,
    host: &str,
    t: DateTime<Utc>,
) -> Result<AccountRef, Missing> {
    if let Some(timeline) = timelines.get(host) {
        if !timeline.is_empty() {
            if let Some(account) = timeline.at(t) {
                return Ok(account.clone());
            }
            return timeline.before_first().cloned().ok_or(Missing::NoAccount);
        }
    }
    match stored.len() {
        1 => stored.iter().next().cloned().ok_or(Missing::NoAccount),
        _ => Err(Missing::NoAccount),
    }
}

/// The one wording of work no stored OpenAI login can be named for: several
/// logins are stored and the host has no `providerSwitch`, so
/// [`openai_subscription_at`] cannot say which one a record or a block belongs
/// to. `what` names the work (`record of {model}`, `block of {abbr}`); the
/// readers leave it out and say so with this sentence.
pub(crate) fn no_openai_login_warning(what: &str, host: &str, t: DateTime<Utc>) -> String {
    format!(
        "the OpenAI {what} on {host} at {t} belongs to no stored OpenAI login \
         (several logins and no providerSwitch); its work is left out of every plan"
    )
}

/// The limit kind of Ollama's month (R4): the credits' period.
fn monthly_credits() -> LimitKind {
    LimitKind::Other("monthly_credits".to_string())
}

/// Whether a window or limit kind is Ollama's month. The one rule the month
/// windows and the week windows are told apart by.
fn is_month_kind(kind: &LimitKind) -> bool {
    matches!(kind, LimitKind::Other(name) if name == "monthly_credits")
}

/// A model id's company and its id in that company's models.dev list: the
/// decision behind [`Company::of_model`] and [`model_price_key`]. An Ollama
/// cloud id loses Ollama's `-cloud` marker there: `<name>:cloud` is `<name>`,
/// `<name>:<tag>-cloud` is `<name>:<tag>`.
fn priced_as(model: &str) -> (Company, &str) {
    if model.starts_with("claude-") {
        return (Company::Anthropic, model);
    }
    if let Some(name) = model.strip_suffix(":cloud") {
        if !name.is_empty() {
            return (Company::Ollama, name);
        }
    }
    if let Some(tagged) = model.strip_suffix("-cloud") {
        if let Some((name, tag)) = tagged.rsplit_once(':') {
            if !name.is_empty() && !tag.is_empty() {
                return (Company::Ollama, tagged);
            }
        }
    }
    (Company::OpenAI, model)
}

/// The key a model id is priced under (spec §11.5): the one rule every price
/// look-up of a token record goes through. The company is
/// [`Company::of_model`]'s.
///
/// - A Claude model is `anthropic/<id>`.
/// - An Ollama cloud model is in models.dev's `ollama-cloud` list without
///   Ollama's `-cloud` marker: `<name>:cloud` is `ollama-cloud/<name>`, and
///   `<name>:<tag>-cloud` is `ollama-cloud/<name>:<tag>`
///   (`deepseek-v4.1-flash:1017-cloud` is
///   `ollama-cloud/deepseek-v4.1-flash:1017`).
/// - A Codex model is `openai/<id>`.
///
/// A key without a price stays a missing price; no other company is tried.
pub fn model_price_key(model: &str) -> String {
    let (company, id) = priced_as(model);
    price_key(company.price_list(), id)
}

/// Whether `model` is a Claude Code model, so its tokens use the Claude plan
/// (spec §1.4, §3.8): [`Company::of_model`] is Anthropic. A DeepSeek Harness or
/// flash worker (`deepseek-v4.1-flash:cloud`), a Codex model (`gpt-6.1-sol`)
/// and an Ollama model (`glm-5:cloud`) are not, so their tokens take no share
/// of any Claude account's week.
pub fn is_plan_model(model: &str) -> bool {
    Company::of_model(model) == Company::Anthropic
}

/// The value of a token or session cost: the provider's own number when it
/// gives one, else models.dev's price. `None` when neither has one: there is
/// never a guessed or zero API value (spec §1.4).
///
pub fn api_value(provider_usd: Option<f64>, models_dev_usd: Option<f64>) -> Option<f64> {
    provider_usd.or(models_dev_usd)
}

/// An API value in US dollars in the API column's currency at `date`, with
/// that currency's code: the one conversion of every API value (models.dev's
/// and a provider's own cost). With the look-ups off it stays in US dollars,
/// whatever currency the settings keep; without a currency it is not known;
/// a currency without a stored rate yet leaves it in US dollars.
pub fn api_in_currency(
    usd: f64,
    settings: &Settings,
    fx: &Fx,
    date: NaiveDate,
) -> Result<(f64, String), Missing> {
    if !settings.look_up_prices {
        return Ok((usd, "USD".to_string()));
    }
    match (&settings.currency, usd_rate(settings, fx, date)) {
        (None, _) => Err(Missing::NoCurrency),
        (Some(currency), Some(rate)) => Ok((usd * rate.value, currency.clone())),
        (Some(_), None) => Ok((usd, "USD".to_string())),
    }
}

/// The dollar rate used at a date (spec §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rate {
    /// Units of the settings' currency per 1 USD.
    pub value: f64,
    /// The day the looked-up rate is from; `None` for USD.
    pub date: Option<NaiveDate>,
}

/// The rate to use at `date`: a point of `fx` looked up for the settings'
/// currency. A rate of another currency is never used. Currency "USD" is always
/// 1.0; without a currency, or with `lookUpPrices` off, there is no rate (the
/// API values are then in USD).
pub fn usd_rate(settings: &Settings, fx: &Fx, date: NaiveDate) -> Option<Rate> {
    let currency = settings.currency.as_deref()?;
    if currency == "USD" {
        return Some(Rate {
            value: 1.0,
            date: None,
        });
    }
    if !settings.look_up_prices || fx.currency != currency {
        return None;
    }
    let point = fx.point_at(date)?;
    Some(Rate {
        value: point.rate,
        date: Some(point.date),
    })
}

/// The European Central Bank's day and the hour in Frankfurt, from UTC with the
/// EU's summer-time rule: UTC+2 from the last Sunday of March 01:00 UTC to the
/// last Sunday of October 01:00 UTC, UTC+1 otherwise. No time-zone crate: the
/// rule is two lines of arithmetic. `None` when chrono cannot build the date
/// [`rate_due`] needs: that is an error there, never a silent hour.
fn frankfurt_time(utc: DateTime<Utc>) -> Option<NaiveDateTime> {
    let naive = utc.naive_utc();
    let start = last_sunday(naive.year(), 3)?.and_hms_opt(1, 0, 0)?;
    let end = last_sunday(naive.year(), 10)?.and_hms_opt(1, 0, 0)?;
    let hours = if start <= naive && naive < end { 2 } else { 1 };
    naive.checked_add_signed(Duration::hours(hours))
}

/// The last Sunday of `month` in `year`; March and October always have 31 days.
/// `None` only when chrono has no such date.
fn last_sunday(year: i32, month: u32) -> Option<NaiveDate> {
    let last = NaiveDate::from_ymd_opt(year, month, 31)?;
    last.checked_sub_days(chrono::Days::new(u64::from(
        last.weekday().num_days_from_sunday(),
    )))
}

/// The text [`rate_due`] reports for a time chrono cannot place in Frankfurt.
fn frankfurt_error(at: DateTime<Utc>) -> String {
    format!(
        "the Frankfurt time of {at} cannot be built: it lies outside the dates this build knows"
    )
}

/// Whether the dollar rate is due at `now` (spec §1.4, lead decision 4): once
/// per Frankfurt day while the stored point is older than today's Frankfurt
/// date, and on Mon–Fri from 16:00 to 18:00 Frankfurt time every 30 minutes
/// after the last ask until an answer carries today's date.
///
/// `last_ask` is the last ask Frankfurter answered, for the stored currency: a
/// failed ask leaves it as it was, so the next check asks again. A time chrono
/// cannot place in Frankfurt is an error with a clear text, never a guessed
/// hour.
pub fn rate_due(
    now: DateTime<Utc>,
    stored: Option<NaiveDate>,
    last_ask: Option<DateTime<Utc>>,
) -> Result<bool, String> {
    let local = frankfurt_time(now).ok_or_else(|| frankfurt_error(now))?;
    let today = local.date();
    if stored.is_some_and(|date| date >= today) {
        return Ok(false);
    }
    let last_answered = match last_ask {
        Some(at) => Some((at, frankfurt_time(at).ok_or_else(|| frankfurt_error(at))?)),
        None => None,
    };
    let Some((last, answered_at)) = last_answered else {
        return Ok(true);
    };
    if answered_at.date() != today {
        return Ok(true);
    }
    let workday = !matches!(today.weekday(), Weekday::Sat | Weekday::Sun);
    let minute = local.hour() * 60 + local.minute();
    if !(workday && (16 * 60..18 * 60).contains(&minute)) {
        return Ok(false);
    }
    Ok(now.signed_duration_since(last) >= Duration::minutes(30))
}

/// What one look-up did.
#[derive(Debug, Clone, PartialEq)]
pub enum Looked {
    NotModified,
    Taken {
        change: Option<PriceChange>,
        left_out: Vec<String>,
    },
}

/// One look-up of models.dev's list (spec §1.4.1): load `prices.json` for its
/// ETag, ask models.dev, then take the app lock and load `prices.json` again so
/// a concurrent write is kept. Take the new list and store the new ETag and
/// `checkedAt`. "Not modified" only moves `checkedAt`. On any error nothing is
/// saved.
pub fn look_up_prices(api: &Api, app: &AppPaths, now: DateTime<Utc>) -> Result<Looked, MoneyError> {
    let etag = Prices::load(app)?.etag;
    let fetched = api.models_dev(etag.as_deref())?;
    let _lock = locks::acquire_app_lock(app, APP_LOCK_TIMEOUT)?;
    let mut prices = Prices::load(app)?;
    match fetched {
        Fetched::NotModified => {
            prices.checked_at = Some(now);
            prices.save(app)?;
            Ok(Looked::NotModified)
        }
        Fetched::New { etag, body } => {
            let list = prices_from_models_dev(&body)?;
            let change = prices.take(list.prices, now);
            prices.etag = etag;
            prices.checked_at = Some(now);
            prices.save(app)?;
            Ok(Looked::Taken {
                change,
                left_out: list.left_out,
            })
        }
    }
}

/// One look-up of the dollar rate for `currency`. "USD" has no rate to look up
/// and sends no request. A rate of an earlier currency in `fx.json` is replaced
/// by a new [`Fx`] for `currency`; a rate that is not finite or not above 0 is an
/// error, and nothing is saved. The file is read and written under the app lock.
/// The point is stamped with `now`, the round's clock, as [`look_up_prices`]
/// stamps its own.
pub fn look_up_rate(
    api: &Api,
    app: &AppPaths,
    currency: &str,
    now: DateTime<Utc>,
) -> Result<Option<FxPoint>, MoneyError> {
    if currency == "USD" {
        return Ok(None);
    }
    let (date, rate) = api.usd_rate(currency)?;
    if !(rate.is_finite() && rate > 0.0) {
        return Err(MoneyError::BadRate {
            currency: currency.to_string(),
            rate,
        });
    }
    let _lock = locks::acquire_app_lock(app, APP_LOCK_TIMEOUT)?;
    let mut fx = Fx::load(app)?;
    if fx.currency != currency {
        fx = Fx {
            version: MONEY_VERSION,
            currency: currency.to_string(),
            points: Vec::new(),
        };
    }
    let fetched_at = Some(now);
    fx.take_fetched(date, rate, fetched_at);
    fx.save(app)?;
    Ok(Some(FxPoint {
        date,
        rate,
        fetched_at,
    }))
}

/// What tokens of `model` cost at API prices at `at`, in USD: the work of
/// auto-burn and the statistics (spec R1 of 1.3). Each of the four token kinds
/// is priced at the model's models.dev price in effect at `at`, its base price:
/// the long-context premium of a tiered price is left out (documented rule).
/// The one place a token count becomes dollars.
///
/// `None` when the model has no price; such work is never valued as 0 or
/// guessed. Four counts of 0 cost nothing, so they are worth 0 even when the
/// model has no price (Claude Code writes such records with model
/// `<synthetic>`). The model is priced under the key [`model_price_key`] gives
/// it: a Claude model as `anthropic`, an Ollama cloud model as `ollama-cloud`,
/// any other (Codex) as `openai`.
pub fn work_usd(
    model: &str,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    prices: &Prices,
    at: DateTime<Utc>,
) -> Option<f64> {
    if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 {
        return Some(0.0);
    }
    let price = prices.of_model(model, at)?;
    Some(price.usd(input, output, cache_write, cache_read))
}

/// What one token record costs at API prices, in USD: [`work_usd`] of its
/// model, counts and time. `None` when the record's model has no price.
pub fn api_usd(record: &TokenRecord, prices: &Prices) -> Option<f64> {
    work_usd(
        &record.model,
        record.input,
        record.out,
        record.cw,
        record.cr,
        prices,
        record.t,
    )
}

/// The money value of one percent of one week of an account: monthly price ×
/// 12 / 52 / 100 (spec §1.4).
pub fn percent_value(monthly_price: f64) -> f64 {
    monthly_price * 12.0 / 52.0 / 100.0
}

/// The money of one weekly window, in the display currency: a full window is
/// `paid`, the share that expired is `expired` and the rest is `used`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowMoney {
    pub(crate) used: f64,
    pub(crate) paid: f64,
    pub(crate) expired: f64,
}

/// The money of a weekly window from its price and the percent it used (a
/// finished window's final percent, or a running window's latest sample):
/// `paid = 100 × pv`, `expired = max(0, 100 − percent) × pv`,
/// `used = paid − expired`.
pub(crate) fn window_money(price: f64, percent: f64) -> WindowMoney {
    let pv = percent_value(price);
    let paid = 100.0 * pv;
    let expired = (100.0 - percent).max(0.0) * pv;
    WindowMoney {
        used: paid - expired,
        paid,
        expired,
    }
}

/// The money of one month of an Ollama subscription (R4, decision P-2 3): the
/// month's credits are the whole of the monthly price, so one percent of them
/// is `price / 100`, not a week's [`percent_value`]. A month above 100 % counts
/// its whole use and expires nothing, as a week does.
pub(crate) fn month_money(price: f64, percent: f64) -> WindowMoney {
    let paid = price;
    let used = percent / 100.0 * price;
    let expired = (100.0 - percent).max(0.0) / 100.0 * price;
    WindowMoney {
        used,
        paid,
        expired,
    }
}

/// Why a value is missing (spec §1.4: the window says so). `Display` is the text a page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Missing {
    NoCurrency,
    NoPrice {
        model: String,
    },
    NoAccount,
    NoAccountPrice,
    PriceInOtherCurrency {
        currency: String,
    },
    PriceWithoutCurrency,
    NoReading,
    UnpricedInWeek {
        model: String,
    },
    /// Ollama's reading names no credits month, so there is no allowance to
    /// take a share of.
    NoMonthlyAllowance,
    /// No running weekly window of an OpenAI or Google subscription: there is
    /// no reading of this week's usage.
    NoWeekReading,
    /// No running month of an Ollama subscription: there is no reading of this
    /// month's usage.
    NoMonthReading,
    /// The subscription's work is in no token record
    /// ([`Company::work_recorded`]), so its worth is unknown.
    WorkNotRecorded,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Missing::NoCurrency => f.write_str("set a currency in Settings → General"),
            Missing::NoPrice { model } => write!(f, "no price for {model}"),
            Missing::NoAccount => f.write_str("no account was in use at that time"),
            Missing::NoAccountPrice => f.write_str("the account has no monthly price"),
            Missing::PriceInOtherCurrency { currency } => write!(
                f,
                "the account's price is in {currency}; enter it again in the current currency"
            ),
            Missing::PriceWithoutCurrency => f.write_str(
                "the account's price was entered before a currency was set; enter it again",
            ),
            Missing::NoReading => f.write_str("no reading of that week's usage"),
            Missing::UnpricedInWeek { model } => {
                write!(
                    f,
                    "that week also has tokens of {model}, which has no price"
                )
            }
            Missing::NoMonthlyAllowance => f.write_str("no monthly allowance in Ollama's reading"),
            Missing::NoWeekReading => f.write_str("no reading of this week's usage"),
            Missing::NoMonthReading => f.write_str("no reading of this month's usage"),
            Missing::WorkNotRecorded => f.write_str("seatswap does not record the work agy does"),
        }
    }
}

/// The two values of one token record, in the settings' currency.
#[derive(Debug, Clone, PartialEq)]
pub struct Valued {
    /// The tokens at API prices, times the rate of the record's day. Without a
    /// rate of the settings' currency this is the USD value (spec §1.4).
    pub api: Option<f64>,
    /// The share of the record's own subscription that its window used: the
    /// Claude account's week, the OpenAI subscription's `weekly_all` week, or
    /// Ollama's `monthly_credits` month (R4). A record without tokens is a
    /// known 0; without a subscription price it is `None` with
    /// [`Self::subscription_missing`], never 0.
    pub subscription: Option<f64>,
    /// The subscription the record belongs to: the Claude account that was
    /// live on its host at its time, or the account taken from the usage
    /// readings when [`Self::account_from_readings`] is true, or the provider
    /// subscription its work belongs to (decision P-2 2, 4). `None` only for a
    /// Claude record without a known account, or a provider record whose
    /// subscription cannot be named.
    pub account: Option<AccountRef>,
    /// Whether `account` was taken from the usage readings (R4) rather than
    /// from a live account on the record's host. A provider record's
    /// subscription always comes from its own logins, so it is false there.
    pub account_from_readings: bool,
    /// The company of the record's model ([`Company::of_model`]).
    pub company: Company,
    /// The currency `api` is in: the settings' currency with a rate, "USD"
    /// without one, and `None` when `api` is `None`.
    pub api_currency: Option<String>,
    /// Why `api` is `None`; `None` when it has a value.
    pub api_missing: Option<Missing>,
    /// Why `subscription` is `None`; `None` when it has a value.
    pub subscription_missing: Option<Missing>,
}

/// One subscription's window, with what its records are split by: a Claude
/// account's week, an OpenAI subscription's `weekly_all` week, or an Ollama
/// subscription's `monthly_credits` month (R4).
struct WeekWindow {
    account: AccountRef,
    /// The limit the window belongs to: `weekly_all` for a week,
    /// `monthly_credits` for Ollama's month.
    kind: LimitKind,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    /// The finished window's final percent, or a running window's last sample.
    used: Option<f64>,
    /// A running window's first percent when a reset began it: its use is
    /// `used − from`, the rule of `stats::window_used`. Absent means 0.
    from: Option<f64>,
    /// A running window's newest weekly reading, in percent: the `final` its
    /// [`Valuer::running_windows`] record carries. `None` for a finished window.
    percent: Option<f64>,
    /// Whether `used` came from a finished [`WindowRecord`].
    finished: bool,
    last_sample: Option<DateTime<Utc>>,
    /// The sum of [`api_usd`] of every record of this account in this window.
    api_sum: f64,
    /// The first record of this account in this window that has tokens but no
    /// price, in read order: no subscription value of the window can be split.
    unpriced: Option<String>,
}

/// One month of an Ollama subscription (decision P-2 9): the use of the
/// credits, its money and the last reading that named it. `money` is in the
/// display currency; `None` with `price_missing` when the subscription has no
/// price.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonthWindow {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    /// The share of the month's credits used, in percent: `final − from`.
    pub(crate) used: f64,
    pub(crate) last_sample: DateTime<Utc>,
    pub(crate) money: Option<WindowMoney>,
    pub(crate) price_missing: Option<Missing>,
}

/// Every account's price changes, in time order: the moment, the monthly price
/// and the currency it was entered in (`None` before a currency was set).
type PriceChanges = BTreeMap<AccountRef, Vec<(DateTime<Utc>, Option<f64>, Option<String>)>>;

/// One stored Anthropic account's usage reading, as R4 reads it: its time and
/// the `weekly_all` and `session` limits it holds, with percent and reset time.
/// A limit the reading does not hold is `None`, never a default.
#[derive(Debug, Clone, Copy)]
struct Reading {
    at: DateTime<Utc>,
    weekly: Option<(f64, Option<DateTime<Utc>>)>,
    session: Option<(f64, Option<DateTime<Utc>>)>,
}

/// The value of token records, built once for a range (spec §3.6): the account
/// of every record, its weekly window, and the window's price at its midpoint.
pub struct Valuer {
    timelines: HashMap<String, Timeline>,
    /// The `providerSwitch` timelines, per host: which OpenAI subscription a
    /// record on that host belongs to (decision P-2 2).
    provider_timelines: HashMap<String, Timeline>,
    /// The OpenAI subscriptions the history names: the accounts of its provider
    /// windows, samples and price changes, and the `from` and `to` logins of its
    /// `providerSwitch` events. A host without a `providerSwitch` uses the one
    /// of them when there is exactly one.
    openai_accounts: BTreeSet<AccountRef>,
    /// Each stored Anthropic account's readings, sorted by time: R4 books a
    /// Claude record without a live account from them. A provider
    /// subscription's readings are never kept here.
    readings: HashMap<AccountRef, Vec<Reading>>,
    windows: Vec<WeekWindow>,
    /// Each account's windows, by their place in `windows`, in the order they
    /// were added: a lookup reads only the account's own windows.
    account_windows: HashMap<AccountRef, Vec<usize>>,
    /// For each reset window, the reading it began at, by the account, the
    /// limit kind and that reading's time: the `from` of the running window it
    /// opened.
    window_starts: HashMap<AccountRef, BTreeMap<(String, DateTime<Utc>), f64>>,
    price_changes: PriceChanges,
    /// The price as `accounts.json` has it now and the currency it was entered
    /// in, for an account without changes.
    stored_prices: BTreeMap<AccountRef, Option<(f64, Option<String>)>>,
    prices: Prices,
    fx: Fx,
    settings: Settings,
}

/// The use of a running window: `percent − from`, through the one rule
/// [`window_used`] (a window that ends below its start has no use). The record
/// only carries the two values that rule reads.
fn open_window_used(
    account: &AccountRef,
    end: DateTime<Utc>,
    at: DateTime<Utc>,
    percent: f64,
    from: Option<f64>,
) -> Option<f64> {
    let record = WindowRecord {
        account: account.clone(),
        k: crate::api::LimitKind::WeeklyAll,
        m: None,
        start: Some(end - Duration::days(7)),
        end,
        final_percent: percent,
        lost: None,
        last_sample: at,
        plan: None,
        uncertain: false,
        reset: false,
        from,
    };
    window_used(&record)
}

/// Whether one limit's percent rose between two readings of one account: both
/// name the same reset time and the later percent is greater. A different or
/// missing reset time on either side means a reset may lie between, and a drop
/// is no rise.
fn limit_rose(
    before: Option<(f64, Option<DateTime<Utc>>)>,
    after: Option<(f64, Option<DateTime<Utc>>)>,
) -> bool {
    let (Some((before_percent, Some(before_reset))), Some((after_percent, Some(after_reset)))) =
        (before, after)
    else {
        return false;
    };
    same_reset(before_reset, after_reset) && after_percent > before_percent
}

/// Whether `readings` show the `weekly_all` rise that makes an account X of R4:
/// its last reading at or before `start` and its first reading at or after
/// `end` are both within `reach` of that end of the span, both hold a
/// `weekly_all` limit, and that limit rose between them.
fn reading_rise(
    readings: &[Reading],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    reach: Duration,
) -> bool {
    let before = readings.partition_point(|reading| reading.at <= start);
    let Some(earlier) = before.checked_sub(1).and_then(|index| readings.get(index)) else {
        return false;
    };
    if start - earlier.at > reach {
        return false;
    }
    let after = readings.partition_point(|reading| reading.at < end);
    let Some(later) = readings.get(after) else {
        return false;
    };
    if later.at - end > reach {
        return false;
    }
    limit_rose(earlier.weekly, later.weekly)
}

/// Whether a new `session` window began between two readings of one account:
/// the later reading holds a `session` percent above 0 whose reset time differs
/// from the earlier reading's beyond [`same_reset`]'s jitter, or whose earlier
/// reading held no session reset at all (no `session` limit or none with a
/// reset). A 5-hour session that starts during the span is use even when its
/// first percent is below the old session's last one. A later reading without a
/// session reset proves nothing, and a session with a reset equal on both sides
/// rises only through [`limit_rose`].
fn session_reset_rose(
    before: Option<(f64, Option<DateTime<Utc>>)>,
    after: Option<(f64, Option<DateTime<Utc>>)>,
) -> bool {
    let Some((after_percent, after_reset)) = after else {
        return false;
    };
    if after_percent <= 0.0 {
        return false;
    }
    let before_reset = before.and_then(|(_, reset)| reset);
    match (before_reset, after_reset) {
        (Some(before), Some(after)) => !same_reset(before, after),
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Whether `readings` show a rise of `session` or `weekly_all` around
/// `[start, end]`: of another stored Anthropic account Y of R4. Y's readings
/// between its last one at or before `start` (taken only when within `reach`)
/// and its first one at or after `end` (likewise; a side without one within
/// the reach is the span's own edge) are walked in pairs. A pair rose when a
/// limit's percent grew between two readings whose reset time is the same, or
/// (the `session` limit only, R4's other-account rule) when the later reading's
/// session reset differs from the earlier one's beyond [`same_reset`]'s jitter
/// or appears where the earlier had none, and the later session percent is
/// above 0: a new 5-hour session began, so Y was working. X's own rise is
/// [`reading_rise`]'s stricter `weekly_all` rule. An account with no readings
/// there did not rise.
fn rising_around(
    readings: &[Reading],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    reach: Duration,
) -> bool {
    let before = readings.partition_point(|reading| reading.at <= start);
    let from = match before.checked_sub(1).and_then(|index| readings.get(index)) {
        Some(last) if start - last.at <= reach => last.at,
        _ => start,
    };
    let after = readings.partition_point(|reading| reading.at < end);
    let to = match readings.get(after) {
        Some(first) if first.at - end <= reach => first.at,
        _ => end,
    };
    let first = readings.partition_point(|reading| reading.at < from);
    let last = readings.partition_point(|reading| reading.at <= to);
    readings[first..last].windows(2).any(|pair| {
        limit_rose(pair[0].weekly, pair[1].weekly)
            || limit_rose(pair[0].session, pair[1].session)
            || session_reset_rose(pair[0].session, pair[1].session)
    })
}

impl Valuer {
    /// Reads samples, windows and tokens of `range` widened to whole windows: a
    /// window that overlaps a point of the range begins at most
    /// [`LONGEST_WINDOW_DAYS`] before it and ends at most that many days after
    /// it, so both the seven-day weeks and Ollama's calendar month are read
    /// whole. Every event over all of history is read too, because a record's
    /// account is the last `Live`/`Switch` at or before it, and its window's
    /// price is the last `PriceChanged` at or before the window's midpoint,
    /// which can be older than the range; a price change after the range still
    /// names the first price, which counts for the past.
    ///
    /// `provider_subscriptions` is `accounts.json`'s list beside `accounts`
    /// (decision P-2 1): their prices are the stored prices where no
    /// `PriceChanged` names the subscription, and their OpenAI logins are the
    /// subscriptions the no-switch rule may pick.
    // The eight inputs are the valuer's whole context; a parameter struct no
    // caller shares would only hide them (decision P-2 1).
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        history: &HistoryDir,
        range: &Range,
        prices: &Prices,
        fx: &Fx,
        settings: &Settings,
        accounts: &[Account],
        provider_subscriptions: &[ProviderSubscription],
        warnings: &RefCell<Vec<String>>,
    ) -> Result<Valuer, MoneyError> {
        let widened = Range {
            from: range
                .from
                .checked_sub_signed(Duration::days(LONGEST_WINDOW_DAYS))
                .ok_or(MoneyError::RangeTooWide {
                    from: range.from,
                    to: range.to,
                })?,
            to: range
                .to
                .checked_add_signed(Duration::days(LONGEST_WINDOW_DAYS))
                .ok_or(MoneyError::RangeTooWide {
                    from: range.from,
                    to: range.to,
                })?,
        };
        // Codex's and Ollama's readings share the files. A Claude account's
        // week, an OpenAI subscription's `weekly_all` week and Ollama's
        // `monthly_credits` month are all read here, because a provider record's
        // subscription value is the same share of its own subscription's
        // window (R4).
        let windows = keep_warnings(warnings, || history.finished_windows(&widened, &Hosts::All))?;
        let samples = keep_warnings(warnings, || history.read::<Sample>(&widened, &Hosts::All))?;
        let events = keep_warnings(warnings, || history.read::<Event>(&Range::ALL, &Hosts::All))?;
        let tokens = keep_warnings(warnings, || {
            history.read::<TokenRecord>(&widened, &Hosts::All)
        })?;

        // The OpenAI subscriptions `accounts.json` stores: a host without a
        // `providerSwitch` uses the one of them when there is exactly one
        // (decision P-2 2). Neither a window nor a sample of the read range
        // decides this, so a record's subscription does not depend on the
        // page's range.
        let openai_accounts = openai_subscriptions(provider_subscriptions);
        let provider_timelines = provider_switch_timelines(&events.records);

        // A reset window says where the window that began at the reset started:
        // the first reading of the same limit at or after the reset. That
        // reading is the running window's `from`, so its use follows
        // `stats::window_used` (`final − from`) instead of counting the tokens
        // from before the reset. The reading's own reset time names the new
        // window, so a reset that also moved the reset time is found too. A
        // week and Ollama's month both use the rule.
        let mut window_starts: HashMap<AccountRef, BTreeMap<(String, DateTime<Utc>), f64>> =
            HashMap::new();
        for hosted in &windows.records {
            let record = &hosted.record;
            if !matches!(record.k, LimitKind::WeeklyAll | LimitKind::Other(_)) || !record.reset {
                continue;
            }
            let start_of = |sample: &Hosted<Sample>| {
                sample
                    .record
                    .l
                    .iter()
                    .find(|limit| limit.k == record.k && limit.r.is_some())
                    .map(|limit| (sample.record.t, limit.p))
            };
            // The samples are sorted by time: the first one at or after the
            // reset is the earliest.
            let after = samples
                .records
                .partition_point(|sample| sample.record.t < record.end);
            let first = samples.records[after..]
                .iter()
                .filter(|sample| sample.record.account == record.account)
                .find_map(start_of);
            if let Some((t, percent)) = first {
                window_starts
                    .entry(record.account.clone())
                    .or_default()
                    .insert((record.k.as_api().to_string(), t), percent);
            }
        }

        // R4's readings: every stored Anthropic account's samples, sorted by
        // time, with the two limits the rule reads. A provider subscription
        // never books work from readings, and a sample of an account that is no
        // longer stored cannot book any either.
        let stored_anthropic: BTreeSet<AccountRef> = accounts
            .iter()
            .map(AccountRef::of)
            .filter(AccountRef::is_claude)
            .collect();
        let mut readings: HashMap<AccountRef, Vec<Reading>> = HashMap::new();
        for hosted in &samples.records {
            let account = &hosted.record.account;
            if !stored_anthropic.contains(account) {
                continue;
            }
            let limit =
                |kind: &LimitKind| hosted.record.limit(kind).map(|limit| (limit.p, limit.r));
            readings.entry(account.clone()).or_default().push(Reading {
                at: hosted.record.t,
                weekly: limit(&LimitKind::WeeklyAll),
                session: limit(&LimitKind::Session),
            });
        }
        for lines in readings.values_mut() {
            lines.sort_by_key(|reading| reading.at);
        }

        let mut valuer = Valuer {
            timelines: timelines(&events.records),
            provider_timelines,
            openai_accounts,
            readings,
            windows: Vec::new(),
            account_windows: HashMap::new(),
            window_starts,
            price_changes: BTreeMap::new(),
            stored_prices: accounts
                .iter()
                .map(|account| {
                    (
                        AccountRef::of(account),
                        account
                            .price
                            .map(|price| (price, account.price_currency.clone())),
                    )
                })
                .chain(provider_subscriptions.iter().map(|subscription| {
                    (
                        subscription.account.clone(),
                        subscription
                            .price
                            .map(|price| (price, subscription.price_currency.clone())),
                    )
                }))
                .collect(),
            prices: prices.clone(),
            fx: fx.clone(),
            settings: settings.clone(),
        };
        for hosted in &windows.records {
            // Only a window of the page's own range is named (its midpoint in
            // `range`, as the weeks list places it), not one of the neighbouring
            // weeks the widened read takes in for valuing.
            if hosted.record.k == crate::api::LimitKind::WeeklyAll
                && window_used(&hosted.record).is_none()
                && window_midpoint(&hosted.record)
                    .is_some_and(|midpoint| range.from <= midpoint && midpoint < range.to)
            {
                // The window adds nothing to the week's use; the weeks list
                // names it with the same text, so the page shows it once.
                warn_window_below_start(warnings, &hosted.record);
            }
            valuer.finished_window(&hosted.record)?;
        }
        for hosted in &samples.records {
            valuer.sample_window(&hosted.record)?;
        }
        valuer.sample_windows_used();
        for hosted in &events.records {
            if let EventKind::PriceChanged {
                account,
                amount,
                currency,
            } = &hosted.record.kind
            {
                valuer
                    .price_changes
                    .entry(account.clone())
                    .or_default()
                    .push((hosted.record.t, *amount, currency.clone()));
            }
        }
        for hosted in &tokens.records {
            valuer.add_to_window(hosted, prices);
        }
        Ok(valuer)
    }

    /// The index of the account's window of `kind` that ends like `end`, adding
    /// it when it is new.
    fn window_index(
        &mut self,
        account: &AccountRef,
        kind: &LimitKind,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> usize {
        let same = self.account_windows.get(account).and_then(|own| {
            own.iter().copied().find(|index| {
                let window = &self.windows[*index];
                window.kind == *kind && same_reset(window.end, end)
            })
        });
        if let Some(index) = same {
            return index;
        }
        self.account_windows
            .entry(account.clone())
            .or_default()
            .push(self.windows.len());
        self.windows.push(WeekWindow {
            account: account.clone(),
            kind: kind.clone(),
            start,
            end,
            used: None,
            from: None,
            percent: None,
            finished: false,
            last_sample: None,
            api_sum: 0.0,
            unpriced: None,
        });
        self.windows.len() - 1
    }

    /// The account's first window of `kind`, in the order they were added, that
    /// holds `at` (`start <= at < end`).
    fn window_holding(
        &self,
        account: &AccountRef,
        kind: &LimitKind,
        at: DateTime<Utc>,
    ) -> Option<usize> {
        self.account_windows
            .get(account)?
            .iter()
            .copied()
            .find(|index| {
                let window = &self.windows[*index];
                window.kind == *kind && window.start <= at && at < window.end
            })
    }

    /// The start of a month: one calendar month before its reset (documented
    /// rule: Ollama's reply names the credits' period end, and the period is a
    /// calendar month). A month that cannot be computed that far back is an
    /// error, never a default of 30 days.
    fn month_start(end: DateTime<Utc>) -> Result<DateTime<Utc>, MoneyError> {
        end.checked_sub_months(Months::new(1))
            .ok_or(MoneyError::MonthStart { end })
    }

    fn finished_window(&mut self, record: &WindowRecord) -> Result<(), MoneyError> {
        if record.k != LimitKind::WeeklyAll && !is_month_kind(&record.k) {
            return Ok(());
        }
        let end = record.end;
        let start = match &record.k {
            LimitKind::WeeklyAll => record.weekly_start(),
            _ => Self::month_start(end)?,
        };
        let index = self.window_index(&record.account, &record.k, start, end);
        let window = &mut self.windows[index];
        // A window that ends below its start has no use (`stats::window_used`
        // says `None`): it adds nothing to the week's use, and `Valuer::build`
        // has already named it in the warnings.
        window.used = Some(window_used(record).unwrap_or(0.0));
        window.finished = true;
        window.last_sample = Some(record.last_sample);
        Ok(())
    }

    fn sample_window(&mut self, sample: &Sample) -> Result<(), MoneyError> {
        for limit in &sample.l {
            let month = is_month_kind(&limit.k);
            if limit.k != LimitKind::WeeklyAll && !month {
                continue;
            }
            let Some(end) = limit.r else {
                continue;
            };
            let start = if month {
                Self::month_start(end)?
            } else {
                end - Duration::days(7)
            };
            let index = self.window_index(&sample.account, &limit.k, start, end);
            let window = &mut self.windows[index];
            if window.finished {
                continue;
            }
            let newer = window.last_sample.is_none_or(|last| sample.t >= last);
            if newer {
                if let Some(from) = self
                    .window_starts
                    .get(&sample.account)
                    .and_then(|starts| starts.get(&(limit.k.as_api().to_string(), sample.t)))
                    .copied()
                {
                    window.from = Some(from);
                }
                window.last_sample = Some(sample.t);
                window.percent = Some(limit.p);
            }
        }
        Ok(())
    }

    /// The use of every window the readings opened and no finished record
    /// closed, from its newest reading: set once after all readings are in.
    fn sample_windows_used(&mut self) {
        for window in &mut self.windows {
            if window.finished {
                continue;
            }
            let (Some(at), Some(percent)) = (window.last_sample, window.percent) else {
                continue;
            };
            window.used = open_window_used(&window.account, window.end, at, percent, window.from);
        }
    }

    fn add_to_window(&mut self, hosted: &Hosted<TokenRecord>, prices: &Prices) {
        let record = &hosted.record;
        // A record without tokens adds nothing anywhere and is never "unpriced".
        if record.input == 0 && record.out == 0 && record.cw == 0 && record.cr == 0 {
            return;
        }
        let company = Company::of_model(&record.model);
        let (account, kind) = match company {
            Company::Anthropic => {
                // The same account the record's value uses (R4): work booked
                // from the readings belongs to X's window, so the window's API
                // sum and its records' values add up.
                let Some(account) = self.claude_account(&hosted.host, record).0 else {
                    return;
                };
                (account, LimitKind::WeeklyAll)
            }
            Company::OpenAI => match self.openai_subscription(&hosted.host, record.t) {
                Ok(account) => (account, LimitKind::WeeklyAll),
                Err(_) => return,
            },
            Company::Ollama => (ollama_subscription(), monthly_credits()),
            // No model id is Google's.
            Company::Google => return,
        };
        let Some(index) = self.window_holding(&account, &kind, record.t) else {
            return;
        };
        match api_usd(record, prices) {
            Some(usd) => self.windows[index].api_sum += usd,
            None => {
                let window = &mut self.windows[index];
                if window.unpriced.is_none() {
                    window.unpriced = Some(record.model.clone());
                }
            }
        }
    }

    /// The weekly windows still running at `now`, one per account and provider
    /// subscription at most, as the records a finished window would have: of
    /// every window the readings opened and no finished record closed, the
    /// account's one with the newest reading, when its reset (`end`) lies after
    /// `now`. A Claude account's week and an OpenAI or Google subscription's
    /// `weekly_all` week count alike; Ollama's month is not a week and stays out. A reading
    /// of another host whose clock runs a little ahead still counts. A window
    /// the readings opened before an early reset that moved the reset time has
    /// an older newest reading than the window after the reset, so it is never
    /// taken for the running one. The record's `start` is seven days
    /// before `end`, `final` the newest reading's percent, `from` the reading a
    /// reset began it at (the `from` its use is counted with), `lastSample` that
    /// reading's time; it has no `lost` (nothing expired yet) and is no reset.
    /// Sorted by end, then account.
    pub(crate) fn running_windows(&self, now: DateTime<Utc>) -> Vec<WindowRecord> {
        let mut newest: BTreeMap<&AccountRef, (&WeekWindow, DateTime<Utc>, f64)> = BTreeMap::new();
        for window in &self.windows {
            if window.finished || window.kind != LimitKind::WeeklyAll {
                continue;
            }
            if Company::of_account(&window.account) == Company::Ollama {
                continue;
            }
            let (Some(last_sample), Some(percent)) = (window.last_sample, window.percent) else {
                continue;
            };
            let newer = newest
                .get(&window.account)
                .is_none_or(|(_, kept, _)| last_sample > *kept);
            if newer {
                newest.insert(&window.account, (window, last_sample, percent));
            }
        }
        let mut running: Vec<WindowRecord> = newest
            .into_values()
            .filter(|(window, _, _)| window.end > now)
            .map(|(window, last_sample, percent)| WindowRecord {
                account: window.account.clone(),
                k: crate::api::LimitKind::WeeklyAll,
                m: None,
                start: Some(window.start),
                end: window.end,
                final_percent: percent,
                lost: None,
                last_sample,
                plan: None,
                uncertain: false,
                reset: false,
                from: window.from,
            })
            .collect();
        running.sort_by(|a, b| a.end.cmp(&b.end).then_with(|| a.account.cmp(&b.account)));
        running
    }

    /// The newest running month of `account` at `now`: Ollama's credits period,
    /// or `None` when no reading of the account names a month that still runs.
    pub(crate) fn running_month(
        &self,
        account: &AccountRef,
        now: DateTime<Utc>,
    ) -> Option<MonthWindow> {
        self.month_windows(account)
            .into_iter()
            .filter(|month| month.end > now)
            .max_by_key(|month| month.last_sample)
    }

    /// Every month of an Ollama subscription that the history holds, finished
    /// and running, oldest end first: the use of the credits, the money of that
    /// use at the month's price, and why the money is missing.
    pub(crate) fn month_windows(&self, account: &AccountRef) -> Vec<MonthWindow> {
        let mut months: Vec<MonthWindow> = self
            .account_windows
            .get(account)
            .into_iter()
            .flatten()
            .filter_map(|index| {
                let window = &self.windows[*index];
                if !is_month_kind(&window.kind) {
                    return None;
                }
                let used = window.used?;
                let (money, price_missing) =
                    match self.window_price(&window.account, window.start, window.end) {
                        Ok(price) => (Some(month_money(price, used)), None),
                        Err(missing) => (None, Some(missing)),
                    };
                Some(MonthWindow {
                    start: window.start,
                    end: window.end,
                    used,
                    last_sample: window.last_sample?,
                    money,
                    price_missing,
                })
            })
            .collect();
        months.sort_by(|a, b| a.end.cmp(&b.end).then_with(|| a.start.cmp(&b.start)));
        months
    }

    /// Which OpenAI subscription a record on `host` at `t` belongs to; the rule
    /// is [`openai_subscription_at`] over the stored subscriptions.
    fn openai_subscription(&self, host: &str, t: DateTime<Utc>) -> Result<AccountRef, Missing> {
        openai_subscription_at(&self.provider_timelines, &self.openai_accounts, host, t)
    }

    /// The Claude account a record of `host` is booked to (R4), and whether it
    /// came from the usage readings rather than from a live account.
    ///
    /// The documented rule: the live timeline names the account when it has one
    /// at the record's start. Otherwise the work is booked from the readings
    /// when exactly one stored Anthropic account X qualifies: X's last reading
    /// at or before the record's start and its first reading at or after the
    /// record's end are both at most [`READING_REACH_MINUTES`] away from that
    /// end of the span, both hold a `weekly_all` limit whose reset time is the
    /// same, and the later percent is greater. No other stored Anthropic account
    /// may have risen around the span: among its readings between its last one
    /// at or before the start and its first one at or after the end (each taken
    /// only when within the same reach; a side without one is the span's own
    /// edge), two consecutive readings with the same reset time for `session`
    /// or `weekly_all` must not show a greater percent, and a later `session`
    /// reading whose reset differs from the earlier one's or appears where it
    /// had none, with a percent above 0, is a new session and a rise
    /// ([`rising_around`]). An account with no reading within the reach did not
    /// rise. Without exactly one qualifying account, or when another account
    /// rose (two hosts busy on two accounts, one host not yet recorded), the
    /// record stays unknown.
    fn claude_account(&self, host: &str, record: &TokenRecord) -> (Option<AccountRef>, bool) {
        if let Some(account) = self
            .timelines
            .get(host)
            .and_then(|timeline| timeline.at(record.t))
        {
            return (Some(account.clone()), false);
        }
        match self.account_from_readings(record) {
            Some(account) => (Some(account), true),
            None => (None, false),
        }
    }

    /// [`Valuer::claude_account`]'s reading rule, over the readings
    /// [`Valuer::build`] kept per stored Anthropic account.
    fn account_from_readings(&self, record: &TokenRecord) -> Option<AccountRef> {
        let start = record.t;
        let end = start + record.span();
        let reach = Duration::minutes(READING_REACH_MINUTES);
        let mut qualifying: Option<&AccountRef> = None;
        for (account, readings) in &self.readings {
            if !reading_rise(readings, start, end, reach) {
                continue;
            }
            // Two accounts' readings rose: the work is not one account's.
            if qualifying.is_some() {
                return None;
            }
            qualifying = Some(account);
        }
        let account = qualifying?;
        for (other, readings) in &self.readings {
            if other != account && rising_around(readings, start, end, reach) {
                return None;
            }
        }
        Some(account.clone())
    }

    /// The dollar rate this valuer was built with: the API column's rate at a
    /// record's day, and at the view's day for a list without records.
    pub fn fx(&self) -> &Fx {
        &self.fx
    }

    /// The money value of one token record of `host`. A record of another
    /// provider than Claude belongs to its provider and to no Claude account,
    /// whichever account was live on its host; it takes its share of that
    /// provider subscription's own window (R4), and [`Valued::account`] is the
    /// subscription's [`AccountRef`] (decision P-2 4).
    pub fn value(&self, host: &str, record: &TokenRecord) -> Valued {
        let company = Company::of_model(&record.model);
        let mut account_missing = None;
        let (account, account_from_readings) = match company {
            Company::Anthropic => self.claude_account(host, record),
            Company::OpenAI => match self.openai_subscription(host, record.t) {
                Ok(account) => (Some(account), false),
                Err(missing) => {
                    account_missing = Some(missing);
                    (None, false)
                }
            },
            Company::Ollama => (Some(ollama_subscription()), false),
            // No model id is Google's.
            Company::Google => {
                account_missing = Some(Missing::WorkNotRecorded);
                (None, false)
            }
        };
        let usd = api_usd(record, &self.prices);
        let (api, api_currency, api_missing) = match usd {
            None => (
                None,
                None,
                Some(Missing::NoPrice {
                    model: record.model.clone(),
                }),
            ),
            Some(usd) => {
                match api_in_currency(usd, &self.settings, &self.fx, record.t.date_naive()) {
                    Ok((amount, currency)) => (Some(amount), Some(currency), None),
                    Err(missing) => (None, None, Some(missing)),
                }
            }
        };
        // A record without tokens uses no share of any subscription, whether or
        // not an account was live: both halves are a known 0 (`api_usd` gives
        // its 0 US dollars), never NoAccount. A provider record takes its share
        // of its own subscription's window, and without a price that share is
        // missing with its reason while the API value stays (Review Focus 5).
        let no_tokens = record.input == 0 && record.out == 0 && record.cw == 0 && record.cr == 0;
        let (subscription, subscription_missing) = if no_tokens {
            (Some(0.0), None)
        } else {
            match (usd, &account) {
                (None, _) => (
                    None,
                    Some(Missing::NoPrice {
                        model: record.model.clone(),
                    }),
                ),
                (Some(_), None) => (None, Some(account_missing.unwrap_or(Missing::NoAccount))),
                (Some(usd), Some(account)) => {
                    let kind = match company {
                        Company::Anthropic | Company::OpenAI | Company::Google => {
                            LimitKind::WeeklyAll
                        }
                        Company::Ollama => monthly_credits(),
                    };
                    match self.subscription(account, &kind, record.t, usd) {
                        Ok(value) => (Some(value), None),
                        Err(missing) => (None, Some(missing)),
                    }
                }
            }
        };
        Valued {
            api,
            subscription,
            account,
            account_from_readings,
            company,
            api_currency,
            api_missing,
            subscription_missing,
        }
    }

    /// Why a subscription has no reading for `account`, in its own words
    /// (decision P-2 7): Ollama without a month window at all has no monthly
    /// allowance to take a share of, while an account with month windows but
    /// none holding the record has no reading of this month's usage. The one
    /// rule the value and the pages use.
    pub(crate) fn no_reading(&self, account: &AccountRef) -> Missing {
        match Company::of_account(account) {
            Company::Anthropic => Missing::NoReading,
            Company::OpenAI | Company::Google => Missing::NoWeekReading,
            Company::Ollama if self.month_windows(account).is_empty() => {
                Missing::NoMonthlyAllowance
            }
            Company::Ollama => Missing::NoMonthReading,
        }
    }

    pub(crate) fn subscription(
        &self,
        account: &AccountRef,
        kind: &LimitKind,
        at: DateTime<Utc>,
        usd: f64,
    ) -> Result<f64, Missing> {
        // A record's missing window is worded for its subscription: Claude
        // keeps "that week", OpenAI says "this week" and Ollama "this month"
        // (decision P-2 7).
        let no_reading = self.no_reading(account);
        let window = self
            .window_holding(account, kind, at)
            .map(|index| &self.windows[index])
            .ok_or(no_reading.clone())?;
        let used = window.used.ok_or(no_reading)?;
        let price = self.window_price(&window.account, window.start, window.end)?;
        if let Some(model) = &window.unpriced {
            return Err(Missing::UnpricedInWeek {
                model: model.clone(),
            });
        }
        // The week has no API value to split (all its prices are 0, or it holds
        // only records without tokens): this record's share is 0.
        if window.api_sum <= 0.0 {
            return Ok(0.0);
        }
        // One percent of a week is a week's share of the monthly price; one
        // percent of Ollama's month is a hundredth of it (decision P-2 3).
        let used_money = match kind {
            LimitKind::WeeklyAll => window_money(price, used).used,
            _ => month_money(price, used).used,
        };
        Ok(usd / window.api_sum * used_money)
    }

    /// The price of the window from `start` to `end`: the account's price at
    /// the window's midpoint `start + (end − start) / 2`, through
    /// [`Valuer::price_at`]. A window can be shorter than a week (one that a
    /// reset ended or began), so its length is never assumed; a
    /// [`WindowRecord`]'s start is [`WindowRecord::weekly_start`].
    pub(crate) fn window_price(
        &self,
        account: &AccountRef,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<f64, Missing> {
        self.price_at(account, start + (end - start) / 2)
    }

    /// The account's monthly price at `at`: the last [`EventKind::PriceChanged`]
    /// at or before it, before the first one the first one; without any change
    /// the price stored for the account now, in the currency `accounts.json`
    /// records for it. A price is only counted in the currency it was entered in
    /// (spec §1.4): a change in another currency is
    /// [`Missing::PriceInOtherCurrency`], one without a currency
    /// [`Missing::PriceWithoutCurrency`]. Only an account without any stored
    /// price is [`Missing::NoAccountPrice`].
    pub fn price_at(&self, account: &AccountRef, at: DateTime<Utc>) -> Result<f64, Missing> {
        let (amount, currency) = match self.price_changes.get(account) {
            Some(changes) if !changes.is_empty() => {
                let after = changes.partition_point(|(t, _, _)| *t <= at);
                let (_, amount, currency) = if after == 0 {
                    changes.first()
                } else {
                    changes.get(after - 1)
                }
                .ok_or(Missing::NoAccountPrice)?;
                (*amount, currency.as_deref())
            }
            _ => {
                return match self.stored_prices.get(account).and_then(Option::as_ref) {
                    Some((amount, currency)) => counted_price(
                        *amount,
                        currency.as_deref(),
                        self.settings.currency.as_deref(),
                    ),
                    None => Err(Missing::NoAccountPrice),
                };
            }
        };
        let amount = amount.ok_or(Missing::NoAccountPrice)?;
        counted_price(amount, currency, self.settings.currency.as_deref())
    }
}

/// A price counts only in the currency it was entered in (spec §1.4). Without
/// a currency in the settings there is none to count it in.
fn counted_price(
    amount: f64,
    currency: Option<&str>,
    current: Option<&str>,
) -> Result<f64, Missing> {
    if current.is_none() {
        return Err(Missing::NoCurrency);
    }
    match currency {
        Some(currency) if Some(currency) != current => Err(Missing::PriceInOtherCurrency {
            currency: currency.to_string(),
        }),
        Some(_) => Ok(amount),
        None => Err(Missing::PriceWithoutCurrency),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use serde_json::json;
    use tempfile::tempdir;

    use crate::api::LimitKind;
    use crate::history::{LimitSample, Source, WindowRecord};
    use crate::project::ProjectKey;
    use crate::stats::tests::{append_codex_records, tiny};

    const AC_ACC: &str = "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04";
    const AC_ORG: &str = "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20";

    const MODELS_DEV: &str = r#"{
      "anthropic": {"id": "anthropic", "models": {
        "claude-opus-5-5": {"id": "claude-opus-5-5", "cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}},
        "claude-haiku-5-5": {"id": "claude-haiku-5-5", "cost": {"input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125,
          "tiers": [{"input": 0.5, "output": 2.5, "cache_read": 0.05, "cache_write": 0.625, "tier": {"type": "context", "size": 100000}}]}},
        "claude-no-price": {"id": "claude-no-price"}
      }},
      "ollama-cloud": {"id": "ollama-cloud", "models": {"deepseek-v4.1-flash": {"id": "deepseek-v4.1-flash", "cost": {"input": 0.3, "output": 1.2}}}},
      "somebody-else": {"id": "somebody-else", "models": {"x": {"id": "x", "cost": {"input": 1, "output": 1}}}}
    }"#;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("test date")
    }

    fn date(text: &str) -> NaiveDate {
        text.parse().expect("test date")
    }

    fn ac() -> AccountRef {
        AccountRef {
            acc: AC_ACC.to_string(),
            org: AC_ORG.to_string(),
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

    fn models_dev() -> serde_json::Value {
        serde_json::from_str(MODELS_DEV).expect("the models.dev fixture")
    }

    fn model_list() -> BTreeMap<String, ModelPrice> {
        prices_from_models_dev(&models_dev())
            .expect("the fixture's prices")
            .prices
    }

    /// The models.dev fixture's prices from `at` on (`claude-opus-5-5` at
    /// 4 / 20 / 5 / 0.2 USD per million input, output, cache-write and
    /// cache-read tokens). `pub(crate)` so auto-burn's tests price their token
    /// records with the same list.
    pub(crate) fn priced_prices(at: DateTime<Utc>) -> Prices {
        let mut prices = Prices::default();
        prices.take(model_list(), at);
        prices
    }

    fn settings_eur() -> Settings {
        Settings {
            currency: Some("EUR".to_string()),
            ..Settings::default()
        }
    }

    fn empty_fx() -> Fx {
        Fx::default()
    }

    /// A looked-up EUR rate of 1 so API values and subscription money agree.
    fn fx_eur() -> Fx {
        let mut fx = Fx {
            version: 1,
            currency: "EUR".to_string(),
            points: Vec::new(),
        };
        fx.take(date("2026-01-01"), 1.0);
        fx
    }

    fn account_with_price(price: Option<f64>) -> Account {
        stored_account(&ac(), "AC", price)
    }

    /// A stored account of `account`'s identity, as `accounts.json` holds it.
    fn stored_account(account: &AccountRef, abbr: &str, price: Option<f64>) -> Account {
        serde_json::from_value(json!({
            "id": format!("{}{}", &account.acc[..8], &account.org[..8]),
            "accountUuid": account.acc,
            "organizationUuid": account.org,
            "label": abbr,
            "abbr": abbr,
            "color": "#3b82f6",
            "email": "sam@example.com",
            "orgName": "Acme",
            "plan": "Team Premium",
            "price": price,
            "oauthAccount": {
                "accountUuid": account.acc,
                "organizationUuid": account.org,
                "emailAddress": "sam@example.com"
            },
            "addedAt": "2026-09-01T00:00:00Z",
            "loginState": "ok",
            "autoBurn": true
        }))
        .expect("a test account")
    }

    fn token(t: DateTime<Utc>, model: &str, input: u64, out: u64, cw: u64, cr: u64) -> TokenRecord {
        TokenRecord {
            t,
            minutes: 60,
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Repo("github.com/acme/atlas".to_string()),
            model: model.to_string(),
            input,
            out,
            cw,
            cr,
            answers: 3,
            agent_answers: 1,
        }
    }

    fn live(at: DateTime<Utc>, account: Option<AccountRef>) -> Event {
        Event {
            t: at,
            kind: EventKind::Live { account },
        }
    }

    fn price_changed(at: DateTime<Utc>, amount: Option<f64>, currency: Option<&str>) -> Event {
        Event {
            t: at,
            kind: EventKind::PriceChanged {
                account: ac(),
                amount,
                currency: currency.map(str::to_string),
            },
        }
    }

    fn weekly_sample(t: DateTime<Utc>, end: DateTime<Utc>, percent: f64) -> Sample {
        Sample {
            t,
            account: ac(),
            src: Source::Poll,
            l: vec![crate::history::LimitSample {
                k: LimitKind::WeeklyAll,
                m: None,
                p: percent,
                r: Some(end),
                b: false,
            }],
        }
    }

    /// One stored account's reading: its `weekly_all` percent and reset, and a
    /// `session` percent and reset when `session` is given.
    fn reading(
        at: DateTime<Utc>,
        account: &AccountRef,
        weekly: (f64, DateTime<Utc>),
        session: Option<(f64, DateTime<Utc>)>,
    ) -> Sample {
        let mut l = vec![LimitSample {
            k: LimitKind::WeeklyAll,
            m: None,
            p: weekly.0,
            r: Some(weekly.1),
            b: false,
        }];
        if let Some((percent, reset)) = session {
            l.push(LimitSample {
                k: LimitKind::Session,
                m: None,
                p: percent,
                r: Some(reset),
                b: false,
            });
        }
        Sample {
            t: at,
            account: account.clone(),
            src: Source::Poll,
            l,
        }
    }

    /// The one Codex subscription the P-2 tests read under, as the provider
    /// creates it.
    fn codex() -> AccountRef {
        AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        }
    }

    /// A stored provider subscription with `abbr` and an optional monthly price,
    /// as `accounts.json` holds it (decision P-2 1).
    fn subscription(account: &AccountRef, abbr: &str, price: Option<f64>) -> ProviderSubscription {
        ProviderSubscription {
            account: account.clone(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#4a6f8c".to_string(),
            plan: Some("Plus".to_string()),
            price,
            price_currency: price.map(|_| "EUR".to_string()),
        }
    }

    fn provider_sample(
        at: DateTime<Utc>,
        account: &AccountRef,
        end: DateTime<Utc>,
        percent: f64,
    ) -> Sample {
        Sample {
            t: at,
            account: account.clone(),
            src: Source::Poll,
            l: vec![crate::history::LimitSample {
                k: LimitKind::WeeklyAll,
                m: None,
                p: percent,
                r: Some(end),
                b: false,
            }],
        }
    }

    fn provider_price_changed(
        at: DateTime<Utc>,
        account: &AccountRef,
        amount: f64,
        currency: &str,
    ) -> Event {
        Event {
            t: at,
            kind: EventKind::PriceChanged {
                account: account.clone(),
                amount: Some(amount),
                currency: Some(currency.to_string()),
            },
        }
    }

    fn provider_token(t: DateTime<Utc>, model: &str, input: u64) -> TokenRecord {
        TokenRecord {
            t,
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

    fn weekly_window(start: DateTime<Utc>, end: DateTime<Utc>, final_percent: f64) -> WindowRecord {
        WindowRecord {
            account: ac(),
            k: LimitKind::WeeklyAll,
            m: None,
            start: Some(start),
            end,
            final_percent,
            lost: None,
            last_sample: end - Duration::hours(1),
            plan: None,
            uncertain: false,
            reset: false,
            from: None,
        }
    }

    fn range(from: &str, to: &str) -> Range {
        Range {
            from: utc(from),
            to: utc(to),
        }
    }

    #[test]
    fn models_dev_gives_three_providers_only() {
        let prices = model_list();
        let keys: Vec<&String> = prices.keys().collect();
        assert_eq!(
            keys,
            vec![
                "anthropic/claude-haiku-5-5",
                "anthropic/claude-opus-5-5",
                "ollama-cloud/deepseek-v4.1-flash",
            ]
        );

        let opus = &prices["anthropic/claude-opus-5-5"];
        assert_eq!(
            (
                opus.input,
                opus.output,
                opus.cache_write,
                opus.cache_read,
                opus.tiered
            ),
            (4.0, 20.0, 5.0, 0.2, false)
        );
        assert!(prices["anthropic/claude-haiku-5-5"].tiered);
        let ollama = &prices["ollama-cloud/deepseek-v4.1-flash"];
        assert_eq!((ollama.input, ollama.output), (0.3, 1.2));
        assert_eq!(ollama.cache_read, 0.0);
        assert_eq!(ollama.cache_write, 0.0);
    }

    #[test]
    fn a_model_id_has_one_price_key() {
        for (model, key) in [
            ("claude-opus-5-5", "anthropic/claude-opus-5-5"),
            ("claude-haiku-5-5", "anthropic/claude-haiku-5-5"),
            (
                "deepseek-v4.1-flash:cloud",
                "ollama-cloud/deepseek-v4.1-flash",
            ),
            (
                "deepseek-v4.1-flash:1017-cloud",
                "ollama-cloud/deepseek-v4.1-flash:1017",
            ),
            (
                "minimax-m3.1-flash:cloud",
                "ollama-cloud/minimax-m3.1-flash",
            ),
            ("gpt-6.1-sol", "openai/gpt-6.1-sol"),
            // Neither Ollama form: an id that only ends in `-cloud` or has an
            // empty name or tag is another provider's id, priced as `openai`.
            ("gpt-6.1-cloud", "openai/gpt-6.1-cloud"),
            (":cloud", "openai/:cloud"),
            (
                "deepseek-v4.1-flash:-cloud",
                "openai/deepseek-v4.1-flash:-cloud",
            ),
        ] {
            assert_eq!(model_price_key(model), key, "{model}");
        }
    }

    /// Every model is priced under its own provider's list (fault A): a
    /// Harness, flash, Ollama or Codex record gets its API value, and a key
    /// without a price stays missing with the model named, never a price of
    /// another provider.
    #[test]
    fn every_provider_is_priced_by_its_own_key() {
        let at = utc("2026-11-14T09:00:00Z");
        let price = |output: f64| ModelPrice {
            input: 1.0,
            output,
            cache_write: 0.0,
            cache_read: 0.0,
            tiered: false,
        };
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([
                ("anthropic/claude-opus-5-5".to_string(), price(20.0)),
                ("ollama-cloud/deepseek-v4.1-flash".to_string(), price(2.0)),
                (
                    "ollama-cloud/deepseek-v4.1-flash:1017".to_string(),
                    price(3.0),
                ),
                ("ollama-cloud/minimax-m3.1-flash".to_string(), price(4.0)),
                ("openai/gpt-6.1-sol".to_string(), price(10.0)),
                // A price stored under the wrong provider is never used.
                ("anthropic/glm-5".to_string(), price(99.0)),
            ]),
            utc("2026-01-01T00:00:00Z"),
        );
        for (model, usd) in [
            ("claude-opus-5-5", 20.0),
            ("deepseek-v4.1-flash:cloud", 2.0),
            ("deepseek-v4.1-flash:1017-cloud", 3.0),
            ("minimax-m3.1-flash:cloud", 4.0),
            ("gpt-6.1-sol", 10.0),
        ] {
            assert_eq!(
                api_usd(&token(at, model, 0, 1_000_000, 0, 0), &prices),
                Some(usd),
                "{model}"
            );
        }
        assert_eq!(
            api_usd(&token(at, "glm-5:cloud", 0, 1_000_000, 0, 0), &prices),
            None,
            "ollama-cloud/glm-5 has no price; anthropic/glm-5 is not tried"
        );

        let settings = settings_eur();
        let history = HistoryDir::at(tempdir().expect("tempdir").path(), "atlas");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-15T00:00:00Z"),
            &prices,
            &fx_eur(),
            &settings,
            &[],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");
        let flash = valuer.value(
            "atlas",
            &token(at, "deepseek-v4.1-flash:cloud", 0, 1_000_000, 0, 0),
        );
        assert_eq!(flash.api, Some(2.0));
        assert_eq!(
            flash.subscription, None,
            "no month window of Ollama's credits: never a 0"
        );
        assert_eq!(
            flash.subscription_missing,
            Some(Missing::NoMonthlyAllowance),
            "no month window at all: the reading names no monthly allowance"
        );
        let unpriced = valuer.value("atlas", &token(at, "glm-5:cloud", 0, 1_000_000, 0, 0));
        assert_eq!(unpriced.api, None);
        assert_eq!(
            unpriced.api_missing,
            Some(Missing::NoPrice {
                model: "glm-5:cloud".to_string()
            }),
            "the reason names the model"
        );
    }

    #[test]
    fn a_body_of_another_shape_is_an_error() {
        assert!(matches!(
            prices_from_models_dev(&json!([])),
            Err(MoneyError::ModelsDev(_))
        ));
    }

    #[test]
    fn take_adds_points_only_for_changes() {
        let t1 = utc("2026-11-01T00:00:00Z");
        let t2 = utc("2026-11-02T00:00:00Z");
        let t3 = utc("2026-11-03T00:00:00Z");
        let mut prices = Prices::default();
        let list = model_list();

        let first = prices.take(list.clone(), t1).expect("a new list changes");
        assert_eq!(first.at, t1);
        assert_eq!(first.added.len(), 3);
        assert!(first.changed.is_empty());
        assert!(first.removed.is_empty());
        assert_eq!(prices.models.len(), 3);
        assert_eq!(prices.models["anthropic/claude-opus-5-5"].len(), 1);

        assert_eq!(prices.take(list.clone(), t2), None);
        for points in prices.models.values() {
            assert_eq!(points.len(), 1, "an unchanged list adds no point");
        }

        let mut repriced = list;
        repriced
            .get_mut("anthropic/claude-opus-5-5")
            .expect("opus")
            .output = 18.0;
        let second = prices.take(repriced, t3).expect("a new price changes");
        assert_eq!(second.changed, vec!["anthropic/claude-opus-5-5"]);
        assert!(second.added.is_empty());
        assert_eq!(prices.models["anthropic/claude-opus-5-5"].len(), 2);
    }

    #[test]
    fn a_model_gone_from_the_list_keeps_its_price() {
        let t1 = utc("2026-11-01T00:00:00Z");
        let t2 = utc("2026-11-02T00:00:00Z");
        let mut prices = Prices::default();
        let mut list = model_list();
        prices.take(list.clone(), t1);
        list.remove("anthropic/claude-haiku-5-5");

        let change = prices.take(list, t2).expect("a removed model is a change");
        assert_eq!(change.removed, vec!["anthropic/claude-haiku-5-5"]);
        assert_eq!(
            prices.models["anthropic/claude-haiku-5-5"].len(),
            1,
            "a model missing from the list keeps its point"
        );
    }

    #[test]
    fn at_gives_the_price_of_that_day() {
        let t1 = utc("2026-11-01T00:00:00Z");
        let t2 = utc("2026-11-02T00:00:00Z");
        let t3 = utc("2026-11-03T00:00:00Z");
        let mut prices = Prices::default();
        let mut list = model_list();
        prices.take(list.clone(), t1);

        let key = "anthropic/claude-opus-5-5";
        assert_eq!(prices.at(key, t2).expect("opus").output, 20.0);
        assert_eq!(
            prices.at(key, t1).expect("opus").output,
            20.0,
            "at the first point"
        );

        list.get_mut(key).expect("opus").output = 18.0;
        prices.take(list, t3);
        assert_eq!(prices.at(key, t2).expect("opus").output, 20.0);
        assert_eq!(
            prices
                .at(key, t3 + Duration::seconds(1))
                .expect("opus")
                .output,
            18.0
        );
        assert_eq!(
            prices.at(key, t1 - Duration::days(1)).expect("opus").output,
            20.0,
            "before the first point, the first point"
        );
    }

    #[test]
    fn a_point_taken_with_an_earlier_time_keeps_the_order() {
        let later = utc("2026-11-03T00:00:00Z");
        let earlier = utc("2026-11-02T00:00:00Z");
        let key = "anthropic/claude-opus-5-5";
        let mut prices = Prices::default();
        let mut list = model_list();
        prices.take(list.clone(), later);

        list.get_mut(key).expect("opus").input = 10.0;
        prices.take(list, earlier);

        let points = &prices.models[key];
        assert_eq!(
            points.iter().map(|point| point.from).collect::<Vec<_>>(),
            vec![earlier, later],
            "a point taken with an earlier time is inserted in time order"
        );
        assert_eq!(prices.at(key, earlier).expect("opus").input, 10.0);
        assert_eq!(
            prices.at(key, later).expect("opus").input,
            4.0,
            "the later point still wins at its time"
        );
    }

    #[test]
    fn current_lists_every_looked_up_price() {
        let at = utc("2026-11-02T00:00:00Z");
        let prices = priced_prices(at);

        let current = prices.current();
        let keys: Vec<&str> = current.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "anthropic/claude-haiku-5-5",
                "anthropic/claude-opus-5-5",
                "ollama-cloud/deepseek-v4.1-flash"
            ],
            "sorted by key"
        );
        let opus = &current[1];
        assert_eq!(opus.price.input, 4.0);
        assert_eq!(opus.since, Some(at));
    }

    #[test]
    fn api_usd_of_a_record() {
        let prices = priced_prices(utc("2026-11-01T00:00:00Z"));
        let record = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            2_000_000,
            1_000_000,
            10_000_000,
        );
        // 4 + 40 + 5 + 2 USD.
        assert_eq!(api_usd(&record, &prices), Some(51.0));
    }

    #[test]
    fn no_price_no_value() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let prices = priced_prices(utc("2026-11-01T00:00:00Z"));
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let record = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-unknown",
            1_000_000,
            0,
            0,
            0,
        );
        assert_eq!(api_usd(&record, &prices), None);
        assert_eq!(
            valuer.value("atlas", &record).api_missing,
            Some(Missing::NoPrice {
                model: "claude-unknown".to_string()
            })
        );
    }

    #[test]
    fn the_rate_is_looked_up_then_missing() {
        let mut settings = Settings {
            currency: Some("EUR".to_string()),
            look_up_prices: true,
            ..Settings::default()
        };
        let mut fx = Fx {
            version: 1,
            currency: "EUR".to_string(),
            points: Vec::new(),
        };
        fx.take(date("2026-11-13"), 0.90481);
        assert_eq!(
            usd_rate(&settings, &fx, date("2026-11-14")),
            Some(Rate {
                value: 0.90481,
                date: Some(date("2026-11-13")),
            })
        );

        assert_eq!(
            usd_rate(&settings, &empty_fx(), date("2026-11-14")),
            None,
            "no rate is no rate"
        );

        settings.look_up_prices = false;
        assert_eq!(
            usd_rate(&settings, &fx, date("2026-11-14")),
            None,
            "with the look-ups off, no stored rate is applied: the API value is in USD"
        );
        settings.look_up_prices = true;

        settings.currency = Some("USD".to_string());
        assert_eq!(
            usd_rate(&settings, &empty_fx(), date("2026-11-14")),
            Some(Rate {
                value: 1.0,
                date: None,
            })
        );
    }

    #[test]
    fn provider_cost_beats_models_dev() {
        assert_eq!(api_value(Some(0.42), Some(0.9)), Some(0.42));
        assert_eq!(
            api_value(Some(0.0), Some(0.9)),
            Some(0.0),
            "a provider's own 0 is a number it gave, not a missing price"
        );
    }

    #[test]
    fn no_price_is_no_value() {
        assert_eq!(api_value(None, Some(0.9)), Some(0.9));
        assert_eq!(api_value(None, None), None, "never a guessed 0");
    }

    /// [`rate_due`] with a time it can place; a test of the error branch calls
    /// `rate_due` itself.
    fn due(now: DateTime<Utc>, stored: Option<NaiveDate>, last_ask: Option<DateTime<Utc>>) -> bool {
        rate_due(now, stored, last_ask).expect("a Frankfurt time")
    }

    #[test]
    fn rate_is_asked_again_after_four_until_todays_date_arrives() {
        // Fri 13 Nov 2026, 15:00 UTC = 16:00 in Frankfurt.
        let four = utc("2026-11-13T15:00:00Z");
        let stored = Some(date("2026-11-12"));
        assert!(due(four, stored, None), "the first round asks");
        assert!(
            due(four + Duration::minutes(30), stored, Some(four)),
            "half an hour after the last ask it asks again"
        );
        assert!(
            !due(four + Duration::minutes(29), stored, Some(four)),
            "not after 29 minutes"
        );
        assert!(
            !due(
                four,
                Some(date("2026-11-13")),
                Some(four - Duration::hours(2))
            ),
            "today's date arrived: no more asks that day"
        );
        assert!(
            due(utc("2026-11-16T08:00:00Z"), stored, Some(four)),
            "the next Frankfurt day asks once, outside the afternoon too"
        );
    }

    /// The afternoon retry runs every 30 minutes after the last answered ask,
    /// not once an hour: a Monday with a stored Friday point is asked at 16:30
    /// and at 17:00 and not at 18:00.
    #[test]
    fn the_afternoon_retry_asks_at_half_past_and_at_five() {
        // Mon 16 Nov 2026; 15:00, 15:30, 16:00 and 17:00 UTC are 16:00, 16:30,
        // 17:00 and 18:00 in Frankfurt (winter time).
        let stored = Some(date("2026-11-13"));
        let answered_at_four = utc("2026-11-16T15:00:00Z");
        assert!(
            !due(answered_at_four, stored, Some(answered_at_four)),
            "16:00 has just been answered with Friday's date: not due again at once"
        );
        assert!(
            due(utc("2026-11-16T15:30:00Z"), stored, Some(answered_at_four)),
            "16:30 is due half an hour after the 16:00 ask"
        );
        assert!(
            due(
                utc("2026-11-16T16:00:00Z"),
                stored,
                Some(utc("2026-11-16T15:30:00Z"))
            ),
            "17:00 is due half an hour after the 16:30 ask"
        );
        assert!(
            !due(
                utc("2026-11-16T17:00:00Z"),
                stored,
                Some(utc("2026-11-16T16:00:00Z"))
            ),
            "18:00 is past the retry's window"
        );
    }

    #[test]
    fn rate_stops_at_six() {
        // 18:00 in Frankfurt is 17:00 UTC in winter.
        let six = utc("2026-11-13T17:00:00Z");
        assert!(
            !due(
                six,
                Some(date("2026-11-12")),
                Some(six - Duration::minutes(31))
            ),
            "the 16:00–18:00 window has ended"
        );
        assert!(
            due(
                six - Duration::minutes(1),
                Some(date("2026-11-12")),
                Some(six - Duration::minutes(31))
            ),
            "17:59 is still inside"
        );
    }

    #[test]
    fn no_second_ask_on_a_weekend() {
        // Sat 14 Nov 2026, 16:30 in Frankfurt (15:30 UTC).
        let afternoon = utc("2026-11-14T15:30:00Z");
        assert!(
            !due(
                afternoon,
                Some(date("2026-11-13")),
                Some(afternoon - Duration::hours(1)),
            ),
            "the afternoon retry is for working days only"
        );
        assert!(
            due(
                afternoon,
                Some(date("2026-11-13")),
                Some(utc("2026-11-13T15:30:00Z"))
            ),
            "the weekend still asks once per Frankfurt day"
        );
    }

    #[test]
    fn frankfurt_time_follows_summer_time() {
        let at = |text: &str| {
            frankfurt_time(utc(text))
                .expect("a Frankfurt time")
                .format("%Y-%m-%d %H:%M")
                .to_string()
        };
        assert_eq!(at("2026-03-29T00:59:00Z"), "2026-03-29 01:59");
        assert_eq!(at("2026-03-29T01:00:00Z"), "2026-03-29 03:00");
        assert_eq!(at("2026-03-28T12:00:00Z"), "2026-03-28 13:00");
        assert_eq!(at("2026-10-25T00:59:00Z"), "2026-10-25 02:59");
        assert_eq!(at("2026-10-25T01:00:00Z"), "2026-10-25 02:00");
        assert_eq!(at("2026-10-26T12:00:00Z"), "2026-10-26 13:00");
    }

    /// The last instant chrono knows lies past Frankfurt's last buildable hour:
    /// `rate_due` says so with a text instead of guessing the hour.
    #[test]
    fn a_time_chrono_cannot_place_in_frankfurt_is_an_error() {
        let error = rate_due(DateTime::<Utc>::MAX_UTC, None, None)
            .expect_err("a time past the range this build knows");
        assert!(
            error.contains("Frankfurt") && error.contains("cannot be built"),
            "the error says what could not be built: {error}"
        );
    }

    #[test]
    fn percent_value_of_180_a_month() {
        let value = percent_value(180.0);
        assert!((value - 180.0 * 12.0 / 52.0 / 100.0).abs() < 1e-9);
    }

    /// Two records of one finished week: 30 and 10 USD of tokens, so the 80 %
    /// that week used are split 75 % / 25 %.
    fn two_records() -> (
        HistoryDir,
        tempfile::TempDir,
        TokenRecord,
        TokenRecord,
        Prices,
    ) {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        history
            .append(&live(utc("2026-11-06T00:00:00Z"), Some(ac())))
            .expect("a live event");
        let mut prices = Prices::default();
        let list = [
            (
                "anthropic/claude-opus-5-5",
                ModelPrice {
                    input: 30.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            ),
            (
                "anthropic/claude-haiku-5-5",
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            ),
        ]
        .into_iter()
        .map(|(key, price)| (key.to_string(), price))
        .collect();
        prices.take(list, utc("2026-11-01T00:00:00Z"));
        let opus = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            0,
            0,
            0,
        );
        let haiku = token(
            utc("2026-11-10T11:00:00Z"),
            "claude-haiku-5-5",
            1_000_000,
            0,
            0,
            0,
        );
        history.append(&opus).expect("a token record");
        history.append(&haiku).expect("a token record");
        (history, dir, opus, haiku, prices)
    }

    /// R4's history: three stored accounts' readings, the first `live` event at
    /// 10:00 and a 5-minute record 09:25–09:30 on atlas.
    fn readings_record(samples: Vec<Sample>) -> (HistoryDir, tempfile::TempDir, TokenRecord) {
        readings_record_on(
            samples
                .into_iter()
                .map(|sample| ("atlas", sample))
                .collect(),
            utc("2026-11-14T09:25:00Z"),
            5,
        )
    }

    /// [`readings_record`] with each reading on the host named beside it, and
    /// the record's start and minutes: one account's readings from two hosts
    /// are one series.
    fn readings_record_on(
        samples: Vec<(&str, Sample)>,
        start: DateTime<Utc>,
        minutes: u32,
    ) -> (HistoryDir, tempfile::TempDir, TokenRecord) {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        for (host, sample) in &samples {
            HistoryDir::at(dir.path(), host)
                .append(sample)
                .expect("a reading");
        }
        history
            .append(&live(utc("2026-11-14T10:00:00Z"), Some(ac())))
            .expect("the first live event");
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("AC's price");
        let mut record = token(start, "claude-opus-5-5", 1_000_000, 0, 0, 0);
        record.minutes = minutes;
        history.append(&record).expect("a token record");
        (history, dir, record)
    }

    fn stored_three() -> [Account; 3] {
        [
            stored_account(&ac(), "AC", Some(140.0)),
            stored_account(&lb(), "LB", None),
            stored_account(&pr(), "PR", None),
        ]
    }

    fn the_valuer(history: &HistoryDir) -> Valuer {
        Valuer::build(
            history,
            &range("2026-11-14T00:00:00Z", "2026-11-15T00:00:00Z"),
            &priced_prices(utc("2026-01-01T00:00:00Z")),
            &fx_eur(),
            &settings_eur(),
            &stored_three(),
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer")
    }

    #[test]
    fn work_before_the_first_live_event_takes_the_account_whose_reading_rose() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, week_end),
                Some((10.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, week_end),
                Some((20.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, Some(ac()));
        assert!(value.account_from_readings);
        assert_eq!(value.api, Some(4.0));
        // The record is valued as any record of AC: its share of the running
        // week that AC's readings opened, whose only record it is.
        assert!(
            (value.subscription.expect("a value") - 43.0 * percent_value(140.0)).abs() < 1e-9,
            "{value:?}"
        );
    }

    #[test]
    fn two_accounts_rising_leave_the_work_unknown() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (15.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (16.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn readings_more_than_30_minutes_away_leave_the_work_unknown() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T08:40:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T10:10:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn a_reset_between_the_readings_is_no_rise() {
        let first_end = utc("2026-11-18T10:00:00Z");
        let second_end = utc("2026-11-19T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, first_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, second_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, first_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, first_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, first_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, first_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn another_accounts_new_session_leaves_the_work_unknown() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let old_session_end = utc("2026-11-14T09:00:00Z");
        let new_session_end = utc("2026-11-14T14:10:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, new_session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, new_session_end)),
            ),
            // LB starts a new 5-hour session during the span: its session reset
            // moves from 09:00 to 14:10 and its percent falls from 78 to 2.
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((78.0, old_session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((2.0, new_session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, new_session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, new_session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        // AC's week rose, but LB was clearly working in a new session: the
        // work is not AC's alone.
        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn another_accounts_first_session_reset_leaves_the_work_unknown() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:10:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            // LB's earlier reading has no `session` limit; the later one has
            // one with a reset and a percent above 0: a session appeared.
            reading(utc("2026-11-14T09:20:00Z"), &lb(), (10.0, week_end), None),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((5.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn a_reading_exactly_30_minutes_away_counts() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T08:55:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T10:00:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        // 08:55 is exactly 30 minutes before the 09:25 start and 10:00 exactly
        // 30 minutes after the 09:30 end: both are within the reach.
        assert_eq!(value.account, Some(ac()));
        assert!(value.account_from_readings);
        assert!(value.subscription.is_some());
    }

    #[test]
    fn a_reading_30_minutes_and_a_second_away_does_not_count() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            // 08:54:59 is 30 minutes and one second before the 09:25 start.
            reading(
                utc("2026-11-14T08:54:59Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T10:00:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, None);
        assert!(!value.account_from_readings);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn readings_of_one_account_from_two_hosts_are_one_series() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:05:00Z"),
                    &ac(),
                    (41.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "vega",
                reading(
                    utc("2026-11-14T09:12:00Z"),
                    &ac(),
                    (41.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:20:00Z"),
                    &lb(),
                    (10.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:50:00Z"),
                    &lb(),
                    (10.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:20:00Z"),
                    &pr(),
                    (5.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:50:00Z"),
                    &pr(),
                    (5.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "vega",
                reading(
                    utc("2026-11-14T09:40:00Z"),
                    &ac(),
                    (43.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
            (
                "atlas",
                reading(
                    utc("2026-11-14T09:47:00Z"),
                    &ac(),
                    (43.0, week_end),
                    Some((0.0, session_end)),
                ),
            ),
        ];
        let (history, dir, record) = readings_record_on(samples, utc("2026-11-14T09:15:00Z"), 15);
        fs::write(
            dir.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .expect("the other host's marker");

        let value = the_valuer(&history).value("atlas", &record);

        assert_eq!(value.account, Some(ac()));
        assert!(value.account_from_readings);
    }

    #[test]
    fn work_after_a_logout_takes_the_account_whose_reading_rose() {
        let week_end = utc("2026-11-18T10:00:00Z");
        let session_end = utc("2026-11-14T14:00:00Z");
        let samples = vec![
            reading(
                utc("2026-11-14T09:20:00Z"),
                &ac(),
                (41.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &ac(),
                (43.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &lb(),
                (10.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:20:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
            reading(
                utc("2026-11-14T09:50:00Z"),
                &pr(),
                (5.0, week_end),
                Some((0.0, session_end)),
            ),
        ];
        let (history, _dir, record) = readings_record(samples);
        history
            .append(&live(utc("2026-11-14T09:00:00Z"), None))
            .expect("the host logged out");

        let value = the_valuer(&history).value("atlas", &record);

        // The host was logged out at 09:25, but AC's reading rose: work after
        // the logout is AC's.
        assert_eq!(value.account, Some(ac()));
        assert!(value.account_from_readings);
    }

    #[test]
    fn subscription_value_splits_the_used_week_by_api_value() {
        let (history, _dir, opus, haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let account = account_with_price(Some(140.0));
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let one = valuer.value("atlas", &opus);
        let two = valuer.value("atlas", &haiku);
        assert_eq!(one.account, Some(ac()));
        assert_eq!(one.api, Some(30.0));
        assert_eq!(two.api, Some(10.0));

        let week = 80.0 * percent_value(140.0);
        assert!((one.subscription.expect("a value") - 0.75 * week).abs() < 1e-9);
        assert!((two.subscription.expect("a value") - 0.25 * week).abs() < 1e-9);
        assert!(
            (one.subscription.expect("a value") + two.subscription.expect("a value") - week).abs()
                < 1e-9
        );
    }

    #[test]
    fn a_window_after_a_reset_is_valued_from_its_first_percent() {
        let (history, _dir, opus, haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let mut window = weekly_window(
            utc("2026-11-07T00:00:00Z"),
            utc("2026-11-14T00:00:00Z"),
            80.0,
        );
        window.from = Some(45.0);
        history.append(&window).expect("a window after a reset");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let one = valuer.value("atlas", &opus);
        let two = valuer.value("atlas", &haiku);
        let used = 35.0 * percent_value(140.0);
        assert!((one.subscription.expect("a value") - 0.75 * used).abs() < 1e-9);
        assert!((two.subscription.expect("a value") - 0.25 * used).abs() < 1e-9);
    }

    /// The weeks list counts a window that ends below its start as 0 and warns
    /// (T-1); the valuer must follow the same rule, not turn it into a negative
    /// subscription value.
    #[test]
    fn proof_a_window_ending_below_its_start_adds_nothing_to_the_valuer() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let mut window = weekly_window(
            utc("2026-11-07T00:00:00Z"),
            utc("2026-11-14T00:00:00Z"),
            20.0,
        );
        window.from = Some(45.0);
        history.append(&window).expect("a window below its start");
        let warnings = RefCell::new(Vec::new());
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &warnings,
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus).subscription.expect("a value");
        assert!(value >= 0.0, "a negative subscription value: {value}");
        assert!(
            !warnings.borrow().is_empty(),
            "the weeks list warns about such a window; the valuer said nothing"
        );
    }

    #[test]
    fn a_short_window_takes_the_price_at_its_midpoint() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(dir.path(), "atlas");
        for (at, amount) in [
            ("2026-11-01T00:00:00Z", 140.0),
            ("2026-11-12T12:00:00Z", 200.0),
        ] {
            history
                .append(&price_changed(utc(at), Some(amount), Some("EUR")))
                .expect("a price change");
        }
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-15T00:00:00Z"),
            &Prices::default(),
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(200.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        // The window after a reset ran two days: its midpoint is Friday 00:00,
        // after the change; 84 h before its end would be Tuesday noon.
        assert_eq!(
            valuer.window_price(
                &ac(),
                utc("2026-11-12T00:00:00Z"),
                utc("2026-11-14T00:00:00Z")
            ),
            Ok(200.0)
        );
        // A full week's midpoint lies before the change.
        assert_eq!(
            valuer.window_price(
                &ac(),
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z")
            ),
            Ok(140.0)
        );
    }

    #[test]
    fn a_window_above_100_percent_hands_out_what_was_paid() {
        let (history, _dir, opus, haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                105.0,
            ))
            .expect("a finished window above 100 %");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let sum = valuer.value("atlas", &opus).subscription.expect("a value")
            + valuer.value("atlas", &haiku).subscription.expect("a value");
        let paid = 100.0 * percent_value(140.0);
        assert!(
            (sum - paid).abs() < 1e-9,
            "the records share {sum}, but only {paid} was paid"
        );
    }

    #[test]
    fn a_window_of_two_hosts_uses_the_latest_last_sample() {
        let (history, dir, opus, haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let end = utc("2026-11-14T16:00:00Z");
        let start = end - Duration::days(7);
        let mut from_atlas = weekly_window(start, end, 61.0);
        from_atlas.last_sample = utc("2026-11-14T09:58:00Z");
        history.append(&from_atlas).expect("atlas's window");
        let vega = HistoryDir::at(dir.path(), "vega");
        let mut from_vega = weekly_window(start, end, 63.0);
        from_vega.last_sample = utc("2026-11-14T10:02:00Z");
        vega.append(&from_vega).expect("vega's window");
        fs::write(
            dir.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .expect("the other host's marker");
        let account = account_with_price(Some(140.0));
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        // The week's value uses vega's 63 %, not atlas's 61 %: the record with
        // the latest lastSample counts (spec §1.5).
        let week = 63.0 * percent_value(140.0);
        let one = valuer.value("atlas", &opus);
        let two = valuer.value("atlas", &haiku);
        assert!((one.subscription.expect("a value") - 0.75 * week).abs() < 1e-9);
        assert!((two.subscription.expect("a value") - 0.25 * week).abs() < 1e-9);
    }

    #[test]
    fn a_running_week_uses_the_percent_so_far() {
        let (history, _dir, opus, haiku, prices) = two_records();
        let end = utc("2026-11-14T00:00:00Z");
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        history
            .append(&weekly_sample(utc("2026-11-13T12:00:00Z"), end, 48.0))
            .expect("a running window's last sample");
        let account = account_with_price(Some(140.0));
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let sum = valuer.value("atlas", &opus).subscription.expect("a value")
            + valuer.value("atlas", &haiku).subscription.expect("a value");
        assert!((sum - 48.0 * percent_value(140.0)).abs() < 1e-9);
    }

    #[test]
    fn an_account_without_a_price_has_no_subscription_value() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let account = account_with_price(None);
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccountPrice));
        assert_eq!(value.api, Some(30.0));
    }

    #[test]
    fn tokens_before_the_first_live_event_have_no_account() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let prices = priced_prices(utc("2026-11-01T00:00:00Z"));
        let opus = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            0,
            0,
            0,
        );
        history.append(&opus).expect("a token record");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        // One event after the record: before the first event there is no account.
        history
            .append(&live(utc("2026-11-10T12:00:00Z"), Some(ac())))
            .expect("a later live event");
        let account = account_with_price(Some(140.0));
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus);
        assert_eq!(value.account, None);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));
    }

    #[test]
    fn prices_round_trip_and_refuse_another_version() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let t1 = utc("2026-11-01T00:00:00Z");
        let mut prices = priced_prices(t1);
        prices.checked_at = Some(utc("2026-11-14T09:00:00Z"));
        prices.etag = Some("W/\"abc\"".to_string());
        prices.save(&app).expect("saved");
        assert_eq!(Prices::load(&app).expect("loaded"), prices);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(app.prices_file())
                .expect("prices.json")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let fresh = AppPaths::at(&dir.path().join("fresh"));
        assert_eq!(
            Prices::load(&fresh).expect("a missing file is empty"),
            Prices::default()
        );

        fs::write(app.prices_file(), br#"{"version":2,"models":{}}"#).expect("another version");
        assert!(matches!(
            Prices::load(&app),
            Err(MoneyError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn an_old_prices_file_with_own_prices_reads_and_ignores_them() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        std::fs::create_dir_all(&app.data_dir).expect("the data folder");
        fs::write(
            app.prices_file(),
            br#"{
              "version": 1,
              "models": {
                "anthropic/claude-opus-5-5": [{
                  "from": "2026-11-01T00:00:00Z",
                  "price": {"input": 4, "output": 20, "cacheWrite": 5, "cacheRead": 0.2, "tiered": false}
                }]
              },
              "overrides": {
                "anthropic/claude-opus-5-5": {"input": 1, "output": 1, "cacheWrite": 0, "cacheRead": 0, "tiered": false}
              }
            }"#,
        )
        .expect("an old prices.json");
        let prices = Prices::load(&app).expect("an older file still loads");
        assert_eq!(
            prices.at("anthropic/claude-opus-5-5", utc("2026-11-10T00:00:00Z")),
            Some(&ModelPrice {
                input: 4.0,
                output: 20.0,
                cache_write: 5.0,
                cache_read: 0.2,
                tiered: false,
            }),
            "the own price in an old file is ignored, not used"
        );
        let saved = serde_json::to_value(&prices).expect("prices JSON");
        assert!(saved.get("overrides").is_none(), "it is not written back");
    }

    #[test]
    fn fx_take_replaces_a_date_and_keeps_order() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let mut fx = Fx::default();
        fx.take(date("2026-11-13"), 0.90);
        fx.take(date("2026-11-11"), 0.91);
        fx.take(date("2026-11-13"), 0.92);
        assert_eq!(
            fx.points.iter().map(|point| point.date).collect::<Vec<_>>(),
            vec![date("2026-11-11"), date("2026-11-13")]
        );
        assert_eq!(fx.at(date("2026-11-13")), Some(0.92));
        assert_eq!(fx.at(date("2026-11-12")), Some(0.91));
        assert_eq!(
            fx.at(date("2026-11-01")),
            Some(0.91),
            "before the first point, the first point"
        );

        let mut stored = fx.clone();
        stored.currency = "EUR".to_string();
        stored.save(&app).expect("saved");
        assert_eq!(Fx::load(&app).expect("loaded"), stored);

        let fresh = AppPaths::at(&dir.path().join("fresh"));
        assert_eq!(
            Fx::load(&fresh).expect("a missing file is empty"),
            Fx::default()
        );

        fs::write(
            app.fx_file(),
            br#"{"version":2,"currency":"EUR","points":[]}"#,
        )
        .expect("another version");
        assert!(matches!(
            Fx::load(&app),
            Err(MoneyError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn an_old_fx_file_has_no_fetch_time() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        std::fs::create_dir_all(&app.data_dir).expect("the data folder");
        fs::write(
            app.fx_file(),
            br#"{"version":1,"currency":"EUR","points":[{"date":"2026-11-13","rate":0.9048}]}"#,
        )
        .expect("an old fx.json");
        let fx = Fx::load(&app).expect("an older file still loads");
        assert_eq!(fx.at(date("2026-11-14")), Some(0.9048));
        assert_eq!(
            fx.fetched_at(date("2026-11-14")),
            None,
            "an old point keeps no fetch time; the page says it was not recorded"
        );
    }

    #[test]
    fn proof_a_second_identical_list_without_a_model_is_no_change() {
        let t1 = utc("2026-11-01T00:00:00Z");
        let mut prices = Prices::default();
        let mut list = model_list();
        prices.take(list.clone(), t1);
        list.remove("anthropic/claude-haiku-5-5");
        prices
            .take(list.clone(), utc("2026-11-02T00:00:00Z"))
            .expect("the first look-up without haiku changes");
        // An hour later the same list: nothing new happened.
        assert_eq!(prices.take(list, utc("2026-11-02T01:00:00Z")), None);
    }

    #[test]
    fn proof_a_body_with_none_of_the_three_providers_is_an_error() {
        // An error object, or any object of another shape, is not a price list.
        assert!(matches!(
            prices_from_models_dev(&json!({"message": "rate limited"})),
            Err(MoneyError::ModelsDev(_))
        ));
    }

    #[test]
    fn proof_the_first_price_counts_for_a_range_before_every_change() {
        let (history, _dir, opus, haiku, prices) = two_records();
        for (at, amount) in [
            ("2026-11-20T00:00:00Z", 100.0),
            ("2026-12-01T00:00:00Z", 180.0),
        ] {
            history
                .append(&Event {
                    t: utc(at),
                    kind: EventKind::PriceChanged {
                        account: ac(),
                        amount: Some(amount),
                        currency: Some("EUR".to_string()),
                    },
                })
                .expect("a price change");
        }
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        // The account costs 180 now; its first price ever was 100, entered on 20 Nov.
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(180.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");
        let sum = valuer.value("atlas", &opus).subscription.expect("a value")
            + valuer.value("atlas", &haiku).subscription.expect("a value");
        assert!(
            (sum - 80.0 * percent_value(100.0)).abs() < 1e-9,
            "the first price counts for the past, got {sum}"
        );
    }

    #[test]
    fn a_negative_cost_leaves_the_model_out_and_names_it() {
        let body = json!({
            "anthropic": {"id": "anthropic", "models": {
                "claude-opus-5-5": {"id": "claude-opus-5-5",
                    "cost": {"input": -4, "output": 20}},
                "claude-haiku-5-5": {"id": "claude-haiku-5-5",
                    "cost": {"input": 0.1, "output": 0.5}}
            }}
        });
        let list = prices_from_models_dev(&body).expect("the rest of the list is read");
        assert!(
            !list.prices.contains_key("anthropic/claude-opus-5-5"),
            "a model with a negative cost is left out"
        );
        assert!(
            list.prices.contains_key("anthropic/claude-haiku-5-5"),
            "the other models are kept"
        );
        assert_eq!(
            list.left_out.len(),
            1,
            "one model is named: {:?}",
            list.left_out
        );
        let entry = &list.left_out[0];
        assert!(
            entry.starts_with("anthropic/claude-opus-5-5: "),
            "the left-out entry names the model, got {entry}"
        );
        assert!(
            entry.contains("input") && entry.contains("-4"),
            "the left-out entry names the field and the value, got {entry}"
        );
    }

    #[test]
    fn a_cost_that_is_not_a_number_leaves_the_model_out_and_names_it() {
        let body = json!({
            "anthropic": {"id": "anthropic", "models": {
                "claude-opus-5-5": {"id": "claude-opus-5-5",
                    "cost": {"input": "four", "output": 20}},
                "claude-haiku-5-5": {"id": "claude-haiku-5-5",
                    "cost": {"input": 0.1, "output": 0.5}}
            }}
        });
        let list = prices_from_models_dev(&body).expect("the rest of the list is read");
        assert!(list.prices.contains_key("anthropic/claude-haiku-5-5"));
        assert_eq!(list.left_out.len(), 1);
        assert!(
            list.left_out[0].starts_with("anthropic/claude-opus-5-5: ")
                && list.left_out[0].contains("input"),
            "got {:?}",
            list.left_out
        );
    }

    #[test]
    fn a_price_must_be_finite_and_zero_or_more() {
        let good = ModelPrice {
            input: 0.0,
            output: 4.0,
            cache_write: 1.5,
            cache_read: 0.2,
            tiered: false,
        };
        assert_eq!(good.validate(), Ok(()));
        for (field, price) in [
            (
                "input",
                ModelPrice {
                    input: -1.0,
                    ..good.clone()
                },
            ),
            (
                "output",
                ModelPrice {
                    output: f64::NAN,
                    ..good.clone()
                },
            ),
            (
                "cache write",
                ModelPrice {
                    cache_write: f64::INFINITY,
                    ..good.clone()
                },
            ),
            (
                "cache read",
                ModelPrice {
                    cache_read: -0.1,
                    ..good.clone()
                },
            ),
        ] {
            let error = price.validate().expect_err("an invalid price");
            assert!(
                error.contains(field),
                "the error names {field}, got {error}"
            );
        }
    }

    #[test]
    fn a_price_in_another_currency_has_no_subscription_value() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&Event {
                t: utc("2026-11-06T00:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: ac(),
                    amount: Some(140.0),
                    currency: Some("USD".to_string()),
                },
            })
            .expect("a price change in USD");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");

        let value_of = |history: &HistoryDir| {
            Valuer::build(
                history,
                &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
                &prices,
                &empty_fx(),
                &settings_eur(),
                &[account_with_price(None)],
                &[],
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer")
        };
        let usd = value_of(&history).value("atlas", &opus);
        assert_eq!(
            usd.subscription, None,
            "a price in USD is not a subscription in EUR"
        );
        assert_eq!(
            usd.subscription_missing,
            Some(Missing::PriceInOtherCurrency {
                currency: "USD".to_string()
            })
        );
        assert_eq!(usd.api, Some(30.0), "the API value needs no subscription");

        // The same price entered in the current currency does count.
        history
            .append(&Event {
                t: utc("2026-11-06T01:00:00Z"),
                kind: EventKind::PriceChanged {
                    account: ac(),
                    amount: Some(140.0),
                    currency: Some("EUR".to_string()),
                },
            })
            .expect("a price change in EUR");
        let eur = value_of(&history).value("atlas", &opus);
        assert!(
            (eur.subscription.expect("a value") - 0.75 * 80.0 * percent_value(140.0)).abs() < 1e-9
        );
    }

    #[test]
    fn unpriced_tokens_make_the_weeks_value_unknown() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let unknown = token(
            utc("2026-11-10T12:00:00Z"),
            "claude-unknown",
            1_000_000,
            0,
            0,
            0,
        );
        history
            .append(&unknown)
            .expect("a token record without a price");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus);
        assert_eq!(
            value.subscription, None,
            "the priced records must not share the unpriced one's tokens"
        );
        assert_eq!(
            value.subscription_missing,
            Some(Missing::UnpricedInWeek {
                model: "claude-unknown".to_string()
            })
        );
        assert_eq!(value.api, Some(30.0), "the API value is still known");
    }

    #[test]
    fn zero_tokens_are_never_unpriced() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let synthetic = token(utc("2026-11-10T12:00:00Z"), "<synthetic>", 0, 0, 0, 0);
        history.append(&synthetic).expect("a record without tokens");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &synthetic);
        assert_eq!(value.api, Some(0.0), "no tokens cost nothing");
        assert_eq!(value.api_missing, None);
        assert_eq!(value.subscription, Some(0.0));

        let opus_value = valuer.value("atlas", &opus);
        assert_eq!(
            opus_value.subscription_missing, None,
            "a record without tokens does not make the week unknown"
        );
        let week = 80.0 * percent_value(140.0);
        assert!((opus_value.subscription.expect("a value") - 0.75 * week).abs() < 1e-9);
    }

    #[test]
    fn a_rate_of_another_currency_is_not_used() {
        let settings = Settings {
            currency: Some("CHF".to_string()),
            ..Settings::default()
        };
        let mut fx = Fx {
            version: 1,
            currency: "EUR".to_string(),
            points: Vec::new(),
        };
        fx.take(date("2026-11-13"), 0.95);
        assert_eq!(
            usd_rate(&settings, &fx, date("2026-11-14")),
            None,
            "an EUR point is not a CHF rate"
        );
    }

    #[test]
    fn a_price_without_a_currency_does_not_count() {
        let value_for = |with_change: bool| {
            let (history, _dir, opus, _haiku, prices) = two_records();
            if with_change {
                history
                    .append(&price_changed(
                        utc("2026-11-06T00:00:00Z"),
                        Some(140.0),
                        None,
                    ))
                    .expect("a price change before a currency was set");
            }
            history
                .append(&weekly_window(
                    utc("2026-11-07T00:00:00Z"),
                    utc("2026-11-14T00:00:00Z"),
                    80.0,
                ))
                .expect("a finished window");
            Valuer::build(
                &history,
                &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
                &prices,
                &empty_fx(),
                &settings_eur(),
                &[account_with_price(Some(140.0))],
                &[],
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer")
            .value("atlas", &opus)
        };

        let change = value_for(true);
        assert_eq!(change.subscription, None);
        assert_eq!(
            change.subscription_missing,
            Some(Missing::PriceWithoutCurrency)
        );

        let stored = value_for(false);
        assert_eq!(stored.subscription, None);
        assert_eq!(
            stored.subscription_missing,
            Some(Missing::PriceWithoutCurrency),
            "a stored price names no currency"
        );
    }

    #[test]
    fn a_stored_price_counts_in_its_own_currency() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let mut account = account_with_price(Some(140.0));
        account.price_currency = Some("EUR".to_string());

        let value_in = |currency: &str| {
            let settings = Settings {
                currency: Some(currency.to_string()),
                ..Settings::default()
            };
            Valuer::build(
                &history,
                &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
                &prices,
                &empty_fx(),
                &settings,
                std::slice::from_ref(&account),
                &[],
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer")
            .value("atlas", &opus)
        };

        let eur = value_in("EUR");
        assert!(
            eur.subscription.is_some(),
            "a stored price in the settings' currency counts: {:?}",
            eur.subscription_missing
        );

        let chf = value_in("CHF");
        assert_eq!(chf.subscription, None);
        assert_eq!(
            chf.subscription_missing,
            Some(Missing::PriceInOtherCurrency {
                currency: "EUR".to_string()
            }),
            "a stored price keeps the currency it was entered in"
        );
    }

    #[test]
    fn proof_without_a_currency_the_value_says_the_currency_is_missing() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let mut account = account_with_price(Some(140.0));
        account.price_currency = Some("EUR".to_string());
        let settings = Settings {
            currency: None,
            ..Settings::default()
        };

        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings,
            &[account],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus);
        assert_eq!(
            value.api_missing,
            Some(Missing::NoCurrency),
            "without a currency the API value says the currency is missing"
        );
        assert_eq!(
            value.subscription_missing,
            Some(Missing::NoCurrency),
            "without a currency there is no current currency to enter the price in"
        );
    }

    #[test]
    fn a_week_without_a_reading_says_so() {
        let (history, _dir, opus, _haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &opus);
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoReading));
    }

    #[test]
    fn every_missing_value_has_the_text_a_page_shows() {
        assert_eq!(
            Missing::NoCurrency.to_string(),
            "set a currency in Settings → General"
        );
        assert_eq!(
            Missing::NoPrice {
                model: "claude-unknown".to_string()
            }
            .to_string(),
            "no price for claude-unknown"
        );
        assert_eq!(
            Missing::NoAccount.to_string(),
            "no account was in use at that time"
        );
        assert_eq!(
            Missing::NoAccountPrice.to_string(),
            "the account has no monthly price"
        );
        assert_eq!(
            Missing::PriceInOtherCurrency {
                currency: "USD".to_string()
            }
            .to_string(),
            "the account's price is in USD; enter it again in the current currency"
        );
        assert_eq!(
            Missing::PriceWithoutCurrency.to_string(),
            "the account's price was entered before a currency was set; enter it again"
        );
        assert_eq!(
            Missing::NoReading.to_string(),
            "no reading of that week's usage"
        );
        assert_eq!(
            Missing::NoMonthlyAllowance.to_string(),
            "no monthly allowance in Ollama's reading"
        );
        assert_eq!(
            Missing::NoWeekReading.to_string(),
            "no reading of this week's usage"
        );
        assert_eq!(
            Missing::NoMonthReading.to_string(),
            "no reading of this month's usage"
        );
        assert_eq!(
            Missing::UnpricedInWeek {
                model: "claude-unknown".to_string()
            }
            .to_string(),
            "that week also has tokens of claude-unknown, which has no price"
        );
    }

    #[test]
    fn a_missing_rate_is_named() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let prices = priced_prices(utc("2026-11-01T00:00:00Z"));
        let opus = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            0,
            0,
            0,
        );
        let value_of = |settings: &Settings| {
            Valuer::build(
                &history,
                &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
                &prices,
                &fx_eur(),
                settings,
                &[],
                &[],
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer")
            .value("atlas", &opus)
        };

        let no_currency = value_of(&Settings::default());
        assert_eq!(
            no_currency.api, None,
            "without a currency there is no amount"
        );
        assert_eq!(
            no_currency.api_missing,
            Some(Missing::NoCurrency),
            "a missing currency is named"
        );
        assert_eq!(
            no_currency.api_currency, None,
            "a missing amount has no currency"
        );

        let with_rate = value_of(&settings_eur());
        assert_eq!(with_rate.api, Some(4.0));
        assert_eq!(with_rate.api_missing, None);
        assert_eq!(with_rate.api_currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn without_a_rate_the_api_value_is_in_usd() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let prices = priced_prices(utc("2026-11-01T00:00:00Z"));
        let opus = token(
            utc("2026-11-10T10:00:00Z"),
            "claude-opus-5-5",
            1_000_000,
            0,
            0,
            0,
        );
        let settings = Settings {
            currency: Some("CHF".to_string()),
            ..Settings::default()
        };
        let value = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings,
            &[],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer")
        .value("atlas", &opus);

        assert_eq!(value.api, Some(4.0), "the USD value is shown");
        assert_eq!(value.api_currency.as_deref(), Some("USD"));
        assert_eq!(value.api_missing, None);
    }

    // -----------------------------------------------------------------------
    // a local HTTP server for the two look-ups (never the internet)

    #[derive(Debug)]
    struct Captured {
        path: String,
        headers: Vec<(String, String)>,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(field, _)| field.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn ok(body: impl Into<String>) -> Reply {
            Reply {
                status: 200,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn status(status: u16) -> Reply {
            Reply {
                status,
                headers: Vec::new(),
                body: String::new(),
            }
        }

        fn header(mut self, name: &str, value: &str) -> Reply {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    fn start_server(
        reply: Reply,
    ) -> (
        String,
        std::sync::mpsc::Receiver<Captured>,
        std::thread::JoinHandle<()>,
    ) {
        use std::sync::mpsc;
        use std::thread;

        use tiny_http::{Header, Response, Server, StatusCode};

        let server = Server::http("127.0.0.1:0").expect("a local server");
        let address = server.server_addr().to_ip().expect("a local address");
        let base = format!("http://{address}");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut request = server.recv().expect("one request");
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("a body");
            let captured = Captured {
                path: request.url().to_string(),
                headers: request
                    .headers()
                    .iter()
                    .map(|header| {
                        (
                            header.field.as_str().as_str().to_string(),
                            header.value.as_str().to_string(),
                        )
                    })
                    .collect(),
            };
            let mut response =
                Response::from_string(reply.body).with_status_code(StatusCode(reply.status));
            for (name, value) in reply.headers {
                response = response
                    .with_header(Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap());
            }
            request.respond(response).expect("a reply");
            sender.send(captured).expect("the capture");
        });
        (base, receiver, handle)
    }

    fn lookup_api(base: &str) -> Api {
        Api::with_urls(
            &format!("{base}/api/oauth/usage"),
            &format!("{base}/v1/oauth/token"),
        )
        .with_lookup_urls(&format!("{base}/api.json"), &format!("{base}/v1/latest"))
    }

    fn captured(
        receiver: &std::sync::mpsc::Receiver<Captured>,
        handle: std::thread::JoinHandle<()>,
    ) -> Captured {
        let request = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the fake server received no request");
        handle.join().expect("the server thread");
        request
    }

    #[test]
    fn look_up_prices_stores_the_etag_and_the_points() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let now = utc("2026-11-14T09:00:00Z");
        let (base, receiver, handle) =
            start_server(Reply::ok(MODELS_DEV).header("ETag", "W/\"new\""));

        let looked = look_up_prices(&lookup_api(&base), &app, now).expect("the look-up");
        assert!(matches!(
            looked,
            Looked::Taken {
                change: Some(_),
                ..
            }
        ));
        let request = captured(&receiver, handle);
        assert_eq!(request.path, "/api.json");
        assert_eq!(request.header("If-None-Match"), None);

        let stored = Prices::load(&app).expect("prices.json");
        assert_eq!(stored.etag.as_deref(), Some("W/\"new\""));
        assert_eq!(stored.checked_at, Some(now));
        assert_eq!(stored.models.len(), 3);
        assert_eq!(stored.models["anthropic/claude-opus-5-5"].len(), 1);
    }

    #[test]
    fn look_up_prices_saves_nothing_on_a_failure() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let stored_at = utc("2026-11-13T09:00:00Z");
        let mut prices = priced_prices(stored_at);
        prices.checked_at = Some(stored_at);
        prices.etag = Some("W/\"old\"".to_string());
        prices.save(&app).expect("the stored prices");

        let (base, receiver, handle) = start_server(Reply::status(500));
        let error = look_up_prices(&lookup_api(&base), &app, utc("2026-11-14T09:00:00Z"))
            .expect_err("a failed look-up");
        assert!(matches!(error, MoneyError::Api(_)), "got {error:?}");
        let request = captured(&receiver, handle);
        assert_eq!(request.header("If-None-Match"), Some("W/\"old\""));

        let after = Prices::load(&app).expect("prices.json");
        assert_eq!(after.checked_at, Some(stored_at), "checkedAt stays");
        assert_eq!(after.etag.as_deref(), Some("W/\"old\""));
        assert_eq!(after.models, prices.models, "the stored prices stay");
    }

    #[test]
    fn look_up_rate_with_usd_sends_nothing() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        // A port with nothing listening: a request would be a network error.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let address = listener.local_addr().expect("the address");
        drop(listener);
        let api = lookup_api(&format!("http://{address}"));

        assert!(look_up_rate(&api, &app, "USD", utc("2026-11-13T10:10:00Z"))
            .expect("USD needs no request")
            .is_none());
        assert!(!app.fx_file().exists(), "USD writes no fx.json");
    }

    #[test]
    fn look_up_rate_replaces_an_fx_of_another_currency() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let old_rate = 0.8;
        let mut fx = Fx {
            currency: "GBP".to_string(),
            ..Fx::default()
        };
        fx.take(date("2026-11-10"), old_rate);
        fx.save(&app).expect("the old fx.json");

        let body = r#"{"base":"USD","date":"2026-11-13","rates":{"EUR":0.90481}}"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let now = utc("2026-11-13T10:10:00Z");
        let point = look_up_rate(&lookup_api(&base), &app, "EUR", now)
            .expect("the rate")
            .expect("a point");
        assert_eq!(point.date, date("2026-11-13"));
        assert_eq!(point.rate, 0.90481);
        assert_eq!(
            point.fetched_at,
            Some(now),
            "the point is stamped with the round's clock, not the wall clock"
        );
        let request = captured(&receiver, handle);
        assert_eq!(request.path, "/v1/latest?base=USD&symbols=EUR");

        let stored = Fx::load(&app).expect("fx.json");
        assert_eq!(stored.currency, "EUR");
        assert_eq!(stored.points.len(), 1, "the old currency's point is gone");
        assert_eq!(stored.points[0].date, date("2026-11-13"));
        assert_eq!(stored.points[0].rate, 0.90481);
        assert_eq!(
            stored.points[0].fetched_at,
            Some(now),
            "a looked-up point records when the round fetched it"
        );
    }

    #[test]
    fn look_up_rate_refuses_a_rate_that_is_not_above_zero() {
        let dir = tempdir().expect("tempdir");
        let app = AppPaths::at(&dir.path().join("data"));
        let body = r#"{"base":"USD","date":"2026-11-13","rates":{"EUR":0}}"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let error = look_up_rate(&lookup_api(&base), &app, "EUR", utc("2026-11-13T10:10:00Z"))
            .expect_err("a bad rate");
        assert!(
            error.to_string().contains("EUR") && error.to_string().contains('0'),
            "the error names the rate, got {error}"
        );
        let _ = captured(&receiver, handle);
        assert!(!app.fx_file().exists(), "a bad rate is not saved");
    }

    #[test]
    fn provider_records_do_not_change_the_claude_windows() {
        let without = tiny();
        let with = tiny();
        append_codex_records(&with.history, utc("2026-11-14T09:00:00Z"));
        let range = Range {
            from: utc("2026-11-02T00:00:00Z"),
            to: utc("2026-11-16T00:00:00Z"),
        };

        let before = Valuer::build(
            &without.history,
            &range,
            &without.prices,
            &without.fx,
            &without.settings,
            &without.accounts,
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("the valuer");
        let after = Valuer::build(
            &with.history,
            &range,
            &with.prices,
            &with.fx,
            &with.settings,
            &with.accounts,
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("the valuer");

        // P-2 gives a provider subscription its own windows; the Claude
        // accounts' windows must stay what they were.
        let windows = |valuer: &Valuer| {
            valuer
                .windows
                .iter()
                .filter(|window| Company::of_account(&window.account) == Company::Anthropic)
                .map(|window| {
                    (
                        window.account.clone(),
                        window.start,
                        window.end,
                        window.used,
                        window.finished,
                        window.last_sample,
                        window.api_sum,
                        window.unpriced.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            windows(&after),
            windows(&before),
            "a Codex window changed the Claude windows"
        );
        assert_eq!(after.price_changes, before.price_changes);
    }

    /// A provider record takes its share of its own subscription's window (R4,
    /// decision P-2 3): an OpenAI week 50 % used of a 23 EUR month price, with
    /// two records 3:1 in API value, gives them 3:1 of the week's used money.
    #[test]
    fn a_provider_record_gets_its_share_of_the_subscription() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let account = codex();
        history
            .append(&provider_sample(
                utc("2026-11-16T09:00:00Z"),
                &account,
                utc("2026-11-20T10:00:00Z"),
                50.0,
            ))
            .expect("the Codex reading");
        history
            .append(&provider_price_changed(
                utc("2026-11-01T00:00:00Z"),
                &account,
                23.0,
                "EUR",
            ))
            .expect("the subscription's price");
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.2-codex".to_string(),
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-11-01T00:00:00Z"),
        );
        let big = provider_token(utc("2026-11-16T10:00:00Z"), "gpt-6.2-codex", 3_000_000);
        let small = provider_token(utc("2026-11-16T11:00:00Z"), "gpt-6.2-codex", 1_000_000);
        history.append(&big).expect("a Codex record");
        history.append(&small).expect("a Codex record");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-16T00:00:00Z", "2026-11-17T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[],
            &[subscription(&account, "OA", None)],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let one = valuer.value("atlas", &big);
        let two = valuer.value("atlas", &small);
        assert_eq!(one.company, Company::OpenAI);
        assert_eq!(one.account, Some(account.clone()));
        assert_eq!(two.account, Some(account));
        assert_eq!(one.api, Some(30.0));
        assert_eq!(two.api, Some(10.0));
        assert_eq!(one.subscription_missing, None);
        assert_eq!(two.subscription_missing, None);

        let week = 50.0 * percent_value(23.0);
        assert!((one.subscription.expect("a value") - 0.75 * week).abs() < 1e-9);
        assert!((two.subscription.expect("a value") - 0.25 * week).abs() < 1e-9);
        assert!(
            (one.subscription.expect("a value") + two.subscription.expect("a value") - week).abs()
                < 1e-9
        );
    }

    /// Without a price the subscription share is missing with its reason, and
    /// the API value stays (decision P-2 3, Review Focus 5); never a 0.
    #[test]
    fn a_provider_without_a_price_has_no_subscription_value() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let account = codex();
        history
            .append(&provider_sample(
                utc("2026-11-16T09:00:00Z"),
                &account,
                utc("2026-11-20T10:00:00Z"),
                50.0,
            ))
            .expect("the Codex reading");
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.2-codex".to_string(),
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-11-01T00:00:00Z"),
        );
        let record = provider_token(utc("2026-11-16T10:00:00Z"), "gpt-6.2-codex", 3_000_000);
        history.append(&record).expect("a Codex record");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-16T00:00:00Z", "2026-11-17T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[],
            &[subscription(&account, "OA", None)],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &record);
        assert_eq!(value.account, Some(account));
        assert_eq!(value.api, Some(30.0), "the API value stays");
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccountPrice));
    }

    /// A record's OpenAI subscription is the stored one the `providerSwitch`
    /// rule names, never a window or a sample of the page's range (decision
    /// P-2 1, review B): two stored logins with no switch are `NoAccount` for a
    /// narrow and a wide range alike, and one stored login is chosen for both.
    #[test]
    fn a_records_openai_subscription_does_not_depend_on_the_range() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let oa = codex();
        let oa2 = AccountRef {
            acc: "5d2e7a1c-3b4f-4a6e-9c8d-1f2a3b4c5d6e".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        // OA was read in October and OA2 on 16 November; neither reading may
        // decide which login the record belongs to.
        history
            .append(&provider_sample(
                utc("2026-10-20T09:00:00Z"),
                &oa,
                utc("2026-11-01T00:00:00Z"),
                30.0,
            ))
            .expect("OA's October reading");
        history
            .append(&provider_sample(
                utc("2026-11-16T09:00:00Z"),
                &oa2,
                utc("2026-11-20T10:00:00Z"),
                40.0,
            ))
            .expect("OA2's reading");
        let record = provider_token(utc("2026-11-16T10:00:00Z"), "gpt-6.2-codex", 1_000_000);
        history.append(&record).expect("a Codex record");
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.2-codex".to_string(),
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-11-01T00:00:00Z"),
        );
        let narrow = range("2026-11-16T00:00:00Z", "2026-11-17T00:00:00Z");
        let wide = range("2026-10-01T00:00:00Z", "2026-11-17T00:00:00Z");

        for range in [&narrow, &wide] {
            let two = [
                subscription(&oa, "OA", None),
                subscription(&oa2, "OA2", None),
            ];
            let valuer = Valuer::build(
                &history,
                range,
                &prices,
                &empty_fx(),
                &settings_eur(),
                &[],
                &two,
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer");
            let value = valuer.value("atlas", &record);
            assert_eq!(
                value.account, None,
                "two stored OpenAI logins with no switch guess one"
            );
            assert_eq!(value.subscription_missing, Some(Missing::NoAccount));

            let one = [subscription(&oa, "OA", None)];
            let valuer = Valuer::build(
                &history,
                range,
                &prices,
                &empty_fx(),
                &settings_eur(),
                &[],
                &one,
                &RefCell::new(Vec::new()),
            )
            .expect("a valuer");
            let value = valuer.value("atlas", &record);
            assert_eq!(
                value.account,
                Some(oa.clone()),
                "the only stored OpenAI login is the one for both ranges"
            );
        }
    }

    /// A subscription's stored monthly price is the price where no
    /// `PriceChanged` names it, exactly as a Claude account's stored price is
    /// (decision P-2 1).
    #[test]
    fn a_provider_price_without_an_event_is_the_stored_price() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let account = codex();
        history
            .append(&provider_sample(
                utc("2026-11-16T09:00:00Z"),
                &account,
                utc("2026-11-20T10:00:00Z"),
                50.0,
            ))
            .expect("the Codex reading");
        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.2-codex".to_string(),
                ModelPrice {
                    input: 10.0,
                    output: 0.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                    tiered: false,
                },
            )]),
            utc("2026-11-01T00:00:00Z"),
        );
        let record = provider_token(utc("2026-11-16T10:00:00Z"), "gpt-6.2-codex", 1_000_000);
        history.append(&record).expect("a Codex record");
        let stored = [subscription(&account, "OA", Some(23.0))];
        let valuer = Valuer::build(
            &history,
            &range("2026-11-16T00:00:00Z", "2026-11-17T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[],
            &stored,
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &record);
        assert_eq!(value.account, Some(account));
        assert_eq!(value.subscription_missing, None);
        assert!(
            (value.subscription.expect("a value") - 50.0 * percent_value(23.0)).abs() < 1e-9,
            "50 % of the stored 23 EUR: {:?}",
            value.subscription
        );
    }

    #[test]
    fn plan_models_are_the_claude_models() {
        assert!(is_plan_model("claude-opus-5-5"));
        assert!(is_plan_model("claude-fable-5-2"));
        assert!(!is_plan_model("deepseek-v4.1-flash:cloud"));
        assert!(!is_plan_model("gpt-6.1-sol"));
        assert!(!is_plan_model("glm-5:cloud"));
    }

    #[test]
    fn company_of_model_maps_gpt_to_openai_and_cloud_tags_to_ollama() {
        for (model, company) in [
            ("claude-opus-5-5", Company::Anthropic),
            ("claude-fable-5-2", Company::Anthropic),
            ("gpt-5.5-codex", Company::OpenAI),
            ("gpt-6.1-sol", Company::OpenAI),
            ("deepseek-v4.1-flash:cloud", Company::Ollama),
            ("deepseek-v4.1-flash:1017-cloud", Company::Ollama),
            ("glm-5:cloud", Company::Ollama),
            // Neither Ollama form: priced as `openai`, so OpenAI's.
            ("gpt-6.1-cloud", Company::OpenAI),
            (":cloud", Company::OpenAI),
            // No record of agy's work is written yet, so no id is Google's.
            ("gemini-4-argon", Company::OpenAI),
        ] {
            assert_eq!(Company::of_model(model), company, "{model}");
            assert_eq!(
                is_plan_model(model),
                company == Company::Anthropic,
                "{model}: one rule"
            );
        }
        assert_eq!(Company::Anthropic.name(), "Anthropic");
        assert_eq!(Company::OpenAI.name(), "OpenAI");
        assert_eq!(Company::Ollama.name(), "Ollama");
        assert_eq!(Company::Google.name(), "Google");
        assert_eq!(
            serde_json::to_value(Company::ALL).unwrap(),
            serde_json::json!(["anthropic", "openai", "ollama", "google"])
        );
    }

    #[test]
    fn company_of_account_follows_the_org() {
        assert_eq!(Company::of_account(&ac()), Company::Anthropic);
        for (org, company) in [
            (crate::providers::CODEX_ORG, Company::OpenAI),
            (crate::providers::OLLAMA_ORG, Company::Ollama),
            (crate::providers::GOOGLE_ORG, Company::Google),
        ] {
            let account = AccountRef {
                acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
                org: org.to_string(),
            };
            assert_eq!(Company::of_account(&account), company, "{org}");
        }
    }

    /// A Codex or Ollama record done while AC was live belongs to its provider
    /// and to no Claude account: its subscription value is a known 0, its API
    /// value its own. A Claude record of the same hour stays AC's.
    #[test]
    fn a_codex_record_belongs_to_no_claude_account() {
        let (history, _dir, opus, _haiku, mut prices) = two_records();
        prices.take(
            BTreeMap::from([(
                "openai/gpt-6.1-sol".to_string(),
                ModelPrice {
                    input: 1.25,
                    output: 10.0,
                    cache_write: 0.0,
                    cache_read: 0.125,
                    tiered: false,
                },
            )]),
            utc("2026-11-01T00:00:00Z"),
        );
        let codex = token(
            utc("2026-11-10T10:30:00Z"),
            "gpt-6.1-sol",
            0,
            2_000_000,
            0,
            0,
        );
        let flash = token(
            utc("2026-11-10T10:40:00Z"),
            "deepseek-v4.1-flash:cloud",
            0,
            1_000_000,
            0,
            0,
        );
        history.append(&codex).expect("a Codex record");
        history.append(&flash).expect("a flash record");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let value = valuer.value("atlas", &codex);
        assert_eq!(value.company, Company::OpenAI);
        assert_eq!(
            value.account, None,
            "AC was live, but Codex uses no Claude account; the history names \
             no OpenAI subscription here, so none is guessed"
        );
        assert_eq!(
            value.api,
            Some(20.0),
            "the API value stays (Review Focus 5)"
        );
        assert_eq!(value.subscription, None);
        assert_eq!(value.subscription_missing, Some(Missing::NoAccount));

        let value = valuer.value("atlas", &flash);
        assert_eq!(value.company, Company::Ollama);
        assert_eq!(value.account, Some(ollama_subscription()));
        assert_eq!(
            value.subscription, None,
            "no month reading of Ollama's credits, and the model has no price"
        );
        assert_eq!(
            value.subscription_missing,
            Some(Missing::NoPrice {
                model: "deepseek-v4.1-flash:cloud".to_string()
            })
        );

        let value = valuer.value("atlas", &opus);
        assert_eq!(value.company, Company::Anthropic);
        assert_eq!(value.account, Some(ac()));
    }

    /// A flash worker's records never touch the Claude account's week: they add
    /// no API money and never make the week's subscription value unknown.
    #[test]
    fn a_worker_record_never_touches_the_claude_week() {
        let (history, _dir, opus, haiku, prices) = two_records();
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        history
            .append(&weekly_window(
                utc("2026-11-07T00:00:00Z"),
                utc("2026-11-14T00:00:00Z"),
                80.0,
            ))
            .expect("a finished window");
        let worker = token(
            utc("2026-11-10T12:00:00Z"),
            "deepseek-v4.1-flash:cloud",
            1_000_000,
            1_000_000,
            0,
            0,
        );
        history.append(&worker).expect("a worker record");

        let valuer = Valuer::build(
            &history,
            &range("2026-11-10T00:00:00Z", "2026-11-11T00:00:00Z"),
            &prices,
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        for record in [&opus, &haiku] {
            let value = valuer.value("atlas", record);
            assert!(
                value.subscription.is_some(),
                "a Claude record lost its week's value: {value:?}"
            );
            assert_eq!(value.subscription_missing, None);
        }
        let worker_value = valuer.value("atlas", &worker);
        assert_eq!(
            worker_value.account,
            Some(ollama_subscription()),
            "the worker belongs to Ollama's subscription, not to AC"
        );
        assert_eq!(
            worker_value.subscription, None,
            "a worker uses no share of the Claude account's week; Ollama's month \
             has no reading here, so its value is unknown, never a 0"
        );
        assert_eq!(
            worker_value.subscription_missing,
            Some(Missing::NoPrice {
                model: "deepseek-v4.1-flash:cloud".to_string()
            })
        );

        let api_sum: f64 = valuer.windows.iter().map(|window| window.api_sum).sum();
        assert!(
            (api_sum - 40.0).abs() < 1e-9,
            "the week's API sum is the Claude records' alone, got {api_sum}"
        );
        assert!(
            valuer
                .windows
                .iter()
                .all(|window| window.unpriced.is_none()),
            "a worker record made the week unpriced"
        );
    }

    /// A reset that also moved the reset time: the reset reading names the new
    /// reset time, so the running window's `from` is that reading's percent and
    /// its use is `p − from` (60 − 45), not its whole percent.
    #[test]
    fn a_reset_that_moved_the_reset_time_still_gives_the_running_window_its_from() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let old_reset = utc("2026-11-20T10:00:00Z");
        let new_reset = utc("2026-11-27T10:00:00Z");
        let before = utc("2026-11-16T09:00:00Z");
        let reset_reading = utc("2026-11-18T08:30:00Z");
        let later = utc("2026-11-19T12:00:00Z");
        history
            .append(&live(utc("2026-11-16T00:00:00Z"), Some(ac())))
            .expect("a live event");
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        history
            .append(&weekly_sample(before, old_reset, 70.0))
            .expect("a weekly sample");
        history
            .append(&weekly_sample(reset_reading, new_reset, 45.0))
            .expect("a weekly sample");
        history
            .append(&weekly_sample(later, new_reset, 60.0))
            .expect("a weekly sample");
        let mut reset = weekly_window(old_reset - Duration::days(7), reset_reading, 70.0);
        reset.reset = true;
        reset.last_sample = before;
        history.append(&reset).expect("the reset window");

        let valuer = Valuer::build(
            &history,
            &range("2026-11-19T00:00:00Z", "2026-11-20T00:00:00Z"),
            &priced_prices(utc("2026-11-01T00:00:00Z")),
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let open = valuer
            .windows
            .iter()
            .find(|window| !window.finished && same_reset(window.end, new_reset))
            .expect("the running window");
        assert_eq!(
            open.used,
            Some(15.0),
            "the running window's use is 60 − 45, not 60"
        );
    }

    /// The running window is the one of the account's newest reading: after a
    /// reset that moved the reset time, the window the older readings named
    /// (reset 20 Nov, still in the future) is not running; the new one is,
    /// with its newest percent and the `from` the reset began it at. Once its
    /// reset has passed, nothing runs.
    #[test]
    fn the_running_window_is_the_one_of_the_newest_reading() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let old_reset = utc("2026-11-20T10:00:00Z");
        let new_reset = utc("2026-11-27T10:00:00Z");
        let before = utc("2026-11-16T09:00:00Z");
        let reset_reading = utc("2026-11-18T08:30:00Z");
        let later = utc("2026-11-19T12:00:00Z");
        history
            .append(&weekly_sample(before, old_reset, 70.0))
            .expect("a weekly sample");
        history
            .append(&weekly_sample(reset_reading, new_reset, 45.0))
            .expect("a weekly sample");
        history
            .append(&weekly_sample(later, new_reset, 60.0))
            .expect("a weekly sample");
        let mut reset = weekly_window(old_reset - Duration::days(7), reset_reading, 70.0);
        reset.reset = true;
        reset.last_sample = before;
        history.append(&reset).expect("the reset window");

        let valuer = Valuer::build(
            &history,
            &range("2026-11-16T00:00:00Z", "2026-11-19T13:00:00Z"),
            &priced_prices(utc("2026-11-01T00:00:00Z")),
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let now = utc("2026-11-19T13:00:00Z");
        assert_eq!(
            valuer.running_windows(now),
            vec![WindowRecord {
                account: ac(),
                k: crate::api::LimitKind::WeeklyAll,
                m: None,
                start: Some(new_reset - Duration::days(7)),
                end: new_reset,
                final_percent: 60.0,
                lost: None,
                last_sample: later,
                plan: None,
                uncertain: false,
                reset: false,
                from: Some(45.0),
            }]
        );
        assert!(valuer
            .running_windows(new_reset + Duration::minutes(1))
            .is_empty());
    }

    /// After a model reset that keeps the reset time, the running window holds
    /// tokens from after the reset: its use is `p − from`, not `p`.
    #[test]
    fn a_running_week_after_a_reset_uses_the_percent_minus_from() {
        let dir = tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let reset_at = utc("2026-11-20T10:00:00Z");
        let before = utc("2026-11-16T09:00:00Z");
        let reset_reading = utc("2026-11-18T08:30:00Z");
        let later = utc("2026-11-19T12:00:00Z");
        history
            .append(&live(utc("2026-11-16T00:00:00Z"), Some(ac())))
            .expect("a live event");
        history
            .append(&price_changed(
                utc("2026-11-06T00:00:00Z"),
                Some(140.0),
                Some("EUR"),
            ))
            .expect("the account's price");
        for (at, percent) in [(before, 70.0), (reset_reading, 45.0), (later, 60.0)] {
            history
                .append(&weekly_sample(at, reset_at, percent))
                .expect("a weekly sample");
        }
        let mut reset = weekly_window(reset_at - Duration::days(7), reset_reading, 70.0);
        reset.reset = true;
        reset.last_sample = before;
        history.append(&reset).expect("the reset window");
        let opus = token(later, "claude-opus-5-5", 1_000_000, 0, 0, 0);
        history.append(&opus).expect("a token record");

        let valuer = Valuer::build(
            &history,
            &range("2026-11-19T00:00:00Z", "2026-11-20T00:00:00Z"),
            &priced_prices(utc("2026-11-01T00:00:00Z")),
            &empty_fx(),
            &settings_eur(),
            &[account_with_price(Some(140.0))],
            &[],
            &RefCell::new(Vec::new()),
        )
        .expect("a valuer");

        let open = valuer
            .windows
            .iter()
            .find(|window| !window.finished && same_reset(window.end, reset_at))
            .expect("the running window");
        assert_eq!(
            open.used,
            Some(15.0),
            "the running window's use is 60 − 45, not 60"
        );
        let value = valuer
            .value("atlas", &opus)
            .subscription
            .expect("a subscription value");
        let expected = 15.0 * percent_value(140.0);
        assert!(
            (value - expected).abs() < 1e-9,
            "the record's subscription uses 15 %, got {value} instead of {expected}"
        );
    }
}
