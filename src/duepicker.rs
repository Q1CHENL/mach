//! Date-and-time picker for the Due field. Both the day and the clock
//! are set from this UI — the field itself is not free-typed.

use chrono::{Datelike, Local, NaiveDate, Timelike};
use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Calendar,
    Hour,
    Minute,
}

impl PickerFocus {
    pub fn next(self) -> Self {
        match self {
            Self::Calendar => Self::Hour,
            Self::Hour => Self::Minute,
            Self::Minute => Self::Calendar,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Calendar => Self::Minute,
            Self::Hour => Self::Calendar,
            Self::Minute => Self::Hour,
        }
    }
}

/// Hit targets filled in while drawing, so mouse events can find a day
/// or the hour/minute cells. Matches ratatui's Monthly layout: each day
/// is three columns (` ` + two-digit day), weeks start on Sunday.
#[derive(Debug, Default, Clone, Copy)]
pub struct PickerLayout {
    pub frame: Rect,
    /// The day grid only (under the month + weekday headers).
    pub days: Rect,
    pub hour: Rect,
    pub minute: Rect,
    pub time_row: Rect,
}

pub struct DuePicker {
    /// The day under the cursor.
    pub day: NaiveDate,
    pub hour: u8,
    pub minute: u8,
    pub focus: PickerFocus,
    /// Digits typed into the focused hour/minute field (0–2).
    entry: Vec<u8>,
    original: String,
    had_date: bool,
    had_year: bool,
    had_time: bool,
    date_changed: bool,
    time_changed: bool,
    /// Last drawn positions for hit-testing.
    pub layout: PickerLayout,
}

impl DuePicker {
    /// Opens on the date/time already in the field, or on now.
    pub fn new(current: &str) -> Self {
        let current = current.trim();
        let now = Local::now();
        let day = parse_day(current).unwrap_or_else(|| now.date_naive());
        let (hour, minute) = match crate::due::sort_key(current) {
            // Only trust the clock when the stored value actually has one.
            Some((_, _, _, h, m)) if current.contains(':') => (h as u8 % 24, m as u8 % 60),
            _ => (now.hour() as u8, now.minute() as u8),
        };
        Self {
            day,
            hour,
            minute,
            focus: PickerFocus::Calendar,
            entry: Vec::new(),
            original: current.to_string(),
            had_date: current.contains('-'),
            had_year: current
                .split_whitespace()
                .next()
                .is_some_and(|date| date.split('-').next().is_some_and(|year| year.len() == 4)),
            had_time: current.contains(':'),
            date_changed: false,
            time_changed: false,
            layout: PickerLayout::default(),
        }
    }

