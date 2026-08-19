mod cache;
mod paths;

pub(crate) use paths::normalize as normalize_path;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use anyhow::{Context, Result};

use cache::{Cache, CacheKind, Metadata, Phase, Projection};
use paths::BootstrapPaths;

const RPC_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_BALANCE_ADDRESSES: usize = 400;

#[derive(Clone, Debug)]
pub struct Config {
    pub state: PathBuf,
    pub cache: Option<PathBuf>,
    pub confirmations: u64,
    pub chunk: u64,
    pub retries: u32,
    pub keep_cache: bool,
}

pub async fn run<P: Provider>(provider: &P, config: Config) -> Result<()> {
    anyhow::ensure!(config.confirmations > 0, "--confirmations must be positive");
    anyhow::ensure!(config.chunk > 0, "--chunk must be positive");
    let paths = BootstrapPaths::resolve(&config.state, config.cache.as_deref())?;
    let _state_lock = crate::map::SnapshotLock::acquire(&paths.state)?;
    let _cache_lock = crate::map::AdvisoryLock::acquire(
        &paths.cache_lock,
        &format!("using bootstrap cache {:?}", paths.cache),
    )?;

    let kind = Cache::inspect(&paths.cache)?;
    let mut cache = match kind {
        CacheKind::Missing => {
            refuse_orphaned_cache_sidecars(&paths)?;
            initialize_new(provider, &paths, &config).await?
        }
        CacheKind::Uninitialized => {
            initialize_after_uncommitted_file(provider, &paths, &config).await?
        }
        CacheKind::Initialized(metadata) => resume(provider, &paths, &config, metadata).await?,
    };

    loop {
        let metadata = cache.metadata()?;
        log_phase(&metadata, &cache)?;
        match metadata.phase {
            Phase::Scanning => scan(provider, &mut cache, &config).await?,
            Phase::ReadingBalances => {
                if read_balances(provider, &mut cache, &paths, &config).await? {
                    continue;
                }
            }
            Phase::ReadyToCommit => {
                if commit_ready(provider, &mut cache, &paths, &config).await? {
                    continue;
                }
            }
            Phase::Complete => {
                recover_complete(&cache, &paths)?;
                finish(cache, &paths, config.keep_cache);
                return Ok(());
            }
        }
    }
}

async fn initialize_new<P: Provider>(
    provider: &P,
    paths: &BootstrapPaths,
    config: &Config,
) -> Result<Cache> {
    refuse_existing_state(&paths.state)?;
    ensure_mainnet(provider, config.retries).await?;
    let (target, hash) = select_target(provider, config.confirmations, config.retries).await?;
    rpc_preflight(provider, target, config.retries).await?;
    refuse_existing_state(&paths.state)?;
    let cache = Cache::initialize(
        &paths.cache,
        &paths.state,
        config.confirmations,
        target,
        hash,
    )?;
    tracing::info!(target, %hash, cache = ?paths.cache, "initialized bootstrap cache");
    Ok(cache)
}

async fn initialize_after_uncommitted_file<P: Provider>(
    provider: &P,
    paths: &BootstrapPaths,
    config: &Config,
) -> Result<Cache> {
    refuse_existing_state(&paths.state)?;
    remove_uninitialized_cache(paths)?;
    initialize_new(provider, paths, config).await
}

async fn resume<P: Provider>(
    provider: &P,
    paths: &BootstrapPaths,
    config: &Config,
    inspected: Metadata,
) -> Result<Cache> {
    validate_resume_binding(&inspected, paths, config)?;
    validate_target(inspected.target_block)?;
    match inspected.phase {
        Phase::Scanning | Phase::ReadingBalances => {
            anyhow::ensure!(
                !paths.state.exists(),
                "state {:?} exists while cache is {}; it is unrelated and will not be overwritten",
                paths.state,
                inspected.phase.as_str()
            );
        }
        Phase::ReadyToCommit | Phase::Complete => {}
    }

    if inspected.phase != Phase::Complete {
        ensure_mainnet(provider, config.retries).await?;
    }
    let mut cache = Cache::open(&paths.cache)?;
    let metadata = cache.metadata()?;
    anyhow::ensure!(
        metadata == inspected,
        "bootstrap cache metadata changed between read-only authentication and read-write open"
    );
    if metadata.phase == Phase::Complete {
        return Ok(cache);
    }
    let current_hash = required_hash(provider, metadata.target_block, config.retries).await?;
    if current_hash == metadata.target_hash {
        return Ok(cache);
    }
    tracing::warn!(
        target = metadata.target_block,
        cached = %metadata.target_hash,
        current = %current_hash,
        "bootstrap target hash changed before commit"
    );
    reset_after_hash_mismatch(provider, &mut cache, paths, config, &metadata).await?;
    Ok(cache)
}

fn validate_resume_binding(
    metadata: &Metadata,
    paths: &BootstrapPaths,
    config: &Config,
) -> Result<()> {
    anyhow::ensure!(
        metadata.state_path == paths.state,
        "bootstrap cache is bound to state {:?}, not {:?}",
        metadata.state_path,
        paths.state
    );
    anyhow::ensure!(
        metadata.confirmations == config.confirmations,
        "bootstrap cache pinned confirmation depth {}, but this invocation requested {}; --chunk and --retries may change, --confirmations may not",
        metadata.confirmations,
        config.confirmations
    );
    Ok(())
}

async fn scan<P: Provider>(provider: &P, cache: &mut Cache, config: &Config) -> Result<()> {
    let metadata = cache.metadata()?;
    let mut lo = metadata
        .scan_cursor
        .checked_add(1)
        .context("scan cursor cannot advance past u64::MAX")?;
    let mut adaptive = config.chunk;
    let mut stalled = RetryCounter::new(config.retries);
    while lo <= metadata.target_block {
        let mut hi = lo.saturating_add(adaptive - 1).min(metadata.target_block);
        let logs = loop {
            let fetched = tokio::time::timeout(
                RPC_TIMEOUT,
                provider.get_logs(&crate::chain::filter(lo, hi)),
            )
            .await;
            match fetched {
                Ok(Ok(logs)) => break logs,
                Ok(Err(error)) if crate::follow::is_result_cap_message(&error.to_string()) => {
                    anyhow::ensure!(
                        hi > lo,
                        "provider result cap is exceeded by single block {lo}; use a provider with a higher complete-log limit"
                    );
                    (hi, adaptive) = narrow_range(lo, hi);
                    tracing::warn!(
                        lo,
                        hi,
                        adaptive,
                        "provider result cap; narrowing bootstrap range"
                    );
                }
                Ok(Err(error)) => {
                    retry_wait(
                        &mut stalled,
                        anyhow::Error::new(error).context(format!("fetching logs {lo}..={hi}")),
                    )
                    .await?;
                }
                Err(error) => {
                    retry_wait(
                        &mut stalled,
                        anyhow::anyhow!(
                            "fetching logs {lo}..={hi} timed out after {RPC_TIMEOUT:?}: {error}"
                        ),
                    )
                    .await?;
                }
            }
        };

        let touched = match discover_range(provider, &logs, lo, hi).await {
            Ok(touched) => touched,
            Err(error) => {
                retry_wait(
                    &mut stalled,
                    error.context(format!("parsing bootstrap range {lo}..={hi}")),
                )
                .await?;
                continue;
            }
        };
        cache.commit_scan_range(lo, hi, &touched)?;
        stalled.progress();
        let (candidates, _) = cache.counts()?;
        let completed = hi - metadata.scan_start + 1;
        let total = metadata.target_block - metadata.scan_start + 1;
        tracing::info!(
            from = lo,
            to = hi,
            percent = completed as f64 * 100.0 / total as f64,
            adaptive_chunk = adaptive,
            candidates,
            "bootstrap scan range committed"
        );
        if adaptive < config.chunk {
            adaptive = grow_range(adaptive, config.chunk);
        }
        lo = hi
            .checked_add(1)
            .context("bootstrap scan reached u64::MAX")?;
    }
    cache.finish_scanning()?;
    Ok(())
}

