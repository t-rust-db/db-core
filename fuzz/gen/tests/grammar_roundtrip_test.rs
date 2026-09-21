//! db-core#544: the EBNF parser must round-trip the real
//! `src/parser/grammar.ebnf` without error, and the walker must terminate
//! and report sane coverage for each of the three entry rules.

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

use fuzz_gen::{load_db_core_grammar, Section, VBlockScope, Walker, WalkerConfig};

fn grammar() -> fuzz_gen::Grammar {
    load_db_core_grammar(env!("CARGO_MANIFEST_DIR")).expect("real grammar.ebnf must parse")
}

#[test]
fn parses_real_grammar_file_into_three_nonempty_sections() {
    let g = grammar();
    assert!(!g.shared.is_empty(), "SHARED section must have rules");
    assert!(!g.column.is_empty(), "COLUMN-RS section must have rules");
    assert!(!g.sqlite.is_empty(), "SQLITE-RS section must have rules");
}

#[test]
fn sqlite_section_carries_v_block_tags() {
    let g = grammar();
    let has_tags = g
        .sqlite
        .values()
        .any(|rule| rule.v_tags.iter().any(|tags| !tags.is_empty()));
    assert!(
        has_tags,
        "sqlite-rs section is documented to carry (* Vn *) tags"
    );
}

#[test]
fn column_and_shared_sections_reference_only_known_rules_or_primitives() {
    // Every NonTerminal in COLUMN + SHARED must resolve to a rule in
    // COLUMN/SHARED, or be one of the four documented tokenizer
    // primitives -- otherwise the walker would silently fabricate a
    // terminal for what should have been a real rule reference.
    let g = grammar();
    // "any"/"character"/"except": STRING's body is written as informal
    // prose (`{ any character except "'" }`) rather than a real
    // production -- see `primitive_terminal` in `walker.rs`.
    let primitives = [
        "ALPHA",
        "DIGIT",
        "IDENT",
        "INTEGER",
        "any",
        "character",
        "except",
    ];
    for rule in g.column.values().chain(g.shared.values()) {
        for seq in &rule.alternatives {
            for name in nonterminal_names(seq) {
                let known = g.column.contains_key(&name)
                    || g.shared.contains_key(&name)
                    || primitives.contains(&name.as_str());
                assert!(
                    known,
                    "rule '{}' references unknown nonterminal '{}'",
                    rule.name, name
                );
            }
        }
    }
}

fn nonterminal_names(seq: &[fuzz_gen::ebnf::Term]) -> Vec<String> {
    use fuzz_gen::ebnf::Term;
    let mut names = Vec::new();
    for term in seq {
        match term {
            Term::NonTerminal(n) => names.push(n.clone()),
            Term::Group(alts) | Term::Optional(alts) | Term::Repetition(alts) => {
                for s in alts {
                    names.extend(nonterminal_names(s));
                }
            }
            Term::Terminal(_) => {}
        }
    }
    names
}

#[test]
fn walker_generates_bounded_output_for_each_entry_rule() {
    let g = grammar();
    let entries = [(Section::Column, "sql-stmt"), (Section::Sqlite, "sql-stmt")];
    for (section, entry) in entries {
        let mut walker = Walker::new(&g, section, 42, WalkerConfig::default());
        for _ in 0..20 {
            let stmt = walker
                .generate(entry)
                .unwrap_or_else(|e| panic!("walk failed for {section:?}::{entry}: {e}"));
            assert!(!stmt.is_empty(), "generated statement must be non-empty");
        }
        let (exercised, in_scope) = walker.coverage();
        assert!(
            exercised > 0,
            "walker must have exercised at least one alternative"
        );
        assert!(in_scope > 0, "grammar must have in-scope alternatives");
        assert!(
            exercised <= in_scope,
            "exercised count cannot exceed in-scope count"
        );
    }
}

