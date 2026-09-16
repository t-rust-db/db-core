//! Cross-segment orchestration over the batch executor (ADR 0007).
//!
//! [`super::batch::Vm`] executes a program against *one* segment's batch.
//! A planned program's trailing `Combine [Sort] [Limit]` sequence
//! (db-core#48; `Combine` merges per-segment partial aggregates,
//! `Sort`/`Limit` apply the final `ORDER BY`/`LIMIT`) is a barrier: it
//! needs every segment's output, so it cannot run inside the per-segment
//! loop (where the VM treats each of those three as a no-op control
//! opcode). [`run`] is the entry point that splits a [`Program`] at that
//! trailing sequence, runs the body per segment via
//! [`run_parallel`]/[`run_parallel_top_n`] (or a sequential bounded scan
//! when the plan is a bare `LIMIT`), and applies the merge/sort/limit
//! once over the concatenated output.
//!
//! The merge/sort/limit logic itself ([`finalize`]) is column-rs's former
//! `query::post_process`, moved here unchanged -- it never touched storage.
//! Likewise [`run_join`]/[`run_join_segments`], the two-phase
//! `HashBuild`/`HashProbe` driver: the build side is named by a
//! [`super::batch::ScanSource`] (ADR 0024, #382) rather than always being
//! an already-materialized table, resolved via [`ScanSourceResolver`] when
//! it isn't. And [`semi_filter`].

use super::batch::{
    compare_for_order, run_parallel, run_parallel_top_n, AggPart, Batch, JoinTables, Opcode,
    Program, QueryOutput, Result, ScanSource, Segment, TopN, Value, Vm, VmError,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Materializes a cross-mode join's build side (ADR 0024, #382/#385): a
/// [`ScanSource::RowTable`] or [`ScanSource::Stream`] names *where* to read
/// from, but `vm::engine` cannot reach a SQLite table or a stream engine
/// itself without inverting the `engine` -> `vm` layering (ADR 0001) --
/// the caller supplies this (`engine::resolve`, which already depends on
/// both `vm::batch` and the row/stream engines). Never called for
/// [`ScanSource::InMemory`] -- see [`resolve_scan_source`].
pub trait ScanSourceResolver {
    /// Materializes `source` (never [`ScanSource::InMemory`]) into a [`Batch`].
    fn resolve(&self, source: &ScanSource) -> Result<Batch>;
}

/// Any closure of this shape is a [`ScanSourceResolver`] -- lets a caller
/// (`engine::resolve`) hand `run_join_segments` a resolver that borrows its
/// enclosing function's locals via closure capture, with no named lifetime
/// spelled out anywhere (the qualified subset, `make check-mvl-limit`,
/// forbids explicit lifetimes beyond function-scoped elision, which a
/// hand-written `struct Resolver<'a> { .. }` would need).
impl<F: Fn(&ScanSource) -> Result<Batch>> ScanSourceResolver for F {
    fn resolve(&self, source: &ScanSource) -> Result<Batch> {
        self(source)
    }
}

/// A [`ScanSourceResolver`] that always errors -- for callers that only
/// ever hand [`run_join`]/[`run_join_segments`] a [`ScanSource::InMemory`]
/// build side (an already-materialized [`Batch`], as [`run_join`] itself
/// always supplies) and so have no real source to resolve.
pub struct NoResolver;

impl ScanSourceResolver for NoResolver {
    fn resolve(&self, source: &ScanSource) -> Result<Batch> {
        Err(VmError::MalformedProgram {
            opcode: "ScanSource",
            reason: format!("no resolver supplied to materialize {source:?}"),
        })
    }
}

/// Materializes `source` into a [`Batch`]: [`ScanSource::InMemory`] is
/// already one (no resolver call needed); anything else is handed to
/// `resolver`. Generic, not `&dyn ScanSourceResolver` -- the qualified
/// subset (`make check-mvl-limit`) forbids dynamic dispatch.
fn resolve_scan_source<R: ScanSourceResolver>(source: ScanSource, resolver: &R) -> Result<Batch> {
    match source {
        ScanSource::InMemory(batch) => Ok(batch),
        other => resolver.resolve(&other),
    }
}

/// A [`Segment`] over an already-materialized [`Batch`] -- for the join/
/// semi-join/window paths, which build one in-memory table and then run
/// the flat program over it as a single segment.
pub struct InMemorySegment(pub Batch);

impl Segment for InMemorySegment {
    fn load(&self) -> Result<Arc<Batch>> {
        // #264: `Batch::clone` is a `HashMap<String, Arc<Vec<Value>>>`
        // clone (refcount bumps) since columns are `Arc`-shared, not a
        // per-cell copy -- cheap even though this allocates a fresh
        // `Batch`/`Arc` per call.
        Ok(Arc::new(self.0.clone()))
    }
}

/// Run `program` over `segments`: body per segment, then the trailing
/// `Combine`/`Sort`/`Limit` sequence once (see module docs). A program
/// with no `Combine` is a plain per-segment concatenation, exactly like
/// [`run_parallel`].
///
/// Two plan shapes short-circuit the general path, both decided from the
/// instruction stream alone:
/// - a bare `LIMIT` (no `Filter`, no aggregates, no `ORDER BY`) scans
///   segments sequentially in order and stops once `limit` rows are
///   collected, so later segments are never loaded (#108);
/// - `ORDER BY ... LIMIT ...` without aggregates runs as a bounded top-N
///   per segment and at the merge (#109) instead of materializing every
///   row before the final sort.
pub fn run<S: Segment>(segments: &[S], program: &Program) -> Result<QueryOutput> {
    let (body, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        num_group_keys,
        distinct,
    }) = combine
    else {
        return run_parallel(segments, &body);
    };
    let order_by = match sort {
        Some(Opcode::Sort { col, descending }) => Some((*col, *descending)),
        _ => None,
    };
    let limit = match limit_op {
        Some(Opcode::Limit { n }) => Some(*n),
        _ => None,
    };

    if let Some(limit) = bounded_scan_limit(program) {
        return bounded_scan(segments, &body, limit);
    }
    // #436: `Combine` is always emitted, but for a plain scan/filter/
    // projection it has nothing to do -- `finalize` would be the identity.
    // Return the chunked output as-is rather than transposing every
    // surviving row into a `Vec` and back (which is what the reverted
    // first attempt did on exactly this path, the one the issue is about).
    if agg_parts.is_empty() && !distinct && order_by.is_none() && limit.is_none() {
        return run_parallel(segments, &body);
    }
    let output = match (agg_parts.is_empty() && !distinct, order_by, limit) {
        (true, Some((col, descending)), Some(limit)) => run_parallel_top_n(
            segments,
            &body,
            &TopN {
                col,
                descending,
                limit,
            },
        )?,
        _ => run_parallel(segments, &body)?,
    };
    finalize(
        agg_parts,
        *num_group_keys,
        *distinct,
        order_by,
        limit,
        output.into_rows(),
    )
}

