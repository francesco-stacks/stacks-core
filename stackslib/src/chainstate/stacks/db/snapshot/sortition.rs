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

use rusqlite::{params, Connection};
use stacks_common::types::chainstate::{BlockHeaderHash, ConsensusHash, SortitionId};

use super::common::{
    clone_schemas_from_source, execute_copy_specs, full_row_except_match,
    with_offline_write_session, TableCopySpec,
};
use super::fork_storage::{collect_canonical_leaf_hashes, copy_canonical_fork_storage};
use crate::chainstate::stacks::index::Error;

/// Required sortition tables always present in production.
pub(crate) const REQUIRED_TABLES: &[&str] = &[
    "db_config",
    "snapshots",
    "leader_keys",
    "block_commits",
    "block_commit_parents",
    "snapshot_transition_ops",
    "stacks_chain_tips",
    "stacks_chain_tips_by_burn_view",
    "missed_commits",
    "stack_stx",
    "transfer_stx",
    "delegate_stx",
    "vote_for_aggregate_key",
    "preprocessed_reward_sets",
    "epochs",
];

/// Row-count statistics returned by [`copy_sortition_side_tables`].
#[derive(Debug, Clone)]
pub struct SortitionSideTableStats {
    pub snapshots_rows: u64,
    pub leader_keys_rows: u64,
    pub block_commits_rows: u64,
    pub block_commit_parents_rows: u64,
    pub snapshot_transition_ops_rows: u64,
    pub stacks_chain_tips_rows: u64,
    pub stacks_chain_tips_by_burn_view_rows: u64,
    pub preprocessed_reward_sets_rows: u64,
    pub missed_commits_rows: u64,
    pub stack_stx_rows: u64,
    pub transfer_stx_rows: u64,
    pub delegate_stx_rows: u64,
    pub vote_for_aggregate_key_rows: u64,
    pub epochs_rows: u64,
    pub db_config_rows: u64,
    pub fork_storage_rows: u64,
}

/// Stacks-side boundary used when copying sortition tip memo tables.
///
/// A squash can anchor the sortition MARF at a burn view whose runtime source
/// tip is later than the copied Stacks MARF. In that case, the memoized
/// sortition tip must be rewritten to the copied Stacks anchor so a booting
/// node replays the intra-tenure descendants instead of believing they are
/// already processed.
#[derive(Debug, Clone)]
pub struct SortitionTipCopyBoundary {
    pub max_stacks_height: u64,
    pub anchor_consensus_hash: ConsensusHash,
    pub anchor_burn_view_consensus_hash: ConsensusHash,
    pub anchor_block_hash: BlockHeaderHash,
    pub anchor_block_height: u64,
}

/// Validation result for sortition side tables in a squashed DB.
///
/// See [`validate_sortition_side_tables`] for the trust boundary - this checks
/// consistency with the destination-declared canonical set, not independent
/// canonicality. MARF trie validation must be done separately.
#[derive(Debug, Clone)]
pub struct SortitionSideTableValidation {
    pub required_tables_present: bool,
    /// Every sortition_id in destination `marf_squashed_blocks` exists in
    /// the source `snapshots` table. False if the destination claims sortition IDs
    /// that the source doesn't have.
    pub canonical_set_in_source: bool,
    pub snapshots_match: bool,
    pub leader_keys_match: bool,
    pub block_commits_match: bool,
    pub block_commit_parents_match: bool,
    pub snapshot_transition_ops_match: bool,
    pub stacks_chain_tips_match: bool,
    pub stacks_chain_tips_by_burn_view_match: bool,
    /// No copied sortition tip memo row points past the copied Stacks MARF
    /// boundary.
    pub stacks_chain_tips_within_stacks_boundary: bool,
    /// The copied burn-view tip row resolves exactly to the selected Stacks
    /// anchor when a Stacks boundary rewrite is requested.
    pub stacks_chain_tips_anchor_match: bool,
    pub preprocessed_reward_sets_match: bool,
    pub missed_commits_match: bool,
    pub stack_stx_match: bool,
    pub transfer_stx_match: bool,
    pub delegate_stx_match: bool,
    pub vote_for_aggregate_key_match: bool,
    pub epochs_match: bool,
    pub db_config_match: bool,
    pub fork_storage_match: bool,
}

