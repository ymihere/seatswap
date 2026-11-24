//! Number and time formatting shared by the CLI, the tray and its menu.
//!
//! The CLI and the tray must show the same percentages, reset times and
//! tooltips, so there is exactly one copy of each format here.

#[cfg(feature = "gui")]
use chrono::Datelike;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};

use seatswap_core::api::{LimitKind, LoadLevel, Usage};
use seatswap_core::duration::duration_text;
use seatswap_core::engine::AccountView;

/// A percentage without the `%` sign, rounded to a whole number.
pub(crate) fn integer_percent(value: f64) -> i64 {
    value.round() as i64
}

/// `97%`.
pub(crate) fn percent(value: f64) -> String {
    format!("{}%", integer_percent(value))
}

/// A login's end in the one duration spelling, for the CLI and the
/// notifications (the pop-up's common.js says it the same way): `login ends
/// today` on the last day, `login ends in 3d` before it, `login ended` after.
pub(crate) fn login_ends_text(days_left: i64) -> String {
    match days_left {
        days if days < 0 => "login ended".to_string(),
        0 => "login ends today".to_string(),
        days => format!(
            "login ends in {}",
            seatswap_core::duration::duration_text(days.unsigned_abs() * 86_400)
        ),
    }
}

/// The age of a poll or a usage fetch: `12s ago`, `5min ago`, `2h ago`,
/// `3d ago` (spec §11 mock-up). An age shows its largest unit only, so the
/// floored amount of that unit is passed to [`duration_text`]. A time a little
/// in the future (the clocks of this machine and the server differ) reads
/// `0s ago`.
/// The panel gets its texts from here too (`panel::time_texts`).
pub(crate) fn format_age(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - fetched_at).num_seconds().max(0);
    if seconds < 60 {
        return format!("{} ago", duration_text(seconds as u64));
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{} ago", duration_text((minutes * 60) as u64));
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{} ago", duration_text((hours * 3_600) as u64));
    }
    format!("{} ago", duration_text((hours / 24 * 86_400) as u64))
}

/// A path with the home folder at its start shown as `~`
/// (`/home/sam/projects/atlas` → `~/projects/atlas`). Only whole path components
/// match, by the platform's own separator rules: `/home/samuel` stays as it is
/// for home `/home/sam`, and on Unix `/home/sam\x` is a different folder.
/// A `Prefix` component compares ASCII case-insensitively, because a payload's
/// `cwd` can spell the drive letter in either case (`c:\Users\sam` for home
/// `C:\Users\sam`, specs/plan/README.md §5).
pub(crate) fn with_tilde(path: &str, home: &std::path::Path) -> String {
    let path = std::path::Path::new(path);
    match strip_prefix_components(path, home) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.to_string_lossy().into_owned(),
    }
}

/// The path a person typed, with a leading `~` for the home folder expanded
/// (`~/seatswap-history.csv` → `/home/sam/seatswap-history.csv`). Only a `~`
/// that starts the path names the home folder; a `~` later in the path is a
/// normal character.
pub(crate) fn expand_tilde(path: &str, home: &std::path::Path) -> std::path::PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    let prefix = format!("~{}", std::path::MAIN_SEPARATOR);
    let rest = path.strip_prefix(&prefix);
    // Windows also accepts the `/` a person types from the examples, beside
    // the `\` the History field shows.
    #[cfg(windows)]
    let rest = rest.or_else(|| path.strip_prefix("~/"));
    match rest {
        Some(rest) => home.join(rest),
        None => std::path::PathBuf::from(path),
    }
}

/// `path` without the whole `home` prefix, compared component by component.
/// No `cfg`: on Unix there is no `Prefix` component to meet, so the one rule
/// costs nothing there.
fn strip_prefix_components<'a>(
    path: &'a std::path::Path,
    home: &std::path::Path,
) -> Option<&'a std::path::Path> {
    let mut path_components = path.components();
    for home_component in home.components() {
        if !same_component(&path_components.next()?, &home_component) {
            return None;
        }
    }
    Some(path_components.as_path())
}