/// The `LIMIT` when `program` can be satisfied by a sequential prefix scan
/// (#108): a trailing `Combine` with no aggregates and no `Sort`, followed
/// by a `Limit`, and no [`Opcode::Filter`] in the body -- i.e. the first
/// `limit` rows of the first however-many segments *are* the answer.
pub fn bounded_scan_limit(program: &Program) -> Option<usize> {
    let (body, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        distinct,
        ..
    }) = combine
    else {
        return None;
    };
    if sort.is_some() {
        return None;
    }
    let Some(Opcode::Limit { n }) = limit_op else {
        return None;
    };
    if *distinct
        || !agg_parts.is_empty()
        || body.iter().any(|op| matches!(op, Opcode::Filter { .. }))
    {
        return None;
    }
    Some(*n)
}

/// Sequentially scan `segments` in order, running `body` against each
/// one's freshly-loaded batch, stopping (and truncating to exactly `limit`
/// rows) as soon as enough have been collected -- segments past that
/// point are never loaded.
fn bounded_scan<S: Segment>(segments: &[S], body: &[Opcode], limit: usize) -> Result<QueryOutput> {
    let mut output = QueryOutput::default();
    for segment in segments {
        if output.num_rows() >= limit {
            break;
        }
        let batch = segment.load()?;
        let mut vm = Vm::new();
        vm.execute(&batch, body)?;
        output.extend(vm.take_output());
    }
    output.truncate(limit);
    Ok(output)
}

/// Apply `Combine`/`Sort`/`Limit`'s semantics to a flat row list: merge
/// rows sharing a group key per `agg_parts`, then `ORDER BY`, then `LIMIT`.
/// Shared by every execution path, and callable directly with `const`
/// data by an AOT-emitted binary.
#[allow(
    clippy::indexing_slicing,
    reason = "`num_group_keys`, `order_by` and `agg_parts` positions were resolved by codegen against the same row width every emitted row carries; `groups[i]` indices come from `index`, which only stores positions already pushed"
)]
pub fn finalize(
    agg_parts: &[AggPart],
    num_group_keys: usize,
    distinct: bool,
    order_by: Option<(usize, bool)>,
    limit: Option<usize>,
    rows: Vec<Vec<Value>>,
) -> Result<QueryOutput> {
    let mut result_rows = if !agg_parts.is_empty() {
        let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in rows {
            let key: Vec<Value> = row[..num_group_keys].to_vec();
            let key_str = key
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\u{0}");
            match index.get(&key_str) {
                Some(&i) => merge_rows(agg_parts, &mut groups[i].1, &row)?,
                None => {
                    index.insert(key_str, groups.len());
                    groups.push((key, row));
                }
            }
        }
        groups
            .into_iter()
            .map(|(_, row)| finalize_row(agg_parts, row))
            .collect::<Result<Vec<_>>>()?
    } else {
        rows
    };

    // `DISTINCT` dedups the fully-projected output rows: for a plain
    // `SELECT DISTINCT` this is the only dedup pass (agg_parts is empty
    // above); combined with `GROUP BY`, the group-key merge above already
    // collapsed rows to one per group, so this second pass only catches
    // coincidental duplicate output rows across distinct groups (e.g. a
    // SELECT list that omits some GROUP BY columns) -- matching DuckDB's
    // semantics of dedup applied after the hash-aggregate. Must run before
    // `ORDER BY`/`LIMIT` per standard SQL evaluation order.
    if distinct {
        let mut seen: HashSet<String> = HashSet::new();
        result_rows.retain(|row| {
            let key = row
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\u{0}");
            seen.insert(key)
        });
    }

    if let Some((pos, descending)) = order_by {
        result_rows.sort_by(|a, b| compare_for_order(&a[pos], &b[pos], descending));
    }

    if let Some(limit) = limit {
        result_rows.truncate(limit);
    }

    Ok(QueryOutput::from_rows(result_rows))
}

