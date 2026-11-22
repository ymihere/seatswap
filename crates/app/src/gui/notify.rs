//! Desktop notifications and their de-duplication (docs/implementation.md
//! "Notifications", spec §7.4, §6.3).
//!
//! The decision "may this notification be shown?" is a plain struct, tested
//! without notify-rust. Sending happens in small helpers; a suggestion with its
//! "Switch" action waits on its own thread so no worker is blocked.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::thread;

use seatswap_core::paths::AppPaths;

#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(unix, not(target_os = "macos")))]
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use notify_rust::Notification as DesktopNotification;
use notify_rust::NotificationResponse;
#[cfg(all(unix, not(target_os = "macos")))]
use notify_rust::Timeout;

use seatswap_core::engine::{all_blocked, AccountView, Blocker, LoginWarning};
use seatswap_core::history::is_early_reset;
use seatswap_core::store::{LoginState, Settings, WaitingHosts};
use seatswap_core::switcher::UnsavedPolicy;

#[cfg(windows)]
use super::registry;
use super::worker::{LiveView, Snapshot, Waiting};
use super::APP_ICON_PNG;
use crate::burn_text::SwitchSuggestion;

/// What the de-duplicator decided to show.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Notification {
    Suggestion(SwitchSuggestion),
    LoginWarning(LoginWarning),
    UnknownLogin {
        email: Option<String>,
        org_name: Option<String>,
    },
    UsableAgain {
        id: String,
        reason: String,
    },
    AllFull {
        next: Option<(String, DateTime<Utc>)>,
    },
    Waiting(Waiting),
    WaitingOver {
        session: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WarningLevel {
    Soon,
    Tomorrow,
    Ended,
}

impl WarningLevel {
    fn of(warning: &LoginWarning) -> WarningLevel {
        match warning {
            LoginWarning::Soon { .. } => WarningLevel::Soon,
            LoginWarning::Tomorrow { .. } => WarningLevel::Tomorrow,
            LoginWarning::Ended { .. } => WarningLevel::Ended,
        }
    }
}

/// What one account looked like in the previous snapshot (spec §4).
#[derive(Debug, Clone)]
struct PreviousView {
    usable: bool,
    blocked_until: Option<DateTime<Utc>>,
    /// The engine's reason a limit blocked the account; `None` when no limit
    /// blocked it (no usage yet, a login problem, or usable).
    blocker: Option<Blocker>,
    /// The percent of the limit `blocker` names in this snapshot, so the next
    /// one can tell a reset before its time by its drop (decision 12).
    blocker_percent: Option<f64>,
}

/// The rules of docs/implementation.md and spec §4:
/// - a suggestion once per target account and hour;
/// - a login warning once per account and level per run;
/// - an unknown live login once per identity per run;
/// - "usable again" once per account and reset;
/// - "all accounts full" when the last usable account fills up;
/// - each wait once per session and start, and its end when it is over.
#[derive(Debug, Default)]
pub(crate) struct Deduper {
    /// One entry per target account and hour (spec §2.5).
    suggestions: HashSet<(String, i64)>,
    warnings: HashSet<(String, WarningLevel)>,
    /// Every unknown identity seen this run, so A -> B -> A announces A once.
    unknown: HashSet<(Option<String>, Option<String>)>,
    /// The accounts as the previous snapshot showed them.
    previous: HashMap<String, PreviousView>,
    /// Whether any view was usable in the previous snapshot.
    previous_any_usable: bool,
    /// The first snapshot only fills [`Self::previous`] and
    /// [`Self::previous_any_usable`]; it announces neither "usable again" nor
    /// "all accounts full".
    initialized: bool,
    /// One entry per account id and its previous reset minute.
    usable_again: HashSet<(String, Option<i64>)>,
    /// The `since` already announced for each waiting session.
    waiting_notified: HashMap<String, DateTime<Utc>>,
}

/// Whether a waiting session's notification is wanted (spec §4.1): this host
/// always, another host only when `notifyWaitingHosts` is all hosts.
fn notifies(waiting: &Waiting, settings: &Settings) -> bool {
    match waiting.host {
        None => true,
        Some(_) => settings.notify_waiting_hosts == WaitingHosts::AllHosts,
    }
}

impl Deduper {
    pub(crate) fn new() -> Deduper {
        Deduper::default()
    }

    /// The notifications this snapshot adds, in the order they are sent.
    pub(crate) fn due(&mut self, snapshot: &Snapshot, now: DateTime<Utc>) -> Vec<Notification> {
        let mut due = Vec::new();

        if let Some(suggestion) = snapshot
            .burn
            .as_ref()
            .and_then(|burn| burn.suggestion.as_ref())
        {
            // One notification per target and hour: a suggestion repeated by
            // the next snapshot in the same hour is the same suggestion.
            let key = (suggestion.to.clone(), now.timestamp().div_euclid(3600));
            if self.suggestions.insert(key) {
                due.push(Notification::Suggestion(suggestion.clone()));
            }
        }

        if snapshot
            .settings
            .as_ref()
            .is_some_and(|settings| settings.notify_login_ends)
        {
            for warning in &snapshot.warnings {
                let id = warning_id(warning);
                let key = (id.to_string(), WarningLevel::of(warning));
                if self.warnings.insert(key) {
                    due.push(Notification::LoginWarning(warning.clone()));
                }
            }
        }

        if let LiveView::Unknown { email, org_name } = &snapshot.live {
            let key = (email.clone(), org_name.clone());
            if self.unknown.insert(key.clone()) {
                due.push(Notification::UnknownLogin {
                    email: key.0,
                    org_name: key.1,
                });
            }
        }

        let any_usable = snapshot.views.iter().any(|view| view.usable);
        if self.initialized {
            self.usable_again(snapshot, now, &mut due);
            self.all_full(snapshot, &mut due);
        }
        self.waiting(snapshot, now, &mut due);

        self.previous = snapshot
            .views
            .iter()
            .map(|view| {
                (
                    view.id.clone(),
                    PreviousView {
                        usable: view.usable,
                        blocked_until: view.blocked_until,
                        blocker: view.blocker.clone(),
                        blocker_percent: view
                            .blocker
                            .as_ref()
                            .and_then(|blocker| blocker_percent(view, blocker)),
                    },
                )
            })
            .collect();
        self.previous_any_usable = any_usable;
        self.initialized = true;

        due
    }

    /// When the earliest waiting session that is not yet old enough reaches
    /// 15 s, so the caller does not wait for the next sessions pass.
    pub(crate) fn recheck_at(
        &self,
        snapshot: &Snapshot,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let sessions = snapshot.sessions.as_ref()?;
        let settings = snapshot.settings.as_ref()?;
        let mut earliest: Option<DateTime<Utc>> = None;
        for waiting in &sessions.waiting {
            if !notifies(waiting, settings) {
                continue;
            }
            if self.waiting_notified.get(&waiting.session) == Some(&waiting.since) {
                continue;
            }
            let at = waiting.since + chrono::Duration::seconds(15);
            if at <= now {
                continue;
            }
            if earliest.is_none_or(|current| at < current) {
                earliest = Some(at);
            }
        }
        earliest
    }

    fn usable_again(
        &mut self,
        snapshot: &Snapshot,
        now: DateTime<Utc>,
        due: &mut Vec<Notification>,
    ) {
        if !snapshot
            .settings
            .as_ref()
            .is_some_and(|settings| settings.notify_usable_again)
        {
            return;
        }
        for view in &snapshot.views {
            if !view.usable || view.is_active {
                continue;
            }
            let Some(previous) = self.previous.get(&view.id) else {
                continue;
            };
            let Some(blocker) = &previous.blocker else {
                continue;
            };
            if previous.usable {
                continue;
            }
            // A reset that has passed ends a block, as it always has. Before
            // its time, only a drop of the blocking limit's percent by at
            // least RESET_DROP_POINTS is a reset (decision 12): a per-model
            // block whose last session closed, or one the server lifted early,
            // keeps the limit's percent and sends nothing (spec §4).
            let Some(reset) = previous.blocked_until else {
                continue;
            };
            let early = is_reset_drop(previous.blocker_percent, view, blocker, reset, now);
            if reset > now && !early {
                continue;
            }
            let key = (view.id.clone(), Some(reset.timestamp() / 60));
            if self.usable_again.insert(key) {
                due.push(Notification::UsableAgain {
                    id: view.id.clone(),
                    reason: if early {
                        early_reset_reason(blocker)
                    } else {
                        blocker_reason(blocker)
                    },
                });
            }
        }
    }

    /// "All accounts are full" only when the previous snapshot had a usable
    /// view and [`all_blocked`] says every account whose login still works is
    /// blocked by a limit. An account whose usage is unknown is not usable, but
    /// it is not blocked either, so it does not count (spec §4).
    fn all_full(&self, snapshot: &Snapshot, due: &mut Vec<Notification>) {
        if !self.previous_any_usable || !all_blocked(&snapshot.views) {
            return;
        }
        let mut next: Option<(String, DateTime<Utc>)> = None;
        for view in snapshot
            .views
            .iter()
            .filter(|view| view.login_state == LoginState::Ok)
        {
            let Some(blocked_until) = view.blocked_until else {
                continue;
            };
            if next
                .as_ref()
                .is_none_or(|(_, earliest)| blocked_until < *earliest)
            {
                next = Some((view.id.clone(), blocked_until));
            }
        }
        due.push(Notification::AllFull { next });
    }

    fn waiting(&mut self, snapshot: &Snapshot, now: DateTime<Utc>, due: &mut Vec<Notification>) {
        let Some(settings) = snapshot.settings.as_ref() else {
            return;
        };
        if !settings.notify_waiting {
            // A shown notification is withdrawn even when the setting is off;
            // nothing new is sent while it is off (spec §4.1).
            let shown: Vec<String> = self.waiting_notified.keys().cloned().collect();
            for session in shown {
                self.waiting_notified.remove(&session);
                due.push(Notification::WaitingOver { session });
            }
            return;
        }
        let Some(sessions) = &snapshot.sessions else {
            return;
        };
        let current: Vec<&Waiting> = sessions
            .waiting
            .iter()
            .filter(|waiting| notifies(waiting, settings))
            .collect();
        for waiting in &current {
            if now.signed_duration_since(waiting.since) < chrono::Duration::seconds(15) {
                continue;
            }
            if self.waiting_notified.get(&waiting.session) == Some(&waiting.since) {
                continue;
            }
            self.waiting_notified
                .insert(waiting.session.clone(), waiting.since);
            due.push(Notification::Waiting((*waiting).clone()));
        }
        let current: HashSet<&str> = current
            .iter()
            .map(|waiting| waiting.session.as_str())
            .collect();
        let over: Vec<String> = self
            .waiting_notified
            .keys()
            .filter(|session| !current.contains(session.as_str()))
            .cloned()
            .collect();
        for session in over {
            self.waiting_notified.remove(&session);
            due.push(Notification::WaitingOver { session });
        }
    }
}

/// The percent of the limit `blocker` names in `view`'s usage, when this
/// snapshot has that limit.
fn blocker_percent(view: &AccountView, blocker: &Blocker) -> Option<f64> {
    let usage = view.usage.as_ref()?;
    blocker.limit_in(usage).map(|limit| limit.percent)
}

/// Whether `view`'s limit that `blocker` names was reset early at `now`:
/// [`is_early_reset`]'s one rule (decision 12), the history's. A drop in the
/// last [`RESET_JITTER`](seatswap_core::history::RESET_JITTER) before the reset time is not early: the block ends at
/// its reset time a moment later, as any block does.
fn is_reset_drop(
    previous_percent: Option<f64>,
    view: &AccountView,
    blocker: &Blocker,
    reset: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    let (Some(previous_percent), Some(current)) =
        (previous_percent, blocker_percent(view, blocker))
    else {
        return false;
    };
    is_early_reset(previous_percent, current, Some(reset), now)
}

/// The limit a [`Blocker`] names, worded as "usable again" uses it.
fn limit_name(blocker: &Blocker) -> String {
    match blocker {
        Blocker::Week => "its week".to_string(),
        Blocker::Session => "its 5h window".to_string(),
        Blocker::Model(model) => format!("its {model} week"),
    }
}

/// What the engine's [`Blocker`] says in "usable again" when its reset time
/// has passed (spec §4).
fn blocker_reason(blocker: &Blocker) -> String {
    format!("{} reset", limit_name(blocker))
}

/// The same limit wording for a reset seen before its reset time, as a drop of
/// the limit's percent (decision 12).
fn early_reset_reason(blocker: &Blocker) -> String {
    format!("{} was reset early", limit_name(blocker))
}

/// The account id a login warning is about. One definition, used by the
/// deduplicator here and by the snapshot's notification text.
pub(crate) fn warning_id(warning: &LoginWarning) -> &str {
    match warning {
        LoginWarning::Soon { id, .. }
        | LoginWarning::Tomorrow { id }
        | LoginWarning::Ended { id } => id,
    }
}

/// notify-rust's `show()` can block while it talks to the notification daemon,
/// so it never runs on the caller's thread — in particular never on Tauri's
/// main thread, which builds every snapshot.
fn notify_in_background(job: impl FnOnce() + Send + 'static) {
    let spawned = thread::Builder::new()
        .name("seatswap-notify".to_string())
        .spawn(job);
    if let Err(error) = spawned {
        eprintln!("seatswap: notification thread: {error}");
    }
}

/// The file a notification's icon is read from: `<data_dir>/notification-icon.png`,
/// written when it is missing or holds another picture (a new data folder, one
/// a cleaner emptied, or the icon of an older seatswap). The path comes from
/// the app's [`AppPaths`], never from a shared temp folder. A write that fails
/// returns an error naming the path, so the notification can still go out
/// without an image and the failure is printed.
fn notification_icon(paths: &AppPaths) -> Result<PathBuf, String> {
    let path = paths.notification_icon();
    if std::fs::read(&path).is_ok_and(|bytes| bytes == APP_ICON_PNG) {
        return Ok(path);
    }
    seatswap_core::atomic::write_atomic(&path, APP_ICON_PNG, seatswap_core::atomic::Access::Shared)
        .map_err(|error| format!("icon {}: {error}", path.display()))?;
    Ok(path)
}

/// Every notification starts here: the summary, the app icon, and on Windows
/// the app id it is shown under. The only place that calls
/// `DesktopNotification::new()`.
fn base(paths: &AppPaths) -> DesktopNotification {
    let mut notification = DesktopNotification::new();
    notification.summary("seatswap");
    match notification_icon(paths) {
        Ok(icon) => {
            notification.image_path(&icon.to_string_lossy());
        }
        Err(error) => eprintln!("seatswap: notification: {error}"),
    }
    #[cfg(windows)]
    notification.app_id(registry::APP_ID);
    notification
}

/// Give [`registry::APP_ID`] the display name "seatswap", so Windows shows
/// notifications under that name (Microsoft's way for programs without an
/// installer). Called once at startup, before the first notification.
#[cfg(windows)]
pub(crate) fn register_app_id() -> Result<(), String> {
    registry::set_string(registry::APP_ID_KEY, "DisplayName", "seatswap").map_err(|error| {
        format!(
            "registry HKEY_CURRENT_USER\\{}: {error}",
            registry::APP_ID_KEY
        )
    })
}

pub(crate) fn send_switch(paths: &AppPaths, message: &str) {
    let body = message.to_string();
    let paths = paths.clone();
    notify_in_background(move || {
        let _ = base(&paths).body(&body).show();
    });
}

pub(crate) fn send_error(paths: &AppPaths, message: &str) {
    let body = message.to_string();
    let paths = paths.clone();
    notify_in_background(move || {
        let _ = base(&paths).body(&body).show();
    });
}

/// The body of a login warning's notification.
fn login_warning_text(account: &str, warning: &LoginWarning) -> String {
    let ends = match warning {
        LoginWarning::Soon { days_left, .. } => crate::format::login_ends_text(*days_left),
        LoginWarning::Tomorrow { .. } => "login ends tomorrow".to_string(),
        LoginWarning::Ended { .. } => "login has ended".to_string(),
    };
    format!("{account} — the {ends}. Run `claude` and `/login` to renew it.")
}

pub(crate) fn send_login_warning(paths: &AppPaths, warning: &LoginWarning, account: &str) {
    let body = login_warning_text(account, warning);
    let paths = paths.clone();
    notify_in_background(move || {
        let _ = base(&paths).body(&body).show();
    });
}

/// What the person clicked in a notification that offers a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    Button,
    Body,
}

