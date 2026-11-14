//! Heap-free fixed-layout formatters for the hot XML/HTTP emit paths.
//!
//! PROPFIND writes two formatted dates, a size and a quoted etag for
//! EVERY row of every listing; `to_rfc3339()` / `to_rfc2822()` run
//! chrono's format-spec interpreter and allocate a `String` each, and
//! `u64::to_string()` allocates another. These helpers render the same
//! bytes into a caller-provided stack buffer: zero heap traffic, no
//! interpreter.
//!
//! Byte-identity with chrono (for whole-second in-range UTC datetimes)
//! is asserted by the unit tests below and by the equivalence gate in
//! `benches/bench_propfind_xml.rs`. Out-of-range seconds (negative or
//! year > 9999, where the fixed-width layout no longer applies) return
//! `None` — callers keep the old chrono path as fallback, so exotic
//! values change nothing observable.

/// Seconds range rendering to a fixed-width 4-digit year: 1970-01-01
/// through 9999-12-31 23:59:59 UTC.
const MAX_4DIGIT_YEAR_SECS: i64 = 253_402_300_799;

const MONTHS: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];
const WEEKDAYS: [&[u8; 3]; 7] = [b"Thu", b"Fri", b"Sat", b"Sun", b"Mon", b"Tue", b"Wed"];

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day-of-era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Two-digit decimal pairs `"00" … "99"` — the same table-driven rendering
/// `core::fmt` uses for integer `Display`. One lookup replaces a div+mod
/// pair per two digits; ROUND10 adopted it after the naive div-by-10 loop
/// benchmarked SLOWER than `u64::to_string()` (std already uses this LUT).
const DEC_LUT: &[u8; 200] = b"0001020304050607080910111213141516171819\
                              2021222324252627282930313233343536373839\
                              4041424344454647484950515253545556575859\
                              6061626364656667686970717273747576777879\
                              8081828384858687888990919293949596979899";

#[inline]
fn push2(out: &mut [u8], pos: usize, v: u32) {
    let d = (v as usize) * 2;
    out[pos] = DEC_LUT[d];
    out[pos + 1] = DEC_LUT[d + 1];
}

#[inline]
fn push4(out: &mut [u8], pos: usize, v: i64) {
    out[pos] = b'0' + (v / 1000 % 10) as u8;
    out[pos + 1] = b'0' + (v / 100 % 10) as u8;
    out[pos + 2] = b'0' + (v / 10 % 10) as u8;
    out[pos + 3] = b'0' + (v % 10) as u8;
}

/// Split epoch seconds into (days, y, m, d, hh, mm, ss).
#[inline]
fn split(secs: i64) -> (i64, i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        days,
        y,
        m,
        d,
        (sod / 3600) as u32,
        (sod / 60 % 60) as u32,
        (sod % 60) as u32,
    )
}

/// `chrono::DateTime<Utc>::to_rfc3339()` for a whole-second timestamp:
/// `2026-07-17T11:47:14+00:00` (25 bytes) written into `buf`.
///
/// Returns `None` when `secs` is outside the fixed-width range —
/// callers fall back to chrono.
pub fn rfc3339_utc(buf: &mut [u8; 25], secs: i64) -> Option<&str> {
    if !(0..=MAX_4DIGIT_YEAR_SECS).contains(&secs) {
        return None;
    }
    let (_days, y, m, d, hh, mm, ss) = split(secs);
    push4(buf, 0, y);
    buf[4] = b'-';
    push2(buf, 5, m);
    buf[7] = b'-';
    push2(buf, 8, d);
    buf[10] = b'T';
    push2(buf, 11, hh);
    buf[13] = b':';
    push2(buf, 14, mm);
    buf[16] = b':';
    push2(buf, 17, ss);
    buf[19..25].copy_from_slice(b"+00:00");
    // SAFETY-free: every byte written above is ASCII.
    Some(std::str::from_utf8(&buf[..]).expect("ascii"))
}