impl SortitionSideTableValidation {
    pub fn is_valid(&self) -> bool {
        self.required_tables_present
            && self.canonical_set_in_source
            && self.snapshots_match
            && self.leader_keys_match
            && self.block_commits_match
            && self.block_commit_parents_match
            && self.snapshot_transition_ops_match
            && self.stacks_chain_tips_match
            && self.stacks_chain_tips_by_burn_view_match
            && self.stacks_chain_tips_within_stacks_boundary
            && self.stacks_chain_tips_anchor_match
            && self.preprocessed_reward_sets_match
            && self.missed_commits_match
            && self.stack_stx_match
            && self.transfer_stx_match
            && self.delegate_stx_match
            && self.vote_for_aggregate_key_match
            && self.epochs_match
            && self.db_config_match
            && self.fork_storage_match
    }
}

/// Build temp tables for the canonical sortition set and canonical burn hashes.
fn populate_canonical_sortitions(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch("CREATE TEMP TABLE canonical_sortitions (sortition_id TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO canonical_sortitions (sortition_id) \
             SELECT lower(hex(block_hash)) FROM marf_squashed_blocks",
            [],
        )
        .map_err(Error::SQLError)?;
    if inserted == 0 {
        return Err(Error::CorruptionError(
            "marf_squashed_blocks is empty; post-squash dst must have at least one canonical sortition"
                .into(),
        ));
    }
    // Source-completeness: every canonical sortition must exist in
    // src.snapshots. A canonical sortition_id missing from src is
    // corruption. The squash claimed a sortition that src doesn't have.
    let orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM canonical_sortitions \
             WHERE sortition_id NOT IN (SELECT sortition_id FROM src.snapshots)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    if orphans > 0 {
        return Err(Error::CorruptionError(format!(
            "{orphans} canonical sortition(s) in marf_squashed_blocks are absent from src.snapshots"
        )));
    }

    conn.execute_batch(
        "CREATE TEMP TABLE canonical_burn_hashes (burn_header_hash TEXT PRIMARY KEY)",
    )
    .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT OR IGNORE INTO canonical_burn_hashes (burn_header_hash) \
         SELECT DISTINCT s.burn_header_hash FROM src.snapshots s \
         INNER JOIN canonical_sortitions cs ON s.sortition_id = cs.sortition_id",
        [],
    )
    .map_err(Error::SQLError)?;

    Ok(())
}

fn validate_tip_boundary(boundary: Option<&SortitionTipCopyBoundary>) -> Result<(), Error> {
    if let Some(boundary) = boundary {
        if boundary.anchor_block_height > boundary.max_stacks_height {
            return Err(Error::CorruptionError(format!(
                "sortition tip rewrite anchor height {} exceeds Stacks boundary {}",
                boundary.anchor_block_height, boundary.max_stacks_height
            )));
        }
        let _ = i64::try_from(boundary.max_stacks_height).map_err(|_| {
            Error::CorruptionError(format!(
                "Stacks boundary height {} exceeds SQLite INTEGER range",
                boundary.max_stacks_height
            ))
        })?;
        let _ = i64::try_from(boundary.anchor_block_height).map_err(|_| {
            Error::CorruptionError(format!(
                "sortition tip rewrite anchor height {} exceeds SQLite INTEGER range",
                boundary.anchor_block_height
            ))
        })?;
    }
    Ok(())
}

