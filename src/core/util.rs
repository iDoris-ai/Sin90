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

/// Same fixed-width shape as [`now_iso8601`], `secs_from_now` seconds later —
/// used by the outbox reconciler (`adapter_agent24::reconciler`, T3.3.2) to
/// compute a retryable failure's `next_attempt_at` (spec.md M3's exponential
/// backoff) without pulling a full date-arithmetic dependency into `core`.
/// Saturates rather than overflowing for an absurdly large input.
pub fn iso8601_after_secs(secs_from_now: u64) -> String {
    iso8601_at(epoch_secs().saturating_add(secs_from_now))
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

/// Canonical `YYYY-MM-DD` calendar date — `Review.period` for `kind: daily`
/// (design §3.2/§4.1's M4 patch) — or `None` if `s` isn't one. Unlike
/// [`is_fixed_iso8601`] (a full `...Thh:mm:ssZ` timestamp), a period carries
/// no time component. Rejects out-of-range months/days, including a Feb 29
/// on a non-leap year, with the same rigor [`canonical_iso_week`] applies to
/// week numbers — a loose `chrono::NaiveDate::from_ymd_opt` isn't available
/// here (`chrono` is dev-only, see `Cargo.toml`'s M6 comment: `core` stays
/// dependency-free at runtime), so this is a small hand-rolled calendar
/// check instead.
pub fn canonical_iso_date(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
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
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    if year < 1000 || month == 0 || month > 12 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
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
    // M4 (scheduler-callback design review, written into spec.md): when
    // BOTH the day-of-month (field 2) and day-of-week (field 4) are
    // restricted (neither is `*`), this `cron` crate ANDs them —
    // `cron-0.15.0/src/schedule.rs:117-125` iterates candidate
    // `day_of_month`s and then `continue`s past any that don't ALSO match
    // `days_of_week`. POSIX cron ORs the same two fields in that situation
    // (run on day-of-month 1 OR every Monday, whichever comes first) — the
    // opposite semantics from the same string. `"0 7 1 * MON"` would read,
    // under POSIX, as "7am on the 1st AND every Monday"; under this crate it
    // silently becomes "7am on whichever Mondays happen to fall on the 1st"
    // (i.e. almost never). Rather than pick a meaning, this validator
    // refuses the ambiguous case outright: at least one of dom/dow must be
    // `*`. (`validate_cron` does not special-case "dow names" here: this
    // check runs on the raw field text, so a still-digit-bearing dow would
    // already have been rejected above, and a `*` dow always passes.)
    let dom = fields[2];
    if dom != "*" && dow != "*" {
        return Err(format!(
            "day-of-month and day-of-week must not both be restricted (this \
             cron crate ANDs them where POSIX ORs them — the same string \
             would mean two different schedules); make one of them '*', \
             got dom={dom:?} dow={dow:?} in {expr:?}"
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

/// Days since the Unix epoch (1970-01-01) for a Gregorian civil date —
/// Howard Hinnant's `days_from_civil`, the exact inverse of the
/// `civil_from_days` arithmetic [`iso8601_at`] uses above (same era/century
/// decomposition), reimplemented on signed `i64` so it stays correct for
/// dates before 1970 (`iso8601_at` can't: it derives `days` from a `u64`
/// seconds count).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The exact inverse of [`days_from_civil`] — `(year, month, day)` for a
/// given day count since the Unix epoch. Same algorithm [`iso8601_at`]
/// inlines for its `u64`-seconds path, extracted here so week-boundary math
/// can call it directly on signed day counts (a week can start before 1970
/// in principle, though `canonical_iso_week` in practice only ever hands
/// this recent years).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// ISO weekday for a day count since the Unix epoch: `1 = Monday .. 7 =
/// Sunday` (ISO-8601's own numbering — NOT the `cron` crate's `1 = Sunday`
/// convention [`validate_cron`]'s doc warns about; those are two unrelated
/// numbering schemes and this function has nothing to do with cron). The
/// epoch (`days = 0`, 1970-01-01) was a Thursday, so `(days + 3).rem_euclid(7)
/// + 1` lands on `4`.
fn iso_weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// `[start, end)` bounds for an ISO-8601 week (`YYYY-Www`), as the same
/// fixed-width `YYYY-MM-DDThh:mm:ssZ` UTC strings [`now_iso8601`] stamps
/// events with — so callers can pass them straight into a lexical `at >= ?
/// AND at < ?` range compare, the same shape [`Sin90Store::attention`]
/// already uses for a start/end window. `start` is the week's Monday at
/// `00:00:00Z`; `end` is the FOLLOWING Monday at `00:00:00Z` (so Sunday
/// `23:59:59Z` of the week itself falls inside the window and the next
/// Monday's `00:00:00Z` does not — a block completed exactly at that instant
/// belongs to the NEXT week).
///
/// ISO-8601's own week rule (not a Sin90 invention): week 1 of a year is the
/// week containing that year's first Thursday, equivalently the week
/// containing January 4th. Every other week is `week1_monday + (n-1)*7`
/// days. `None` if `iso_week` doesn't parse as `YYYY-Www`
/// ([`canonical_iso_week`]).
///
/// No timezone conversion: Sin90 stamps every event `at` in UTC only (there
/// is no per-user timezone setting anywhere in this codebase today — a
/// `Routine.tz` is per-routine scheduling metadata, not a viewer's clock),
/// so "the week" here is the UTC calendar week, matching the only clock
/// events are ever written against.
pub fn iso_week_bounds(iso_week: &str) -> Option<(String, String)> {
    let canon = canonical_iso_week(iso_week)?;
    let year: i64 = canon[0..4].parse().ok()?;
    let week: i64 = canon[6..8].parse().ok()?;

    let jan4 = days_from_civil(year, 1, 4);
    let week1_monday = jan4 - (iso_weekday(jan4) - 1);
    let start_days = week1_monday + (week - 1) * 7;
    let end_days = start_days + 7;

    let (sy, sm, sd) = civil_from_days(start_days);
    let (ey, em, ed) = civil_from_days(end_days);
    Some((
        format!("{sy:04}-{sm:02}-{sd:02}T00:00:00Z"),
        format!("{ey:04}-{em:02}-{ed:02}T00:00:00Z"),
    ))
}

/// The ISO-8601 week label (`YYYY-Www`) that a fixed-width UTC timestamp
/// (`YYYY-MM-DDThh:mm:ssZ`, [`now_iso8601`]'s shape) falls in — the inverse
/// of [`iso_week_bounds`]: for any `at` this accepts, `iso_week_bounds(&
/// iso_week_of(at).unwrap()).unwrap()` is a `[start, end)` window with
/// `start <= at < end`. `None` if `at` isn't fixed-width ISO-8601
/// ([`is_fixed_iso8601`]) or its date part isn't a real calendar date (same
/// rigor [`canonical_iso_date`] applies — a nonsense `2026-13-40` does not
/// silently produce some week label).
///
/// T4.3.2 (spec.md M4 "review Routine 到点自动建草稿"): used to turn a
/// fired `Routine{kind:review}`'s `scheduled_for` into the ISO week whose
/// draft it should ensure exists. No timezone conversion, same posture
/// [`iso_week_bounds`]'s doc states: Sin90 only ever stamps UTC.
pub fn iso_week_of(at: &str) -> Option<String> {
    if !is_fixed_iso8601(at) {
        return None;
    }
    let year: u32 = at[0..4].parse().ok()?;
    let month: u32 = at[5..7].parse().ok()?;
    let day: u32 = at[8..10].parse().ok()?;
    if year < 1000 || month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
        return None;
    }

    let days = days_from_civil(year as i64, month as i64, day as i64);
    let wd = iso_weekday(days); // 1=Mon..7=Sun
    let monday = days - (wd - 1);
    // ISO-8601: a week belongs to the Gregorian year containing its Thursday
    // (equivalently, the year whose Jan 4th falls in this same week) — this
    // is what lets `2026-W01` legitimately start on 2025-12-29
    // (`iso_week_bounds_matches_independently_verified_dates` pins that
    // example for the inverse direction).
    let thursday = monday + 3;
    let (iso_year, _, _) = civil_from_days(thursday);

    let jan4 = days_from_civil(iso_year, 1, 4);
    let week1_monday = jan4 - (iso_weekday(jan4) - 1);
    let week = (monday - week1_monday) / 7 + 1;
    Some(format!("{iso_year:04}-W{week:02}"))
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
    fn iso_date_labels_are_validated() {
        assert_eq!(
            canonical_iso_date("2026-09-24").as_deref(),
            Some("2026-09-24")
        );
        // Leap day: 2024 is a leap year, 2026 is not.
        assert!(canonical_iso_date("2024-02-29").is_some());
        assert!(canonical_iso_date("2026-02-29").is_none());
        // Leap-year rule itself: divisible by 100 but not 400 is NOT leap.
        assert!(canonical_iso_date("2000-02-29").is_some());
        assert!(canonical_iso_date("1900-02-29").is_none());
        for bad in [
            "garbage",
            "2026-13-01", // month 13
            "2026-00-01", // month 0
            "2026-04-31", // April has 30 days
            "2026-01-32", // day 32
            "2026-01-00", // day 0
            "26-01-01",   // 2-digit year
            "2026-1-01",  // month not zero-padded
            "2026/01/01", // wrong separator
            "2026-01-01T00:00:00Z",
        ] {
            assert!(canonical_iso_date(bad).is_none(), "{bad} must be rejected");
        }
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

    /// M4 (scheduler-callback design review): this `cron` crate ANDs a
    /// restricted day-of-month with a restricted day-of-week
    /// (`cron-0.15.0/src/schedule.rs:117-125`), where POSIX ORs them — the
    /// same string means two different schedules depending on which
    /// convention the reader has in mind, so both restricted at once is
    /// rejected outright. Positive controls: either field alone restricted
    /// (the other `*`) is accepted.
    #[test]
    fn routine_validate_cron_rejects_both_dom_and_dow_restricted() {
        assert!(validate_cron("0 7 1 * MON").is_err());
        assert!(validate_cron("0 7 1 * *").is_ok());
        assert!(validate_cron("0 7 * * MON").is_ok());
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

    /// T4.3.1: `iso_week_bounds` against dates independently verified via
    /// Python's `datetime.date.fromisocalendar` (not derived from this
    /// code): `2026-W39` is `2026-09-21 .. 2026-09-28`, `2026-W01` starts
    /// `2025-12-29` (a week can start in the PREVIOUS calendar year — the
    /// case that makes a naive "Jan 1 = week 1" implementation wrong), and
    /// `2025-W52` is the week right before it.
    #[test]
    fn iso_week_bounds_matches_independently_verified_dates() {
        let (start, end) = iso_week_bounds("2026-W39").unwrap();
        assert_eq!(start, "2026-09-21T00:00:00Z");
        assert_eq!(end, "2026-09-28T00:00:00Z");

        let (start, end) = iso_week_bounds("2026-W01").unwrap();
        assert_eq!(
            start, "2025-12-29T00:00:00Z",
            "2026-W01 starts in the previous calendar year"
        );
        assert_eq!(end, "2026-01-05T00:00:00Z");

        let (start, end) = iso_week_bounds("2025-W52").unwrap();
        assert_eq!(start, "2025-12-22T00:00:00Z");
        assert_eq!(
            end, "2025-12-29T00:00:00Z",
            "2025-W52's end must be exactly 2026-W01's start — adjacent weeks tile with no gap or overlap"
        );
    }

    #[test]
    fn iso_week_bounds_accepts_lowercase_w_and_rejects_garbage() {
        assert_eq!(
            iso_week_bounds("2026-w39"),
            iso_week_bounds("2026-W39"),
            "lowercase w must canonicalize the same as uppercase"
        );
        for bad in ["2026-W99", "2026-W00", "garbage", "2026-39", ""] {
            assert!(iso_week_bounds(bad).is_none(), "{bad:?} must be rejected");
        }
    }

    /// Every emitted timestamp ([`now_iso8601`]'s shape) must satisfy
    /// `is_fixed_iso8601` so `at >= start AND at < end` compares correctly —
    /// pins that `iso_week_bounds`' own output is the same fixed width, not
    /// just eyeballed in the assertions above.
    #[test]
    fn iso_week_bounds_output_is_fixed_width_iso8601() {
        let (start, end) = iso_week_bounds("2026-W39").unwrap();
        assert!(is_fixed_iso8601(&start), "{start}");
        assert!(is_fixed_iso8601(&end), "{end}");
    }

    /// T4.3.2: `iso_week_of` is the exact inverse of `iso_week_bounds` — the
    /// `start` instant of every week's `[start, end)` window maps back to
    /// that same week's label, and the `end` instant (exclusive) maps to
    /// the FOLLOWING week's label, not this one.
    #[test]
    fn iso_week_of_is_the_inverse_of_iso_week_bounds() {
        for week in ["2026-W39", "2026-W01", "2025-W52", "2020-W53"] {
            let (start, end_exclusive) = iso_week_bounds(week).unwrap();
            assert_eq!(
                iso_week_of(&start).as_deref(),
                Some(week),
                "start of {week} must map back to {week}"
            );
            let next_week = iso_week_of(&end_exclusive).unwrap();
            assert_ne!(
                next_week, week,
                "the exclusive end of {week} must belong to the NEXT week"
            );
            // And that next week's own bounds must start exactly there —
            // adjacent weeks tile with no gap or overlap.
            let (next_start, _) = iso_week_bounds(&next_week).unwrap();
            assert_eq!(next_start, end_exclusive);
        }
    }

    /// The concrete case T4.3.1's own fixture already hand-verified
    /// independently (module doc there): 2026-W39 = 2026-09-21 .. 2026-09-28.
    #[test]
    fn iso_week_of_matches_independently_verified_dates() {
        assert_eq!(
            iso_week_of("2026-09-21T00:00:00Z").as_deref(),
            Some("2026-W39"),
            "Monday 00:00:00Z is inside the week"
        );
        assert_eq!(
            iso_week_of("2026-09-27T23:59:59Z").as_deref(),
            Some("2026-W39"),
            "Sunday 23:59:59Z is still inside the week"
        );
        assert_eq!(
            iso_week_of("2026-09-28T00:00:00Z").as_deref(),
            Some("2026-W40"),
            "next Monday 00:00:00Z belongs to the NEXT week"
        );
        assert_eq!(
            iso_week_of("2025-12-29T00:00:00Z").as_deref(),
            Some("2026-W01"),
            "a week can start in the previous calendar year"
        );
    }

    #[test]
    fn iso_week_of_rejects_malformed_or_nonsense_timestamps() {
        for bad in [
            "garbage",
            "2026-09-24",           // not fixed-width (missing time)
            "2026-13-01T00:00:00Z", // month 13
            "2026-02-30T00:00:00Z", // Feb 30 doesn't exist
            "",
        ] {
            assert!(iso_week_of(bad).is_none(), "{bad:?} must be rejected");
        }
    }
}
