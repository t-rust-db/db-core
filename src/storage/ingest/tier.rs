//! A tiered table's outputs (ADR 0028, #573): a closed hour from its
//! pages, a closed day from its hour files.
//!
//! Both write a directory of files the caller publishes with one rename,
//! each file a contiguous LSN range for the manifest. A closed hour is one
//! file: its rows sorted by `sort_key`, then cut into row groups, so each
//! row group holds a range of the sort key. A day merge copies those row
//! groups as they are (a row group never spans an hour), and starts a new
//! file at an input file boundary once a file reaches `max_file_size`.
//!
//! A record table's (#594) hour and day keep its declared schema: its rows
//! are sorted and written as the typed columns they are, and a day merge
//! reads every column back as its declared type, so logical types (a
//! DECIMAL, an unsigned integer, a timestamp) survive compaction.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::flush::{
    add_field, add_page_fields, bounds, convert, predefined_specs, schema, FlushedFile,
};
use super::layout::{compare, TierOptions};
use super::manifest::FileEntry;
use super::page::{Page, PageBody};
use super::record::{compare_across, compare_rows, concat, gather, read_column};
use super::snapshot::{cast, column_type};
use super::table::IngestError;
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::writer::{ColumnSpec, ColumnValues, ParquetWriter, WriterOptions};
use crate::storage::parquet::{Field, ParquetFile};
use crate::storage::stream::adapter::stored_column_values;
use crate::storage::stream::detect::Format;
use crate::storage::RowGroupReader;
use crate::vm::batch::Value;

/// One file of an output, named inside the output directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Output {
    pub(crate) name: String,
    pub(crate) lsn_lo: u64,
    pub(crate) lsn_hi: u64,
    pub(crate) file: FlushedFile,
}

/// The file being written.
struct OpenFile {
    writer: ParquetWriter<BufWriter<File>>,
    path: PathBuf,
    lsn_lo: u64,
    lsn_hi: u64,
}

/// Writes an output directory's files in turn.
struct Files {
    dir: PathBuf,
    specs: Vec<ColumnSpec>,
    options: WriterOptions,
    max_file_size: u64,
    open: Option<OpenFile>,
    done: Vec<Output>,
}

impl Files {
    fn new(
        dir: &Path,
        specs: Vec<ColumnSpec>,
        parquet: &WriterOptions,
        tier: &TierOptions,
    ) -> Self {
        let mut options = parquet.clone();
        // Row groups are cut here, never by row count in the writer.
        options.row_group_rows = usize::MAX;
        Files {
            dir: dir.to_path_buf(),
            specs,
            options,
            max_file_size: tier.max_file_size,
            open: None,
            done: Vec::new(),
        }
    }

    /// Appends one row group holding LSNs `lsn_lo..=lsn_hi`.
    fn row_group(
        &mut self,
        columns: Vec<ColumnValues>,
        lsn_lo: u64,
        lsn_hi: u64,
    ) -> Result<(), IngestError> {
        let open = match self.open.take() {
            Some(open) => open,
            None => {
                let path = self.dir.join(format!("part-{}.parquet", self.done.len()));
                OpenFile {
                    writer: ParquetWriter::new(
                        BufWriter::new(File::create(&path)?),
                        self.specs.clone(),
                        self.options.clone(),
                    )?,
                    path,
                    lsn_lo,
                    lsn_hi,
                }
            }
        };
        let mut open = open;
        open.writer.write_columns(columns)?;
        open.writer.flush_row_group()?;
        open.lsn_hi = lsn_hi;
        self.open = Some(open);
        Ok(())
    }

    /// Closes the open file if it has reached the size limit.
    fn maybe_cut(&mut self) -> Result<(), IngestError> {
        if self
            .open
            .as_ref()
            .is_some_and(|o| o.writer.bytes_written() >= self.max_file_size)
        {
            self.cut()?;
        }
        Ok(())
    }

