# Cached one-shot bootstrap command

## Objective

Implement a one-shot, crash-resumable bootstrap command that:

- discovers every Ethereum-mainnet address that may hold USDT or USDC by
  scanning from the token deployment blocks through one fixed confirmed block
  `T`;
- reads the absolute USDT and USDC balances of those addresses at `T`;
- writes a complete native balance snapshot with cursor `T`; and
- hands that snapshot to `serve`, which catches up from `T + 1`, builds the PIR
  database, and then follows the chain independently.

Bootstrap is not a second continuous syncer. It runs once to create the complete
starting snapshot and exits. Later chain updates belong to `serve`.

The intermediate SQLite cache exists only to preserve expensive bootstrap work
across process crashes, host restarts, RPC failures, or an intentional rerun.
It is not serving state and does not need to be transferred to the PIR host.

## Confirmation model

Each new bootstrap run selects its target once:

~~~text
head = eth_blockNumber at bootstrap start
T    = head - confirmations
~~~

`--confirmations` is a positive integer and defaults to 4. A depth of 4 means
exactly `T = head - 4`: block `T` has four descendants. The block at `head` is
not counted as its own confirmation. The subtraction must be checked; it must
not silently saturate to block zero. Numeric confirmation handling in `serve`
must use the same positive, checked resolver. The existing `finalized` policy
may remain available to `serve`, but the bootstrap workflow uses a numeric
depth.

`T` is pinned in the cache and is not advanced when a crashed bootstrap
resumes, even if the chain head has moved. Once the snapshot at `T` is
complete, `serve` catches up the missing range.

The application treats a block at the selected depth as final. This is not
Ethereum consensus finality. Repairing a reorganization after a snapshot has
been committed is deliberately out of scope, including repair after a process
restart. The existing continuous follower and its best-effort reorg behavior
remain unchanged and are not part of this handoff; bootstrap and startup
catch-up correctness must not claim post-commit repair.

Bootstrap records the hash of `T` to avoid committing a result assembled
across two versions of the chain. It checks the hash when resuming and again
before commit. If the hash changed before commit, the target-specific cache is
reset and bootstrap starts a new run at a newly selected confirmed target. This
is a pre-commit consistency check, not rollback support. Once the cache reaches
`Complete`, the target is final by application policy and later invocations do
not reopen the completed result merely because the target hash later changed.

## CLI

~~~sh
usdt-pir bootstrap \
  --rpc "$ETH_RPC_URL" \
  --confirmations 4 \
  --state data/balances.snapshot \
  --cache data/balances.snapshot.bootstrap.sqlite \
  --chunk 10000 \
  --retries 10
~~~

Options:

- `--rpc`: Ethereum mainnet RPC URL, also accepted through `ETH_RPC_URL`.
- `--confirmations`: application confirmation depth; default 4.
- `--state`: final native snapshot path; default `data/balances.snapshot`.
- `--cache`: intermediate SQLite path. When omitted, append
  `.bootstrap.sqlite` to the complete `--state` filename; for example,
  `balances.snapshot` becomes `balances.snapshot.bootstrap.sqlite`. Do not
  replace the state extension, because two different state filenames with the
  same stem would then collide.
- `--chunk`: desired historical log range. Bootstrap starts large and adapts
  downward or upward around provider limits.
- `--retries`: additional attempts allowed after the initial failed attempt for
  the same stalled unit. Thus `--retries 10` permits at most 11 failed attempts
  with no progress. A successfully committed unit resets the counter, and
  adaptive range narrowing or batch splitting does not consume it.
- `--keep-cache`: retain a completed cache for diagnostics. By default the
  cache is removed only after the final snapshot has been saved and reloaded
  successfully.

Bootstrap must not start a new run when `--state` already exists. Because the
snapshot intentionally carries no bootstrap provenance, any existing state
file without a matching cache is presumed to be an existing bootstrap and is
refused, even if it may actually be a partial snapshot made by another command.
Operator discipline is the trust boundary for now.

The only exceptions are recovery through a matching `ReadyToCommit` or
retained `Complete` cache entry. Such recovery must prove exact semantic
equality with the cache projection defined below; cursor, count, and aggregate
totals alone are not sufficient.

## Token starting points

Add verified Ethereum-mainnet constants for:

~~~text
USDT deployment block: 4,634,748
USDC deployment block: 6,082,465
USDT initial owner:     0x36928500Bc1dCd7af6a2B4008875CC336b927D57
~~~

