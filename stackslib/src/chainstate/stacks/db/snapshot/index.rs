// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashSet;
use std::time::Instant;

use rusqlite::{params, Connection, OptionalExtension};
use stacks_common::types::chainstate::StacksBlockId;

use super::common::{
    clone_schemas_from_source, copied_rows, dst_subset_of_src, execute_copy_specs, spec_result,
    validate_copy_specs, with_offline_write_session, with_readonly_session, TableCopySpec,
};
use super::fork_storage::{
    collect_canonical_leaf_hashes, copy_canonical_fork_storage, validate_canonical_fork_storage,
};
use crate::burnchains::PoxConstants;
use crate::chainstate::stacks::index::{trie_sql, Error, MARFValue};
use crate::util_lib::db::table_exists;

/// Tables copied (with canonical-filtered content) into the squashed index
/// DB and validated row-for-row against the source.
pub(super) const COPIED_TABLES: &[&str] = &[
    "db_config",
    "block_headers",
    "nakamoto_block_headers",
    "payments",
    "transactions",
    "nakamoto_tenure_events",
    "nakamoto_reward_sets",
    "signer_stats",
    "matured_rewards",
    "burnchain_txids",
    "epoch_transitions",
    "staging_blocks",
];

/// Tables whose schema the index copy clones for fidelity but does not
/// populate itself. `staging_microblocks`/`staging_microblocks_data` are filled
/// in later by the block-preservation phase; the other two stay empty. Cloning
/// their schema prevents missing-table crashes if any code path references them.
pub(super) const SCHEMA_ONLY_TABLES: &[&str] = &[
    "staging_microblocks",
    "staging_microblocks_data",
    "invalidated_microblocks_data", // Only written when orphaning epoch 2.x blocks
    "user_supporters",              // Dead table: zero runtime references
];

/// The schema-only tables the index phase expects to stay empty (never written
/// by any squash phase). Validated as empty. `staging_microblocks*` are
/// deliberately excluded: the separate block-preservation phase populates them,
/// so asserting them empty here would spuriously fail a `--blocks`/`--all` run.
const EXPECTED_EMPTY_TABLES: &[&str] = &["invalidated_microblocks_data", "user_supporters"];

/// Every table whose schema must exist in the squashed dst (copied + schema-only).
fn all_required_tables() -> Vec<&'static str> {
    COPIED_TABLES
        .iter()
        .chain(SCHEMA_ONLY_TABLES)
        .copied()
        .collect()
}

/// Row-count statistics returned by [`copy_index_side_tables`].
#[derive(Debug, Clone)]
pub struct IndexSideTableStats {
    pub block_headers_rows: u64,
    pub nakamoto_block_headers_rows: u64,
    pub payments_rows: u64,
    pub transactions_rows: u64,
    pub nakamoto_tenure_events_rows: u64,
    pub nakamoto_reward_sets_rows: u64,
    pub signer_stats_rows: u64,
    pub matured_rewards_rows: u64,
    pub burnchain_txids_rows: u64,
    pub epoch_transitions_rows: u64,
    pub staging_blocks_rows: u64,
    pub fork_storage_rows: u64,
}

/// Validation result for index side tables in a squashed DB.
#[derive(Debug, Clone)]
pub struct IndexSideTableValidation {
    /// The schema-only staging-microblock tables exist (the only tables no
    /// validation query reads; see `validate_index_side_tables`).
    pub staging_microblock_tables_present: bool,
    pub db_config_matches: bool,
    pub fork_storage_match: bool,
    pub block_headers_match: bool,
    pub nakamoto_headers_match: bool,
    pub payments_match: bool,
    pub transactions_match: bool,
    pub nakamoto_tenure_events_match: bool,
    pub nakamoto_reward_sets_match: bool,
    pub signer_stats_match: bool,
    pub matured_rewards_match: bool,
    pub burnchain_txids_match: bool,
    pub epoch_transitions_match: bool,
    pub staging_blocks_match: bool,
    pub expected_tables_empty: bool,
}

