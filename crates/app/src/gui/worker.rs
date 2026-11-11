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

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;

use seatswap_core::api::Api;
#[cfg(test)]
use seatswap_core::api::ApiError;
#[cfg(test)]
use seatswap_core::engine::Remote;
use seatswap_core::engine::{
    self, AccountView, Engine, EngineError, LoginWarning, PollReason, Suggestion, UsageCache,
};
use seatswap_core::locks::{self, LockError};
use seatswap_core::paths::{AppPaths, ClaudePaths};
use seatswap_core::store::{self, AccountsFile, Settings, Store};
use seatswap_core::switcher::{self, LiveState, SwitchError, UnsavedPolicy};
#[cfg(test)]
use seatswap_core::types::Login;

use super::autostart;
use super::notify;
use crate::ops;

/// How often the engine thread polls even without a command.
const TICK: Duration = Duration::from_secs(30);

/// More wall-clock time than this beyond the monotonic clock between two ticks
/// means the machine slept.
const WAKE_THRESHOLD: Duration = Duration::from_secs(90);

/// What the panel sees: the whole GUI state, serde camelCase (docs/implementation.md).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub views: Vec<AccountView>,
    pub active_id: Option<String>,
    pub live: LiveView,
    pub suggestion: Option<Suggestion>,
    pub warnings: Vec<LoginWarning>,
    pub pending: Option<PendingSwitch>,
    pub secrets_warning: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// `None` when accounts.json cannot be read: the panel shows the error and
    /// disables Save instead of offering defaults that would overwrite it.
    pub settings: Option<Settings>,
}

