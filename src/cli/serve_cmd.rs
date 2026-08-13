use std::path::Path;
use std::time::Instant;

use alloy::providers::ProviderBuilder;
use anyhow::Result;

use crate::cli::args::Cmd;
use crate::follow::FollowConfig;
use crate::map::BalanceMap;

use super::serve_config::{ServeArgs, Serving, pir_config, progress_config};

pub async fn run(cmd: Cmd) -> Result<()> {
    let args = ServeArgs::take(cmd);
    let provider = ProviderBuilder::new().connect(&args.rpc).await?;
    let _lock = crate::map::SnapshotLock::acquire(&args.state)?;
    let mut balances = super::commands::open_map(&provider, &args.state, args.from_block).await?;
    crate::follow::preflight(&provider, balances.cursor + 1).await?;
    let (cfg, progress) = progress_config(&args.follow, args.state.clone());

    catch_up_before_publish(&provider, &mut balances, &args.state, &cfg).await?;
    run_pir_loop(&provider, balances, cfg, progress, args.serving).await
}

async fn run_pir_loop<P: alloy::providers::Provider>(
    provider: &P,
    mut balances: BalanceMap,
    cfg: FollowConfig,
    progress: crate::progress::Handle,
    serving: Serving,
) -> Result<()> {
    let rebuild_every = serving.rebuild_every;
    let (tx, rx) = std::sync::mpsc::channel();
    let pir = crate::publish::spawn(
        &balances,
        pir_config(
            rebuild_every,
            serving.compact_after,
            serving.compact_tail_percent,
            &serving.keyword,
        ),
        rx,
    )?;

    let endpoint = match serving.listen {
        Some(addr) => Some(
            crate::http::spawn(crate::http::Endpoint {
                listen: addr,
                responder: pir.responder.clone(),
                directory: pir.directory.clone(),
                progress: progress.clone(),
                web: serving.web,
                batch: serving.batch,
                rate: serving.rate,
            })
            .await?,
        ),
        None => None,
    };
    if endpoint.is_none() {
        tracing::warn!("no --listen given: the database is kept current but nothing can query it");
    }

    tracing::info!("serving: syncing every pass and publishing every {rebuild_every}s");
    let outcome = crate::follow::run(provider, &mut balances, &cfg, Some(&tx)).await;
    drop(tx);
    if let Some(endpoint) = endpoint {
        endpoint.abort();
    }
    let _ = pir.handle.join();
    outcome
}

/// Retries instead of propagating: a transient RPC failure here would otherwise
/// kill the process before the endpoint ever opens. Progress is snapshotted on
/// every attempt, so a retry resumes rather than restarts.
async fn catch_up_before_publish<P: alloy::providers::Provider>(
    provider: &P,
    balances: &mut BalanceMap,
    state: &Path,
    cfg: &FollowConfig,
) -> Result<()> {
    let mut failures = 0u32;
    loop {
        match catch_up_once(provider, balances, state, cfg).await {
            Ok(()) => {
                if failures > 0 {
                    tracing::info!(failures, cursor = balances.cursor, "catch-up recovered");
                }
                return Ok(());
            }
            Err(e) => {
                failures += 1;
                if failures >= 5 {
                    tracing::error!(failures, cursor = balances.cursor, retry_in = ?cfg.retry_base, "catch-up still failing: {e:#}");
                } else {
                    tracing::warn!(failures, cursor = balances.cursor, retry_in = ?cfg.retry_base, "catch-up failed: {e:#}");
                }
                tokio::time::sleep(cfg.retry_base).await;
            }
        }
    }
}

async fn catch_up_once<P: alloy::providers::Provider>(
    provider: &P,
    balances: &mut BalanceMap,
    state: &Path,
    cfg: &FollowConfig,
) -> Result<()> {
    let target = cfg.tip.resolve(provider).await?;
    if target <= balances.cursor {
        return Ok(());
    }
    tracing::info!(behind = target - balances.cursor, "catching up to {target}");
    if let Some(progress) = &cfg.progress {
        progress.record(balances.cursor, target);
    }
    sync_to_target(provider, balances, state, cfg, target).await
}

async fn sync_to_target<P: alloy::providers::Provider>(
    provider: &P,
    balances: &mut BalanceMap,
    state: &Path,
    cfg: &FollowConfig,
    target: u64,
) -> Result<()> {
    let mut last_snapshot = Instant::now();
    let mut st = crate::follow::PassState::default();
    let synced = crate::follow::sync(
        provider,
        balances,
        balances.cursor + 1,
        target,
        cfg,
        &mut last_snapshot,
        &mut st,
    )
    .await;
    balances.save(state)?;
    synced.map(|_| ())
}
