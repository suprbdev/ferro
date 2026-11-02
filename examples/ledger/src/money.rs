use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{LedgerError, Result};

pub fn exponent(currency: &str) -> u32 {
    match currency {
        "JPY" | "KRW" => 0,
        "KWD" | "BHD" => 3,
        _ => 2,
    }
}

pub fn parse_amount(text: &str, currency: &str) -> Result<i64> {
    let bad = || LedgerError::BadAmount(text.to_string());
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, frac) = match digits.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (digits, ""),
    };
    let places = exponent(currency) as usize;
    if (whole.is_empty() && frac.is_empty())
        || frac.len() > places
        || !whole
            .bytes()
            .chain(frac.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let mut value: i128 = 0;
    for byte in whole.bytes().chain(frac.bytes()) {
        value = value * 10 + i128::from(byte - b'0');
        if value > i128::from(i64::MAX) {
            return Err(bad());
        }
    }
    for _ in frac.len()..places {
        value *= 10;
        if value > i128::from(i64::MAX) {
            return Err(bad());
        }
    }
    let value = i64::try_from(value).map_err(|_| bad())?;
    Ok(if negative { -value } else { value })
}

pub fn format_amount(minor: i128, currency: &str) -> String {
    let places = exponent(currency);
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    if places == 0 {
        return format!("{sign}{abs}");
    }
    let scale = 10u128.pow(places);
    format!(
        "{sign}{}.{:0width$}",
        abs / scale,
        abs % scale,
        width = places as usize
    )
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn parse_date(text: &str) -> Result<i32> {
    let bad = || LedgerError::BadDate(text.to_string());
    let mut parts = text.trim().split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let year: i64 = y.parse().map_err(|_| bad())?;
    let month: i64 = m.parse().map_err(|_| bad())?;
    let day: i64 = d.parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return Err(bad());
    }
    i32::try_from(days).map_err(|_| bad())
}

pub fn format_date(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn format_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn today() -> i32 {
    (now_ms() / 86_400_000) as i32
}
