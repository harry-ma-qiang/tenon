//! The tenon message fabric (RFC P4 sections 2-4): one `Envelope`, a lock-free
//! `Hub` with per-subscriber rings, a durable writer with 5 ms group commit and
//! log replay, and a `tracing` layer so any Rust component publishes by `info!`.
//!
//! The hub knows nothing of SQLite: the host supplies durability behind the
//! `Durable` trait, and env-scoping (RFC 8d.2) is a `Filter` the host pins to a
//! caller's env before it ever reaches here.

pub mod envelope;
pub mod filter;
pub mod hub;
pub mod layer;
pub mod ring;

pub use envelope::{now_ms, ulid, Envelope, Level};
pub use filter::{glob, Filter};
pub use hub::{Durable, Hub, SubOpts};
pub use layer::BusLayer;
pub use ring::{Published, Ring, Subscription};
