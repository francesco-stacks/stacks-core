use rusqlite::{params, Connection};

use crate::chainstate::stacks::index::Error;

/// Row-count statistics returned by [`copy_index_side_tables`].
#[derive(Debug, Clone)]
pub struct IndexSideTableStats {
    /// Rows copied into `block_headers` (epoch 2.x headers, canonical only).
    pub block_headers_rows: u64,
    /// Rows copied into `nakamoto_block_headers` (canonical only).
    pub nakamoto_block_headers_rows: u64,
    /// Rows copied into `payments` (canonical only).
    pub payments_rows: u64,
    /// Rows copied into `transactions` (canonical only).
    pub transactions_rows: u64,
    /// Rows copied into `nakamoto_tenure_events` (canonical only).
    pub nakamoto_tenure_events_rows: u64,
    /// Rows copied into `nakamoto_reward_sets` (full copy).
    pub nakamoto_reward_sets_rows: u64,
    /// Rows copied into `signer_stats` (full copy).
    pub signer_stats_rows: u64,
    /// Rows copied into `matured_rewards` (full copy).
    pub matured_rewards_rows: u64,
    /// Rows copied into `burnchain_txids` (full copy).
    pub burnchain_txids_rows: u64,
    /// Rows copied into `epoch_transitions` (full copy).
    pub epoch_transitions_rows: u64,
}

/// Validation result for index side tables in a squashed DB.
#[derive(Debug, Clone)]
pub struct IndexSideTableValidation {
    /// All required tables exist in destination.
    pub tables_present: bool,
    /// `db_config` is a verbatim copy of the source.
    pub db_config_matches: bool,
    /// Canonical-filtered tables: exact rowcount match (filtered source == destination).
    pub block_headers_count_match: bool,
    pub nakamoto_headers_count_match: bool,
    pub payments_count_match: bool,
    pub transactions_count_match: bool,
    pub nakamoto_tenure_events_count_match: bool,
    /// "Copy all" tables: exact rowcount match (src == dst).
    pub nakamoto_reward_sets_count_match: bool,
    pub signer_stats_count_match: bool,
    pub matured_rewards_count_match: bool,
    pub burnchain_txids_count_match: bool,
    pub epoch_transitions_count_match: bool,
    /// Schema-only tables exist and are empty.
    pub staging_tables_empty: bool,
    /// No out-of-range rows leaked into destination.
    pub transactions_no_extra_blocks: bool,
    pub tenure_events_no_extra_blocks: bool,
}

impl IndexSideTableValidation {
    /// Returns `true` if every validation check passed (all tables present,
    /// all row counts match, no out-of-range rows, staging tables empty).
    pub fn is_valid(&self) -> bool {
        self.tables_present
            && self.db_config_matches
            && self.block_headers_count_match
            && self.nakamoto_headers_count_match
            && self.payments_count_match
            && self.transactions_count_match
            && self.nakamoto_tenure_events_count_match
            && self.nakamoto_reward_sets_count_match
            && self.signer_stats_count_match
            && self.matured_rewards_count_match
            && self.burnchain_txids_count_match
            && self.epoch_transitions_count_match
            && self.staging_tables_empty
            && self.transactions_no_extra_blocks
            && self.tenure_events_no_extra_blocks
    }
}

/// Required table names that must be present in the squashed index DB.
const REQUIRED_TABLES: &[&str] = &[
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
    "staging_microblocks",
    "staging_microblocks_data",
];

/// Clone table and index schemas from the source DB (via `sqlite_master`) into the
/// destination connection. This avoids duplicating any CREATE TABLE / ALTER TABLE /
/// CREATE INDEX statements and is always in sync with whatever migration version the
/// source is at.
///
/// Expects the source DB to be ATTACHed as `src`.
fn clone_schemas_from_source(conn: &Connection, tables: &[&str]) -> Result<(), Error> {
    // Collect all CREATE TABLE and CREATE INDEX statements for the required tables.
    let mut stmts: Vec<String> = Vec::new();

    for table in tables {
        // Get the CREATE TABLE statement.
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM src.sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| row.get(0),
            )
            .ok();

        if let Some(create_sql) = sql {
            // sqlite_master stores the original CREATE TABLE without IF NOT EXISTS.
            // Replace "CREATE TABLE" with "CREATE TABLE IF NOT EXISTS" for idempotency.
            let safe_sql = create_sql.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1);
            stmts.push(safe_sql);
        }

        // Get all indexes for this table.
        let mut idx_stmt = conn
            .prepare("SELECT sql FROM src.sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL")
            .map_err(Error::SQLError)?;
        let idx_rows = idx_stmt
            .query_map(params![table], |row| row.get::<_, String>(0))
            .map_err(Error::SQLError)?;
        for idx_sql in idx_rows {
            let idx_sql = idx_sql.map_err(Error::SQLError)?;
            let safe_sql = idx_sql.replacen("CREATE INDEX", "CREATE INDEX IF NOT EXISTS", 1);
            stmts.push(safe_sql);
        }
    }

    for stmt in &stmts {
        conn.execute_batch(stmt).map_err(Error::SQLError)?;
    }

    Ok(())
}

/// Clone schemas only for tables that exist in the source DB.
/// Returns the list of tables that were actually cloned.
fn clone_optional_schemas_from_source(
    conn: &Connection,
    tables: &[&str],
) -> Result<Vec<String>, Error> {
    let mut present = Vec::new();
    for table in tables {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| row.get(0),
            )
            .map_err(Error::SQLError)?;
        if exists {
            clone_schemas_from_source(conn, &[table])?;
            present.push(table.to_string());
        }
    }
    Ok(present)
}

/// Populate a temp table with the canonical block hashes from the squashed MARF's
/// `marf_squash_block_heights` metadata. This table was written during squash from
/// the MARF's canonical chain walk, so it contains exactly the canonical blocks and
/// excludes fork data.
fn populate_canonical_blocks(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch("CREATE TEMP TABLE canonical_blocks (index_block_hash TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT OR IGNORE INTO canonical_blocks (index_block_hash) \
         SELECT block_hash FROM marf_squash_block_heights",
        [],
    )
    .map_err(Error::SQLError)?;
    Ok(())
}

/// Copy required non-MARF tables from the source `index.sqlite` into the
/// squashed destination. Only canonical rows (determined by the squashed MARF's
/// `marf_squash_block_heights`) are included, excluding non-canonical fork data.
pub fn copy_index_side_tables(
    src_path: &str,
    dst_path: &str,
    height: u32,
) -> Result<IndexSideTableStats, Error> {
    let conn = Connection::open(dst_path).map_err(Error::SQLError)?;

    conn.execute("ATTACH DATABASE ?1 AS src", params![src_path])
        .map_err(Error::SQLError)?;

    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(Error::SQLError)?;

    // Clone schemas inside the transaction so rollback is fully atomic.
    if let Err(e) = clone_schemas_from_source(&conn, REQUIRED_TABLES) {
        let _ = conn.execute_batch("ROLLBACK");
        let _ = conn.execute_batch("DETACH DATABASE src");
        return Err(e);
    }

    let result = copy_tables_inner(&conn, height);

    match result {
        Ok(stats) => {
            conn.execute_batch("COMMIT").map_err(Error::SQLError)?;
            conn.execute_batch("DETACH DATABASE src")
                .map_err(Error::SQLError)?;
            Ok(stats)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            let _ = conn.execute_batch("DETACH DATABASE src");
            Err(e)
        }
    }
}

