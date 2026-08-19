#![allow(dead_code)]

pub use tenon_test_support::*;

use std::path::PathBuf;
use std::time::Duration;

pub const BIN: &str = env!("CARGO_BIN_EXE_tenon");

pub fn fixture(name: &str) -> Option<Fixture> {
    fixture_with_config(name, None)
}

/// The adversarial suite kills base with -9 and races container teardown, so
/// every fixture here serializes against the others and sweeps whatever pids
/// its home still owns.
pub fn fixture_with_config(name: &str, config: Option<&str>) -> Option<Fixture> {
    let release = release_dir(name)?;
    Some(Fixture::open(
        BIN,
        release,
        Spec {
            name,
            config,
            lock: true,
            reap_pids: true,
            limit: Some(Duration::from_secs(60)),
            ..Spec::default()
        },
    ))
}

fn release_dir(name: &str) -> Option<PathBuf> {
    skip_release(name)
}
