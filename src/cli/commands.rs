use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use super::args::{Cli, Cmd, Target};
use crate::follow::FollowConfig;
use crate::map::BalanceMap;

pub async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        cmd @ Cmd::Bootstrap { .. } => crate::cli::bootstrap_cmd::run(cmd).await,
        Cmd::InstallSnapshot { source, state } => {
            let source = crate::bootstrap::normalize_path(&source)?;
            let state = crate::bootstrap::normalize_path(&state)?;
            let installed = crate::map::install_snapshot(&source, &state)?;
            println!(
                "installed verified snapshot at {:?}: cursor {}, {} holders",
                state,
                installed.cursor,
                installed.len()
            );
            Ok(())
        }
        cmd @ Cmd::Follow { .. } => crate::cli::follow_cmd::run(cmd).await,
        cmd @ Cmd::Sync { .. } => crate::cli::sync_cmd::run(cmd).await,
        cmd @ Cmd::Serve { .. } => crate::cli::serve_cmd::run(cmd).await,
        Cmd::Lookup { address, state } => crate::cli::inspect::lookup(address, state),
        Cmd::Stat { state } => crate::cli::inspect::stat(state),
        Cmd::Sample { count, min, state } => crate::cli::inspect::sample(count, min, state),
    }
}

pub(super) async fn open_map<P: alloy::providers::Provider>(
    provider: &P,
    state: &Path,
    from_block: Option<Target>,
) -> Result<BalanceMap> {
    match BalanceMap::load(state) {
        Ok(m) => resume_existing_map(m, state, from_block),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            start_empty_map(provider, state, from_block).await
        }
        Err(e) => Err(e).context(format!("reading {state:?}")),
    }
}

fn resume_existing_map(
    map: BalanceMap,
    state: &Path,
    from_block: Option<Target>,
) -> Result<BalanceMap> {
    anyhow::ensure!(
        from_block.is_none(),
        "--from-block only applies when starting fresh, and {state:?} already \
         holds a snapshot at cursor {}. To replay, run: usdt-pir sync --from <BLOCK> \
         --state {state:?}",
        map.cursor,
    );
    tracing::info!(
        addresses = map.len(),
        cursor = map.cursor,
        "resumed from {state:?}"
    );
    Ok(map)
}

async fn start_empty_map<P: alloy::providers::Provider>(
    provider: &P,
    state: &Path,
    from_block: Option<Target>,
) -> Result<BalanceMap> {
    let start = from_block.context(
        "no snapshot at that path. A fresh start begins from an EMPTY map, which is \
         not an authoritative holder set. Either bootstrap a snapshot first, or pass \
         --from-block <BLOCK> (or --from-block finalized).",
    )?;
    let start = start.resolve(provider).await?;
    tracing::warn!(
        "no snapshot at {state:?}; starting from an EMPTY map at block {start}. \
         This tracks only addresses that move from here on."
    );
    Ok(BalanceMap::new(start.saturating_sub(1)))
}

pub(super) fn follow_config(
    chunk: u64,
    snapshot_path: PathBuf,
    snapshot_every: u64,
    tip: crate::chain::Tip,
    reorg_window: u64,
    poll_interval: u64,
) -> FollowConfig {
    FollowConfig {
        chunk,
        snapshot_path,
        snapshot_every: Duration::from_secs(snapshot_every),
        tip,
        reorg_window,
        poll_interval: Duration::from_secs(poll_interval),
        ..Default::default()
    }
}

pub(super) fn log_numeric_confirmations(tip: crate::chain::Tip, reorg_window: u64) {
    if let crate::chain::Tip::Confirmations(n) = tip {
        tracing::info!(
            "tracking head-{n}: a reorg deeper than {reorg_window} blocks would \
             leave balances that nothing revisits"
        );
    }
}
