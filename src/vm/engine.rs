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

use std::sync::Mutex;

use super::batch::{
    chunk_len, compare_rows_for_order, execute_in_slices, execute_in_slices_folding,
    hash_group_key, int_fold_applies, run_morsels, run_morsels_fold, run_parallel,
    run_parallel_streaming, run_parallel_top_n, sliceable, AggPart, Batch, Chunk, GroupKey,
    IntGroupFold, JoinTables, Opcode, Program, QueryOutput, Result, ScanSource, Segment, TopN,
    Value, Vm, VmError, SLICE_ROWS,
};
use super::combine::{
    combine_chunks, combine_partials, eval_agg_expr, finish_avg, finish_int_set, finish_sketch,
    finish_str_set, merge_slot, order_key, slot_ops, Combiner, Partial, SlotOp,
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
    let mut output = run_page(segments, program)?;
    // #725: a `CASE`/`COALESCE` column types its branches by the values of
    // each segment; one type for the whole column, as DuckDB gives it.
    let (body, combine, ..) = program.split_finalize();
    for column in branch_output_columns(&body, combine) {
        output.unify_numeric_column("Select", column)?;
    }
    // `LIMIT n OFFSET m` (#679): every path above kept the first `n + m`
    // rows; the first `m` are not part of the page.
    match program.instructions.last().map(|i| &i.opcode) {
        Some(Opcode::Limit { offset, .. }) if *offset > 0 => Ok(output.skip_rows(*offset)),
        _ => Ok(output),
    }
}

/// The output columns that are the result of an [`Opcode::Select`] (a
/// `CASE`, a `COALESCE`): those `Emit`ted, by the program, or by the
/// projection of its `Combine`. Aggregates and group keys are not.
fn branch_output_columns(body: &[Opcode], combine: Option<&Opcode>) -> Vec<usize> {
    let ops: &[Opcode] = match combine {
        None => body,
        Some(Opcode::Combine { project, .. }) if !project.is_empty() => project,
        // A projection without aggregates or reordering: the `Emit` is the
        // output.
        Some(Opcode::Combine {
            agg_parts,
            output_order,
            ..
        }) if agg_parts.is_empty() && output_order.is_empty() => body,
        Some(_) => return Vec::new(),
    };
    let Some(Opcode::Emit { registers }) = ops
        .iter()
        .rev()
        .find(|op| matches!(op, Opcode::Emit { .. }))
    else {
        return Vec::new();
    };
    registers
        .iter()
        .enumerate()
        .filter(|(_, reg)| {
            ops.iter()
                .any(|op| matches!(op, Opcode::Select { dst, .. } if dst == *reg))
        })
        .map(|(column, _)| column)
        .collect()
}

/// [`run`] up to the end of the page: a `LIMIT n OFFSET m` keeps the first
/// `n + m` rows, and [`run`] drops the first `m`.
fn run_page<S: Segment>(segments: &[S], program: &Program) -> Result<QueryOutput> {
    let (body, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        num_group_keys,
        distinct,
        output_order,
        project,
    }) = combine
    else {
        return run_parallel(segments, &body);
    };
    let (order_by, hidden): (&[(usize, bool)], usize) = match sort {
        Some(Opcode::Sort { keys, hidden }) => (keys, *hidden),
        _ => (&[], 0),
    };
    let limit = match limit_op {
        // Every path keeps the first `n + offset` rows; `run` drops the
        // first `offset` of them at the end (#679).
        Some(Opcode::Limit { n, offset }) => Some(n.saturating_add(*offset)),
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
    if is_identity_finalize(agg_parts, *distinct, output_order, project, order_by, limit) {
        return run_parallel(segments, &body);
    }
    // #478/#488: aggregates never go through `run_parallel` + a row
    // transpose. Each pool worker pre-aggregates every segment it claims
    // into one partial (`vm::combine::Combiner`), so the final merge sees
    // at most `threads x groups` rows instead of `segments x groups`, and
    // merges them column-major. Only the DISTINCT/ORDER BY/LIMIT tail,
    // whose input is bounded by the number of surviving groups, still
    // goes through the row-based `finalize`.
    if !agg_parts.is_empty() {
        if segments.is_empty() && *num_group_keys == 0 {
            // An aggregate without GROUP BY is one row even over no rows
            // (`count` 0, the others NULL), and that row comes from a
            // segment's `Reduce`: with every row group pruned, or a file
            // with none, run the body once over an empty batch.
            let empty = program
                .columns_to_load()
                .into_iter()
                .fold(Batch::new(0), |batch, column| {
                    batch.with_column(column, Vec::new())
                });
            return run_page(&[InMemorySegment(empty)], program);
        }
        let combined = if preaggregation_pays(segments.len()) {
            let partials = run_parallel_preaggregate(segments, &body, agg_parts, *num_group_keys)?;
            combine_partials(agg_parts, *num_group_keys, partials)?
        } else {
            let output =
                run_parallel_segment_partials(segments, &body, agg_parts, *num_group_keys)?;
            combine_chunks(agg_parts, *num_group_keys, output.chunks())?
        };
        let combined = apply_projection(combined.project(output_order)?, project)?;
        if !*distinct && order_by.is_empty() && limit.is_none() {
            return Ok(combined);
        }
        return finalize(&[], 0, *distinct, order_by, limit, combined.into_rows());
    }
    // The parallel top-N keeps one key per row; several keys sort the
    // whole result in `finalize` (#645).
    let output = match (agg_parts.is_empty() && !distinct, order_by, limit) {
        (true, &[(col, descending)], Some(limit)) => run_parallel_top_n(
            segments,
            &body,
            &TopN {
                col,
                descending,
                limit,
            },
        )?,
        _ => run_parallel(segments, &body)?,
    }
    .project(output_order)?;
    let output = apply_projection(output, project)?;
    let output = finalize(
        agg_parts,
        *num_group_keys,
        *distinct,
        order_by,
        limit,
        output.into_rows(),
    )?;
    // The columns `ORDER BY` needed beyond the select list (#645).
    let keep: Vec<usize> = (0..output.num_columns().saturating_sub(hidden)).collect();
    if hidden == 0 || keep.is_empty() {
        return Ok(output);
    }
    output.project(&keep)
}

/// Segments per worker below which `run()` merges the per-segment chunks
/// directly instead of pre-aggregating per worker (#488). Pre-aggregation
/// costs each worker a copy of its first chunk into an accumulator and the
/// final merge a sort of the groups back into first-seen order; it pays
/// only when a worker folds several segments' partials into one. Every
/// engine gates thread-local aggregation the same way (DuckDB above ~10K
/// groups per thread, ClickHouse above 100K keys): measured here, 16
/// segments over ~10 threads was 14% *slower* pre-aggregated, 64 segments
/// clearly faster.
const PREAGGREGATE_MIN_SEGMENTS_PER_THREAD: usize = 4;

