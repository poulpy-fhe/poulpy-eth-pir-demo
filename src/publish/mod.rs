//! The PIR half: absorb balance changes and publish them on a fixed cadence.
//!
//! This runs on its own OS thread, not a tokio task. eth-pir is synchronous and
//! CPU-bound, so putting it on the runtime would stall the syncer and anything
//! else sharing it. The channel is also where this can split into its own
//! process if the PIR server ever moves to another host.
//!
//! Absorbing an update is cheap and immediate. Publishing re-encodes the whole
//! database and reruns its precomputation, so updates are batched on a timer.
//! Nothing absorbed is retrievable until the next publish.

mod checkpoint;
mod config;
mod worker;

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use eth_pir::{EthPirResponder, EthPirServer};

pub use config::PirConfig;

use crate::map::{BalanceMap, Entry};
use crate::record::{UsdtUsdc, keyword};

type Ready = std::result::Result<EthPirResponder, String>;

/// One pass worth of changed entries, as handed over by the syncer.
pub type UpdateBatch = Vec<(eth_pir::Address, Entry)>;
pub type PirSnapshot = HashMap<eth_pir::Address, Entry>;

/// A running PIR server: the query handle, plus the thread that owns it.
pub struct Pir {
    pub responder: EthPirResponder,
    /// Directory generation the HTTP layer serves, refreshed after each publish.
    pub directory: crate::directory::Handle,
    pub handle: JoinHandle<()>,
}

/// Build the PIR database from `map`, then keep it current from `updates`.
///
/// Blocks until the database is built and serving, because there is no useful
/// thing to do with a half-initialised one.
pub fn spawn(map: &BalanceMap, cfg: PirConfig, updates: Receiver<UpdateBatch>) -> Result<Pir> {
    let initial = snapshot(map);
    log_initial_build(&initial);

    let directory = crate::directory::handle(crate::directory::Directory::empty()?);
    let handle = spawn_thread(cfg, updates, initial, directory.clone())?;
    let responder = wait_until_ready(handle.ready)?;

    Ok(Pir {
        responder,
        directory,
        handle: handle.thread,
    })
}

pub fn snapshot(map: &BalanceMap) -> PirSnapshot {
    map.iter().map(|(a, e)| (keyword(a), *e)).collect()
}

fn log_initial_build(initial: &PirSnapshot) {
    tracing::info!(
        addresses = initial.len(),
        "building the PIR database; this allocates gigabytes and takes a while"
    );
}

struct Spawned {
    thread: JoinHandle<()>,
    ready: std::sync::mpsc::Receiver<Ready>,
}

fn spawn_thread(
    cfg: PirConfig,
    updates: Receiver<UpdateBatch>,
    initial: PirSnapshot,
    directory: crate::directory::Handle,
) -> Result<Spawned> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("pir".into())
        .spawn(move || run_thread(cfg, updates, initial, directory, ready_tx))
        .context("spawning the PIR thread")?;
    Ok(Spawned {
        thread,
        ready: ready_rx,
    })
}

fn run_thread(
    cfg: PirConfig,
    updates: Receiver<UpdateBatch>,
    initial: PirSnapshot,
    directory: crate::directory::Handle,
    ready_tx: Sender<Ready>,
) {
    let started = Instant::now();
    let server = match checkpoint::open_server(&cfg.keyword, &initial) {
        Ok(server) => server,
        Err(e) => {
            return_initial_error(ready_tx, e);
            return;
        }
    };
    log_ready(&server, started);
    checkpoint::publish_directory(&directory, &cfg.keyword, &server);
    if ready_tx.send(Ok(server.responder())).is_ok() {
        worker::serve(server, cfg, updates, initial, directory);
    }
}

fn return_initial_error(ready_tx: Sender<Ready>, error: anyhow::Error) {
    let _ = ready_tx.send(Err(format!("{error:#}")));
}

fn log_ready(server: &EthPirServer<UsdtUsdc>, started: Instant) {
    tracing::info!(
        addresses = server.len(),
        elapsed = ?started.elapsed(),
        "PIR database ready"
    );
}

fn wait_until_ready(ready_rx: std::sync::mpsc::Receiver<Ready>) -> Result<EthPirResponder> {
    ready_rx
        .recv()
        .context("the PIR thread died before reporting readiness")?
        .map_err(|e| anyhow::anyhow!("initialising the PIR database: {e}"))
}
