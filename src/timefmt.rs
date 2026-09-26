//! Time handling. The log stores epoch milliseconds as the machine-readable
//! field and a local ISO-8601 stamp beside it for a human reading the file; the
//! reader only ever compares the millis.

use crate::sys;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `2026-09-26T11:24:51.910+02:00`
pub fn format_local(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let sub = millis.rem_euclid(1000);
    let tm = sys::local_tm(secs);
    let off_min = tm.gmtoff / 60;
    let (sign, off_abs) = if off_min < 0 {
        ('-', -off_min)
    } else {
        ('+', off_min)
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
        tm.year + 1900,
        tm.mon + 1,
        tm.mday,
        tm.hour,
        tm.min,
        tm.sec,
        sub,
        sign,
        off_abs / 60,
        off_abs % 60
    )
}

/// `11:24:51` — what the `tail` view shows, since the date is usually today.
pub fn format_clock(millis: i64) -> String {
    let tm = sys::local_tm(millis.div_euclid(1000));
    format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec)
}

/// Accepts a relative duration (`90s`, `30m`, `2h`, `7d`) or a local
/// wall-clock instant (`2026-09-26`, `2026-09-26 11:00`, `2026-09-26T11:00:00`).
/// Returns the cutoff as epoch milliseconds.
pub fn parse_since(spec: &str, now_millis: i64) -> Result<i64, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty --since value".into());
    }
    if let Some(ms) = parse_duration(spec) {
        return Ok(now_millis - ms);
    }
    parse_instant(spec).ok_or_else(|| {
        format!(
            "cannot read '{spec}' as a duration (90s, 30m, 2h, 7d) or a date (2026-09-26T11:00)"
        )
    })
}

/// A duration in milliseconds, or `None` if this is not a duration at all.
pub fn parse_duration(spec: &str) -> Option<i64> {
    let (digits, unit) = spec.split_at(spec.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = digits.parse().ok()?;
    let scale = match unit {
        "s" | "sec" | "secs" => 1_000,
        "m" | "min" | "mins" => 60_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(scale)
}

fn parse_instant(spec: &str) -> Option<i64> {
    let (date, time) = match spec.split_once(['T', ' ']) {
        Some((d, t)) => (d, t),
        None => (spec, "00:00:00"),
    };
    let mut d = date.split('-');
    let year: i32 = d.next()?.parse().ok()?;
    let mon: i32 = d.next()?.parse().ok()?;
    let mday: i32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hour: i32 = t.next()?.parse().ok()?;
    let min: i32 = t.next().unwrap_or("0").parse().ok()?;
    let sec: i32 = t.next().unwrap_or("0").parse().ok()?;
    if t.next().is_some() {
        return None;
    }
    let mut tm = sys::Tm::zeroed();
    tm.year = year - 1900;
    tm.mon = mon - 1;
    tm.mday = mday;
    tm.hour = hour;
    tm.min = min;
    tm.sec = sec;
    Some(sys::local_epoch(tm) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_known_instant_with_millis_and_offset() {
        let s = format_local(1_790_000_000_123);
        // The offset depends on this machine's zone, so check the shape, not the hour.
        assert!(s.starts_with("2026-"), "{s}");
        assert!(s.contains(".123"), "{s}");
        assert!(s.contains('+') || s.contains("T00:00:00.123-"), "{s}");
        assert_eq!(s.len(), "2026-09-26T11:24:51.910+02:00".len(), "{s}");
    }

    #[test]
    fn formats_sub_second_boundaries_without_borrowing_a_second() {
        assert!(format_local(1_790_000_000_000).contains(".000"));
        assert!(format_local(1_790_000_000_999).contains(".999"));
    }

    #[test]
    fn durations_parse_in_every_accepted_spelling() {
        assert_eq!(parse_duration("90s"), Some(90_000));
        assert_eq!(parse_duration("30m"), Some(1_800_000));
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("2hours"), Some(7_200_000));
        assert_eq!(parse_duration("7d"), Some(604_800_000));
        assert_eq!(parse_duration("7"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("2026-09-26"), None);
    }

    #[test]
    fn since_subtracts_a_duration_from_now() {
        assert_eq!(
            parse_since("1h", 10_000_000).unwrap(),
            10_000_000 - 3_600_000
        );
    }

    #[test]
    fn since_accepts_absolute_local_instants_and_round_trips_them() {
        let midnight = parse_since("2026-09-26", 0).unwrap();
        let same = parse_since("2026-09-26 00:00:00", 0).unwrap();
        let tee = parse_since("2026-09-26T00:00:00", 0).unwrap();
        assert_eq!(midnight, same);
        assert_eq!(midnight, tee);
        assert_eq!(format_local(midnight)[..19], *"2026-09-26T00:00:00");
    }

    #[test]
    fn since_rejects_nonsense_with_a_usable_message() {
        let err = parse_since("yesterday", 0).unwrap_err();
        assert!(err.contains("yesterday"), "{err}");
        assert!(parse_since("", 0).is_err());
        assert!(parse_since("2026-09", 0).is_err());
        assert!(parse_since("2026-09-26T00:00:00:00", 0).is_err());
    }

    #[test]
    fn clock_is_just_the_wall_time() {
        let s = format_clock(1_790_000_000_000);
        assert_eq!(s.len(), 8, "{s}");
        assert_eq!(s.matches(':').count(), 2, "{s}");
    }
}
