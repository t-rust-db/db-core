//! Push-driven codegen for live/unbounded sources -- one of
//! `sql-codegen`'s three emitters (see crate root docs), pairing with
//! `crate::vm::stream`.
//!
//! **Not yet implemented.** No product has a streaming query planner to
//! extract this from yet -- unlike [`super::batch`] (extracted from
//! column-rs's real `src/codegen.rs`) and [`super::row`] (a real,
//! documented port target once `crate::vm::row` exists), there is no
//! existing "streaming codegen" anywhere in the ecosystem today to
//! mechanically port. Primary anticipated consumer: loglume (live log
//! tailing from files, Docker, journald, K8s), per `crate::vm::stream`'s
//! own doc comment.
//!
//! [`super::batch`] is the reference for what "done" looks like here.
