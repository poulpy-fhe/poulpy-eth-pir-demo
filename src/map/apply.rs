use std::collections::HashSet;

use alloy::primitives::Address;

use super::types::{BalanceMap, Entry, Reading};
use crate::chain::Touch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Inserted,
    Updated,
    Removed(Entry),
    Unchanged,
    SkippedNewZero,
}

impl BalanceMap {
    pub fn apply(&mut self, addr: Address, r: Reading, seen: Touch) -> Applied {
        match self.inner.get_mut(&addr) {
            Some(e) => {
                let applied = apply_existing(e, r, seen);
                let removed = e.is_zero().then_some(*e);
                if let Some(removed) = removed {
                    self.inner.remove(&addr);
                    Applied::Removed(removed)
                } else {
                    applied
                }
            }
            None if r.is_zero() => Applied::SkippedNewZero,
            None => self.insert_new(addr, r, seen),
        }
    }

    fn insert_new(&mut self, addr: Address, r: Reading, seen: Touch) -> Applied {
        self.inner.insert(
            addr,
            Entry {
                usdt: r.usdt,
                usdt_block: seen.usdt.map(block32).unwrap_or(0),
                usdc: r.usdc,
                usdc_block: seen.usdc.map(block32).unwrap_or(0),
            },
        );
        Applied::Inserted
    }

    pub fn prune_zero(&mut self, candidates: &HashSet<Address>) -> usize {
        let before = self.inner.len();
        for addr in candidates {
            self.remove_if_zero(addr);
        }
        before - self.inner.len()
    }

    fn remove_if_zero(&mut self, addr: &Address) {
        if self.inner.get(addr).is_some_and(Entry::is_zero) {
            self.inner.remove(addr);
        }
    }

    #[allow(dead_code)]
    pub fn seed(&mut self, addr: Address, e: Entry) {
        self.inner.insert(addr, e);
    }
}

fn apply_existing(e: &mut Entry, r: Reading, seen: Touch) -> Applied {
    let changed = stamp_seen_blocks(e, seen) | write_balances(e, r);
    if changed {
        Applied::Updated
    } else {
        Applied::Unchanged
    }
}

fn stamp_seen_blocks(e: &mut Entry, seen: Touch) -> bool {
    update_block(&mut e.usdt_block, seen.usdt) | update_block(&mut e.usdc_block, seen.usdc)
}

fn update_block(slot: &mut u32, block: Option<u64>) -> bool {
    let Some(block) = block.map(block32) else {
        return false;
    };
    if *slot == block {
        return false;
    }
    *slot = block;
    true
}

fn write_balances(e: &mut Entry, r: Reading) -> bool {
    update_balance(&mut e.usdt, r.usdt) | update_balance(&mut e.usdc, r.usdc)
}

fn update_balance(slot: &mut u128, value: u128) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

fn block32(b: u64) -> u32 {
    u32::try_from(b).expect("block number outgrew u32 after ~1,630 years")
}
