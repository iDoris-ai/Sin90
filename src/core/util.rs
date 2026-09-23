//! Tiny, dependency-free time/id helpers.
//!
//! Sin90's `core` layer has zero Agent24 dependency (design §5.2), so these are
//! reimplemented locally rather than pulled from `agent24-core` — same shapes,
//! same guarantees (fixed-width UTC timestamps so a lexical compare is
//! chronological; ULIDs are lexically sortable by creation time).

use rand::RngCore;
use std::str::FromStr;

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

/// Start of the user's current local day, as a fixed-width UTC timestamp
/// comparable with event `at` values. "Local" is the system timezone (`TZ`,
/// which Agent24 passes to modules, else `/etc/localtime`); a personal OS's
/// "today" is the user's calendar day, not UTC's.
pub fn local_day_start_utc() -> String {
    day_start_utc(jiff::Timestamp::now(), &jiff::tz::TimeZone::system())
}

pub(crate) fn day_start_utc(now: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    let start = now
        .to_zoned(tz.clone())
        .start_of_day()
        .expect("every civil day has a start");
    start.timestamp().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
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

/// Validate a `Routine.cron` expression (design §3.2, M3) the same way
/// Agent24's `agent24-scheduler` validates a `ScheduleSpec::Cron.expr`
/// (`agent24-scheduler/src/next_fire.rs::normalize_cron`/`validate`): exactly
/// 5 fields (`min hour dom month dow`, no seconds, no embedded year), checked
/// by prepending a `0` seconds field and parsing with the `cron` crate.
/// Sin90 never computes a next firing itself — that's the kernel scheduler's
/// job once T3.3.1's outbox upserts this string — so this is syntax
/// validation only, not a semantic "will this ever fire" check.
///
/// **H1 (T3.1.1 review)**: the day-of-week field is deliberately NOT POSIX.
/// The `cron` crate this validator and the kernel scheduler both link
/// (pinned in `Cargo.toml` — must move in lockstep with theirs) numbers
/// weekdays `1..=7` with **`1 = Sunday`**
/// (`cron-0.15.0/src/time_unit/days_of_week.rs::ordinal_from_name`'s
/// `sun|sunday => 1`), not POSIX's `0` (or `7`) `= Sunday, 1 = Monday`. The
/// exact same digit range therefore means a DIFFERENT set of days depending
/// on which convention the person typing it has in their head —
/// `"0 7 * * 1-5"`, read as POSIX "weekdays", is actually Sun..Thu here, a
/// silent one-day-early bug that would only surface as "why did this fire on
/// Sunday". So this validator refuses ANY digit in the day-of-week field
/// outright and accepts only `*` or the English weekday names the crate
/// itself parses case-insensitively (`mon`/`monday` .. `sun`/`sunday`, with
/// ranges like `MON-FRI` and lists like `MON,WED,FRI`) — a name means the
/// same day under either convention, so it can never be silently
/// misinterpreted.
pub fn validate_cron(expr: &str) -> Result<(), String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron must have exactly 5 fields (min hour dom month dow), got {}: {expr:?}",
            fields.len()
        ));
    }
    let dow = fields[4];
    if dow.chars().any(|c| c.is_ascii_digit()) {
        return Err(format!(
            "day-of-week field must be '*' or weekday names (mon..sun, e.g. \
             MON-FRI or MON,WED,FRI) — digits are ambiguous between POSIX \
             (0/7=Sun, 1=Mon) and this cron crate's own 1=Sun..7=Sat \
             ordinals, got {dow:?} in {expr:?}"
        ));
    }
    let normalized = format!("0 {expr}");
    cron::Schedule::from_str(&normalized)
        .map_err(|e| format!("invalid cron expression {expr:?}: {e}"))?;
    Ok(())
}

