// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Child process for `tests/tier_crash.rs` (#648): a tiered record table
//! under a fast clock (one real millisecond is [`SPEED`] simulated ones),
//! so hours close, the open hour spills and days merge while the parent
//! kills this process with SIGKILL at random points.
//!
//! Usage: `tier_probe <table> <clock origin ns> <first id>`. Rows are
//! `(id, kind, n)` with `id` counting up from `<first id>`; each is
//! appended in groups of five, four `Ack::Memory` then one `Ack::Durable`,
//! whose fsync covers the group, printed as `ack <first>..=<last>` once
//! acked. Every
//! seal, flush and compact is bracketed by `begin <phase>` / `end
//! <phase>`, where the phase of a seal that spills is `spill`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    reason = "test helper process, killed mid-run by design"
)]

use std::io::Write;
use std::path::PathBuf;

#[path = "tier_clock.rs"]
mod tier_clock;

use db_core::storage::ingest::{Ack, Page, RecordValue, TableWriter};

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let table = PathBuf::from(
        args.next()
            .expect("usage: tier_probe <table> <origin> <first id>"),
    );
    tier_clock::set_origin(args.next().expect("origin").parse().expect("origin ns"));
    let mut id: i64 = args.next().expect("first id").parse().expect("first id");
    let mut w = TableWriter::open(&table, tier_clock::options()).expect("open table");
    let mut step = 0u64;
    loop {
        let kind = format!("k{}", id % 3);
        // Four in memory, then one durable: its fsync covers all five.
        let durable = step % 5 == 4;
        w.append_row(
            vec![
                RecordValue::Int(id),
                RecordValue::Str(kind),
                RecordValue::Int(id % 97),
            ],
            if durable { Ack::Durable } else { Ack::Memory },
        )
        .unwrap();
        if durable {
            say(&format!("ack {}..={id}", id - 4));
        }
        id += 1;
        step += 1;
        if step.is_multiple_of(7) {
            let pending: usize = w.sealed_pages().iter().map(Page::len).sum::<usize>()
                + w.head_page().map_or(0, |p| p.len());
            let phase = if pending > tier_clock::SPILL_ROWS {
                "spill"
            } else {
                "seal"
            };
            say(&format!("begin {phase}"));
            w.seal_head().unwrap();
            say(&format!("end {phase}"));
        }
        if step.is_multiple_of(19) {
            say("begin flush");
            w.flush().unwrap();
            say("end flush");
        }
        if step.is_multiple_of(53) {
            say("begin compact");
            w.compact().unwrap();
            say("end compact");
        }
    }
}