/// Groups a worker's pre-aggregation may hold between segments before it
/// spills them into the shared combiner (#643). Above the 100K groups a
/// `GROUP BY <key>` over a customer-sized key makes, so that query's
/// workers keep their groups and merge once at the end rather than after
/// every segment (#863); it bounds a worker at this many groups plus one
/// segment's.
const SPILL_GROUPS: usize = 131_072;

/// Groups a worker's final partial may hold before it is merged into the
/// shared partitions by the worker itself, in parallel, instead of by the
/// one thread of the final `Combine` (#863).
const HANDOFF_GROUPS: usize = 4_096;

/// Key-hash partitions of the shared combiner a spill merges into, each
/// under its own lock, so workers spilling at once rarely wait on one
/// another (#863).
const SPILL_PARTITIONS: usize = 32;

/// Whether `num_segments` gives each worker enough segments to make
/// per-worker pre-aggregation worthwhile.
fn preaggregation_pays(num_segments: usize) -> bool {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    num_segments >= threads.saturating_mul(PREAGGREGATE_MIN_SEGMENTS_PER_THREAD)
}

/// Runs `body` over every segment on the morsel pool and pre-aggregates
/// per worker (#488): each worker folds the `GroupReduce` output of every
/// segment it claims into its own [`Combiner`], and hands back one
/// still-mergeable partial -- `(chunk, first-seen keys)` -- when the pool
/// drains. `combine_partials` then merges at most `threads x groups`
/// rows. The per-segment `GroupReduce` itself is unchanged; this is
/// DuckDB's / Velox's thread-local pre-aggregation applied at the merge
/// boundary rather than inside the VM.
pub(crate) fn run_parallel_preaggregate<S: Segment>(
    segments: &[S],
    body: &[Opcode],
    agg_parts: &[AggPart],
    num_group_keys: usize,
) -> Result<Vec<Partial>> {
    let items: Vec<(usize, &S)> = segments.iter().enumerate().collect();
    // A worker that has seen more than `SPILL_GROUPS` groups hands them to
    // one shared combiner and starts over, so the pool holds the groups
    // once plus a bounded partial per worker, not every worker's copy of
    // them (#643).
    let shared: Vec<Mutex<Combiner>> = (0..SPILL_PARTITIONS)
        .map(|_| Mutex::new(Combiner::new(agg_parts, num_group_keys)))
        .collect();
    let mut partials = run_morsels_fold(
        &items,
        || Worker {
            combiner: Combiner::new(agg_parts, num_group_keys),
            fold: None,
        },
        |worker, (segment_index, segment)| {
            let batch = segment.load()?;
            let Worker { combiner, fold } = worker;
            if int_fold_applies(body, &batch) {
                // A single `Int` key: this worker's groups fold across its
                // segments, one hash per row (#894). Checked after every
                // slice, so the fold never holds more than a slice's
                // groups over the spill threshold.
                let int_fold = fold.get_or_insert_with(IntGroupFold::default);
                execute_in_slices_folding(
                    &batch,
                    body,
                    int_fold,
                    *segment_index,
                    |output, int_fold| {
                        if let Some((offset, output)) = output {
                            for chunk in output.into_chunks() {
                                combiner.push_chunk(&chunk, |row| {
                                    order_key(*segment_index, offset.saturating_add(row))
                                })?;
                            }
                        }
                        if int_fold.groups() > SPILL_GROUPS {
                            flush_fold(Some(int_fold.take()), combiner, body)?;
                        }
                        Ok(())
                    },
                )?;
            } else {
                execute_in_slices(&batch, body, |offset, output| {
                    for chunk in output.into_chunks() {
                        combiner.push_chunk(&chunk, |row| {
                            order_key(*segment_index, offset.saturating_add(row))
                        })?;
                    }
                    Ok(())
                })?;
            }
            if combiner.groups() > SPILL_GROUPS {
                spill(
                    &shared,
                    std::mem::replace(combiner, Combiner::new(agg_parts, num_group_keys)),
                    num_group_keys,
                )?;
            }
            Ok(())
        },
        // The worker's last groups go the same way, in parallel, so the
        // final merge sees disjoint partitions, not every worker's copy
        // of the groups (#863).
        |worker| {
            let Worker {
                mut combiner, fold, ..
            } = worker;
            flush_fold(fold, &mut combiner, body)?;
            if combiner.groups() > HANDOFF_GROUPS {
                spill(&shared, combiner, num_group_keys)?;
                return Ok(Partial::default());
            }
            Ok(combiner.finish_partial())
        },
    )?;
    partials.extend(shared.into_iter().map(|partition| {
        partition
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_partial()
    }));
    Ok(partials)
}

/// A pre-aggregating worker's state: its combiner, and the groups of a
/// single-`Int`-key `GROUP BY` it has folded across its segments but not
/// yet handed to the combiner (#894).
struct Worker {
    combiner: Combiner,
    fold: Option<IntGroupFold>,
}

/// Hands `fold`'s groups, finalized as the segments' own partial rows
/// would be, to `combiner`.
fn flush_fold(
    fold: Option<IntGroupFold>,
    combiner: &mut Combiner,
    program: &[Opcode],
) -> Result<()> {
    let Some(fold) = fold.filter(|fold| fold.groups() > 0) else {
        return Ok(());
    };
    let (output, first_seen) = fold.finish(program)?;
    let mut base = 0usize;
    for chunk in output.into_chunks() {
        let rows = chunk_len(&chunk);
        combiner.push_chunk(&chunk, |row| {
            first_seen
                .get(base.saturating_add(row))
                .copied()
                .unwrap_or(u64::MAX)
        })?;
        base = base.saturating_add(rows);
    }
    Ok(())
}

/// Merges `combiner`'s groups into the shared `partitions`, each group
/// into the one its key hash selects, under that partition's lock only.
fn spill(partitions: &[Mutex<Combiner>], combiner: Combiner, num_group_keys: usize) -> Result<()> {
    let spilled = combiner.finish_partial();
    for (partition, rows) in partitions
        .iter()
        .zip(spilled.partition_rows(num_group_keys, partitions.len()))
        .filter(|(_, rows)| !rows.is_empty())
    {
        // Poison recovery: a panicking worker re-raises when the pool's
        // scope closes, so the state is never used after.
        partition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge_partial_rows(&spilled, &rows)?;
    }
    Ok(())
}

