use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, address, keccak256};
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

fn destroyed_log(token: Address, account: Address, block: u64) -> Log {
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(word(account).as_slice());
    data.extend_from_slice(&nonzero_word());
    Log {
        inner: alloy::primitives::Log::new(token, vec![DESTROYED_BLACK_FUNDS], Bytes::from(data))
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

#[test]
fn strict_discovery_accepts_well_formed_transfer_and_special_events() {
    let from = Address::repeat_byte(0x81);
    let to = Address::repeat_byte(0x82);
    let destroyed = Address::repeat_byte(0x83);
    let mut transfer = transfer_log(USDT, from, to, 1_000);
    transfer.block_hash = Some(B256::repeat_byte(1));
    let mut destroyed_log = destroyed_log(USDT, destroyed, 1_001);
    destroyed_log.block_hash = Some(B256::repeat_byte(2));
    let mut issue = supply_log(USDT, ISSUE, 1_002);
    issue.block_hash = Some(B256::repeat_byte(3));
    let mut seen = HashMap::new();
    let collected =
        collect_touched_strict(&[transfer, destroyed_log, issue], 1_000..=1_002, &mut seen)
            .unwrap();
    assert_eq!(collected.understood, 3);
    assert_eq!(collected.usdt_supply_blocks, vec![1_002]);
    assert_eq!(seen[&from].usdt, Some(1_000));
    assert_eq!(seen[&to].usdt, Some(1_000));
    assert_eq!(seen[&destroyed].usdt, Some(1_001));
}

#[test]
fn strict_destroyed_black_funds_requires_the_real_unindexed_usdt_shape() {
    let account = Address::repeat_byte(0x84);
    let mut valid = destroyed_log(USDT, account, 1_100);
    valid.block_hash = Some(B256::repeat_byte(4));
    let mut seen = HashMap::new();
    collect_touched_strict(&[valid.clone()], 1_100..=1_100, &mut seen).unwrap();
    assert_eq!(seen[&account].usdt, Some(1_100));

    let old_incorrect_shape = Log {
        inner: alloy::primitives::Log::new(
            USDT,
            vec![DESTROYED_BLACK_FUNDS, word(account)],
            Bytes::from(nonzero_word()),
        )
        .unwrap(),
        block_hash: valid.block_hash,
        block_number: valid.block_number,
        ..Default::default()
    };
    let mut truncated = valid.clone();
    truncated.inner = alloy::primitives::Log::new(
        USDT,
        vec![DESTROYED_BLACK_FUNDS],
        Bytes::from(vec![0u8; 63]),
    )
    .unwrap();
    let mut unpadded = valid;
    let mut unpadded_data = unpadded.data().data.to_vec();
    unpadded_data[0] = 1;
    unpadded.inner = alloy::primitives::Log::new(
        USDT,
        vec![DESTROYED_BLACK_FUNDS],
        Bytes::from(unpadded_data),
    )
    .unwrap();

    for malformed in [old_incorrect_shape, truncated, unpadded] {
        let mut seen = HashMap::new();
        assert!(collect_touched_strict(&[malformed], 1_100..=1_100, &mut seen).is_err());
        assert!(seen.is_empty());
    }
}

#[test]
fn strict_discovery_rejects_removed_blockless_hashless_out_of_range_and_malformed_logs() {
    let from = Address::repeat_byte(0x91);
    let to = Address::repeat_byte(0x92);
    let valid = || {
        let mut log = transfer_log(USDC, from, to, 2_000);
        log.block_hash = Some(B256::repeat_byte(4));
        log
    };
    let mut cases = Vec::new();
    let mut removed = valid();
    removed.removed = true;
    cases.push(removed);
    let mut blockless = valid();
    blockless.block_number = None;
    cases.push(blockless);
    let mut hashless = valid();
    hashless.block_hash = None;
    cases.push(hashless);
    let mut outside = valid();
    outside.block_number = Some(2_001);
    cases.push(outside);
    let mut malformed = valid();
    malformed.inner = alloy::primitives::Log::new(
        USDC,
        vec![TRANSFER, word(from), word(to)],
        Bytes::from(vec![1u8; 31]),
    )
    .unwrap();
    cases.push(malformed);

    for log in cases {
        let mut seen = HashMap::new();
        assert!(collect_touched_strict(&[log], 2_000..=2_000, &mut seen).is_err());
        assert!(seen.is_empty());
    }

    let mut first = valid();
    first.block_number = Some(1_999);
    let mut malformed_last = valid();
    malformed_last.block_hash = None;
    let mut seen = HashMap::new();
    assert!(collect_touched_strict(&[first, malformed_last], 1_999..=2_000, &mut seen).is_err());
    assert!(
        seen.is_empty(),
        "a failed range must publish no partial touches"
    );
}

#[test]
fn strict_valid_inert_transfers_are_intentional_noops() {
    let address = Address::repeat_byte(0xa1);
    let mut zero = zero_transfer_log(USDT, address, Address::repeat_byte(0xa2), 3_000);
    zero.block_hash = Some(B256::repeat_byte(5));
    let mut self_transfer = transfer_log(USDT, address, address, 3_000);
    self_transfer.block_hash = Some(B256::repeat_byte(5));
    let mut seen = HashMap::new();
    let collected =
        collect_touched_strict(&[zero, self_transfer], 3_000..=3_000, &mut seen).unwrap();
    assert_eq!(collected.understood, 2);
    assert!(seen.is_empty());
}

/// Supplementary production-path smoke test. It is ignored so the ordinary
/// suite remains deterministic and offline; run it explicitly with
/// `ETH_RPC_URL` set.
#[tokio::test]
#[ignore = "requires ETH_RPC_URL and bounded mainnet archive access"]
async fn live_mainnet_rpc_smoke_is_pinned_and_bounded() {
    use alloy::providers::{Provider, ProviderBuilder};

    let rpc = std::env::var("ETH_RPC_URL").expect("ETH_RPC_URL must be set");
    let provider = live_result(ProviderBuilder::new().connect(&rpc).await);
    live_result(ensure_mainnet(&provider).await);
    let head = live_result(provider.get_block_number().await);
    let target = confirmed_target(head, 4).unwrap();
    assert!(live_result(block_hash(&provider, target).await).is_some());

    const DESTROYED_REGRESSION_BLOCK: u64 = 22_719_557;
    let destroyed_logs = live_result(
        provider
            .get_logs(&filter(
                DESTROYED_REGRESSION_BLOCK,
                DESTROYED_REGRESSION_BLOCK,
            ))
            .await,
    )
    .into_iter()
    .filter(|log| log.address() == USDT && log.topics().first() == Some(&DESTROYED_BLACK_FUNDS))
    .collect::<Vec<_>>();
    assert_eq!(destroyed_logs.len(), 3);
    let mut destroyed_accounts = HashMap::new();
    let destroyed = live_result(collect_touched_strict(
        &destroyed_logs,
        DESTROYED_REGRESSION_BLOCK..=DESTROYED_REGRESSION_BLOCK,
        &mut destroyed_accounts,
    ));
    assert_eq!(destroyed.understood, 3);
    assert!(!destroyed_accounts.is_empty());

    let logs = live_result(
        provider
            .get_logs(&filter(
                crate::tokens::USDT_DEPLOY_BLOCK,
                crate::tokens::USDT_DEPLOY_BLOCK,
            ))
            .await,
    );
    let mut touched = HashMap::new();
    live_result(collect_touched_strict(
        &logs,
        crate::tokens::USDT_DEPLOY_BLOCK..=crate::tokens::USDT_DEPLOY_BLOCK,
        &mut touched,
    ));
    live_result(crate::bootstrap::rpc_preflight(&provider, target, 0).await);
    assert!(
        live_result(usdt_owner_strict(&provider, crate::tokens::USDT_OWNER_PREFLIGHT_BLOCK).await)
            == address!("0xc6cde7c39eb2f0f0095f41570af89efc2c1ea828")
    );
    let balances = live_result(
        read_balances_strict(&provider, &[crate::tokens::USDT_INITIAL_OWNER], target).await,
    );
    assert_eq!(balances.len(), 1);
    let supplies = live_result(read_total_supplies(&provider, target).await);
    assert!(supplies.usdt > 0 && supplies.usdc > 0);
    eprintln!("bounded mainnet smoke passed at public block {target}");
}

fn live_result<T, E: std::fmt::Display>(result: std::result::Result<T, E>) -> T {
    result.unwrap_or_else(|error| {
        panic!(
            "live RPC smoke failed: {}",
            crate::redact::urls(&error.to_string())
        )
    })
}
