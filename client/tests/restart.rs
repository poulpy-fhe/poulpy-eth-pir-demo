//! What a server restart does to a client that is already running.
//!
//! The MPHF is not reproducible, so without a persisted checkpoint a restart
//! renumbers every slot and every client silently reads the wrong record until
//! it resyncs. These tests pin the property that makes that not happen.

use std::collections::HashMap;

use eth_pir::{EthPirServer, KeywordCheckpoint};
use poulpy_pir::config::{Collapse, Config};
use poulpy_pir::database::DatabaseLayout;
use poulpy_pir::payload::U512P65536;
use usdt_pir_client::Client;
use usdt_pir_record::{Entry, UsdtUsdc};

const GAMMA0: usize = 32;
const COLS: usize = 64;

/// One server is ~6 GiB even at this shape — the ring and the packing scratch
/// are sized by `n` and `gamma1`, not by the database. The harness runs tests
/// one per core, so without this they all allocate at once and the machine dies.
/// Held for the whole test, because the servers have to stay alive.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn shape() -> (Config<U512P65536>, DatabaseLayout<U512P65536>) {
    (
        Config::<U512P65536>::with_collapse(Collapse::Recursion {
            gamma0: GAMMA0,
            gamma1: 1024,
            gamma2: 32,
        }),
        DatabaseLayout::<U512P65536>::new(COLS * GAMMA0, COLS),
    )
}

fn addr(i: u8) -> [u8; 20] {
    let mut a = [0u8; 20];
    a[0] = i;
    a[19] = i.wrapping_mul(7).wrapping_add(1);
    a
}

fn hex(a: &[u8; 20]) -> String {
    let mut s = String::from("0x");
    for b in a {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn entry(i: u8) -> Entry {
    Entry {
        usdt: u128::from(i) * 1_000_000,
        usdt_block: 21_000_000 + u32::from(i),
        usdc: u128::from(i) * 7,
        usdc_block: 21_500_000 + u32::from(i),
    }
}

fn holders(range: std::ops::RangeInclusive<u8>) -> HashMap<[u8; 20], Entry> {
    range.map(|i| (addr(i), entry(i))).collect()
}

fn boot(map: &HashMap<[u8; 20], Entry>) -> (EthPirServer<UsdtUsdc>, KeywordCheckpoint) {
    let (config, layout) = shape();
    let server = EthPirServer::<UsdtUsdc>::init_with(config, layout, map).expect("init");
    let checkpoint = server.checkpoint().expect("checkpoint");
    (server, checkpoint)
}

/// The checkpoint alone, with the server dropped. Two live servers is 11 GiB
/// against 6, and most of these tests only ever need the saved bytes.
fn checkpoint_of(map: &HashMap<[u8; 20], Entry>) -> KeywordCheckpoint {
    boot(map).1
}

fn ask(
    client: &mut Client,
    server: &EthPirServer<UsdtUsdc>,
    a: &[u8; 20],
) -> usdt_pir_client::Report {
    let q = client.query(&hex(a)).expect("query");
    let response = server
        .responder()
        .try_respond_bytes(&q.bytes)
        .expect("respond");
    client.decode(q.id, &response).expect("decode")
}

/// The point of the whole checkpoint: a client that synced before the restart
/// keeps working afterwards, without refetching anything.
#[test]
fn a_restart_from_a_checkpoint_keeps_every_client_slot() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let map = holders(1..=24);
    let (server, checkpoint) = boot(&map);

    let mut client = Client::with_shape(config, layout, &checkpoint.directory).expect("client");
    let before: Vec<usize> = (1..=24)
        .map(|i| client.slot(&hex(&addr(i))).unwrap())
        .collect();
    let version_before = client.version();

    // Restart: same map, restored from the checkpoint.
    drop(server);
    let (restarted, report) = EthPirServer::<UsdtUsdc>::restore_with(
        config,
        layout,
        &checkpoint.directory,
        &checkpoint.keys,
        &map,
    )
    .expect("restore");
    assert_eq!(report.placed, 24);
    assert_eq!(report.appended, 0, "nothing new, nothing to append");
    assert_eq!(report.vacant, 0);

    // The client never touched the network across the restart.
    assert_eq!(client.version(), version_before, "no version bump");
    let after: Vec<usize> = (1..=24)
        .map(|i| client.slot(&hex(&addr(i))).unwrap())
        .collect();
    assert_eq!(before, after, "every slot survived the restart");

    for i in 1..=24u8 {
        let report = ask(&mut client, &restarted, &addr(i));
        assert!(report.held, "address {i} lost across the restart");
        assert_eq!(report.usdt.raw, entry(i).usdt);
        assert_eq!(report.usdc.raw, entry(i).usdc);
    }
}

/// Without the slot table, membership cannot be recovered: the MPHF answers for
/// addresses it never saw. Rebuilding from scratch is the alternative, and it
/// moves everything — which is exactly what the checkpoint avoids.
#[test]
fn rebuilding_instead_of_restoring_moves_slots() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let map = holders(1..=24);
    let checkpoint = checkpoint_of(&map);
    let mut client = Client::with_shape(config, layout, &checkpoint.directory).expect("client");
    let before: Vec<usize> = (1..=24)
        .map(|i| client.slot(&hex(&addr(i))).unwrap())
        .collect();

    let fresh_checkpoint = checkpoint_of(&map);
    let fresh_client =
        Client::with_shape(config, layout, &fresh_checkpoint.directory).expect("client");
    let after: Vec<usize> = (1..=24)
        .map(|i| fresh_client.slot(&hex(&addr(i))).unwrap())
        .collect();

    assert_ne!(
        before, after,
        "a fresh MPHF over the same keys should permute them; if this ever holds, \
         the checkpoint is not buying anything"
    );
    let _ = &mut client;
}