/// Combine two emitted rows for the same group key, applying the
/// associative merge appropriate to each [`AggPart`].
///
/// `row_idx` tracks the row cursor directly rather than `parts`'
/// enumeration index: an `AVG` occupies two `row` registers (sum, count)
/// but only one `AggPart` entry, so every part after it is offset in
/// `row` by however many extra registers came before it. Indexing by the
/// enumeration position instead (as this used to) silently merges the
/// wrong registers into each other once an `AVG` precedes another
/// aggregate in the same query -- confirmed live by db-core#404's
/// segment-split invariance harness, which is also why `finalize_row`
/// (below) uses the identical cursor.
#[allow(
    clippy::indexing_slicing,
    reason = "`row_idx` (and `Avg`'s own `sum_i`/`count_i`) always stay in `row`'s \
              range by construction (db-core#404)"
)]
fn merge_rows(parts: &[AggPart], into: &mut [Value], from: &[Value]) -> Result<()> {
    let mut row_idx = 0usize;
    for part in parts {
        match part {
            AggPart::GroupKey => {
                row_idx = row_idx.saturating_add(1);
            }
            AggPart::Sum => {
                into[row_idx] = merge_sum_partials(&into[row_idx], &from[row_idx])?;
                row_idx = row_idx.saturating_add(1);
            }
            // A merged COUNT stays an integer, as a single segment's does
            // (#272): before, it came back as `Float`, so the *type* of
            // `COUNT(*)` depended on how many segments the scan had.
            AggPart::Count => {
                let total = partial_i64(&into[row_idx])?
                    .checked_add(partial_i64(&from[row_idx])?)
                    .ok_or_else(|| VmError::MalformedProgram {
                        opcode: "Combine",
                        reason: "partial COUNT overflowed i64".to_string(),
                    })?;
                into[row_idx] = Value::Int(total);
                row_idx = row_idx.saturating_add(1);
            }
            AggPart::Min => {
                if let (Some(a), Some(b)) = (into[row_idx].as_f64(), from[row_idx].as_f64()) {
                    into[row_idx] = Value::Float(a.min(b));
                } else if matches!(into[row_idx], Value::Null) {
                    into[row_idx] = from[row_idx].clone();
                }
                row_idx = row_idx.saturating_add(1);
            }
            AggPart::Max => {
                if let (Some(a), Some(b)) = (into[row_idx].as_f64(), from[row_idx].as_f64()) {
                    into[row_idx] = Value::Float(a.max(b));
                } else if matches!(into[row_idx], Value::Null) {
                    into[row_idx] = from[row_idx].clone();
                }
                row_idx = row_idx.saturating_add(1);
            }
            // Previously a no-op (db-core#404): with more than one
            // segment, `AVG` silently returned the first segment's local
            // average instead of the true merged mean. Both registers are
            // read via `partial_f64`, matching `finalize_row`'s own read
            // of them below -- the per-segment count register is a
            // `Value::Int` in real execution, but it is never surfaced on
            // its own (only ever divided into by `finalize_row`), so
            // there is no reason to route it through the
            // integer-overflow-checked `AggPart::Count` path as well.
            AggPart::Avg(sum_i, count_i) => {
                into[*sum_i] = merge_sum_partials(&into[*sum_i], &from[*sum_i])?;
                into[*count_i] =
                    Value::Float(partial_f64(&into[*count_i])? + partial_f64(&from[*count_i])?);
                row_idx = count_i.saturating_add(1);
            }
        }
    }
    Ok(())
}

/// A partial COUNT slot as an integer. NULL is the additive identity (a
/// segment that saw no rows); anything else non-integer is a planner bug.
fn partial_i64(v: &Value) -> Result<i64> {
    match v {
        Value::Null => Ok(0),
        Value::Int(n) => Ok(*n),
        other => Err(VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("partial COUNT slot holds {other:?}, not an integer"),
        }),
    }
}