A combined USDT/USDC filter may scan from the earlier USDT deployment block.
The USDC address has no logs before its own deployment, so this is equivalent
to maintaining two independent scan cursors and keeps crash recovery simpler.

The target must be at or after both token deployment blocks and Multicall3's
mainnet deployment block because all target balances are read through
Multicall3.

## Stored data

### Native snapshot

The final `--state` path is the only balance-state artifact handed from
bootstrap to `serve`. Reuse the existing checksummed USDTPIR3 format:

~~~text
magic | cursor(u64) | count(u64) | rows | checksum
~~~

Each row is:

~~~text
address(20) | usdt(u128) | usdt_block(u32) |
usdc(u128) | usdc_block(u32)
~~~

Balances are six-decimal base units. A token block field is the most recent
supported balance-changing event observed while the address was tracked, not
the snapshot cursor and not a proof of the address's complete lifetime history.
Addresses whose two balances are zero are omitted. If only one token balance is
zero, retain that token's last observed block when one exists.

Omitting a zero/zero address intentionally discards its two block stamps. If the
address later becomes a holder again, an untouched token may consequently have
block stamp `0`, meaning "no supported event observed since this row entered
the current map." This is accepted to avoid retaining tombstones for every
historical address.

The final snapshot cursor is exactly `T`, which means the next block to process
is exactly `T + 1`.

USDTPIR3 does not encode chain ID, token addresses, target hash, confirmation
depth, or a completeness marker. `serve` therefore trusts the operator-provided
snapshot and cannot independently prove that it was produced by bootstrap.
Adding snapshot provenance or a sidecar manifest is deliberately deferred to
keep this implementation simple.

### Intermediate SQLite cache

Use bundled `rusqlite` so bootstrap does not depend on a system SQLite package.
Enable crash-safe journaling and commit each completed unit of work in a
transaction. WAL mode with `synchronous=FULL` is appropriate because this cache
exists specifically to survive crashes.

The logical schema is:

~~~text
metadata
  schema_version
  bootstrap_identity
  phase
  chain_id
  state_path
  confirmations
  target_block
  target_hash
  scan_start
  scan_cursor

candidates
  address         BLOB(20) PRIMARY KEY
  usdt_block      INTEGER NULL
  usdc_block      INTEGER NULL
  usdt_balance    BLOB(16) NULL
  usdc_balance    BLOB(16) NULL

partial index on address where either balance is NULL
~~~

SQLite integers cannot represent `u128`, so balances use fixed-width 16-byte
little-endian blobs, matching USDTPIR3. `NULL` means "not read yet"; an encoded
zero is a completed zero balance. `target_hash` is exactly 32 bytes and an
address is exactly 20 bytes. SQLite type names do not enforce these widths, so
schema constraints and load-time validation must.

The cache is bound to:

- schema version;
- Ethereum chain ID 1;
- `bootstrap_identity`, a versioned identifier covering the fixed USDT/USDC
  addresses, deployment blocks, initial seed, and event-discovery semantics;
- the intended snapshot path;
- target block and target hash; and
- the bootstrap phase.

A cache with incompatible identity must be rejected or explicitly reset; it
must never authorize an unrelated snapshot.

Normalize every state, cache, lock, sidecar, and temporary path before creating
or locking any of them. Make relative paths absolute, canonicalize the nearest
existing ancestor to resolve symlinks, and lexically append any not-yet-existing
suffix. For existing paths, also compare filesystem identity. Reject a
configuration in which any derived artifacts alias or collide. Store the
normalized intended state path in the cache.

## Bootstrap phases

The durable phases are:

~~~text
Scanning -> ReadingBalances -> ReadyToCommit -> Complete
~~~

Phase changes occur in SQLite transactions. In-memory work may be repeated
after a crash, but a durable cursor or completed balance must never be recorded
before the corresponding data is durable.

`Complete` is the application commit boundary. Before that phase, a target-hash
mismatch invalidates target-specific work. After it, later reorganization is an
accepted finality risk and does not reopen the run.

### New run initialization

1. Resolve and validate all paths as above, then acquire the snapshot lock for
   `--state` followed by the cache lock.
2. Verify `eth_chainId == 1`.
3. Refuse any existing state file; a new run is permitted only when `--state`
   is absent.
4. Resolve `head`, calculate `T = head - confirmations`, and require that `T`
   is at or after both token deployments and Multicall3's deployment.
