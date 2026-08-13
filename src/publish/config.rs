use std::time::Duration;

#[derive(Clone, Debug)]
pub struct PirConfig {
    /// How often to publish absorbed updates. A publish costs the same whatever
    /// the batch size, so this is the dial between staleness and CPU.
    pub rebuild_every: Duration,
    /// Compact the keyword delta once it has grown by this many addresses.
    ///
    /// Until compaction, new addresses stay queryable through an append-only
    /// delta that every client must sync, at 20 B each. Compaction folds that
    /// back into the MPHF, and costs a full client resync.
    pub compact_after: usize,
    /// Compact once the tail download reaches this percentage of the MPHF blob.
    /// `0` disables the size-based trigger.
    pub compact_tail_percent: usize,
    /// Where the keyword index is persisted, so a restart keeps every slot.
    pub keyword: crate::keyword_store::Paths,
}

impl Default for PirConfig {
    fn default() -> Self {
        Self {
            keyword: crate::keyword_store::Paths::new(std::path::Path::new("data/keyword")),
            rebuild_every: Duration::from_secs(30),
            // eth-pir measures the delta as cheaper up to ~212 K inserts
            // against a 4.05 MiB MPHF refetch.
            compact_after: 200_000,
            compact_tail_percent: 100,
        }
    }
}
