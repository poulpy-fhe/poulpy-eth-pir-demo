use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};

use super::{BalanceMap, SnapshotLock};

/// Install a staged snapshot without ever copying over the serving path. The
/// caller supplies normalized paths; this function holds the same lock as
/// `serve`, validates the copied temporary file strictly, then renames it.
pub fn install_snapshot(source: &Path, destination: &Path) -> Result<BalanceMap> {
    install_snapshot_with(source, destination, io::copy, super::snapshot::fsync_parent)
}

fn install_snapshot_with<C, S>(
    source: &Path,
    destination: &Path,
    copy: C,
    sync_parent: S,
) -> Result<BalanceMap>
where
    C: FnOnce(
        &mut io::BufReader<std::fs::File>,
        &mut io::BufWriter<std::fs::File>,
    ) -> io::Result<u64>,
    S: FnOnce(&Path) -> io::Result<()>,
{
    anyhow::ensure!(
        source != destination,
        "snapshot source and destination are the same path"
    );
    let _lock = SnapshotLock::acquire(destination)?;
    anyhow::ensure!(
        !destination.exists(),
        "destination {destination:?} already exists; refusing to overwrite serving state"
    );
    if let Some(parent) = super::snapshot::parent_to_create(destination) {
        std::fs::create_dir_all(parent)?;
    }

    let (temporary, file) = super::snapshot::create_unique_temp(destination)?;
    let result = install_through_temp(source, destination, &temporary, file, copy, sync_parent);
    if result.is_err() {
        std::fs::remove_file(&temporary).ok();
    }
    result
}

fn install_through_temp<C, S>(
    source: &Path,
    destination: &Path,
    temporary: &Path,
    file: std::fs::File,
    copy: C,
    sync_parent: S,
) -> Result<BalanceMap>
where
    C: FnOnce(
        &mut io::BufReader<std::fs::File>,
        &mut io::BufWriter<std::fs::File>,
    ) -> io::Result<u64>,
    S: FnOnce(&Path) -> io::Result<()>,
{
    let mut input = io::BufReader::new(
        std::fs::File::open(source)
            .with_context(|| format!("opening staged snapshot {source:?}"))?,
    );
    let mut output = io::BufWriter::new(file);
    copy(&mut input, &mut output)
        .with_context(|| format!("copying {source:?} to temporary file {temporary:?}"))?;
    output.flush()?;
    output.into_inner()?.sync_all()?;

    let loaded = BalanceMap::load_strict(temporary)
        .with_context(|| format!("validating transferred snapshot {temporary:?}"))?;
    anyhow::ensure!(
        !destination.exists(),
        "destination {destination:?} appeared during transfer; refusing to overwrite it"
    );
    std::fs::rename(temporary, destination)
        .with_context(|| format!("installing verified snapshot at {destination:?}"))?;
    sync_parent(destination)?;
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use std::cell::Cell;
    use std::io::Read;

    fn root(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("usdt-pir-install-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn staged(root: &Path) -> (std::path::PathBuf, BalanceMap) {
        let path = root.join("uploaded.snapshot");
        let mut map = BalanceMap::new(20_000_000);
        map.seed(
            Address::repeat_byte(0x78),
            crate::map::Entry {
                usdt: 9,
                usdt_block: 19_999_999,
                ..Default::default()
            },
        );
        map.save(&path).unwrap();
        (path, map)
    }

    #[test]
    fn verified_snapshot_is_portable_to_a_different_path() {
        let root = root("portable");
        let (source, expected) = staged(&root);
        let destination = root.join("data/serving.snapshot");
        let installed = install_snapshot(&source, &destination).unwrap();
        assert!(installed.semantically_eq(&expected));
        assert!(
            BalanceMap::load_strict(&destination)
                .unwrap()
                .semantically_eq(&expected)
        );
    }

    #[test]
    fn corrupt_or_interrupted_transfer_never_creates_destination() {
        let root = root("corrupt");
        let source = root.join("partial.upload");
        std::fs::write(&source, b"USDTPIR3truncated").unwrap();
        let destination = root.join("serving.snapshot");
        assert!(install_snapshot(&source, &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn transfer_interruption_removes_partial_temporary_file() {
        let root = root("interrupted-copy");
        let (source, _) = staged(&root);
        let destination = root.join("serving.snapshot");

        let error = install_snapshot_with(
            &source,
            &destination,
            |input, output| {
                let mut prefix = [0_u8; 16];
                input.read_exact(&mut prefix)?;
                output.write_all(&prefix)?;
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "injected transfer interruption",
                ))
            },
            super::super::snapshot::fsync_parent,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("injected transfer interruption"));
        assert!(!destination.exists());
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains("serving.snapshot.tmp."))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "partial files remain: {leftovers:?}");
    }

    #[test]
    fn destination_parent_is_synced_only_after_verified_rename() {
        let root = root("parent-sync");
        let (source, expected) = staged(&root);
        let destination = root.join("data/serving.snapshot");
        let parent_syncs = Cell::new(0_u8);

        let installed = install_snapshot_with(&source, &destination, io::copy, |path| {
            assert_eq!(path, destination);
            assert!(
                BalanceMap::load_strict(path)
                    .unwrap()
                    .semantically_eq(&expected),
                "the durable-directory boundary must follow the verified rename"
            );
            parent_syncs.set(parent_syncs.get() + 1);
            super::super::snapshot::fsync_parent(path)
        })
        .unwrap();

        assert!(installed.semantically_eq(&expected));
        assert_eq!(parent_syncs.get(), 1);
    }

    #[test]
    fn existing_destination_and_live_server_lock_are_both_refused() {
        let root = root("refusal");
        let (source, _) = staged(&root);
        let destination = root.join("serving.snapshot");
        std::fs::write(&destination, b"existing").unwrap();
        assert!(install_snapshot(&source, &destination).is_err());
        std::fs::remove_file(&destination).unwrap();

        let _held = SnapshotLock::acquire(&destination).unwrap();
        let error = install_snapshot(&source, &destination).unwrap_err();
        assert!(format!("{error:#}").contains("already writing"));
        assert!(!destination.exists());
    }
}
