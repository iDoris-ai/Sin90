//! Tiny, dependency-free time/id helpers.
//!
//! Sin90's `core` layer has zero Agent24 dependency (design §5.2), so these are
//! reimplemented locally rather than pulled from `agent24-core` — same shapes,
//! same guarantees (fixed-width UTC timestamps so a lexical compare is
//! chronological; ULIDs are lexically sortable by creation time).

use rand::RngCore;

pub fn ulid() -> String {
    const B32: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut out = [0u8; 26];
    let mut t = ms;
    for i in (0..10).rev() {
        out[i] = B32[(t % 32) as usize];
        t /= 32;
    }
    let mut rnd = [0u8; 16];
    rand::rng().fill_bytes(&mut rnd);
    for i in 0..16 {
        out[10 + i] = B32[(rnd[i] % 32) as usize];
    }
    String::from_utf8(out.to_vec()).expect("B32 alphabet is ASCII")
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn iso8601_at(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Fixed-width `YYYY-MM-DDThh:mm:ssZ` UTC timestamp, `now`.
pub fn now_iso8601() -> String {
    iso8601_at(epoch_secs())
}

/// A fixed-width `YYYY-MM-DDThh:mm:ssZ` (20 chars) — the exact shape
/// [`now_iso8601`] stamps events with, so a lexical window compare is
/// chronological. Deliberately not a full RFC3339 parser: we only need to
/// reject shapes (like a bare date) that would compare wrong.
pub fn is_fixed_iso8601(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19) || c.is_ascii_digit())
}

/// Canonical ISO-8601 week label (`YYYY-Www`) or `None` if `s` isn't one.
/// Accepts a lowercase `w`, emits uppercase; rejects week 00 and weeks past
/// the year's real count (52, or 53 when Jan 1 is a Thursday, or a Wednesday
/// in a leap year).
pub fn canonical_iso_week(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() != 8 || b[4] != b'-' || !matches!(b[5], b'W' | b'w') {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let year = digits(0..4)?;
    let week = digits(6..8)?;
    if year < 1000 || week == 0 || week > iso_weeks_in_year(year) {
        return None;
    }
    Some(format!("{year:04}-W{week:02}"))
}

fn iso_weeks_in_year(year: u32) -> u32 {
    let y = year - 1;
    // Day of week of Jan 1 (Gregorian), 0 = Sunday.
    let jan1 = (1 + 5 * (y % 4) + 4 * (y % 100) + 6 * (y % 400)) % 7;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    if jan1 == 4 || (leap && jan1 == 3) {
        53
    } else {
        52
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_chars_and_lexically_sortable_by_time() {
        let a = ulid();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = ulid();
        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn now_iso8601_is_fixed_width() {
        let s = now_iso8601();
        assert!(is_fixed_iso8601(&s), "{s}");
    }

    #[test]
    fn is_fixed_iso8601_rejects_bare_date() {
        assert!(!is_fixed_iso8601("2026-08-11"));
        assert!(is_fixed_iso8601("2026-08-11T00:00:00Z"));
    }

    #[test]
    fn iso_week_labels_are_validated_and_canonicalized() {
        assert_eq!(canonical_iso_week("2026-W42").as_deref(), Some("2026-W42"));
        assert_eq!(canonical_iso_week("2026-w07").as_deref(), Some("2026-W07"));
        // 2026 starts on a Thursday → 53 weeks; 2021 (Friday) → 52; 2020
        // (leap, Wednesday) → 53.
        assert!(canonical_iso_week("2026-W53").is_some());
        assert!(canonical_iso_week("2021-W53").is_none());
        assert!(canonical_iso_week("2020-W53").is_some());
        for bad in [
            "garbage",
            "2026-W99",
            "2026-W00",
            "2026-42",
            "2026W42",
            "26-W42",
            "2026-W4",
            "２026-W42",
            "2026-W+1",
        ] {
            assert!(canonical_iso_week(bad).is_none(), "{bad} must be rejected");
        }
    }
}
