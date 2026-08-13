# usdt-pir-client

A WASM-compatible PIR client for USDT/USDC balances. It asks the server for one
address's balances without telling it which address.

The Rust core is transport-agnostic: bytes in, bytes out. It never opens a
socket, so the same code runs in a browser, in Node, and in a native test. The
JS wrapper in [`web/pir.js`](web/pir.js) does the fetching.

## What a lookup does

```text
1. sync      GET  /v1/status              -> version, tailLen
             GET  /v1/directory           -> MPHF blob        (bootstrap / rebuild)
             GET  /v1/directory/tail?from -> delta            (same generation)

2. query     address -> slot (locally, through the MPHF)
             slot    -> encrypted query
             POST /v1/query               -> encrypted response

3. decode    decrypt, check the record's address prefix, report balances
```

The slot is computed **on the client**. The server sees an encrypted query of a
fixed size and answers it without learning the index.

## Cost

Measured at the deployed 2 GiB shape over 250,000 addresses, with the portable
backend (the one wasm gets):

| | |
| --- | --- |
| directory download | 65 KB (2.13 bits/address) |
| bootstrap | ~1 ms |
| query generation | 8–17 ms in Node, 3.5 ms native |
| query / response on the wire | 675 KiB / 192 KiB |
| wasm bundle | 401 KB |
| peak memory | ~12 MiB |

The query is the same size whatever the address, so its size leaks nothing.

## Building

```sh
cargo build --release --target wasm32-unknown-unknown -p usdt-pir-client
wasm-bindgen --target web --out-dir client/web \
  target/wasm32-unknown-unknown/release/usdt_pir_client.wasm
```

`--target nodejs` instead of `--target web` for a Node build.

Requires `wasm-bindgen-cli` at the same version as the `wasm-bindgen`
dependency, and the `wasm32-unknown-unknown` target.

## Client/server boundary

This directory is the client side:

- `client/src` is the Rust/WASM client. It accepts bytes and returns bytes; it
  never opens sockets.
- `client/web` is the browser portal. It calls same-origin `/v1/*`.
- `client/tools` and `client/tests` are client-side test helpers.

The server side is the root crate under `src/`. It syncs Ethereum, owns the
snapshot/PIR database, and serves `/v1/*`.

## Running locally

From the repository root:

```sh
ETH_RPC_URL=https://your-mainnet-rpc ./scripts/local-demo.sh
```

That starts:

- backend API: <http://127.0.0.1:8787>
- portal page: <http://127.0.0.1:8080>

The portal serves this directory and proxies `/v1/*` to the backend. The browser
does not talk to the backend port directly.

Manual equivalent:

```sh
RUSTFLAGS="-C target-feature=+avx2,+fma" \
  cargo build --release --features avx2-fhe -p usdt-pir

ETH_RPC_URL=https://... ./target/release/usdt-pir serve \
  --listen 127.0.0.1:8787
```

For local single-process smoke tests, `serve --web client/web` still works:

```sh
./target/release/usdt-pir serve --listen 127.0.0.1:8787 --web client/web
```

Then open <http://127.0.0.1:8787>.

### Security

Nothing here is authenticated or encrypted. Before exposing it:

| | State |
| --- | --- |
| Transport | Plain local HTTP. |
| Authentication | None. Anyone who can reach the port can query. |
| Rate limiting | Basic backend token bucket. |
| Query privacy | The one real guarantee, and it holds against the operator. |

Two things that are easy to get backwards:

**TLS is not what makes a query private.** The server cannot tell which address
was asked for even though it decrypts nothing — that is the PIR construction, not
the transport. Queries are a fixed 675 KiB whatever the address, so size leaks
nothing either.

**But balances are only as trustworthy as the server.** The client checks that
the record it got back carries the address it asked for, which defends against
the MPHF handing it a *different* holder's record. It does not authenticate the
*value*: there is no proof binding a balance to chain state. A malicious or
MITM'd server can return any number for the right address and the client will
display it. Over plain HTTP any network attacker can do that.

## API

```rust
let mut client = Client::new(&directory_blob)?;

match client.sync_need(server_version, server_tail_len) {
    SyncNeed::UpToDate => {}
    SyncNeed::Tail { from } => client.apply_tail(&fetch_tail(from))?,
    SyncNeed::Full       => client.resync(&fetch_full())?,
}

let pending = client.query("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")?;
let report  = client.decode(pending.id, &post(pending.bytes))?;

if report.held {
    println!("{} USDT, {} USDC", report.usdt.amount, report.usdc.amount);
}
```

From JS the same flow is `new UsdtPirClient(blob)`, `syncNeed`, `applyTail` /
`resync`, `query`, `decode`, with `decode` returning a JSON string.

### Addresses

Accepted all-lowercase, all-uppercase, or EIP-55 mixed case; a mixed-case address
with a bad checksum is rejected, which is what catches a mistyped character.
Reports echo the EIP-55 form.

### `held == false` is an answer, not an error