    /// True when `(x, y)` is inside the picker frame.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        let r = self.layout.frame;
        !r.is_empty() && r.contains(ratatui::layout::Position { x, y })
    }

    /// Apply a left-click. Returns true when the click was handled.
    pub fn click(&mut self, x: u16, y: u16) -> bool {
        let layout = self.layout;
        if hit(layout.hour, x, y) {
            self.set_focus(PickerFocus::Hour);
            return true;
        }
        if hit(layout.minute, x, y) {
            self.set_focus(PickerFocus::Minute);
            return true;
        }
        if let Some(day) = date_at_click(self.day, layout.days, x, y) {
            self.set_day(day);
            self.set_focus(PickerFocus::Calendar);
            return true;
        }
        // Click elsewhere inside the frame just focuses the calendar.
        if self.contains(x, y) {
            self.set_focus(PickerFocus::Calendar);
            return true;
        }
        false
    }

    /// Scroll over the picker: calendar months, or hour/minute steps.
    pub fn scroll(&mut self, x: u16, y: u16, up: bool) -> bool {
        let layout = self.layout;
        let delta = if up { 1 } else { -1 };
        if hit(layout.hour, x, y) || (hit(layout.time_row, x, y) && self.focus == PickerFocus::Hour)
        {
            self.set_focus(PickerFocus::Hour);
            self.bump_hour(delta);
            return true;
        }
        if hit(layout.minute, x, y)
            || (hit(layout.time_row, x, y) && self.focus == PickerFocus::Minute)
        {
            self.set_focus(PickerFocus::Minute);
            self.bump_minute(delta * 5);
            return true;
        }
        if self.contains(x, y) {
            self.set_focus(PickerFocus::Calendar);
            self.move_months(delta);
            return true;
        }
        false
    }

    /// Preserve the representation the user opened. A missing date or clock
    /// is only added when the user actually changes that component.
    pub fn value(&self) -> String {
        if !self.date_changed && !self.time_changed {
            if !self.original.is_empty() {
                return self.original.clone();
            }
            return format!(
                "{} {:02}:{:02}",
                self.day.format("%Y-%m-%d"),
                self.hour,
                self.minute
            );
        }
        let include_date = self.had_date || self.date_changed;
        let include_time = self.had_time || self.time_changed;
        match (include_date, include_time) {
            (true, true) => format!(
                "{} {:02}:{:02}",
                self.day.format("%Y-%m-%d"),
                self.hour,
                self.minute
            ),
            (true, false) => {
                if self.had_date && !self.had_year {
                    self.day.format("%m-%d").to_string()
                } else {
                    self.day.format("%Y-%m-%d").to_string()
                }
            }
            (false, true) => format!("{:02}:{:02}", self.hour, self.minute),
            (false, false) => String::new(),
        }
    }

    pub fn focus_next(&mut self) {
        self.set_focus(self.focus.next());
    }

    pub fn focus_prev(&mut self) {
        self.set_focus(self.focus.prev());
    }

    fn set_focus(&mut self, focus: PickerFocus) {
        if focus != self.focus {
            self.entry.clear();
        }
        self.focus = focus;
    }

    pub fn move_days(&mut self, days: i64) {
        self.entry.clear();
        if let Some(day) = self.day.checked_add_signed(chrono::Duration::days(days)) {
            self.set_day(day);
        }
    }

    pub fn move_months(&mut self, months: i32) {
        self.entry.clear();
        let (mut year, mut month) = (self.day.year(), self.day.month() as i32 + months);
        while month < 1 {
            month += 12;
            year -= 1;
        }
        while month > 12 {
            month -= 12;
            year += 1;
        }
        // Clamp onto the last day of a shorter month.
        let day = self.day.day().min(days_in_month(year, month as u32));
        if let Some(date) = NaiveDate::from_ymd_opt(year, month as u32, day) {
            self.set_day(date);
        }
    }

    /// Jump the day to today; leave the clock as it is.
    pub fn today(&mut self) {
        self.set_day(Local::now().date_naive());
    }

    /// Set the clock to the current time of day.
    pub fn now_time(&mut self) {
        self.entry.clear();
        let now = Local::now();
        self.set_time(now.hour() as u8, now.minute() as u8);
    }

    pub fn bump_hour(&mut self, delta: i32) {
        self.entry.clear();
        let h = (self.hour as i32 + delta).rem_euclid(24) as u8;
        self.set_time(h, self.minute);
    }

    pub fn bump_minute(&mut self, delta: i32) {
        self.entry.clear();
        let total = self.hour as i32 * 60 + self.minute as i32 + delta;
        let total = total.rem_euclid(24 * 60);
        self.set_time((total / 60) as u8, (total % 60) as u8);
    }

    /// Type a digit into the clock. On the calendar, jumps to hour first.
    /// Hour: 0–23 (auto-advances to minute after a complete value).
    /// Minute: 0–59.
    pub fn type_digit(&mut self, d: u8) {
        if d > 9 {
            return;
        }
        if self.focus == PickerFocus::Calendar {
            self.set_focus(PickerFocus::Hour);
        }
        match self.focus {
            PickerFocus::Hour => self.push_hour_digit(d),
            PickerFocus::Minute => self.push_minute_digit(d),
            PickerFocus::Calendar => {}
        }
    }

    fn push_hour_digit(&mut self, d: u8) {
        if self.entry.is_empty() {
            self.entry.push(d);
            self.set_time(d, self.minute);
            // 3–9 can only be single-digit hours → commit and move on.
            if d >= 3 {
                self.entry.clear();
                self.set_focus(PickerFocus::Minute);
            }
            return;
        }
        let h = self.entry[0] * 10 + d;
        self.set_time(if h < 24 { h } else { d }, self.minute);
        self.entry.clear();
        self.set_focus(PickerFocus::Minute);
    }

    fn push_minute_digit(&mut self, d: u8) {
        if self.entry.is_empty() {
            self.entry.push(d);
            self.set_time(self.hour, d);
            // 6–9 can only be single-digit minutes.
            if d >= 6 {
                self.entry.clear();
            }
            return;
        }
        let m = self.entry[0] * 10 + d;
        self.set_time(self.hour, if m < 60 { m } else { d });
        self.entry.clear();
    }

    fn set_day(&mut self, day: NaiveDate) {
        if day != self.day {
            self.day = day;
            self.date_changed = true;
        }
    }

    fn set_time(&mut self, hour: u8, minute: u8) {
        if (hour, minute) != (self.hour, self.minute) {
            self.hour = hour;
            self.minute = minute;
            self.time_changed = true;
        }
    }
}