/// Build the copy/validate source SQL for a sortition Stacks-tip memo table.
///
/// Handles both `stacks_chain_tips` and (with `include_burn_view`)
/// `stacks_chain_tips_by_burn_view`; the latter carries an extra
/// `burn_view_consensus_hash` column and a correspondingly stricter anchor
/// match. With a `boundary`, rows whose `block_height` exceeds it are rewritten
/// down to the anchor (see [`SortitionTipCopyBoundary`]).
fn stacks_chain_tip_memo_source_sql(
    table: &str,
    sid: &str,
    include_burn_view: bool,
    boundary: Option<&SortitionTipCopyBoundary>,
) -> String {
    let Some(boundary) = boundary else {
        return format!("SELECT * FROM src.{table} WHERE sortition_id IN ({sid})");
    };
    let max_height = boundary.max_stacks_height;
    let anchor_height = boundary.anchor_block_height;
    let anchor_ch = &boundary.anchor_consensus_hash;
    let anchor_bhh = &boundary.anchor_block_hash;
    let anchor_burn_view_ch = &boundary.anchor_burn_view_consensus_hash;
    let burn_view_column = if include_burn_view {
        format!(
            "CASE WHEN block_height > {max_height} THEN '{anchor_burn_view_ch}' \
                  ELSE burn_view_consensus_hash END, "
        )
    } else {
        String::new()
    };
    let anchor_match = if include_burn_view {
        format!(
            "(consensus_hash = '{anchor_ch}' \
              AND burn_view_consensus_hash = '{anchor_burn_view_ch}')"
        )
    } else {
        format!("consensus_hash = '{anchor_ch}'")
    };
    format!(
        "SELECT sortition_id, \
                CASE WHEN block_height > {max_height} THEN '{anchor_ch}' ELSE consensus_hash END, \
                {burn_view_column}\
                CASE WHEN block_height > {max_height} THEN '{anchor_bhh}' ELSE block_hash END, \
                CASE WHEN block_height > {max_height} THEN {anchor_height} ELSE block_height END \
         FROM src.{table} \
         WHERE sortition_id IN ({sid}) \
           AND (block_height <= {max_height} OR {anchor_match})"
    )
}

fn sortition_tip_heights_within_boundary(
    conn: &Connection,
    boundary: Option<&SortitionTipCopyBoundary>,
) -> Result<bool, Error> {
    let Some(boundary) = boundary else {
        return Ok(true);
    };
    let max_height = i64::try_from(boundary.max_stacks_height).map_err(|_| {
        Error::CorruptionError(format!(
            "Stacks boundary height {} exceeds SQLite INTEGER range",
            boundary.max_stacks_height
        ))
    })?;
    conn.query_row(
        "SELECT COUNT(*) = 0 FROM ( \
             SELECT block_height FROM stacks_chain_tips WHERE block_height > ?1 \
             UNION ALL \
             SELECT block_height FROM stacks_chain_tips_by_burn_view WHERE block_height > ?1 \
         )",
        params![max_height],
        |row| row.get(0),
    )
    .map_err(Error::SQLError)
}

fn sortition_tip_anchor_matches_boundary(
    conn: &Connection,
    boundary: Option<&SortitionTipCopyBoundary>,
) -> Result<bool, Error> {
    let Some(boundary) = boundary else {
        return Ok(true);
    };
    let anchor_height = i64::try_from(boundary.anchor_block_height).map_err(|_| {
        Error::CorruptionError(format!(
            "sortition tip rewrite anchor height {} exceeds SQLite INTEGER range",
            boundary.anchor_block_height
        ))
    })?;
    let anchor_ch = boundary.anchor_consensus_hash.to_string();
    let anchor_burn_view_ch = boundary.anchor_burn_view_consensus_hash.to_string();
    let anchor_bhh = boundary.anchor_block_hash.to_string();

    let burn_view_anchor_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM stacks_chain_tips_by_burn_view \
             WHERE consensus_hash = ?1 \
               AND burn_view_consensus_hash = ?2 \
               AND block_hash = ?3 \
               AND block_height = ?4",
            params![&anchor_ch, &anchor_burn_view_ch, &anchor_bhh, anchor_height],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    let burn_view_mismatches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM stacks_chain_tips_by_burn_view \
             WHERE burn_view_consensus_hash = ?1 \
               AND (consensus_hash != ?2 OR block_hash != ?3 OR block_height != ?4)",
            params![&anchor_burn_view_ch, &anchor_ch, &anchor_bhh, anchor_height],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;
    let legacy_mismatches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM stacks_chain_tips \
             WHERE consensus_hash = ?1 \
               AND (block_hash != ?2 OR block_height != ?3)",
            params![&anchor_ch, &anchor_bhh, anchor_height],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;

    Ok(burn_view_anchor_rows > 0 && burn_view_mismatches == 0 && legacy_mismatches == 0)
}

