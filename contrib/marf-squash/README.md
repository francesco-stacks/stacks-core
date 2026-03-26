# marf-squash

Offline CLI for producing Genesis State Snapshots (GSS) from a Stacks node's
chainstate. Squashes the three MARFs (Clarity, Index, Sortition), copies
canonical block data and Bitcoin auxiliary files, generates a manifest with
SHA-256 checksums, and provides offline verification against WSCP checkpoints.

## Build

From the repository root:

```bash
cargo build -p marf-squash --release
```

## Subcommands

| Command | Purpose |
|---------|---------|
| `squash` | Produce a complete GSS from a running node's chainstate |
| `validate` | Producer-side comparison of squashed output against source |
| `verify` | Consumer-side offline verification of a standalone GSS |
| `latest-height` | Print the latest confirmed block height in a MARF |

## Usage

### Produce a full GSS

```bash
marf-squash squash \
  --chainstate /data/mainnet \
  --out-dir /tmp/gss \
  --tenure-start-bitcoin-height 880000 \
  --all --blocks
```

`--all` squashes all three MARFs (Clarity, Index, Sortition). `--blocks` copies
canonical block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
When all three MARFs and blocks are included, Bitcoin auxiliary files
(burnchain.sqlite, headers.sqlite) are also copied and a `GSS_manifest.toml` is
generated with SHA-256 checksums for every artifact.

Individual MARFs can be squashed selectively with `--clarity`, `--index`, or
`--sortition`. `--blocks` requires `--index` (or `--all`).

Flags:

- `--skip-validate` - skip post-squash validation (faster, less safe)
- `--full` - full leaf-by-leaf comparison instead of hash-based check (slow)

### Validate against source

Producer-side check that compares a squashed output against the original
chainstate:

```bash
marf-squash validate \
  --source-chainstate /data/mainnet \
  --squashed-chainstate /tmp/gss \
  --tenure-start-bitcoin-height 880000 \
  --all --blocks
```

### Verify a standalone GSS

Consumer-side offline verification of a GSS directory. Does not require access
to the original chainstate. Runs four verification levels:

| Level | Check |
|-------|-------|
| 0 | Directory cleanliness - no extra files, symlinks, or SQLite sidecars |
| 1 | SHA-256 checksum verification of every artifact |
| 2 | Squash root node hash recomputation from MARF contents |
| 3 | WSCP checkpoint comparison (requires `--checkpoint-file`) |

```bash
marf-squash verify \
  --gss-dir /tmp/gss \
  --checkpoint-file checkpoint.toml
```

The checkpoint file is a TOML file with trusted squash root hashes published by
an independent source:

```toml
height = 150000
clarity_squash_root_node_hash = "0x..."
index_squash_root_node_hash = "0x..."
sortition_squash_root_node_hash = "0x..."
```

Levels 0-2 always run. Level 3 runs only when `--checkpoint-file` is provided.

### Report the latest height

```bash
marf-squash latest-height \
  --chainstate /data/mainnet \
  --index
```

Also accepts `--clarity` or `--sortition` (prints burn block height).

## GSS output layout

A full GSS (`--all --blocks`) mirrors the node's working directory structure:

```
/tmp/gss/
├── chainstate/
│   ├── vm/
│   │   ├── clarity/
│   │   │   ├── marf.sqlite
│   │   │   └── marf.sqlite.blobs
│   │   └── index.sqlite
│   │       └── index.sqlite.blobs
│   └── blocks/
│       ├── nakamoto.sqlite
│       └── {XX}/{YY}/{hash}...
├── burnchain/
│   ├── sortition/
│   │   └── marf.sqlite
│   └── burnchain.sqlite
├── headers.sqlite
└── GSS_manifest.toml
```

## Using a GSS to bootstrap a node

1. Produce or download a GSS directory
2. Verify it: `marf-squash verify --gss-dir /data/my-node/mainnet --checkpoint-file checkpoint.toml`
3. Set `[node].working_dir` in your Stacks config to the **parent** of the GSS directory (e.g. `/data/my-node`)
4. Start the node normally

The node is unaware it is running from a GSS. All trust verification happens
offline via `marf-squash verify`.

## Trust model

- **WSCP (Weak-Subjectivity Checkpoint)** authenticates the three squashed MARFs
  via their recomputed content hashes. These are the trust anchor.
- **Manifest checksums** verify artifact integrity (file-level SHA-256). The
  manifest itself is part of the untrusted artifact - it is NOT authenticated by
  the WSCP.
- Level 2 recomputes squash root hashes by walking the trie structure
  bottom-up. It does not trust stored SQL metadata.