/// A partial SUM/COUNT/AVG slot as a number. NULL is the additive identity
/// (a segment that saw no rows); anything else non-numeric is a planner
/// bug -- before, it silently merged as `0.0` into a plausible wrong total
/// (db-core#232).
/// Merges two partial `SUM`s (also `AVG`'s sum slot). A segment with no
/// surviving rows emits `Null` (#452: `Reduce` always emits one row), and
/// `Null` must be the identity here -- `Null` with `Null` stays `Null`, so
/// a `SUM` over zero rows across every segment is `NULL` as SQL requires,
/// not `0.0`; `Null` with a number is that number.
fn merge_sum_partials(into: &Value, from: &Value) -> Result<Value> {
    match (into, from) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Ok(Value::Float(partial_f64(into)? + partial_f64(from)?)),
    }
}

fn partial_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Null => Ok(0.0),
        other => other.as_f64().ok_or_else(|| VmError::MalformedProgram {
            opcode: "Combine",
            reason: format!("partial aggregate slot holds {other:?}, not a number"),
        }),
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "`row_idx` tracks the row cursor directly (not the `parts` \
              enumeration index), advancing by 2 over an `Avg`'s sum/count \
              registers and by 1 otherwise, so it always stays in `row`'s range"
)]
fn finalize_row(parts: &[AggPart], row: Vec<Value>) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(parts.len());
    let mut row_idx = 0usize;
    for part in parts {
        match part {
            // `Avg`'s two registers (sum, count) occupy one `parts` entry
            // but two `row` slots; every part after it is offset in `row`
            // by however many extra registers came before -- comparing
            // `count_i` against the *parts* enumeration index (as this
            // used to) confuses the two spaces and, once another
            // aggregate follows an `AVG` in the same query, silently
            // corrupts or drops it (db-core#404).
            AggPart::Avg(sum_i, count_i) => {
                let (sum, count) = (partial_f64(&row[*sum_i])?, partial_f64(&row[*count_i])?);
                out.push(if count == 0.0 {
                    Value::Null
                } else {
                    Value::Float(sum / count)
                });
                row_idx = count_i.saturating_add(1);
            }
            _ => {
                out.push(row[row_idx].clone());
                row_idx = row_idx.saturating_add(1);
            }
        }
    }
    Ok(out)
}

/// A planned two-table equi-join, as produced by
/// `crate::codegen::batch::compile_join` and driven by [`run_join`]: which
/// columns each side must materialize (in register order), the build and
/// probe programs, where the probe lands the build side's payload, and the
/// flat body (ending in [`Opcode::Combine`], optionally followed by
/// `Sort`/`Limit`) to run over the joined batch.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinProgram {
    /// Left (probe-side) column names, in the register order the probe program loads them.
    pub left_columns: Vec<String>,
    /// Right (build-side) column names carried as join payload, in `payload_dst` order.
    pub right_columns: Vec<String>,
    /// Program run over the right table to populate the hash table.
    pub build: Program,
    /// Program run over the left table to probe the hash table and emit matches.
    pub probe: Program,
    /// Register per `right_columns` entry into which the probe writes that payload column.
    pub payload_dst: Vec<usize>,
    /// The flat query body (ending in `Finalize`) run over the joined batch.
    pub body: Program,
    /// #441: when `Some`, `probe`'s last opcode is
    /// `Opcode::HashProbeGroupReduce` instead of `Opcode::HashProbe` --
    /// the join's output feeds only a `GROUP BY`/aggregate, so it was
    /// fused into the probe itself rather than materializing a joined
    /// row per match. Lists `(register, synthetic column name)` for each
    /// of that opcode's `group_by`/`agg_dst` output registers;
    /// `left_columns`/`right_columns`/`payload_dst` are unused in this
    /// case (there is no per-row joined output left to reshape), and
    /// `body` is just the original compiled body's suffix *after* its
    /// `GroupReduce` (`Emit`, `Combine`, ...), prefixed with `LoadColumn`s
    /// that read these synthetic names back into the same registers the
    /// original `GroupReduce` would have written.
    pub fused_group_by: Option<Vec<(usize, String)>>,
}

/// Execute a [`JoinProgram`] over two fully materialized tables: run the
/// build program on `right`, the probe program on `left`, assemble the
/// joined batch (left columns then right payload), and run `body` over it
/// as a single in-memory segment via [`run`]. Both sides are already
/// `Batch`es, so no [`ScanSourceResolver`] is needed (see [`NoResolver`]).
pub fn run_join(left: &Batch, right: &Batch, plan: &JoinProgram) -> Result<QueryOutput> {
    run_join_segments(
        vec![InMemorySegment(left.clone())],
        ScanSource::InMemory(right.clone()),
        plan,
        &NoResolver,
    )
}

