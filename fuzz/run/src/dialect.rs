//! Schema-aware terminal substitution for the ROW target.
//!
//! The grammar's lexical productions (`STRING ::= "'" { any character
//! except "'" } "'"`, `NUMBER` with no production at all, `table-name ::=
//! identifier`) generate prose or arbitrary letters. Replacing them with
//! names from the opened database's catalog and typed literals means most
//! generated statements get *past* the parser and name resolution and
//! actually exercise codegen and the VM. A small share of names is still
//! drawn at random so the not-found paths stay covered too.

use db_core::engine::TableInfo;
use fuzz_gen::{Dialect, Rng};

/// One-in-`UNKNOWN_NAME_ODDS` identifiers is a fresh random name rather
/// than a catalog name, to keep `no such table`/`no such column` paths
/// in the run.
const UNKNOWN_NAME_ODDS: usize = 8;

#[derive(Debug, Clone)]
pub struct RowDialect {
    tables: Vec<String>,
    columns: Vec<String>,
    types: Vec<&'static str>,
}

impl RowDialect {
    /// Builds the substitution pools from a live catalog listing.
    pub fn from_tables(tables: &[TableInfo]) -> Self {
        let mut columns: Vec<String> = tables
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| c.name.clone()))
            .collect();
        columns.sort();
        columns.dedup();
        RowDialect {
            tables: tables.iter().map(|t| t.name.clone()).collect(),
            columns,
            types: vec!["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC", "VARCHAR(20)"],
        }
    }

    fn pick<'a>(pool: &'a [String], rng: &mut Rng) -> Option<&'a str> {
        if pool.is_empty() {
            return None;
        }
        pool.get(rng.gen_range(pool.len())).map(String::as_str)
    }

    fn random_ident(rng: &mut Rng) -> String {
        let letters = "abcdefghijklmnopqrstuvwxyz";
        let len = 1usize.saturating_add(rng.gen_range(6));
        (0..len)
            .map(|_| {
                letters
                    .chars()
                    .nth(rng.gen_range(letters.len()))
                    .unwrap_or('x')
            })
            .collect()
    }

    fn name_from(pool: &[String], rng: &mut Rng) -> String {
        if rng.gen_range(UNKNOWN_NAME_ODDS) == 0 {
            return Self::random_ident(rng);
        }
        Self::pick(pool, rng)
            .map(str::to_string)
            .unwrap_or_else(|| Self::random_ident(rng))
    }

    fn number(rng: &mut Rng) -> String {
        match rng.gen_range(8) {
            0 => "0".to_string(),
            1 => format!("-{}", rng.gen_range(1000)),
            2 => format!("{}.{}", rng.gen_range(100), rng.gen_range(100)),
            3 => format!("{}e{}", rng.gen_range(10), rng.gen_range(6)),
            4 => format!("0x{:x}", rng.gen_range(0xFFFF)),
            5 => "9223372036854775807".to_string(),
            6 => ".5".to_string(),
            _ => rng.gen_range(1000).to_string(),
        }
    }

    fn string(rng: &mut Rng) -> String {
        match rng.gen_range(6) {
            0 => "''".to_string(),
            1 => "'it''s'".to_string(),
            2 => "'%a_'".to_string(),
            3 => "'abc'".to_string(),
            4 => "'ünïcödé'".to_string(),
            _ => format!("'{}'", Self::random_ident(rng)),
        }
    }

    fn blob(rng: &mut Rng) -> String {
        match rng.gen_range(3) {
            0 => "X''".to_string(),
            1 => "X'00ff'".to_string(),
            _ => format!("X'{:02x}{:02x}'", rng.gen_range(256), rng.gen_range(256)),
        }
    }
}

impl Dialect for RowDialect {
    fn substitute(&mut self, rule: &str, rng: &mut Rng) -> Option<String> {
        Some(match rule {
            "table-name" | "view-name" => Self::name_from(&self.tables, rng),
            "column-name" | "column-alias" | "alias" => Self::name_from(&self.columns, rng),
            // A bare `identifier` may be either; column names dominate
            // in expressions, so lean that way.
            "identifier" | "ident-chars" | "IDENT" => {
                if rng.gen_range(4) == 0 {
                    Self::name_from(&self.tables, rng)
                } else {
                    Self::name_from(&self.columns, rng)
                }
            }
            "quoted-identifier" => format!("\"{}\"", Self::name_from(&self.columns, rng)),
            "collation-name" => ["BINARY", "NOCASE", "RTRIM"]
                .get(rng.gen_range(3))
                .copied()
                .unwrap_or("BINARY")
                .to_string(),
            "type-name" => self
                .types
                .get(rng.gen_range(self.types.len()))
                .copied()
                .unwrap_or("INTEGER")
                .to_string(),
            "NUMBER" | "INTEGER" | "FLOAT" | "signed-number" => Self::number(rng),
            "STRING" => Self::string(rng),
            "BLOB" => Self::blob(rng),
            _ => return None,
        })
    }
}
