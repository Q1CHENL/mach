//! Due dates may be typed inline as `[...]` at the end of a task description.
//! Input accepts `[yyyy-mm-dd hh:mm]`, `[yyyy-mm-dd]`, `[mm-dd hh:mm]`,
//! `[mm-dd]`, and `[hh:mm]`; persistence canonicalizes shorthand to an
//! absolute date and stores it separately from the title.

use std::sync::OnceLock;

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use regex::Regex;

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Month and day accept one or two digits everywhere, so a date
        // that parses on its own still parses once a time is appended.
        [
            r"\[(\d{4}-\d{1,2}-\d{1,2} \d{2}:\d{2})\]\s*$",
            r"\[(\d{4}-\d{1,2}-\d{1,2})\]\s*$",
            r"\[(\d{1,2}-\d{1,2} \d{2}:\d{2})\]\s*$",
            r"\[(\d{1,2}-\d{1,2})\]\s*$",
            r"\[(\d{2}:\d{2})\]\s*$",
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
    normalize_for_write(text).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDue {
    value: String,
}

impl std::fmt::Display for InvalidDue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid due {:?}; use YYYY-MM-DD, MM-DD, or HH:MM, optionally with a time",
            self.value
        )
    }
}

impl std::error::Error for InvalidDue {}

/// Canonicalize a due value at the moment it is persisted.
///
/// Fully qualified dates keep their year, including dates in the past.
/// Shorthand values name their next occurrence: a past `MM-DD` rolls to a
/// later year and a past `HH:MM` rolls to tomorrow. The stored result is
/// always an absolute `YYYY-MM-DD` date, optionally followed by `HH:MM`.
pub fn normalize_for_write(text: &str) -> Result<String, InvalidDue> {
    normalize_for_write_at(text, Local::now().naive_local())
}

pub fn normalize_for_write_at(text: &str, now: NaiveDateTime) -> Result<String, InvalidDue> {
    normalize_at(text, now, true)
}

/// Freeze legacy shorthand using the migration clock instead of treating it
/// as a newly entered future occurrence. This preserves what an existing
/// `MM-DD` / `HH:MM` value meant on the day SQLite first imports it.
pub fn normalize_legacy_at(text: &str, now: NaiveDateTime) -> Result<String, InvalidDue> {
    normalize_at(text, now, false)
}

fn normalize_at(
    text: &str,
    now: NaiveDateTime,
    roll_shorthand_forward: bool,
) -> Result<String, InvalidDue> {
    let original = text.trim();
    if original.is_empty() {
        return Ok(String::new());
    }
    let input = original.replace('T', " ");
    let (matched, rest) = parse(&format!("[{input}]"));
    if matched != input || !rest.is_empty() {
        return Err(InvalidDue {
            value: original.to_string(),
        });
    }

    let (date_part, time_part) = match input.split_once(' ') {
        Some((date, time)) if !date.is_empty() && !time.is_empty() => (date, Some(time)),
        None if input.contains(':') => ("", Some(input.as_str())),
        None => (input.as_str(), None),
        _ => {
            return Err(InvalidDue {
                value: original.to_string(),
            });
        }
    };
    let time = match time_part {
        Some(value) => Some(parse_time(value).ok_or_else(|| InvalidDue {
            value: original.to_string(),
        })?),
        None => None,
    };

    let pieces: Vec<&str> = date_part.split('-').collect();
    let date = match pieces.as_slice() {
        [year, month, day] => {
            let year = year.parse::<i32>().ok();
            let month = month.parse::<u32>().ok();
            let day = day.parse::<u32>().ok();
            match (year, month, day) {
                (Some(year), Some(month), Some(day)) => NaiveDate::from_ymd_opt(year, month, day),
                _ => None,
            }
            .ok_or_else(|| InvalidDue {
                value: original.to_string(),
            })?
        }
        [month, day] => {
            let month = month.parse::<u32>().ok();
            let day = day.parse::<u32>().ok();
            let (Some(month), Some(day)) = (month, day) else {
                return Err(InvalidDue {
                    value: original.to_string(),
                });
            };
            if roll_shorthand_forward {
                next_month_day(month, day, time, now)
            } else {
                month_day_in_migration_year(month, day, now)
            }
            .ok_or_else(|| InvalidDue {
                value: original.to_string(),
            })?
        }
        [""] => {
            let Some(clock) = time else {
                return Err(InvalidDue {
                    value: original.to_string(),
                });
            };
            let today = now.date();
            let candidate = today.and_time(clock);
            if !roll_shorthand_forward || candidate >= now {
                today
            } else {
                today.succ_opt().ok_or_else(|| InvalidDue {
                    value: original.to_string(),
                })?
            }
        }
        _ => {
            return Err(InvalidDue {
                value: original.to_string(),
            });
        }
    };

    Ok(match time {
        Some(time) => format!("{} {}", date.format("%Y-%m-%d"), time.format("%H:%M")),
        None => date.format("%Y-%m-%d").to_string(),
    })
}