/// Build the copy specs for sortition side tables.
///
/// Tables are grouped by their filter key:
/// - `sortition_id` filtered
/// - `burn_header_hash` filtered
/// - full-copy
fn sortition_copy_specs(boundary: Option<&SortitionTipCopyBoundary>) -> Vec<TableCopySpec> {
    let sid = "SELECT sortition_id FROM canonical_sortitions";
    let bhh = "SELECT burn_header_hash FROM canonical_burn_hashes";

    vec![
        // sortition_id-filtered tables
        TableCopySpec {
            table: "snapshots",
            source_sql: format!("SELECT * FROM src.snapshots WHERE sortition_id IN ({sid})"),
        },
        TableCopySpec {
            table: "leader_keys",
            source_sql: format!("SELECT * FROM src.leader_keys WHERE sortition_id IN ({sid})"),
        },
        TableCopySpec {
            table: "block_commits",
            source_sql: format!("SELECT * FROM src.block_commits WHERE sortition_id IN ({sid})"),
        },
        TableCopySpec {
            table: "block_commit_parents",
            source_sql: format!(
                "SELECT * FROM src.block_commit_parents WHERE block_commit_sortition_id IN ({sid})"
            ),
        },
        TableCopySpec {
            table: "snapshot_transition_ops",
            source_sql: format!(
                "SELECT * FROM src.snapshot_transition_ops WHERE sortition_id IN ({sid})"
            ),
        },
        TableCopySpec {
            table: "stacks_chain_tips",
            source_sql: stacks_chain_tip_memo_source_sql("stacks_chain_tips", sid, false, boundary),
        },
        TableCopySpec {
            table: "stacks_chain_tips_by_burn_view",
            source_sql: stacks_chain_tip_memo_source_sql(
                "stacks_chain_tips_by_burn_view",
                sid,
                true,
                boundary,
            ),
        },
        TableCopySpec {
            table: "preprocessed_reward_sets",
            source_sql: format!(
                "SELECT * FROM src.preprocessed_reward_sets WHERE sortition_id IN ({sid})"
            ),
        },
        TableCopySpec {
            table: "missed_commits",
            source_sql: format!(
                "SELECT * FROM src.missed_commits WHERE intended_sortition_id IN ({sid})"
            ),
        },
        // burn_header_hash-filtered tables
        TableCopySpec {
            table: "stack_stx",
            source_sql: format!("SELECT * FROM src.stack_stx WHERE burn_header_hash IN ({bhh})"),
        },
        TableCopySpec {
            table: "transfer_stx",
            source_sql: format!("SELECT * FROM src.transfer_stx WHERE burn_header_hash IN ({bhh})"),
        },
        TableCopySpec {
            table: "delegate_stx",
            source_sql: format!("SELECT * FROM src.delegate_stx WHERE burn_header_hash IN ({bhh})"),
        },
        TableCopySpec {
            table: "vote_for_aggregate_key",
            source_sql: format!(
                "SELECT * FROM src.vote_for_aggregate_key WHERE burn_header_hash IN ({bhh})"
            ),
        },
        // Full-copy tables
        TableCopySpec {
            table: "epochs",
            source_sql: "SELECT * FROM src.epochs".to_string(),
        },
    ]
}

/// Copy required non-MARF tables from the source sortition DB into the
/// squashed destination. Only canonical rows (determined by the squashed MARF's
/// `marf_squashed_blocks`) are included.
pub fn copy_sortition_side_tables(
    src_path: &str,
    dst_path: &str,
) -> Result<SortitionSideTableStats, Error> {
    copy_sortition_side_tables_with_boundary(src_path, dst_path, None)
}

