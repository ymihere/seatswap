//! The tray icon, its menu and its tooltip (spec §8, docs/implementation.md
//! "app: GUI").
//!
//! The icon is drawn by `icon.rs`; this module only decides *what* to draw and
//! which texts the menu and tooltip carry.

use chrono::{DateTime, Local, Utc};
use seatswap_core::api::Usage;
use seatswap_core::engine::AccountView;
use seatswap_core::store::{IconLimit, LoginState, Settings, TrayText};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use super::format;
use super::worker::{LiveView, Snapshot};
use super::GuiState;
use crate::icon::{self, IconSpec};

/// The grey of the unknown ring (`#555b66`).
const GREY: [u8; 3] = [0x55, 0x5b, 0x66];
/// Pixel size of the image handed to the platform.
const ICON_SIZE: u32 = 44;
/// Tray id used by `update`.
const TRAY_ID: &str = "seatswap-tray";
/// The longest tooltip the platform shows in full, in UTF-16 units. Windows
/// cuts a tray tooltip without notice at 128 units including the closing NUL.
#[cfg(windows)]
const TOOLTIP_LIMIT: Option<usize> = Some(127);
#[cfg(not(windows))]
const TOOLTIP_LIMIT: Option<usize> = None;

fn active_view(views: &[AccountView]) -> Option<&AccountView> {
    views.iter().find(|view| view.is_active)
}

/// The icon for the live account and the `iconLimit` setting.
pub(crate) fn icon_spec(views: &[AccountView], settings: Option<&Settings>) -> IconSpec {
    let Some(view) = active_view(views).filter(|_| settings.is_some()) else {
        return IconSpec {
            abbr: "?".to_string(),
            account_color: GREY,
            ring: None,
            locked: false,
            monochrome: false,
        };
    };
    let Some(settings) = settings else {
        return IconSpec {
            abbr: "?".to_string(),
            account_color: GREY,
            ring: None,
            locked: false,
            monochrome: false,
        };
    };
    let color = format::parse_color(&view.color).unwrap_or(GREY);
    let limit = view.usage.as_ref().map(|usage| match settings.icon_limit {
        IconLimit::Binding => usage.binding(),
        IconLimit::Session => usage.session(),
        IconLimit::Week => usage.weekly_all(),
    });
    let limit = limit.flatten();
    let ring = limit.map(|limit| {
        let level_color = format::parse_color(limit.level().color()).unwrap_or(GREY);
        (limit.percent, level_color)
    });
    IconSpec {
        abbr: view.abbr.clone(),
        account_color: color,
        ring,
        locked: limit.is_some_and(|limit| limit.percent >= 100.0),
        monochrome: false,
    }
}

/// The label next to the tray icon (`trayText`).
pub(crate) fn tray_text(
    views: &[AccountView],
    settings: Option<&Settings>,
    now: DateTime<Utc>,
) -> String {
    let Some(settings) = settings else {
        return String::new();
    };
    if settings.tray_text == TrayText::Off {
        return String::new();
    }
    let Some(view) = active_view(views) else {
        return String::new();
    };
    let Some(limit) = view.usage.as_ref().and_then(Usage::binding) else {
        return String::new();
    };
    let percent = format::percent(limit.percent);
    if settings.tray_text == TrayText::PercentReset {
        if let Some(resets_at) = limit.resets_at {
            return format!(
                "{percent} · {}",
                format::format_reset_at(resets_at, now, &Local)
            );
        }
    }
    percent
}

/// One account's menu item text:
/// `AC  Acme   97 % · 1h12m` and `✓` for the live account;
/// `AC  Acme   logged out` when the stored login ended.
pub(crate) fn menu_text(view: &AccountView, now: DateTime<Utc>) -> String {
    let mut text = format!("{}  {}", view.abbr, view.label);
    if view.login_state == LoginState::LoggedOut {
        text.push_str("   logged out");
    } else if let Some(limit) = view.usage.as_ref().and_then(Usage::binding) {
        text.push_str(&format!("   {} %", format::integer_percent(limit.percent)));
        if let Some(resets_at) = limit.resets_at {
            text.push_str(" · ");
            text.push_str(&format::format_reset_at(resets_at, now, &Local));
        }
    } else {
        text.push_str("   no usage");
    }
    if view.is_active {
        text.push_str(" ✓");
    }
    text
}

