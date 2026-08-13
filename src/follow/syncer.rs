use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::time::Instant;

use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::Result;

use crate::follow::config::{FollowConfig, SyncStats};
use crate::follow::loop_run::PassState;
use crate::map::BalanceMap;

pub async fn preflight<P: Provider>(provider: &P, from: u64) -> Result<()> {
    crate::chain::ensure_mainnet(provider).await?;
    crate::chain::ensure_multicall_at(from)?;
    ensure_state_available(provider, from).await?;
    ensure_logs_available(provider, from).await
}

async fn ensure_state_available<P: Provider>(provider: &P, from: u64) -> Result<()> {
    crate::chain::read_balances(provider, &[crate::tokens::USDT], from)
        .await
        .map(|_| ())
        .map_err(|e| {
            anyhow::anyhow!(
                "provider will not serve historical balanceOf state at block {from}: {e:#}"
            )
        })
}

async fn ensure_logs_available<P: Provider>(provider: &P, from: u64) -> Result<()> {
    if provider
        .get_logs(&crate::chain::filter(from, from))
        .await
        .is_ok()
    {
        return Ok(());
    }
    let head = provider.get_block_number().await?;
    if provider
        .get_logs(&crate::chain::filter(head, head))
        .await
        .is_err()
    {
        anyhow::bail!("provider cannot serve logs at all; check the endpoint");
    }
    anyhow::bail!(
        "provider will not serve logs at block {from} ({} blocks behind head {head}), \
         but does at the head.",
        head.saturating_sub(from),
    )
}

pub async fn sync<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    from: u64,
    to: u64,
    cfg: &FollowConfig,
    last_snapshot: &mut Instant,
    state: &mut PassState,
) -> Result<SyncStats> {
    let mut totals = start_totals(from, to);
    sync_into(
        provider,
        map,
        from..=to,
        cfg,
        last_snapshot,
        state,
        &mut totals,
    )
    .await?;
    Ok(totals)
}

/// [`sync`] accumulating into a caller-owned total, so a caller that retries can
/// report the whole range instead of only the attempt that happened to finish.
///
/// Leaves `totals.blocks` alone: across retries the ranges overlap, so only the
/// caller knows how far the cursor actually moved.
pub async fn sync_into<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    range: RangeInclusive<u64>,
    cfg: &FollowConfig,
    last_snapshot: &mut Instant,
    state: &mut PassState,
    totals: &mut SyncStats,
) -> Result<()> {
    state.changed.clear();
    let (from, to) = (*range.start(), *range.end());
    if from > to {
        return Ok(());
    }
    let mut run = SyncRun::new(provider, map, cfg, last_snapshot, state, totals);
    run.sync_chunks(from, to).await?;
    prune_transient(run.map, &run.inserted, run.totals);
    Ok(())
}

fn start_totals(from: u64, to: u64) -> SyncStats {
    SyncStats {
        blocks: from.checked_sub(to).map_or(to - from + 1, |_| 0),
        ..Default::default()
    }
}

struct SyncRun<'a, P: Provider> {
    provider: &'a P,
    map: &'a mut BalanceMap,
    cfg: &'a FollowConfig,
    last_snapshot: &'a mut Instant,
    state: &'a mut PassState,
    totals: &'a mut SyncStats,
    inserted: HashSet<Address>,
}

impl<'a, P: Provider> SyncRun<'a, P> {
    fn new(
        provider: &'a P,
        map: &'a mut BalanceMap,
        cfg: &'a FollowConfig,
        last_snapshot: &'a mut Instant,
        state: &'a mut PassState,
        totals: &'a mut SyncStats,
    ) -> Self {
        Self {
            provider,
            map,
            cfg,
            last_snapshot,
            state,
            totals,
            inserted: HashSet::new(),
        }
    }

    async fn sync_chunks(&mut self, from: u64, to: u64) -> Result<()> {
        let mut chunk = self.cfg.chunk;
        let mut lo = from;
        while lo <= to {
            lo = self.sync_one_chunk(lo, to, &mut chunk).await?;
        }
        Ok(())
    }

    async fn sync_one_chunk(&mut self, lo: u64, to: u64, chunk: &mut u64) -> Result<u64> {
        let want = (lo + *chunk - 1).min(to);
        let (logs, hi) = crate::follow::logs::fetch_logs(self.provider, lo, want, chunk).await?;
        let stats = self.apply_chunk(&logs, hi).await?;
        self.map.cursor = hi;
        self.totals.fold(stats);
        log_chunk(lo, hi, self.map, stats);
        maybe_snapshot(self.map, self.cfg, self.last_snapshot)?;
        Ok(hi + 1)
    }

    async fn apply_chunk(
        &mut self,
        logs: &[alloy::rpc::types::eth::Log],
        hi: u64,
    ) -> Result<crate::follow::config::BatchStats> {
        let mut touched = Vec::new();
        let stats = crate::follow::apply_range(
            self.provider,
            self.map,
            logs,
            hi,
            &mut self.inserted,
            &mut touched,
            &mut self.state.changed,
        )
        .await?;
        self.state.watch.record(hi, touched);
        Ok(stats)
    }
}

fn log_chunk(lo: u64, hi: u64, map: &BalanceMap, stats: crate::follow::config::BatchStats) {
    if stats.touched == 0 {
        return;
    }
    tracing::info!(
        from = lo,
        to = hi,
        logs = stats.logs,
        touched = stats.touched,
        inserted = stats.inserted,
        updated = stats.updated,
        removed = stats.removed,
        skipped = stats.skipped_new_zero,
        size = map.len(),
        "applied",
    );
}

fn maybe_snapshot(map: &BalanceMap, cfg: &FollowConfig, last_snapshot: &mut Instant) -> Result<()> {
    if last_snapshot.elapsed() < cfg.snapshot_every {
        return Ok(());
    }
    map.save(&cfg.snapshot_path)?;
    *last_snapshot = Instant::now();
    tracing::debug!(cursor = map.cursor, "snapshot written");
    Ok(())
}

fn prune_transient(map: &mut BalanceMap, inserted: &HashSet<Address>, totals: &mut SyncStats) {
    totals.pruned = map.prune_zero(inserted);
    totals.inserted -= totals.pruned;
    if totals.pruned > 0 {
        tracing::info!(
            pruned = totals.pruned,
            "dropped transient zero-balance addresses"
        );
    }
}