pub fn copy_sortition_side_tables_with_boundary(
    src_path: &str,
    dst_path: &str,
    stacks_boundary: Option<&SortitionTipCopyBoundary>,
) -> Result<SortitionSideTableStats, Error> {
    validate_tip_boundary(stacks_boundary)?;
    // Walk the squashed trie before opening dst R/W.
    let leaf_hashes = collect_canonical_leaf_hashes::<SortitionId>(dst_path)?;

    with_offline_write_session(dst_path, &[("src", src_path)], "", |conn| {
        clone_schemas_from_source(conn, REQUIRED_TABLES)?;
        copy_sortition_tables_inner(conn, &leaf_hashes, stacks_boundary)
    })
}

fn copy_sortition_tables_inner(
    conn: &Connection,
    leaf_hashes: &HashSet<String>,
    stacks_boundary: Option<&SortitionTipCopyBoundary>,
) -> Result<SortitionSideTableStats, Error> {
    // Copy db_config verbatim.
    let db_config_rows = conn
        .execute(
            "INSERT OR REPLACE INTO db_config SELECT * FROM src.db_config",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // Copy only canonical __fork_storage rows. The squashed MARF trie
    // leaves reference these by value_hash. Non-canonical fork entries
    // are excluded.
    let fork_storage_rows = copy_canonical_fork_storage(conn, leaf_hashes)?;

    // Build canonical sortition set from squash metadata.
    populate_canonical_sortitions(conn)?;

    // Execute descriptor-driven copies.
    let specs = sortition_copy_specs(stacks_boundary);
    let results = execute_copy_specs(conn, &specs)?;
    if !sortition_tip_heights_within_boundary(conn, stacks_boundary)? {
        return Err(Error::CorruptionError(
            "copied sortition tip row points past the Stacks MARF boundary".into(),
        ));
    }

    conn.execute_batch("DROP TABLE IF EXISTS canonical_sortitions")
        .map_err(Error::SQLError)?;
    conn.execute_batch("DROP TABLE IF EXISTS canonical_burn_hashes")
        .map_err(Error::SQLError)?;

    // Map results to stats struct by table name.
    let get = |name: &str| -> u64 {
        results
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, r)| *r)
            .unwrap_or(0)
    };

    Ok(SortitionSideTableStats {
        snapshots_rows: get("snapshots"),
        leader_keys_rows: get("leader_keys"),
        block_commits_rows: get("block_commits"),
        block_commit_parents_rows: get("block_commit_parents"),
        snapshot_transition_ops_rows: get("snapshot_transition_ops"),
        stacks_chain_tips_rows: get("stacks_chain_tips"),
        stacks_chain_tips_by_burn_view_rows: get("stacks_chain_tips_by_burn_view"),
        preprocessed_reward_sets_rows: get("preprocessed_reward_sets"),
        missed_commits_rows: get("missed_commits"),
        stack_stx_rows: get("stack_stx"),
        transfer_stx_rows: get("transfer_stx"),
        delegate_stx_rows: get("delegate_stx"),
        vote_for_aggregate_key_rows: get("vote_for_aggregate_key"),
        epochs_rows: get("epochs"),
        db_config_rows,
        fork_storage_rows,
    })
}

/// Validate that the squashed sortition DB has the correct side tables by
/// comparing against the source using full-row EXCEPT queries.
///
/// # Trust boundary
///
/// This validator checks that side-table rows are consistent with the canonical
/// set declared by the destination's `marf_squashed_blocks` metadata, which
/// was populated during the MARF squash by walking the canonical tip. It does NOT
/// independently re-derive the canonical chain from the source MARF - that is the
/// job of `validate_squashed_at_height` on the MARF trie itself. The
/// `canonical_set_in_source` check catches fabricated sortition IDs (IDs not
/// present anywhere in the source), but cannot detect a coherent wrong-fork
/// canonical set where all IDs exist in the source but are from a non-canonical
/// fork. Full canonicality assurance requires validating the squashed MARF trie
/// first, then using this function to verify side-table consistency.
pub fn validate_sortition_side_tables(
    src_path: &str,
    dst_path: &str,
) -> Result<SortitionSideTableValidation, Error> {
    validate_sortition_side_tables_with_boundary(src_path, dst_path, None)
}

