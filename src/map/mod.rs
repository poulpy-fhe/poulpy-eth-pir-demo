//! The balance map: the authoritative plaintext table this service serves from.

mod apply;
mod lock;
mod snapshot;
mod types;

pub use apply::Applied;
pub use lock::SnapshotLock;
pub use types::{BalanceMap, Entry, Reading};

#[cfg(test)]
mod tests;
