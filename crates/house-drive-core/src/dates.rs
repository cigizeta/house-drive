//! Calendar dates (UTC) for trash folders and conflict names, without a date library: two small
//! conversions between days since the Unix epoch and year-month-day (Howard Hinnant's
//! algorithms).

/// Days since 1970-01-01 of a time in nanoseconds since the Unix epoch.
pub fn day_of(nanos: i64) -> i64 {
    return nanos.div_euclid(86_400_000_000_000);
}

/// `YYYY-MM-DD` of a time in nanoseconds since the Unix epoch.
pub fn date_of(nanos: i64) -> String {
    let (year, month, day) = civil_from_days(day_of(nanos));
    return format!("{year:04}-{month:02}-{day:02}");
}

/// `YYYY-MM-DD HH:MM:SS UTC` of a time in nanoseconds since the Unix epoch.
pub fn date_time_of(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000).rem_euclid(86_400);
    return format!(
        "{} {:02}:{:02}:{:02} UTC",
        date_of(nanos),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date; `None` for anything else.
pub fn parse_date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    return Some(days);
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    return (year, month, day);
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146_097 + doe - 719_468;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000_000_000;

    #[test]
    fn known_dates() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(DAY - 1), "1970-01-01");
        assert_eq!(date_of(11_017 * DAY), "2000-03-01");
        assert_eq!(date_of(-1), "1969-12-31");
        assert_eq!(parse_date("2000-03-01"), Some(11_017));
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(date_time_of(DAY + 3_723_000_000_000), "1970-01-02 01:02:03 UTC");
    }

    #[test]
    fn round_trips_over_centuries() {
        // Nanoseconds in an i64 reach about 292 years either side of 1970.
        for days in (-100_000..100_000).step_by(37) {
            assert_eq!(parse_date(&date_of(days * DAY)), Some(days));
        }
    }

    #[test]
    fn refuses_non_dates() {
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-1-01"), None);
        assert_eq!(parse_date("notes"), None);
    }
}