impl IndexSideTableValidation {
    /// Every validation dimension as `(name, passed)` pairs. This is the
    /// single source of truth for [`is_valid`](Self::is_valid) and for
    /// diagnostics: a new dimension is wired into the overall verdict simply
    /// by listing it here, so the verdict and the printout can't drift apart.
    pub fn checks(&self) -> [(&'static str, bool); 15] {
        [
            (
                "staging_microblock_tables_present",
                self.staging_microblock_tables_present,
            ),
            ("db_config_matches", self.db_config_matches),
            ("fork_storage_match", self.fork_storage_match),
            ("block_headers_match", self.block_headers_match),
            ("nakamoto_headers_match", self.nakamoto_headers_match),
            ("payments_match", self.payments_match),
            ("transactions_match", self.transactions_match),
            (
                "nakamoto_tenure_events_match",
                self.nakamoto_tenure_events_match,
            ),
            (
                "nakamoto_reward_sets_match",
                self.nakamoto_reward_sets_match,
            ),
            ("signer_stats_match", self.signer_stats_match),
            ("matured_rewards_match", self.matured_rewards_match),
            ("burnchain_txids_match", self.burnchain_txids_match),
            ("epoch_transitions_match", self.epoch_transitions_match),
            ("staging_blocks_match", self.staging_blocks_match),
            ("expected_tables_empty", self.expected_tables_empty),
        ]
    }

    pub fn is_valid(&self) -> bool {
        self.checks().iter().all(|(_, ok)| *ok)
    }
}

/// Populate a temp table with the canonical block hashes from the squashed
/// MARF's metadata. Chainstate `index_block_hash` columns are lowercase
/// hex TEXT, so each id is inserted as its hex form to keep the joins
/// compatible. Returns the canonical tip (the highest squashed block).
fn populate_canonical_blocks(conn: &Connection) -> Result<StacksBlockId, Error> {
    let canonical = trie_sql::bulk_read_squashed_blocks::<StacksBlockId>(conn)?;
    let Some((_, tip, _)) = canonical.last() else {
        return Err(Error::CorruptionError(
            "marf_squashed_blocks is empty; post-squash dst must have at least one canonical block"
                .into(),
        ));
    };
    let tip = tip.clone();

    conn.execute_batch("CREATE TEMP TABLE canonical_blocks (index_block_hash TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;
    let mut insert = conn
        .prepare("INSERT INTO canonical_blocks (index_block_hash) VALUES (?1)")
        .map_err(Error::SQLError)?;
    for (_, block_hash, _) in &canonical {
        insert
            .execute(params![block_hash])
            .map_err(Error::SQLError)?;
    }
    drop(insert);

    // Source-completeness: every canonical block must exist in src as an
    // epoch-2 or Nakamoto header. A canonical ID not in src is corruption.
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM canonical_blocks \
             WHERE index_block_hash NOT IN (SELECT index_block_hash FROM src.block_headers) \
               AND index_block_hash NOT IN (SELECT index_block_hash FROM src.nakamoto_block_headers)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    if orphans > 0 {
        return Err(Error::CorruptionError(format!(
            "{orphans} canonical block(s) in marf_squashed_blocks are absent from \
             src.block_headers and src.nakamoto_block_headers"
        )));
    }
    Ok(tip)
}

/// Derive the `signer_stats` cutoff: the reward cycle of the canonical tip,
/// which must be a Nakamoto block.
///
/// Tip-cycle counters are copied as stored in src; `signer_stats` is a
/// non-consensus RPC counter (`/v3/signer`), so counts that include
/// post-boundary signatures are acceptable.
fn derive_max_reward_cycle(
    conn: &Connection,
    canonical_tip: &StacksBlockId,
    first_burn_height: u64,
    reward_cycle_len: u64,
) -> Result<u64, Error> {
    let tip_burn_height: u64 = conn
        .query_row(
            "SELECT burn_header_height FROM src.nakamoto_block_headers \
             WHERE index_block_hash = ?1",
            params![canonical_tip],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(Error::SQLError)?
        .map(|h| h as u64)
        .ok_or_else(|| {
            Error::CorruptionError(
                "canonical tip is not a Nakamoto block (no match in \
                 src.nakamoto_block_headers); squashing requires an epoch 3.4+ chainstate"
                    .into(),
            )
        })?;

    let cycle = PoxConstants::static_block_height_to_reward_cycle(
        tip_burn_height,
        first_burn_height,
        reward_cycle_len,
    )
    .ok_or_else(|| {
        Error::CorruptionError(format!(
            "cannot derive reward cycle: tip_burn_height={tip_burn_height}, \
             first_burn_height={first_burn_height}, reward_cycle_len={reward_cycle_len}"
        ))
    })?;
    info!("[index] derive_max_reward_cycle: {cycle} (tip_burn_height={tip_burn_height})");
    Ok(cycle)
}

/// Build the copy specs for every SQL-expressible index-table copy.
/// Most tables filter uniformly by `index_block_hash IN canonical_blocks`.
///
/// Special cases:
/// - `db_config` is copied in full.
/// - `staging_blocks` keeps only processed, non-orphaned blocks.
/// - `signer_stats` is cut off at the canonical tip's reward cycle.
pub(super) fn index_copy_specs(max_reward_cycle: u64) -> Vec<TableCopySpec> {
    let cb = "SELECT index_block_hash FROM canonical_blocks";
    vec![
        TableCopySpec {
            table: "db_config",
            source_sql: "SELECT * FROM src.db_config".into(),
        },
        TableCopySpec {
            table: "block_headers",
            source_sql: format!("SELECT * FROM src.block_headers WHERE index_block_hash IN ({cb})"),
        },
        TableCopySpec {
            table: "nakamoto_block_headers",
            source_sql: format!(
                "SELECT * FROM src.nakamoto_block_headers WHERE index_block_hash IN ({cb})"
            ),
        },
        TableCopySpec {
            table: "payments",
            source_sql: format!("SELECT * FROM src.payments WHERE index_block_hash IN ({cb})"),
        },
        TableCopySpec {
            table: "transactions",
            source_sql: format!("SELECT * FROM src.transactions WHERE index_block_hash IN ({cb})"),
        },
        TableCopySpec {
            table: "nakamoto_tenure_events",
            source_sql: format!(
                "SELECT * FROM src.nakamoto_tenure_events WHERE block_id IN ({cb})"
            ),
        },
        TableCopySpec {
            table: "nakamoto_reward_sets",
            source_sql: format!(
                "SELECT * FROM src.nakamoto_reward_sets WHERE index_block_hash IN ({cb})"
            ),
        },
        TableCopySpec {
            table: "matured_rewards",
            source_sql: format!(
                "SELECT * FROM src.matured_rewards WHERE child_index_block_hash IN ({cb})"
            ),
        },
        TableCopySpec {
            table: "burnchain_txids",
            source_sql: format!(
                "SELECT * FROM src.burnchain_txids WHERE index_block_hash IN ({cb})"
            ),
        },
        TableCopySpec {
            table: "epoch_transitions",
            source_sql: format!("SELECT * FROM src.epoch_transitions WHERE block_id IN ({cb})"),
        },
        TableCopySpec {
            table: "staging_blocks",
            // Only canonical, fully-processed, non-orphaned blocks.
            source_sql: format!(
                "SELECT s.* FROM src.staging_blocks s \
                 WHERE s.index_block_hash IN ({cb}) \
                   AND s.processed = 1 \
                   AND s.orphaned = 0"
            ),
        },
        TableCopySpec {
            table: "signer_stats",
            source_sql: format!(
                "SELECT * FROM src.signer_stats WHERE reward_cycle <= {max_reward_cycle}"
            ),
        },
    ]
}

/// Copy required non-MARF tables from the source `index.sqlite` into the
/// squashed destination. Only canonical rows (determined by the squashed MARF's
/// `marf_squashed_blocks`) are included, excluding non-canonical fork data.
///
/// Per the squash preconditions, src must be an epoch 3.4+ chainstate:
/// the canonical set must contain a Nakamoto tip.
pub fn copy_index_side_tables(
    src_path: &str,
    dst_path: &str,
    first_burn_height: u64,
    reward_cycle_len: u64,
) -> Result<IndexSideTableStats, Error> {
    let leaf_hashes = collect_canonical_leaf_hashes::<StacksBlockId>(dst_path)?;

    with_offline_write_session(dst_path, &[("src", src_path)], "", |conn| {
        clone_schemas_from_source(conn, &all_required_tables())?;
        copy_tables_inner(conn, &leaf_hashes, first_burn_height, reward_cycle_len)
    })
}

fn copy_tables_inner(
    conn: &Connection,
    leaf_hashes: &HashSet<MARFValue>,
    first_burn_height: u64,
    reward_cycle_len: u64,
) -> Result<IndexSideTableStats, Error> {
    let total_start = Instant::now();

    // Copy only canonical __fork_storage rows. The squashed MARF trie
    // leaves reference these by value_hash. Non-canonical fork entries
    // are excluded.
    let fork_storage_rows = copy_canonical_fork_storage(conn, leaf_hashes)?;

    // Build canonical block set from squash metadata.
    let t = Instant::now();
    let canonical_tip = populate_canonical_blocks(conn)?;
    info!(
        "[index] canonical_blocks temp table built in {:?}",
        t.elapsed()
    );

    let max_reward_cycle =
        derive_max_reward_cycle(conn, &canonical_tip, first_burn_height, reward_cycle_len)?;

    let specs: Vec<TableCopySpec> = index_copy_specs(max_reward_cycle);
    let results = execute_copy_specs(conn, &specs)?;

    conn.execute_batch("DROP TABLE IF EXISTS canonical_blocks")
        .map_err(Error::SQLError)?;

    info!("[index] all tables done in {:?}", total_start.elapsed());

    Ok(IndexSideTableStats {
        block_headers_rows: copied_rows(&results, "block_headers"),
        nakamoto_block_headers_rows: copied_rows(&results, "nakamoto_block_headers"),
        payments_rows: copied_rows(&results, "payments"),
        transactions_rows: copied_rows(&results, "transactions"),
        nakamoto_tenure_events_rows: copied_rows(&results, "nakamoto_tenure_events"),
        nakamoto_reward_sets_rows: copied_rows(&results, "nakamoto_reward_sets"),
        signer_stats_rows: copied_rows(&results, "signer_stats"),
        matured_rewards_rows: copied_rows(&results, "matured_rewards"),
        burnchain_txids_rows: copied_rows(&results, "burnchain_txids"),
        epoch_transitions_rows: copied_rows(&results, "epoch_transitions"),
        staging_blocks_rows: copied_rows(&results, "staging_blocks"),
        fork_storage_rows,
    })
}

/// Validate that the squashed index DB has the correct side tables by
/// comparing against the source.
pub fn validate_index_side_tables(
    src_path: &str,
    dst_path: &str,
    first_burn_height: u64,
    reward_cycle_len: u64,
) -> Result<IndexSideTableValidation, Error> {
    with_readonly_session(dst_path, &[("src", src_path)], |conn| {
        // Single-`i64` query (COUNT etc.), error-propagating rather than
        // swallowing into a sentinel: a SQL failure here is itself corruption.
        let count = |sql: &str| -> Result<i64, Error> {
            conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map_err(Error::SQLError)
        };

        // Existence is only checked for the tables no validation query reads:
        // every other required table is read below, so a missing one already
        // fails its own check. The staging-microblock tables are populated by
        // the separate block-preservation phase, and an index-only validation
        // never touches them.
        let mut staging_microblock_tables_present = true;
        for table in SCHEMA_ONLY_TABLES
            .iter()
            .filter(|table| !EXPECTED_EMPTY_TABLES.contains(table))
        {
            staging_microblock_tables_present &= table_exists(conn, table)?;
        }

        let fork_storage_match = validate_canonical_fork_storage::<StacksBlockId>(conn, dst_path)?;

        // Build the canonical block set using the SAME guarded path as the
        // copy (rejects empty `marf_squashed_blocks` and canonical ids absent
        // from src), so validation is never more lenient than the copy that
        // produced the dst.
        let canonical_tip = populate_canonical_blocks(conn)?;
        let max_reward_cycle =
            derive_max_reward_cycle(conn, &canonical_tip, first_burn_height, reward_cycle_len)?;

        // Bidirectional full-row EXCEPT against each copy spec (not
        // count-only), so a row with a canonical key but corrupted contents
        // is still caught. signer_stats and matured_rewards are skipped: the
        // source legitimately drifts for them after the snapshot.
        let results = validate_copy_specs(
            conn,
            &index_copy_specs(max_reward_cycle),
            &["signer_stats", "matured_rewards"],
        )?;

        let cb = "SELECT index_block_hash FROM canonical_blocks";

        // signer_stats is a non-consensus counter table whose only writer uses
        // INSERT ... ON CONFLICT DO UPDATE SET blocks_signed = blocks_signed + 1.
        // After the snapshot the source keeps incrementing, so we check:
        //   1. every (public_key, reward_cycle) key in dst exists in filtered src
        //   2. dst.blocks_signed <= src.blocks_signed
        let signer_stats_match = {
            // No fabricated keys.
            let keys_ok = dst_subset_of_src(
                conn,
                "SELECT public_key, reward_cycle FROM signer_stats",
                &format!(
                    "SELECT public_key, reward_cycle FROM src.signer_stats \
                     WHERE reward_cycle <= {max_reward_cycle}"
                ),
            )?;
            // No inflated counters.
            let inflated = count(
                "SELECT COUNT(*) FROM signer_stats d \
                 JOIN src.signer_stats s \
                   ON d.public_key = s.public_key AND d.reward_cycle = s.reward_cycle \
                 WHERE d.blocks_signed > s.blocks_signed",
            )?;
            keys_ok && inflated == 0
        };

        // matured_rewards is a non-consensus cache populated as new blocks
        // trigger maturation of older canonical blocks' rewards. The source
        // legitimately gains rows after the snapshot, so we only verify no
        // fabricated rows exist in the destination.
        let matured_rewards_match = dst_subset_of_src(
            conn,
            "SELECT * FROM matured_rewards",
            &format!("SELECT * FROM src.matured_rewards WHERE child_index_block_hash IN ({cb})"),
        )?;

        // Tables that no squash phase should ever write must be empty.
        // (staging_microblocks* are intentionally excluded: the block-preservation
        // phase populates them, so they are not asserted empty here.)
        let mut expected_tables_empty = true;
        for &table in EXPECTED_EMPTY_TABLES {
            let rows = count(&format!("SELECT COUNT(*) FROM {table}"))?;
            if rows != 0 {
                warn!(
                    "[index] table expected to be empty is non-empty in squashed dst";
                    "table" => table, "rows" => rows
                );
                expected_tables_empty = false;
            }
        }

        Ok(IndexSideTableValidation {
            staging_microblock_tables_present,
            db_config_matches: spec_result(&results, "db_config"),
            fork_storage_match,
            block_headers_match: spec_result(&results, "block_headers"),
            nakamoto_headers_match: spec_result(&results, "nakamoto_block_headers"),
            payments_match: spec_result(&results, "payments"),
            transactions_match: spec_result(&results, "transactions"),
            nakamoto_tenure_events_match: spec_result(&results, "nakamoto_tenure_events"),
            nakamoto_reward_sets_match: spec_result(&results, "nakamoto_reward_sets"),
            signer_stats_match,
            matured_rewards_match,
            burnchain_txids_match: spec_result(&results, "burnchain_txids"),
            epoch_transitions_match: spec_result(&results, "epoch_transitions"),
            staging_blocks_match: spec_result(&results, "staging_blocks"),
            expected_tables_empty,
        })
    })
}
