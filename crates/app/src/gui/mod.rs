//! The tray app (docs/implementation.md "app: GUI").
//!
//! `run` starts Tauri, one engine thread, one action thread, the file watcher,
//! the single-instance socket and the tray. The main thread owns the windows and
//! the `Snapshot`; the worker threads never touch Tauri objects except through
//! the `UiEvent` channel.

#[cfg(feature = "gui")]
pub(crate) use crate::format;

pub(crate) mod ipc;
// `parse_target` is used by the CLI and the socket listener; the window itself
// is built only with the GUI.
pub(crate) mod window;

#[cfg(feature = "gui")]
pub(crate) mod autostart;
#[cfg(feature = "gui")]
pub(crate) mod burn_rt;
#[cfg(feature = "gui")]
pub(crate) mod desktop_entry;
#[cfg(feature = "gui")]
pub(crate) mod lookup_rt;
#[cfg(feature = "gui")]
pub(crate) mod notify;
#[cfg(feature = "gui")]
pub(crate) mod panel;
#[cfg(feature = "gui")]
pub(crate) mod providers_rt;
#[cfg(feature = "gui")]
pub(crate) mod registry;
#[cfg(feature = "gui")]
pub(crate) mod sessions_rt;
#[cfg(feature = "gui")]
pub(crate) mod tray;
#[cfg(feature = "gui")]
pub(crate) mod worker;

#[cfg(feature = "gui")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "gui")]
use std::path::PathBuf;
#[cfg(feature = "gui")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "gui")]
use std::sync::mpsc;
#[cfg(feature = "gui")]
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(feature = "gui")]
use std::thread;

#[cfg(feature = "gui")]
use chrono::{DateTime, Local, Utc};

#[cfg(feature = "gui")]
use seatswap_core::history::By;
#[cfg(feature = "gui")]
use seatswap_core::paths::AppPaths;
#[cfg(feature = "gui")]
use seatswap_core::store;
#[cfg(feature = "gui")]
use seatswap_core::switcher::UnsavedPolicy;

#[cfg(feature = "gui")]
use notify::Notification;

#[cfg(feature = "gui")]
use tauri::{AppHandle, Emitter, Manager};

#[cfg(feature = "gui")]
use burn_rt::KeptForecast;
#[cfg(feature = "gui")]
use lookup_rt::{LookupClient, LookupHandle};
#[cfg(feature = "gui")]
use providers_rt::{CodexLoginView, ProviderView, ProvidersClient, ProvidersHandle};
#[cfg(feature = "gui")]
use sessions_rt::{SessionsClient, SessionsHandle};
#[cfg(feature = "gui")]
use worker::SessionsSummary;
#[cfg(feature = "gui")]
use worker::{
    ActionCommand, ActionReply, EngineCommand, PendingSwitch, PrintedWarnings, Snapshot, UiEvent,
};

/// The app icon (`crates/app/icons/icon.png`, 256 px), built into the program:
/// the notifications' image, the window icon and the Linux menu entry's icon.
#[cfg(feature = "gui")]
pub(crate) const APP_ICON_PNG: &[u8] = include_bytes!("../../icons/icon.png");

/// Prints and notifies an app error once per text for this run. The snapshot,
/// the action thread's history failures and the provider thread all report
/// through one instance (spec §1.2).
#[cfg(feature = "gui")]
pub(crate) struct ReportedErrors {
    notified_errors: Mutex<HashSet<String>>,
    send: Box<dyn Fn(&str) + Send + Sync>,
}

#[cfg(feature = "gui")]
impl ReportedErrors {
    pub(crate) fn new(send: Box<dyn Fn(&str) + Send + Sync>) -> ReportedErrors {
        ReportedErrors {
            notified_errors: Mutex::new(HashSet::new()),
            send,
        }
    }

    /// Print and notify `text` when this run has not reported it yet.
    pub(crate) fn report(&self, text: &str) {
        let new = lock(&self.notified_errors).insert(text.to_string());
        if new {
            eprintln!("seatswap: {text}");
            (self.send)(text);
        }
    }
}

/// The one pending 15 s re-check: when it wakes and whether its thread runs.
#[cfg(feature = "gui")]
#[derive(Default)]
struct Recheck {
    at: Option<DateTime<Utc>>,
    running: bool,
}

/// Everything the Tauri commands and the tray share. The snapshot is updated
/// only on the main thread (from `UiEvent`s) or by a command that then emits it.
#[cfg(feature = "gui")]
pub(crate) struct GuiState {
    snapshot: Mutex<Snapshot>,
    /// The last snapshot the pages were sent ([`send_snapshot`]).
    sent: Mutex<SentSnapshot>,
    /// The paths the app was started with, for the settings page's history and
    /// skill rows.
    pub(crate) app: AppPaths,
    pub(crate) claude: seatswap_core::paths::ClaudePaths,
    /// The history folder the recorder opened at startup, or why it could not
    /// be opened. A changed setting takes effect after a restart.
    pub(crate) history: Result<seatswap_core::history::HistoryDir, String>,
    /// The plan inputs, regime and oldest reading of the last snapshot's
    /// auto-burn decision; the Forecast page reads them. `None` until a
    /// situation exists.
    pub(crate) forecast: Arc<Mutex<Option<KeptForecast>>>,
    /// The latest sessions summary; every snapshot is emitted with it.
    sessions: Mutex<Option<SessionsSummary>>,
    /// The sessions thread's commands; `None` while `readSessions` is off.
    pub(crate) sessions_client: Option<SessionsClient>,
    pub(crate) engine: std::sync::mpsc::Sender<EngineCommand>,
    pub(crate) action: std::sync::mpsc::Sender<ActionCommand>,
    /// The look-up thread's view, and the handle that keeps the thread alive.
    pub(crate) lookup: LookupClient,
    _lookup: Mutex<Option<LookupHandle>>,
    /// The latest provider cards; every snapshot is emitted with them.
    providers: Mutex<Vec<ProviderView>>,
    /// The latest saved Codex logins (R8); every snapshot is emitted with them.
    codex_logins: Mutex<Vec<CodexLoginView>>,
    /// The provider thread's commands; the pop-up and the settings save use them.
    pub(crate) providers_client: ProvidersClient,
    _providers: Mutex<Option<ProvidersHandle>>,
    deduper: Mutex<notify::Deduper>,
    /// The waiting notifications that can still be withdrawn, by session id.
    waiting: Mutex<HashMap<String, notify::WaitingHandle>>,
    /// The one pending 15 s re-check for a session that has just started to
    /// wait (`notify::Deduper::recheck_at`).
    recheck: Mutex<Recheck>,
    /// The one app-wide error reporter: print and notify once per text.
    pub(crate) reported_errors: Arc<ReportedErrors>,
    /// The one set of history read warnings this run has printed; every thread
    /// and every statistics command prints through it.
    pub(crate) printed_warnings: Arc<Mutex<PrintedWarnings>>,
    /// Where the panel was opened, its last height and when focus loss last
    /// hid it (`panel::PanelState`).
    panel: Mutex<panel::PanelState>,
    tray: Mutex<Option<tauri::tray::TrayIcon>>,
    /// The login watch; `None` when it could not start (that error is then
    /// one of the start-up errors). Its state says whether Claude Code's
    /// folder exists yet.
    watcher: Mutex<Option<seatswap_core::watcher::Watcher>>,
    _sessions: Mutex<Option<SessionsHandle>>,
    /// The start-up errors joined once; [`with_gui_facts`] adds them to every
    /// new snapshot.
    startup_error: Option<String>,
    /// A normal quit sets this before the engine thread stops, so the UI
    /// disconnect is not mistaken for a crash.
    exiting: AtomicBool,
}

