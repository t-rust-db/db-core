use super::*;
use crate::test_rng::Rng;

fn like(pattern: &str, text: &str) -> bool {
    Matcher::new(pattern, Dialect::Like, false).matches(text, &mut String::new())
}

#[test]
fn segment_patterns_match_as_like_does() {
    for (pattern, text, want) in [
        ("", "", true),
        ("", "a", false),
        ("%", "", true),
        ("%%", "anything", true),
        ("abc", "abc", true),
        ("abc", "abcd", false),
        ("abc%", "abcdef", true),
        ("abc%", "xabc", false),
        ("%def", "abcdef", true),
        ("%def", "defx", false),
        ("%host 42%", "request 7 from host 42", true),
        ("%host 42%", "request 7 from host 4", false),
        ("a%b", "ab", true),
        ("a%b", "a", false),
        // Prefix and suffix may not share bytes.
        ("ab%ba", "aba", false),
        ("ab%ba", "abba", true),
        ("%a%b%c%", "xaybzc", true),
        ("%a%b%c%", "cba", false),
        ("é%ü", "éxü", true),
    ] {
        assert_eq!(like(pattern, text), want, "{text:?} LIKE {pattern:?}");
        assert_eq!(
            like_match_exact(text, pattern),
            want,
            "reference: {text:?} LIKE {pattern:?}"
        );
    }
}

#[test]
fn ilike_lowercases_both_sides_once() {
    let m = Matcher::new("%HOST%", Dialect::Like, true);
    let mut buf = String::new();
    assert!(m.matches("from Host 4", &mut buf));
    assert!(
        m.matches("ÉHOSTÉ", &mut buf),
        "non-ASCII text lowercases too"
    );
    assert!(!m.matches("hos t", &mut buf));
    let glob = Matcher::new("a*c", Dialect::Glob, false);
    assert!(glob.matches("abbc", &mut buf));
    assert!(!glob.matches("ABBC", &mut buf), "GLOB is case-sensitive");
}

/// A random string over `alphabet`, up to `max` characters.
fn random(rng: &mut Rng, alphabet: &[char], max: u64) -> String {
    (0..rng.below(max + 1))
        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
        .collect()
}

/// The compiled matcher agrees with the general matchers on every
/// pattern, both its own segment shape and the ones it hands on.
#[test]
fn compiled_patterns_agree_with_the_general_matchers() {
    let mut rng = Rng::new(691);
    let text_chars = ['a', 'b', 'A', 'é', 'É', ' ', '%', '_', '*', '?'];
    let like_chars = ['a', 'b', 'A', 'é', ' ', '%', '%', '_'];
    let glob_chars = ['a', 'b', 'é', ' ', '*', '*', '?', '[', ']', '^'];
    let mut buf = String::new();
    for _ in 0..20_000 {
        let text = random(&mut rng, &text_chars, 8);
        let p = random(&mut rng, &like_chars, 6);
        assert_eq!(
            Matcher::new(&p, Dialect::Like, false).matches(&text, &mut buf),
            like_match_exact(&text, &p),
            "{text:?} LIKE {p:?}"
        );
        assert_eq!(
            Matcher::new(&p, Dialect::Like, true).matches(&text, &mut buf),
            like_match_exact(&text.to_lowercase(), &p.to_lowercase()),
            "{text:?} ILIKE {p:?}"
        );
        let g = random(&mut rng, &glob_chars, 6);
        assert_eq!(
            Matcher::new(&g, Dialect::Glob, false).matches(&text, &mut buf),
            glob_match(&text, &g),
            "{text:?} GLOB {g:?}"
        );
    }
}

// vm_like_segments_match_c210b42f: an all-wildcard or empty pattern,
// `!(anchored_start && anchored_end) || text.is_empty()`.
#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_segments_match_c210b42f__v1_a_leading_wildcard_matches_anything() {
    assert!(segments_match("x", &[], false, false));
}

#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_segments_match_c210b42f__v2_a_trailing_wildcard_matches_anything() {
    assert!(segments_match("x", &[], true, false));
}

#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_segments_match_c210b42f__v3_the_empty_pattern_matches_the_empty_text() {
    assert!(segments_match("", &[], true, true));
}

#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_segments_match_c210b42f__v4_the_empty_pattern_refuses_other_text() {
    assert!(!segments_match("x", &[], true, true));
}

// vm_like_new_2730a306: the failure-table build's fallback loop,
// `k > 0 && n.get(k) != Some(b)`.
#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_new_2730a306__v1_no_border_yet_skips_the_fallback() {
    assert_eq!(Finder::new("ab").fail, [0, 0]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_new_2730a306__v2_a_mismatch_after_a_border_falls_back() {
    assert_eq!(Finder::new("aab").fail, [0, 1, 0]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__vm_like_new_2730a306__v3_a_match_after_a_border_extends_it() {
    assert_eq!(Finder::new("aaa").fail, [0, 1, 2]);
}

#[test]
fn finder_handles_overlap_edges_and_adversarial_input() {
    for (pattern, text, want) in [
        ("%aa%aa%", "aaaa", true),
        ("%aa%aa%", "aaa", false),
        ("%abc", "xxabc", true),
        ("%abcd%", "abc", false),
        ("%日本%", "x日本y", true),
        ("%日本%", "x日y本", false),
        ("%aab%", "aaaab", true),
    ] {
        assert_eq!(like(pattern, text), want, "{text:?} LIKE {pattern:?}");
        assert_eq!(like_match_exact(text, pattern), want, "reference");
    }
    let m = Matcher::new("%HÉ%", Dialect::Like, true);
    assert!(m.matches("xhÉy", &mut String::new()));
    // `a…ab` in `aaaa…`: linear, so this finishes instantly.
    let pattern = format!("%{}b%", "a".repeat(5_000));
    let text = "a".repeat(2_000_000);
    assert!(!like(&pattern, &text));
}
