use std::path::Path;

use alloy::providers::ProviderBuilder;
use anyhow::{Context, Result};

use crate::cli::args::Cmd;
use crate::follow::FollowConfig;
use crate::map::BalanceMap;

use super::serve_config::{ServeArgs, Serving, pir_config, progress_config};

const STARTUP_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

pub async fn run(cmd: Cmd) -> Result<()> {
    run_inner(cmd).await.map_err(crate::redact::error)
}

async fn run_inner(cmd: Cmd) -> Result<()> {
    let args = ServeArgs::take(cmd);
    let provider = ProviderBuilder::new().connect(&args.rpc).await?;
    run_with_provider(&provider, args).await
}

async fn run_with_provider<P: alloy::providers::Provider>(
    provider: &P,
    mut args: ServeArgs,
) -> Result<()> {
    args.state = crate::bootstrap::normalize_path(&args.state)?;
    let _lock = crate::map::SnapshotLock::acquire(&args.state)?;
    let balances = open_serving_map(provider, &args.state, args.from_block).await?;
    let (cfg, progress) = progress_config(&args.follow, args.state.clone());

    let balances =
        catch_up_before_publish(provider, balances, &args.state, &cfg, &args.serving.keyword)
            .await?;
    run_pir_loop(provider, balances, cfg, progress, args.serving).await
}