/// The click behind `response`: on the button with the action id `button`, or
/// on the notification's body. `None` for everything that is not a click: the
/// notification timed out, was dismissed or closed, or answered with an action
/// we did not offer. Those must do nothing.
fn click(response: &NotificationResponse, button: &str) -> Option<Click> {
    match response {
        NotificationResponse::Action(action) if action == button => Some(Click::Button),
        NotificationResponse::Default => Some(Click::Body),
        _ => None,
    }
}

/// A notification that can be clicked, without its text and button yet.
///
/// A Linux notification service reports a click on the body only as the action
/// `default`, and only when the notification announces that action (it shows no
/// button for it). Windows and macOS report a body click by themselves and
/// would draw a button for the action (on macOS a second action even turns the
/// buttons into a drop-down), so it is announced on Linux only.
fn clickable(paths: &AppPaths) -> DesktopNotification {
    let mut notification = base(paths);
    if cfg!(all(unix, not(target_os = "macos"))) {
        notification.action("default", "Open");
    }
    notification
}

/// A switch suggestion with a "Switch" action. Clicking the body opens the
/// panel instead (spec §7.4).
pub(crate) fn send_suggestion(
    paths: &AppPaths,
    app: tauri::AppHandle,
    suggestion: &SwitchSuggestion,
) {
    let body = suggestion.text.clone();
    let to_id = suggestion.to.clone();
    let paths = paths.clone();
    // Waiting for an action blocks, so it gets its own thread.
    thread::spawn(move || {
        let shown = clickable(&paths)
            .body(&body)
            .action("switch", "Switch")
            .show();
        let answered = shown.and_then(|handle| {
            handle.wait_for_response(move |response: &NotificationResponse| {
                match click(response, "switch") {
                    Some(Click::Button) => {
                        super::request_switch(&app, to_id, UnsavedPolicy::Refuse, None);
                    }
                    Some(Click::Body) => {
                        let handle = app.clone();
                        let _ = app.run_on_main_thread(move || super::show_panel(&handle, "main"));
                    }
                    None => {}
                }
            })
        });
        if let Err(error) = answered {
            eprintln!("seatswap: notification: {error}");
        }
    });
}

