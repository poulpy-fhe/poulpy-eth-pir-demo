use std::path::Path;
use std::time::{Duration, Instant};

use alloy::providers::ProviderBuilder;
use anyhow::Result;

use crate::cli::args::Cmd;
use crate::follow::FollowConfig;
use crate::map::BalanceMap;

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

struct ServeArgs {
    rpc: String,
    state: std::path::PathBuf,
    from_block: Option<crate::cli::args::Target>,
    follow: FollowArgs,
    serving: Serving,
}

struct FollowArgs {
    chunk: u64,
    snapshot_every: u64,
    confirmations: crate::chain::Tip,
    reorg_window: u64,
    poll_interval: u64,
}

impl ServeArgs {
    fn take(cmd: Cmd) -> Self {
        let Cmd::Serve {
            rpc,
            state,
            from_block,
            chunk,
            snapshot_every,
            confirmations,
            reorg_window,
            poll_interval,
            rebuild_every,
            compact_after,
            keyword,
            listen,
            web,
            batch_window,
            max_batch,
            queue_depth,
            rate_limit,
            rate_burst,
        } = cmd
        else {
            unreachable!("serve_cmd only handles serve")
        };
        Self {
            rpc,
            state,
            from_block,
            follow: FollowArgs {
                chunk,
                snapshot_every,
                confirmations,
                reorg_window,
                poll_interval,
            },
            serving: Serving::new(
                rebuild_every,
                compact_after,
                keyword,
                listen,
                web,
                batch_window,
                max_batch,
                queue_depth,
                rate_limit,
                rate_burst,
            ),
        }
    }
}

fn progress_config(
    follow: &FollowArgs,
    state: std::path::PathBuf,
) -> (FollowConfig, crate::progress::Handle) {
    let mut cfg = super::commands::follow_config(
        follow.chunk,
        state,
        follow.snapshot_every,
        follow.confirmations,
        follow.reorg_window,
        follow.poll_interval,
    );
    let progress = crate::progress::handle();
    cfg.progress = Some(progress.clone());
    (cfg, progress)
}

/// Everything `serve` needs once the map is caught up.
struct Serving {
    rebuild_every: u64,
    compact_after: usize,
    keyword: std::path::PathBuf,
    listen: Option<std::net::SocketAddr>,
    web: Option<std::path::PathBuf>,
    batch: crate::http::BatchConfig,
    rate: crate::http::RateLimit,
}

impl Serving {
    #[allow(clippy::too_many_arguments)]
    fn new(
        rebuild_every: u64,
        compact_after: usize,
        keyword: std::path::PathBuf,
        listen: Option<std::net::SocketAddr>,
        web: Option<std::path::PathBuf>,
        batch_window: u64,
        max_batch: usize,
        queue_depth: usize,
        rate_limit: u32,
        rate_burst: u32,
    ) -> Self {
        Self {
            rebuild_every,
            compact_after,
            keyword,
            listen,
            web,
            batch: crate::http::BatchConfig {
                window: Duration::from_millis(batch_window),
                max: max_batch,
                queue_depth,
            },
            rate: crate::http::RateLimit {
                per_minute: rate_limit,
                burst: rate_burst,
            },
        }
    }
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
        pir_config(rebuild_every, serving.compact_after, &serving.keyword),
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

fn pir_config(
    rebuild_every: u64,
    compact_after: usize,
    keyword: &Path,
) -> crate::publish::PirConfig {
    crate::publish::PirConfig {
        rebuild_every: Duration::from_secs(rebuild_every),
        compact_after,
        keyword: crate::keyword_store::Paths::new(keyword),
    }
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
    let mut backoff = cfg.retry_base;
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
                    tracing::error!(failures, cursor = balances.cursor, retry_in = ?backoff, "catch-up still failing: {e:#}");
                } else {
                    tracing::warn!(failures, cursor = balances.cursor, retry_in = ?backoff, "catch-up failed: {e:#}");
                }
                tokio::time::sleep(backoff).await;
                backoff = crate::follow::next_backoff(backoff, cfg.retry_max);
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
