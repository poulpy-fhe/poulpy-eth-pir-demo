use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct FollowConfig {
    pub chunk: u64,
    pub poll_interval: Duration,
    pub snapshot_every: Duration,
    pub snapshot_path: PathBuf,
    pub retry_base: Duration,
    pub tip: crate::chain::Tip,
    pub reorg_window: u64,
    /// Where to publish how far behind we are. `None` outside `serve`.
    pub progress: Option<crate::progress::Handle>,
}

impl Default for FollowConfig {
    fn default() -> Self {
        Self {
            chunk: 50,
            poll_interval: Duration::from_secs(60),
            snapshot_every: Duration::from_secs(600),
            snapshot_path: PathBuf::from("data/balances.snapshot"),
            retry_base: Duration::from_secs(2),
            tip: crate::chain::Tip::Finalized,
            reorg_window: 64,
            progress: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BatchStats {
    pub logs: usize,
    pub touched: usize,
    pub inserted: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub skipped_new_zero: usize,
}

/// Totals for one sync call.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyncStats {
    pub blocks: u64,
    pub batches: usize,
    pub logs: usize,
    pub touched: usize,
    pub inserted: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub skipped_new_zero: usize,
    pub pruned: usize,
}

impl SyncStats {
    pub fn fold(&mut self, b: BatchStats) {
        self.batches += 1;
        self.logs += b.logs;
        self.touched += b.touched;
        self.inserted += b.inserted;
        self.updated += b.updated;
        self.removed += b.removed;
        self.unchanged += b.unchanged;
        self.skipped_new_zero += b.skipped_new_zero;
    }
}