/// Whether two path components are the same: a `Prefix` component (only
/// Windows has one) compares ASCII case-insensitively, every other component
/// exactly.
fn same_component(a: &std::path::Component<'_>, b: &std::path::Component<'_>) -> bool {
    match (a, b) {
        (std::path::Component::Prefix(a), std::path::Component::Prefix(b)) => {
            a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
        }
        _ => a == b,
    }
}

/// Time to a reset (spec §11), the one rule for the panel, tray and CLI:
/// under 24 h hours and minutes (`35min`, `1h 5min`), up to 48 h days and
/// hours (`1d 18h`), beyond 48 h the local weekday and clock time
/// (`Sun 02:00`). Minutes round up, so a reset 30 s away reads `1min`, not
/// `0s`; it is [`compact_left`]'s rule, without the week's day/hour split.
/// The panel gets its texts from here too (`panel::time_texts`).
pub(crate) fn format_reset_at<Tz: TimeZone>(
    resets_at: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let resets_at = seatswap_core::history::reset_minute(resets_at);
    let seconds = resets_at.signed_duration_since(now).num_seconds();
    if seconds > 48 * 60 * 60 {
        return resets_at.with_timezone(tz).format("%a %H:%M").to_string();
    }
    compact_left(seconds, false)
}

/// The tooltip's reset time is a local clock time as in the spec's example
/// (`resets 14:40`); above 24 h the weekday joins it. This follows the
/// tooltip's own rule (spec §8.1), not §7.4.5's today/not-today rule:
/// [`clock`] follows §7.4.5 and keeps its two forms apart.
pub(crate) fn format_reset_clock<Tz: TimeZone>(
    resets_at: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let resets_at = seatswap_core::history::reset_minute(resets_at);
    let seconds = resets_at.signed_duration_since(now).num_seconds();
    if seconds > 24 * 60 * 60 {
        resets_at.with_timezone(tz).format("%a %H:%M").to_string()
    } else {
        resets_at.with_timezone(tz).format("%H:%M").to_string()
    }
}

/// A local time in auto-burn's words (spec §7.4.5): `HH:MM` when it is today,
/// `Mon HH:MM` otherwise. [`weekday_clock`] is the always-weekday form.
pub(crate) fn clock(at: DateTime<Utc>, now: DateTime<Utc>, offset: FixedOffset) -> String {
    let local = at.with_timezone(&offset);
    if local.date_naive() == now.with_timezone(&offset).date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %H:%M").to_string()
    }
}

/// The local weekday and clock (spec §7.4.5), as in "resets on Mon 02:00".
pub(crate) fn weekday_clock(at: DateTime<Utc>, offset: FixedOffset) -> String {
    at.with_timezone(&offset).format("%a %H:%M").to_string()
}

/// A locally read day and month, as in `13 Nov`.
#[cfg(feature = "gui")]
pub(crate) fn day<Tz: TimeZone>(at: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = at.with_timezone(tz);
    format!("{} {}", local.day(), local.format("%b"))
}

/// A calendar date as written, never moved by the time zone of the machine:
/// `13 Nov`.
#[cfg(feature = "gui")]
pub(crate) fn date(at: DateTime<Utc>) -> String {
    let date = at.date_naive();
    format!("{} {}", date.day(), date.format("%b"))
}

/// A local call time: `09:41` today, `Thu 09:41` within the last six days,
/// and `13 Nov 09:41` older.
#[cfg(feature = "gui")]
pub(crate) fn day_clock<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = at.with_timezone(tz);
    let today = now.with_timezone(tz).date_naive();
    let days = (today - local.date_naive()).num_days();
    if days == 0 {
        local.format("%H:%M").to_string()
    } else if (1..=6).contains(&days) {
        local.format("%a %H:%M").to_string()
    } else {
        format!(
            "{} {} {}",
            local.day(),
            local.format("%b"),
            local.format("%H:%M")
        )
    }
}

