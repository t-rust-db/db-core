// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's date and time functions: `date`, `time`, `datetime`, `julianday`, `unixepoch`, `strftime`, `timediff` (#651), with sqlite3 3.53.4's semantics.
//!
//! A port of sqlite3's `date.c`: every value is a `DateTime` holding a
//! julian day number in milliseconds (`ijd`) and/or the broken-down
//! Y-M-D and h:m:s fields, each with a validity flag, converted lazily by
//! `compute_jd`/`compute_ymd`/`compute_hms` exactly as sqlite3
//! does. Any parse error or out-of-range value yields NULL.
//!
//! One deliberate approximation: `localtime` and `utc` treat the local
//! zone as UTC (sqlite3 run under `TZ=UTC` behaves identically). `'now'`
//! is one instant per statement: the row VM passes it
//! ([`super::call_at`]).
//!
//! Integer arithmetic on calendar fields uses `std::num::Wrapping`: every field
//! is bounded (years -4713..=9999 before any formula runs, julian days
//! below 5.4e6) so wrapping never occurs; julian-day accumulation from
//! modifiers saturates instead, and the result is range-checked last.

use std::num::Wrapping as W;
use std::rc::Rc;

use super::{FunctionError, ScalarFn};
use crate::value::Value;

/// Milliseconds in one day.
const DAY_MS: i64 = 86_400_000;
/// Half a day in milliseconds: julian days start at noon.
const HALF_DAY_MS: i64 = 43_200_000;
/// The julian day (times 86400000) of 1970-01-01 00:00:00.
const UNIX_EPOCH_JD_MS: i64 = 210_866_760_000_000;
/// The largest valid `ijd`: 9999-12-31 23:59:59.999.
const MAX_JD_MS: i64 = 464_269_060_799_999;

/// One date/time value, sqlite3's `struct DateTime`.
#[derive(Debug, Clone, Copy, Default)]
struct DateTime {
    /// The julian day number times 86400000.
    ijd: i64,
    /// Year.
    y: i64,
    /// Month, 1..=12.
    mo: i64,
    /// Day of month.
    d: i64,
    /// Hour.
    h: i64,
    /// Minute.
    mi: i64,
    /// Timezone offset in minutes.
    tz: i64,
    /// Seconds, with fraction.
    s: f64,
    /// `ijd` is valid.
    valid_jd: bool,
    /// `y`/`mo`/`d` are valid.
    valid_ymd: bool,
    /// `h`/`mi`/`s` are valid.
    valid_hms: bool,
    /// Days to subtract for the `floor` modifier.
    n_floor: i64,
    /// A raw numeric value is stored in `s`.
    raw_s: bool,
    /// An overflow or invalid value has occurred.
    is_error: bool,
    /// Display subsecond precision.
    use_subsec: bool,
    /// The time is known to be UTC.
    is_utc: bool,
    /// The time is known to be local time.
    is_local: bool,
}

/// The function named `name` (lowercase) at `arity`, if this family has
/// it. sqlite3 registers all but `timediff` as variadic (`-1`), so even
/// `strftime()` with no format is accepted and returns NULL.
pub(super) fn lookup(name: &str, arity: usize) -> Option<ScalarFn> {
    let f: ScalarFn = match (name, arity) {
        ("date", _) => |a| date(a, now()),
        ("time", _) => |a| time(a, now()),
        ("datetime", _) => |a| datetime(a, now()),
        ("julianday", _) => |a| julianday(a, now()),
        ("unixepoch", _) => |a| unixepoch(a, now()),
        ("strftime", _) => |a| strftime(a, now()),
        ("timediff", 2) => |a| timediff(a, now()),
        _ => return None,
    };
    Some(f)
}

/// The function named `name` (lowercase) at `arity`, taking `'now'` as an
/// argument: the row VM passes one instant for the whole statement, as
/// sqlite3 does (`sqlite3StmtCurrentTime`).
pub(super) fn lookup_at(name: &str, arity: usize) -> Option<super::ScalarAtFn> {
    let f: super::ScalarAtFn = match (name, arity) {
        ("date", _) => date,
        ("time", _) => time,
        ("datetime", _) => datetime,
        ("julianday", _) => julianday,
        ("unixepoch", _) => unixepoch,
        ("strftime", _) => strftime,
        ("timediff", 2) => timediff,
        _ => return None,
    };
    Some(f)
}

/// The clock, for a call outside a statement.
fn now() -> i64 {
    use crate::clock::{Clock, SystemClock};
    SystemClock.now_ns()
}

// ---------------------------------------------------------------------
// Small helpers: C-string style byte access and numeric conversions.
// ---------------------------------------------------------------------

/// The byte at `i`, or 0 past the end -- the C NUL terminator date.c's
/// pointer walks rely on.
fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

/// `z` advanced by `n` bytes (empty past the end), date.c's `z += n`.
fn adv(z: &[u8], n: usize) -> &[u8] {
    z.get(n..).unwrap_or(&[])
}

/// sqlite3's `sqlite3Isspace`: ASCII space, `\t`, `\n`, `\v`, `\f`, `\r`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The decimal value of an ASCII digit.
fn digit(c: u8) -> Option<i64> {
    char::from(c).to_digit(10).map(i64::from)
}

/// C's `(sqlite3_int64)x`: truncation toward zero.
#[allow(
    clippy::cast_possible_truncation,
    reason = "mirrors date.c's (sqlite3_int64) casts; Rust's `as` saturates where C would be undefined, and every result is range-checked by valid_julian_day afterwards"
)]
fn f2i(x: f64) -> i64 {
    x as i64
}

/// Whether `ijd` is within 0000-...-9999 range sqlite3 accepts
/// (`validJulianDay`).
fn valid_julian_day(ijd: i64) -> bool {
    (0..=MAX_JD_MS).contains(&ijd)
}

/// sqlite3's `sqlite3AtoF` validity test plus value: `Some(r)` when all of
/// `z` (modulo surrounding whitespace) is `[+-]digits[.digits][e[+-]digits]`
/// with at least one digit, else `None`.
fn atof(z: &[u8]) -> Option<f64> {
    let start = z.iter().position(|&c| !is_space(c))?;
    let end = z.iter().rposition(|&c| !is_space(c))?;
    let t = z.get(start..=end)?;
    let mut i = 0usize;
    if matches!(at(t, 0), b'+' | b'-') {
        i = 1;
    }
    let mut n_digit = 0usize;
    while at(t, i).is_ascii_digit() {
        i = i.saturating_add(1);
        n_digit = n_digit.saturating_add(1);
    }
    if at(t, i) == b'.' {
        i = i.saturating_add(1);
        while at(t, i).is_ascii_digit() {
            i = i.saturating_add(1);
            n_digit = n_digit.saturating_add(1);
        }
    }
    if n_digit == 0 {
        return None;
    }
    if matches!(at(t, i), b'e' | b'E') {
        i = i.saturating_add(1);
        if matches!(at(t, i), b'+' | b'-') {
            i = i.saturating_add(1);
        }
        if !at(t, i).is_ascii_digit() {
            return None;
        }
        while at(t, i).is_ascii_digit() {
            i = i.saturating_add(1);
        }
    }
    if i != t.len() {
        return None;
    }
    std::str::from_utf8(t).ok()?.parse::<f64>().ok()
}

/// One `getDigits` field: digit count, minimum, maximum, and the
/// separator that must follow (0 for the last field).
type DigitSpec = (usize, i64, i64, u8);

/// `HH:MM` as date.c's `"20c:20e"`.
const HH_MM: [DigitSpec; 2] = [(2, 0, 24, b':'), (2, 0, 59, 0)];

/// sqlite3's `getDigits`: convert fixed-width digit fields of `z`
/// according to `specs`, stopping at the first one that fails. Returns
/// the fields converted so far.
fn get_digits(z: &[u8], specs: &[DigitSpec]) -> Vec<i64> {
    let mut out = Vec::with_capacity(specs.len());
    let mut z = z;
    for &(n, min, max, next) in specs {
        let mut val = W(0i64);
        for _ in 0..n {
            let Some(dv) = digit(at(z, 0)) else {
                return out;
            };
            val = val * W(10) + W(dv);
            z = adv(z, 1);
        }
        if val.0 < min || val.0 > max || (next != 0 && next != at(z, 0)) {
            return out;
        }
        out.push(val.0);
        z = adv(z, 1);
    }
    out
}

/// The text of a SQL value as `sqlite3_value_text` gives it (`None` for
/// NULL), cut at an embedded NUL as date.c's C strings are.
fn arg_text(v: &Value) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = match v {
        Value::Null => return None,
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Real(r) => crate::value::format_real(*r).into_bytes(),
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::Blob(b) => b.to_vec(),
    };
    Some(bytes.split(|&c| c == 0).next().unwrap_or(&[]).to_vec())
}

// ---------------------------------------------------------------------
// The DateTime state machine (date.c's compute*/parse* functions).
// ---------------------------------------------------------------------

/// Put `p` into its error state (`datetimeError`).
fn set_error(p: &mut DateTime) {
    *p = DateTime {
        is_error: true,
        ..DateTime::default()
    };
}

/// Clear the YMD and HMS fields and the timezone (`clearYMD_HMS_TZ`).
fn clear_ymd_hms_tz(p: &mut DateTime) {
    p.valid_ymd = false;
    p.valid_hms = false;
    p.tz = 0;
}