async fn open_serving_map<P: alloy::providers::Provider>(
    provider: &P,
    state: &Path,
    from_block: Option<crate::cli::args::Target>,
) -> Result<BalanceMap> {
    match BalanceMap::load_strict(state) {
        Ok(map) => {
            anyhow::ensure!(
                from_block.is_none(),
                "--from-block only applies when starting fresh; {state:?} already holds cursor {}",
                map.cursor
            );
            tracing::info!(
                addresses = map.len(),
                cursor = map.cursor,
                "resumed strict USDTPIR3 state"
            );
            Ok(map)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::commands::open_map(provider, state, from_block).await
        }
        Err(error) => Err(error).context(format!(
            "authoritative serve state {state:?} must be strict checksummed USDTPIR3"
        )),
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

/// Build one target-pinned startup unit in memory. Nothing above the loaded
/// cursor reaches disk, PIR construction, or the endpoint until validation and
/// the final target-hash check both succeed.
async fn catch_up_before_publish<P: alloy::providers::Provider>(
    provider: &P,
    loaded: BalanceMap,
    state: &Path,
    cfg: &FollowConfig,
    keyword: &Path,
) -> Result<BalanceMap> {
    let trusted_cursor = loaded.cursor;
    loop {
        let target = resolve_startup_target(provider, cfg).await?;
        if target < trusted_cursor {
            anyhow::bail!(
                "serving RPC target {target} is behind imported snapshot cursor {trusted_cursor}; refusing to expose ahead state"
            );
        }
        u32::try_from(target).context("startup target does not fit USDTPIR3 block stamps")?;
        let target_hash = if target > trusted_cursor {
            Some(required_startup_hash(provider, target, cfg).await)
        } else {
            None
        };
        let chain_id = retry_startup(cfg, "reading Ethereum chain id", || async {
            tokio::time::timeout(STARTUP_RPC_TIMEOUT, provider.get_chain_id())
                .await
                .context("eth_chainId timed out during startup")?
                .map_err(Into::into)
        })
        .await;
        anyhow::ensure!(
            chain_id == crate::tokens::MAINNET_CHAIN_ID,
            "RPC reports chain id {chain_id}, not Ethereum mainnet ({})",
            crate::tokens::MAINNET_CHAIN_ID
        );
        crate::chain::ensure_multicall_at(target)?;

        let mut balances = loaded.clone();
        if target > trusted_cursor {
            tracing::info!(
                from = trusted_cursor + 1,
                to = target,
                hash = %target_hash.expect("captured"),
                "starting pinned startup catch-up"
            );
            if let Some(progress) = &cfg.progress {
                progress.record(trusted_cursor, target);
            }
            while balances.cursor < target {
                match crate::follow::sync_startup(provider, &mut balances, target, cfg.chunk).await
                {
                    Ok(stats) => tracing::info!(
                        blocks = stats.blocks,
                        cursor = balances.cursor,
                        "startup catch-up reached pinned target"
                    ),
                    Err(error) => {
                        tracing::warn!(
                            cursor = balances.cursor,
                            target,
                            retry_in = ?cfg.retry_base,
                            "startup catch-up failed without disk commit: {}",
                            crate::redact::urls(&format!("{error:#}"))
                        );
                        tokio::time::sleep(cfg.retry_base).await;
                    }
                }
            }
        }

        let semantic = validate_startup_map(&balances, target);
        let allocation = crate::publish::validate_initial_allocation(
            &balances,
            &crate::keyword_store::Paths::new(keyword),
        );

        if let Some(captured) = target_hash {
            let final_hash = required_startup_hash(provider, target, cfg).await;
            if final_hash != captured {
                tracing::warn!(
                    target,
                    captured = %captured,
                    current = %final_hash,
                    "startup target changed; discarding the entire in-memory attempt"
                );
                continue;
            }
        }
        semantic?;
        let allocation = allocation?;
        tracing::info!(
            occupied = allocation.occupied,
            vacant = allocation.vacant,
            appended = allocation.appended,
            final_slots = allocation.final_slots,
            capacity = allocation.capacity,
            "startup keyword allocation validated"
        );

        if target > trusted_cursor {
            balances.save(state)?;
            let reloaded = BalanceMap::load_strict(state)
                .with_context(|| format!("reloading committed startup snapshot {state:?}"))?;
            anyhow::ensure!(
                reloaded.semantically_eq(&balances),
                "saved startup snapshot does not exactly match validated in-memory map"
            );
        }
        if let Some(progress) = &cfg.progress {
            progress.record(target, target);
        }
        return Ok(balances);
    }
}

async fn resolve_startup_target<P: alloy::providers::Provider>(
    provider: &P,
    cfg: &FollowConfig,
) -> Result<u64> {
    match cfg.tip {
        crate::chain::Tip::Finalized => Ok(retry_startup(
            cfg,
            "resolving finalized startup target",
            || async {
                tokio::time::timeout(STARTUP_RPC_TIMEOUT, crate::chain::finalized_block(provider))
                    .await
                    .context("finalized startup target request timed out")?
            },
        )
        .await),
        crate::chain::Tip::Confirmations(confirmations) => {
            let head = retry_startup(cfg, "reading startup head", || async {
                tokio::time::timeout(STARTUP_RPC_TIMEOUT, provider.get_block_number())
                    .await
                    .context("startup eth_blockNumber request timed out")?
                    .map_err(Into::into)
            })
            .await;
            crate::chain::confirmed_target(head, confirmations)
        }
    }
}

async fn required_startup_hash<P: alloy::providers::Provider>(
    provider: &P,
    target: u64,
    cfg: &FollowConfig,
) -> alloy::primitives::B256 {
    retry_startup(
        cfg,
        &format!("reading startup target hash {target}"),
        || async {
            tokio::time::timeout(
                STARTUP_RPC_TIMEOUT,
                crate::chain::block_hash(provider, target),
            )
            .await
            .context("startup block-hash request timed out")??
            .with_context(|| format!("RPC returned no block/hash for startup target {target}"))
        },
    )
    .await
}

async fn retry_startup<T, F, Fut>(cfg: &FollowConfig, label: &str, mut operation: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut failures = 0u64;
    loop {
        match operation().await {
            Ok(value) => return value,
            Err(error) => {
                failures = failures.saturating_add(1);
                let error = crate::redact::urls(&format!("{error:#}"));
                tracing::warn!(
                    failures,
                    retry_in = ?cfg.retry_base,
                    "{label}: {error}"
                );
                tokio::time::sleep(cfg.retry_base).await;
            }
        }
    }
}

fn validate_startup_map(balances: &BalanceMap, target: u64) -> Result<()> {
    anyhow::ensure!(
        balances.cursor == target,
        "startup map cursor is {}, expected {target}",
        balances.cursor
    );
    let mut usdt = 0u128;
    let mut usdc = 0u128;
    for (address, entry) in balances.iter() {
        anyhow::ensure!(!address.is_zero(), "startup map contains the zero address");
        anyhow::ensure!(
            !entry.is_zero(),
            "startup map contains zero/zero row {address}"
        );
        validate_stamp(
            "USDT",
            *address,
            entry.usdt_block,
            crate::tokens::USDT_DEPLOY_BLOCK,
            target,
        )?;
        validate_stamp(
            "USDC",
            *address,
            entry.usdc_block,
            crate::tokens::USDC_DEPLOY_BLOCK,
            target,
        )?;
        usdt = usdt
            .checked_add(entry.usdt)
            .context("startup USDT sum overflow")?;
        usdc = usdc
            .checked_add(entry.usdc)
            .context("startup USDC sum overflow")?;
    }
    Ok(())
}

fn validate_stamp(
    token: &str,
    address: alloy::primitives::Address,
    stamp: u32,
    deploy: u64,
    target: u64,
) -> Result<()> {
    // Stamp 0 is an intentional "unknown since this row entered the current
    // map" value. It can survive a zero/zero removal followed by re-entry.
    if stamp != 0 {
        let stamp = u64::from(stamp);
        anyhow::ensure!(
            (deploy..=target).contains(&stamp),
            "{token} stamp {stamp} for {address} is outside {deploy}..={target}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
    use alloy::rpc::types::eth::Block;
    use alloy::transports::mock::{Asserter, MockTransport};
    use alloy::transports::{TransportError, TransportFut};
    use eth_pir::KeywordCheckpoint;
    use poulpy_pir::keyword::{KeywordDirectory, KeywordIndex};
    use std::sync::{Arc, Mutex};
    use tower::Service;

    #[derive(Clone)]
    struct RecordingTransport {
        inner: MockTransport,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Service<RequestPacket> for RecordingTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RequestPacket) -> Self::Future {
            let mut calls = self.calls.lock().unwrap();
            let requests = request
                .as_single()
                .into_iter()
                .chain(request.as_batch().into_iter().flatten());
            for request in requests {
                calls.push((
                    request.method().to_string(),
                    request
                        .params()
                        .map_or_else(String::new, |params| params.get().to_string()),
                ));
            }
            drop(calls);
            self.inner.call(request)
        }
    }

    fn root(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("usdt-pir-startup-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn loaded(cursor: u64) -> BalanceMap {
        let mut map = BalanceMap::new(cursor);
        map.seed(
            alloy::primitives::Address::repeat_byte(0x31),
            crate::map::Entry {
                usdt: 10,
                usdt_block: (cursor - 1) as u32,
                usdc: 20,
                usdc_block: (cursor - 1) as u32,
            },
        );
        map
    }

    fn loaded_with_future_stamp(cursor: u64) -> BalanceMap {
        let mut map = loaded(cursor);
        map.seed(
            alloy::primitives::Address::repeat_byte(0x31),
            crate::map::Entry {
                usdt: 10,
                usdt_block: (cursor + 2) as u32,
                usdc: 20,
                usdc_block: (cursor - 1) as u32,
            },
        );
        map
    }

    fn config(state: &Path) -> FollowConfig {
        FollowConfig {
            chunk: 10,
            snapshot_path: state.to_path_buf(),
            retry_base: std::time::Duration::ZERO,
            tip: crate::chain::Tip::Confirmations(4),
            ..Default::default()
        }
    }

    fn block(number: u64, hash: B256) -> Block {
        let mut block: Block = Default::default();
        block.header.hash = hash;
        block.header.inner.number = number;
        block
    }

    fn save_keyword_checkpoint(
        base: &Path,
        addresses: &[alloy::primitives::Address],
        capacity: usize,
    ) {
        let keys = addresses
            .iter()
            .map(crate::record::keyword)
            .collect::<Vec<_>>();
        let mphf = KeywordIndex::build(&keys).unwrap();
        let directory = KeywordDirectory::new(mphf, capacity, 0).unwrap();
        let mut blob = Vec::new();
        directory.write_to(&mut blob).unwrap();
        let mut slots = vec![[0u8; 20]; keys.len()];
        for key in &keys {
            slots[directory.index(key)] = *key;
        }
        crate::keyword_store::save_checkpoint(
            &crate::keyword_store::Paths::new(base),
            &KeywordCheckpoint {
                directory: blob,
                version: 0,
                keys: slots,
            },
        )
        .unwrap();
    }

    fn serve_args(
        state: std::path::PathBuf,
        keyword: std::path::PathBuf,
        listen: std::net::SocketAddr,
    ) -> ServeArgs {
        ServeArgs::take(Cmd::Serve {
            rpc: "unused in mocked test".into(),
            state,
            from_block: None,
            chunk: 10,
            snapshot_every: 600,
            confirmations: crate::chain::Tip::Confirmations(4),
            reorg_window: 64,
            poll_interval: 12,
            rebuild_every: 30,
            compact_after: 200_000,
            compact_tail_percent: 100,
            keyword,
            listen: Some(listen),
            web: None,
            batch_window: 0,
            max_batch: 1,
            queue_depth: 1,
            rate_limit: 0,
            rate_burst: 1,
        })
    }

    fn push_successful_attempt(
        asserter: &Asserter,
        cursor: u64,
        initial_hash: B256,
        final_hash: B256,
    ) {
        let target = cursor + 1;
        asserter.push_success(&(target + 4));
        asserter.push_success(&Some(block(target, initial_hash)));
        asserter.push_success(&1u64);
        asserter.push_success(&Vec::<alloy::rpc::types::eth::Log>::new());
        asserter.push_success(&Some(block(target, final_hash)));
    }

    #[tokio::test]
    async fn later_loaded_cursor_requests_exactly_c_plus_one_through_target() {
        const ORIGINAL_BOOTSTRAP_TARGET: u64 = 20_000_000;
        let cursor = ORIGINAL_BOOTSTRAP_TARGET + 100;
        let target = cursor + 2;
        let asserter = Asserter::new();
        asserter.push_success(&Vec::<alloy::rpc::types::eth::Log>::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = RecordingTransport {
            inner: MockTransport::new(asserter.clone()),
            calls: calls.clone(),
        };
        let client = alloy::rpc::client::RpcClient::new(transport, true);
        let provider = ProviderBuilder::new().connect_client(client);
        let mut map = loaded(cursor);

        let stats = crate::follow::sync_startup(&provider, &mut map, target, 10)
            .await
            .unwrap();

        assert_eq!(stats.blocks, 2);
        assert_eq!(map.cursor, target);
        assert!(asserter.read_q().is_empty());
        let calls = calls.lock().unwrap();
        let log_calls = calls
            .iter()
            .filter(|(method, _)| method == "eth_getLogs")
            .collect::<Vec<_>>();
        assert_eq!(log_calls.len(), 1, "recorded calls: {calls:?}");
        let params = &log_calls[0].1;
        assert!(
            params.contains(&format!("\"fromBlock\":\"{:#x}\"", cursor + 1)),
            "request did not begin at C + 1: {params}"
        );
        assert!(
            params.contains(&format!("\"toBlock\":\"{target:#x}\"")),
            "request did not end at the pinned target: {params}"
        );
        assert!(!params.contains(&format!(
            "\"fromBlock\":\"{:#x}\"",
            ORIGINAL_BOOTSTRAP_TARGET + 1
        )));
    }

    #[tokio::test]
    async fn startup_saves_pinned_target_without_total_supply_reads() {
        let root = root("commit");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let asserter = Asserter::new();
        push_successful_attempt(
            &asserter,
            original.cursor,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x11),
        );
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let caught_up = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap();
        assert_eq!(caught_up.cursor, 20_000_001);
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_001);
    }

    #[tokio::test]
    async fn deterministic_validation_failure_leaves_saved_cursor_at_c() {
        let root = root("validation-failure");
        let state = root.join("state.snapshot");
        let original = loaded_with_future_stamp(20_000_000);
        original.save(&state).unwrap();
        let asserter = Asserter::new();
        push_successful_attempt(
            &asserter,
            original.cursor,
            B256::repeat_byte(0x22),
            B256::repeat_byte(0x22),
        );
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let error = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("stamp"));
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_000);
    }

    #[tokio::test]
    async fn failed_startup_reopens_from_disk_c_and_retries_c_plus_one() {
        let root = root("failure-reopen");
        let state = root.join("state.snapshot");
        loaded(20_000_000).save(&state).unwrap();

        let first = Asserter::new();
        push_successful_attempt(
            &first,
            20_000_000,
            B256::repeat_byte(0x23),
            B256::repeat_byte(0x23),
        );
        let provider = ProviderBuilder::new().connect_mocked_client(first.clone());
        let map = loaded_with_future_stamp(20_000_000);
        assert!(
            catch_up_before_publish(
                &provider,
                map,
                &state,
                &config(&state),
                &root.join("keyword"),
            )
            .await
            .is_err()
        );
        assert!(first.read_q().is_empty());
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_000);

        let second = Asserter::new();
        push_successful_attempt(
            &second,
            20_000_000,
            B256::repeat_byte(0x24),
            B256::repeat_byte(0x24),
        );
        let provider = ProviderBuilder::new().connect_mocked_client(second.clone());
        let reopened = BalanceMap::load_strict(&state).unwrap();
        let caught_up = catch_up_before_publish(
            &provider,
            reopened,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap();
        assert!(second.read_q().is_empty());
        assert_eq!(caught_up.cursor, 20_000_001);
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_001);
    }

    #[tokio::test]
    async fn startup_uses_restored_keyword_capacity_before_accepting_state() {
        let root = root("restored-capacity");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let keyword = root.join("keyword");
        save_keyword_checkpoint(
            &keyword,
            &[alloy::primitives::Address::repeat_byte(0x32)],
            1,
        );
        let asserter = Asserter::new();
        asserter.push_success(&(original.cursor + 4));
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = catch_up_before_publish(&provider, original, &state, &config(&state), &keyword)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("restored keyword allocation needs"));
        assert!(asserter.read_q().is_empty());
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_000);
    }

    #[tokio::test]
    async fn production_run_does_not_bind_endpoint_before_catchup_validation() {
        let root = root("endpoint-before-catchup");
        let state = root.join("state.snapshot");
        let original = loaded_with_future_stamp(20_000_000);
        original.save(&state).unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = reservation.local_addr().unwrap();
        drop(reservation);
        let asserter = Asserter::new();
        push_successful_attempt(
            &asserter,
            original.cursor,
            B256::repeat_byte(0x25),
            B256::repeat_byte(0x25),
        );
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let args = serve_args(state.clone(), root.join("keyword"), listen);
        assert!(run_with_provider(&provider, args).await.is_err());
        assert!(asserter.read_q().is_empty());
        let rebound = std::net::TcpListener::bind(listen)
            .expect("endpoint must remain unbound when startup validation fails");
        drop(rebound);
        assert_eq!(
            BalanceMap::load_strict(&state).unwrap().cursor,
            original.cursor
        );
    }

    #[tokio::test]
    async fn endpoint_is_not_bound_when_initial_pir_construction_fails() {
        let root = root("endpoint-before-pir");
        let state = root.join("state.snapshot");
        let keyword = root.join("keyword");
        let paths = crate::keyword_store::Paths::new(&keyword);
        std::fs::write(&paths.index, b"corrupt keyword checkpoint").unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = reservation.local_addr().unwrap();
        drop(reservation);
        let serving = serve_args(state.clone(), keyword, listen).serving;
        let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
        let error = run_pir_loop(
            &provider,
            loaded(20_000_000),
            config(&state),
            crate::progress::handle(),
            serving,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("keyword"));
        let rebound = std::net::TcpListener::bind(listen)
            .expect("endpoint must remain unbound until PIR construction succeeds");
        drop(rebound);
    }

    #[tokio::test]
    async fn startup_refuses_a_target_behind_c_before_hash_or_preflight() {
        let root = root("behind");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let asserter = Asserter::new();
        asserter.push_success(&(original.cursor + 3)); // head - 4 == C - 1
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("behind imported snapshot"));
        assert!(
            asserter.read_q().is_empty(),
            "no hash/preflight response was consumed"
        );
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_000);
    }

    #[tokio::test]
    async fn changed_hash_discards_the_attempt_and_repins() {
        let root = root("hash-change");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let asserter = Asserter::new();
        push_successful_attempt(
            &asserter,
            original.cursor,
            B256::repeat_byte(0x33),
            B256::repeat_byte(0x34),
        );
        // The next attempt resolves S == C: no hash or log request, but chain,
        // map and capacity validation still run.
        asserter.push_success(&(original.cursor + 4));
        asserter.push_success(&1u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let caught_up = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap();
        assert_eq!(caught_up.cursor, 20_000_000);
        assert_eq!(BalanceMap::load_strict(&state).unwrap().cursor, 20_000_000);
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn remove_then_reenter_lifecycle_produces_an_accepted_unknown_token_stamp() {
        let target = 20_000_000;
        let mut map = BalanceMap::new(target);
        let address = alloy::primitives::Address::repeat_byte(0x71);
        assert_eq!(
            map.apply(
                address,
                crate::map::Reading { usdt: 10, usdc: 0 },
                crate::chain::Touch {
                    usdt: Some(target - 3),
                    usdc: None,
                },
            ),
            crate::map::Applied::Inserted
        );
        assert!(matches!(
            map.apply(
                address,
                crate::map::Reading::default(),
                crate::chain::Touch {
                    usdt: Some(target - 2),
                    usdc: None,
                },
            ),
            crate::map::Applied::Removed(_)
        ));
        assert_eq!(
            map.apply(
                address,
                crate::map::Reading { usdt: 10, usdc: 20 },
                crate::chain::Touch {
                    usdt: None,
                    usdc: Some(target - 1),
                },
            ),
            crate::map::Applied::Inserted
        );
        assert_eq!(
            map.get(&address).unwrap(),
            crate::map::Entry {
                usdt: 10,
                usdt_block: 0,
                usdc: 20,
                usdc_block: (target - 1) as u32,
            },
        );
        validate_startup_map(&map, target).unwrap();
    }

    #[tokio::test]
    async fn a_successful_wrong_chain_response_is_a_startup_error() {
        let root = root("wrong-chain");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let asserter = Asserter::new();
        asserter.push_success(&(original.cursor + 4));
        asserter.push_success(&2u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("not Ethereum mainnet"));
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn startup_keeps_s_pinned_across_catchup_and_final_hash_retries() {
        let root = root("pinned-retries");
        let state = root.join("state.snapshot");
        let original = loaded(20_000_000);
        original.save(&state).unwrap();
        let target = original.cursor + 1;
        let hash = B256::repeat_byte(0x72);
        let asserter = Asserter::new();
        asserter.push_success(&(target + 4));
        asserter.push_success(&Some(block(target, hash)));
        asserter.push_success(&1u64);
        asserter.push_failure_msg("temporary startup log outage");
        asserter.push_success(&Vec::<alloy::rpc::types::eth::Log>::new());
        asserter.push_success(&Option::<Block>::None);
        asserter.push_success(&Some(block(target, hash)));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let caught_up = catch_up_before_publish(
            &provider,
            original,
            &state,
            &config(&state),
            &root.join("keyword"),
        )
        .await
        .unwrap();
        assert_eq!(caught_up.cursor, target);
        assert!(
            asserter.read_q().is_empty(),
            "S was unexpectedly re-resolved"
        );
    }

    #[tokio::test]
    async fn authoritative_serve_refuses_checksumless_state() {
        let root = root("legacy-state");
        let state = root.join("state.snapshot");
        loaded(20_000_000).save(&state).unwrap();
        let mut bytes = std::fs::read(&state).unwrap();
        bytes[..8].copy_from_slice(b"USDTPIR2");
        bytes.truncate(bytes.len() - 8);
        std::fs::write(&state, bytes).unwrap();
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let error = open_serving_map(&provider, &state, None).await.unwrap_err();
        assert!(format!("{error:#}").contains("strict checksummed USDTPIR3"));
        assert!(asserter.read_q().is_empty());
    }
}
