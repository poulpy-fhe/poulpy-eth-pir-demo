use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, keccak256};
use alloy::rpc::types::eth::Log;

use super::events::addr_from_word;
use super::*;
use crate::tokens::{DESTROYED_BLACK_FUNDS, ISSUE, REDEEM, TRANSFER, USDC, USDT};

#[test]
fn topic_hashes_match_their_signatures() {
    assert_eq!(TRANSFER, keccak256("Transfer(address,address,uint256)"));
    assert_eq!(
        DESTROYED_BLACK_FUNDS,
        keccak256("DestroyedBlackFunds(address,uint256)")
    );
    assert_eq!(ISSUE, keccak256("Issue(uint256)"));
    assert_eq!(REDEEM, keccak256("Redeem(uint256)"));
}

#[test]
fn usdt_supply_events_are_flagged_for_owner_resolution() {
    let mut seen = HashMap::new();
    let got = collect_touched(
        &[
            supply_log(USDT, ISSUE, 900),
            supply_log(USDT, REDEEM, 950),
            supply_log(USDT, ISSUE, 910),
            supply_log(USDT, ISSUE, 950),
        ],
        &mut seen,
    );
    assert!(seen.is_empty());
    assert_eq!(got.understood, 4);
    assert_eq!(got.usdt_supply_blocks, vec![900, 910, 950]);
}

#[test]
fn supply_events_from_usdc_are_ignored() {
    let mut seen = HashMap::new();
    let got = collect_touched(&[supply_log(USDC, ISSUE, 900)], &mut seen);
    assert_eq!(got.usdt_supply_blocks, Vec::<u64>::new());
    assert!(seen.is_empty());
}

#[test]
fn padded_words_decode_and_junk_is_rejected() {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&[0xab; 20]);
    assert_eq!(addr_from_word(&w), Some(Address::from([0xab; 20])));
    assert_eq!(addr_from_word(&[0u8; 32]), None);
    w[0] = 1;
    assert_eq!(addr_from_word(&w), None);
    assert_eq!(addr_from_word(&w[..31]), None);
}

#[test]
fn transfer_yields_both_sides_and_skips_mints() {
    let from = Address::from([0x11; 20]);
    let to = Address::from([0x22; 20]);
    let logs = vec![
        transfer_log(USDT, Address::ZERO, to, 10),
        transfer_log(USDT, from, to, 11),
    ];
    let mut seen = HashMap::new();
    assert_eq!(collect_touched(&logs, &mut seen).understood, 2);
    assert_eq!(seen.len(), 2);
    assert!(seen.contains_key(&from) && seen.contains_key(&to));
}

#[test]
fn touch_keeps_the_highest_block_per_token() {
    let a = Address::from([0x33; 20]);
    let b = Address::from([0x44; 20]);
    let logs = vec![
        transfer_log(USDT, a, b, 100),
        transfer_log(USDT, a, b, 250),
        transfer_log(USDC, a, b, 175),
        transfer_log(USDT, a, b, 90),
    ];
    let mut seen = HashMap::new();
    collect_touched(&logs, &mut seen);
    assert_eq!(
        seen[&a],
        Touch {
            usdt: Some(250),
            usdc: Some(175)
        }
    );
    assert_eq!(
        seen[&b],
        Touch {
            usdt: Some(250),
            usdc: Some(175)
        }
    );
}

#[test]
fn zero_value_and_self_transfers_touch_nobody() {
    let a = Address::from([0x66; 20]);
    let b = Address::from([0x77; 20]);
    let mut seen = HashMap::new();
    assert_eq!(
        collect_touched(&[zero_transfer_log(USDT, a, b, 400)], &mut seen).understood,
        1
    );
    assert!(seen.is_empty());
    assert_eq!(
        collect_touched(&[transfer_log(USDT, a, a, 401)], &mut seen).understood,
        1
    );
    assert!(seen.is_empty());
}

#[test]
fn a_token_never_seen_leaves_its_touch_empty() {
    let a = Address::from([0x55; 20]);
    let mut seen = HashMap::new();
    collect_touched(&[transfer_log(USDC, a, Address::ZERO, 7)], &mut seen);
    assert_eq!(
        seen[&a],
        Touch {
            usdt: None,
            usdc: Some(7)
        }
    );
}

fn supply_log(token: Address, topic: B256, block: u64) -> Log {
    Log {
        inner: alloy::primitives::Log::new(token, vec![topic], Bytes::from(vec![0u8; 32])).unwrap(),
        block_number: Some(block),
        ..Default::default()
    }
}

fn transfer_log(token: Address, from: Address, to: Address, block: u64) -> Log {
    transfer_log_with_data(token, from, to, block, nonzero_word())
}

fn zero_transfer_log(token: Address, from: Address, to: Address, block: u64) -> Log {
    transfer_log_with_data(token, from, to, block, vec![0u8; 32])
}

fn transfer_log_with_data(
    token: Address,
    from: Address,
    to: Address,
    block: u64,
    data: Vec<u8>,
) -> Log {
    Log {
        inner: alloy::primitives::Log::new(
            token,
            vec![TRANSFER, word(from), word(to)],
            Bytes::from(data),
        )
        .unwrap(),
        block_number: Some(block),
        ..Default::default()
    }
}

fn word(a: Address) -> B256 {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    B256::from(w)
}

fn nonzero_word() -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[31] = 1;
    v
}