/// [`run_join`] with the probe (left) side as segments (#272): the build
/// side runs once, then `probe ++ body` runs per left segment through the
/// same morsel-driven [`run`] every single-table query uses -- parallel
/// across segments, with the trailing `Combine`/`Sort`/`Limit` merging the
/// per-segment aggregates. Nothing materializes the joined table: each
/// [`JoinedSegment`] hands its probe registers straight to the body as a
/// [`Batch`] (moved, not copied).
///
/// `right` names where the build side comes from (ADR 0024, #382/#385): an
/// already-materialized [`ScanSource::InMemory`] batch needs no resolver
/// (see [`run_join`]); a [`ScanSource::RowTable`]/[`ScanSource::Stream`]
/// is materialized by `resolver`, supplied by the caller since `vm::engine`
/// cannot reach a SQLite table or a stream engine itself.
///
/// Takes `left` by value so a segment can be wrapped without a borrow
/// (the qualified subset forbids the struct lifetime that would need).
pub fn run_join_segments<S: Segment, R: ScanSourceResolver>(
    left: Vec<S>,
    right: ScanSource,
    plan: &JoinProgram,
    resolver: &R,
) -> Result<QueryOutput> {
    let right_batch = resolve_scan_source(right, resolver)?;
    let build: Vec<Opcode> = plan.build.opcodes().cloned().collect();
    let mut builder = Vm::new();
    builder.execute(&right_batch, &build)?;
    let tables = builder.join_tables();

    let shape = Arc::new(JoinShape {
        probe: plan.probe.opcodes().cloned().collect(),
        left_columns: plan.left_columns.clone(),
        right_columns: plan.right_columns.clone(),
        payload_dst: plan.payload_dst.clone(),
        fused_group_by: plan.fused_group_by.clone(),
    });
    let segments: Vec<JoinedSegment<S>> = left
        .into_iter()
        .map(|segment| JoinedSegment {
            left: segment,
            tables: tables.clone(),
            shape: Arc::clone(&shape),
        })
        .collect();
    run(&segments, &plan.body)
}

/// The per-plan, read-only part every [`JoinedSegment`] of one join shares.
struct JoinShape {
    probe: Vec<Opcode>,
    left_columns: Vec<String>,
    right_columns: Vec<String>,
    payload_dst: Vec<usize>,
    /// #441: see [`JoinProgram::fused_group_by`].
    fused_group_by: Option<Vec<(usize, String)>>,
}

/// A left segment plus the already-built join table: [`Segment::load`]
/// loads the left batch, probes it, and returns the joined batch for the
/// body -- the join happens inside the load, per segment, on whichever
/// worker thread [`run_parallel`] hands it to.
struct JoinedSegment<S: Segment> {
    left: S,
    tables: JoinTables,
    shape: Arc<JoinShape>,
}

impl<S: Segment> Segment for JoinedSegment<S> {
    fn load(&self) -> Result<Arc<Batch>> {
        let batch = self.left.load()?;
        let mut vm = Vm::with_join_tables(self.tables.clone());
        vm.execute(&batch, &self.shape.probe)?;

        // #441: a fused join has no per-row joined output to reshape --
        // `self.shape.probe` already ends in `Opcode::HashProbeGroupReduce`,
        // whose `group_by`/`agg_dst` registers (one value per *group*, not
        // per matched row) are exactly what `fused_group_by` names.
        if let Some(fused) = &self.shape.fused_group_by {
            let num_rows = match fused.first() {
                Some((reg, _)) => vm.register(*reg)?.len(),
                None => 0,
            };
            let mut joined = Batch::new(num_rows);
            for (reg, name) in fused {
                joined
                    .columns
                    .insert(name.clone(), Arc::new(vm.take_register(*reg)?));
            }
            return Ok(Arc::new(joined));
        }

        let num_rows = vm.register(0)?.len();
        let mut joined = Batch::new(num_rows);
        for (reg, name) in self.shape.left_columns.iter().enumerate() {
            joined
                .columns
                .insert(name.clone(), Arc::new(vm.take_register(reg)?));
        }
        for (name, &reg) in self.shape.right_columns.iter().zip(&self.shape.payload_dst) {
            joined
                .columns
                .insert(name.clone(), Arc::new(vm.take_register(reg)?));
        }
        Ok(Arc::new(joined))
    }
}

/// One `JOIN` target's build side within a [`MultiJoinProgram`] (#394): the
/// SQLite lookup table's name, the payload columns it carries (in
/// `payload_dst` order), the program that hashes it into `table: <index in
/// MultiJoinProgram::builds>`, and where the shared probe program lands
/// that payload.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinBuildSide {
    /// The build side's table name (for diagnostics; not read by execution).
    pub table_name: String,
    /// Build-side column names carried as join payload, in `payload_dst` order.
    pub right_columns: Vec<String>,
    /// Program run over this table to populate its hash table (`table:
    /// <index>`, matching this side's position in `MultiJoinProgram::builds`).
    pub build: Program,
    /// Registers in the shared probe program this build's payload lands in.
    pub payload_dst: Vec<usize>,
}