/// `chrono::DateTime<Utc>::to_rfc2822()` for a whole-second timestamp:
/// `Fri, 17 Jul 2026 11:47:14 +0000` written into `buf`.
///
/// chrono does NOT zero-pad the day (`Thu, 1 Jan 1970 …`), so the
/// rendered length is 30 or 31 bytes — the round-4 PROPFIND equivalence
/// gate caught an early padded version of this function; the sweep test
/// below pins parity byte-for-byte across 60 years.
pub fn rfc2822_utc(buf: &mut [u8; 31], secs: i64) -> Option<&str> {
    if !(0..=MAX_4DIGIT_YEAR_SECS).contains(&secs) {
        return None;
    }
    let (days, y, m, d, hh, mm, ss) = split(secs);
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    buf[0..3].copy_from_slice(weekday);
    buf[3] = b',';
    buf[4] = b' ';
    let mut p = 5;
    if d >= 10 {
        buf[p] = b'0' + (d / 10) as u8;
        p += 1;
    }
    buf[p] = b'0' + (d % 10) as u8;
    p += 1;
    buf[p] = b' ';
    p += 1;
    buf[p..p + 3].copy_from_slice(MONTHS[(m - 1) as usize]);
    p += 3;
    buf[p] = b' ';
    p += 1;
    push4(buf, p, y);
    p += 4;
    buf[p] = b' ';
    p += 1;
    push2(buf, p, hh);
    p += 2;
    buf[p] = b':';
    p += 1;
    push2(buf, p, mm);
    p += 2;
    buf[p] = b':';
    p += 1;
    push2(buf, p, ss);
    p += 2;
    buf[p..p + 6].copy_from_slice(b" +0000");
    p += 6;
    Some(std::str::from_utf8(&buf[..p]).expect("ascii"))
}

/// Backward two-digit-chunk render of `v` into the tail of `buf`;
/// returns the first populated index. Shared core of
/// [`u64_str`] / [`i64_str`].
#[inline]
fn digits_to_tail(buf: &mut [u8], mut v: u64) -> usize {
    let mut pos = buf.len();
    while v >= 100 {
        let d = ((v % 100) as usize) * 2;
        v /= 100;
        pos -= 2;
        buf[pos] = DEC_LUT[d];
        buf[pos + 1] = DEC_LUT[d + 1];
    }
    if v >= 10 {
        let d = (v as usize) * 2;
        pos -= 2;
        buf[pos] = DEC_LUT[d];
        buf[pos + 1] = DEC_LUT[d + 1];
    } else {
        pos -= 1;
        buf[pos] = b'0' + v as u8;
    }
    pos
}

/// `u64::to_string()` without the heap `String`: renders into `buf`,
/// returns the populated tail slice.
pub fn u64_str(buf: &mut [u8; 20], v: u64) -> &str {
    let pos = digits_to_tail(buf, v);
    std::str::from_utf8(&buf[pos..]).expect("ascii")
}

/// `i64::to_string()` without the heap `String` (quota bytes are `i64`).
pub fn i64_str(buf: &mut [u8; 21], v: i64) -> &str {
    let mut pos = digits_to_tail(buf, v.unsigned_abs());
    if v < 0 {
        pos -= 1;
        buf[pos] = b'-';
    }
    std::str::from_utf8(&buf[pos..]).expect("ascii")
}

/// Lower-case hex of `bytes` into one preallocated `String`.
///
/// Replaces the `.map(|b| format!("{b:02x}")).collect()` shape, which heap-
/// allocates a 2-byte `String` per digest byte (16 for MD5, 32 for SHA-256)
/// before collect concatenates them.
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `chrono::DateTime<Utc>::format("%Y%m%dT%H%M%SZ")` for a whole-second
/// timestamp: the compact iCal/vCard UTC form `20260717T114714Z` (16 bytes)
/// written into `buf`.
///
/// This is the `DTSTAMP` / `REV` / `CREATED` / `LAST-MODIFIED` stamp emitted
/// per contact in every CardDAV vCard (`contact_to_vcard` / `generate_vcard`)
/// and per event on the calendar create path. chrono's `.format("%Y%m%dT%H%M%SZ")`
/// builds a `DelayedFormat` that re-parses the strftime spec (`StrftimeItems`)
/// and formats six zero-padded fields through `core::fmt` on every call — the
/// exact interpreter cost [`rfc3339_utc`] / [`rfc2822_utc`] were added to
/// remove, but neither covers this compact no-separator form.
///
/// Returns `None` when `secs` is outside the fixed-width range —
/// callers fall back to chrono.
pub fn compact_ical_utc(buf: &mut [u8; 16], secs: i64) -> Option<&str> {
    if !(0..=MAX_4DIGIT_YEAR_SECS).contains(&secs) {
        return None;
    }
    let (_days, y, m, d, hh, mm, ss) = split(secs);
    push4(buf, 0, y);
    push2(buf, 4, m);
    push2(buf, 6, d);
    buf[8] = b'T';
    push2(buf, 9, hh);
    push2(buf, 11, mm);
    push2(buf, 13, ss);
    buf[15] = b'Z';
    // SAFETY-free: every byte written above is ASCII.
    Some(std::str::from_utf8(&buf[..]).expect("ascii"))
}

