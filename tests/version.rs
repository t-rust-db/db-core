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
