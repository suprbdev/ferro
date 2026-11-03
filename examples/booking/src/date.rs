use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{BookingError, Result};
use crate::model::{Cents, Night};

pub const DAY_MS: u64 = 86_400_000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn night_of(ms: u64) -> Night {
    (ms / DAY_MS) as Night
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn format_night(night: Night) -> String {
    let (year, month, day) = civil(i64::from(night));
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn parse_night(text: &str) -> Result<Night> {
    let bad = || BookingError::BadDate(text.to_string());
    let mut parts = text.trim().splitn(3, '-');
    let year: i64 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let month: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let day: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let count = days(year, month, day);
    if civil(count) != (year, month, day) {
        return Err(bad());
    }
    Night::try_from(count).map_err(|_| bad())
}

pub fn format_time(ms: u64) -> String {
    let night = night_of(ms);
    let rest = ms % DAY_MS;
    format!(
        "{} {:02}:{:02}:{:02}.{:03}",
        format_night(night),
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

pub fn parse_time(text: &str) -> Result<u64> {
    let text = text.trim();
    if let Ok(ms) = text.parse::<u64>() {
        return Ok(ms);
    }
    let bad = || BookingError::BadDate(text.to_string());
    let (day, clock) = text
        .split_once(['T', ' '])
        .map_or((text, ""), |(day, clock)| (day, clock.trim_end_matches('Z')));
    let night = parse_night(day)?;
    let mut ms = u64::from(night) * DAY_MS;
    if !clock.is_empty() {
        let (clock, millis) = clock.split_once('.').unwrap_or((clock, "0"));
        let parts: Vec<u64> = clock
            .split(':')
            .map(|part| part.parse().map_err(|_| bad()))
            .collect::<Result<_>>()?;
        let (hours, minutes, seconds) = match parts.as_slice() {
            [h, m] => (*h, *m, 0),
            [h, m, s] => (*h, *m, *s),
            _ => return Err(bad()),
        };
        let millis: u64 = format!("{millis:0<3}").get(..3).and_then(|m| m.parse().ok()).ok_or_else(bad)?;
        if hours > 23 || minutes > 59 || seconds > 59 {
            return Err(bad());
        }
        ms += hours * 3_600_000 + minutes * 60_000 + seconds * 1000 + millis;
    }
    Ok(ms)
}

pub fn format_money(cents: Cents) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

pub fn parse_money(text: &str) -> Result<Cents> {
    let bad = || BookingError::BadMoney(text.to_string());
    let text = text.trim();
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() || frac.len() > 2 || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let whole: i64 = whole.parse().map_err(|_| bad())?;
    let frac: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<2}").parse().map_err(|_| bad())?
    };
    whole.checked_mul(100).and_then(|c| c.checked_add(frac)).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nights_round_trip() {
        for night in [0, 1, 59, 60, 365, 11_016, 20_000, 30_000] {
            assert_eq!(parse_night(&format_night(night)).unwrap(), night);
        }
        assert_eq!(format_night(0), "1970-01-01");
        assert_eq!(parse_night("2024-02-29").unwrap(), 19_782);
        assert!(parse_night("2023-02-29").is_err());
        assert!(parse_night("2024-13-01").is_err());
    }

    #[test]
    fn times_round_trip() {
        let ms = 1_793_000_123_456;
        assert_eq!(parse_time(&format_time(ms)).unwrap(), ms);
        assert_eq!(parse_time("1970-01-02").unwrap(), DAY_MS);
        assert_eq!(parse_time("1970-01-01T00:01").unwrap(), 60_000);
        assert_eq!(parse_time("42").unwrap(), 42);
        assert!(parse_time("1970-01-01 25:00").is_err());
    }

    #[test]
    fn money_round_trip() {
        assert_eq!(parse_money("129.5").unwrap(), 12_950);
        assert_eq!(parse_money("129").unwrap(), 12_900);
        assert_eq!(parse_money("0.07").unwrap(), 7);
        assert!(parse_money("-1").is_err());
        assert!(parse_money("1.234").is_err());
        assert_eq!(format_money(12_950), "129.50");
        assert_eq!(format_money(-7), "-0.07");
    }
}