/// A star join (#394): one driving/probe table joined to N SQLite lookup
/// tables, each via its own `HashBuild`/`HashProbe` pair (`table: 0..N`).
/// Unlike [`JoinProgram`], every join key is a column of the driving side
/// (`engine::resolve`'s `resolve_multi_sides` enforces this) -- no lookup
/// table is joined against another lookup table's payload, so every build
/// runs independently and the probe program chains their `HashProbe`s in
/// `builds` order.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiJoinProgram {
    /// Driving-side column names, in the register order the probe program loads them.
    pub left_columns: Vec<String>,
    /// One entry per `JOIN` clause, in query order == `table` index order.
    pub builds: Vec<JoinBuildSide>,
    /// Program run over the driving side: loads `left_columns`, then probes
    /// every build's hash table in order.
    pub probe: Program,
    /// The flat query body (ending in `Finalize`) run over the joined batch.
    pub body: Program,
}

/// [`run_join_segments`] generalized to N build sides (#394): each of
/// `rights` is resolved and hashed in turn (`table: 0, 1, .., N-1`, matching
/// `plan.builds`' order) into one shared [`JoinTables`], via
/// [`super::batch::Vm::clear_registers`] between builds so each build's
/// registers don't leak into the next -- built tables persist across that
/// call, only registers/selection reset. The probe side then runs exactly
/// like [`run_join_segments`], with `plan.probe` chaining one `HashProbe`
/// per build instead of one.
///
/// `rights.len()` must equal `plan.builds.len()`; mismatched lengths are a
/// caller (planner) bug, surfaced as [`VmError::MalformedProgram`] rather
/// than a panic or a silently truncated join.
pub fn run_multi_join_segments<S: Segment, R: ScanSourceResolver>(
    left: Vec<S>,
    rights: Vec<ScanSource>,
    plan: &MultiJoinProgram,
    resolver: &R,
) -> Result<QueryOutput> {
    if rights.len() != plan.builds.len() {
        return Err(VmError::MalformedProgram {
            opcode: "HashBuild",
            reason: format!(
                "{} build sources supplied for {} planned joins",
                rights.len(),
                plan.builds.len()
            ),
        });
    }

    let mut builder = Vm::new();
    for (i, (right, build_side)) in rights.into_iter().zip(&plan.builds).enumerate() {
        if i > 0 {
            builder.clear_registers();
        }
        let right_batch = resolve_scan_source(right, resolver)?;
        let build: Vec<Opcode> = build_side.build.opcodes().cloned().collect();
        builder.execute(&right_batch, &build)?;
    }
    let tables = builder.join_tables();

    let shape = Arc::new(MultiJoinShape {
        probe: plan.probe.opcodes().cloned().collect(),
        left_columns: plan.left_columns.clone(),
        builds: plan
            .builds
            .iter()
            .map(|b| (b.right_columns.clone(), b.payload_dst.clone()))
            .collect(),
    });
    let segments: Vec<MultiJoinedSegment<S>> = left
        .into_iter()
        .map(|segment| MultiJoinedSegment {
            left: segment,
            tables: tables.clone(),
            shape: Arc::clone(&shape),
        })
        .collect();
    run(&segments, &plan.body)
}

/// The per-plan, read-only part every [`MultiJoinedSegment`] of one
/// multi-join shares -- [`JoinShape`] generalized to N builds.
struct MultiJoinShape {
    probe: Vec<Opcode>,
    left_columns: Vec<String>,
    /// One `(right_columns, payload_dst)` per build, in `table` index order.
    builds: Vec<(Vec<String>, Vec<usize>)>,
}

/// [`JoinedSegment`] generalized to N builds (#394): probes every build's
/// hash table via the shared chained probe program, then assembles the
/// joined batch from the driving columns followed by each build's payload,
/// in `builds` order.
struct MultiJoinedSegment<S: Segment> {
    left: S,
    tables: JoinTables,
    shape: Arc<MultiJoinShape>,
}

impl<S: Segment> Segment for MultiJoinedSegment<S> {
    fn load(&self) -> Result<Arc<Batch>> {
        let batch = self.left.load()?;
        let mut vm = Vm::with_join_tables(self.tables.clone());
        vm.execute(&batch, &self.shape.probe)?;

        let num_rows = vm.register(0)?.len();
        let mut joined = Batch::new(num_rows);
        for (reg, name) in self.shape.left_columns.iter().enumerate() {
            joined
                .columns
                .insert(name.clone(), Arc::new(vm.take_register(reg)?));
        }
        for (right_columns, payload_dst) in &self.shape.builds {
            for (name, &reg) in right_columns.iter().zip(payload_dst) {
                joined
                    .columns
                    .insert(name.clone(), Arc::new(vm.take_register(reg)?));
            }
        }
        Ok(Arc::new(joined))
    }
}

