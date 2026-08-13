// End-to-end check against a running `usdt-pir serve --listen`.
//
//   node client/tools/e2e.mjs [baseUrl] [address...]
//
// With no addresses it only exercises sync + a not-held lookup. Pass addresses
// from `usdt-pir sample` to check real balances.
//
// Requires the Node bundle: ./client/build.sh nodejs

import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);

const BASE = (process.argv[2] || 'http://127.0.0.1:8787').replace(/\/$/, '');
const ADDRESSES = process.argv.slice(3);

let wasm;
try {
  wasm = require('../pkg-node/usdt_pir_client.js');
} catch {
  console.error('missing client/pkg-node — run: ./client/build.sh nodejs');
  process.exit(2);
}

const get = async (p) => {
  const r = await fetch(BASE + p);
  if (!r.ok) throw new Error(`GET ${p} -> ${r.status} ${await r.text()}`);
  return r;
};
const bytes = async (p) => new Uint8Array(await (await get(p)).arrayBuffer());

function fmt(n) {
  return n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KiB` : `${(n / 1048576).toFixed(2)} MiB`;
}

let failures = 0;
const check = (ok, msg) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${msg}`);
  if (!ok) failures++;
};

// ---------------------------------------------------------------- 1. sync
console.log(`\n[1] directory sync from ${BASE}`);
const status = await (await get('/v1/status')).json();
console.log(`  server: version=${status.version} addresses=${status.len} ` +
            `delta=${status.tailLen} directory=${fmt(status.directoryBytes)}`);

const t0 = performance.now();
const directory = await bytes('/v1/directory');
const client = new wasm.UsdtPirClient(directory);
console.log(`  bootstrapped in ${(performance.now() - t0).toFixed(0)} ms from ${fmt(directory.length)}`);

check(client.version === BigInt(status.version), `client version ${client.version} matches server`);
check(client.syncNeed(BigInt(status.version), status.tailLen).action === 'up-to-date',
      'client reports up-to-date right after bootstrap');

const stale = client.syncNeed(BigInt(status.version), status.tailLen + 10);
check(stale.action === 'tail' && stale.from === client.tailLen,
      `a grown delta asks for a tail from ${stale.from}`);
check(client.syncNeed(BigInt(status.version) + 1n, 0).action === 'full',
      'a version bump asks for a full resync');

const tail = await bytes(`/v1/directory/tail?from=${client.tailLen}`);
client.applyTail(tail);
check(true, `applied a ${fmt(tail.length)} tail`);

// ---------------------------------------------------------------- 2. query
async function lookup(address) {
  const t = performance.now();
  const q = client.query(address);
  const encrypt = performance.now() - t;

  const t1 = performance.now();
  const res = await fetch(`${BASE}/v1/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/octet-stream' },
    body: q.bytes,
  });
  if (!res.ok) throw new Error(`POST /v1/query -> ${res.status} ${await res.text()}`);
  const response = new Uint8Array(await res.arrayBuffer());
  const rtt = performance.now() - t1;

  const t2 = performance.now();
  const report = JSON.parse(client.decode(q.id, response));
  const decrypt = performance.now() - t2;

  return { report, encrypt, rtt, decrypt, sent: q.bytes.length, got: response.length };
}

console.log(`\n[2] lookups`);
for (const address of ADDRESSES) {
  const { report, encrypt, rtt, decrypt, sent, got } = await lookup(address);
  console.log(`  ${report.address}`);
  console.log(`    held=${report.held}  USDT=${report.usdt.amount}  USDC=${report.usdc.amount}` +
              `  asOfBlock=${report.asOfBlock}`);
  console.log(`    sent ${fmt(sent)}, got ${fmt(got)} | encrypt ${encrypt.toFixed(0)}ms` +
              ` server ${rtt.toFixed(0)}ms decrypt ${decrypt.toFixed(0)}ms`);
  check(report.held, 'address is held (pass one from `usdt-pir sample`)');
}

// An address nothing has ever touched must come back not-held, not as some
// other holder's balance.
const random = '0x' + Array.from({ length: 20 }, () =>
  Math.floor(Math.random() * 256).toString(16).padStart(2, '0')).join('');
const { report: absent } = await lookup(random);
check(!absent.held, `a random address reports not held (${random.slice(0, 12)}…)`);

// ---------------------------------------------------------------- 3. errors
console.log(`\n[3] rejections`);
// The last one is a valid address with one case bit flipped: EIP-55's job.
for (const bad of ['0xnothex', '', '0x1234', '0xD8dA6BF26964aF9D7eEd9e03E53415D37aA96045']) {
  let threw = false;
  try { client.query(bad); } catch { threw = true; }
  check(threw, `rejected ${JSON.stringify(bad.slice(0, 24))}`);
}

let threw = false;
try { client.decode(99999, new Uint8Array(8)); } catch { threw = true; }
check(threw, 'rejected an unknown query id');

const short = await fetch(`${BASE}/v1/query`, {
  method: 'POST', headers: { 'content-type': 'application/octet-stream' }, body: new Uint8Array(4) });
check(short.status === 400, `server rejected a malformed query (${short.status})`);

console.log(failures === 0 ? '\nAll checks passed.' : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