fn copy_tables_inner(conn: &Connection, _height: u32) -> Result<IndexSideTableStats, Error> {
    // Copy db_config verbatim.
    conn.execute(
        "INSERT OR REPLACE INTO db_config SELECT * FROM src.db_config",
        [],
    )
    .map_err(Error::SQLError)?;

    // Build canonical block set from squash metadata.
    // marf_squash_block_heights was populated during squash from the MARF's
    // canonical chain walk (get_block_at_height for each height 0..H).
    populate_canonical_blocks(conn)?;

    // Copy only canonical block_headers (by index_block_hash, not by height).
    let block_headers_rows = conn
        .execute(
            "INSERT INTO block_headers SELECT * FROM src.block_headers \
             WHERE index_block_hash IN (SELECT index_block_hash FROM canonical_blocks)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let nakamoto_block_headers_rows = conn
        .execute(
            "INSERT INTO nakamoto_block_headers SELECT * FROM src.nakamoto_block_headers \
             WHERE index_block_hash IN (SELECT index_block_hash FROM canonical_blocks)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // payments: filter by index_block_hash (canonical blocks only).
    let payments_rows = conn
        .execute(
            "INSERT INTO payments SELECT * FROM src.payments \
             WHERE index_block_hash IN (SELECT index_block_hash FROM canonical_blocks)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // Identifier-filtered tables using the canonical block set.
    let transactions_rows = conn
        .execute(
            "INSERT INTO transactions \
             SELECT * FROM src.transactions \
             WHERE index_block_hash IN (SELECT index_block_hash FROM canonical_blocks)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let nakamoto_tenure_events_rows = conn
        .execute(
            "INSERT INTO nakamoto_tenure_events \
             SELECT * FROM src.nakamoto_tenure_events \
             WHERE block_id IN (SELECT index_block_hash FROM canonical_blocks)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // "Copy all" tables.
    let nakamoto_reward_sets_rows = conn
        .execute(
            "INSERT INTO nakamoto_reward_sets SELECT * FROM src.nakamoto_reward_sets",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let signer_stats_rows = conn
        .execute(
            "INSERT INTO signer_stats SELECT * FROM src.signer_stats",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let matured_rewards_rows = conn
        .execute(
            "INSERT INTO matured_rewards SELECT * FROM src.matured_rewards",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let burnchain_txids_rows = conn
        .execute(
            "INSERT INTO burnchain_txids SELECT * FROM src.burnchain_txids",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let epoch_transitions_rows = conn
        .execute(
            "INSERT INTO epoch_transitions SELECT * FROM src.epoch_transitions",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    conn.execute_batch("DROP TABLE IF EXISTS canonical_blocks")
        .map_err(Error::SQLError)?;

    Ok(IndexSideTableStats {
        block_headers_rows,
        nakamoto_block_headers_rows,
        payments_rows,
        transactions_rows,
        nakamoto_tenure_events_rows,
        nakamoto_reward_sets_rows,
        signer_stats_rows,
        matured_rewards_rows,
        burnchain_txids_rows,
        epoch_transitions_rows,
    })
}

/// Validate that the squashed index DB has the correct side tables by
/// comparing against the source.
pub fn validate_index_side_tables(
    src_path: &str,
    dst_path: &str,
    _height: u32,
) -> Result<IndexSideTableValidation, Error> {
    let conn = Connection::open(dst_path).map_err(Error::SQLError)?;
    conn.execute("ATTACH DATABASE ?1 AS src", params![src_path])
        .map_err(Error::SQLError)?;

    // Check all required tables exist.
    let tables_present = REQUIRED_TABLES.iter().all(|table| {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    });

    // db_config verbatim match.
    let db_config_matches = conn
        .query_row(
            "SELECT COUNT(*) FROM (
                SELECT version, mainnet, chain_id FROM db_config
                EXCEPT
                SELECT version, mainnet, chain_id FROM src.db_config
            )",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(1)
        == 0
        && conn
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT version, mainnet, chain_id FROM src.db_config
                    EXCEPT
                    SELECT version, mainnet, chain_id FROM db_config
                )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(1)
            == 0;

    // Build canonical block set from the squashed MARF's metadata (authoritative
    // source, not derived from copied headers).
    let _ = conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS val_canonical_blocks (index_block_hash TEXT PRIMARY KEY)",
    );
    let _ = conn.execute(
        "INSERT OR IGNORE INTO val_canonical_blocks (index_block_hash) \
         SELECT block_hash FROM marf_squash_block_heights",
        [],
    );

    // Canonical-filtered tables: count in source (canonical only) == count in destination.
    let block_headers_count_match = {
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM src.block_headers \
                 WHERE index_block_hash IN (SELECT index_block_hash FROM val_canonical_blocks)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let dst_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM block_headers", [], |row| row.get(0))
            .unwrap_or(-2);
        src_count == dst_count
    };

    let nakamoto_headers_count_match = {
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM src.nakamoto_block_headers \
                 WHERE index_block_hash IN (SELECT index_block_hash FROM val_canonical_blocks)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let dst_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nakamoto_block_headers", [], |row| {
                row.get(0)
            })
            .unwrap_or(-2);
        src_count == dst_count
    };

    let payments_count_match = {
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM src.payments \
                 WHERE index_block_hash IN (SELECT index_block_hash FROM val_canonical_blocks)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let dst_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM payments", [], |row| row.get(0))
            .unwrap_or(-2);
        src_count == dst_count
    };

    let transactions_count_match = {
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM src.transactions \
                 WHERE index_block_hash IN (SELECT index_block_hash FROM val_canonical_blocks)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let dst_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))
            .unwrap_or(-2);
        src_count == dst_count
    };

    let nakamoto_tenure_events_count_match = {
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM src.nakamoto_tenure_events \
                 WHERE block_id IN (SELECT index_block_hash FROM val_canonical_blocks)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let dst_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nakamoto_tenure_events", [], |row| {
                row.get(0)
            })
            .unwrap_or(-2);
        src_count == dst_count
    };

    // No out-of-range rows leaked.
    let transactions_no_extra_blocks = conn
        .query_row(
            "SELECT COUNT(*) FROM transactions \
             WHERE index_block_hash NOT IN (SELECT index_block_hash FROM val_canonical_blocks)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(1)
        == 0;

    let tenure_events_no_extra_blocks = conn
        .query_row(
            "SELECT COUNT(*) FROM nakamoto_tenure_events \
             WHERE block_id NOT IN (SELECT index_block_hash FROM val_canonical_blocks)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(1)
        == 0;

    let _ = conn.execute_batch("DROP TABLE IF EXISTS val_canonical_blocks");

    // "Copy all" tables: exact rowcount match.
    let nakamoto_reward_sets_count_match = count_match(
        &conn,
        "SELECT COUNT(*) FROM src.nakamoto_reward_sets",
        "SELECT COUNT(*) FROM nakamoto_reward_sets",
    );
    let signer_stats_count_match = count_match(
        &conn,
        "SELECT COUNT(*) FROM src.signer_stats",
        "SELECT COUNT(*) FROM signer_stats",
    );
    let matured_rewards_count_match = count_match(
        &conn,
        "SELECT COUNT(*) FROM src.matured_rewards",
        "SELECT COUNT(*) FROM matured_rewards",
    );
    let burnchain_txids_count_match = count_match(
        &conn,
        "SELECT COUNT(*) FROM src.burnchain_txids",
        "SELECT COUNT(*) FROM burnchain_txids",
    );
    let epoch_transitions_count_match = count_match(
        &conn,
        "SELECT COUNT(*) FROM src.epoch_transitions",
        "SELECT COUNT(*) FROM epoch_transitions",
    );

    // Schema-only tables should be empty.
    let staging_tables_empty = [
        "staging_blocks",
        "staging_microblocks",
        "staging_microblocks_data",
    ]
    .iter()
    .all(|table| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(1)
            == 0
    });

    conn.execute_batch("DETACH DATABASE src")
        .map_err(Error::SQLError)?;

    Ok(IndexSideTableValidation {
        tables_present,
        db_config_matches,
        block_headers_count_match,
        nakamoto_headers_count_match,
        payments_count_match,
        transactions_count_match,
        nakamoto_tenure_events_count_match,
        nakamoto_reward_sets_count_match,
        signer_stats_count_match,
        matured_rewards_count_match,
        burnchain_txids_count_match,
        epoch_transitions_count_match,
        staging_tables_empty,
        transactions_no_extra_blocks,
        tenure_events_no_extra_blocks,
    })
}

fn count_match(conn: &Connection, src_sql: &str, dst_sql: &str) -> bool {
    let src_count: i64 = conn.query_row(src_sql, [], |row| row.get(0)).unwrap_or(-1);
    let dst_count: i64 = conn.query_row(dst_sql, [], |row| row.get(0)).unwrap_or(-2);
    src_count == dst_count
}

/// Check bidirectional full-row EXCEPT equality for a table.
/// Returns true if the two result sets are identical.
fn full_row_except_match(conn: &Connection, dst_sql: &str, src_sql: &str) -> bool {
    // dst has rows not in src
    let extra_in_dst: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM ({dst_sql} EXCEPT {src_sql})"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    // src has rows not in dst
    let extra_in_src: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM ({src_sql} EXCEPT {dst_sql})"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    extra_in_dst == 0 && extra_in_src == 0
}

// ---------------------------------------------------------------------------
// Sortition side-table copy and validation
// ---------------------------------------------------------------------------

/// Required sortition tables always present in production.
const SORTITION_REQUIRED_TABLES: &[&str] = &[
    "db_config",
    "snapshots",
    "leader_keys",
    "block_commits",
    "block_commit_parents",
    "snapshot_transition_ops",
    "stacks_chain_tips",
    "missed_commits",
    "stack_stx",
    "transfer_stx",
    "delegate_stx",
    "vote_for_aggregate_key",
    "preprocessed_reward_sets",
    "epochs",
];

/// Optional sortition tables (may not exist in production).
const SORTITION_OPTIONAL_TABLES: &[&str] = &[
    "ast_rule_heights",            // dropped by SORTITION_DB_SCHEMA_10
    "snapshot_burn_distributions", // test-only (#[cfg(test)])
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
    pub preprocessed_reward_sets_rows: u64,
    pub missed_commits_rows: u64,
    pub stack_stx_rows: u64,
    pub transfer_stx_rows: u64,
    pub delegate_stx_rows: u64,
    pub vote_for_aggregate_key_rows: u64,
    pub epochs_rows: u64,
    pub db_config_rows: u64,
}

/// Validation result for sortition side tables in a squashed DB.
///
/// See [`validate_sortition_side_tables`] for the trust boundary — this checks
/// consistency with the destination-declared canonical set, not independent
/// canonicality. MARF trie validation must be done separately.
#[derive(Debug, Clone)]
pub struct SortitionSideTableValidation {
    pub required_tables_present: bool,
    /// Every sortition_id in destination `marf_squash_block_heights` exists in
    /// the source `snapshots` table. False if the destination claims sortition IDs
    /// that the source doesn't have.
    pub canonical_set_in_source: bool,
    // Full-row EXCEPT equality for each table (true = bidirectional EXCEPT both empty)
    pub snapshots_match: bool,
    pub leader_keys_match: bool,
    pub block_commits_match: bool,
    pub block_commit_parents_match: bool,
    pub snapshot_transition_ops_match: bool,
    pub stacks_chain_tips_match: bool,
    pub preprocessed_reward_sets_match: bool,
    pub missed_commits_match: bool,
    pub stack_stx_match: bool,
    pub transfer_stx_match: bool,
    pub delegate_stx_match: bool,
    pub vote_for_aggregate_key_match: bool,
    // Full-copy tables (also full-row EXCEPT)
    pub epochs_match: bool,
    pub db_config_match: bool,
    // Optional tables (None = absent in both source and dest, which is OK)
    pub ast_rule_heights_match: Option<bool>,
    pub snapshot_burn_distributions_match: Option<bool>,
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
            && self.preprocessed_reward_sets_match
            && self.missed_commits_match
            && self.stack_stx_match
            && self.transfer_stx_match
            && self.delegate_stx_match
            && self.vote_for_aggregate_key_match
            && self.epochs_match
            && self.db_config_match
            && self.ast_rule_heights_match.unwrap_or(true)
            && self.snapshot_burn_distributions_match.unwrap_or(true)
    }
}

/// Build temp tables for the canonical sortition set and canonical burn hashes.
fn populate_canonical_sortitions(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch("CREATE TEMP TABLE canonical_sortitions (sortition_id TEXT PRIMARY KEY)")
        .map_err(Error::SQLError)?;
    conn.execute(
        "INSERT OR IGNORE INTO canonical_sortitions (sortition_id) \
         SELECT block_hash FROM marf_squash_block_heights",
        [],
    )
    .map_err(Error::SQLError)?;

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

/// Copy required non-MARF tables from the source sortition DB into the
/// squashed destination. Only canonical rows (determined by the squashed MARF's
/// `marf_squash_block_heights`) are included.
pub fn copy_sortition_side_tables(
    src_path: &str,
    dst_path: &str,
) -> Result<SortitionSideTableStats, Error> {
    let conn = Connection::open(dst_path).map_err(Error::SQLError)?;

    conn.execute("ATTACH DATABASE ?1 AS src", params![src_path])
        .map_err(Error::SQLError)?;

    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(Error::SQLError)?;

    if let Err(e) = clone_schemas_from_source(&conn, SORTITION_REQUIRED_TABLES) {
        let _ = conn.execute_batch("ROLLBACK");
        let _ = conn.execute_batch("DETACH DATABASE src");
        return Err(e);
    }
    // Clone optional tables if present in source.
    if let Err(e) = clone_optional_schemas_from_source(&conn, SORTITION_OPTIONAL_TABLES) {
        let _ = conn.execute_batch("ROLLBACK");
        let _ = conn.execute_batch("DETACH DATABASE src");
        return Err(e);
    }

    let result = copy_sortition_tables_inner(&conn);

    match result {
        Ok(stats) => {
            conn.execute_batch("COMMIT").map_err(Error::SQLError)?;
            conn.execute_batch("DETACH DATABASE src")
                .map_err(Error::SQLError)?;
            Ok(stats)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            let _ = conn.execute_batch("DETACH DATABASE src");
            Err(e)
        }
    }
}

fn copy_sortition_tables_inner(conn: &Connection) -> Result<SortitionSideTableStats, Error> {
    // Copy db_config verbatim.
    let db_config_rows = conn
        .execute(
            "INSERT OR REPLACE INTO db_config SELECT * FROM src.db_config",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // Build canonical sortition set from squash metadata.
    populate_canonical_sortitions(conn)?;

    // sortition_id-filtered tables.
    let snapshots_rows = conn
        .execute(
            "INSERT INTO snapshots SELECT * FROM src.snapshots \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let leader_keys_rows = conn
        .execute(
            "INSERT INTO leader_keys SELECT * FROM src.leader_keys \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let block_commits_rows = conn
        .execute(
            "INSERT INTO block_commits SELECT * FROM src.block_commits \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let block_commit_parents_rows = conn
        .execute(
            "INSERT INTO block_commit_parents SELECT * FROM src.block_commit_parents \
             WHERE block_commit_sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let snapshot_transition_ops_rows = conn
        .execute(
            "INSERT INTO snapshot_transition_ops SELECT * FROM src.snapshot_transition_ops \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let stacks_chain_tips_rows = conn
        .execute(
            "INSERT INTO stacks_chain_tips SELECT * FROM src.stacks_chain_tips \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let preprocessed_reward_sets_rows = conn
        .execute(
            "INSERT INTO preprocessed_reward_sets SELECT * FROM src.preprocessed_reward_sets \
             WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let missed_commits_rows = conn
        .execute(
            "INSERT INTO missed_commits SELECT * FROM src.missed_commits \
             WHERE intended_sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // burn_header_hash-filtered tables.
    let stack_stx_rows = conn
        .execute(
            "INSERT INTO stack_stx SELECT * FROM src.stack_stx \
             WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let transfer_stx_rows = conn
        .execute(
            "INSERT INTO transfer_stx SELECT * FROM src.transfer_stx \
             WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let delegate_stx_rows = conn
        .execute(
            "INSERT INTO delegate_stx SELECT * FROM src.delegate_stx \
             WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    let vote_for_aggregate_key_rows = conn
        .execute(
            "INSERT INTO vote_for_aggregate_key SELECT * FROM src.vote_for_aggregate_key \
             WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
            [],
        )
        .map_err(Error::SQLError)? as u64;

    // Full-copy tables.
    let epochs_rows = conn
        .execute("INSERT INTO epochs SELECT * FROM src.epochs", [])
        .map_err(Error::SQLError)? as u64;

    // Optional tables: copy if present in source. If the table exists, propagate
    // copy errors rather than silently ignoring them.
    for (table, filter) in [
        ("ast_rule_heights", ""),
        (
            "snapshot_burn_distributions",
            " WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
        ),
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name=?1",
                params![table],
                |row| row.get(0),
            )
            .map_err(Error::SQLError)?;
        if exists {
            conn.execute(
                &format!("INSERT INTO {table} SELECT * FROM src.{table}{filter}"),
                [],
            )
            .map_err(Error::SQLError)?;
        }
    }

    conn.execute_batch("DROP TABLE IF EXISTS canonical_sortitions")
        .map_err(Error::SQLError)?;
    conn.execute_batch("DROP TABLE IF EXISTS canonical_burn_hashes")
        .map_err(Error::SQLError)?;

    Ok(SortitionSideTableStats {
        snapshots_rows,
        leader_keys_rows,
        block_commits_rows,
        block_commit_parents_rows,
        snapshot_transition_ops_rows,
        stacks_chain_tips_rows,
        preprocessed_reward_sets_rows,
        missed_commits_rows,
        stack_stx_rows,
        transfer_stx_rows,
        delegate_stx_rows,
        vote_for_aggregate_key_rows,
        epochs_rows,
        db_config_rows,
    })
}

/// Validate that the squashed sortition DB has the correct side tables by
/// comparing against the source using full-row EXCEPT queries.
///
/// # Trust boundary
///
/// This validator checks that side-table rows are consistent with the canonical
/// set declared by the destination's `marf_squash_block_heights` metadata, which
/// was populated during the MARF squash by walking the canonical tip. It does NOT
/// independently re-derive the canonical chain from the source MARF — that is the
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
    let conn = Connection::open(dst_path).map_err(Error::SQLError)?;
    conn.execute("ATTACH DATABASE ?1 AS src", params![src_path])
        .map_err(Error::SQLError)?;

    // Check all required tables exist in destination.
    let required_tables_present = SORTITION_REQUIRED_TABLES.iter().all(|table| {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    });

    // Build canonical set from squash metadata.
    let _ = conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS canonical_sortitions (sortition_id TEXT PRIMARY KEY)",
    );
    let _ = conn.execute(
        "INSERT OR IGNORE INTO canonical_sortitions (sortition_id) \
         SELECT block_hash FROM marf_squash_block_heights",
        [],
    );

    // Cross-check: every sortition_id the destination claims as canonical must
    // actually exist in the source snapshots table. This catches tampered or
    // fabricated marf_squash_block_heights entries.
    let canonical_set_in_source: bool = conn
        .query_row(
            "SELECT COUNT(*) = 0 FROM canonical_sortitions cs \
             WHERE cs.sortition_id NOT IN (SELECT sortition_id FROM src.snapshots)",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    let _ = conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS canonical_burn_hashes (burn_header_hash TEXT PRIMARY KEY)",
    );
    let _ = conn.execute(
        "INSERT OR IGNORE INTO canonical_burn_hashes (burn_header_hash) \
         SELECT DISTINCT s.burn_header_hash FROM src.snapshots s \
         INNER JOIN canonical_sortitions cs ON s.sortition_id = cs.sortition_id",
        [],
    );

    // Full-row EXCEPT for sortition_id-filtered tables.
    let snapshots_match = full_row_except_match(
        &conn,
        "SELECT * FROM snapshots",
        "SELECT * FROM src.snapshots WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let leader_keys_match = full_row_except_match(
        &conn,
        "SELECT * FROM leader_keys",
        "SELECT * FROM src.leader_keys WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let block_commits_match = full_row_except_match(
        &conn,
        "SELECT * FROM block_commits",
        "SELECT * FROM src.block_commits WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let block_commit_parents_match = full_row_except_match(
        &conn,
        "SELECT * FROM block_commit_parents",
        "SELECT * FROM src.block_commit_parents WHERE block_commit_sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let snapshot_transition_ops_match = full_row_except_match(
        &conn,
        "SELECT * FROM snapshot_transition_ops",
        "SELECT * FROM src.snapshot_transition_ops WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let stacks_chain_tips_match = full_row_except_match(
        &conn,
        "SELECT * FROM stacks_chain_tips",
        "SELECT * FROM src.stacks_chain_tips WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let preprocessed_reward_sets_match = full_row_except_match(
        &conn,
        "SELECT * FROM preprocessed_reward_sets",
        "SELECT * FROM src.preprocessed_reward_sets WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );
    let missed_commits_match = full_row_except_match(
        &conn,
        "SELECT * FROM missed_commits",
        "SELECT * FROM src.missed_commits WHERE intended_sortition_id IN (SELECT sortition_id FROM canonical_sortitions)",
    );

    // Full-row EXCEPT for burn_header_hash-filtered tables.
    let stack_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM stack_stx",
        "SELECT * FROM src.stack_stx WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
    );
    let transfer_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM transfer_stx",
        "SELECT * FROM src.transfer_stx WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
    );
    let delegate_stx_match = full_row_except_match(
        &conn,
        "SELECT * FROM delegate_stx",
        "SELECT * FROM src.delegate_stx WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
    );
    let vote_for_aggregate_key_match = full_row_except_match(
        &conn,
        "SELECT * FROM vote_for_aggregate_key",
        "SELECT * FROM src.vote_for_aggregate_key WHERE burn_header_hash IN (SELECT burn_header_hash FROM canonical_burn_hashes)",
    );

    // Full-copy tables.
    let epochs_match =
        full_row_except_match(&conn, "SELECT * FROM epochs", "SELECT * FROM src.epochs");
    let db_config_match = full_row_except_match(
        &conn,
        "SELECT * FROM db_config",
        "SELECT * FROM src.db_config",
    );

    // Optional tables.
    let ast_rule_heights_match = check_optional_table_match(&conn, "ast_rule_heights", None);
    let snapshot_burn_distributions_match = check_optional_table_match(
        &conn,
        "snapshot_burn_distributions",
        Some("WHERE sortition_id IN (SELECT sortition_id FROM canonical_sortitions)"),
    );

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
        preprocessed_reward_sets_match,
        missed_commits_match,
        stack_stx_match,
        transfer_stx_match,
        delegate_stx_match,
        vote_for_aggregate_key_match,
        epochs_match,
        db_config_match,
        ast_rule_heights_match,
        snapshot_burn_distributions_match,
    })
}

/// Check an optional table's match status.
/// Returns None if absent in both, Some(false) if present in one but not other,
/// Some(true/false) from full-row EXCEPT if present in both.
fn check_optional_table_match(
    conn: &Connection,
    table: &str,
    src_filter: Option<&str>,
) -> Option<bool> {
    let in_dst: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .unwrap_or(false);
    let in_src: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM src.sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .unwrap_or(false);

    match (in_dst, in_src) {
        (false, false) => None,
        (true, false) | (false, true) => Some(false),
        (true, true) => {
            let src_sql = match src_filter {
                Some(filter) => format!("SELECT * FROM src.{table} {filter}"),
                None => format!("SELECT * FROM src.{table}"),
            };
            Some(full_row_except_match(
                conn,
                &format!("SELECT * FROM {table}"),
                &src_sql,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    use super::{copy_index_side_tables, validate_index_side_tables};
    use crate::chainstate::nakamoto::{
        NAKAMOTO_CHAINSTATE_SCHEMA_1, NAKAMOTO_CHAINSTATE_SCHEMA_2, NAKAMOTO_CHAINSTATE_SCHEMA_3,
        NAKAMOTO_CHAINSTATE_SCHEMA_4, NAKAMOTO_CHAINSTATE_SCHEMA_5, NAKAMOTO_CHAINSTATE_SCHEMA_6,
        NAKAMOTO_CHAINSTATE_SCHEMA_7, NAKAMOTO_CHAINSTATE_SCHEMA_8,
    };
    use crate::chainstate::stacks::db::{
        CHAINSTATE_INDEXES, CHAINSTATE_INITIAL_SCHEMA, CHAINSTATE_SCHEMA_2, CHAINSTATE_SCHEMA_3,
        CHAINSTATE_SCHEMA_4, CHAINSTATE_SCHEMA_5,
    };

    /// Create a source `index.sqlite` with the full chainstate schema by replaying
    /// the real migration pipeline. Returns the connection for inserting test data.
    fn create_source_db(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();

        for cmd in CHAINSTATE_INITIAL_SCHEMA {
            conn.execute_batch(cmd).unwrap();
        }
        conn.execute(
            "INSERT INTO db_config (version, mainnet, chain_id) VALUES (?1, ?2, ?3)",
            params!["1", 1i64, 1i64],
        )
        .unwrap();

        // Apply all migrations in order (same as StacksChainState::apply_schema_migrations).
        for cmd in CHAINSTATE_SCHEMA_2 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in CHAINSTATE_SCHEMA_3 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_1.iter() {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_2 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_3 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_4 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_5 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in CHAINSTATE_SCHEMA_4 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_6 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in CHAINSTATE_SCHEMA_5 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_7 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in NAKAMOTO_CHAINSTATE_SCHEMA_8 {
            conn.execute_batch(cmd).unwrap();
        }
        for cmd in CHAINSTATE_INDEXES {
            conn.execute_batch(cmd).unwrap();
        }

        conn
    }

    /// Create a destination DB that simulates a squashed MARF by adding the
    /// `marf_squash_block_heights` table with the given canonical block hashes.
    fn create_dest_db_with_canonical_blocks(path: &std::path::Path, canonical: &[&str]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS marf_squash_block_heights (block_hash TEXT NOT NULL, height INTEGER NOT NULL)",
        )
        .unwrap();
        for (h, bh) in canonical.iter().enumerate() {
            conn.execute(
                "INSERT INTO marf_squash_block_heights (block_hash, height) VALUES (?1, ?2)",
                params![bh, h as i64],
            )
            .unwrap();
        }
    }

    /// Insert a block_headers row at the given height.
    fn insert_block_header(conn: &Connection, height: u32, suffix: &str) {
        conn.execute(
            "INSERT INTO block_headers (version, total_burn, total_work, proof, parent_block, \
             parent_microblock, parent_microblock_sequence, tx_merkle_root, state_index_root, \
             microblock_pubkey_hash, block_hash, index_block_hash, block_height, index_root, \
             consensus_hash, burn_header_hash, burn_header_height, burn_header_timestamp, \
             parent_block_id, cost, block_size) \
             VALUES (1,'0','0','p','par','mb',0,'mr','sr','mph',?1,?2,?3,'ir',?4,'bhh',?3,0,'pid','0','0')",
            params![
                format!("bh{suffix}"),
                format!("ibh{suffix}"),
                height,
                format!("ch{suffix}"),
            ],
        )
        .unwrap();
    }

    /// Insert a payment row at the given height.
    fn insert_payment(conn: &Connection, height: u32, suffix: &str) {
        conn.execute(
            "INSERT INTO payments (address, block_hash, consensus_hash, parent_block_hash, \
             parent_consensus_hash, coinbase, tx_fees_anchored, tx_fees_streamed, stx_burns, \
             burnchain_commit_burn, burnchain_sortition_burn, miner, stacks_block_height, \
             index_block_hash, vtxindex, recipient, schedule_type) \
             VALUES ('addr',?1,?2,'pbh','pch','100','0','0','0',0,0,1,?3,?4,0,NULL,'Epoch2')",
            params![
                format!("bh{suffix}"),
                format!("ch{suffix}"),
                height,
                format!("ibh{suffix}"),
            ],
        )
        .unwrap();
    }

    /// Insert a transaction row for the given index_block_hash.
    fn insert_transaction(conn: &Connection, id: i64, ibh: &str) {
        conn.execute(
            "INSERT INTO transactions (id, txid, index_block_hash, tx_hex, result) \
             VALUES (?1, ?2, ?3, '0x00', 'ok')",
            params![id, format!("tx{id}"), ibh],
        )
        .unwrap();
    }

    #[test]
    fn test_copy_index_side_tables_round_trip() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_index.sqlite");
        let conn = create_source_db(&src_path);

        // Insert test data at heights 1, 2, 3.
        for (h, s) in [(1, "1"), (2, "2"), (3, "3")] {
            insert_block_header(&conn, h, s);
            insert_payment(&conn, h, s);
            insert_transaction(&conn, h as i64, &format!("ibh{s}"));
        }
        conn.execute(
            "INSERT INTO nakamoto_tenure_events (tenure_id_consensus_hash, prev_tenure_id_consensus_hash, \
             burn_view_consensus_hash, cause, block_hash, block_id, coinbase_height, num_blocks_confirmed) \
             VALUES ('ch1','ch0','bv1',0,'bh1','ibh1',1,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nakamoto_reward_sets (index_block_hash, reward_set) VALUES ('ibh_rs','{}')",
            [],
        )
        .unwrap();
        drop(conn);

        // Destination: canonical blocks are ibh1, ibh2 (height 0, 1) — ibh3 is NOT canonical.
        let dst_path = dir.path().join("dst_index.sqlite");
        create_dest_db_with_canonical_blocks(&dst_path, &["ibh1", "ibh2"]);

        // Copy: only canonical blocks ibh1 and ibh2 should be included.
        let stats =
            copy_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 2)
                .unwrap();

        assert_eq!(stats.block_headers_rows, 2, "2 canonical block_headers");
        assert_eq!(stats.payments_rows, 2, "2 canonical payments");
        assert_eq!(stats.transactions_rows, 2, "2 canonical transactions");
        assert_eq!(
            stats.nakamoto_tenure_events_rows, 1,
            "1 tenure event for ibh1"
        );
        assert_eq!(stats.nakamoto_reward_sets_rows, 1);

        // Validate.
        let validation =
            validate_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 2)
                .unwrap();

        assert!(
            validation.is_valid(),
            "validation should pass: {validation:?}"
        );
        assert!(validation.tables_present);
        assert!(validation.db_config_matches);
        assert!(validation.block_headers_count_match);
        assert!(validation.payments_count_match);
        assert!(validation.transactions_count_match);
        assert!(validation.nakamoto_tenure_events_count_match);
        assert!(validation.transactions_no_extra_blocks);
        assert!(validation.tenure_events_no_extra_blocks);
        assert!(validation.staging_tables_empty);
    }

    #[test]
    fn test_copy_excludes_fork_rows() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_index.sqlite");
        let conn = create_source_db(&src_path);

        // Insert canonical block at height 1.
        insert_block_header(&conn, 1, "1_canonical");
        insert_transaction(&conn, 1, "ibh1_canonical");
        // Insert fork block at same height 1 (different consensus hash).
        insert_block_header(&conn, 1, "1_fork");
        insert_transaction(&conn, 2, "ibh1_fork");
        drop(conn);

        // Only ibh1_canonical is in the canonical set.
        let dst_path = dir.path().join("dst_index.sqlite");
        create_dest_db_with_canonical_blocks(&dst_path, &["ibh1_canonical"]);

        let stats =
            copy_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 1)
                .unwrap();

        // Only canonical block should be copied, not the fork.
        assert_eq!(stats.block_headers_rows, 1, "only canonical block_headers");
        assert_eq!(stats.transactions_rows, 1, "only canonical transactions");

        // Validate passes — fork rows excluded.
        let validation =
            validate_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 1)
                .unwrap();
        assert!(
            validation.is_valid(),
            "validation should pass without fork rows: {validation:?}"
        );
    }

    #[test]
    fn test_validate_index_side_tables_detects_extra_rows() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_index.sqlite");
        let conn = create_source_db(&src_path);

        // Insert one block + transaction.
        insert_block_header(&conn, 1, "1");
        insert_transaction(&conn, 1, "ibh1");
        drop(conn);

        let dst_path = dir.path().join("dst_index.sqlite");
        create_dest_db_with_canonical_blocks(&dst_path, &["ibh1"]);

        let _stats =
            copy_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 1)
                .unwrap();

        // Inject a transaction for a block NOT in the canonical set.
        {
            let conn = Connection::open(&dst_path).unwrap();
            conn.execute(
                "INSERT INTO transactions VALUES (99, 'tx_bad', 'ibh_UNKNOWN', '0x00', 'ok')",
                [],
            )
            .unwrap();
        }

        let validation =
            validate_index_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap(), 1)
                .unwrap();

        assert!(
            !validation.transactions_no_extra_blocks,
            "should detect extra block"
        );
        assert!(
            !validation.transactions_count_match,
            "count should mismatch"
        );
        assert!(!validation.is_valid(), "validation must fail");
    }

    // ---------------------------------------------------------------
    // Sortition side-table tests
    // ---------------------------------------------------------------

    use super::{copy_sortition_side_tables, validate_sortition_side_tables};

    /// Create a sortition source DB with a minimal schema matching production
    /// (after all migrations through schema 10). Returns the connection for
    /// inserting test data.
    fn create_sortition_source_db(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;

            CREATE TABLE snapshots(
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sortition_id TEXT UNIQUE NOT NULL,
                parent_sortition_id TEXT NOT NULL,
                burn_header_timestamp INT NOT NULL,
                parent_burn_header_hash TEXT NOT NULL,
                consensus_hash TEXT UNIQUE NOT NULL,
                ops_hash TEXT NOT NULL,
                total_burn TEXT NOT NULL,
                sortition INTEGER NOT NULL,
                sortition_hash TEXT NOT NULL,
                winning_block_txid TEXT NOT NULL,
                winning_stacks_block_hash TEXT NOT NULL,
                index_root TEXT UNIQUE NOT NULL,
                num_sortitions INTEGER NOT NULL,
                stacks_block_accepted INTEGER NOT NULL,
                stacks_block_height INTEGER NOT NULL,
                arrival_index INTEGER NOT NULL,
                canonical_stacks_tip_height INTEGER NOT NULL,
                canonical_stacks_tip_hash TEXT NOT NULL,
                canonical_stacks_tip_consensus_hash TEXT NOT NULL,
                pox_valid INTEGER NOT NULL,
                accumulated_coinbase_ustx TEXT NOT NULL,
                pox_payouts TEXT NOT NULL,
                miner_pk_hash TEXT DEFAULT NULL,
                PRIMARY KEY(sortition_id)
            );

            CREATE TABLE snapshot_transition_ops(
                sortition_id TEXT PRIMARY KEY,
                accepted_ops TEXT NOT NULL,
                consumed_keys TEXT NOT NULL
            );

            CREATE TABLE leader_keys(
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sortition_id TEXT NOT NULL,
                consensus_hash TEXT NOT NULL,
                public_key TEXT NOT NULL,
                memo TEXT,
                PRIMARY KEY(txid,sortition_id),
                FOREIGN KEY(sortition_id) REFERENCES snapshots(sortition_id)
            );

            CREATE TABLE block_commits(
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sortition_id TEXT NOT NULL,
                block_header_hash TEXT NOT NULL,
                new_seed TEXT NOT NULL,
                parent_block_ptr INTEGER NOT NULL,
                parent_vtxindex INTEGER NOT NULL,
                key_block_ptr INTEGER NOT NULL,
                key_vtxindex INTEGER NOT NULL,
                memo TEXT,
                commit_outs TEXT,
                burn_fee TEXT NOT NULL,
                sunset_burn TEXT NOT NULL,
                input TEXT NOT NULL,
                apparent_sender TEXT NOT NULL,
                burn_parent_modulus INTEGER NOT NULL,
                punished TEXT DEFAULT NULL,
                PRIMARY KEY(txid,sortition_id),
                FOREIGN KEY(sortition_id) REFERENCES snapshots(sortition_id)
            );

            CREATE TABLE stack_stx (
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sender_addr TEXT NOT NULL,
                reward_addr TEXT NOT NULL,
                stacked_ustx TEXT NOT NULL,
                num_cycles INTEGER NOT NULL,
                signer_key TEXT DEFAULT NULL,
                max_amount TEXT DEFAULT NULL,
                auth_id INTEGER DEFAULT NULL,
                PRIMARY KEY(txid,burn_header_hash)
            );

            CREATE TABLE transfer_stx (
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sender_addr TEXT NOT NULL,
                recipient_addr TEXT NOT NULL,
                transfered_ustx TEXT NOT NULL,
                memo TEXT NOT NULL,
                PRIMARY KEY(txid,burn_header_hash)
            );

            CREATE TABLE missed_commits (
                txid TEXT NOT NULL,
                input TEXT NOT NULL,
                intended_sortition_id TEXT NOT NULL,
                PRIMARY KEY(txid, intended_sortition_id)
            );

            CREATE TABLE db_config(version TEXT PRIMARY KEY);

            CREATE TABLE epochs (
                start_block_height INTEGER NOT NULL,
                end_block_height INTEGER NOT NULL,
                epoch_id INTEGER NOT NULL,
                block_limit TEXT NOT NULL,
                network_epoch INTEGER NOT NULL,
                PRIMARY KEY(start_block_height,epoch_id)
            );

            CREATE TABLE block_commit_parents (
                block_commit_txid TEXT NOT NULL,
                block_commit_sortition_id TEXT NOT NULL,
                parent_sortition_id TEXT NOT NULL,
                PRIMARY KEY(block_commit_txid,block_commit_sortition_id),
                FOREIGN KEY(block_commit_txid,block_commit_sortition_id)
                    REFERENCES block_commits(txid,sortition_id)
            );

            CREATE TABLE delegate_stx (
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sender_addr TEXT NOT NULL,
                delegate_to TEXT NOT NULL,
                reward_addr TEXT NOT NULL,
                delegated_ustx TEXT NOT NULL,
                until_burn_height INTEGER,
                PRIMARY KEY(txid,burn_header_hash)
            );

            CREATE TABLE preprocessed_reward_sets (
                sortition_id TEXT PRIMARY KEY,
                reward_set TEXT NOT NULL
            );

            CREATE TABLE stacks_chain_tips (
                sortition_id TEXT PRIMARY KEY,
                consensus_hash TEXT NOT NULL,
                block_hash TEXT NOT NULL,
                block_height INTEGER NOT NULL
            );

            CREATE TABLE vote_for_aggregate_key (
                txid TEXT NOT NULL,
                vtxindex INTEGER NOT NULL,
                block_height INTEGER NOT NULL,
                burn_header_hash TEXT NOT NULL,
                sender_addr TEXT NOT NULL,
                aggregate_key TEXT NOT NULL,
                round INTEGER NOT NULL,
                reward_cycle INTEGER NOT NULL,
                signer_index INTEGER NOT NULL,
                signer_key TEXT NOT NULL,
                PRIMARY KEY(txid,burn_header_hash)
            );
            ",
        )
        .unwrap();
        conn.execute("INSERT INTO db_config (version) VALUES ('10')", [])
            .unwrap();
        conn
    }

    /// Insert a snapshot row for the given sortition_id and burn_header_hash.
    fn insert_snapshot(
        conn: &Connection,
        sortition_id: &str,
        burn_header_hash: &str,
        block_height: u32,
    ) {
        conn.execute(
            "INSERT INTO snapshots (
                block_height, burn_header_hash, sortition_id, parent_sortition_id,
                burn_header_timestamp, parent_burn_header_hash, consensus_hash,
                ops_hash, total_burn, sortition, sortition_hash,
                winning_block_txid, winning_stacks_block_hash, index_root,
                num_sortitions, stacks_block_accepted, stacks_block_height,
                arrival_index, canonical_stacks_tip_height, canonical_stacks_tip_hash,
                canonical_stacks_tip_consensus_hash, pox_valid,
                accumulated_coinbase_ustx, pox_payouts, miner_pk_hash
            ) VALUES (
                ?1, ?2, ?3, 'parent_sort', 1000, 'parent_bhh', ?4,
                'ops', '0', 1, 'shash', 'wbtxid', 'wsbh', ?5,
                ?1, 0, 0, ?1, 0, 'csth', 'cstch', 1, '0', '[]', NULL
            )",
            params![
                block_height,
                burn_header_hash,
                sortition_id,
                format!("ch_{sortition_id}"),
                format!("ir_{sortition_id}"),
            ],
        )
        .unwrap();
    }

    /// Insert a leader_keys row for the given sortition_id.
    fn insert_leader_key(conn: &Connection, sortition_id: &str) {
        conn.execute(
            "INSERT INTO leader_keys (txid, vtxindex, block_height, burn_header_hash, \
             sortition_id, consensus_hash, public_key, memo) \
             VALUES (?1, 0, 1, 'bhh', ?2, 'ch', 'pk', 'memo')",
            params![format!("lk_tx_{sortition_id}"), sortition_id],
        )
        .unwrap();
    }

    /// Insert a block_commits row for the given sortition_id.
    fn insert_block_commit(conn: &Connection, sortition_id: &str) {
        conn.execute(
            "INSERT INTO block_commits (txid, vtxindex, block_height, burn_header_hash, \
             sortition_id, block_header_hash, new_seed, parent_block_ptr, parent_vtxindex, \
             key_block_ptr, key_vtxindex, memo, commit_outs, burn_fee, sunset_burn, \
             input, apparent_sender, burn_parent_modulus, punished) \
             VALUES (?1, 0, 1, 'bhh', ?2, 'bhh', 'seed', 0, 0, 0, 0, '', '', '0', '0', \
             'input', 'sender', 0, NULL)",
            params![format!("bc_tx_{sortition_id}"), sortition_id],
        )
        .unwrap();
    }

    /// Insert a block_commit_parents row.
    fn insert_block_commit_parent(conn: &Connection, sortition_id: &str) {
        conn.execute(
            "INSERT INTO block_commit_parents (block_commit_txid, block_commit_sortition_id, \
             parent_sortition_id) VALUES (?1, ?2, 'parent_sort')",
            params![format!("bc_tx_{sortition_id}"), sortition_id],
        )
        .unwrap();
    }

    /// Insert a stack_stx row for the given burn_header_hash.
    fn insert_stack_stx(conn: &Connection, burn_header_hash: &str, txid: &str) {
        conn.execute(
            "INSERT INTO stack_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, reward_addr, stacked_ustx, num_cycles, signer_key, max_amount, auth_id) \
             VALUES (?1, 0, 1, ?2, 'sender', 'reward', '1000', 1, NULL, NULL, NULL)",
            params![txid, burn_header_hash],
        )
        .unwrap();
    }

    /// Insert an epochs row.
    fn insert_epoch(conn: &Connection, start: u32, epoch_id: u32) {
        conn.execute(
            "INSERT INTO epochs (start_block_height, end_block_height, epoch_id, \
             block_limit, network_epoch) VALUES (?1, ?2, ?3, '{}', 1)",
            params![start, start + 100, epoch_id],
        )
        .unwrap();
    }

    /// Create a sortition dest DB simulating a squashed MARF with the given
    /// canonical sortition IDs.
    fn create_sortition_dest_db(path: &std::path::Path, canonical_sortition_ids: &[&str]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS marf_squash_block_heights \
             (block_hash TEXT NOT NULL, height INTEGER NOT NULL)",
        )
        .unwrap();
        for (h, sid) in canonical_sortition_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO marf_squash_block_heights (block_hash, height) VALUES (?1, ?2)",
                params![sid, h as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn test_sortition_copy_excludes_fork_data() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        // Canonical chain: sort_0 at height 0, sort_1 at height 1.
        insert_snapshot(&conn, "sort_0", "bhh_0", 0);
        insert_snapshot(&conn, "sort_1", "bhh_1", 1);
        // Fork at height 1: sort_1_fork with different burn hash.
        insert_snapshot(&conn, "sort_1_fork", "bhh_1_fork", 1);

        // Insert related data for canonical and fork.
        insert_leader_key(&conn, "sort_1");
        insert_leader_key(&conn, "sort_1_fork");
        insert_block_commit(&conn, "sort_1");
        insert_block_commit(&conn, "sort_1_fork");
        insert_block_commit_parent(&conn, "sort_1");
        insert_block_commit_parent(&conn, "sort_1_fork");
        insert_stack_stx(&conn, "bhh_1", "stx_tx_canon");
        insert_stack_stx(&conn, "bhh_1_fork", "stx_tx_fork");
        insert_epoch(&conn, 0, 1);

        // Transition ops.
        conn.execute(
            "INSERT INTO snapshot_transition_ops (sortition_id, accepted_ops, consumed_keys) \
             VALUES ('sort_1', '[]', '[]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshot_transition_ops (sortition_id, accepted_ops, consumed_keys) \
             VALUES ('sort_1_fork', '[]', '[]')",
            [],
        )
        .unwrap();

        // Stacks chain tips.
        conn.execute(
            "INSERT INTO stacks_chain_tips (sortition_id, consensus_hash, block_hash, block_height) \
             VALUES ('sort_1', 'ch', 'bh', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stacks_chain_tips (sortition_id, consensus_hash, block_hash, block_height) \
             VALUES ('sort_1_fork', 'ch2', 'bh2', 1)",
            [],
        )
        .unwrap();

        // Missed commits.
        conn.execute(
            "INSERT INTO missed_commits (txid, input, intended_sortition_id) \
             VALUES ('mc_tx', 'input', 'sort_1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO missed_commits (txid, input, intended_sortition_id) \
             VALUES ('mc_tx_fork', 'input', 'sort_1_fork')",
            [],
        )
        .unwrap();

        // Preprocessed reward sets.
        conn.execute(
            "INSERT INTO preprocessed_reward_sets (sortition_id, reward_set) \
             VALUES ('sort_1', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO preprocessed_reward_sets (sortition_id, reward_set) \
             VALUES ('sort_1_fork', '{}')",
            [],
        )
        .unwrap();

        drop(conn);

        // Only sort_0 and sort_1 are canonical.
        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0", "sort_1"]);

        let stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        // Only canonical rows should be copied.
        assert_eq!(stats.snapshots_rows, 2, "2 canonical snapshots");
        assert_eq!(stats.leader_keys_rows, 1, "only sort_1 leader key");
        assert_eq!(stats.block_commits_rows, 1, "only sort_1 block commit");
        assert_eq!(
            stats.block_commit_parents_rows, 1,
            "only sort_1 block commit parent"
        );
        assert_eq!(
            stats.snapshot_transition_ops_rows, 1,
            "only sort_1 transition ops"
        );
        assert_eq!(stats.stacks_chain_tips_rows, 1, "only sort_1 chain tip");
        assert_eq!(stats.missed_commits_rows, 1, "only sort_1 missed commit");
        assert_eq!(
            stats.preprocessed_reward_sets_rows, 1,
            "only sort_1 reward set"
        );
        assert_eq!(stats.stack_stx_rows, 1, "only bhh_1 stack_stx");
        assert_eq!(stats.epochs_rows, 1, "epochs full copy");
        assert_eq!(stats.db_config_rows, 1, "db_config full copy");

        // Validate passes.
        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();
        assert!(
            validation.is_valid(),
            "validation should pass: {validation:?}"
        );
    }

    #[test]
    fn test_sortition_validate_detects_payload_corruption() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        insert_snapshot(&conn, "sort_0", "bhh_0", 0);
        insert_epoch(&conn, 0, 1);
        drop(conn);

        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0"]);

        let _stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        // Corrupt a non-key column in the destination snapshots table.
        {
            let conn = Connection::open(&dst_path).unwrap();
            conn.execute(
                "UPDATE snapshots SET burn_header_timestamp = 9999 WHERE sortition_id = 'sort_0'",
                [],
            )
            .unwrap();
        }

        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        assert!(
            !validation.snapshots_match,
            "payload corruption should be detected"
        );
        assert!(!validation.is_valid(), "validation must fail");
    }

    #[test]
    fn test_sortition_validate_detects_extra_rows() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        insert_snapshot(&conn, "sort_0", "bhh_0", 0);
        insert_epoch(&conn, 0, 1);
        drop(conn);

        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0"]);

        let _stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        // Inject an extra leader_keys row in destination that doesn't exist in source.
        {
            let conn = Connection::open(&dst_path).unwrap();
            conn.execute(
                "INSERT INTO leader_keys (txid, vtxindex, block_height, burn_header_hash, \
                 sortition_id, consensus_hash, public_key, memo) \
                 VALUES ('extra_tx', 0, 1, 'bhh', 'sort_0', 'ch', 'pk', 'memo')",
                [],
            )
            .unwrap();
        }

        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        assert!(
            !validation.leader_keys_match,
            "extra rows should be detected"
        );
        assert!(!validation.is_valid(), "validation must fail");
    }

    #[test]
    fn test_sortition_burn_header_hash_filtering() {
        // Verify that burn_header_hash-keyed tables (stack_stx, transfer_stx, etc.)
        // correctly exclude rows associated with non-canonical burn hashes.
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        insert_snapshot(&conn, "sort_0", "bhh_canon", 0);
        insert_snapshot(&conn, "sort_0_fork", "bhh_fork", 0);

        // stack_stx at canonical and fork burn hashes.
        insert_stack_stx(&conn, "bhh_canon", "stx_canon");
        insert_stack_stx(&conn, "bhh_fork", "stx_fork");

        // transfer_stx at canonical and fork.
        conn.execute(
            "INSERT INTO transfer_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, recipient_addr, transfered_ustx, memo) \
             VALUES ('xfer_canon', 0, 0, 'bhh_canon', 's', 'r', '100', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transfer_stx (txid, vtxindex, block_height, burn_header_hash, \
             sender_addr, recipient_addr, transfered_ustx, memo) \
             VALUES ('xfer_fork', 0, 0, 'bhh_fork', 's', 'r', '100', 'x')",
            [],
        )
        .unwrap();

        insert_epoch(&conn, 0, 1);
        drop(conn);

        // Only sort_0 is canonical → bhh_canon is the only canonical burn hash.
        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0"]);

        let stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        assert_eq!(stats.stack_stx_rows, 1, "only bhh_canon stack_stx");
        assert_eq!(stats.transfer_stx_rows, 1, "only bhh_canon transfer_stx");

        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();
        assert!(
            validation.is_valid(),
            "should pass with canonical-only data: {validation:?}"
        );
    }

    #[test]
    fn test_sortition_validate_detects_fabricated_canonical_set() {
        // Destination claims a sortition_id that doesn't exist in source snapshots.
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        insert_snapshot(&conn, "sort_0", "bhh_0", 0);
        insert_epoch(&conn, 0, 1);
        drop(conn);

        // Destination claims sort_0 AND sort_fake as canonical.
        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0", "sort_fake"]);

        let _stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        assert!(
            !validation.canonical_set_in_source,
            "fabricated sortition_id should be detected"
        );
        assert!(!validation.is_valid(), "validation must fail");
    }

    #[test]
    fn test_sortition_optional_table_asymmetry() {
        // Source has snapshot_burn_distributions but destination doesn't
        // (e.g., table was created in source but clone_optional_schemas
        // somehow didn't create it in destination). Validation should
        // report Some(false).
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src_sort.sqlite");
        let conn = create_sortition_source_db(&src_path);

        // Add the optional table to source.
        conn.execute_batch(
            "CREATE TABLE snapshot_burn_distributions (
                sortition_id TEXT PRIMARY KEY,
                data TEXT NOT NULL
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshot_burn_distributions (sortition_id, data) \
             VALUES ('sort_0', 'dist_data')",
            [],
        )
        .unwrap();

        insert_snapshot(&conn, "sort_0", "bhh_0", 0);
        insert_epoch(&conn, 0, 1);
        drop(conn);

        let dst_path = dir.path().join("dst_sort.sqlite");
        create_sortition_dest_db(&dst_path, &["sort_0"]);

        // Do the copy — this should copy snapshot_burn_distributions.
        let _stats =
            copy_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();

        // Validation should pass with the table present in both.
        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();
        assert_eq!(
            validation.snapshot_burn_distributions_match,
            Some(true),
            "should match when present in both"
        );
        assert!(validation.is_valid(), "should pass: {validation:?}");

        // Now drop the table from destination to simulate asymmetry.
        {
            let conn = Connection::open(&dst_path).unwrap();
            conn.execute_batch("DROP TABLE snapshot_burn_distributions")
                .unwrap();
        }

        let validation =
            validate_sortition_side_tables(src_path.to_str().unwrap(), dst_path.to_str().unwrap())
                .unwrap();
        assert_eq!(
            validation.snapshot_burn_distributions_match,
            Some(false),
            "should detect table present in source but not dest"
        );
        assert!(
            !validation.is_valid(),
            "asymmetric optional table must fail"
        );
    }
}