fn hit(r: Rect, x: u16, y: u16) -> bool {
    !r.is_empty() && r.contains(ratatui::layout::Position { x, y })
}

/// Map a click on the Monthly day grid to a calendar date.
///
/// Layout (per ratatui-widgets calendar): each of 7 days is three cells
/// wide (` ` + `dd`), rows are Sunday-based weeks starting from the
/// Sunday on or before the 1st of the displayed month.
fn date_at_click(anchor: NaiveDate, days: Rect, x: u16, y: u16) -> Option<NaiveDate> {
    if !hit(days, x, y) {
        return None;
    }
    let week = (y - days.y) as i64;
    let col = ((x - days.x) / 3) as i64;
    if !(0..=6).contains(&col) {
        return None;
    }
    let first = anchor.with_day(1)?;
    let offset = first.weekday().num_days_from_sunday() as i64;
    let start = first.checked_sub_signed(chrono::Duration::days(offset))?;
    start.checked_add_signed(chrono::Duration::days(week * 7 + col))
}

/// Reads back the shapes mach stores, so reopening the picker lands
/// on the date that is already set.
fn parse_day(due: &str) -> Option<NaiveDate> {
    let (year, month, day, _, _) = crate::due::sort_key(due)?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|first| first.pred_opt())
        .map(|last| last.day())
        .unwrap_or(28)
}