#[cfg(feature = "gui")]
impl GuiState {
    pub(crate) fn snapshot(&self) -> Result<Snapshot, String> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| "the snapshot lock is poisoned".to_string())
    }

    /// A save that the action thread has confirmed: the snapshot holds the
    /// saved settings now, not when the engine's next `Publish` arrives. The
    /// settings page reads the saved values before its restart note is drawn.
    pub(crate) fn remember_settings(&self, settings: seatswap_core::store::Settings) {
        lock(&self.snapshot).settings = Some(settings);
    }

    /// The sessions thread's client; when `readSessions` is off, the one text
    /// every sessions command answers with.
    pub(crate) fn sessions_commands(&self) -> Result<SessionsClient, String> {
        self.sessions_client
            .clone()
            .ok_or_else(|| sessions_rt::SESSIONS_OFF.to_string())
    }
}

#[cfg(feature = "gui")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the person is told when a worker thread has ended: the tray must never
/// keep looking alive.
#[cfg(feature = "gui")]
pub(crate) const THREAD_STOPPED: &str = "seatswap stopped working: restart it";

#[cfg(feature = "gui")]
pub(crate) fn thread_stopped_message() -> &'static str {
    THREAD_STOPPED
}

/// A send to a worker thread whose receiver is gone. The closed channel is the
/// same sentence everywhere, never a silent return.
#[cfg(feature = "gui")]
pub(crate) fn send_to_worker<T>(result: Result<(), mpsc::SendError<T>>) -> Result<(), String> {
    result.map_err(|_| thread_stopped_message().to_string())
}

/// A reply from a worker thread whose sender is gone.
#[cfg(feature = "gui")]
pub(crate) fn worker_reply<T>(result: Result<T, mpsc::RecvError>) -> Result<T, String> {
    result.map_err(|_| thread_stopped_message().to_string())
}

/// The snapshot the tray shows after its UI channel closed: the one error that
/// explains why nothing will update again.
#[cfg(feature = "gui")]
fn stopped_snapshot(mut snapshot: Snapshot) -> Snapshot {
    snapshot.error = Some(thread_stopped_message().to_string());
    snapshot
}