/// `chrono::NaiveDate::format("%Y-%m-%d")` for a calendar date: the vCard
/// `BDAY` / ISO date form `2026-07-17` (10 bytes) written into `buf`.
///
/// The vCard emit path (`contact_to_vcard`) stamps `BDAY` per
/// contact-with-birthday, and `write!(…, "{}", date.format("%Y-%m-%d"))` runs
/// chrono's strftime interpreter and heap-allocates — the same interpreter cost
/// [`compact_ical_utc`] removed for the `REV` stamp (benches/ROUND19.md §V2:
/// 3→0 allocs). This is the date-only companion to that helper.
///
/// Takes the pre-split `year`/`month`/`day` (so `fmt` stays chrono-free off the
/// test path); callers read them via `chrono::Datelike`. Returns `None` when
/// `year` is outside the fixed-width 4-digit range — where chrono widens or
/// sign-prefixes `%Y` — so callers keep the chrono path as fallback.
pub fn compact_date(buf: &mut [u8; 10], year: i32, month: u32, day: u32) -> Option<&str> {
    if !(0..=9999).contains(&year) {
        return None;
    }
    push4(buf, 0, year as i64);
    buf[4] = b'-';
    push2(buf, 5, month);
    buf[7] = b'-';
    push2(buf, 8, day);
    Some(std::str::from_utf8(&buf[..]).expect("ascii"))
}

