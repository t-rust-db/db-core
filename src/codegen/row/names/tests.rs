use super::*;
use crate::parser::row::parse_select;

fn select(sql: &str) -> Select {
    match parse_select(sql) {
        ParseOutcome::Accepted(s) => *s,
        _ => panic!("{sql}"),
    }
}

fn table(name: &str, columns: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        ..TableSchema::default()
    }
}

fn names_of(sql: &str, schemas: &[TableSchema], views: &[ViewSchema]) -> Vec<String> {
    result_column_names(&select(sql), sql, schemas, views)
}

#[test]
fn repeated_derived_names_get_a_counter() {
    let mut v = vec![
        "a".to_string(),
        "A".to_string(),
        "a".to_string(),
        "b".to_string(),
    ];
    dedupe(&mut v);
    assert_eq!(v, ["a", "A:1", "a:2", "b"]);
    let mut clash = vec!["a".to_string(), "a:1".to_string(), "a".to_string()];
    dedupe(&mut clash);
    assert_eq!(clash, ["a", "a:1", "a:2"]);
}

#[test]
fn a_span_outside_the_text_names_nothing() {
    let s = select("SELECT 1 + 2");
    let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
        panic!()
    };
    assert_eq!(source_text(expr, "SELECT 1 + 2"), "1 + 2");
    assert_eq!(source_text(expr, "SEL"), "");
}

#[test]
fn rowid_names_follow_the_table() {
    let mut keyed = table("k", &["id", "a"]);
    keyed.rowid_alias = Some(0);
    let plain = table("p", &["a"]);
    let mut without = table("w", &["a"]);
    without.without_rowid = true;
    let schemas = [keyed, plain, without];
    assert_eq!(
        names_of("SELECT rowid, a FROM k", &schemas, &[]),
        ["id", "a"]
    );
    assert_eq!(names_of("SELECT OID FROM p", &schemas, &[]), ["rowid"]);
    // No rowid: the name as written.
    assert_eq!(
        names_of("SELECT _rowid_ FROM w", &schemas, &[]),
        ["_rowid_"]
    );
    // A real column called rowid wins.
    let shadow = [table("s", &["RowId"])];
    assert_eq!(names_of("SELECT rowid FROM s", &shadow, &[]), ["RowId"]);
}

#[test]
fn unknown_sources_and_columns_fall_back_to_the_name_as_written() {
    assert_eq!(names_of("SELECT a, b.c FROM nowhere", &[], &[]), ["a", "c"]);
    assert_eq!(
        names_of("SELECT * FROM nowhere", &[], &[]),
        Vec::<String>::new()
    );
    assert_eq!(
        names_of("SELECT x.* FROM nowhere", &[], &[]),
        Vec::<String>::new()
    );
    let schemas = [table("t", &["Abc"])];
    assert_eq!(
        names_of("SELECT q.abc FROM t", &schemas, &[]),
        ["abc"],
        "wrong qualifier"
    );
}

#[test]
fn views_resolve_and_a_cycle_does_not_loop() {
    let schemas = [table("t", &["a"])];
    let views = [
        ViewSchema {
            name: "v".into(),
            sql: "CREATE VIEW v AS SELECT a, a + 1 FROM t".into(),
        },
        ViewSchema {
            name: "loop1".into(),
            sql: "CREATE VIEW loop1 AS SELECT * FROM loop2".into(),
        },
        ViewSchema {
            name: "loop2".into(),
            sql: "CREATE VIEW loop2 AS SELECT * FROM loop1".into(),
        },
        ViewSchema {
            name: "broken".into(),
            sql: "CREATE VIEW broken AS SELEC".into(),
        },
    ];
    assert_eq!(
        names_of("SELECT * FROM v", &schemas, &views),
        ["a", "a + 1"]
    );
    assert_eq!(
        names_of("SELECT * FROM loop1", &schemas, &views),
        Vec::<String>::new()
    );
    assert_eq!(
        names_of("SELECT * FROM broken", &schemas, &views),
        Vec::<String>::new()
    );
}

#[test]
fn ctes_see_earlier_ctes_and_shadow_tables() {
    let schemas = [table("t", &["a"])];
    let sql = "WITH t AS (SELECT 1 AS z), c AS (SELECT * FROM t) SELECT * FROM c";
    assert_eq!(names_of(sql, &schemas, &[]), ["z"]);
}
