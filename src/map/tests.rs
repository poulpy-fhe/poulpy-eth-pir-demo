use std::io;
use std::path::Path;

use alloy::primitives::{Address, address};

use super::snapshot::parent_to_create;
use super::*;
use crate::chain::Touch;

const A: Address = address!("0x00000000000000000000000000000000000000aa");

fn both(b: u64) -> Touch {
    Touch {
        usdt: Some(b),
        usdc: Some(b),
    }
}

fn usdt_at(b: u64) -> Touch {
    Touch {
        usdt: Some(b),
        usdc: None,
    }
}

#[test]
fn each_token_stamps_only_its_own_block() {
    let mut m = BalanceMap::new(0);
    m.apply(A, Reading { usdt: 10, usdc: 20 }, both(100));
    assert_eq!(m.get(&A).unwrap().usdt_block, 100);
    assert_eq!(m.get(&A).unwrap().usdc_block, 100);
    assert_eq!(
        m.apply(A, Reading { usdt: 11, usdc: 20 }, usdt_at(200)),
        Applied::Updated
    );
    let e = m.get(&A).unwrap();
    assert_eq!((e.usdt, e.usdt_block), (11, 200));
    assert_eq!((e.usdc, e.usdc_block), (20, 100));
    assert_eq!(
        m.apply(A, Reading { usdt: 11, usdc: 20 }, Touch::default()),
        Applied::Unchanged
    );
}

#[test]
fn a_round_trip_back_to_the_same_balance_still_stamps() {
    let mut m = BalanceMap::new(0);
    m.apply(A, Reading { usdt: 500, usdc: 0 }, usdt_at(100));
    assert_eq!(
        m.apply(A, Reading { usdt: 500, usdc: 0 }, usdt_at(140)),
        Applied::Updated
    );
    assert_eq!(m.get(&A).unwrap().usdt_block, 140);
}

#[test]
fn per_chunk_maxima_fold_to_the_whole_range() {
    let mut one_pass = BalanceMap::new(0);
    one_pass.apply(
        A,
        Reading { usdt: 7, usdc: 3 },
        Touch {
            usdt: Some(190),
            usdc: Some(155),
        },
    );
    let mut chunked = BalanceMap::new(0);
    chunked.apply(
        A,
        Reading { usdt: 4, usdc: 3 },
        Touch {
            usdt: Some(120),
            usdc: Some(155),
        },
    );
    chunked.apply(A, Reading { usdt: 4, usdc: 3 }, Touch::default());
    chunked.apply(
        A,
        Reading { usdt: 7, usdc: 3 },
        Touch {
            usdt: Some(190),
            usdc: None,
        },
    );
    assert_eq!(one_pass.get(&A), chunked.get(&A));
}

#[test]
fn a_token_never_seen_keeps_block_zero() {
    let mut m = BalanceMap::new(0);
    m.apply(A, Reading { usdt: 7, usdc: 0 }, usdt_at(500));
    let e = m.get(&A).unwrap();
    assert_eq!((e.usdt, e.usdt_block), (7, 500));
    assert_eq!((e.usdc, e.usdc_block), (0, 0));
}

#[test]
fn holding_a_token_never_seen_moving_reads_as_unknown() {
    let mut m = BalanceMap::new(0);
    m.apply(A, Reading { usdt: 7, usdc: 900 }, usdt_at(500));
    assert_eq!(
        (m.get(&A).unwrap().usdc, m.get(&A).unwrap().usdc_block),
        (900, 0)
    );
}

#[test]
fn snapshot_roundtrips_entries_and_cursor() {
    let m = populated_map();
    let path = std::env::temp_dir().join("usdt_pir_snapshot_roundtrip.bin");
    m.save(&path).unwrap();
    let back = BalanceMap::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(back.cursor, m.cursor);
    assert_eq!(back.len(), m.len());
    for (addr, e) in m.iter() {
        assert_eq!(back.get(addr).as_ref(), Some(e));
    }
}

