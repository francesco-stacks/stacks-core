#!/usr/bin/env python3
"""Verify the cycle-from-burn-height SQL formula used by
`NakamotoChainState::get_reward_set_below_squash`.

The runtime fast path computes a reward-cycle number from each
`nakamoto_block_headers.burn_header_height` using:

    CASE WHEN (bh - first_burn) % len = 0
         THEN (bh - first_burn) / len
         ELSE (bh - first_burn) / len + 1
    END

This differs from the canonical `block_height_to_reward_cycle` (plain integer
division) and is supposed to map a calc block to the cycle whose reward set
it produced — i.e. canonical_cycle(block) + 1 for blocks in the previous
cycle's prepare phase.

This script opens a real chainstate index DB, walks `nakamoto_reward_sets`
joined with `nakamoto_block_headers`, and checks for every row that:
  - the formula's cycle equals canonical_cycle + 1, AND
  - the block lies in cycle N-1's prepare phase.

If every row passes, the formula is sound for the data the runtime will see.
Any mismatch points to either a real bug or an unconsidered edge case.

Usage:
    python3 verify_cycle_math.py <chainstate-dir> \\
        --first-burn 666050 \\
        --cycle-length 2100 \\
        --prepare-length 100

Mainnet PoX defaults (from stackslib/src/core/mod.rs):
    --first-burn 666050 --cycle-length 2100 --prepare-length 100
"""

import argparse
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path


def sql_formula_cycle(bh: int, first_burn: int, cycle_len: int) -> int:
    """Replicate the SQL CASE expression from get_reward_set_below_squash."""
    diff = bh - first_burn
    if diff % cycle_len == 0:
        return diff // cycle_len
    return diff // cycle_len + 1


def canonical_cycle(bh: int, first_burn: int, cycle_len: int) -> int:
    """Plain integer division — what static_block_height_to_reward_cycle does."""
    return (bh - first_burn) // cycle_len


def is_in_prepare_phase(bh: int, first_burn: int, cycle_len: int, prepare_len: int) -> bool:
    """Last `prepare_len` blocks of the current canonical cycle."""
    offset = (bh - first_burn) % cycle_len
    return offset >= cycle_len - prepare_len


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("chainstate_dir", help="path to chainstate dir containing vm/index.sqlite")
    parser.add_argument("--first-burn", type=int, required=True, help="first Bitcoin block height the network monitors")
    parser.add_argument("--cycle-length", type=int, required=True, help="reward cycle length in Bitcoin blocks")
    parser.add_argument("--prepare-length", type=int, required=True, help="prepare-phase length in Bitcoin blocks")
    parser.add_argument("--show", type=int, default=10, help="how many rows/groups to print in detail")
    args = parser.parse_args()

    db_path = Path(args.chainstate_dir) / "vm" / "index.sqlite"
    if not db_path.exists():
        sys.exit(f"chainstate index DB not found at {db_path}")

    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = conn.execute(
        """
        SELECT lower(hex(n.index_block_hash)) AS idx,
               h.block_height,
               h.burn_header_height
        FROM nakamoto_reward_sets n
        JOIN nakamoto_block_headers h ON n.index_block_hash = h.index_block_hash
        ORDER BY h.block_height
        """
    ).fetchall()

    if not rows:
        sys.exit("nakamoto_reward_sets is empty — nothing to verify")

    print(f"Loaded {len(rows)} reward-set rows from {db_path}")
    print(
        f"PoX: first_burn={args.first_burn}, "
        f"cycle_length={args.cycle_length}, "
        f"prepare_length={args.prepare_length}"
    )
    print()

    by_sql_cycle: dict[int, list[tuple[str, int, int, int, bool]]] = defaultdict(list)
    cycle_mismatches: list[tuple[str, int, int, int, int]] = []
    not_in_prepare: list[tuple[str, int, int, int, int]] = []

    for idx_block, block_h, burn_h in rows:
        sql_c = sql_formula_cycle(burn_h, args.first_burn, args.cycle_length)
        canon_c = canonical_cycle(burn_h, args.first_burn, args.cycle_length)
        in_prep = is_in_prepare_phase(burn_h, args.first_burn, args.cycle_length, args.prepare_length)
        by_sql_cycle[sql_c].append((idx_block, block_h, burn_h, canon_c, in_prep))

        # Expected invariant: reward set is calculated in the prepare phase of
        # cycle N-1, so it targets cycle N == canon_c + 1.
        expected_sql_c = canon_c + 1
        if sql_c != expected_sql_c:
            cycle_mismatches.append((idx_block, block_h, burn_h, sql_c, expected_sql_c))
        if not in_prep:
            offset = (burn_h - args.first_burn) % args.cycle_length
            not_in_prepare.append((idx_block, block_h, burn_h, canon_c, offset))

    print(f"== Rows grouped by SQL formula cycle ==")
    cycles_sorted = sorted(by_sql_cycle.keys())
    for c in cycles_sorted[: args.show]:
        rows_in = by_sql_cycle[c]
        burn_hs = sorted(set(r[2] for r in rows_in))
        canon_cs = sorted(set(r[3] for r in rows_in))
        in_preps = sorted(set(r[4] for r in rows_in))
        bh_preview = burn_hs[:3] + (["..."] if len(burn_hs) > 3 else [])
        print(
            f"  cycle {c}: {len(rows_in)} rows, "
            f"burn_h {bh_preview}, "
            f"canonical {canon_cs}, "
            f"in_prep={in_preps}"
        )
    if len(cycles_sorted) > args.show:
        print(f"  ... and {len(cycles_sorted) - args.show} more cycles")
    print()

    multi_row_cycles = {c: rs for c, rs in by_sql_cycle.items() if len(rs) > 1}
    print(f"== Cycles with multiple reward-set rows: {len(multi_row_cycles)} ==")
    for c, rs in list(multi_row_cycles.items())[: args.show]:
        bhs = [r[2] for r in rs]
        block_hs = [r[1] for r in rs]
        print(f"  cycle {c}: {len(rs)} rows; block_heights {block_hs}, burn_heights {bhs}")
    print()

    print(f"== Anomalies ==")
    print(f"  Rows whose SQL cycle != canonical_cycle + 1: {len(cycle_mismatches)}")
    for idx, bh_s, bh_b, sql_c, exp_c in cycle_mismatches[: args.show]:
        print(f"    block_height={bh_s}, burn_height={bh_b}: SQL={sql_c}, expected={exp_c}")
    print(f"  Rows NOT in prepare phase of any cycle:       {len(not_in_prepare)}")
    for idx, bh_s, bh_b, canon_c, offset in not_in_prepare[: args.show]:
        print(
            f"    block_height={bh_s}, burn_height={bh_b}: "
            f"canonical={canon_c}, offset_in_cycle={offset} "
            f"(prepare phase starts at offset {args.cycle_length - args.prepare_length})"
        )
    print()

    has_issues = bool(cycle_mismatches or not_in_prepare)
    if has_issues:
        print(
            f"❌ Found {len(cycle_mismatches)} cycle-mismatch and "
            f"{len(not_in_prepare)} not-in-prepare-phase rows. "
            f"The SQL formula may not match how the chain writes reward sets."
        )
        return 1
    print(
        "✓ Every reward-set row maps to canonical_cycle + 1 AND falls in the "
        "previous cycle's prepare phase. The SQL formula is consistent with the "
        "chain's reward-set placement."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
