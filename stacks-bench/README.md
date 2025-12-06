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