/// Validate an IANA timezone name (`Routine.tz`, design §3.2 — defaults to
/// `"UTC"`). A bare `TEXT` column has no way to enforce this at the SQLite
/// level, so it's checked here before every write that sets `tz`.
pub fn validate_tz(tz: &str) -> Result<(), String> {
    chrono_tz::Tz::from_str(tz)
        .map(|_| ())
        .map_err(|_| format!("unknown IANA timezone: {tz:?}"))
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

    #[test]
    fn day_start_follows_the_local_calendar_day_not_utc() {
        let now: jiff::Timestamp = "2026-09-22T20:00:00Z".parse().unwrap();
        // 03:00 on Sep 23 in UTC+7: the local day began at 17:00 UTC on Sep 22.
        let plus7 = jiff::tz::TimeZone::fixed(jiff::tz::offset(7));
        assert_eq!(day_start_utc(now, &plus7), "2026-09-22T17:00:00Z");
        // Control: in UTC it is still Sep 22.
        assert_eq!(
            day_start_utc(now, &jiff::tz::TimeZone::UTC),
            "2026-09-22T00:00:00Z"
        );
        assert!(is_fixed_iso8601(&day_start_utc(now, &plus7)));
    }

    #[test]
    fn routine_validate_cron_accepts_5_field_expressions_with_star_weekday() {
        assert!(validate_cron("*/15 * * * *").is_ok());
        assert!(validate_cron("0 7 * * *").is_ok());
    }

    #[test]
    fn routine_validate_cron_rejects_wrong_field_count_and_garbage() {
        for bad in [
            "",
            "0 7 * *",       // 4 fields
            "0 0 7 * * MON", // 6 fields (seconds not accepted from a Routine)
            // 5 whitespace-separated tokens, so it PASSES the field-count
            // check — the point of this case is that `cron::Schedule` itself
            // must still reject "not"/"a"/"cron"/"at" as field expressions.
            "not a cron at all",
        ] {
            assert!(validate_cron(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// H1 (T3.1.1 review): a digit in the day-of-week field is ALWAYS
    /// rejected, regardless of whether it happens to be in-range for this
    /// crate's `1..=7` ordinals — `"0 7 * * 1-5"` reads as "weekdays" under
    /// POSIX but is Sun..Thu under this crate's `1=Sun` convention, so
    /// there's no way to accept it without silently picking one meaning.
    #[test]
    fn routine_validate_cron_rejects_any_digit_in_day_of_week() {
        for bad in [
            "0 7 * * 1-5",   // POSIX reads this as weekdays; this crate as Sun..Thu
            "0 7 * * 1,3,5", // same ambiguity via a list
            "0 7 * * 0",     // POSIX Sunday; not a valid ordinal at all here (0 is out of 1..=7)
            "0 7 * * 7",     // POSIX Sunday-as-7; this crate's Saturday — opposite days
            "99 99 99 99 99",
        ] {
            assert!(validate_cron(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn routine_validate_cron_accepts_weekday_abbreviations_ranges_and_lists() {
        assert!(validate_cron("0 7 * * MON").is_ok());
        assert!(validate_cron("0 7 * * MON-FRI").is_ok());
        assert!(validate_cron("0 7 * * MON,WED,FRI").is_ok());
        // Case-insensitive: matches the `cron` crate's own
        // `name.to_lowercase()` in `ordinal_from_name`.
        assert!(validate_cron("0 7 * * mon-fri").is_ok());
        assert!(validate_cron("0 7 * * Sun,Sat").is_ok());
    }

    /// H1's actual payoff: prove the accepted named form fires on the
    /// weekday it says, not on whatever a shifted digit ordinal would mean.
    /// Fixed starting point (a known Saturday) — deliberately NOT
    /// `Utc::now()`/`Schedule::upcoming`, which reads the wall clock and
    /// would make this test's outcome depend on what day it happens to run;
    /// `Schedule::after(&fixed_start)` is the crate's own "same as
    /// `upcoming`, but you name the start instant" method.
    #[test]
    fn routine_named_weekday_range_fires_on_the_intended_weekday() {
        use chrono::{Datelike, TimeZone, Weekday};

        // 2026-01-10 is a Saturday (verified independently via `date`/python,
        // not derived from this code).
        let start = chrono::Utc.with_ymd_and_hms(2026, 1, 10, 12, 0, 0).unwrap();
        let schedule = cron::Schedule::from_str("0 0 7 * * MON-FRI").unwrap();
        let next = schedule
            .after(&start)
            .next()
            .expect("a weekday schedule always has a next fire");
        assert_eq!(
            next.weekday(),
            Weekday::Mon,
            "got {next}, expected a Monday"
        );
        assert_eq!(
            next.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 12).unwrap(),
            "next weekday fire after Saturday 2026-01-10 must be Monday 2026-01-12"
        );
    }

    #[test]
    fn routine_validate_tz_accepts_iana_names_and_rejects_everything_else() {
        assert!(validate_tz("UTC").is_ok());
        assert!(validate_tz("America/New_York").is_ok());
        assert!(validate_tz("Asia/Shanghai").is_ok());
        for bad in ["", "Not/AZone", "GMT+8", "UTC+8", "shanghai"] {
            assert!(validate_tz(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
