//! Reading the chain: which addresses moved, and what they hold now.

mod events;
mod rpc;
mod tip;

pub use events::{Touch, collect_touched, collect_touched_strict, filter};
#[cfg(test)]
pub(crate) use rpc::encode_balance_response_for_test;
pub use rpc::{
    TotalSupplies, ensure_mainnet, ensure_multicall_at, read_balances, read_balances_strict,
    read_total_supplies, usdt_owner, usdt_owner_strict,
};
pub use tip::{Tip, block_hash, confirmed_target, finalized_block};

#[cfg(test)]
mod tests;
