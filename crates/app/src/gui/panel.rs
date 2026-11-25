//! The pop-up window, its placement and the Tauri commands the panel calls
//! (spec §9.2, §11; docs/implementation.md "app: GUI", "Panel UI").
//!
//! All commands return `Result<_, String>` because that is what
//! `window.__TAURI__.core.invoke` rejects with; the error text is shown in the
//! panel.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use seatswap_core::atomic::{self, Access};
use seatswap_core::engine::PollReason;
use seatswap_core::history::{
    export_csv, export_json, AccountRef, By, Event, EventKind, HistoryDir, HostConflict, HostInfo,
    Hosts, Range,
};
use seatswap_core::projects::{self, ProjectEdit, Projects, ProjectsLogRecord};
use seatswap_core::store::{self, Account, Settings};
use seatswap_core::switcher::UnsavedPolicy;

use super::format;
use super::lookup_rt::{price_rows, MoneyView, PriceRow};
use super::worker::{ActionCommand, ActionReply, EngineCommand, SavedSetting, Snapshot};
use super::GuiState;
use crate::agent_skill::{self, SkillStatus};
use crate::format::{expand_tilde, with_tilde};
use crate::icon::{self, IconSpec};

/// Window label used by `tauri.conf.json`'s capability and by every command.
pub(crate) const PANEL_LABEL: &str = "panel";
/// Fixed width of the pop-up (spec §11).
const PANEL_WIDTH: f64 = 380.0;
/// Height before the page reports its own.
const PANEL_HEIGHT: f64 = 520.0;

/// A monitor rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where to put the panel (spec §9.2): centred on Wayland; otherwise centred on
/// the mouse horizontally, above the mouse in the lower half and below it in the
/// upper half, kept inside the monitor on all four edges.
pub(crate) fn place_panel(
    cursor: (f64, f64),
    monitor: Rect,
    panel: (f64, f64),
    wayland: bool,
) -> (f64, f64) {
    if wayland {
        return (
            monitor.x + (monitor.width - panel.0) / 2.0,
            monitor.y + (monitor.height - panel.1) / 2.0,
        );
    }
    let middle = monitor.y + monitor.height / 2.0;
    let wanted_x = cursor.0 - panel.0 / 2.0;
    let wanted_y = if cursor.1 >= middle {
        cursor.1 - panel.1
    } else {
        cursor.1
    };
    let min_x = monitor.x;
    let min_y = monitor.y;
    // `max(min)` keeps a panel larger than the monitor at the top-left corner.
    let max_x = (monitor.x + monitor.width - panel.0).max(min_x);
    let max_y = (monitor.y + monitor.height - panel.1).max(min_y);
    (
        wanted_x.max(min_x).min(max_x),
        wanted_y.max(min_y).min(max_y),
    )
}

/// True when the session is Wayland: the compositor decides where a window goes.
pub(crate) fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("XDG_SESSION_TYPE").is_some_and(|value| value == "wayland")
}

/// Where the panel was opened: the mouse, its monitor and the scale, all taken
/// once by `show`. Every later resize places the panel from this anchor and the
/// new height, never from the window's own position and size: on X11 those
/// answer with the old values until the window system has applied a change, so
/// reading them back after a resize drifts the panel off screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Anchor {
    pub cursor: (f64, f64),
    pub monitor: Rect,
    pub scale: f64,
}

/// The panel's state between calls: the anchor of the current opening (none
/// when no monitor was found), the logical height the page last asked for, and
/// when focus loss last hid it (see `tray_click`) and whether it waits to appear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PanelState {
    pub anchor: Option<Anchor>,
    pub height: f64,
    pub focus_hidden_at: Option<Instant>,
    /// `show` was asked for while the panel was hidden; it appears with the
    /// next `fit_panel` (or after `SHOW_DEADLINE`).
    pub pending_show: bool,
}

impl PanelState {
    pub(crate) fn new() -> Self {
        Self {
            anchor: None,
            height: PANEL_HEIGHT,
            focus_hidden_at: None,
            pending_show: false,
        }
    }
}

/// A click on the tray icon right after focus loss hid the panel is the same
/// click that took the focus away: it must not open the panel again.
const REOPEN_GUARD: Duration = Duration::from_millis(400);
/// Focus loss is judged after this delay, once the window system has settled
/// where the focus went (an opening drop-down list takes it a moment later).
const FOCUS_RECHECK: Duration = Duration::from_millis(150);

/// How long a hidden panel waits for the page's height before it appears anyway.
const SHOW_DEADLINE: Duration = Duration::from_millis(300);

/// What a left click on the tray icon does to the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayClick {
    Show,
    Hide,
    Ignore,
}

/// A visible panel closes; a panel that focus loss hid within `REOPEN_GUARD`
/// stays closed (the click itself caused the focus loss); otherwise it opens.
pub(crate) fn tray_click(visible: bool, focus_hidden_ago: Option<Duration>) -> TrayClick {
    if visible {
        TrayClick::Hide
    } else if focus_hidden_ago.is_some_and(|ago| ago < REOPEN_GUARD) {
        TrayClick::Ignore
    } else {
        TrayClick::Show
    }
}

/// Left click on the tray icon: open or close the panel (`tray_click`).
pub(crate) fn toggle(app: &AppHandle) -> Result<(), String> {
    let visible = match app.get_webview_window(PANEL_LABEL) {
        Some(window) => window.is_visible().map_err(|error| error.to_string())?,
        None => false,
    };
    let ago = app
        .try_state::<GuiState>()
        .and_then(|state| super::lock(&state.panel).focus_hidden_at)
        .map(|at| at.elapsed());
    match tray_click(visible, ago) {
        TrayClick::Show => show(app, "main"),
        TrayClick::Hide => hide(app),
        TrayClick::Ignore => Ok(()),
    }
}

fn hide(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(PANEL_LABEL)
        .ok_or_else(|| "the panel is not open".to_string())?;
    window.hide().map_err(|error| error.to_string())
}

/// The panel's top-left corner in physical pixels for a logical `height`.
/// Because the anchor stays fixed, a panel above the mouse keeps its bottom
/// edge and a panel below it keeps its top edge while it resizes, and it stays
/// inside the monitor whatever the height.
pub(crate) fn anchored_position(anchor: &Anchor, height: f64, wayland: bool) -> (f64, f64) {
    place_panel(
        anchor.cursor,
        anchor.monitor,
        (PANEL_WIDTH * anchor.scale, height * anchor.scale),
        wayland,
    )
}

/// Create the hidden panel at startup (see `setup`).
pub(crate) fn preload(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(PANEL_LABEL).is_none() {
        create(app, "main")?;
    }
    Ok(())
}

/// Show the panel, creating it on first use, and ask it to navigate to `view`.
pub(crate) fn show(app: &AppHandle, view: &str) -> Result<(), String> {
    if !matches!(view, "main" | "add") {
        return Err(format!("unknown panel view {view:?}"));
    }
    let (window, created) = match app.get_webview_window(PANEL_LABEL) {
        Some(window) => (window, false),
        None => (create(app, view)?, true),
    };
    let state = app
        .try_state::<GuiState>()
        .ok_or_else(|| "the app state is missing".to_string())?;
    let anchor = anchor_at_cursor(&window)?;
    let was_visible = window.is_visible().map_err(|error| error.to_string())?;
    let height = {
        let mut panel = super::lock(&state.panel);
        panel.anchor = anchor;
        panel.pending_show = !was_visible;
        panel.height
    };
    // The page answers every navigation with `fit_panel` (setView), and a newly
    // created one does the same once loaded.
    if !created {
        window
            .emit("navigate", view)
            .map_err(|error| error.to_string())?;
    }
    if was_visible {
        if let Some(anchor) = anchor {
            move_to(&window, &anchor, height)?;
        }
        take_focus(&window)?;
    } else {
        // A hidden panel appears only once the page has drawn the new view and
        // reported its height (`fit_panel` → `reveal`), so it never shows the
        // previous view or size first. If no height comes in time, it appears
        // anyway at the last one.
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(SHOW_DEADLINE);
            let main = handle.clone();
            let _ = main.run_on_main_thread(move || {
                let Some(state) = handle.try_state::<GuiState>() else {
                    return;
                };
                if !super::lock(&state.panel).pending_show {
                    return;
                }
                eprintln!(
                    "seatswap: the panel page reported no height within {} ms; showing it anyway",
                    SHOW_DEADLINE.as_millis()
                );
                if let Err(error) = reveal(&handle, &state) {
                    eprintln!("seatswap: panel: {error}");
                }
            });
        });
    }

    let _ = state
        .engine
        .send(EngineCommand::Poll(PollReason::PanelOpened));
    let _ = state.providers_client.opened();
    Ok(())
}

