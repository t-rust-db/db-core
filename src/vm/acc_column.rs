// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! A `Combine` accumulator column (#643): one entry per group, typed where
//! the column is a `GROUP BY` key of integers or strings.
//!
//! A [`super::combine::Combiner`] keeps one entry per group per slot, and
//! a worker's partial keeps every group it has seen until the final merge.
//! As `Vec<Value>` that is 32 bytes a cell, plus a heap string for every
//! string key. A key column of integers is 8 bytes a group here and a key
//! column of strings 4, a code into the column's own [`Interner`]; the
//! aggregate slots, which `merge_slot` updates, stay [`Value`]s.
//!
//! A cell hashes exactly as its [`Value`] does ([`hash_one_value`]) and
//! compares as `Value ==` does, so typed and untyped columns, and a typed
//! partial against an untyped chunk, find the same groups.

use std::collections::HashMap;
use std::sync::Arc;

use super::batch::{hash_one_value, Value};

/// The code a `NULL` string key takes in [`AccColumn::Strs`].
const NULL_CODE: u32 = u32::MAX;

/// A column's distinct strings, each stored once, with its hash.
#[derive(Debug, Default, Clone)]
pub(crate) struct Interner {
    strings: Vec<Arc<str>>,
    hashes: Vec<u64>,
    codes: HashMap<Arc<str>, u32>,
}

impl Interner {
    /// `s`'s code, added if new; `None` once `u32` codes run out.
    fn code(&mut self, s: &str) -> Option<u32> {
        if let Some(&code) = self.codes.get(s) {
            return Some(code);
        }
        let code = u32::try_from(self.strings.len())
            .ok()
            .filter(|&c| c != NULL_CODE)?;
        let string: Arc<str> = Arc::from(s);
        self.hashes
            .push(hash_one_value(&Value::Str(s.to_string().into())));
        self.strings.push(Arc::clone(&string));
        self.codes.insert(string, code);
        Some(code)
    }

    fn get(&self, code: u32) -> Option<&str> {
        self.strings
            .get(usize::try_from(code).ok()?)
            .map(AsRef::as_ref)
    }

    fn hash(&self, code: u32) -> u64 {
        usize::try_from(code)
            .ok()
            .and_then(|c| self.hashes.get(c))
            .copied()
            .unwrap_or_else(|| hash_one_value(&Value::Null))
    }
}

/// One accumulator column: an entry per group.
#[derive(Debug, Clone)]
pub(crate) enum AccColumn {
    /// Any values.
    Values(Vec<Value>),
    /// A key column of integers; `nulls[g]` marks a `NULL` key.
    Ints { data: Vec<i64>, nulls: Vec<bool> },
    /// A key column of strings, as codes into `strings` ([`NULL_CODE`]
    /// for a `NULL` key).
    Strs { codes: Vec<u32>, strings: Interner },
}

impl AccColumn {
    /// An empty column shaped for the first chunk's `values`: integers
    /// when the column is a `key` or a `count` and every value is an
    /// integer, interned strings when a `key` of strings (`NULL`s allowed
    /// either way).
    pub(crate) fn for_values(key: bool, count: bool, values: &[Value], capacity: usize) -> Self {
        let all = |f: fn(&Value) -> bool| values.iter().all(|v| matches!(v, Value::Null) || f(v));
        if (key || count) && all(|v| matches!(v, Value::Int(_))) {
            AccColumn::Ints {
                data: Vec::with_capacity(capacity),
                nulls: Vec::with_capacity(capacity),
            }
        } else if key && all(|v| matches!(v, Value::Str(_))) {
            AccColumn::Strs {
                codes: Vec::with_capacity(capacity),
                strings: Interner::default(),
            }
        } else {
            AccColumn::Values(Vec::with_capacity(capacity))
        }
    }

    /// An empty column of `other`'s kind (its own interner).
    pub(crate) fn like(other: &AccColumn, capacity: usize) -> Self {
        match other {
            AccColumn::Values(_) => AccColumn::Values(Vec::with_capacity(capacity)),
            AccColumn::Ints { .. } => AccColumn::Ints {
                data: Vec::with_capacity(capacity),
                nulls: Vec::with_capacity(capacity),
            },
            AccColumn::Strs { .. } => AccColumn::Strs {
                codes: Vec::with_capacity(capacity),
                strings: Interner::default(),
            },
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            AccColumn::Values(values) => values.len(),
            AccColumn::Ints { data, .. } => data.len(),
            AccColumn::Strs { codes, .. } => codes.len(),
        }
    }

    /// Entry `g` as a [`Value`] (`NULL` past the end).
    pub(crate) fn value(&self, g: usize) -> Value {
        match self {
            AccColumn::Values(values) => values.get(g).cloned().unwrap_or(Value::Null),
            AccColumn::Ints { data, nulls } => match (data.get(g), nulls.get(g)) {
                (Some(&v), Some(false)) => Value::Int(v),
                _ => Value::Null,
            },
            AccColumn::Strs { codes, strings } => codes
                .get(g)
                .and_then(|&c| strings.get(c))
                .map_or(Value::Null, |s| Value::Str(s.to_string().into())),
        }
    }

