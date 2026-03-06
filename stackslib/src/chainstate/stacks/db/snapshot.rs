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
fn clone_schemas_from_source(conn: &Connection) -> Result<(), Error> {
    // Collect all CREATE TABLE and CREATE INDEX statements for the required tables.
    let mut stmts: Vec<String> = Vec::new();

    for table in REQUIRED_TABLES {
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
    if let Err(e) = clone_schemas_from_source(&conn) {
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
}
