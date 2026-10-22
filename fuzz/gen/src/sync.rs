//! The grammar-sync check (db-core#565): db-core's SQLITE-RS ONLY section of
//! `src/parser/grammar.ebnf` against sqlite-rs's
//! `.openspec/grammar/sqlite.ebnf`, which it was migrated from. Both
//! describe the same parser, so they must agree rule by rule: the same rule
//! names, and for each rule the same alternatives, in the same order, with
//! the same V-block tags.
//!
//! A rule on db-core's side is resolved as the generator resolves it: the
//! SQLITE-RS ONLY section first, then SHARED.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::ebnf::{Grammar, Rule, Section};

/// One way the two grammars disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// A rule sqlite-rs's grammar defines that db-core's does not.
    OnlyInSqliteRs(String),
    /// A rule db-core's SQLITE-RS ONLY section defines that sqlite-rs's
    /// grammar does not.
    OnlyInDbCore(String),
    /// The rule has a different number of alternatives.
    AlternativeCount {
        rule: String,
        db_core: usize,
        sqlite_rs: usize,
    },
    /// Alternative `index` (0-based) has a different shape.
    Shape { rule: String, index: usize },
    /// Alternative `index` (0-based) carries different V-block tags.
    VTags {
        rule: String,
        index: usize,
        db_core: Vec<String>,
        sqlite_rs: Vec<String>,
    },
}

impl fmt::Display for Drift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Drift::OnlyInSqliteRs(rule) => write!(f, "{rule}: only in sqlite-rs"),
            Drift::OnlyInDbCore(rule) => write!(f, "{rule}: only in db-core"),
            Drift::AlternativeCount {
                rule,
                db_core,
                sqlite_rs,
            } => write!(
                f,
                "{rule}: {db_core} alternatives in db-core, {sqlite_rs} in sqlite-rs"
            ),
            Drift::Shape { rule, index } => {
                write!(
                    f,
                    "{rule}: alternative {} differs in shape",
                    index.saturating_add(1)
                )
            }
            Drift::VTags {
                rule,
                index,
                db_core,
                sqlite_rs,
            } => write!(
                f,
                "{rule}: alternative {} tagged {db_core:?} in db-core, {sqlite_rs:?} in sqlite-rs",
                index.saturating_add(1)
            ),
        }
    }
}

/// Every disagreement between db-core's grammar (`db_core`, its SQLITE-RS
/// ONLY section over SHARED) and sqlite-rs's (`sqlite_rs`, one section), by
/// rule name.
pub fn compare(db_core: &Grammar, sqlite_rs: &BTreeMap<String, Rule>) -> Vec<Drift> {
    let ours: BTreeSet<&str> = db_core
        .section(Section::Sqlite)
        .keys()
        .map(String::as_str)
        .collect();
    let mut drift = Vec::new();
    for (name, theirs) in sqlite_rs {
        match db_core.resolve(Section::Sqlite, name) {
            Some(rule) => compare_rule(rule, theirs, &mut drift),
            None => drift.push(Drift::OnlyInSqliteRs(name.clone())),
        }
    }
    for name in ours {
        if !sqlite_rs.contains_key(name) {
            drift.push(Drift::OnlyInDbCore(name.to_string()));
        }
    }
    drift
}

fn compare_rule(ours: &Rule, theirs: &Rule, drift: &mut Vec<Drift>) {
    if ours.alternatives.len() != theirs.alternatives.len() {
        drift.push(Drift::AlternativeCount {
            rule: ours.name.clone(),
            db_core: ours.alternatives.len(),
            sqlite_rs: theirs.alternatives.len(),
        });
        return;
    }
    let pairs = ours.alternatives.iter().zip(&theirs.alternatives);
    for (index, (a, b)) in pairs.enumerate() {
        if a != b {
            drift.push(Drift::Shape {
                rule: ours.name.clone(),
                index,
            });
        }
        let empty = Vec::new();
        let tags = |r: &Rule| r.v_tags.get(index).unwrap_or(&empty).clone();
        let (ours_tags, theirs_tags) = (tags(ours), tags(theirs));
        if ours_tags != theirs_tags {
            drift.push(Drift::VTags {
                rule: ours.name.clone(),
                index,
                db_core: ours_tags,
                sqlite_rs: theirs_tags,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ebnf::{parse_section, parse_source};

    /// A three-section db-core grammar whose SQLITE-RS ONLY section is
    /// `sqlite`, over a SHARED section defining `shared-rule`.
    fn db_core(sqlite: &str) -> Grammar {
        let src = format!(
            "(* ===== 1. SHARED ===== *)\nshared-rule ::= \"S\" ;\n\
             (* ===== 2. COLUMN-RS ONLY ===== *)\ncol ::= \"C\" ;\n\
             (* ===== 3. SQLITE-RS ONLY ===== *)\n{sqlite}"
        );
        parse_source(&src).expect("valid db-core grammar")
    }

    fn sqlite_rs(src: &str) -> BTreeMap<String, Rule> {
        parse_section(src).expect("valid sqlite-rs grammar")
    }

    #[test]
    fn identical_grammars_agree_and_shared_rules_resolve() {
        let ours = db_core("stmt ::= \"A\" shared-rule (* V2 *)\n | \"B\" ; (* V3 *)\n");
        let theirs = sqlite_rs(
            "stmt ::= \"A\" shared-rule (* V2 *)\n | \"B\" ; (* V3 *)\nshared-rule ::= \"S\" ;\n",
        );
        assert_eq!(compare(&ours, &theirs), Vec::new());
    }

    #[test]
    fn each_kind_of_drift_is_reported() {
        let ours = db_core(
            "count ::= \"A\" | \"B\" ;\n\
             shape ::= \"A\" | \"B\" ;\n\
             tags ::= \"A\" (* V2 *)\n | \"B\" ; (* V3 *)\n\
             ours-only ::= \"X\" ;\n",
        );
        let theirs = sqlite_rs(
            "count ::= \"A\" ;\n\
             shape ::= \"A\" | \"C\" ;\n\
             tags ::= \"A\" (* V2 *)\n | \"B\" ; (* V4 *)\n\
             theirs-only ::= \"Y\" ;\n",
        );
        assert_eq!(
            compare(&ours, &theirs),
            vec![
                Drift::AlternativeCount {
                    rule: "count".into(),
                    db_core: 2,
                    sqlite_rs: 1,
                },
                Drift::Shape {
                    rule: "shape".into(),
                    index: 1,
                },
                Drift::VTags {
                    rule: "tags".into(),
                    index: 1,
                    db_core: vec!["V3".into()],
                    sqlite_rs: vec!["V4".into()],
                },
                Drift::OnlyInSqliteRs("theirs-only".into()),
                Drift::OnlyInDbCore("ours-only".into()),
            ]
        );
    }
}