5. Read and require the block hash for `T`.
6. In one SQLite transaction, create cache metadata in phase `Scanning` with
   `scan_cursor` set to one block before the USDT deployment block and seed the
   USDT initial owner with `usdt_block` equal to the deployment block. The
   constructor allocated supply without emitting a Transfer. Metadata and the
   seed must never become durable separately.

### Resume initialization

1. Resolve and validate all paths as above, then acquire the snapshot lock and
   the cache lock before inspecting or changing cache state.
2. Validate the cache schema, cached chain, token set, and intended state path,
   and require the current `--confirmations` value to equal the cached value.
   `--chunk` and `--retries` may change between invocations. Independently
   require the current RPC's `eth_chainId == 1` before any hash comparison or
   mutation.
3. For `Scanning`, `ReadingBalances`, or `ReadyToCommit`, re-read the cached
   target block hash. An RPC error, missing block, or missing hash is not a
   mismatch: retry it under the normal budget and leave all cached work and any
   state file untouched.
4. If a successfully read hash still matches, resume the recorded phase without
   selecting a newer target, even if the head has advanced.
5. If a successfully read hash differs before `Complete`, resolve and validate
   a replacement head, target, and target hash without mutating the old cache.
   First classify any existing state against the old cache projection. A
   `ReadyToCommit` snapshot that exactly matches is an authenticated,
   uncommitted artifact: remove it and fsync its parent. Refuse any unrelated,
   non-matching, or corrupt state. Only after that classification/removal, reset
   in one SQLite transaction: clear target-specific candidates, install all
   replacement metadata, reseed the constructor owner, and enter `Scanning`.
6. For `Complete`, do not re-check the target hash. If the snapshot exists and
   exactly matches the cache projection, report that bootstrap is already
   complete. If the matching snapshot is missing, reconstruct and exactly
   verify it from the cache. Refuse a corrupt or non-matching existing state;
   the retained cache cannot prove whether it is the original bootstrap file or
   a later `serve` snapshot, so overwriting it could silently roll state back.

The recovery matrix is:

| Cache state | State path | Action |
| --- | --- | --- |
| no cache | absent | start a new run |
| no cache | present | refuse; presume an existing bootstrap |
| `Scanning` / `ReadingBalances` | absent | resume cached work |
| `Scanning` / `ReadingBalances` | present | refuse as unrelated |
| `ReadyToCommit`, hash matches | absent | rebuild and commit |
| `ReadyToCommit`, hash matches | exact projected-cache match | verify and complete |
| `ReadyToCommit`, hash changed | absent | reset to a new target |
| `ReadyToCommit`, hash changed | exact projected-cache match | remove state, then reset |
| `ReadyToCommit` | other valid/corrupt state | refuse as unrelated |
| `Complete` | absent | reconstruct, verify, and report complete |
| `Complete` | exact projected-cache match | verify and report complete |
| `Complete` | other valid/corrupt state | refuse; never overwrite it |

A crash can create the SQLite file before the initialization transaction commits
its schema, metadata, and constructor seed. With the state absent and both locks
held, a zero-byte file or a valid SQLite database with no committed user schema
is classified as uninitialized and may be removed with its WAL/SHM sidecars and
initialized again. Any partial/unknown schema is corrupt or unrelated and must
be refused. An existing state still blocks reinitialization.

Crash resumption is always from the last durable unit:

- scanning restarts at `scan_cursor + 1` and repeats only an uncommitted range;
- balance reading skips rows whose two balance blobs are already non-`NULL` and
  repeats only an uncommitted batch; and
- commit recovery rebuilds or verifies the snapshot from the completed cache.

If the cache is lost while no state file exists, bootstrap restarts from the
deployment blocks. If a state file already exists, the missing cache does not
authorize a restart or overwrite: bootstrap refuses it as an existing result.
A corrupt or incompatible cache is rejected with explicit removal/reset
instructions; it is never silently adopted. No `--force` overwrite path is
required for this implementation. The operator may manually remove a rejected
cache only after confirming that the state path is absent; an existing state
continues to block a new bootstrap.

## Historical address scan

Scan the inclusive range:

~~~text
USDT_DEPLOY_BLOCK ..= T
~~~

Use the existing token-address and event-topic filter. Logs only discover
candidates and their most recent token-specific event blocks. Do not calculate
balances by applying event amounts.

### Address discovery rules

Reuse the live syncer's event semantics:

- For a nonzero, non-self USDT or USDC Transfer, record both nonzero endpoints
  and stamp the corresponding token block.
- For USDT `DestroyedBlackFunds`, record the destroyed account and stamp the
  USDT block.