/// [`run_parallel`] for an aggregate body that is not pre-aggregated per
/// worker: one partial chunk per segment, as before slicing -- a segment
/// run in slices folds its slices' partials into one first, so the merge
/// never sees a group once per slice (#643).
fn run_parallel_segment_partials<S: Segment>(
    segments: &[S],
    body: &[Opcode],
    agg_parts: &[AggPart],
    num_group_keys: usize,
) -> Result<QueryOutput> {
    let per_segment: Vec<Result<QueryOutput>> = run_morsels(segments, |segment| {
        let batch = segment.load()?;
        if batch.num_rows <= SLICE_ROWS || !sliceable(body) {
            let mut vm = Vm::new();
            vm.execute(&batch, body)?;
            return Ok(vm.take_output());
        }
        let mut combiner = Combiner::new(agg_parts, num_group_keys);
        execute_in_slices(&batch, body, |offset, output| {
            for chunk in output.into_chunks() {
                combiner.push_chunk(&chunk, |row| order_key(0, offset.saturating_add(row)))?;
            }
            Ok(())
        })?;
        let chunk = combiner.finish_partial().into_chunk();
        // No slice emitted a chunk (every row filtered out): no partial.
        Ok(if chunk.is_empty() {
            QueryOutput::default()
        } else {
            QueryOutput::from_chunk(chunk)
        })
    });
    let mut all = QueryOutput::default();
    for output in per_segment {
        all.extend(output?);
    }
    Ok(all)
}

/// Whether `program`'s finalize step (a `Combine` plus optional `Sort`/
/// `Limit`) is the identity -- no aggregates, no `DISTINCT`, no `ORDER BY`,
/// no `LIMIT` -- so its chunked output can be returned (or streamed) as-is.
/// Shared by [`run`] and [`run_streaming`] so the two entry points agree
/// on exactly which programs skip finalize (#456).
fn is_identity_finalize(
    agg_parts: &[AggPart],
    distinct: bool,
    output_order: &[usize],
    project: &[Opcode],
    order_by: &[(usize, bool)],
    limit: Option<usize>,
) -> bool {
    agg_parts.is_empty()
        && !distinct
        && output_order.is_empty()
        && project.is_empty()
        && order_by.is_empty()
        && limit.is_none()
}

/// Runs `Combine`'s post-aggregation `project` program (#623) over every
/// merged chunk: the chunk's columns become a batch named `#0`, `#1`, ...
/// by position, and the program's `Emit` is the output. Empty `project` is
/// the identity. The columns are shared (`Arc`), not copied.
fn apply_projection(output: QueryOutput, project: &[Opcode]) -> Result<QueryOutput> {
    if project.is_empty() {
        return Ok(output);
    }
    let mut projected = QueryOutput::default();
    for chunk in output.into_chunks() {
        let mut batch = Batch::new(chunk_len(&chunk));
        for (i, column) in chunk.into_iter().enumerate() {
            batch.columns.insert(format!("#{i}"), column);
        }
        let mut vm = Vm::new();
        vm.execute(&batch, project)?;
        projected.extend(vm.take_output());
    }
    Ok(projected)
}

/// Whether [`run_streaming`] can stream `program` at all -- i.e. whether
/// it would return `Ok` rather than [`VmError::NotStreamable`], without
/// running anything. A caller that wants to commit to a row-by-row
/// consumer (print a header, hand out a writer) before the first chunk
/// arrives needs this decided up front: `run_streaming` itself only
/// discovers a non-streamable shape after it has already started
/// producing output for the streamable cases, which is too late for a
/// caller that already committed (#456).
pub fn is_streamable(program: &Program) -> bool {
    let (_, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        distinct,
        output_order,
        project,
        ..
    }) = combine
    else {
        return true;
    };
    let order_by: &[(usize, bool)] = match sort {
        Some(Opcode::Sort { keys, .. }) => keys,
        _ => &[],
    };
    let limit = match limit_op {
        // Every path keeps the first `n + offset` rows; `run` drops the
        // first `offset` of them at the end (#679).
        Some(Opcode::Limit { n, offset }) => Some(n.saturating_add(*offset)),
        _ => None,
    };
    is_identity_finalize(agg_parts, *distinct, output_order, project, order_by, limit)
}

/// Streaming counterpart to [`run`] (#456): hands each segment's chunks to
/// `sink`, in segment order, as soon as they are ready, instead of
/// collecting the whole result into one [`QueryOutput`] first. Peak memory
/// is bounded by the in-flight window (see
/// [`super::batch::run_parallel_streaming`]) plus whatever `sink` retains
/// -- not by the result size, which is `run`'s tradeoff for a plain scan/
/// filter/projection at result sizes where materializing is the cost.
///
/// Only programs [`is_identity_finalize`] accepts can stream: an aggregate,
/// `DISTINCT`, `ORDER BY`, or `LIMIT` finalize step needs every segment's
/// output before it can produce its first (and often only) row, which is
/// exactly what streaming exists to avoid ever holding. Anything else
/// returns [`VmError::NotStreamable`] -- a typed error, not a panic -- so
/// the caller can fall back to `run`.
pub fn run_streaming<S: Segment>(
    segments: &[S],
    program: &Program,
    sink: impl FnMut(Chunk) -> Result<()>,
) -> Result<()> {
    let (body, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        distinct,
        output_order,
        project,
        ..
    }) = combine
    else {
        return run_parallel_streaming(segments, &body, sink);
    };
    let order_by: &[(usize, bool)] = match sort {
        Some(Opcode::Sort { keys, .. }) => keys,
        _ => &[],
    };
    let limit = match limit_op {
        // Every path keeps the first `n + offset` rows; `run` drops the
        // first `offset` of them at the end (#679).
        Some(Opcode::Limit { n, offset }) => Some(n.saturating_add(*offset)),
        _ => None,
    };
    if !is_identity_finalize(agg_parts, *distinct, output_order, project, order_by, limit) {
        let reason = if !agg_parts.is_empty() {
            "query has aggregates".to_string()
        } else if *distinct {
            "query is DISTINCT".to_string()
        } else {
            match (!order_by.is_empty(), limit) {
                (true, Some(_)) => "query has ORDER BY ... LIMIT".to_string(),
                (true, None) => "query has ORDER BY".to_string(),
                (false, Some(_)) => "query has LIMIT".to_string(),
                // Only a non-empty `output_order` or `project` is left
                // (#585, #623); codegen sets one only alongside
                // aggregates, but a hand-built program can carry one on
                // its own.
                (false, None) => "query reorders or projects its output columns".to_string(),
            }
        };
        return Err(VmError::NotStreamable { reason });
    }
    run_parallel_streaming(segments, &body, sink)
}

