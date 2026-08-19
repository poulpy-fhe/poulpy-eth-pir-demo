use std::collections::HashMap;
use std::time::Duration;

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};

use crate::map::Reading;
use crate::tokens::{MAINNET_CHAIN_ID, MULTICALL3, MULTICALL3_DEPLOY_BLOCK, TOKENS, USDT};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC20Owned {
        function owner() external view returns (address);
    }

    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function aggregate3(Call3[] calldata calls) external payable
            returns (Result[] memory returnData);
    }
}

const CALLS_PER_MULTICALL: usize = 800;
const STRICT_RPC_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TotalSupplies {
    pub usdt: u128,
    pub usdc: u128,
}

pub async fn usdt_owner<P: Provider>(provider: &P, block: u64) -> Result<Address> {
    IERC20Owned::new(USDT, provider)
        .owner()
        .block(BlockId::number(block))
        .call()
        .await
        .context("reading USDT owner() to apply an Issue/Redeem")
}

pub async fn usdt_owner_strict<P: Provider>(provider: &P, block: u64) -> Result<Address> {
    tokio::time::timeout(STRICT_RPC_TIMEOUT, usdt_owner(provider, block))
        .await
        .with_context(|| format!("USDT owner() timed out after {STRICT_RPC_TIMEOUT:?}"))?
}

pub async fn read_balances<P: Provider>(
    provider: &P,
    addrs: &[Address],
    block: u64,
) -> Result<HashMap<Address, Reading>> {
    let mut out = HashMap::with_capacity(addrs.len());
    if addrs.is_empty() {
        return Ok(out);
    }
    let multicall = IMulticall3::new(MULTICALL3, provider);
    for chunk in addrs.chunks(addresses_per_multicall()) {
        let results = call_balance_chunk(&multicall, chunk, block).await?;
        insert_balance_results(&mut out, chunk, results)?;
    }
    Ok(out)
}

/// Strict, adaptively split balance reads for bootstrap and the initial serve
/// catch-up. No failed or malformed subcall is ever interpreted as zero.
pub async fn read_balances_strict<P: Provider>(
    provider: &P,
    addrs: &[Address],
    block: u64,
) -> Result<HashMap<Address, Reading>> {
    let mut out = HashMap::with_capacity(addrs.len());
    for initial in addrs.chunks(addresses_per_multicall()) {
        let mut pending = vec![initial.to_vec()];
        while let Some(batch) = pending.pop() {
            match read_balance_batch_strict(provider, &batch, block).await {
                Ok(values) => out.extend(values),
                Err(error) if batch.len() > 1 && balance_error_is_splittable(&error) => {
                    let middle = batch.len() / 2;
                    tracing::warn!(
                        addresses = batch.len(),
                        block,
                        "strict Multicall batch failed; splitting: {}",
                        crate::redact::urls(&format!("{error:#}"))
                    );
                    pending.push(batch[middle..].to_vec());
                    pending.push(batch[..middle].to_vec());
                }
                Err(error) => {
                    let address = batch[0];
                    return Err(error.context(format!(
                        "strict USDT/USDC balance read for {address} at block {block}"
                    )));
                }
            }
        }
    }
    Ok(out)
}