    /// Closes the open file: footer, fsync, its final name.
    fn cut(&mut self) -> Result<(), IngestError> {
        let Some(mut open) = self.open.take() else {
            return Ok(());
        };
        open.writer.set_key_value_metadata(vec![
            ("db-core.lsn_lo".to_string(), open.lsn_lo.to_string()),
            ("db-core.lsn_hi".to_string(), open.lsn_hi.to_string()),
        ]);
        let (out, summary) = open.writer.finish()?;
        let file = out
            .into_inner()
            .map_err(|e| IngestError::Io(e.into_error()))?;
        file.sync_all()?;
        let name = format!("{:020}-{:020}.parquet", open.lsn_lo, open.lsn_hi);
        fs::rename(&open.path, self.dir.join(&name))?;
        self.done.push(Output {
            name,
            lsn_lo: open.lsn_lo,
            lsn_hi: open.lsn_hi,
            file: FlushedFile {
                rows: summary.num_rows,
                bytes: summary.bytes,
                observed_ts: None,
                timestamp: None,
            },
        });
        Ok(())
    }

    /// Closes the last file and fsyncs the directory.
    fn finish(mut self) -> Result<Vec<Output>, IngestError> {
        self.cut()?;
        File::open(&self.dir)?.sync_all()?;
        Ok(self.done)
    }
}

/// An optional `(min, max)` range.
type Bounds = Option<(i64, i64)>;