- For USDT `Issue` and `Redeem`, resolve `owner()` at the event block, record
  that owner, and stamp the USDT block.
- Seed the deployment-time USDT owner as described above.
- Ignore zero-address endpoints, zero-value transfers, and self-transfers.

Bootstrap is stricter than the current permissive live parser. Every log
returned for a watched token/topic must have a block number and block hash, fall
inside the requested range, not be marked removed, and have the expected ABI
topic/data shape. A structurally malformed matched event fails the range and
does not advance `scan_cursor`; it must not be silently converted into an
ignored event. Valid zero-value transfers, self-transfers, and zero endpoints
remain intentional no-ops.

Use the same structural validation for the initial `serve` catch-up. A malformed
matched log is logged as a startup range failure; `serve` leaves its cursor
before that range and retries while the endpoint remains unopened. Continuous
following after the initial PIR build remains unchanged and is out of scope.

For each address, cache the maximum observed block independently for USDT and
USDC.

`owner()` at a numeric block observes end-of-block state. This design accepts
the historical assumption that USDT ownership did not change within a block
after an Issue/Redeem event that needed the previous owner. Transaction-level
state reconstruction and tracing are out of scope.

### Atomic scan progress

For every successfully fetched contiguous range `lo..=hi`:

1. Parse every returned log.
2. Resolve all required USDT owners for that range. Deduplicate owner reads by
   block.
3. Begin one SQLite transaction.
4. Upsert candidate rows, taking the maximum non-null block per token.
5. Set `scan_cursor = hi` in the same transaction.
6. Commit.

An empty log range still advances `scan_cursor` transactionally. If fetching,
parsing, owner resolution, or the transaction fails, `scan_cursor` remains at
the previous fully completed block and the range is safe to retry.

The requested log chunk should start large for a multi-year scan. On a provider
result cap, narrow the current range. After successful sparse ranges, allow the
requested size to grow back toward the configured `--chunk`; the current live
fetcher's permanently shrinking 50-block behavior is not suitable for
bootstrap.

If a single block still exceeds a provider's result cap, exit with a diagnostic
requiring a different provider; range splitting cannot make further progress.
Ethereum JSON-RPC cannot prove that a provider did not silently truncate a
successful response. The operator is therefore responsible for supplying a
complete, coherent archive RPC. The total-supply checks detect missing current
positive balances but do not prove complete historical block-stamp coverage.

When `scan_cursor == T`, transition transactionally to `ReadingBalances`.

## Balance reads

Balances are read once, at the fixed target `T`:

~~~text
USDT.balanceOf(address) at T
USDC.balanceOf(address) at T
~~~

Reuse Multicall3 with at most 800 calls per request, which is 400 addresses for
two tokens. On a provider request-size, response-size, execution-gas, or timeout
limit, split the logical batch and retry smaller halves down to one address. A
single-address failure receives the normal retry budget and then exits with the
token and address in the diagnostic.

Bootstrap balance reads are strict:

- a failed subcall fails the entire logical batch;
- malformed return data fails the batch;
- a value that does not fit `u128` fails the batch; and
- no failure may be converted into a zero balance.

Keeping `allowFailure: true` inside Multicall3 is acceptable if the decoder
turns every `success == false` result into an error with token and address
context. This preserves useful diagnostics while retaining batch-level retry.

Repeatedly select up to 400 candidates for which either balance is `NULL`, using
the partial unread-address index rather than rescanning the completed prefix.
Read both balances and store both results in one SQLite transaction. A crash
after the RPC response but before commit merely repeats that batch. A committed
zero is distinguishable from an unread value.

After no unread candidates remain, build and validate the candidate map before
transitioning transactionally to `ReadyToCommit`.

Use the same strict balance-reader behavior for initial `serve` catch-up. A
failed subcall must never be converted into zero or advance the startup map
cursor past the failed range. `serve` logs the error, keeps the endpoint
unopened, backs off, and retries the same in-memory catch-up unit. Continuous
following after the initial PIR build keeps today's behavior and is out of
scope.

Bootstrap remains a bounded one-shot command: after the initial failed attempt
plus the configured number of additional retries with no progress, it exits,
leaving all committed cache work resumable on rerun.

## Build and validation

Build an in-memory `BalanceMap` from the completed cache:

1. Set its cursor to `T`.
2. Apply each cached address with its two balances and token-specific maximum
   event blocks.
3. Omit candidates whose balances are both zero.
4. Preserve a zero token's last-change block when the other token is nonzero.