fn narrow_range(lo: u64, hi: u64) -> (u64, u64) {
    debug_assert!(hi > lo);
    let narrowed_hi = lo + (hi - lo) / 2;
    (narrowed_hi, narrowed_hi - lo + 1)
}

fn grow_range(current: u64, configured: u64) -> u64 {
    current.saturating_mul(2).min(configured).max(1)
}

async fn discover_range<P: Provider>(
    provider: &P,
    logs: &[alloy::rpc::types::eth::Log],
    lo: u64,
    hi: u64,
) -> Result<HashMap<Address, crate::chain::Touch>> {
    let mut touched = HashMap::new();
    let collected = crate::chain::collect_touched_strict(logs, lo..=hi, &mut touched)?;
    for block in collected.usdt_supply_blocks {
        let owner = crate::chain::usdt_owner_strict(provider, block).await?;
        anyhow::ensure!(
            !owner.is_zero(),
            "USDT owner() returned zero at block {block}"
        );
        touched
            .entry(owner)
            .or_default()
            .see(crate::tokens::USDT, Some(block));
    }
    Ok(touched)
}

/// Returns true when a hash mismatch reset the cache and the outer phase loop
/// should restart from Scanning.
async fn read_balances<P: Provider>(
    provider: &P,
    cache: &mut Cache,
    paths: &BootstrapPaths,
    config: &Config,
) -> Result<bool> {
    let mut stalled = RetryCounter::new(config.retries);
    loop {
        let metadata = cache.metadata()?;
        let batch = cache.unread_batch(MAX_BALANCE_ADDRESSES)?;
        if batch.is_empty() {
            let projection = cache.projection()?;
            // A bootstrap can run for days. Reassert the chain binding at the
            // semantic validation boundary rather than relying only on the
            // check made when this process started.
            ensure_mainnet(provider, config.retries).await?;
            let supplies = read_supplies(provider, metadata.target_block, config.retries).await?;
            let validation = validate_projection(&projection, supplies);
            let hash = required_hash(provider, metadata.target_block, config.retries).await?;
            if hash != metadata.target_hash {
                reset_after_hash_mismatch(provider, cache, paths, config, &metadata).await?;
                return Ok(true);
            }
            validation?;
            cache.set_ready_to_commit()?;
            tracing::info!(
                holders = projection.map.len(),
                usdt = projection.usdt_sum,
                usdc = projection.usdc_sum,
                "bootstrap cache validated and ready to commit"
            );
            return Ok(false);
        }

        match crate::chain::read_balances_strict(provider, &batch, metadata.target_block).await {
            Ok(readings) => {
                anyhow::ensure!(
                    readings.len() == batch.len(),
                    "strict balance reader returned {} rows for {} candidates",
                    readings.len(),
                    batch.len()
                );
                cache.commit_balances(&readings)?;
                stalled.progress();
                let (_, unread) = cache.counts()?;
                tracing::info!(
                    committed = batch.len(),
                    unread,
                    "bootstrap balance batch committed"
                );
            }
            Err(error) => {
                retry_wait(
                    &mut stalled,
                    error.context("reading bootstrap balance batch"),
                )
                .await?;
            }
        }
    }
}

fn validate_projection(
    projection: &Projection,
    supplies: crate::chain::TotalSupplies,
) -> Result<()> {
    validate_projection_with_capacity(projection, supplies, crate::publish::capacity())
}

fn validate_projection_with_capacity(
    projection: &Projection,
    supplies: crate::chain::TotalSupplies,
    capacity: usize,
) -> Result<()> {
    anyhow::ensure!(
        projection.usdt_sum == supplies.usdt,
        "USDT holder sum {} does not equal totalSupply {}",
        projection.usdt_sum,
        supplies.usdt
    );
    anyhow::ensure!(
        projection.usdc_sum == supplies.usdc,
        "USDC holder sum {} does not equal totalSupply {}",
        projection.usdc_sum,
        supplies.usdc
    );
    anyhow::ensure!(
        projection.map.len() <= capacity,
        "{} holders exceed deployed PIR capacity {capacity}",
        projection.map.len()
    );
    Ok(())
}

/// Returns true when the target changed and the cache was reset.
async fn commit_ready<P: Provider>(
    provider: &P,
    cache: &mut Cache,
    paths: &BootstrapPaths,
    config: &Config,
) -> Result<bool> {
    commit_ready_with_parent_sync(provider, cache, paths, config, crate::map::fsync_parent).await
}

async fn commit_ready_with_parent_sync<P, S>(
    provider: &P,
    cache: &mut Cache,
    paths: &BootstrapPaths,
    config: &Config,
    sync_parent: S,
) -> Result<bool>
where
    P: Provider,
    S: FnOnce(&Path) -> std::io::Result<()>,
{
    let metadata = cache.metadata()?;
    let projection = cache.projection()?;
    ensure_mainnet(provider, config.retries).await?;
    if paths.state.exists() {
        require_exact_snapshot(&paths.state, &projection.map)?;
    } else {
        projection
            .map
            .save(&paths.state)
            .with_context(|| format!("saving bootstrap snapshot {:?}", paths.state))?;
    }
    require_exact_snapshot(&paths.state, &projection.map)?;
    let hash = required_hash(provider, metadata.target_block, config.retries).await?;
    if hash != metadata.target_hash {
        reset_after_hash_mismatch(provider, cache, paths, config, &metadata).await?;
        return Ok(true);
    }
    sync_parent(&paths.state).with_context(|| {
        format!(
            "making verified bootstrap snapshot directory entry durable at {:?}",
            paths.state
        )
    })?;
    cache.set_complete()?;
    tracing::info!(
        snapshot = ?paths.state,
        cursor = metadata.target_block,
        holders = projection.map.len(),
        candidates = projection.candidates,
        usdt = projection.usdt_sum,
        usdc = projection.usdc_sum,
        "bootstrap snapshot committed and verified"
    );
    Ok(false)
}

fn recover_complete(cache: &Cache, paths: &BootstrapPaths) -> Result<()> {
    let projection = cache.projection()?;
    if paths.state.exists() {
        require_exact_snapshot(&paths.state, &projection.map)?;
    } else {
        projection
            .map
            .save(&paths.state)
            .with_context(|| format!("reconstructing completed snapshot {:?}", paths.state))?;
        require_exact_snapshot(&paths.state, &projection.map)?;
    }
    crate::map::fsync_parent(&paths.state).with_context(|| {
        format!(
            "making recovered bootstrap snapshot directory entry durable at {:?}",
            paths.state
        )
    })?;
    tracing::info!(
        snapshot = ?paths.state,
        cursor = projection.map.cursor,
        holders = projection.map.len(),
        "bootstrap already complete; snapshot verified"
    );
    Ok(())
}

