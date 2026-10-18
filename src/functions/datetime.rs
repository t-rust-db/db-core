// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's date and time functions: `date`, `time`, `datetime`, `julianday`, `unixepoch`, `strftime`, `timediff` (#651), with sqlite3 3.53.4's semantics.

use super::ScalarFn;

/// The function named `name` (lowercase) at `arity`, if this family has
/// it.
pub(super) fn lookup(name: &str, arity: usize) -> Option<ScalarFn> {
    let _ = (name, arity);
    None
}