/// Append the upper-cased form of `s` to `buf` without a temporary `String`.
///
/// Byte-identical to `buf.push_str(&s.to_uppercase())` — same
/// `char::to_uppercase` expansion (incl. ß → SS, ﬀ → FF) — but writes straight
/// into the caller's buffer. The vCard emit path (`contact_to_vcard`,
/// `generate_vcard`) formats an `EMAIL`/`TEL`/`ADR` `TYPE=` token per line, and
/// the old `write!(…, "{}", ty.to_uppercase())` heap-allocated one throw-away
/// `String` per token per contact (benches/ROUND17.md §V1).
pub fn push_upper(buf: &mut String, s: &str) {
    for c in s.chars() {
        for u in c.to_uppercase() {
            buf.push(u);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    /// `hex_lower` must match the `format!("{b:02x}")`-per-byte shape it
    /// replaced, byte for byte.
    #[test]
    fn hex_lower_matches_format() {
        let cases: [&[u8]; 5] = [
            &[],
            &[0x00],
            &[0xff, 0x00, 0xab],
            &(0u8..=255).collect::<Vec<u8>>(),
            b"The quick brown fox",
        ];
        for bytes in cases {
            let reference: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex_lower(bytes), reference);
        }
    }

    /// `push_upper` must match `push_str(&s.to_uppercase())` byte for byte,
    /// including multi-char upper-casings (ß → SS) and dotless-i.
    #[test]
    fn push_upper_matches_to_uppercase() {
        let cases = [
            "", "home", "WORK", "Cell", "voice", "x-custom", "café", "straße", "ﬀ", "ı",
        ];
        for s in cases {
            let mut got = String::new();
            push_upper(&mut got, s);
            assert_eq!(got, s.to_uppercase(), "push_upper differs for {s:?}");
        }
    }

    /// Edge-heavy corpus: epoch, single-digit day (padding!), leap day,
    /// end-of-year, DST-irrelevant midsummer, far future, max in-range.
    const CASES: [i64; 12] = [
        0,
        1,
        86_399,
        86_400,
        951_782_400,   // 2000-02-29 (leap)
        1_120_176_000, // 2005-07-01 (day < 10 → chrono pads)
        1_752_753_434,
        2_147_483_647,
        4_102_444_799, // 2099-12-31 23:59:59
        7_258_118_400,
        250_000_000_000,
        MAX_4DIGIT_YEAR_SECS,
    ];

    #[test]
    fn rfc3339_matches_chrono() {
        for &secs in &CASES {
            let dt = Utc.timestamp_opt(secs, 0).unwrap();
            let mut buf = [0u8; 25];
            assert_eq!(
                rfc3339_utc(&mut buf, secs).expect("in range"),
                dt.to_rfc3339(),
                "secs={secs}"
            );
        }
    }

    #[test]
    fn rfc2822_matches_chrono() {
        for &secs in &CASES {
            let dt = Utc.timestamp_opt(secs, 0).unwrap();
            let mut buf = [0u8; 31];
            assert_eq!(
                rfc2822_utc(&mut buf, secs).expect("in range"),
                dt.to_rfc2822(),
                "secs={secs}"
            );
        }
    }

    #[test]
    fn compact_ical_matches_chrono() {
        for &secs in &CASES {
            let dt = Utc.timestamp_opt(secs, 0).unwrap();
            let mut buf = [0u8; 16];
            assert_eq!(
                compact_ical_utc(&mut buf, secs).expect("in range"),
                dt.format("%Y%m%dT%H%M%SZ").to_string(),
                "secs={secs}"
            );
        }
    }

    #[test]
    fn compact_date_matches_chrono() {
        use chrono::{Datelike, NaiveDate};
        // Padding (day/month < 10), leap day, min/max in-range 4-digit year,
        // 3-digit year (chrono zero-pads %Y to 4).
        let cases = [
            (2026, 7, 17),
            (2000, 2, 29),
            (2005, 7, 1),
            (1970, 1, 1),
            (9999, 12, 31),
            (1, 1, 1),
            (876, 5, 9),
        ];
        for (y, m, d) in cases {
            let date = NaiveDate::from_ymd_opt(y, m, d).unwrap();
            let mut buf = [0u8; 10];
            assert_eq!(
                compact_date(&mut buf, date.year(), date.month(), date.day()).expect("in range"),
                date.format("%Y-%m-%d").to_string(),
                "date={y}-{m}-{d}"
            );
        }
    }

    #[test]
    fn out_of_range_falls_back() {
        let mut b3 = [0u8; 25];
        let mut b2 = [0u8; 31];
        let mut bc = [0u8; 16];
        let mut bd = [0u8; 10];
        assert!(rfc3339_utc(&mut b3, -1).is_none());
        assert!(rfc2822_utc(&mut b2, -1).is_none());
        assert!(compact_ical_utc(&mut bc, -1).is_none());
        assert!(compact_date(&mut bd, -1, 1, 1).is_none());
        assert!(compact_date(&mut bd, 10000, 1, 1).is_none());
        assert!(rfc3339_utc(&mut b3, MAX_4DIGIT_YEAR_SECS + 1).is_none());
        assert!(compact_ical_utc(&mut bc, MAX_4DIGIT_YEAR_SECS + 1).is_none());
    }

    #[test]
    fn ints_match_std() {
        let mut b = [0u8; 20];
        for v in [0u64, 1, 9, 10, 42, 1024, u64::MAX] {
            assert_eq!(u64_str(&mut b, v), v.to_string());
        }
        let mut b = [0u8; 21];
        for v in [0i64, -1, 42, -1024, i64::MIN, i64::MAX] {
            assert_eq!(i64_str(&mut b, v), v.to_string());
        }
    }

    /// Exhaustive-ish sweep: every 6h13m across 60 years — catches any
    /// weekday / month-boundary drift against chrono.
    #[test]
    fn sweep_matches_chrono() {
        let mut secs: i64 = 0;
        while secs < 60 * 366 * 86_400 {
            let dt = Utc.timestamp_opt(secs, 0).unwrap();
            let mut b3 = [0u8; 25];
            let mut b2 = [0u8; 31];
            let mut bc = [0u8; 16];
            assert_eq!(rfc3339_utc(&mut b3, secs).unwrap(), dt.to_rfc3339());
            assert_eq!(rfc2822_utc(&mut b2, secs).unwrap(), dt.to_rfc2822());
            assert_eq!(
                compact_ical_utc(&mut bc, secs).unwrap(),
                dt.format("%Y%m%dT%H%M%SZ").to_string()
            );
            secs += 22_380; // 6h13m — walks through all times of day + weekdays
        }
    }
}