fn populated_map() -> BalanceMap {
    let mut m = BalanceMap::new(21_000_000);
    for i in 0..1000u32 {
        let mut raw = [0u8; 20];
        raw[..4].copy_from_slice(&i.to_be_bytes());
        m.apply(
            Address::from(raw),
            Reading {
                usdt: i as u128 * 7,
                usdc: u128::MAX - i as u128,
            },
            both(20_000_000 + i as u64),
        );
    }
    m
}

#[test]
fn saving_creates_a_missing_parent_directory() {
    let root = std::env::temp_dir().join("usdt_pir_mkdir_test");
    std::fs::remove_dir_all(&root).ok();
    let path = root.join("data").join("balances.snapshot");
    let mut m = BalanceMap::new(21_000_000);
    m.apply(A, Reading { usdt: 5, usdc: 7 }, both(20_999_999));
    m.save(&path).expect("save must create ./data");
    assert_eq!(BalanceMap::load(&path).unwrap().get(&A), m.get(&A));
    assert_eq!(path.with_extension("tmp").parent(), path.parent());
    assert!(!path.with_extension("tmp").exists());
    m.save(&path).expect("second save");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn only_a_real_parent_directory_is_created() {
    assert_eq!(
        parent_to_create(Path::new("data/balances.snapshot")),
        Some(Path::new("data"))
    );
    assert_eq!(Path::new("bare.snapshot").parent(), Some(Path::new("")));
    assert_eq!(parent_to_create(Path::new("bare.snapshot")), None);
}

#[test]
fn a_foreign_file_is_rejected_rather_than_misread() {
    let path = std::env::temp_dir().join("usdt_pir_bad_magic.bin");
    std::fs::write(&path, b"NOTMINE!\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
    let err = BalanceMap::load(&path).unwrap_err();
    std::fs::remove_file(&path).ok();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn new_zero_is_refused_and_known_zero_is_removed() {
    let mut m = BalanceMap::new(0);
    assert_eq!(
        m.apply(A, Reading::default(), both(100)),
        Applied::SkippedNewZero
    );
    assert_eq!(m.len(), 0);
    let funded = Reading { usdt: 5, usdc: 0 };
    assert_eq!(m.apply(A, funded, usdt_at(101)), Applied::Inserted);
    assert_eq!(m.apply(A, funded, Touch::default()), Applied::Unchanged);
    let removed = m.apply(A, Reading::default(), usdt_at(103));
    assert!(matches!(removed, Applied::Removed(e) if e.usdt == 0 && e.usdt_block == 103));
    assert_eq!(m.len(), 0);
    assert_eq!(m.get(&A), None);
}

/// A snapshot that was cut short mid-write must be refused, not loaded as a
/// smaller map: silently serving a truncated holder set is worse than failing.
#[test]
fn a_truncated_snapshot_is_rejected() {
    let path = std::env::temp_dir().join("usdt_pir_truncated.bin");
    populated_map().save(&path).unwrap();

    let full = std::fs::read(&path).unwrap();
    std::fs::write(&path, &full[..full.len() - 8]).unwrap();

    let err = BalanceMap::load(&path).expect_err("truncated file must not load");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
}

#[test]
fn a_corrupted_row_is_rejected() {
    let path = std::env::temp_dir().join("usdt_pir_corrupt_row.bin");
    populated_map().save(&path).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    let row = 24 + 4; // inside the first row's address
    bytes[row] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let err = BalanceMap::load(&path).expect_err("flipped bit must not load");
    assert!(format!("{err}").contains("checksum"), "{err}");
}

/// A header claiming an absurd row count must not be believed up front.
#[test]
fn an_absurd_row_count_does_not_allocate() {
    let path = std::env::temp_dir().join("usdt_pir_absurd_count.bin");
    populated_map().save(&path).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let err = BalanceMap::load(&path).expect_err("must fail on the short read");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "{err}");
}
