use std::collections::{HashSet, VecDeque};

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use anyhow::Result;

use crate::chain::Touch;
use crate::map::BalanceMap;

#[derive(Debug, Default)]
pub struct ChainWatch {
    pub(super) cursor_hash: Option<B256>,
    recent: VecDeque<(u64, Vec<Address>)>,
}

impl ChainWatch {
    pub fn record(&mut self, through: u64, addrs: Vec<Address>) {
        if !addrs.is_empty() {
            self.recent.push_back((through, addrs));
        }
    }

    pub fn trim(&mut self, cursor: u64, window: u64) {
        let keep_from = cursor.saturating_sub(window);
        while self.recent.front().is_some_and(|&(t, _)| t < keep_from) {
            self.recent.pop_front();
        }
    }

    pub fn addresses(&self) -> Vec<Address> {
        let mut seen = HashSet::new();
        for (_, addrs) in &self.recent {
            seen.extend(addrs.iter().copied());
        }
        seen.into_iter().collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    pub async fn repair_if_reorged<P: Provider>(
        &mut self,
        provider: &P,
        map: &mut BalanceMap,
        window: u64,
    ) -> Result<()> {
        let Some(known) = self.cursor_hash else {
            return Ok(());
        };
        if crate::chain::block_hash(provider, map.cursor).await? == Some(known) {
            return Ok(());
        }
        self.repair(provider, map, window).await
    }

    async fn repair<P: Provider>(
        &mut self,
        provider: &P,
        map: &mut BalanceMap,
        window: u64,
    ) -> Result<()> {
        let rewind_to = map.cursor.saturating_sub(window);
        let stale = self.addresses();
        self.log_reorg(map.cursor, rewind_to, stale.len());
        self.correct_stale_addresses(provider, map, &stale, rewind_to)
            .await?;
        map.cursor = rewind_to;
        self.cursor_hash = None;
        self.recent.clear();
        Ok(())
    }

    fn log_reorg(&self, cursor: u64, rewind_to: u64, addresses: usize) {
        tracing::warn!(
            cursor,
            rewind_to,
            addresses,
            "reorg detected: block {cursor} no longer hashes to what was folded in"
        );
    }

    async fn correct_stale_addresses<P: Provider>(
        &self,
        provider: &P,
        map: &mut BalanceMap,
        stale: &[Address],
        rewind_to: u64,
    ) -> Result<()> {
        if stale.is_empty() {
            return Ok(());
        }
        for (addr, reading) in crate::chain::read_balances(provider, stale, rewind_to).await? {
            map.apply(addr, reading, Touch::default());
        }
        Ok(())
    }

    pub async fn store_cursor_hash<P: Provider>(
        &mut self,
        provider: &P,
        cursor: u64,
    ) -> Result<()> {
        self.cursor_hash = crate::chain::block_hash(provider, cursor).await?;
        Ok(())
    }
}