/// Start the tray app. Returns the process exit code.
#[cfg(feature = "gui")]
pub(crate) fn run(
    app: AppPaths,
    claude_override: Option<PathBuf>,
    data_override: Option<PathBuf>,
) -> std::process::ExitCode {
    let result = tauri::Builder::default()
        .setup(move |tauri_app| {
            setup(
                tauri_app,
                app.clone(),
                claude_override.clone(),
                data_override.clone(),
            )
        })
        .invoke_handler(tauri::generate_handler![
            panel::app_version,
            panel::get_snapshot,
            panel::refresh_now,
            panel::switch_account,
            panel::cancel_pending,
            panel::save_current,
            panel::edit_account,
            panel::edit_subscription,
            panel::remove_account,
            panel::switch_codex,
            panel::connect_agy,
            panel::remove_codex_login,
            panel::save_setting,
            panel::set_auto_burn,
            panel::pause_burn,
            panel::money_view,
            panel::price_list,
            panel::lookup_now,
            panel::icon_preview,
            panel::time_texts,
            panel::fit_panel,
            panel::hide_panel,
            panel::open_panel,
            panel::agent_calls,
            panel::skill_status,
            panel::skill_install,
            panel::projects_view,
            panel::projects_edit,
            panel::history_view,
            panel::history_import,
            panel::history_export,
            window::open_window,
            window::sessions_view,
            window::sessions_subscribe,
            window::sessions_unsubscribe,
            window::stats_overview,
            window::stats_this_week,
            window::stats_weeks,
            window::stats_week_summary,
            window::stats_window_detail,
            window::stats_projects,
            window::stats_project_detail,
            window::stats_subscriptions,
            window::stats_forecast,
            window::stats_rhythm,
            window::stats_activity,
            window::stats_frame,
            window::statusline_preview,
            window::statusline_presets,
            window::statusline_kinds,
            window::statusline_record,
            window::statusline_keys,
            window::statusline_fields,
            window::statusline_install_state,
            window::statusline_install,
            window::statusline_restore,
        ])
        .build(tauri::generate_context!());
    match result {
        Ok(app) => {
            app.run(|handle, event| match event {
                // The tray app has no window of its own: closing the large
                // window (it is destroyed, not hidden) must leave the tray
                // running. A quit calls `app.exit(0)`, which carries a code.
                tauri::RunEvent::ExitRequested { code, api, .. } if keeps_running(code) => {
                    api.prevent_exit();
                }
                tauri::RunEvent::Exit => {
                    if let Some(state) = handle.try_state::<GuiState>() {
                        state.exiting.store(true, Ordering::SeqCst);
                        // A quit drops the waiting notifications; the process
                        // is ending, so nothing has to close them one by one.
                        lock(&state.waiting).clear();
                        let _ = state.engine.send(EngineCommand::Shutdown);
                    }
                }
                _ => {}
            });
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("seatswap: cannot start the tray app: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(feature = "gui")]
fn setup(
    tauri_app: &mut tauri::App,
    app: AppPaths,
    claude_override: Option<PathBuf>,
    data_override: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    // macOS: a menu-bar app, without a Dock icon or a place in the app
    // switcher, like the tray app on Linux and Windows.
    #[cfg(target_os = "macos")]
    tauri_app
        .handle()
        .set_activation_policy(tauri::ActivationPolicy::Accessory)?;

    // Read the settings without opening the secret store: the main thread never
    // probes the keyring. A file that cannot be read is reported in the snapshot
    // and the tray keeps running.
    let (settings, settings_error) = match store::load_accounts(&app) {
        Ok(file) => (Some(file.settings), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let setting_dir = settings
        .as_ref()
        .and_then(|settings| settings.claude_config_dir.clone());

    let autostart_args =
        autostart::startup_arguments(data_override.as_deref(), claude_override.as_deref())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    let paths = crate::cli::resolve_paths(app, claude_override.as_deref(), setting_dir.as_deref())?;
    let (app, claude) = paths.into_parts();

    // Store errors are reported by the engine, which retries and can clear them;
    // watcher and single-instance problems cannot be retried and stay here.
    let mut startup_errors = Vec::new();
    if let Some(error) = settings_error {
        startup_errors.push(accounts_error(&error, &claude));
    }

    // Before the listener, the first place that can send a notification.
    #[cfg(windows)]
    if let Err(error) = notify::register_app_id() {
        startup_errors.push(format!("notification name: {error}"));
    }

    // Single instance: a held `gui.lock` takes the socket; a busy one sends
    // `open-panel main` to the instance that is already running and exits 0.
    let handle = tauri_app.handle().clone();
    let listener_action = handle.clone();
    // The listener can receive a feed before the worker exists; the slot is
    // filled as soon as it does, and a feed before that is simply dropped.
    let engine_slot: Arc<std::sync::OnceLock<mpsc::Sender<EngineCommand>>> =
        Arc::new(std::sync::OnceLock::new());
    let listener_engine = Arc::clone(&engine_slot);
    let listener = match ipc::Listener::start(&app, move |command| match command {
        ipc::IpcCommand::OpenPanel(view) => {
            let handle = listener_action.clone();
            let _ = listener_action.run_on_main_thread(move || {
                let _ = panel::show(&handle, &view);
            });
        }
        ipc::IpcCommand::OpenWindow(target) => {
            let handle = listener_action.clone();
            let _ = listener_action.run_on_main_thread(move || {
                show_window(&handle, &target);
            });
        }
        ipc::IpcCommand::Feed => {
            if let Some(engine) = listener_engine.get() {
                let _ = engine.send(EngineCommand::Feed);
            }
        }
    }) {
        Ok(listener) => Some(listener),
        Err(ipc::IpcError::AlreadyRunning) => {
            // Another instance holds the lock. Ask it to show the panel; if that
            // fails, say so and exit 1 instead of pretending it opened.
            match second_instance_outcome(ipc::send_open_panel(&app, "main")) {
                Ok(()) => handle.exit(0),
                Err(error) => {
                    eprintln!("seatswap: {error}");
                    handle.exit(1);
                }
            }
            return Ok(());
        }
        Err(error) => {
            // Never run without the single-instance lock: a second tray would
            // duplicate every notification and fight over the same files.
            let message = listener_start_failure(&error);
            eprintln!("seatswap: {message}");
            notify::send_error(&app, &message);
            handle.exit(1);
            return Ok(());
        }
    };

    let (ui_tx, ui_rx) = mpsc::channel();
    let notified_paths = app.clone();
    let reported_errors = Arc::new(ReportedErrors::new(Box::new(move |text: &str| {
        notify::send_error(&notified_paths, text)
    })));
    let printed_warnings = Arc::new(Mutex::new(PrintedWarnings::new()));
    let mut worker = worker::spawn(
        app.clone(),
        claude.clone(),
        autostart_args.clone(),
        ui_tx,
        settings.clone(),
        Arc::clone(&reported_errors),
        Arc::clone(&printed_warnings),
    )?;
    let _ = engine_slot.set(worker.engine.clone());

    let watcher = {
        let action = worker.action.clone();
        match seatswap_core::watcher::watch(&claude, move || {
            let _ = action.send(ActionCommand::SyncLive);
        }) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                startup_errors.push(format!("file watcher: {error}"));
                None
            }
        }
    };

    if let Some(settings) = &settings {
        if let Err(error) = autostart::apply_autostart(settings.start_at_login, &autostart_args) {
            startup_errors.push(error);
        }
    }

    // The Linux menu entry and its icon are written at every start, whatever
    // `startAtLogin` says (Windows and macOS have none; the outcome is empty).
    // A path that is not seatswap's own is left alone with one line; a failure
    // is reported and never stops the start.
    match desktop_entry::apply_at_start(&autostart_args) {
        Ok(outcome) => {
            for path in &outcome.left_alone {
                eprintln!(
                    "seatswap: {} was not made by seatswap; it is left as it is.",
                    path.display()
                );
            }
        }
        Err(error) => startup_errors.push(format!("menu entry: {error}")),
    }

    let startup_error = if startup_errors.is_empty() {
        None
    } else {
        Some(startup_errors.join("; "))
    };
    let initial = with_gui_facts(
        Snapshot::empty(None, None, settings),
        startup_error.as_deref(),
        watcher
            .as_ref()
            .map(seatswap_core::watcher::Watcher::state)
            .as_ref(),
    );
    let sessions = worker.sessions.take();
    let lookup_handle = worker
        .lookup
        .take()
        .ok_or_else(|| "the look-up thread did not start".to_string())?;
    let lookup = lookup_handle.client();
    let providers_handle = worker
        .providers
        .take()
        .ok_or_else(|| "the provider thread did not start".to_string())?;
    let providers_client = providers_handle.client();
    let history = worker.history.clone();
    let forecast = worker.forecast.clone();

    tauri_app.manage(GuiState {
        snapshot: Mutex::new(initial.clone()),
        sent: Mutex::new(SentSnapshot::default()),
        app: app.clone(),
        claude: claude.clone(),
        history,
        forecast,
        sessions: Mutex::new(None),
        sessions_client: sessions.as_ref().map(SessionsHandle::client),
        engine: worker.engine.clone(),
        action: worker.action.clone(),
        lookup,
        _lookup: Mutex::new(Some(lookup_handle)),
        providers: Mutex::new(Vec::new()),
        codex_logins: Mutex::new(Vec::new()),
        providers_client,
        _providers: Mutex::new(Some(providers_handle)),
        deduper: Mutex::new(notify::Deduper::new()),
        waiting: Mutex::new(HashMap::new()),
        recheck: Mutex::new(Recheck::default()),
        reported_errors,
        printed_warnings,
        panel: Mutex::new(panel::PanelState::new()),
        tray: Mutex::new(None),
        watcher: Mutex::new(watcher),
        _sessions: Mutex::new(sessions),
        startup_error,
        exiting: AtomicBool::new(false),
    });
    if let Some(listener) = listener {
        tauri_app.manage(ListenerHolder {
            _listener: Mutex::new(Some(listener)),
        });
    }

    tray::start(tauri_app.handle());

    // Build the (hidden) panel now, so its page has loaded and sized itself
    // before the first opening; otherwise that opening shows a white window
    // that then shrinks and grows.
    panel::preload(tauri_app.handle())?;

    // The page is not loaded yet; the engine's first snapshot follows shortly.
    send_snapshot(&handle, &tauri_app.state::<GuiState>(), &initial);

    let event_handle = tauri_app.handle().clone();
    thread::Builder::new()
        .name("seatswap-ui".to_string())
        .spawn(move || {
            while let Ok(event) = ui_rx.recv() {
                let handle = event_handle.clone();
                let _ = event_handle.run_on_main_thread(move || {
                    handle_ui_event(&handle, event);
                });
            }
            // Every sender is gone: the engine thread ended (normally it runs
            // until shutdown) and no new snapshot will arrive. Say so instead
            // of leaving the tray looking alive.
            let handle = event_handle.clone();
            let _ = event_handle.run_on_main_thread(move || {
                if let Some(state) = handle.try_state::<GuiState>() {
                    if state.exiting.load(Ordering::SeqCst) {
                        return;
                    }
                    let snapshot = stopped_snapshot(lock(&state.snapshot).clone());
                    apply_snapshot(&handle, Shown::New(Box::new(snapshot)));
                }
            });
        })?;

    Ok(())
}

/// The visible startup message when accounts.json cannot be read: the error
/// plus the Claude folder the tray is using meanwhile.
#[cfg(feature = "gui")]
fn accounts_error(error: &str, claude: &seatswap_core::paths::ClaudePaths) -> String {
    format!(
        "cannot read accounts.json ({error}); using Claude config folder {}",
        claude.config_dir.display()
    )
}

/// The exit code and message of a second instance. A successful send exits 0;
/// a failed one is a visible error and exit 1.
#[cfg(feature = "gui")]
fn second_instance_outcome(result: Result<(), ipc::IpcError>) -> Result<(), String> {
    result.map_err(|error| error.to_string())
}

/// The visible text for a listener that failed for any reason other than
/// "already running": the app must stop, because it cannot take the lock.
#[cfg(feature = "gui")]
fn listener_start_failure(error: &ipc::IpcError) -> String {
    format!("cannot start the single-instance listener: {error}")
}

/// Owns the socket listener so it lives as long as the app.
#[cfg(feature = "gui")]
struct ListenerHolder {
    _listener: Mutex<Option<ipc::Listener>>,
}

#[cfg(feature = "gui")]
fn handle_ui_event(app: &AppHandle, event: UiEvent) {
    match event {
        UiEvent::Snapshot(snapshot) => apply_snapshot(app, Shown::New(Box::new(snapshot))),
        UiEvent::Sessions(summary) => {
            let state = app.state::<GuiState>();
            *lock(&state.sessions) = Some(summary);
            apply_snapshot(app, Shown::Again);
        }
        UiEvent::Providers(lists) => {
            let state = app.state::<GuiState>();
            *lock(&state.providers) = lists.cards;
            *lock(&state.codex_logins) = lists.codex_logins;
            apply_snapshot(app, Shown::Again);
        }
        UiEvent::Money(view) => {
            let _ = app.emit("money", &view);
        }
        UiEvent::SessionsView(view) => {
            let _ = app.emit_to(window::WINDOW_LABEL, "sessions-view", &view);
        }
    }
}

/// What [`apply_snapshot`] shows: a snapshot that comes from the engine (or
/// replaces the engine's, as the stopped one does), or the stored one again
/// because the sessions, the providers or a re-check changed.
#[cfg(feature = "gui")]
enum Shown {
    New(Box<Snapshot>),
    Again,
}

/// The snapshot to show. Only a new one gets the GUI's own facts: the stored
/// one already carries them, and adding them again joined every start-up error
/// to itself ("X; X") on the first sessions or providers update.
#[cfg(feature = "gui")]
fn to_show(
    shown: Shown,
    stored: &Mutex<Snapshot>,
    startup: Option<&str>,
    watch: Option<&seatswap_core::watcher::WatchState>,
) -> Snapshot {
    match shown {
        Shown::New(snapshot) => with_gui_facts(*snapshot, startup, watch),
        Shown::Again => lock(stored).clone(),
    }
}

/// Add what the GUI knows and the engine does not: the start-up errors, the
/// login watch's failure, and whether Claude Code's folder exists yet.
#[cfg(feature = "gui")]
fn with_gui_facts(
    mut snapshot: Snapshot,
    startup: Option<&str>,
    watch: Option<&seatswap_core::watcher::WatchState>,
) -> Snapshot {
    use seatswap_core::watcher::WatchState;
    if let Some(startup) = startup {
        snapshot.error = worker::combine(snapshot.error.take(), startup.to_string());
    }
    if let Some(WatchState::Failed(text)) = watch {
        snapshot.error = worker::combine(snapshot.error.take(), format!("file watcher: {text}"));
    }
    snapshot.claude_not_set_up = matches!(watch, Some(WatchState::NotSetUp));
    snapshot
}

#[cfg(feature = "gui")]
fn apply_snapshot(app: &AppHandle, shown: Shown) {
    let state = app.state::<GuiState>();
    let watch = lock(&state.watcher)
        .as_ref()
        .map(seatswap_core::watcher::Watcher::state);
    let mut snapshot = to_show(
        shown,
        &state.snapshot,
        state.startup_error.as_deref(),
        watch.as_ref(),
    );
    snapshot.sessions = lock(&state.sessions).clone();
    snapshot.providers = lock(&state.providers).clone();
    snapshot.codex_logins = lock(&state.codex_logins).clone();
    let now = Utc::now();
    let (due, recheck) = {
        let mut current = lock(&state.snapshot);
        snapshot.retain_pending(current.pending.as_ref());
        let mut deduper = lock(&state.deduper);
        let due = deduper.due(&snapshot, now);
        let recheck = deduper.recheck_at(&snapshot, now);
        drop(deduper);
        *current = snapshot.clone();
        (due, recheck)
    };

    send_snapshot(app, &state, &snapshot);
    if let Some(tray) = lock(&state.tray).as_ref() {
        tray::update(tray, &snapshot);
    }

    if let Some(error) = &snapshot.error {
        state.reported_errors.report(error);
    }

    for notification in due {
        match notification {
            Notification::Suggestion(suggestion) => {
                notify::send_suggestion(&state.app, app.clone(), &suggestion);
            }
            Notification::LoginWarning(warning) => {
                let id = notify::warning_id(&warning).to_string();
                let name = view_name(&snapshot, &id);
                notify::send_login_warning(&state.app, &warning, &name);
            }
            Notification::UnknownLogin { email, org_name } => {
                notify::send_unknown_login(
                    &state.app,
                    app.clone(),
                    email.as_deref(),
                    org_name.as_deref(),
                );
            }
            Notification::UsableAgain { id, reason } => {
                let abbr = view_abbr(&snapshot, &id);
                let switch_app = app.clone();
                let switch_id = id.clone();
                let on_switch = move || {
                    request_switch(&switch_app, switch_id, UnsavedPolicy::Refuse, None);
                };
                let open_app = app.clone();
                let on_open = move || {
                    let handle = open_app.clone();
                    let _ = open_app.run_on_main_thread(move || show_panel(&handle, "main"));
                };
                notify::send_usable_again(&state.app, &abbr, &reason, on_switch, on_open);
            }
            Notification::AllFull { next } => match next {
                Some((id, at)) => {
                    let abbr = view_abbr(&snapshot, &id);
                    let time = format::format_reset_clock(at, Utc::now(), &Local);
                    notify::send_all_full(&state.app, Some((&abbr, &time)));
                }
                None => notify::send_all_full(&state.app, None),
            },
            Notification::Waiting(waiting) => {
                let this_host = state
                    .history
                    .as_ref()
                    .ok()
                    .map(|history| history.host().to_string());
                let open_app = app.clone();
                let session = waiting.session.clone();
                let on_open = move || {
                    let handle = open_app.clone();
                    let target = format!("sessions?{session}");
                    let _ = open_app.run_on_main_thread(move || show_window(&handle, &target));
                };
                let handle =
                    notify::send_waiting(&state.app, &waiting, this_host.as_deref(), on_open);
                let previous = lock(&state.waiting).insert(waiting.session.clone(), handle);
                if let Some(previous) = previous {
                    previous.withdraw();
                }
            }
            Notification::WaitingOver { session } => {
                let handle = lock(&state.waiting).remove(&session);
                if let Some(handle) = handle {
                    handle.withdraw();
                }
            }
        }
    }
    schedule_recheck(app, recheck);
}

/// Keep at most one thread for the next 15 s re-check. A newer moment replaces
/// the pending one's purpose: the thread that wakes for nothing finds nothing
/// due and stops.
#[cfg(feature = "gui")]
fn schedule_recheck(app: &AppHandle, at: Option<DateTime<Utc>>) {
    let Some(at) = at else {
        return;
    };
    let state = app.state::<GuiState>();
    let start = {
        let mut recheck = lock(&state.recheck);
        recheck.at = Some(at);
        if recheck.running {
            false
        } else {
            recheck.running = true;
            true
        }
    };
    if !start {
        return;
    }
    let handle = app.clone();
    let spawned = thread::Builder::new()
        .name("seatswap-recheck".to_string())
        .spawn(move || loop {
            let Some(state) = handle.try_state::<GuiState>() else {
                return;
            };
            let target = {
                let recheck = lock(&state.recheck);
                match recheck.at {
                    Some(target) => target,
                    None => {
                        drop(recheck);
                        let mut recheck = lock(&state.recheck);
                        recheck.running = false;
                        return;
                    }
                }
            };
            let now = Utc::now();
            if target <= now {
                apply_recheck_now(&handle);
                return;
            }
            let wait = match (target - now).to_std() {
                Ok(wait) => wait,
                Err(_) => {
                    // The target is not in the future (the clock moved past it
                    // after the read above): apply the snapshot at once, as
                    // the branch above does.
                    apply_recheck_now(&handle);
                    return;
                }
            };
            thread::sleep(wait);
        });
    if let Err(error) = spawned {
        eprintln!("seatswap: re-check thread: {error}");
        let state = app.state::<GuiState>();
        let mut recheck = lock(&state.recheck);
        recheck.at = None;
        recheck.running = false;
    }
}

/// Apply the stored snapshot again because a pending 15 s re-check is due.
#[cfg(feature = "gui")]
fn apply_recheck_now(handle: &AppHandle) {
    let Some(state) = handle.try_state::<GuiState>() else {
        return;
    };
    {
        let mut recheck = lock(&state.recheck);
        recheck.at = None;
        recheck.running = false;
    }
    let wake = handle.clone();
    let _ = handle.run_on_main_thread(move || apply_snapshot(&wake, Shown::Again));
}

#[cfg(feature = "gui")]
fn view_abbr(snapshot: &Snapshot, id: &str) -> String {
    snapshot
        .views
        .iter()
        .find(|view| view.id == id)
        .map(|view| view.abbr.clone())
        .unwrap_or_else(|| id.to_string())
}

#[cfg(feature = "gui")]
fn view_name(snapshot: &Snapshot, id: &str) -> String {
    snapshot
        .views
        .iter()
        .find(|view| view.id == id)
        .map(|view| format!("{} — {}", view.abbr, view.label))
        .unwrap_or_else(|| id.to_string())
}

#[cfg(feature = "gui")]
pub(crate) fn show_panel(app: &AppHandle, view: &str) {
    if let Err(error) = panel::show(app, view) {
        eprintln!("seatswap: panel: {error}");
    }
}

#[cfg(feature = "gui")]
pub(crate) fn show_window(app: &AppHandle, target: &str) {
    if let Err(error) = window::show(app, target) {
        report_error(app, &format!("window: {error}"));
    }
}

/// Report an error through the app's one reporter (printed and notified once
/// per text); before the reporter is managed, printed only.
#[cfg(feature = "gui")]
pub(crate) fn report_error(app: &AppHandle, text: &str) {
    match app.try_state::<GuiState>() {
        Some(state) => state.reported_errors.report(text),
        None => eprintln!("seatswap: {text}"),
    }
}

/// Whether the app keeps running after a window asked to exit. Closing the
/// large window sends `code: None`; `app.exit(0)` from the tray's quit carries
/// a code and ends the app.
#[cfg(feature = "gui")]
fn keeps_running(code: Option<i32>) -> bool {
    code.is_none()
}

/// The last snapshot sent to the pages, as sent. The sessions thread can finish
/// several passes a second while sessions work, and each pass made a snapshot
/// that both pages drew again, mostly unchanged (1.4.2).
#[cfg(feature = "gui")]
#[derive(Debug, Default)]
struct SentSnapshot(Option<serde_json::Value>);

#[cfg(feature = "gui")]
impl SentSnapshot {
    /// The payload to send for `snapshot`, or `None` when it is the one sent last.
    fn changed(&mut self, snapshot: &Snapshot) -> Result<Option<serde_json::Value>, String> {
        let payload = serde_json::to_value(snapshot)
            .map_err(|error| format!("cannot send the snapshot: {error}"))?;
        if self.0.as_ref() == Some(&payload) {
            return Ok(None);
        }
        self.0 = Some(payload.clone());
        Ok(Some(payload))
    }
}

/// Send `snapshot` to the pages (event `snapshot`) unless it equals the last one
/// sent. The one way every snapshot reaches the pages; a page that loads later
/// reads the stored one with `get_snapshot`.
#[cfg(feature = "gui")]
fn send_snapshot(app: &AppHandle, state: &GuiState, snapshot: &Snapshot) {
    match lock(&state.sent).changed(snapshot) {
        Ok(Some(payload)) => {
            let _ = app.emit("snapshot", payload);
        }
        Ok(None) => {}
        Err(error) => report_error(app, &error),
    }
}

#[cfg(feature = "gui")]
pub(crate) fn set_pending(
    app: &AppHandle,
    state: &GuiState,
    pending: PendingSwitch,
) -> Result<(), String> {
    let snapshot = {
        let mut current = lock(&state.snapshot);
        current.set_pending(pending);
        current.clone()
    };
    send_snapshot(app, state, &snapshot);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || show_panel(&handle, "main"));
    Ok(())
}

#[cfg(feature = "gui")]
pub(crate) fn clear_pending(app: &AppHandle, state: &GuiState) -> Result<(), String> {
    let snapshot = {
        let mut current = lock(&state.snapshot);
        current.clear_pending();
        current.clone()
    };
    send_snapshot(app, state, &snapshot);
    Ok(())
}

/// A switch started from the tray menu or a notification. The panel is opened
/// only when the live login must be saved or discarded first.
#[cfg(feature = "gui")]
pub(crate) fn request_switch(
    app: &AppHandle,
    key: String,
    unsaved: seatswap_core::switcher::UnsavedPolicy,
    label: Option<String>,
) {
    let state = app.state::<GuiState>();
    let action = state.action.clone();
    let paths = state.app.clone();
    let app = app.clone();
    thread::spawn(move || {
        let (reply, received) = mpsc::channel();
        if let Err(error) = send_to_worker(action.send(ActionCommand::Switch {
            key,
            unsaved,
            label,
            by: By::User,
            reason: None,
            reply,
        })) {
            notify::send_error(&paths, &error);
            return;
        }
        match worker_reply(received.recv()) {
            Ok(ActionReply::NotSaved(pending)) => {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(state) = handle.try_state::<GuiState>() {
                        let _ = set_pending(&handle, &state, pending);
                    }
                });
            }
            Ok(_) => {}
            Err(error) => notify::send_error(&paths, &error),
        }
    });
}

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;

    use std::path::Path;

    use seatswap_core::paths::ClaudePaths;

    // Windows links the .ico resource into the binaries only, so the unit-test harness has no default window icon.
    #[cfg(not(windows))]
    #[test]
    fn the_bundle_icon_is_the_default_window_icon() {
        // Every window is built without its own icon, so Tauri's default window
        // icon (the bundle's icons/icon.png) must be there. `test = true`: on
        // macOS every other context would embed a second Info.plist symbol
        // beside the app's own.
        let context: tauri::Context<tauri::Wry> = tauri::generate_context!(test = true);
        let icon = context
            .default_window_icon()
            .expect("the bundle icon is the default window icon");
        assert_eq!((icon.width(), icon.height()), (256, 256));
    }

    #[test]
    fn only_a_changed_snapshot_is_sent_again() {
        let mut sent = SentSnapshot::default();
        let first = Snapshot::empty(None, None, None);
        let payload = sent
            .changed(&first)
            .expect("serialises")
            .expect("first is sent");
        assert_eq!(payload["views"], serde_json::json!([]));
        assert_eq!(
            sent.changed(&first.clone()),
            Ok(None),
            "the same snapshot again"
        );

        let mut polled = first.clone();
        polled.last_poll_at = Some(
            DateTime::parse_from_rfc3339("2026-11-19T12:30:05Z")
                .expect("a time")
                .with_timezone(&Utc),
        );
        let payload = sent
            .changed(&polled)
            .expect("serialises")
            .expect("a change is sent");
        assert_eq!(payload["lastPollAt"], "2026-11-19T12:30:05Z");
        assert_eq!(sent.changed(&polled), Ok(None));
        // Back to the earlier one is a change against the last sent.
        assert!(sent.changed(&first).expect("serialises").is_some());
    }

    #[test]
    fn closing_a_window_keeps_the_app_running() {
        assert!(
            keeps_running(None),
            "closing a window must not end the tray app"
        );
        assert!(!keeps_running(Some(0)), "Quit seatswap still ends the app");
    }

    /// The windows show the app icon: the Swap S drawing of
    /// `crates/app/icons/icon.png`. The default window icon is the bundle's
    /// icon, so its picture must be that file's picture.
    // Windows links the .ico resource into the binaries only, so the unit-test harness has no default window icon.
    #[test]
    #[cfg(not(windows))]
    fn the_default_window_icon_is_the_app_icon() {
        let context: tauri::Context<tauri::Wry> = tauri::generate_context!(test = true);
        let icon = context
            .default_window_icon()
            .expect("the bundle icon is the default window icon");
        let app_icon =
            tauri::image::Image::from_bytes(APP_ICON_PNG).expect("the app icon is a PNG");
        assert_eq!((icon.width(), icon.height()), (256, 256));
        assert_eq!(
            icon.rgba(),
            app_icon.rgba(),
            "the window icon must be the app icon"
        );
    }

    /// The `.ico` holds the six sizes the task names, each a PNG.
    #[test]
    fn the_icon_file_holds_the_sizes_the_icon_needs() {
        let ico = include_bytes!("../../icons/icon.ico");
        assert_eq!(&ico[..4], &[0, 0, 1, 0], "an ICO file");
        let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
        assert_eq!(count, 6, "six sizes in the .ico");
        let mut sizes = Vec::new();
        for index in 0..count {
            let entry = &ico[6 + index * 16..6 + (index + 1) * 16];
            let width = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            let height = if entry[1] == 0 {
                256
            } else {
                u32::from(entry[1])
            };
            let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
            let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
            let frame = &ico[offset..offset + length];
            assert_eq!(
                &frame[..8],
                b"\x89PNG\r\n\x1a\n",
                "the {width} px frame is a PNG"
            );
            sizes.push((width, height));
        }
        assert_eq!(
            sizes,
            vec![(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)]
        );
    }

    /// The arrow heads of the Swap S drawing at the small sizes: the pixels
    /// that belong to an arrow head and not to the stroke of the "s". The
    /// drawing is `crates/app/icons/icon.svg` (256 units): the top arrow head
    /// is the triangle (164,50) (206,76) (164,102) and the bottom one the
    /// triangle (92,154) (50,180) (92,206); the stroke is 26 units wide.
    /// Measured today: 3 pixels per head at 16 px, 7 at 24 px.
    #[test]
    fn the_small_frames_keep_the_arrow_heads() {
        const TOP_HEAD: [(f64, f64); 3] = [(164.0, 50.0), (206.0, 76.0), (164.0, 102.0)];
        const TOP_BAR: [(f64, f64); 2] = [(168.0, 76.0), (104.0, 76.0)];
        const BOTTOM_HEAD: [(f64, f64); 3] = [(92.0, 154.0), (50.0, 180.0), (92.0, 206.0)];
        const BOTTOM_BAR: [(f64, f64); 2] = [(152.0, 180.0), (88.0, 180.0)];
        /// Half the width of the "s" stroke.
        const STROKE_HALF: f64 = 13.0;

        for size in [16u32, 24] {
            let frame = icon_frame(size);
            let top = arrow_head_pixels(&frame, size, TOP_HEAD, TOP_BAR, STROKE_HALF);
            let bottom = arrow_head_pixels(&frame, size, BOTTOM_HEAD, BOTTOM_BAR, STROKE_HALF);
            assert!(top >= 2, "{size} px: {top} pixels of the top arrow head");
            assert!(
                bottom >= 2,
                "{size} px: {bottom} pixels of the bottom arrow head"
            );
        }
    }

    /// The frame of the `.ico` with the wanted size, as RGBA.
    fn icon_frame(size: u32) -> Vec<u8> {
        let ico = include_bytes!("../../icons/icon.ico");
        let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
        for index in 0..count {
            let entry = &ico[6 + index * 16..6 + (index + 1) * 16];
            let width = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            if width != size {
                continue;
            }
            let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
            let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
            let image = tauri::image::Image::from_bytes(&ico[offset..offset + length])
                .expect("the frame is a PNG");
            return image.rgba().to_vec();
        }
        panic!("the .ico has no {size} px frame");
    }

    /// The pixels of `frame` that lie in the arrow head `head`, more than
    /// `stroke_half` away from the bar `bar` the head covers, and carry the
    /// drawing's ink.
    fn arrow_head_pixels(
        frame: &[u8],
        size: u32,
        head: [(f64, f64); 3],
        bar: [(f64, f64); 2],
        stroke_half: f64,
    ) -> usize {
        let scale = 256.0 / f64::from(size);
        let mut count = 0;
        for y in 0..size {
            for x in 0..size {
                let point = ((f64::from(x) + 0.5) * scale, (f64::from(y) + 0.5) * scale);
                if !in_triangle(point, head) {
                    continue;
                }
                if distance_to_segment(point, bar[0], bar[1]) <= stroke_half {
                    continue;
                }
                let index = ((y * size + x) * 4) as usize;
                let red = f64::from(frame[index]);
                let green = f64::from(frame[index + 1]);
                let blue = f64::from(frame[index + 2]);
                let light = 0.299 * red + 0.587 * green + 0.114 * blue;
                if light < 100.0 {
                    count += 1;
                }
            }
        }
        count
    }

    /// Whether the point lies in the triangle.
    fn in_triangle(point: (f64, f64), triangle: [(f64, f64); 3]) -> bool {
        let side = |a: (f64, f64), b: (f64, f64), c: (f64, f64)| {
            (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
        };
        let first = side(triangle[0], triangle[1], point);
        let second = side(triangle[1], triangle[2], point);
        let third = side(triangle[2], triangle[0], point);
        let negative = first < 0.0 || second < 0.0 || third < 0.0;
        let positive = first > 0.0 || second > 0.0 || third > 0.0;
        !(negative && positive)
    }

    /// The distance from the point to the line through `a` and `b`.
    fn distance_to_segment(point: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length = dx * dx + dy * dy;
        let along = if length == 0.0 {
            0.0
        } else {
            (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length).clamp(0.0, 1.0)
        };
        let (x, y) = (a.0 + along * dx, a.1 + along * dy);
        ((point.0 - x).powi(2) + (point.1 - y).powi(2)).sqrt()
    }

    #[test]
    fn a_stopped_worker_reports_one_sentence_and_a_closed_channel_is_it() {
        assert_eq!(
            thread_stopped_message(),
            "seatswap stopped working: restart it"
        );

        let (sender, receiver) = mpsc::channel::<()>();
        drop(receiver);
        assert_eq!(
            send_to_worker(sender.send(())),
            Err("seatswap stopped working: restart it".to_string())
        );

        let (sender, receiver) = mpsc::channel::<()>();
        drop(sender);
        assert_eq!(
            worker_reply(receiver.recv()),
            Err("seatswap stopped working: restart it".to_string())
        );
    }

    #[test]
    fn a_disconnected_ui_thread_puts_the_sentence_in_the_snapshot() {
        let snapshot = Snapshot::empty(None, None, None);
        assert_eq!(
            stopped_snapshot(snapshot).error.as_deref(),
            Some("seatswap stopped working: restart it")
        );
    }

    #[test]
    fn an_unreadable_accounts_file_names_the_claude_folder_in_use() {
        let claude =
            ClaudePaths::for_config_dir(Path::new("/home/u/.claude"), Path::new("/home/u"));
        let message = accounts_error("accounts.json is not valid JSON", &claude);
        assert!(
            message.contains("accounts.json is not valid JSON"),
            "{message}"
        );
        assert!(
            message.contains("using Claude config folder /home/u/.claude"),
            "{message}"
        );
    }
    #[test]
    fn a_second_instance_that_cannot_reach_the_first_is_an_error() {
        assert_eq!(second_instance_outcome(Ok(())), Ok(()));
        assert_eq!(
            second_instance_outcome(Err(ipc::IpcError::NotRunning)),
            Err("seatswap is not running".to_string())
        );
    }

    #[test]
    fn a_listener_failure_is_reported_with_its_cause() {
        let error = ipc::IpcError::Io {
            path: std::path::PathBuf::from("/run/seatswap.sock"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let message = listener_start_failure(&error);
        assert!(
            message.contains("cannot start the single-instance listener"),
            "{message}"
        );
        assert!(message.contains("/run/seatswap.sock"), "{message}");
    }

    #[test]
    fn startup_settings_come_from_accounts_json_or_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));
        std::fs::create_dir_all(&app.data_dir).expect("data folder");

        // Missing accounts.json is an empty list with default settings, so it is
        // readable and the real (default) settings are shown.
        let (settings, error) = match store::load_accounts(&app) {
            Ok(file) => (Some(file.settings), None),
            Err(error) => (None, Some(error.to_string())),
        };
        assert!(settings.is_some());
        assert!(error.is_none());

        // A corrupt file is None plus an error, never defaults.
        std::fs::write(app.accounts_file(), b"{not json").expect("corrupt accounts");
        let (settings, error) = match store::load_accounts(&app) {
            Ok(file) => (Some(file.settings), None),
            Err(error) => (None, Some(error.to_string())),
        };
        assert!(settings.is_none());
        assert!(error.is_some());
    }

    /// The one app error reporter: the same text is printed and notified once,
    /// a second text again.
    #[test]
    fn an_app_error_is_notified_once_per_text() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&sent);
        let errors = ReportedErrors::new(Box::new(move |text: &str| {
            lock(&sink).push(text.to_string());
        }));

        errors.report("the history cannot be written: no space left on device");
        errors.report("the history cannot be written: no space left on device");
        errors.report("auto-burn: cannot read the history");
        errors.report("the history cannot be written: no space left on device");

        assert_eq!(
            *lock(&sent),
            vec![
                "the history cannot be written: no space left on device".to_string(),
                "auto-burn: cannot read the history".to_string(),
            ],
            "each error text is notified once, and a new text is notified"
        );
    }

    /// The start-up error is added to each new snapshot once. Showing the
    /// stored snapshot again (a sessions or providers update, a re-check) must
    /// not add it a second time: that printed "X; X" at every start.
    #[test]
    fn a_startup_error_is_shown_once_however_often_the_snapshot_is_shown() {
        let startup = "notification name: the registry refused the write";
        let stored = Mutex::new(to_show(
            Shown::New(Box::new(Snapshot::empty(None, None, None))),
            &Mutex::new(Snapshot::empty(None, None, None)),
            Some(startup),
            None,
        ));
        assert_eq!(lock(&stored).error.as_deref(), Some(startup));

        for _ in 0..3 {
            let again = to_show(Shown::Again, &stored, Some(startup), None);
            assert_eq!(again.error.as_deref(), Some(startup));
            *lock(&stored) = again;
        }

        // A new engine snapshot with its own error: both, each once.
        let engine = Snapshot::empty(Some("usage: rate limited".to_string()), None, None);
        let shown = to_show(Shown::New(Box::new(engine)), &stored, Some(startup), None);
        assert_eq!(
            shown.error.as_deref(),
            Some(format!("usage: rate limited; {startup}").as_str())
        );
    }

    /// A machine without Claude Code's folder is a calm state, not an error;
    /// a watch that cannot follow the folders is an error.
    #[test]
    fn the_watch_state_reaches_the_snapshot() {
        use seatswap_core::watcher::WatchState;
        let empty = || Snapshot::empty(None, None, None);

        let not_set_up = with_gui_facts(empty(), None, Some(&WatchState::NotSetUp));
        assert!(not_set_up.claude_not_set_up);
        assert_eq!(not_set_up.error, None);

        let watching = with_gui_facts(empty(), None, Some(&WatchState::Watching));
        assert!(!watching.claude_not_set_up);
        assert_eq!(watching.error, None);

        let failed = with_gui_facts(
            empty(),
            None,
            Some(&WatchState::Failed(
                "/home/sam/.claude: not a folder".to_string(),
            )),
        );
        assert!(!failed.claude_not_set_up);
        assert_eq!(
            failed.error.as_deref(),
            Some("file watcher: /home/sam/.claude: not a folder")
        );
    }
}
