// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3 3.53's `sqlite3FpDecode` (`util.c`) and the REAL-to-text
//! rendering built on it (`%!.17g`) (#652), including its double-double
//! approximation past about 16 digits.

use super::json::{add, at, span};

/// `aBase[]` of sqlite3's `powerOfTen`: 10^p, p = 0..26, normalised.
const POW10_BASE: [u64; 27] = [
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
];

/// `aScale[]` of sqlite3's `powerOfTen`: 10^(27k), high 64 bits.
const POW10_SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae,
    0xcf42894a5dce35ea,
    0xa76c582338ed2621,
    0x873e4f75e2224e68,
    0xda7f5bf590966848,
    0xb080392cc4349dec,
    0x8e938662882af53e,
    0xe65829b3046b0afa,
    0xba121a4650e4ddeb,
    0x964e858c91ba2655,
    0xf2d56790ab41c2a2,
    0xc428d05aa4751e4c,
    0x9e74d1b791e07e48,
    0xcccccccccccccccc,
    0xcecb8f27f4200f3a,
    0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd,
    0xda01ee641a708de9,
    0xb01ae745b101e9e4,
    0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81,
    0xb9a74a0637ce2ee1,
    0x95f83d0a1fb69cd9,
    0xf24a01a73cf2dccf,
    0xc3b8358109e84f07,
    0x9e19db92b4e31ba9,
];

/// `aScaleLo[]` of sqlite3's `powerOfTen`: the next 32 bits of `aScale[]`.
const POW10_SCALE_LO: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b, 0x0cb4a5a3,
    0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc, 0x00000000, 0x999090b6,
    0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9, 0x6d953e2c, 0x4abdaf10, 0xbc633b39,
    0x0a862f81, 0x6c07a2c2,
];

/// The high 64 bits of a 128-bit product (`sqlite3Multiply128`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "the shifted 128-bit product's high half fits in 64 bits"
)]
fn mul_hi(a: u64, b: u64) -> u64 {
    ((u128::from(a).wrapping_mul(u128::from(b))) >> 64) as u64
}

/// `sqlite3Multiply160`: upper 64 bits and the next 32 bits of
/// `((a<<32)+a_lo) * b`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "each half is masked or shifted to fit its width"
)]
fn mul160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let mut r = u128::from(a).wrapping_mul(u128::from(b));
    r = r.wrapping_add(u128::from(a_lo).wrapping_mul(u128::from(b)) >> 32);
    ((r >> 64) as u64, ((r >> 32) & 0xffff_ffff) as u32)
}

/// `powerOfTen(p)`: the top 64 (and next 32) bits of 10^p.
fn power_of_ten(p: i32) -> (u64, u32) {
    let at64 = |t: &[u64], k: i32| {
        usize::try_from(k)
            .ok()
            .and_then(|k| t.get(k).copied())
            .unwrap_or(0)
    };
    let at32 = |k: i32| {
        usize::try_from(k)
            .ok()
            .and_then(|k| POW10_SCALE_LO.get(k).copied())
            .unwrap_or(0)
    };
    let (g, n);
    if p < 0 {
        if p == -1 {
            return (at64(&POW10_SCALE, 13), at32(13));
        }
        let (mut g0, mut n0) = (p.wrapping_div(27), p.wrapping_rem(27));
        if n0 != 0 {
            g0 = g0.wrapping_sub(1);
            n0 = n0.wrapping_add(27);
        }
        (g, n) = (g0, n0);
    } else if p < 27 {
        return (at64(&POW10_BASE, p), 0);
    } else {
        (g, n) = (p.wrapping_div(27), p.wrapping_rem(27));
    }
    let gi = g.wrapping_add(13);
    let s = at64(&POW10_SCALE, gi);
    if n == 0 {
        return (s, at32(gi));
    }
    let (mut x, mut lo) = mul160(s, at32(gi), at64(&POW10_BASE, n));
    if x & (1u64 << 63) == 0 {
        x = (x << 1) | u64::from((lo >> 31) & 1);
        lo = (lo << 1) | 1;
    }
    (x, lo)
}

/// `pwr10to2(p)`: floor(log2(10^p)).
fn pwr10to2(p: i32) -> i32 {
    p.wrapping_mul(108_853) >> 15
}

/// `pwr2to10(p)`: floor(log10(2^p)).
fn pwr2to10(p: i32) -> i32 {
    p.wrapping_mul(78_913) >> 18
}

