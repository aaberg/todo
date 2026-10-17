use std::error::Error;

use chrono::{Datelike, NaiveDate};

pub fn parse_relative_due_date(value: &str, today: NaiveDate) -> Result<NaiveDate, Box<dyn Error>> {
    let days = value
        .strip_suffix('d')
        .ok_or("due date must be a number of days followed by d, for example 1d")?
        .parse::<i64>()?;

    if days < 0 {
        return Err("due date cannot be negative".into());
    }

    today
        .checked_add_signed(chrono::TimeDelta::days(days))
        .ok_or_else(|| "due date is out of range".into())
}

/// Format a date as "Mon DDth, YYYY" with an ordinal suffix on the day.
/// Example: 2026-10-19 -> "Oct 19th, 2026"
pub fn format_date(date: NaiveDate) -> String {
    let day = date.day();
    let suffix = match day {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    date.format(&format!("%b {day}{suffix}, %Y")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn relative_due_dates_are_calculated_from_today() {
        let today = date("2026-08-11");
        assert_eq!(
            parse_relative_due_date("1d", today).unwrap(),
            date("2026-08-12")
        );
        assert_eq!(parse_relative_due_date("0d", today).unwrap(), today);
        assert!(parse_relative_due_date("tomorrow", today).is_err());
        assert!(parse_relative_due_date("-1d", today).is_err());
    }

    #[test]
    fn format_date_uses_ordinal_suffixes() {
        assert_eq!(format_date(date("2026-10-01")), "Oct 1st, 2026");
        assert_eq!(format_date(date("2026-10-02")), "Oct 2nd, 2026");
        assert_eq!(format_date(date("2026-10-03")), "Oct 3rd, 2026");
        assert_eq!(format_date(date("2026-10-04")), "Oct 4th, 2026");
        assert_eq!(format_date(date("2026-10-11")), "Oct 11th, 2026");
        assert_eq!(format_date(date("2026-10-12")), "Oct 12th, 2026");
        assert_eq!(format_date(date("2026-10-13")), "Oct 13th, 2026");
        assert_eq!(format_date(date("2026-10-19")), "Oct 19th, 2026");
        assert_eq!(format_date(date("2026-10-21")), "Oct 21st, 2026");
        assert_eq!(format_date(date("2026-10-22")), "Oct 22nd, 2026");
        assert_eq!(format_date(date("2026-10-23")), "Oct 23rd, 2026");
        assert_eq!(format_date(date("2026-01-31")), "Jan 31st, 2026");
    }
}
