# marf-squash

Offline CLI for producing Pruned Chainstate Snapshots (PCS) from a Stacks node's
chainstate. Squashes the three MARFs (Clarity, Index, Sortition), copies
canonical block data and Bitcoin auxiliary files, generates a self-describing
manifest with SHA-256 checksums for the fixed artifacts plus one aggregate hash
for the epoch-2 block archive, and provides offline verification against WSCP
checkpoints.

## Build

From the repository root:

```bash
cargo build -p marf-squash --release
```

## Subcommands

| Command | Purpose |
|---------|---------|
| `squash` | Produce a complete PCS from a running node's chainstate |
| `validate` | Producer-side comparison of squashed output against source |
| `verify` | Consumer-side offline verification of a standalone PCS |

## Usage

### Produce a full PCS

```bash
marf-squash squash \
  --chainstate /data/mainnet \
  --out-dir /tmp/pcs \
  --tenure-start-bitcoin-height 880000 \
  --all
```

`--all` squashes all three MARFs, copies canonical block data, copies Bitcoin
auxiliary files, and generates a `PCS_manifest.toml` with SHA-256 checksums for
the fixed artifacts plus one aggregate hash for the epoch-2 block archive under
`chainstate/blocks/`.

Individual MARFs can be squashed selectively with `--clarity`, `--index`, or
`--sortition`. `--blocks` requires `--index` (or `--all`); `--bitcoin` requires
`--sortition` (or `--all`). A node config can be supplied with `--config`.

Flags:

- `--skip-validate` - skip post-squash validation (faster, less safe)
- `--full` - full leaf-by-leaf comparison instead of hash-based check (slow)

### Validate against source

Producer-side check that compares a squashed output against the original
chainstate:

```bash
marf-squash validate \
  --source-chainstate /data/mainnet \
  --squashed-chainstate /tmp/pcs/mainnet \
  --tenure-start-bitcoin-height 880000 \
  --all
```

### Verify a standalone PCS

Consumer-side offline verification of a PCS directory. Does not require access
to the original chainstate. `verify` only accepts a full PCS produced with
`marf-squash squash --all`. It runs four verification levels:

| Level | Check |
|-------|-------|
| 0 | Directory cleanliness - no extra files, symlinks, or SQLite sidecars |
| 1 | SHA-256 verification of fixed artifacts plus the aggregate epoch-2 block archive hash |
| 2 | Squash root node hash recomputation from MARF contents |
| 3 | WSCP checkpoint comparison (requires `--checkpoint-file`) |

```bash
marf-squash verify \
  --pcs-dir /tmp/pcs/mainnet \
  --checkpoint-file checkpoint.toml
```

The checkpoint file is a TOML file with trusted squash root hashes published by
an independent source:

```toml
stacks_height = 150000
bitcoin_height = 880000
clarity_squash_root_node_hash = "0x..."
index_squash_root_node_hash = "0x..."
sortition_squash_root_node_hash = "0x..."
```

Levels 0-2 always run. Level 3 runs only when `--checkpoint-file` is provided.

## PCS output layout

A full PCS (`--all`) mirrors the node's working directory structure:

```
/tmp/pcs/
├── chainstate/
│   ├── vm/
│   │   ├── clarity/
│   │   │   ├── marf.sqlite
│   │   │   └── marf.sqlite.blobs
│   │   ├── index.sqlite
│   │   └── index.sqlite.blobs
│   └── blocks/
│       ├── nakamoto.sqlite
│       └── {XX}/{YY}/{hash}... # Epoch 2.x blocks
├── burnchain/
│   ├── sortition/
│   │   ├── marf.sqlite
│   │   └── marf.sqlite.blobs
│   └── burnchain.sqlite
├── headers.sqlite
└── PCS_manifest.toml
```

## The PCS manifest

`PCS_manifest.toml` is a self-describing record of the snapshot: the three MARFs'
squash root node hashes and archival MARF root hashes, the block range, and
SHA-256 checksums (file-level for the fixed artifacts, one aggregate hash for the
epoch-2 block archive). It is written by `squash` for a full PCS (`--all`) and
consumed by `verify`.

The squash root node hashes are the intended trust anchor: `verify`
authenticates them against an independently published checkpoint. The manifest
itself is part of the untrusted artifact and is not authenticated.

## Using a PCS to bootstrap a node

1. Produce or download a PCS directory.
2. Verify it: `marf-squash verify --pcs-dir /data/my-node/mainnet --checkpoint-file checkpoint.toml`
3. Set `[node].working_dir` in your Stacks config to the **parent** of the PCS
   directory (e.g. `/data/my-node`).
4. Start the node normally.

The node is unaware it is running from a PCS. All trust verification happens
offline via `marf-squash verify`.

## Trust model

- **WSCP (Weak-Subjectivity Checkpoint)** authenticates the three squashed MARFs
  via their recomputed content hashes. These are the trust anchor.
- **Manifest checksums** verify artifact integrity: file-level SHA-256 for the
  fixed artifacts and one aggregate hash for the epoch-2 block archive. The
  manifest itself is part of the untrusted artifact - it is NOT authenticated by
  the WSCP.
- Level 2 recomputes squash root hashes by walking the trie structure
  bottom-up. It does not trust stored SQL metadata.