pub fn validate_sortition_side_tables_with_boundary(
    src_path: &str,
    dst_path: &str,
    stacks_boundary: Option<&SortitionTipCopyBoundary>,
) -> Result<SortitionSideTableValidation, Error> {
    validate_tip_boundary(stacks_boundary)?;
    let conn = Connection::open(dst_path).map_err(Error::SQLError)?;
    conn.execute("ATTACH DATABASE ?1 AS src", params![src_path])
        .map_err(Error::SQLError)?;

    // Check all required tables exist in destination.
    let required_tables_present = REQUIRED_TABLES.iter().all(|table| {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    });

    // Build canonical set from squash metadata.
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS canonical_sortitions (sortition_id TEXT PRIMARY KEY)",
    )
    .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT OR IGNORE INTO canonical_sortitions (sortition_id) \
         SELECT lower(hex(block_hash)) FROM marf_squashed_blocks",
        [],
    )
    .map_err(Error::SQLError)?;

    // Cross-check: every sortition_id the destination claims as canonical must
    // actually exist in the source snapshots table.
    let canonical_set_in_source: bool = conn
        .query_row(
            "SELECT COUNT(*) = 0 FROM canonical_sortitions cs \
             WHERE cs.sortition_id NOT IN (SELECT sortition_id FROM src.snapshots)",
            [],
            |row| row.get(0),
        )
        .map_err(Error::SQLError)?;

    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS canonical_burn_hashes (burn_header_hash TEXT PRIMARY KEY)",
    )
    .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT OR IGNORE INTO canonical_burn_hashes (burn_header_hash) \
         SELECT DISTINCT s.burn_header_hash FROM src.snapshots s \
         INNER JOIN canonical_sortitions cs ON s.sortition_id = cs.sortition_id",
        [],
    )
    .map_err(Error::SQLError)?;

    let sid = "SELECT sortition_id FROM canonical_sortitions";
    let bhh = "SELECT burn_header_hash FROM canonical_burn_hashes";

    // sortition_id-filtered tables
    let snapshots_match = full_row_except_match(
        &conn,
        "SELECT * FROM snapshots",
        &format!("SELECT * FROM src.snapshots WHERE sortition_id IN ({sid})"),
    )?;
    let leader_keys_match = full_row_except_match(
        &conn,
        "SELECT * FROM leader_keys",
        &format!("SELECT * FROM src.leader_keys WHERE sortition_id IN ({sid})"),
    )?;
    let block_commits_match = full_row_except_match(
        &conn,
        "SELECT * FROM block_commits",
        &format!("SELECT * FROM src.block_commits WHERE sortition_id IN ({sid})"),
    )?;
    let block_commit_parents_match = full_row_except_match(
        &conn,
        "SELECT * FROM block_commit_parents",
        &format!(
            "SELECT * FROM src.block_commit_parents WHERE block_commit_sortition_id IN ({sid})"
        ),
    )?;
    let snapshot_transition_ops_match = full_row_except_match(
        &conn,
        "SELECT * FROM snapshot_transition_ops",
        &format!("SELECT * FROM src.snapshot_transition_ops WHERE sortition_id IN ({sid})"),
    )?;
    let stacks_chain_tips_match = full_row_except_match(
        &conn,
        "SELECT * FROM stacks_chain_tips",
        &stacks_chain_tip_memo_source_sql("stacks_chain_tips", sid, false, stacks_boundary),
    )?;
    let stacks_chain_tips_by_burn_view_match = full_row_except_match(
        &conn,
        "SELECT * FROM stacks_chain_tips_by_burn_view",
        &stacks_chain_tip_memo_source_sql(
            "stacks_chain_tips_by_burn_view",
            sid,
            true,
            stacks_boundary,
        ),
    )?;
    let stacks_chain_tips_within_stacks_boundary =
        sortition_tip_heights_within_boundary(&conn, stacks_boundary)?;
    let stacks_chain_tips_anchor_match =
        sortition_tip_anchor_matches_boundary(&conn, stacks_boundary)?;
    let preprocessed_reward_sets_match = full_row_except_match(
        &conn,
        "SELECT * FROM preprocessed_reward_sets",
        &format!("SELECT * FROM src.preprocessed_reward_sets WHERE sortition_id IN ({sid})"),
    )?;
    let missed_commits_match = full_row_except_match(
        &conn,
        "SELECT * FROM missed_commits",
        &format!("SELECT * FROM src.missed_commits WHERE intended_sortition_id IN ({sid})"),
    )?;

    // burn_header_hash-filtered tables
    let stack_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM stack_stx",
        &format!("SELECT * FROM src.stack_stx WHERE burn_header_hash IN ({bhh})"),
    )?;
    let transfer_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM transfer_stx",
        &format!("SELECT * FROM src.transfer_stx WHERE burn_header_hash IN ({bhh})"),
    )?;
    let delegate_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM delegate_stx",
        &format!("SELECT * FROM src.delegate_stx WHERE burn_header_hash IN ({bhh})"),
    )?;
    let vote_for_aggregate_key_match = full_row_except_match(
        &conn,
        "SELECT * FROM vote_for_aggregate_key",
        &format!("SELECT * FROM src.vote_for_aggregate_key WHERE burn_header_hash IN ({bhh})"),
    )?;

    // Full-copy tables
    let epochs_match =
        full_row_except_match(&conn, "SELECT * FROM epochs", "SELECT * FROM src.epochs")?;
    let db_config_match = full_row_except_match(
        &conn,
        "SELECT * FROM db_config",
        "SELECT * FROM src.db_config",
    )?;

    // __fork_storage: canonical-only copy.
    // Validate against the canonical filtered source set.
    let fork_storage_match = {
        let leaf_hashes = collect_canonical_leaf_hashes::<SortitionId>(dst_path)?;
        conn.execute_batch("CREATE TEMP TABLE val_fork_leaf_values (value_hash TEXT PRIMARY KEY)")
            .map_err(Error::SQLError)?;
        {
            let mut stmt = conn
                .prepare("INSERT INTO val_fork_leaf_values (value_hash) VALUES (?1)")
                .map_err(Error::SQLError)?;
            for hash in &leaf_hashes {
                stmt.execute([hash]).map_err(Error::SQLError)?;
            }
        }
        let ok = full_row_except_match(
            &conn,
            "SELECT * FROM __fork_storage",
            "SELECT f.* FROM src.__fork_storage f \
             INNER JOIN val_fork_leaf_values lv ON f.value_hash = lv.value_hash",
        )?;
        conn.execute_batch("DROP TABLE IF EXISTS val_fork_leaf_values")
            .map_err(Error::SQLError)?;
        ok
    };

    let _ = conn.execute_batch("DROP TABLE IF EXISTS canonical_sortitions");
    let _ = conn.execute_batch("DROP TABLE IF EXISTS canonical_burn_hashes");
    conn.execute_batch("DETACH DATABASE src")
        .map_err(Error::SQLError)?;

    Ok(SortitionSideTableValidation {
        required_tables_present,
        canonical_set_in_source,
        snapshots_match,
        leader_keys_match,
        block_commits_match,
        block_commit_parents_match,
        snapshot_transition_ops_match,
        stacks_chain_tips_match,
        stacks_chain_tips_by_burn_view_match,
        stacks_chain_tips_within_stacks_boundary,
        stacks_chain_tips_anchor_match,
        preprocessed_reward_sets_match,
        missed_commits_match,
        stack_stx_match,
        transfer_stx_match,
        delegate_stx_match,
        vote_for_aggregate_key_match,
        epochs_match,
        db_config_match,
        fork_storage_match,
    })
}