/// Addresses that arrived after the checkpoint was written are appended to the
/// delta. They must not displace anyone: an existing client keeps its slots and
/// picks the new ones up as an ordinary tail.
#[test]
fn addresses_added_since_the_checkpoint_are_appended_not_swapped() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let saved = holders(1..=20);
    let checkpoint = checkpoint_of(&saved);

    let mut client = Client::with_shape(config, layout, &checkpoint.directory).expect("client");
    let before: Vec<usize> = (1..=20)
        .map(|i| client.slot(&hex(&addr(i))).unwrap())
        .collect();

    // The map moved on before the crash: four new addresses, one drained away.
    let mut drifted = holders(1..=24);
    drifted.remove(&addr(5));

    let (restarted, report) = EthPirServer::<UsdtUsdc>::restore_with(
        config,
        layout,
        &checkpoint.directory,
        &checkpoint.keys,
        &drifted,
    )
    .expect("restore");
    assert_eq!(report.appended, 4, "the four new addresses");
    assert_eq!(report.vacant, 1, "the drained one left its slot empty");
    assert_eq!(report.placed, 19);

    let after: Vec<usize> = (1..=20)
        .map(|i| client.slot(&hex(&addr(i))).unwrap())
        .collect();
    assert_eq!(before, after);
    assert_old_slots_hold(&mut client, &restarted);

    apply_tail(&mut client, &restarted);
    assert_new_tail_visible(&mut client, &restarted);
}

fn assert_old_slots_hold(client: &mut Client, server: &EthPirServer<UsdtUsdc>) {
    for i in (1..=20u8).filter(|i| *i != 5) {
        assert!(ask(client, server, &addr(i)).held, "address {i} moved");
    }
    assert!(!ask(client, server, &addr(5)).held, "drained address");
}

fn apply_tail(client: &mut Client, server: &EthPirServer<UsdtUsdc>) {
    let wire = server.keyword();
    let tail = wire.try_tail(client.tail_len()).expect("tail");
    client.apply_tail(&tail).expect("apply tail");
}

fn assert_new_tail_visible(client: &mut Client, server: &EthPirServer<UsdtUsdc>) {
    for i in 21..=24u8 {
        let report = ask(client, server, &addr(i));
        assert!(report.held, "address {i} not visible after the tail");
        assert_eq!(report.usdt.raw, entry(i).usdt);
    }
}

#[test]
fn a_reappearing_delta_address_reuses_its_slot() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let saved = holders(1..=20);
    let mut server = boot(&saved).0;
    server
        .update(addr(21), entry(21))
        .expect("append delta key");
    server
        .try_rebuild_database()
        .expect("refresh")
        .expect("pending");
    let checkpoint = server.checkpoint().expect("checkpoint with delta");
    drop(server);

    let mut client = Client::with_shape(config, layout, &checkpoint.directory).expect("client");
    let version_before = client.version();
    let slot_before = client.slot(&hex(&addr(21))).expect("slot");

    let (mut restarted, _) = EthPirServer::<UsdtUsdc>::restore_with(
        config,
        layout,
        &checkpoint.directory,
        &checkpoint.keys,
        &saved,
    )
    .expect("restore with drained delta key");

    restarted
        .update(addr(21), entry(21))
        .expect("reactivate delta key");
    restarted
        .try_rebuild_database()
        .expect("refresh")
        .expect("pending");

    assert_eq!(client.version(), version_before);
    assert_eq!(client.slot(&hex(&addr(21))).unwrap(), slot_before);
    let report = ask(&mut client, &restarted, &addr(21));
    assert!(report.held, "reactivated address must be visible");
    assert_eq!(report.usdt.raw, entry(21).usdt);
}

/// The keys table describes one MPHF generation. Pairing it with a directory
/// from another must fail loudly rather than scatter records.
#[test]
fn a_mismatched_checkpoint_is_rejected() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let map = holders(1..=20);
    let first = checkpoint_of(&map);
    let second = checkpoint_of(&holders(1..=30));

    let err = EthPirServer::<UsdtUsdc>::restore_with(
        config,
        layout,
        &first.directory,
        &second.keys,
        &map,
    );
    assert!(err.is_err(), "key count must be checked against the MPHF");
}