impl Snapshot {
    pub(crate) fn empty(
        error: Option<String>,
        secrets_warning: Option<String>,
        settings: Option<Settings>,
    ) -> Snapshot {
        Snapshot {
            views: Vec::new(),
            active_id: None,
            live: LiveView::NoLogin,
            suggestion: None,
            warnings: Vec::new(),
            pending: None,
            secrets_warning,
            last_poll_at: None,
            error,
            settings,
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
#[derive(Debug, Clone, Copy)]
pub(crate) enum EngineCommand {
    Poll(PollReason),
    Publish,
    Shutdown,
}

/// Action thread commands. Each request that needs an answer carries its own
/// one-shot sender, so a Tauri command never waits on a shared reply slot.
pub(crate) enum ActionCommand {
    Switch {
        key: String,
        unsaved: UnsavedPolicy,
        label: Option<String>,
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
        reply: Sender<ActionReply>,
    },
    Remove {
        key: String,
        reply: Sender<ActionReply>,
    },
    SaveSettings {
        settings: Settings,
        reply: Sender<ActionReply>,
    },
    /// The watcher saw a change; mirror it when `mirrorLive` is on.
    SyncLive,
}

#[derive(Debug)]
pub(crate) enum ActionReply {
    Done,
    Switched(String),
    NotSaved(PendingSwitch),
    Failed(String),
}

/// What the engine thread hands to the main thread.
#[derive(Debug)]
pub(crate) enum UiEvent {
    Snapshot(Snapshot),
}

/// The handles the Tauri side keeps.
pub(crate) struct Worker {
    pub engine: Sender<EngineCommand>,
    pub action: Sender<ActionCommand>,
}

pub(crate) fn spawn(
    app: AppPaths,
    claude: ClaudePaths,
    autostart_args: Vec<String>,
    ui: Sender<UiEvent>,
) -> Result<Worker, String> {
    let (engine_tx, engine_rx) = mpsc::channel::<EngineCommand>();
    let (action_tx, action_rx) = mpsc::channel::<ActionCommand>();

    let action_engine = engine_tx.clone();
    let engine_app = app.clone();
    let engine_claude = claude.clone();
    thread::Builder::new()
        .name("seatswap-engine".to_string())
        .spawn(move || EngineRuntime::new(engine_app, engine_claude, ui).run(engine_rx))
        .map_err(|error| format!("cannot start the engine thread: {error}"))?;

    let action_claude = claude;
    thread::Builder::new()
        .name("seatswap-action".to_string())
        .spawn(move || {
            ActionRuntime::new(app, action_claude, autostart_args, action_engine).run(action_rx)
        })
        .map_err(|error| format!("cannot start the action thread: {error}"))?;

    Ok(Worker {
        engine: engine_tx,
        action: action_tx,
    })
}

// ---------------------------------------------------------------------------
// Engine thread

/// Join two error texts without losing either.
fn combine(first: Option<String>, second: String) -> Option<String> {
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
}

/// The one place that turns the engine's data into a [`Snapshot`]. The engine
/// thread and the tests both use it, so a test cannot pass against a copy of
/// the real rules.
pub(crate) fn build_snapshot(parts: SnapshotParts) -> Snapshot {
    let mut error = parts.engine_error;
    let (settings, live, suggestion, warnings) = match parts.accounts {
        Ok(file) => {
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
                engine::suggestion(&parts.views, &file.settings),
                engine::login_warnings(&parts.views),
            )
        }
        Err(store_error) => {
            error = combine(error, format!("cannot read accounts.json: {store_error}"));
            (None, LiveView::NoLogin, None, Vec::new())
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
        active_id,
        live,
        suggestion,
        warnings,
        pending: parts.pending,
        secrets_warning: parts.secrets_warning,
        last_poll_at: parts.last_poll_at,
        error,
        settings,
    }
}

/// Open the store and the engine. A corrupt usage cache is moved aside and the
/// engine opened again; the returned notice says where the cache went and is
/// reported once for this run.
struct OpenedEngine {
    engine: Engine<Api>,
    secrets_warning: Option<String>,
    notice: Option<String>,
}

fn open_engine(app: &AppPaths, claude: &ClaudePaths) -> Result<OpenedEngine, String> {
    let (store, warning) = store::open_store(app.clone()).map_err(|error| error.to_string())?;
    match Engine::new(claude.clone(), store, Api::new()) {
        Ok(engine) => Ok(OpenedEngine {
            engine,
            secrets_warning: warning,
            notice: None,
        }),
        Err(EngineError::CacheCorrupt { .. }) => {
            let moved = UsageCache::set_aside(app).map_err(|error| error.to_string())?;
            let (store, second_warning) =
                store::open_store(app.clone()).map_err(|error| error.to_string())?;
            let engine = Engine::new(claude.clone(), store, Api::new())
                .map_err(|error| error.to_string())?;
            Ok(OpenedEngine {
                engine,
                secrets_warning: warning.or(second_warning),
                notice: Some(format!(
                    "the usage cache was unreadable and was moved to {}; usage is fetched again",
                    moved.display()
                )),
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

struct EngineRuntime {
    app: AppPaths,
    claude: ClaudePaths,
    ui: Sender<UiEvent>,
    engine: Option<Engine<Api>>,
    secrets_warning: Option<String>,
    /// The engine-wide error of the last open/poll attempt.
    open_error: Option<String>,
    /// A one-shot message (the moved usage cache) for the next snapshot.
    notice: Option<String>,
    snapshot: Snapshot,
    baseline: ClockBaseline,
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
    fn new(app: AppPaths, claude: ClaudePaths, ui: Sender<UiEvent>) -> EngineRuntime {
        let mut runtime = EngineRuntime {
            app,
            claude,
            ui,
            engine: None,
            secrets_warning: None,
            open_error: None,
            notice: None,
            snapshot: Snapshot::empty(None, None, None),
            baseline: ClockBaseline::now(),
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
        loop {
            match commands.recv_timeout(TICK) {
                Ok(command) => {
                    // A command can arrive after any gap, including one that
                    // hid a suspend; check and update the baseline every time.
                    let (baseline, wake) = wake_step(self.baseline, ClockBaseline::now());
                    self.baseline = baseline;
                    if wake && !matches!(command, EngineCommand::Shutdown) {
                        self.poll(PollReason::Wake);
                    }
                    match command {
                        EngineCommand::Poll(reason) => self.poll(reason),
                        EngineCommand::Publish => self.publish(),
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
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Open the store and the engine. False when this failed; the message is in
    /// `open_error` and reaches the next snapshot.
    fn open(&mut self) -> bool {
        match open_engine(&self.app, &self.claude) {
            Ok(opened) => {
                self.engine = Some(opened.engine);
                self.secrets_warning = opened
                    .secrets_warning
                    .or_else(|| self.secrets_warning.clone());
                if let Some(notice) = opened.notice {
                    self.notice = Some(match self.notice.take() {
                        Some(previous) => format!("{previous}; {notice}"),
                        None => notice,
                    });
                }
                self.open_error = None;
                true
            }
            Err(error) => {
                self.open_error = Some(error);
                false
            }
        }
    }

    fn poll(&mut self, reason: PollReason) {
        if self.engine.is_none() && !self.open() {
            self.capture(Vec::new(), self.snapshot.last_poll_at);
            self.send();
            return;
        }
        let now = Utc::now();
        let result = match self.engine.as_mut() {
            Some(engine) => engine.poll(reason, now),
            None => return,
        };
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
        self.snapshot = build_snapshot(SnapshotParts {
            views,
            accounts,
            live,
            engine_error: self.open_error.clone(),
            secrets_warning: self.secrets_warning.clone(),
            last_poll_at,
            pending: None,
            notice: self.notice.take(),
        });
    }

    fn send(&self) {
        let _ = self.ui.send(UiEvent::Snapshot(self.snapshot.clone()));
    }
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
}

impl ActionRuntime {
    fn new(
        app: AppPaths,
        claude: ClaudePaths,
        autostart_args: Vec<String>,
        engine: Sender<EngineCommand>,
    ) -> ActionRuntime {
        ActionRuntime {
            app,
            claude,
            autostart_args,
            engine,
            store: None,
        }
    }

    fn run(mut self, commands: Receiver<ActionCommand>) {
        for command in commands {
            match command {
                ActionCommand::Switch {
                    key,
                    unsaved,
                    label,
                    reply,
                } => {
                    let result = self.switch(&key, unsaved, label);
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
                    reply,
                } => {
                    let _ = reply.send(self.edit(&key, label, abbr, color));
                }
                ActionCommand::Remove { key, reply } => {
                    let _ = reply.send(self.remove(&key));
                }
                ActionCommand::SaveSettings { settings, reply } => {
                    let _ = reply.send(self.save_settings(settings));
                }
                ActionCommand::SyncLive => self.sync_live(),
            }
        }
    }

    fn store(&mut self) -> Result<&Store, String> {
        if self.store.is_none() {
            let (store, _warning) =
                store::open_store(self.app.clone()).map_err(|e| e.to_string())?;
            self.store = Some(store);
        }
        self.store
            .as_ref()
            .ok_or_else(|| "the store is not open".to_string())
    }

    fn switch(&mut self, key: &str, unsaved: UnsavedPolicy, label: Option<String>) -> ActionReply {
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
                let message = match self.store() {
                    Ok(store) => match ops::switch_message(store, &outcome.to) {
                        Ok(message) => message,
                        Err(error) => ops::switched_without_list(&error),
                    },
                    Err(error) => ops::switched_without_list(&error),
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
                        SwitchNotice::Switched(text) => notify::send_switch(&text),
                        SwitchNotice::Error(text) => notify::send_error(&text),
                    }
                }
                let _ = self
                    .engine
                    .send(EngineCommand::Poll(PollReason::AfterSwitch));
                ActionReply::Switched(message)
            }
            Err(SwitchError::CurrentNotSaved { email }) => match self.pending_for(key, email) {
                Ok(pending) => ActionReply::NotSaved(pending),
                Err(message) => {
                    notify::send_error(&message);
                    ActionReply::Failed(message)
                }
            },
            Err(error) => {
                let message = error.to_string();
                // A switch error is always shown, whatever notifyOnSwitch says.
                notify::send_error(&message);
                ActionReply::Failed(message)
            }
        }
    }

    fn pending_for(&mut self, key: &str, email: Option<String>) -> Result<PendingSwitch, String> {
        let file = self.store()?.load().map_err(|error| error.to_string())?;
        let live = switcher::live_state(&self.claude, &file).map_err(|error| error.to_string())?;
        pending_switch(&file, &live, key, email)
    }

    fn save_current(&mut self, label: Option<String>) -> ActionReply {
        let claude = self.claude.clone();
        let result = match self.store() {
            Ok(store) => ops::save_live_login(&claude, store, label, Utc::now()),
            Err(error) => return ActionReply::Failed(error),
        };
        match result {
            Ok(_) => {
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
    ) -> ActionReply {
        let result = match self.store() {
            Ok(store) => switcher::edit_account(
                store,
                key,
                label.as_deref(),
                abbr.as_deref(),
                color.as_deref(),
            ),
            Err(error) => return ActionReply::Failed(error),
        };
        match result {
            Ok(_) => {
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    fn remove(&mut self, key: &str) -> ActionReply {
        let result = match self.store() {
            Ok(store) => switcher::remove_account(store, key),
            Err(error) => return ActionReply::Failed(error),
        };
        match result {
            Ok(_) => {
                let _ = self.engine.send(EngineCommand::Publish);
                ActionReply::Done
            }
            Err(error) => ActionReply::Failed(error.to_string()),
        }
    }

    fn save_settings(&mut self, settings: Settings) -> ActionReply {
        if let Err(error) = settings.validate() {
            return ActionReply::Failed(error.to_string());
        }
        let result = (|| -> Result<(), String> {
            let store = self.store()?;
            let _lock = locks::acquire_app_lock(store.app_paths(), switcher::APP_LOCK_TIMEOUT)
                .map_err(|error| error.to_string())?;
            let mut file = store.load().map_err(|error| error.to_string())?;
            file.settings = settings.clone();
            store.save(&file).map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            return ActionReply::Failed(error);
        }
        if let Err(error) =
            autostart::apply_autostart(settings.start_at_login, &self.autostart_args)
        {
            // The settings themselves are saved; say so instead of claiming
            // that the whole save failed.
            let _ = self.engine.send(EngineCommand::Publish);
            return ActionReply::Failed(autostart_failure_message(&error));
        }
        let _ = self.engine.send(EngineCommand::Publish);
        ActionReply::Done
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
        if mirror {
            let claude = self.claude.clone();
            let result = match self.store() {
                Ok(store) => Some(switcher::sync_live(&claude, store, None, Utc::now())),
                Err(_) => None,
            };
            match result {
                Some(Ok(_)) | None => {}
                // Claude Code is mid-renewal; its next change calls us again.
                Some(Err(SwitchError::Lock(LockError::Busy { .. }))) => {}
                Some(Err(error)) => eprintln!("seatswap: watcher: {error}"),
            }
        }
        let _ = self.engine.send(EngineCommand::Publish);
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::live;
    use seatswap_core::store::FileSecrets;
    use seatswap_core::store::{self, Account, AccountsFile, SecretsBackend};
    use serde_json::json;

    const ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const EMAIL: &str = "someone@example.com";

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
            store.save(&file).expect("save accounts");

            Fixture {
                _tmp: tmp,
                claude,
                app,
                account,
            }
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
                "activeId",
                "error",
                "lastPollAt",
                "live",
                "pending",
                "secretsWarning",
                "settings",
                "suggestion",
                "views",
                "warnings",
            ]
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
        let mut action = ActionRuntime::new(fixture.app, fixture.claude, Vec::new(), engine);
        match action.save_current(None) {
            ActionReply::Failed(message) => {
                assert!(message.contains("no live login"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
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
        let runtime = EngineRuntime::new(fixture.app, fixture.claude, sender);
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

        let opened = open_engine(&fixture.app, &fixture.claude).expect("the engine reopens");
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
        let runtime = EngineRuntime::new(fixture.app, fixture.claude, sender);
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
        let runtime = EngineRuntime::new(fixture.app, fixture.claude, sender);
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
        let runtime = EngineRuntime::new(fixture.app, fixture.claude, sender);
        assert!(runtime.snapshot.settings.is_none());
        let error = runtime.snapshot.error.expect("an error is shown");
        assert!(error.contains("accounts.json"), "{error}");
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
}
