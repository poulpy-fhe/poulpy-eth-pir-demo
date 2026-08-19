//! The listener: fold every balance-moving event into the map.

mod apply;
mod config;
mod logs;
mod loop_run;
mod syncer;
mod watch;

pub use apply::{apply_range, apply_range_strict};
pub use config::{FollowConfig, SyncStats};
pub use logs::is_result_cap_message;
pub use loop_run::{PassState, run};
pub use syncer::{preflight, sync, sync_into, sync_startup};

#[cfg(test)]
mod tests;