/// The widest of two optional `(min, max)` ranges.
fn widen_bounds(a: Bounds, b: Bounds) -> Bounds {
    match (a, b) {
        (Some((alo, ahi)), Some((blo, bhi))) => Some((alo.min(blo), ahi.max(bhi))),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Every row of `pages`, column `spec`, as a `format` table stores it.
fn page_rows(pages: &[Page], spec: &ColumnSpec, format: Format) -> Vec<Value> {
    pages
        .iter()
        .filter_map(Page::lines)
        .flat_map(|segment| {
            stored_column_values(segment, &spec.name, format)
                .unwrap_or_else(|| vec![Value::Null; segment.len()])
        })
        .map(|v| cast(v, spec.column_type))
        .collect()
}

/// `columns` reordered by `sort_key` (stable: ties keep arrival order).
fn sorted(columns: Vec<Vec<Value>>, specs: &[ColumnSpec], sort_key: &[String]) -> Vec<Vec<Value>> {
    let keys: Vec<usize> = sort_key
        .iter()
        .filter_map(|k| specs.iter().position(|s| s.name == *k))
        .collect();
    let rows = columns.first().map_or(0, Vec::len);
    let mut order: Vec<usize> = (0..rows).collect();
    order.sort_by(|&a, &b| {
        keys.iter()
            .map(|&k| {
                let column = columns.get(k);
                let value = |row: usize| column.and_then(|c| c.get(row)).unwrap_or(&Value::Null);
                compare(value(a), value(b))
            })
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    columns
        .into_iter()
        .map(|column| {
            order
                .iter()
                .map(|&i| column.get(i).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

/// Rows sorted by `sort_key`, as typed columns, with the ranges of their
/// `observed_ts` and `timestamp`.
struct Sorted {
    columns: Vec<ColumnValues>,
    rows: usize,
    observed: Bounds,
    timestamp: Bounds,
}

/// The rows of `pages` (contiguous, oldest first) as the columns of
/// `specs`, sorted by `sort_key` (stable: ties keep arrival order). A
/// record table's pages carry their own schema, which `specs` must be.
fn sort_pages(pages: &[Page], format: Format, specs: &[ColumnSpec], tier: &TierOptions) -> Sorted {
    let row_pages: Vec<_> = pages
        .iter()
        .filter_map(|p| match &p.body {
            PageBody::Rows(rows) => Some(rows),
            PageBody::Lines(_) => None,
        })
        .collect();
    if row_pages.is_empty() {
        let columns: Vec<Vec<Value>> = specs.iter().map(|s| page_rows(pages, s, format)).collect();
        let columns = sorted(columns, specs, &tier.sort_key);
        let range_of = |name: &str| {
            specs
                .iter()
                .position(|s| s.name == name)
                .and_then(|i| columns.get(i))
                .and_then(|c| {
                    bounds(c.iter().map(|v| match v {
                        Value::Int(i) => Some(*i),
                        _ => None,
                    }))
                })
        };
        let (observed, timestamp) = (range_of("observed_ts"), range_of("timestamp"));
        let rows = columns.first().map_or(0, Vec::len);
        let columns = columns
            .into_iter()
            .zip(specs)
            .map(|(values, spec)| convert(values, spec.column_type))
            .collect();
        return Sorted {
            columns,
            rows,
            observed,
            timestamp,
        };
    }
    let columns: Vec<_> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let parts: Vec<_> = row_pages
                .iter()
                .filter_map(|r| r.columns().get(i))
                .collect();
            concat(spec.column_type, &parts)
        })
        .collect();
    let keys = key_positions(specs, tier);
    let rows: usize = row_pages.iter().map(|r| r.len()).sum();
    let mut order: Vec<usize> = (0..rows).collect();
    order.sort_by(|&a, &b| {
        keys.iter()
            .filter_map(|&k| columns.get(k))
            .map(|c| compare_rows(c, a, b))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // A record table has only its own columns: `observed_ts` is the rows'
    // arrival times, not a column, and there is no `timestamp` range.
    let observed = bounds(
        row_pages
            .iter()
            .flat_map(|r| r.observed_ts_ns().iter().copied().map(Some)),
    );
    Sorted {
        columns: columns.iter().map(|c| gather(c, &order)).collect(),
        rows,
        observed,
        timestamp: None,
    }
}

/// The positions in `specs` of the sort key's columns (a name the table
/// does not have sorts as NULL, so it is left out).
fn key_positions(specs: &[ColumnSpec], tier: &TierOptions) -> Vec<usize> {
    tier.sort_key
        .iter()
        .filter_map(|k| specs.iter().position(|s| s.name == *k))
        .collect()
}

/// Writes `sorted` into `dir` as one file of LSNs `lsn_lo..=lsn_hi`, cut
/// into row groups of at most `row_group_max_rows`.
fn write_sorted(
    dir: &Path,
    specs: Vec<ColumnSpec>,
    sorted: Sorted,
    (lsn_lo, lsn_hi): (u64, u64),
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let chunk = tier.row_group_max_rows.max(1);
    let mut files = Files::new(dir, specs, parquet, tier);
    if sorted.rows <= chunk {
        files.row_group(sorted.columns, lsn_lo, lsn_hi)?;
    } else {
        let mut lo = 0usize;
        while lo < sorted.rows {
            let hi = lo.saturating_add(chunk).min(sorted.rows);
            let range: Vec<usize> = (lo..hi).collect();
            let values = sorted.columns.iter().map(|c| gather(c, &range)).collect();
            files.row_group(values, lsn_lo, lsn_hi)?;
            lo = hi;
        }
    }
    let mut outputs = files.finish()?;
    for output in &mut outputs {
        output.file.observed_ts = sorted.observed;
        output.file.timestamp = sorted.timestamp;
    }
    Ok(outputs)
}

/// Writes one closed hour's `pages` (contiguous, oldest first, at least
/// one) into `dir`, which must exist and be empty: one file, the hour's
/// rows sorted by `sort_key` and cut into row groups of at most
/// `row_group_max_rows`, so each row group holds a range of the sort key
/// and its statistics prune on it (amendment to ADR 0028). A record
/// table's (#594) output's `observed_ts` range is the rows' arrival times
/// (not written: a record table has only its own columns); it has no
/// `timestamp` range. A spill to `staging/` is written the same way.
pub(crate) fn write_hour(
    dir: &Path,
    pages: &[Page],
    format: Format,
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
        return Err(IngestError::Corrupt("no pages to write".to_string()));
    };
    let specs = schema(pages);
    let sorted = sort_pages(pages, format, &specs, tier);
    write_sorted(
        dir,
        specs,
        sorted,
        (first.lsn_lo, last.lsn_hi),
        tier,
        parquet,
    )
}

/// One column chunk of `rg` as values, by its physical type.
fn read_values(
    rg: &RowGroupReader<'_, '_>,
    index: usize,
    pt: PhysicalType,
) -> Result<Vec<Value>, String> {
    fn map<T>(values: Vec<Option<T>>, f: impl Fn(T) -> Value) -> Vec<Value> {
        values
            .into_iter()
            .map(|v| v.map_or(Value::Null, &f))
            .collect()
    }
    let e = |e: crate::storage::FileError| e.to_string();
    Ok(match pt {
        PhysicalType::Int32 => map(rg.read_int32_column(index).map_err(e)?, |v| {
            Value::Int(i64::from(v))
        }),
        PhysicalType::Int64 => map(rg.read_int64_column(index).map_err(e)?, Value::Int),
        PhysicalType::Double => map(rg.read_double_column(index).map_err(e)?, Value::Float),
        PhysicalType::Float => map(rg.read_float_column(index).map_err(e)?, |v| {
            Value::Float(f64::from(v))
        }),
        PhysicalType::Boolean => map(rg.read_boolean_column(index).map_err(e)?, Value::Bool),
        PhysicalType::ByteArray => map(rg.read_string_column(index).map_err(e)?, |s| {
            Value::Str(s.into())
        }),
        PhysicalType::Int96 | PhysicalType::FixedLenByteArray | PhysicalType::Unknown(_) => {
            return Err(format!("unsupported physical type {pt:?}"))
        }
    })
}

/// Row group `index` of `file` (with its `fields`, at `path`) as the
/// columns of `specs`, `None` past the last. A record table (`typed`)
/// reads every column as declared; a lines table casts each to the
/// widened type, and a column the file lacks is NULL.
fn row_group(
    file: &ParquetFile<'_>,
    fields: &[Field],
    path: &str,
    index: usize,
    specs: &[ColumnSpec],
    typed: bool,
) -> Result<Option<(Vec<ColumnValues>, u64)>, IngestError> {
    let Some(rg) = file.row_group(index) else {
        return Ok(None);
    };
    let rows = u64::try_from(rg.num_rows())
        .map_err(|_| corrupt(path, "negative row count".to_string()))?;
    let mut columns = Vec::with_capacity(specs.len());
    for spec in specs {
        let field = fields.iter().find(|f| f.name == spec.name);
        if typed {
            // A record table's file has every column, as declared (a MAP
            // is a field of two leaves).
            let field =
                field.ok_or_else(|| corrupt(path, format!("has no column {}", spec.name)))?;
            columns.push(
                read_column(&rg, field, spec.column_type)
                    .map_err(|e| corrupt(path, e.to_string()))?,
            );
            continue;
        }
        let leaf = field
            .filter(|f| !f.nested)
            .and_then(|f| Some((f.leaves.start, f.element.physical_type?)));
        let values = match leaf {
            Some((index, pt)) => read_values(&rg, index, pt)
                .map_err(|why| corrupt(path, why))?
                .into_iter()
                .map(|v| cast(v, spec.column_type))
                .collect(),
            None => vec![Value::Null; usize::try_from(rows).unwrap_or(0)],
        };
        columns.push(convert(values, spec.column_type));
    }
    Ok(Some((columns, rows)))
}

fn corrupt(path: &str, why: String) -> IngestError {
    IngestError::Corrupt(format!("{path}: {why}"))
}

/// A lines table's merge schema: the predefined columns, every field of
/// `inputs` and of `pages`, each widened to fit all of them.
fn widened_specs(files: &[&ParquetFile<'_>], pages: &[Page]) -> Vec<ColumnSpec> {
    let mut specs = predefined_specs();
    for file in files {
        for element in file.metadata().schema.iter().skip(1) {
            if let Some(t) = element.physical_type.and_then(column_type) {
                add_field(&mut specs, &element.name, t);
            }
        }
    }
    for page in pages {
        add_page_fields(&mut specs, page);
    }
    specs
}

/// Merges one closed day's hour files (`inputs`, contiguous, oldest first)
/// into `dir`, which must exist and be empty: their row groups, in order,
/// under the day's widened schema, or a record table's `schema`.
pub(crate) fn merge_day(
    dir: &Path,
    inputs: &[(&FileEntry, &[u8])],
    schema: Option<&[ColumnSpec]>,
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let opened = inputs
        .iter()
        .map(|(entry, bytes)| {
            let file = ParquetFile::open(bytes).map_err(|e| corrupt(&entry.path, e.to_string()))?;
            let fields = file.fields();
            Ok((*entry, file, fields))
        })
        .collect::<Result<Vec<_>, IngestError>>()?;
    let specs = match schema {
        Some(schema) => schema.to_vec(),
        None => widened_specs(&opened.iter().map(|o| &o.1).collect::<Vec<_>>(), &[]),
    };
    let mut files = Files::new(dir, specs.clone(), parquet, tier);
    let mut bounds_by_file = Vec::new();
    let mut current: (Bounds, Bounds) = (None, None);
    for (entry, file, fields) in &opened {
        let mut next = entry.lsn_lo;
        let mut index = 0usize;
        while let Some((columns, rows)) =
            row_group(file, fields, &entry.path, index, &specs, schema.is_some())?
        {
            let hi = next.saturating_add(rows).saturating_sub(1);
            files.row_group(columns, next, hi)?;
            next = hi.saturating_add(1);
            index = index.saturating_add(1);
        }
        if next.saturating_sub(1) != entry.lsn_hi {
            return Err(corrupt(
                &entry.path,
                format!(
                    "holds LSNs up to {}, the manifest says {}",
                    next.saturating_sub(1),
                    entry.lsn_hi
                ),
            ));
        }
        current = (
            widen_bounds(current.0, entry.observed_ts),
            widen_bounds(current.1, entry.timestamp),
        );
        let before = files.done.len();
        files.maybe_cut()?;
        if files.done.len() > before {
            bounds_by_file.push(std::mem::take(&mut current));
        }
    }
    if files.open.is_some() {
        bounds_by_file.push(current);
    }
    let mut outputs = files.finish()?;
    for (output, (observed, timestamp)) in outputs.iter_mut().zip(bounds_by_file) {
        output.file.observed_ts = observed;
        output.file.timestamp = timestamp;
    }
    Ok(outputs)
}

/// One sorted run of a [`close_hour`] merge: its current chunk of rows,
/// and the file the next chunks come from (none for the pages' run).
struct Run {
    chunk: Rc<Vec<ColumnValues>>,
    /// The chunk's index in the merge's `held`.
    held: usize,
    pos: usize,
    len: usize,
    /// Its file's index in the merge's inputs.
    input: Option<usize>,
    next_row_group: usize,
}

/// Closes an hour that spilled to `staging/` (ADR 0028): its staging runs
/// (`staging`, contiguous, oldest first, each sorted by `sort_key`) and
/// its remaining `pages` (contiguous after them; possibly none) merged into
/// `dir`, which must exist and be empty, as one file sorted by `sort_key`
/// and cut into row groups of at most `row_group_max_rows`, like
/// [`write_hour`]. The merge holds one row group of each run, not the
/// hour; ties keep arrival order. Without staging runs this is
/// [`write_hour`].
pub(crate) fn close_hour(
    dir: &Path,
    staging: &[(&FileEntry, &[u8])],
    pages: &[Page],
    format: Format,
    schema: Option<&[ColumnSpec]>,
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let (Some(first), Some(last)) = (staging.first(), staging.last()) else {
        return write_hour(dir, pages, format, tier, parquet);
    };
    let lsn_lo = first.0.lsn_lo;
    let lsn_hi = pages.last().map_or(last.0.lsn_hi, |p| p.lsn_hi);
    let inputs = staging
        .iter()
        .map(|(entry, bytes)| {
            let file = ParquetFile::open(bytes).map_err(|e| corrupt(&entry.path, e.to_string()))?;
            let fields = file.fields();
            Ok((*entry, file, fields))
        })
        .collect::<Result<Vec<_>, IngestError>>()?;
    let specs = match schema {
        Some(schema) => schema.to_vec(),
        None => widened_specs(&inputs.iter().map(|o| &o.1).collect::<Vec<_>>(), pages),
    };
    let typed = schema.is_some();
    let keys = key_positions(&specs, tier);

    let mut observed = None;
    let mut timestamp = None;
    let mut held: Vec<Rc<Vec<ColumnValues>>> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    for (i, (entry, _, _)) in inputs.iter().enumerate() {
        observed = widen_bounds(observed, entry.observed_ts);
        timestamp = widen_bounds(timestamp, entry.timestamp);
        runs.push(Run {
            chunk: Rc::new(Vec::new()),
            held: 0,
            pos: 0,
            len: 0,
            input: Some(i),
            next_row_group: 0,
        });
    }
    if !pages.is_empty() {
        let sorted = sort_pages(pages, format, &specs, tier);
        observed = widen_bounds(observed, sorted.observed);
        timestamp = widen_bounds(timestamp, sorted.timestamp);
        runs.push(Run {
            chunk: Rc::new(sorted.columns),
            held: 0,
            pos: 0,
            len: sorted.rows,
            input: None,
            next_row_group: 0,
        });
    }
    for run in &mut runs {
        if run.pos == run.len {
            load(run, &inputs, &specs, typed)?;
        }
        run.held = held.len();
        held.push(Rc::clone(&run.chunk));
    }

    let chunk = tier.row_group_max_rows.max(1);
    let mut files = Files::new(dir, specs.clone(), parquet, tier);
    // (index in `held`, row) of each row of the row group being built.
    let mut picks: Vec<(usize, usize)> = Vec::with_capacity(chunk.min(1 << 16));
    loop {
        let mut best: Option<usize> = None;
        for (i, run) in runs.iter().enumerate() {
            if run.pos == run.len {
                continue;
            }
            let better = best.and_then(|b| runs.get(b)).is_none_or(|b| {
                keys.iter()
                    .map(|&k| match (run.chunk.get(k), b.chunk.get(k)) {
                        (Some(x), Some(y)) => compare_across(x, run.pos, y, b.pos),
                        _ => std::cmp::Ordering::Equal,
                    })
                    .find(|o| o.is_ne())
                    .is_some_and(std::cmp::Ordering::is_lt)
            });
            if better {
                best = Some(i);
            }
        }
        let Some(run) = best.and_then(|b| runs.get_mut(b)) else {
            break;
        };
        picks.push((run.held, run.pos));
        run.pos = run.pos.saturating_add(1);
        if run.pos == run.len && load(run, &inputs, &specs, typed)? {
            run.held = held.len();
            held.push(Rc::clone(&run.chunk));
        }
        if picks.len() >= chunk {
            files.row_group(take_rows(&held, &picks, &specs), lsn_lo, lsn_hi)?;
            picks.clear();
            held.clear();
            for run in &mut runs {
                run.held = held.len();
                held.push(Rc::clone(&run.chunk));
            }
        }
    }
    if !picks.is_empty() {
        files.row_group(take_rows(&held, &picks, &specs), lsn_lo, lsn_hi)?;
    }
    let mut outputs = files.finish()?;
    for output in &mut outputs {
        output.file.observed_ts = observed;
        output.file.timestamp = timestamp;
    }
    Ok(outputs)
}

/// Moves `run` to its file's next non-empty row group; returns whether it
/// has one (a run without a file, or at its file's end, is done).
fn load(
    run: &mut Run,
    inputs: &[(&FileEntry, ParquetFile<'_>, Vec<Field>)],
    specs: &[ColumnSpec],
    typed: bool,
) -> Result<bool, IngestError> {
    let Some((entry, file, fields)) = run.input.and_then(|i| inputs.get(i)) else {
        return Ok(false);
    };
    while let Some((columns, rows)) =
        row_group(file, fields, &entry.path, run.next_row_group, specs, typed)?
    {
        run.next_row_group = run.next_row_group.saturating_add(1);
        if rows > 0 {
            run.chunk = Rc::new(columns);
            run.pos = 0;
            run.len = usize::try_from(rows).unwrap_or(0);
            return Ok(true);
        }
    }
    run.chunk = Rc::new(Vec::new());
    run.pos = 0;
    run.len = 0;
    Ok(false)
}

/// The rows `picks` names (index in `held`, row), in order, as columns.
fn take_rows(
    held: &[Rc<Vec<ColumnValues>>],
    picks: &[(usize, usize)],
    specs: &[ColumnSpec],
) -> Vec<ColumnValues> {
    specs
        .iter()
        .enumerate()
        .map(|(c, spec)| {
            let mut out = ColumnValues::empty(spec.column_type);
            // Gather each stretch of rows from one chunk at once.
            for stretch in picks.chunk_by(|a, b| a.0 == b.0) {
                let Some(values) = stretch.first().and_then(|(h, _)| held.get(*h)?.get(c)) else {
                    continue;
                };
                let rows: Vec<usize> = stretch.iter().map(|(_, r)| *r).collect();
                out.append(gather(values, &rows));
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests;
