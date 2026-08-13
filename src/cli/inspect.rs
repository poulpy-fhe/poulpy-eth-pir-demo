use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::{Context, Result};

use crate::map::{BalanceMap, Entry};

pub fn lookup(address: Address, state: PathBuf) -> Result<()> {
    let balances = BalanceMap::load(&state).context(format!("reading {state:?}"))?;
    match balances.get(&address) {
        Some(e) => print_lookup(address, balances.cursor, e),
        None => println!("{address} not in the map (as of block {})", balances.cursor),
    }
    Ok(())
}

fn print_lookup(address: Address, cursor: u64, e: Entry) {
    println!("address  {address}");
    println!("as of    block {cursor}");
    print_balance("USDT", e.usdt, e.usdt_block);
    print_balance("USDC", e.usdc, e.usdc_block);
}

fn print_balance(symbol: &str, amount: u128, block: u32) {
    println!(
        "{symbol:<8} {:>24}   last changed {}",
        crate::tokens::format_units(amount),
        since(block)
    );
}

fn since(block: u32) -> String {
    match block {
        0 => "never".to_string(),
        block => format!("block {block}"),
    }
}

/// Addresses a client can be pointed at. The map is not a complete holder set,
/// so a well-known address is usually *not* in it; these are.
pub fn sample(count: usize, min: u128, state: PathBuf) -> Result<()> {
    let balances = BalanceMap::load(&state).context(format!("reading {state:?}"))?;
    let floor = min.saturating_mul(10u128.pow(crate::tokens::DECIMALS));

    let mut shown = 0;
    for (address, e) in balances.iter() {
        if shown == count {
            break;
        }
        if e.usdt < floor && e.usdc < floor {
            continue;
        }
        println!(
            "{address}  USDT {:>20}  USDC {:>20}",
            crate::tokens::format_units(e.usdt),
            crate::tokens::format_units(e.usdc)
        );
        shown += 1;
    }

    if shown == 0 {
        println!(
            "no address holds at least {min} of either token (map has {} entries)",
            balances.len()
        );
    }
    Ok(())
}

pub fn stat(state: PathBuf) -> Result<()> {
    let balances = BalanceMap::load(&state).context(format!("reading {state:?}"))?;
    SnapshotStats::from_map(&balances).print(&balances);
    Ok(())
}

struct SnapshotStats {
    usdt_holders: usize,
    usdc_holders: usize,
    both: usize,
    usdt_total: u128,
    usdc_total: u128,
    stalest: u32,
}

impl SnapshotStats {
    fn from_map(balances: &BalanceMap) -> Self {
        let mut out = Self::empty();
        for (_, e) in balances.iter() {
            out.add(*e);
        }
        out
    }

    fn empty() -> Self {
        Self {
            usdt_holders: 0,
            usdc_holders: 0,
            both: 0,
            usdt_total: 0,
            usdc_total: 0,
            stalest: u32::MAX,
        }
    }

    fn add(&mut self, e: Entry) {
        self.add_usdt(e);
        self.add_usdc(e);
        if e.usdt > 0 && e.usdc > 0 {
            self.both += 1;
        }
    }

    fn add_usdt(&mut self, e: Entry) {
        if e.usdt == 0 {
            return;
        }
        self.usdt_holders += 1;
        self.usdt_total += e.usdt;
        self.stalest = self.stalest.min(e.usdt_block);
    }

    fn add_usdc(&mut self, e: Entry) {
        if e.usdc == 0 {
            return;
        }
        self.usdc_holders += 1;
        self.usdc_total += e.usdc;
        self.stalest = self.stalest.min(e.usdc_block);
    }

    fn print(&self, balances: &BalanceMap) {
        println!("cursor        block {}", balances.cursor);
        println!("addresses     {}", balances.len());
        println!(
            "USDT holders  {}  ({} USDT)",
            self.usdt_holders,
            crate::tokens::format_units(self.usdt_total)
        );
        println!(
            "USDC holders  {}  ({} USDC)",
            self.usdc_holders,
            crate::tokens::format_units(self.usdc_total)
        );
        println!("hold both     {}", self.both);
        self.print_oldest_change();
        println!(
            "PIR occupancy {:.1}% of 33,554,432",
            balances.len() as f64 * 100.0 / 33_554_432.0
        );
    }

    fn print_oldest_change(&self) {
        if self.stalest != u32::MAX {
            println!("oldest change block {}", self.stalest);
        }
    }
}
