use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use blockstack_lib::chainstate::stacks::index::ClarityMarfTrieId;
use chrono::NaiveDateTime;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::Binary;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::StacksBlockId;

use crate::metrics::BlockMetrics;
use crate::{BlockSummary, Network, ResolveEpochFromHeight};

pub mod models;
pub mod schema;

// This macro embeds the SQL files from the "migrations" directory into the binary
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

const MERGE_STAGING_SQL: &str = include_str!("merge_staging.sql");

pub struct AppDb {
    conn: SqliteConnection,
}

impl AppDb {
    /// Default filename for the app database.
    pub const DEFAULT_DB_FILENAME: &'static str = "stacks-bench.db";

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        // Diesel requires a &str for the connection URL.
        // to_str() returns None if the path contains invalid UTF-8.
        let database_url = path_ref
            .to_str()
            .ok_or_else(|| anyhow!("Invalid database path (non-UTF8): {:?}", path_ref))?;

        let mut conn = SqliteConnection::establish(database_url)
            .with_context(|| format!("Failed to connect to app DB at {}", database_url))?;

        // Use WAL mode
        diesel::sql_query("PRAGMA journal_mode=WAL").execute(&mut conn)?;

        // 1. Run Migrations (Create tables if they don't exist)
        // This will automatically apply the SQL defined in step 2
        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow!("Failed to run database migrations: {}", e))?;

        // 2. Ensure foreign keys are enforced (SQLite defaults to OFF)
        diesel::sql_query("PRAGMA foreign_keys = ON").execute(&mut conn)?;

        Ok(AppDb { conn })
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        diesel::sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut self.conn)
            .context("Failed to perform WAL checkpoint")?;
        Ok(())
    }

    pub fn vacuum(&mut self) -> Result<()> {
        diesel::sql_query("VACUUM")
            .execute(&mut self.conn)
            .context("Failed to vacuum the database")?;
        Ok(())
    }

    /// Maps the Network enum to the static IDs defined in the initial migration.
    /// 1=mainnet, 2=testnet, 3=regtest
    fn resolve_network_id(network: Network) -> i32 {
        match network {
            Network::Mainnet => models::Network::MAINNET,
            Network::Testnet => models::Network::TESTNET,
            Network::Regtest => models::Network::REGTEST,
        }
    }

    pub fn get_or_create_chainstate(
        &mut self,
        network: Network,
        chain_id: u32,
        tip_block_id: &StacksBlockId,
        tip_height: u64,
        source_epochs: &[crate::db::node::sortition::models::Epoch],
    ) -> Result<(models::Chainstate, Vec<models::Epoch>)> {
        use self::schema::{chainstate, epoch};

        let network_id = Self::resolve_network_id(network);
        let chain_id_i32 = chain_id.try_into()?;
        let tip_height_i32 = tip_height.try_into()?;

        // 1. Compute the configuration hash
        let epochs_hash = Self::compute_epochs_hash(source_epochs);

        self.conn
            .transaction::<_, anyhow::Error, _>(|conn| {
                // 2. Try to find existing chainstate with matching config
                let existing = chainstate::dsl::chainstate
                    .filter(chainstate::dsl::network_id.eq(network_id))
                    .filter(chainstate::dsl::chain_id.eq(chain_id_i32))
                    .filter(chainstate::dsl::tip_index_hash.eq(tip_block_id.as_bytes()))
                    .filter(chainstate::dsl::epochs_hash.eq(&epochs_hash))
                    .first::<models::Chainstate>(conn)
                    .optional()?;

                if let Some(chainstate) = existing {
                    // 3a. Found it! Load and return associated epochs
                    let epochs = epoch::dsl::epoch
                        .filter(epoch::dsl::chainstate_id.eq(chainstate.id))
                        .order(epoch::dsl::start_height.asc())
                        .load::<models::Epoch>(conn)?;

                    Ok((chainstate, epochs))
                } else {
                    // 3b. Not found! Create new chainstate
                    let new_chainstate = models::NewChainstate {
                        network_id,
                        chain_id: chain_id_i32,
                        tip_index_hash: tip_block_id.0.to_vec(),
                        tip_height: tip_height_i32,
                        epochs_hash,
                    };

                    let chainstate: models::Chainstate =
                        diesel::insert_into(chainstate::dsl::chainstate)
                            .values(&new_chainstate)
                            .get_result(conn)?;

                    // 4. Insert epochs for this new chainstate
                    let new_epochs_data = source_epochs
                        .iter()
                        .map(|e| {
                            Ok(models::NewEpoch {
                                chainstate_id: chainstate.id,
                                stacks_epoch_id: e.epoch_id() as i32,
                                network_epoch_id: e.network_epoch_id() as i32,
                                start_height: e.start_block_height() as i64,
                                end_height: e.end_block_height() as i64,
                                write_length_budget: e.block_limits.write_length.try_into()?,
                                write_count_budget: e.block_limits.write_count.try_into()?,
                                read_length_budget: e.block_limits.read_length.try_into()?,
                                read_count_budget: e.block_limits.read_count.try_into()?,
                                runtime_budget: e.block_limits.runtime.try_into()?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;

                    let epochs: Vec<models::Epoch> = diesel::insert_into(epoch::dsl::epoch)
                        .values(&new_epochs_data)
                        .get_results(conn)?;

                    Ok((chainstate, epochs))
                }
            })
            .context("Failed to get or create chainstate with epochs")
    }

    pub fn get_or_create_burn_block(
        &mut self,
        hash: &[u8],
        height: u32,
    ) -> Result<models::BurnBlock> {
        use self::schema::burn_block::dsl;

        let height_i64: i64 = height.into();

        self.conn
            .transaction(|conn| {
                if let Some(existing) = dsl::burn_block
                    .filter(dsl::block_hash.eq(hash))
                    .first::<models::BurnBlock>(conn)
                    .optional()?
                {
                    Ok(existing)
                } else {
                    let new_burn = models::NewBurnBlock {
                        block_hash: hash.to_vec(),
                        height: height_i64,
                    };
                    diesel::insert_into(dsl::burn_block)
                        .values(&new_burn)
                        .get_result(conn)
                }
            })
            .context("Failed to get or create burn_block")
    }

    pub fn get_or_create_stacks_block(
        &mut self,
        block: &BlockSummary,
    ) -> Result<models::StacksBlock> {
        use self::schema::stacks_block::dsl;

        let height_i64: i64 = block.height.try_into()?;

        // 1. Resolve Burn Block ID
        let burn_hash = block
            .burn_block_hash
            .as_ref()
            .ok_or_else(|| anyhow!("Block {} missing burn block hash", block.id))?;
        let burn_height = block
            .burn_block_height
            .ok_or_else(|| anyhow!("Block {} missing burn block height", block.id))?;

        let burn_block = self.get_or_create_burn_block(&burn_hash.0, burn_height)?;

        // 2. Try to resolve Parent ID (if it exists in DB)
        // We do this inside the transaction to ensure consistency
        self.conn
            .transaction(|conn| {
                // Check if block already exists
                if let Some(existing) = dsl::stacks_block
                    .filter(dsl::index_hash.eq(block.id.as_bytes()))
                    .first::<models::StacksBlock>(conn)
                    .optional()?
                {
                    return Ok(existing);
                }

                // Try to find parent by hash
                let parent_id_val = dsl::stacks_block
                    .select(dsl::id)
                    .filter(dsl::index_hash.eq(block.parent_id.as_bytes()))
                    .first::<i64>(conn)
                    .optional()?;

                let new_block = models::NewStacksBlock {
                    index_hash: block.id.0.to_vec(),
                    height: height_i64,
                    parent_stacks_block_id: parent_id_val,
                    burn_block_id: burn_block.id,
                };

                diesel::insert_into(dsl::stacks_block)
                    .values(&new_block)
                    .get_result(conn)
            })
            .context("Failed to get or create stacks_block")
    }

    pub fn get_or_create_stacks_tx(
        &mut self,
        block_id: i64,
        hash: &[u8],
        type_str: &str,
    ) -> Result<models::StacksTx> {
        use self::schema::stacks_tx::dsl::*;

        self.conn
            .transaction(|conn| {
                if let Some(existing) = stacks_tx
                    .filter(tx_hash.eq(hash))
                    .first::<models::StacksTx>(conn)
                    .optional()?
                {
                    Ok(existing)
                } else {
                    let new_tx = models::NewStacksTx {
                        stacks_block_id: block_id,
                        tx_hash: hash.to_vec(),
                        tx_type: type_str.to_string(),
                    };
                    diesel::insert_into(stacks_tx)
                        .values(&new_tx)
                        .get_result(conn)
                }
            })
            .context("Failed to get or create stacks_tx")
    }

    pub fn create_benchmark_run(
        &mut self,
        new_run: models::NewBenchmarkRun,
    ) -> Result<models::BenchmarkRun> {
        use self::schema::benchmark_run::dsl::*;

        diesel::insert_into(benchmark_run)
            .values(&new_run)
            .get_result(&mut self.conn)
            .context("Failed to create benchmark run")
    }

    pub fn finish_benchmark_run(&mut self, run_id: i32, end_ts: NaiveDateTime) -> Result<()> {
        use self::schema::benchmark_run::dsl::*;

        diesel::update(benchmark_run.find(run_id))
            .set(end_time.eq(end_ts))
            .execute(&mut self.conn)
            .context("Failed to update benchmark run end time")?;
        Ok(())
    }

    pub fn insert_stacks_block_stats(
        &mut self,
        stats: &[models::NewStacksBlockStats],
    ) -> Result<()> {
        use self::schema::stacks_block_stats::dsl::*;

        diesel::insert_into(stacks_block_stats)
            .values(stats)
            .execute(&mut self.conn)
            .context("Failed to insert stacks block stats")?;
        Ok(())
    }

    pub fn insert_stacks_tx_stats(&mut self, stats: &[models::NewStacksTxStats]) -> Result<()> {
        use self::schema::stacks_tx_stats::dsl::*;

        // Chunking to avoid SQLite variable limit issues with large batches
        for chunk in stats.chunks(500) {
            diesel::insert_into(stacks_tx_stats)
                .values(chunk)
                .execute(&mut self.conn)
                .context("Failed to insert stacks tx stats chunk")?;
        }
        Ok(())
    }

    /// Retrieves the internal DB ID for a Stacks block by its hash.
    pub fn get_stacks_block_id(&mut self, block_id: &StacksBlockId) -> Result<i64> {
        use self::schema::stacks_block::dsl;
        dsl::stacks_block
            .select(dsl::id)
            .filter(dsl::index_hash.eq(block_id.as_bytes()))
            .first(&mut self.conn)
            .optional()?
            .ok_or_else(|| anyhow!("Stacks block not found in DB"))
    }

    /// Saves execution metrics for a block and its transactions.
    /// Assumes the block and transactions have already been indexed.
    pub fn save_block_metrics(
        &mut self,
        run_id: i32,
        block_id: &StacksBlockId,
        metrics: &BlockMetrics,
    ) -> Result<()> {
        use self::schema::stacks_tx::dsl as tx_dsl;

        // 1. Get Block ID
        let block_id = self.get_stacks_block_id(block_id)?;

        // 2. Insert Block Stats
        let block_stats = models::NewStacksBlockStats {
            benchmark_run_id: run_id,
            stacks_block_id: block_id,
            total_duration_us: metrics.total_duration.as_micros() as i32,
            setup_duration_us: metrics.setup_duration.as_micros() as i32,
            execution_duration_us: metrics.execution_duration.as_micros() as i32,
            commit_duration_us: metrics.commit_duration.as_micros() as i32,
            commit_overhead_baseline_us: metrics.commit_overhead_baseline.as_micros() as i32,
            clarity_write_length: metrics.total_clarity_cost.write_length as i32,
            clarity_write_count: metrics.total_clarity_cost.write_count as i32,
            clarity_read_length: metrics.total_clarity_cost.read_length as i32,
            clarity_read_count: metrics.total_clarity_cost.read_count as i32,
            clarity_runtime: metrics.total_clarity_cost.runtime as i32,
        };
        self.insert_stacks_block_stats(&[block_stats])?;

        // 3. Insert Tx Stats
        // Optimization: Fetch all tx IDs for this block in one query to avoid N lookups
        let tx_map: HashMap<Vec<u8>, i64> = tx_dsl::stacks_tx
            .select((tx_dsl::tx_hash, tx_dsl::id))
            .filter(tx_dsl::stacks_block_id.eq(block_id))
            .load::<(Vec<u8>, i64)>(&mut self.conn)?
            .into_iter()
            .collect();

        let mut tx_stats_batch = Vec::with_capacity(metrics.transactions.len());
        for tx_metric in &metrics.transactions {
            let tx_hash_bytes =
                hex::decode(&tx_metric.txid).map_err(|e| anyhow!("Invalid hex in txid: {}", e))?;

            if let Some(&tx_id) = tx_map.get(&tx_hash_bytes) {
                tx_stats_batch.push(models::NewStacksTxStats {
                    benchmark_run_id: run_id,
                    stacks_tx_id: tx_id,
                    duration_us: tx_metric.duration.as_micros() as i32,
                    estimated_commit_impact_us: tx_metric.estimated_commit_impact.as_micros()
                        as i32,
                    clarity_write_length: tx_metric.cost.write_length as i32,
                    clarity_write_count: tx_metric.cost.write_count as i32,
                    clarity_read_length: tx_metric.cost.read_length as i32,
                    clarity_read_count: tx_metric.cost.read_count as i32,
                    clarity_runtime: tx_metric.cost.runtime as i32,
                });
            }
        }

        if !tx_stats_batch.is_empty() {
            self.insert_stacks_tx_stats(&tx_stats_batch)?;
        }

        Ok(())
    }

    /// Retrieves the ordered list of block IDs for the canonical chain segment.
    /// This is lightweight and suitable for driving a lazy iterator.
    pub fn get_chain_block_ids(
        &mut self,
        tip_index_hash: &StacksBlockId,
        start_height: u32,
        end_height: u32,
    ) -> Result<Vec<StacksBlockId>> {
        use diesel::sql_query;
        use diesel::sql_types::{BigInt, Binary};

        // Recursive CTE to walk backwards from tip, then order ascending
        let query = r#"
            WITH RECURSIVE chain(index_hash, height, parent_stacks_block_id) AS (
                SELECT index_hash, height, parent_stacks_block_id
                FROM stacks_block
                WHERE index_hash = ?
                UNION ALL
                SELECT p.index_hash, p.height, p.parent_stacks_block_id
                FROM stacks_block p
                INNER JOIN chain c ON c.parent_stacks_block_id = p.id
                WHERE c.height > ?
            )
            SELECT index_hash
            FROM chain
            WHERE height <= ? AND height >= ?
            ORDER BY height ASC
        "#;

        #[derive(Debug, QueryableByName)]
        struct RawId {
            #[diesel(sql_type = Binary)]
            index_hash: Vec<u8>,
        }

        let raw_ids: Vec<RawId> = sql_query(query)
            .bind::<Binary, _>(tip_index_hash.as_bytes())
            .bind::<BigInt, _>(start_height as i64)
            .bind::<BigInt, _>(end_height as i64)
            .bind::<BigInt, _>(start_height as i64)
            .load(&mut self.conn)
            .context("Failed to query chain block IDs")?;

        let ids = raw_ids
            .into_iter()
            .map(|r| {
                StacksBlockId::from_vec(&r.index_hash)
                    .ok_or_else(|| anyhow!("Invalid hash in DB: {:?}", r.index_hash))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ids)
    }

    /// Fetches a single block summary by ID, resolving parent and burn info.
    pub fn get_block<EpochResolver>(
        &mut self,
        id: &StacksBlockId,
        epochs: &EpochResolver,
    ) -> Result<BlockSummary>
    where
        EpochResolver: ResolveEpochFromHeight + ?Sized,
    {
        use diesel::dsl::sql;

        use self::schema::{burn_block, stacks_block};

        // We need the block, its burn info, and its parent's hash.
        // We use a subselect for the parent hash to avoid a complex self-join alias in Diesel DSL
        let (height, burn_hash, burn_height, parent_hash) = stacks_block::dsl::stacks_block
            .inner_join(burn_block::dsl::burn_block)
            .select((
                stacks_block::dsl::height,
                burn_block::dsl::block_hash,
                burn_block::dsl::height,
                sql::<diesel::sql_types::Nullable<Binary>>("(SELECT index_hash FROM stacks_block p WHERE p.id = stacks_block.parent_stacks_block_id)"),
            ))
            .filter(stacks_block::dsl::index_hash.eq(id.as_bytes()))
            .first::<(i64, Vec<u8>, i64, Option<Vec<u8>>)>(&mut self.conn)
            .optional()?
            .ok_or_else(|| anyhow!("Block {} not found in App DB", id))?;

        let parent_id = if let Some(ph) = parent_hash {
            StacksBlockId::from_vec(&ph)
                .ok_or_else(|| anyhow!("Invalid parent hash in DB for block {id}: {ph:?}"))?
        } else {
            StacksBlockId::sentinel()
        };

        let epoch = epochs.resolve_stacks_epoch(height as u64).ok_or_else(|| {
            anyhow!(
                "Could not resolve epoch for block {} at height {}",
                id,
                height
            )
        })?;

        let burn_hash_obj =
            clarity::types::chainstate::BurnchainHeaderHash::from_vec(&burn_hash)
                .ok_or_else(|| anyhow!("Invalid burn hash in DB for block {id}: {burn_hash:?}"))?;

        Ok(
            BlockSummary::new(id.clone(), parent_id, height as u64, epoch)
                .with_burn_info(burn_height as u32, burn_hash_obj),
        )
    }

    pub fn index_blocks_streaming<I>(&mut self, blocks: I) -> Result<()>
    where
        I: IntoIterator<Item = BlockSummary>,
    {
        use self::models::{StagedStacksBlock, StagedStacksTx};
        use self::schema::{_staged_stacks_block, _staged_stacks_tx};

        // 1. Process Iterator in Chunks
        const CHUNK_SIZE: usize = 1000;
        let mut block_count: u64 = 0;
        let mut tx_count: u64 = 0;
        let mut block_buffer = Vec::with_capacity(CHUNK_SIZE);
        let mut tx_buffer = Vec::with_capacity(CHUNK_SIZE * 2000);

        let flush = |conn: &mut SqliteConnection,
                     blocks: &mut Vec<StagedStacksBlock>,
                     txs: &mut Vec<StagedStacksTx>|
         -> Result<()> {
            println!(
                "Flushing {} blocks and {} txs to staging tables",
                blocks.len(),
                txs.len()
            );
            if !blocks.is_empty() {
                diesel::insert_into(_staged_stacks_block::table)
                    .values(&*blocks)
                    .execute(conn)?;
                blocks.clear();
            }
            if !txs.is_empty() {
                for chunk in txs.chunks(2000) {
                    diesel::insert_into(_staged_stacks_tx::table)
                        .values(chunk)
                        .execute(conn)?;
                }
                txs.clear();
            }
            Ok(())
        };

        self.conn.transaction::<_, anyhow::Error, _>(|conn| {
            // 0. Truncate Staging Tables (Clean start)
            println!("Truncating staging tables");
            // Diesel optimizes delete without filter to TRUNCATE or equivalent
            diesel::delete(_staged_stacks_block::table).execute(conn)?;
            diesel::delete(_staged_stacks_tx::table).execute(conn)?;

            println!("Beginning block indexing");
            for block in blocks {
                block_count += 1;
                let burn_hash = block
                    .burn_block_hash
                    .as_ref()
                    .ok_or_else(|| anyhow!("Block {} missing burn block hash", block.id))?;
                let burn_height = block
                    .burn_block_height
                    .ok_or_else(|| anyhow!("Block {} missing burn block height", block.id))?;

                block_buffer.push(StagedStacksBlock {
                    index_hash: block.id.0.to_vec(),
                    parent_index_hash: block.parent_id.0.to_vec(),
                    height: block.height as i64,
                    burn_block_hash: burn_hash.0.to_vec(),
                    burn_block_height: burn_height as i64,
                });

                if let Some(txs) = block.transactions() {
                    for tx in txs {
                        tx_count += 1;
                        tx_buffer.push(StagedStacksTx {
                            block_index_hash: block.id.0.to_vec(),
                            tx_hash: tx.txid().0.to_vec(),
                            tx_type: "unknown".to_string(),
                        });
                    }
                }

                if block_buffer.len() >= CHUNK_SIZE {
                    flush(conn, &mut block_buffer, &mut tx_buffer)?;
                }
            }
            // Final flush
            flush(conn, &mut block_buffer, &mut tx_buffer)?;

            // 3. Execute Merge Logic
            // We invoke the SQL script as if it were a stored procedure
            println!("Merging staged data into main tables");
            conn.batch_execute(MERGE_STAGING_SQL)
                .context("Failed to execute merge_staging.sql")?;

            // 4. Cleanup (Truncate again to save space)
            println!("Cleaning up staging tables");
            let deleted_staging_blocks =
                diesel::delete(_staged_stacks_block::table).execute(conn)?;
            let deleted_staging_transactions =
                diesel::delete(_staged_stacks_tx::table).execute(conn)?;
            println!(
                "Deleted {} staged blocks and {} staged transactions",
                deleted_staging_blocks, deleted_staging_transactions
            );

            println!(
                "Indexing complete: {} blocks and {} txs indexed",
                block_count, tx_count
            );
            Ok(())
        })
    }

    /// Computes a deterministic hash of the epoch configuration.
    fn compute_epochs_hash(epochs: &[crate::db::node::sortition::models::Epoch]) -> Vec<u8> {
        let mut hasher = Sha256::new();
        // Sort by start height to ensure determinism
        let mut sorted = epochs.to_vec();
        sorted.sort_by_key(|e| e.start_block_height());

        for epoch in sorted {
            hasher.update(epoch.epoch_id().to_le_bytes());
            hasher.update(epoch.start_block_height().to_le_bytes());
            hasher.update(epoch.end_block_height().to_le_bytes());
            hasher.update(epoch.block_limits.write_length.to_le_bytes());
            hasher.update(epoch.block_limits.write_count.to_le_bytes());
            hasher.update(epoch.block_limits.read_length.to_le_bytes());
            hasher.update(epoch.block_limits.read_count.to_le_bytes());
            hasher.update(epoch.block_limits.runtime.to_le_bytes());
        }
        hasher.finalize().to_vec()
    }
}
