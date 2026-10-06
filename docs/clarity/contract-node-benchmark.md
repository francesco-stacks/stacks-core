# Contract-loading node benchmark

Compare the Postcard candidate with an unchanged node to measure the overall
change. The candidate reuses deployment resolution, loads selected functions, borrows function
definitions and shares immutable common data. Cache lifetime remains one
transaction, and the interpreter uses ordinary owned ASTs.

## Build

From the repository root:

```sh
contrib/contract-loading/build-benchmarks.sh CONTROL_COMMIT
```

The required control revision must precede the storage changes. The script builds
that unchanged checkout and the working-tree candidate
with the same toolchain, release profile and flags. It starts no node and opens
no chainstate. Outputs under `target/contract-loading-live/`:

| File | Purpose |
| --- | --- |
| `stacks-node-original` | Unchanged full-contract JSON control |
| `stacks-node-postcard` | Optimized selective loader; Postcard headers and functions |
| `build-info.json` | Control commit, source fingerprint, feature flags and binary hashes |

An optional second argument chooses the output directory. Keep the generated
manifest with results: a commit ID alone does not identify uncommitted changes.

To build only the candidate:

```sh
cargo build --locked --release -p stacks-node
```

Rebuild on the benchmark host with identical `RUSTFLAGS` for both variants.

## Clone the original snapshot

Use a separate working directory cloned from the same **stopped original
snapshot** for each variant. Candidate startup migrates executable metadata;
the unchanged node cannot read a migrated database. Use an original snapshot for both variants.

On this Mac, use the helper that fails if copy-on-write cloning is unavailable:

```sh
python3 contrib/contract-loading/clone-snapshot-macos.py /path/to/original-snapshot /path/to/new-benchmark-directory
```

The destination must be new, its parent must exist, and both paths must be on
the same filesystem. The helper calls `clonefile(2)` for regular files, rejects
symlinks/special files, and never falls back to a full copy. A failed clone can
leave a partial destination. macOS `cp -c` can silently fall back to a full copy.
On Linux with reflinks, use `cp -a --reflink=always SOURCE NEW_DESTINATION`.

Preserve the entire snapshot, including WAL/SHM and MARF `.blobs` files. Cloning
a running node does not produce a consistent snapshot. Migration/new blocks
consume space as shared extents are rewritten; run variants sequentially with
one disposable clone at a time when space is limited. Keep the original intact.

Set `[node].working_dir` in each config to its clone, matching the snapshot layout
(for mainnet this root contains `mainnet/`). Start the selected binary:

```sh
target/contract-loading-live/stacks-node-postcard start --config /path/to/postcard.toml
```

## Compare equal work

1. Measure startup migration separately from steady block processing.
2. Process the same block interval. During live catch-up, separate network wait
   from processing and compare common block IDs, not only equal elapsed time.
3. Run one variant at a time with identical logging/settings/resources. Repeat
   in alternating order. Record wall/CPU time, peak RSS, blocks/transactions and
   per-block latency where available.
4. Verify equal accepted blocks, resulting logical state, receipts, events and
   execution costs. SQLite file hashes differ because metadata layout changes.
5. Include deployment-heavy blocks and broad dependency closures. Binary encoding
   adds deployment work; selective-loading gains depend on the called functions.

Execution costs and memory reservation remain unchanged in every epoch,
including 4.1. The candidate retains the original source-length-plus-constant-data
charge, so comparisons with the unchanged node must also agree on costs when
replaying across the epoch boundary. Cache weights are implementation details.

## Focused validation

The [storage document](contract-storage.md) describes the format and migration. Core verification commands:

```sh
cargo nextest run --locked -p clarity --lib
cargo nextest run --locked -p clarity --lib vm::database:: --features developer-mode,rollback_value_check
cargo nextest run --locked -p stackslib --lib clarity_vm::
cargo nextest run --locked -p stackslib --lib chainstate::stacks::db::snapshot::tests::clarity::
cargo nextest run --locked -p stackslib --lib net::api::tests::getclaritymetadata::
cargo clippy-stacks
cargo clippy-stackslib
cargo fmt-stacks --check
```

For load-only measurements and a read-only corpus audit:

```sh
cargo bench --locked -p clarity --features testing --bench contract_loading -- --quick
cargo run --release -p clarity --features testing --example contract_storage_snapshot -- audit /path/to/original/marf.sqlite /path/to/inventory.json
```

The benchmark includes large constants, charged calls with dependencies, explicit
and implicit trait dispatch, batches of eight separate transactions, and cache
pressure. It counts backing-store operations, including the two metadata batches
for a charged cold call. `legacy_cache_lookup_only` is a HashMap component timing;
compare the transaction cases for end-to-end work, not that component with
`split_cached_load`. Ephemeral-store tests separately exercise the production
query and check that its work remains bounded as unrelated metadata grows.
Its in-memory store does not measure actual MARF traversal or disk I/O.
The corpus tool opens audit and extraction sources read-only; migration requires
an explicit disposable database. Generated reports belong outside Git. Use node
replay to establish block-processing gains.

## Cold SQLite page-cache measurements

An empty VM cache does not imply cold SQLite pages or cold filesystem reads.
The corpus tool's `bench` mode uses an in-memory store with warm SQLite pages.
Use `bench-disk` to measure metadata retrieval and decoding from real files:

