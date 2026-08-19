# usdt-pir

Syncs USDT and USDC balances from Ethereum mainnet into a local balance map.
The map is intended to back an `eth-pir` database so clients can query balances
privately.

Current state:

- Chain sync is implemented.
- Server/backend code lives in [`src/`](src/): chain sync, snapshot storage,
  PIR publishing, and the `/v1/*` query API.
- Client code lives in [`client/`](client/README.md): a transport-agnostic
  Rust/WASM client plus a small browser portal in `client/web`.
- Shared record layout code lives in [`crates/record/`](crates/record).
- A fresh empty map only tracks addresses that move after it starts, by design:
  a full holder snapshot is imported separately, and syncing forward from a
  chosen block is what this side has to get right.

For local testing, run both sides with:

```sh
./scripts/local-demo.sh
```

That starts the backend on `127.0.0.1:8787` and a local portal on
`127.0.0.1:8080`. The script defaults to `https://rpc.ankr.com/eth`; override
it with `ETH_RPC_URL=...` if you want a different mainnet RPC.

## Sync Model

Logs are used to find addresses. Balances are always read with `balanceOf`.
Transfer amounts are not applied as deltas.

```text
getLogs(USDT, USDC)
  -> touched addresses
balanceOf(address) at the range end
  -> absolute USDT and USDC balances
apply to BalanceMap
  -> insert or update snapshot rows
```

Watched events:

| Event | Reason |
| --- | --- |
| `Transfer` | Normal balance movement for both tokens. |
| `DestroyedBlackFunds` | USDT can clear a blacklisted balance without `Transfer`. |
| `Issue` / `Redeem` | USDT changes owner balance without `Transfer`. |

Ignored cases:

- zero address
- zero-value transfers
- self-transfers
- malformed address topics

Addresses with zero USDT and zero USDC are not kept in the snapshot map. New
addresses with zero balances are not inserted, and addresses inserted and
drained inside the same sync are dropped before publishing. If an address that
was already published drains to zero, the plaintext map removes it and the PIR
worker receives a zero record so it does not keep serving the old balance. The
old PIR keyword is removed only at keyword-index rebuild time, when clients
already have to resync the MPHF; that rebuild is made from the current holder
map.

## Building

```sh
RUSTFLAGS="-C target-feature=+avx2,+fma" \
  cargo build --release --features avx2-fhe
```

The binary lands at `./target/release/usdt-pir`. Running that file directly does
not rebuild it; for local testing, prefer `./scripts/run-release.sh ...`, which
rebuilds first and then forwards the command. To get `usdt-pir` on your `PATH`
instead:

```sh
RUSTFLAGS="-C target-feature=+avx2,+fma" \
  cargo install --path . --features avx2-fhe
```

`serve` needs the AVX2 build; without `--features avx2-fhe` it falls back to
poulpy's portable backend, which is correct but far slower. The other commands
never touch the FHE path, so a plain `cargo build --release` is enough for
`sync`, `follow`, `lookup`, `stat`, and `sample`.

## Commands

All commands use `--rpc` or `ETH_RPC_URL`.
The default snapshot path is `data/balances.snapshot`.
Only one process may write a given snapshot at a time — two syncs against the
same `--state` will clobber each other's progress.

### `follow`

Run the chain syncer only.

```sh
./scripts/run-release.sh follow --from-block finalized
```

