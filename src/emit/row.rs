//! Rust-source emitter for a future `vm::row` program -- one of `emit`'s
//! three emitters (see module docs).
//!
//! **Not yet implemented.** Blocked on [`crate::codegen::row`] (the
//! row planner) and [`crate::vm::row`] existing for real; there is no
//! planned row `Program` to render yet. sqlite-rs itself has no AOT
//! source-emission step, so unlike `codegen::row` this is not a port
//! target -- whether a row emitter is ever wanted is an open question.