/// `sqlite3Fp2Convert10(m, e, 18)`: `m * 2^e` as `d * 10^p`, about 18
/// significant digits.
fn fp2convert10(m: u64, e: i32, n: i32, half: bool) -> (u64, i32) {
    let p = n.wrapping_sub(1).wrapping_sub(pwr2to10(e.wrapping_add(63)));
    let (pw, _) = power_of_ten(p);
    let h = mul_hi(m, pw);
    let shift = e.wrapping_add(pwr10to2(p)).wrapping_add(2).wrapping_neg();
    let h = u32::try_from(shift)
        .ok()
        .and_then(|s| h.checked_shr(s))
        .unwrap_or(0);
    if half {
        (h.wrapping_add((h << 1) & 2) >> 1, p.wrapping_neg())
    } else {
        (h >> 1, p.wrapping_neg())
    }
}

/// `sqlite3Fp10Convert2(d, p)`: the double nearest `d * 10^p`.
fn fp10convert2(d: u64, p: i32) -> f64 {
    if p < -348 {
        return 0.0;
    }
    if p > 347 {
        return f64::INFINITY;
    }
    format!("{d}e{p}").parse().unwrap_or(f64::NAN)
}

/// The decimal digits of `digits` as an integer.
fn digits_value(digits: &[u8]) -> u64 {
    digits.iter().fold(0u64, |v, d| {
        v.wrapping_mul(10)
            .wrapping_add(u64::from(d.wrapping_sub(b'0')))
    })
}

/// `sqlite3FpDecode(&s, r, 17, 20)`: sign, significant digits and
/// decimal-point position of a finite non-zero `r`, or which special it
/// is.
pub(crate) enum Decoded {
    /// Infinity (1) or NaN (2) -- `isSpecial`.
    Special(u8),
    /// A finite value: `digits` with the point after `idp` of them.
    Finite {
        /// Significant digits, no trailing zeros.
        digits: Vec<u8>,
        /// `iDP`.
        idp: i32,
    },
}

/// `sqlite3FpDecode` at `%!.17g`'s precision, for `r >= 0`.
pub(crate) fn fp_decode17(r: f64) -> Decoded {
    fp_decode(r, 17, 20)
}

/// `sqlite3FpDecode(&s, r, i_round, mx_round)` for `r >= 0`: with
/// `i_round <= 0` it rounds to `-i_round` places after the decimal point,
/// otherwise to `i_round` significant digits, never more than `mx_round`.
pub(crate) fn fp_decode(r: f64, i_round: i32, mx_round: usize) -> Decoded {
    if r == 0.0 {
        return Decoded::Finite {
            digits: b"0".to_vec(),
            idp: 1,
        };
    }
    let bits = r.to_bits();
    let e0 = (bits >> 52) & 0x7ff;
    if e0 == 0x7ff {
        return Decoded::Special(if bits & 0x7fff_ffff_ffff_ffff == 0x7ff0_0000_0000_0000 {
            1
        } else {
            2
        });
    }
    let mut v = bits & 0x000f_ffff_ffff_ffff;
    let e: i32 = if e0 == 0 {
        let nn = v.leading_zeros();
        v <<= nn;
        (-1074i32).wrapping_sub(i32::try_from(nn).unwrap_or(0))
    } else {
        v = (v << 11) | (1u64 << 63);
        i32::try_from(e0).unwrap_or(0).wrapping_sub(1086)
    };
    // 18 digits, rounded, to find where `iRound` falls; a rounding to
    // 16 digits or fewer then reads `iRound + 1` truncated digits.
    let digits_of = |n: i32, half: bool| {
        let (d, exp) = fp2convert10(v, e, n, half);
        (d.to_string().into_bytes(), exp)
    };
    let (mut z, mut exp) = digits_of(18, true);
    let mut n0 = i32::try_from(z.len()).unwrap_or(0);
    let mut idp = n0.wrapping_add(exp);
    let wanted = if i_round <= 0 {
        idp.wrapping_sub(i_round)
    } else {
        i_round
    };
    let mut i_round = wanted;
    if (1..=16).contains(&wanted) {
        (z, exp) = digits_of(wanted.wrapping_add(1), false);
        n0 = i32::try_from(z.len()).unwrap_or(0);
        idp = n0.wrapping_add(exp);
    }
    let mut n = z.len();
    if i_round == 0 && at(&z, 0) >= b'5' {
        i_round = 1;
        z.insert(0, b'0');
        n = add(n, 1);
        idp = idp.wrapping_add(1);
    }
    let i_round = usize::try_from(i_round).unwrap_or(0);
    if i_round > 0 && (i_round < n || n > mx_round) {
        let mut round = i_round.min(mx_round);
        if round != 17 {
            // No approximation shortcut: plain rounding of the digits.
        } else if at(&z, 15) == b'9' && at(&z, 14) == b'9' {
            let mut jj = 14usize;
            while jj > 0 && at(&z, jj.saturating_sub(1)) == b'9' {
                jj = jj.saturating_sub(1);
            }
            let v2 = if jj == 0 {
                1
            } else {
                digits_value(span(&z, 0, jj)).wrapping_add(1)
            };
            let p2 = exp
                .wrapping_add(n0)
                .wrapping_sub(i32::try_from(jj).unwrap_or(0));
            if r == fp10convert2(v2, p2) {
                round = add(jj, 1);
            }
        } else if idp >= n0 || (at(&z, 15) == b'0' && at(&z, 14) == b'0' && at(&z, 13) == b'0') {
            let mut jj = 13usize;
            while jj > 1 && at(&z, jj.saturating_sub(1)) == b'0' {
                jj = jj.saturating_sub(1);
            }
            let v2 = digits_value(span(&z, 0, jj));
            let p2 = exp
                .wrapping_add(n0)
                .wrapping_sub(i32::try_from(jj).unwrap_or(0));
            if r == fp10convert2(v2, p2) {
                round = add(jj, 1);
            }
        }
        n = round;
        if at(&z, round) >= b'5' {
            let mut j = round.saturating_sub(1);
            while let Some(dig) = z.get_mut(j) {
                *dig = dig.wrapping_add(1);
                if *dig <= b'9' {
                    break;
                }
                *dig = b'0';
                if j == 0 {
                    z.insert(0, b'1');
                    n = add(n, 1);
                    idp = idp.wrapping_add(1);
                    break;
                }
                j = j.saturating_sub(1);
            }
        }
    }
    z.truncate(n);
    while z.len() > 1 && z.last() == Some(&b'0') {
        z.pop();
    }
    Decoded::Finite { digits: z, idp }
}

