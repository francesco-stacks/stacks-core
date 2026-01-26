# `stacks-bench`: Stacks-Core Benchmarking Tool

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
- Recommended environment: internal NVMe _or_ high-quality USB4/TB3 NVMe

## Benchmark Data Storage

`stacks-bench` stores its benchmarking data in an SQLite database, which by default is written
to `./.stacks-bench/appdata/stacks-bench.db`, relative to the `stacks-bench` binary's working directory (e.g. the workspace `target/release/` directory if executed using `cargo run` or the `cargo stacks-bench` alias).

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
psql -h localhost -p 5432 -U USERNAME -d PASSWORD
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