/// The local calendar day and year, as in `17 Aug 2026`.
#[cfg(feature = "gui")]
pub(crate) fn day_year<Tz: TimeZone>(at: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = at.with_timezone(tz);
    format!(
        "{} {} {}",
        local.day(),
        local.format("%b"),
        local.format("%Y")
    )
}

/// The compact duration of `seconds`: `35min`, `2h`, `2h 5min`, `3d 16h`
/// (spec §7.4.5), the status line's compact time left too (spec §5.1). The
/// larger unit only when it is not zero; minutes round up, so 30 s reads
/// `1min`; a time that has passed reads `0s`. `week` makes the larger unit a
/// day and the smaller an hour, rounded up to the whole hour; otherwise they
/// are an hour and a minute.
pub(crate) fn compact_left(seconds: i64, week: bool) -> String {
    let seconds = seconds.max(0);
    if week {
        let hours = (seconds + 3_599) / 3_600;
        duration_text((hours * 3_600) as u64)
    } else {
        let minutes = (seconds + 59) / 60;
        duration_text((minutes * 60) as u64)
    }
}

/// The time left until `at` as the pop-up's card header and the auto-burn line
/// write it: [`compact_left`] in days and hours from a day on (`3d 2h`), else
/// in hours and minutes (`14h 3min`).
#[cfg(feature = "gui")]
pub(crate) fn time_left(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (at - now).num_seconds();
    compact_left(seconds, seconds >= 24 * 60 * 60)
}

/// The waybar/i3blocks class name of a load level (§7.3).
pub(crate) fn class_name(level: Option<LoadLevel>) -> &'static str {
    match level {
        Some(LoadLevel::Fine) => "fine",
        Some(LoadLevel::Close) => "close",
        Some(LoadLevel::Critical) => "critical",
        Some(LoadLevel::Locked) => "locked",
        None => "none",
    }
}

/// What to call a limit in the tooltip: `session` or `week`.
pub(crate) fn kind_label(kind: &LimitKind) -> &str {
    match kind {
        LimitKind::Session => "session",
        LimitKind::WeeklyAll | LimitKind::WeeklyScoped => "week",
        LimitKind::Other(other) => other,
    }
}

/// The tray tooltip of the live account (spec §8.1):
/// `AC · Acme — session 97 % (resets 14:40) · week 48 %`.
pub(crate) fn tooltip(view: &AccountView, usage: &Usage, now: DateTime<Utc>) -> String {
    let mut details = Vec::new();
    if let Some(session) = usage.session() {
        let mut part = format!("session {} %", integer_percent(session.percent));
        if let Some(resets_at) = session.resets_at {
            part.push_str(&format!(
                " (resets {})",
                format_reset_clock(resets_at, now, &chrono::Local)
            ));
        }
        details.push(part);
    }
    if let Some(week) = usage.weekly_all() {
        details.push(format!("week {} %", integer_percent(week.percent)));
    }
    if details.is_empty() {
        match usage.binding() {
            Some(binding) => details.push(format!(
                "{} {} %",
                kind_label(&binding.kind),
                integer_percent(binding.percent)
            )),
            // An empty reply must not end the line with a dangling separator.
            None => return format!("{} · {} — no usage yet", view.abbr, view.label),
        }
    }
    format!("{} · {} — {}", view.abbr, view.label, details.join(" · "))
}

/// `#rrggbb` (either case) → its three channels.
pub(crate) fn parse_color(color: &str) -> Option<[u8; 3]> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([red, green, blue])
}

/// The dark letter colour (`#0f1115`): the one definition, used by the tray
/// icon and the status line's account segment.
pub(crate) const LETTER_DARK: [u8; 3] = [0x0f, 0x11, 0x15];

/// The colour a letter on `background` takes: white or [`LETTER_DARK`],
/// whichever has the higher WCAG contrast ratio. The tray icon and the status
/// line's account segment share this one rule.
pub(crate) fn letter_color(background: [u8; 3]) -> [u8; 3] {
    const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
    if contrast_ratio(background, WHITE) >= contrast_ratio(background, LETTER_DARK) {
        WHITE
    } else {
        LETTER_DARK
    }
}