Call this deterministic nonzero row set the **cache projection**. Exact
snapshot/cache equality always means that the snapshot cursor equals `T`, it
contains every row in this projection with identical balances and block stamps,
and it contains no extra row. Zero/zero cache candidates are intentionally not
snapshot rows, but every such cache row must still pass the cache width, value,
and block-stamp validation below.

Before writing the final snapshot, require:

- Ethereum mainnet chain ID;
- `scan_cursor == T`;
- no candidate with an unread balance;
- every cached address and balance blob to have the expected width;
- every non-null token block to fit `u32` and lie between that token's
  deployment block and `T`;
- every nonzero token balance to have a non-null token-specific block stamp
  (including the seeded USDT constructor owner);
- the sum of saved USDT balances to equal `USDT.totalSupply()` at `T`;
- the sum of saved USDC balances to equal `USDC.totalSupply()` at `T`;
- checked arithmetic for both sums;
- the target hash to remain equal to the cached hash; and
- the holder count not to exceed the capacity of the deployed PIR shape.

Derive or centralize the PIR capacity from the `eth-pir` deployment shape
rather than introducing another unrelated magic number.

## Atomic snapshot commit and recovery

The snapshot and SQLite cache cannot be committed in one filesystem
transaction, so use the cache phase to make the sequence recoverable:

1. Finish all semantic validation.
2. Persist phase `ReadyToCommit`.
3. Save the `BalanceMap` through the existing atomic USDTPIR3 writer.
4. Reload the saved snapshot and require the checksummed USDTPIR3 format, no
   duplicate or zero/zero rows, no trailing bytes, and exact semantic equality
   with the cache projection. The checksum, cursor, holder count, and aggregate
   totals are useful diagnostics but are not substitutes for exact row-set
   equality.
5. Re-read the target hash once more. An RPC error or unavailable hash leaves
   phase `ReadyToCommit` and the snapshot untouched for bounded retry. Only a
   successfully read different hash follows the hash-mismatch recovery path.
6. Persist phase `Complete`.
7. Remove the completed cache unless `--keep-cache` was supplied.

Startup recovery for `ReadyToCommit` is:

- If the snapshot does not exist, rebuild it from the complete cache and repeat
  the commit sequence.
- If the snapshot exists with cursor `T`, reload it and require exact semantic
  equality with the cache projection, re-read the target hash, and only then mark
  `Complete`. A mismatch follows the pre-completion reset path.
- If an unrelated snapshot exists, refuse to overwrite it.

This handles a crash after the snapshot rename but before SQLite was marked
complete.

Before relying on the existing atomic writer, harden it to use a non-colliding
temporary filename tied to the complete state filename, fsync the temporary
file, rename it, and fsync the parent directory before phase `Complete` or cache
deletion. The current `with_extension("tmp")` name can collide for distinct
state paths with the same stem, and rename without a parent-directory fsync is
not a complete power-loss durability boundary.

If a retained `Complete` cache exists but the matching snapshot is missing,
the snapshot may be reconstructed from that cache after repeating cache
structural validation and exact cache-projection comparison. Do not re-read chain
state or the completed target hash; post-commit reorganization is outside the
chosen finality model.

Failure to delete a completed cache is a warning, not a failed bootstrap: the
authoritative snapshot is already committed and verified.

## RPC requirements and preflight

Bootstrap needs a separate preflight from ordinary live sync. The existing
preflight rejects ranges before Multicall3's deployment, but bootstrap only
uses Multicall3 for balance reads at the modern target `T`.

Require an RPC that provides:

- Ethereum mainnet chain ID 1;
- complete historical logs from the USDT deployment block onward;
- historical `owner()` state at every USDT Issue/Redeem event block;
- block hashes for the pinned target;
- Multicall3 state and execution at `T`;
- USDT and USDC `balanceOf` state at `T`; and
- USDT and USDC `totalSupply` state at `T`.

Preflight should test old log access separately from target-state access. It
must not require Multicall3 to exist at the historical scan start.

Because balance and owner calls remain pinned by block number for simplicity,
the RPC must provide a coherent view across all requests. A load balancer that
routes historical calls to disagreeing backends is unsupported. EIP-1898
block-hash-pinned calls and multi-provider verification are deliberately out of
scope. The start/resume/pre-commit target-hash checks remain the protection
against a reorganization visible through the configured provider.

