//! Reading the chain: which addresses moved, and what they hold now.

mod events;
mod rpc;
mod tip;

pub use events::{Touch, collect_touched, filter};
pub use rpc::{ensure_mainnet, ensure_multicall_at, read_balances, usdt_owner};
pub use tip::{Tip, block_hash, finalized_block};

#[cfg(test)]
mod tests;
