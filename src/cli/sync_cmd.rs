use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::providers::ProviderBuilder;
use anyhow::{Context, Result};

use crate::cli::args::Cmd;
use crate::follow::FollowConfig;
use crate::map::BalanceMap;

pub async fn run(cmd: Cmd) -> Result<()> {
    let Cmd::Sync {
        rpc,
        state,
        from,
        to,
        chunk,
        snapshot_every,
        retries,
    } = cmd
    else {
        unreachable!("sync_cmd only handles sync")
    };
    let provider = ProviderBuilder::new().connect(&rpc).await?;
    let _lock = crate::map::SnapshotLock::acquire(&state)?;
    let to = to.resolve(&provider).await?;
    let mut balances = load_or_create_map(&state, from)?;
    let from = prepare_replay(&mut balances, from);
    anyhow::ensure!(from <= to, "nothing to do: --from {from} is past --to {to}");
    crate::follow::preflight(&provider, from).await?;
    let cfg = bounded_config(chunk, state.clone(), snapshot_every);
    let stats = run_sync(&provider, &mut balances, from, to, &cfg, &state, retries).await?;
    print_summary(from, to, &balances, stats);
    Ok(())
}

fn load_or_create_map(state: &Path, from: Option<u64>) -> Result<BalanceMap> {
    match BalanceMap::load(state) {
        Ok(m) => Ok(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_map(from, state),
        Err(e) => Err(e).context(format!("reading {state:?}")),
    }
}

fn create_map(from: Option<u64>, state: &Path) -> Result<BalanceMap> {
    let start = from.context("no snapshot at that path, so --from is required")?;
    tracing::info!("no snapshot at {state:?}; starting empty at block {start}");
    Ok(BalanceMap::new(start.saturating_sub(1)))
}

fn prepare_replay(balances: &mut BalanceMap, from: Option<u64>) -> u64 {
    let from = from.unwrap_or(balances.cursor + 1);
    if from <= balances.cursor {
        tracing::info!(cursor = balances.cursor, "replaying from {from}");
        // `from` is a block to *include*, so the cursor sits one before it.
        // Saturating: `--from 0` is refused later by preflight, but a debug
        // build would panic here first.
        balances.cursor = from.saturating_sub(1);
    }
    from
}

fn bounded_config(chunk: u64, snapshot_path: PathBuf, snapshot_every: u64) -> FollowConfig {
    FollowConfig {
        chunk,
        snapshot_path,
        snapshot_every: Duration::from_secs(snapshot_every),
        ..Default::default()
    }
}

/// Sync `from..=to`, retrying from wherever it got to.
///
/// `retries` bounds *consecutive failures that advanced nothing*, not total
/// failures. A rate-limited endpoint fails constantly while still moving the
/// cursor a few hundred blocks each time, and counting those as failures makes
/// the command quit part-way through a range it was steadily completing.
/// Terminating either way: every attempt that advances shortens what is left.
async fn run_sync<P: alloy::providers::Provider>(
    provider: &P,
    balances: &mut BalanceMap,
    from: u64,
    to: u64,
    cfg: &FollowConfig,
    state: &Path,
    retries: u32,
) -> Result<crate::follow::SyncStats> {
    let mut total = crate::follow::SyncStats::default();
    let mut stalled = 0u32;
    let mut start = from;
    let origin = balances.cursor;

    loop {
        let before = balances.cursor;
        match sync_once(provider, balances, start, to, cfg, &mut total).await {
            Ok(()) => {
                total.blocks = balances.cursor.saturating_sub(origin);
                balances.save(state)?;
                return Ok(total);
            }
            Err(e) => {
                wait_to_retry(balances, state, cfg, to, before, retries, e, &mut stalled).await?;
                start = balances.cursor + 1;
                if start > to {
                    total.blocks = balances.cursor.saturating_sub(origin);
                    return Ok(total);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_to_retry(
    balances: &BalanceMap,
    state: &Path,
    cfg: &FollowConfig,
    to: u64,
    before: u64,
    retries: u32,
    error: anyhow::Error,
    stalled: &mut u32,
) -> Result<()> {
    save_failed_progress(balances, state);
    let advanced = balances.cursor.saturating_sub(before);
    record_retry_progress(advanced, stalled);
    if *stalled > retries {
        return Err(error.context(format!(
            "giving up after {retries} attempt(s) with no progress at block {}; re-run to resume",
            balances.cursor
        )));
    }
    tracing::warn!(
        advanced,
        stalled = *stalled,
        cursor = balances.cursor,
        remaining = to.saturating_sub(balances.cursor),
        retry_in = ?cfg.retry_base,
        "sync interrupted: {error:#}"
    );
    tokio::time::sleep(cfg.retry_base).await;
    Ok(())
}

fn record_retry_progress(advanced: u64, stalled: &mut u32) {
    if advanced > 0 {
        *stalled = 0;
    } else {
        *stalled += 1;
    }
}

async fn sync_once<P: alloy::providers::Provider>(
    provider: &P,
    balances: &mut BalanceMap,
    from: u64,
    to: u64,
    cfg: &FollowConfig,
    totals: &mut crate::follow::SyncStats,
) -> Result<()> {
    let mut last_snapshot = Instant::now();
    let mut watch = crate::follow::PassState::default();
    crate::follow::sync_into(
        provider,
        balances,
        from..=to,
        cfg,
        &mut last_snapshot,
        &mut watch,
        totals,
    )
    .await
}

fn save_failed_progress(balances: &BalanceMap, state: &Path) {
    match balances.save(state) {
        Ok(()) => tracing::info!(cursor = balances.cursor, "sync failed; progress saved"),
        Err(save) => tracing::error!("could not save progress: {save}"),
    }
}

fn print_summary(from: u64, to: u64, balances: &BalanceMap, stats: crate::follow::SyncStats) {
    println!(
        "synced        blocks {from}..={to} ({} blocks)",
        stats.blocks
    );
    println!("batches       {}", stats.batches);
    println!("logs          {}", stats.logs);
    println!("touched       {}", stats.touched);
    println!("inserted      {}", stats.inserted);
    println!("updated       {}", stats.updated);
    println!("removed       {}", stats.removed);
    println!("unchanged     {}", stats.unchanged);
    println!("skipped zero  {}", stats.skipped_new_zero);
    println!("pruned        {}", stats.pruned);
    println!("map size      {}", balances.len());
}