Require `T` to be at or after Multicall3's deployment block, even though a
normal present-day mainnet target will always satisfy it. Configure finite RPC
timeouts and exponential backoff with jitter. Provider-cap range narrowing and
adaptive balance-batch splitting do not consume the stalled retry budget while
they are making progress; `--retries 10` means ten retries after the initial
failed attempt with no progress.

## Handoff to serve

The first bootstrap snapshot includes block `T`, so its first `serve` starts at
`T + 1`. On any later `serve` restart, the same state file may already have
advanced beyond `T`. Let `C` be the cursor actually loaded from the snapshot;
USDTPIR3 does not retain the original bootstrap target.

At server startup:

~~~text
loaded cursor   = C
S               = startup head - confirmations, resolved once
S hash          = block hash captured when S > C
catch-up range  = C + 1 ..= S, when S > C
~~~

The required order is:

1. Acquire the existing snapshot lock and load the snapshot at cursor `C`.
   Require checksummed USDTPIR3; do not accept the legacy checksumless USDTPIR2
   format as authoritative serving state.
2. Resolve and pin the application-confirmed block number `S` once for this
   startup catch-up. If `S < C`, exit with the behind-state diagnostic before
   preflight or any block-hash read. If `S > C`, capture its block hash.
   Ordinary RPC retries within the same attempt must not re-resolve a moving
   `S`. A process restart may select a new `S` from its newly loaded cursor.
3. Strictly sync `C + 1..=S` in memory without saving any cursor above `C`.
   Startup catch-up is one validation unit; a crash repeats it from `C` rather
   than trusting a partially validated prefix.
4. At `S`, collect checked total-supply results and run map validation. Load the
   restored keyword artifacts, if any, and dry-run the same slot allocation
   used by PIR construction, accounting for occupied/vacant slots plus holders
   that would be appended. Record any semantic or capacity mismatch, but when
   `S > C` do not classify it as deterministic until the next hash check.
5. Immediately after completing all `S`-pinned validation RPCs, re-read the
   hash of `S` when `S > C`. An unavailable hash is retried without changing disk state. If
   a successfully read hash differs, discard the in-memory map, reload cursor
   `C`, and start a new startup attempt with a newly resolved target/hash. Never
   combine chunks from the two attempts. This prevents assembly across a fork
   while still treating state through trusted cursor `C` as final; repairing a
   reorganization at or before `C` remains out of scope. Only a matching final
   hash makes any recorded supply, map, or capacity mismatch a deterministic
   startup error.
6. After the final hash matches and every recorded validation passes,
   atomically save the caught-up map with cursor `S`.
7. Build the initial PIR database from the validated map and the already
   capacity-checked keyword allocation.
8. Start the query endpoint.
9. Hand the map to the existing continuous follower and PIR worker without
   changing their update, publication, retry, or reorg behavior. The shared
   numeric confirmation depth remains the one explicit exception: its default
   is aligned to 4 as specified below.

Transient RPC failures while reaching or validating `S` are logged and retried
with backoff while the in-memory catch-up unit is retained. A successfully read but
unequal total supply, invalid map, or insufficient initial PIR capacity is a
deterministic startup error; retrying it without a state or configuration change
cannot make the initial database safe. On such an exit, the disk cursor remains
exactly `C`, so a restart cannot trust newly caught-up but unvalidated state.

If `S == C`, skip catch-up and build the PIR database directly from the loaded
snapshot after validation. As required above, `S < C` is an error: it indicates
that the serving RPC or its configured finality policy is behind the imported
state, and the ahead snapshot must not be exposed.

The current `serve` structure already catches up before building and exposing
the PIR database. Preserve that ordering and align its default numeric
confirmation count with bootstrap's default of 4.
USDTPIR3 cannot enforce that bootstrap and serve used the same depth, so this
remains an operator configuration convention.

No SQLite cache is needed on the PIR machine. Transfer only the completed
snapshot and configure it through `USDT_PIR_STATE`. The EC2 launcher must use
the same confirmation count and must not pass `--from-block` when the snapshot
exists. To prevent recreating the incomplete demo behavior, it should fail with
transfer instructions when the required snapshot is absent instead of starting
from an empty near-head map.

Acquire the same snapshot lock used by `serve`, transfer to a unique temporary
filename beside the destination on the same filesystem, fsync it, and load it
as strict checksummed USDTPIR3. Only then rename it into place and fsync the
destination parent directory. Never copy directly over a state path or bypass
the lock held by a running `serve`. Initial keyword restoration retains today's
behavior: a fresh host builds the directory, while a host with matching
`.index`/`.keys` restores it and accounts for occupied/vacant slots during
capacity validation. Publication after startup remains unchanged.

