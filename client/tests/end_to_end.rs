//! The whole chain, at a small PIR shape: server record -> query bytes ->
//! response bytes -> decoded report.
//!
//! The 2 GiB production shape needs ~14 GiB of RAM and half a minute to build,
//! so this runs the same code paths at a shape that fits in a test. Only the
//! geometry differs; the record layout, the wire codecs, and the not-in-set
//! check are the deployed ones.

use std::collections::HashMap;

use eth_pir::EthPirServer;
use poulpy_pir::config::{Collapse, Config};
use poulpy_pir::database::DatabaseLayout;
use poulpy_pir::payload::U512P65536;
use usdt_pir_client::Client;
use usdt_pir_record::{Entry, UsdtUsdc};

/// One server is ~6 GiB even at this shape — the ring and the packing scratch
/// are sized by `n` and `gamma1`, not by the database. The harness runs tests
/// one per core, so without this they all allocate at once and the machine dies.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const GAMMA0: usize = 32;
/// Capacity is `COLS²` payloads, against 33,554,432 in the deployed shape.
const COLS: usize = 64;

fn shape() -> (Config<U512P65536>, DatabaseLayout<U512P65536>) {
    let config = Config::<U512P65536>::with_collapse(Collapse::Recursion {
        gamma0: GAMMA0,
        gamma1: 1024,
        gamma2: 32,
    });
    (
        config,
        DatabaseLayout::<U512P65536>::new(COLS * GAMMA0, COLS),
    )
}

fn addr(byte: u8) -> [u8; 20] {
    let mut a = [byte; 20];
    a[0] = byte.wrapping_add(1);
    a
}

fn hex(a: &[u8; 20]) -> String {
    let mut s = String::from("0x");
    for b in a {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn holders() -> HashMap<[u8; 20], Entry> {
    (1u8..=32)
        .map(|i| {
            (
                addr(i),
                Entry {
                    usdt: u128::from(i) * 1_000_000,
                    usdt_block: 21_000_000 + u32::from(i),
                    usdc: u128::from(i) * 250_000,
                    usdc_block: 21_500_000 + u32::from(i),
                },
            )
        })
        .collect()
}

#[test]
fn a_lookup_returns_the_balances_the_server_holds() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let map = holders();
    let server = EthPirServer::<UsdtUsdc>::init_with(config, layout, &map).expect("server");
    let responder = server.responder();
    let directory = server.keyword().try_full().expect("directory");

    let mut client = Client::with_shape(config, layout, &directory).expect("client");

    for probe in [1u8, 7, 32] {
        let key = addr(probe);
        let expected = map[&key];

        let q = client.query(&hex(&key)).expect("query");
        let response = responder.try_respond_bytes(&q.bytes).expect("respond");
        let report = client.decode(q.id, &response).expect("decode");

        assert!(report.held, "address {probe} should be held");
        assert_eq!(report.usdt.raw, expected.usdt);
        assert_eq!(report.usdc.raw, expected.usdc);
        assert_eq!(report.usdt.last_change_block, expected.usdt_block);
        assert_eq!(report.usdc.last_change_block, expected.usdc_block);
        assert_eq!(report.address.to_lowercase(), hex(&key));
    }
}

/// The MPHF is total: an address it never indexed still resolves to some
/// occupied slot. The address prefix in the record is what turns that into an
/// honest "not held" instead of another holder's balance.
#[test]
fn an_address_the_server_does_not_hold_reports_not_held() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let server = EthPirServer::<UsdtUsdc>::init_with(config, layout, &holders()).expect("server");
    let responder = server.responder();
    let directory = server.keyword().try_full().expect("directory");
    let mut client = Client::with_shape(config, layout, &directory).expect("client");

    let stranger = [0xEEu8; 20];
    let q = client.query(&hex(&stranger)).expect("query");
    let response = responder.try_respond_bytes(&q.bytes).expect("respond");
    let report = client.decode(q.id, &response).expect("decode");

    assert!(!report.held);
    assert_eq!(report.usdt.raw, 0);
    assert_eq!(report.usdc.raw, 0);
}

/// A record whose balances are zero is still a record: the server holds it, so
/// the answer is "held, zero", not "not held".
#[test]
fn a_zero_balance_holder_is_still_held() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let mut map = holders();
    let zeroed = addr(200);
    map.insert(zeroed, Entry::default());

    let server = EthPirServer::<UsdtUsdc>::init_with(config, layout, &map).expect("server");
    let responder = server.responder();
    let directory = server.keyword().try_full().expect("directory");
    let mut client = Client::with_shape(config, layout, &directory).expect("client");

    let q = client.query(&hex(&zeroed)).expect("query");
    let response = responder.try_respond_bytes(&q.bytes).expect("respond");
    let report = client.decode(q.id, &response).expect("decode");

    assert!(report.held);
    assert_eq!(report.usdt.raw, 0);
    assert_eq!(report.usdc.raw, 0);
}

/// A garbled response must fail rather than decode into a plausible balance.
#[test]
fn a_corrupted_response_is_rejected() {
    let _pir = exclusive();
    let (config, layout) = shape();
    let map = holders();
    let server = EthPirServer::<UsdtUsdc>::init_with(config, layout, &map).expect("server");
    let responder = server.responder();
    let directory = server.keyword().try_full().expect("directory");
    let mut client = Client::with_shape(config, layout, &directory).expect("client");

    let key = addr(3);
    let q = client.query(&hex(&key)).expect("query");
    let mut response = responder.try_respond_bytes(&q.bytes).expect("respond");
    response.truncate(response.len() / 2);

    assert!(client.decode(q.id, &response).is_err());
}