/// WCAG 2.x relative luminance.
fn relative_luminance(color: [u8; 3]) -> f32 {
    fn linear(channel: u8) -> f32 {
        let value = channel as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{Duration, FixedOffset};

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse::<DateTime<Utc>>().expect("test date")
    }

    #[test]
    fn reset_times_are_relative_within_a_day() {
        let now = utc("2026-11-10T12:00:00Z");
        let tz = FixedOffset::east_opt(2 * 60 * 60).expect("offset");

        assert_eq!(
            format_reset_at(now + Duration::minutes(35), now, &tz),
            "35min"
        );
        assert_eq!(
            format_reset_at(now + Duration::minutes(72), now, &tz),
            "1h 12min"
        );
        assert_eq!(
            format_reset_at(now + Duration::minutes(65), now, &tz),
            "1h 5min"
        );
        assert_eq!(format_reset_at(now + Duration::hours(23), now, &tz), "23h");
        // Minutes round up: 30 s left is still a minute to wait.
        assert_eq!(
            format_reset_at(now + Duration::seconds(30), now, &tz),
            "1min"
        );
        assert_eq!(
            format_reset_at(now + Duration::seconds(4 * 3600 + 59 * 60 + 30), now, &tz),
            "5h"
        );
        assert_eq!(format_reset_at(now, now, &tz), "0s");
        assert_eq!(format_reset_at(now - Duration::minutes(5), now, &tz), "0s");
    }

    /// The server's reset times sit just before the boundary (as seen in a real
    /// usage reply, 2026-11-10): they show as the full minute or hour.
    #[test]
    fn reset_times_round_up_to_the_full_minute() {
        let now = utc("2026-11-10T12:00:00Z");
        let tz = FixedOffset::east_opt(60 * 60).expect("offset");
        assert_eq!(
            format_reset_at(utc("2026-11-16T00:59:59.591693Z"), now, &tz),
            "Mon 02:00"
        );
        assert_eq!(
            format_reset_clock(utc("2026-11-14T17:59:59.704120Z"), now, &tz),
            "Sat 19:00"
        );
        assert_eq!(
            format_reset_clock(utc("2026-11-10T13:39:59.591674Z"), now, &tz),
            "14:40"
        );
        // 1h39m59.59s away is 1h 40min.
        assert_eq!(
            format_reset_at(utc("2026-11-10T13:39:59.591674Z"), now, &tz),
            "1h 40min"
        );
        // An exact minute stays as it is.
        assert_eq!(
            format_reset_clock(utc("2026-11-10T13:40:00Z"), now, &tz),
            "14:40"
        );
    }

    #[test]
    fn reset_times_between_one_and_two_days_use_days_and_hours() {
        let now = utc("2026-11-10T12:00:00Z");
        let tz = FixedOffset::east_opt(2 * 60 * 60).expect("offset");
        assert_eq!(format_reset_at(now + Duration::hours(24), now, &tz), "1d");
        assert_eq!(
            format_reset_at(now + Duration::hours(42) + Duration::minutes(50), now, &tz),
            "1d 18h"
        );
        // Exactly 48 h is still days and hours; one second more is a weekday
        // (shown rounded up to the full minute).
        assert_eq!(format_reset_at(now + Duration::hours(48), now, &tz), "2d");
        assert_eq!(
            format_reset_at(now + Duration::hours(48) + Duration::seconds(1), now, &tz),
            "Thu 14:01"
        );
    }

    #[test]
    fn reset_times_over_two_days_use_the_local_weekday_and_clock() {
        let now = utc("2026-11-10T12:00:00Z");
        let tz = FixedOffset::east_opt(2 * 60 * 60).expect("offset");
        // Sunday 00:00 UTC is Sunday 02:00 at +02:00.
        let resets_at = utc("2026-11-15T00:00:00Z");
        assert_eq!(format_reset_at(resets_at, now, &tz), "Sun 02:00");

        // The tooltip form keeps the clock time below 24 h and adds the day above it.
        assert_eq!(
            format_reset_clock(utc("2026-11-10T13:40:00Z"), now, &tz),
            "15:40"
        );
        assert_eq!(format_reset_clock(resets_at, now, &tz), "Sun 02:00");
    }

    #[test]
    fn load_levels_map_to_the_waybar_class_names() {
        assert_eq!(class_name(None), "none");
        assert_eq!(class_name(Some(LoadLevel::Fine)), "fine");
        assert_eq!(class_name(Some(LoadLevel::Close)), "close");
        assert_eq!(class_name(Some(LoadLevel::Critical)), "critical");
        assert_eq!(class_name(Some(LoadLevel::Locked)), "locked");
    }

    #[test]
    fn percents_are_integers() {
        assert_eq!(percent(97.0), "97%");
        assert_eq!(percent(96.6), "97%");
        assert_eq!(percent(0.0), "0%");
    }

    #[test]
    fn ages_are_human_readable() {
        let now = utc("2026-11-10T12:00:00Z");
        assert_eq!(format_age(now - Duration::seconds(10), now), "10s ago");
        assert_eq!(format_age(now + Duration::seconds(3), now), "0s ago");
        assert_eq!(format_age(now - Duration::minutes(5), now), "5min ago");
        assert_eq!(format_age(now - Duration::hours(2), now), "2h ago");
        assert_eq!(format_age(now - Duration::days(3), now), "3d ago");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_day_year_is_the_local_calendar_day() {
        let at = utc("2026-12-31T23:30:00Z");
        let plus_one = FixedOffset::east_opt(60 * 60).expect("offset");
        assert_eq!(day_year(at, &plus_one), "1 Jan 2027");
        assert_eq!(day_year(at, &Utc), "31 Dec 2026");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_home_folder_becomes_a_tilde() {
        let home = crate::test_support::home();
        assert_eq!(
            with_tilde(
                &crate::test_support::at_home("projects/atlas")
                    .display()
                    .to_string(),
                home
            ),
            crate::test_support::tilde("projects/atlas")
        );
        assert_eq!(with_tilde(&home.display().to_string(), home), "~");
        assert_eq!(with_tilde("/home/samuel", home), "/home/samuel");
        assert_eq!(
            with_tilde("/home/samuel/projects", home),
            "/home/samuel/projects"
        );
        assert_eq!(with_tilde("/srv/atlas", home), "/srv/atlas");
        // A backslash is a separator only where the platform says it is.
        #[cfg(unix)]
        assert_eq!(with_tilde("/home/sam\\x", home), "/home/sam\\x");
        #[cfg(windows)]
        assert_eq!(
            with_tilde(
                &crate::test_support::at_home("projects")
                    .display()
                    .to_string(),
                home
            ),
            crate::test_support::tilde("projects")
        );
    }

    #[test]
    fn a_tilde_at_the_start_of_a_path_becomes_the_home_folder() {
        let home = std::path::Path::new("/home/sam");
        assert_eq!(
            expand_tilde("~/seatswap-history.csv", home),
            std::path::PathBuf::from("/home/sam/seatswap-history.csv")
        );
        assert_eq!(
            expand_tilde("~", home),
            std::path::PathBuf::from("/home/sam")
        );
        assert_eq!(
            expand_tilde("/home/sam/x", home),
            std::path::PathBuf::from("/home/sam/x")
        );
        // Only a `~` that starts the path names the home folder.
        assert_eq!(
            expand_tilde("/tmp/~/x", home),
            std::path::PathBuf::from("/tmp/~/x")
        );
        assert_eq!(
            expand_tilde("~/Projects/atlas", home),
            std::path::PathBuf::from("/home/sam/Projects/atlas")
        );
    }

    #[test]
    fn a_tooltip_without_limits_says_no_usage_yet() {
        use seatswap_core::store::LoginState;

        let view = AccountView {
            id: "id".to_string(),
            label: "Acme".to_string(),
            abbr: "AC".to_string(),
            color: "#3b82f6".to_string(),
            email: Some("someone@example.com".to_string()),
            plan: None,
            auto_burn: true,
            is_active: true,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: None,
            last_error: None,
        };
        let empty = Usage {
            limits: Vec::new(),
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            tooltip(&view, &empty, utc("2026-11-10T12:00:00Z")),
            "AC · Acme — no usage yet"
        );
    }

    #[test]
    fn a_tooltip_names_an_other_limit() {
        use seatswap_core::api::{Limit, LimitKind, Severity};
        use seatswap_core::store::LoginState;

        let view = AccountView {
            id: "id".to_string(),
            label: "Acme".to_string(),
            abbr: "AC".to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            auto_burn: true,
            is_active: true,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: None,
            last_error: None,
        };
        let usage = Usage {
            limits: vec![Limit {
                kind: LimitKind::Other("daily_tokens".to_string()),
                percent: 42.0,
                severity: Severity::Normal,
                resets_at: None,
                model: None,
                is_active: true,
            }],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            tooltip(&view, &usage, utc("2026-11-10T12:00:00Z")),
            "AC · Acme — daily_tokens 42 %"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn colors_are_parsed_only_when_they_are_rrggbb() {
        assert_eq!(parse_color("#3b82f6"), Some([0x3b, 0x82, 0xf6]));
        assert_eq!(parse_color("#ABCDEF"), Some([0xab, 0xcd, 0xef]));
        assert_eq!(parse_color("3b82f6"), None);
        assert_eq!(parse_color("#3b82f"), None);
        assert_eq!(parse_color("#3b82fg"), None);
    }

    #[test]
    fn a_home_prefix_is_compared_component_by_component() {
        use std::ffi::OsStr;
        use std::path::{Component, Path, PathBuf};

        assert!(same_component(
            &Component::Normal(OsStr::new("atlas")),
            &Component::Normal(OsStr::new("atlas"))
        ));
        assert!(!same_component(
            &Component::Normal(OsStr::new("atlas")),
            &Component::Normal(OsStr::new("Atlas"))
        ));
        assert!(same_component(&Component::RootDir, &Component::RootDir));
        assert!(!same_component(&Component::RootDir, &Component::CurDir));

        let home = Path::new("/home/sam");
        assert_eq!(
            strip_prefix_components(Path::new("/home/sam/projects/atlas"), home)
                .map(Path::to_path_buf),
            Some(PathBuf::from("projects/atlas"))
        );
        assert_eq!(
            strip_prefix_components(Path::new("/home/samuel"), home),
            None
        );
        // A `Prefix` component (a Windows drive letter) cannot be built on
        // Linux: `PrefixComponent` has no public constructor there. The
        // `#[cfg(windows)]` test below runs that branch.
    }

    #[cfg(windows)]
    #[test]
    fn a_lower_case_drive_letter_still_becomes_a_tilde() {
        assert_eq!(
            with_tilde(
                r"c:\Users\sam\projects\atlas",
                std::path::Path::new(r"C:\Users\sam")
            ),
            r"~\projects\atlas"
        );
    }

    #[test]
    fn the_dark_letter_colour_is_one_constant() {
        assert_eq!(LETTER_DARK, [0x0f, 0x11, 0x15]);
        assert_eq!(letter_color([0xfd, 0xe0, 0x47]), LETTER_DARK);
    }

    #[test]
    fn a_letter_is_white_on_dark_and_dark_on_light() {
        // The same two colours `icon.rs` tests its letters with.
        assert_eq!(letter_color([0x1e, 0x3a, 0x8a]), [0xff, 0xff, 0xff]);
        assert_eq!(letter_color([0xfd, 0xe0, 0x47]), [0x0f, 0x11, 0x15]);
    }

    /// The login's end in the one duration spelling (review item 42); 0 days is
    /// today, as the pop-up says it.
    #[test]
    fn the_login_end_is_said_in_days() {
        assert_eq!(login_ends_text(3), "login ends in 3d");
        assert_eq!(login_ends_text(1), "login ends in 1d");
        assert_eq!(login_ends_text(0), "login ends today");
        assert_eq!(login_ends_text(-1), "login ended");
    }
}