fn month_day_in_migration_year(month: u32, day: u32, now: NaiveDateTime) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(now.year(), month, day)
}

fn parse_time(value: &str) -> Option<NaiveTime> {
    if value.len() != 5 || value.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let hour = value.get(..2)?.parse().ok()?;
    let minute = value.get(3..)?.parse().ok()?;
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn next_month_day(
    month: u32,
    day: u32,
    time: Option<NaiveTime>,
    now: NaiveDateTime,
) -> Option<NaiveDate> {
    // Eight years always crosses a leap year, including the Gregorian
    // century exception. The wider bound also keeps this correct near i32::MAX.
    for offset in 0..=8 {
        let year = now.year().checked_add(offset)?;
        let Some(candidate) = NaiveDate::from_ymd_opt(year, month, day) else {
            continue;
        };
        let is_future = match time {
            Some(clock) => candidate.and_time(clock) >= now,
            None => candidate >= now.date(),
        };
        if is_future {
            return Some(candidate);
        }
    }
    None
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

/// A comparable point in time for sorting. New writes are canonical absolute
/// values; the shorthand branches remain for displaying pre-migration callers
/// without turning malformed text into a real date. `None` is "no due date"
/// or an invalid value, both of which sort last.
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

    let date = match date_part.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, d] => NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?,
        [m, d] => NaiveDate::from_ymd_opt(today.year(), m.parse().ok()?, d.parse().ok()?)?,
        [""] if time_part.is_some() => today,
        _ => return None,
    };
    let time = match time_part {
        Some(value) => parse_time(value)?,
        None => NaiveTime::from_hms_opt(0, 0, 0)?,
    };
    Some((
        date.year(),
        date.month(),
        date.day(),
        time.hour(),
        time.minute(),
    ))
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
        assert_eq!(
            parse("plan [2030-01-02] launch"),
            (String::new(), "plan [2030-01-02] launch".to_string()),
            "only a trailing date token is due syntax"
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
    fn invalid_due_values_do_not_masquerade_as_today() {
        for value in ["garbage", "2030-99-99", "25:99"] {
            assert_eq!(sort_key(value), None, "{value}");
            assert_eq!(display(value, "Y-M-D"), format!("[{value}]"));
        }
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
    fn normalizes_shorthand_to_the_next_absolute_moment() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 8)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        assert_eq!(normalize_for_write_at("8-8", now).unwrap(), "2026-08-08");
        assert_eq!(normalize_for_write_at("8-7", now).unwrap(), "2027-08-07");
        assert_eq!(
            normalize_for_write_at("8-8 13:00", now).unwrap(),
            "2026-08-08 13:00"
        );
        assert_eq!(
            normalize_for_write_at("8-8 11:00", now).unwrap(),
            "2027-08-08 11:00"
        );
        assert_eq!(
            normalize_for_write_at("13:00", now).unwrap(),
            "2026-08-08 13:00"
        );
        assert_eq!(
            normalize_for_write_at("11:00", now).unwrap(),
            "2026-08-09 11:00"
        );
    }

    #[test]
    fn absolute_due_values_are_canonicalized_without_rolling_forward() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 8)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        assert_eq!(
            normalize_for_write_at("2020-1-2", now).unwrap(),
            "2020-01-02"
        );
        assert_eq!(
            normalize_for_write_at("2020-1-2 03:04", now).unwrap(),
            "2020-01-02 03:04"
        );
        assert!(normalize_for_write_at("2026-02-30", now).is_err());
    }

    #[test]
    fn legacy_shorthand_is_frozen_to_the_migration_day_and_year() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 8)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        assert_eq!(normalize_legacy_at("8-7", now).unwrap(), "2026-08-07");
        assert_eq!(
            normalize_legacy_at("11:00", now).unwrap(),
            "2026-08-08 11:00"
        );
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
