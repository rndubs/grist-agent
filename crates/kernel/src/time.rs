//! Wall-clock formatting: RFC 3339 UTC with millisecond precision
//! (`2026-09-06T12:34:56.789Z`), the only time format on the wire
//! (`kernel-interface.md` §2). No calendar crate: the civil-from-days
//! algorithm below is exact for the proleptic Gregorian calendar.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as an RFC 3339 UTC string with millisecond precision.
pub fn now_rfc3339_ms() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339_ms(now.as_millis())
}

/// Format a Unix timestamp in milliseconds as RFC 3339 UTC with millisecond precision.
pub fn format_rfc3339_ms(unix_ms: u128) -> String {
    let secs = (unix_ms / 1000) as i64;
    let millis = (unix_ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        // 2026-09-06T12:34:56.789Z
        assert_eq!(
            format_rfc3339_ms(1_788_698_096_789),
            "2026-09-06T12:34:56.789Z"
        );
        // Leap day.
        assert_eq!(
            format_rfc3339_ms(951_782_400_000),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn now_has_the_wire_shape() {
        let s = now_rfc3339_ms();
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
    }
}