/// `text` if it has at most `max_units` UTF-16 units; otherwise its longest
/// prefix that, with "…" appended, has at most `max_units` units. Never splits
/// a character.
fn fit_tooltip(text: &str, max_units: usize) -> String {
    if text.encode_utf16().count() <= max_units {
        return text.to_string();
    }
    let budget = max_units.saturating_sub('…'.len_utf16());
    let mut used = 0;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        used += character.len_utf16();
        if used > budget {
            break;
        }
        end = index + character.len_utf8();
    }
    format!("{}…", &text[..end])
}

/// The tooltip as handed to the tray: [`tooltip`], cut to [`TOOLTIP_LIMIT`]
/// where the platform has one, so the cut is visible ("…") and never splits a
/// character.
fn tray_tooltip(snapshot: &Snapshot, now: DateTime<Utc>) -> String {
    let text = tooltip(snapshot, now);
    match TOOLTIP_LIMIT {
        Some(max_units) => fit_tooltip(&text, max_units),
        None => text,
    }
}

/// The tooltip of the live account (spec §8.1), or the startup error so it can
/// be read without opening the panel.
pub(crate) fn tooltip(snapshot: &Snapshot, now: DateTime<Utc>) -> String {
    if let Some(error) = &snapshot.error {
        return format!("seatswap — {error}");
    }
    match active_view(&snapshot.views) {
        Some(view) => match &view.usage {
            Some(usage) => format::tooltip(view, usage, now),
            None => format!("{} · {} — no usage yet", view.abbr, view.label),
        },
        None => match &snapshot.live {
            LiveView::Unknown { email, org_name } => format!(
                "seatswap — new login: {}",
                email
                    .as_deref()
                    .or(org_name.as_deref())
                    .unwrap_or("unknown")
            ),
            _ => "seatswap — no live account".to_string(),
        },
    }
}

/// Build the tray icon. The menu event handler is wired here; clicks and menu
/// entries call into `gui` for the actual window work.
/// Create the tray icon from the current snapshot. Without a tray to show it
/// in (on Linux: no StatusNotifierWatcher, because the desktop has no tray
/// support or its panel starts after seatswap at login), seatswap keeps running
/// and says so; the pop-up stays reachable with `seatswap panel`, and the icon
/// is created as soon as a tray appears.
pub(crate) fn start(app: &AppHandle) {
    if let Err(error) = try_build(app) {
        let message = no_tray_message(&error);
        eprintln!("seatswap: {message}");
        super::notify::send_error(&message);
        build_when_a_tray_appears(app);
    }
}

fn no_tray_message(error: &str) -> String {
    format!(
        "no system tray to show the icon in ({error}). The icon appears as soon as the \
         desktop's tray runs (on GNOME it needs the AppIndicator extension); until then \
         `seatswap panel` opens the pop-up"
    )
}

fn try_build(app: &AppHandle) -> Result<(), String> {
    let state = app
        .try_state::<GuiState>()
        .ok_or_else(|| "the app state is missing".to_string())?;
    let snapshot = state.snapshot()?;
    let tray = build(app, &snapshot).map_err(|error| error.to_string())?;
    *super::lock(&state.tray) = Some(tray);
    Ok(())
}

/// Watch the session bus for the StatusNotifierWatcher and create the icon when
/// it appears. Once the icon exists, ksni registers it again by itself whenever
/// the watcher restarts.
#[cfg(target_os = "linux")]
fn build_when_a_tray_appears(app: &AppHandle) {
    use gtk::gio;
    use std::sync::{Arc, Mutex};

    // Holds the watch id (`gio::bus_watch_name` returns it after the closure exists).
    let watch = Arc::new(Mutex::new(None));
    let appeared_watch = Arc::clone(&watch);
    let app = app.clone();
    let id = gio::bus_watch_name(
        gio::BusType::Session,
        STATUS_NOTIFIER_WATCHER,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| match try_build(&app) {
            Ok(()) => {
                eprintln!("seatswap: a system tray appeared; the icon is shown");
                if let Some(id) = super::lock(&appeared_watch).take() {
                    gio::bus_unwatch_name(id);
                }
            }
            Err(error) => eprintln!("seatswap: tray icon: {error}"),
        },
        |_, _| {},
    );
    *super::lock(&watch) = Some(id);
}

