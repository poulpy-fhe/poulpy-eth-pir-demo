//! Pool queries arriving close together and answer them in one database pass.
//!
//! The PIR server holds a mutex and every query walks the whole database, so
//! answering one at a time leaves the machine idle between passes. Batching
//! amortizes that walk: eth-pir measures 8.7 q/s unbatched against 33.7 q/s at
//! batch 64 on its reference host, for about +5 GiB of peak memory.
//!
//! The trade is latency. Every query in a batch waits for the whole batch, so a
//! lone query on an idle server pays up to the full window before it is even
//! dispatched. `--batch-window 0` answers each query on its own instead.
//!
//! Batches run one at a time: the server could not overlap them anyway, and
//! serializing means the next window fills while the current batch computes, so
//! a busy server naturally forms full batches.

use std::time::Duration;

use eth_pir::EthPirResponder;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct BatchConfig {
    /// How long to keep pooling after the first query arrives.
    pub window: Duration,
    /// Never exceed this many queries in one pass, whatever the window.
    pub max: usize,
    /// Queries allowed to wait before new ones are refused outright.
    pub queue_depth: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(1),
            max: 64,
            queue_depth: 256,
        }
    }
}

struct Job {
    query: Vec<u8>,
    reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

#[derive(Clone)]
pub struct Batcher {
    jobs: mpsc::Sender<Job>,
}

/// Why a query could not even be queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The queue is full: the server already holds as much work as it will.
    Overloaded,
    /// The batch worker is gone, so nothing can be answered.
    Stopped,
}

impl Rejected {
    pub fn message(self) -> &'static str {
        match self {
            Self::Overloaded => "server is at capacity; retry shortly",
            Self::Stopped => "query service is not running",
        }
    }
}

impl Batcher {
    pub fn spawn(responder: EthPirResponder, cfg: BatchConfig) -> Self {
        let (jobs, rx) = mpsc::channel(cfg.queue_depth.max(1));
        tokio::spawn(worker(responder, cfg, rx));
        Self { jobs }
    }

    /// Queue a query and wait for its answer.
    ///
    /// Refuses rather than queues without bound: the wait is already a window
    /// plus a database pass, and an unbounded queue only turns a fast rejection
    /// into a slow timeout.
    pub async fn submit(&self, query: Vec<u8>) -> Result<Result<Vec<u8>, String>, Rejected> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .try_send(Job { query, reply })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => Rejected::Overloaded,
                mpsc::error::TrySendError::Closed(_) => Rejected::Stopped,
            })?;
        answer.await.map_err(|_| Rejected::Stopped)
    }
}

async fn worker(responder: EthPirResponder, cfg: BatchConfig, mut rx: mpsc::Receiver<Job>) {
    while let Some(first) = rx.recv().await {
        let batch = collect(&mut rx, first, cfg).await;
        answer(&responder, batch).await;
    }
    tracing::debug!("no more query senders; batch worker exiting");
}

/// Everything already queued, then stragglers until the window closes.
async fn collect(rx: &mut mpsc::Receiver<Job>, first: Job, cfg: BatchConfig) -> Vec<Job> {
    let mut batch = vec![first];
    let deadline = Instant::now() + cfg.window;

    while batch.len() < cfg.max {
        // Take what is waiting without yielding; only wait on the clock once the
        // queue is dry, so a busy server never sits on a timer it does not need.
        match rx.try_recv() {
            Ok(job) => {
                batch.push(job);
                continue;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {}
        }
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(job)) => batch.push(job),
            // Window closed, or every sender went away.
            Ok(None) | Err(_) => break,
        }
    }
    batch
}

async fn answer(responder: &EthPirResponder, batch: Vec<Job>) {
    let size = batch.len();
    let (queries, replies): (Vec<Vec<u8>>, Vec<_>) =
        batch.into_iter().map(|j| (j.query, j.reply)).unzip();

    // Seconds of CPU under a mutex: never on the runtime.
    let responder = responder.clone();
    let started = std::time::Instant::now();
    let answered =
        tokio::task::spawn_blocking(move || responder.try_respond_batch_bytes(&queries)).await;
    let elapsed = started.elapsed();

    match answered {
        Ok(Ok(results)) => {
            for (reply, result) in replies.into_iter().zip(results) {
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            tracing::info!(
                size,
                ?elapsed,
                per_query = ?elapsed.checked_div(size as u32).unwrap_or_default(),
                "answered a batch",
            );
        }
        // The batch itself failed, so no individual query can be blamed for it.
        Ok(Err(e)) => fail(replies, size, e.to_string()),
        Err(e) => fail(replies, size, format!("batch worker panicked: {e}")),
    }
}

fn fail(replies: Vec<oneshot::Sender<Result<Vec<u8>, String>>>, size: usize, message: String) {
    tracing::error!(size, "batch failed: {message}");
    for reply in replies {
        let _ = reply.send(Err(message.clone()));
    }
}
