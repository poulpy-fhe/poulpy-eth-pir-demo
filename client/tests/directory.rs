//! Client behaviour against a real keyword directory. No PIR server is involved:
//! everything here is client-side, which is exactly the half that has to run in
//! a browser.

use poulpy_pir::keyword::{KeywordDirectory, KeywordIndex};
use usdt_pir_client::{Client, SyncNeed};

const CAPACITY: usize = 33_554_432;

fn addresses(n: usize) -> Vec<[u8; 20]> {
    (0..n as u64)
        .map(|i| {
            let mut k = [0u8; 20];
            let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
            k[..8].copy_from_slice(&h.to_le_bytes());
            k[8..16].copy_from_slice(&(h ^ 0xA5A5_A5A5_A5A5_A5A5).to_le_bytes());
            k
        })
        .collect()
}

fn directory(keys: &[[u8; 20]]) -> KeywordDirectory<20> {
    KeywordDirectory::new(KeywordIndex::build(keys).expect("mphf"), CAPACITY, 0).expect("directory")
}

fn blob(dir: &KeywordDirectory<20>) -> Vec<u8> {
    let mut out = Vec::new();
    dir.write_to(&mut out).expect("serialize");
    out
}

fn hex(addr: &[u8; 20]) -> String {
    let mut s = String::from("0x");
    for b in addr {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn a_client_bootstraps_and_builds_a_query() {
    let keys = addresses(5_000);
    let client = directory(&keys);
    let mut client = Client::new(&blob(&client)).expect("bootstrap");

    let q = client.query(&hex(&keys[42])).expect("query");
    assert!(
        q.bytes.len() > 100_000,
        "a query at the 2 GiB shape is ~675 KiB, got {}",
        q.bytes.len()
    );
    assert_eq!(client.pending_count(), 1);

    // Distinct ids, and cancelling frees the slot.
    let q2 = client.query(&hex(&keys[43])).expect("query");
    assert_ne!(q.id, q2.id);
    assert!(client.cancel(q.id));
    assert!(!client.cancel(q.id), "cancelling twice is a no-op");
    assert_eq!(client.pending_count(), 1);
}

#[test]
fn decoding_an_unknown_id_is_an_error_not_a_panic() {
    let keys = addresses(64);
    let mut client = Client::new(&blob(&directory(&keys))).expect("bootstrap");
    assert!(client.decode(999, &[0u8; 8]).is_err());
}

#[test]
fn sync_need_tracks_version_and_tail() {
    let keys = addresses(1_000);
    let mut dir = directory(&keys);
    let mut client = Client::new(&blob(&dir)).expect("bootstrap");

    assert_eq!(client.sync_need(dir.version(), 0), SyncNeed::UpToDate);

    // Server appends two addresses to the delta.
    let extra = addresses(1_002);
    dir.push(&extra[1_000]).expect("push");
    dir.push(&extra[1_001]).expect("push");
    assert_eq!(
        client.sync_need(dir.version(), dir.delta_len()),
        SyncNeed::Tail { from: 0 }
    );

    let mut tail = Vec::new();
    dir.write_delta_envelope_from(&mut tail, 0).expect("tail");
    client.apply_tail(&tail).expect("apply tail");
    assert_eq!(client.tail_len(), 2);
    assert_eq!(client.sync_need(dir.version(), 2), SyncNeed::UpToDate);

    // A rebuild permutes every index, so a tail is meaningless: resync in full.
    let rebuilt = dir.rebuilt(&extra).expect("rebuild");
    assert_eq!(client.sync_need(rebuilt.version(), 0), SyncNeed::Full);
    client.resync(&blob(&rebuilt)).expect("resync");
    assert_eq!(client.version(), rebuilt.version());
    assert_eq!(client.sync_need(rebuilt.version(), 0), SyncNeed::UpToDate);
}

/// A resync invalidates outstanding queries: their indices came from the old
/// MPHF, so decoding one against the new database would verify against the
/// wrong record.
#[test]
fn resync_drops_pending_queries() {
    let keys = addresses(500);
    let dir = directory(&keys);
    let mut client = Client::new(&blob(&dir)).expect("bootstrap");

    let q = client.query(&hex(&keys[7])).expect("query");
    assert_eq!(client.pending_count(), 1);

    let rebuilt = dir.rebuilt(&keys).expect("rebuild");
    client.resync(&blob(&rebuilt)).expect("resync");

    assert_eq!(client.pending_count(), 0);
    assert!(client.decode(q.id, &[0u8; 8]).is_err());
}