/// "New login found: <email>. Save it as an account?" — a click on the button
/// or on the body opens the add view.
pub(crate) fn send_unknown_login(
    paths: &AppPaths,
    app: tauri::AppHandle,
    email: Option<&str>,
    org_name: Option<&str>,
) {
    let who = email.or(org_name).unwrap_or("unknown account");
    let body = format!("New login found: {who}. Save it as an account?");
    let paths = paths.clone();
    thread::spawn(move || {
        let shown = clickable(&paths)
            .body(&body)
            .action("open", "Add account…")
            .show();
        let answered = shown.and_then(|handle| {
            handle.wait_for_response(move |response: &NotificationResponse| {
                if click(response, "open").is_some() {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || super::show_panel(&handle, "add"));
                }
            })
        });
        if let Err(error) = answered {
            eprintln!("seatswap: notification: {error}");
        }
    });
}

/// `PR is usable again: its week reset.`
fn usable_again_text(account: &str, reason: &str) -> String {
    format!("{account} is usable again: {reason}.")
}

/// `All accounts are full. Next: PR at 14:40.` — or without a next account.
fn all_full_text(next: Option<(&str, &str)>) -> String {
    match next {
        Some((abbr, time)) => format!("All accounts are full. Next: {abbr} at {time}."),
        None => "All accounts are full.".to_string(),
    }
}

/// `Fix the invoice export is waiting for you (acme-kb, on atlas).` — the
/// session named by Claude Code's own title when it is known, else by its
/// label (`acme-kb-dd`); the waiting session's own host when it is another
/// one, this host's name otherwise; the host part is left out when there is no
/// acceptable host name (spec §4.1).
fn waiting_text(waiting: &Waiting, this_host: Option<&str>) -> String {
    let name = waiting.title.as_deref().unwrap_or(&waiting.label);
    match waiting.host.as_deref().or(this_host) {
        Some(host) => format!(
            "{} is waiting for you ({}, on {host}).",
            name, waiting.project
        ),
        None => format!("{} is waiting for you ({}).", name, waiting.project),
    }
}

/// "PR is usable again: its week reset." with a "Switch" action; a click on
/// the text opens the pop-up (spec §4).
pub(crate) fn send_usable_again(
    paths: &AppPaths,
    account: &str,
    reason: &str,
    on_switch: impl FnOnce() + Send + 'static,
    on_open: impl FnOnce() + Send + 'static,
) {
    let body = usable_again_text(account, reason);
    let paths = paths.clone();
    notify_in_background(move || {
        let shown = clickable(&paths)
            .body(&body)
            .action("switch", "Switch")
            .show();
        let answered = shown.and_then(|handle| {
            handle.wait_for_response(move |response: &NotificationResponse| {
                match click(response, "switch") {
                    Some(Click::Button) => on_switch(),
                    Some(Click::Body) => on_open(),
                    None => {}
                }
            })
        });
        if let Err(error) = answered {
            eprintln!("seatswap: notification: {error}");
        }
    });
}

/// "All accounts are full." plus the next reset when there is one. Not
/// clickable (spec §4).
pub(crate) fn send_all_full(paths: &AppPaths, next: Option<(&str, &str)>) {
    let body = all_full_text(next);
    let paths = paths.clone();
    notify_in_background(move || {
        let _ = base(&paths).body(&body).show();
    });
}

/// A waiting session's notification that stays until it is clicked or
/// withdrawn (spec §4.1).
///
/// `host` is this host's name, used for a session with no host of its own
/// (`None` in [`Waiting`]); another host's name is in the waiting session.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn send_waiting(
    paths: &AppPaths,
    waiting: &Waiting,
    host: Option<&str>,
    on_open: impl FnOnce() + Send + 'static,
) -> WaitingHandle {
    let body = waiting_text(waiting, host);
    let state = Arc::new(WaitingState::default());
    let thread_state = Arc::clone(&state);
    let thread_paths = paths.clone();
    notify_in_background(move || {
        match clickable(&thread_paths)
            .body(&body)
            .timeout(Timeout::Never)
            .show()
        {
            Ok(handle) => {
                let id = handle.id();
                let withdrawn = {
                    let mut slot = thread_state
                        .id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *slot = Some(id);
                    thread_state.withdrawn.load(Ordering::SeqCst)
                };
                if withdrawn {
                    close_by_id(id, &thread_paths);
                    return;
                }
                let answered = handle.wait_for_response(move |response: &NotificationResponse| {
                    if click(response, "default").is_some() {
                        on_open();
                    }
                });
                if let Err(error) = answered {
                    eprintln!("seatswap: notification: {error}");
                }
            }
            Err(error) => eprintln!("seatswap: notification: {error}"),
        }
    });
    WaitingHandle {
        state,
        paths: paths.clone(),
    }
}

