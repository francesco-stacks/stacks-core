# marf-squash

Offline CLI for squashing MARF databases and validating the results. Supports
the Index MARF and Clarity MARF.

## What it does

- Squashes MARFs at a target height into compact snapshots.
- Validates squashed MARFs against a source chainstate.
- Reports the latest confirmed height.
- Optionally copies Clarity side-storage tables (`data_table`, `metadata_table`)
  into the squashed output when `--clarity-db` is provided.

## Build

From the repository root:

```bash
cargo build -p marf-squash --release
```

## Usage

### Squash MARFs from a chainstate folder

Index MARF only:

```bash
marf-squash squash \
  --chainstate /path/to/chainstate-folder-name \
  --height 150000 \
  --out-dir /tmp/squashed \
  --index
```

Clarity MARF only (copies side tables automatically):

```bash
marf-squash squash \
  --chainstate /path/to/chainstate-folder-name \
  --height 150000 \
  --out-dir /tmp/squashed \
  --clarity
```

Both Index + Clarity:

marf-squash squash \
  --chainstate /path/to/chainstate-folder-name \
  --height 150000 \
  --out-dir /tmp/squashed \
  --all

Notes:

- `--all` currently includes `--clarity` and `--index` only (burnchain/sortition
  not yet supported).
- Use `--skip-validate` to skip validation and speed up size measurements.
- Use `--full` for a full leaf-by-leaf validation (slow).

### Validate a squashed MARF

```bash
marf-squash validate \
  --source-chainstate /path/to/chainstate-folder-name \
  --squashed-chainstate /tmp/squashed \
  --height 150000 \
  --index
```

Clarity validation (includes side-table checks):

```bash
marf-squash validate \
  --source-chainstate /path/to/chainstate-folder-name \
  --squashed-chainstate /tmp/squashed \
  --height 150000 \
  --clarity
```

### Report the latest height

```bash
marf-squash latest-height \
  --chainstate /path/to/chainstate-folder-name \
  --index
```

## Output layout

`squash` writes a new database and blobs file into `--out-dir` with the same
base filename as the input:

```
/tmp/squashed/
  index.sqlite
  index.sqlite.blobs
```

## Troubleshooting

- Ensure the chainstate folder contains the standard layout:
  `chainstate/vm/clarity/marf.sqlite`, `chainstate/vm/index.sqlite`.
- Use `--clarity` or `--index` explicitly if `--all` is not desired.