/// Elsewhere the tray is part of the system; there is nothing to wait for.
#[cfg(not(target_os = "linux"))]
fn build_when_a_tray_appears(_app: &AppHandle) {}

/// The D-Bus name of the tray host registry on Linux (StatusNotifierItem spec).
#[cfg(target_os = "linux")]
const STATUS_NOTIFIER_WATCHER: &str = "org.kde.StatusNotifierWatcher";

fn build(app: &AppHandle, snapshot: &Snapshot) -> tauri::Result<TrayIcon> {
    let menu = build_menu(app, snapshot)?;
    let spec = icon_spec(&snapshot.views, snapshot.settings.as_ref());
    let image = Image::new_owned(icon::render(&spec, ICON_SIZE), ICON_SIZE, ICON_SIZE);
    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .title(tray_text(
            &snapshot.views,
            snapshot.settings.as_ref(),
            Utc::now(),
        ))
        .tooltip(tray_tooltip(snapshot, Utc::now()))
        .icon(image)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| menu_event(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Err(error) = super::panel::toggle(tray.app_handle()) {
                    eprintln!("seatswap: panel: {error}");
                }
            }
        })
        .build(app)
}

/// Update icon, tooltip, title and menu from a new snapshot.
pub(crate) fn update(tray: &TrayIcon, snapshot: &Snapshot) {
    let now = Utc::now();
    let spec = icon_spec(&snapshot.views, snapshot.settings.as_ref());
    let image = Image::new_owned(icon::render(&spec, ICON_SIZE), ICON_SIZE, ICON_SIZE);
    if let Err(error) = tray.set_icon(Some(image)) {
        eprintln!("seatswap: tray icon: {error}");
    }
    if let Err(error) = tray.set_tooltip(Some(tray_tooltip(snapshot, now))) {
        eprintln!("seatswap: tray tooltip: {error}");
    }
    let title = tray_text(&snapshot.views, snapshot.settings.as_ref(), now);
    let empty = title.is_empty();
    if let Err(error) = tray.set_title(if empty { None::<String> } else { Some(title) }) {
        eprintln!("seatswap: tray title: {error}");
    }
    match build_menu(tray.app_handle(), snapshot) {
        Ok(menu) => {
            if let Err(error) = tray.set_menu(Some(menu)) {
                eprintln!("seatswap: tray menu: {error}");
            }
        }
        Err(error) => eprintln!("seatswap: tray menu: {error}"),
    }
}