async fn reset_after_hash_mismatch<P: Provider>(
    provider: &P,
    cache: &mut Cache,
    paths: &BootstrapPaths,
    config: &Config,
    old: &Metadata,
) -> Result<()> {
    ensure_mainnet(provider, config.retries).await?;
    let (replacement, replacement_hash) =
        select_target(provider, config.confirmations, config.retries).await?;
    rpc_preflight(provider, replacement, config.retries).await?;

    remove_authenticated_state_for_reset(cache, paths, old)?;
    cache.reset_target(replacement, replacement_hash)?;
    tracing::warn!(
        old_target = old.target_block,
        target = replacement,
        %replacement_hash,
        "discarded pre-commit work after target hash mismatch"
    );
    Ok(())
}

fn remove_authenticated_state_for_reset(
    cache: &Cache,
    paths: &BootstrapPaths,
    old: &Metadata,
) -> Result<()> {
    if !paths.state.exists() {
        return Ok(());
    }
    anyhow::ensure!(
        old.phase == Phase::ReadyToCommit,
        "state {:?} exists while resetting phase {}; refusing unrelated artifact",
        paths.state,
        old.phase.as_str()
    );
    let old_projection = cache.projection()?;
    require_exact_snapshot(&paths.state, &old_projection.map)?;
    crate::map::remove_file_durable(&paths.state).with_context(|| {
        format!(
            "removing authenticated uncommitted snapshot {:?}",
            paths.state
        )
    })?;
    Ok(())
}

fn require_exact_snapshot(path: &Path, expected: &crate::map::BalanceMap) -> Result<()> {
    let loaded = crate::map::BalanceMap::load_strict(path)
        .with_context(|| format!("strictly loading snapshot {path:?}"))?;
    anyhow::ensure!(
        loaded.semantically_eq(expected),
        "snapshot {path:?} is not an exact row-for-row match for the bootstrap cache projection"
    );
    Ok(())
}

async fn ensure_mainnet<P: Provider>(provider: &P, retries: u32) -> Result<()> {
    retry_rpc(retries, "checking eth_chainId", || async {
        tokio::time::timeout(RPC_TIMEOUT, crate::chain::ensure_mainnet(provider))
            .await
            .context("eth_chainId timed out")?
    })
    .await
}

async fn select_target<P: Provider>(
    provider: &P,
    confirmations: u64,
    retries: u32,
) -> Result<(u64, B256)> {
    let head = retry_rpc(retries, "reading bootstrap head", || async {
        tokio::time::timeout(RPC_TIMEOUT, provider.get_block_number())
            .await
            .context("eth_blockNumber timed out")?
            .map_err(Into::into)
    })
    .await?;
    let target = crate::chain::confirmed_target(head, confirmations)?;
    validate_target(target)?;
    let hash = required_hash(provider, target, retries).await?;
    Ok((target, hash))
}

fn validate_target(target: u64) -> Result<()> {
    for (name, deployment) in [
        ("USDT", crate::tokens::USDT_DEPLOY_BLOCK),
        ("USDC", crate::tokens::USDC_DEPLOY_BLOCK),
        ("Multicall3", crate::tokens::MULTICALL3_DEPLOY_BLOCK),
    ] {
        anyhow::ensure!(
            target >= deployment,
            "confirmed target {target} predates {name} deployment block {deployment}"
        );
    }
    u32::try_from(target).context("confirmed target does not fit USDTPIR3 block stamps")?;
    Ok(())
}

async fn required_hash<P: Provider>(provider: &P, block: u64, retries: u32) -> Result<B256> {
    retry_rpc(retries, &format!("reading block hash {block}"), || async {
        let hash = tokio::time::timeout(RPC_TIMEOUT, crate::chain::block_hash(provider, block))
            .await
            .context("block-hash request timed out")??;
        hash.with_context(|| format!("RPC returned no block/hash for {block}"))
    })
    .await
}

async fn read_supplies<P: Provider>(
    provider: &P,
    block: u64,
    retries: u32,
) -> Result<crate::chain::TotalSupplies> {
    retry_rpc(
        retries,
        &format!("reading total supplies at {block}"),
        || async { crate::chain::read_total_supplies(provider, block).await },
    )
    .await
}

pub(crate) async fn rpc_preflight<P: Provider>(
    provider: &P,
    target: u64,
    retries: u32,
) -> Result<()> {
    crate::chain::ensure_multicall_at(target)?;
    let logs = retry_rpc(retries, "checking historical log access", || async {
        tokio::time::timeout(
            RPC_TIMEOUT,
            provider.get_logs(&crate::chain::filter(
                crate::tokens::USDT_DEPLOY_BLOCK,
                crate::tokens::USDT_DEPLOY_BLOCK,
            )),
        )
        .await
        .context("historical eth_getLogs preflight timed out")?
        .map_err(Into::into)
    })
    .await?;
    let mut touched = HashMap::new();
    crate::chain::collect_touched_strict(
        &logs,
        crate::tokens::USDT_DEPLOY_BLOCK..=crate::tokens::USDT_DEPLOY_BLOCK,
        &mut touched,
    )
    .context("historical log preflight returned malformed matched logs")?;

    let owner_probe = crate::tokens::USDT_OWNER_PREFLIGHT_BLOCK;
    let supply_logs = retry_rpc(
        retries,
        "checking historical USDT supply-event access",
        || async {
            tokio::time::timeout(
                RPC_TIMEOUT,
                provider.get_logs(&crate::chain::filter(owner_probe, owner_probe)),
            )
            .await
            .context("historical USDT supply-event eth_getLogs preflight timed out")?
            .map_err(Into::into)
        },
    )
    .await?;
    let mut supply_touched = HashMap::new();
    let collected = crate::chain::collect_touched_strict(
        &supply_logs,
        owner_probe..=owner_probe,
        &mut supply_touched,
    )
    .context("historical USDT supply-event preflight returned malformed matched logs")?;
    anyhow::ensure!(
        collected
            .usdt_supply_blocks
            .binary_search(&owner_probe)
            .is_ok(),
        "verified USDT owner preflight block {owner_probe} no longer contains Issue/Redeem"
    );
    let owner = retry_rpc(
        retries,
        "checking historical USDT owner() state",
        || async { crate::chain::usdt_owner_strict(provider, owner_probe).await },
    )
    .await?;
    anyhow::ensure!(
        !owner.is_zero(),
        "USDT owner() returned zero at historical preflight block {owner_probe}"
    );

    retry_rpc(retries, "checking target Multicall state", || async {
        crate::chain::read_balances_strict(provider, &[crate::tokens::USDT_INITIAL_OWNER], target)
            .await
            .map(|_| ())
    })
    .await?;
    read_supplies(provider, target, retries).await?;
    Ok(())
}

async fn retry_rpc<T, F, Fut>(retries: u32, label: &str, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut counter = RetryCounter::new(retries);
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => retry_wait(&mut counter, error.context(label.to_string())).await?,
        }
    }
}

struct RetryCounter {
    additional: u32,
    failures: u32,
}

impl RetryCounter {
    fn new(additional: u32) -> Self {
        Self {
            additional,
            failures: 0,
        }
    }

    fn progress(&mut self) {
        self.failures = 0;
    }

    fn fail(&mut self, error: anyhow::Error) -> Result<Duration> {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.additional {
            return Err(error.context(format!(
                "giving up after {} failed attempt(s) with no progress ({} additional retries configured)",
                self.failures, self.additional
            )));
        }
        Ok(backoff(self.failures))
    }
}