It means the server has no record for that address. The MPHF is *total* — an
address it never indexed still resolves to some occupied slot — so every record
carries its own address and the client compares it before trusting the payload.
Without that check a lookup for an unknown address would return another holder's
balance with nothing to indicate it.

An address that is held but has drained to zero reports `held: true` with zero
balances, which is a different statement from `held: false`.

### Resync invalidates pending queries

A server-side MPHF rebuild permutes every index, so `resync` drops outstanding
queries rather than let them decode against slots that have moved.

## Manual testing

### 1. Without a chain or a server

```sh
cargo test --workspace
```

`client/tests/end_to_end.rs` runs a real server record through query bytes,
response bytes and back to a decoded report, at a small PIR shape. No RPC, no
14 GiB, ~10 s.

### 2. Start the private backend

```sh
RUSTFLAGS="-C target-feature=+avx2,+fma" \
  cargo build --release --features avx2-fhe -p usdt-pir

./client/build.sh web
./client/build.sh nodejs

export ETH_RPC_URL=https://rpc.mevblocker.io
./target/release/usdt-pir serve \
  --listen 127.0.0.1:8787 \
  --confirmations 32 --rebuild-every 30 --chunk 25
```

Wait for `PIR database ready` then `query endpoint listening`. From a cold
snapshot that is a catch-up sync plus ~20 s of PIR build, and it needs ~14 GiB.

### 3. Start the portal

In another terminal:

```sh
python3 scripts/local_portal.py \
  --listen 127.0.0.1:8080 \
  --backend http://127.0.0.1:8787 \
  --web client/web
```

### 4. Poke the backend with curl

```sh
curl -s localhost:8787/v1/status
# {"version":0,"len":266979,"tailLen":1639,"directoryBytes":78384}

curl -s -o /dev/null -w '%{size_download}\n' localhost:8787/v1/directory
curl -s -o /dev/null -w '%{size_download}\n' 'localhost:8787/v1/directory/tail?from=0'

curl -s 'localhost:8787/v1/directory/tail?from=abc'   # from must be a whole number
curl -s --path-as-is localhost:8787/../Cargo.toml     # bad path
```

### 5. The browser

Open the portal, for example <http://127.0.0.1:8080>. Get an address that is
actually held:

```sh
./target/release/usdt-pir sample -n 5
```

Paste one in. Then check the answer against the plaintext map:

```sh
./target/release/usdt-pir lookup <address>
```

The two must agree. Open devtools' network tab while you search: you will see a
675 KiB POST and a 192 KiB response, and no sign of the address anywhere.

Do **not** test with a famous address. The map only holds addresses that have
moved since the sync started, so Binance's hot wallet correctly reports "not
held" and looks like a bug.

### 6. Scripted end-to-end

```sh
node client/tools/e2e.mjs http://127.0.0.1:8787 \
  $(./target/release/usdt-pir sample -n 3 | awk '{print $1}')
```

Checks directory sync, the three `syncNeed` branches, tail application, real
balances, a random address reporting not-held, and every rejection path. Exits
non-zero on failure.

### 7. Cross-architecture agreement

The server is 64-bit and the client is 32-bit, and a hash that disagrees between
them makes every lookup report "not held". Nothing native can catch that, so
compare slots directly:

```sh
cargo run --release -p usdt-pir-client --example fixture -- /tmp/fix 250000
```

That writes a directory blob plus the slots this 64-bit host resolves. A wasm
client loading the same blob must agree on all of them — see the `slot()` method.
`poulpy-pir`'s `the_key_hash_does_not_depend_on_word_size` pins the underlying
hash so a regression fails there first.

## Testing

```sh
cargo test -p usdt-pir-client            # unit + directory + end-to-end
```

`tests/end_to_end.rs` runs the real chain — server record, query bytes, response
bytes, decoded report — at a small PIR shape, so it exercises the deployed record
layout, wire codecs, and not-in-set check without needing 14 GiB.

Cross-architecture agreement is not visible from a native test: the server is
64-bit and the client is 32-bit. `examples/fixture.rs` writes a directory blob
plus the slots this host resolves, for a Node script to check against. The
underlying properties are pinned upstream by
`poulpy-pir`'s `mphf_indices_are_pinned`.

## Why the dependency patches exist

`Cargo.toml` at the workspace root patches `poulpy-pir` to a local path and
`ptr_hash` to a fork. Both are needed for wasm; see
[`../../poulpy-pir/vendor/ptr_hash/FORK.md`](../../poulpy-pir/vendor/ptr_hash/FORK.md).
In short:

- `private-gemm-x86` is raw x86-64 assembly, so it is target-gated in poulpy-pir
  and a portable GEMM fallback covers other targets. It is server-only code; the
  client never calls it.
- `epserde` writes `usize` at the host's width and refuses to read a blob written
  at another, so the MPHF now ships in a fixed-width format instead.
- `std::hash::Hash` for slices prefixes the length with `write_usize`, so the
  same key hashed to different values on 32- and 64-bit hosts. The fork writes
  that prefix at a fixed 64 bits.

The last two were found by testing, not by reading: before the fix, 0 of 40
probe addresses resolved to the same slot in wasm as on the server.
