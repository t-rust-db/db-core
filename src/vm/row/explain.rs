//! `EXPLAIN` output rendering (db-core#88), ported from sqlite-rs's
//! `vdbe::explain`: one row per instruction -- `addr`, `opcode`, `p1`,
//! `p2`, `p3`, `p4` (rendered in its display form, not its raw bytes),
//! `p5`, `comment`.
//!
//! Unlike sqlite-rs, `vm::row`'s [`super::program::Instruction`]
//! already carries an optional `comment` field (ADR 0007's
//! convention, set by a caller such as `codegen::row` at emit time) --
//! [`explain`] prefers that verbatim when present, and only falls back
//! to [`comment_for`]'s computed annotation when it is absent, so a
//! codegen-supplied comment is never silently discarded.
//!
//! sqlite-rs's `explain.rs` also renders the `EXPLAIN QUERY PLAN` tree,
//! but that lives in its `codegen/select/eqp.rs` (the query *planner*,
//! not the VDBE) -- out of scope for `vm::row`, which only executes
//! already-compiled bytecode.

use super::program::{GroupKeyColumn, Opcode, Program, SortKeyColumn, P4};
use super::value::Collation;

/// One rendered `EXPLAIN` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainRow {
    /// Program address (instruction index) of this row.
    pub addr: usize,
    /// The opcode's mnemonic name.
    pub opcode: &'static str,
    /// First operand.
    pub p1: i32,
    /// Second operand.
    pub p2: i32,
    /// Third operand.
    pub p3: i32,
    /// Fourth operand, rendered in its display form (not raw bytes).
    pub p4: String,
    /// Fifth operand (flags).
    pub p5: u16,
    /// A short human-readable annotation of the instruction's effect.
    pub comment: String,
}

/// Renders every instruction in `program` as one [`ExplainRow`], in
/// program order.
pub fn explain(program: &Program) -> Vec<ExplainRow> {
    program
        .instructions
        .iter()
        .enumerate()
        .map(|(addr, instr)| ExplainRow {
            addr,
            opcode: opcode_name(instr.opcode),
            p1: instr.p1,
            p2: instr.p2,
            p3: instr.p3,
            p4: render_p4(&instr.p4),
            p5: instr.p5,
            comment: instr
                .comment
                .clone()
                .unwrap_or_else(|| comment_for(instr.opcode, instr.p1, instr.p2, instr.p3)),
        })
        .collect()
}