#[test]
fn walker_is_seed_stable() {
    let g = grammar();
    let mut a = Walker::new(&g, Section::Sqlite, 7, WalkerConfig::default());
    let mut b = Walker::new(&g, Section::Sqlite, 7, WalkerConfig::default());
    for _ in 0..10 {
        assert_eq!(
            a.generate("sql-stmt").expect("walk ok"),
            b.generate("sql-stmt").expect("walk ok")
        );
    }
}

#[test]
fn stream_expr_entry_walks_with_landed_vblock_scope() {
    let g = grammar();
    let landed: std::collections::HashSet<String> =
        ["V1", "V2", "V3"].iter().map(|s| s.to_string()).collect();
    let config = WalkerConfig {
        max_depth: 32,
        scope: VBlockScope::Landed(landed),
        dialect: None,
    };
    let mut walker = Walker::new(&g, Section::Sqlite, 99, config);
    let stmt = walker.generate("expr").expect("walk with landed scope ok");
    assert!(!stmt.is_empty());
}

struct Shout;

impl fuzz_gen::Dialect for Shout {
    fn substitute(&mut self, rule: &str, _rng: &mut fuzz_gen::Rng) -> Option<String> {
        match rule {
            "table-name" => Some("TBL".to_string()),
            "column-name" | "identifier" => Some("COL".to_string()),
            "NUMBER" => Some("42".to_string()),
            _ => None,
        }
    }
}

#[test]
fn dialect_hook_intercepts_rules_before_grammar_expansion() {
    let g = grammar();
    let config = WalkerConfig {
        max_depth: 12,
        scope: VBlockScope::All,
        dialect: Some(Box::new(Shout)),
    };
    let mut walker = Walker::new(&g, Section::Sqlite, 5, config);
    let out: Vec<String> = (0..40)
        .map(|_| walker.generate("sql-stmt").unwrap())
        .collect();
    let joined = out.join("\n");
    assert!(joined.contains("TBL") || joined.contains("COL"), "{joined}");
    assert!(!joined.contains(" NUMBER"), "{joined}");
    // Untouched rules still expand normally: keywords survive.
    assert!(joined.contains("SELECT") || joined.contains("CREATE") || joined.contains("DROP"));
}

#[test]
fn coverage_denominator_counts_reachable_non_substituted_alternatives_only() {
    let g = grammar();
    // Without a dialect: every alternative reachable from `sql-stmt`,
    // which excludes SHARED rules only the column section references
    // (`comparison-op` is one).
    let mut plain = Walker::new(&g, Section::Sqlite, 1, WalkerConfig::default());
    for _ in 0..2000 {
        plain.generate("sql-stmt").unwrap();
    }
    let (_, plain_total) = plain.coverage();
    assert!(
        !plain
            .unexercised()
            .iter()
            .any(|(r, _, _)| r == "comparison-op"),
        "comparison-op is unreachable from sql-stmt and must not count"
    );
    // With a dialect answering table-name/column-name/identifier/NUMBER:
    // those rules' alternatives leave the denominator instead of being
    // reported as never exercised.
    let config = WalkerConfig {
        max_depth: 16,
        scope: VBlockScope::All,
        dialect: Some(Box::new(Shout)),
    };
    let mut dialect = Walker::new(&g, Section::Sqlite, 1, config);
    for _ in 0..2000 {
        dialect.generate("sql-stmt").unwrap();
    }
    let (_, dialect_total) = dialect.coverage();
    assert!(
        dialect_total < plain_total,
        "{dialect_total} vs {plain_total}"
    );
    let unexercised = dialect.unexercised();
    for (rule, _, _) in &unexercised {
        assert!(
            !matches!(
                rule.as_str(),
                "table-name" | "column-name" | "identifier" | "NUMBER"
            ),
            "substituted rule {rule} must not be reported"
        );
    }
    // Reported entries render as EBNF-ish text.
    let mut w = Walker::new(&g, Section::Sqlite, 1, WalkerConfig::default());
    w.generate("sql-stmt").unwrap();
    let first = w.unexercised();
    assert!(!first.is_empty());
    assert!(first.iter().all(|(_, _, text)| !text.is_empty()));
}
