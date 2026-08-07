//! Due dates are typed inline as `[...]` at the end of a task description
//! and stored separately. Supported: `[yyyy-mm-dd hh:mm]`, `[yyyy-mm-dd]`,
//! `[mm-dd hh:mm]`, `[mm-dd]`, `[hh:mm]`.

use std::sync::OnceLock;

use chrono::{Datelike, Local, NaiveDate};
use regex::Regex;

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Month and day accept one or two digits everywhere, so a date
        // that parses on its own still parses once a time is appended.
        [
            r"\[(\d{4}-\d{1,2}-\d{1,2} \d{2}:\d{2})\]",
            r"\[(\d{4}-\d{1,2}-\d{1,2})\]",
            r"\[(\d{1,2}-\d{1,2} \d{2}:\d{2})\]",
            r"\[(\d{1,2}-\d{1,2})\]",
            r"\[(\d{2}:\d{2})\]",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid due-date pattern"))
        .collect()
    })
}

/// Split a description into `(due, remaining_text)`.
pub fn parse(description: &str) -> (String, String) {
    for re in patterns() {
        if let Some(m) = re.find(description) {
            let due = re
                .captures(description)
                .and_then(|c| c.get(1))
                .map(|g| g.as_str().to_string())
                .unwrap_or_default();
            let mut rest = String::with_capacity(description.len());
            rest.push_str(&description[..m.start()]);
            rest.push_str(&description[m.end()..]);
            return (due, rest.trim().to_string());
        }
    }
    (String::new(), description.trim().to_string())
}

/// Whether a bare date (no brackets) is one mach can store. An empty
/// string counts as valid: it just means "no due date".
pub fn is_valid(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let (due, rest) = parse(&format!("[{text}]"));
    !due.is_empty() && rest.is_empty() && names_a_real_moment(&due)
}

/// Whether a due string of the right shape also names a day that exists
/// and a time on the clock. The patterns only count digits, so without
/// this `2026-13-45 30:70` would be stored and then silently rewritten
/// (or dropped) by everything that reads it back.
fn names_a_real_moment(due: &str) -> bool {
    let Some((year, month, day, hour, minute)) = sort_key(due) else {
        return false;
    };
    hour < 24 && minute < 60 && NaiveDate::from_ymd_opt(year, month, day).is_some()
}

pub fn is_today(due: &str) -> bool {
    relative_day(due) == Some(0)
}

/// Days from today for `due`: `-1` yesterday, `0` today, `1` tomorrow.
fn relative_day(due: &str) -> Option<i64> {
    if due.is_empty() {
        return None;
    }
    // Bare hh:mm means today.
    if due.len() == 5 && due.contains(':') {
        return Some(0);
    }
    let (y, mo, d, _, _) = sort_key(due)?;
    let date = NaiveDate::from_ymd_opt(y, mo, d)?;
    Some((date - Local::now().date_naive()).num_days())
}

/// The bracketed string shown at the right edge of a task row.
/// Date order follows `date_format` (`Y-M-D` / `D-M-Y` / `M-D-Y`).
/// Nearby days use Today / Tomorrow / Yesterday.
pub fn display(due: &str, date_format: &str) -> String {
    if due.is_empty() {
        return String::new();
    }
    let Some((y, mo, d, h, mi)) = sort_key(due) else {
        return format!("[{due}]");
    };
    let label = match relative_day(due) {
        Some(0) => "Today".to_string(),
        Some(1) => "Tomorrow".to_string(),
        Some(-1) => "Yesterday".to_string(),
        _ => format_date(y, mo, d, date_format),
    };
    let text = if due.contains(':') {
        format!("{label} {h:02}:{mi:02}")
    } else {
        label
    };
    format!("[{text}]")
}

fn format_date(year: i32, month: u32, day: u32, date_format: &str) -> String {
    match date_format {
        "D-M-Y" => format!("{day:02}-{month:02}-{year}"),
        "M-D-Y" => format!("{month:02}-{day:02}-{year}"),
        _ => format!("{year}-{month:02}-{day:02}"),
    }
}

/// A comparable point in time for sorting. The stored forms differ in
/// shape — a bare time means today, a bare `mm-dd` means this year — so
/// comparing the strings themselves would put December before next
/// January. `None` is "no due date", which sorts last.
pub fn sort_key(due: &str) -> Option<(i32, u32, u32, u32, u32)> {
    if due.is_empty() {
        return None;
    }
    let today = Local::now().date_naive();
    let (date_part, time_part) = match due.split_once(' ') {
        Some((date, time)) => (date, Some(time)),
        None if due.contains(':') => ("", Some(due)),
        None => (due, None),
    };

    let (year, month, day) = match date_part.split('-').collect::<Vec<_>>()[..] {
        [y, m, d] => (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?),
        [m, d] => (today.year(), m.parse().ok()?, d.parse().ok()?),
        _ => (today.year(), today.month(), today.day()),
    };
    let (hour, minute) = match time_part.and_then(|t| t.split_once(':')) {
        Some((h, m)) => (h.parse().ok()?, m.parse().ok()?),
        None => (0, 0),
    };
    Some((year, month, day, hour, minute))
}