/// The picker's day as the `time` crate spells it, which is what
/// ratatui's calendar widget takes.
pub fn to_time_date(day: NaiveDate) -> Option<time::Date> {
    let month = time::Month::try_from(day.month() as u8).ok()?;
    time::Date::from_calendar_date(day.year(), month, day.day() as u8).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_on_the_date_already_set() {
        let picker = DuePicker::new("2030-01-02");
        assert_eq!(picker.day, NaiveDate::from_ymd_opt(2030, 1, 2).unwrap());
        assert_eq!(picker.value(), "2030-01-02");
    }

    #[test]
    fn keeps_the_time_that_was_there() {
        let picker = DuePicker::new("2030-01-02 09:30");
        assert_eq!(picker.hour, 9);
        assert_eq!(picker.minute, 30);
        assert_eq!(picker.value(), "2030-01-02 09:30");
    }

    #[test]
    fn a_bare_time_means_today_at_that_time() {
        let picker = DuePicker::new("09:30");
        assert_eq!(picker.day, Local::now().date_naive());
        assert_eq!(picker.hour, 9);
        assert_eq!(picker.minute, 30);
        assert_eq!(picker.value(), "09:30");
    }

    #[test]
    fn changing_only_an_existing_component_preserves_due_precision() {
        let mut date = DuePicker::new("2030-01-02");
        date.move_days(1);
        assert_eq!(date.value(), "2030-01-03");

        let mut time = DuePicker::new("09:30");
        time.bump_minute(5);
        assert_eq!(time.value(), "09:35");

        let mut yearless = DuePicker::new("08-09");
        yearless.move_days(1);
        assert_eq!(yearless.value(), "08-10");
    }

    #[test]
    fn changing_a_missing_component_intentionally_adds_it() {
        let mut date = DuePicker::new("2030-01-02");
        date.bump_hour(1);
        assert!(date.value().starts_with("2030-01-02 "));

        let mut time = DuePicker::new("09:30");
        time.move_days(1);
        assert!(time.value().ends_with(" 09:30"));
    }

    #[test]
    fn walks_days_and_months() {
        let mut picker = DuePicker::new("2030-01-31");
        picker.move_days(1);
        assert_eq!(picker.day, NaiveDate::from_ymd_opt(2030, 2, 1).unwrap());
        picker.move_months(-1);
        assert_eq!(picker.day, NaiveDate::from_ymd_opt(2030, 1, 1).unwrap());
    }

    #[test]
    fn clamps_onto_a_shorter_month() {
        let mut picker = DuePicker::new("2030-01-31");
        picker.move_months(1);
        assert_eq!(
            picker.day,
            NaiveDate::from_ymd_opt(2030, 2, 28).unwrap(),
            "January 31st has no February counterpart"
        );
    }

    #[test]
    fn bumps_hour_and_minute_wrapping() {
        let mut picker = DuePicker::new("2030-01-02 23:55");
        picker.bump_hour(1);
        assert_eq!((picker.hour, picker.minute), (0, 55));
        picker.bump_minute(10);
        assert_eq!((picker.hour, picker.minute), (1, 5));
        picker.bump_minute(-10);
        assert_eq!((picker.hour, picker.minute), (0, 55));
    }

    #[test]
    fn tab_walks_focus() {
        let mut picker = DuePicker::new("");
        assert_eq!(picker.focus, PickerFocus::Calendar);
        picker.focus_next();
        assert_eq!(picker.focus, PickerFocus::Hour);
        picker.focus_next();
        assert_eq!(picker.focus, PickerFocus::Minute);
        picker.focus_next();
        assert_eq!(picker.focus, PickerFocus::Calendar);
    }

    #[test]
    fn click_picks_a_day_from_the_grid() {
        // August 2026 starts on Saturday → Sunday before is July 26.
        let mut picker = DuePicker::new("2026-08-05 12:00");
        picker.layout.days = Rect {
            x: 10,
            y: 5,
            width: 21,
            height: 6,
        };
        // Week 0, column 3 (Wed) → July 29 (surrounding month).
        assert!(picker.click(10 + 3 * 3 + 1, 5));
        assert_eq!(picker.day, NaiveDate::from_ymd_opt(2026, 7, 29).unwrap());

        // Reset to August so the grid is that month again.
        picker.day = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        // Week 1, column 3 → August 5.
        assert!(picker.click(10 + 3 * 3 + 1, 6));
        assert_eq!(picker.day, NaiveDate::from_ymd_opt(2026, 8, 5).unwrap());
    }

    #[test]
    fn click_focuses_hour_and_minute() {
        let mut picker = DuePicker::new("2026-08-05 12:00");
        picker.layout.hour = Rect {
            x: 5,
            y: 10,
            width: 2,
            height: 1,
        };
        picker.layout.minute = Rect {
            x: 8,
            y: 10,
            width: 2,
            height: 1,
        };
        assert!(picker.click(5, 10));
        assert_eq!(picker.focus, PickerFocus::Hour);
        assert!(picker.click(8, 10));
        assert_eq!(picker.focus, PickerFocus::Minute);
    }

    #[test]
    fn digits_type_hour_and_minute() {
        let mut picker = DuePicker::new("2030-01-02 00:00");
        picker.focus = PickerFocus::Hour;
        picker.type_digit(1);
        picker.type_digit(5);
        assert_eq!(picker.hour, 15);
        assert_eq!(picker.focus, PickerFocus::Minute);
        picker.type_digit(3);
        picker.type_digit(0);
        assert_eq!(picker.minute, 30);
        assert_eq!(picker.value(), "2030-01-02 15:30");
    }

    #[test]
    fn single_digit_hour_auto_advances() {
        let mut picker = DuePicker::new("2030-01-02 00:00");
        picker.focus = PickerFocus::Hour;
        picker.type_digit(9);
        assert_eq!(picker.hour, 9);
        assert_eq!(picker.focus, PickerFocus::Minute);
    }

    #[test]
    fn digit_on_calendar_starts_hour() {
        let mut picker = DuePicker::new("2030-01-02 08:00");
        assert_eq!(picker.focus, PickerFocus::Calendar);
        picker.type_digit(1);
        assert_eq!(picker.focus, PickerFocus::Hour);
        assert_eq!(picker.hour, 1);
    }
}