/// Put a hidden panel on screen at its anchor and last height.
fn reveal(app: &AppHandle, state: &GuiState) -> Result<(), String> {
    let window = app
        .get_webview_window(PANEL_LABEL)
        .ok_or_else(|| "the panel is not open".to_string())?;
    let (anchor, height) = {
        let mut panel = super::lock(&state.panel);
        panel.pending_show = false;
        (panel.anchor, panel.height)
    };
    // Move first, then show: the window manager maps it at the right place.
    if let Some(anchor) = anchor {
        move_to(&window, &anchor, height)?;
    }
    window.show().map_err(|error| error.to_string())?;
    // On Linux the panel asks for focus once it is mapped (`focus_when_mapped`);
    // asking now would map it through GTK before the move is applied.
    if !cfg!(target_os = "linux") {
        take_focus(&window)?;
    }
    Ok(())
}

fn create(app: &AppHandle, view: &str) -> Result<WebviewWindow, String> {
    let initial = view.to_string();
    let window = WebviewWindowBuilder::new(app, PANEL_LABEL, WebviewUrl::App("index.html".into()))
        .title("seatswap")
        .inner_size(PANEL_WIDTH, PANEL_HEIGHT)
        // Resizable on purpose: GTK ignores `set_size` on a window that is not,
        // so `fit_panel` could never change the height (seen headless,
        // 2026-11-10). Without decorations there is no edge to drag.
        .resizable(true)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        // Before the page's scripts run, so the pop-up opens on its view.
        .initialization_script(format!("window.location.hash = {initial:?};"))
        .build()
        .map_err(|error| error.to_string())?;

    focus_when_mapped(&window)?;

    // Losing focus hides the pop-up (spec §11): clicking elsewhere is a dismiss.
    // Not while one of the panel's own pop-ups (a drop-down list) has the focus;
    // that list closes on the next click outside and gives the focus back.
    let watched = window.clone();
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            let window = watched.clone();
            let handle = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(FOCUS_RECHECK);
                let main = handle.clone();
                let _ = main.run_on_main_thread(move || {
                    if window.is_focused().unwrap_or(true) || own_popup_open() {
                        return;
                    }
                    if window.hide().is_ok() {
                        if let Some(state) = handle.try_state::<GuiState>() {
                            super::lock(&state.panel).focus_hidden_at = Some(Instant::now());
                        }
                    }
                });
            });
        }
    });
    Ok(window)
}

