//! The balance map: the authoritative plaintext table this service serves from.

mod apply;
mod install;
mod lock;
mod snapshot;
mod types;

pub use apply::Applied;
pub use install::install_snapshot;
pub use lock::SnapshotLock;
pub(crate) use lock::{AdvisoryLock, appended_path, lock_path};
pub(crate) use snapshot::{fsync_parent, remove_file_durable};
pub use types::{BalanceMap, Entry, Reading};

#[cfg(test)]
mod tests;
