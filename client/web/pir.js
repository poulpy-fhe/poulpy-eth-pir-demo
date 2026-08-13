// Transport for the wasm client: fetches the directory, posts queries.
//
// The wasm module never touches the network. It says what it needs, this fetches
// it, and it decodes what comes back. Only the encrypted query leaves the page —
// the address being looked up does not.

import init, { UsdtPirClient } from './usdt_pir_client.js';

export class PirSession {
  #client = null;
  #base;

  constructor(baseUrl = '') {
    this.#base = baseUrl.replace(/\/$/, '');
  }

  /** What this session talks to; `same origin` when unset. */
  get endpoint() {
    return this.#base || `${location.origin} (same origin)`;
  }

  get ready() {
    return this.#client !== null;
  }

  async status() {
    const res = await fetch(`${this.#base}/v1/status`);
    if (!res.ok) throw new Error(`status: ${res.status} ${await res.text()}`);
    return res.json();
  }

  /** Bootstrap, or bring an existing directory up to date. Returns what it did. */
  async sync(onProgress = () => {}) {
    await init();
    const status = await this.status();

    if (!this.#client) {
      onProgress(`downloading directory (${fmtBytes(status.directoryBytes)})`);
      this.#client = new UsdtPirClient(await this.#bytes('/v1/directory'));
      return { action: 'full', version: status.version, addresses: status.len };
    }

    const plan = this.#client.syncNeed(BigInt(status.version), status.tailLen);
    if (plan.action === 'full') {
      onProgress('server rebuilt its index; refetching directory');
      this.#client.resync(await this.#bytes('/v1/directory'));
    } else if (plan.action === 'tail') {
      onProgress(`fetching ${status.tailLen - plan.from} new addresses`);
      this.#client.applyTail(await this.#bytes(`/v1/directory/tail?from=${plan.from}`));
    }
    return { action: plan.action, version: status.version, addresses: status.len };
  }

  /** Look up one address. Returns the decoded report plus timings. */
  async lookup(address, onProgress = () => {}) {
    if (!this.#client) throw new Error('call sync() first');

    onProgress('encrypting query');
    const t0 = performance.now();
    const query = this.#client.query(address);
    const encrypt = performance.now() - t0;

    onProgress(`sending ${fmtBytes(query.bytes.length)}`);
    const t1 = performance.now();
    const res = await fetch(`${this.#base}/v1/query`, {
      method: 'POST',
      headers: { 'content-type': 'application/octet-stream' },
      body: query.bytes,
    });
    if (!res.ok) {
      this.#client.cancel(query.id);
      throw new Error(`query: ${res.status} ${await res.text()}`);
    }
    const response = new Uint8Array(await res.arrayBuffer());
    const roundTrip = performance.now() - t1;

    onProgress('decrypting response');
    const t2 = performance.now();
    const report = JSON.parse(this.#client.decode(query.id, response));
    const decrypt = performance.now() - t2;

    return {
      report,
      timings: { encrypt, roundTrip, decrypt },
      sizes: { query: query.bytes.length, response: response.length },
    };
  }

  async #bytes(path) {
    const res = await fetch(`${this.#base}${path}`);
    if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
    return new Uint8Array(await res.arrayBuffer());
  }
}

export function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(2)} MiB`;
}
