use std::collections::HashMap;

use alloy::primitives::Address;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub usdt: u128,
    pub usdc: u128,
}

impl Reading {
    pub fn is_zero(&self) -> bool {
        self.usdt == 0 && self.usdc == 0
    }
}

pub use usdt_pir_record::Entry;

#[derive(Clone, Debug, Default)]
pub struct BalanceMap {
    pub(super) inner: HashMap<Address, Entry>,
    pub cursor: u64,
}

impl BalanceMap {
    pub fn new(cursor: u64) -> Self {
        Self {
            inner: HashMap::new(),
            cursor,
        }
    }

    pub fn get(&self, addr: &Address) -> Option<Entry> {
        self.inner.get(addr).copied()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Address, &Entry)> {
        self.inner.iter()
    }

    pub fn semantically_eq(&self, other: &Self) -> bool {
        self.cursor == other.cursor && self.inner == other.inner
    }
}