async fn retry_wait(counter: &mut RetryCounter, error: anyhow::Error) -> Result<()> {
    let detail = crate::redact::urls(&format!("{error:#}"));
    let delay = counter.fail(error)?;
    tracing::warn!(
        failed_attempts = counter.failures,
        retry_in = ?delay,
        "bootstrap unit failed; retrying: {detail}"
    );
    tokio::time::sleep(delay).await;
    Ok(())
}

fn backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(6);
    let base_ms = 500u64.saturating_mul(1u64 << exponent).min(30_000);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % (base_ms / 4 + 1);
    Duration::from_millis(base_ms + jitter)
}

fn refuse_existing_state(state: &Path) -> Result<()> {
    anyhow::ensure!(
        !state.exists(),
        "state {state:?} already exists without a matching recovery cache; refusing to start or overwrite it"
    );
    Ok(())
}

fn remove_uninitialized_cache(paths: &BootstrapPaths) -> Result<()> {
    anyhow::ensure!(
        Cache::inspect(&paths.cache)? == CacheKind::Uninitialized,
        "bootstrap cache {:?} is no longer an authenticated uninitialized database; refusing cleanup",
        paths.cache
    );
    for (name, path) in [
        ("cache WAL", &paths.cache_wal),
        ("cache SHM", &paths.cache_shm),
        ("cache rollback journal", &paths.cache_journal),
        ("cache", &paths.cache),
    ] {
        require_regular_or_missing(name, path)?;
    }
    for path in [
        &paths.cache_wal,
        &paths.cache_shm,
        &paths.cache_journal,
        &paths.cache,
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("removing {path:?}")),
        }
    }
    crate::map::fsync_parent(&paths.cache)?;
    Ok(())
}

fn refuse_orphaned_cache_sidecars(paths: &BootstrapPaths) -> Result<()> {
    let mut existing = Vec::new();
    for (name, path) in [
        ("WAL", &paths.cache_wal),
        ("SHM", &paths.cache_shm),
        ("rollback journal", &paths.cache_journal),
    ] {
        match std::fs::symlink_metadata(path) {
            Ok(_) => existing.push(format!("{name} {path:?}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting cache {name} {path:?}"));
            }
        }
    }
    anyhow::ensure!(
        existing.is_empty(),
        "bootstrap cache {:?} is missing but derived sidecars exist ({}); refusing to delete or initialize over unauthenticated artifacts",
        paths.cache,
        existing.join(", ")
    );
    Ok(())
}

fn require_regular_or_missing(name: &str, path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspecting {name} {path:?}")),
    };
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "{name} {path:?} is a symlink; refusing cleanup"
    );
    anyhow::ensure!(
        metadata.is_file(),
        "{name} {path:?} is not a regular file; refusing cleanup"
    );
    Ok(())
}

fn finish(cache: Cache, paths: &BootstrapPaths, keep_cache: bool) {
    if keep_cache {
        tracing::info!(cache = ?cache.path(), "retaining completed bootstrap cache");
        return;
    }
    let cache_path = cache.path().to_path_buf();
    if let Err(error) = cache.checkpoint_wal() {
        tracing::warn!(
            "snapshot is complete, but final cache checkpoint failed; retaining cache: {error:#}"
        );
        return;
    }
    drop(cache);
    let cleanup = (|| -> Result<()> {
        for path in [
            &paths.cache_wal,
            &paths.cache_shm,
            &paths.cache_journal,
            &cache_path,
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("removing {path:?}")),
            }
        }
        crate::map::fsync_parent(&cache_path)?;
        Ok(())
    })();
    match cleanup {
        Ok(()) => tracing::info!(cache = ?cache_path, "removed completed bootstrap cache"),
        Err(error) => tracing::warn!(
            "snapshot is complete, but cache cleanup failed; it may be retained for diagnostics: {error:#}"
        ),
    }
}

