//! The two worker threads behind the tray app (docs/implementation.md
//! "app: GUI", **Threads**).
//!
//! - the engine thread owns `Engine<Api>` and its own `Store`; it polls the
//!   usage, notices a wake-up and publishes `Snapshot`s;
//! - the action thread owns a second `Store`; switches, edits, removals and
//!   settings changes run there one at a time, so no Tauri command blocks the
//!   main thread and no core call runs on the watcher thread.
//!
//! Nothing here reads or writes Claude Code's files directly: only `switcher`,
//! `engine` and `store` do.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use serde::Serialize;

use seatswap_core::api::Api;
#[cfg(test)]
use seatswap_core::api::ApiError;
#[cfg(test)]
use seatswap_core::burn::decide::Regime;
#[cfg(test)]
use seatswap_core::burn::plan::Inputs;
#[cfg(test)]
use seatswap_core::engine::Remote;
use seatswap_core::engine::{
    AccountView, Engine, EngineError, LoginWarning, PollReason, UsageCache,
};
use seatswap_core::feed;
use seatswap_core::history::{
    AccountRef, By, Event, EventKind, HistoryDir, HistoryError, Hosts, Range, Recorder,
    SessionCounts,
};
use seatswap_core::locks::{self, LockError};
use seatswap_core::money::Company;
use seatswap_core::paths::{AppPaths, ClaudePaths, CodexPaths};
use seatswap_core::providers;
use seatswap_core::shared;
use seatswap_core::store::{self, Account, AccountsFile, ProviderSubscription, Settings, Store};
use seatswap_core::switcher::{self, LiveState, SwitchError, Synced, UnsavedPolicy};
#[cfg(test)]
use seatswap_core::types::Login;

use super::autostart;
use super::burn_rt::{BurnRuntime, BurnTick, KeptForecast};
use super::lookup_rt::{self, LookupClient, LookupHandle, MoneyView};
use super::notify;
use super::providers_rt::{
    self, CodexLoginView, Provider, ProviderLists, ProviderView, ProvidersClient, ProvidersHandle,
};
use super::sessions_rt::{self, SessionsHandle};
use super::ReportedErrors;
use crate::format::{expand_tilde, with_tilde};
use crate::ops;

/// How often the engine thread polls even without a command.
const TICK: Duration = Duration::from_secs(30);

/// How often the engine thread asks the action thread to exchange the shared
/// looks and settings with the other hosts (spec §5): on its poll tick, at most
/// this often. Another host's edit arrives with the delay of the folder's sync
/// plus up to this.
const SHARE_EVERY_MIN: i64 = 5;

/// The next timer poll as a fixed deadline. A command may arrive first, but it
/// never moves the deadline: the timer poll runs as soon as it has passed.
struct TickDeadline {
    next: Instant,
}

impl TickDeadline {
    fn new(now: Instant) -> TickDeadline {
        TickDeadline { next: now + TICK }
    }

    /// How long to wait for a command before the timer poll is due.
    fn wait(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next
    }

    /// Move past `now`: the next deadline is the first one that has not passed.
    fn advance(&mut self, now: Instant) {
        self.next += TICK;
        while self.next <= now {
            self.next += TICK;
        }
    }
}

/// More wall-clock time than this beyond the monotonic clock between two ticks
/// means the machine slept.
const WAKE_THRESHOLD: Duration = Duration::from_secs(90);

/// What the panel sees: the whole GUI state, serde camelCase (docs/implementation.md).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub views: Vec<AccountView>,
    /// The stored accounts behind the views: the Accounts group needs fields the
    /// card does not (`price`, `orgName`) and never a usage payload.
    pub accounts: Vec<AccountRow>,
    /// The provider subscriptions, listed under the Claude accounts in Settings
    /// → Accounts (spec R4).
    pub provider_subscriptions: Vec<ProviderSubscriptionRow>,
    pub active_id: Option<String>,
    pub live: LiveView,
    pub burn: Option<super::burn_rt::BurnView>,
    pub warnings: Vec<LoginWarning>,
    pub pending: Option<PendingSwitch>,
    pub secrets_warning: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// `None` when `readSessions` is off; the sessions thread fills it in on the
    /// main thread.
    pub sessions: Option<SessionsSummary>,
    /// The switched-on provider cards; the provider thread fills them in on the
    /// main thread.
    pub providers: Vec<ProviderView>,
    /// The saved Codex logins (R8), live first; the provider thread fills them
    /// in on the main thread and it is empty while the OpenAI card is off.
    pub codex_logins: Vec<CodexLoginView>,
    /// `None` when accounts.json cannot be read: the panel shows the error and
    /// disables Save instead of offering defaults that would overwrite it.
    pub settings: Option<Settings>,
    /// Claude Code's config folder as the app runs with it, with `~` for the
    /// home folder. The General group shows it as the empty field's placeholder.
    pub claude_config_dir: String,
    /// Claude Code's folder does not exist yet (it was never run on this
    /// machine). The pop-up says so calmly instead of an error; the GUI sets it
    /// from the login watch (`watcher::WatchState::NotSetUp`).
    pub claude_not_set_up: bool,
}

/// One stored account as the Accounts group shows it. `loginDaysLeft` comes
/// from the matching view, so the page need not do date arithmetic.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountRow {
    pub id: String,
    pub label: String,
    pub abbr: String,
    pub color: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub plan: Option<String>,
    pub price: Option<f64>,
    /// The currency `price` was entered in; `None` when there is no price or it
    /// was entered before a currency was set.
    pub price_currency: Option<String>,
    pub auto_burn: bool,
    pub login_ends_at_ms: Option<i64>,
    pub login_days_left: Option<i64>,
    pub is_active: bool,
}

impl AccountRow {
    fn of(account: &Account, views: &[AccountView]) -> AccountRow {
        let view = views.iter().find(|view| view.id == account.id);
        AccountRow {
            id: account.id.clone(),
            label: account.label.clone(),
            abbr: account.abbr.clone(),
            color: account.color.clone(),
            email: account.email.clone(),
            org_name: account.org_name.clone(),
            plan: account.plan.clone(),
            price: account.price,
            price_currency: account.price_currency.clone(),
            auto_burn: account.auto_burn,
            login_ends_at_ms: account.login_ends_at_ms,
            login_days_left: view.and_then(|view| view.login_days_left),
            is_active: view.is_some_and(|view| view.is_active),
        }
    }
}

/// One provider subscription as Settings → Accounts shows it: its account
/// reference, the company the org names, and its look, plan and price.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderSubscriptionRow {
    pub account: AccountRef,
    pub company: Company,
    pub label: String,
    pub abbr: String,
    pub color: String,
    pub plan: Option<String>,
    pub price: Option<f64>,
    pub price_currency: Option<String>,
}

impl ProviderSubscriptionRow {
    fn of(subscription: &ProviderSubscription) -> ProviderSubscriptionRow {
        ProviderSubscriptionRow {
            account: subscription.account.clone(),
            company: Company::of_account(&subscription.account),
            label: subscription.label.clone(),
            abbr: subscription.abbr.clone(),
            color: subscription.color.clone(),
            plan: subscription.plan.clone(),
            price: subscription.price,
            price_currency: subscription.price_currency.clone(),
        }
    }
}

/// Claude Code's running sessions as the panel shows them (spec §1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionsSummary {
    pub running: u32,
    pub busy: u32,
    /// Sessions whose last model ties its thinking to the account (spec §2.11).
    pub tied: u32,
    /// Of those, the sessions that are busy now.
    pub tied_busy: u32,
    pub waiting: Vec<Waiting>,
    pub unavailable: Option<String>,
    pub warning: Option<String>,
}

/// A running session that waits for the person (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Waiting {
    pub session: String,
    pub label: String,
    /// Claude Code's own title of the session; the waiting notification names
    /// the session by it when it is known, else by `label`.
    pub title: Option<String>,
    pub project: String,
    pub since: DateTime<Utc>,
    /// The host the session runs on; `None` is this host.
    pub host: Option<String>,
}

impl Snapshot {
    pub(crate) fn empty(
        error: Option<String>,
        secrets_warning: Option<String>,
        settings: Option<Settings>,
    ) -> Snapshot {
        Snapshot {
            views: Vec::new(),
            accounts: Vec::new(),
            provider_subscriptions: Vec::new(),
            active_id: None,
            live: LiveView::NoLogin,
            burn: None,
            warnings: Vec::new(),
            pending: None,
            secrets_warning,
            last_poll_at: None,
            error,
            sessions: None,
            providers: Vec::new(),
            codex_logins: Vec::new(),
            settings,
            claude_config_dir: String::new(),
            claude_not_set_up: false,
        }
    }

    pub(crate) fn set_pending(&mut self, pending: PendingSwitch) {
        self.pending = Some(pending);
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// A pending switch survives only while its target is still in the views.
    pub(crate) fn retain_pending(&mut self, previous: Option<&PendingSwitch>) {
        self.pending = previous
            .filter(|pending| self.views.iter().any(|view| view.id == pending.target_id))
            .cloned();
    }
}

/// The live login as the panel shows it. Carries no token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub(crate) enum LiveView {
    NoLogin,
    Ended {
        account_id: Option<String>,
    },
    Known {
        account_id: String,
    },
    Unknown {
        email: Option<String>,
        org_name: Option<String>,
    },
}

impl From<LiveState> for LiveView {
    fn from(state: LiveState) -> LiveView {
        match state {
            LiveState::NoLogin => LiveView::NoLogin,
            LiveState::Ended { account_id } => LiveView::Ended { account_id },
            LiveState::Known { account_id } => LiveView::Known { account_id },
            LiveState::Unknown { email, org_name } => LiveView::Unknown { email, org_name },
        }
    }
}

/// A switch waiting for "save or discard the current login?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingSwitch {
    pub target_id: String,
    pub target_label: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
}

/// Engine thread commands.
#[derive(Debug, Clone)]
pub(crate) enum EngineCommand {
    Poll(PollReason),
    Publish,
    /// The status line wrote `feed.json`; read it and merge it into the cache.
    Feed,
    /// The sessions thread saw these models in use (lower-case ids), or `None`
    /// when it cannot tell, how many sessions are on a model whose thinking is
    /// tied to the account, and those sessions' answers with thinking minute by
    /// minute (spec §2.11, §2.12).
    ModelsInUse {
        models: Option<BTreeSet<String>>,
        tied_busy: u32,
        thinking: Vec<(DateTime<Utc>, u32)>,
    },
    /// A pause or resume was written to the history; learn it before deciding.
    HistoryChanged,
    Shutdown,
}

/// Action thread commands. Each request that needs an answer carries its own
/// one-shot sender, so a Tauri command never waits on a shared reply slot.
pub(crate) enum ActionCommand {
    Switch {
        key: String,
        unsaved: UnsavedPolicy,
        label: Option<String>,
        by: By,
        reason: Option<String>,
        reply: Sender<ActionReply>,
    },
    SaveCurrent {
        label: Option<String>,
        reply: Sender<ActionReply>,
    },
    Edit {
        key: String,
        label: Option<String>,
        abbr: Option<String>,
        color: Option<String>,
        auto_burn: Option<bool>,
        /// `None` leaves the price alone; `Some(None)` clears it.
        price: Option<Option<f64>>,
        reply: Sender<ActionReply>,
    },
    EditSubscription {
        account: AccountRef,
        label: Option<String>,
        abbr: Option<String>,
        color: Option<String>,
        /// `None` leaves the plan alone; `Some(None)` clears it. OpenAI refuses
        /// either because its plan comes from the provider.
        plan: Option<Option<String>>,
        price: Option<Option<f64>>,
        reply: Sender<ActionReply>,
    },
    Remove {
        key: String,
        reply: Sender<ActionReply>,
    },
    /// Switch Codex's live login to a saved one (R8).
    SwitchCodex {
        account: AccountRef,
        reply: Sender<ActionReply>,
    },
    /// Delete seatswap's copy of a saved Codex login (R8); Codex's own
    /// `auth.json` is not touched.
    RemoveCodexLogin {
        account: AccountRef,
        reply: Sender<ActionReply>,
    },
    /// Write seatswap's status line into `agy`'s settings (spec §7.9).
    ConnectAgy { reply: Sender<ActionReply> },
    SaveSetting {
        field: String,
        value: serde_json::Value,
        reply: Sender<ActionReply>,
    },
    /// The watcher saw a change; mirror it when `mirrorLive` is on.
    SyncLive,
    /// Pause auto-burn for `hours`, or resume with `None` (spec §2.5).
    PauseBurn { hours: Option<i64> },
    /// Exchange the shared account looks and settings with the other hosts
    /// (spec §5): the engine thread asks at start and every
    /// [`SHARE_EVERY_MIN`] minutes.
    Share,
}

/// One saved setting: the settings as they are after the merge, and a warning
/// when the settings were saved but the autostart entry could not be written.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedSetting {
    pub(crate) settings: Settings,
    pub(crate) warning: Option<String>,
}

/// `Saved` carries the whole settings, including the status line's previous
/// value; a settings save happens once per edit, so boxing it would only move
/// the allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum ActionReply {
    Done,
    Switched(String),
    NotSaved(PendingSwitch),
    Saved(SavedSetting),
    Failed(String),
}

/// What the engine thread hands to the main thread.
///
/// The snapshot is much larger than a sessions summary; boxing it would add an
/// allocation to every tick and change the event's shape for the UI thread.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum UiEvent {
    Snapshot(Snapshot),
    Sessions(SessionsSummary),
    Providers(ProviderLists),
    Money(MoneyView),
    /// A pushed Sessions area view; the main thread sends it to the large
    /// window only.
    SessionsView(super::window::SessionsView),
}

/// The handles the Tauri side keeps.
pub(crate) struct Worker {
    pub engine: Sender<EngineCommand>,
    pub action: Sender<ActionCommand>,
    /// Present only while `readSessions` is on; the tray keeps it alive.
    pub sessions: Option<SessionsHandle>,
    /// The look-up thread runs whatever `lookUpPrices` says.
    pub lookup: Option<LookupHandle>,
    /// The provider thread; it runs whatever the two card switches say.
    pub providers: Option<ProvidersHandle>,
    /// The history folder the recorder opened at startup. Every reader uses
    /// this one; a changed setting takes effect after a restart.
    pub history: Result<seatswap_core::history::HistoryDir, String>,
    /// The plan inputs, regime and oldest reading of the last auto-burn
    /// decision; the Forecast page reads them through `GuiState`.
    pub forecast: Arc<Mutex<Option<KeptForecast>>>,
}