fn balance_error_is_splittable(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "request size",
        "response size",
        "too large",
        "limit exceeded",
        "execution gas",
        "out of gas",
        "gas required",
        "gas limit",
        "gas too low",
        "exceeds allowance",
        "multicall returned",
        "subcall failed",
        "bytes, expected 32",
        "exceeds u128",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

async fn read_balance_batch_strict<P: Provider>(
    provider: &P,
    addrs: &[Address],
    block: u64,
) -> Result<HashMap<Address, Reading>> {
    let multicall = IMulticall3::new(MULTICALL3, provider);
    let calls = balance_calls(addrs);
    let results = tokio::time::timeout(
        STRICT_RPC_TIMEOUT,
        multicall
            .aggregate3(calls)
            .block(BlockId::number(block))
            .call(),
    )
    .await
    .with_context(|| format!("Multicall timed out after {STRICT_RPC_TIMEOUT:?}"))?
    .with_context(|| {
        format!(
            "aggregate3 of {} calls at block {block}",
            addrs.len() * TOKENS.len()
        )
    })?;
    ensure_result_count(addrs, &results)?;

    let mut out = HashMap::with_capacity(addrs.len());
    for (address, pair) in addrs.iter().zip(results.chunks_exact(TOKENS.len())) {
        out.insert(
            *address,
            Reading {
                usdt: decode_balance_strict(&pair[0], "USDT", *address)?,
                usdc: decode_balance_strict(&pair[1], "USDC", *address)?,
            },
        );
    }
    Ok(out)
}

fn decode_balance_strict(
    result: &IMulticall3::Result,
    token: &str,
    address: Address,
) -> Result<u128> {
    anyhow::ensure!(
        result.success,
        "{token}.balanceOf({address}) subcall failed"
    );
    anyhow::ensure!(
        result.returnData.len() == 32,
        "{token}.balanceOf({address}) returned {} bytes, expected 32",
        result.returnData.len()
    );
    let value = U256::from_be_slice(&result.returnData);
    u128::try_from(value).with_context(|| format!("{token}.balanceOf({address}) exceeds u128"))
}

pub async fn read_total_supplies<P: Provider>(provider: &P, block: u64) -> Result<TotalSupplies> {
    let usdt = read_total_supply(provider, USDT, "USDT", block).await?;
    let usdc = read_total_supply(provider, crate::tokens::USDC, "USDC", block).await?;
    Ok(TotalSupplies { usdt, usdc })
}

async fn read_total_supply<P: Provider>(
    provider: &P,
    token: Address,
    name: &str,
    block: u64,
) -> Result<u128> {
    let contract = IERC20::new(token, provider);
    let value = tokio::time::timeout(
        STRICT_RPC_TIMEOUT,
        contract.totalSupply().block(BlockId::number(block)).call(),
    )
    .await
    .with_context(|| format!("{name}.totalSupply timed out after {STRICT_RPC_TIMEOUT:?}"))?
    .with_context(|| format!("reading {name}.totalSupply at block {block}"))?;
    u128::try_from(value)
        .with_context(|| format!("{name}.totalSupply at block {block} exceeds u128"))
}

fn addresses_per_multicall() -> usize {
    CALLS_PER_MULTICALL / TOKENS.len()
}

async fn call_balance_chunk<P: Provider>(
    multicall: &IMulticall3::IMulticall3Instance<&P>,
    chunk: &[Address],
    block: u64,
) -> Result<Vec<IMulticall3::Result>> {
    let calls = balance_calls(chunk);
    multicall
        .aggregate3(calls)
        .block(BlockId::number(block))
        .call()
        .await
        .with_context(|| format!("aggregate3 of {} calls at block {block}", chunk.len() * 2))
}

fn balance_calls(chunk: &[Address]) -> Vec<IMulticall3::Call3> {
    chunk
        .iter()
        .flat_map(|&owner| {
            let data = Bytes::from(IERC20::balanceOfCall { owner }.abi_encode());
            TOKENS.map(|target| IMulticall3::Call3 {
                target,
                allowFailure: true,
                callData: data.clone(),
            })
        })
        .collect()
}

fn insert_balance_results(
    out: &mut HashMap<Address, Reading>,
    chunk: &[Address],
    results: Vec<IMulticall3::Result>,
) -> Result<()> {
    ensure_result_count(chunk, &results)?;
    for (addr, pair) in chunk.iter().zip(results.chunks_exact(TOKENS.len())) {
        out.insert(
            *addr,
            Reading {
                usdt: decode_balance(&pair[0])?,
                usdc: decode_balance(&pair[1])?,
            },
        );
    }
    Ok(())
}

fn ensure_result_count(chunk: &[Address], results: &[IMulticall3::Result]) -> Result<()> {
    anyhow::ensure!(
        results.len() == chunk.len() * TOKENS.len(),
        "multicall returned {} results for {} calls",
        results.len(),
        chunk.len() * TOKENS.len()
    );
    Ok(())
}

fn decode_balance(r: &IMulticall3::Result) -> Result<u128> {
    if !r.success {
        tracing::warn!("balanceOf reverted; treating as zero");
        return Ok(0);
    }
    anyhow::ensure!(
        r.returnData.len() == 32,
        "balanceOf returned {} bytes, expected 32",
        r.returnData.len()
    );
    let v = U256::from_be_slice(&r.returnData);
    u128::try_from(v).context("balance exceeds u128; record layout assumption is broken")
}

#[cfg(test)]
pub(crate) fn encode_balance_response_for_test(readings: &[Reading]) -> Bytes {
    let results = readings
        .iter()
        .flat_map(|reading| [reading.usdt, reading.usdc])
        .map(|value| {
            let mut word = [0u8; 32];
            word[16..].copy_from_slice(&value.to_be_bytes());
            IMulticall3::Result {
                success: true,
                returnData: Bytes::copy_from_slice(&word),
            }
        })
        .collect::<Vec<_>>();
    Bytes::from(IMulticall3::aggregate3Call::abi_encode_returns(&results))
}

pub async fn ensure_mainnet<P: Provider>(provider: &P) -> Result<()> {
    let id = provider
        .get_chain_id()
        .await
        .context("reading eth_chainId")?;
    anyhow::ensure!(
        id == MAINNET_CHAIN_ID,
        "RPC reports chain id {id}, not Ethereum mainnet ({MAINNET_CHAIN_ID})."
    );
    Ok(())
}

pub fn ensure_multicall_at(block: u64) -> Result<()> {
    if block >= MULTICALL3_DEPLOY_BLOCK {
        return Ok(());
    }
    anyhow::bail!(
        "block {block} predates Multicall3's mainnet deployment at {MULTICALL3_DEPLOY_BLOCK}; \
         bootstrap from a snapshot instead, then follow forward."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;

    fn returned(value: u128) -> IMulticall3::Result {
        let mut word = [0u8; 32];
        word[16..].copy_from_slice(&value.to_be_bytes());
        IMulticall3::Result {
            success: true,
            returnData: Bytes::copy_from_slice(&word),
        }
    }

    fn encoded_results(results: Vec<IMulticall3::Result>) -> Bytes {
        Bytes::from(IMulticall3::aggregate3Call::abi_encode_returns(&results))
    }

    #[test]
    fn strict_decoder_rejects_failed_malformed_and_oversized_values() {
        let address = Address::repeat_byte(0x55);
        let failed = IMulticall3::Result {
            success: false,
            returnData: Bytes::new(),
        };
        assert!(decode_balance_strict(&failed, "USDT", address).is_err());
        let malformed = IMulticall3::Result {
            success: true,
            returnData: Bytes::from(vec![0u8; 31]),
        };
        assert!(decode_balance_strict(&malformed, "USDT", address).is_err());
        let mut oversized = [0u8; 32];
        oversized[0] = 1;
        let oversized = IMulticall3::Result {
            success: true,
            returnData: Bytes::copy_from_slice(&oversized),
        };
        assert!(decode_balance_strict(&oversized, "USDT", address).is_err());
    }

    #[test]
    fn only_size_execution_and_decode_failures_trigger_batch_splitting() {
        assert!(balance_error_is_splittable(&anyhow::anyhow!(
            "response size too large"
        )));
        assert!(balance_error_is_splittable(&anyhow::anyhow!(
            "USDT subcall failed"
        )));
        assert!(balance_error_is_splittable(&anyhow::anyhow!(
            "gas required exceeds allowance"
        )));
        assert!(!balance_error_is_splittable(&anyhow::anyhow!(
            "429 rate limited"
        )));
    }

    #[tokio::test]
    async fn strict_reader_splits_a_provider_limited_batch_and_reassembles_it() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("response size too large");
        asserter.push_success(&encoded_results(vec![returned(1), returned(2)]));
        asserter.push_success(&encoded_results(vec![returned(3), returned(4)]));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let first = Address::repeat_byte(0x61);
        let second = Address::repeat_byte(0x62);
        let readings = read_balances_strict(&provider, &[first, second], 20_000_000)
            .await
            .unwrap();
        assert_eq!(readings[&first], Reading { usdt: 1, usdc: 2 });
        assert_eq!(readings[&second], Reading { usdt: 3, usdc: 4 });
    }

    #[tokio::test]
    async fn one_address_failure_names_token_and_address() {
        let asserter = Asserter::new();
        asserter.push_success(&encoded_results(vec![
            IMulticall3::Result {
                success: false,
                returnData: Bytes::new(),
            },
            returned(2),
        ]));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let address = Address::repeat_byte(0x71);
        let error = read_balances_strict(&provider, &[address], 20_000_000)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("USDT.balanceOf"), "{message}");
        assert!(message.contains(&address.to_string()), "{message}");
    }
}
