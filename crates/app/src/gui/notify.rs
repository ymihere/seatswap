//! Desktop notifications and their de-duplication (docs/implementation.md
//! "Notifications", spec §7.4, §6.3).
//!
//! The decision "may this notification be shown?" is a plain struct, tested
//! without notify-rust. Sending happens in small helpers; a suggestion with its
//! "Switch" action waits on its own thread so no worker is blocked.

use std::collections::HashSet;
use std::thread;

use chrono::{Local, Utc};
use notify_rust::Notification as DesktopNotification;
use notify_rust::NotificationResponse;

use seatswap_core::engine::{LoginWarning, Suggestion};
use seatswap_core::switcher::UnsavedPolicy;

use super::format;
#[cfg(windows)]
use super::registry;
use super::worker::{LiveView, Snapshot};

/// What the de-duplicator decided to show.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Notification {
    Suggestion(Suggestion),
    LoginWarning(LoginWarning),
    UnknownLogin {
        email: Option<String>,
        org_name: Option<String>,
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

/// The rules of docs/implementation.md:
/// - a suggestion once per account and reset window;
/// - a login warning once per account and level per run;
/// - an unknown live login once per identity per run.
#[derive(Debug, Default)]
pub(crate) struct Deduper {
    /// One entry per from-account and reset minute (docs/implementation.md).
    suggestions: HashSet<(String, Option<i64>)>,
    warnings: HashSet<(String, WarningLevel)>,
    /// Every unknown identity seen this run, so A -> B -> A announces A once.
    unknown: HashSet<(Option<String>, Option<String>)>,
}

impl Deduper {
    pub(crate) fn new() -> Deduper {
        Deduper::default()
    }

    /// The notifications this snapshot adds, in the order they are sent.
    pub(crate) fn due(&mut self, snapshot: &Snapshot) -> Vec<Notification> {
        let mut due = Vec::new();

        if let Some(suggestion) = &snapshot.suggestion {
            // Rounded to the minute: a snapshot a few milliseconds later in the
            // same window is still the same suggestion.
            let key = (
                suggestion.from_id.clone(),
                suggestion
                    .from_resets_at
                    .map(|reset| reset.timestamp() / 60),
            );
            if self.suggestions.insert(key) {
                due.push(Notification::Suggestion(suggestion.clone()));
            }
        }

        for warning in &snapshot.warnings {
            let id = warning_id(warning);
            let key = (id.to_string(), WarningLevel::of(warning));
            if self.warnings.insert(key) {
                due.push(Notification::LoginWarning(warning.clone()));
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

        due
    }
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

/// Every notification starts here: the summary, and on Windows the app id it
/// is shown under. The only place that calls `DesktopNotification::new()`.
fn base() -> DesktopNotification {
    let mut notification = DesktopNotification::new();
    notification.summary("seatswap");
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

pub(crate) fn send_switch(message: &str) {
    let body = message.to_string();
    notify_in_background(move || {
        let _ = base().body(&body).show();
    });
}

pub(crate) fn send_error(message: &str) {
    let body = message.to_string();
    notify_in_background(move || {
        let _ = base().body(&body).show();
    });
}

pub(crate) fn send_login_warning(warning: &LoginWarning, account: &str) {
    let body = match warning {
        LoginWarning::Soon { days_left, .. } => format!(
            "{account} — the login ends in {days_left} days. Run `claude` and `/login` to renew it."
        ),
        LoginWarning::Tomorrow { .. } => {
            format!("{account} — the login ends tomorrow. Run `claude` and `/login` to renew it.")
        }
        LoginWarning::Ended { .. } => {
            format!("{account} — the login has ended. Run `claude` and `/login` to renew it.")
        }
    };
    notify_in_background(move || {
        let _ = base().body(&body).show();
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
/// button for it). Windows reports a body click by itself and would draw a
/// button for the action, so it is announced on Linux only.
fn clickable() -> DesktopNotification {
    let mut notification = base();
    if cfg!(all(unix, not(target_os = "macos"))) {
        notification.action("default", "Open");
    }
    notification
}

/// A switch suggestion with a "Switch" action. Clicking the body opens the
/// panel instead (spec §7.4).
pub(crate) fn send_suggestion(
    app: tauri::AppHandle,
    suggestion: &Suggestion,
    from_name: &str,
    to_name: &str,
) {
    let body = format!(
        "{from_name} is at {} %{}. Switch to {to_name} ({} %)?",
        format::integer_percent(suggestion.from_percent),
        suggestion
            .from_resets_at
            .map(|resets| format!(
                " (resets {})",
                format::format_reset_clock(resets, Utc::now(), &Local)
            ))
            .unwrap_or_default(),
        format::integer_percent(suggestion.to_percent)
    );
    let to_id = suggestion.to_id.clone();
    // Waiting for an action blocks, so it gets its own thread.
    thread::spawn(move || {
        let shown = clickable().body(&body).action("switch", "Switch").show();
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
    app: tauri::AppHandle,
    email: Option<&str>,
    org_name: Option<&str>,
) {
    let who = email.or(org_name).unwrap_or("unknown account");
    let body = format!("New login found: {who}. Save it as an account?");
    thread::spawn(move || {
        let shown = clickable()
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

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::DateTime;
    use seatswap_core::store::Settings;

    fn snapshot(
        suggestion: Option<Suggestion>,
        warnings: Vec<LoginWarning>,
        live: LiveView,
    ) -> Snapshot {
        Snapshot {
            views: Vec::new(),
            active_id: None,
            live,
            suggestion,
            warnings,
            pending: None,
            secrets_warning: None,
            last_poll_at: None,
            error: None,
            settings: Some(Settings::default()),
        }
    }

    fn suggestion(resets_at: Option<DateTime<Utc>>) -> Suggestion {
        Suggestion {
            from_id: "as".to_string(),
            to_id: "p".to_string(),
            from_percent: 97.0,
            to_percent: 12.0,
            from_resets_at: resets_at,
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
        assert_eq!(deduper.due(&a).len(), 1);
        assert_eq!(deduper.due(&b).len(), 1);
        assert!(
            deduper.due(&a).is_empty(),
            "a login already announced this run must stay quiet"
        );
    }

    #[test]
    fn a_suggestion_a_p_b_p_a_p_fires_a_p_once() {
        let mut deduper = Deduper::new();
        let reset = Some(Utc::now() + chrono::Duration::hours(1));
        let make = |from: &str, to: &str| Suggestion {
            from_id: from.to_string(),
            to_id: to.to_string(),
            from_percent: 97.0,
            to_percent: 12.0,
            from_resets_at: reset,
        };
        let a_to_p = snapshot(Some(make("a", "p")), Vec::new(), LiveView::NoLogin);
        let b_to_p = snapshot(Some(make("b", "p")), Vec::new(), LiveView::NoLogin);
        let a_to_p_again = snapshot(Some(make("a", "p")), Vec::new(), LiveView::NoLogin);
        assert_eq!(deduper.due(&a_to_p).len(), 1);
        assert_eq!(deduper.due(&b_to_p).len(), 1);
        assert!(
            deduper.due(&a_to_p_again).is_empty(),
            "a suggestion already announced this run must stay quiet"
        );
    }

    #[test]
    fn a_suggestion_is_keyed_by_the_reset_minute() {
        let mut deduper = Deduper::new();
        // Start at a minute boundary so 20 s later is still the same minute.
        let minute = Utc::now().timestamp() / 60 * 60;
        let reset = DateTime::<Utc>::from_timestamp(minute + 3600, 0).expect("valid timestamp");
        let first = snapshot(Some(suggestion(Some(reset))), Vec::new(), LiveView::NoLogin);
        let same_minute = snapshot(
            Some(suggestion(Some(reset + chrono::Duration::seconds(20)))),
            Vec::new(),
            LiveView::NoLogin,
        );
        assert_eq!(deduper.due(&first).len(), 1);
        assert!(deduper.due(&same_minute).is_empty());

        let next_minute = snapshot(
            Some(suggestion(Some(reset + chrono::Duration::seconds(60)))),
            Vec::new(),
            LiveView::NoLogin,
        );
        assert_eq!(deduper.due(&next_minute).len(), 1);
    }

    #[test]
    fn a_suggestion_fires_once_per_reset_window() {
        let mut deduper = Deduper::new();
        let now = Utc::now();
        let reset = Some(now + chrono::Duration::hours(1));

        let first = snapshot(Some(suggestion(reset)), Vec::new(), LiveView::NoLogin);
        assert_eq!(deduper.due(&first).len(), 1);
        assert!(
            deduper.due(&first).is_empty(),
            "the same window must not repeat"
        );

        // A new reset window may be announced again.
        let next_reset = Some(now + chrono::Duration::hours(6));
        let second = snapshot(Some(suggestion(next_reset)), Vec::new(), LiveView::NoLogin);
        assert_eq!(deduper.due(&second).len(), 1);
    }

    #[test]
    fn a_warning_fires_once_per_account_and_level() {
        let mut deduper = Deduper::new();
        let soon = LoginWarning::Soon {
            id: "as".to_string(),
            days_left: 4,
        };
        let first = snapshot(None, vec![soon.clone()], LiveView::NoLogin);
        assert_eq!(deduper.due(&first).len(), 1);

        // Same level, changed day count: still the same warning.
        let soon_again = LoginWarning::Soon {
            id: "as".to_string(),
            days_left: 3,
        };
        let again = snapshot(None, vec![soon_again], LiveView::NoLogin);
        assert!(deduper.due(&again).is_empty());

        // A different level for the same account is a new warning.
        let tomorrow = LoginWarning::Tomorrow {
            id: "as".to_string(),
        };
        let next = snapshot(None, vec![tomorrow], LiveView::NoLogin);
        assert_eq!(deduper.due(&next).len(), 1);

        // A different account is new too.
        let other = LoginWarning::Ended {
            id: "p".to_string(),
        };
        let other = snapshot(None, vec![other], LiveView::NoLogin);
        assert_eq!(deduper.due(&other).len(), 1);
    }

    #[test]
    fn an_unknown_login_fires_once_per_identity() {
        let mut deduper = Deduper::new();
        let live = LiveView::Unknown {
            email: Some("new@example.com".to_string()),
            org_name: Some("Acme".to_string()),
        };
        let first = snapshot(None, Vec::new(), live.clone());
        assert_eq!(deduper.due(&first).len(), 1);
        assert!(deduper.due(&first).is_empty());

        let other = snapshot(
            None,
            Vec::new(),
            LiveView::Unknown {
                email: Some("other@example.com".to_string()),
                org_name: Some("Acme".to_string()),
            },
        );
        assert_eq!(deduper.due(&other).len(), 1);
    }
}
