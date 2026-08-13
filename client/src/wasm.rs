//! wasm-bindgen surface. Bytes in, bytes out; JS owns the transport.

use wasm_bindgen::prelude::*;

use crate::{Client, SyncNeed};

#[wasm_bindgen(js_name = UsdtPirClient)]
pub struct WasmClient {
    inner: Client,
}

/// What the client must fetch next: `"up-to-date"`, `"full"`, or `"tail"`.
#[wasm_bindgen(getter_with_clone)]
pub struct SyncPlan {
    pub action: String,
    /// Offset to request the tail from; `0` unless `action == "tail"`.
    pub from: usize,
}

#[wasm_bindgen(getter_with_clone)]
pub struct Query {
    pub id: u32,
    pub bytes: Vec<u8>,
}

#[wasm_bindgen(js_class = UsdtPirClient)]
impl WasmClient {
    /// Bootstrap from the server's full directory blob.
    #[wasm_bindgen(constructor)]
    pub fn new(directory: &[u8]) -> Result<WasmClient, JsError> {
        Ok(Self {
            inner: Client::new(directory).map_err(to_js)?,
        })
    }

    #[wasm_bindgen(getter)]
    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    #[wasm_bindgen(getter, js_name = tailLen)]
    pub fn tail_len(&self) -> usize {
        self.inner.tail_len()
    }

    #[wasm_bindgen(js_name = syncNeed)]
    pub fn sync_need(&self, server_version: u64, server_tail_len: usize) -> SyncPlan {
        match self.inner.sync_need(server_version, server_tail_len) {
            SyncNeed::UpToDate => SyncPlan {
                action: "up-to-date".into(),
                from: 0,
            },
            SyncNeed::Tail { from } => SyncPlan {
                action: "tail".into(),
                from,
            },
            SyncNeed::Full => SyncPlan {
                action: "full".into(),
                from: 0,
            },
        }
    }

    /// The database slot an address resolves to. Diagnostics only.
    pub fn slot(&self, address: &str) -> Result<usize, JsError> {
        self.inner.slot(address).map_err(to_js)
    }

    #[wasm_bindgen(js_name = applyTail)]
    pub fn apply_tail(&mut self, tail: &[u8]) -> Result<(), JsError> {
        self.inner.apply_tail(tail).map_err(to_js)
    }

    pub fn resync(&mut self, directory: &[u8]) -> Result<(), JsError> {
        self.inner.resync(directory).map_err(to_js)
    }

    /// Build an encrypted query. Returns the id to pass back to `decode`.
    pub fn query(&mut self, address: &str) -> Result<Query, JsError> {
        let q = self.inner.query(address).map_err(to_js)?;
        Ok(Query {
            id: q.id,
            bytes: q.bytes,
        })
    }

    /// Decrypt a response into a JSON report.
    pub fn decode(&mut self, id: u32, response: &[u8]) -> Result<String, JsError> {
        Ok(self.inner.decode(id, response).map_err(to_js)?.to_json())
    }

    pub fn cancel(&mut self, id: u32) -> bool {
        self.inner.cancel(id)
    }

    #[wasm_bindgen(getter, js_name = pendingCount)]
    pub fn pending_count(&self) -> usize {
        self.inner.pending_count()
    }
}

fn to_js(e: crate::ClientError) -> JsError {
    JsError::new(&e.to_string())
}
