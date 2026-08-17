# Bootstrap implementation review

## Purpose and authority

Review and test the implementation of `docs/agents/bootstrap.md` after the
implementation agent has finished.

`docs/agents/bootstrap.md` is the single source of truth for behavior. This
document defines the review procedure and report format; it does not restate or
override the implementation requirements. If the two documents appear to
conflict, follow `bootstrap.md` and report the conflict.

The review is read-only unless the user separately asks for fixes. Do not edit
source files, documentation, tests, lockfiles, or sibling repositories merely
because a problem is found. Running commands that create ordinary build/test
artifacts is allowed.

## Inputs

The reviewer has:

- the repository worktree containing the implementation;
- `docs/agents/bootstrap.md`;
- the implementation agent's final report, when available; and
- `ETH_RPC_URL`, which may be used for bounded, read-only Ethereum-mainnet
  checks.

Never print, log, commit, or embed the value of `ETH_RPC_URL`. Do not use verbose
HTTP output that could reveal credentials. Pass the environment variable by
name rather than expanding it into a recorded command.

The worktree may already contain unrelated user changes. In particular, the
AVX2 fallback changes in `scripts/run-ec2-demo.sh` predate the bootstrap
implementation. Preserve them and distinguish them from bootstrap-related
launcher changes in the report.

## Scope guard

Review the implementation through:

- the bootstrap command and its durable cache;
- final snapshot creation and recovery;
- transfer of the completed snapshot;
- `serve` startup catch-up and validation;
- initial PIR construction and endpoint startup; and
- the documentation and launcher changes required by `bootstrap.md`.

Enforce the scope boundary in `bootstrap.md`. After the initial endpoint opens,
the existing follower and PIR worker must be unchanged except for the documented
numeric confirmation default. Treat new live-update protocols, publication
watermarks, worker supervision, staged PIR publication, keyword-compaction
redesigns, or PIR library API changes as scope regressions.

Do not review unrelated cryptography or performance internals unless the
implementation changed them or they directly invalidate a bootstrap
requirement. Do not modify `../eth-pir` or any other sibling repository.

## Review procedure

### 1. Establish the baseline

1. Read `docs/agents/bootstrap.md` completely, including its tests and
   non-goals.
2. Read any repository-level `AGENTS.md` instructions.
3. Inspect `git status`, tracked diffs, untracked implementation files, and the
   implementation agent's report.
4. Identify which changes implement bootstrap and which were already present or
   unrelated. Do not use destructive Git commands to manufacture a clean tree.
5. Record the compiler/toolchain and relevant feature configuration used for
   verification.

### 2. Build a requirement trace

Create a coverage table from the requirements in `bootstrap.md`. Do not copy
those requirements into this file in advance. For each requirement, record:

| Requirement reference | Implementation evidence | Test evidence | Status |
| --- | --- | --- | --- |
| section or line in `bootstrap.md` | file and line | test name or command | pass/fail/partial/untested |

At minimum, trace every normative item in:

- CLI;
- stored data;
- bootstrap phases;
- historical address scan;
- balance reads;
- build and validation;
- atomic snapshot commit and recovery;
- RPC requirements and preflight;
- handoff to serve;
- concurrency; and
- tests.

A passing test is not implementation evidence by itself. Inspect the production
path to confirm that the tested behavior is actually wired into the command.

### 3. Static and adversarial review

Review for correctness at boundaries and failure points, with particular
attention to:

- inclusive/exclusive block boundaries and checked arithmetic;
- the distinction between a chain head, confirmed target, loaded cursor, and
  next block;
- transaction ordering between cached work and durable cursors;
- restart behavior at every durable phase;
- classification of missing, matching, unrelated, and corrupt artifacts;
- target-hash checks and the ordering of cache reset or snapshot removal;
- strict decoding of logs, addresses, hashes, balances, and SQLite blobs;
- failures that could be silently converted into zero balances or skipped
  events;
- arithmetic overflow in balances, supplies, counts, and block conversions;
- exact cache-projection versus snapshot comparison;
- atomic-write durability, temporary-path collision, and lock ordering;
- provider truncation, retry counters, adaptive splitting, and terminal
  one-address/one-block failures;
- startup catch-up pinning and validation before the initial endpoint opens;
- restart from a post-bootstrap snapshot whose cursor is later than the original
  bootstrap target;
- snapshot portability to a different destination path or machine; and
- unintended changes beyond the scope guard above.

Check error paths as carefully as success paths. An error message should name
the failed phase, range or address when applicable, preserve resumable work, and
never expose `ETH_RPC_URL`.

### 4. Deterministic verification

Run formatting and the broadest practical deterministic test suite. Start with
the tests closest to the changed modules, then expand to the workspace-level
suite supported by the environment.

