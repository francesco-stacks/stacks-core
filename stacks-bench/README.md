<!-- markdownlint-disable MD060 -->
# `stacks-bench`: Stacks-Core Benchmarking Tool

## Features

`stacks-bench` supports the following features

| Feature | Description | CLI Command | MCP Tool | MCP Resource |
| ---- | ---- | ---- | ---- | ---- |
| Run benchmark | Replay a block range recording per-block timing, clarity costs, and profiler data | `bench run` | `run_benchmark` | — |
| Re-run benchmark | Re-run a previous benchmark using its stored parameters | `bench rerun` | `rerun_benchmark` | — |
| List runs | List benchmark runs with optional filters and sorting | `bench list` | `list_runs` | — |
| Show run details | Display detailed stats and profiler hotspots for a run | `bench show` | `get_run_details` | — |
| Delete runs | Delete benchmark runs and all dependent data | `bench remove` | `delete_run` | — |
| Profiler hotspots | View top-N slowest profiler spans for a run | `bench show --profiler-hot N` | `get_hotspots` | — |
| Per-block stats | Paginated per-block timing breakdown and clarity costs | — | `get_block_stats` | — |
| Per-tx stats | Paginated per-transaction timing and clarity costs | — | `get_tx_stats` | — |
| Compare runs | Diff two runs at summary and per-span level | — | `compare_runs` | — |
| Index chainstate | Index blocks from the node database into the app DB | `chainstate index` | `index_chainstate` | — |
| List chainstates | List indexed chainstates | `chainstate list` | `list_chainstates` | — |
| Chainstate details | View chainstate info including epochs and cost budgets | — | `get_chainstate` | — |
| Delete chainstates | Delete chainstates and all associated data | `chainstate remove` | `delete_chainstate` | — |
| Metabase analytics | Launch a pre-configured Metabase instance for BI dashboards | `metabase` | — | — |
| Profiler explorer | Interactive web UI for hierarchical profiler call trees | `explorer start/stop/status` | — | — |
| Schema discovery | Expose the database DDL as MCP resources | — | — | `stacks-bench://schema` |

## MCP Server

`stacks-bench` includes an MCP (Model Context Protocol) server that exposes all benchmark data
and operations as tools and resources. This allows AI agents to run benchmarks, query results,
compare runs, and explore the database schema programmatically.

The server uses stdio transport and supports progress notifications for long-running operations
(indexing, benchmarking).

### Configuration

Configure `stacks-bench` as a `STDIO`-style MCP server. Example:

```json
{
  "mcpServers": {
    "stacks-bench": {
      "command": "cargo",
      "args": [
        "run",
        "-p", "stacks-bench",
        "--release",
        "--",
        "--db", "/path/to/stacks-bench-data",
        "mcp"
      ]
    }
  }
}
```

### Tools

| Tool | Description | Destructive |
| ---- | ---- | ---- |
| `list_runs` | List benchmark runs with optional filters (name, incomplete) | No |
| `get_run_details` | Detailed run info with summary stats and top-N profiler hotspots | No |
| `get_hotspots` | Profiler hotspots sorted by estimated self wall time | No |
| `get_block_stats` | Paginated per-block timing, clarity costs, and storage delta | No |
| `get_tx_stats` | Paginated per-tx timing and clarity costs, filterable by block | No |
| `compare_runs` | Summary-level and per-span diff between two runs | No |
| `list_chainstates` | List indexed chainstates with run counts | No |
| `get_chainstate` | Chainstate detail including epochs and cost budgets | No |
| `run_benchmark` | Run a benchmark (block range or single tx) with progress notifications | Yes |
| `rerun_benchmark` | Re-run a previous benchmark by ID | Yes |
| `index_chainstate` | Index chainstate blocks into the app DB | Yes |
| `delete_run` | Delete a benchmark run and all dependent data | Yes |
| `delete_chainstate` | Delete a chainstate and all associated runs | Yes |

### Resources

| URI | Description |
| ---- | ---- |
| `stacks-bench://schema` | SQL DDL for the stacks-bench database (excludes internal/staging tables) |
| `stacks-bench://schema/{table}` | DDL for a single table and its indexes |

## Hardware Disk Qualification Test

`stacks-bench` is sensitive to disk speed. If your storage cannot deliver fast random reads/writes, benchmark results will reflect storage bottlenecks atypical of a production node.

