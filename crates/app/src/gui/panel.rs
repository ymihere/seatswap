//! The pop-up window, its placement and the Tauri commands the panel calls
//! (spec §9.2, §11; docs/implementation.md "app: GUI", "Panel UI").
//!
//! All commands return `Result<_, String>` because that is what
//! `window.__TAURI__.core.invoke` rejects with; the error text is shown in the
//! panel.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tauri::webview::PageLoadEvent;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use seatswap_core::engine::PollReason;
use seatswap_core::store::{self, Settings};
use seatswap_core::switcher::UnsavedPolicy;

use super::format;
use super::worker::{ActionCommand, ActionReply, EngineCommand, Snapshot};
use super::GuiState;
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
/// Focus can leave for a moment (a drop-down list of a `<select>` opens its own
/// window); the panel hides only if it is still unfocused after this delay.
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
    if !matches!(view, "main" | "add" | "settings") {
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
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.eval(format!("window.location.hash = {initial:?};"));
            }
        })
        .build()
        .map_err(|error| error.to_string())?;

    focus_when_mapped(&window)?;

    // Losing focus hides the pop-up (spec §11): clicking elsewhere is a dismiss.
    // Checked again after FOCUS_RECHECK, so a focus that only passes through a
    // drop-down list does not close it.
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
                    if window.is_focused().unwrap_or(true) {
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
const FOCUS_RETRIES: u32 = 5;
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
        ActionReply::Done => {
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
) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::Edit {
            key,
            label,
            abbr,
            color,
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
pub(crate) async fn save_settings(
    state: State<'_, GuiState>,
    settings: Settings,
) -> Result<(), String> {
    let action = state.action.clone();
    let reply = tauri::async_runtime::spawn_blocking(move || {
        request(&action, |reply| ActionCommand::SaveSettings {
            settings,
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
        abbr,
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

/// How the panel writes a reset time: `countdown` as in the cards
/// (`format_reset_at`), `clock` as in the switch suggestion (`format_reset_clock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ResetStyle {
    Countdown,
    Clock,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ResetRequest {
    at: DateTime<Utc>,
    style: ResetStyle,
}

/// The panel's reset times, one text per request in order, so the panel uses
/// the same rule as the tray and the CLI instead of a copy in JavaScript.
#[tauri::command]
pub(crate) fn reset_texts(resets: Vec<ResetRequest>) -> Vec<String> {
    reset_texts_at(&resets, Utc::now(), &Local)
}

fn reset_texts_at<Tz: TimeZone>(resets: &[ResetRequest], now: DateTime<Utc>, tz: &Tz) -> Vec<String>
where
    Tz::Offset: std::fmt::Display,
{
    resets
        .iter()
        .map(|reset| match reset.style {
            ResetStyle::Countdown => format::format_reset_at(reset.at, now, tz),
            ResetStyle::Clock => format::format_reset_clock(reset.at, now, tz),
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

    /// The panel sends what `panel.js` puts into `data-resets-at` (the
    /// snapshot's RFC 3339 times) and gets the tray's and the CLI's texts back.
    #[test]
    fn reset_texts_use_the_shared_rules_in_order() {
        let resets: Vec<ResetRequest> = serde_json::from_value(serde_json::json!([
            { "at": "2026-11-11T14:39:59.591693Z", "style": "countdown" },
            { "at": "2026-11-11T14:39:59.591693Z", "style": "clock" },
            { "at": "2026-11-15T02:00:00Z", "style": "countdown" },
            { "at": "2026-11-15T02:00:00Z", "style": "clock" },
        ]))
        .expect("the panel's request");
        let now = DateTime::parse_from_rfc3339("2026-11-11T13:35:00Z")
            .expect("now")
            .with_timezone(&Utc);
        assert_eq!(
            reset_texts_at(&resets, now, &Utc),
            vec!["1h05m", "14:40", "Sun 02:00", "Sun 02:00"]
        );
    }

    #[test]
    fn a_reset_request_with_an_unknown_style_or_time_is_refused() {
        let unknown_style = serde_json::from_value::<ResetRequest>(
            serde_json::json!({ "at": "2026-11-11T14:40:00Z", "style": "weekday" }),
        );
        assert!(unknown_style.is_err());
        let bad_time = serde_json::from_value::<ResetRequest>(
            serde_json::json!({ "at": "tomorrow", "style": "clock" }),
        );
        assert!(bad_time.is_err());
    }

    #[test]
    fn wayland_centres_the_panel() {
        let (x, y) = place_panel((10.0, 10.0), monitor(), (380.0, 400.0), true);
        assert_eq!(x, (1920.0 - 380.0) / 2.0);
        assert_eq!(y, (1080.0 - 400.0) / 2.0);
    }
}
