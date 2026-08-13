//! The keyword directory as the HTTP layer sees it.
//!
//! The live directory belongs to the `EthPirServer` on the PIR thread, which
//! spends seconds at a time inside a rebuild. Serving reads from there would
//! inherit that latency, so the PIR thread instead publishes an owned copy here
//! after every rebuild, and the HTTP layer only ever reads this.
//!
//! Publishing *after* the rebuild is the point: an address absorbed but not yet
//! rebuilt has a directory index whose record is not in the served database yet.
//! Handing that index to a client would answer "not held" for an address that is.

use std::sync::{Arc, RwLock};

use anyhow::Result;
use poulpy_pir::keyword::KeywordDirectory;

/// Shared slot the PIR thread writes and the HTTP layer reads.
pub type Handle = Arc<RwLock<Arc<Directory>>>;

pub struct Directory {
    pub version: u64,
    /// Addresses addressed: MPHF base plus delta.
    pub len: usize,
    /// Addresses addressed by the MPHF base, excluding the append-only tail.
    pub mphf_len: usize,
    /// Serialized MPHF bytes inside the full directory blob.
    pub mphf_bytes: usize,
    /// Delta entries past the MPHF base.
    pub tail_len: usize,
    /// Bytes returned by `/v1/directory/tail?from=0`.
    pub tail_bytes: usize,
    pub full: Vec<u8>,
    parsed: KeywordDirectory<20>,
}

impl Directory {
    pub fn from_blob(full: Vec<u8>) -> Result<Self> {
        let parsed = KeywordDirectory::<20>::read_from(&mut &full[..])?;
        let tail_len = parsed.delta_len();
        Ok(Self {
            version: parsed.version(),
            len: parsed.len(),
            mphf_len: parsed.mphf().len(),
            mphf_bytes: mphf_bytes(full.len(), tail_len),
            tail_len,
            tail_bytes: tail_bytes(tail_len),
            full,
            parsed,
        })
    }

    /// The append-only tail from `from`, in a versioned envelope.
    pub fn tail(&self, from: usize) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.parsed.write_delta_envelope_from(&mut out, from)?;
        Ok(out)
    }

    pub fn empty() -> Result<Self> {
        let parsed =
            KeywordDirectory::<20>::new(poulpy_pir::keyword::KeywordIndex::build(&[])?, 0, 0)?;
        let mut full = Vec::new();
        parsed.write_to(&mut full)?;
        Ok(Self {
            version: parsed.version(),
            len: 0,
            mphf_len: 0,
            mphf_bytes: mphf_bytes(full.len(), 0),
            tail_len: 0,
            tail_bytes: tail_bytes(0),
            full,
            parsed,
        })
    }
}

fn mphf_bytes(full_bytes: usize, tail_len: usize) -> usize {
    full_bytes.saturating_sub(24usize.saturating_add(20usize.saturating_mul(tail_len)))
}

fn tail_bytes(tail_len: usize) -> usize {
    48usize.saturating_add(20usize.saturating_mul(tail_len))
}

pub fn handle(directory: Directory) -> Handle {
    Arc::new(RwLock::new(Arc::new(directory)))
}

pub fn publish(handle: &Handle, directory: Directory) {
    match handle.write() {
        Ok(mut slot) => *slot = Arc::new(directory),
        Err(_) => tracing::error!("directory slot poisoned; clients keep the previous generation"),
    }
}

pub fn current(handle: &Handle) -> Option<Arc<Directory>> {
    handle.read().ok().map(|slot| slot.clone())
}