/// Y-M-D h:m:s to julian day (`computeJD`, Meeus p. 61), Gregorian
/// calendar throughout; a missing date means 2000-01-01.
fn compute_jd(p: &mut DateTime) {
    if p.valid_jd {
        return;
    }
    let (y, mo, d) = if p.valid_ymd {
        (p.y, p.mo, p.d)
    } else {
        (2000, 1, 1)
    };
    if !(-4713..=9999).contains(&y) || p.raw_s {
        set_error(p);
        return;
    }
    let (y, mo) = if mo <= 2 {
        (W(y) - W(1), W(mo) + W(12))
    } else {
        (W(y), W(mo))
    };
    // Meeus's century terms, offset by 4800 years so the divisions floor
    // for negative years too (sqlite3 3.53: -0100 is not a leap year).
    let a = W((y + W(4800)).0 / 100);
    let b = W(38) - a + W(a.0 / 4);
    let x1 = W((W(36525) * (y + W(4716))).0 / 100);
    let x2 = W((W(306_001) * (mo + W(1))).0 / 10000);
    let days = x1 + x2 + W(d) + b;
    p.ijd = f2i((days.0 as f64 - 1524.5) * 86_400_000.0);
    p.valid_jd = true;
    if p.valid_hms {
        let hm = W(p.h) * W(3_600_000) + W(p.mi) * W(60_000);
        p.ijd = p
            .ijd
            .saturating_add(hm.0)
            .saturating_add(f2i(p.s * 1000.0 + 0.5));
        if p.tz != 0 {
            p.ijd = p.ijd.saturating_sub((W(p.tz) * W(60_000)).0);
            p.valid_ymd = false;
            p.valid_hms = false;
            p.tz = 0;
            p.is_utc = true;
            p.is_local = false;
        }
    }
}

/// The number of days `floor` would subtract to undo day-of-month
/// overflow (`computeFloor`): 2023-02-31 has an `n_floor` of 3.
fn compute_floor(p: &mut DateTime) {
    p.n_floor = if p.d <= 28 || matches!(p.mo, 1 | 3 | 5 | 7 | 8 | 10 | 12) {
        0
    } else if p.mo != 2 {
        i64::from(p.d == 31)
    } else if p.y % 4 != 0 || (p.y % 100 == 0 && p.y % 400 != 0) {
        (W(p.d) - W(28)).0
    } else {
        (W(p.d) - W(29)).0
    };
}

/// Julian day to Y-M-D (`computeYMD`); an out-of-range day is an error.
fn compute_ymd(p: &mut DateTime) {
    if p.valid_ymd {
        return;
    }
    if !p.valid_jd {
        p.y = 2000;
        p.mo = 1;
        p.d = 1;
    } else if !valid_julian_day(p.ijd) {
        set_error(p);
        return;
    } else {
        let z = W((W(p.ijd) + W(HALF_DAY_MS)).0 / DAY_MS);
        // As in compute_jd, offset so the divisions floor before 400 AD.
        let alpha = W(f2i((z.0 as f64 + 32044.75) / 36524.25)) - W(52);
        let a = z + W(1) + alpha - W((alpha + W(100)).0 / 4) + W(25);
        let b = a + W(1524);
        let c = W(f2i((b.0 as f64 - 122.1) / 365.25));
        let d = W((W(36525) * (c & W(32767))).0 / 100);
        let e = W(f2i((b - d).0 as f64 / 30.6001));
        let x1 = W(f2i(30.6001 * e.0 as f64));
        p.d = (b - d - x1).0;
        p.mo = if e.0 < 14 {
            (e - W(1)).0
        } else {
            (e - W(13)).0
        };
        p.y = if p.mo > 2 {
            (c - W(4716)).0
        } else {
            (c - W(4715)).0
        };
    }
    p.valid_ymd = true;
}

/// Julian day to h:m:s (`computeHMS`).
fn compute_hms(p: &mut DateTime) {
    if p.valid_hms {
        return;
    }
    compute_jd(p);
    let day_ms = (W(p.ijd) + W(HALF_DAY_MS)).0 % DAY_MS;
    p.s = (day_ms % 60_000) as f64 / 1000.0;
    let day_min = day_ms / 60_000;
    p.mi = day_min % 60;
    p.h = day_min / 60;
    p.raw_s = false;
    p.valid_hms = true;
}

/// Both [`compute_ymd`] and [`compute_hms`].
fn compute_ymd_hms(p: &mut DateTime) {
    compute_ymd(p);
    compute_hms(p);
}

/// A trailing `[+-]HH:MM` or `Z` (`parseTimezone`); returns true on a
/// parse error. No zone at all is fine.
fn parse_timezone(z: &[u8], p: &mut DateTime) -> bool {
    let mut z = z;
    while is_space(at(z, 0)) {
        z = adv(z, 1);
    }
    p.tz = 0;
    let sgn = match at(z, 0) {
        b'-' => -1,
        b'+' => 1,
        b'Z' | b'z' => {
            p.is_local = false;
            p.is_utc = true;
            return trailing_junk(adv(z, 1));
        }
        c => return c != 0,
    };
    z = adv(z, 1);
    let &[hr, mn] = get_digits(z, &[(2, 0, 14, b':'), (2, 0, 59, 0)]).as_slice() else {
        return true;
    };
    p.tz = (W(sgn) * (W(mn) + W(hr) * W(60))).0;
    trailing_junk(adv(z, 5))
}

/// Whether anything but whitespace remains in `z`.
fn trailing_junk(z: &[u8]) -> bool {
    z.iter().any(|&c| !is_space(c))
}

/// `HH:MM`, `HH:MM:SS` or `HH:MM:SS.FFFF` plus an optional zone
/// (`parseHhMmSs`); returns true on a parse error.
fn parse_hhmmss(z: &[u8], p: &mut DateTime) -> bool {
    let &[h, m] = get_digits(z, &HH_MM).as_slice() else {
        return true;
    };
    let mut z = adv(z, 5);
    let mut ms = 0.0;
    let s = if at(z, 0) == b':' {
        z = adv(z, 1);
        let &[s] = get_digits(z, &[(2, 0, 59, 0)]).as_slice() else {
            return true;
        };
        z = adv(z, 2);
        if at(z, 0) == b'.' && at(z, 1).is_ascii_digit() {
            let mut scale = 1.0;
            z = adv(z, 1);
            while let Some(dv) = digit(at(z, 0)) {
                ms = ms * 10.0 + dv as f64;
                scale *= 10.0;
                z = adv(z, 1);
            }
            ms /= scale;
            // sqlite3 3.53 truncates to whole milliseconds' range so a
            // fraction never rounds up into the next second.
            if ms > 0.999 {
                ms = 0.999;
            }
        }
        s
    } else {
        0
    };
    p.valid_jd = false;
    p.raw_s = false;
    p.valid_hms = true;
    p.h = h;
    p.mi = m;
    p.s = s as f64 + ms;
    parse_timezone(z, p)
}

/// `[-]YYYY-MM-DD` optionally followed (after spaces or `T`) by a time
/// (`parseYyyyMmDd`); returns true on a parse error.
fn parse_yyyymmdd(z: &[u8], p: &mut DateTime) -> bool {
    let (neg, mut z) = if at(z, 0) == b'-' {
        (true, adv(z, 1))
    } else {
        (false, z)
    };
    let &[y, m, d] =
        get_digits(z, &[(4, 0, 14712, b'-'), (2, 1, 12, b'-'), (2, 1, 31, 0)]).as_slice()
    else {
        return true;
    };
    z = adv(z, 10);
    while is_space(at(z, 0)) || at(z, 0) == b'T' {
        z = adv(z, 1);
    }
    if !parse_hhmmss(z, p) {
        // got the time
    } else if z.is_empty() {
        p.valid_hms = false;
    } else {
        return true;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { (-W(y)).0 } else { y };
    p.mo = m;
    p.d = d;
    compute_floor(p);
    if p.tz != 0 {
        compute_jd(p);
    }
    false
}

/// The current time as a julian day in ms (`sqlite3StmtCurrentTime`).
fn now_jd_ms(now_ns: i64) -> i64 {
    (now_ns / 1_000_000).saturating_add(UNIX_EPOCH_JD_MS)
}

/// Set `p` to the current time (`setDateTimeToCurrent`).
fn set_to_current(p: &mut DateTime, now_ns: i64) {
    p.ijd = now_jd_ms(now_ns);
    p.valid_jd = true;
    p.is_utc = true;
    p.is_local = false;
    clear_ymd_hms_tz(p);
}

/// A number that may be a julian day or (with a modifier) unix time
/// (`setRawDateNumber`).
fn set_raw_date_number(p: &mut DateTime, r: f64) {
    p.s = r;
    p.raw_s = true;
    if (0.0..5_373_484.5).contains(&r) {
        p.ijd = f2i(r * 86_400_000.0 + 0.5);
        p.valid_jd = true;
    }
}

/// Parse a time-value string (`parseDateOrTime`): a date and/or time,
/// `now`, a number, or `subsec`. Returns true on a parse error.
fn parse_date_or_time(z: &[u8], p: &mut DateTime, now_ns: i64) -> bool {
    if !parse_yyyymmdd(z, p) || !parse_hhmmss(z, p) {
        return false;
    }
    if z.eq_ignore_ascii_case(b"now") {
        set_to_current(p, now_ns);
        return false;
    }
    if let Some(r) = atof(z) {
        set_raw_date_number(p, r);
        return false;
    }
    if z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond") {
        p.use_subsec = true;
        set_to_current(p, now_ns);
        return false;
    }
    true
}

/// A raw number read as unix time when it is not a julian day
/// (`autoAdjustDate`, the `auto` modifier).
fn auto_adjust_date(p: &mut DateTime) {
    if !p.raw_s || p.valid_jd {
        p.raw_s = false;
    } else if (-210_866_760_000.0..=253_402_300_799.0).contains(&p.s) {
        let r = p.s * 1000.0 + UNIX_EPOCH_JD_MS as f64;
        clear_ymd_hms_tz(p);
        p.ijd = f2i(r + 0.5);
        p.valid_jd = true;
        p.raw_s = false;
    }
}

