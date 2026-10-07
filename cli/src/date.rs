use std::error::Error;

use chrono::NaiveDate;

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
}
