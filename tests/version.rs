#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

//! Drift guard for `db_core::VERSION`.
//!
//! `src/lib.rs` carries the crate version as a plain constant (the
//! qualified-subset gate, `make check-mvl-limit`, keeps `env!` out of
//! `src/`), so nothing in the library itself ties it to `Cargo.toml`.
//! This integration test does -- it lives under `tests/`, outside the
//! gate's scan, where `env!` is fine.

#[test]
fn version_constant_matches_cargo_toml() {
    assert_eq!(
        db_core::VERSION,
        env!("CARGO_PKG_VERSION"),
        "src/lib.rs::VERSION and Cargo.toml's `version` have drifted -- bump both together"
    );
}
