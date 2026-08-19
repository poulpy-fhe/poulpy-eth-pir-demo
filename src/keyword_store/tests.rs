use super::*;
use eth_pir::KeywordCheckpoint;
use poulpy_pir::keyword::{KeywordDirectory, KeywordIndex};

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("usdt-pir-kw-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.push("keyword");
    p
}

fn addresses(n: usize) -> Vec<Address> {
    (0..n as u64)
        .map(|i| {
            let mut k = [0u8; 20];
            k[..8].copy_from_slice(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
            k
        })
        .collect()
}

fn checkpoint(keys: Vec<Address>, version: u64) -> KeywordCheckpoint {
    let mphf = KeywordIndex::build(&keys).unwrap();
    let dir = KeywordDirectory::new(mphf, 1 << 20, version).unwrap();
    let mut blob = Vec::new();
    dir.write_to(&mut blob).unwrap();
    // Slot order, which is what the real checkpoint stores.
    let mut slots = vec![[0u8; 20]; keys.len()];
    for key in &keys {
        slots[dir.index(key)] = *key;
    }
    KeywordCheckpoint {
        directory: blob,
        version,
        keys: slots,
    }
}

#[test]
fn a_checkpoint_round_trips() {
    let base = tmp("roundtrip");
    let paths = Paths::new(&base);
    let saved = checkpoint(addresses(500), 0);

    assert!(load(&paths).unwrap().is_none(), "nothing saved yet");
    save_checkpoint(&paths, &saved).unwrap();

    let loaded = load(&paths).unwrap().expect("saved checkpoint");
    assert_eq!(loaded.version, saved.version);
    assert_eq!(loaded.keys, saved.keys);
    assert_eq!(loaded.directory, saved.directory);
}

/// The `.keys` table only describes the MPHF it was built with. Pairing it with
/// a newer `.index` would put addresses at slots that are not theirs, so a
/// mismatch has to read as "nothing saved" and force a rebuild.
#[test]
fn a_stale_keys_file_is_refused() {
    let base = tmp("stale");
    let paths = Paths::new(&base);

    save_checkpoint(&paths, &checkpoint(addresses(300), 0)).unwrap();
    // A later rebuild wrote a new directory but died before the keys file.
    save_index(&paths, &checkpoint(addresses(300), 1).directory).unwrap();

    assert!(
        load(&paths).unwrap().is_none(),
        "version mismatch must refuse"
    );
}

/// A publish rewrites only the index; the keys file stays valid because the MPHF
/// did not change.
#[test]
fn saving_the_index_alone_keeps_the_pair_loadable() {
    let base = tmp("index-only");
    let paths = Paths::new(&base);
    let mut saved = checkpoint(addresses(200), 7);
    save_checkpoint(&paths, &saved).unwrap();

    // Same MPHF generation, delta appended.
    let mut dir = KeywordDirectory::<20>::read_from(&mut &saved.directory[..]).unwrap();
    for extra in addresses(205).into_iter().skip(200) {
        dir.push(&extra).unwrap();
    }
    saved.directory.clear();
    dir.write_to(&mut saved.directory).unwrap();
    save_index(&paths, &saved.directory).unwrap();

    let loaded = load(&paths).unwrap().expect("still loadable");
    assert_eq!(loaded.version, 7);
    assert_eq!(
        loaded.keys.len(),
        200,
        "keys still cover the MPHF range only"
    );
    let reloaded = KeywordDirectory::<20>::read_from(&mut &loaded.directory[..]).unwrap();
    assert_eq!(reloaded.delta_len(), 5, "the delta survived");
}

#[test]
fn a_missing_half_reads_as_nothing_saved() {
    let base = tmp("half");
    let paths = Paths::new(&base);

    save_index(&paths, &checkpoint(addresses(50), 0).directory).unwrap();
    assert!(load(&paths).unwrap().is_none(), "index without keys");

    let _ = std::fs::remove_file(&paths.index);
    save_checkpoint(&paths, &checkpoint(addresses(50), 0)).unwrap();
    let _ = std::fs::remove_file(&paths.index);
    assert!(load(&paths).unwrap().is_none(), "keys without index");
}

#[test]
fn a_corrupt_file_is_an_error_not_a_silent_rebuild() {
    let base = tmp("corrupt");
    let paths = Paths::new(&base);
    save_checkpoint(&paths, &checkpoint(addresses(50), 0)).unwrap();

    std::fs::write(&paths.index, b"not a keyword index at all").unwrap();
    assert!(
        load(&paths).is_err(),
        "a garbled file must be reported, not treated as absent"
    );
}

#[test]
fn paths_hang_off_one_base() {
    let paths = Paths::new(Path::new("data/keyword"));
    assert_eq!(paths.index, Path::new("data/keyword.index"));
    assert_eq!(paths.keys, Path::new("data/keyword.keys"));
}

#[test]
fn restored_capacity_counts_retained_vacant_and_new_slots_like_restore() {
    let base = tmp("dry-run");
    let paths = Paths::new(&base);
    let all = addresses(6);
    let old = all[1..4].to_vec();
    save_checkpoint(&paths, &checkpoint(old.clone(), 0)).unwrap();
    let saved = load(&paths).unwrap().unwrap();
    let map: crate::publish::PirSnapshot = [old[0], old[2], all[4], all[5]]
        .into_iter()
        .map(|address| {
            (
                address,
                crate::map::Entry {
                    usdt: 1,
                    ..Default::default()
                },
            )
        })
        .collect();
    let report = dry_run_allocation(&saved, &map, 5).unwrap();
    assert_eq!(report.occupied, 2);
    assert_eq!(report.vacant, 1);
    assert_eq!(report.appended, 2);
    assert_eq!(report.final_slots, 5);
    assert!(dry_run_allocation(&saved, &map, 4).is_err());
}
