use std::collections::HashMap;

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

pub async fn usdt_owner<P: Provider>(provider: &P, block: u64) -> Result<Address> {
    IERC20Owned::new(USDT, provider)
        .owner()
        .block(BlockId::number(block))
        .call()
        .await
        .context("reading USDT owner() to apply an Issue/Redeem")
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