Verify that:

- the normal test suite does not require network access or `ETH_RPC_URL`;
- provider behavior is exercised through deterministic mocks or fixtures;
- the cases required by the `Tests` section of `bootstrap.md` exist and assert
  the intended state, not merely an error string;
- crash tests reopen durable artifacts rather than continuing with the original
  in-memory object;
- corruption tests mutate serialized bytes or database state in realistic ways;
- lock tests use distinct processes or descriptors where process semantics
  matter;
- portability tests load a copied USDTPIR3 snapshot from a different path; and
- startup tests prove that the endpoint is not opened before catch-up,
  validation, and initial PIR construction complete.

Use temporary directories for state and cache artifacts. Do not delete or
overwrite user data. Do not weaken a test or production invariant merely to
make the suite pass.

If a broad test cannot run because of CPU features, memory, missing sibling
dependencies, or another environmental limitation, run the largest meaningful
subset and report the exact limitation and skipped coverage.

### 5. Bounded live-RPC checks

Confirm that `ETH_RPC_URL` is set without printing its value. Live checks are
supplementary; deterministic tests remain mandatory.

Use the configured endpoint only for bounded, read-only checks such as:

- Ethereum chain ID;
- confirmed-target calculation and block-hash retrieval;
- a small historical log range for each watched token/event family;
- historical `owner()` state at a selected USDT supply-event block;
- strict Multicall balance decoding at one modern historical block; and
- USDT/USDC `totalSupply()` reads at that same block.

Pin block numbers within each check and record those public block numbers and
results. Use finite timeouts. Do not run a full deployment-to-head bootstrap as
a review smoke test, do not issue an unbounded log request, and do not make the
checked-in test suite depend on endpoint availability.

If the implementation provides ignored or feature-gated live tests, prefer
those. Otherwise use the smallest existing command or test harness that reaches
the production readers. Avoid ad-hoc checks that bypass the code under review.

### 6. Operational handoff review

Review the documented and scripted operator flow without provisioning or
mutating external infrastructure:

1. Bootstrap produces one completed USDTPIR3 snapshot.
2. The snapshot can be copied to a different path/device without the SQLite
   cache.
3. The destination invokes `serve` without `--from-block`.
4. `serve` resumes from the loaded cursor plus one, catches up, validates, builds
   PIR, and only then opens the endpoint.
5. Missing required state fails with actionable transfer instructions.

Review `scripts/run-ec2-demo.sh` statically and, where practical, exercise its
argument/environment branches without launching an instance. Confirm that the
pre-existing AVX2 fallback remains intact.

## Finding severity

Use these severities consistently:

- **Critical:** can produce or serve a silently incorrect holder snapshot, lose
  committed bootstrap work irrecoverably, overwrite unrelated state, or expose
  a secret.
- **High:** violates a required recovery/finality/boundary invariant, permits an
  incomplete snapshot to be accepted, or prevents the documented workflow from
  completing.
- **Medium:** important failure handling, durability, operability, performance,
  or test-coverage defect without demonstrated silent state corruption.
- **Low:** localized maintainability, diagnostic, or documentation problem with
  limited operational impact.

Do not inflate severity because a fix is large. Conversely, do not lower it
because a failure is unlikely on a trusted RPC.

## Required report

Return a self-contained report in this order:

1. **Verdict:** ready, ready with qualifications, or not ready.
2. **Findings:** ordered by severity, then likelihood. For each finding include:
   - concise title and severity;
   - the violated `bootstrap.md` reference;
   - production evidence with file and line;
   - test or reproduction evidence;
   - impact; and
   - a concrete recommended correction.
3. **Requirement trace:** the completed coverage table.
4. **Deterministic verification:** exact commands, results, and skipped tests.
5. **Live-RPC verification:** public blocks/operations checked and results,
   without the RPC URL.
6. **Scope audit:** confirmation that post-startup PIR behavior and sibling
   repositories were not expanded, or details of any expansion.
7. **Residual risks and unvalidated items:** including anything that would
   require a full historical run or production-sized PIR allocation.

If there are no findings, state that explicitly but still provide the coverage
table and verification evidence. Do not claim that a full historical bootstrap
was validated unless it actually completed and its resulting snapshot was
independently checked.

## Exit criteria

Recommend the implementation as ready only when:

- no Critical or High finding remains;
- every normative `bootstrap.md` requirement has implementation evidence;
- every required deterministic test is present and passing, or a clearly
  justified qualification is reported;
- bounded live-RPC checks exercise the production RPC paths without exposing
  credentials;
- the completed snapshot portability/startup flow is demonstrated; and
- no out-of-scope continuous PIR or sibling-library work was introduced.