### Scope after startup

Once the validated snapshot at `S` has produced the initial PIR database and
the endpoint is open, this handoff is complete. Continuous following, update
transport, PIR rebuild/publication, keyword compaction, retry behavior, and
reorg handling continue exactly as implemented today, except that the shared
numeric confirmation default is now 4. No other post-startup follower, PIR
worker, or PIR library work belongs to this handoff.

## Concurrency

Bootstrap, sync, follow, and serve must not concurrently write the same native
snapshot. Bootstrap holds the existing snapshot lock for its full invocation,
including cache recovery and final commit. `serve` likewise holds it across
startup catch-up and continuous following.

Bootstrap also acquires a non-blocking advisory lock for the cache for its full
invocation. SQLite protects individual transactions, but a cache is logically
owned by one bootstrap/state identity; two bootstraps must not share it across
different state paths. Acquire the snapshot lock before the cache lock
everywhere to avoid deadlock. Derive the cache lock by appending `.lock` to the
complete cache filename rather than replacing an extension.

WAL mode assumes a suitable local filesystem with working file locks and shared
memory semantics. Reapply and verify `journal_mode=WAL` and
`synchronous=FULL` whenever the cache is opened. Before deleting a completed
cache, run and verify a final WAL checkpoint while the connection is open, close
the connection, then delete the database and any remaining `-wal` and `-shm`
sidecars.

## Progress and shutdown

This can be a multi-day operation. Log the cache phase, pinned target number and
hash, scan cursor and percentage, current/adaptive chunk size, candidate count,
unread balance count, retry/backoff state, and final holder/supply totals. The
completion message must name the verified snapshot path and cursor.

Normal interruption and process termination require no special logical
checkpoint beyond closing the current transaction. A fetched-but-uncommitted
log range or balance batch is intentionally repeated on restart. Never advance
a durable cursor from a signal handler or best-effort shutdown path.

## Implementation outline

- Add `Bootstrap` to the CLI arguments and command dispatcher.
- Add `src/cli/bootstrap_cmd.rs` for command orchestration.
- Add a small bootstrap module for the SQLite schema, phase transitions,
  historical scan, cached balance batches, validation, and commit recovery.
- Add bundled `rusqlite` to the root crate.
- Add token deployment and USDT initial-owner constants.
- Reuse the token filter, USDT owner reader, `BalanceMap`, and snapshot lock.
  Reuse the event semantics behind a stricter bootstrap validator rather than
  silently accepting malformed matched logs.
- Apply the same structural log validation to initial `serve` catch-up,
  retrying a malformed range without advancing its startup cursor.
- Harden the atomic snapshot writer with non-colliding temporary paths and a
  parent-directory fsync.
- Expose or relocate adaptive log fetching so bootstrap can use it with a large
  range and recovery growth.
- Add strict balance-reader behavior, adaptive Multicall splitting, and a pinned
  `totalSupply()` reader.
- Use strict reads for initial `serve` catch-up; a failed startup range remains
  uncommitted and is retried before the endpoint opens.
- Pin startup catch-up target/hash `S`, use the loaded cursor `C`, reject
  `S < C`, and validate the hash and supply/capacity at `S` before saving any
  cursor above `C` or building PIR.
- Change the serve confirmation default to 4.
- Update the EC2 launcher and README for the bootstrap-transfer-serve workflow.

Keep the implementation one-shot: there is no incremental bootstrap mode after
a successful snapshot. `serve` owns every later block.

## Tests

Cover at least:

- checked `head - confirmations` target calculation;
- the depth definition that `head - 4` has four descendants, and rejection of
  zero or underflowing numeric confirmation depths;
- target pinning across a crash and resume;
- rejection of a changed confirmation depth on resume while allowing chunk and
  retry tuning to change;
- rejection of a target before token deployment;
- inclusive deployment-through-`T` scan boundaries;
- maximum per-token event block upserts;
- USDT special events and constructor allocation;
- atomic new-run metadata plus constructor-owner seeding;
- recovery from a crash that leaves an uninitialized SQLite file, and rejection
  of an unknown partial schema;
- rejection of structurally malformed, removed, out-of-range, or blockless
  matched logs without cursor advancement;
