use std::collections::HashMap;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::Result;

use crate::follow::config::FollowConfig;
use crate::follow::watch::ChainWatch;
use crate::map::{BalanceMap, Entry};

#[derive(Debug, Default)]
pub struct PassState {
    pub watch: ChainWatch,
    pub changed: HashMap<Address, Entry>,
}

pub async fn run<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    cfg: &FollowConfig,
    updates: Option<&std::sync::mpsc::Sender<crate::publish::UpdateBatch>>,
) -> Result<()> {
    let mut last_snapshot = Instant::now();
    let mut backoff = cfg.retry_base;
    let mut failures = 0u32;
    let mut state = PassState::default();
    loop {
        match pass(provider, map, cfg, &mut last_snapshot, &mut state, updates).await {
            Ok(()) => on_success(map, cfg, &mut failures, &mut backoff).await,
            Err(e) => {
                on_failure(map, cfg, &mut last_snapshot, &mut failures, &mut backoff, e).await
            }
        }
    }
}

async fn on_success(
    map: &BalanceMap,
    cfg: &FollowConfig,
    failures: &mut u32,
    backoff: &mut Duration,
) {
    if *failures > 0 {
        tracing::info!(failures = *failures, cursor = map.cursor, "recovered");
    }
    *failures = 0;
    *backoff = cfg.retry_base;
    tokio::time::sleep(cfg.poll_interval).await;
}

async fn on_failure(
    map: &BalanceMap,
    cfg: &FollowConfig,
    last_snapshot: &mut Instant,
    failures: &mut u32,
    backoff: &mut Duration,
    error: anyhow::Error,
) {
    *failures += 1;
    snapshot_failed_progress(map, cfg, last_snapshot);
    log_failure(map, *failures, *backoff, &error);
    tokio::time::sleep(*backoff).await;
    *backoff = next_backoff(*backoff, cfg.retry_max);
}

fn snapshot_failed_progress(map: &BalanceMap, cfg: &FollowConfig, last_snapshot: &mut Instant) {
    if let Err(save) = map.save(&cfg.snapshot_path) {
        tracing::error!("could not snapshot after a failed pass: {save}");
    } else {
        *last_snapshot = Instant::now();
    }
}

fn log_failure(map: &BalanceMap, failures: u32, backoff: Duration, error: &anyhow::Error) {
    if failures >= 5 {
        tracing::error!(failures, cursor = map.cursor, retry_in = ?backoff, "sync pass still failing: {error:#}");
    } else {
        tracing::warn!(failures, cursor = map.cursor, retry_in = ?backoff, "sync pass failed: {error:#}");
    }
}

pub fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(max).min(max)
}

async fn pass<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    cfg: &FollowConfig,
    last_snapshot: &mut Instant,
    state: &mut PassState,
    updates: Option<&std::sync::mpsc::Sender<crate::publish::UpdateBatch>>,
) -> Result<()> {
    repair_if_needed(provider, map, cfg, state).await?;
    let target = cfg.tip.resolve(provider).await?;
    if target <= map.cursor {
        return Ok(());
    }
    log_backfill(map.cursor, target, cfg.chunk);
    if let Some(progress) = &cfg.progress {
        progress.record(map.cursor, target);
    }
    crate::follow::sync(
        provider,
        map,
        map.cursor + 1,
        target,
        cfg,
        last_snapshot,
        state,
    )
    .await?;
    finish_pass(provider, map, cfg, last_snapshot, state, updates).await
}

async fn repair_if_needed<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    cfg: &FollowConfig,
    state: &mut PassState,
) -> Result<()> {
    if cfg.tip.is_reorgable() {
        state
            .watch
            .repair_if_reorged(provider, map, cfg.reorg_window)
            .await?;
    }
    Ok(())
}

fn log_backfill(cursor: u64, target: u64, chunk: u64) {
    let behind = target - cursor;
    if behind > chunk {
        tracing::info!(behind, "backfilling to {target}");
    }
}

async fn finish_pass<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    cfg: &FollowConfig,
    last_snapshot: &mut Instant,
    state: &mut PassState,
    updates: Option<&std::sync::mpsc::Sender<crate::publish::UpdateBatch>>,
) -> Result<()> {
    update_reorg_watch(provider, map, cfg, state).await?;
    map.save(&cfg.snapshot_path)?;
    if let Some(progress) = &cfg.progress {
        progress.record(map.cursor, map.cursor.max(progress.sample().tip));
    }
    *last_snapshot = Instant::now();
    publish_changes(state, updates)
}

async fn update_reorg_watch<P: Provider>(
    provider: &P,
    map: &BalanceMap,
    cfg: &FollowConfig,
    state: &mut PassState,
) -> Result<()> {
    if cfg.tip.is_reorgable() {
        state.watch.store_cursor_hash(provider, map.cursor).await?;
        state.watch.trim(map.cursor, cfg.reorg_window);
    }
    Ok(())
}

fn publish_changes(
    state: &PassState,
    updates: Option<&std::sync::mpsc::Sender<crate::publish::UpdateBatch>>,
) -> Result<()> {
    let Some(tx) = updates else {
        return Ok(());
    };
    if state.changed.is_empty() {
        return Ok(());
    }
    let batch = changed_batch(&state.changed);
    let n = batch.len();
    tx.send(batch)
        .map_err(|_| anyhow::anyhow!("PIR thread is gone"))?;
    tracing::debug!(entries = n, "handed a batch to the PIR thread");
    Ok(())
}

fn changed_batch(changed: &HashMap<Address, Entry>) -> crate::publish::UpdateBatch {
    changed
        .iter()
        .map(|(a, e)| (crate::record::keyword(a), *e))
        .collect()
}
