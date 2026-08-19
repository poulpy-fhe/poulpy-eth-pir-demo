use std::collections::HashMap;
use std::ops::RangeInclusive;

use alloy::primitives::Address;
use alloy::rpc::types::eth::{Filter, Log};

use crate::tokens::{
    DESTROYED_BLACK_FUNDS, SUPPLY_TOPICS, TOKENS, TRANSFER, USDC, USDT, WATCHED_TOPICS,
};

/// An address topic is a 20-byte address left-padded into a 32-byte word.
pub(super) fn addr_from_word(word: &[u8]) -> Option<Address> {
    if word.len() != 32 || word[..12] != [0u8; 12] {
        return None;
    }
    let a = Address::from_slice(&word[12..]);
    (!a.is_zero()).then_some(a)
}

/// The log filter for one block range.
pub fn filter(from: u64, to: u64) -> Filter {
    Filter::new()
        .address(TOKENS.to_vec())
        .event_signature(WATCHED_TOPICS.to_vec())
        .from_block(from)
        .to_block(to)
}

/// The last block, per token, in which an address appeared in a log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    pub usdt: Option<u64>,
    pub usdc: Option<u64>,
}

impl Touch {
    pub fn see(&mut self, token: Address, block: Option<u64>) {
        let slot = if token == USDT {
            &mut self.usdt
        } else if token == USDC {
            &mut self.usdc
        } else {
            return;
        };
        *slot = max_block(*slot, block);
    }
}

fn max_block(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// What one pass of [`collect_touched`] learned beyond the address set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub understood: usize,
    pub usdt_supply_blocks: Vec<u64>,
}

/// Project logs onto the addresses whose balance may have moved.
pub fn collect_touched(logs: &[Log], out: &mut HashMap<Address, Touch>) -> Collected {
    let mut collected = Collected::default();
    for log in logs {
        collect_log(log, out, &mut collected);
    }
    collected.usdt_supply_blocks.sort_unstable();
    collected.usdt_supply_blocks.dedup();
    collected
}

/// Strict projection used while assembling authoritative state. A response
/// that matched our RPC filter but cannot be proved to be a well-formed event
/// fails the whole range, so its durable cursor cannot move past it.
pub fn collect_touched_strict(
    logs: &[Log],
    range: RangeInclusive<u64>,
    out: &mut HashMap<Address, Touch>,
) -> anyhow::Result<Collected> {
    let mut collected = Collected::default();
    let mut pending = HashMap::new();
    for (index, log) in logs.iter().enumerate() {
        collect_log_strict(log, range.clone(), &mut pending, &mut collected)
            .map_err(|e| anyhow::anyhow!("matched log {index} is invalid: {e}"))?;
    }
    for (address, touch) in pending {
        let entry = out.entry(address).or_default();
        entry.see(USDT, touch.usdt);
        entry.see(USDC, touch.usdc);
    }
    collected.usdt_supply_blocks.sort_unstable();
    collected.usdt_supply_blocks.dedup();
    Ok(collected)
}

fn collect_log_strict(
    log: &Log,
    range: RangeInclusive<u64>,
    out: &mut HashMap<Address, Touch>,
    collected: &mut Collected,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        TOKENS.contains(&log.address()),
        "unexpected token {}",
        log.address()
    );
    anyhow::ensure!(!log.removed, "log is marked removed");
    let block = log
        .block_number
        .ok_or_else(|| anyhow::anyhow!("missing block number"))?;
    anyhow::ensure!(range.contains(&block), "block {block} is outside {range:?}");
    anyhow::ensure!(
        log.block_hash.is_some(),
        "missing block hash at block {block}"
    );

    let topics = log.topics();
    let topic0 = topics
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing event signature topic"))?;
    anyhow::ensure!(
        WATCHED_TOPICS.contains(topic0),
        "unexpected event signature {topic0}"
    );
    match *topic0 {
        TRANSFER => collect_transfer_strict(log, block, out)?,
        DESTROYED_BLACK_FUNDS => collect_destroyed_strict(log, block, out)?,
        topic if SUPPLY_TOPICS.contains(&topic) => collect_supply_strict(log, block, collected)?,
        _ => unreachable!("WATCHED_TOPICS is exhaustive"),
    }
    collected.understood += 1;
    Ok(())
}