fn log_phase(metadata: &Metadata, cache: &Cache) -> Result<()> {
    let (candidates, unread) = cache.counts()?;
    tracing::info!(
        phase = metadata.phase.as_str(),
        target = metadata.target_block,
        hash = %metadata.target_hash,
        scan_cursor = metadata.scan_cursor,
        candidates,
        unread,
        "bootstrap resume point"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::eth::{Block, Log};
    use alloy::transports::mock::Asserter;

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "usdt-pir-bootstrap-flow-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn config(root: &Path) -> Config {
        Config {
            state: root.join("state.snapshot"),
            cache: None,
            confirmations: 4,
            chunk: 10_000,
            retries: 10,
            keep_cache: true,
        }
    }

    fn block(number: u64, hash: B256) -> Block {
        let mut block: Block = Default::default();
        block.header.hash = hash;
        block.header.inner.number = number;
        block
    }

    fn inspected_metadata(paths: &BootstrapPaths) -> Metadata {
        match Cache::inspect(&paths.cache).unwrap() {
            CacheKind::Initialized(metadata) => metadata,
            other => panic!("expected initialized cache, found {other:?}"),
        }
    }

    fn switch_cache_to_delete_journal(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "delete");
    }

    fn assert_delete_journal(path: &Path) {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "delete");
    }

    fn uint_word(value: u128) -> Bytes {
        let mut word = [0u8; 32];
        word[16..].copy_from_slice(&value.to_be_bytes());
        Bytes::copy_from_slice(&word)
    }

    fn address_word(address: Address) -> B256 {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(address.as_slice());
        B256::from(word)
    }

    fn address_return(address: Address) -> Bytes {
        Bytes::copy_from_slice(address_word(address).as_slice())
    }

    fn issue_log(block: u64) -> Log {
        Log {
            inner: alloy::primitives::Log::new(
                crate::tokens::USDT,
                vec![crate::tokens::ISSUE],
                uint_word(1),
            )
            .unwrap(),
            block_hash: Some(B256::repeat_byte(0xa1)),
            block_number: Some(block),
            ..Default::default()
        }
    }

    fn inert_transfer(block: u64) -> Log {
        let address = Address::repeat_byte(0x33);
        Log {
            inner: alloy::primitives::Log::new(
                crate::tokens::USDT,
                vec![
                    crate::tokens::TRANSFER,
                    address_word(address),
                    address_word(address),
                ],
                Bytes::from(vec![0u8; 32]),
            )
            .unwrap(),
            block_hash: Some(B256::repeat_byte(0xa2)),
            block_number: Some(block),
            ..Default::default()
        }
    }

    fn push_preflight_success(
        asserter: &Asserter,
        owner: Address,
        reading: crate::map::Reading,
        supplies: crate::chain::TotalSupplies,
    ) {
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&vec![issue_log(crate::tokens::USDT_OWNER_PREFLIGHT_BLOCK)]);
        asserter.push_success(&address_return(owner));
        asserter.push_success(&crate::chain::encode_balance_response_for_test(&[reading]));
        asserter.push_success(&uint_word(supplies.usdt));
        asserter.push_success(&uint_word(supplies.usdc));
    }

    fn ready_cache(root: &Path) -> (BootstrapPaths, Cache, crate::map::BalanceMap) {
        let cfg = config(root);
        ready_cache_for_config(&cfg)
    }

    fn ready_cache_for_config(cfg: &Config) -> (BootstrapPaths, Cache, crate::map::BalanceMap) {
        let paths = BootstrapPaths::resolve(&cfg.state, cfg.cache.as_deref()).unwrap();
        let target = 20_000_000;
        let mut cache = Cache::initialize(
            &paths.cache,
            &paths.state,
            cfg.confirmations,
            target,
            B256::repeat_byte(0x41),
        )
        .unwrap();
        let address = Address::repeat_byte(0x51);
        cache
            .commit_scan_range(
                crate::tokens::USDT_DEPLOY_BLOCK,
                target,
                &HashMap::from([(
                    address,
                    crate::chain::Touch {
                        usdt: Some(target - 2),
                        usdc: Some(target - 1),
                    },
                )]),
            )
            .unwrap();
        cache.finish_scanning().unwrap();
        cache
            .commit_balances(&HashMap::from([
                (
                    crate::tokens::USDT_INITIAL_OWNER,
                    crate::map::Reading::default(),
                ),
                (address, crate::map::Reading { usdt: 11, usdc: 7 }),
            ]))
            .unwrap();
        let projection = cache.projection().unwrap().map;
        cache.set_ready_to_commit().unwrap();
        (paths, cache, projection)
    }

    fn completed_cache(root: &Path) -> (BootstrapPaths, Cache, crate::map::BalanceMap) {
        let (paths, mut cache, projection) = ready_cache(root);
        cache.set_complete().unwrap();
        (paths, cache, projection)
    }

    #[tokio::test]
    async fn full_run_scans_both_inclusive_boundaries_and_reopens_complete_artifacts() {
        let root = root("full-orchestration");
        let mut cfg = config(&root);
        cfg.chunk = u64::MAX;
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let target = crate::tokens::USDT_OWNER_PREFLIGHT_BLOCK + 100;
        let hash = B256::repeat_byte(0xb1);
        let owner = Address::repeat_byte(0xb2);
        let reading = crate::map::Reading { usdt: 10, usdc: 0 };
        let supplies = crate::chain::TotalSupplies { usdt: 10, usdc: 0 };
        let asserter = Asserter::new();

        asserter.push_success(&1u64);
        asserter.push_success(&(target + cfg.confirmations));
        asserter.push_success(&Some(block(target, hash)));
        push_preflight_success(&asserter, owner, reading, supplies);
        asserter.push_success(&vec![
            inert_transfer(crate::tokens::USDT_DEPLOY_BLOCK),
            inert_transfer(target),
        ]);
        asserter.push_success(&crate::chain::encode_balance_response_for_test(&[reading]));
        asserter.push_success(&1u64);
        asserter.push_success(&uint_word(supplies.usdt));
        asserter.push_success(&uint_word(supplies.usdc));
        asserter.push_success(&Some(block(target, hash)));
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(target, hash)));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        run(&provider, cfg).await.unwrap();
        assert!(asserter.read_q().is_empty());

        let snapshot = crate::map::BalanceMap::load_strict(&paths.state).unwrap();
        assert_eq!(snapshot.cursor, target);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot
                .get(&crate::tokens::USDT_INITIAL_OWNER)
                .unwrap()
                .usdt,
            10
        );
        let cache = Cache::open(&paths.cache).unwrap();
        let metadata = cache.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Complete);
        assert_eq!(metadata.scan_start, crate::tokens::USDT_DEPLOY_BLOCK);
        assert_eq!(metadata.scan_cursor, target);
        assert_eq!(cache.counts().unwrap(), (1, 0));
        assert!(cache.projection().unwrap().map.semantically_eq(&snapshot));
    }

    #[tokio::test]
    async fn production_scan_narrows_on_provider_cap_then_grows_to_configured_chunk() {
        let root = root("adaptive-production");
        let mut cfg = config(&root);
        cfg.chunk = 8;
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let start = crate::tokens::USDT_DEPLOY_BLOCK;
        let target = start + 7;
        let mut cache = Cache::initialize(
            &paths.cache,
            &paths.state,
            cfg.confirmations,
            target,
            B256::repeat_byte(0xb3),
        )
        .unwrap();
        let asserter = Asserter::new();
        asserter.push_failure_msg("query returned more than the provider result cap");
        asserter.push_success(&vec![inert_transfer(start), inert_transfer(start + 3)]);
        asserter.push_success(&vec![inert_transfer(start + 4), inert_transfer(target)]);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        scan(&provider, &mut cache, &cfg).await.unwrap();
        assert!(asserter.read_q().is_empty());
        let metadata = cache.metadata().unwrap();
        assert_eq!(metadata.scan_cursor, target);
        assert_eq!(metadata.phase, Phase::ReadingBalances);
    }

    #[tokio::test]
    async fn malformed_scan_log_leaves_reopened_durable_cursor_unchanged() {
        let root = root("malformed-durable-cursor");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let start = crate::tokens::USDT_DEPLOY_BLOCK;
        let target = start + 1;
        let mut cache = Cache::initialize(
            &paths.cache,
            &paths.state,
            cfg.confirmations,
            target,
            B256::repeat_byte(0xb4),
        )
        .unwrap();
        let mut malformed = inert_transfer(start);
        malformed.removed = true;
        let asserter = Asserter::new();
        asserter.push_success(&vec![malformed]);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let error = scan(&provider, &mut cache, &cfg).await.unwrap_err();
        assert!(format!("{error:#}").contains("marked removed"));
        assert!(asserter.read_q().is_empty());
        drop(cache);

        let reopened = Cache::open(&paths.cache).unwrap();
        let metadata = reopened.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Scanning);
        assert_eq!(metadata.scan_cursor, start - 1);
        assert_eq!(reopened.counts().unwrap(), (1, 1));
    }

    async fn run_ready_recovery(
        root: &Path,
        paths: &BootstrapPaths,
        expected: &crate::map::BalanceMap,
    ) {
        let mut cfg = config(root);
        cfg.retries = 0;
        let hash = B256::repeat_byte(0x41);
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(expected.cursor, hash)));
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(expected.cursor, hash)));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        run(&provider, cfg).await.unwrap();
        assert!(asserter.read_q().is_empty());

        let reopened = Cache::open(&paths.cache).unwrap();
        assert_eq!(reopened.metadata().unwrap().phase, Phase::Complete);
        let snapshot = crate::map::BalanceMap::load_strict(&paths.state).unwrap();
        assert!(snapshot.semantically_eq(expected));
        assert!(
            reopened
                .projection()
                .unwrap()
                .map
                .semantically_eq(&snapshot)
        );
    }

    #[tokio::test]
    async fn ready_crash_before_snapshot_save_recovers_through_full_run() {
        let root = root("ready-before-save");
        let (paths, cache, expected) = ready_cache(&root);
        drop(cache); // Simulated process exit immediately after ReadyToCommit.
        assert!(!paths.state.exists());
        run_ready_recovery(&root, &paths, &expected).await;
    }

    #[tokio::test]
    async fn ready_crash_after_snapshot_rename_recovers_through_full_run() {
        let root = root("ready-after-rename");
        let (paths, cache, expected) = ready_cache(&root);
        expected.save(&paths.state).unwrap();
        drop(cache); // Snapshot is durable, but phase is still ReadyToCommit.
        assert_eq!(
            Cache::open(&paths.cache).unwrap().metadata().unwrap().phase,
            Phase::ReadyToCommit
        );
        run_ready_recovery(&root, &paths, &expected).await;
    }

    #[tokio::test]
    async fn ready_recovery_fsyncs_state_parent_before_complete_and_cache_cleanup() {
        let root = root("ready-rename-before-parent-sync");
        let state_dir = root.join("state-device");
        let cache_dir = root.join("cache-device");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        let mut cfg = config(&root);
        cfg.state = state_dir.join("balances.snapshot");
        cfg.cache = Some(cache_dir.join("bootstrap.sqlite"));
        cfg.retries = 0;
        cfg.keep_cache = false;
        let (paths, cache, expected) = ready_cache_for_config(&cfg);
        assert_ne!(paths.state.parent(), paths.cache.parent());

        // The staging file is durable, but this rename intentionally omits the
        // destination-parent fsync to model a crash at that exact boundary.
        let staging = state_dir.join("renamed-before-directory-fsync");
        expected.save(&staging).unwrap();
        std::fs::rename(&staging, &paths.state).unwrap();
        drop(cache);

        let mut reopened = Cache::open(&paths.cache).unwrap();
        assert_eq!(reopened.metadata().unwrap().phase, Phase::ReadyToCommit);
        let failing = Asserter::new();
        failing.push_success(&1u64);
        failing.push_success(&Some(block(expected.cursor, B256::repeat_byte(0x41))));
        let provider = ProviderBuilder::new().connect_mocked_client(failing.clone());
        let error = commit_ready_with_parent_sync(&provider, &mut reopened, &paths, &cfg, |_| {
            Err(std::io::Error::other("injected state-parent fsync failure"))
        })
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected state-parent fsync failure"));
        assert_eq!(reopened.metadata().unwrap().phase, Phase::ReadyToCommit);
        assert!(paths.cache.exists());
        assert!(failing.read_q().is_empty());

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(expected.cursor, B256::repeat_byte(0x41))));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let parent_synced = std::cell::Cell::new(false);

        let reset =
            commit_ready_with_parent_sync(&provider, &mut reopened, &paths, &cfg, |state| {
                assert_eq!(state, paths.state);
                let observer = rusqlite::Connection::open_with_flags(
                    &paths.cache,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let phase: String = observer
                    .query_row("SELECT phase FROM metadata WHERE singleton=1", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(phase, Phase::ReadyToCommit.as_str());
                assert!(
                    paths.cache.exists(),
                    "cache must still exist at state fsync"
                );
                crate::map::fsync_parent(state)?;
                parent_synced.set(true);
                Ok(())
            })
            .await
            .unwrap();

        assert!(!reset);
        assert!(parent_synced.get());
        assert_eq!(reopened.metadata().unwrap().phase, Phase::Complete);
        assert!(asserter.read_q().is_empty());
        finish(reopened, &paths, false);
        assert!(!paths.cache.exists());
        assert!(
            crate::map::BalanceMap::load_strict(&paths.state)
                .unwrap()
                .semantically_eq(&expected)
        );
    }

    fn push_hash_mismatch_replacement(
        asserter: &Asserter,
        old_target: u64,
        replacement: u64,
        replacement_hash: B256,
    ) {
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(old_target, B256::repeat_byte(0xc1))));
        asserter.push_success(&1u64);
        asserter.push_success(&(replacement + 4));
        asserter.push_success(&Some(block(replacement, replacement_hash)));
    }

    #[tokio::test]
    async fn hash_mismatch_validates_replacement_before_removing_ready_snapshot_and_resetting() {
        let root = root("reset-ordering");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let (paths, cache, expected) = ready_cache(&root);
        let old = cache.metadata().unwrap();
        expected.save(&paths.state).unwrap();
        drop(cache);
        let replacement = crate::tokens::USDT_OWNER_PREFLIGHT_BLOCK + 200;
        let replacement_hash = B256::repeat_byte(0xc2);

        let failing = Asserter::new();
        push_hash_mismatch_replacement(&failing, old.target_block, replacement, replacement_hash);
        failing.push_failure_msg("historical preflight unavailable");
        let provider = ProviderBuilder::new().connect_mocked_client(failing.clone());
        assert!(
            resume(&provider, &paths, &cfg, inspected_metadata(&paths))
                .await
                .is_err()
        );
        assert!(failing.read_q().is_empty());
        assert!(
            crate::map::BalanceMap::load_strict(&paths.state)
                .unwrap()
                .semantically_eq(&expected)
        );
        let unchanged = Cache::open(&paths.cache).unwrap();
        let unchanged_metadata = unchanged.metadata().unwrap();
        assert_eq!(unchanged_metadata.phase, Phase::ReadyToCommit);
        assert_eq!(unchanged_metadata.target_block, old.target_block);
        drop(unchanged);

        let successful = Asserter::new();
        push_hash_mismatch_replacement(
            &successful,
            old.target_block,
            replacement,
            replacement_hash,
        );
        push_preflight_success(
            &successful,
            Address::repeat_byte(0xc3),
            crate::map::Reading::default(),
            crate::chain::TotalSupplies { usdt: 0, usdc: 0 },
        );
        let provider = ProviderBuilder::new().connect_mocked_client(successful.clone());
        let reset = resume(&provider, &paths, &cfg, inspected_metadata(&paths))
            .await
            .unwrap();
        assert!(successful.read_q().is_empty());
        assert!(!paths.state.exists());
        let metadata = reset.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Scanning);
        assert_eq!(metadata.target_block, replacement);
        assert_eq!(metadata.target_hash, replacement_hash);
        assert_eq!(metadata.scan_cursor, crate::tokens::USDT_DEPLOY_BLOCK - 1);
        assert_eq!(reset.counts().unwrap(), (1, 1));
    }

    #[test]
    fn retry_budget_counts_initial_failure_plus_additional_attempts() {
        let mut counter = RetryCounter::new(2);
        assert!(counter.fail(anyhow::anyhow!("one")).is_ok());
        assert!(counter.fail(anyhow::anyhow!("two")).is_ok());
        let error = counter.fail(anyhow::anyhow!("three")).unwrap_err();
        assert!(format!("{error:#}").contains("3 failed attempt"));
    }

    #[tokio::test]
    async fn production_rpc_retry_budget_is_one_initial_plus_configured_additional_calls() {
        let asserter = Asserter::new();
        for attempt in 1..=3 {
            asserter.push_failure_msg(format!("injected failure {attempt}"));
        }
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let error = required_hash(&provider, 20_000_000, 2).await.unwrap_err();

        assert!(format!("{error:#}").contains("giving up after 3 failed attempt(s)"));
        assert!(
            asserter.read_q().is_empty(),
            "exactly three RPC responses must be consumed"
        );
    }

    #[test]
    fn progress_resets_the_stalled_retry_count() {
        let mut counter = RetryCounter::new(1);
        counter.fail(anyhow::anyhow!("one")).unwrap();
        counter.progress();
        assert!(counter.fail(anyhow::anyhow!("one again")).is_ok());
    }

    #[test]
    fn targets_must_cover_both_tokens_and_multicall() {
        assert!(validate_target(crate::tokens::MULTICALL3_DEPLOY_BLOCK).is_ok());
        let error = validate_target(crate::tokens::USDC_DEPLOY_BLOCK).unwrap_err();
        assert!(format!("{error:#}").contains("Multicall3"));
    }

    #[test]
    fn projection_validation_rejects_supply_mismatches_and_injected_capacity() {
        let empty = Projection {
            map: crate::map::BalanceMap::new(20_000_000),
            candidates: 0,
            usdt_sum: 10,
            usdc_sum: 20,
        };
        let usdt_error = validate_projection_with_capacity(
            &empty,
            crate::chain::TotalSupplies { usdt: 11, usdc: 20 },
            1,
        )
        .unwrap_err();
        assert!(format!("{usdt_error:#}").contains("USDT holder sum"));
        let usdc_error = validate_projection_with_capacity(
            &empty,
            crate::chain::TotalSupplies { usdt: 10, usdc: 21 },
            1,
        )
        .unwrap_err();
        assert!(format!("{usdc_error:#}").contains("USDC holder sum"));

        let mut map = crate::map::BalanceMap::new(20_000_000);
        for byte in [0xe1, 0xe2] {
            map.seed(
                Address::repeat_byte(byte),
                crate::map::Entry {
                    usdt: 1,
                    usdt_block: 19_999_999,
                    ..Default::default()
                },
            );
        }
        let over_capacity = Projection {
            map,
            candidates: 2,
            usdt_sum: 2,
            usdc_sum: 0,
        };
        let error = validate_projection_with_capacity(
            &over_capacity,
            crate::chain::TotalSupplies { usdt: 2, usdc: 0 },
            1,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("2 holders exceed deployed PIR capacity 1"));
    }

    #[test]
    fn adaptive_log_ranges_narrow_then_grow_back_to_configuration() {
        let (hi, chunk) = narrow_range(1_000, 1_999);
        assert_eq!((hi, chunk), (1_499, 500));
        assert_eq!(grow_range(chunk, 1_000), 1_000);
        let (hi, chunk) = narrow_range(7, 8);
        assert_eq!((hi, chunk), (7, 1));
        assert_eq!(grow_range(chunk, 10_000), 2);
    }

    #[test]
    fn resume_binding_pins_target_identity_and_confirmation_depth_only() {
        let root = root("binding");
        let cfg = config(&root);
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let cache = Cache::initialize(
            &paths.cache,
            &paths.state,
            4,
            20_000_000,
            B256::repeat_byte(0x44),
        )
        .unwrap();
        let metadata = cache.metadata().unwrap();
        let mut tuned = cfg.clone();
        tuned.chunk = 1;
        tuned.retries = 99;
        assert!(validate_resume_binding(&metadata, &paths, &tuned).is_ok());
        tuned.confirmations = 5;
        let error = validate_resume_binding(&metadata, &paths, &tuned).unwrap_err();
        assert!(format!("{error:#}").contains("pinned confirmation depth"));
        assert_eq!(metadata.target_block, 20_000_000, "target remains pinned");
    }

    #[test]
    fn complete_cache_reconstructs_missing_state_without_chain_reads() {
        let root = root("complete-recovery");
        let (paths, cache, expected) = completed_cache(&root);
        assert!(!paths.state.exists());
        recover_complete(&cache, &paths).unwrap();
        let loaded = crate::map::BalanceMap::load_strict(&paths.state).unwrap();
        assert!(loaded.semantically_eq(&expected));
    }

    #[test]
    fn complete_cache_refuses_to_overwrite_corrupt_existing_state() {
        let root = root("complete-corrupt");
        let (paths, cache, _) = completed_cache(&root);
        std::fs::write(&paths.state, b"unrelated state").unwrap();
        let error = recover_complete(&cache, &paths).unwrap_err();
        assert!(format!("{error:#}").contains("strictly loading"));
        assert_eq!(std::fs::read(&paths.state).unwrap(), b"unrelated state");
    }

    #[test]
    fn completed_cache_is_deleted_only_after_verified_state_survives() {
        let root = root("complete-cleanup");
        let (paths, cache, expected) = completed_cache(&root);
        recover_complete(&cache, &paths).unwrap();
        finish(cache, &paths, false);
        assert!(!paths.cache.exists());
        assert!(!paths.cache_wal.exists());
        assert!(!paths.cache_shm.exists());
        assert!(
            crate::map::BalanceMap::load_strict(&paths.state)
                .unwrap()
                .semantically_eq(&expected)
        );
    }

    #[test]
    fn hash_reset_removes_only_an_exact_authenticated_ready_snapshot() {
        let root = root("ready-reset");
        let (paths, cache, projection) = ready_cache(&root);
        projection.save(&paths.state).unwrap();
        let metadata = cache.metadata().unwrap();
        remove_authenticated_state_for_reset(&cache, &paths, &metadata).unwrap();
        assert!(!paths.state.exists());

        std::fs::write(&paths.state, b"unrelated").unwrap();
        assert!(remove_authenticated_state_for_reset(&cache, &paths, &metadata).is_err());
        assert_eq!(std::fs::read(&paths.state).unwrap(), b"unrelated");
    }

    #[test]
    fn exact_comparison_rejects_same_cursor_count_and_totals_with_different_rows() {
        let root = root("exact");
        let path = root.join("state.snapshot");
        let a = Address::repeat_byte(0x61);
        let b = Address::repeat_byte(0x62);
        let mut expected = crate::map::BalanceMap::new(20_000_000);
        expected.seed(
            a,
            crate::map::Entry {
                usdt: 10,
                usdt_block: 19_999_990,
                ..Default::default()
            },
        );
        expected.seed(
            b,
            crate::map::Entry {
                usdt: 20,
                usdt_block: 19_999_991,
                ..Default::default()
            },
        );
        let mut different = crate::map::BalanceMap::new(20_000_000);
        different.seed(
            a,
            crate::map::Entry {
                usdt: 20,
                usdt_block: 19_999_991,
                ..Default::default()
            },
        );
        different.seed(
            b,
            crate::map::Entry {
                usdt: 10,
                usdt_block: 19_999_990,
                ..Default::default()
            },
        );
        different.save(&path).unwrap();
        assert_eq!(expected.len(), different.len());
        assert_eq!(
            expected.iter().map(|(_, entry)| entry.usdt).sum::<u128>(),
            different.iter().map(|(_, entry)| entry.usdt).sum::<u128>()
        );
        assert!(require_exact_snapshot(&path, &expected).is_err());
    }

    #[test]
    fn any_existing_state_without_recovery_authority_is_refused() {
        let root = root("existing");
        let state = root.join("state.snapshot");
        std::fs::write(&state, b"even a corrupt file blocks bootstrap").unwrap();
        assert!(refuse_existing_state(&state).is_err());
    }

    #[tokio::test]
    async fn full_run_refuses_and_preserves_a_view_only_cache_before_rpc() {
        let root = root("view-cache-preservation");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let conn = rusqlite::Connection::open(&paths.cache).unwrap();
        conn.execute_batch("CREATE VIEW operator_data AS SELECT 7 AS value")
            .unwrap();
        drop(conn);
        let before = std::fs::read(&paths.cache).unwrap();

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = run(&provider, cfg).await.unwrap_err();

        assert!(format!("{error:#}").contains("corrupt or unrelated"));
        assert_eq!(std::fs::read(&paths.cache).unwrap(), before);
        assert!(
            !asserter.read_q().is_empty(),
            "schema ownership must be decided before any RPC request"
        );
        let conn = rusqlite::Connection::open_with_flags(
            &paths.cache,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let value: i64 = conn
            .query_row("SELECT value FROM operator_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn incompatible_identity_is_rejected_without_changing_delete_journal_cache() {
        let root = root("identity-read-only");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        drop(
            Cache::initialize(
                &paths.cache,
                &paths.state,
                cfg.confirmations,
                20_000_000,
                B256::repeat_byte(0xd1),
            )
            .unwrap(),
        );
        switch_cache_to_delete_journal(&paths.cache);
        let conn = rusqlite::Connection::open(&paths.cache).unwrap();
        conn.execute(
            "UPDATE metadata SET bootstrap_identity='operator-owned-identity' WHERE singleton=1",
            [],
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&paths.cache).unwrap();

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = run(&provider, cfg).await.unwrap_err();

        assert!(format!("{error:#}").contains("identity is incompatible"));
        assert_eq!(std::fs::read(&paths.cache).unwrap(), before);
        assert_delete_journal(&paths.cache);
        assert!(!paths.cache_wal.exists());
        assert!(!paths.cache_shm.exists());
        assert!(
            !asserter.read_q().is_empty(),
            "incompatible identity must be rejected before RPC"
        );
    }

    #[tokio::test]
    async fn invocation_binding_is_rejected_before_read_write_cache_open() {
        let root = root("binding-read-only");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        drop(
            Cache::initialize(
                &paths.cache,
                &paths.state,
                cfg.confirmations,
                20_000_000,
                B256::repeat_byte(0xd2),
            )
            .unwrap(),
        );
        switch_cache_to_delete_journal(&paths.cache);
        let before = std::fs::read(&paths.cache).unwrap();
        cfg.confirmations += 1;

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = run(&provider, cfg).await.unwrap_err();

        assert!(format!("{error:#}").contains("pinned confirmation depth"));
        assert_eq!(std::fs::read(&paths.cache).unwrap(), before);
        assert_delete_journal(&paths.cache);
        assert!(!paths.cache_wal.exists());
        assert!(!paths.cache_shm.exists());
        assert!(
            !asserter.read_q().is_empty(),
            "invocation binding must be rejected before RPC"
        );
    }

    #[tokio::test]
    async fn missing_main_cache_never_authorizes_deleting_an_arbitrary_sidecar() {
        let root = root("orphaned-sidecar");
        let mut cfg = config(&root);
        cfg.cache = Some(root.join("cache.sqlite"));
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, cfg.cache.as_deref()).unwrap();
        std::fs::write(&paths.cache_wal, b"unrelated WAL-named bytes").unwrap();

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = run(&provider, cfg).await.unwrap_err();

        assert!(format!("{error:#}").contains("missing but derived sidecars exist"));
        assert!(!paths.cache.exists());
        assert_eq!(
            std::fs::read(&paths.cache_wal).unwrap(),
            b"unrelated WAL-named bytes"
        );
        assert!(
            !asserter.read_q().is_empty(),
            "orphaned sidecars must be refused before RPC"
        );
    }

    #[test]
    fn authenticated_uninitialized_main_allows_only_regular_sidecar_cleanup() {
        let root = root("authenticated-sidecar-cleanup");
        let mut cfg = config(&root);
        cfg.cache = Some(root.join("cache.sqlite"));
        let paths = BootstrapPaths::resolve(&cfg.state, cfg.cache.as_deref()).unwrap();
        std::fs::File::create(&paths.cache).unwrap();
        for path in [&paths.cache_wal, &paths.cache_shm, &paths.cache_journal] {
            std::fs::write(path, b"crash sidecar").unwrap();
        }

        remove_uninitialized_cache(&paths).unwrap();

        for path in [
            &paths.cache,
            &paths.cache_wal,
            &paths.cache_shm,
            &paths.cache_journal,
        ] {
            assert!(!path.exists(), "authenticated artifact survived: {path:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uninitialized_main_never_authorizes_following_a_sidecar_symlink() {
        let root = root("uninitialized-sidecar-symlink");
        let mut cfg = config(&root);
        cfg.cache = Some(root.join("cache.sqlite"));
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, cfg.cache.as_deref()).unwrap();
        std::fs::File::create(&paths.cache).unwrap();
        let operator_file = root.join("operator-owned");
        std::fs::write(&operator_file, b"must survive").unwrap();
        std::os::unix::fs::symlink(&operator_file, &paths.cache_wal).unwrap();

        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = run(&provider, cfg).await.unwrap_err();

        assert!(format!("{error:#}").contains("cache WAL"));
        assert!(format!("{error:#}").contains("symlink"));
        assert_eq!(std::fs::metadata(&paths.cache).unwrap().len(), 0);
        assert_eq!(std::fs::read(&operator_file).unwrap(), b"must survive");
        assert!(
            std::fs::symlink_metadata(&paths.cache_wal)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !asserter.read_q().is_empty(),
            "unsafe sidecars must be rejected before RPC"
        );
    }

    #[tokio::test]
    async fn resume_reuses_pinned_target_without_reading_a_new_head() {
        let root = root("pinned-resume");
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let target = 20_000_000;
        let hash = B256::repeat_byte(0x81);
        drop(Cache::initialize(&paths.cache, &paths.state, 4, target, hash).unwrap());
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&Some(block(target, hash)));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let resumed = resume(&provider, &paths, &cfg, inspected_metadata(&paths))
            .await
            .unwrap();
        assert_eq!(resumed.metadata().unwrap().target_block, target);
        assert!(
            asserter.read_q().is_empty(),
            "no eth_blockNumber response was requested"
        );
    }

    #[tokio::test]
    async fn wrong_chain_leaves_cached_work_untouched() {
        let (paths, cfg, target) = resumable_cache("wrong-chain");
        switch_cache_to_delete_journal(&paths.cache);
        let before = std::fs::read(&paths.cache).unwrap();
        let inspected = inspected_metadata(&paths);
        let asserter = Asserter::new();
        asserter.push_success(&2u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        assert!(resume(&provider, &paths, &cfg, inspected).await.is_err());
        assert_eq!(std::fs::read(&paths.cache).unwrap(), before);
        assert_delete_journal(&paths.cache);
        assert_cache_untouched(&paths, target);
    }

    #[tokio::test]
    async fn missing_target_hash_leaves_cached_work_untouched() {
        let (paths, cfg, target) = resumable_cache("missing-hash");
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&Option::<Block>::None);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        assert!(
            resume(&provider, &paths, &cfg, inspected_metadata(&paths))
                .await
                .is_err()
        );
        assert_cache_untouched(&paths, target);
    }

    #[tokio::test]
    async fn complete_resume_does_not_read_chain_state_or_target_hash() {
        let root = root("complete-offline");
        let cfg = config(&root);
        let (paths, cache, _) = completed_cache(&root);
        drop(cache);
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let resumed = resume(&provider, &paths, &cfg, inspected_metadata(&paths))
            .await
            .unwrap();
        assert_eq!(resumed.metadata().unwrap().phase, Phase::Complete);
        assert!(asserter.read_q().is_empty());
    }

    fn resumable_cache(name: &str) -> (BootstrapPaths, Config, u64) {
        let root = root(name);
        let mut cfg = config(&root);
        cfg.retries = 0;
        let paths = BootstrapPaths::resolve(&cfg.state, None).unwrap();
        let target = 20_000_000;
        drop(
            Cache::initialize(
                &paths.cache,
                &paths.state,
                4,
                target,
                B256::repeat_byte(0x82),
            )
            .unwrap(),
        );
        (paths, cfg, target)
    }

    fn assert_cache_untouched(paths: &BootstrapPaths, target: u64) {
        let cache = Cache::open(&paths.cache).unwrap();
        let metadata = cache.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Scanning);
        assert_eq!(metadata.target_block, target);
        assert_eq!(metadata.scan_cursor, crate::tokens::USDT_DEPLOY_BLOCK - 1);
        assert_eq!(cache.counts().unwrap(), (1, 1));
    }
}
