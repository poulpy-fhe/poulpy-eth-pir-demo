use std::collections::{HashMap, HashSet};

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::eth::Log;
use anyhow::Result;

use crate::chain::Touch;
use crate::follow::config::BatchStats;
use crate::map::{Applied, BalanceMap, Entry};

pub async fn apply_range<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    logs: &[Log],
    at: u64,
    inserted: &mut HashSet<Address>,
    touched_out: &mut Vec<Address>,
    changed_out: &mut HashMap<Address, Entry>,
) -> Result<BatchStats> {
    let mut stats = BatchStats {
        logs: logs.len(),
        ..Default::default()
    };
    let touched = collect_addresses(provider, logs).await?;
    stats.touched = touched.len();
    if touched.is_empty() {
        return Ok(stats);
    }
    let mut out = ApplyOutputs {
        inserted,
        touched_out,
        changed_out,
        stats: &mut stats,
    };
    apply_touched(provider, map, at, &touched, &mut out).await
}

struct ApplyOutputs<'a> {
    inserted: &'a mut HashSet<Address>,
    touched_out: &'a mut Vec<Address>,
    changed_out: &'a mut HashMap<Address, Entry>,
    stats: &'a mut BatchStats,
}

async fn collect_addresses<P: Provider>(
    provider: &P,
    logs: &[Log],
) -> Result<HashMap<Address, Touch>> {
    let mut touched = HashMap::new();
    let collected = crate::chain::collect_touched(logs, &mut touched);
    add_usdt_owner_touches(provider, &mut touched, &collected.usdt_supply_blocks).await?;
    Ok(touched)
}

async fn add_usdt_owner_touches<P: Provider>(
    provider: &P,
    touched: &mut HashMap<Address, Touch>,
    blocks: &[u64],
) -> Result<()> {
    for &block in blocks {
        let owner = crate::chain::usdt_owner(provider, block).await?;
        tracing::info!(%owner, block, "USDT supply event; refreshing the treasury balance");
        touched
            .entry(owner)
            .or_default()
            .see(crate::tokens::USDT, Some(block));
    }
    Ok(())
}

async fn apply_touched<P: Provider>(
    provider: &P,
    map: &mut BalanceMap,
    at: u64,
    touched: &HashMap<Address, Touch>,
    out: &mut ApplyOutputs<'_>,
) -> Result<BatchStats> {
    let addrs: Vec<Address> = touched.keys().copied().collect();
    out.touched_out.extend_from_slice(&addrs);
    for (addr, reading) in crate::chain::read_balances(provider, &addrs, at).await? {
        let seen = touched.get(&addr).copied().unwrap_or_default();
        let applied = map.apply(addr, reading, seen);
        record_apply_result(applied, addr, out);
        record_current_entry(map, addr, out.changed_out);
    }
    Ok(*out.stats)
}

fn record_current_entry(map: &BalanceMap, addr: Address, changed: &mut HashMap<Address, Entry>) {
    if let Some(entry) = map.get(&addr)
        && changed.contains_key(&addr)
    {
        changed.insert(addr, entry);
    }
}

fn record_apply_result(applied: Applied, addr: Address, out: &mut ApplyOutputs<'_>) {
    match applied {
        Applied::Inserted => record_inserted(addr, out),
        Applied::Updated => record_updated(addr, out),
        Applied::Removed(entry) => record_removed(addr, entry, out),
        Applied::Unchanged => out.stats.unchanged += 1,
        Applied::SkippedNewZero => out.stats.skipped_new_zero += 1,
    }
}

fn record_inserted(addr: Address, out: &mut ApplyOutputs<'_>) {
    out.stats.inserted += 1;
    out.inserted.insert(addr);
    out.changed_out.insert(addr, Entry::default());
}

fn record_updated(addr: Address, out: &mut ApplyOutputs<'_>) {
    out.stats.updated += 1;
    out.changed_out.insert(addr, Entry::default());
}

fn record_removed(addr: Address, entry: Entry, out: &mut ApplyOutputs<'_>) {
    out.stats.removed += 1;
    if out.inserted.remove(&addr) {
        out.changed_out.remove(&addr);
    } else {
        out.changed_out.insert(addr, entry);
    }
}
