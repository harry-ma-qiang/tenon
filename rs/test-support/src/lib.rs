pub mod procs;
pub mod temp;
pub mod wire;

#[cfg(feature = "node")]
pub mod fixture;

#[cfg(feature = "node")]
pub use fixture::{collect, release, repo, skip, skip_release, wait_until, Fixture, Spec};

pub use procs::{alive, kill, kill_alive, pids_by_home, pids_by_sock, wait_gone};
pub use temp::Temp;
pub use wire::{raw_connect, read_frame, send_frame, send_raw};
