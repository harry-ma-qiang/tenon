#![allow(dead_code)]

pub use tenon_test_support::*;

use std::path::PathBuf;

/// `CARGO_BIN_EXE_*` exists only in this crate's own test targets, so the
/// shared crate is handed the binary path rather than finding it.
pub const BIN: &str = env!("CARGO_BIN_EXE_tenon");

pub fn fixture(name: &str, release: PathBuf, config: &str, harness: &str) -> Fixture {
    Fixture::new(BIN, name, release, config, harness)
}

pub fn plain(name: &str, release: PathBuf, config: &str) -> Fixture {
    Fixture::open(
        BIN,
        release,
        Spec {
            name,
            config: Some(config),
            ..Spec::default()
        },
    )
}