/// On Windows a toast, and on macOS a notification, follows the system's
/// rules: it is shown once, may time out, and the pop-up keeps the count
/// (spec §4.1). notify-rust 4.19's macOS handle has no id and no close, like
/// its Windows one, and reports a click on the body by itself.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn send_waiting(
    paths: &AppPaths,
    waiting: &Waiting,
    host: Option<&str>,
    on_open: impl FnOnce() + Send + 'static,
) -> WaitingHandle {
    let body = waiting_text(waiting, host);
    let paths = paths.clone();
    notify_in_background(move || match clickable(&paths).body(&body).show() {
        Ok(handle) => {
            let answered = handle.wait_for_response(move |response: &NotificationResponse| {
                if click(response, "default").is_some() {
                    on_open();
                }
            });
            if let Err(error) = answered {
                eprintln!("seatswap: notification: {error}");
            }
        }
        Err(error) => eprintln!("seatswap: notification: {error}"),
    });
    WaitingHandle {}
}

/// The handle that can withdraw a waiting notification before the person
/// clicks it.
pub(crate) struct WaitingHandle {
    #[cfg(all(unix, not(target_os = "macos")))]
    state: Arc<WaitingState>,
    /// The paths the notification was shown with, so closing it can name the
    /// same icon.
    #[cfg(all(unix, not(target_os = "macos")))]
    paths: AppPaths,
}

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Default)]
struct WaitingState {
    /// The notification's id, set by the showing thread before it waits.
    id: Mutex<Option<u32>>,
    /// Set by a withdraw that ran before the notification was shown, so the
    /// showing thread closes it itself.
    withdrawn: AtomicBool,
}

impl WaitingHandle {
    /// Take the notification back (spec §4.1). A click after this does
    /// nothing.
    pub(crate) fn withdraw(self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let id = {
                let slot = self
                    .state
                    .id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.state.withdrawn.store(true, Ordering::SeqCst);
                *slot
            };
            if let Some(id) = id {
                close_by_id(id, &self.paths);
            }
        }
        // Windows and macOS remove notifications by their own rules;
        // notify-rust 4.19 gives no id on its handle there, so there is
        // nothing to close.
        #[cfg(any(windows, target_os = "macos"))]
        let _ = self;
    }
}

