//! Vendored subset of POSIX syscalls: byte-range `fcntl` locking and
//! `termios` raw mode. Extracted from sqlite-rs's `src/sys/*` (db-core#11)
//! -- the lowest-level, dependency-free module in that crate's own
//! vendored-syscall layer (see sqlite-rs's `.openspec/adr/
//! 0031-vendor-nix-subset.md` for why these are hand-rolled FFI rather
//! than a `nix` dependency).
//!
//! This is db-core's sole `#![allow(unsafe_code)]` carve-out -- every
//! other workspace crate `#![forbid(unsafe_code)]`s. Raw syscall FFI has
//! no pure-Rust/`std` equivalent for byte-range locks or terminal raw
//! mode, so the boundary here is unavoidable, not merely convenient.

#![allow(unsafe_code)]

pub mod fcntl;
pub mod termios;
