// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `vm::stream` -- the opcodes `vm::batch` has no notion of (ADR 0018
//! §Opcodes). Only `Prune` (segment selection before any row is
//! materialized) is implemented so far; `Parse`/`Window`/`Watermark`/
//! `Emit` are later phases of the epic (#307-#309).
//!
//! A stream [`Program`] is a prologue (`Prune`) around a `Body` reused
//! verbatim from [`crate::vm::batch`]: `codegen::stream::compile` builds
//! one, `engine::stream::StreamEngine` drives the prologue itself
//! (selecting segments) before handing the body to `vm::batch::Vm::execute`.

use std::time::Duration;

/// How far back from EOF a stream query reaches (ADR 0018 §Scope and
/// retention). Per query; retention (what the ring holds hot) is the
/// ring's own business.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    /// A duration back from the latest observed/event time.
    Time(Duration),
    /// The last `n` lines.
    Lines(u64),
    /// The last `n` bytes.
    Bytes(u64),
    /// Every held/reachable segment; well-defined only for non-blocking
    /// queries in follow mode (ADR 0018 §Planner point 1).
    All,
}

/// A segment-skipping predicate `Prune` selects on: a leaf `codegen::
/// stream::compile` lifted out of `WHERE`/`SINCE`/`UNTIL` because
/// `storage::stream::Segment` can answer it without materializing a row
/// (minmax for time ranges, dictionaries for `=`).
#[derive(Debug, Clone, PartialEq)]
pub enum IndexPred {
    /// Event-time range `[lo, hi)`, nanoseconds since epoch.
    TimeRange {
        /// Inclusive lower bound.
        lo: i64,
        /// Exclusive upper bound.
        hi: i64,
    },
    /// `column = value` on a dictionary-encoded Tier-3/Tier-2b column.
    DictEq {
        /// The column name.
        column: String,
        /// The literal value compared against.
        value: String,
    },
}

/// Segment selection before any row is materialized (ADR 0018 §Opcodes).
#[derive(Debug, Clone, PartialEq)]
pub struct Prune {
    /// The scope this query is bounded to.
    pub scope: Scope,
    /// Index-pushable predicates, all of which must hold (AND) for a
    /// segment to survive pruning.
    pub preds: Vec<IndexPred>,
}

/// A compiled stream query: a `Prune` prologue plus a `vm::batch::Program`
/// body reused verbatim (ADR 0018's core claim -- filter/project/
/// aggregate/sort/limit need no stream-specific reimplementation).
#[derive(Debug, Clone)]
pub struct Program {
    /// Segment-selection prologue.
    pub prune: Prune,
    /// The residual query, compiled by `codegen::batch::compile` and run
    /// unchanged per surviving segment.
    pub body: crate::vm::batch::Program,
}