pub(crate) fn spawn(
    app: AppPaths,
    claude: ClaudePaths,
    autostart_args: Vec<String>,
    ui: Sender<UiEvent>,
    settings: Option<Settings>,
    reported_errors: Arc<ReportedErrors>,
    printed_warnings: Arc<Mutex<PrintedWarnings>>,
) -> Result<Worker, String> {
    let (engine_tx, engine_rx) = mpsc::channel::<EngineCommand>();
    let (action_tx, action_rx) = mpsc::channel::<ActionCommand>();

    // The one history folder this run uses: the recorder's and every read's.
    // Opening it here means a changed `hostName` or `historyDir` is not read
    // from a newer snapshot while the recorder still writes the old folder.
    // A changed history folder first gets this host's history. `gui::run` took
    // `gui.lock` before this, and no thread of this run writes the history
    // yet, so nothing writes the old folder while it is copied. A failed move
    // is reported and the folder opens anyway; the next start tries again.
    let history = match settings.as_ref() {
        Some(settings) => {
            match ops::bring_history_along(&app, settings, ops::Bringer::TrayApp) {
                Ok(warnings) => {
                    for warning in &warnings {
                        reported_errors.report(warning);
                    }
                }
                Err(error) => reported_errors.report(&error),
            }
            ops::open_history(&app, settings).map(|recorder| recorder.dir().clone())
        }
        None => Err(ops::history_unwritable(&"the settings are not loaded")),
    };
    let forecast = Arc::new(Mutex::new(None));

    let action_engine = engine_tx.clone();
    let engine_action = action_tx.clone();
    let engine_commands = engine_tx.clone();
    let engine_app = app.clone();
    let engine_claude = claude.clone();
    let engine_ui = ui.clone();
    let engine_history = history.clone();
    let engine_forecast = Arc::clone(&forecast);
    let engine_warnings = Arc::clone(&printed_warnings);
    thread::Builder::new()
        .name("seatswap-engine".to_string())
        .spawn(move || {
            EngineRuntime::new(
                engine_app,
                engine_claude,
                engine_ui,
                engine_action,
                engine_commands,
                engine_history,
                engine_forecast,
                engine_warnings,
            )
            .run(engine_rx)
        })
        .map_err(|error| format!("cannot start the engine thread: {error}"))?;

    let lookup = Some(lookup_rt::spawn(app.clone(), Api::new(), ui.clone())?);

    let providers = Some(providers_rt::spawn(
        app.clone(),
        history.clone(),
        Api::new(),
        ui.clone(),
        Arc::clone(&reported_errors),
        Arc::clone(&printed_warnings),
    )?);
    let providers_client = providers.as_ref().map(ProvidersHandle::client);

    let sessions = match settings.as_ref().filter(|settings| settings.read_sessions) {
        Some(settings) => Some(sessions_rt::spawn(
            app.clone(),
            claude.clone(),
            history.clone(),
            seatswap_core::harness::HarnessPaths::resolve()
                .map_err(|error| format!("the Harness folders cannot be found: {error}")),
            seatswap_core::paths::CodexPaths::resolve(settings.codex_home.as_deref())
                .map_err(|error| format!("the Codex folder cannot be found: {error}")),
            lookup.as_ref().map(LookupHandle::client),
            ui,
            engine_tx.clone(),
        )?),
        None => None,
    };
    let lookup_client = lookup.as_ref().map(LookupHandle::client);
    let latest = sessions
        .as_ref()
        .map(SessionsHandle::latest)
        .unwrap_or_else(|| Arc::new(Mutex::new(None)));

    let action_claude = claude;
    let action_history = history.clone();
    thread::Builder::new()
        .name("seatswap-action".to_string())
        .spawn(move || {
            ActionRuntime::new(
                app,
                action_claude,
                autostart_args,
                action_engine,
                action_history,
                reported_errors,
                printed_warnings,
            )
            .with_sessions(latest)
            .with_providers(providers_client)
            .with_lookup(lookup_client)
            .run(action_rx)
        })
        .map_err(|error| format!("cannot start the action thread: {error}"))?;

    Ok(Worker {
        engine: engine_tx,
        action: action_tx,
        sessions,
        lookup,
        providers,
        history,
        forecast,
    })
}

// ---------------------------------------------------------------------------
// Engine thread

/// Join two error texts without losing either.
pub(crate) fn combine(first: Option<String>, second: String) -> Option<String> {
    Some(match first {
        Some(first) => format!("{first}; {second}"),
        None => second,
    })
}

/// Everything [`build_snapshot`] needs. The accounts file and the live state
/// are results, not values, because their read errors belong in the snapshot.
pub(crate) struct SnapshotParts {
    pub views: Vec<AccountView>,
    pub accounts: Result<AccountsFile, String>,
    pub live: Result<LiveState, String>,
    pub engine_error: Option<String>,
    pub secrets_warning: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub pending: Option<PendingSwitch>,
    /// A one-shot message (the moved usage cache) for this snapshot only.
    pub notice: Option<String>,
    /// The config folder in effect, already written with `~` for the home folder.
    pub claude_config_dir: String,
}

/// The one place that turns the engine's data into a [`Snapshot`]. The engine
/// thread and the tests both use it, so a test cannot pass against a copy of
/// the real rules.
pub(crate) fn build_snapshot(parts: SnapshotParts) -> Snapshot {
    let mut error = parts.engine_error;
    let mut accounts = Vec::new();
    let mut provider_subscriptions = Vec::new();
    let (settings, live, warnings) = match parts.accounts {
        Ok(file) => {
            accounts = file
                .accounts
                .iter()
                .map(|account| AccountRow::of(account, &parts.views))
                .collect();
            provider_subscriptions = file
                .provider_subscriptions
                .iter()
                .map(ProviderSubscriptionRow::of)
                .collect();
            let live = match parts.live {
                Ok(state) => LiveView::from(state),
                Err(live_error) => {
                    error = combine(error, live_error);
                    LiveView::NoLogin
                }
            };
            (
                Some(file.settings.clone()),
                live,
                seatswap_core::engine::login_warnings(&parts.views),
            )
        }
        Err(store_error) => {
            error = combine(error, format!("cannot read accounts.json: {store_error}"));
            (None, LiveView::NoLogin, Vec::new())
        }
    };
    for view in &parts.views {
        if crate::format::parse_color(&view.color).is_none() {
            error = combine(
                error,
                format!(
                    "account {} has an invalid colour {:?}; set one in the account editor",
                    view.abbr, view.color
                ),
            );
        }
    }
    if let Some(notice) = parts.notice {
        error = combine(error, notice);
    }
    let active_id = parts
        .views
        .iter()
        .find(|view| view.is_active)
        .map(|view| view.id.clone());
    Snapshot {
        views: parts.views,
        accounts,
        provider_subscriptions,
        active_id,
        live,
        burn: None,
        warnings,
        pending: parts.pending,
        secrets_warning: parts.secrets_warning,
        last_poll_at: parts.last_poll_at,
        error,
        sessions: None,
        providers: Vec::new(),
        codex_logins: Vec::new(),
        settings,
        claude_config_dir: parts.claude_config_dir,
        claude_not_set_up: false,
    }
}

/// Open the store and the engine. A corrupt usage cache is moved aside and the
/// engine opened again; the returned notice says where the cache went and is
/// reported once for this run.
struct OpenedEngine {
    engine: Engine<Api>,
    secrets_warning: Option<String>,
    notice: Option<String>,
    history_error: Option<String>,
}

fn open_engine(
    app: &AppPaths,
    claude: &ClaudePaths,
    history: &Result<HistoryDir, String>,
) -> Result<OpenedEngine, String> {
    let (store, mut warning) = store::open_store(app.clone()).map_err(|error| error.to_string())?;
    let (engine, notice) = match Engine::new(claude.clone(), store, Api::new()) {
        Ok(engine) => (engine, None),
        Err(EngineError::CacheCorrupt { .. }) => {
            let moved = UsageCache::set_aside(app).map_err(|error| error.to_string())?;
            let (store, second_warning) =
                store::open_store(app.clone()).map_err(|error| error.to_string())?;
            warning = warning.or(second_warning);
            let engine = Engine::new(claude.clone(), store, Api::new())
                .map_err(|error| error.to_string())?;
            (
                engine,
                Some(format!(
                    "the usage cache was unreadable and was moved to {}; usage is fetched again",
                    moved.display()
                )),
            )
        }
        Err(error) => return Err(error.to_string()),
    };
    let (engine, history_error) = match history {
        Ok(dir) => (engine.with_recorder(Recorder::new(dir.clone())), None),
        Err(error) => (engine, Some(error.clone())),
    };
    Ok(OpenedEngine {
        engine,
        secrets_warning: warning,
        notice,
        history_error,
    })
}

struct EngineRuntime {
    app: AppPaths,
    claude: ClaudePaths,
    ui: Sender<UiEvent>,
    engine: Option<Engine<Api>>,
    /// The history folder opened at startup, shared with every other reader.
    history: Result<HistoryDir, String>,
    secrets_warning: Option<String>,
    /// The engine-wide error of the last open/poll attempt.
    open_error: Option<String>,
    /// The engine thread's history state: why the history cannot be opened,
    /// or the error of the last failed write. Cleared by the next successful
    /// `Live` or heartbeat write; `publish()` never touches it.
    history_error: Option<String>,
    /// The startup `Live` event has been written.
    live_written: bool,
    /// A one-shot message (the moved usage cache) for the next snapshot.
    notice: Option<String>,
    snapshot: Snapshot,
    baseline: ClockBaseline,
    burn: BurnRuntime,
    /// The last auto-burn plan inputs, regime and oldest reading, shared with
    /// `GuiState` for the Forecast page.
    forecast: Arc<Mutex<Option<KeptForecast>>>,
    /// The one set of history read warnings this run has printed.
    printed_warnings: Arc<Mutex<PrintedWarnings>>,
    /// The sessions thread's busy sessions on a thinking-tied model. It is 0
    /// when the sessions thread cannot tell (with `readSessions` off): nothing
    /// is held back then (spec §2.12).
    tied_busy: u32,
    /// The running tied sessions' answers with thinking, minute by minute. It is
    /// emptied together with `tied_busy`.
    thinking: Vec<(DateTime<Utc>, u32)>,
    /// The action thread, asked to exchange the shared looks and settings.
    action: Sender<ActionCommand>,
    /// When that was asked last; `None` until the first ask at start.
    share_asked_at: Option<DateTime<Utc>>,
}

/// The wall and monotonic clocks at one moment. Wake detection compares two of
/// these, so the baseline is updated after every `recv_timeout` result.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClockBaseline {
    pub wall: DateTime<Utc>,
    pub monotonic: Instant,
}

impl ClockBaseline {
    fn now() -> ClockBaseline {
        ClockBaseline {
            wall: Utc::now(),
            monotonic: Instant::now(),
        }
    }
}