- atomic candidate upsert plus `scan_cursor` advancement;
- restart at `scan_cursor + 1` after an interrupted scan;
- adaptive log-range narrowing and recovery growth;
- strict handling of failed or malformed balance subcalls;
- adaptive balance-batch splitting and a terminal one-address diagnostic;
- atomic balance-batch persistence and unread-batch resume;
- distinction between unread `NULL` and an encoded zero balance;
- target-hash mismatch resetting target-specific work;
- wrong-chain resume and unavailable/missing target-hash reads leaving all
  cached work untouched;
- classification/removal of an authenticated uncommitted snapshot before cache
  reset on a real target-hash mismatch;
- hash mismatch after an uncommitted snapshot was renamed;
- `Complete` recovery without reopening a later reorganization;
- missing-state reconstruction from a retained `Complete` cache, but refusal to
  overwrite a corrupt existing state;
- zero/zero omission, preservation of one-token-zero block stamps, and accepted
  stamp `0` after a removed address later re-enters the map;
- total-supply, checked-sum, and PIR-capacity validation;
- refusal of every existing state without a matching recovery cache;
- matching `ReadyToCommit`/`Complete` recovery and rejection of an unrelated
  snapshot or cache;
- exact row-by-row cache-projection comparison, including zero/zero cached rows
  and a different snapshot with the same cursor, count, and totals;
- crash recovery before snapshot save;
- crash recovery after snapshot save but before phase `Complete`;
- saved snapshot cursor, checksum, strict framing, and exact projected-cache
  verification;
- non-colliding state/cache/temp derivation and state/cache lock contention;
- normalized-path alias detection through existing symlink ancestors;
- exact retry attempt counts, reset after progress, and non-consumption by
  adaptive range/batch splitting;
- serve catch-up beginning at exactly `T + 1`;
- authoritative `serve` refusal of checksumless USDTPIR2;
- a later `serve` restart from loaded `C > T` beginning at `C + 1`;
- startup target `S` remaining pinned across catch-up retries;
- start/end `S`-hash equality, with an unavailable hash retaining the attempt
  and a changed hash discarding it before a newly pinned attempt;
- a supply mismatch accompanied by a changed `S` hash causing attempt discard,
  not a deterministic validation exit;
- no-op catch-up only when `S == C`, and an error when `S < C`;
- a catch-up crash or validation failure at `S` leaving the saved cursor exactly
  `C`, and saving `S` only after successful validation;
- supply and actual restored-keyword capacity validation at `S`;
- interrupted snapshot transfer, strict temporary-file validation, lock
  contention with a running server, and destination-parent durability; and
- PIR construction and endpoint startup only after catch-up succeeds.

Use deterministic provider fixtures or a mock provider for tests. Do not make
the test suite depend on a live public archive RPC.

## Operational trade-offs

- A lost or intentionally deleted cache before a state file was committed means
  bootstrap restarts from the token deployment blocks. If the state file
  already exists, bootstrap refuses it instead of inferring that a restart is
  safe.
- A retained cache can consume substantial disk because it contains every
  address ever discovered for these tokens, including addresses that are zero
  at `T`.
- The in-memory final `BalanceMap` still requires enough RAM for the current
  nonzero holder set.
- A four-confirmation target can theoretically be reorganized later. The
  application accepts that risk and performs no post-commit repair.
- A checksummed snapshot is trusted by operator provenance; USDTPIR3 cannot
  prove that it is a complete bootstrap or enforce the original confirmation
  depth.
- A silently truncating or internally inconsistent RPC can produce incomplete
  history despite returning successful calls. Use a trusted coherent provider.

## Non-goals

- Incrementally running bootstrap after a successful snapshot.
- Chasing a moving head during bootstrap.
- Reading balances at every historical block.
- Reconstructing balances from Transfer amounts.
- Adopting an unrelated or partial empty-map snapshot.
- Starting the PIR endpoint before catch-up completes.
- Rollback journals or repair of post-commit reorganizations.
- Ethereum consensus-finalized snapshots unless the operator chooses an
  appropriately conservative confirmation count.
- Snapshot provenance, a bootstrap-completeness marker, or an atomic manifest.
- Monitoring or supporting future USDT deprecation or USDC implementation
  upgrades; the fixed current token/event semantics are accepted for now.
- Transaction-level reconstruction of USDT ownership changes within an
  Issue/Redeem block.
- Changing continuous-follow behavior after the initial PIR endpoint opens,
  other than aligning its numeric confirmation default to 4.
- Changing PIR update transport, publication, compaction, failure handling, or
  worker lifecycle.
- Modifying PIR library APIs.