/// Close the notification with `id`: notify-rust 4.19 has no other way to
/// close a notification whose handle waits for a click. Showing a notification
/// built from [`base`] with the same id replaces that one, and the immediate
/// `close` takes it away.
#[cfg(all(unix, not(target_os = "macos")))]
fn close_by_id(id: u32, paths: &AppPaths) {
    let paths = paths.clone();
    notify_in_background(move || {
        if let Ok(handle) = base(&paths).id(id).show() {
            handle.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::DateTime;
    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::store::{LoginState, Settings, WaitingHosts};

    use crate::format;
    use crate::gui::burn_rt::BurnView;
    use crate::gui::worker::SessionsSummary;

    fn snapshot(
        suggestion: Option<SwitchSuggestion>,
        warnings: Vec<LoginWarning>,
        live: LiveView,
    ) -> Snapshot {
        Snapshot {
            views: Vec::new(),
            accounts: Vec::new(),
            provider_subscriptions: Vec::new(),
            codex_logins: Vec::new(),
            active_id: None,
            live,
            burn: suggestion.map(|suggestion| BurnView {
                suggestion: Some(suggestion),
                ..BurnView::default()
            }),
            warnings,
            pending: None,
            secrets_warning: None,
            last_poll_at: None,
            error: None,
            sessions: None,
            providers: Vec::new(),
            settings: Some(Settings::default()),
            claude_config_dir: String::new(),
            claude_not_set_up: false,
        }
    }

    fn suggestion(to: &str, text: &str) -> SwitchSuggestion {
        SwitchSuggestion {
            to: to.to_string(),
            text: text.to_string(),
        }
    }

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a valid test time")
            .with_timezone(&Utc)
    }

    fn view(
        id: &str,
        abbr: &str,
        usable: bool,
        blocked_until: Option<DateTime<Utc>>,
        is_active: bool,
    ) -> AccountView {
        AccountView {
            id: id.to_string(),
            label: id.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            auto_burn: true,
            is_active,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable,
            blocked_until,
            blocker: None,
            fetched_at: None,
            last_error: None,
        }
    }

    fn blocked_view(
        id: &str,
        abbr: &str,
        blocked_until: Option<DateTime<Utc>>,
        is_active: bool,
        blocker: Blocker,
    ) -> AccountView {
        let mut view = view(id, abbr, false, blocked_until, is_active);
        view.blocker = Some(blocker);
        view
    }

    /// A usage answer with `limits`, as the engine's views carry it.
    fn usage_of(limits: Vec<Limit>) -> Usage {
        Usage {
            limits,
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn limit(
        kind: LimitKind,
        percent: f64,
        resets_at: Option<DateTime<Utc>>,
        model: Option<&str>,
        is_active: bool,
    ) -> Limit {
        Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at,
            model: model.map(str::to_string),
            is_active,
        }
    }

    /// `view` with `limits` as the usage the engine read.
    fn with_usage(mut view: AccountView, limits: Vec<Limit>) -> AccountView {
        view.usage = Some(usage_of(limits));
        view
    }

    fn with_views(
        views: Vec<AccountView>,
        sessions: Option<SessionsSummary>,
        settings: Option<Settings>,
    ) -> Snapshot {
        Snapshot {
            accounts: Vec::new(),
            provider_subscriptions: Vec::new(),
            codex_logins: Vec::new(),
            active_id: views
                .iter()
                .find(|view| view.is_active)
                .map(|view| view.id.clone()),
            views,
            live: LiveView::NoLogin,
            burn: None,
            warnings: Vec::new(),
            pending: None,
            secrets_warning: None,
            last_poll_at: None,
            error: None,
            sessions,
            providers: Vec::new(),
            settings,
            claude_config_dir: String::new(),
            claude_not_set_up: false,
        }
    }

    fn waiting(since: DateTime<Utc>) -> Waiting {
        Waiting {
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            label: "acme-kb-dd".to_string(),
            title: None,
            project: "acme-kb".to_string(),
            since,
            host: None,
        }
    }

    fn sessions(waiting: Vec<Waiting>, busy: u32) -> SessionsSummary {
        SessionsSummary {
            running: waiting.len() as u32 + busy,
            busy,
            tied: 0,
            tied_busy: 0,
            waiting,
            unavailable: None,
            warning: None,
        }
    }

    #[test]
    fn only_a_click_on_the_button_or_the_body_counts() {
        use notify_rust::CloseReason;

        let button = NotificationResponse::Action("switch".to_string());
        assert_eq!(click(&button, "switch"), Some(Click::Button));
        assert_eq!(
            click(&NotificationResponse::Default, "switch"),
            Some(Click::Body)
        );
        // A notification that times out, is dismissed or is closed by the
        // service is not a click, and neither is an action we did not offer.
        for closed in [
            CloseReason::Expired,
            CloseReason::Dismissed,
            CloseReason::CloseAction,
            CloseReason::Other(4),
        ] {
            assert_eq!(click(&NotificationResponse::Closed(closed), "switch"), None);
        }
        let other = NotificationResponse::Action("open".to_string());
        assert_eq!(click(&other, "switch"), None);
        let reply = NotificationResponse::Reply("yes".to_string());
        assert_eq!(click(&reply, "switch"), None);
    }

    #[test]
    fn notifications_run_off_the_calling_thread() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let caller = thread::current().id();
        notify_in_background(move || {
            let _ = sender.send(thread::current().id());
        });
        let worker = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the notification thread runs the job");
        assert_ne!(worker, caller, "show() must not run on the calling thread");
    }

    #[test]
    fn an_unknown_login_a_b_a_notifies_a_once() {
        let mut deduper = Deduper::new();
        let a = snapshot(
            None,
            Vec::new(),
            LiveView::Unknown {
                email: Some("a@example.com".to_string()),
                org_name: None,
            },
        );
        let b = snapshot(
            None,
            Vec::new(),
            LiveView::Unknown {
                email: Some("b@example.com".to_string()),
                org_name: None,
            },
        );
        let now = Utc::now();
        assert_eq!(deduper.due(&a, now).len(), 1);
        assert_eq!(deduper.due(&b, now).len(), 1);
        assert!(
            deduper.due(&a, now).is_empty(),
            "a login already announced this run must stay quiet"
        );
    }

    #[test]
    fn a_suggestion_to_the_same_target_fires_once_an_hour() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T12:00:00Z");
        let a_to_p = snapshot(
            Some(suggestion("p", "Switch to P now: its 5h window is open.")),
            Vec::new(),
            LiveView::NoLogin,
        );
        let a_to_p_again = snapshot(
            Some(suggestion("p", "Switch to P now: its 5h window is open.")),
            Vec::new(),
            LiveView::NoLogin,
        );
        assert_eq!(deduper.due(&a_to_p, now).len(), 1);
        assert!(
            deduper.due(&a_to_p_again, now).is_empty(),
            "a suggestion to the same target announced this hour must stay quiet"
        );
    }

    #[test]
    fn a_suggestion_is_keyed_by_the_hour() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T12:00:00Z");
        let first = snapshot(
            Some(suggestion(
                "p",
                "Switch to P now: its week resets on Mon 02:00.",
            )),
            Vec::new(),
            LiveView::NoLogin,
        );
        assert_eq!(deduper.due(&first, now).len(), 1);
        assert!(deduper
            .due(&first, now + chrono::Duration::minutes(59))
            .is_empty());
        assert_eq!(
            deduper.due(&first, now + chrono::Duration::hours(1)).len(),
            1,
            "the next hour may announce the same suggestion again"
        );
    }

    #[test]
    fn different_targets_are_announced_separately() {
        let mut deduper = Deduper::new();
        let now = Utc::now();
        let to_p = snapshot(
            Some(suggestion("p", "Switch to P now: its 5h window is open.")),
            Vec::new(),
            LiveView::NoLogin,
        );
        let to_ac = snapshot(
            Some(suggestion(
                "ac",
                "Switch to AC now: its week resets on Mon 02:00.",
            )),
            Vec::new(),
            LiveView::NoLogin,
        );
        assert_eq!(deduper.due(&to_p, now).len(), 1);
        assert_eq!(deduper.due(&to_ac, now).len(), 1);
    }

    #[test]
    fn a_warning_fires_once_per_account_and_level() {
        let mut deduper = Deduper::new();
        let soon = LoginWarning::Soon {
            id: "as".to_string(),
            days_left: 4,
        };
        let now = Utc::now();
        let first = snapshot(None, vec![soon.clone()], LiveView::NoLogin);
        assert_eq!(deduper.due(&first, now).len(), 1);

        // Same level, changed day count: still the same warning.
        let soon_again = LoginWarning::Soon {
            id: "as".to_string(),
            days_left: 3,
        };
        let again = snapshot(None, vec![soon_again], LiveView::NoLogin);
        assert!(deduper.due(&again, now).is_empty());

        // A different level for the same account is a new warning.
        let tomorrow = LoginWarning::Tomorrow {
            id: "as".to_string(),
        };
        let next = snapshot(None, vec![tomorrow], LiveView::NoLogin);
        assert_eq!(deduper.due(&next, now).len(), 1);

        // A different account is new too.
        let other = LoginWarning::Ended {
            id: "p".to_string(),
        };
        let other = snapshot(None, vec![other], LiveView::NoLogin);
        assert_eq!(deduper.due(&other, now).len(), 1);
    }

    #[test]
    fn a_login_warning_speaks_days_as_the_cli_does() {
        // The 5-day warning: `ends in 5d`, not "5 days". The 1-day warning is
        // `Tomorrow`, which carries no day count and keeps its word.
        let soon = LoginWarning::Soon {
            id: "lb".to_string(),
            days_left: 5,
        };
        assert_eq!(
            login_warning_text("LB", &soon),
            "LB — the login ends in 5d. Run `claude` and `/login` to renew it."
        );

        let tomorrow = LoginWarning::Tomorrow {
            id: "ac".to_string(),
        };
        assert_eq!(
            login_warning_text("AC", &tomorrow),
            "AC — the login ends tomorrow. Run `claude` and `/login` to renew it."
        );
    }

    #[test]
    fn login_warnings_follow_their_setting() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T12:00:00Z");
        let warning = LoginWarning::Soon {
            id: "lb".to_string(),
            days_left: 4,
        };

        let mut off = snapshot(None, vec![warning.clone()], LiveView::NoLogin);
        off.settings = Some(Settings {
            notify_login_ends: false,
            ..Settings::default()
        });
        assert!(
            deduper.due(&off, now).is_empty(),
            "the login warnings are off"
        );

        let on = snapshot(None, vec![warning], LiveView::NoLogin);
        let due = deduper.due(&on, now);
        assert_eq!(due.len(), 1, "the login warnings are on again");
        assert!(matches!(due[0], Notification::LoginWarning(_)));
    }

    #[test]
    fn an_unknown_login_fires_once_per_identity() {
        let mut deduper = Deduper::new();
        let live = LiveView::Unknown {
            email: Some("new@example.com".to_string()),
            org_name: Some("Acme".to_string()),
        };
        let now = Utc::now();
        let first = snapshot(None, Vec::new(), live.clone());
        assert_eq!(deduper.due(&first, now).len(), 1);
        assert!(deduper.due(&first, now).is_empty());

        let other = snapshot(
            None,
            Vec::new(),
            LiveView::Unknown {
                email: Some("other@example.com".to_string()),
                org_name: Some("Acme".to_string()),
            },
        );
        assert_eq!(deduper.due(&other, now).len(), 1);
    }

    #[test]
    fn usable_again_fires_once_when_a_blocked_account_returns() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:35:00Z");
        let blocked_until = Some(utc("2026-11-10T14:40:00Z"));

        let blocked = with_views(
            vec![blocked_view(
                "AC",
                "AC",
                blocked_until,
                false,
                Blocker::Week,
            )],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&blocked, now).is_empty());

        let usable = with_views(
            vec![view("AC", "AC", true, None, false)],
            None,
            Some(Settings::default()),
        );
        assert_eq!(
            deduper.due(&usable, now + chrono::Duration::minutes(5)),
            vec![Notification::UsableAgain {
                id: "AC".to_string(),
                reason: "its week reset".to_string(),
            }]
        );
        assert!(deduper
            .due(&usable, now + chrono::Duration::minutes(6))
            .is_empty());
    }

    #[test]
    fn usable_again_is_not_sent_for_the_active_account() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:35:00Z");
        let blocked_until = Some(utc("2026-11-10T14:40:00Z"));

        let blocked = with_views(
            vec![blocked_view(
                "AC",
                "AC",
                blocked_until,
                false,
                Blocker::Week,
            )],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&blocked, now).is_empty());

        let usable = with_views(
            vec![view("AC", "AC", true, None, true)],
            None,
            Some(Settings::default()),
        );
        assert!(deduper
            .due(&usable, now + chrono::Duration::minutes(5))
            .is_empty());
    }

    #[test]
    fn usable_again_respects_the_setting() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:35:00Z");
        let blocked_until = Some(utc("2026-11-10T14:40:00Z"));
        let settings = Settings {
            notify_usable_again: false,
            ..Settings::default()
        };

        let blocked = with_views(
            vec![blocked_view(
                "AC",
                "AC",
                blocked_until,
                false,
                Blocker::Week,
            )],
            None,
            Some(settings.clone()),
        );
        assert!(deduper.due(&blocked, now).is_empty());

        let usable = with_views(
            vec![view("AC", "AC", true, None, false)],
            None,
            Some(settings),
        );
        assert!(deduper
            .due(&usable, now + chrono::Duration::minutes(5))
            .is_empty());
    }

    #[test]
    fn usable_again_names_the_limit_that_blocked() {
        let now = utc("2026-11-10T14:35:00Z");
        let blocked_until = Some(utc("2026-11-10T14:40:00Z"));
        for (blocker, reason) in [
            (Blocker::Week, "its week reset"),
            (Blocker::Session, "its 5h window reset"),
            (Blocker::Model("opus".to_string()), "its opus week reset"),
        ] {
            let mut deduper = Deduper::new();
            let blocked = with_views(
                vec![blocked_view("AC", "AC", blocked_until, false, blocker)],
                None,
                Some(Settings::default()),
            );
            assert!(deduper.due(&blocked, now).is_empty());

            let usable = with_views(
                vec![view("AC", "AC", true, None, false)],
                None,
                Some(Settings::default()),
            );
            assert_eq!(
                deduper.due(&usable, now + chrono::Duration::minutes(5)),
                vec![Notification::UsableAgain {
                    id: "AC".to_string(),
                    reason: reason.to_string(),
                }],
                "{reason}"
            );
        }
    }

    #[test]
    fn a_reset_before_its_time_is_usable_again_at_once() {
        let mut deduper = Deduper::new();
        let reset = utc("2026-11-14T18:59:00Z");
        let limits = |percent: f64| {
            vec![limit(
                LimitKind::WeeklyAll,
                percent,
                Some(reset),
                None,
                true,
            )]
        };
        let blocked = with_usage(
            blocked_view("AC", "AC", Some(reset), false, Blocker::Week),
            limits(100.0),
        );
        let usable = with_usage(view("AC", "AC", true, None, false), limits(0.0));

        assert!(deduper
            .due(&snapshot_of(vec![blocked]), utc("2026-11-10T09:55:00Z"))
            .is_empty());
        assert_eq!(
            deduper.due(&snapshot_of(vec![usable]), utc("2026-11-10T10:00:00Z")),
            vec![Notification::UsableAgain {
                id: "AC".to_string(),
                reason: "its week was reset early".to_string(),
            }],
            "the week dropped to 0 % before Sat 18:59"
        );
    }

    #[test]
    fn a_model_reset_is_usable_again_at_once() {
        let mut deduper = Deduper::new();
        let reset = utc("2026-11-16T16:00:00Z");
        let limits = |week: f64, fable: f64, active: bool| {
            vec![
                limit(LimitKind::WeeklyAll, week, Some(reset), None, false),
                limit(
                    LimitKind::WeeklyScoped,
                    fable,
                    Some(reset),
                    Some("Fable"),
                    active,
                ),
            ]
        };
        let blocked = with_usage(
            blocked_view(
                "AC",
                "AC",
                Some(reset),
                false,
                Blocker::Model("Fable".to_string()),
            ),
            limits(70.0, 100.0, true),
        );
        let usable = with_usage(
            view("AC", "AC", true, None, false),
            limits(45.0, 0.0, false),
        );

        assert!(deduper
            .due(&snapshot_of(vec![blocked]), utc("2026-11-14T09:55:00Z"))
            .is_empty());
        assert_eq!(
            deduper.due(&snapshot_of(vec![usable]), utc("2026-11-14T10:00:00Z")),
            vec![Notification::UsableAgain {
                id: "AC".to_string(),
                reason: "its Fable week was reset early".to_string(),
            }]
        );
    }

    #[test]
    fn a_block_lifted_without_a_drop_is_still_silent() {
        let mut deduper = Deduper::new();
        let reset = utc("2026-11-16T16:00:00Z");
        let limits = |active: bool| {
            vec![limit(
                LimitKind::WeeklyScoped,
                100.0,
                Some(reset),
                Some("Fable"),
                active,
            )]
        };
        let blocked = with_usage(
            blocked_view(
                "AC",
                "AC",
                Some(reset),
                false,
                Blocker::Model("Fable".to_string()),
            ),
            limits(true),
        );
        let usable = with_usage(view("AC", "AC", true, None, false), limits(false));

        assert!(deduper
            .due(&snapshot_of(vec![blocked]), utc("2026-11-14T09:55:00Z"))
            .is_empty());
        assert!(
            deduper
                .due(&snapshot_of(vec![usable]), utc("2026-11-14T10:00:00Z"))
                .is_empty(),
            "the Fable limit stayed at 100 %: the block was lifted, not reset"
        );
    }

    #[test]
    fn an_early_reset_is_not_announced_twice() {
        let mut deduper = Deduper::new();
        let reset = utc("2026-11-14T18:59:00Z");
        let limits = |percent: f64| {
            vec![limit(
                LimitKind::WeeklyAll,
                percent,
                Some(reset),
                None,
                true,
            )]
        };
        let active = view("lb-id", "LB", true, None, true);
        let blocked = || {
            snapshot_of(vec![
                active.clone(),
                with_usage(
                    blocked_view("AC", "AC", Some(reset), false, Blocker::Week),
                    limits(100.0),
                ),
            ])
        };
        let usable = || {
            snapshot_of(vec![
                active.clone(),
                with_usage(view("AC", "AC", true, None, false), limits(0.0)),
            ])
        };

        assert!(deduper
            .due(&blocked(), utc("2026-11-10T09:55:00Z"))
            .is_empty());
        assert_eq!(
            deduper.due(&usable(), utc("2026-11-10T10:00:00Z")),
            vec![Notification::UsableAgain {
                id: "AC".to_string(),
                reason: "its week was reset early".to_string(),
            }]
        );
        // The week fills again before the old reset time, which does not move.
        assert!(deduper
            .due(&blocked(), utc("2026-11-13T09:00:00Z"))
            .is_empty());
        assert!(
            deduper
                .due(&usable(), utc("2026-11-14T19:00:00Z"))
                .is_empty(),
            "the reset at the old time is the one already announced early"
        );
    }

    #[test]
    fn all_full_names_the_next_account() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:00:00Z");

        let some_usable = with_views(
            vec![
                view("AC", "AC", true, None, true),
                blocked_view(
                    "LB",
                    "LB",
                    Some(utc("2026-11-10T16:00:00Z")),
                    false,
                    Blocker::Week,
                ),
            ],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&some_usable, now).is_empty());

        let all_full = with_views(
            vec![
                blocked_view(
                    "AC",
                    "AC",
                    Some(utc("2026-11-10T14:40:00Z")),
                    false,
                    Blocker::Week,
                ),
                blocked_view(
                    "LB",
                    "LB",
                    Some(utc("2026-11-10T16:00:00Z")),
                    false,
                    Blocker::Week,
                ),
            ],
            None,
            Some(Settings::default()),
        );
        assert_eq!(
            deduper.due(&all_full, now + chrono::Duration::minutes(1)),
            vec![Notification::AllFull {
                next: Some(("AC".to_string(), utc("2026-11-10T14:40:00Z"))),
            }]
        );
        assert!(deduper
            .due(&all_full, now + chrono::Duration::minutes(2))
            .is_empty());
    }

    #[test]
    fn the_first_snapshot_sends_nothing() {
        let now = utc("2026-11-10T14:00:00Z");
        let blocked_until = Some(utc("2026-11-10T14:40:00Z"));

        let mut deduper = Deduper::new();
        let pair = with_views(
            vec![
                blocked_view("AC", "AC", blocked_until, false, Blocker::Week),
                view("LB", "LB", true, None, true),
            ],
            None,
            Some(Settings::default()),
        );
        assert!(
            deduper.due(&pair, now).is_empty(),
            "the first snapshot only fills the memory: no UsableAgain and no AllFull"
        );

        let mut deduper = Deduper::new();
        let full = with_views(
            vec![
                blocked_view("AC", "AC", blocked_until, false, Blocker::Week),
                blocked_view(
                    "LB",
                    "LB",
                    Some(utc("2026-11-10T16:00:00Z")),
                    false,
                    Blocker::Week,
                ),
            ],
            None,
            Some(Settings::default()),
        );
        assert!(
            deduper.due(&full, now).is_empty(),
            "a first snapshot that is already full is not a change to full"
        );
    }

    #[test]
    fn the_waiting_notification_names_the_title_when_known() {
        let mut wait = waiting(utc("2026-11-10T09:56:00Z"));
        assert_eq!(
            waiting_text(&wait, Some("atlas")),
            "acme-kb-dd is waiting for you (acme-kb, on atlas)."
        );
        wait.title = Some("Fix the invoice export".to_string());
        assert_eq!(
            waiting_text(&wait, Some("atlas")),
            "Fix the invoice export is waiting for you (acme-kb, on atlas)."
        );
    }

    #[test]
    fn waiting_needs_fifteen_seconds() {
        let mut deduper = Deduper::new();
        let wait = waiting(utc("2026-11-10T09:56:00Z"));

        let early = with_views(
            Vec::new(),
            Some(sessions(vec![wait.clone()], 0)),
            Some(Settings::default()),
        );
        assert!(deduper.due(&early, utc("2026-11-10T09:56:10Z")).is_empty());

        assert_eq!(
            deduper.due(&early, utc("2026-11-10T09:56:15Z")),
            vec![Notification::Waiting(wait)]
        );
        assert!(deduper.due(&early, utc("2026-11-10T09:57:00Z")).is_empty());
    }

    #[test]
    fn waiting_over_withdraws() {
        let mut deduper = Deduper::new();
        let wait = waiting(utc("2026-11-10T09:56:00Z"));

        let waiting_now = with_views(
            Vec::new(),
            Some(sessions(vec![wait.clone()], 0)),
            Some(Settings::default()),
        );
        assert_eq!(
            deduper.due(&waiting_now, utc("2026-11-10T09:56:20Z")),
            vec![Notification::Waiting(wait)]
        );

        let busy_again = with_views(
            Vec::new(),
            Some(sessions(Vec::new(), 1)),
            Some(Settings::default()),
        );
        assert_eq!(
            deduper.due(&busy_again, utc("2026-11-10T09:56:30Z")),
            vec![Notification::WaitingOver {
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            }]
        );
    }

    #[test]
    fn another_hosts_waiting_session_notifies_only_with_all_hosts() {
        let mut deduper = Deduper::new();
        let mut on_vega = waiting(utc("2026-11-10T09:56:00Z"));
        on_vega.host = Some("vega".to_string());

        let this_host_only = with_views(
            Vec::new(),
            Some(sessions(vec![on_vega.clone()], 0)),
            Some(Settings::default()),
        );
        assert!(
            deduper
                .due(&this_host_only, utc("2026-11-10T09:56:20Z"))
                .is_empty(),
            "this host's setting does not notify another host's wait"
        );

        let all_hosts = Settings {
            notify_waiting_hosts: WaitingHosts::AllHosts,
            ..Settings::default()
        };
        let everywhere = with_views(
            Vec::new(),
            Some(sessions(vec![on_vega.clone()], 0)),
            Some(all_hosts),
        );
        let due = deduper.due(&everywhere, utc("2026-11-10T09:56:21Z"));
        assert_eq!(due, vec![Notification::Waiting(on_vega.clone())]);
        match &due[0] {
            Notification::Waiting(waiting) => assert_eq!(
                waiting_text(waiting, Some("atlas")),
                "acme-kb-dd is waiting for you (acme-kb, on vega)."
            ),
            other => panic!("a waiting notification, not {other:?}"),
        }
    }

    #[test]
    fn the_waiting_text_names_the_waiting_sessions_host() {
        let mut on_vega = waiting(utc("2026-11-10T09:56:00Z"));
        on_vega.host = Some("vega".to_string());
        assert_eq!(
            waiting_text(&on_vega, Some("atlas")),
            "acme-kb-dd is waiting for you (acme-kb, on vega)."
        );
        assert_eq!(
            waiting_text(&waiting(utc("2026-11-10T09:56:00Z")), Some("atlas")),
            "acme-kb-dd is waiting for you (acme-kb, on atlas)."
        );
    }

    #[test]
    fn recheck_at_is_15_s_after_a_new_wait() {
        let now = utc("2026-11-10T10:00:00Z");
        let wait = waiting(utc("2026-11-10T09:59:55Z"));
        let snapshot = with_views(
            Vec::new(),
            Some(sessions(vec![wait.clone()], 0)),
            Some(Settings::default()),
        );

        let mut deduper = Deduper::new();
        assert_eq!(
            deduper.recheck_at(&snapshot, now),
            Some(utc("2026-11-10T10:00:10Z")),
            "a wait 5 s old is due 10 s later"
        );

        assert_eq!(
            deduper.due(&snapshot, utc("2026-11-10T10:00:10Z")),
            vec![Notification::Waiting(wait.clone())]
        );
        assert_eq!(
            deduper.recheck_at(&snapshot, now),
            None,
            "a wait already announced needs no re-check"
        );

        let mut later = waiting(utc("2026-11-10T09:59:58Z"));
        later.session = "2b7e4c1a-8d3f-4e6a-b8c5-2f0a1d7e9b34".to_string();
        let two = with_views(
            Vec::new(),
            Some(sessions(vec![later, wait], 0)),
            Some(Settings::default()),
        );
        let deduper = Deduper::new();
        assert_eq!(
            deduper.recheck_at(&two, now),
            Some(utc("2026-11-10T10:00:10Z")),
            "the earlier of two waits is the re-check"
        );
    }

    #[test]
    fn the_texts_are_exact() {
        let wait = waiting(utc("2026-11-10T09:56:00Z"));

        assert_eq!(
            usable_again_text("PR", "its week reset"),
            "PR is usable again: its week reset."
        );
        assert_eq!(
            all_full_text(Some(("PR", "14:40"))),
            "All accounts are full. Next: PR at 14:40."
        );
        assert_eq!(all_full_text(None), "All accounts are full.");
        assert_eq!(
            waiting_text(&wait, Some("atlas")),
            "acme-kb-dd is waiting for you (acme-kb, on atlas)."
        );
        assert_eq!(
            waiting_text(&wait, None),
            "acme-kb-dd is waiting for you (acme-kb)."
        );
    }

    #[test]
    fn all_full_shows_the_weekday_of_a_far_reset() {
        let now = utc("2026-11-13T01:00:00Z");
        // Three days away: the shared helper shows the weekday.
        let reset = utc("2026-11-16T01:00:00Z");
        let time = format::format_reset_clock(reset, now, &Utc);
        assert_eq!(
            all_full_text(Some(("PR", &time))),
            "All accounts are full. Next: PR at Mon 01:00."
        );
    }

    // ---- Proof tests for the review of Task 10 (they fail today) ----

    /// An account whose login ended is not a full account, so the "all
    /// accounts are full" notification must not go out (spec §4: it is sent
    /// "when the last usable account fills up").
    #[test]
    fn proof_all_full_is_not_sent_when_the_last_usable_account_logged_out() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:00:00Z");

        let before = with_views(
            vec![view("AC", "AC", true, None, true)],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&before, now).is_empty());

        let mut logged_out = view("AC", "AC", false, None, true);
        logged_out.login_state = LoginState::LoggedOut;
        let after = with_views(vec![logged_out], None, Some(Settings::default()));
        assert!(
            deduper
                .due(&after, now + chrono::Duration::minutes(1))
                .is_empty(),
            "an account whose login ended is not full: no \"All accounts are full.\""
        );
    }

    /// The same rule with no account left: an empty list is not a full list.
    #[test]
    fn proof_all_full_is_not_sent_when_no_account_is_left() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:00:00Z");

        let before = with_views(
            vec![view("AC", "AC", true, None, true)],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&before, now).is_empty());

        let after = with_views(Vec::new(), None, Some(Settings::default()));
        assert!(
            deduper
                .due(&after, now + chrono::Duration::minutes(1))
                .is_empty(),
            "no account is not a full account: no \"All accounts are full.\""
        );
    }

    /// The notification was shown and the setting is switched off: the wait is
    /// over, so the notification must be withdrawn (spec §4.1).
    #[test]
    fn proof_a_shown_waiting_notification_is_withdrawn_when_the_setting_is_off() {
        let mut deduper = Deduper::new();
        let wait = waiting(utc("2026-11-10T09:56:00Z"));

        let on = with_views(
            Vec::new(),
            Some(sessions(vec![wait.clone()], 0)),
            Some(Settings::default()),
        );
        assert_eq!(
            deduper.due(&on, utc("2026-11-10T09:56:20Z")),
            vec![Notification::Waiting(wait.clone())]
        );

        let off = with_views(
            Vec::new(),
            Some(sessions(Vec::new(), 1)),
            Some(Settings {
                notify_waiting: false,
                ..Settings::default()
            }),
        );
        assert_eq!(
            deduper.due(&off, utc("2026-11-10T09:56:30Z")),
            vec![Notification::WaitingOver {
                session: wait.session.clone()
            }],
            "a shown notification must be withdrawn when the wait is over"
        );
    }

    #[test]
    fn a_model_block_that_ends_with_its_session_is_not_a_reset() {
        let reset = utc("2026-11-16T16:00:00Z");
        let active = view("lb-id", "LB", true, None, true);
        let blocked = || {
            snapshot_of(vec![
                active.clone(),
                blocked_view(
                    "ac-id",
                    "AC",
                    Some(reset),
                    false,
                    Blocker::Model("Fable".to_string()),
                ),
            ])
        };
        let usable = || snapshot_of(vec![active.clone(), view("ac-id", "AC", true, None, false)]);
        let mut deduper = Deduper::new();

        // 10:00 a Fable session is open; 10:05 it closed (no reset happened).
        deduper.due(&blocked(), utc("2026-11-14T10:00:00Z"));
        let closed = deduper.due(&usable(), utc("2026-11-14T10:05:00Z"));
        assert!(
            closed.is_empty(),
            "a block that ends before its reset is not a reset: {closed:?}"
        );

        // A Fable session opens again; at 16:00:30 the week really resets.
        deduper.due(&blocked(), utc("2026-11-14T11:00:00Z"));
        let after_reset = deduper.due(&usable(), utc("2026-11-16T16:00:30Z"));
        assert!(
            after_reset
                .iter()
                .any(|n| matches!(n, Notification::UsableAgain { .. })),
            "the real reset must notify: {after_reset:?}"
        );
    }

    /// An account whose usage is unknown is not usable, but no limit is full
    /// either, so it does not make "all accounts are full" true.
    #[test]
    fn unknown_usage_on_every_account_is_not_all_full() {
        let mut deduper = Deduper::new();
        let now = utc("2026-11-10T14:00:00Z");

        let some_usable = with_views(
            vec![view("AC", "AC", true, None, true)],
            None,
            Some(Settings::default()),
        );
        assert!(deduper.due(&some_usable, now).is_empty());

        let all_unknown = with_views(
            vec![
                view("AC", "AC", false, None, true),
                view("LB", "LB", false, None, false),
            ],
            None,
            Some(Settings::default()),
        );
        assert!(
            deduper
                .due(&all_unknown, now + chrono::Duration::minutes(1))
                .is_empty(),
            "an account with unknown usage is not blocked, so not all accounts are full"
        );
    }

    /// A per-model block whose reset time passes while the session that caused
    /// it is still running is a real reset: it sends "usable again" once.
    #[test]
    fn a_model_block_notifies_once_when_its_reset_passes_with_the_session_still_running() {
        let reset = utc("2026-11-16T16:00:00Z");
        let active = view("lb-id", "LB", true, None, true);
        let blocked = || {
            snapshot_of(vec![
                active.clone(),
                blocked_view(
                    "ac-id",
                    "AC",
                    Some(reset),
                    false,
                    Blocker::Model("Fable".to_string()),
                ),
            ])
        };
        let usable = || snapshot_of(vec![active.clone(), view("ac-id", "AC", true, None, false)]);
        let mut deduper = Deduper::new();

        deduper.due(&blocked(), utc("2026-11-16T15:59:00Z"));
        assert_eq!(
            deduper.due(&usable(), utc("2026-11-16T16:00:30Z")),
            vec![Notification::UsableAgain {
                id: "ac-id".to_string(),
                reason: "its Fable week reset".to_string(),
            }],
            "the reset happened while the Fable session still ran"
        );
        assert!(
            deduper
                .due(&usable(), utc("2026-11-16T16:01:00Z"))
                .is_empty(),
            "once per account and reset minute"
        );
    }

    fn snapshot_of(views: Vec<AccountView>) -> Snapshot {
        with_views(views, None, Some(Settings::default()))
    }

    // ---- Proof tests from the review of Task I-1 ----

    /// The reviewer's second proof test, replaced as the fix brief asks: a
    /// link planted in the shared temp folder is no longer possible now that
    /// the icon lives in the user's own data folder. The icon's path must be
    /// `AppPaths::notification_icon()` under the data folder given to it.
    #[test]
    fn proof_the_notification_icon_lives_in_the_app_data_folder() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let paths = AppPaths::at(tmp.path());
        let icon = notification_icon(&paths).expect("the icon is written");
        assert_eq!(icon, paths.notification_icon());
        assert_eq!(icon, tmp.path().join("notification-icon.png"));
        assert!(icon.is_file(), "the icon file is there");
    }

    /// The icon step writes the embedded PNG into a data folder that has none
    /// and returns that path.
    #[test]
    fn the_notification_icon_is_written_into_a_new_data_folder() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let paths = AppPaths::at(tmp.path());
        assert!(
            !paths.notification_icon().exists(),
            "the new data folder holds no icon yet"
        );
        let icon = notification_icon(&paths).expect("the icon is written");
        assert_eq!(icon, paths.notification_icon());
        assert_eq!(std::fs::read(&icon).expect("the icon file"), APP_ICON_PNG);
    }

    /// A cleaner may delete the icon: the next notification writes it again.
    #[test]
    fn a_deleted_notification_icon_is_written_again() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let paths = AppPaths::at(tmp.path());
        let first = notification_icon(&paths).expect("the icon is written");
        std::fs::remove_file(&first).expect("the icon is removed");
        let second = notification_icon(&paths).expect("the icon is written again");
        assert_eq!(second, first);
        assert_eq!(std::fs::read(&second).expect("the icon file"), APP_ICON_PNG);
    }

    /// An icon file that holds another picture (an older seatswap's icon) is
    /// replaced by this version's icon.
    #[test]
    fn an_outdated_notification_icon_is_replaced() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let paths = AppPaths::at(tmp.path());
        std::fs::write(paths.notification_icon(), b"an older icon").expect("an old icon");
        let icon = notification_icon(&paths).expect("the icon is written");
        assert_eq!(std::fs::read(&icon).expect("the icon file"), APP_ICON_PNG);
    }

    /// A failed write returns an error naming the icon's path, so the
    /// notification that cannot show it still says where the icon should be.
    #[test]
    fn a_notification_icon_that_cannot_be_written_names_the_path() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let not_a_folder = tmp.path().join("data");
        std::fs::write(&not_a_folder, b"a file where the data folder goes")
            .expect("the file at the data folder's path");
        let paths = AppPaths::at(&not_a_folder);
        let error = notification_icon(&paths).expect_err("the write must fail");
        assert!(
            error.contains(&paths.notification_icon().display().to_string()),
            "the error names the icon's path: {error}"
        );
    }
}