/// Shift a UTC `p` to local time (`toLocaltime`). The local zone is
/// taken to be UTC, so this only renormalises `p` the way sqlite3 does
/// under `TZ=UTC` -- including the year mapping sqlite3 applies outside
/// 1970..2038, whose error paths land on 2000-01-01 or 0000-01-01.
fn to_localtime(p: &mut DateTime) {
    compute_jd(p);
    // localtime_r only covers 1970..2038: map other years to one with the
    // same leap-ness (2000 + Y%4), convert, and map back.
    let (year_diff, t_jd) = if (210_866_760_000_000..=213_014_145_600_000).contains(&p.ijd) {
        (0, p.ijd)
    } else {
        let mut x = *p;
        compute_ymd_hms(&mut x);
        let diff = (W(2000) + W(x.y % 4) - W(x.y)).0;
        x.y = (W(x.y) + W(diff)).0;
        x.valid_jd = false;
        compute_jd(&mut x);
        (diff, x.ijd)
    };
    // The UTC broken-down time of whole second `t_jd / 1000`.
    let mut tm = DateTime {
        ijd: (W(t_jd / 1000) * W(1000)).0,
        valid_jd: true,
        ..DateTime::default()
    };
    compute_ymd_hms(&mut tm);
    p.y = (W(tm.y) - W(year_diff)).0;
    p.mo = tm.mo;
    p.d = tm.d;
    p.h = tm.h;
    p.mi = tm.mi;
    p.s = tm.s.trunc() + (p.ijd % 1000) as f64 * 0.001;
    p.valid_ymd = true;
    p.valid_hms = true;
    p.valid_jd = false;
    p.raw_s = false;
    p.tz = 0;
    p.is_error = false;
}

/// The `NNN units` modifiers: name, limit on `|NNN|`, and seconds per
/// unit -- sqlite3's `aXformType`, whose limits and factors are `float`s.
const XFORMS: [(&[u8], f32, f32); 6] = [
    (b"second", 4.6427e+14, 1.0),
    (b"minute", 7.7379e+12, 60.0),
    (b"hour", 1.2897e+11, 3600.0),
    (b"day", 5_373_485.0, 86400.0),
    (b"month", 176_546.0, 30.0 * 86400.0),
    (b"year", 14713.0, 365.0 * 86400.0),
];

/// Normalise a month that has run past 1..=12 into the year.
fn carry_months(p: &mut DateTime) {
    let x = if p.mo > 0 {
        (W(p.mo) - W(1)).0 / 12
    } else {
        (W(p.mo) - W(12)).0 / 12
    };
    p.y = (W(p.y) + W(x)).0;
    p.mo = (W(p.mo) - W(x) * W(12)).0;
}

/// Apply one modifier `z` (argument `idx`, 1-based) to `p`
/// (`parseModifier`); returns true if it is invalid.
fn parse_modifier(z: &[u8], p: &mut DateTime, idx: usize) -> bool {
    match at(z, 0).to_ascii_lowercase() {
        b'a' if z.eq_ignore_ascii_case(b"auto") => {
            // Only valid directly after the time value.
            if idx > 1 {
                return true;
            }
            auto_adjust_date(p);
            false
        }
        b'c' if z.eq_ignore_ascii_case(b"ceiling") => {
            // Day-of-month overflow rolls forward: the default.
            compute_jd(p);
            clear_ymd_hms_tz(p);
            p.n_floor = 0;
            false
        }
        b'f' if z.eq_ignore_ascii_case(b"floor") => {
            compute_jd(p);
            p.ijd = p.ijd.saturating_sub((W(p.n_floor) * W(DAY_MS)).0);
            clear_ymd_hms_tz(p);
            false
        }
        b'j' if z.eq_ignore_ascii_case(b"julianday") => {
            if idx > 1 {
                return true;
            }
            if p.valid_jd && p.raw_s {
                p.raw_s = false;
                return false;
            }
            true
        }
        b'l' if z.eq_ignore_ascii_case(b"localtime") => {
            if !p.is_local {
                to_localtime(p);
            }
            p.is_utc = false;
            p.is_local = true;
            false
        }
        b'u' if z.eq_ignore_ascii_case(b"unixepoch") && p.raw_s => {
            if idx > 1 {
                return true;
            }
            let r = p.s * 1000.0 + UNIX_EPOCH_JD_MS as f64;
            if !(0.0..464_269_060_800_000.0).contains(&r) {
                return true;
            }
            clear_ymd_hms_tz(p);
            p.ijd = f2i(r + 0.5);
            p.valid_jd = true;
            p.raw_s = false;
            false
        }
        b'u' if z.eq_ignore_ascii_case(b"utc") => {
            if !p.is_utc {
                modifier_utc(p);
            }
            false
        }
        b'w' => modifier_weekday(z, p),
        b's' => modifier_start_of(z, p),
        b'+' | b'-' | b'0'..=b'9' => modifier_numeric(z, p),
        _ => true,
    }
}

/// `utc`: find the UTC time whose local time is `p`, by sqlite3's
/// up-to-four-guess iteration; resets every other flag as sqlite3 does.
fn modifier_utc(p: &mut DateTime) {
    compute_jd(p);
    let orig = p.ijd;
    let mut guess = orig;
    let mut err = 0i64;
    let mut cnt = 0u8;
    loop {
        guess = guess.saturating_sub(err);
        let mut new = DateTime {
            ijd: guess,
            valid_jd: true,
            ..DateTime::default()
        };
        to_localtime(&mut new);
        compute_jd(&mut new);
        err = new.ijd.saturating_sub(orig);
        if err == 0 || cnt >= 3 {
            break;
        }
        cnt = cnt.saturating_add(1);
    }
    *p = DateTime {
        ijd: guess,
        valid_jd: true,
        is_utc: true,
        ..DateTime::default()
    };
}

/// `weekday N`: advance to the next day that is weekday N (0 = Sunday),
/// a no-op if already there.
fn modifier_weekday(z: &[u8], p: &mut DateTime) -> bool {
    let is_prefix = z
        .get(..8)
        .is_some_and(|h| h.eq_ignore_ascii_case(b"weekday "));
    let Some(r) = is_prefix.then(|| atof(adv(z, 8))).flatten() else {
        return true;
    };
    if !(0.0..7.0).contains(&r) || r.fract() != 0.0 {
        return true;
    }
    let n = f2i(r);
    compute_ymd_hms(p);
    p.tz = 0;
    p.valid_jd = false;
    compute_jd(p);
    let mut wd = (W(p.ijd) + W(129_600_000)).0 / DAY_MS % 7;
    if wd > n {
        wd = (W(wd) - W(7)).0;
    }
    p.ijd = p
        .ijd
        .saturating_add((W(n) - W(wd)).0.saturating_mul(DAY_MS));
    clear_ymd_hms_tz(p);
    false
}

/// `start of month|year|day`, and `subsec`/`subsecond`.
fn modifier_start_of(z: &[u8], p: &mut DateTime) -> bool {
    let is_start = z
        .get(..9)
        .is_some_and(|h| h.eq_ignore_ascii_case(b"start of "));
    if !is_start {
        if z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond") {
            p.use_subsec = true;
            return false;
        }
        return true;
    }
    if !p.valid_jd && !p.valid_ymd && !p.valid_hms {
        return true;
    }
    let unit = adv(z, 9);
    compute_ymd(p);
    p.valid_hms = true;
    p.h = 0;
    p.mi = 0;
    p.s = 0.0;
    p.raw_s = false;
    p.tz = 0;
    p.valid_jd = false;
    if unit.eq_ignore_ascii_case(b"month") {
        p.d = 1;
        false
    } else if unit.eq_ignore_ascii_case(b"year") {
        p.mo = 1;
        p.d = 1;
        false
    } else {
        !unit.eq_ignore_ascii_case(b"day")
    }
}

/// The modifiers starting with a sign or digit: `±YYYY-MM-DD[ HH:MM...]`,
/// `±HH:MM[:SS[.FFF]]`, and `±NNN units`.
fn modifier_numeric(z: &[u8], p: &mut DateTime) -> bool {
    let z0 = at(z, 0);
    // Find the end of the leading number: a ':', a space, or the '-'
    // after a 4- or 5-digit year.
    let mut n = 1usize;
    loop {
        let c = at(z, n);
        if c == 0 || c == b':' || is_space(c) {
            break;
        }
        if c == b'-'
            && ((n == 5 && get_digits(adv(z, 1), &[(4, 0, 14712, 0)]).len() == 1)
                || (n == 6 && get_digits(adv(z, 1), &[(5, 0, 14712, 0)]).len() == 1))
        {
            break;
        }
        n = n.saturating_add(1);
    }
    let Some(r) = z.get(..n).and_then(atof) else {
        return true;
    };
    let mut z = z;
    let mut z2 = z;
    if at(z, n) == b'-' {
        // (+|-)YYYY-MM-DD: add or subtract years, months (0-11) and
        // days (0-30).
        if z0 != b'+' && z0 != b'-' {
            return true;
        }
        let ymd = if n == 5 {
            get_digits(
                adv(z, 1),
                &[(4, 0, 14712, b'-'), (2, 0, 12, b'-'), (2, 0, 31, 0)],
            )
        } else {
            let v = get_digits(
                adv(z, 1),
                &[(5, 0, 14712, b'-'), (2, 0, 12, b'-'), (2, 0, 31, 0)],
            );
            z = adv(z, 1);
            v
        };
        let &[y, m, d] = ymd.as_slice() else {
            return true;
        };
        if m >= 12 || d >= 31 {
            return true;
        }
        compute_ymd_hms(p);
        p.valid_jd = false;
        let d = if z0 == b'-' {
            p.y = (W(p.y) - W(y)).0;
            p.mo = (W(p.mo) - W(m)).0;
            (-W(d)).0
        } else {
            p.y = (W(p.y) + W(y)).0;
            p.mo = (W(p.mo) + W(m)).0;
            d
        };
        carry_months(p);
        compute_floor(p);
        compute_jd(p);
        p.valid_hms = false;
        p.valid_ymd = false;
        p.ijd = p.ijd.saturating_add(d.saturating_mul(DAY_MS));
        if at(z, 11) == 0 {
            return false;
        }
        if is_space(at(z, 11)) && get_digits(adv(z, 12), &HH_MM).len() == 2 {
            z2 = adv(z, 12);
            n = 2;
        } else {
            return true;
        }
    }
    if at(z2, n) == b':' {
        // (+|-)HH:MM:SS.FFF: add or subtract a time of day.
        if !at(z2, 0).is_ascii_digit() {
            z2 = adv(z2, 1);
        }
        let mut tx = DateTime::default();
        if parse_hhmmss(z2, &mut tx) {
            return true;
        }
        compute_jd(&mut tx);
        tx.ijd = tx.ijd.saturating_sub(HALF_DAY_MS);
        let day = tx.ijd / DAY_MS;
        tx.ijd = tx.ijd.saturating_sub(day.saturating_mul(DAY_MS));
        if z0 == b'-' {
            tx.ijd = tx.ijd.saturating_neg();
        }
        compute_jd(p);
        clear_ymd_hms_tz(p);
        p.ijd = p.ijd.saturating_add(tx.ijd);
        return false;
    }
    // NNN units.
    let mut unit = adv(z, n);
    while is_space(at(unit, 0)) {
        unit = adv(unit, 1);
    }
    if !(3..=10).contains(&unit.len()) {
        return true;
    }
    if unit.last().is_some_and(|c| c.eq_ignore_ascii_case(&b's')) {
        unit = unit.get(..unit.len().saturating_sub(1)).unwrap_or(&[]);
    }
    compute_jd(p);
    let rounder = if r < 0.0 { -0.5 } else { 0.5 };
    p.n_floor = 0;
    let mut rc = true;
    for (i, &(name, limit, xform)) in XFORMS.iter().enumerate() {
        let limit = f64::from(limit);
        if unit.eq_ignore_ascii_case(name) && r > -limit && r < limit {
            let mut r = r;
            if i == 4 || i == 5 {
                // Months and years move the calendar fields, then any
                // fraction is added as 30- or 365-day units.
                compute_ymd_hms(p);
                if i == 4 {
                    p.mo = (W(p.mo) + W(f2i(r))).0;
                    carry_months(p);
                } else {
                    p.y = (W(p.y) + W(f2i(r))).0;
                }
                compute_floor(p);
                p.valid_jd = false;
                r -= r.trunc();
            }
            compute_jd(p);
            p.ijd = p
                .ijd
                .saturating_add(f2i(r * 1000.0 * f64::from(xform) + rounder));
            rc = false;
            break;
        }
    }
    clear_ymd_hms_tz(p);
    rc
}