    /// The [`hash_one_value`] of entry `g`.
    pub(crate) fn hash(&self, g: usize) -> u64 {
        match self {
            AccColumn::Values(values) => values
                .get(g)
                .map_or_else(|| hash_one_value(&Value::Null), hash_one_value),
            AccColumn::Ints { data, nulls } => match (data.get(g), nulls.get(g)) {
                (Some(&v), Some(false)) => hash_one_value(&Value::Int(v)),
                _ => hash_one_value(&Value::Null),
            },
            AccColumn::Strs { codes, strings } => {
                strings.hash(codes.get(g).copied().unwrap_or(NULL_CODE))
            }
        }
    }

    /// Whether entry `g` equals `v` as `Value ==` would have it.
    pub(crate) fn eq_value(&self, g: usize, v: &Value) -> bool {
        match (self, v) {
            (AccColumn::Values(values), _) => values.get(g) == Some(v),
            (AccColumn::Ints { data, nulls }, Value::Int(x)) => {
                nulls.get(g) == Some(&false) && data.get(g) == Some(x)
            }
            (AccColumn::Ints { nulls, .. }, Value::Null) => nulls.get(g) == Some(&true),
            (AccColumn::Strs { codes, strings }, Value::Str(s)) => codes
                .get(g)
                .and_then(|&c| strings.get(c))
                .is_some_and(|t| t == s.as_ref()),
            (AccColumn::Strs { codes, .. }, Value::Null) => codes.get(g) == Some(&NULL_CODE),
            _ => false,
        }
    }

    /// Whether entry `g` equals entry `r` of `other`.
    pub(crate) fn eq_entry(&self, g: usize, other: &AccColumn, r: usize) -> bool {
        match (self, other) {
            (
                AccColumn::Ints { data, nulls },
                AccColumn::Ints {
                    data: od,
                    nulls: on,
                },
            ) => {
                let null = nulls.get(g);
                null.is_some()
                    && null == on.get(r)
                    && (null == Some(&true) || data.get(g) == od.get(r))
            }
            (
                AccColumn::Strs { codes, strings },
                AccColumn::Strs {
                    codes: oc,
                    strings: os,
                },
            ) => match (codes.get(g), oc.get(r)) {
                (Some(&NULL_CODE), Some(&NULL_CODE)) => true,
                (Some(&a), Some(&b)) if a != NULL_CODE && b != NULL_CODE => {
                    strings.get(a).is_some() && strings.get(a) == os.get(b)
                }
                _ => false,
            },
            _ => self.eq_value(g, &other.value(r)),
        }
    }

    /// Appends `v`. A value the typed column cannot hold turns it back
    /// into [`AccColumn::Values`] first.
    pub(crate) fn push_value(&mut self, v: &Value) {
        let pushed = match (&mut *self, v) {
            (AccColumn::Values(values), _) => {
                values.push(v.clone());
                true
            }
            (AccColumn::Ints { data, nulls }, Value::Int(x)) => {
                data.push(*x);
                nulls.push(false);
                true
            }
            (AccColumn::Ints { data, nulls }, Value::Null) => {
                data.push(0);
                nulls.push(true);
                true
            }
            (AccColumn::Strs { codes, strings }, Value::Str(s)) => match strings.code(s) {
                Some(code) => {
                    codes.push(code);
                    true
                }
                None => false,
            },
            (AccColumn::Strs { codes, .. }, Value::Null) => {
                codes.push(NULL_CODE);
                true
            }
            _ => false,
        };
        if !pushed {
            self.demote();
            if let AccColumn::Values(values) = self {
                values.push(v.clone());
            }
        }
    }

    /// Appends entry `r` of `other`, re-coding a string into this
    /// column's interner.
    pub(crate) fn push_entry(&mut self, other: &AccColumn, r: usize) {
        match (&mut *self, other) {
            (
                AccColumn::Ints { data, nulls },
                AccColumn::Ints {
                    data: od,
                    nulls: on,
                },
            ) => {
                data.push(od.get(r).copied().unwrap_or(0));
                nulls.push(on.get(r).copied().unwrap_or(true));
            }
            (AccColumn::Values(values), AccColumn::Values(ov)) => {
                values.push(ov.get(r).cloned().unwrap_or(Value::Null));
            }
            _ => self.push_value(&other.value(r)),
        }
    }

    /// Adds `n` to integer entry `g`, a `NULL` entry counting as 0 (a
    /// typed `COUNT` slot); `None` when the column is not integers or
    /// the sum overflows.
    pub(crate) fn add_int(&mut self, g: usize, n: i64) -> Option<()> {
        let AccColumn::Ints { data, nulls } = self else {
            return None;
        };
        let (Some(d), Some(null)) = (data.get_mut(g), nulls.get_mut(g)) else {
            return None;
        };
        *d = (if *null { 0 } else { *d }).checked_add(n)?;
        *null = false;
        Some(())
    }