/// Current date and time for the status bar, in the configured order.
pub fn now_string(date_format: &str) -> String {
    let now = Local::now();
    let date = match date_format {
        "D-M-Y" => now.format("%d-%m-%Y"),
        "M-D-Y" => now.format("%m-%d-%Y"),
        _ => now.format("%Y-%m-%d"),
    };
    format!("{} {}", date, now.format("%H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn parses_every_supported_shape() {
        let cases = [
            (
                "buy milk [2026-08-06 09:00]",
                "2026-08-06 09:00",
                "buy milk",
            ),
            ("buy milk [2026-8-6]", "2026-8-6", "buy milk"),
            ("buy milk [08-06 09:00]", "08-06 09:00", "buy milk"),
            ("buy milk [8-6]", "8-6", "buy milk"),
            ("buy milk [09:00]", "09:00", "buy milk"),
        ];
        for (input, due, rest) in cases {
            assert_eq!(parse(input), (due.to_string(), rest.to_string()), "{input}");
        }
    }

    #[test]
    fn leaves_plain_descriptions_alone() {
        assert_eq!(
            parse("read [the] book"),
            (String::new(), "read [the] book".to_string())
        );
    }

    #[test]
    fn sorts_across_the_different_shapes() {
        let year = Local::now().year();
        assert_eq!(sort_key(""), None);
        assert_eq!(sort_key("2030-01-02 09:30"), Some((2030, 1, 2, 9, 30)));
        assert_eq!(sort_key("2030-01-02"), Some((2030, 1, 2, 0, 0)));
        assert_eq!(sort_key("12-25"), Some((year, 12, 25, 0, 0)));
        assert!(
            sort_key("12-25") > sort_key("01-02"),
            "December comes after January of the same year"
        );
        assert!(
            sort_key(&format!("{}-01-01", year + 1)) > sort_key("12-25"),
            "next year comes after this December"
        );
    }

    #[test]
    fn validates_bare_dates() {
        assert!(is_valid(""));
        assert!(is_valid("09:00"));
        assert!(is_valid("2030-01-02"));
        assert!(is_valid("01-02 09:00"));
        // A date and a time that are each valid stay valid once joined.
        assert!(is_valid("8-6"));
        assert!(is_valid("8-6 14:30"));
        assert!(is_valid("2026-8-6 14:30"));
        assert!(!is_valid("next tuesday"));
        assert!(!is_valid("2030/01/02"));
        assert!(!is_valid("09:00 and more"));
    }

    #[test]
    fn rejects_digits_that_are_not_a_real_date_or_time() {
        assert!(!is_valid("2026-99-99"), "month 99");
        assert!(!is_valid("2026-00-00"), "month and day zero");
        assert!(!is_valid("2026-02-30"), "February has no 30th");
        assert!(!is_valid("25:99"), "hour and minute out of range");
        assert!(!is_valid("2026-08-10 30:70"));
        assert!(is_valid("2028-02-29"), "a real leap day still passes");
    }

    #[test]
    fn today_is_labelled() {
        let today = Local::now().date_naive();
        let today_s = today.format("%Y-%m-%d").to_string();
        assert!(is_today(&today_s));
        assert_eq!(display(&today_s, "Y-M-D"), "[Today]");
        assert_eq!(
            display(&format!("{today_s} 07:30"), "Y-M-D"),
            "[Today 07:30]"
        );
        assert_eq!(display("09:00", "D-M-Y"), "[Today 09:00]");
        assert_eq!(display("2000-01-01", "Y-M-D"), "[2000-01-01]");
        assert_eq!(display("2000-01-01", "D-M-Y"), "[01-01-2000]");
        assert_eq!(display("2000-01-01 09:30", "M-D-Y"), "[01-01-2000 09:30]");
        assert_eq!(display("", "Y-M-D"), "");

        let tom = (today + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let yest = (today - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(display(&tom, "Y-M-D"), "[Tomorrow]");
        assert_eq!(
            display(&format!("{tom} 09:00"), "Y-M-D"),
            "[Tomorrow 09:00]"
        );
        assert_eq!(display(&yest, "Y-M-D"), "[Yesterday]");
        assert_eq!(
            display(&format!("{yest} 18:00"), "D-M-Y"),
            "[Yesterday 18:00]"
        );
    }
}