/// Give the panel the keyboard focus. On X11, `set_focus` asks without a
/// timestamp, and a window manager with focus-stealing prevention (Cinnamon's
/// Muffin) refuses that: the panel then never has focus, so neither Esc nor a
/// click elsewhere can close it (seen on Cinnamon, 2026-11-10). Asking with the
/// X server's current time is what a real click would carry.
#[cfg(target_os = "linux")]
fn take_focus(window: &WebviewWindow) -> Result<(), String> {
    let gtk_window = window.gtk_window().map_err(|error| error.to_string())?;
    if !focus_x11(&gtk_window)? {
        // Wayland: the compositor decides about focus; asking is all we can do.
        window.set_focus().map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The X11 part of `take_focus`; false when the window is not an X11 window.
/// A request for a window that is not on screen yet is ignored by the window
/// manager, so `create` also calls this when the window is mapped.
#[cfg(target_os = "linux")]
fn focus_x11(gtk_window: &gtk::ApplicationWindow) -> Result<bool, String> {
    use gtk::prelude::{Cast, GtkWindowExt, WidgetExt};
    gtk_window.realize();
    let gdk_window = gtk_window
        .window()
        .ok_or_else(|| "the panel has no window after realize".to_string())?;
    let Ok(x11) = gdk_window.clone().downcast::<gdkx11::X11Window>() else {
        return Ok(false);
    };
    let time = gdkx11::functions::x11_get_server_time(&x11);
    gtk_window.present_with_time(time);
    gdk_window.focus(time);
    Ok(true)
}

/// How often and how far apart `focus_when_mapped` asks again. A window that
/// was just mapped is not yet managed by the window manager, which ignores a
/// focus request for it (seen on Cinnamon: the first opening never got focus,
/// every later one did).
#[cfg(target_os = "linux")]
const FOCUS_RETRIES: u32 = 5;
#[cfg(target_os = "linux")]
const FOCUS_RETRY_GAP: Duration = Duration::from_millis(80);

/// Ask for focus again each time the panel appears on screen, until it has it
/// or `FOCUS_RETRIES` requests went unanswered (see `focus_x11`).
#[cfg(target_os = "linux")]
fn focus_when_mapped(window: &WebviewWindow) -> Result<(), String> {
    use gtk::prelude::WidgetExtManual;
    let gtk_window = window.gtk_window().map_err(|error| error.to_string())?;
    gtk_window.connect_map_event(|mapped, _| {
        retry_focus(mapped.clone(), FOCUS_RETRIES);
        gtk::glib::Propagation::Proceed
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn retry_focus(gtk_window: gtk::ApplicationWindow, left: u32) {
    use gtk::prelude::{GtkWindowExt, WidgetExt};
    if left == 0 || !gtk_window.is_visible() || gtk_window.is_active() {
        if left == 0 && !gtk_window.is_active() {
            eprintln!("seatswap: the panel did not get the keyboard focus");
        }
        return;
    }
    if let Err(error) = focus_x11(&gtk_window) {
        eprintln!("seatswap: panel focus: {error}");
        return;
    }
    gtk::glib::timeout_add_local_once(FOCUS_RETRY_GAP, move || {
        retry_focus(gtk_window, left - 1);
    });
}

/// True while a pop-up of this process holds the input, such as the list of a
/// `<select>`: WebKitGTK shows it as a separate window with a GTK grab, so the
/// panel loses the focus for as long as the list is open (seen on Cinnamon and
/// in the headless run with Muffin, 2026-11-11: picking a setting closed the
/// panel).
#[cfg(target_os = "linux")]
fn own_popup_open() -> bool {
    gtk::grab_get_current().is_some()
}

/// Elsewhere a drop-down list does not take the focus from its window.
#[cfg(not(target_os = "linux"))]
fn own_popup_open() -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn focus_when_mapped(_window: &WebviewWindow) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn take_focus(window: &WebviewWindow) -> Result<(), String> {
    window.set_focus().map_err(|error| error.to_string())
}

/// The cursor and the monitor it is on. Falls back to the window's monitor and
/// then the primary one, so placement still has a rectangle without a pointer.
fn cursor_and_monitor(window: &WebviewWindow) -> (Option<(f64, f64)>, Option<Rect>) {
    let cursor = window.cursor_position().ok();
    let position = cursor.map(|position| (position.x, position.y));
    let monitor = cursor
        .and_then(|cursor| {
            window.available_monitors().ok().and_then(|monitors| {
                monitors.into_iter().find(|monitor| {
                    let origin = monitor.position();
                    let size = monitor.size();
                    cursor.x >= f64::from(origin.x)
                        && cursor.y >= f64::from(origin.y)
                        && cursor.x < f64::from(origin.x) + f64::from(size.width)
                        && cursor.y < f64::from(origin.y) + f64::from(size.height)
                })
            })
        })
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten())
        .map(|monitor| Rect {
            x: f64::from(monitor.position().x),
            y: f64::from(monitor.position().y),
            width: f64::from(monitor.size().width),
            height: f64::from(monitor.size().height),
        });
    (position, monitor)
}

/// The anchor for this opening, or `None` when the system reports no monitor
/// at all (the window system then places the panel itself). Without a pointer
/// position the panel goes to the middle of the monitor.
fn anchor_at_cursor(window: &WebviewWindow) -> Result<Option<Anchor>, String> {
    let (cursor, monitor) = cursor_and_monitor(window);
    let Some(monitor) = monitor else {
        return Ok(None);
    };
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let cursor = cursor.unwrap_or((
        monitor.x + monitor.width / 2.0,
        monitor.y + monitor.height / 2.0,
    ));
    Ok(Some(Anchor {
        cursor,
        monitor,
        scale,
    }))
}

fn move_to(window: &WebviewWindow, anchor: &Anchor, height: f64) -> Result<(), String> {
    let (x, y) = anchored_position(anchor, height, is_wayland());
    window
        .set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Commands called by ui/panel.js

#[tauri::command]
pub(crate) fn get_snapshot(state: State<'_, GuiState>) -> Result<Snapshot, String> {
    state.snapshot()
}

/// The crate's version, shown as the first line of Settings → General (R7).
#[tauri::command]
pub(crate) fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
pub(crate) fn refresh_now(state: State<'_, GuiState>) -> Result<(), String> {
    super::send_to_worker(state.engine.send(EngineCommand::Poll(PollReason::Wake)))
}

#[tauri::command]
pub(crate) async fn switch_account(
    app: AppHandle,
    state: State<'_, GuiState>,
    key: String,
    unsaved: String,
    label: Option<String>,
) -> Result<String, String> {
    let unsaved = match unsaved.as_str() {
        "refuse" => UnsavedPolicy::Refuse,
        "discard" => UnsavedPolicy::Discard,
        "save" => UnsavedPolicy::Save { label: None },
        other => return Err(format!("unknown unsaved policy {other:?}")),
    };
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::Switch {
            key,
            unsaved,
            label,
            by: By::User,
            reason: None,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    switch_reply(
        reply,
        || super::clear_pending(&app, &state),
        |pending| super::set_pending(&app, &state, pending),
    )
}

/// What a switch reply means for the pending switch. A failed switch drops a
/// stale pending one; only "not saved" keeps one and returns exactly that text.
fn switch_reply(
    reply: ActionReply,
    clear_pending: impl FnOnce() -> Result<(), String>,
    set_pending: impl FnOnce(super::worker::PendingSwitch) -> Result<(), String>,
) -> Result<String, String> {
    match reply {
        ActionReply::Switched(message) => {
            clear_pending()?;
            Ok(message)
        }
        ActionReply::NotSaved(pending) => {
            set_pending(pending)?;
            Err("not saved".to_string())
        }
        ActionReply::Failed(message) => {
            clear_pending()?;
            Err(message)
        }
        ActionReply::Done | ActionReply::Saved(_) => {
            clear_pending()?;
            Err("the action thread gave an unexpected reply".to_string())
        }
    }
}

#[tauri::command]
pub(crate) fn cancel_pending(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    super::clear_pending(&app, &state)
}

#[tauri::command]
pub(crate) async fn save_current(
    app: AppHandle,
    state: State<'_, GuiState>,
    label: Option<String>,
) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::SaveCurrent { label, reply })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => super::clear_pending(&app, &state),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

#[tauri::command]
pub(crate) async fn edit_account(
    state: State<'_, GuiState>,
    key: String,
    label: Option<String>,
    abbr: Option<String>,
    color: Option<String>,
    auto_burn: Option<bool>,
    // Absent leaves the price alone, `null` clears it, a number sets it. A
    // plain `Option<Option<f64>>` cannot tell the first two apart over JSON.
    price: Option<serde_json::Value>,
) -> Result<(), String> {
    let price = match price {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(value) => match value.as_f64() {
            Some(amount) => Some(Some(amount)),
            None => return Err("the price must be a number above 0 per month".to_string()),
        },
    };
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::Edit {
            key,
            label,
            abbr,
            color,
            auto_burn,
            price,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => Ok(()),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn edit_subscription(
    state: State<'_, GuiState>,
    account: AccountRef,
    label: Option<String>,
    abbr: Option<String>,
    color: Option<String>,
    // Absent leaves the plan alone, `null` clears it, text sets it.
    plan: Option<serde_json::Value>,
    // Absent leaves the price alone, `null` clears it, a number sets it.
    price: Option<serde_json::Value>,
) -> Result<(), String> {
    let plan = match plan {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(text)) => Some(Some(text)),
        Some(_) => return Err("the plan must be text or null".to_string()),
    };
    let price = match price {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(value) => match value.as_f64() {
            Some(amount) => Some(Some(amount)),
            None => return Err("the price must be a number above 0 per month".to_string()),
        },
    };
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::EditSubscription {
            account,
            label,
            abbr,
            color,
            plan,
            price,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => Ok(()),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

/// Connect `agy` from the Google card (spec §7.9): seatswap's status line in
/// its settings. A refusal (another status line, a broken file) is the error the
/// card shows.
#[tauri::command]
pub(crate) async fn connect_agy(state: State<'_, GuiState>) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::ConnectAgy { reply })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => Ok(()),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

/// Switch Codex's live login to a saved one (R8). The answer is the note the
/// pop-up shows under the card: it names the sessions still on the old login.
#[tauri::command]
pub(crate) async fn switch_codex(
    state: State<'_, GuiState>,
    account: AccountRef,
) -> Result<String, String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::SwitchCodex {
            account,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Switched(note) => Ok(note),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

/// Delete seatswap's saved copy of a Codex login (R8). The subscription and
/// Codex's own login file stay.
#[tauri::command]
pub(crate) async fn remove_codex_login(
    state: State<'_, GuiState>,
    account: AccountRef,
) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::RemoveCodexLogin {
            account,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => Ok(()),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

#[tauri::command]
pub(crate) async fn remove_account(state: State<'_, GuiState>, key: String) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::Remove { key, reply })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Done => Ok(()),
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

#[tauri::command]
pub(crate) async fn save_setting(
    state: State<'_, GuiState>,
    field: String,
    value: serde_json::Value,
) -> Result<SavedSetting, String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::SaveSetting {
            field,
            value,
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Saved(saved) => {
            state.remember_settings(saved.settings.clone());
            Ok(saved)
        }
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

/// Turn auto-burn on or off from the pop-up. It sends the one field through the
/// same settings path as the settings page, so the change is merged under the
/// app lock and written to the history in one place (`worker::save_setting`).
#[tauri::command]
/// The answer is the save's warning (the autostart entry could not be
/// applied), which the pop-up shows; the setting is saved all the same.
pub(crate) async fn set_auto_burn(
    state: State<'_, GuiState>,
    on: bool,
) -> Result<Option<String>, String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::SaveSetting {
            field: "autoBurn".to_string(),
            value: serde_json::Value::Bool(on),
            reply,
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    match reply {
        ActionReply::Saved(saved) => {
            state.remember_settings(saved.settings);
            Ok(saved.warning)
        }
        ActionReply::Failed(message) => Err(message),
        _ => Err("the action thread gave an unexpected reply".to_string()),
    }
}

/// The money facts the General group reads. The look-up thread owns them; this
/// only clones its view.
#[tauri::command]
pub(crate) async fn money_view(state: State<'_, GuiState>) -> Result<MoneyView, String> {
    state.lookup.view()
}

/// Every model with an effective price, for the price list under API prices.
#[tauri::command]
pub(crate) async fn price_list(state: State<'_, GuiState>) -> Result<Vec<PriceRow>, String> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        price_rows(&app).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// One look-up now, after a setting in the Money section changed.
#[tauri::command]
pub(crate) async fn lookup_now(state: State<'_, GuiState>) -> Result<MoneyView, String> {
    let lookup = state.lookup.clone();
    tauri::async_runtime::spawn_blocking(move || lookup.now())
        .await
        .map_err(|error| error.to_string())?
}

/// Pause auto-burn for `hours`, or resume with `None`. The action thread writes
/// the history event and wakes the engine.
#[tauri::command]
pub(crate) fn pause_burn(state: State<'_, GuiState>, hours: Option<i64>) -> Result<(), String> {
    super::send_to_worker(state.action.send(ActionCommand::PauseBurn { hours }))
}

/// One call by an agent as the settings page lists it (spec §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentCallRow {
    pub at: DateTime<Utc>,
    pub command: String,
    pub cwd: Option<String>,
    pub allowed: bool,
}

/// The history folder the tray app opened at startup, the one its recorder
/// keeps writing to; a folder that could not be opened is the error text.
fn history_of(state: &GuiState) -> Result<HistoryDir, String> {
    state.history.clone()
}

/// The `AgentCall` events of this host, newest first, at most `limit` of them,
/// each folder shown with `~` for `home`. A history that cannot be read is an
/// error, never an empty list.
fn agent_calls_from(
    history: &HistoryDir,
    home: &Path,
    limit: u32,
) -> Result<Vec<AgentCallRow>, String> {
    let read = history
        .read::<Event>(
            &Range {
                from: DateTime::<Utc>::MIN_UTC,
                to: DateTime::<Utc>::MAX_UTC,
            },
            &Hosts::One(history.host().to_string()),
        )
        .map_err(|error| error.to_string())?;
    let mut rows: Vec<AgentCallRow> = read
        .records
        .into_iter()
        .filter_map(|hosted| match hosted.record.kind {
            EventKind::AgentCall {
                command,
                cwd,
                allowed,
            } => Some(AgentCallRow {
                at: hosted.record.t,
                command,
                cwd: cwd.map(|cwd| with_tilde(&cwd, home)),
                allowed,
            }),
            _ => None,
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.at));
    Ok(rows.into_iter().take(limit as usize).collect())
}

// The three commands below read or write files: `async`, so they run off the
// window's thread, like the other commands that touch files.
#[tauri::command]
pub(crate) async fn agent_calls(
    state: State<'_, GuiState>,
    limit: u32,
) -> Result<Vec<AgentCallRow>, String> {
    agent_calls_from(&history_of(&state)?, &state.claude.home, limit)
}

/// The skill file as the page shows it: [`SkillStatus`] with the path written
/// with `~` for the home folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillRow {
    pub path: String,
    pub installed: bool,
    pub differs: bool,
}

fn skill_row(status: SkillStatus, home: &Path) -> SkillRow {
    SkillRow {
        path: with_tilde(&status.path.to_string_lossy(), home),
        installed: status.installed,
        differs: status.differs,
    }
}

#[tauri::command]
pub(crate) async fn skill_status(state: State<'_, GuiState>) -> Result<SkillRow, String> {
    agent_skill::status(&state.claude)
        .map(|status| skill_row(status, &state.claude.home))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn skill_install(
    state: State<'_, GuiState>,
    force: bool,
) -> Result<SkillRow, String> {
    agent_skill::install(&state.claude, force)
        .map(|status| skill_row(status, &state.claude.home))
        .map_err(|error| error.to_string())
}

/// Every host's session records and project log lines (spec §3.6), over all
/// time, with each folder written with `~` for `home` as the app shows paths
/// everywhere: a torn line is skipped by the reader with its warning, which
/// the caller prints through the app's one printer.
fn projects_from(
    history: &HistoryDir,
    home: &Path,
    accounts: &[Account],
) -> Result<(Projects, Vec<String>), String> {
    let (mut projects, mut warnings) =
        projects::load(history, accounts).map_err(|error| error.to_string())?;
    warnings.extend(
        projects
            .with_tokens(history)
            .map_err(|error| error.to_string())?,
    );
    for project in &mut projects.projects {
        for cwds in project.folders.values_mut() {
            for cwd in cwds.iter_mut() {
                *cwd = with_tilde(cwd, home);
            }
        }
    }
    Ok((projects, warnings))
}

/// One project edit: validated first, so a refused edit appends nothing. The
/// line goes to this host's own log through the history it already has, and
/// the new resolution is returned with the read's warnings.
fn edit_projects(
    history: &HistoryDir,
    home: &Path,
    accounts: &[Account],
    at: DateTime<Utc>,
    edit: ProjectEdit,
) -> Result<(Projects, Vec<String>), String> {
    // The edit is checked on its own and against the projects as they are now
    // (a second organization of one name, an owner's organization removed):
    // a refused edit never reaches the shared history. This pass needs the
    // resolution only, not the token lines or the `~` folders: read it
    // directly, so one Settings edit does not read every token line twice.
    let (current, _) = projects::load(history, accounts).map_err(|error| error.to_string())?;
    current
        .validate_edit(&edit)
        .map_err(|error| error.to_string())?;
    history
        .append(&ProjectsLogRecord { t: at, edit })
        .map_err(|error| error.to_string())?;
    projects_from(history, home, accounts)
}

/// The stored accounts, read from `accounts.json`; an unreadable file is the
/// error text, never an empty list.
fn stored_accounts(state: &GuiState) -> Result<Vec<Account>, String> {
    store::load_accounts(&state.app)
        .map(|file| file.accounts)
        .map_err(|error| error.to_string())
}

/// The Projects group: every project of every host with its folders, its
/// organization and its session count.
#[tauri::command]
pub(crate) async fn projects_view(state: State<'_, GuiState>) -> Result<Projects, String> {
    let (projects, warnings) = projects_from(
        &history_of(&state)?,
        &state.claude.home,
        &stored_accounts(&state)?,
    )?;
    super::lock(&state.printed_warnings).print(Some("history"), &warnings);
    Ok(projects)
}

/// Rename, join, unjoin or assign a project, or edit an organization. Returns
/// the new resolution; a refused edit is the error text and appends nothing.
#[tauri::command]
pub(crate) async fn projects_edit(
    state: State<'_, GuiState>,
    edit: ProjectEdit,
) -> Result<Projects, String> {
    let (projects, warnings) = edit_projects(
        &history_of(&state)?,
        &state.claude.home,
        &stored_accounts(&state)?,
        Utc::now(),
        edit,
    )?;
    super::lock(&state.printed_warnings).print(Some("history"), &warnings);
    Ok(projects)
}

// ---------------------------------------------------------------------------
// History and hosts (spec §1.5, §3.7; step C1 task 7)
// ---------------------------------------------------------------------------

/// What the Settings page's History and hosts group shows: this host's name,
/// the folder the history is read from, and every host folder in it, this host
/// included even before it holds a record. `saved_host` and `saved_dir` are the
/// saved settings' values when they differ from the ones in effect: a restart
/// makes those take effect.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryView {
    host: String,
    dir: String,
    hosts: Vec<HostInfo>,
    bytes: u64,
    saved_host: Option<String>,
    saved_dir: Option<String>,
    /// `<Dropbox>/seatswap` when Dropbox's `info.json` names a folder and no
    /// `historyDir` is set: a suggestion only, nothing is created or written.
    suggested_folder: Option<String>,
    /// Why Dropbox's `info.json` could not be read, when it could not; the
    /// suggestion is then absent and the page shows this text.
    suggested_error: Option<String>,
}

/// What an import copied, the hosts it refused, and the folder afterwards.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportResult {
    copied: Vec<String>,
    conflicts: Vec<HostConflict>,
    warnings: Vec<String>,
    view: HistoryView,
}

/// The saved settings' host name and folder when they differ from the ones the
/// recorder opened at startup; `None` when they agree or no settings are loaded.
fn saved_history(
    app: &seatswap_core::paths::AppPaths,
    home: &Path,
    settings: Option<Settings>,
    history: &HistoryDir,
) -> Result<(Option<String>, Option<String>), String> {
    let Some(settings) = settings else {
        return Ok((None, None));
    };
    let (root, host) =
        seatswap_core::history::location(app, &settings).map_err(|error| error.to_string())?;
    Ok((
        (host != history.host()).then_some(host),
        (root != history.root()).then(|| with_tilde(&root.to_string_lossy(), home)),
    ))
}

fn history_view_of(
    history: &HistoryDir,
    home: &Path,
    saved_host: Option<String>,
    saved_dir: Option<String>,
    suggested_folder: Option<String>,
    suggested_error: Option<String>,
) -> Result<HistoryView, String> {
    let hosts = history.host_infos().map_err(|error| error.to_string())?;
    let bytes = hosts
        .iter()
        .try_fold(0u64, |total, info| total.checked_add(info.bytes))
        .ok_or_else(|| "the host folders add up to more than 2^64 bytes".to_string())?;
    Ok(HistoryView {
        host: history.host().to_string(),
        dir: with_tilde(&history.root().to_string_lossy(), home),
        hosts,
        bytes,
        saved_host,
        saved_dir,
        suggested_folder,
        suggested_error,
    })
}

/// Dropbox's suggested history folder, `<Dropbox>/seatswap`, and the error when
/// Dropbox's `info.json` cannot be read. The file is only read when no
/// `historyDir` is set, and nothing is created or written for the suggestion.
fn dropbox_suggestion(home: &Path) -> (Option<String>, Option<String>) {
    match seatswap_core::paths::dropbox_folder() {
        Ok(Some(dropbox)) => (
            Some(with_tilde(
                &dropbox.join("seatswap").to_string_lossy(),
                home,
            )),
            None,
        ),
        Ok(None) => (None, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// Whether the view may suggest Dropbox's folder: only when the saved settings
/// name no history folder. Settings that could not be read never make a
/// suggestion.
fn wants_dropbox_suggestion(settings: Option<&Settings>) -> bool {
    settings.is_some_and(|settings| settings.history_dir.is_none())
}

/// The history view of the folder in effect, with the saved values that a
/// restart would use.
fn history_view_for(state: &GuiState, history: &HistoryDir) -> Result<HistoryView, String> {
    let snapshot = state.snapshot()?;
    let (suggested_folder, suggested_error) =
        if wants_dropbox_suggestion(snapshot.settings.as_ref()) {
            dropbox_suggestion(&state.claude.home)
        } else {
            (None, None)
        };
    let (saved_host, saved_dir) =
        saved_history(&state.app, &state.claude.home, snapshot.settings, history)?;
    history_view_of(
        history,
        &state.claude.home,
        saved_host,
        saved_dir,
        suggested_folder,
        suggested_error,
    )
}

/// The Settings page's History and hosts group.
#[tauri::command]
pub(crate) async fn history_view(state: State<'_, GuiState>) -> Result<HistoryView, String> {
    let history = history_of(&state)?;
    history_view_for(&state, &history)
}

/// Copy another history folder's host folders in and show what the folder holds
/// afterwards. A folder that is this history folder, or that holds no host
/// folder, is an error.
#[tauri::command]
pub(crate) async fn history_import(
    state: State<'_, GuiState>,
    from: String,
) -> Result<ImportResult, String> {
    let history = history_of(&state)?;
    let from = expand_tilde(&from, &state.claude.home);
    if !from.is_absolute() {
        return Err(format!(
            "the folder must be an absolute path, got {}",
            from.display()
        ));
    }
    let imported = history.import(&from).map_err(|error| error.to_string())?;
    let view = history_view_for(&state, &history)?;
    Ok(ImportResult {
        copied: imported.copied,
        conflicts: imported.conflicts,
        warnings: imported.warnings,
        view,
    })
}

/// The range an export covers: `from` inclusive, `until` exclusive, either end
/// open when it is `None`.
fn export_range(
    from: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Range, String> {
    Range::between(from, until).map_err(|error| error.to_string())
}

/// Write the history of the range to `to` as CSV or JSON. An existing file is
/// refused, never overwritten; the file is written through `atomic`, so a
/// reader never sees half of it.
fn export_history(
    history: &HistoryDir,
    format: &str,
    to: &Path,
    from: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let range = export_range(from, until)?;
    if !to.is_absolute() {
        return Err(format!(
            "the export path must be an absolute path, got {}",
            to.display()
        ));
    }
    if to.exists() {
        return Err(format!("{} exists; choose another name", to.display()));
    }
    let mut bytes = Vec::new();
    match format {
        "csv" => export_csv(history, &range, &mut bytes),
        "json" => export_json(history, &range, &mut bytes),
        other => {
            return Err(format!(
                "unknown export format {other:?}: use \"csv\" or \"json\""
            ))
        }
    }
    .map_err(|error| error.to_string())?;
    atomic::write_atomic(to, &bytes, Access::Private)
        .map_err(|source| format!("{}: {source}", to.display()))
}

/// Write the history as CSV or JSON to a file the person names.
#[tauri::command]
pub(crate) async fn history_export(
    state: State<'_, GuiState>,
    format: String,
    to: String,
    from: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let to = expand_tilde(&to, &state.claude.home);
    export_history(&history_of(&state)?, &format, &to, from, until)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IconPx {
    size: u32,
    rgba: Vec<u8>,
}

#[tauri::command]
pub(crate) fn icon_preview(abbr: String, color: String) -> Result<Vec<IconPx>, String> {
    let abbr = store::validate_abbr(&abbr, &[]).map_err(|error| error.to_string())?;
    let color = store::validate_color(&color).map_err(|error| error.to_string())?;
    let account_color =
        format::parse_color(&color).ok_or_else(|| format!("invalid colour {color:?}"))?;
    let spec = IconSpec {
        text: abbr,
        account_color,
        ring: None,
        locked: false,
        monochrome: false,
    };
    Ok([16_u32, 22, 32]
        .into_iter()
        .map(|size| IconPx {
            size,
            rgba: icon::render(&spec, size),
        })
        .collect())
}

/// How the panel writes a time: `countdown` to a reset as in the cards
/// (`format_reset_at`), `clock` as in the switch suggestion
/// (`format_reset_clock`), `dayClock` for a call today as `09:41`, within the
/// last six days as `Thu 09:41`, and older as `13 Nov 09:41`, `age` of a poll
/// or a usage fetch (`format_age`), `day` for a looked-up value's time in local
/// time, `date` for a calendar date without a time zone conversion (`13 Nov`),
/// `dayYear` for the local calendar day and year (`17 Aug 2026`), and `left`
/// for the time left until it in the status line's compact form (`3d 2h`,
/// `14h 3min`; `format::time_left`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TimeStyle {
    Countdown,
    Clock,
    Age,
    Day,
    Date,
    DayClock,
    DayYear,
    Left,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct TimeRequest {
    at: DateTime<Utc>,
    style: TimeStyle,
}

/// The panel's time texts, one per request in order, so the panel uses the
/// same rules as the tray and the CLI instead of a copy in JavaScript.
#[tauri::command]
pub(crate) fn time_texts(times: Vec<TimeRequest>) -> Vec<String> {
    time_texts_at(&times, Utc::now(), &Local)
}

fn time_texts_at<Tz: TimeZone>(times: &[TimeRequest], now: DateTime<Utc>, tz: &Tz) -> Vec<String>
where
    Tz::Offset: std::fmt::Display,
{
    times
        .iter()
        .map(|time| match time.style {
            TimeStyle::Countdown => format::format_reset_at(time.at, now, tz),
            TimeStyle::Clock => format::format_reset_clock(time.at, now, tz),
            TimeStyle::Age => format::format_age(time.at, now),
            TimeStyle::Day => format::day(time.at, tz),
            TimeStyle::Date => format::date(time.at),
            TimeStyle::DayClock => format::day_clock(time.at, now, tz),
            TimeStyle::DayYear => format::day_year(time.at, tz),
            TimeStyle::Left => format::time_left(time.at, now),
        })
        .collect()
}

#[tauri::command]
pub(crate) fn fit_panel(
    app: AppHandle,
    state: State<'_, GuiState>,
    height: f64,
) -> Result<(), String> {
    if !height.is_finite() || height < 1.0 {
        return Err(format!("invalid panel height {height}"));
    }
    let window = app
        .get_webview_window(PANEL_LABEL)
        .ok_or_else(|| "the panel is not open".to_string())?;
    window
        .set_size(LogicalSize::new(PANEL_WIDTH, height))
        .map_err(|error| error.to_string())?;
    let (anchor, pending) = {
        let mut panel = super::lock(&state.panel);
        panel.height = height;
        (panel.anchor, panel.pending_show)
    };
    if pending {
        return reveal(&app, &state);
    }
    match anchor {
        // On Wayland the compositor ignores the position; `move_to` centres
        // anyway so both paths stay one.
        Some(anchor) => move_to(&window, &anchor, height),
        None => Ok(()),
    }
}

#[tauri::command]
pub(crate) fn hide_panel(app: AppHandle) -> Result<(), String> {
    hide(&app)
}

/// Open the pop-up from a page, the way the tray and the notifications do.
#[tauri::command]
pub(crate) fn open_panel(app: AppHandle) {
    super::show_panel(&app, "main");
}

/// Send one request and wait for its own reply. Runs on the blocking pool.
fn request(
    action: &mpsc::Sender<ActionCommand>,
    build: impl FnOnce(mpsc::Sender<ActionReply>) -> ActionCommand,
) -> Result<ActionReply, String> {
    let (reply, received) = mpsc::channel();
    super::send_to_worker(action.send(build(reply)))?;
    super::worker_reply(received.recv())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use seatswap_core::history::Recorder;
    use seatswap_core::paths::ClaudePaths;
    use seatswap_core::project::ProjectKey;
    use seatswap_core::projects::ProjectId;

    fn monitor() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    #[test]
    fn lower_half_puts_the_panel_above_the_cursor() {
        let (x, y) = place_panel((960.0, 900.0), monitor(), (380.0, 400.0), false);
        assert_eq!(x, 960.0 - 190.0);
        assert_eq!(y, 900.0 - 400.0);
    }

    #[test]
    fn upper_half_puts_the_panel_below_the_cursor() {
        let (x, y) = place_panel((960.0, 100.0), monitor(), (380.0, 400.0), false);
        assert_eq!(x, 960.0 - 190.0);
        assert_eq!(y, 100.0);
    }

    #[test]
    fn the_panel_is_clamped_at_all_four_monitor_edges() {
        // Left edge.
        let (x, _) = place_panel((10.0, 100.0), monitor(), (380.0, 400.0), false);
        assert_eq!(x, 0.0);
        // Right edge.
        let (x, _) = place_panel((1910.0, 100.0), monitor(), (380.0, 400.0), false);
        assert_eq!(x, 1920.0 - 380.0);
        // Top edge.
        let (_, y) = place_panel((960.0, -10.0), monitor(), (380.0, 400.0), false);
        assert_eq!(y, 0.0);
        // Bottom edge.
        let (_, y) = place_panel((960.0, 1200.0), monitor(), (380.0, 400.0), false);
        assert_eq!(y, 1080.0 - 400.0);
    }

    #[test]
    fn a_second_monitor_uses_its_own_origin() {
        let second = Rect {
            x: 1920.0,
            y: 0.0,
            width: 1280.0,
            height: 1024.0,
        };
        let (x, y) = place_panel((2000.0, 900.0), second, (380.0, 400.0), false);
        assert_eq!(x, 1920.0);
        assert_eq!(y, 900.0 - 400.0);
    }

    #[test]
    fn agent_calls_returns_the_newest_first() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        let call = |at: &str, command: &str, allowed: bool| {
            recorder
                .event(
                    DateTime::parse_from_rfc3339(at)
                        .expect("a test time")
                        .with_timezone(&Utc),
                    EventKind::AgentCall {
                        command: command.to_string(),
                        cwd: Some(
                            crate::test_support::at_home("projects/ledger")
                                .display()
                                .to_string(),
                        ),
                        allowed,
                    },
                )
                .expect("an agent call event");
        };
        call("2026-11-10T09:41:00Z", "status --json", true);
        call("2026-11-10T09:42:00Z", "use PR", false);
        call("2026-11-10T09:43:00Z", "next", true);
        // Another host writes the same history folder; its calls are not this host's.
        Recorder::new(HistoryDir::at(history.root(), "vega"))
            .event(
                DateTime::parse_from_rfc3339("2026-11-10T09:44:00Z")
                    .expect("a test time")
                    .with_timezone(&Utc),
                EventKind::AgentCall {
                    command: "status".to_string(),
                    cwd: None,
                    allowed: true,
                },
            )
            .expect("another host's agent call");

        let home = crate::test_support::home();
        let rows = agent_calls_from(&history, home, 20).expect("the agent calls are read");
        let commands: Vec<&str> = rows.iter().map(|row| row.command.as_str()).collect();
        assert_eq!(commands, ["next", "use PR", "status --json"]);
        assert!(!rows[1].allowed, "use PR was refused");
        let expected = crate::test_support::tilde("projects/ledger");
        assert_eq!(rows[0].cwd.as_deref(), Some(expected.as_str()));

        let two = agent_calls_from(&history, home, 2).expect("two agent calls are read");
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].command, "next");
        assert_eq!(two[1].command, "use PR");
    }

    #[test]
    fn skill_status_knows_missing_same_and_different() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let claude = ClaudePaths::for_config_dir(&tmp.path().join("claude"), tmp.path());
        let path = claude.skills_dir().join("seatswap/SKILL.md");

        let missing = agent_skill::status(&claude).expect("the status is readable");
        assert!(!missing.installed, "no file yet");
        assert!(!missing.differs);
        assert_eq!(missing.path, path);

        fs::create_dir_all(path.parent().expect("the skill folder")).expect("the skill folder");
        fs::write(&path, agent_skill::AGENT_SKILL).expect("the bundled skill");
        let same = agent_skill::status(&claude).expect("the status is readable");
        assert!(same.installed);
        assert!(!same.differs);

        fs::write(&path, "another skill").expect("a different skill");
        let different = agent_skill::status(&claude).expect("the status is readable");
        assert!(different.installed);
        assert!(different.differs);

        let row = skill_row(different, tmp.path());
        assert_eq!(
            row.path,
            format!(
                "~{0}claude{0}skills{0}seatswap{0}SKILL.md",
                std::path::MAIN_SEPARATOR
            )
        );
        assert!(row.installed && row.differs);
    }

    #[test]
    fn a_name_over_40_characters_is_refused() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let key = ProjectId::of(&ProjectKey::Folder("atlas".to_string()), "atlas");
        let at = DateTime::parse_from_rfc3339("2026-11-14T10:00:00Z")
            .expect("a test time")
            .with_timezone(&Utc);
        let rename = |name: String| ProjectEdit::Rename {
            key: key.clone(),
            name,
        };
        let log = tmp.path().join("atlas/projects-2026-11.jsonl");

        let refused = edit_projects(&history, tmp.path(), &[], at, rename("a".repeat(41)))
            .expect_err("a name over 40 characters is refused");
        assert!(refused.contains("40"), "{refused}");
        assert!(
            !log.exists(),
            "a refused edit appends nothing, but {} exists",
            log.display()
        );

        // Exactly 40 characters are accepted, and the line lands in this
        // host's own project log.
        edit_projects(&history, tmp.path(), &[], at, rename("a".repeat(40)))
            .expect("a name of 40 characters is accepted");
        let lines = fs::read_to_string(&log).expect("the project log");
        assert_eq!(lines.lines().count(), 1, "{lines}");
        assert!(lines.contains(&"a".repeat(40)), "{lines}");
    }

    #[test]
    fn project_folders_are_written_with_tilde() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let at = DateTime::parse_from_rfc3339("2026-11-14T10:00:00Z")
            .expect("a test time")
            .with_timezone(&Utc);
        history
            .append(&seatswap_core::history::SessionRecord {
                t: at,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: crate::test_support::at_home("projects/atlas")
                    .display()
                    .to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                entrypoint: Some("cli".to_string()),
                title: None,
                first: at,
                last: at,
                agents_started: 0,
                agents_finished: 0,
                closed: false,
            })
            .expect("a session line");
        let (projects, warnings) = projects_from(&history, crate::test_support::home(), &[])
            .expect("the projects resolve");
        assert!(warnings.is_empty(), "a clean history has no warning");
        assert_eq!(projects.projects.len(), 1);
        assert_eq!(
            projects.projects[0].folders.get("atlas"),
            Some(&vec![crate::test_support::tilde("projects/atlas")])
        );
    }

    #[test]
    fn icon_preview_returns_the_three_panel_sizes() {
        let preview = icon_preview("as".to_string(), "#AABBCC".to_string()).expect("preview");
        let sizes: Vec<u32> = preview.iter().map(|icon| icon.size).collect();
        assert_eq!(sizes, vec![16, 22, 32]);
        for icon in preview {
            assert_eq!(icon.rgba.len(), (icon.size * icon.size * 4) as usize);
        }

        assert!(icon_preview("ABC".to_string(), "#aabbcc".to_string()).is_err());
        assert!(icon_preview("AS".to_string(), "blue".to_string()).is_err());
    }

    fn anchor(cursor: (f64, f64)) -> Anchor {
        Anchor {
            cursor,
            monitor: monitor(),
            scale: 1.0,
        }
    }

    #[test]
    fn a_resize_keeps_the_top_edge_when_the_panel_is_below_the_mouse() {
        let below = anchor((960.0, 100.0));
        assert_eq!(anchored_position(&below, 400.0, false), (770.0, 100.0));
        assert_eq!(anchored_position(&below, 84.0, false), (770.0, 100.0));
        assert_eq!(anchored_position(&below, 713.0, false), (770.0, 100.0));
    }

    #[test]
    fn a_resize_keeps_the_bottom_edge_when_the_panel_is_above_the_mouse() {
        let above = anchor((960.0, 900.0));
        assert_eq!(anchored_position(&above, 400.0, false), (770.0, 500.0));
        assert_eq!(anchored_position(&above, 84.0, false), (770.0, 816.0));
        assert_eq!(anchored_position(&above, 713.0, false), (770.0, 187.0));
    }

    /// The headless run of 2026-11-10: mouse in the middle of a 1280×800
    /// screen, the page asking 84, 472 and then 713 px. Reading the window's
    /// stale size back moved the panel to y = 836, then 884 (off screen) and
    /// for the tall settings page to y = -193, -386. From the anchor, every
    /// height lands on screen and repeated calls give the same answer.
    #[test]
    fn repeated_resizes_never_leave_the_monitor() {
        let small = Anchor {
            cursor: (640.0, 400.0),
            monitor: Rect {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
            scale: 1.0,
        };
        for height in [84.0, 84.0, 472.0, 472.0, 713.0, 713.0, 713.0, 1200.0] {
            let (x, y) = anchored_position(&small, height, false);
            assert_eq!(x, 450.0);
            assert!(y >= 0.0, "height {height}: y {y} above the screen");
            assert!(
                y + height <= 800.0 || y == 0.0,
                "height {height}: y {y} runs off the bottom"
            );
        }
        assert_eq!(anchored_position(&small, 84.0, false), (450.0, 316.0));
        assert_eq!(anchored_position(&small, 713.0, false), (450.0, 0.0));
    }

    #[test]
    fn the_scale_turns_logical_sizes_into_physical_pixels() {
        let hidpi = Anchor {
            cursor: (1920.0, 1800.0),
            monitor: Rect {
                x: 0.0,
                y: 0.0,
                width: 3840.0,
                height: 2160.0,
            },
            scale: 2.0,
        };
        // 380 × 400 logical is 760 × 800 physical, centred on the mouse, above it.
        assert_eq!(anchored_position(&hidpi, 400.0, false), (1540.0, 1000.0));
    }

    #[test]
    fn a_resize_on_wayland_stays_centred() {
        let (x, y) = anchored_position(&anchor((10.0, 10.0)), 520.0, true);
        assert_eq!(x, (1920.0 - 380.0) / 2.0);
        assert_eq!(y, (1080.0 - 520.0) / 2.0);
    }

    #[test]
    fn a_tray_click_closes_an_open_panel() {
        assert_eq!(tray_click(true, None), TrayClick::Hide);
        assert_eq!(
            tray_click(true, Some(Duration::from_millis(50))),
            TrayClick::Hide
        );
    }

    #[test]
    fn the_click_that_took_the_focus_away_does_not_reopen_the_panel() {
        assert_eq!(
            tray_click(false, Some(Duration::from_millis(120))),
            TrayClick::Ignore
        );
        assert_eq!(
            tray_click(false, Some(Duration::from_millis(399))),
            TrayClick::Ignore
        );
    }

    #[test]
    fn a_later_tray_click_opens_the_panel() {
        assert_eq!(tray_click(false, None), TrayClick::Show);
        assert_eq!(tray_click(false, Some(REOPEN_GUARD)), TrayClick::Show);
        assert_eq!(
            tray_click(false, Some(Duration::from_secs(30))),
            TrayClick::Show
        );
    }

    #[test]
    fn a_failed_switch_clears_a_stale_pending() {
        let mut cleared = false;
        let mut set = false;
        let error = switch_reply(
            ActionReply::Failed("no such account".to_string()),
            || {
                cleared = true;
                Ok(())
            },
            |_| {
                set = true;
                Ok(())
            },
        )
        .expect_err("the error is returned");
        assert_eq!(error, "no such account");
        assert!(cleared, "a failed switch must drop the pending one");
        assert!(!set, "a failed switch must not set a pending one");
    }

    #[test]
    fn only_not_saved_keeps_the_pending_switch() {
        let pending = crate::gui::worker::PendingSwitch {
            target_id: "target".to_string(),
            target_label: "Private".to_string(),
            email: Some("new@example.com".to_string()),
            org_name: None,
        };
        let mut cleared = false;
        let mut saved = None;
        let error = switch_reply(
            ActionReply::NotSaved(pending.clone()),
            || {
                cleared = true;
                Ok(())
            },
            |pending| {
                saved = Some(pending);
                Ok(())
            },
        )
        .expect_err("not saved is an error");
        assert_eq!(error, "not saved");
        assert!(!cleared, "not saved keeps the pending switch");
        assert_eq!(saved, Some(pending));
    }

    #[test]
    fn a_successful_switch_clears_the_pending() {
        let mut cleared = false;
        let message = switch_reply(
            ActionReply::Switched("Switched to AC — Acme".to_string()),
            || {
                cleared = true;
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("a switch succeeds");
        assert_eq!(message, "Switched to AC — Acme");
        assert!(cleared);
    }

    /// The panel sends what `panel.js` puts into `data-time` (the snapshot's
    /// RFC 3339 times) and gets the tray's and the CLI's texts back.
    #[test]
    fn time_texts_use_the_shared_rules_in_order() {
        let times: Vec<TimeRequest> = serde_json::from_value(serde_json::json!([
            { "at": "2026-11-11T14:39:59.591693Z", "style": "countdown" },
            { "at": "2026-11-11T14:39:59.591693Z", "style": "clock" },
            { "at": "2026-11-15T02:00:00Z", "style": "countdown" },
            { "at": "2026-11-15T02:00:00Z", "style": "clock" },
            { "at": "2026-11-11T13:34:48Z", "style": "age" },
            { "at": "2026-11-11T13:30:00Z", "style": "age" },
            { "at": "2026-11-11T14:39:59.591693Z", "style": "left" },
            { "at": "2026-11-15T02:00:00Z", "style": "left" },
        ]))
        .expect("the panel's request");
        let now = DateTime::parse_from_rfc3339("2026-11-11T13:35:00Z")
            .expect("now")
            .with_timezone(&Utc);
        assert_eq!(
            time_texts_at(&times, now, &Utc),
            vec![
                "1h 5min",
                "14:40",
                "Sun 02:00",
                "Sun 02:00",
                "12s ago",
                "5min ago",
                "1h 5min",
                "3d 13h"
            ]
        );
    }

    /// Last calls keep the clock today, name the weekday within the last six
    /// days, and keep the day and month for anything older.
    #[test]
    fn day_clock_names_the_day_unless_today() {
        let times: Vec<TimeRequest> = serde_json::from_value(serde_json::json!([
            { "at": "2026-11-20T09:41:00Z", "style": "dayClock" },
            { "at": "2026-11-19T09:41:00Z", "style": "dayClock" },
            { "at": "2026-11-14T09:41:00Z", "style": "dayClock" },
            { "at": "2026-11-13T09:41:00Z", "style": "dayClock" },
        ]))
        .expect("the panel's request");
        let now = DateTime::parse_from_rfc3339("2026-11-20T13:35:00Z")
            .expect("now")
            .with_timezone(&Utc);
        assert_eq!(
            time_texts_at(&times, now, &Utc),
            vec!["09:41", "Thu 09:41", "Sat 09:41", "13 Nov 09:41"]
        );
    }

    /// A `dayYear` is the local calendar day: a first record on 31 Dec 23:30
    /// UTC reads 1 Jan 2027 at UTC+1 and 31 Dec 2026 at UTC.
    #[test]
    fn time_texts_day_year_read_the_local_calendar_day() {
        let times: Vec<TimeRequest> = serde_json::from_value(serde_json::json!([
            { "at": "2026-12-31T23:30:00Z", "style": "dayYear" },
        ]))
        .expect("the panel's request");
        let now = DateTime::parse_from_rfc3339("2026-12-31T23:30:00Z")
            .expect("now")
            .with_timezone(&Utc);
        let plus_one = chrono::FixedOffset::east_opt(60 * 60).expect("a time zone");
        assert_eq!(time_texts_at(&times, now, &plus_one), vec!["1 Jan 2027"]);
        assert_eq!(time_texts_at(&times, now, &Utc), vec!["31 Dec 2026"]);
    }

    /// The rate's date is a calendar date: `13 Nov` also west of UTC, where a
    /// local-time rendering of 00:00Z would say the 12th.
    #[test]
    fn a_calendar_date_is_not_moved_by_the_time_zone() {
        let times: Vec<TimeRequest> = serde_json::from_value(serde_json::json!([
            { "at": "2026-11-13T00:00:00Z", "style": "date" },
            { "at": "2026-11-13T00:00:00Z", "style": "day" },
        ]))
        .expect("the panel's request");
        let now = DateTime::parse_from_rfc3339("2026-11-13T12:00:00Z")
            .expect("now")
            .with_timezone(&Utc);
        let west = chrono::FixedOffset::west_opt(5 * 3600).expect("a time zone");
        assert_eq!(time_texts_at(&times, now, &west), vec!["13 Nov", "12 Nov"]);
    }

    #[test]
    fn a_time_request_with_an_unknown_style_or_time_is_refused() {
        let unknown_style = serde_json::from_value::<TimeRequest>(
            serde_json::json!({ "at": "2026-11-11T14:40:00Z", "style": "weekday" }),
        );
        assert!(unknown_style.is_err());
        let bad_time = serde_json::from_value::<TimeRequest>(
            serde_json::json!({ "at": "tomorrow", "style": "clock" }),
        );
        assert!(bad_time.is_err());
    }

    #[test]
    fn saved_history_is_the_settings_folder() {
        let tmp = tempfile::tempdir().expect("a temp folder");
        let app = seatswap_core::paths::AppPaths::at(&tmp.path().join("data"));
        let home = Path::new("/home/sam");
        for settings in [
            Settings {
                history_dir: Some(tmp.path().join("shared")),
                host_name: Some("vega".to_string()),
                ..Settings::default()
            },
            Settings::default(),
        ] {
            let (root, host) =
                seatswap_core::history::location(&app, &settings).expect("a history location");
            let history = HistoryDir::at(&root, &host);
            let (saved_host, saved_dir) =
                saved_history(&app, home, Some(settings), &history).expect("the saved history");
            assert_eq!(saved_host, None, "the saved and the open host agree");
            assert_eq!(saved_dir, None, "the saved and the open folder agree");
        }
    }

    #[test]
    fn a_dropbox_suggestion_needs_no_history_folder() {
        assert!(wants_dropbox_suggestion(Some(&Settings::default())));
        let with_dir = Settings {
            history_dir: Some(Path::new("/home/sam/Sync/seatswap-history").to_path_buf()),
            ..Settings::default()
        };
        assert!(!wants_dropbox_suggestion(Some(&with_dir)));
        // Settings that could not be read must not make one up either.
        assert!(!wants_dropbox_suggestion(None));
    }

    /// The names the page reads: `suggestedFolder` and `suggestedError`. A
    /// renamed field would leave the page without a suggestion and without
    /// the reason, and nothing else would show it.
    #[test]
    fn the_history_view_carries_the_suggestion_the_page_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let view = history_view_of(
            &history,
            Path::new("/home/sam"),
            None,
            None,
            Some("~/Dropbox/seatswap".to_string()),
            Some("Dropbox's info file /home/sam/.dropbox/info.json: not JSON".to_string()),
        )
        .expect("the history view");
        let json = serde_json::to_value(&view).expect("the view as JSON");
        assert_eq!(json["suggestedFolder"], "~/Dropbox/seatswap");
        assert_eq!(
            json["suggestedError"],
            "Dropbox's info file /home/sam/.dropbox/info.json: not JSON"
        );

        // Without a suggestion the fields are null, never missing.
        let view = history_view_of(&history, Path::new("/home/sam"), None, None, None, None)
            .expect("the history view");
        let json = serde_json::to_value(&view).expect("the view as JSON");
        assert!(json["suggestedFolder"].is_null());
        assert!(json["suggestedError"].is_null());
    }

    #[test]
    fn history_export_refuses_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let out = dir.path().join("seatswap-history.csv");
        fs::write(&out, "an older export").expect("an existing file");

        let error = export_history(&history, "csv", &out, None, None)
            .expect_err("an existing file is refused");
        assert!(error.contains(&out.display().to_string()), "{error}");
        assert!(error.contains("exists; choose another name"), "{error}");
        assert_eq!(
            fs::read_to_string(&out).expect("the file"),
            "an older export",
            "the file is never overwritten"
        );

        // A new name gets the header and this host's empty history.
        let fresh = dir.path().join("seatswap-history-2.csv");
        export_history(&history, "csv", &fresh, None, None).expect("the export is written");
        assert_eq!(
            fs::read_to_string(&fresh).expect("the export"),
            "kind,account,org,limit,model,start,end,percent,day_utc,project,input,output,cache_write,cache_read\n"
        );

        // The format is checked before anything is written.
        let error = export_history(&history, "xml", &dir.path().join("x.xml"), None, None)
            .expect_err("an unknown format");
        assert!(error.contains("unknown export format"), "{error}");
        assert!(!dir.path().join("x.xml").exists());
    }

    #[test]
    fn wayland_centres_the_panel() {
        let (x, y) = place_panel((10.0, 10.0), monitor(), (380.0, 400.0), true);
        assert_eq!(x, (1920.0 - 380.0) / 2.0);
        assert_eq!(y, (1080.0 - 400.0) / 2.0);
    }

    /// The Settings page's first line is built from this one string; it must be
    /// the crate's version and read as three numbers (R7).
    #[test]
    fn app_version_is_the_crate_version() {
        let version = app_version();
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert!(!version.is_empty(), "the version is empty");
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "not three dot-separated numbers: {version:?}"
        );
        for part in parts {
            assert!(
                !part.is_empty() && part.chars().all(|digit| digit.is_ascii_digit()),
                "not a number in {version:?}"
            );
        }
    }
}