Useful flags:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--state` | `data/balances.snapshot` | Snapshot file. |
| `--from-block` | required for new snapshots | First block to scan, or `finalized`. |
| `--confirmations` | `finalized` | Tip policy. Use a number to track `head - N`. |
| `--poll-interval` | `12` | Seconds between tip checks. |
| `--snapshot-every` | `600` | Seconds between snapshot writes. |
| `--reorg-window` | `64` | Blocks re-read after a reorg under numeric confirmations. |
| `--chunk` | `50` | Max blocks per `getLogs`; reduced automatically on provider caps. |

### `sync`

Bring the snapshot up to date, then exit. This is the standalone catch-up.

```sh
./scripts/run-release.sh sync                       # resume the snapshot -> finalized
./scripts/run-release.sh sync --to latest           # to the head, reorg-exposed
./scripts/run-release.sh sync --from 25735201       # replay a range
```

With an existing snapshot and no `--from`, it resumes at `cursor + 1`; `--to`
defaults to `finalized`. `--from` is inclusive. Re-running a range is safe
because balances are re-read as absolute values — replaying an old range does
not restore old balances, it only refreshes addresses that moved in it.

A long catch-up will meet a rate limit or a dropped connection, so a failed
range is retried after a fixed short delay, resuming from the cursor rather than
the start.
Progress is saved after every attempt and again if the command gives up, so
re-running always resumes.

`--retries` (default 10) bounds *consecutive failures that advanced nothing*,
not total failures. A throttled endpoint fails constantly while still moving the
cursor hundreds of blocks at a time; counting those would abandon a range that
was steadily completing. Each attempt that advances resets the stalled counter
and shortens what is left, so it still terminates.

It does **not** check for reorgs: it has no previous block hash to compare
against. Syncing to `finalized` makes that moot. `--to latest` warns, and any
balance read from a block that later reorgs away stays wrong until that address
moves again. Use `follow` or `serve` if you want the reorg tripwire.

### `serve`

Run the syncer and keep an `eth-pir` database updated.

```sh
./scripts/run-release.sh serve --from-block finalized \
  --confirmations 32 \
  --poll-interval 12 \
  --rebuild-every 30 \
  --compact-tail-percent 100 \
  --listen 127.0.0.1:8787
