//! The two tokens this service tracks, and the event topics that name an
//! address whose balance may have changed.

use alloy::primitives::{Address, B256, address, b256};

/// Tether USD, 6 decimals.
pub const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
/// Circle USD Coin, 6 decimals.
pub const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// Multicall3, same address on every chain it is deployed to.
pub const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

pub use usdt_pir_record::{DECIMALS, format_units};

/// `keccak256("Transfer(address,address,uint256)")`.
pub const TRANSFER: B256 =
    b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// `keccak256("DestroyedBlackFunds(address,uint256)")`.
///
/// USDT can zero a blacklisted balance without emitting a `Transfer`. We never
/// read the amount: the event only tells us *who* to re-read.
pub const DESTROYED_BLACK_FUNDS: B256 =
    b256!("0x61e6e66b0d6339b2980aecc6ccc0039736791f0ccde9ed512e789a7fbdd698c6");

/// `keccak256("Issue(uint256)")` — USDT mint.
///
/// USDT does not mint through a `Transfer` from the zero address the way most
/// ERC-20s do. `issue(uint)` credits `balances[owner]` directly and emits only
/// this, so an issuance is invisible to a `Transfer`-only watcher and the
/// treasury balance goes stale. Same for [`REDEEM`].
///
/// Neither event carries an address — only an amount — so the address to
/// re-read has to come from the contract's `owner()`.
pub const ISSUE: B256 = b256!("0xcb8241adb0c3fdb35b70c24ce35c5eb0c17af7431c99f827d44a445ca624176a");

/// `keccak256("Redeem(uint256)")` — USDT burn. See [`ISSUE`].
pub const REDEEM: B256 =
    b256!("0x702d5967f45f6513a38ffc42d6ba9bf230bd40e8f53b16363c7eb4fd2deb9a44");

/// USDT supply events, which move the owner's balance and name nobody.
pub const SUPPLY_TOPICS: [B256; 2] = [ISSUE, REDEEM];

/// Ethereum mainnet. Every address in this module is mainnet-specific, and
/// Multicall3 exists at the same address on dozens of chains, so pointing this
/// at the wrong network would silently produce a plausible, wrong map.
pub const MAINNET_CHAIN_ID: u64 = 1;

/// The tokens, in the fixed order used by every packed record and every
/// multicall batch.
pub const TOKENS: [Address; 2] = [USDT, USDC];

/// Every event that can move a balance on either token.
pub const WATCHED_TOPICS: [B256; 4] = [TRANSFER, DESTROYED_BLACK_FUNDS, ISSUE, REDEEM];

/// Block at which Multicall3 was deployed to mainnet. Batched reads pinned
/// before this hit an address with no code.
pub const MULTICALL3_DEPLOY_BLOCK: u64 = 14_353_601;