/// Keep only the rows of `batch` whose `key_column` value (stringified)
/// appears in `allowed` -- the `WHERE col IN (SELECT ...)` semi-join
/// filter, applied before the flat body runs over the survivors.
#[allow(
    clippy::indexing_slicing,
    reason = "every column in a `Batch` holds `num_rows` values and `keep` was drawn from `0..num_rows`"
)]
pub fn semi_filter(batch: &Batch, key_column: &str, allowed: &HashSet<String>) -> Result<Batch> {
    let key = batch
        .columns
        .get(key_column)
        .ok_or_else(|| VmError::UnknownColumn {
            opcode: "SemiFilter",
            column: key_column.to_string(),
        })?;
    let keep: Vec<usize> = (0..batch.num_rows)
        .filter(|&i| key.get(i).is_some_and(|v| allowed.contains(&v.to_string())))
        .collect();

    let mut filtered = Batch::new(keep.len());
    for (name, column) in &batch.columns {
        let values: Vec<Value> = keep.iter().map(|&i| column[i].clone()).collect();
        filtered.columns.insert(name.clone(), Arc::new(values));
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::batch::AggFunc;
    use crate::vm::batch::Instruction;

    fn seg(rows: &[(i64, i64)]) -> InMemorySegment {
        let batch = Batch::new(rows.len())
            .with_column("k", rows.iter().map(|(k, _)| Value::Int(*k)).collect())
            .with_column("v", rows.iter().map(|(_, v)| Value::Int(*v)).collect());
        InMemorySegment(batch)
    }

    /// `tail` is the trailing sequential-phase opcode sequence (empty,
    /// `[Combine]`, `[Combine, Sort]`, `[Combine, Limit]`, or `[Combine,
    /// Sort, Limit]`) -- callers build it directly rather than through
    /// `codegen::batch::compile`, since these tests exercise `engine::run`
    /// in isolation from the planner.
    fn group_sum_program(tail: Vec<Opcode>) -> Program {
        let mut instructions = vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::LoadColumn {
                reg: 1,
                column: "v".into(),
            }),
            Instruction::new(Opcode::GroupReduce {
                group_by: vec![0].into(),
                aggs: vec![(AggFunc::Sum, Some(1))].into(),
                agg_dst: vec![2].into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0, 2].into(),
            }),
        ];
        instructions.extend(tail.into_iter().map(Instruction::new));
        Program::new(instructions)
    }

    #[test]
    fn finalize_merges_partial_group_aggregates_across_segments() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let program = group_sum_program(vec![
            Opcode::Combine {
                agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
            },
            Opcode::Sort {
                col: 0,
                descending: false,
            },
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Float(13.0)],
                vec![Value::Int(2), Value::Float(5.0)],
            ]
        );
    }

    #[test]
    fn program_without_finalize_is_plain_concatenation() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let program = group_sum_program(vec![]);
        let rows = run(&segments, &program).unwrap();
        // Per-segment partials, unmerged: (1,10),(2,5) then (1,3).
        assert_eq!(rows.len(), 3);
    }

    fn scan_program(tail: Vec<Opcode>, with_filter: bool) -> Program {
        let mut instructions = vec![Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "k".into(),
        })];
        if with_filter {
            instructions.push(Instruction::new(Opcode::Filter { predicate: 0 }));
        }
        instructions.push(Instruction::new(Opcode::Emit {
            registers: vec![0].into(),
        }));
        instructions.extend(tail.into_iter().map(Instruction::new));
        Program::new(instructions)
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_88675f4d__v1_distinct_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: true,
                },
                Opcode::Limit { n: 5 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_88675f4d__v2_non_empty_agg_parts_disqualifies_bounded_scan(
    ) {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![AggPart::Sum].into(),
                    num_group_keys: 0,
                    distinct: false,
                },
                Opcode::Limit { n: 5 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_88675f4d__v3_filter_in_body_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                },
                Opcode::Limit { n: 5 },
            ],
            true,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_88675f4d__v4_no_distinct_no_aggs_no_filter_allows_bounded_scan(
    ) {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                },
                Opcode::Limit { n: 5 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), Some(5));
    }

    #[test]
    fn bare_limit_stops_before_loading_later_segments() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        struct Counting(Batch);
        impl Segment for Counting {
            fn load(&self) -> Result<Arc<Batch>> {
                LOADS.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(self.0.clone()))
            }
        }
        let mk = |n: i64| -> Counting {
            Counting(Batch::new(2).with_column("k", vec![Value::Int(n), Value::Int(n + 1)]))
        };
        let segments = vec![mk(0), mk(10), mk(20)];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Combine {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
            }),
            Instruction::new(Opcode::Limit { n: 3 }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            LOADS.load(Ordering::SeqCst),
            2,
            "third segment never loaded"
        );
    }

    #[test]
    fn distinct_dedups_rows_across_segments() {
        // Two segments each carry a duplicate (k=1,v=0) row; DISTINCT should
        // collapse them to one, keeping the other rows untouched.
        let segments = vec![seg(&[(1, 0), (2, 0)]), seg(&[(1, 0), (3, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Combine {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: true,
            }),
            Instruction::new(Opcode::Sort {
                col: 0,
                descending: false,
            }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)],
            ]
        );
    }

    #[test]
    fn distinct_dedups_before_order_by_and_limit() {
        // A naive "sort/limit first, dedup later" plan would keep both
        // copies of k=1 if limit truncated before dedup ran; the correct
        // order (dedup, then sort, then limit) collapses them first.
        let segments = vec![seg(&[(1, 0), (1, 0), (1, 0)]), seg(&[(2, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Combine {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: true,
            }),
            Instruction::new(Opcode::Sort {
                col: 0,
                descending: false,
            }),
            Instruction::new(Opcode::Limit { n: 2 }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }

    #[test]
    fn distinct_with_group_by_does_not_collapse_genuinely_distinct_groups() {
        // Same shape as `finalize_merges_partial_group_aggregates_across_segments`
        // but with `distinct: true` -- the GROUP BY merge already yields one
        // row per key (1 -> 13, 2 -> 5), and since those two output rows
        // aren't equal, DISTINCT's post-aggregate dedup pass must leave both.
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let program = group_sum_program(vec![
            Opcode::Combine {
                agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: true,
            },
            Opcode::Sort {
                col: 0,
                descending: false,
            },
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Float(13.0)],
                vec![Value::Int(2), Value::Float(5.0)],
            ]
        );
    }

    #[test]
    fn order_by_limit_without_aggregates_takes_top_n() {
        let segments = vec![seg(&[(3, 0), (1, 0)]), seg(&[(2, 0), (0, 0)])];
        let program = Program::new(vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::Emit {
                registers: vec![0].into(),
            }),
            Instruction::new(Opcode::Combine {
                agg_parts: vec![].into(),
                num_group_keys: 0,
                distinct: false,
            }),
            Instruction::new(Opcode::Sort {
                col: 0,
                descending: true,
            }),
            Instruction::new(Opcode::Limit { n: 2 }),
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(2)]]);
    }

    #[test]
    fn finalize_avg_divides_sum_by_count_and_handles_nulls() {
        let rows = vec![
            vec![Value::Int(1), Value::Float(10.0), Value::Float(2.0)],
            vec![Value::Int(1), Value::Float(5.0), Value::Float(1.0)],
        ];
        let out = finalize(
            &[AggPart::GroupKey, AggPart::Avg(1, 2)],
            1,
            false,
            None,
            None,
            rows,
        )
        .unwrap();
        assert_eq!(out, vec![vec![Value::Int(1), Value::Float(5.0)]]);
    }

    #[test]
    fn program_derives_columns_to_load_and_splits_trailing_finalize() {
        let program = group_sum_program(vec![Opcode::Combine {
            agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
            num_group_keys: 1,
            distinct: false,
        }]);
        assert_eq!(program.columns_to_load(), vec!["k", "v"]);
        let (body, combine, sort, limit) = program.split_finalize();
        assert_eq!(body.len(), 4);
        assert!(matches!(combine, Some(Opcode::Combine { .. })));
        assert!(sort.is_none());
        assert!(limit.is_none());
        assert!(matches!(body.last(), Some(Opcode::Emit { .. })));

        let plain = Program::from_opcodes(body.clone());
        let (body2, combine2, sort2, limit2) = plain.split_finalize();
        assert_eq!(body2, body);
        assert!(combine2.is_none());
        assert!(sort2.is_none());
        assert!(limit2.is_none());
        assert_eq!(plain.len(), 4);
        assert!(!plain.is_empty());
        assert!(plain.get(0).is_some() && plain.get(4).is_none());
    }

    #[test]
    fn per_segment_vm_treats_combine_sort_limit_as_no_ops() {
        let batch = Batch::new(1).with_column("k", vec![Value::Int(1)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                },
                Opcode::Sort {
                    col: 0,
                    descending: false,
                },
                Opcode::Limit { n: 0 },
            ],
        )
        .unwrap();
        assert!(vm.take_output().is_empty());
    }

    #[test]
    fn semi_filter_keeps_only_allowed_keys() {
        let batch = Batch::new(3)
            .with_column("k", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column("v", vec![Value::Int(10), Value::Int(20), Value::Int(30)]);
        let allowed: HashSet<String> = ["1", "3"].iter().map(|s| s.to_string()).collect();
        let out = semi_filter(&batch, "k", &allowed).unwrap();
        assert_eq!(out.num_rows, 2);
        assert_eq!(
            out.columns["v"].as_slice(),
            &[Value::Int(10), Value::Int(30)]
        );
        assert!(semi_filter(&batch, "nope", &allowed).is_err());
    }
}