```sh
cargo build --locked --release -p clarity --features testing --example contract_storage_snapshot
mkdir /path/to/legacy-metadata
target/release/examples/contract_storage_snapshot extract /path/to/original/marf.sqlite /path/to/legacy-metadata/marf.sqlite
# Preserve the extracted directory; create a disposable clone for migration.
# Both benchmark inputs should contain the same complete extracted corpus.
python3 contrib/contract-loading/clone-snapshot-macos.py /path/to/legacy-metadata /path/to/postcard-metadata
target/release/examples/contract_storage_snapshot migrate /path/to/postcard-metadata/marf.sqlite /path/to/migration.json
target/release/examples/contract_storage_snapshot bench /path/to/legacy-metadata/marf.sqlite /path/to/postcard-metadata/marf.sqlite /path/to/inventory.json /path/to/sample.json
target/release/examples/contract_storage_snapshot bench-disk /path/to/legacy-metadata/marf.sqlite /path/to/postcard-metadata/marf.sqlite /path/to/sample.json /path/to/cold.json
```

The sample selects the same deployments and entrypoints as the warm benchmark.
The baseline reconstructs original monolithic JSON loading with three point
reads; the candidate uses the production selective loader and batch queries.
Both use current VM types and canonicalization, so this isolates the storage
format/load path rather than comparing complete unchanged/candidate binaries.
There is no VM cache. Deployment and epoch are provided by the fixture;
MARF traversal, transaction execution and consensus charging are excluded.

Each case has 40 individual timed loads in balanced alternating order. Cold cases
run `PRAGMA shrink_memory` outside the timer before every load and assert positive
SQLite `CACHE_MISS` counts. Memory mapping is disabled and each connection has a
2,000 KiB page-cache target. Reports include median, p10/p90 and page misses.
Primed cases perform an untimed load immediately before measurement; a contract
larger than the page-cache budget can still miss, as the counters show. Timings
include decoding, canonicalization, construction and destruction of the context.
Page misses verify that clearing the SQLite cache had an effect; they are not
total file reads or bytes. SQLite's [direct overflow reads](https://www.sqlite.org/compile.html#direct_overflow_read)
can bypass that cache for large JSON strings and BLOBs.

**The OS file cache is uncontrolled. These are SQLite-page-cold measurements,
not physical-disk-cold measurements or node throughput.** Neither opening a new
SQLite connection nor clearing its page cache evicts the OS cache. Measure disk
and MARF effects separately during node replay on the benchmark host.

On the supplied mainnet snapshot, two runs of 118 sampled entrypoints in 53
contracts gave **1.549× and 1.546× median speedups** over the full JSON storage
path with cold SQLite pages. The files contained the complete 141,938-contract
extraction, about 3.6 GB per format. This is an unweighted sample including all
boot contracts, not a transaction-frequency-weighted node speedup.

Representative medians from the repeat run (SQLite page cache cold, VM cache absent):

| Entrypoint | Original full JSON | Selective Postcard | Speedup |
| --- | ---: | ---: | ---: |
| `pox-4.current-pox-reward-cycle` | 374.94 µs | 29.17 µs | 12.86× |
| `pox-4.stack-stx` | 372.81 µs | 115.42 µs | 3.23× |
| `aaabridge.set_master_contract` | 1,373.00 µs | 45.83 µs | 29.96× |
| `aaabridge.transfer` (505/506 functions) | 1,381.15 µs | 1,237.81 µs | 1.12× |
| `test-x.hi` (one function) | 10.58 µs | 12.56 µs | 0.84× |

Both runs showed regressions in **53/118 cases**, all contracts with one or two
functions and 547–1,590 bytes of original JSON. Among those regressions, the
repeat run's median time increase was 5.1%, and the worst was 18.7% (about 2 µs).
Small contracts have little decoding work to save, so extra row/page work can
outweigh selective loading. Sparse calls into large contracts benefit much more
than dense calls. The same file-backed fixture with primed SQLite pages measured
1.692× median in the repeat run; compare cache conditions within this fixture,
not against a different in-memory benchmark.

Raw reports are generated under `target/contract-loading-live/mainnet-cold.json`
and `mainnet-cold-repeat.json`; generated measurements remain outside Git.

## Custom codec experiment

A separate prototype replaced Postcard with handwritten contract/AST encoding
and the value codec from [PR #7534](https://github.com/stacks-network/stacks-core/pull/7534),
pinned at `acca986e8b7097ebcf2faab9d2908527103b419c`. A second iteration inlined
small integers and booleans to avoid packed-record overhead for AST literals.
Both round-tripped all 141,938 contracts and 1,570,258 functions. Thirty-five
value encodings needed a lossless fallback to preserve historical runtime
metadata; reconstructing consensus bytes alone was insufficient.

Against the **current split Postcard implementation**, the tuned prototype's
cold SQLite median speedups were 0.998× and 0.994× across the same 118 entrypoints.
Summing sampled median load times gave 1.087× and 1.066×, with gains concentrated
in dense calls. Several small PoX calls regressed. The synthetic large-constant
case improved 1.38× with warm SQLite pages. Neither measurement estimates node
throughput or physical-disk-cold performance. Total encoded payload changed by
only +0.057%.

The candidate retains Postcard: the mixed gains do not justify roughly 867
lines of custom codec implementation versus the existing 246-line codec.
Narrower reuse of #7534 for constants remains worth testing; that hybrid has not
been measured. Experimental source, reproducible patches, binaries and reports
are retained outside Git under
`target/contract-loading-history/packed-contract-prototype/`.
