//! WASM-compatible PIR client for USDT/USDC balance lookups.
//!
//! Transport-agnostic: every method takes or returns bytes, and the caller moves
//! them. Nothing here opens a socket, so the same code runs in a browser, in
//! Node, and in a native test.
//!
//! The three things a lookup needs, in order:
//!
//! 1. **The keyword directory.** [`Client::sync_need`] says whether to fetch the
//!    full blob or only the append-only tail; feed the bytes to
//!    [`Client::resync`] or [`Client::apply_tail`].
//! 2. **A query.** [`Client::query`] maps the address to an index through the
//!    directory and encrypts it. The index never leaves the client.
//! 3. **A response.** [`Client::decode`] decrypts it and checks the record's
//!    address prefix against the one queried.

mod error;
mod report;

#[cfg(target_family = "wasm")]
mod wasm;

use std::collections::HashMap;

use alloy_primitives::Address;
use eth_pir::{EthPirClient, EthPirError, LookupState};
use usdt_pir_record::UsdtUsdc;

pub use error::ClientError;
pub use report::{Report, TokenBalance};

/// What the client must fetch to be current with the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncNeed {
    UpToDate,
    /// Fetch the delta tail from this offset and pass it to
    /// [`Client::apply_tail`].
    Tail {
        from: usize,
    },
    /// Fetch the full directory blob and pass it to [`Client::resync`].
    ///
    /// A version change means the MPHF was rebuilt, so every index may have
    /// moved and an append-only tail is not meaningful.
    Full,
}

/// A query awaiting its response.
pub struct PendingQuery {
    pub id: u32,
    pub bytes: Vec<u8>,
}

pub struct Client {
    inner: EthPirClient<UsdtUsdc>,
    pending: HashMap<u32, (LookupState, String)>,
    next_id: u32,
}

impl Client {
    /// Bootstrap from the server's full directory blob.
    pub fn new(directory: &[u8]) -> Result<Self, ClientError> {
        Ok(Self {
            inner: EthPirClient::<UsdtUsdc>::try_new(directory)?,
            pending: HashMap::new(),
            next_id: 0,
        })
    }

    /// Bootstrap at a caller-chosen PIR shape. The server must use the same one;
    /// nothing on the wire identifies it.
    pub fn with_shape(
        config: poulpy_pir::config::Config<poulpy_pir::payload::U512P65536>,
        layout: poulpy_pir::database::DatabaseLayout<poulpy_pir::payload::U512P65536>,
        directory: &[u8],
    ) -> Result<Self, ClientError> {
        Ok(Self {
            inner: EthPirClient::<UsdtUsdc>::try_with_shape(config, layout, directory)?,
            pending: HashMap::new(),
            next_id: 0,
        })
    }

    /// The MPHF generation this client holds.
    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// How much of the append-only tail this client already holds.
    pub fn tail_len(&self) -> usize {
        self.inner.tail_len()
    }

    pub fn sync_need(&self, server_version: u64, server_tail_len: usize) -> SyncNeed {
        if server_version != self.version() {
            SyncNeed::Full
        } else if server_tail_len > self.tail_len() {
            SyncNeed::Tail {
                from: self.tail_len(),
            }
        } else {
            SyncNeed::UpToDate
        }
    }

    /// The database slot an address resolves to. Diagnostics only; a real
    /// lookup goes through [`query`](Self::query) and [`decode`](Self::decode).
    pub fn slot(&self, address: &str) -> Result<usize, ClientError> {
        Ok(self.inner.slot(&parse_address(address)?.into_array()))
    }

    /// Apply a delta tail envelope.
    pub fn apply_tail(&mut self, tail: &[u8]) -> Result<(), ClientError> {
        self.inner.try_apply_tail(tail)?;
        Ok(())
    }

    /// Replace the directory after a server-side MPHF rebuild.
    ///
    /// Pending queries are dropped: their indices were resolved through the old
    /// MPHF and mean nothing under the new one.
    pub fn resync(&mut self, directory: &[u8]) -> Result<(), ClientError> {
        self.inner.try_resync(directory)?;
        self.pending.clear();
        Ok(())
    }

    /// Build an encrypted query for an address.
    pub fn query(&mut self, address: &str) -> Result<PendingQuery, ClientError> {
        let addr = parse_address(address)?;
        let (bytes, lookup) = self.inner.try_query_bytes(addr.into_array())?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(id, (lookup, addr.to_checksum(None)));
        Ok(PendingQuery { id, bytes })
    }

    /// Decrypt a response and verify it against the address that was queried.
    pub fn decode(&mut self, id: u32, response: &[u8]) -> Result<Report, ClientError> {
        let (lookup, address) = self
            .pending
            .remove(&id)
            .ok_or(ClientError::UnknownQuery(id))?;
        match self.inner.try_decrypt_bytes(response, &lookup) {
            Ok(entry) => Ok(Report::found(address, entry)),
            Err(EthPirError::NotInSet) => Ok(Report::not_held(address)),
            Err(e) => Err(e.into()),
        }
    }

    /// Drop a pending query without decoding it.
    pub fn cancel(&mut self, id: u32) -> bool {
        self.pending.remove(&id).is_some()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Accepts all-lowercase, all-uppercase, or a valid EIP-55 mixed-case address.
fn parse_address(s: &str) -> Result<Address, ClientError> {
    let body = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if body.len() != 40 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ClientError::BadAddress(s.to_string()));
    }

    let mixed = body.bytes().any(|b| b.is_ascii_uppercase())
        && body.bytes().any(|b| b.is_ascii_lowercase());
    let prefixed = format!("0x{body}");
    if mixed {
        Address::parse_checksummed(&prefixed, None)
            .map_err(|_| ClientError::BadAddress(format!("{s} (EIP-55 checksum mismatch)")))
    } else {
        prefixed
            .parse::<Address>()
            .map_err(|_| ClientError::BadAddress(s.to_string()))
    }
}

#[cfg(test)]
mod tests;