fn collect_transfer_strict(
    log: &Log,
    block: u64,
    out: &mut HashMap<Address, Touch>,
) -> anyhow::Result<()> {
    let topics = log.topics();
    anyhow::ensure!(
        topics.len() == 3,
        "Transfer has {} topics, expected 3",
        topics.len()
    );
    anyhow::ensure!(
        log.data().data.len() == 32,
        "Transfer has {} data bytes, expected 32",
        log.data().data.len()
    );
    let from = strict_addr_word(topics[1].as_slice())?;
    let to = strict_addr_word(topics[2].as_slice())?;
    if log.data().data.iter().all(|byte| *byte == 0) || from == to {
        return Ok(());
    }
    for address in [from, to] {
        if !address.is_zero() {
            out.entry(address)
                .or_default()
                .see(log.address(), Some(block));
        }
    }
    Ok(())
}

fn collect_destroyed_strict(
    log: &Log,
    block: u64,
    out: &mut HashMap<Address, Touch>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        log.address() == USDT,
        "DestroyedBlackFunds is only supported for USDT"
    );
    let topics = log.topics();
    anyhow::ensure!(
        topics.len() == 1,
        "DestroyedBlackFunds has {} topics, expected 1",
        topics.len()
    );
    let data = &log.data().data;
    anyhow::ensure!(
        data.len() == 64,
        "DestroyedBlackFunds has {} data bytes, expected 64",
        data.len()
    );
    // USDT declares both arguments as non-indexed. The first data word is the
    // destroyed account and the complete second word is its former balance.
    let address = strict_addr_word(&data[..32])?;
    if !address.is_zero() {
        out.entry(address).or_default().see(USDT, Some(block));
    }
    Ok(())
}

fn collect_supply_strict(log: &Log, block: u64, collected: &mut Collected) -> anyhow::Result<()> {
    anyhow::ensure!(
        log.address() == USDT,
        "Issue/Redeem is only supported for USDT"
    );
    anyhow::ensure!(
        log.topics().len() == 1,
        "Issue/Redeem has {} topics, expected 1",
        log.topics().len()
    );
    anyhow::ensure!(
        log.data().data.len() == 32,
        "Issue/Redeem has {} data bytes, expected 32",
        log.data().data.len()
    );
    collected.usdt_supply_blocks.push(block);
    Ok(())
}

fn strict_addr_word(word: &[u8]) -> anyhow::Result<Address> {
    anyhow::ensure!(word.len() == 32, "address topic is not 32 bytes");
    anyhow::ensure!(word[..12] == [0u8; 12], "address topic is not left padded");
    Ok(Address::from_slice(&word[12..]))
}

fn collect_log(log: &Log, out: &mut HashMap<Address, Touch>, collected: &mut Collected) {
    let topics = log.topics();
    let Some(&topic0) = topics.first() else {
        return;
    };
    if topic0 == TRANSFER {
        collect_transfer(log, out, collected);
    } else if topic0 == DESTROYED_BLACK_FUNDS {
        collect_destroyed_black_funds(log, out, collected);
    } else if SUPPLY_TOPICS.contains(&topic0) && log.address() == USDT {
        collect_supply_event(log, collected);
    }
}

fn collect_transfer(log: &Log, out: &mut HashMap<Address, Touch>, collected: &mut Collected) {
    let topics = log.topics();
    if topics.len() != 3 {
        return;
    }
    if transfer_is_inert(log) {
        collected.understood += 1;
        return;
    }
    for t in &topics[1..3] {
        if let Some(a) = addr_from_word(t.as_slice()) {
            out.entry(a)
                .or_default()
                .see(log.address(), log.block_number);
        }
    }
    collected.understood += 1;
}

fn transfer_is_inert(log: &Log) -> bool {
    let topics = log.topics();
    let is_zero_value = log.data().data.iter().all(|&b| b == 0);
    is_zero_value || topics[1] == topics[2]
}

fn collect_destroyed_black_funds(
    log: &Log,
    out: &mut HashMap<Address, Touch>,
    collected: &mut Collected,
) {
    let word = log
        .topics()
        .get(1)
        .map(|t| t.as_slice())
        .or_else(|| log.data().data.get(..32));
    if let Some(a) = word.and_then(addr_from_word) {
        out.entry(a)
            .or_default()
            .see(log.address(), log.block_number);
        collected.understood += 1;
    }
}

fn collect_supply_event(log: &Log, collected: &mut Collected) {
    if let Some(block) = log.block_number {
        collected.usdt_supply_blocks.push(block);
    }
    collected.understood += 1;
}
