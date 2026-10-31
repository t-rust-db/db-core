// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `functions::{like_match, like_match_exact, glob_match}` (#871): the
//! iterative matcher that replaced the recursive one answers exactly as
//! it did, and neither aborts on a long pattern nor backtracks
//! exponentially. Lives under `tests/` rather than inline so the
//! recursive reference matcher stays out of `src/`'s MC/DC obligations.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::functions::{glob_match, like_match, like_match_exact};

/// The recursive matchers #871 replaced, kept as the
/// reference for [`iterative_matchers_agree_with_the_recursive_ones`].
mod recursive {
    pub(super) fn like(t: &[char], p: &[char], esc: Option<char>, exact: bool) -> bool {
        like_rec(t, p, esc, exact, 0, 0)
    }

    fn like_rec(
        t: &[char],
        p: &[char],
        escape: Option<char>,
        exact: bool,
        mut ti: usize,
        mut pi: usize,
    ) -> bool {
        let same = |a: char, b: char| {
            if exact {
                a == b
            } else {
                a.eq_ignore_ascii_case(&b)
            }
        };
        loop {
            if pi == p.len() {
                return ti == t.len();
            }
            let pc = p[pi];
            // The one deliberate change from the original: a trailing
            // escape matches nothing (sqlite3's `patternCompare`); the
            // recursive matcher took it as a literal.
            if Some(pc) == escape && pi + 1 == p.len() {
                return false;
            }
            if Some(pc) == escape && pi + 1 < p.len() {
                if ti >= t.len() || !same(t[ti], p[pi + 1]) {
                    return false;
                }
                ti += 1;
                pi += 2;
                continue;
            }
            match pc {
                '%' => {
                    while pi < p.len() && p[pi] == '%' {
                        pi += 1;
                    }
                    if pi == p.len() {
                        return true;
                    }
                    return (ti..=t.len()).any(|s| like_rec(t, p, escape, exact, s, pi));
                }
                '_' if ti < t.len() => {}
                '_' => return false,
                _ if ti < t.len() && same(t[ti], pc) => {}
                _ => return false,
            }
            ti += 1;
            pi += 1;
        }
    }

    pub(super) fn glob(t: &[char], p: &[char], mut ti: usize, mut pi: usize) -> bool {
        loop {
            if pi == p.len() {
                return ti == t.len();
            }
            match p[pi] {
                '*' => {
                    while pi < p.len() && p[pi] == '*' {
                        pi += 1;
                    }
                    if pi == p.len() {
                        return true;
                    }
                    return (ti..=t.len()).any(|s| glob(t, p, s, pi));
                }
                '?' if ti < t.len() => pi += 1,
                '[' => {
                    let Some((matched, next)) = class(p, pi, t.get(ti).copied()) else {
                        return false;
                    };
                    if !matched {
                        return false;
                    }
                    pi = next;
                }
                c if ti < t.len() && t[ti] == c => pi += 1,
                _ => return false,
            }
            ti += 1;
        }
    }

    fn class(p: &[char], start: usize, c: Option<char>) -> Option<(bool, usize)> {
        let mut i = start + 1;
        let negate = p.get(i) == Some(&'^');
        if negate {
            i += 1;
        }
        let class_start = i;
        let mut matched = false;
        loop {
            if i >= p.len() {
                return None;
            }
            if p[i] == ']' && i > class_start {
                i += 1;
                break;
            }
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                if let Some(c) = c {
                    matched |= c >= p[i] && c <= p[i + 2];
                }
                i += 3;
            } else {
                matched |= Some(p[i]) == c;
                i += 1;
            }
        }
        Some((matched != negate && c.is_some(), i))
    }
}

/// #871: the iterative matchers answer exactly as the recursive ones
/// did, over random short patterns dense in metacharacters.
#[test]
fn iterative_matchers_agree_with_the_recursive_ones() {
    const ALPHABET: [char; 13] = [
        'a', 'A', 'b', '%', '_', '\\', '*', '?', '[', ']', '^', '-', 'z',
    ];
    // xorshift64, seeded: the cases are deterministic run to run.
    let mut state: u64 = 871;
    let mut below = |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let mut draw = |max: u64| -> Vec<char> {
        let len = below(max + 1);
        (0..len)
            .map(|_| ALPHABET[below(ALPHABET.len() as u64) as usize])
            .collect()
    };
    for _ in 0..50_000 {
        let (p, t) = (draw(8), draw(6));
        let (ps, ts): (String, String) = (p.iter().collect(), t.iter().collect());
        for esc in [None, Some('\\'), Some('%')] {
            assert_eq!(
                like_match(&ts, &ps, esc),
                recursive::like(&t, &p, esc, false),
                "{ts:?} LIKE {ps:?} ESCAPE {esc:?}"
            );
        }
        assert_eq!(
            like_match_exact(&ts, &ps),
            recursive::like(&t, &p, None, true),
            "{ts:?} LIKE(exact) {ps:?}"
        );
        assert_eq!(
            glob_match(&ts, &ps),
            recursive::glob(&t, &p, 0, 0),
            "{ts:?} GLOB {ps:?}"
        );
    }
}

/// #871: a pattern with a million wildcard groups neither overflows a
/// 2 MiB thread's stack (the recursive matcher aborted the process at
/// ~200k groups on the 8 MiB main thread) nor backtracks
/// exponentially (`'%a' x 30 || 'b'` alone took 6 s).
#[test]
fn long_and_pathological_patterns_match_without_recursion() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let text = "a".repeat(1_000_000);
            assert!(like_match(&text, &"%a".repeat(1_000_000), None));
            assert!(like_match_exact(&text, &"%a".repeat(1_000_000)));
            assert!(glob_match(&text, &"*a".repeat(1_000_000)));

            let text = "a".repeat(2_000);
            let like = format!("{}b", "%a".repeat(1_000));
            let glob = format!("{}b", "*a".repeat(1_000));
            assert!(!like_match(&text, &like, None));
            assert!(!like_match_exact(&text, &like));
            assert!(!glob_match(&text, &glob));
        })
        .unwrap()
        .join()
        .unwrap();
}