/// sqlite3's `printf("%!.17g", r)` (`zeropad` false: how a REAL becomes
/// TEXT) or `"%!0.17g"` (`zeropad` true: how JSON functions print a
/// REAL; infinities become `9.0e+999`, NaN `null`).
pub(crate) fn format_real17(r: f64, zeropad: bool) -> Vec<u8> {
    let neg = r < 0.0;
    let (digits, idp) = match fp_decode17(r.abs()) {
        Decoded::Special(2) => {
            return if zeropad {
                b"null".to_vec()
            } else {
                b"NaN".to_vec()
            }
        }
        Decoded::Special(_) => {
            if zeropad {
                (b"9".to_vec(), 1000)
            } else {
                return if neg {
                    b"-Inf".to_vec()
                } else {
                    b"Inf".to_vec()
                };
            }
        }
        Decoded::Finite { digits, idp } => (digits, idp),
    };
    let mut out = Vec::new();
    if neg {
        out.push(b'-');
    }
    let n = i32::try_from(digits.len()).unwrap_or(0);
    let exp = idp.wrapping_sub(1);
    let exp_form = !(-4..=16).contains(&exp);
    let mut precision: i32 = if exp_form {
        16
    } else {
        16i32.wrapping_sub(exp)
    };
    let mut e2: i32 = if exp_form { 0 } else { exp };
    let mut j: i32 = 0;
    if e2 < 0 {
        out.push(b'0');
    } else {
        j = e2.wrapping_add(1).min(n);
        out.extend_from_slice(span(&digits, 0, usize::try_from(j).unwrap_or(0)));
        e2 = e2.wrapping_sub(j);
        if e2 >= 0 {
            out.resize(
                add(out.len(), usize::try_from(e2.wrapping_add(1)).unwrap_or(0)),
                b'0',
            );
            e2 = -1;
        }
    }
    out.push(b'.');
    if e2 < -1 && precision > 0 {
        let nn = (-1i32).wrapping_sub(e2).min(precision);
        out.resize(add(out.len(), usize::try_from(nn).unwrap_or(0)), b'0');
        precision = precision.wrapping_sub(nn);
    }
    if precision > 0 {
        let nn = n.wrapping_sub(j).min(precision);
        if nn > 0 {
            let from = usize::try_from(j).unwrap_or(0);
            out.extend_from_slice(span(
                &digits,
                from,
                add(from, usize::try_from(nn).unwrap_or(0)),
            ));
        }
    }
    while out.last() == Some(&b'0') {
        out.pop();
    }
    if out.last() == Some(&b'.') {
        out.push(b'0');
    }
    if exp_form {
        out.push(b'e');
        out.push(if exp < 0 { b'-' } else { b'+' });
        let e = exp.unsigned_abs();
        if e >= 100 {
            out.extend_from_slice((e / 100).to_string().as_bytes());
        }
        out.extend_from_slice(format!("{:02}", e % 100).as_bytes());
    }
    out
}
