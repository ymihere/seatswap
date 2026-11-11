//! Number and time formatting shared by the CLI, the tray and its menu.
//!
//! The CLI and the tray must show the same percentages, reset times and
//! tooltips, so there is exactly one copy of each format here.

use chrono::{DateTime, TimeZone, Utc};

use seatswap_core::api::{LimitKind, LoadLevel, Usage};
use seatswap_core::engine::AccountView;

/// A percentage without the `%` sign, rounded to a whole number.
pub(crate) fn integer_percent(value: f64) -> i64 {
    value.round() as i64
}

/// `97%`.
pub(crate) fn percent(value: f64) -> String {
    format!("{}%", integer_percent(value))
}

/// The age of a poll or a usage fetch: `12 s ago`, `5 min ago`, `2 h ago`,
/// `3 d ago` (spec §11 mock-up). A time a little in the future (the clocks of
/// this machine and the server differ) reads `0 s ago`.
/// The panel gets its texts from here too (`panel::time_texts`).
pub(crate) fn format_age(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - fetched_at).num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds} s ago");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h ago");
    }
    format!("{} d ago", hours / 24)
}

/// A reset time as shown: rounded up to the full minute. The server sends a
/// moment just before the boundary (`00:59:59.59`), which would read `00:59`
/// instead of `01:00`.
fn reset_minute(resets_at: DateTime<Utc>) -> DateTime<Utc> {
    let into_minute = resets_at.timestamp().rem_euclid(60) * 1_000_000_000
        + i64::from(resets_at.timestamp_subsec_nanos());
    if into_minute == 0 {
        resets_at
    } else {
        resets_at + chrono::Duration::nanoseconds(60_000_000_000 - into_minute)
    }
}

/// Time to a reset (spec §11), the one rule for the panel, tray and CLI:
/// under 24 h hours and minutes (`35m`, `1h05m`), up to 48 h days and hours
/// (`1d18h`), beyond 48 h the local weekday and clock time (`Sun 02:00`).
/// Minutes round up, so a reset 30 s away reads `1m`, not `0m`.
/// The panel gets its texts from here too (`panel::time_texts`).
pub(crate) fn format_reset_at<Tz: TimeZone>(
    resets_at: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let resets_at = reset_minute(resets_at);
    let seconds = resets_at.signed_duration_since(now).num_seconds();
    if seconds <= 0 {
        return "0m".to_string();
    }
    if seconds > 48 * 60 * 60 {
        return resets_at.with_timezone(tz).format("%a %H:%M").to_string();
    }
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 24 * 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d{}h", minutes / (24 * 60), minutes % (24 * 60) / 60)
    }
}

/// The tooltip's reset time is a local clock time as in the spec's example
/// (`resets 14:40`); above 24 h the weekday joins it.
pub(crate) fn format_reset_clock<Tz: TimeZone>(
    resets_at: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let resets_at = reset_minute(resets_at);
    let seconds = resets_at.signed_duration_since(now).num_seconds();
    if seconds > 24 * 60 * 60 {
        resets_at.with_timezone(tz).format("%a %H:%M").to_string()
    } else {
        resets_at.with_timezone(tz).format("%H:%M").to_string()
    }
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
#[cfg(feature = "gui")]
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
            "35m"
        );
        assert_eq!(
            format_reset_at(now + Duration::minutes(72), now, &tz),
            "1h12m"
        );
        assert_eq!(
            format_reset_at(now + Duration::minutes(65), now, &tz),
            "1h05m"
        );
        assert_eq!(
            format_reset_at(now + Duration::hours(23), now, &tz),
            "23h00m"
        );
        // Minutes round up: 30 s left is still a minute to wait.
        assert_eq!(format_reset_at(now + Duration::seconds(30), now, &tz), "1m");
        assert_eq!(
            format_reset_at(now + Duration::seconds(4 * 3600 + 59 * 60 + 30), now, &tz),
            "5h00m"
        );
        assert_eq!(format_reset_at(now, now, &tz), "0m");
        assert_eq!(format_reset_at(now - Duration::minutes(5), now, &tz), "0m");
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
        // 1h39m59.59s away is 1h40m.
        assert_eq!(
            format_reset_at(utc("2026-11-10T13:39:59.591674Z"), now, &tz),
            "1h40m"
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
        assert_eq!(format_reset_at(now + Duration::hours(24), now, &tz), "1d0h");
        assert_eq!(
            format_reset_at(now + Duration::hours(42) + Duration::minutes(50), now, &tz),
            "1d18h"
        );
        // Exactly 48 h is still days and hours; one second more is a weekday
        // (shown rounded up to the full minute).
        assert_eq!(format_reset_at(now + Duration::hours(48), now, &tz), "2d0h");
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
        assert_eq!(format_age(now - Duration::seconds(10), now), "10 s ago");
        assert_eq!(format_age(now + Duration::seconds(3), now), "0 s ago");
        assert_eq!(format_age(now - Duration::minutes(5), now), "5 min ago");
        assert_eq!(format_age(now - Duration::hours(2), now), "2 h ago");
        assert_eq!(format_age(now - Duration::days(3), now), "3 d ago");
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
            is_active: true,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            fetched_at: None,
            last_error: None,
        };
        let empty = Usage {
            limits: Vec::new(),
            extra_usage_enabled: false,
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
            is_active: true,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
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
}
