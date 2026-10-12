//! A tiered table's outputs (ADR 0028, #573): a closed hour from its
//! pages, a closed day from its hour files.
//!
//! Both write a directory of files the caller publishes with one rename,
//! each file a contiguous LSN range for the manifest. A closed hour is one
//! file: its rows sorted by `sort_key`, then cut into row groups, so each
//! row group holds a range of the sort key. A day merge copies those row
//! groups as they are (a row group never spans an hour), and starts a new
//! file at an input file boundary once a file reaches `max_file_size`.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use super::flush::{add_field, bounds, convert, predefined_specs, schema, FlushedFile};
use super::layout::{compare, TierOptions};
use super::manifest::FileEntry;
use super::page::Page;
use super::snapshot::{cast, column_type};
use super::table::IngestError;
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::writer::{ColumnSpec, ParquetWriter, WriterOptions};
use crate::storage::parquet::ParquetFile;
use crate::storage::stream::adapter::column_values;
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
        columns: Vec<crate::storage::parquet::writer::ColumnValues>,
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

/// Every row of `pages`, column `spec`, as the table's values.
fn page_rows(pages: &[Page], spec: &ColumnSpec) -> Vec<Value> {
    pages
        .iter()
        .flat_map(|page| {
            column_values(&page.segment, &spec.name)
                .unwrap_or_else(|| vec![Value::Null; page.segment.len()])
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

/// Writes one closed hour's `pages` (contiguous, oldest first, at least
/// one) into `dir`, which must exist and be empty: one file, the hour's
/// rows sorted by `sort_key` and cut into row groups of at most
/// `row_group_max_rows`, so each row group holds a range of the sort key
/// and its statistics prune on it (amendment to ADR 0028).
pub(crate) fn write_hour(
    dir: &Path,
    pages: &[Page],
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
        return Err(IngestError::Corrupt("no pages to write".to_string()));
    };
    let specs = schema(pages);
    let total: usize = pages.iter().map(|p| p.segment.len()).sum();
    let columns: Vec<Vec<Value>> = specs.iter().map(|s| page_rows(pages, s)).collect();
    let columns = sorted(columns, &specs, &tier.sort_key);
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
    let chunk = tier.row_group_max_rows.max(1);
    let mut files = Files::new(dir, specs.clone(), parquet, tier);
    let mut lo = 0usize;
    while lo < total {
        let hi = lo.saturating_add(chunk).min(total);
        let values = columns
            .iter()
            .zip(&specs)
            .map(|(values, spec)| {
                convert(
                    values.get(lo..hi).unwrap_or_default().to_vec(),
                    spec.column_type,
                )
            })
            .collect();
        files.row_group(values, first.lsn_lo, last.lsn_hi)?;
        lo = hi;
    }
    let mut outputs = files.finish()?;
    for output in &mut outputs {
        output.file.observed_ts = observed;
        output.file.timestamp = timestamp;
    }
    Ok(outputs)
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

/// Merges one closed day's hour files (`inputs`, contiguous, oldest first)
/// into `dir`, which must exist and be empty: their row groups, in order,
/// under the day's widened schema.
pub(crate) fn merge_day(
    dir: &Path,
    inputs: &[(&FileEntry, &[u8])],
    tier: &TierOptions,
    parquet: &WriterOptions,
) -> Result<Vec<Output>, IngestError> {
    let corrupt = |path: &str, why: String| IngestError::Corrupt(format!("{path}: {why}"));
    let opened = inputs
        .iter()
        .map(|(entry, bytes)| {
            ParquetFile::open(bytes)
                .map(|f| (*entry, f))
                .map_err(|e| corrupt(&entry.path, e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut specs = predefined_specs();
    for (_, file) in &opened {
        for element in file.metadata().schema.iter().skip(1) {
            if let Some(t) = element.physical_type.and_then(column_type) {
                add_field(&mut specs, &element.name, t);
            }
        }
    }
    let mut files = Files::new(dir, specs.clone(), parquet, tier);
    let mut bounds_by_file = Vec::new();
    let mut current: (Bounds, Bounds) = (None, None);
    for (entry, file) in &opened {
        let leaves: Vec<(&str, usize, PhysicalType)> = file
            .metadata()
            .schema
            .iter()
            .skip(1)
            .enumerate()
            .filter_map(|(i, e)| e.physical_type.map(|pt| (e.name.as_str(), i, pt)))
            .collect();
        let mut next = entry.lsn_lo;
        for rg in file.row_groups() {
            let rows = u64::try_from(rg.num_rows())
                .map_err(|_| corrupt(&entry.path, "negative row count".to_string()))?;
            let mut columns = Vec::with_capacity(specs.len());
            for spec in &specs {
                let values = match leaves.iter().find(|(name, _, _)| *name == spec.name) {
                    Some((_, index, pt)) => read_values(&rg, *index, *pt)
                        .map_err(|why| corrupt(&entry.path, why))?
                        .into_iter()
                        .map(|v| cast(v, spec.column_type))
                        .collect(),
                    None => vec![Value::Null; usize::try_from(rows).unwrap_or(0)],
                };
                columns.push(convert(values, spec.column_type));
            }
            let hi = next.saturating_add(rows).saturating_sub(1);
            files.row_group(columns, next, hi)?;
            next = hi.saturating_add(1);
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

#[cfg(test)]
mod tests;