```

`serve` catches the snapshot up before building the PIR database. The PIR worker
runs on a dedicated OS thread because rebuilds are CPU-bound. A transient RPC
failure during catch-up is retried after a fixed short delay rather than
propagated, so it cannot kill the process before the endpoint opens.

For the browser demo, keep the backend and portal as separate pieces. The
portal serves `client/web` and proxies `/v1/*` to `127.0.0.1:8787`; browsers
only call the portal origin. The local helper script does that on localhost.

Important flags:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--rebuild-every` | `30` | Seconds between PIR database publishes. |
| `--compact-after` | `200000` | New addresses before keyword index compaction. |
| `--compact-tail-percent` | `100` | Compact when the tail download reaches this percentage of the MPHF blob. `0` disables this trigger. |
| `--listen` | none | Private API bind address. Without it nothing can query. |
| `--web` | none | Local single-process smoke test only. |
| `--batch-window` | `1000` | Milliseconds to pool queries before one database pass. `0` answers each alone. |
| `--max-batch` | `64` | Most queries in one pass. |
| `--queue-depth` | `256` | Queries allowed to wait before new ones get `503`. |
| `--rate-limit` | `60` | Sustained queries per minute per client IP. `0` disables. |
| `--rate-burst` | `10` | Queries one client may issue back-to-back. |

## Query Endpoint

The browser portal calls same-origin `/v1/*`. For local testing,
`scripts/local_portal.py` serves `client/web` and proxies those requests to the
backend. A Netlify function provides the same proxy for the hosted portal; see
[`client/README.md`](client/README.md#deploying-the-portal-on-netlify). The
backend does not enable CORS.

| Route | Returns |
| --- | --- |
| `GET /v1/status` | directory generation plus chain freshness, below |
| `GET /v1/directory` | Full keyword directory blob. |
| `GET /v1/directory/tail?from=N` | Append-only delta from `N`. |
| `POST /v1/query` | One PIR response for one PIR query. |

A client reads `/v1/status`, fetches the full directory if `version` differs from
its own or the delta otherwise, then posts queries. See
[`client/README.md`](client/README.md).

```json
{
  "version": 0, "len": 342511, "tailLen": 151, "directoryBytes": 73344,
  "cursor": 25744330, "tip": 25744362, "lagBlocks": 32, "lastSyncAgeSecs": 7
}
```

The last four are the chain half, and they are what a monitor should watch.
Without them a stalled syncer is invisible: `serve` keeps answering queries from
the map it last built, so an RPC that died hours ago looks exactly like a quiet
market. Alarm on `lagBlocks` and `lastSyncAgeSecs` together — lag alone stays
flat if the tip stops being fetched at all.

`lastSyncAgeSecs` is `null`, not `0`, before the first pass completes: "never
synced" and "synced a moment ago" must not look alike. `lagBlocks` is the gap to
the tip the syncer was aiming at, so under `--confirmations 32` it sits at about
32 when healthy, not 0.

The directory the endpoint serves is a copy published by the PIR thread after
each rebuild, so serving never waits on one. It is published *after* the rebuild
on purpose: an address absorbed but not yet rebuilt has an index whose record is
not in the served database, and handing that out would answer "not held" for an
address that is.

### Batching

Every query walks the whole database under one mutex, so answering them one at a
time leaves the machine idle between passes. Queries arriving within
`--batch-window` are answered in a single pass. Batches run one at a time — the
server cannot overlap them anyway — so the next window fills while the current
batch computes and a busy server naturally forms full batches.

A malformed query fails on its own rather than failing the batch: one bad request
in a 64-query window must not deny the other 63.

Measured here (24-thread i9-12900K, AVX2, 350 K addresses), with no rebuild
running:

| Batch | Per query | Throughput |
| --- | --- | --- |
| 1 | 148 ms | ~6.7 q/s |
| 8 | 181 ms | ~5.5 q/s |
| 64 | 99 ms | ~9.7 q/s |

So about **1.5x** here, not the 3.9x eth-pir measures on its 64-core AVX-512
reference host — a single query already keeps 24 threads busy, so there is less
idle time for batching to reclaim. Expect more benefit on wider machines.

The cost is latency: every query waits for its whole batch, and a lone query on
an idle server waits the full window before it is even dispatched (1.16 s
measured at the 1 s default). `--batch-window 0` answers each query alone.

**Rebuilds dominate this trade.** A publish takes 13-20 s of precompute here and
`--rebuild-every` defaults to 30 s, so the server spends roughly half its time
rebuilding, and batches that overlap one take 15 s instead of 6.3 s. On this
hardware raise `--rebuild-every` to 60-120 s if query latency matters more than
freshness.

### Rate limiting

`--rate-limit` is a per-client-IP token bucket: a burst of `--rate-burst` is
allowed, then the sustained rate binds. Over the limit returns `429` with
`Retry-After`. Saturating `--queue-depth` returns `503` instead — the limiter
bounds one client's share, the queue bounds total work in flight, and neither
substitutes for the other.

Behind a portal proxy every request appears to come from the proxy, so the
backend's per-IP limiting sees one client. `X-Forwarded-For` is deliberately not
trusted: without knowing the proxy is there, honouring it would let anyone forge
their identity.

Current measured cost on a 24-thread i9-12900K with AVX2 and 250K addresses:

| Operation | Cost |
| --- | --- |
| PIR cold start | 17.3 s |
| Publish | 13.4 s |
| Resident memory | 13.7 GiB |
| Peak memory | 15.4 GiB |

### `lookup` and `stat`

Inspect the local snapshot. These commands are not private.

```sh
./target/release/usdt-pir lookup 0xF977814e90dA44bFA03b6295A0616a897441aceC
./target/release/usdt-pir stat
```

## Tip Policy

`--confirmations finalized` reads finalized blocks only. This is about 13
minutes behind the head and does not need reorg repair.

`--confirmations N` reads block `head - N`. This gives fresher data. Reorgs are
detected by checking the last synced block hash. If the hash changed, the syncer
rewinds up to `--reorg-window` blocks and re-reads affected addresses.

## RPC Requirements

The RPC endpoint must be Ethereum mainnet. The program checks `eth_chainId == 1`
before touching the snapshot.

The endpoint must serve:

- historical logs for the synced range
- historical state for `balanceOf` at each chunk end

Many free endpoints do not provide enough history. `preflight` checks this
before a long sync starts.

Full historical backfill cannot use this path from genesis. Balance reads are
batched through Multicall3, which exists on mainnet from block `14,353,601`.
USDC and USDT are older. Use an external bootstrap snapshot for older history,
then sync forward.

## Snapshot

`data/balances.snapshot` is the durable state.

Format:

```text
magic(8) | cursor(u64) | count(u64) | rows...
```

Each row is 60 bytes:

```text
address(20) | usdt(u128) | usdt_block(u32) | usdc(u128) | usdc_block(u32)
```

Files are written to a temp path, fsynced, then renamed, and end with an FNV-1a
checksum over every row plus the header. A torn or corrupted snapshot is refused
at load rather than served as a smaller, plausible holder set. The row count in
the header is not trusted for allocation, so a corrupt header cannot drive an
enormous reserve.

`USDTPIR2` files (no checksum) still load, with a warning, and are rewritten as
`USDTPIR3` on the next save.

### One writer at a time

`follow`, `sync`, and `serve` take an exclusive `flock` on
`<snapshot>.lock` at startup, and refuse to run if another process holds it:

```
Error: another process is already writing "data/balances.snapshot"
(lock "data/balances.snapshot.lock"). Stop it first, or pass a different --state.
```

Two writers would otherwise both write-temp-then-rename, and the later rename
would silently discard the other's blocks — no error, no corruption, just lost
work. The lock is released by the kernel when the process exits, including on a
crash, so there is nothing to clean up. `lookup`, `stat`, and `sample` read the
snapshot and take no lock.

### Keyword index

`serve` also persists the slot assignment, under `--keyword` (default
`data/keyword`):

| File | Holds | Rewritten |
| --- | --- | --- |
| `keyword.index` | MPHF + delta + version — the blob clients download | every publish |
| `keyword.keys` | slot -> address over the MPHF range, 20 B each | only on a full rebuild |

Without this, a restart is a full resync for every client: the MPHF is not
reproducible, so rebuilding it over the same addresses permutes every slot.
With it, addresses keep their slots and only what arrived since the last save is
appended — which clients pick up as an ordinary delta tail.

`.keys` is what makes that exact. A minimal perfect hash is *total*: it answers
for addresses it was never built over, so membership cannot be recovered from the
blob alone, and guessing would hand one address's slot to another. Delta
membership needs no table — only delta keys index past the MPHF.

`keyword.index` is written **before** the directory is exposed to clients, so
what is on disk is never behind what a client already holds. If that write fails,
the previous generation keeps being served rather than handing out slots that
would not survive a restart. On a rebuild, `.keys` is written first: dying in
between leaves the versions disagreeing, which reads as "nothing saved" and
rebuilds, rather than pairing a table with a directory it does not describe.

Delete both files to force a fresh MPHF. Every client will have to resync.

PIR records are 64 bytes:

```text
address(20) | usdt(16) | usdt_block(4) | usdc(16) | usdc_block(4) | reserved(4)
```

Balances and block numbers are little-endian in records.

## Layout

| Path | What |
| --- | --- |
| `src/` | Syncer, PIR publisher, query endpoint. |
| `client/` | WASM PIR client and browser portal. |
| `crates/record/` | The 64-byte record layout shared by server and client. |
| `scripts/` | Local-only smoke-test launchers. |
| `../poulpy-pir/vendor/ptr_hash/` | Forked MPHF; see its `FORK.md`. |

The workspace root patches `poulpy-pir` to a local path and `ptr_hash` to that
fork. Both are required for the client to build for wasm32.

## Missing

- An `ingest` command to load an externally-produced holder snapshot. Until then
  a map starts empty and fills in as addresses move, so a well-known wallet
  reads as "not held" until it next transacts.
- Large-scale validation of keyword index compaction. It is wired to
  `--compact-after` but has never run at a 200 K delta.
- Authentication on the endpoint. Rate limiting exists; anyone who can reach the
  port can still query.
- Reorg detection in `sync`. `follow` and `serve` carry the tripwire; `sync` has
  no previous hash to compare against.
