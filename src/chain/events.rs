use std::collections::HashMap;

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