impl EngineRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        app: AppPaths,
        claude: ClaudePaths,
        ui: Sender<UiEvent>,
        action: Sender<ActionCommand>,
        engine: Sender<EngineCommand>,
        history: Result<HistoryDir, String>,
        forecast: Arc<Mutex<Option<KeptForecast>>>,
        printed_warnings: Arc<Mutex<PrintedWarnings>>,
    ) -> EngineRuntime {
        let mut runtime = EngineRuntime {
            app,
            claude,
            ui,
            engine: None,
            history,
            secrets_warning: None,
            open_error: None,
            history_error: None,
            live_written: false,
            notice: None,
            snapshot: Snapshot::empty(None, None, None),
            baseline: ClockBaseline::now(),
            burn: BurnRuntime::new(action.clone(), engine),
            forecast,
            printed_warnings,
            tied_busy: 0,
            thinking: Vec::new(),
            action,
            share_asked_at: None,
        };
        runtime.open();
        // Show the real settings even when the engine could not open.
        let views = runtime
            .engine
            .as_ref()
            .and_then(|engine| engine.views(Utc::now()).ok())
            .unwrap_or_default();
        runtime.capture(views, None);
        runtime
    }

    fn run(mut self, commands: Receiver<EngineCommand>) {
        // The first snapshot is the one built above; it carries a one-shot
        // notice (the moved usage cache) and the settings read at startup.
        self.send();
        self.ask_share(Utc::now());
        let mut deadline = TickDeadline::new(Instant::now());
        loop {
            let wait = deadline.wait(Instant::now());
            match commands.recv_timeout(wait) {
                Ok(command) => {
                    // A command can arrive after any gap, including one that
                    // hid a suspend; check and update the baseline every time.
                    let (baseline, wake) = wake_step(self.baseline, ClockBaseline::now());
                    self.baseline = baseline;
                    let now = Instant::now();
                    let due = deadline.due(now);
                    if !matches!(&command, EngineCommand::Shutdown) && (wake || due) {
                        self.poll(if wake {
                            PollReason::Wake
                        } else {
                            PollReason::Timer
                        });
                    }
                    if due {
                        deadline.advance(now);
                    }
                    match command {
                        EngineCommand::Poll(reason) => self.poll(reason),
                        EngineCommand::Publish => self.publish(),
                        EngineCommand::Feed => self.apply_feed(),
                        EngineCommand::ModelsInUse {
                            models,
                            tied_busy,
                            thinking,
                        } => {
                            self.tied_busy = if models.is_some() { tied_busy } else { 0 };
                            self.thinking = if models.is_some() {
                                thinking
                            } else {
                                Vec::new()
                            };
                            let now = Utc::now();
                            let views = match self.engine.as_mut() {
                                Some(engine) => {
                                    engine.set_models_in_use(models);
                                    match engine.views(now) {
                                        Ok(views) => Some(views),
                                        Err(error) => {
                                            self.open_error = Some(error.to_string());
                                            None
                                        }
                                    }
                                }
                                None => None,
                            }
                            .unwrap_or_else(|| self.snapshot.views.clone());
                            self.capture(views, self.snapshot.last_poll_at);
                            self.send();
                        }
                        EngineCommand::HistoryChanged => {
                            self.burn.force_learn();
                            self.publish();
                        }
                        EngineCommand::Shutdown => return,
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let (baseline, wake) = wake_step(self.baseline, ClockBaseline::now());
                    self.baseline = baseline;
                    self.poll(if wake {
                        PollReason::Wake
                    } else {
                        PollReason::Timer
                    });
                    deadline.advance(Instant::now());
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Open the store and the engine. False when this failed; the message is in
    /// `open_error` and reaches the next snapshot.
    fn open(&mut self) -> bool {
        match open_engine(&self.app, &self.claude, &self.history) {
            Ok(opened) => {
                self.engine = Some(opened.engine);
                self.history_error = opened.history_error;
                self.secrets_warning = opened
                    .secrets_warning
                    .or_else(|| self.secrets_warning.clone());
                if let Some(notice) = opened.notice {
                    self.notice = combine(self.notice.take(), notice);
                }
                self.open_error = None;
                self.write_live_once();
                true
            }
            Err(error) => {
                self.open_error = Some(error);
                false
            }
        }
    }

    /// `EventKind::Live{account}` once, after the first successful open with a
    /// recorder. An unreadable live state is tried again on the next tick.
    fn write_live_once(&mut self) {
        if self.live_written {
            return;
        }
        let now = Utc::now();
        let account = match store::load_accounts(&self.app) {
            Ok(file) => match switcher::live_state(&self.claude, &file) {
                Ok(live) => ops::live_account_ref(&live, &file),
                Err(_) => return,
            },
            Err(_) => return,
        };
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let Some(recorder) = engine.recorder() else {
            return;
        };
        match recorder.event(now, EventKind::Live { account }) {
            Ok(()) => {
                self.live_written = true;
                self.history_error = None;
            }
            Err(error) => self.history_error = Some(ops::history_unwritable(&error)),
        }
    }

    /// The engine thread's heartbeat: one `up` line per poll when it has a
    /// recorder. A failed write is the history state; a successful one clears it.
    fn heartbeat(&mut self, now: DateTime<Utc>) {
        let result = self
            .engine
            .as_mut()
            .and_then(|engine| engine.recorder().map(|recorder| recorder.up(now)));
        match result {
            Some(Ok(())) => self.history_error = None,
            Some(Err(error)) => self.history_error = Some(ops::history_unwritable(&error)),
            None => {}
        }
    }

    /// Ask the action thread to exchange the shared looks and settings with the
    /// other hosts, at start and then at most every [`SHARE_EVERY_MIN`]
    /// minutes. The exchange runs on the action thread, where every edit and
    /// settings save runs, so it never lands between an edit and its event.
    fn ask_share(&mut self, now: DateTime<Utc>) {
        if share_due(self.share_asked_at, now) {
            self.share_asked_at = Some(now);
            let _ = self.action.send(ActionCommand::Share);
        }
    }

    /// The recorder's read warnings (spec §1.2): a torn last line, a line this
    /// version does not know. The app prints each one once; core never prints.
    fn print_history_warnings(&mut self) {
        let warnings = self
            .engine
            .as_mut()
            .and_then(|engine| engine.recorder())
            .map(|recorder| recorder.take_warnings());
        if let Some(warnings) = warnings {
            super::lock(&self.printed_warnings).print(Some("history"), &warnings);
        }
    }

    fn poll(&mut self, reason: PollReason) {
        if self.engine.is_none() && !self.open() {
            self.capture(Vec::new(), self.snapshot.last_poll_at);
            self.send();
            return;
        }
        self.write_live_once();
        let now = Utc::now();
        let result = match self.engine.as_mut() {
            Some(engine) => engine.poll(reason, now),
            None => return,
        };
        self.print_history_warnings();
        // The heartbeat belongs to the engine thread: it runs with
        // readSessions off too. Its error is the thread's history state, not
        // the engine's open error, so `publish()` cannot clear it.
        self.heartbeat(now);
        self.ask_share(now);
        match result {
            Ok(views) => {
                self.open_error = None;
                self.capture(views, Some(now));
            }
            Err(error) => {
                self.open_error = Some(error.to_string());
                let views = self.snapshot.views.clone();
                self.capture(views, self.snapshot.last_poll_at);
            }
        }
        self.send();
    }

    /// Read the status line's feed and merge it into the cache. A read or apply
    /// error is the engine thread's open error, as a failed poll is; the
    /// snapshot is published either way.
    fn apply_feed(&mut self) {
        if self.engine.is_none() && !self.open() {
            self.capture(Vec::new(), self.snapshot.last_poll_at);
            self.send();
            return;
        }
        let now = Utc::now();
        let result = match feed::read(&self.app) {
            Ok(Some(feed)) => match self.engine.as_mut() {
                Some(engine) => engine
                    .apply_feed(&feed, now)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                None => Ok(()),
            },
            Ok(None) => Ok(()),
            Err(error) => Err(format!(
                "cannot read {}: {error}",
                self.app.feed_file().display()
            )),
        };
        match result {
            Ok(()) => {
                self.open_error = None;
                let views = match self.engine.as_ref() {
                    Some(engine) => match engine.views(now) {
                        Ok(views) => views,
                        Err(error) => {
                            self.open_error = Some(error.to_string());
                            self.snapshot.views.clone()
                        }
                    },
                    None => self.snapshot.views.clone(),
                };
                self.capture(views, self.snapshot.last_poll_at);
            }
            Err(error) => {
                self.open_error = Some(error);
                self.capture(self.snapshot.views.clone(), self.snapshot.last_poll_at);
            }
        }
        self.send();
    }

    fn publish(&mut self) {
        if self.engine.is_none() {
            self.open();
        }
        let now = Utc::now();
        let (views, last_poll_at) = match self.engine.as_ref() {
            Some(engine) => match engine.views(now) {
                Ok(views) => {
                    self.open_error = None;
                    (views, self.snapshot.last_poll_at)
                }
                Err(error) => {
                    self.open_error = Some(error.to_string());
                    (self.snapshot.views.clone(), self.snapshot.last_poll_at)
                }
            },
            None => (self.snapshot.views.clone(), self.snapshot.last_poll_at),
        };
        self.capture(views, last_poll_at);
        self.send();
    }

    fn capture(&mut self, views: Vec<AccountView>, last_poll_at: Option<DateTime<Utc>>) {
        let accounts = store::load_accounts(&self.app).map_err(|error| error.to_string());
        let live = match &accounts {
            Ok(file) => switcher::live_state(&self.claude, file).map_err(|error| error.to_string()),
            Err(_) => Err(
                "the live login was not checked because accounts.json could not be read"
                    .to_string(),
            ),
        };
        let mut engine_error = self.open_error.clone();
        if let Some(history_error) = &self.history_error {
            engine_error = combine(engine_error, history_error.clone());
        }
        let accounts_for_burn = accounts.as_ref().ok().map(|file| file.accounts.clone());
        let claude_config_dir = with_tilde(
            &self.claude.config_dir.to_string_lossy(),
            self.claude.home(),
        );
        self.snapshot = build_snapshot(SnapshotParts {
            views,
            accounts,
            live,
            engine_error,
            secrets_warning: self.secrets_warning.clone(),
            last_poll_at,
            pending: None,
            notice: self.notice.take(),
            claude_config_dir,
        });
        self.update_burn(accounts_for_burn);
    }

    /// Run auto-burn after the snapshot is built: learn, decide, send. Its error
    /// joins the snapshot's error; a warning is printed once.
    fn update_burn(&mut self, accounts: Option<Vec<Account>>) {
        let (Some(accounts), Some(settings)) = (accounts, self.snapshot.settings.as_ref().cloned())
        else {
            return;
        };
        let now = Utc::now();
        let history = self
            .engine
            .as_mut()
            .and_then(|engine| engine.recorder())
            .map(|recorder| recorder.dir());
        let tick = BurnTick {
            app: &self.app,
            history,
            views: &self.snapshot.views,
            accounts: &accounts,
            settings: &settings,
            active: self.snapshot.active_id.as_deref(),
            now,
            offset: *Local::now().offset(),
            tied_busy: self.tied_busy,
            thinking: &self.thinking,
        };
        let report = self.burn.after_snapshot(&tick);
        *super::lock(&self.forecast) = report.forecast;
        self.snapshot.burn = report.view;
        if let Some(error) = report.error {
            self.snapshot.error = combine(self.snapshot.error.take(), error);
        }
        super::lock(&self.printed_warnings).print(Some("history"), &report.warnings);
        if let Some(text) = report.notification {
            if settings.notify_on_switch {
                notify::send_switch(&self.app, &text);
            }
        }
    }

    fn send(&self) {
        let _ = self.ui.send(UiEvent::Snapshot(self.snapshot.clone()));
    }
}

/// Whether the shared looks and settings are due again: never asked yet, or
/// asked at least [`SHARE_EVERY_MIN`] minutes before `now`.
fn share_due(asked_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    asked_at.is_none_or(|at| {
        now.signed_duration_since(at) >= chrono::Duration::minutes(SHARE_EVERY_MIN)
    })
}

/// True when the wall clock moved more than [`WAKE_THRESHOLD`] further than the
/// monotonic clock: the machine was asleep in between.
pub(crate) fn wake_detected(
    previous_wall: DateTime<Utc>,
    previous_monotonic: Instant,
    now_wall: DateTime<Utc>,
    now_monotonic: Instant,
) -> bool {
    let wall_ms = now_wall
        .signed_duration_since(previous_wall)
        .num_milliseconds();
    let monotonic_ms = i64::try_from(now_monotonic.duration_since(previous_monotonic).as_millis())
        .unwrap_or(i64::MAX);
    wall_ms.saturating_sub(monotonic_ms)
        > i64::try_from(WAKE_THRESHOLD.as_millis()).unwrap_or(i64::MAX)
}

/// The engine loop's wake decision as one pure step: the next baseline is
/// `now`, and `wake` is true when the clocks disagree by more than
/// [`WAKE_THRESHOLD`]. Called after every command and every timeout.
pub(crate) fn wake_step(previous: ClockBaseline, now: ClockBaseline) -> (ClockBaseline, bool) {
    let wake = wake_detected(previous.wall, previous.monotonic, now.wall, now.monotonic);
    (now, wake)
}

// ---------------------------------------------------------------------------
// Action thread

/// A notification a successful switch sends, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchNotice {
    Switched(String),
    Error(String),
}

/// `notifyOnSwitch` followed by `false` is silence; a read failure sends the
/// switch itself *and* the error, because the switch happened and the person
/// must learn both. An unknown setting is never treated as `true`.
pub(crate) fn switch_notices(
    message: &str,
    notify_on_switch: Result<bool, String>,
) -> Vec<SwitchNotice> {
    match notify_on_switch {
        Ok(true) => vec![SwitchNotice::Switched(message.to_string())],
        Ok(false) => Vec::new(),
        Err(error) => vec![
            SwitchNotice::Switched(message.to_string()),
            SwitchNotice::Error(format!(
                "the switch notification setting could not be read: {error}"
            )),
        ],
    }
}

struct ActionRuntime {
    app: AppPaths,
    claude: ClaudePaths,
    autostart_args: Vec<String>,
    engine: Sender<EngineCommand>,
    store: Option<Store>,
    /// The recorder, or the sentence that says why there is none.
    history: Result<Recorder, String>,
    latest_sessions: Arc<Mutex<Option<SessionsSummary>>>,
    /// The provider thread's commands; every settings change tells it.
    providers: Option<ProvidersClient>,
    /// The look-up thread's commands; every settings change tells it, so a
    /// new currency gets its rate at once.
    lookup: Option<LookupClient>,
    /// The last account seen live, so an external switch has a `from`.
    live: Option<AccountRef>,
    /// The one set of history read warnings this run has printed, so a torn
    /// line is not printed again at every live-account change.
    printed_warnings: Arc<Mutex<PrintedWarnings>>,
    /// The one app error reporter for a failed history write.
    reported_errors: Arc<ReportedErrors>,
}

impl ActionRuntime {
    fn new(
        app: AppPaths,
        claude: ClaudePaths,
        autostart_args: Vec<String>,
        engine: Sender<EngineCommand>,
        history: Result<HistoryDir, String>,
        reported_errors: Arc<ReportedErrors>,
        printed_warnings: Arc<Mutex<PrintedWarnings>>,
    ) -> ActionRuntime {
        let loaded = store::load_accounts(&app);
        let history = history.map(Recorder::new);
        let live = loaded.ok().and_then(|file| {
            switcher::live_state(&claude, &file)
                .ok()
                .and_then(|state| ops::live_account_ref(&state, &file))
        });
        ActionRuntime {
            app,
            claude,
            autostart_args,
            engine,
            store: None,
            history,
            latest_sessions: Arc::new(Mutex::new(None)),
            providers: None,
            lookup: None,
            live,
            printed_warnings,
            reported_errors,
        }
    }

    fn with_sessions(mut self, latest: Arc<Mutex<Option<SessionsSummary>>>) -> ActionRuntime {
        self.latest_sessions = latest;
        self
    }

    fn with_providers(mut self, providers: Option<ProvidersClient>) -> ActionRuntime {
        self.providers = providers;
        self
    }

    fn with_lookup(mut self, lookup: Option<LookupClient>) -> ActionRuntime {
        self.lookup = lookup;
        self
    }

    /// The settings changed, by a save here or by a shared setting from
    /// another host: the threads that read them act at once.
    fn settings_changed(&self) {
        if let Some(providers) = &self.providers {
            let _ = providers.settings_saved();
        }
        if let Some(lookup) = &self.lookup {
            let _ = lookup.settings_saved();
        }
    }

    fn recorder(&self) -> Result<&Recorder, String> {
        self.history.as_ref().map_err(Clone::clone)
    }

    fn session_counts(&self) -> Option<SessionCounts> {
        let latest = match self.latest_sessions.lock() {
            Ok(latest) => latest,
            Err(poisoned) => poisoned.into_inner(),
        };
        latest.as_ref().map(|summary| SessionCounts {
            running: summary.running,
            busy: summary.busy,
            tied: Some(summary.tied),
        })
    }

    fn run(mut self, commands: Receiver<ActionCommand>) {
        for command in commands {
            match command {
                ActionCommand::Switch {
                    key,
                    unsaved,
                    label,
                    by,
                    reason,
                    reply,
                } => {
                    let result = self.switch(&key, unsaved, label, by, reason);
                    let _ = reply.send(result);
                }
                ActionCommand::SaveCurrent { label, reply } => {
                    let _ = reply.send(self.save_current(label));
                }
                ActionCommand::Edit {
                    key,
                    label,
                    abbr,
                    color,
                    auto_burn,
                    price,
                    reply,
                } => {
                    let _ = reply.send(self.edit(&key, label, abbr, color, auto_burn, price));
                }
                ActionCommand::EditSubscription {
                    account,
                    label,
                    abbr,
                    color,
                    plan,
                    price,
                    reply,
                } => {
                    let _ = reply
                        .send(self.edit_subscription(&account, label, abbr, color, plan, price));
                }
                ActionCommand::Remove { key, reply } => {
                    let _ = reply.send(self.remove(&key));
                }
                ActionCommand::SwitchCodex { account, reply } => {
                    let _ = reply.send(self.switch_codex(&account));
                }
                ActionCommand::RemoveCodexLogin { account, reply } => {
                    let _ = reply.send(self.remove_codex_login(&account));
                }
                ActionCommand::ConnectAgy { reply } => {
                    let _ = reply.send(self.connect_agy());
                }
                ActionCommand::SaveSetting {
                    field,
                    value,
                    reply,
                } => {
                    let _ = reply.send(self.save_setting(field, value));
                }
                ActionCommand::SyncLive => self.sync_live(),
                ActionCommand::PauseBurn { hours } => {
                    if let Err(error) = self.pause_burn(hours) {
                        notify::send_error(&self.app, &error);
                    }
                    let _ = self.engine.send(EngineCommand::HistoryChanged);
                }
                ActionCommand::Share => self.share(),
            }
        }
    }

    fn ensure_store(&mut self) -> Result<(), String> {
        if self.store.is_none() {
            let (store, _warning) =
                store::open_store(self.app.clone()).map_err(|e| e.to_string())?;
            self.store = Some(store);
        }
        Ok(())
    }

    fn store(&mut self) -> Result<&Store, String> {
        self.ensure_store()?;
        self.store
            .as_ref()
            .ok_or_else(|| "the store is not open".to_string())
    }

    /// The recorder for one action and, when there is none, the reason. The
    /// reason is reported even when the action's record was not attempted, so
    /// a history that cannot be opened is never silently dropped.
    fn history_for_action(&self) -> (Option<&Recorder>, Option<String>) {
        match &self.history {
            Ok(recorder) => (Some(recorder), None),
            Err(error) => (None, Some(error.clone())),
        }
    }

    fn switch(
        &mut self,
        key: &str,
        unsaved: UnsavedPolicy,
        label: Option<String>,
        by: By,
        reason: Option<String>,
    ) -> ActionReply {
        let policy = match (unsaved, label) {
            (UnsavedPolicy::Save { .. }, Some(label)) => UnsavedPolicy::Save { label: Some(label) },
            (policy, _) => policy,
        };
        let claude = self.claude.clone();
        let result = match self.store() {
            Ok(store) => switcher::switch_to(&claude, store, key, policy, Utc::now()),
            Err(error) => return ActionReply::Failed(error),
        };
        match result {
            Ok(outcome) => {
                if outcome.saved_new.is_some() {
                    self.share();
                }
                let message = match self.store() {
                    Ok(store) => match ops::switch_message(store, &outcome.to) {
                        Ok(message) => message,
                        Err(error) => ops::switched_without_list(&error),
                    },
                    Err(error) => ops::switched_without_list(&error),
                };
                let now = Utc::now();
                let counts = self.session_counts();
                let loaded = self
                    .store()
                    .and_then(|store| store.load().map_err(|error| error.to_string()));
                let mut to_ref = None;
                let mut history_error = None;
                match loaded {
                    Ok(file) => match file.find(&outcome.to) {
                        Some(account) => {
                            let target = AccountRef::of(account);
                            to_ref = Some(target.clone());
                            let from = outcome
                                .from
                                .as_deref()
                                .and_then(|id| file.find(id))
                                .map(AccountRef::of);
                            let saved_new =
                                outcome.saved_new.as_deref().and_then(|id| file.find(id));
                            match self.recorder() {
                                Ok(recorder) => {
                                    if let Err(error) = ops::record_switch(
                                        recorder, from, target, saved_new, by, reason, None,
                                        counts, now,
                                    ) {
                                        history_error = Some(error.to_string());
                                    } else if let Some(change) = &outcome.plan_change {
                                        if let Err(error) =
                                            ops::record_plan_change(recorder, change, now)
                                        {
                                            history_error = Some(error.to_string());
                                        }
                                    }
                                }
                                Err(error) => history_error = Some(error),
                            }
                        }
                        None => {
                            history_error = Some(format!("the store has no account {}", outcome.to))
                        }
                    },
                    Err(error) => history_error = Some(error),
                }
                if to_ref.is_some() && history_error.is_none() {
                    // The event is written: the next learn must see the switch
                    // (its hold and its cost) before the next snapshot decides.
                    let _ = self.engine.send(EngineCommand::HistoryChanged);
                }
                if let Some(target) = to_ref {
                    self.live = Some(target);
                }
                let reply_message = match history_error {
                    Some(error) => format!("{message} (not written to the history: {error})"),
                    None => message.clone(),
                };
                let notify_on_switch = match self
                    .store()
                    .and_then(|store| store.load().map_err(|e| e.to_string()))
                {
                    Ok(file) => Ok(file.settings.notify_on_switch),
                    Err(error) => Err(error),
                };
                for notice in switch_notices(&message, notify_on_switch) {
                    match notice {
                        // An automatic switch has its own text (spec §2.5); the
                        // engine thread sends it once the switch has landed.
                        SwitchNotice::Switched(_) if by == By::Auto => {}
                        SwitchNotice::Switched(text) => notify::send_switch(&self.app, &text),
                        SwitchNotice::Error(text) => notify::send_error(&self.app, &text),
                    }
                }
                let _ = self
                    .engine
                    .send(EngineCommand::Poll(PollReason::AfterSwitch));
                ActionReply::Switched(reply_message)
            }
            Err(SwitchError::CurrentNotSaved { email }) => match self.pending_for(key, email) {
                Ok(pending) => ActionReply::NotSaved(pending),
                Err(message) => {
                    notify::send_error(&self.app, &message);
                    ActionReply::Failed(message)
                }
            },
            Err(error) => {
                let message = error.to_string();
                // A switch error is always shown, whatever notifyOnSwitch says.
                notify::send_error(&self.app, &message);
                ActionReply::Failed(message)
            }
        }
    }

    fn pending_for(&mut self, key: &str, email: Option<String>) -> Result<PendingSwitch, String> {
        let file = self.store()?.load().map_err(|error| error.to_string())?;
        let live = switcher::live_state(&self.claude, &file).map_err(|error| error.to_string())?;
        pending_switch(&file, &live, key, email)
    }

    /// Pause auto-burn for `hours`, or resume with `None`, as one history
    /// event (spec §2.5). A zero or negative pause is refused, never clamped.
    fn pause_burn(&mut self, hours: Option<i64>) -> Result<(), String> {
        let now = Utc::now();
        let paused_until = match hours {
            Some(hours) if hours <= 0 => {
                return Err(format!("a pause must be at least one hour, got {hours}"))
            }
            Some(hours) => Some(now + chrono::Duration::hours(hours)),
            None => None,
        };
        let on = self
            .store()?
            .load()
            .map_err(|error| error.to_string())?
            .settings
            .auto_burn;
        self.record(EventKind::AutoBurn { on, paused_until })
    }

    /// Write one event to the history. A pause or resume and a settings save
    /// that turns auto-burn on or off both come here, each with its own event.
    fn record(&mut self, kind: EventKind) -> Result<(), String> {
        let recorder = self.recorder()?;
        recorder
            .event(Utc::now(), kind)
            .map_err(|error| ops::history_unwritable(&error))
    }

    /// Exchange the shared looks and settings with the other hosts (spec §5):
    /// seed what no host shared yet and apply the newest values. Asked by the
    /// engine thread at start and every few minutes, and run after an account
    /// was added so a login of a known account takes its shared look at once.
    /// Without a history folder nothing is shared; its error is shown already.
    /// A change is published; the warnings are printed once, an error is
    /// reported once.
    fn share(&mut self) {
        if let Err(error) = self.ensure_store() {
            self.reported_errors.report(&error);
            return;
        }
        let (Some(store), Ok(recorder)) = (self.store.as_ref(), self.history.as_ref()) else {
            return;
        };
        match shared::sync(store, recorder, Utc::now()) {
            Ok(applied) => {
                super::lock(&self.printed_warnings).print(Some("history"), &applied.warnings);
                if !applied.settings.is_empty() {
                    self.settings_changed();
                }
                if applied.changed() {
                    let _ = self.engine.send(EngineCommand::Publish);
                }
            }
            Err(error) => self.reported_errors.report(&error.to_string()),
        }
    }

    fn save_current(&mut self, label: Option<String>) -> ActionReply {
        if let Err(error) = self.ensure_store() {
            return ActionReply::Failed(error);
        }
        let claude = self.claude.clone();
        let Some(store) = self.store.as_ref() else {
            return ActionReply::Failed("the store is not open".to_string());
        };
        let (history, open_error) = self.history_for_action();
        let result = ops::save_live_login(&claude, store, history, label, Utc::now());
        match result {
            Ok((_, history_error)) => {
                if let Some(error) = history_error.or(open_error) {
                    self.reported_errors.report(&error);
                }
                self.share();
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error),
        }
    }

    fn edit(
        &mut self,
        key: &str,
        label: Option<String>,
        abbr: Option<String>,
        color: Option<String>,
        auto_burn: Option<bool>,
        price: Option<Option<f64>>,
    ) -> ActionReply {
        let result = match self.store() {
            Ok(store) => switcher::edit_account(
                store,
                key,
                label.as_deref(),
                abbr.as_deref(),
                color.as_deref(),
                auto_burn,
                price,
            ),
            Err(error) => return ActionReply::Failed(error),
        };
        match result {
            Ok(edited) => {
                let (history, open_error) = self.history_for_action();
                if let Some(error) = ops::record_edit(history, &edited, Utc::now()).or(open_error) {
                    self.reported_errors.report(&error);
                }
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn edit_subscription(
        &mut self,
        account: &AccountRef,
        label: Option<String>,
        abbr: Option<String>,
        color: Option<String>,
        plan: Option<Option<String>>,
        price: Option<Option<f64>>,
    ) -> ActionReply {
        let result = switcher::edit_subscription(
            &self.app,
            account,
            label.as_deref(),
            abbr.as_deref(),
            color.as_deref(),
            plan.as_ref().map(|value| value.as_deref()),
            price,
        );
        match result {
            Ok(edited) => {
                let now = Utc::now();
                let (history, open_error) = self.history_for_action();
                let mut error = ops::record_edit(history, &edited, now);
                if let (Some(change), Some(recorder)) = (&edited.plan_change, history) {
                    if let Err(plan_error) = ops::record_plan_change(recorder, change, now) {
                        error.get_or_insert_with(|| ops::history_unwritable(&plan_error));
                    }
                }
                if let Some(error) = error.or(open_error) {
                    self.reported_errors.report(&error);
                }
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    fn remove(&mut self, key: &str) -> ActionReply {
        if let Err(error) = self.ensure_store() {
            return ActionReply::Failed(error);
        }
        let Some(store) = self.store.as_ref() else {
            return ActionReply::Failed("the store is not open".to_string());
        };
        let (history, open_error) = self.history_for_action();
        match ops::remove_account(store, key, history, Utc::now()) {
            Ok((_, history_error)) => {
                if let Some(error) = history_error.or(open_error) {
                    self.reported_errors.report(&error);
                }
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error),
        }
    }

    /// Codex's folder for an action: the `codexHome` setting, then the
    /// environment, resolved the one way the provider thread resolves it.
    fn codex_paths(&mut self) -> Result<CodexPaths, String> {
        let settings = self
            .store()?
            .load()
            .map_err(|error| error.to_string())?
            .settings;
        CodexPaths::resolve(settings.codex_home.as_deref()).map_err(|error| error.to_string())
    }

    /// Tell the provider thread that a provider's own files changed, so it polls
    /// that provider at once. A thread that cannot be reached is reported, never
    /// dropped.
    fn provider_changed(&self, provider: Provider) {
        if let Some(providers) = &self.providers {
            if let Err(text) = providers.changed(provider) {
                self.reported_errors.report(&text);
            }
        }
    }

    /// Switch Codex's live login to a saved one (R8). The one decision is
    /// [`ops::switch_codex`], shared with the CLI: the switch, the
    /// `providerSwitch` event and the note about running sessions. A history
    /// that cannot be written is reported and never fails the switch.
    fn switch_codex(&mut self, account: &AccountRef) -> ActionReply {
        if let Err(error) = self.ensure_store() {
            return ActionReply::Failed(error);
        }
        let paths = match self.codex_paths() {
            Ok(paths) => paths,
            Err(error) => return ActionReply::Failed(error),
        };
        let Some(store) = self.store.as_ref() else {
            return ActionReply::Failed("the store is not open".to_string());
        };
        let (history, open_error) = self.history_for_action();
        let result = ops::switch_codex(&self.app, &paths, store, history, account, Utc::now());
        match result {
            Ok(switched) => {
                let mut warnings = switched.warnings;
                if let Some(error) = open_error {
                    warnings.push(error);
                }
                for text in &warnings {
                    self.reported_errors.report(text);
                }
                self.provider_changed(Provider::Codex);
                ActionReply::Switched(switched.note)
            }
            Err(error) => ActionReply::Failed(error),
        }
    }

    /// Connect `agy` (spec §7.9): seatswap's `statusLine` in its settings, then
    /// a Google poll at once so the card says it is connected.
    fn connect_agy(&mut self) -> ActionReply {
        let agy = match providers_rt::real_agy() {
            Ok(agy) => agy,
            Err(error) => return ActionReply::Failed(error),
        };
        match providers::google::connect_agy(&agy) {
            Ok(()) => {
                self.provider_changed(Provider::Google);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    /// Delete seatswap's copy of a saved Codex login (R8); the subscription, its
    /// history and its price stay, and Codex's own login file is not touched.
    /// The live login's copy cannot be removed: the next poll would save it
    /// again, so the worker refuses with the sentence Settings shows.
    fn remove_codex_login(&mut self, account: &AccountRef) -> ActionReply {
        if let Err(error) = self.ensure_store() {
            return ActionReply::Failed(error);
        }
        let paths = match self.codex_paths() {
            Ok(paths) => paths,
            Err(error) => return ActionReply::Failed(error),
        };
        match providers::codex::live_account(&paths) {
            Ok(Some(live)) if &live == account => {
                return ActionReply::Failed(providers::codex::LIVE_LOGIN_REMOVE.to_string())
            }
            Ok(_) => {}
            // A keyring or API-key login means no saved login is live, so a
            // stored copy can be removed. Any other error still refuses with
            // that error: we cannot know which login is live.
            Err(providers::ProviderError::KeyringLogin)
            | Err(providers::ProviderError::ApiKeyLogin) => {}
            Err(error) => return ActionReply::Failed(error.to_string()),
        }
        let Some(store) = self.store.as_ref() else {
            return ActionReply::Failed("the store is not open".to_string());
        };
        match providers::codex::forget(&self.app, store.secrets(), account) {
            Ok(()) => {
                self.provider_changed(Provider::Codex);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    fn save_setting(&mut self, field: String, value: serde_json::Value) -> ActionReply {
        let home = self.claude.home.clone();
        let result = (|| -> Result<(Settings, Settings), String> {
            let store = self.store()?;
            let _lock = locks::acquire_app_lock(store.app_paths(), switcher::APP_LOCK_TIMEOUT)
                .map_err(|error| error.to_string())?;
            let mut file = store.load().map_err(|error| error.to_string())?;
            let previous = file.settings.clone();
            let settings = merge_setting(&previous, &field, value, &home)?;
            file.settings = settings.clone();
            store.save(&file).map_err(|error| error.to_string())?;
            Ok((previous, settings))
        })();
        let (previous, settings) = match result {
            Ok(saved) => saved,
            Err(error) => return ActionReply::Failed(error),
        };
        // The setting is saved; its history events come from one place, also
        // used when the pop-up changes `autoBurn`. Only `autoBurnSet` changes
        // what auto-burn learns.
        match settings_change_events(&previous, &settings) {
            Ok(events) => {
                for event in events {
                    let burn = matches!(event, EventKind::AutoBurnSet { .. });
                    match self.record(event) {
                        Ok(()) if burn => {
                            let _ = self.engine.send(EngineCommand::HistoryChanged);
                        }
                        Ok(()) => {}
                        Err(error) => self.reported_errors.report(&error),
                    }
                }
            }
            Err(error) => self.reported_errors.report(&error),
        }
        let warning = autostart::apply_autostart(settings.start_at_login, &self.autostart_args)
            .err()
            .map(|error| autostart_failure_message(&error));
        self.settings_changed();
        let _ = self.engine.send(EngineCommand::Publish);
        ActionReply::Saved(SavedSetting { settings, warning })
    }

    fn sync_live(&mut self) {
        let mirror = match self.store() {
            Ok(store) => match store.load() {
                Ok(file) => file.settings.mirror_live,
                Err(error) => {
                    eprintln!("seatswap: watcher: {error}");
                    false
                }
            },
            Err(error) => {
                eprintln!("seatswap: watcher: {error}");
                false
            }
        };
        let mut lock_busy = false;
        if mirror {
            let claude = self.claude.clone();
            let result = match self.store() {
                Ok(store) => Some(switcher::sync_live(&claude, store, None, Utc::now())),
                Err(_) => None,
            };
            match result {
                Some(Ok((
                    _,
                    Synced::Updated {
                        plan_change: Some(change),
                    },
                ))) => match self.recorder() {
                    Ok(recorder) => {
                        if let Err(error) = ops::record_plan_change(recorder, &change, Utc::now()) {
                            eprintln!("seatswap: watcher: {}", ops::history_unwritable(&error));
                        }
                    }
                    Err(error) => eprintln!("seatswap: watcher: {error}"),
                },
                Some(Ok(_)) | None => {}
                // Claude Code is mid-renewal; its next change calls us again.
                Some(Err(SwitchError::Lock(LockError::Busy { .. }))) => lock_busy = true,
                Some(Err(error)) => eprintln!("seatswap: watcher: {error}"),
            }
        }
        if !lock_busy {
            let loaded = self
                .store()
                .and_then(|store| store.load().map_err(|error| error.to_string()));
            match loaded {
                Ok(file) => match switcher::live_state(&self.claude, &file) {
                    Ok(state) => {
                        let current = ops::live_account_ref(&state, &file);
                        if current != self.live {
                            if let Some(to) = &current {
                                let counts = self.session_counts();
                                match &self.history {
                                    Ok(recorder) => match record_external_switch(
                                        recorder,
                                        &self.printed_warnings,
                                        self.live.clone(),
                                        to,
                                        counts,
                                        Utc::now(),
                                    ) {
                                        Ok(RecentSwitch::Already { by: By::Agent, cwd }) => {
                                            if let Some(account) =
                                                file.find_by_identity(&to.acc, &to.org)
                                            {
                                                notify::send_switch(
                                                    &self.app,
                                                    &agent_switch_message(
                                                        &account.abbr,
                                                        cwd.as_deref(),
                                                        self.claude.home(),
                                                    ),
                                                );
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            eprintln!("seatswap: watcher: {error}")
                                        }
                                    },
                                    Err(error) => eprintln!("seatswap: watcher: {error}"),
                                }
                            }
                            self.live = current;
                        }
                    }
                    Err(error) => eprintln!("seatswap: watcher: {error}"),
                },
                Err(error) => eprintln!("seatswap: watcher: {error}"),
            }
        }
        let _ = self.engine.send(EngineCommand::Publish);
    }
}

/// The settings with one field replaced, checked as a whole by
/// [`Settings::with_setting`]: `field` must name a key of the settings' JSON
/// object; a bad value is the field's name followed by serde's text, and an
/// invalid value is [`Settings::validate`]'s error. A `~` path typed into one
/// of the path settings starts at the home folder (`format::expand_tilde`:
/// `~/`, and `~\` on Windows).
fn merge_setting(
    settings: &Settings,
    field: &str,
    value: serde_json::Value,
    home: &Path,
) -> Result<Settings, String> {
    let value = match value {
        serde_json::Value::String(text)
            if matches!(field, "claudeConfigDir" | "historyDir" | "codexHome") =>
        {
            // `expand_tilde` owns which spellings name the home folder (`~\`
            // too on Windows, as the page shows it); any other text stays.
            let expanded = expand_tilde(&text, home);
            let text = expanded
                .into_os_string()
                .into_string()
                .map_err(|_| format!("{field}: the home folder is not valid UTF-8"))?;
            serde_json::Value::String(text)
        }
        other => other,
    };
    settings
        .with_setting(field, value)
        .map_err(|error| error.to_string())
}

/// The history events a settings save writes, none when the save changes none
/// of the settings the history records. Turning auto-burn on or off is
/// `AutoBurnSet`, never a pause or resume (spec §2.5); a changed shared setting
/// is `SettingSet` ([`shared::setting_events`], spec §5).
fn settings_change_events(previous: &Settings, next: &Settings) -> Result<Vec<EventKind>, String> {
    let mut events = Vec::new();
    if previous.auto_burn != next.auto_burn {
        events.push(EventKind::AutoBurnSet { on: next.auto_burn });
    }
    events.extend(shared::setting_events(previous, next).map_err(|error| error.to_string())?);
    Ok(events)
}

/// The stderr lines this run of the app has already printed: history read
/// warnings and a found provider's error. A month file is read again at every
/// live-account change, so its torn lines would otherwise be printed again each
/// time.
#[derive(Debug, Default)]
pub(crate) struct PrintedWarnings {
    seen: BTreeSet<String>,
}

impl PrintedWarnings {
    pub(crate) fn new() -> PrintedWarnings {
        PrintedWarnings {
            seen: BTreeSet::new(),
        }
    }

    /// Print each text this run has not printed yet with its label: history
    /// warnings read `seatswap: history: <warning>` (spec §1.2), and a found
    /// provider's error reads `seatswap: <error>` as `ReportedErrors::report`
    /// prints it with the card on. `None` is no label. Each line is remembered,
    /// so the next call with the same line prints nothing.
    pub(crate) fn print(&mut self, label: Option<&str>, texts: &[String]) {
        for text in texts {
            let line = match label {
                Some(label) => format!("seatswap: {label}: {text}"),
                None => format!("seatswap: {text}"),
            };
            if self.seen.insert(line.clone()) {
                eprintln!("{line}");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.seen.len()
    }

    /// The lines this run printed, for the tests that check a label.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.seen.iter().cloned().collect()
    }
}

/// What [`record_external_switch`] found: it wrote a new external switch, or a
/// switch to the same account is already in this host's last 15 s.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecentSwitch {
    Written,
    Already { by: By, cwd: Option<String> },
}

/// Write `Switch{by: External}` for a live account, unless this host's events
/// already hold a switch to the same account in the last 15 s; that event's
/// `by` and `cwd` are returned so an agent's switch can be announced. The
/// month file's read warnings are printed once per run through `printed`.
fn record_external_switch(
    recorder: &Recorder,
    printed: &Mutex<PrintedWarnings>,
    from: Option<AccountRef>,
    to: &AccountRef,
    sessions: Option<SessionCounts>,
    now: DateTime<Utc>,
) -> Result<RecentSwitch, HistoryError> {
    let read = recorder.dir().read::<Event>(
        &Range {
            from: now - chrono::Duration::seconds(15),
            to: now + chrono::Duration::seconds(1),
        },
        &Hosts::One(recorder.dir().host().to_string()),
    )?;
    super::lock(printed).print(Some("history"), &read.warnings);
    let already = read
        .records
        .iter()
        .rev()
        .find_map(|hosted| match &hosted.record.kind {
            EventKind::Switch {
                to: existing,
                by,
                cwd,
                ..
            } if existing == to => Some((*by, cwd.clone())),
            _ => None,
        });
    if let Some((by, cwd)) = already {
        return Ok(RecentSwitch::Already { by, cwd });
    }
    ops::record_switch(
        recorder,
        from,
        to.clone(),
        None,
        By::External,
        None,
        None,
        sessions,
        now,
    )?;
    Ok(RecentSwitch::Written)
}

/// The tray app announces every switch an agent made (spec §6.2), whatever
/// `notifyOnSwitch` says.
fn agent_switch_message(abbr: &str, cwd: Option<&str>, home: &Path) -> String {
    match cwd {
        Some(cwd) => format!(
            "An agent switched to {abbr} (from {}).",
            with_tilde(cwd, home)
        ),
        None => format!("An agent switched to {abbr}."),
    }
}

/// A switch waiting for "save or discard?" only when the target key names a
/// real account; an unknown key is the store's error, never a pending carrying
/// the raw key.
fn pending_switch(
    file: &AccountsFile,
    live: &LiveState,
    key: &str,
    email: Option<String>,
) -> Result<PendingSwitch, String> {
    let account = file
        .find_by_key(key)
        .ok_or_else(|| format!("unknown account {key}"))?;
    let org_name = match live {
        LiveState::Unknown { org_name, .. } => org_name.clone(),
        _ => None,
    };
    Ok(PendingSwitch {
        target_id: account.id.clone(),
        target_label: account.label.clone(),
        email,
        org_name,
    })
}

/// The reply when accounts.json was saved but the autostart entry could not be
/// written. It must say that the settings are saved, not that the save failed.
fn autostart_failure_message(error: &str) -> String {
    format!("settings saved; start at login could not be set: {error}")
}

/// A `Remote` used only by the worker tests.
#[cfg(test)]
#[derive(Clone)]
struct FakeRemote {
    usage: seatswap_core::api::Usage,
}

#[cfg(test)]
impl Remote for FakeRemote {
    fn usage(&self, _access_token: &str) -> Result<seatswap_core::api::Usage, ApiError> {
        Ok(self.usage.clone())
    }

    fn refresh(
        &self,
        _login: &Login,
        _expected: (&str, &str),
        _now_ms: i64,
    ) -> Result<Login, ApiError> {
        Err(ApiError::Decode("the fake does not refresh".to_string()))
    }
}

/// Gather the real inputs for [`build_snapshot`] from a test engine; all the
/// snapshot rules stay in the one function the engine thread uses too.
#[cfg(test)]
fn snapshot_from_engine<R: Remote>(
    engine: &Engine<R>,
    views: Vec<AccountView>,
    secrets_warning: Option<String>,
    last_poll_at: Option<DateTime<Utc>>,
) -> Snapshot {
    let accounts = engine.store().load().map_err(|error| error.to_string());
    let live = match &accounts {
        Ok(file) => {
            switcher::live_state(engine.claude_paths(), file).map_err(|error| error.to_string())
        }
        Err(_) => Err("accounts.json could not be read".to_string()),
    };
    build_snapshot(SnapshotParts {
        views,
        accounts,
        live,
        engine_error: None,
        secrets_warning,
        last_poll_at,
        pending: None,
        notice: None,
        claude_config_dir: with_tilde(
            &engine.claude_paths().config_dir.to_string_lossy(),
            engine.claude_paths().home(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::live;
    use seatswap_core::providers::CODEX_ORG;
    use seatswap_core::store::FileSecrets;
    use seatswap_core::store::{self, Account, AccountsFile, SecretsBackend};
    use serde_json::json;

    const ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const EMAIL: &str = "someone@example.com";

    /// Two made-up Codex logins for the Codex switch (R8): the one Codex has
    /// open and the saved one the switch goes to.
    const CODEX_LIVE_ID: &str = "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13";
    const CODEX_OTHER_ID: &str = "7d2b9e4c-1a3f-4c5e-8b6d-9f0a1c2e3d4b";
    const CODEX_LIVE_AUTH: &str = r#"{"OPENAI_API_KEY":null,"auth_mode":"chatgpt","last_refresh":"2026-11-14T16:53:03.750022420Z","tokens":{"id_token":"eyJhbGciOi.id.sig","access_token":"eyJhbGciOi.access.sig","refresh_token":"rt_9f2c","account_id":"0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13","login_owner_id":"user-sam","oauth_provenance":{"issuer":"https://auth.openai.com","client_id":"app_4c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f"}}}"#;
    const CODEX_OTHER_AUTH: &str = r#"{"OPENAI_API_KEY":null,"auth_mode":"chatgpt","last_refresh":"2026-11-15T09:10:00.000000000Z","tokens":{"id_token":"eyJhbGciOi.other.id","access_token":"eyJhbGciOi.other.sig","refresh_token":"rt_51ab","account_id":"7d2b9e4c-1a3f-4c5e-8b6d-9f0a1c2e3d4b","login_owner_id":"user-sam","oauth_provenance":{"issuer":"https://auth.openai.com","client_id":"app_4c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f"}}}"#;

    /// The app's one error reporter with a sink a test ignores.
    fn test_errors() -> Arc<ReportedErrors> {
        Arc::new(ReportedErrors::new(Box::new(|_| {})))
    }

    /// The app's one warning printer for a runtime a test builds by hand.
    fn test_printed_warnings() -> Arc<Mutex<PrintedWarnings>> {
        Arc::new(Mutex::new(PrintedWarnings::new()))
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        claude: ClaudePaths,
        app: AppPaths,
        account: Account,
    }

    fn usage(session: f64, week: f64) -> Usage {
        Usage {
            limits: vec![
                Limit {
                    kind: LimitKind::Session,
                    percent: session,
                    severity: Severity::Critical,
                    resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
                    model: None,
                    is_active: true,
                },
                Limit {
                    kind: LimitKind::WeeklyAll,
                    percent: week,
                    severity: Severity::Normal,
                    resets_at: None,
                    model: None,
                    is_active: false,
                },
            ],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn credentials_json(refresh_token: &str) -> serde_json::Value {
        json!({
            "mcpOAuth": {
                "plugin:engineering:linear|5f1c2a9be0d34c11": {
                    "serverName": "plugin:engineering:linear",
                    "accessToken": "lin_oauth_placeholder",
                    "expiresAt": 1_794_400_000_000_i64
                }
            },
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", "a".repeat(95)),
                "refreshToken": refresh_token,
                "expiresAt": 1_794_356_040_000_i64,
                "refreshTokenExpiresAt": 1_796_772_840_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": ORG_UUID
        })
    }

    fn identity_json(account_uuid: &str, org_uuid: &str) -> serde_json::Value {
        json!({
            "numStartups": 41,
            "oauthAccount": {
                "accountUuid": account_uuid,
                "emailAddress": EMAIL,
                "organizationUuid": org_uuid,
                "organizationName": "Acme",
                "organizationRole": "admin",
                "billingType": "stripe_subscription",
                "hasExtraUsageEnabled": false
            },
            "projects": {}
        })
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().expect("tempdir");
            let config = tmp.path().join("claude");
            fs::create_dir_all(&config).expect("config folder");
            let claude = ClaudePaths::for_config_dir(&config, tmp.path());
            let app = AppPaths::at(&tmp.path().join("data"));
            fs::create_dir_all(app.data_dir.as_path()).expect("data folder");

            fs::write(
                &claude.credentials_file,
                credentials_json(&format!("sk-ant-ort01-{}", "b".repeat(95))).to_string(),
            )
            .expect("credentials");
            fs::write(
                &claude.identity_file,
                identity_json(ACCOUNT_UUID, ORG_UUID).to_string(),
            )
            .expect("identity");

            let store = Store::new(app.clone(), Box::new(FileSecrets::new(app.secrets_dir())));
            let login = live::read_login(&claude)
                .expect("read login")
                .expect("live login");
            let identity = live::read_identity(&claude)
                .expect("read identity")
                .expect("live identity");
            let file = store.load().expect("empty accounts");
            let account = store::new_account(&login, &identity, Some("Acme"), &file, Utc::now())
                .expect("new account");
            store
                .secrets()
                .set(&account.id, &login)
                .expect("store secret");
            let mut file: AccountsFile = store.load().expect("accounts");
            file.accounts.push(account.clone());
            // Record the file backend so `open_store` never probes the real keyring.
            file.secrets_backend = Some(SecretsBackend::File);
            // The display currency a price change is recorded in (Task 4).
            file.settings.currency = Some("EUR".to_string());
            store.save(&file).expect("save accounts");

            Fixture {
                _tmp: tmp,
                claude,
                app,
                account,
            }
        }

        fn history(&self) -> Result<seatswap_core::history::HistoryDir, String> {
            let file = store::load_accounts(&self.app).map_err(|error| error.to_string())?;
            ops::open_history(&self.app, &file.settings).map(|recorder| recorder.dir().clone())
        }

        fn engine(&self) -> Engine<FakeRemote> {
            let store = Store::new(
                self.app.clone(),
                Box::new(FileSecrets::new(self.app.secrets_dir())),
            );
            Engine::new(
                self.claude.clone(),
                store,
                FakeRemote {
                    usage: usage(97.0, 48.0),
                },
            )
            .expect("engine")
        }

        fn write_live(&self, credentials: serde_json::Value, identity: serde_json::Value) {
            fs::write(&self.claude.credentials_file, credentials.to_string())
                .expect("write credentials");
            fs::write(&self.claude.identity_file, identity.to_string()).expect("write identity");
        }
    }

    /// The two senders a runtime needs. The receivers are dropped: these tests
    /// read the snapshot, not the commands.
    fn burn_channels() -> (Sender<ActionCommand>, Sender<EngineCommand>) {
        let (action, _action_rx) = mpsc::channel();
        let (engine, _engine_rx) = mpsc::channel();
        (action, engine)
    }

    #[test]
    fn snapshot_names_the_active_account_and_the_live_login() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let now = Utc::now();
        let views = engine.poll(PollReason::Timer, now).expect("poll");
        let snapshot = snapshot_from_engine(&engine, views, None, Some(now));

        assert_eq!(
            snapshot.active_id.as_deref(),
            Some(fixture.account.id.as_str())
        );
        assert_eq!(
            snapshot.live,
            LiveView::Known {
                account_id: fixture.account.id.clone()
            }
        );
        assert_eq!(snapshot.views.len(), 1);
        assert!(snapshot.views[0].is_active);
        assert!(snapshot.views[0].usage.is_some());
        assert_eq!(snapshot.last_poll_at, Some(now));
    }

    #[test]
    fn every_live_state_maps_to_the_panel_view() {
        let fixture = Fixture::new();
        let engine = fixture.engine();

        // Known.
        let views = engine.views(Utc::now()).expect("views");
        assert_eq!(
            snapshot_from_engine(&engine, views, None, None).live,
            LiveView::Known {
                account_id: fixture.account.id.clone()
            }
        );

        // Ended: the refresh token is empty, the identity still matches.
        fixture.write_live(credentials_json(""), identity_json(ACCOUNT_UUID, ORG_UUID));
        let views = engine.views(Utc::now()).expect("views");
        assert_eq!(
            snapshot_from_engine(&engine, views, None, None).live,
            LiveView::Ended {
                account_id: Some(fixture.account.id.clone())
            }
        );

        // Unknown: the identity belongs to no stored account.
        fixture.write_live(
            credentials_json(&format!("sk-ant-ort01-{}", "b".repeat(95))),
            identity_json(
                "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e",
                "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e",
            ),
        );
        let views = engine.views(Utc::now()).expect("views");
        assert_eq!(
            snapshot_from_engine(&engine, views, None, None).live,
            LiveView::Unknown {
                email: Some(EMAIL.to_string()),
                org_name: Some("Acme".to_string())
            }
        );

        // No login at all.
        fs::remove_file(&fixture.claude.credentials_file).expect("remove credentials");
        let views = engine.views(Utc::now()).expect("views");
        assert_eq!(
            snapshot_from_engine(&engine, views, None, None).live,
            LiveView::NoLogin
        );
    }

    #[test]
    fn pending_is_set_and_cleared() {
        let fixture = Fixture::new();
        let engine = fixture.engine();
        let mut snapshot = snapshot_from_engine(&engine, Vec::new(), None, None);
        assert!(snapshot.pending.is_none());

        let pending = PendingSwitch {
            target_id: "target".to_string(),
            target_label: "Private".to_string(),
            email: Some(EMAIL.to_string()),
            org_name: None,
        };
        snapshot.set_pending(pending.clone());
        assert_eq!(snapshot.pending, Some(pending));

        snapshot.clear_pending();
        assert!(snapshot.pending.is_none());
    }

    #[test]
    fn snapshot_json_has_the_contract_field_names() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let now = Utc::now();
        let views = engine.poll(PollReason::Timer, now).expect("poll");
        let snapshot = snapshot_from_engine(&engine, views, Some("warning".to_string()), Some(now));
        let value = serde_json::to_value(&snapshot).expect("snapshot JSON");
        let object = value.as_object().expect("snapshot is an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "accounts",
                "activeId",
                "burn",
                "claudeConfigDir",
                "claudeNotSetUp",
                "codexLogins",
                "error",
                "lastPollAt",
                "live",
                "pending",
                "providerSubscriptions",
                "providers",
                "secretsWarning",
                "sessions",
                "settings",
                "views",
                "warnings",
            ]
        );
        assert_eq!(
            object["claudeConfigDir"],
            crate::test_support::tilde("claude")
        );
        assert_eq!(object["live"]["kind"], "known");
        assert_eq!(object["live"]["accountId"], fixture.account.id.as_str());
        assert_eq!(object["views"][0]["isActive"], true);
        assert!(object["settings"].get("trayText").is_some(), "{value}");
    }

    #[test]
    fn pending_survives_only_while_its_target_is_listed() {
        let fixture = Fixture::new();
        let engine = fixture.engine();
        let views = engine.views(Utc::now()).expect("views");
        let mut snapshot = snapshot_from_engine(&engine, views, None, None);
        let pending = PendingSwitch {
            target_id: fixture.account.id.clone(),
            target_label: fixture.account.label.clone(),
            email: Some(EMAIL.to_string()),
            org_name: None,
        };
        snapshot.set_pending(pending.clone());
        snapshot.retain_pending(Some(&pending));
        assert_eq!(snapshot.pending, Some(pending.clone()));

        // The target disappeared from the views: the pending must go too.
        snapshot.views.clear();
        snapshot.retain_pending(Some(&pending));
        assert!(snapshot.pending.is_none());
    }

    #[test]
    fn a_switch_notification_read_error_sends_the_switch_and_the_error() {
        let message = "Switched to AC — Acme";
        assert_eq!(
            switch_notices(message, Err("accounts.json is corrupt".to_string())),
            vec![
                SwitchNotice::Switched(message.to_string()),
                SwitchNotice::Error(
                    "the switch notification setting could not be read: accounts.json is corrupt"
                        .to_string()
                ),
            ]
        );
        assert_eq!(
            switch_notices(message, Ok(true)),
            vec![SwitchNotice::Switched(message.to_string())]
        );
        assert!(switch_notices(message, Ok(false)).is_empty());
    }

    #[test]
    fn commands_every_10_s_then_a_5_minute_jump_are_a_wake_at_the_next_command() {
        let wall = Utc::now();
        let monotonic = Instant::now();
        let mut baseline = ClockBaseline { wall, monotonic };

        // Commands every 10 s: the wall and monotonic clocks agree, no wake.
        for step in 1..=3 {
            let now = ClockBaseline {
                wall: wall + chrono::Duration::seconds(10 * step),
                monotonic: monotonic + Duration::from_secs(10 * step as u64),
            };
            let (next, wake) = wake_step(baseline, now);
            baseline = next;
            assert!(!wake, "a 10 s command gap is not a wake");
        }

        // The machine slept: 5 minutes of wall time, only 5 seconds monotonic.
        let now = ClockBaseline {
            wall: baseline.wall + chrono::Duration::seconds(300),
            monotonic: baseline.monotonic + Duration::from_secs(5),
        };
        let (next, wake) = wake_step(baseline, now);
        assert!(wake, "the 5-minute jump is a wake at the next command");
        assert_eq!(next.wall, now.wall);
        assert_eq!(next.monotonic, now.monotonic);
    }

    #[test]
    fn save_current_without_a_live_login_is_an_error_not_a_success() {
        let fixture = Fixture::new();
        fs::remove_file(&fixture.claude.credentials_file).expect("remove credentials");
        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history();
        let mut action = ActionRuntime::new(
            fixture.app,
            fixture.claude,
            Vec::new(),
            engine,
            history,
            test_errors(),
            test_printed_warnings(),
        );
        match action.save_current(None) {
            ActionReply::Failed(message) => {
                assert!(message.contains("no live login"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// R8: a Codex switch goes through `ops::switch_codex`, writes one
    /// `providerSwitch` event with `by: user`, and the reply says that the
    /// sessions already running keep their login.
    #[test]
    fn a_codex_switch_records_a_provider_switch_and_says_running_sessions_keep_theirs() {
        let fixture = Fixture::new();
        let codex_home = fixture._tmp.path().join("codex");
        fs::create_dir_all(&codex_home).expect("the Codex folder");
        fs::write(codex_home.join("auth.json"), CODEX_LIVE_AUTH).expect("auth.json");
        let live = AccountRef {
            acc: CODEX_LIVE_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let other = AccountRef {
            acc: CODEX_OTHER_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts.json");
        file.settings.codex_home = Some(codex_home.clone());
        for (account, abbr, plan) in [
            (live.clone(), "OA", Some("Plus")),
            (other.clone(), "OA2", Some("Pro")),
        ] {
            file.provider_subscriptions.push(ProviderSubscription {
                account,
                label: "OpenAI".to_string(),
                abbr: abbr.to_string(),
                color: "#db2777".to_string(),
                plan: plan.map(str::to_string),
                price: None,
                price_currency: None,
            });
        }
        store.save(&file).expect("accounts.json");
        let stored: seatswap_core::types::CodexLogin =
            serde_json::from_str(CODEX_OTHER_AUTH).expect("a Codex login");
        store
            .secrets()
            .set_codex(&providers::codex::secret_id(&other), &stored)
            .expect("the saved copy");

        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history().expect("the history folder");
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            Ok(history.clone()),
            test_errors(),
            test_printed_warnings(),
        );
        let note = match action.switch_codex(&other) {
            ActionReply::Switched(note) => note,
            other => panic!("expected Switched, got {other:?}"),
        };
        assert_eq!(
            note,
            "Codex now uses OA2. Codex sessions already running keep OA until they restart."
        );

        // Codex's own file now holds the saved login, byte for byte.
        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("auth.json"),
            CODEX_OTHER_AUTH.as_bytes(),
            "auth.json does not hold the saved login byte for byte"
        );

        let now = Utc::now();
        let read = history
            .read::<Event>(
                &Range {
                    from: now - chrono::Duration::hours(1),
                    to: now + chrono::Duration::hours(1),
                },
                &Hosts::One(history.host().to_string()),
            )
            .expect("the events are read");
        let switches: Vec<&EventKind> = read
            .records
            .iter()
            .map(|record| &record.record.kind)
            .filter(|kind| matches!(kind, EventKind::ProviderSwitch { .. }))
            .collect();
        assert_eq!(switches.len(), 1, "one providerSwitch event");
        assert_eq!(
            switches[0],
            &EventKind::ProviderSwitch {
                from: Some(live),
                to: other,
                by: By::User,
            }
        );
    }

    /// R8: Remove deletes seatswap's copy only. The subscription, its plan and
    /// Codex's own login file stay.
    #[test]
    fn remove_codex_login_deletes_the_copy_and_keeps_the_subscription() {
        let fixture = Fixture::new();
        let codex_home = fixture._tmp.path().join("codex");
        fs::create_dir_all(&codex_home).expect("the Codex folder");
        fs::write(codex_home.join("auth.json"), CODEX_LIVE_AUTH).expect("auth.json");
        let live = AccountRef {
            acc: CODEX_LIVE_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let other = AccountRef {
            acc: CODEX_OTHER_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts.json");
        file.settings.codex_home = Some(codex_home.clone());
        for (account, abbr) in [(live.clone(), "OA"), (other.clone(), "OA2")] {
            file.provider_subscriptions.push(ProviderSubscription {
                account,
                label: "OpenAI".to_string(),
                abbr: abbr.to_string(),
                color: "#db2777".to_string(),
                plan: Some("Pro".to_string()),
                price: None,
                price_currency: None,
            });
        }
        store.save(&file).expect("accounts.json");
        for (account, auth) in [(&live, CODEX_LIVE_AUTH), (&other, CODEX_OTHER_AUTH)] {
            let login = serde_json::from_str(auth).expect("a Codex login");
            store
                .secrets()
                .set_codex(&providers::codex::secret_id(account), &login)
                .expect("a saved copy");
        }
        let auth_before = fs::read(codex_home.join("auth.json")).expect("auth.json");

        let (engine, _engine_rx) = mpsc::channel();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            fixture.history(),
            test_errors(),
            test_printed_warnings(),
        );
        match action.remove_codex_login(&other) {
            ActionReply::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }

        assert!(
            store
                .secrets()
                .get_codex(&providers::codex::secret_id(&other))
                .expect("the secret store")
                .is_none(),
            "the copy is gone"
        );
        let file = store.load().expect("accounts.json");
        assert!(
            file.provider_subscription(&other).is_some(),
            "the subscription stays"
        );
        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("auth.json"),
            auth_before,
            "Codex's own login file is not touched"
        );
    }

    /// R8: a keyring or API-key login means no saved login is live, so Remove
    /// deletes the stored copy instead of refusing with the live file's error.
    #[test]
    fn remove_codex_login_proceeds_on_a_keyring_or_api_key_login() {
        let fixture = Fixture::new();
        let codex_home = fixture._tmp.path().join("codex");
        fs::create_dir_all(&codex_home).expect("the Codex folder");
        let other = AccountRef {
            acc: CODEX_OTHER_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts.json");
        file.settings.codex_home = Some(codex_home.clone());
        file.provider_subscriptions.push(ProviderSubscription {
            account: other.clone(),
            label: "OpenAI".to_string(),
            abbr: "OA2".to_string(),
            color: "#db2777".to_string(),
            plan: Some("Pro".to_string()),
            price: None,
            price_currency: None,
        });
        store.save(&file).expect("accounts.json");
        let login = serde_json::from_str(CODEX_OTHER_AUTH).expect("a Codex login");

        let (engine, _engine_rx) = mpsc::channel();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            fixture.history(),
            test_errors(),
            test_printed_warnings(),
        );
        for auth in [
            r#"{"auth_mode":"chatgpt","tokens":null}"#,
            r#"{"OPENAI_API_KEY":"sk-made-up","auth_mode":"apikey","tokens":null}"#,
        ] {
            fs::write(codex_home.join("auth.json"), auth).expect("auth.json");
            store
                .secrets()
                .set_codex(&providers::codex::secret_id(&other), &login)
                .expect("the saved copy");
            match action.remove_codex_login(&other) {
                ActionReply::Done => {}
                other => panic!("expected Done, got {other:?}"),
            }
            assert!(
                store
                    .secrets()
                    .get_codex(&providers::codex::secret_id(&other))
                    .expect("the secret store")
                    .is_none(),
                "the copy is gone for {auth}"
            );
        }
    }

    /// R8: the live login's copy cannot be removed, because the next poll would
    /// save it again. The worker is a guard of its own, not only the UI.
    #[test]
    fn the_live_codex_login_cannot_be_removed() {
        let fixture = Fixture::new();
        let codex_home = fixture._tmp.path().join("codex");
        fs::create_dir_all(&codex_home).expect("the Codex folder");
        fs::write(codex_home.join("auth.json"), CODEX_LIVE_AUTH).expect("auth.json");
        let live = AccountRef {
            acc: CODEX_LIVE_ID.to_string(),
            org: CODEX_ORG.to_string(),
        };
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts.json");
        file.settings.codex_home = Some(codex_home.clone());
        file.provider_subscriptions.push(ProviderSubscription {
            account: live.clone(),
            label: "OpenAI".to_string(),
            abbr: "OA".to_string(),
            color: "#db2777".to_string(),
            plan: Some("Plus".to_string()),
            price: None,
            price_currency: None,
        });
        store.save(&file).expect("accounts.json");
        let login = serde_json::from_str(CODEX_LIVE_AUTH).expect("a Codex login");
        store
            .secrets()
            .set_codex(&providers::codex::secret_id(&live), &login)
            .expect("the live copy");

        let (engine, _engine_rx) = mpsc::channel();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            fixture.history(),
            test_errors(),
            test_printed_warnings(),
        );

        match action.remove_codex_login(&live) {
            ActionReply::Failed(text) => {
                assert_eq!(text, providers::codex::LIVE_LOGIN_REMOVE)
            }
            other => panic!("expected the live-login refusal, got {other:?}"),
        }
        assert!(
            store
                .secrets()
                .get_codex(&providers::codex::secret_id(&live))
                .expect("the secret store")
                .is_some(),
            "the live login's copy stays"
        );
    }

    /// Turning auto-burn on or off writes `AutoBurnSet`, not the pause/resume
    /// event: a toggle must not end an explicit pause. The event a save
    /// writes comes from `settings_change_events`, so this needs no
    /// `save_setting` (which applies `startAtLogin`).
    #[test]
    fn a_toggle_writes_auto_burn_set() {
        let fixture = Fixture::new();
        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            history,
            test_errors(),
            test_printed_warnings(),
        );
        let off = Settings {
            auto_burn: false,
            ..Settings::default()
        };
        let on = Settings {
            auto_burn: true,
            ..off.clone()
        };
        let mut events = settings_change_events(&off, &on).expect("the events");
        assert_eq!(events.len(), 1, "a toggle writes one event");
        let event = events.remove(0);
        assert_eq!(event, EventKind::AutoBurnSet { on: true });
        assert_eq!(
            serde_json::to_value(&event).expect("the event as JSON"),
            json!({"type": "autoBurnSet", "on": true})
        );
        assert_eq!(
            settings_change_events(&on, &on),
            Ok(Vec::new()),
            "a save that leaves auto-burn alone writes no event"
        );
        let other = Settings {
            hotkey: "Ctrl+Alt+S".to_string(),
            ..on.clone()
        };
        assert_eq!(
            settings_change_events(&on, &other),
            Ok(Vec::new()),
            "another setting is not auto-burn"
        );

        action.record(event).expect("the event is written");
        let host = seatswap_core::paths::host_name(None).expect("the host name");
        let history = HistoryDir::at(&fixture.app.history_dir(), &host);
        let now = Utc::now();
        let events = history
            .read::<Event>(
                &Range {
                    from: now - chrono::Duration::hours(1),
                    to: now + chrono::Duration::hours(1),
                },
                &Hosts::One(host),
            )
            .expect("the events are read");
        assert_eq!(events.records.len(), 1, "one event for one toggle");
        assert_eq!(
            events.records[0].record.kind,
            EventKind::AutoBurnSet { on: true }
        );
    }

    #[test]
    fn merge_setting_changes_one_field_and_keeps_the_others() {
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let merged = merge_setting(&settings, "currency", json!("EUR"), Path::new("/home/sam"))
            .expect("the currency is merged");
        assert_eq!(merged.currency.as_deref(), Some("EUR"));
        assert!(merged.auto_burn, "another writer's autoBurn is kept");
        assert_eq!(
            merged.poll_active_secs, settings.poll_active_secs,
            "every other field is kept"
        );
    }

    #[test]
    fn merge_setting_refuses_an_unknown_field() {
        let error = merge_setting(
            &Settings::default(),
            "noSuchSetting",
            json!(true),
            Path::new("/home/sam"),
        )
        .expect_err("an unknown field is refused");
        assert_eq!(error, "unknown setting noSuchSetting");
    }

    #[test]
    fn merge_setting_names_the_field_of_a_bad_value() {
        let error = merge_setting(
            &Settings::default(),
            "pollActiveSecs",
            json!("x"),
            Path::new("/home/sam"),
        )
        .expect_err("a string is not a poll interval");
        assert!(error.starts_with("pollActiveSecs: "), "{error}");
    }

    #[test]
    fn merge_setting_refuses_an_invalid_value() {
        let error = merge_setting(
            &Settings::default(),
            "currency",
            json!("eur"),
            Path::new("/home/sam"),
        )
        .expect_err("a currency is three capital letters");
        assert!(
            error.contains("currency must be three capital letters"),
            "{error}"
        );
    }

    #[test]
    fn merge_setting_expands_a_tilde_in_the_two_path_settings() {
        let home = crate::test_support::home();
        let history = merge_setting(
            &Settings::default(),
            "historyDir",
            json!(crate::test_support::tilde("hist")),
            home,
        )
        .expect("the history folder is a path");
        assert_eq!(history.history_dir, Some(home.join("hist")));
        let config = merge_setting(
            &Settings::default(),
            "claudeConfigDir",
            json!(crate::test_support::tilde("claude")),
            home,
        )
        .expect("the config folder is a path");
        assert_eq!(config.claude_config_dir, Some(home.join("claude")));
        // Only the path settings expand: the host name stays "~x" and is
        // refused by its own rule, exactly as it would be without this merge.
        let error = merge_setting(&Settings::default(), "hostName", json!("~x"), home)
            .expect_err("a host name may not start with ~");
        assert!(error.contains("host name \"~x\""), "{error}");
        assert!(
            !error.contains(&home.display().to_string()),
            "the host name is not a path: {error}"
        );
    }

    #[test]
    fn merge_setting_expands_a_tilde_in_the_codex_folder() {
        let home = crate::test_support::home();
        let merged = merge_setting(
            &Settings::default(),
            "codexHome",
            json!(crate::test_support::tilde("codex")),
            home,
        )
        .expect("the Codex folder is a path");
        assert_eq!(merged.codex_home, Some(home.join("codex")));
    }

    #[test]
    fn a_changed_price_is_written_to_the_history() {
        let fixture = Fixture::new();
        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            history,
            test_errors(),
            test_printed_warnings(),
        );
        let reply = action.edit(
            &fixture.account.id,
            None,
            None,
            None,
            None,
            Some(Some(180.0)),
        );
        assert!(matches!(reply, ActionReply::Done), "{reply:?}");

        let host = seatswap_core::paths::host_name(None).expect("the host name");
        let history = HistoryDir::at(&fixture.app.history_dir(), &host);
        let read = history
            .read::<Event>(
                &Range {
                    from: DateTime::<Utc>::MIN_UTC,
                    to: DateTime::<Utc>::MAX_UTC,
                },
                &Hosts::One(host),
            )
            .expect("the events are read");
        let prices: Vec<_> = read
            .records
            .iter()
            .filter(|hosted| matches!(hosted.record.kind, EventKind::PriceChanged { .. }))
            .collect();
        assert_eq!(prices.len(), 1, "one event for one price change");
        let value = serde_json::to_value(&prices[0].record.kind).expect("the event as JSON");
        assert_eq!(value["type"], "priceChanged");
        assert_eq!(value["amount"], json!(180.0));
        assert_eq!(
            value["currency"],
            json!("EUR"),
            "the currency setting at the time of the change"
        );
    }

    /// An edit writes `accountLook` with only the fields it changed, through
    /// `ops::record_edit`, the function the CLI's edit uses too (spec §5).
    #[test]
    fn an_account_edit_writes_only_the_changed_look_fields() {
        let fixture = Fixture::new();
        let (engine, _engine_rx) = mpsc::channel();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            fixture.history(),
            test_errors(),
            test_printed_warnings(),
        );
        let same_abbr = fixture.account.abbr.clone();
        let reply = action.edit(
            &fixture.account.id,
            Some("Acme Lab".to_string()),
            Some(same_abbr),
            Some("#14b8a6".to_string()),
            None,
            None,
        );
        assert!(matches!(reply, ActionReply::Done), "{reply:?}");

        let host = seatswap_core::paths::host_name(None).expect("the host name");
        let read = HistoryDir::at(&fixture.app.history_dir(), &host)
            .read::<Event>(&Range::ALL, &Hosts::One(host))
            .expect("the events are read");
        let kinds: Vec<EventKind> = read
            .records
            .into_iter()
            .map(|hosted| hosted.record.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![EventKind::AccountLook {
                account: AccountRef::of(&fixture.account),
                label: Some("Acme Lab".to_string()),
                abbr: None,
                color: Some("#14b8a6".to_string()),
                auto_burn: None,
            }],
            "the unchanged abbreviation and the untouched fields are left out"
        );
    }

    /// The action thread's share takes another host's edit into
    /// `accounts.json` and has the engine publish it, so the open pages show it
    /// without a restart (spec §5).
    #[test]
    fn a_share_applies_another_hosts_edit_and_publishes() {
        let fixture = Fixture::new();
        let vega = fixture.app.history_dir().join("vega");
        fs::create_dir_all(&vega).expect("vega's folder");
        fs::write(vega.join("host.json"), r#"{"machineId":"made-up"}"#).expect("marker");
        HistoryDir::at(&fixture.app.history_dir(), "vega")
            .append(&Event {
                t: Utc::now(),
                kind: EventKind::SettingSet {
                    key: "currency".to_string(),
                    value: json!("CHF"),
                },
            })
            .expect("vega's event");
        HistoryDir::at(&fixture.app.history_dir(), "vega")
            .append(&Event {
                t: Utc::now(),
                kind: EventKind::AccountLook {
                    account: AccountRef::of(&fixture.account),
                    label: Some("Acme Lab".to_string()),
                    abbr: None,
                    color: None,
                    auto_burn: None,
                },
            })
            .expect("vega's event");
        let (engine, engine_rx) = mpsc::channel();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            fixture.history(),
            test_errors(),
            test_printed_warnings(),
        );

        action.share();

        let file = store::load_accounts(&fixture.app).expect("accounts");
        assert_eq!(file.settings.currency.as_deref(), Some("CHF"));
        assert_eq!(file.accounts[0].label, "Acme Lab");
        assert!(
            engine_rx
                .try_iter()
                .any(|command| matches!(command, EngineCommand::Publish)),
            "the change is published"
        );
    }

    #[test]
    fn the_share_is_asked_at_start_and_then_every_five_minutes() {
        let start = Utc::now();
        assert!(share_due(None, start), "the first ask is at start");
        assert!(!share_due(
            Some(start),
            start + chrono::Duration::seconds(299)
        ));
        assert!(share_due(Some(start), start + chrono::Duration::minutes(5)));
    }

    /// A save of a shared setting writes `settingSet` with the value as the
    /// settings file holds it; a setting that is not shared writes nothing.
    #[test]
    fn a_currency_save_writes_setting_set_and_a_hotkey_save_none() {
        let before = Settings::default();
        let currency = Settings {
            currency: Some("EUR".to_string()),
            ..before.clone()
        };
        assert_eq!(
            settings_change_events(&before, &currency),
            Ok(vec![EventKind::SettingSet {
                key: "currency".to_string(),
                value: json!("EUR"),
            }])
        );
        let hotkey = Settings {
            hotkey: "Ctrl+Alt+S".to_string(),
            ..before.clone()
        };
        assert_eq!(settings_change_events(&before, &hotkey), Ok(Vec::new()));
    }

    /// The mirror copies the live login into the stored account. When the plan
    /// of that account changes, the change belongs in the history, written by
    /// the code that records the events of this path.
    #[test]
    fn a_changed_plan_is_written_to_the_history_when_the_live_login_is_mirrored() {
        let fixture = Fixture::new();
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        // The stored plan is stale; the live login's tier says "Team Premium".
        let mut file = store.load().expect("accounts");
        file.accounts[0].plan = Some("Team".to_string());
        store.save(&file).expect("the stale plan is stored");

        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            history,
            test_errors(),
            test_printed_warnings(),
        );
        action.sync_live();

        assert_eq!(
            store.load().expect("accounts").accounts[0].plan.as_deref(),
            Some("Team Premium"),
            "the plan of the account is the login's"
        );

        let host = seatswap_core::paths::host_name(None).expect("the host name");
        let history = HistoryDir::at(&fixture.app.history_dir(), &host);
        let read = history
            .read::<Event>(
                &Range {
                    from: DateTime::<Utc>::MIN_UTC,
                    to: DateTime::<Utc>::MAX_UTC,
                },
                &Hosts::One(host),
            )
            .expect("the events are read");
        let changed: Vec<_> = read
            .records
            .iter()
            .filter(|hosted| matches!(hosted.record.kind, EventKind::PlanChanged { .. }))
            .collect();
        assert_eq!(changed.len(), 1, "one event for one plan change");
    }

    /// One event per real change: the same price again writes nothing, and
    /// clearing the price is a change of its own.
    #[test]
    fn an_unchanged_price_is_not_written_to_the_history() {
        let fixture = Fixture::new();
        let (engine, _engine_rx) = mpsc::channel();
        let history = fixture.history();
        let mut action = ActionRuntime::new(
            fixture.app.clone(),
            fixture.claude.clone(),
            Vec::new(),
            engine,
            history,
            test_errors(),
            test_printed_warnings(),
        );
        let host = seatswap_core::paths::host_name(None).expect("the host name");
        let history = HistoryDir::at(&fixture.app.history_dir(), &host);
        let prices = || {
            let read = history
                .read::<Event>(
                    &Range {
                        from: DateTime::<Utc>::MIN_UTC,
                        to: DateTime::<Utc>::MAX_UTC,
                    },
                    &Hosts::One(host.clone()),
                )
                .expect("the events are read");
            read.records
                .iter()
                .filter(|hosted| matches!(hosted.record.kind, EventKind::PriceChanged { .. }))
                .count()
        };

        assert!(matches!(
            action.edit(
                &fixture.account.id,
                None,
                None,
                None,
                None,
                Some(Some(180.0))
            ),
            ActionReply::Done
        ));
        assert_eq!(prices(), 1, "the first change is written");

        assert!(matches!(
            action.edit(
                &fixture.account.id,
                None,
                None,
                None,
                None,
                Some(Some(180.0))
            ),
            ActionReply::Done
        ));
        assert_eq!(prices(), 1, "the same price writes nothing");

        assert!(matches!(
            action.edit(&fixture.account.id, None, None, None, None, None),
            ActionReply::Done
        ));
        assert_eq!(prices(), 1, "a field that does not change writes nothing");

        assert!(matches!(
            action.edit(&fixture.account.id, None, None, None, None, Some(None)),
            ActionReply::Done
        ));
        assert_eq!(prices(), 2, "clearing the price is a change");
    }

    #[test]
    fn a_failed_autostart_says_the_settings_were_saved() {
        assert_eq!(
            autostart_failure_message("$HOME is empty"),
            "settings saved; start at login could not be set: $HOME is empty"
        );
    }

    #[test]
    fn an_unknown_pending_key_is_an_error_not_a_raw_pending() {
        let fixture = Fixture::new();
        let file = fixture.engine().store().load().expect("accounts");
        let live = LiveState::Unknown {
            email: Some(EMAIL.to_string()),
            org_name: Some("Acme".to_string()),
        };
        let pending =
            pending_switch(&file, &live, "AC", Some(EMAIL.to_string())).expect("known key");
        assert_eq!(pending.target_id, fixture.account.id);
        assert_eq!(pending.target_label, "Acme");
        assert_eq!(pending.org_name.as_deref(), Some("Acme"));

        let error = pending_switch(&file, &live, "ZZ", None).expect_err("unknown key");
        assert!(error.contains("unknown account ZZ"), "{error}");
    }

    #[test]
    fn an_invalid_account_colour_is_named_in_the_snapshot() {
        let fixture = Fixture::new();
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts");
        file.accounts[0].color = "xyz".to_string();
        store.save(&file).expect("save the invalid colour");

        let (sender, _receiver) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let history = fixture.history();
        let runtime = EngineRuntime::new(
            fixture.app,
            fixture.claude,
            sender,
            action,
            engine_commands,
            history,
            Arc::new(Mutex::new(None)),
            test_printed_warnings(),
        );
        let error = runtime
            .snapshot
            .error
            .expect("an invalid account colour is reported");
        assert_eq!(
            error,
            "account AC has an invalid colour \"xyz\"; set one in the account editor"
        );
    }

    #[test]
    fn a_corrupt_usage_cache_is_moved_aside_and_the_engine_reopens() {
        let fixture = Fixture::new();
        fs::write(fixture.app.usage_cache_file(), b"{ not json").expect("corrupt cache");

        let opened = open_engine(&fixture.app, &fixture.claude, &fixture.history())
            .expect("the engine reopens");
        let message = opened.notice.expect("a notice names the moved cache");
        assert!(message.contains("usage cache was unreadable"), "{message}");
        assert!(message.contains("usage is fetched again"), "{message}");
        assert!(opened.engine.views(Utc::now()).is_ok());
        assert!(
            !fixture.app.usage_cache_file().exists(),
            "the corrupt cache is renamed"
        );
        let aside = fs::read_dir(&fixture.app.data_dir)
            .expect("data dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("usage-cache.json.corrupt-")
            })
            .count();
        assert_eq!(aside, 1, "one cache was moved aside");
    }

    #[test]
    fn a_moved_cache_is_reported_in_the_first_snapshot() {
        let fixture = Fixture::new();
        fs::write(fixture.app.usage_cache_file(), b"{ not json").expect("corrupt cache");
        let (sender, _receiver) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let history = fixture.history();
        let runtime = EngineRuntime::new(
            fixture.app,
            fixture.claude,
            sender,
            action,
            engine_commands,
            history,
            Arc::new(Mutex::new(None)),
            test_printed_warnings(),
        );
        assert!(runtime.engine.is_some(), "the engine reopened");
        let error = runtime
            .snapshot
            .error
            .expect("the notice is in the snapshot");
        assert!(error.contains("usage cache was unreadable"), "{error}");
        assert!(error.contains("usage is fetched again"), "{error}");
    }

    #[test]
    fn settings_come_from_accounts_json_even_when_the_engine_is_not_open() {
        let fixture = Fixture::new();
        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let mut file = store.load().expect("accounts");
        file.settings.poll_active_secs = 77;
        store.save(&file).expect("save settings");
        // A folder where the cache file should be makes Engine::new fail while
        // accounts.json stays readable.
        fs::create_dir(fixture.app.usage_cache_file()).expect("cache path is a folder");

        let (sender, _receiver) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let history = fixture.history();
        let runtime = EngineRuntime::new(
            fixture.app,
            fixture.claude,
            sender,
            action,
            engine_commands,
            history,
            Arc::new(Mutex::new(None)),
            test_printed_warnings(),
        );
        assert!(runtime.engine.is_none(), "the engine could not open");
        assert_eq!(
            runtime
                .snapshot
                .settings
                .as_ref()
                .map(|settings| settings.poll_active_secs),
            Some(77),
            "the real settings are shown, not defaults"
        );
        assert!(
            runtime.snapshot.error.is_some(),
            "the engine failure is still reported"
        );
    }

    #[test]
    fn unreadable_settings_are_none_with_an_error_and_no_defaults() {
        let fixture = Fixture::new();
        fs::write(fixture.app.accounts_file(), b"{ not json").expect("corrupt accounts");
        let (sender, _receiver) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let history = fixture.history();
        let runtime = EngineRuntime::new(
            fixture.app,
            fixture.claude,
            sender,
            action,
            engine_commands,
            history,
            Arc::new(Mutex::new(None)),
            test_printed_warnings(),
        );
        assert!(runtime.snapshot.settings.is_none());
        let error = runtime.snapshot.error.expect("an error is shown");
        assert!(error.contains("accounts.json"), "{error}");
    }

    #[test]
    fn commands_do_not_delay_the_timer_tick() {
        let start = Instant::now();
        let mut deadline = TickDeadline::new(start);
        assert_eq!(deadline.wait(start), TICK, "the first wait is one tick");

        let mut ticks = 0;
        // A command every two seconds: each one is handled, but none moves the
        // deadline, so the timer still runs at 30 s and 60 s.
        for seconds in (2..=70).step_by(2) {
            let now = start + Duration::from_secs(seconds);
            if deadline.due(now) {
                ticks += 1;
                deadline.advance(now);
            }
        }
        assert_eq!(ticks, 2, "two timer polls in seventy seconds");
    }

    #[test]
    fn wake_detection_ignores_89_s_and_reports_91_s() {
        let monotonic = Instant::now();
        let wall = Utc::now();
        let previous_monotonic = monotonic - Duration::from_secs(1);

        assert!(!wake_detected(
            wall - chrono::Duration::seconds(90),
            previous_monotonic,
            wall,
            monotonic
        ));
        assert!(wake_detected(
            wall - chrono::Duration::seconds(92),
            previous_monotonic,
            wall,
            monotonic
        ));
    }

    #[test]
    fn an_external_change_is_not_recorded_twice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let account = AccountRef {
            acc: ACCOUNT_UUID.to_string(),
            org: ORG_UUID.to_string(),
        };
        let now = Utc::now();

        // A CLI switch to the same account 5 s ago: the watcher must not write
        // a second event for it.
        let first = HistoryDir::at(&tmp.path().join("first"), "atlas");
        let recorder = Recorder::new(HistoryDir::at(first.root(), "atlas"));
        ops::record_switch(
            &recorder,
            None,
            account.clone(),
            None,
            By::Cli,
            None,
            None,
            None,
            now - chrono::Duration::seconds(5),
        )
        .expect("the CLI switch is written");
        assert_eq!(
            record_external_switch(
                &recorder,
                &Mutex::new(PrintedWarnings::new()),
                None,
                &account,
                None,
                now
            )
            .expect("the events can be read"),
            RecentSwitch::Already {
                by: By::Cli,
                cwd: None
            }
        );

        // With no such event, an external switch is written exactly once.
        let second = HistoryDir::at(&tmp.path().join("second"), "atlas");
        let recorder = Recorder::new(HistoryDir::at(second.root(), "atlas"));
        assert_eq!(
            record_external_switch(
                &recorder,
                &Mutex::new(PrintedWarnings::new()),
                None,
                &account,
                None,
                now
            )
            .expect("the external switch is written"),
            RecentSwitch::Written
        );
        let read = second
            .read::<Event>(
                &Range {
                    from: now - chrono::Duration::seconds(1),
                    to: now + chrono::Duration::seconds(1),
                },
                &Hosts::One("atlas".to_string()),
            )
            .expect("the events can be read");
        let switches: Vec<&EventKind> = read
            .records
            .iter()
            .map(|hosted| &hosted.record.kind)
            .filter(|kind| matches!(kind, EventKind::Switch { .. }))
            .collect();
        assert_eq!(switches.len(), 1);
        match switches[0] {
            EventKind::Switch { by, to, .. } => {
                assert_eq!(*by, By::External);
                assert_eq!(to, &account);
            }
            other => panic!("expected a switch, got {other:?}"),
        }
    }

    #[test]
    fn a_read_warning_is_printed_once_per_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let account = AccountRef {
            acc: ACCOUNT_UUID.to_string(),
            org: ORG_UUID.to_string(),
        };
        let now = Utc::now();
        let dir = HistoryDir::at(&tmp.path().join("history"), "atlas");
        let host_dir = dir.root().join("atlas");
        fs::create_dir_all(&host_dir).expect("history folder");
        // A torn last line: reading the month file warns about it every time.
        fs::write(
            host_dir.join(format!("events-{}.jsonl", now.format("%Y-%m"))),
            b"{\"t\":",
        )
        .expect("the torn event line");
        let recorder = Recorder::new(dir);
        let printed = Mutex::new(PrintedWarnings::new());

        record_external_switch(&recorder, &printed, None, &account, None, now)
            .expect("the events can be read");
        assert_eq!(
            super::super::lock(&printed).len(),
            1,
            "the torn line is one warning"
        );
        record_external_switch(&recorder, &printed, None, &account, None, now)
            .expect("the events can be read again");
        assert_eq!(
            super::super::lock(&printed).len(),
            1,
            "the same warning text is remembered, not printed again"
        );
    }

    #[test]
    fn an_agent_switch_is_announced_with_the_folder() {
        let home = crate::test_support::home();
        let folder = crate::test_support::at_home("projects/atlas")
            .display()
            .to_string();
        assert_eq!(
            agent_switch_message("PR", Some(&folder), home),
            format!(
                "An agent switched to PR (from {}).",
                crate::test_support::tilde("projects/atlas")
            )
        );
        assert_eq!(
            agent_switch_message("PR", None, home),
            "An agent switched to PR."
        );
    }

    /// A history that cannot be written is shown once. When the history writes
    /// again, the error is gone: it must not stay in every snapshot.
    #[test]
    fn a_history_error_is_cleared_when_the_history_writes_again() {
        let fixture = Fixture::new();
        let history_root = fixture.app.history_dir();
        let host_dir = history_root.join("atlas");
        fs::create_dir_all(&host_dir).expect("history folder");
        // A folder where the events file belongs: every append fails.
        let events = host_dir.join(format!("events-{}.jsonl", Utc::now().format("%Y-%m")));
        fs::create_dir_all(&events).expect("the blocked events file");

        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let engine = Engine::new(fixture.claude.clone(), store, Api::new())
            .expect("engine")
            .with_recorder(Recorder::new(HistoryDir::at(&history_root, "atlas")));
        let (ui, _rx) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let mut runtime = EngineRuntime {
            app: fixture.app.clone(),
            claude: fixture.claude.clone(),
            ui,
            engine: Some(engine),
            history: fixture.history(),
            secrets_warning: None,
            open_error: None,
            history_error: None,
            live_written: false,
            notice: None,
            snapshot: Snapshot::empty(None, None, None),
            baseline: ClockBaseline::now(),
            burn: BurnRuntime::new(action.clone(), engine_commands),
            forecast: Arc::new(Mutex::new(None)),
            printed_warnings: test_printed_warnings(),
            tied_busy: 0,
            thinking: Vec::new(),
            action,
            share_asked_at: None,
        };

        runtime.write_live_once();
        assert!(
            runtime.history_error.is_some(),
            "the failed write must be reported: {:?}",
            runtime.history_error
        );

        // The history works again: the error must be gone.
        fs::remove_dir(&events).expect("the blocked events file is gone");
        runtime.write_live_once();
        assert!(runtime.live_written, "the live event is written");
        assert!(
            runtime.history_error.is_none(),
            "the history error must be cleared when the history writes again: {:?}",
            runtime.history_error
        );
    }

    /// `publish()` writes nothing to the history, so it must not clear the
    /// engine thread's history error: the error stays until a write succeeds.
    #[test]
    fn a_heartbeat_error_survives_a_publish() {
        let fixture = Fixture::new();
        let history_root = fixture.app.history_dir();
        let host_dir = history_root.join("atlas");
        fs::create_dir_all(&host_dir).expect("history folder");
        // A folder where the up file belongs: every heartbeat write fails.
        let up = host_dir.join(format!("up-{}.jsonl", Utc::now().format("%Y-%m")));
        fs::create_dir_all(&up).expect("the blocked up file");

        let store = Store::new(
            fixture.app.clone(),
            Box::new(FileSecrets::new(fixture.app.secrets_dir())),
        );
        let engine = Engine::new(fixture.claude.clone(), store, Api::new())
            .expect("engine")
            .with_recorder(Recorder::new(HistoryDir::at(&history_root, "atlas")));
        let (ui, _rx) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let mut runtime = EngineRuntime {
            app: fixture.app.clone(),
            claude: fixture.claude.clone(),
            ui,
            engine: Some(engine),
            history: fixture.history(),
            secrets_warning: None,
            open_error: None,
            history_error: None,
            live_written: false,
            notice: None,
            snapshot: Snapshot::empty(None, None, None),
            baseline: ClockBaseline::now(),
            burn: BurnRuntime::new(action.clone(), engine_commands),
            forecast: Arc::new(Mutex::new(None)),
            printed_warnings: test_printed_warnings(),
            tied_busy: 0,
            thinking: Vec::new(),
            action,
            share_asked_at: None,
        };

        runtime.heartbeat(Utc::now());
        assert!(
            runtime
                .history_error
                .as_deref()
                .is_some_and(|text| text.starts_with("the history cannot be written:")),
            "the failed heartbeat must be reported: {:?}",
            runtime.history_error
        );

        runtime.publish();
        let error = runtime
            .snapshot
            .error
            .as_deref()
            .expect("the heartbeat error is shown");
        assert!(
            error.contains("the history cannot be written:"),
            "publish() must not clear the history error: {error}"
        );
    }

    /// A tick that computes no situation clears the kept forecast: the Forecast
    /// page must not show the plan of an older reading as if it were current.
    #[test]
    fn a_tick_without_a_situation_clears_the_forecast() {
        let fixture = Fixture::new();
        let file = store::load_accounts(&fixture.app).expect("accounts");
        let forecast = Arc::new(Mutex::new(Some((
            Inputs {
                now: Utc::now(),
                offset: chrono::FixedOffset::east_opt(0).expect("UTC"),
                accounts: Vec::new(),
                active: None,
                rhythm: seatswap_core::burn::Rhythm { usd: [0.0; 168] },
                last_switch_at: None,
                paused_until: None,
            },
            Regime {
                early: false,
                why: seatswap_core::burn::decide::RegimeWhy::NoCostYet,
                lazy_blocked_h: 0.0,
                eager_blocked_h: 0.0,
                extra_switches: 0,
                cost_h: None,
            },
            Utc::now(),
        ))));
        let (ui, _rx) = mpsc::channel();
        let (action, engine_commands) = burn_channels();
        let mut runtime = EngineRuntime {
            app: fixture.app.clone(),
            claude: fixture.claude.clone(),
            ui,
            engine: None,
            history: fixture.history(),
            secrets_warning: None,
            open_error: None,
            history_error: None,
            live_written: false,
            notice: None,
            snapshot: Snapshot::empty(None, None, Some(file.settings)),
            baseline: ClockBaseline::now(),
            burn: BurnRuntime::new(action.clone(), engine_commands),
            forecast: Arc::clone(&forecast),
            printed_warnings: test_printed_warnings(),
            tied_busy: 0,
            thinking: Vec::new(),
            action,
            share_asked_at: None,
        };

        // No account: the burn runtime has no situation to compute a plan from.
        runtime.update_burn(Some(Vec::new()));

        assert!(
            super::super::lock(&forecast).is_none(),
            "a tick without a situation must clear the forecast"
        );
    }
}
