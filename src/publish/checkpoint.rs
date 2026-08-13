use anyhow::Result;
use eth_pir::EthPirServer;

use crate::record::UsdtUsdc;

use super::PirSnapshot;

/// Restore the saved keyword index if there is one, else build a fresh MPHF and
/// save it.
///
/// Restoring keeps clients working across a restart: checkpointed addresses
/// keep their slots, and only addresses added since the checkpoint are appended
/// for clients to pick up as an ordinary delta tail.
pub(super) fn open_server(
    paths: &crate::keyword_store::Paths,
    map: &PirSnapshot,
) -> Result<EthPirServer<UsdtUsdc>> {
    match crate::keyword_store::load(paths)? {
        Some(saved) => restore_server(&saved, map),
        None => init_server(paths, map),
    }
}

fn restore_server(
    saved: &crate::keyword_store::Loaded,
    map: &PirSnapshot,
) -> Result<EthPirServer<UsdtUsdc>> {
    tracing::info!(
        version = saved.version,
        keys = saved.keys.len(),
        "restoring the saved keyword index"
    );
    let (server, report) = EthPirServer::<UsdtUsdc>::restore(&saved.directory, &saved.keys, map)
        .map_err(|e| anyhow::anyhow!("restoring the keyword index: {e}"))?;
    tracing::info!(
        placed = report.placed,
        appended = report.appended,
        vacant = report.vacant,
        version = server.keyword().version(),
        "keyword index restored; clients keep their slots"
    );
    Ok(server)
}

fn init_server(
    paths: &crate::keyword_store::Paths,
    map: &PirSnapshot,
) -> Result<EthPirServer<UsdtUsdc>> {
    tracing::info!(
        addresses = map.len(),
        "no saved keyword index; building one (clients must resync)"
    );
    let server = EthPirServer::<UsdtUsdc>::init(map)
        .map_err(|e| anyhow::anyhow!("initialising the PIR database: {e}"))?;
    save_checkpoint(paths, &server);
    Ok(server)
}

pub(super) fn save_checkpoint(
    paths: &crate::keyword_store::Paths,
    server: &EthPirServer<UsdtUsdc>,
) {
    let checkpoint = match server.checkpoint() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("could not build a keyword checkpoint: {e}");
            return;
        }
    };
    match crate::keyword_store::save_checkpoint(paths, &checkpoint) {
        Ok(()) => tracing::info!(
            version = checkpoint.version,
            keys = checkpoint.keys.len(),
            "saved the keyword index"
        ),
        Err(e) => tracing::error!("could not save the keyword index: {e:#}"),
    }
}

/// Only ever called after a rebuild: an index absorbed but not yet rebuilt
/// points at a record the served database does not carry.
///
/// The blob is written to disk before it is exposed. If the write fails we keep
/// serving the previous generation rather than hand out slots we could not
/// persist.
pub(super) fn publish_directory(
    handle: &crate::directory::Handle,
    paths: &crate::keyword_store::Paths,
    server: &EthPirServer<UsdtUsdc>,
) {
    let blob = match server.keyword().try_full() {
        Ok(blob) => blob,
        Err(e) => {
            tracing::error!("serializing the keyword directory: {e}");
            return;
        }
    };
    if let Err(e) = crate::keyword_store::save_index(paths, &blob) {
        tracing::error!("could not persist the keyword directory, not publishing it: {e:#}");
        return;
    }
    match crate::directory::Directory::from_blob(blob) {
        Ok(directory) => crate::directory::publish(handle, directory),
        Err(e) => tracing::error!("parsing the keyword directory: {e}"),
    }
}