Internal NVMe storage is strongly recommended. High-quality USB4/Thunderbolt NVMe can also pass.

---

### Run the qualification test

Run this `fio` command in a temporary directory on the target disk:

```text
fio --name=stacks-hw-check \
    --ioengine=psync --direct=1 \
    --rw=randrw --rwmixread=70 \
    --bs=16k \
    --size=8G --numjobs=1 --iodepth=1 \
    --time_based=1 --runtime=30 \
    --refill_buffers=1 --randrepeat=0 \
    --fsync=0
```

This roughly simulates the mixed random I/O pattern of the Stacks MARF state and SQLite commit operations.

---

### How to interpret results

Focus only on the final **READ**/**WRITE** bandwidth values:

| Metric           | Minimum Recommended | Meaning |
|-----------------|-------------------:|--------|
| **Read BW**     | ≥ **300 MiB/s**     | MARF/state reads are not bottlenecked |
| **Write BW**    | ≥ **120 MiB/s**     | Commit path is not blocked on disk |

_(Thresholds based on typical NVMe performance in cloud production setups: AWS `m6i` + `gp3`/`gp4`)_

Example (PASS):

```text
READ: bw=418MiB/s
WRITE: bw=179MiB/s
```

---

### PASS / WARNING / FAIL guidance

| Result | Interpretation | Action |
|--------|----------------|--------|
| **PASS** | Meets recommended thresholds | Benchmark results are valid |
| **WARNING** | Slightly below thresholds | Results may under-represent node performance |
| **FAIL** | Well below thresholds | Disk is a bottleneck → upgrade or move benchmark to faster storage |

---

### Summary

- This test is the **official storage sanity-check** for `stacks-bench`
- Ensures reliable and comparable benchmark results
- Recommended environment: internal NVMe _or_ high-quality USB4/Thunderbolt NVMe enclosure

## Benchmark Data Storage

`stacks-bench` stores its benchmarking data in an SQLite database at `~/.stacks-bench/appdata/stacks-bench.db`.

The data directory can be overridden in three ways (highest priority first):

1. **`--db <path>`** CLI flag
2. **`STACKS_BENCH_DATA_DIR`** environment variable
3. **Default:** `~/.stacks-bench`

Using a fixed home-relative path means benchmark data is shared across worktrees, making cross-branch comparisons straightforward.

## Analyzing Benchmark Data

To help users better understand their benchmarking/profiling data, a small [Metabase](https://www.metabase.com/) instance is supplied with pre-configured questions/dashboards.

Running `stacks-bench metabase` will have `stacks-bench` setup/configure a PostgreSQL database for Metabase as well as Metabase itself using Docker. You can then access Metabase at <http://localhost:3000/>.

### Backing up the Metabase database

```bash
docker exec stacks-bench-postgres \
  pg_dump -U metabase -F c metabase \
  > metabase_backup_$(date +"%Y%m%d%H%M").dump
```

### Restoring the Metabase database

First, connect to the database:

```bash
psql -h localhost -p 5432 -U metabase -d metabase
```

Create a metabase user and database to use as a restore target:

```sql
CREATE USER metabase WITH PASSWORD 'metabase';
CREATE DATABASE metabase OWNER metabase;
\q
```

Restore the Metabase backup into the newly created database:

```bash
pg_restore \
  -h localhost \
  -p 5432 \
  -U metabase \
  -d metabase \
  --clean --if-exists \
  BACKUP_FILE.dump
```

### Cleaning the Metabase database

```sql
-- Clear logs and execution history
TRUNCATE TABLE query_execution;
TRUNCATE TABLE task_history;
TRUNCATE TABLE view_log;
TRUNCATE TABLE login_history;

-- Clear cache (forces Metabase to fetch fresh data)
TRUNCATE TABLE query_cache;
TRUNCATE TABLE metabase_fieldvalues; -- Clears cached filter dropdown values

-- Clear user sessions (Forces all users to log in again)
TRUNCATE TABLE core_session;

-- Clear activity stream (removes "user X created dashboard Y" history)
TRUNCATE TABLE activity;

-- (Optional) Clear edit history
-- Only run this if you want to remove the "Undo" history for questions/dashboards.
-- If you want to keep the version history of how the dashboard was built, skip this.
TRUNCATE TABLE revision;
```
