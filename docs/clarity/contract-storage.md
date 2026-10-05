# Executable contract storage

Deployment analyzes the complete source, evaluates its top-level expressions,
initializes state, and saves analysis after successful execution and
post-condition checks. Executable code is stored separately from source,
analysis, and mutable chainstate. Execution costs and VM memory reservations
retain their historical values in every epoch.

## Disk format

Each deployment has these SQLite metadata records:

* `vm-metadata::9::contract-header`: constants, traits, state definitions,
  Clarity version, and a sorted function directory.
* `vm-metadata::9::contract-function::<name>`: one executable function.

Both records are Postcard BLOBs. The `SCHD` and `SCFN` envelopes identify the
record kind and binary schema version, currently 1. There is no version in the
payload or key. The function schema preserves expression IDs and available
diagnostics in fixed positions across `developer-mode` builds. A build without
`developer-mode` discards stored diagnostics when decoding. Use a build with
`developer-mode` to preserve spans and comments when migrating a developer
database or reconstructing its RPC response. Fixed-byte tests
cover headers, function bodies, nested types and values, and both diagnostic
layouts. An incompatible schema change requires a new envelope version.

The directory stores direct dependencies as indices into its sorted names.
Validation checks name order, uniqueness, and index bounds. The loader moves the
names into a shared VM namespace and keeps dependency indices in the loaded
header. The VM context does not depend on the disk directory type.

Deployment and migration borrow shared constants and function definitions for
encoding, avoiding a second copy of the complete contract. Encoded bytes enter the rollback log once and are
committed without JSON conversion. Backing stores must preserve text and binary
metadata explicitly. Generic text metadata reads reject BLOBs; the historical
`contract` metadata RPC explicitly reconstructs the original JSON shape and
field order. Internal executable keys are not accepted by the RPC.

## Loading

A charged call reads the original size records and raw header in one batch,
charges, decodes the header, and fetches the selected function bodies in a
second batch. Both batches reuse the same resolved deployment block. Charging
happens before missing-header, decoding, canonicalization, and body errors.
Storage I/O failures can occur during the initial raw read.

The selected function includes its transitive local dependencies. The scanner
conservatively includes every atom naming a local function, except names that
resolve to a native function first. This must follow the resolution order in
`lookup_function`: reserved variable names are not native functions. External
entrypoints select their definition directly, including implementations with a
native name. Read-only expressions use the same dependency rules. Extra selected
bodies affect implementation work, not consensus costs. A declared function
whose body was omitted produces an internal error rather than an ordinary
undefined-function failure.

Constant queries and native cost setup load only shared metadata. Trait checks
load shared metadata and, for implicit implementations, the selected definition
without dependencies. Normal execution subsequently charges and loads the
needed bodies. PoX event synthesis also starts with shared metadata; each event
expression selects its own function bodies. Existence checks inspect raw metadata
without interpreting it. The duplicate-deployment probe retains full loading
to preserve historical constant-sanitization errors.

Persistent stores use indexed batch reads. Ephemeral stores filter both
underlying tables before combining their results; pending ephemeral rows override
matching disk rows in both scalar and batch reads. Results preserve caller order,
missing rows, and duplicates. Metadata commits rebuild the ephemeral views once
per batch. Tests verify bounded query work as unrelated metadata grows and
restoration of views after insertion errors.

## Ownership and accounting

Function lookup borrows definitions. Constants and function definitions have
shared immutable ownership. The context exposes `loaded_functions()` for the
available subset and `has_function()` for the complete namespace.

The execution cache belongs to one transaction. It retains headers and function
bodies under a 5 MiB encoded-byte budget, limiting retained work across repeated
calls without an unbounded contract working set. This is not an exact decoded
heap limit. Eviction may cause another read and decode. Pending writes and
historical views bypass the cache. The stored epoch is read on each load,
including hits; this adds a MARF read to hits compared with the original loader.

Every load uses the historical charge and memory reservation:

```text
original source length + constant-data size
```

Neither cache hits nor function selection change that amount. Mutable state,
execution costs, events, and writes are not cached.

## Constants and types

All constants are decoded and sanitized using the execution epoch's rules. Trait
and argument types use the existing canonicalization rules, including the epoch
2.2 exception. Recursive decoding retains stack protection. The header reserves
a false sanitizer flag for a later compatible optimization; readers do not skip
sanitization.

## Migration and recovery

Node startup migrates monolithic JSON records in the database-migration step,
before opening chainstate for ordinary processing. It does not rerun analysis or
initialization. The chainstate version advances to 15 **before** record conversion,
so an older node refuses even a partially migrated database. The Clarity format
completion marker is written only after conversion finishes.

Ordinary `StacksChainState::open`, `MarfedKV::open`, and `open_unconfirmed`
refuse existing databases that need conversion before opening writable handles.
New databases start in the current format. Explicit migration APIs support
confirmed and unconfirmed stores. Deployment identities, forks, unconfirmed
records, and MARF commitments are preserved.

Migration processes at most 256 contracts per transaction and buffers at most
16 MiB of source records plus the final individual record. Each batch acquires a
write lock before scanning. Replacement records and deletion of the originals
commit atomically. An error rolls back that batch; earlier batches survive and
remaining original records identify the work to resume. Completion is recorded
in `clarity_contract_storage`. Unsupported storage formats are rejected.

No chain resync is required. Inspection tools and clarity-cli do not migrate an
existing database implicitly. To inspect legacy chainstate with a new build,
make a disposable clone and explicitly run:

```sh
stacks-inspect migrate-contract-storage /path/to/clone/chainstate
```

The path must contain `vm/index.sqlite` and `vm/clarity/marf.sqlite`. This command
sets the node's downgrade guard and converts executable metadata. For a standalone
Clarity database, such as a clarity-cli database or an extracted metadata copy:

```sh
stacks-inspect migrate-contract-storage --standalone /path/to/clone/marf.sqlite
```

Standalone databases have no node schema version; do not reopen them with older
Clarity tools after migration. Both commands resume interrupted conversion and
refuse missing files, unrelated databases, and unsupported formats. Retain an
original snapshot for rollback and benchmark comparisons. Snapshot export and
import preserve executable BLOBs and the completion marker.

## Compatibility validation

Tests cover charging and error order, rollback, forks, migration restart, RPC
bytes from the original writer, and reserved-name dispatch. Frozen reference
results come from unchanged commit `3fd5d990f4d53562dd1def3eb7849ae801c2d807`:
5,696 boot-entrypoint cases across epochs 2.2, 3.3, 3.4 and 4.0, plus successful
PoX calls on a real MARF store with native events and committed state roots.
Epoch 4.1 has behavioral tests but is excluded from frozen historical results
while its consensus rules are under development. Regeneration instructions live
with `contract_storage_baseline.rs`, `contract_storage_pox_baseline.rs`, and
`contract_storage_fixtures.rs` under `stackslib/src/clarity_vm/tests/`.