/// Parse a time value and its modifiers (`isDate`); `None` means NULL.
/// No arguments at all means `'now'`.
fn is_date(args: &[Value], now_ns: i64) -> Option<DateTime> {
    let mut p = DateTime::default();
    let Some(first) = args.first() else {
        set_to_current(&mut p, now_ns);
        return Some(p);
    };
    match first {
        Value::Integer(i) => set_raw_date_number(&mut p, *i as f64),
        Value::Real(r) => set_raw_date_number(&mut p, *r),
        other => {
            let z = arg_text(other)?;
            if parse_date_or_time(&z, &mut p, now_ns) {
                return None;
            }
        }
    }
    for (idx, m) in args.iter().enumerate().skip(1) {
        let z = arg_text(m)?;
        if parse_modifier(&z, &mut p, idx) {
            return None;
        }
    }
    compute_jd(&mut p);
    if p.is_error || !valid_julian_day(p.ijd) {
        return None;
    }
    if args.len() == 1 && p.valid_ymd && p.d > 28 {
        // Normalise an overflowing YYYY-MM-DD: 2023-02-31 -> 2023-03-03.
        p.valid_ymd = false;
    }
    Some(p)
}

// ---------------------------------------------------------------------
// Output formatting.
// ---------------------------------------------------------------------

/// A text result.
fn text(s: String) -> Value {
    Value::Text(Rc::from(s))
}

/// `YYYY-MM-DD`, with a leading `-` for negative years.
fn ymd_string(p: &DateTime) -> String {
    let sign = if p.y < 0 { "-" } else { "" };
    format!("{sign}{:04}-{:02}-{:02}", p.y.abs(), p.mo, p.d)
}

/// `HH:MM:SS`, or `HH:MM:SS.SSS` with `subsec`.
fn hms_string(p: &DateTime) -> String {
    if p.use_subsec {
        let ms = f2i(1000.0 * p.s + 0.5);
        format!("{:02}:{:02}:{:02}.{:03}", p.h, p.mi, ms / 1000, ms % 1000)
    } else {
        format!("{:02}:{:02}:{:02}", p.h, p.mi, f2i(p.s))
    }
}

/// The decimal digits of `|x|` to 20 significant figures and the
/// exponent of the first: roughly what sqlite3's `sqlite3FpDecode`
/// produces before it rounds.
fn decimal_digits(x: f64) -> (Vec<u8>, i64) {
    let sci = format!("{:.19e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    (digits, exp.parse().unwrap_or(0))
}

/// Round `digits` (exponent `exp`) half-up, as sqlite3's printf does, to
/// `keep` significant digits (capped at 16); returns the new digits and
/// exponent. `keep <= 0` rounds to zero or to one unit at that place.
fn round_half_up(digits: &[u8], exp: i64, keep: i64) -> (Vec<u8>, i64) {
    let keep = keep.min(16);
    let Ok(k) = usize::try_from(keep) else {
        return (vec![b'0'], exp);
    };
    let mut out: Vec<u8> = digits.iter().take(k).copied().collect();
    if at(digits, k) >= b'5' {
        let mut carry = true;
        for c in out.iter_mut().rev() {
            if *c == b'9' {
                *c = b'0';
            } else {
                *c = c.saturating_add(1);
                carry = false;
                break;
            }
        }
        if carry {
            out.insert(0, b'1');
            return (out, exp.saturating_add(1));
        }
    }
    if out.is_empty() {
        out.push(b'0');
    }
    (out, exp)
}

/// sqlite3's `%.<prec>f`.
fn fmt_fixed(x: f64, prec: usize) -> String {
    let (digits, exp) = decimal_digits(x);
    let prec_i = i64::try_from(prec).unwrap_or(0);
    let keep = exp.saturating_add(1).saturating_add(prec_i);
    let (mut d, mut e) = round_half_up(&digits, exp, keep);
    if x == 0.0 || d.iter().all(|&c| c == b'0') {
        d = vec![b'0'];
        e = 0;
    }
    // Place the digits: integer part, then `prec` fraction digits.
    let mut int_part = String::new();
    let mut frac = String::new();
    let e_plus = e.saturating_add(1);
    for pos in 0..e_plus.max(1) {
        let c = usize::try_from(pos)
            .ok()
            .and_then(|i| d.get(i))
            .copied()
            .filter(|_| e >= 0)
            .unwrap_or(b'0');
        int_part.push(char::from(c));
    }
    for j in 0..prec_i {
        let pos = e_plus.saturating_add(j);
        let c = usize::try_from(pos)
            .ok()
            .and_then(|i| d.get(i))
            .copied()
            .unwrap_or(b'0');
        frac.push(char::from(c));
    }
    let sign = if x < 0.0 && (int_part.bytes().chain(frac.bytes())).any(|c| c != b'0') {
        "-"
    } else {
        ""
    };
    if prec == 0 {
        format!("{sign}{int_part}")
    } else {
        format!("{sign}{int_part}.{frac}")
    }
}

/// sqlite3's `%.16g`, for `%J` (julian days are positive and below 1e7,
/// so the exponential form never arises in practice but is handled).
fn fmt_g16(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    let (digits, exp) = decimal_digits(x);
    let (d, e) = round_half_up(&digits, exp, 16);
    let sign = if x < 0.0 { "-" } else { "" };
    let ds: String = d.iter().map(|&c| char::from(c)).collect();
    if !(-4..16).contains(&e) {
        let (lead, rest) = ds.split_at_checked(1).unwrap_or((ds.as_str(), ""));
        let rest = rest.trim_end_matches('0');
        let dot = if rest.is_empty() { "" } else { "." };
        let esign = if e < 0 { '-' } else { '+' };
        return format!("{sign}{lead}{dot}{rest}e{esign}{:02}", e.abs());
    }
    let body = if e < 0 {
        let zeros = "0".repeat(usize::try_from(e.unsigned_abs().saturating_sub(1)).unwrap_or(0));
        format!("0.{zeros}{}", ds.trim_end_matches('0'))
    } else {
        let split = usize::try_from(e).unwrap_or(0).saturating_add(1);
        let mut int_part = ds.get(..split.min(ds.len())).unwrap_or("").to_string();
        while int_part.len() < split {
            int_part.push('0');
        }
        let frac = ds
            .get(split.min(ds.len())..)
            .unwrap_or("")
            .trim_end_matches('0');
        if frac.is_empty() {
            int_part
        } else {
            format!("{int_part}.{frac}")
        }
    };
    format!("{sign}{body}")
}

// ---------------------------------------------------------------------
// The SQL functions.
// ---------------------------------------------------------------------

/// `date(TIMEVALUE, MOD...)`: `YYYY-MM-DD`.
fn date(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    Ok(is_date(args, now_ns).map_or(Value::Null, |mut p| {
        compute_ymd(&mut p);
        text(ymd_string(&p))
    }))
}

/// `time(TIMEVALUE, MOD...)`: `HH:MM:SS`.
fn time(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    Ok(is_date(args, now_ns).map_or(Value::Null, |mut p| {
        compute_hms(&mut p);
        text(hms_string(&p))
    }))
}

/// `datetime(TIMEVALUE, MOD...)`: `YYYY-MM-DD HH:MM:SS`.
fn datetime(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    Ok(is_date(args, now_ns).map_or(Value::Null, |mut p| {
        compute_ymd_hms(&mut p);
        text(format!("{} {}", ymd_string(&p), hms_string(&p)))
    }))
}

/// `julianday(TIMEVALUE, MOD...)`: the julian day number, a REAL.
fn julianday(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    Ok(is_date(args, now_ns).map_or(Value::Null, |mut p| {
        compute_jd(&mut p);
        Value::Real(p.ijd as f64 / 86_400_000.0)
    }))
}

/// `unixepoch(TIMEVALUE, MOD...)`: seconds since 1970, an INTEGER, or a
/// REAL with millisecond precision under `subsec`.
fn unixepoch(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    Ok(is_date(args, now_ns).map_or(Value::Null, |mut p| {
        compute_jd(&mut p);
        if p.use_subsec {
            Value::Real((W(p.ijd) - W(UNIX_EPOCH_JD_MS)).0 as f64 / 1000.0)
        } else {
            Value::Integer((W(p.ijd / 1000) - W(UNIX_EPOCH_JD_MS / 1000)).0)
        }
    }))
}

/// Days since January 1 of `p`'s year (`daysAfterJan01`).
fn days_after_jan01(p: &DateTime) -> i64 {
    let mut jan01 = *p;
    jan01.valid_jd = false;
    jan01.mo = 1;
    jan01.d = 1;
    compute_jd(&mut jan01);
    (W(p.ijd) - W(jan01.ijd) + W(HALF_DAY_MS)).0 / DAY_MS
}

/// Day of the week, 0 = Monday (`daysAfterMonday`).
fn days_after_monday(p: &DateTime) -> i64 {
    (W(p.ijd) + W(HALF_DAY_MS)).0 / DAY_MS % 7
}

/// Day of the week, 0 = Sunday (`daysAfterSunday`).
fn days_after_sunday(p: &DateTime) -> i64 {
    (W(p.ijd) + W(129_600_000)).0 / DAY_MS % 7
}

/// The Thursday of `x`'s ISO week, with its Y-M-D computed: the ISO
/// year of `%G`/`%g` and the base of `%V`.
fn iso_thursday(x: &DateTime) -> DateTime {
    let mut y = *x;
    y.ijd = (W(y.ijd) + (W(3) - W(days_after_monday(x))) * W(DAY_MS)).0;
    y.valid_ymd = false;
    compute_ymd(&mut y);
    y
}

/// One `strftime` conversion `cf` of `x`, or `None` for an unknown one
/// (which makes the whole result NULL).
fn strftime_conv(cf: u8, x: &DateTime) -> Option<String> {
    let twelve = |h: i64| match h {
        0 => 12,
        h if h > 12 => (W(h) - W(12)).0,
        h => h,
    };
    Some(match cf {
        b'd' => format!("{:02}", x.d),
        b'e' => format!("{:2}", x.d),
        b'f' => pad6(fmt_fixed(x.s.min(59.999), 3)),
        b'F' => format!("{:04}-{:02}-{:02}", x.y, x.mo, x.d),
        b'G' => format!("{:04}", iso_thursday(x).y),
        b'g' => format!("{:02}", iso_thursday(x).y % 100),
        b'H' => format!("{:02}", x.h),
        b'k' => format!("{:2}", x.h),
        b'I' => format!("{:02}", twelve(x.h)),
        b'l' => format!("{:2}", twelve(x.h)),
        b'j' => format!("{:03}", (W(days_after_jan01(x)) + W(1)).0),
        b'J' => fmt_g16(x.ijd as f64 / 86_400_000.0),
        b'm' => format!("{:02}", x.mo),
        b'M' => format!("{:02}", x.mi),
        b'p' => (if x.h >= 12 { "PM" } else { "AM" }).to_string(),
        b'P' => (if x.h >= 12 { "pm" } else { "am" }).to_string(),
        b'R' => format!("{:02}:{:02}", x.h, x.mi),
        b's' => {
            if x.use_subsec {
                fmt_fixed((W(x.ijd) - W(UNIX_EPOCH_JD_MS)).0 as f64 / 1000.0, 3)
            } else {
                (W(x.ijd / 1000) - W(UNIX_EPOCH_JD_MS / 1000)).0.to_string()
            }
        }
        b'S' => format!("{:02}", f2i(x.s)),
        b'T' => format!("{:02}:{:02}:{:02}", x.h, x.mi, f2i(x.s)),
        b'u' => {
            let w = days_after_sunday(x);
            (if w == 0 { 7 } else { w }).to_string()
        }
        b'w' => days_after_sunday(x).to_string(),
        b'U' => format!(
            "{:02}",
            (W(days_after_jan01(x)) - W(days_after_sunday(x)) + W(7)).0 / 7
        ),
        b'V' => format!(
            "{:02}",
            (W(days_after_jan01(&iso_thursday(x)) / 7) + W(1)).0
        ),
        b'W' => format!(
            "{:02}",
            (W(days_after_jan01(x)) - W(days_after_monday(x)) + W(7)).0 / 7
        ),
        b'Y' => format!("{:04}", x.y),
        b'%' => "%".to_string(),
        _ => return None,
    })
}

/// `s` left-padded with zeros to six characters: C's `%06.3f` width.
fn pad6(s: String) -> String {
    format!("{s:0>6}")
}

/// `strftime(FORMAT, TIMEVALUE, MOD...)`: `FORMAT` with each `%`
/// conversion substituted; an unknown conversion gives NULL.
fn strftime(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    let Some((fmt, rest)) = args.split_first() else {
        return Ok(Value::Null);
    };
    let Some(fmt) = arg_text(fmt) else {
        return Ok(Value::Null);
    };
    let Some(mut x) = is_date(rest, now_ns) else {
        return Ok(Value::Null);
    };
    compute_jd(&mut x);
    compute_ymd_hms(&mut x);
    let mut out: Vec<u8> = Vec::with_capacity(fmt.len());
    let mut bytes = fmt.iter();
    while let Some(&c) = bytes.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        let cf = bytes.next().copied().unwrap_or(0);
        let Some(s) = strftime_conv(cf, &x) else {
            return Ok(Value::Null);
        };
        out.extend_from_slice(s.as_bytes());
    }
    Ok(text(String::from_utf8_lossy(&out).into_owned()))
}

