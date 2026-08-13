use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use eth_pir::EthPirServer;

use crate::map::Entry;
use crate::record::UsdtUsdc;

use super::checkpoint::{publish_directory, save_checkpoint};
use super::{PirConfig, PirSnapshot, UpdateBatch};

/// Absorb updates, publish on the cadence, compact when the delta warrants it.
///
/// Single-threaded on purpose. A publish takes seconds, and updates simply
/// queue on the channel and are drained together afterwards.
pub(super) fn serve(
    mut server: EthPirServer<UsdtUsdc>,
    cfg: PirConfig,
    updates: Receiver<UpdateBatch>,
    current: PirSnapshot,
    directory: crate::directory::Handle,
) {
    let mut worker = PirWorker::new(cfg, server.len(), current);
    loop {
        if !worker.wait_for_deadline(&mut server, &updates) {
            return;
        }
        worker.publish(&mut server);
        worker.save_after_compaction(&mut server);
        publish_directory(&directory, &worker.cfg.keyword, &server);
    }
}

struct PirWorker {
    cfg: PirConfig,
    last_publish: Instant,
    len_at_compaction: usize,
    absorbed_since_publish: usize,
    current: PirSnapshot,
}

impl PirWorker {
    fn new(cfg: PirConfig, len_at_compaction: usize, current: PirSnapshot) -> Self {
        Self {
            cfg,
            last_publish: Instant::now(),
            len_at_compaction,
            absorbed_since_publish: 0,
            current,
        }
    }

    fn wait_for_deadline(
        &mut self,
        server: &mut EthPirServer<UsdtUsdc>,
        updates: &Receiver<UpdateBatch>,
    ) -> bool {
        loop {
            let wait = self.publish_wait();
            if wait.is_zero() {
                return true;
            }
            match updates.recv_timeout(wait) {
                Ok(batch) => self.absorb(server, batch),
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::info!("syncer stopped; PIR thread exiting");
                    return false;
                }
            }
        }
    }

    fn publish_wait(&self) -> Duration {
        (self.last_publish + self.cfg.rebuild_every).saturating_duration_since(Instant::now())
    }

    fn absorb(&mut self, server: &mut EthPirServer<UsdtUsdc>, batch: UpdateBatch) {
        for (addr, entry) in batch {
            self.absorb_one(server, addr, entry);
        }
    }

    fn absorb_one(
        &mut self,
        server: &mut EthPirServer<UsdtUsdc>,
        addr: eth_pir::Address,
        entry: Entry,
    ) {
        match server.update(addr, entry) {
            Ok(()) => self.record_absorbed(addr, entry),
            Err(e) => tracing::error!("PIR update rejected: {e}"),
        }
    }

    fn record_absorbed(&mut self, addr: eth_pir::Address, entry: Entry) {
        self.record_current_value(addr, entry);
        self.absorbed_since_publish += 1;
    }

    fn record_current_value(&mut self, addr: eth_pir::Address, entry: Entry) {
        if entry.is_zero() {
            self.current.remove(&addr);
        } else {
            self.current.insert(addr, entry);
        }
    }

    fn publish(&mut self, server: &mut EthPirServer<UsdtUsdc>) {
        let started = Instant::now();
        match server.try_rebuild_database() {
            Ok(Some(timings)) => self.record_publish(server, timings, started),
            Ok(None) => tracing::debug!("nothing pending; skipped a publish"),
            Err(e) => tracing::error!("publishing failed, keeping the current database: {e}"),
        }
        self.last_publish = started;
    }

    fn record_publish(
        &mut self,
        server: &EthPirServer<UsdtUsdc>,
        timings: eth_pir::RefreshTimings,
        started: Instant,
    ) {
        tracing::info!(
            absorbed = self.absorbed_since_publish,
            addresses = server.len(),
            encode = ?timings.database_encode,
            precompute = ?timings.precompute,
            install = ?timings.install,
            elapsed = ?started.elapsed(),
            "published",
        );
        self.absorbed_since_publish = 0;
    }

    fn save_after_compaction(&mut self, server: &mut EthPirServer<UsdtUsdc>) {
        if self.compact_if_needed(server) {
            save_checkpoint(&self.cfg.keyword, server);
        }
    }

    /// Returns whether the MPHF was rebuilt, which is when the checkpoint's
    /// `.keys` half becomes stale and has to be rewritten.
    fn compact_if_needed(&mut self, server: &mut EthPirServer<UsdtUsdc>) -> bool {
        let delta = server.len().saturating_sub(self.len_at_compaction);
        if delta < self.cfg.compact_after {
            return false;
        }
        tracing::info!(delta, "rebuilding the keyword index; clients must resync");
        self.resync_from_current_map(server)
    }

    fn resync_from_current_map(&mut self, server: &mut EthPirServer<UsdtUsdc>) -> bool {
        let started = Instant::now();
        match server.rebuild_keyword_index_from(&self.current) {
            Ok(t) => self.record_resync(server, t, started),
            Err(e) => {
                tracing::error!("keyword rebuild failed, keeping the current index: {e}");
                false
            }
        }
    }

    fn record_resync(
        &mut self,
        server: &EthPirServer<UsdtUsdc>,
        timings: eth_pir::KeywordRebuildTimings,
        started: Instant,
    ) -> bool {
        self.len_at_compaction = server.len();
        self.absorbed_since_publish = 0;
        self.last_publish = Instant::now();
        tracing::info!(
            addresses = self.current.len(),
            collect = ?timings.collect_keys,
            mphf = ?timings.mphf_rebuild,
            permute = ?timings.permute,
            refresh = ?timings.refresh.total(),
            elapsed = ?started.elapsed(),
            "rebuilt PIR database from the current holder map",
        );
        true
    }
}
