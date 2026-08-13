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
    /// Delta entries past the MPHF base.
    pub tail_len: usize,
    pub full: Vec<u8>,
    parsed: KeywordDirectory<20>,
}

impl Directory {
    pub fn from_blob(full: Vec<u8>) -> Result<Self> {
        let parsed = KeywordDirectory::<20>::read_from(&mut &full[..])?;
        Ok(Self {
            version: parsed.version(),
            len: parsed.len(),
            tail_len: parsed.delta_len(),
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
            tail_len: 0,
            full,
            parsed,
        })
    }
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