/// The `LIMIT` when `program` can be satisfied by a sequential prefix scan
/// (#108): a trailing `Combine` with no aggregates and no `Sort`, followed
/// by a `Limit`, and no [`Opcode::Filter`] in the body -- i.e. the first
/// `limit` rows of the first however-many segments *are* the answer. A
/// `Combine` that reorders or projects its rows (#585, #623) is not a
/// prefix scan either: the bounded scan returns the body's rows as-is.
pub fn bounded_scan_limit(program: &Program) -> Option<usize> {
    let (body, combine, sort, limit_op) = program.split_finalize();
    let Some(Opcode::Combine {
        agg_parts,
        distinct,
        output_order,
        project,
        ..
    }) = combine
    else {
        return None;
    };
    if sort.is_some() {
        return None;
    }
    let Some(Opcode::Limit { n, offset }) = limit_op else {
        return None;
    };
    if *distinct
        || !agg_parts.is_empty()
        || !output_order.is_empty()
        || !project.is_empty()
        || body.iter().any(|op| matches!(op, Opcode::Filter { .. }))
    {
        return None;
    }
    // The rows up to the end of the page; `run` skips the offset.
    Some(n.saturating_add(*offset))
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
    order_by: &[(usize, bool)],
    limit: Option<usize>,
    rows: Vec<Vec<Value>>,
) -> Result<QueryOutput> {
    // `COUNT(DISTINCT x)` groups twice (#633); `Combiner` holds that
    // logic once, so the row form goes through it.
    if agg_parts.contains(&AggPart::DistinctKey) {
        let width = rows.first().map_or(0, Vec::len);
        let mut columns: Vec<Vec<Value>> = vec![Vec::with_capacity(rows.len()); width];
        for row in rows {
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        let chunk: Chunk = columns.into_iter().map(Arc::new).collect();
        let chunks = if width == 0 { Vec::new() } else { vec![chunk] };
        let merged = combine_chunks(agg_parts, num_group_keys, &chunks)?;
        return finalize(&[], 0, distinct, order_by, limit, merged.into_rows());
    }
    let mut result_rows = if !agg_parts.is_empty() {
        // #478: a typed, variant-tagged hash over the raw `Value`s
        // (`GroupKey`'s scheme, #263/#266) instead of stringifying+joining
        // every group-key column per row -- at 100K groups x 82 segments
        // the per-row `Vec<String>`/`join`/SipHash-over-string cost was the
        // entire query. Hash-then-verify (buckets of row indices sharing a
        // hash, exact `Vec<Value>` equality on collision) rather than a
        // plain `HashMap<GroupKey, _>`: the overwhelming majority of rows
        // land in a group already seen (8.1M of 8.2M here), so probing
        // with a *borrowed* key slice via `hash_group_key` -- allocating
        // an owned `GroupKey`/`Vec<Value>` only for the rare new-group
        // case -- avoids a heap allocation on almost every row. `Null ==
        // Null` still matches (`GROUP BY` semantics, unlike a join key).
        let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
        let ops = slot_ops(agg_parts, rows.first().map_or(0, Vec::len))?;
        for row in rows {
            let key_slice = &row[..num_group_keys];
            let hash = hash_group_key(key_slice);
            let bucket = index.entry(hash).or_default();
            let existing = bucket.iter().copied().find(|&i| groups[i].0 == *key_slice);
            match existing {
                Some(i) => merge_rows(&ops, &mut groups[i].1, &row)?,
                None => {
                    bucket.push(groups.len());
                    groups.push((key_slice.to_vec(), row));
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
        let mut seen: HashSet<GroupKey> = HashSet::new();
        result_rows.retain(|row| seen.insert(GroupKey(row.clone())));
    }

    if !order_by.is_empty() {
        result_rows.sort_by(|a, b| compare_rows_for_order(a, b, order_by));
    }

    if let Some(limit) = limit {
        result_rows.truncate(limit);
    }

    Ok(QueryOutput::from_rows(result_rows))
}

/// Combine two emitted rows for the same group key: one [`SlotOp`] per
/// slot, resolved once by `slot_ops` (#404: an `AVG` is one part but two
/// slots, so every part after it is offset -- resolving by slot position
/// rather than part index is what keeps the right registers merging into
/// each other). The merge semantics themselves live in `vm::combine`,
/// shared with the column-major path.
fn merge_rows(ops: &[SlotOp], into: &mut [Value], from: &[Value]) -> Result<()> {
    for ((op, into), from) in ops.iter().zip(into.iter_mut()).zip(from) {
        merge_slot(*op, into, from)?;
    }
    Ok(())
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
                out.push(finish_avg(&row[*sum_i], &row[*count_i])?);
                row_idx = count_i.saturating_add(1);
            }
            // The merged sketch becomes its estimate (#603).
            AggPart::ApproxCountDistinct => {
                out.push(finish_sketch(&row[row_idx])?);
                row_idx = row_idx.saturating_add(1);
            }
            // The merged set becomes its size (#817).
            AggPart::ExactCountDistinct => {
                out.push(finish_int_set(&row[row_idx])?);
                row_idx = row_idx.saturating_add(1);
            }
            // The same for strings (#820).
            AggPart::ExactCountDistinctStr => {
                out.push(finish_str_set(&row[row_idx])?);
                row_idx = row_idx.saturating_add(1);
            }
            // Merged into `row[row_idx]` like its plain counterpart, but
            // not itself an output value (#496) -- the cursor still
            // advances past its slot; only an `Expr` part reads it.
            AggPart::Hidden(_) => {
                row_idx = row_idx.saturating_add(1);
            }
            // No row slot of its own -- both operands name a slot another
            // part already claimed, so the cursor doesn't move (#496).
            AggPart::Expr(map_op, lhs, rhs) => {
                out.push(eval_agg_expr(*map_op, lhs, rhs, |i| {
                    row.get(i)
                        .cloned()
                        .ok_or_else(|| VmError::MalformedProgram {
                            opcode: "Combine",
                            reason: format!("Expr operand slot {i} is outside the emitted row"),
                        })
                })?);
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
///
/// Superseded by [`crate::codegen::batch::compile_semi_body`] (tools#90):
/// comparing text, a NULL key matches a NULL in `allowed`, `5` never
/// matches `5.0` and `5` matches `'5'`, where a literal `IN` list does not.
/// Kept until column-rs no longer calls it.
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
    /// A `Select` into register 3, `Emit`ted as column 0 next to a plain
    /// register at column 1.
    fn emits_a_select() -> Vec<Opcode> {
        let plain = |out| crate::vm::batch::Branch {
            ops: std::borrow::Cow::Borrowed(&[]),
            out,
            inputs: std::borrow::Cow::Borrowed(&[]),
        };
        vec![
            Opcode::Select {
                dst: 3,
                cond: 0,
                then: plain(1),
                else_: plain(2),
            },
            Opcode::Emit {
                registers: vec![3, 1].into(),
            },
        ]
    }

    fn combine_of(
        agg_parts: Vec<AggPart>,
        output_order: Vec<usize>,
        project: Vec<Opcode>,
    ) -> Opcode {
        Opcode::Combine {
            agg_parts: agg_parts.into(),
            num_group_keys: 0,
            distinct: false,
            output_order: output_order.into(),
            project: project.into(),
        }
    }

    // vm_engine_branch_output_columns_83d0052f:
    // `agg_parts.is_empty() && output_order.is_empty()`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_branch_output_columns_83d0052f__v1_a_plain_projection_emits_its_selects() {
        let body = emits_a_select();
        let combine = combine_of(vec![], vec![], vec![]);
        assert_eq!(branch_output_columns(&body, Some(&combine)), vec![0]);
        // No Combine at all is the same.
        assert_eq!(branch_output_columns(&body, None), vec![0]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_branch_output_columns_83d0052f__v2_a_reordering_combine_is_left_alone() {
        let body = emits_a_select();
        let combine = combine_of(vec![], vec![1, 0], vec![]);
        assert!(branch_output_columns(&body, Some(&combine)).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_branch_output_columns_83d0052f__v3_an_aggregate_has_no_select_columns() {
        let body = emits_a_select();
        let combine = combine_of(vec![AggPart::Sum], vec![], vec![]);
        assert!(branch_output_columns(&body, Some(&combine)).is_empty());
    }

    /// With a projection, its `Emit` is the output, not the body's.
    #[test]
    fn a_projection_decides_the_output_columns() {
        let combine = combine_of(vec![AggPart::Sum], vec![], emits_a_select());
        assert_eq!(branch_output_columns(&[], Some(&combine)), vec![0]);
    }

    use super::*;

    /// #643 review: one segment of several `SLICE_ROWS` slices, whose keys
    /// first appear in different slices, merges to the same groups, in
    /// first-seen order, through both aggregate paths -- per-worker
    /// pre-aggregation and the per-segment fold -- whatever the core count
    /// that picks between them in `run`.
    #[test]
    fn a_sliced_segment_merges_the_same_on_both_aggregate_paths() {
        use crate::vm::batch::SLICE_ROWS;
        let schema = ["k".to_string(), "v".to_string()];
        let select = crate::codegen::batch::expand_star(
            &crate::parser::parse("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k").unwrap(),
            &schema,
        )
        .unwrap();
        let program = crate::codegen::batch::compile(&select).unwrap();
        let (body, combine, _, _) = program.split_finalize();
        let Some(Opcode::Combine {
            agg_parts,
            num_group_keys,
            ..
        }) = combine
        else {
            panic!("no Combine");
        };
        let n = SLICE_ROWS * 2 + 500;
        let key = |i: usize| i64::try_from(i / 3000).unwrap();
        let big = InMemorySegment(
            Batch::new(n)
                .with_column("k", (0..n).map(|i| Value::Int(key(i))).collect())
                .with_column("v", (0..n).map(|_| Value::Int(1)).collect()),
        );
        let mut expected: Vec<Vec<Value>> = Vec::new();
        for i in 0..n {
            match expected.last_mut() {
                Some(row) if row[0] == Value::Int(key(i)) => {
                    let Value::Int(c) = row[1] else {
                        panic!("count")
                    };
                    row[1] = Value::Int(c + 1);
                    row[2] = Value::Int(c + 1);
                }
                _ => expected.push(vec![Value::Int(key(i)), Value::Int(1), Value::Int(1)]),
            }
        }
        let body: Vec<Opcode> = body.to_vec();
        let pre = combine_partials(
            agg_parts,
            *num_group_keys,
            run_parallel_preaggregate(
                std::slice::from_ref(&big),
                &body,
                agg_parts,
                *num_group_keys,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(pre.into_rows(), expected);
        let folded = run_parallel_segment_partials(
            std::slice::from_ref(&big),
            &body,
            agg_parts,
            *num_group_keys,
        )
        .unwrap();
        let folded = combine_chunks(agg_parts, *num_group_keys, folded.chunks()).unwrap();
        assert_eq!(folded.into_rows(), expected);
    }

    #[test]
    fn dict_keyed_segments_fold_their_slices_and_merge_in_first_seen_order() {
        // #751: `GROUP BY` a dictionary column over several sliced
        // segments, each folding its slices by code, merges to exactly
        // one row per key, in first-seen order across the segments.
        use crate::vm::batch::SLICE_ROWS;
        use crate::vm::column::{Bitmap, Column};
        let schema = ["k".to_string(), "v".to_string()];
        let select = crate::codegen::batch::expand_star(
            &crate::parser::parse("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k").unwrap(),
            &schema,
        )
        .unwrap();
        let program = crate::codegen::batch::compile(&select).unwrap();
        let (body, combine, _, _) = program.split_finalize();
        let Some(Opcode::Combine {
            agg_parts,
            num_group_keys,
            ..
        }) = combine
        else {
            panic!("no Combine");
        };
        let n = SLICE_ROWS * 2 + 500;
        // Segment `s` sees key `(i + s) % 40` at row `i`: every segment
        // folds to 40 groups, and segment 1's first key (`k1`) is also
        // segment 0's second, so first-seen order is `k0, k1, ..., k39`.
        let key = |s: usize, i: usize| (i + s) % 40;
        let segment = |s: usize| {
            let dict: Vec<Arc<str>> = (0..40).map(|i| format!("k{i}").into()).collect();
            InMemorySegment(
                Batch::new(n)
                    .with_typed_column(
                        "k",
                        Column::Dict {
                            dict,
                            indices: (0..n).map(|i| key(s, i) as u32).collect(),
                            valid: Bitmap::from_bools((0..n).map(|_| true)),
                        },
                    )
                    .with_column("v", (0..n).map(|i| Value::Int(i as i64)).collect()),
            )
        };
        let segments = [segment(0), segment(1)];
        let mut expected: Vec<Vec<Value>> = (0..40)
            .map(|k| {
                vec![
                    Value::Str(format!("k{k}").into()),
                    Value::Int(0),
                    Value::Int(0),
                ]
            })
            .collect();
        for s in 0..2 {
            for i in 0..n {
                let row = &mut expected[key(s, i)];
                let (Value::Int(c), Value::Int(t)) = (&row[1], &row[2]) else {
                    panic!("ints");
                };
                (row[1], row[2]) = (Value::Int(c + 1), Value::Int(t + i as i64));
            }
        }
        let body: Vec<Opcode> = body.to_vec();
        assert!(crate::vm::batch::folds_dict_groups(&body), "{body:?}");
        let pre = combine_partials(
            agg_parts,
            *num_group_keys,
            run_parallel_preaggregate(&segments, &body, agg_parts, *num_group_keys).unwrap(),
        )
        .unwrap();
        assert_eq!(pre.into_rows(), expected);
        let per_segment =
            run_parallel_segment_partials(&segments, &body, agg_parts, *num_group_keys).unwrap();
        let merged = combine_chunks(agg_parts, *num_group_keys, per_segment.chunks()).unwrap();
        assert_eq!(merged.into_rows(), expected);
    }

    fn two_key_segment(n: usize) -> InMemorySegment {
        InMemorySegment(
            Batch::new(n)
                .with_column("k", (0..n).map(|i| Value::Int((i % 2) as i64)).collect())
                .with_column("v", (0..n).map(|_| Value::Int(1)).collect()),
        )
    }

    fn group_sum_body() -> Vec<Opcode> {
        vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            },
            Opcode::LoadColumn {
                reg: 1,
                column: "v".into(),
            },
            Opcode::GroupReduce {
                group_by: vec![0].into(),
                aggs: vec![(AggFunc::Sum, Some(1))].into(),
                agg_dst: vec![2].into(),
            },
            Opcode::Emit {
                registers: vec![0, 2].into(),
            },
        ]
    }

    fn segment_partial_rows(n: usize, body: &[Opcode]) -> usize {
        let parts = [AggPart::GroupKey, AggPart::Sum];
        run_parallel_segment_partials(&[two_key_segment(n)], body, &parts, 1)
            .unwrap()
            .num_rows()
    }

    // vm_engine_run_parallel_segment_partials_d80b3ca6
    // (`run_parallel_segment_partials`):
    // `batch.num_rows <= SLICE_ROWS || !sliceable(body)`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_parallel_segment_partials_d80b3ca6__v1_a_small_segment_runs_whole() {
        assert_eq!(segment_partial_rows(4, &group_sum_body()), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_parallel_segment_partials_d80b3ca6__v2_a_big_non_sliceable_segment_runs_whole(
    ) {
        use crate::vm::batch::SLICE_ROWS;
        let body = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        assert_eq!(segment_partial_rows(SLICE_ROWS + 1, &body), SLICE_ROWS + 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_parallel_segment_partials_d80b3ca6__v3_a_big_sliceable_segment_folds_its_slices(
    ) {
        use crate::vm::batch::SLICE_ROWS;
        assert_eq!(segment_partial_rows(SLICE_ROWS + 1, &group_sum_body()), 2);
    }

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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            },
            Opcode::Sort {
                keys: vec![(0, false)].into(),
                hidden: 0,
            },
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(13)],
                vec![Value::Int(2), Value::Int(5)],
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
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v1_distinct_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: true,
                    output_order: Vec::new().into(),
                    project: Vec::new().into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v2_non_empty_agg_parts_disqualifies_bounded_scan(
    ) {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![AggPart::Sum].into(),
                    num_group_keys: 0,
                    distinct: false,
                    output_order: Vec::new().into(),
                    project: Vec::new().into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v3_output_order_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                    output_order: vec![1, 0].into(),
                    project: Vec::new().into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v4_project_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                    output_order: Vec::new().into(),
                    project: vec![Opcode::Emit {
                        registers: vec![0].into(),
                    }]
                    .into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
            ],
            false,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v5_filter_in_body_disqualifies_bounded_scan() {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                    output_order: Vec::new().into(),
                    project: Vec::new().into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
            ],
            true,
        );
        assert_eq!(bounded_scan_limit(&program), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_bounded_scan_limit_72191bdf__v6_no_distinct_no_aggs_no_filter_allows_bounded_scan(
    ) {
        let program = scan_program(
            vec![
                Opcode::Combine {
                    agg_parts: vec![].into(),
                    num_group_keys: 0,
                    distinct: false,
                    output_order: Vec::new().into(),
                    project: Vec::new().into(),
                },
                Opcode::Limit { n: 5, offset: 0 },
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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
            Instruction::new(Opcode::Limit { n: 3, offset: 0 }),
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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
            Instruction::new(Opcode::Sort {
                keys: vec![(0, false)].into(),
                hidden: 0,
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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
            Instruction::new(Opcode::Sort {
                keys: vec![(0, false)].into(),
                hidden: 0,
            }),
            Instruction::new(Opcode::Limit { n: 2, offset: 0 }),
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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            },
            Opcode::Sort {
                keys: vec![(0, false)].into(),
                hidden: 0,
            },
        ]);
        let rows = run(&segments, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(13)],
                vec![Value::Int(2), Value::Int(5)],
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
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
            Instruction::new(Opcode::Sort {
                keys: vec![(0, true)].into(),
                hidden: 0,
            }),
            Instruction::new(Opcode::Limit { n: 2, offset: 0 }),
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
            &[],
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
            output_order: Vec::new().into(),
            project: Vec::new().into(),
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
                    output_order: Vec::new().into(),
                    project: Vec::new().into(),
                },
                Opcode::Sort {
                    keys: vec![(0, false)].into(),
                    hidden: 0,
                },
                Opcode::Limit { n: 0, offset: 0 },
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

    fn combine(distinct: bool) -> Opcode {
        Opcode::Combine {
            agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
            num_group_keys: 1,
            distinct,
            output_order: Vec::new().into(),
            project: Vec::new().into(),
        }
    }

    // vm_engine_run_page_90c1a10a (`run_page`, aggregate branch):
    // `!*distinct && order_by.is_empty() && limit.is_none()`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_90c1a10a__v1_plain_aggregate_returns_the_merge_directly() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let rows = run(&segments, &group_sum_program(vec![combine(false)])).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(13)],
                vec![Value::Int(2), Value::Int(5)],
            ]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_90c1a10a__v2_distinct_goes_through_finalize() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let rows = run(&segments, &group_sum_program(vec![combine(true)])).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_90c1a10a__v3_order_by_goes_through_finalize() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let rows = run(
            &segments,
            &group_sum_program(vec![
                combine(false),
                Opcode::Sort {
                    keys: vec![(0, true)].into(),
                    hidden: 0,
                },
            ]),
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Int(5)],
                vec![Value::Int(1), Value::Int(13)],
            ]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_90c1a10a__v4_limit_goes_through_finalize() {
        let segments = vec![seg(&[(1, 10), (2, 5)]), seg(&[(1, 3)])];
        let rows = run(
            &segments,
            &group_sum_program(vec![combine(false), Opcode::Limit { n: 1, offset: 0 }]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    fn first_row_width(hidden: usize) -> usize {
        let segments = vec![seg(&[(2, 10), (1, 5)]), seg(&[(3, 3)])];
        let mut instructions = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            },
            Opcode::LoadColumn {
                reg: 1,
                column: "v".into(),
            },
            Opcode::Emit {
                registers: vec![0, 1].into(),
            },
            Opcode::Combine {
                agg_parts: Vec::new().into(),
                num_group_keys: 0,
                distinct: false,
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            },
            Opcode::Sort {
                keys: vec![(0, false)].into(),
                hidden,
            },
        ];
        let program = Program::new(instructions.drain(..).map(Instruction::new).collect());
        let rows = run(&segments, &program).unwrap().into_rows();
        assert_eq!(rows.len(), 3);
        rows[0].len()
    }

    // vm_engine_run_page_1c741d3f (`run_page`, hidden sort columns):
    // `hidden == 0 || keep.is_empty()`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_1c741d3f__v1_no_hidden_columns_returns_the_output_whole() {
        assert_eq!(first_row_width(0), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_1c741d3f__v2_hidden_columns_are_projected_away() {
        assert_eq!(first_row_width(1), 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_page_1c741d3f__v3_all_columns_hidden_returns_the_output_whole() {
        assert_eq!(first_row_width(2), 2);
    }
}

#[cfg(test)]
mod preaggregation_tests {
    use super::super::batch::{
        run_parallel, AggFunc, AggPart, Batch, Instruction, Opcode, Program, Value,
    };
    use super::super::combine::combine_chunks;
    use super::{run, run_parallel_preaggregate, InMemorySegment};

    fn segments(num_segments: usize, groups: i64) -> Vec<InMemorySegment> {
        (0..num_segments)
            .map(|seg| {
                // Every segment sees every group, in a segment-dependent
                // rotation so first-seen order differs from key order.
                let keys: Vec<Value> = (0..groups)
                    .map(|g| Value::Int((g + seg as i64) % groups))
                    .collect();
                let amounts: Vec<Value> = (0..groups).map(|g| Value::Int(g + seg as i64)).collect();
                InMemorySegment(
                    Batch::new(groups as usize)
                        .with_column("k", keys)
                        .with_column("v", amounts),
                )
            })
            .collect()
    }

    fn group_sum_program() -> Program {
        Program::new(vec![
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
            Instruction::new(Opcode::Combine {
                agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
        ])
    }

    /// #488's acceptance criterion: the final merge receives at most
    /// `threads x groups` partial rows -- 40 segments x 500 groups is
    /// 20K rows into `Combine` today, but only `threads x 500` here.
    #[test]
    fn combine_receives_at_most_threads_times_groups_partial_rows() {
        let (num_segments, groups) = (40usize, 500i64);
        let segs = segments(num_segments, groups);
        let program = group_sum_program();
        let (body, ..) = program.split_finalize();
        let partials =
            run_parallel_preaggregate(&segs, &body, &[AggPart::GroupKey, AggPart::Sum], 1).unwrap();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        // The workers' partials and the shared partitions' (empty here).
        assert!(partials.len() <= threads.min(num_segments) + super::SPILL_PARTITIONS);
        let total: usize = partials.iter().map(super::Partial::len).sum();
        assert!(
            total <= threads * groups as usize,
            "{total} partial rows for {threads} threads x {groups} groups"
        );
        assert!(total < num_segments * groups as usize / 2 || threads * 2 > num_segments);
    }

    /// The shared combiner's partial, last of `run_parallel_preaggregate`'s.
    fn spilled_groups(groups: i64) -> usize {
        let segs = segments(8, groups);
        let program = group_sum_program();
        let (body, ..) = program.split_finalize();
        run_parallel_preaggregate(&segs, &body, &[AggPart::GroupKey, AggPart::Sum], 1)
            .unwrap()
            .iter()
            .rev()
            .take(super::SPILL_PARTITIONS)
            .map(super::Partial::len)
            .sum()
    }

    // vm_engine_run_parallel_preaggregate_b7b1b7ef
    // (`run_parallel_preaggregate`): `combiner.groups() > SPILL_GROUPS`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_parallel_preaggregate_b7b1b7ef__v1_few_groups_stay_in_the_worker() {
        assert_eq!(spilled_groups(500), 0);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_engine_run_parallel_preaggregate_b7b1b7ef__v2_many_groups_spill_to_the_shared_combiner(
    ) {
        let groups = i64::try_from(super::SPILL_GROUPS).unwrap() + 900;
        assert!(spilled_groups(groups) > 0);
        // Between the two thresholds the worker keeps its groups until it
        // drains, then hands them on.
        let groups = i64::try_from(super::HANDOFF_GROUPS).unwrap() + 900;
        assert!(spilled_groups(groups) > 0);
    }

    /// More groups than `SPILL_GROUPS` per worker: the spill into the shared
    /// combiner must not change the answer or the group order (#643).
    #[test]
    fn spilled_preaggregation_matches_the_direct_merge() {
        let segs = segments(40, i64::try_from(super::HANDOFF_GROUPS).unwrap() + 900);
        let program = group_sum_program();
        let (body, ..) = program.split_finalize();
        let per_segment = run_parallel(&segs, &body).unwrap();
        let direct =
            combine_chunks(&[AggPart::GroupKey, AggPart::Sum], 1, per_segment.chunks()).unwrap();
        for _ in 0..3 {
            assert_eq!(run(&segs, &program).unwrap(), direct);
        }
    }

    /// As above with a worker over `SPILL_GROUPS` between segments: it
    /// spills mid-run, then hands on the rest when it drains (#863).
    #[test]
    fn mid_run_spilled_preaggregation_matches_the_direct_merge() {
        let segs = segments(8, i64::try_from(super::SPILL_GROUPS).unwrap() + 900);
        let program = group_sum_program();
        let (body, ..) = program.split_finalize();
        let per_segment = run_parallel(&segs, &body).unwrap();
        let direct =
            combine_chunks(&[AggPart::GroupKey, AggPart::Sum], 1, per_segment.chunks()).unwrap();
        assert_eq!(run(&segs, &program).unwrap(), direct);
    }

    /// A segment of `rows` rows over `keys` int keys (every ninth `NULL`),
    /// with an int column (every seventh `NULL`) and a float column of
    /// halves, so float sums are exact whatever order they are added in --
    /// once as typed columns, once as `Value`s.
    fn mixed_segment(seg: usize, rows: usize, keys: usize, typed: bool) -> InMemorySegment {
        use crate::vm::batch::{Bitmap, Column};
        let key = |r: usize| (r * 7 + seg * 13) % keys;
        let int = |r: usize| i64::try_from(r * 3 + seg).unwrap() - 500;
        let float = |r: usize| f64::from(u32::try_from((r + seg) % 40).unwrap()) / 2.0 - 5.0;
        let key_null = |r: usize| r % 9 == 4;
        let int_null = |r: usize| r % 7 == 3;
        let batch = Batch::new(rows);
        InMemorySegment(if typed {
            let mut keys_valid = Bitmap::new(rows, true);
            let mut ints_valid = Bitmap::new(rows, true);
            for r in (0..rows).filter(|&r| key_null(r)) {
                keys_valid.set(r, false);
            }
            for r in (0..rows).filter(|&r| int_null(r)) {
                ints_valid.set(r, false);
            }
            batch
                .with_typed_column(
                    "k",
                    Column::Int {
                        data: (0..rows).map(|r| i64::try_from(key(r)).unwrap()).collect(),
                        valid: keys_valid,
                    },
                )
                .with_typed_column(
                    "i",
                    Column::Int {
                        data: (0..rows).map(int).collect(),
                        valid: ints_valid,
                    },
                )
                .with_typed_column(
                    "f",
                    Column::Float {
                        data: (0..rows).map(float).collect(),
                        valid: Bitmap::new(rows, true),
                    },
                )
        } else {
            batch
                .with_column(
                    "k",
                    (0..rows)
                        .map(|r| {
                            if key_null(r) {
                                Value::Null
                            } else {
                                Value::Int(i64::try_from(key(r)).unwrap())
                            }
                        })
                        .collect(),
                )
                .with_column(
                    "i",
                    (0..rows)
                        .map(|r| {
                            if int_null(r) {
                                Value::Null
                            } else {
                                Value::Int(int(r))
                            }
                        })
                        .collect(),
                )
                .with_column("f", (0..rows).map(|r| Value::Float(float(r))).collect())
        })
    }

    /// Every aggregate the single-`Int`-key fold holds compactly (#894),
    /// over typed columns -- sliced segments, `NULL` keys and inputs -- must
    /// give what the same data as `Value` columns (which never fold) does.
    #[test]
    fn int_key_fold_matches_the_unfolded_path() {
        let aggs: Vec<(AggFunc, Option<usize>)> = vec![
            (AggFunc::Count, None),
            (AggFunc::Count, Some(1)),
            (AggFunc::Sum, Some(1)),
            (AggFunc::Sum, Some(2)),
            (AggFunc::Min, Some(1)),
            (AggFunc::Max, Some(1)),
            (AggFunc::Min, Some(2)),
            (AggFunc::Max, Some(2)),
            (AggFunc::AvgSum, Some(2)),
            (AggFunc::Count, Some(2)),
            (AggFunc::AvgSum, Some(1)),
            (AggFunc::Count, Some(1)),
        ];
        let parts = vec![
            AggPart::GroupKey,
            AggPart::Count,
            AggPart::Count,
            AggPart::Sum,
            AggPart::Sum,
            AggPart::Min,
            AggPart::Max,
            AggPart::Min,
            AggPart::Max,
            AggPart::Avg(9, 10),
            AggPart::Avg(11, 12),
        ];
        let first_dst = 3usize;
        let mut ops = vec![
            Instruction::new(Opcode::LoadColumn {
                reg: 0,
                column: "k".into(),
            }),
            Instruction::new(Opcode::LoadColumn {
                reg: 1,
                column: "i".into(),
            }),
            Instruction::new(Opcode::LoadColumn {
                reg: 2,
                column: "f".into(),
            }),
        ];
        let agg_dst: Vec<usize> = (first_dst..first_dst + aggs.len()).collect();
        ops.push(Instruction::new(Opcode::GroupReduce {
            group_by: vec![0].into(),
            aggs: aggs.into(),
            agg_dst: agg_dst.clone().into(),
        }));
        ops.push(Instruction::new(Opcode::Emit {
            registers: std::iter::once(0).chain(agg_dst).collect::<Vec<_>>().into(),
        }));
        ops.push(Instruction::new(Opcode::Combine {
            agg_parts: parts.into(),
            num_group_keys: 1,
            distinct: false,
            output_order: Vec::new().into(),
            project: Vec::new().into(),
        }));
        let program = Program::new(ops);
        // Past `SLICE_ROWS`, so each segment folds in several slices.
        let typed: Vec<InMemorySegment> = (0..6)
            .map(|seg| mixed_segment(seg, 20_000, 600, true))
            .collect();
        let values: Vec<InMemorySegment> = (0..6)
            .map(|seg| mixed_segment(seg, 20_000, 600, false))
            .collect();
        let (body, ..) = program.split_finalize();
        assert!(crate::vm::batch::int_fold_applies(&body, &typed[0].0));
        assert!(!crate::vm::batch::int_fold_applies(&body, &values[0].0));
        // The pre-aggregation itself, not `run()`, which only pre-aggregates
        // given several segments per thread.
        let parts = [
            AggPart::GroupKey,
            AggPart::Count,
            AggPart::Count,
            AggPart::Sum,
            AggPart::Sum,
            AggPart::Min,
            AggPart::Max,
            AggPart::Min,
            AggPart::Max,
            AggPart::Avg(9, 10),
            AggPart::Avg(11, 12),
        ];
        let per_segment = run_parallel(&values, &body).unwrap();
        let expected = combine_chunks(&parts, 1, per_segment.chunks()).unwrap();
        let got = super::combine_partials(
            &parts,
            1,
            run_parallel_preaggregate(&typed, &body, &parts, 1).unwrap(),
        )
        .unwrap();
        assert_eq!(got.rows().count(), 601);
        // Groups come out in first-seen order, which both paths agree on.
        assert_eq!(got, expected);
    }

    /// The pre-aggregated `run()` must produce exactly what the direct
    /// per-segment merge does -- values and group order -- and do so
    /// identically on every run, whatever the pool's segment interleaving.
    #[test]
    fn preaggregated_run_matches_the_direct_merge_and_is_deterministic() {
        let segs = segments(40, 500);
        let program = group_sum_program();
        let (body, ..) = program.split_finalize();
        let per_segment = run_parallel(&segs, &body).unwrap();
        let direct =
            combine_chunks(&[AggPart::GroupKey, AggPart::Sum], 1, per_segment.chunks()).unwrap();
        for _ in 0..10 {
            let got = run(&segs, &program).unwrap();
            assert_eq!(got, direct);
        }
        // Rotation puts key 0 first in segment 0, so first-seen order is
        // 0, 1, 2, ... regardless of which worker saw which segment.
        let rows = direct.into_rows();
        assert_eq!(rows[0][0], Value::Int(0));
        assert_eq!(rows[1][0], Value::Int(1));
        assert_eq!(rows[499][0], Value::Int(499));
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json` by
    //! `make test-mcdc`).

    use super::super::batch::{AggFunc, AggPart, Batch, Instruction, Opcode, Program, Value};
    use super::{run, InMemorySegment};

    /// `SELECT count(x)` (`grouped`: `SELECT x, count(x) GROUP BY x`).
    fn count_program(grouped: bool) -> Program {
        let load = Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "x".into(),
        });
        let (reduce, emit, parts, keys) = if grouped {
            (
                Opcode::GroupReduce {
                    group_by: vec![0].into(),
                    aggs: vec![(AggFunc::Count, Some(0))].into(),
                    agg_dst: vec![1].into(),
                },
                vec![0, 1],
                vec![AggPart::GroupKey, AggPart::Count],
                1,
            )
        } else {
            (
                Opcode::Reduce {
                    func: AggFunc::Count,
                    src: Some(0),
                    dst: 1,
                },
                vec![1],
                vec![AggPart::Count],
                0,
            )
        };
        Program::new(vec![
            load,
            Instruction::new(reduce),
            Instruction::new(Opcode::Emit {
                registers: emit.into(),
            }),
            Instruction::new(Opcode::Combine {
                agg_parts: parts.into(),
                num_group_keys: keys,
                distinct: false,
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }),
        ])
    }

    fn rows(segments: &[InMemorySegment], grouped: bool) -> Vec<Vec<Value>> {
        run(segments, &count_program(grouped)).unwrap().into_rows()
    }

    // vm_engine_run_page_7330e6cc: `segments.is_empty() && *num_group_keys == 0`
    #[test]
    fn mcdc__vm_engine_run_page_7330e6cc__v1_no_segments_ungrouped_is_one_row_of_zero() {
        assert_eq!(rows(&[], false), vec![vec![Value::Int(0)]]);
    }

    #[test]
    fn mcdc__vm_engine_run_page_7330e6cc__v2_with_segments_the_segments_supply_the_row() {
        let segment =
            InMemorySegment(Batch::new(2).with_column("x", vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(rows(&[segment], false), vec![vec![Value::Int(2)]]);
    }

    #[test]
    fn mcdc__vm_engine_run_page_7330e6cc__v3_no_segments_grouped_is_no_rows() {
        assert!(rows(&[], true).is_empty());
    }
}