fn render_p4(p4: &P4) -> String {
    match p4 {
        P4::None => String::new(),
        P4::Int(i) => i.to_string(),
        P4::Real(r) => super::value::format_real(*r),
        P4::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        P4::Str(s) => s.clone(),
        P4::CollSeq {
            collation,
            affinity,
        } => {
            format!("{}-{affinity}", collation_name(*collation))
        }
        P4::AggFunc {
            name,
            arity,
            collation,
        } => format!("{name}({arity})-{}", collation_name(*collation)),
        P4::SortKey(cols) => render_sort_key(cols),
        P4::GroupKey(cols) => render_group_key(cols),
        P4::Affinity(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        P4::SeekKey(collations) => collations
            .iter()
            .map(|c| format!("{c:?}").to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(","),
        P4::Bool(b) => i32::from(*b).to_string(),
        P4::CreateTable { name, sql } => format!("{name}: {sql}"),
        P4::CreateView { name, sql } => format!("{name}: {sql}"),
        P4::DropTable { name, .. } => name.clone(),
        P4::CreateIndex { name, sql, .. } => format!("{name}: {sql}"),
        P4::DropIndex { name, .. } => name.clone(),
        P4::Analyze { targets } => targets
            .iter()
            .map(|t| t.table_name.as_str())
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn collation_name(collation: Collation) -> &'static str {
    match collation {
        Collation::Binary => "BINARY",
        Collation::NoCase => "NOCASE",
        Collation::RTrim => "RTRIM",
    }
}

/// Renders a sort-key descriptor as `k(N,dir,dir,...)`: `N` key
/// columns, each a direction sign (`-` for descending, none for
/// ascending) followed by the collation's first letter.
fn render_sort_key(cols: &[SortKeyColumn]) -> String {
    let mut parts = Vec::with_capacity(cols.len());
    for col in cols {
        let sign = if col.descending { "-" } else { "" };
        let coll = match col.collation {
            Collation::Binary => "B",
            Collation::NoCase => "N",
            Collation::RTrim => "R",
        };
        parts.push(format!("{sign}{coll}"));
    }
    format!("k({},{})", cols.len(), parts.join(","))
}

/// `HashAggOpen`'s group-key descriptor, rendered in the same
/// `g(N,...)` shape [`render_sort_key`] uses for the sorter's -- one
/// entry per key column, its collation's first letter.
fn render_group_key(cols: &[GroupKeyColumn]) -> String {
    let mut parts = Vec::with_capacity(cols.len());
    for col in cols {
        let coll = match col.collation {
            Collation::Binary => "B",
            Collation::NoCase => "N",
            Collation::RTrim => "R",
        };
        parts.push(coll.to_string());
    }
    format!("g({},{})", cols.len(), parts.join(","))
}

fn opcode_name(opcode: Opcode) -> &'static str {
    match opcode {
        Opcode::Init => "Init",
        Opcode::Goto => "Goto",
        Opcode::Once => "Once",
        Opcode::BeginSubrtn => "BeginSubrtn",
        Opcode::Return => "Return",
        Opcode::Halt => "Halt",
        Opcode::Transaction => "Transaction",
        Opcode::AutoCommit => "AutoCommit",
        Opcode::SetJournalMode => "SetJournalMode",
        Opcode::IntegrityCheck => "IntegrityCheck",
        Opcode::Synchronous => "Synchronous",
        Opcode::IfNot => "IfNot",
        Opcode::IfNotZero => "IfNotZero",
        Opcode::IfPos => "IfPos",
        Opcode::DecrJumpZero => "DecrJumpZero",
        Opcode::IsNull => "IsNull",
        Opcode::NotNull => "NotNull",
        Opcode::MustBeInt => "MustBeInt",
        Opcode::OffsetLimit => "OffsetLimit",
        Opcode::OpenRead => "OpenRead",
        Opcode::OpenWrite => "OpenWrite",
        Opcode::OpenEphemeral => "OpenEphemeral",
        Opcode::OpenDup => "OpenDup",
        Opcode::OpenPseudo => "OpenPseudo",
        Opcode::Rewind => "Rewind",
        Opcode::Last => "Last",
        Opcode::Next => "Next",
        Opcode::Column => "Column",
        Opcode::Rowid => "Rowid",
        Opcode::SeekRowid => "SeekRowid",
        Opcode::SeekIndexEq => "SeekIndexEq",
        Opcode::SeekIndexGE => "SeekIndexGE",
        Opcode::IdxCompareGT => "IdxCompareGT",
        Opcode::IdxRowid => "IdxRowid",
        Opcode::IdxRewind => "IdxRewind",
        Opcode::IdxLast => "IdxLast",
        Opcode::IdxNext => "IdxNext",
        Opcode::IdxPrev => "IdxPrev",
        Opcode::AutoIndexInsert => "AutoIndexInsert",
        Opcode::AutoIndexSeek => "AutoIndexSeek",
        Opcode::AutoIndexRowid => "AutoIndexRowid",
        Opcode::AutoIndexNext => "AutoIndexNext",
        Opcode::NullRow => "NullRow",
        Opcode::Sequence => "Sequence",
        Opcode::Found => "Found",
        Opcode::IdxInsert => "IdxInsert",
        Opcode::IdxDelete => "IdxDelete",
        Opcode::Count => "Count",
        Opcode::NoConflict => "NoConflict",
        Opcode::IdxLE => "IdxLE",
        Opcode::Delete => "Delete",
        Opcode::Insert => "Insert",
        Opcode::NewRowid => "NewRowid",
        Opcode::CreateTable => "CreateTable",
        Opcode::CreateView => "CreateView",
        Opcode::DropTable => "DropTable",
        Opcode::CreateIndex => "CreateIndex",
        Opcode::DropIndex => "DropIndex",
        Opcode::Analyze => "Analyze",
        Opcode::Eq => "Eq",
        Opcode::Ge => "Ge",
        Opcode::Gt => "Gt",
        Opcode::Le => "Le",
        Opcode::Lt => "Lt",
        Opcode::RealAffinity => "RealAffinity",
        Opcode::Add => "Add",
        Opcode::Subtract => "Subtract",
        Opcode::Multiply => "Multiply",
        Opcode::Divide => "Divide",
        Opcode::Remainder => "Remainder",
        Opcode::Not => "Not",
        Opcode::BitAnd => "BitAnd",
        Opcode::BitOr => "BitOr",
        Opcode::ShiftLeft => "ShiftLeft",
        Opcode::ShiftRight => "ShiftRight",
        Opcode::BitNot => "BitNot",
        Opcode::Concat => "Concat",
        Opcode::Cast => "Cast",
        Opcode::Function => "Function",
        Opcode::AggStep => "AggStep",
        Opcode::AggFinal => "AggFinal",
        Opcode::Integer => "Integer",
        Opcode::Int64 => "Int64",
        Opcode::Real => "Real",
        Opcode::Blob => "Blob",
        Opcode::Null => "Null",
        Opcode::String8 => "String8",
        Opcode::Variable => "Variable",
        Opcode::MakeRecord => "MakeRecord",
        Opcode::ResultRow => "ResultRow",
        Opcode::Copy => "Copy",
        Opcode::SorterOpen => "SorterOpen",
        Opcode::SorterInsert => "SorterInsert",
        Opcode::SorterSort => "SorterSort",
        Opcode::SorterNext => "SorterNext",
        Opcode::SorterData => "SorterData",
        Opcode::Sort => "Sort",
        Opcode::HashAggOpen => "HashAggOpen",
        Opcode::HashAggFind => "HashAggFind",
        Opcode::HashAggStep => "HashAggStep",
        Opcode::HashAggRewind => "HashAggRewind",
        Opcode::HashAggData => "HashAggData",
        Opcode::HashAggNext => "HashAggNext",
    }
}

/// A short, human-readable annotation of the instruction's effect,
/// used only when the instruction carries no explicit `comment`
/// (see [`explain`]'s own doc). Not required to be byte-identical to
/// sqlite3's own `EXPLAIN` comments.
fn comment_for(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> String {
    match opcode {
        Opcode::Init => format!("start at {p2}"),
        Opcode::Goto => format!("goto {p2}"),
        Opcode::OpenRead => format!("cursor {p1} on root page {p2}"),
        Opcode::OpenWrite => format!("cursor {p1} write on root page {p2}"),
        Opcode::OpenEphemeral => format!("cursor {p1} ephemeral"),
        Opcode::OpenDup => format!("cursor {p1} duplicates cursor {p2}"),
        Opcode::OpenPseudo => format!("cursor {p1} pseudo, reads r[{p2}]"),
        Opcode::Rewind => format!("cursor {p1} rewind, jump {p2} if empty"),
        Opcode::Last => format!("cursor {p1} to last row, jump {p2} if empty"),
        Opcode::Next => format!("cursor {p1} next, jump {p2} if row found"),
        Opcode::Column => format!("r[{p3}] = cursor {p1} column {p2}"),
        Opcode::Rowid => format!("r[{p2}] = cursor {p1} rowid"),
        Opcode::ResultRow => format!("output r[{p1}..{p1}+{p2}]"),
        Opcode::Eq => format!("if r[{p1}]=r[{p3}] goto {p2}"),
        Opcode::Ge => format!("if r[{p1}]>=r[{p3}] goto {p2}"),
        Opcode::Gt => format!("if r[{p1}]>r[{p3}] goto {p2}"),
        Opcode::Le => format!("if r[{p1}]<=r[{p3}] goto {p2}"),
        Opcode::Lt => format!("if r[{p1}]<r[{p3}] goto {p2}"),
        Opcode::Add => format!("r[{p3}] = r[{p1}] + r[{p2}]"),
        Opcode::Subtract => format!("r[{p3}] = r[{p2}] - r[{p1}]"),
        Opcode::Multiply => format!("r[{p3}] = r[{p1}] * r[{p2}]"),
        Opcode::Divide => format!("r[{p3}] = r[{p2}] / r[{p1}]"),
        Opcode::Remainder => format!("r[{p3}] = r[{p2}] % r[{p1}]"),
        Opcode::Function => format!("r[{p3}] = func(r[{p2}..])"),
        Opcode::Integer => format!("r[{p2}] = {p1}"),
        Opcode::Variable => format!("r[{p2}] = parameter({p1})"),
        Opcode::String8 => format!("r[{p2}] = <string>"),
        Opcode::Halt => "halt".to_string(),
        Opcode::SorterOpen => format!("cursor {p1} sorter open"),
        Opcode::SorterInsert => format!("cursor {p1} sorter insert r[{p2}]"),
        Opcode::SorterSort | Opcode::Sort => format!("cursor {p1} sort, jump {p2} if empty"),
        Opcode::SorterNext => format!("cursor {p1} sorter next, jump {p2} if row found"),
        Opcode::SorterData => format!("r[{p2}] = cursor {p1} sorted row"),
        Opcode::HashAggOpen => format!("cursor {p1} hash aggregation open"),
        Opcode::HashAggFind => format!("cursor {p1} locate group for r[{p2}]"),
        Opcode::HashAggStep => format!("cursor {p3} group accumulator {p1} += r[{p2}]"),
        Opcode::HashAggRewind => format!("cursor {p1} first group, jump {p2} if none"),
        Opcode::HashAggData => format!("r[{p2}] = cursor {p1} group row"),
        Opcode::HashAggNext => format!("cursor {p1} next group, jump {p2} if group found"),
        Opcode::Found => format!("cursor {p1} found key at r[{p3}..], jump {p2}"),
        Opcode::IdxInsert => format!("cursor {p1} insert key r[{p2}..]"),
        Opcode::IdxDelete => format!("cursor {p1} delete key r[{p2}..]"),
        Opcode::Count => format!("r[{p2}] = count of b-tree rooted at page {p1}"),
        Opcode::NoConflict => format!("cursor {p1} no matching key at r[{p3}..], jump {p2}"),
        Opcode::SeekIndexEq => {
            format!("cursor {p1} seek index key at r[{p3}..], jump {p2} if miss")
        }
        Opcode::SeekIndexGE => {
            format!("cursor {p1} seek index key >= r[{p3}..], jump {p2} if none")
        }
        Opcode::IdxCompareGT => {
            format!("cursor {p1} index key > r[{p3}..], jump {p2} if so")
        }
        Opcode::IdxRowid => format!("r[{p2}] = cursor {p1} indexed rowid"),
        Opcode::IdxRewind => format!("cursor {p1} index rewind, jump {p2} if empty"),
        Opcode::IdxLast => format!("cursor {p1} to last index entry, jump {p2} if empty"),
        Opcode::IdxNext => format!("cursor {p1} index next, jump {p2} if entry found"),
        Opcode::IdxPrev => format!("cursor {p1} index prev, jump {p2} if entry found"),
        Opcode::AutoIndexInsert => {
            format!("cursor {p1} auto-index insert key r[{p2}] rowid r[{p3}]")
        }
        Opcode::AutoIndexSeek => {
            format!("cursor {p1} auto-index seek key at r[{p3}..], jump {p2} if miss")
        }
        Opcode::AutoIndexRowid => format!("r[{p2}] = cursor {p1} auto-index rowid"),
        Opcode::AutoIndexNext => format!("cursor {p1} auto-index next, jump {p2} if row found"),
        Opcode::Insert => format!("cursor {p1} insert rowid r[{p2}] record r[{p3}]"),
        Opcode::NewRowid => format!("r[{p2}] = cursor {p1} new rowid"),
        Opcode::Delete => format!("cursor {p1} delete current row"),
        Opcode::Copy => format!("r[{p2}] = r[{p1}]"),
        _ => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::program::{AnalyzeTarget, Instruction};
    use super::*;

    #[test]
    fn explain_renders_one_row_per_instruction() {
        let program = Program::new(vec![
            Instruction::new(Opcode::Init, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = explain(&program);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].addr, 0);
        assert_eq!(rows[0].opcode, "Init");
        assert_eq!(rows[1].addr, 1);
        assert_eq!(rows[1].opcode, "Halt");
    }

    #[test]
    fn p4_renders_display_form_not_debug_bytes() {
        let program = Program::new(vec![Instruction::with_p4(
            Opcode::String8,
            0,
            1,
            0,
            P4::Str("g%".to_string()),
        )]);
        let rows = explain(&program);
        assert_eq!(rows[0].p4, "g%");
    }

    #[test]
    fn explicit_instruction_comment_wins_over_the_computed_one() {
        let program = Program::new(vec![
            Instruction::new(Opcode::Goto, 0, 7, 0).with_comment("loop back to the top")
        ]);
        let rows = explain(&program);
        assert_eq!(rows[0].comment, "loop back to the top");
    }

    #[test]
    fn opcode_name_covers_all_variants() {
        let cases: &[(Opcode, &str)] = &[
            (Opcode::Init, "Init"),
            (Opcode::Goto, "Goto"),
            (Opcode::Once, "Once"),
            (Opcode::BeginSubrtn, "BeginSubrtn"),
            (Opcode::Return, "Return"),
            (Opcode::Halt, "Halt"),
            (Opcode::Transaction, "Transaction"),
            (Opcode::AutoCommit, "AutoCommit"),
            (Opcode::SetJournalMode, "SetJournalMode"),
            (Opcode::IntegrityCheck, "IntegrityCheck"),
            (Opcode::Synchronous, "Synchronous"),
            (Opcode::IfNot, "IfNot"),
            (Opcode::IfNotZero, "IfNotZero"),
            (Opcode::IfPos, "IfPos"),
            (Opcode::DecrJumpZero, "DecrJumpZero"),
            (Opcode::IsNull, "IsNull"),
            (Opcode::NotNull, "NotNull"),
            (Opcode::MustBeInt, "MustBeInt"),
            (Opcode::OffsetLimit, "OffsetLimit"),
            (Opcode::OpenRead, "OpenRead"),
            (Opcode::OpenWrite, "OpenWrite"),
            (Opcode::OpenEphemeral, "OpenEphemeral"),
            (Opcode::OpenDup, "OpenDup"),
            (Opcode::OpenPseudo, "OpenPseudo"),
            (Opcode::Rewind, "Rewind"),
            (Opcode::Last, "Last"),
            (Opcode::Next, "Next"),
            (Opcode::Column, "Column"),
            (Opcode::Rowid, "Rowid"),
            (Opcode::SeekRowid, "SeekRowid"),
            (Opcode::NullRow, "NullRow"),
            (Opcode::Sequence, "Sequence"),
            (Opcode::Found, "Found"),
            (Opcode::IdxInsert, "IdxInsert"),
            (Opcode::IdxDelete, "IdxDelete"),
            (Opcode::Count, "Count"),
            (Opcode::NoConflict, "NoConflict"),
            (Opcode::IdxLE, "IdxLE"),
            (Opcode::AutoIndexInsert, "AutoIndexInsert"),
            (Opcode::AutoIndexSeek, "AutoIndexSeek"),
            (Opcode::AutoIndexRowid, "AutoIndexRowid"),
            (Opcode::AutoIndexNext, "AutoIndexNext"),
            (Opcode::Delete, "Delete"),
            (Opcode::Insert, "Insert"),
            (Opcode::NewRowid, "NewRowid"),
            (Opcode::CreateTable, "CreateTable"),
            (Opcode::CreateView, "CreateView"),
            (Opcode::DropTable, "DropTable"),
            (Opcode::CreateIndex, "CreateIndex"),
            (Opcode::DropIndex, "DropIndex"),
            (Opcode::Analyze, "Analyze"),
            (Opcode::Eq, "Eq"),
            (Opcode::Ge, "Ge"),
            (Opcode::Gt, "Gt"),
            (Opcode::Le, "Le"),
            (Opcode::Lt, "Lt"),
            (Opcode::RealAffinity, "RealAffinity"),
            (Opcode::Add, "Add"),
            (Opcode::Subtract, "Subtract"),
            (Opcode::Multiply, "Multiply"),
            (Opcode::Divide, "Divide"),
            (Opcode::Remainder, "Remainder"),
            (Opcode::Not, "Not"),
            (Opcode::BitAnd, "BitAnd"),
            (Opcode::BitOr, "BitOr"),
            (Opcode::ShiftLeft, "ShiftLeft"),
            (Opcode::ShiftRight, "ShiftRight"),
            (Opcode::BitNot, "BitNot"),
            (Opcode::Concat, "Concat"),
            (Opcode::Cast, "Cast"),
            (Opcode::Function, "Function"),
            (Opcode::Integer, "Integer"),
            (Opcode::Int64, "Int64"),
            (Opcode::Real, "Real"),
            (Opcode::Blob, "Blob"),
            (Opcode::Null, "Null"),
            (Opcode::String8, "String8"),
            (Opcode::Variable, "Variable"),
            (Opcode::MakeRecord, "MakeRecord"),
            (Opcode::ResultRow, "ResultRow"),
            (Opcode::SorterOpen, "SorterOpen"),
            (Opcode::SorterInsert, "SorterInsert"),
            (Opcode::SorterSort, "SorterSort"),
            (Opcode::SorterNext, "SorterNext"),
            (Opcode::SorterData, "SorterData"),
            (Opcode::Sort, "Sort"),
            (Opcode::HashAggOpen, "HashAggOpen"),
            (Opcode::HashAggFind, "HashAggFind"),
            (Opcode::HashAggStep, "HashAggStep"),
            (Opcode::HashAggRewind, "HashAggRewind"),
            (Opcode::HashAggData, "HashAggData"),
            (Opcode::HashAggNext, "HashAggNext"),
        ];
        let program = Program::new(
            cases
                .iter()
                .map(|(op, _)| Instruction::new(*op, 0, 0, 0))
                .collect(),
        );
        let rows = explain(&program);
        for (row, (_, expected)) in rows.iter().zip(cases.iter()) {
            assert_eq!(row.opcode, *expected);
        }
    }

    #[test]
    fn render_p4_covers_scalar_variants() {
        let program = Program::new(vec![
            Instruction::with_p4(Opcode::Integer, 0, 0, 0, P4::None),
            Instruction::with_p4(Opcode::Integer, 0, 0, 0, P4::Int(42)),
            Instruction::with_p4(Opcode::Real, 0, 0, 0, P4::Real(3.5)),
            Instruction::with_p4(Opcode::Blob, 0, 0, 0, P4::Blob(b"abc".to_vec())),
            Instruction::with_p4(
                Opcode::Eq,
                0,
                0,
                0,
                P4::CollSeq {
                    collation: Collation::Binary,
                    affinity: 65,
                },
            ),
            Instruction::with_p4(
                Opcode::Eq,
                0,
                0,
                0,
                P4::CollSeq {
                    collation: Collation::NoCase,
                    affinity: 65,
                },
            ),
            Instruction::with_p4(
                Opcode::Eq,
                0,
                0,
                0,
                P4::CollSeq {
                    collation: Collation::RTrim,
                    affinity: 65,
                },
            ),
            Instruction::with_p4(Opcode::MakeRecord, 0, 0, 0, P4::Affinity(b"BC".to_vec())),
        ]);
        let rows = explain(&program);
        assert_eq!(rows[0].p4, "");
        assert_eq!(rows[1].p4, "42");
        assert_eq!(rows[2].p4, "3.5");
        assert_eq!(rows[3].p4, "abc");
        assert_eq!(rows[4].p4, "BINARY-65");
        assert_eq!(rows[5].p4, "NOCASE-65");
        assert_eq!(rows[6].p4, "RTRIM-65");
        assert_eq!(rows[7].p4, "BC");
    }

    #[test]
    fn render_p4_covers_sort_key_and_group_key() {
        let program = Program::new(vec![
            Instruction::with_p4(
                Opcode::SorterOpen,
                0,
                0,
                0,
                P4::SortKey(vec![
                    SortKeyColumn {
                        index: 0,
                        descending: true,
                        collation: Collation::Binary,
                        nulls_first: false,
                    },
                    SortKeyColumn {
                        index: 1,
                        descending: false,
                        collation: Collation::NoCase,
                        nulls_first: false,
                    },
                ]),
            ),
            Instruction::with_p4(
                Opcode::HashAggOpen,
                0,
                0,
                0,
                P4::GroupKey(vec![
                    GroupKeyColumn {
                        index: 0,
                        collation: Collation::Binary,
                    },
                    GroupKeyColumn {
                        index: 1,
                        collation: Collation::RTrim,
                    },
                ]),
            ),
        ]);
        let rows = explain(&program);
        assert_eq!(rows[0].p4, "k(2,-B,N)");
        assert_eq!(rows[1].p4, "g(2,B,R)");
    }

    #[test]
    fn render_p4_covers_ddl_variants() {
        let program = Program::new(vec![
            Instruction::with_p4(
                Opcode::CreateTable,
                0,
                0,
                0,
                P4::CreateTable {
                    name: "t".to_string(),
                    sql: "CREATE TABLE t(a)".to_string(),
                },
            ),
            Instruction::with_p4(
                Opcode::CreateView,
                0,
                0,
                0,
                P4::CreateView {
                    name: "v".to_string(),
                    sql: "CREATE VIEW v AS SELECT 1".to_string(),
                },
            ),
            Instruction::with_p4(
                Opcode::DropTable,
                0,
                0,
                0,
                P4::DropTable {
                    name: "t".to_string(),
                    root_page: 2,
                    indexes: vec![],
                },
            ),
            Instruction::with_p4(
                Opcode::CreateIndex,
                0,
                0,
                0,
                P4::CreateIndex {
                    name: "idx".to_string(),
                    table_name: "t".to_string(),
                    table_root_page: 2,
                    sql: "CREATE INDEX idx ON t(a)".to_string(),
                    column_indices: vec![0],
                    unique: false,
                },
            ),
            Instruction::with_p4(
                Opcode::DropIndex,
                0,
                0,
                0,
                P4::DropIndex {
                    name: "idx".to_string(),
                    root_page: 3,
                },
            ),
            Instruction::with_p4(
                Opcode::Analyze,
                0,
                0,
                0,
                P4::Analyze {
                    targets: vec![AnalyzeTarget {
                        table_name: "t".to_string(),
                        table_root_page: 2,
                        indexes: vec![],
                    }],
                },
            ),
        ]);
        let rows = explain(&program);
        assert_eq!(rows[0].p4, "t: CREATE TABLE t(a)");
        assert_eq!(rows[1].p4, "v: CREATE VIEW v AS SELECT 1");
        assert_eq!(rows[2].p4, "t");
        assert_eq!(rows[3].p4, "idx: CREATE INDEX idx ON t(a)");
        assert_eq!(rows[4].p4, "idx");
        assert_eq!(rows[5].p4, "t");
    }

    #[test]
    fn comment_for_covers_all_annotated_opcodes() {
        let program = Program::new(vec![
            Instruction::new(Opcode::Init, 0, 5, 0),
            Instruction::new(Opcode::Goto, 0, 7, 0),
            Instruction::new(Opcode::OpenRead, 1, 2, 0),
            Instruction::new(Opcode::OpenWrite, 1, 2, 0),
            Instruction::new(Opcode::OpenEphemeral, 1, 0, 0),
            Instruction::new(Opcode::OpenPseudo, 1, 2, 0),
            Instruction::new(Opcode::Rewind, 1, 9, 0),
            Instruction::new(Opcode::Last, 1, 9, 0),
            Instruction::new(Opcode::Next, 1, 9, 0),
            Instruction::new(Opcode::Column, 1, 2, 3),
            Instruction::new(Opcode::Rowid, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Eq, 1, 2, 3),
            Instruction::new(Opcode::Ge, 1, 2, 3),
            Instruction::new(Opcode::Gt, 1, 2, 3),
            Instruction::new(Opcode::Le, 1, 2, 3),
            Instruction::new(Opcode::Lt, 1, 2, 3),
            Instruction::new(Opcode::Add, 1, 2, 3),
            Instruction::new(Opcode::Subtract, 1, 2, 3),
            Instruction::new(Opcode::Multiply, 1, 2, 3),
            Instruction::new(Opcode::Divide, 1, 2, 3),
            Instruction::new(Opcode::Remainder, 1, 2, 3),
            Instruction::new(Opcode::Function, 1, 2, 3),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::Variable, 1, 2, 0),
            Instruction::new(Opcode::String8, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::SorterOpen, 1, 0, 0),
            Instruction::new(Opcode::SorterInsert, 1, 2, 0),
            Instruction::new(Opcode::SorterSort, 1, 9, 0),
            Instruction::new(Opcode::Sort, 1, 9, 0),
            Instruction::new(Opcode::SorterNext, 1, 9, 0),
            Instruction::new(Opcode::SorterData, 1, 2, 0),
            Instruction::new(Opcode::HashAggOpen, 1, 0, 0),
            Instruction::new(Opcode::HashAggFind, 1, 2, 0),
            Instruction::new(Opcode::HashAggStep, 1, 2, 3),
            Instruction::new(Opcode::HashAggRewind, 1, 9, 0),
            Instruction::new(Opcode::HashAggData, 1, 2, 0),
            Instruction::new(Opcode::HashAggNext, 1, 9, 0),
            Instruction::new(Opcode::Found, 1, 2, 3),
            Instruction::new(Opcode::IdxInsert, 1, 2, 0),
            Instruction::new(Opcode::IdxDelete, 1, 2, 0),
            Instruction::new(Opcode::Insert, 1, 2, 3),
            Instruction::new(Opcode::NewRowid, 1, 2, 0),
            Instruction::new(Opcode::Delete, 1, 0, 0),
            Instruction::new(Opcode::Not, 0, 0, 0),
        ]);
        let rows = explain(&program);
        let comments: Vec<&str> = rows.iter().map(|r| r.comment.as_str()).collect();
        assert_eq!(
            comments,
            vec![
                "start at 5",
                "goto 7",
                "cursor 1 on root page 2",
                "cursor 1 write on root page 2",
                "cursor 1 ephemeral",
                "cursor 1 pseudo, reads r[2]",
                "cursor 1 rewind, jump 9 if empty",
                "cursor 1 to last row, jump 9 if empty",
                "cursor 1 next, jump 9 if row found",
                "r[3] = cursor 1 column 2",
                "r[2] = cursor 1 rowid",
                "output r[1..1+2]",
                "if r[1]=r[3] goto 2",
                "if r[1]>=r[3] goto 2",
                "if r[1]>r[3] goto 2",
                "if r[1]<=r[3] goto 2",
                "if r[1]<r[3] goto 2",
                "r[3] = r[1] + r[2]",
                "r[3] = r[2] - r[1]",
                "r[3] = r[1] * r[2]",
                "r[3] = r[2] / r[1]",
                "r[3] = r[2] % r[1]",
                "r[3] = func(r[2..])",
                "r[2] = 1",
                "r[2] = parameter(1)",
                "r[2] = <string>",
                "halt",
                "cursor 1 sorter open",
                "cursor 1 sorter insert r[2]",
                "cursor 1 sort, jump 9 if empty",
                "cursor 1 sort, jump 9 if empty",
                "cursor 1 sorter next, jump 9 if row found",
                "r[2] = cursor 1 sorted row",
                "cursor 1 hash aggregation open",
                "cursor 1 locate group for r[2]",
                "cursor 3 group accumulator 1 += r[2]",
                "cursor 1 first group, jump 9 if none",
                "r[2] = cursor 1 group row",
                "cursor 1 next group, jump 9 if group found",
                "cursor 1 found key at r[3..], jump 2",
                "cursor 1 insert key r[2..]",
                "cursor 1 delete key r[2..]",
                "cursor 1 insert rowid r[2] record r[3]",
                "r[2] = cursor 1 new rowid",
                "cursor 1 delete current row",
                "",
            ]
        );
    }
}