fn build_menu(app: &AppHandle, snapshot: &Snapshot) -> tauri::Result<Menu<tauri::Wry>> {
    let now = Utc::now();
    let menu = Menu::new(app)?;
    for view in &snapshot.views {
        let item = MenuItem::with_id(
            app,
            format!("account:{}", view.id),
            menu_text(view, now),
            true,
            None::<&str>,
        )?;
        menu.append(&item)?;
    }
    if let LiveView::Unknown { email, org_name } = &snapshot.live {
        let who = email
            .as_deref()
            .or(org_name.as_deref())
            .unwrap_or("new login");
        let item = MenuItem::with_id(
            app,
            "save-current",
            format!("Save current login ({who})…"),
            true,
            None::<&str>,
        )?;
        menu.append(&item)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for (id, text) in [
        ("open-panel", "Open panel…"),
        ("add-account", "Add account…"),
        ("settings", "Settings…"),
    ] {
        let item = MenuItem::with_id(app, id, text, true, None::<&str>)?;
        menu.append(&item)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let quit = MenuItem::with_id(app, "quit", "Quit seatswap", true, None::<&str>)?;
    menu.append(&quit)?;
    Ok(menu)
}

fn menu_event(app: &AppHandle, id: &str) {
    match id {
        "open-panel" => super::show_panel(app, "main"),
        "add-account" | "save-current" => super::show_panel(app, "add"),
        "settings" => super::show_panel(app, "settings"),
        "quit" => app.exit(0),
        other => {
            if let Some(key) = other.strip_prefix("account:") {
                super::request_switch(
                    app,
                    key.to_string(),
                    seatswap_core::switcher::UnsavedPolicy::Refuse,
                    None,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use seatswap_core::api::{Limit, LimitKind, Severity};
    use seatswap_core::store::Settings;

    use chrono::Duration;

    fn usage(limits: Vec<Limit>) -> Usage {
        Usage {
            limits,
            extra_usage_enabled: false,
        }
    }

    fn limit(
        kind: LimitKind,
        percent: f64,
        active: bool,
        resets_at: Option<DateTime<Utc>>,
    ) -> Limit {
        Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at,
            model: None,
            is_active: active,
        }
    }

    fn view(
        abbr: &str,
        label: &str,
        is_active: bool,
        usage: Option<Usage>,
        login_state: LoginState,
    ) -> AccountView {
        AccountView {
            id: format!("id-{abbr}"),
            label: label.to_string(),
            abbr: abbr.to_string(),
            color: "#1e3a8a".to_string(),
            email: Some("someone@example.com".to_string()),
            plan: None,
            is_active,
            login_state,
            login_ends_at_ms: None,
            login_days_left: None,
            usage,
            fetched_at: None,
            last_error: None,
        }
    }

    fn both_limits() -> Usage {
        let now = Utc::now();
        usage(vec![
            limit(
                LimitKind::Session,
                97.0,
                true,
                Some(now + Duration::minutes(72)),
            ),
            limit(
                LimitKind::WeeklyAll,
                48.0,
                false,
                Some(now + Duration::hours(72)),
            ),
        ])
    }

    fn weekly_only() -> Usage {
        usage(vec![limit(
            LimitKind::WeeklyAll,
            31.0,
            true,
            Some(Utc::now() + Duration::hours(72)),
        )])
    }

    #[test]
    fn icon_limit_chooses_binding_session_or_week() {
        let mut settings = Settings::default();
        let views = vec![view(
            "AC",
            "Acme",
            true,
            Some(both_limits()),
            LoginState::Ok,
        )];

        settings.icon_limit = IconLimit::Binding;
        let spec = icon_spec(&views, Some(&settings));
        // Both limits exist; `binding()` takes the active session limit.
        assert_eq!(spec.ring.map(|(percent, _)| percent), Some(97.0));

        settings.icon_limit = IconLimit::Session;
        let spec = icon_spec(&views, Some(&settings));
        assert_eq!(spec.ring.map(|(percent, _)| percent), Some(97.0));

        settings.icon_limit = IconLimit::Week;
        let spec = icon_spec(&views, Some(&settings));
        assert_eq!(spec.ring.map(|(percent, _)| percent), Some(48.0));
    }

    #[test]
    fn a_full_limit_is_locked() {
        let settings = Settings {
            icon_limit: IconLimit::Session,
            ..Settings::default()
        };
        let views = vec![view(
            "AC",
            "Acme",
            true,
            Some(usage(vec![limit(LimitKind::Session, 100.0, true, None)])),
            LoginState::Ok,
        )];
        let spec = icon_spec(&views, Some(&settings));
        assert!(spec.locked);
        assert_eq!(spec.abbr, "AC");
        assert_eq!(spec.account_color, [0x1e, 0x3a, 0x8a]);
    }

    #[test]
    fn no_usage_gives_a_grey_ring_and_no_live_account_gets_the_unknown_icon() {
        let settings = Settings::default();
        let views = vec![view("AC", "Acme", true, None, LoginState::Ok)];
        assert_eq!(icon_spec(&views, Some(&settings)).ring, None);

        let spec = icon_spec(&[], Some(&settings));
        assert_eq!(spec.abbr, "?");
        assert_eq!(spec.account_color, GREY);
        assert_eq!(spec.ring, None);
        assert!(!spec.locked);
    }

    #[test]
    fn tray_text_follows_the_setting() {
        let now = Utc::now();
        let views = vec![view(
            "AC",
            "Acme",
            true,
            Some(both_limits()),
            LoginState::Ok,
        )];

        let settings = Settings::default();
        assert_eq!(tray_text(&views, Some(&settings), now), "97%");

        let settings = Settings {
            tray_text: TrayText::PercentReset,
            ..Settings::default()
        };
        assert!(tray_text(&views, Some(&settings), now).starts_with("97% · "));

        let settings = Settings {
            tray_text: TrayText::Off,
            ..Settings::default()
        };
        assert_eq!(tray_text(&views, Some(&settings), now), "");
        assert_eq!(tray_text(&[], Some(&settings), now), "");
    }

    #[test]
    fn menu_texts_show_the_binding_limit_and_the_live_mark() {
        let now = Utc::now();
        let with_both = view("AC", "Acme", true, Some(both_limits()), LoginState::Ok);
        let text = menu_text(&with_both, now);
        assert!(text.starts_with("AC  Acme   97 % · "), "{text}");
        assert!(text.ends_with("✓"), "{text}");

        let weekly = view("P", "Private", false, Some(weekly_only()), LoginState::Ok);
        let text = menu_text(&weekly, now);
        assert!(text.starts_with("P  Private   31 % · "), "{text}");
        assert!(!text.ends_with("✓"), "{text}");

        let logged_out = view("AC", "Acme", false, None, LoginState::LoggedOut);
        let text = menu_text(&logged_out, now);
        assert!(text.contains("logged out"), "{text}");
    }

    #[test]
    fn fit_tooltip_keeps_a_text_that_fits() {
        assert_eq!(fit_tooltip("AS · Acme — 42 %", 127), "AS · Acme — 42 %");
        let exact = "a".repeat(127);
        assert_eq!(fit_tooltip(&exact, 127), exact);
    }

    #[test]
    fn fit_tooltip_cuts_one_unit_over_and_marks_it() {
        let text = format!("{}b", "a".repeat(127));
        let fitted = fit_tooltip(&text, 127);
        assert_eq!(fitted, format!("{}…", "a".repeat(126)));
        assert_eq!(fitted.encode_utf16().count(), 127);
    }

    #[test]
    fn fit_tooltip_never_splits_a_two_unit_character() {
        // 125 units, then an emoji of two units across the cut point at 126.
        let text = format!("{}🔥 and more", "a".repeat(125));
        let fitted = fit_tooltip(&text, 127);
        assert_eq!(fitted, format!("{}…", "a".repeat(125)));
        assert!(fitted.encode_utf16().count() <= 127);
    }

    #[test]
    fn tooltips_follow_the_live_account_shape() {
        let now = Utc::now();
        let views = vec![view(
            "AC",
            "Acme",
            true,
            Some(both_limits()),
            LoginState::Ok,
        )];
        let snapshot = snapshot_with(views, None);
        let text = tooltip(&snapshot, now);
        assert!(text.starts_with("AC · Acme — session 97 %"), "{text}");
        assert!(text.contains("week 48 %"), "{text}");

        let views = vec![view(
            "P",
            "Private",
            true,
            Some(weekly_only()),
            LoginState::Ok,
        )];
        let text = tooltip(&snapshot_with(views, None), now);
        assert!(text.starts_with("P · Private — week 31 %"), "{text}");

        let text = tooltip(&snapshot_with(Vec::new(), None), now);
        assert_eq!(text, "seatswap — no live account");

        let text = tooltip(&snapshot_with(Vec::new(), Some("accounts.json: nope")), now);
        assert!(text.contains("accounts.json: nope"), "{text}");
    }

    fn snapshot_with(views: Vec<AccountView>, error: Option<&str>) -> Snapshot {
        Snapshot {
            active_id: views
                .iter()
                .find(|view| view.is_active)
                .map(|view| view.id.clone()),
            views,
            live: LiveView::NoLogin,
            suggestion: None,
            warnings: Vec::new(),
            pending: None,
            secrets_warning: None,
            last_poll_at: None,
            error: error.map(str::to_string),
            settings: Some(Settings::default()),
        }
    }
}