    /// A mutable entry of a [`AccColumn::Values`] column (the aggregate
    /// slots `merge_slot` updates); `None` for a typed column.
    pub(crate) fn value_mut(&mut self, g: usize) -> Option<&mut Value> {
        match self {
            AccColumn::Values(values) => values.get_mut(g),
            _ => None,
        }
    }

    /// Swaps entries `i` and `j`.
    pub(crate) fn swap(&mut self, i: usize, j: usize) {
        let n = self.len();
        if i >= n || j >= n {
            return;
        }
        match self {
            AccColumn::Values(values) => values.swap(i, j),
            AccColumn::Ints { data, nulls } => {
                data.swap(i, j);
                nulls.swap(i, j);
            }
            AccColumn::Strs { codes, .. } => codes.swap(i, j),
        }
    }

    /// Every entry as a [`Value`].
    pub(crate) fn into_values(self) -> Vec<Value> {
        match self {
            AccColumn::Values(values) => values,
            typed => (0..typed.len()).map(|g| typed.value(g)).collect(),
        }
    }

    /// Turns a typed column into [`AccColumn::Values`], keeping its
    /// entries.
    fn demote(&mut self) {
        if !matches!(self, AccColumn::Values(_)) {
            let typed = std::mem::replace(self, AccColumn::Values(Vec::new()));
            *self = AccColumn::Values(typed.into_values());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(x.to_string().into())
    }

    #[test]
    fn typed_columns_hash_and_compare_as_their_values_do() {
        let values = [Value::Int(7), Value::Null, Value::Int(-1)];
        let mut ints = AccColumn::for_values(true, false, &values, 0);
        let strs_in = [s("a"), Value::Null, s("bb"), s("a")];
        let mut strs = AccColumn::for_values(true, false, &strs_in, 0);
        assert!(matches!(ints, AccColumn::Ints { .. }));
        assert!(matches!(strs, AccColumn::Strs { .. }));
        for v in &values {
            ints.push_value(v);
        }
        for v in &strs_in {
            strs.push_value(v);
        }
        for (g, v) in values.iter().enumerate() {
            assert_eq!(ints.hash(g), hash_one_value(v));
            assert!(ints.eq_value(g, v));
            assert_eq!(ints.value(g), *v);
        }
        for (g, v) in strs_in.iter().enumerate() {
            assert_eq!(strs.hash(g), hash_one_value(v));
            assert!(strs.eq_value(g, v));
            assert_eq!(strs.value(g), *v);
        }
        assert!(!ints.eq_value(0, &Value::Null));
        assert!(!ints.eq_value(1, &Value::Int(0)));
        assert!(!strs.eq_value(0, &s("bb")));
        assert!(!strs.eq_value(1, &s("")));
        // "a" is stored once.
        let AccColumn::Strs { strings, .. } = &strs else {
            panic!("a string key column")
        };
        assert_eq!(strings.strings.len(), 2);
    }

    #[test]
    fn entries_move_between_typed_columns_and_demote_on_a_foreign_value() {
        let mut a = AccColumn::for_values(true, false, &[s("x")], 0);
        let mut b = AccColumn::for_values(true, false, &[s("y")], 0);
        a.push_value(&s("x"));
        b.push_value(&s("y"));
        b.push_value(&s("x"));
        b.push_value(&Value::Null);
        a.push_entry(&b, 1);
        a.push_entry(&b, 2);
        assert!(a.eq_entry(0, &b, 1));
        assert!(a.eq_entry(2, &b, 2));
        assert!(!a.eq_entry(0, &b, 0));
        assert_eq!(a.clone().into_values(), vec![s("x"), s("x"), Value::Null]);
        a.swap(0, 2);
        assert_eq!(a.value(0), Value::Null);
        a.push_value(&Value::Int(1));
        assert!(matches!(a, AccColumn::Values(_)));
        assert_eq!(
            a.into_values(),
            vec![Value::Null, s("x"), s("x"), Value::Int(1)]
        );

        let mut ints = AccColumn::for_values(true, false, &[Value::Int(1)], 0);
        let mut other = AccColumn::like(&ints, 0);
        other.push_value(&Value::Int(1));
        other.push_value(&Value::Null);
        ints.push_entry(&other, 0);
        ints.push_entry(&other, 1);
        assert!(ints.eq_entry(0, &other, 0) && ints.eq_entry(1, &other, 1));
        assert!(!ints.eq_entry(0, &other, 1));
        assert!(ints.value_mut(0).is_none());
        ints.push_value(&Value::Float(0.5));
        assert_eq!(ints.value(2), Value::Float(0.5));
        assert!(ints.value_mut(2).is_some());
        // Not keys: never typed.
        assert!(matches!(
            AccColumn::for_values(false, false, &[Value::Int(1)], 0),
            AccColumn::Values(_)
        ));
    }
}