/// `timediff(A, B)`: what to add to B to reach A, as
/// `±YYYY-MM-DD HH:MM:SS.SSS`.
fn timediff(args: &[Value], now_ns: i64) -> Result<Value, FunctionError> {
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        return Ok(Value::Null);
    };
    let (Some(mut d1), Some(mut d2)) = (
        is_date(std::slice::from_ref(a), now_ns),
        is_date(std::slice::from_ref(b), now_ns),
    ) else {
        return Ok(Value::Null);
    };
    compute_ymd_hms(&mut d1);
    compute_ymd_hms(&mut d2);
    let sign;
    let mut y;
    let mut m;
    if d1.ijd >= d2.ijd {
        sign = '+';
        y = (W(d1.y) - W(d2.y)).0;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = (W(d1.mo) - W(d2.mo)).0;
        if m < 0 {
            y = (W(y) - W(1)).0;
            m = (W(m) + W(12)).0;
        }
        if m != 0 {
            d2.mo = d1.mo;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.ijd < d2.ijd && !d2.is_error {
            m = (W(m) - W(1)).0;
            if m < 0 {
                m = 11;
                y = (W(y) - W(1)).0;
            }
            d2.mo = (W(d2.mo) - W(1)).0;
            if d2.mo < 1 {
                d2.mo = 12;
                d2.y = (W(d2.y) - W(1)).0;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.ijd = (W(d1.ijd) - W(d2.ijd)).0;
    } else {
        sign = '-';
        y = (W(d2.y) - W(d1.y)).0;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = (W(d2.mo) - W(d1.mo)).0;
        if m < 0 {
            y = (W(y) - W(1)).0;
            m = (W(m) + W(12)).0;
        }
        if m != 0 {
            d2.mo = d1.mo;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.ijd > d2.ijd && !d2.is_error {
            m = (W(m) - W(1)).0;
            if m < 0 {
                m = 11;
                y = (W(y) - W(1)).0;
            }
            d2.mo = (W(d2.mo) + W(1)).0;
            if d2.mo > 12 {
                d2.mo = 1;
                d2.y = (W(d2.y) + W(1)).0;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.ijd = (W(d2.ijd) - W(d1.ijd)).0;
    }
    // Re-base the difference on 0000-12-31 12:00 so its broken-down
    // fields read as a duration.
    d1.ijd = (W(d1.ijd) + W(148_699_540_800_000)).0;
    clear_ymd_hms_tz(&mut d1);
    compute_ymd_hms(&mut d1);
    Ok(text(format!(
        "{sign}{y:04}-{m:02}-{:02} {:02}:{:02}:{}",
        (W(d1.d) - W(1)).0,
        d1.h,
        d1.mi,
        pad6(fmt_fixed(d1.s, 3))
    )))
}

#[cfg(test)]
mod tests {
    //! Expectations taken from sqlite3 3.53.4; the full set is checked
    //! against it by tools/row_functions_cases_datetime.sql.

    use super::super::call as call_fn;
    use super::*;

    /// A TEXT argument.
    fn t(s: &str) -> Value {
        Value::Text(Rc::from(s))
    }

    /// `name(args)`, with any error turned into a test failure.
    fn call(name: &str, args: &[Value]) -> Value {
        call_fn(name, args).expect("date functions never error")
    }

    #[test]
    fn arities_match_sqlite3() {
        assert!(lookup("date", 0).is_some());
        assert!(lookup("strftime", 9).is_some());
        assert!(lookup("timediff", 2).is_some());
        assert!(lookup("timediff", 1).is_none());
        assert!(lookup("timediff", 3).is_none());
        assert!(lookup("current_date", 0).is_none());
    }

    #[test]
    fn now_has_the_right_shape_and_types() {
        let Value::Text(d) = call("date", &[]) else {
            panic!("date() is text")
        };
        assert_eq!(d.len(), 10);
        let Value::Text(dt) = call("datetime", &[t("now"), t("subsec")]) else {
            panic!("datetime(now, subsec) is text")
        };
        assert_eq!(dt.len(), 23);
        assert!(matches!(call("julianday", &[t("NOW")]), Value::Real(_)));
        assert!(matches!(call("unixepoch", &[]), Value::Integer(_)));
        assert!(matches!(
            call("unixepoch", &[t("now"), t("subsec")]),
            Value::Real(_)
        ));
    }

    #[test]
    fn date_plain() {
        assert_eq!(call("date", &[t("2024-03-15")]), t("2024-03-15"));
    }

    #[test]
    fn datetime_t_separator() {
        assert_eq!(
            call("datetime", &[t("2024-03-15T13:45:30")]),
            t("2024-03-15 13:45:30")
        );
    }

    #[test]
    fn datetime_hhmm() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45")]),
            t("2024-03-15 13:45:00")
        );
    }

    #[test]
    fn time_fraction_truncated() {
        assert_eq!(call("time", &[t("13:45:30.250")]), t("13:45:30"));
    }

    #[test]
    fn time_only_defaults_2000() {
        assert_eq!(call("datetime", &[t("13:45")]), t("2000-01-01 13:45:00"));
    }

    #[test]
    fn date_overflow_normalised() {
        assert_eq!(call("date", &[t("2026-02-30")]), t("2026-03-02"));
    }

    #[test]
    fn date_day_32_invalid() {
        assert_eq!(call("date", &[t("2024-01-32")]), Value::Null);
    }

    #[test]
    fn date_month_13_invalid() {
        assert_eq!(call("date", &[t("2024-13-01")]), Value::Null);
    }

    #[test]
    fn date_junk_invalid() {
        assert_eq!(call("date", &[t("2024-03-15x")]), Value::Null);
    }

    #[test]
    fn tz_zulu() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45Z")]),
            t("2024-03-15 13:45:00")
        );
    }

    #[test]
    fn tz_positive_offset() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45+02:00")]),
            t("2024-03-15 11:45:00")
        );
    }

    #[test]
    fn tz_negative_offset_crosses_day() {
        assert_eq!(
            call("datetime", &[t("2024-12-31 23:00-02:00")]),
            t("2025-01-01 01:00:00")
        );
    }

    #[test]
    fn tz_offset_too_large() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45+15:00")]),
            Value::Null
        );
    }

    #[test]
    fn negative_year() {
        assert_eq!(call("date", &[t("-0044-03-15")]), t("-0044-03-15"));
    }

    #[test]
    fn bc_century_not_leap() {
        assert_eq!(call("date", &[t("-0100-02-29")]), t("-0100-03-01"));
    }

    #[test]
    fn min_julian_day() {
        assert_eq!(
            call("datetime", &[t("-4713-11-24 12:00:00")]),
            t("-4713-11-24 12:00:00")
        );
    }

    #[test]
    fn before_min_julian_day() {
        assert_eq!(call("datetime", &[t("-4713-11-24 11:59:59")]), Value::Null);
    }

    #[test]
    fn max_datetime() {
        assert_eq!(
            call("datetime", &[t("9999-12-31 23:59:59.999"), t("subsec")]),
            t("9999-12-31 23:59:59.999")
        );
    }

    #[test]
    fn year_10000_invalid() {
        assert_eq!(call("datetime", &[t("10000-01-01")]), Value::Null);
    }

    #[test]
    fn julian_day_number() {
        assert_eq!(
            call("datetime", &[Value::Real(2460385.25)]),
            t("2024-03-15 18:00:00")
        );
    }

    #[test]
    fn julian_day_text() {
        assert_eq!(
            call("datetime", &[t("2460385.5")]),
            t("2024-03-16 00:00:00")
        );
    }

    #[test]
    fn unixepoch_modifier() {
        assert_eq!(
            call("datetime", &[Value::Integer(1700000000), t("unixepoch")]),
            t("2023-11-14 22:13:20")
        );
    }

    #[test]
    fn unixepoch_out_of_range() {
        assert_eq!(
            call("datetime", &[Value::Integer(253402300800), t("unixepoch")]),
            Value::Null
        );
    }

    #[test]
    fn auto_unix() {
        assert_eq!(
            call("datetime", &[Value::Integer(1700000000), t("auto")]),
            t("2023-11-14 22:13:20")
        );
    }

    #[test]
    fn auto_julian() {
        assert_eq!(
            call("datetime", &[Value::Real(2460385.5), t("auto")]),
            t("2024-03-16 00:00:00")
        );
    }

    #[test]
    fn auto_not_first_modifier() {
        assert_eq!(
            call(
                "datetime",
                &[Value::Integer(1700000000), t("+1 day"), t("auto")]
            ),
            Value::Null
        );
    }

    #[test]
    fn julianday_modifier_on_text() {
        assert_eq!(
            call("datetime", &[t("2024-01-01"), t("julianday")]),
            Value::Null
        );
    }

    #[test]
    fn large_number_without_modifier() {
        assert_eq!(call("datetime", &[Value::Integer(1700000000)]), Value::Null);
    }

    #[test]
    fn null_time_value() {
        assert_eq!(call("date", &[Value::Null]), Value::Null);
    }

    #[test]
    fn null_modifier() {
        assert_eq!(call("date", &[t("2024-01-01"), Value::Null]), Value::Null);
    }

    #[test]
    fn bogus_modifier() {
        assert_eq!(
            call("datetime", &[t("2024-03-15"), t("bogus")]),
            Value::Null
        );
    }

    #[test]
    fn julianday_real() {
        assert_eq!(
            call("julianday", &[t("2024-03-15 13:45:30.123")]),
            Value::Real(2460385.0732653127)
        );
    }

    #[test]
    fn julianday_integer_input_real_output() {
        assert_eq!(
            call("julianday", &[Value::Integer(2460385)]),
            Value::Real(2460385.0)
        );
    }

    #[test]
    fn unixepoch_integer() {
        assert_eq!(
            call("unixepoch", &[t("1969-12-31 23:59:59.5")]),
            Value::Integer(-1)
        );
    }

    #[test]
    fn unixepoch_subsec_real() {
        assert_eq!(
            call("unixepoch", &[t("1969-12-31 23:59:59.5"), t("subsec")]),
            Value::Real(-0.5)
        );
    }

    #[test]
    fn plus_days() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45"), t("+1.5 days")]),
            t("2024-03-17 01:45:00")
        );
    }

    #[test]
    fn minus_minutes() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45"), t("-90 minutes")]),
            t("2024-03-15 12:15:00")
        );
    }

    #[test]
    fn unit_case_and_plural() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45"), t("+1 Hours")]),
            t("2024-03-15 14:45:00")
        );
    }

    #[test]
    fn unit_unknown() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45"), t("+1 fortnight")]),
            Value::Null
        );
    }

    #[test]
    fn plus_month_overflow() {
        assert_eq!(
            call("date", &[t("2024-01-31"), t("+1 month")]),
            t("2024-03-02")
        );
    }

    #[test]
    fn plus_month_floor() {
        assert_eq!(
            call("date", &[t("2024-01-31"), t("+1 month"), t("floor")]),
            t("2024-02-29")
        );
    }

    #[test]
    fn plus_month_ceiling() {
        assert_eq!(
            call("date", &[t("2024-01-31"), t("+1 month"), t("ceiling")]),
            t("2024-03-02")
        );
    }

    #[test]
    fn leap_day_plus_year_floor() {
        assert_eq!(
            call("date", &[t("2024-02-29"), t("+1 year"), t("floor")]),
            t("2025-02-28")
        );
    }

    #[test]
    fn fractional_months() {
        assert_eq!(
            call("datetime", &[t("2024-03-15"), t("+1.5 months")]),
            t("2024-04-30 00:00:00")
        );
    }

    #[test]
    fn ymd_shift() {
        assert_eq!(
            call(
                "datetime",
                &[t("2024-03-15 13:45"), t("-0001-02-03 04:05:06.5")]
            ),
            t("2023-01-12 09:39:53")
        );
    }

    #[test]
    fn ymd_shift_month_12_invalid() {
        assert_eq!(
            call("datetime", &[t("2024-03-15"), t("+0000-12-00")]),
            Value::Null
        );
    }

    #[test]
    fn hms_shift() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45"), t("+12:00:30.5")]),
            t("2024-03-16 01:45:30")
        );
    }

    #[test]
    fn start_of_month() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45:30"), t("start of month")]),
            t("2024-03-01 00:00:00")
        );
    }

    #[test]
    fn start_of_year() {
        assert_eq!(
            call("datetime", &[t("2024-03-15 13:45:30"), t("start of year")]),
            t("2024-01-01 00:00:00")
        );
    }

    #[test]
    fn start_of_week_invalid() {
        assert_eq!(
            call("datetime", &[t("2024-03-15"), t("start of week")]),
            Value::Null
        );
    }

    #[test]
    fn weekday_sunday() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("weekday 0")]),
            t("2024-03-17")
        );
    }

    #[test]
    fn weekday_same_day() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("weekday 5")]),
            t("2024-03-15")
        );
    }

    #[test]
    fn weekday_fraction_invalid() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("weekday 1.5")]),
            Value::Null
        );
    }

    #[test]
    fn subsec_capped_fraction() {
        assert_eq!(
            call("time", &[t("13:45:59.9999"), t("subsec")]),
            t("13:45:59.999")
        );
    }

    #[test]
    fn utc_on_zulu() {
        assert_eq!(
            call(
                "datetime",
                &[t("2024-03-15 13:45:30.5Z"), t("utc"), t("subsec")]
            ),
            t("2024-03-15 13:45:30.500")
        );
    }

    #[test]
    fn strftime_all_a() {
        assert_eq!(
            call("strftime", &[t("%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%"), t("2024-03-05 07:08:09.123")]),
            t("05| 5|09.123|2024-03-05|2024|24|07|07|065|2460374.797327812| 7| 7|03|08|AM|am|07:08|1709622489|09|07:08:09|2|09|10|2|10|2024|%")
        );
    }

    #[test]
    fn strftime_all_b() {
        assert_eq!(
            call("strftime", &[t("%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%"), t("2023-12-31 23:59:59.999")]),
            t("31|31|59.999|2023-12-31|2023|23|23|11|365|2460310.499999988|23|11|12|59|PM|pm|23:59|1704067199|59|23:59:59|7|53|52|0|52|2023|%")
        );
    }

    #[test]
    fn strftime_iso_week_prev_year() {
        assert_eq!(
            call("strftime", &[t("%G-W%V-%u"), t("2021-01-01")]),
            t("2020-W53-5")
        );
    }

    #[test]
    fn strftime_iso_week_next_year() {
        assert_eq!(
            call("strftime", &[t("%G-W%V-%u"), t("2024-12-30")]),
            t("2025-W01-1")
        );
    }

    #[test]
    fn strftime_twelve_hour_midnight() {
        assert_eq!(
            call("strftime", &[t("%I %l %p %P"), t("2024-01-01 00:30")]),
            t("12 12 AM am")
        );
    }

    #[test]
    fn strftime_f_rounds_half_up() {
        assert_eq!(
            call("strftime", &[t("%f"), t("2024-01-01 00:00:00.0625")]),
            t("00.063")
        );
    }

    #[test]
    fn strftime_s_subsec_negative() {
        assert_eq!(
            call(
                "strftime",
                &[t("%s"), t("1969-12-31 23:59:59.25"), t("subsec")]
            ),
            t("-0.750")
        );
    }

    #[test]
    fn strftime_bc() {
        assert_eq!(
            call("strftime", &[t("%Y %F %j"), t("-0044-03-15")]),
            t("-044 -044-03-15 075")
        );
    }

    #[test]
    fn strftime_unknown_conversion() {
        assert_eq!(call("strftime", &[t("%Q"), t("2024-01-01")]), Value::Null);
    }

    #[test]
    fn strftime_trailing_percent() {
        assert_eq!(
            call("strftime", &[t("a%Yb%"), t("2024-01-01")]),
            Value::Null
        );
    }

    #[test]
    fn strftime_null_format() {
        assert_eq!(
            call("strftime", &[Value::Null, t("2024-01-01")]),
            Value::Null
        );
    }

    #[test]
    fn timediff_forward() {
        assert_eq!(
            call("timediff", &[t("2025-03-15"), t("2024-01-31")]),
            t("+0001-01-12 00:00:00.000")
        );
    }

    #[test]
    fn timediff_backward() {
        assert_eq!(
            call("timediff", &[t("2024-01-31"), t("2025-03-15")]),
            t("-0001-01-15 00:00:00.000")
        );
    }

    #[test]
    fn timediff_subsecond() {
        assert_eq!(
            call(
                "timediff",
                &[t("2024-03-15 00:00"), t("2024-03-14 23:59:59.999")]
            ),
            t("+0000-00-00 00:00:00.001")
        );
    }

    #[test]
    fn timediff_full_range() {
        assert_eq!(
            call(
                "timediff",
                &[t("9999-12-31 23:59:59.999"), t("-4713-11-24 12:00")]
            ),
            t("+14712-01-07 11:59:59.999")
        );
    }

    #[test]
    fn timediff_unix_number_invalid() {
        assert_eq!(
            call("timediff", &[Value::Integer(1700000000), t("2024-01-01")]),
            Value::Null
        );
    }

    // functions_datetime_get_digits_884ad082: val.0 < min || val.0 > max || (next != 0 && next != at(z, 0))
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_get_digits_884ad082__v1_all_false_accepts() {
        assert_eq!(get_digits(b"12", &[(2, 5, 50, 0)]), vec![12]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_get_digits_884ad082__v2_below_min() {
        assert!(get_digits(b"03", &[(2, 5, 50, 0)]).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_get_digits_884ad082__v3_above_max() {
        assert!(get_digits(b"11", &[(2, 0, 10, 0)]).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_get_digits_884ad082__v4_separator_matches() {
        assert_eq!(get_digits(b"12:", &[(2, 0, 99, b':')]), vec![12]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_get_digits_884ad082__v5_separator_mismatch() {
        assert!(get_digits(b"12;", &[(2, 0, 99, b':')]).is_empty());
    }

    // functions_datetime_compute_jd_1393f1d2: !(-4713..=9999).contains(&y) || p.raw_s
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_jd_1393f1d2__v1_in_range_not_raw() {
        let mut p = DateTime {
            valid_ymd: true,
            y: 2000,
            mo: 1,
            d: 1,
            ..DateTime::default()
        };
        compute_jd(&mut p);
        assert!(p.valid_jd);
        assert!(!p.is_error);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_jd_1393f1d2__v2_year_out_of_range() {
        let mut p = DateTime {
            valid_ymd: true,
            y: 10000,
            mo: 1,
            d: 1,
            ..DateTime::default()
        };
        compute_jd(&mut p);
        assert!(p.is_error);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_jd_1393f1d2__v3_raw_seconds() {
        let mut p = DateTime {
            raw_s: true,
            ..DateTime::default()
        };
        compute_jd(&mut p);
        assert!(p.is_error);
    }

    // functions_datetime_compute_floor_0c55f755: p.d <= 28 || matches!(p.mo, 1 | 3 | 5 | 7 | 8 | 10 | 12)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_0c55f755__v1_day_at_most_28() {
        let mut p = DateTime {
            y: 2024,
            mo: 4,
            d: 1,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 0);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_0c55f755__v2_long_month() {
        let mut p = DateTime {
            y: 2024,
            mo: 1,
            d: 31,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 0);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_0c55f755__v3_short_month_day_31() {
        let mut p = DateTime {
            y: 2024,
            mo: 4,
            d: 31,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 1);
    }

    // functions_datetime_compute_floor_88967cd4: p.y % 4 != 0 || (p.y % 100 == 0 && p.y % 400 != 0)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_88967cd4__v1_not_multiple_of_4() {
        let mut p = DateTime {
            y: 2023,
            mo: 2,
            d: 30,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_88967cd4__v2_plain_leap_year() {
        let mut p = DateTime {
            y: 2024,
            mo: 2,
            d: 30,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_88967cd4__v3_century_not_leap() {
        let mut p = DateTime {
            y: 1900,
            mo: 2,
            d: 30,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_compute_floor_88967cd4__v4_century_400_leap() {
        let mut p = DateTime {
            y: 2000,
            mo: 2,
            d: 30,
            ..DateTime::default()
        };
        compute_floor(&mut p);
        assert_eq!(p.n_floor, 1);
    }

    // functions_datetime_parse_hhmmss_00a2f70e: at(z, 0) == b'.' && at(z, 1).is_ascii_digit()
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_hhmmss_00a2f70e__v1_no_dot() {
        let mut p = DateTime::default();
        assert!(!parse_hhmmss(b"12:30:45", &mut p));
        assert_eq!(p.s, 45.0);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_hhmmss_00a2f70e__v2_dot_without_digit() {
        let mut p = DateTime::default();
        assert!(parse_hhmmss(b"12:30:45.x", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_hhmmss_00a2f70e__v3_dot_with_digit() {
        let mut p = DateTime::default();
        assert!(!parse_hhmmss(b"12:30:45.5", &mut p));
        assert_eq!(p.s, 45.5);
    }

    // functions_datetime_parse_yyyymmdd_5a15ed2a: is_space(at(z, 0)) || at(z, 0) == b'T'
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_yyyymmdd_5a15ed2a__v1_neither_space_nor_t() {
        let mut p = DateTime::default();
        assert!(parse_yyyymmdd(b"2024-03-15x10:00", &mut p));
        assert!(!p.valid_hms);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_yyyymmdd_5a15ed2a__v2_space_separator() {
        let mut p = DateTime::default();
        assert!(!parse_yyyymmdd(b"2024-03-15 10:00", &mut p));
        assert!(p.valid_hms);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_yyyymmdd_5a15ed2a__v3_t_separator() {
        let mut p = DateTime::default();
        assert!(!parse_yyyymmdd(b"2024-03-15T10:00", &mut p));
        assert!(p.valid_hms);
    }

    // functions_datetime_parse_date_or_time_bf598f61: !parse_yyyymmdd(z, p) || !parse_hhmmss(z, p)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_bf598f61__v1_date_parses() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(p.valid_ymd);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_bf598f61__v2_time_only_parses() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"10:30", &mut p, 0));
        assert!(!p.valid_ymd);
        assert!(p.valid_hms);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_bf598f61__v3_neither_parses() {
        let mut p = DateTime::default();
        assert!(parse_date_or_time(b"bogus", &mut p, 0));
    }

    // functions_datetime_parse_date_or_time_022df7ff: z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond")
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_022df7ff__v1_neither() {
        let mut p = DateTime::default();
        assert!(parse_date_or_time(b"bogus", &mut p, 0));
        assert!(!p.use_subsec);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_022df7ff__v2_subsec() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"subsec", &mut p, 0));
        assert!(p.use_subsec);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_date_or_time_022df7ff__v3_subsecond() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"subsecond", &mut p, 0));
        assert!(p.use_subsec);
    }

    // functions_datetime_modifier_start_of_022df7ff: z.eq_ignore_ascii_case(b"subsec") || z.eq_ignore_ascii_case(b"subsecond")
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_022df7ff__v1_neither() {
        let mut p = DateTime::default();
        assert!(modifier_start_of(b"bogus", &mut p));
        assert!(!p.use_subsec);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_022df7ff__v2_subsec() {
        let mut p = DateTime::default();
        assert!(!modifier_start_of(b"subsec", &mut p));
        assert!(p.use_subsec);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_022df7ff__v3_subsecond() {
        let mut p = DateTime::default();
        assert!(!modifier_start_of(b"subsecond", &mut p));
        assert!(p.use_subsec);
    }

    // functions_datetime_auto_adjust_date_11585db7: !p.raw_s || p.valid_jd
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_auto_adjust_date_11585db7__v1_raw_unix_seconds_adjusted() {
        let mut p = DateTime {
            raw_s: true,
            s: 0.0,
            ..DateTime::default()
        };
        auto_adjust_date(&mut p);
        assert!(p.valid_jd);
        assert!(!p.raw_s);
        assert_eq!(p.ijd, UNIX_EPOCH_JD_MS);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_auto_adjust_date_11585db7__v2_not_raw() {
        let mut p = DateTime::default();
        auto_adjust_date(&mut p);
        assert!(!p.valid_jd);
        assert!(!p.raw_s);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_auto_adjust_date_11585db7__v3_raw_julian_day() {
        let mut p = DateTime {
            raw_s: true,
            valid_jd: true,
            ijd: 5,
            ..DateTime::default()
        };
        auto_adjust_date(&mut p);
        assert_eq!(p.ijd, 5);
        assert!(!p.raw_s);
    }

    // functions_datetime_parse_modifier_a3c1b99d: z.eq_ignore_ascii_case(b"unixepoch") && p.raw_s
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_a3c1b99d__v1_not_unixepoch() {
        let mut p = DateTime::default();
        assert!(!parse_modifier(b"utc", &mut p, 1));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_a3c1b99d__v2_unixepoch_without_raw() {
        let mut p = DateTime::default();
        assert!(parse_modifier(b"unixepoch", &mut p, 1));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_a3c1b99d__v3_unixepoch_with_raw() {
        let mut p = DateTime {
            raw_s: true,
            s: 0.0,
            ..DateTime::default()
        };
        assert!(!parse_modifier(b"unixepoch", &mut p, 1));
        assert!(p.valid_jd);
        assert_eq!(p.ijd, UNIX_EPOCH_JD_MS);
    }

    // functions_datetime_parse_modifier_99eb0071: p.valid_jd && p.raw_s
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_99eb0071__v1_no_julian_day() {
        let mut p = DateTime {
            raw_s: true,
            ..DateTime::default()
        };
        assert!(parse_modifier(b"julianday", &mut p, 1));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_99eb0071__v2_not_raw() {
        let mut p = DateTime {
            valid_jd: true,
            ..DateTime::default()
        };
        assert!(parse_modifier(b"julianday", &mut p, 1));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_parse_modifier_99eb0071__v3_raw_julian_day() {
        let mut p = DateTime {
            valid_jd: true,
            raw_s: true,
            ..DateTime::default()
        };
        assert!(!parse_modifier(b"julianday", &mut p, 1));
        assert!(!p.raw_s);
    }

    // functions_datetime_modifier_weekday_8f1c5994: !(0.0..7.0).contains(&r) || r.fract() != 0.0
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_weekday_8f1c5994__v1_whole_in_range() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_weekday(b"weekday 1", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_weekday_8f1c5994__v2_out_of_range() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_weekday(b"weekday 9", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_weekday_8f1c5994__v3_fractional() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_weekday(b"weekday 1.5", &mut p));
    }

    // functions_datetime_modifier_start_of_7dc9be9f: !p.valid_jd && !p.valid_ymd && !p.valid_hms
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_7dc9be9f__v1_nothing_valid() {
        let mut p = DateTime {
            ..DateTime::default()
        };
        assert!(modifier_start_of(b"start of day", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_7dc9be9f__v2_julian_day_valid() {
        let mut p = DateTime {
            valid_jd: true,
            ..DateTime::default()
        };
        assert!(!modifier_start_of(b"start of day", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_7dc9be9f__v3_ymd_valid() {
        let mut p = DateTime {
            valid_ymd: true,
            y: 2024,
            mo: 3,
            d: 15,
            ..DateTime::default()
        };
        assert!(!modifier_start_of(b"start of day", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_start_of_7dc9be9f__v4_hms_valid() {
        let mut p = DateTime {
            valid_hms: true,
            ..DateTime::default()
        };
        assert!(!modifier_start_of(b"start of day", &mut p));
    }

    // functions_datetime_modifier_numeric_37daa820: c == 0 || c == b':' || is_space(c)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_37daa820__v1_ordinary_char_continues() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+10 days", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_37daa820__v2_end_of_text() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+5", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_37daa820__v3_colon() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+10:30", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_37daa820__v4_space() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+1 day", &mut p));
    }

    // functions_datetime_modifier_numeric_01236e48: c == b'-' && ((n == 5 && get_digits(adv(z, 1), &[(4, 0, 14712, 0)]).len() == 1)     || (n == 6 && get_digits(adv(z, 1), &[(5, 0, 14712, 0)]).len() == 1))
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v1_not_a_dash() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+1 day", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v2_dash_at_other_position() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+1-2", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v3_dash_at_5_year_not_digits() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+12a4-01", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v4_dash_at_5_year_digits() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+2024-01-15", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v5_dash_at_6_year_not_digits() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+12a45-01", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_01236e48__v6_dash_at_6_year_digits() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+12345-01-15", &mut p));
    }

    // functions_datetime_modifier_numeric_a61d4831: z0 != b'+' && z0 != b'-'
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_a61d4831__v1_plus_sign() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+2024-01-15", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_a61d4831__v2_minus_sign() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"-2024-01-15", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_a61d4831__v3_digit_first() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"92024-01-01", &mut p));
    }

    // functions_datetime_modifier_numeric_150cb5de: m >= 12 || d >= 31
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_150cb5de__v1_month_and_day_in_range() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+2024-01-15", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_150cb5de__v2_month_12() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+2024-12-01", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_150cb5de__v3_day_31() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+2024-01-31", &mut p));
    }

    // functions_datetime_modifier_numeric_0b40fe1c: is_space(at(z, 11)) && get_digits(adv(z, 12), &HH_MM).len() == 2
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_0b40fe1c__v1_space_and_time() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+2024-01-15 10:30", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_0b40fe1c__v2_no_space() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+2024-01-15x", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_0b40fe1c__v3_space_without_time() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+2024-01-15 xx", &mut p));
    }

    // functions_datetime_modifier_numeric_da9dc157: unit.eq_ignore_ascii_case(name) && r > -limit && r < limit
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_da9dc157__v1_unit_and_range_match() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(!modifier_numeric(b"+1 day", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_da9dc157__v2_unit_unknown() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+1 foo", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_da9dc157__v3_below_negative_limit() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"-6000000 days", &mut p));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_da9dc157__v4_above_limit() {
        let mut p = DateTime::default();
        assert!(!parse_date_or_time(b"2024-03-15", &mut p, 0));
        assert!(modifier_numeric(b"+6000000 days", &mut p));
    }

    // functions_datetime_modifier_numeric_1063a50b: i == 4 || i == 5
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_1063a50b__v1_day_unit() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("+1 day")]),
            t("2024-03-16")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_1063a50b__v2_month_unit() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("+1 month")]),
            t("2024-04-15")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_numeric_1063a50b__v3_year_unit() {
        assert_eq!(
            call("date", &[t("2024-03-15"), t("+1 year")]),
            t("2025-03-15")
        );
    }

    // functions_datetime_is_date_7e7af6e2: p.is_error || !valid_julian_day(p.ijd)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_7e7af6e2__v1_valid_day() {
        assert!(is_date(&[t("2024-03-15")], 0).is_some());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_7e7af6e2__v2_error_state() {
        assert!(is_date(&[Value::Real(-1.0)], 0).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_7e7af6e2__v3_julian_day_out_of_range() {
        assert!(is_date(&[t("9999-12-31"), t("+1 day")], 0).is_none());
    }

    // functions_datetime_is_date_d595070a: args.len() == 1 && p.valid_ymd && p.d > 28
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_d595070a__v1_day_over_28_normalised() {
        let p = is_date(&[t("2024-03-31")], 0).expect("valid");
        assert!(!p.valid_ymd);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_d595070a__v2_two_args() {
        let p = is_date(&[t("2024-03-31"), t("subsec")], 0).expect("valid");
        assert!(p.valid_ymd);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_d595070a__v3_no_ymd() {
        let p = is_date(&[Value::Real(2_460_000.5)], 0).expect("valid");
        assert!(!p.valid_ymd);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_is_date_d595070a__v4_day_28_or_less() {
        let p = is_date(&[t("2024-03-15")], 0).expect("valid");
        assert!(p.valid_ymd);
    }

    // functions_datetime_fmt_fixed_c176eb0e: x == 0.0 || d.iter().all(|&c| c == b'0')
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_c176eb0e__v1_nonzero_digits() {
        assert_eq!(fmt_fixed(1.5, 2), "1.50");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_c176eb0e__v2_zero() {
        assert_eq!(fmt_fixed(0.0, 2), "0.00");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_c176eb0e__v3_rounds_to_zero() {
        assert_eq!(fmt_fixed(0.0001, 2), "0.00");
    }

    // functions_datetime_fmt_fixed_722ebf45: x < 0.0 && (int_part.bytes().chain(frac.bytes())).any(|c| c != b'0')
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_722ebf45__v1_positive() {
        assert_eq!(fmt_fixed(1.5, 2), "1.50");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_722ebf45__v2_negative_nonzero() {
        assert_eq!(fmt_fixed(-1.5, 2), "-1.50");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_fmt_fixed_722ebf45__v3_negative_rounds_to_zero() {
        assert_eq!(fmt_fixed(-0.0001, 2), "0.00");
    }

    // functions_datetime_modifier_utc_c5789e7f: err == 0 || cnt >= 3
    // A julian day outside the valid range never round-trips through
    // `to_localtime`, so every guess has a non-zero error.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_utc_c5789e7f__v1_first_guess_exact() {
        let mut p = DateTime {
            ijd: UNIX_EPOCH_JD_MS,
            valid_jd: true,
            ..DateTime::default()
        };
        modifier_utc(&mut p);
        assert!(p.is_utc);
        assert_eq!(p.ijd, UNIX_EPOCH_JD_MS);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_utc_c5789e7f__v2_error_and_guesses_remain() {
        let mut p = DateTime {
            ijd: -1,
            valid_jd: true,
            ..DateTime::default()
        };
        modifier_utc(&mut p);
        assert!(p.is_utc);
        assert!(p.valid_jd);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_modifier_utc_c5789e7f__v3_error_and_guesses_exhausted() {
        let mut p = DateTime {
            ijd: MAX_JD_MS + 1,
            valid_jd: true,
            ..DateTime::default()
        };
        modifier_utc(&mut p);
        assert!(p.is_utc);
        assert!(p.valid_jd);
    }

    // functions_datetime_timediff_af54b5d0: d1.ijd < d2.ijd && !d2.is_error
    // (the second leaf cannot be false while the first is true: an errored
    // `d2` has `ijd == 0`, never above a valid `d1.ijd`.)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_timediff_af54b5d0__v1_d1_not_before_d2() {
        assert_eq!(
            call("timediff", &[t("2024-03-15"), t("2024-03-10")]),
            t("+0000-00-05 00:00:00.000")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_timediff_af54b5d0__v2_d1_before_d2_no_error() {
        assert_eq!(
            call("timediff", &[t("2024-03-10"), t("2024-02-15")]),
            t("+0000-00-24 00:00:00.000")
        );
    }

    // functions_datetime_timediff_be8b89ec: d1.ijd > d2.ijd && !d2.is_error
    // (same: the leaf `!d2.is_error` cannot flip the outcome.)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_timediff_be8b89ec__v1_d1_not_after_d2() {
        assert_eq!(
            call("timediff", &[t("2024-03-10"), t("2024-03-15")]),
            t("-0000-00-05 00:00:00.000")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_datetime_timediff_be8b89ec__v2_d1_after_d2_no_error() {
        assert_eq!(
            call("timediff", &[t("2024-02-15"), t("2024-03-10")]),
            t("-0000-00-24 00:00:00.000")
        );
    }
}
