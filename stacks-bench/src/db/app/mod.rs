use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};
use chrono::NaiveDateTime;
use diesel::sql_types::{BigInt, Binary};
use diesel::upsert::excluded;
use diesel::{
    ExpressionMethods as _, JoinOnDsl as _, NullableExpressionMethods as _, OptionalExtension as _,
    QueryDsl as _, QueryableByName, SelectableHelper as _, SqliteConnection, sql_query,
};
use diesel_async::pooled_connection::bb8::{Pool, PooledConnection};
use diesel_async::pooled_connection::{AsyncDieselConnectionManager, ManagerConfig};
use diesel_async::sync_connection_wrapper::SyncConnectionWrapper;
use diesel_async::{
    AsyncConnection, AsyncMigrationHarness, RunQueryDsl as _, SimpleAsyncConnection,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use futures::FutureExt;
use futures::future::BoxFuture;
use models::*;
use schema::*;
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::StacksBlockId;
use tokio::sync::RwLock;

use crate::blocks::{BlockHeaderProvider, BlockRef, ChainCache};
use crate::metrics::BlockMetrics;
use crate::{StacksBlockHeader, StacksEpoch};

pub mod models;
pub mod schema;
mod util;

use super::{AsyncSqliteConnection, SqlitePool};

//type AsyncSqliteConnection = SyncConnectionWrapper<SqliteConnection>;
//type SqlitePool = Pool<AsyncSqliteConnection>;

// This macro embeds the SQL files from the "migrations" directory into the binary
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

const MERGE_STAGING_SQL: &str = include_str!("scripts/merge_staging.sql");
const POST_RUN_SQL: &str = include_str!("scripts/post_run.sql");
const MERGE_PROFILER_KV_SQL: &str = include_str!("scripts/merge_profiler_kv.sql");
const MERGE_PROFILER_CLARITY_COSTS_SQL: &str =
    include_str!("scripts/merge_profiler_clarity_costs.sql");

#[derive(Debug, Clone, Copy)]
pub enum SynchronizationMode {
    Normal,
    Off,
}

#[derive(Debug, Clone, Copy)]
pub enum ForeignKeyMode {
    Enforced,
    Off,
}

#[derive(Debug, Clone, Copy)]
pub enum CheckpointMode {
    Full,
    Truncate,
    Restart,
    Passive,
}

#[derive(Debug, QueryableByName)]
struct CheckpointResult {
    #[diesel(sql_type = BigInt)]
    busy: i64,
    #[diesel(sql_type = BigInt)]
    log: i64,
    #[diesel(sql_type = BigInt)]
    checkpointed: i64,
}

#[derive(Clone)]
pub struct AppDb {
    pool: SqlitePool,
    /// Cache of profiler span name to ID mappings.
    profiler_span_cache: Arc<RwLock<HashMap<(Option<&'static str>, &'static str), i32>>>,
    /// Cache of profiler location (file,line) to ID mappings.
    profiler_loc_cache: Arc<RwLock<HashMap<(String, i32), i32>>>,
    /// Cache of profiler tag string to ID mappings.
    profiler_tag_cache: Arc<RwLock<HashMap<&'static str, i32>>>,
}

impl AppDb {
    /// Default filename for the app database.
    pub const DEFAULT_DB_FILENAME: &'static str = "stacks-bench.db";

    /// Helper to apply standard PRAGMAs to a connection.
    /// Used by both the connection pool and the standalone migration connection.
    async fn setup_connection(
        conn: &mut AsyncSqliteConnection,
    ) -> Result<(), diesel::ConnectionError> {
        sql_query("PRAGMA page_size = 8192;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA page_size=8192: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA wal_autocheckpoint = 0;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA wal_autocheckpoint=0: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA journal_mode=WAL")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA journal_mode=WAL: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA locking_mode = NORMAL;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA locking_mode=NORMAL: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA synchronous = NORMAL;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA synchronous=NORMAL: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA temp_store = MEMORY;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA temp_store=MEMORY: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA cache_size = -262144;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA cache_size=-262144: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA mmap_size = 30000000000;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA mmap_size=30000000000: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        sql_query("PRAGMA foreign_keys = ON")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA foreign_keys=ON: {}", e))
            .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
        Ok(())
    }

    async fn run_migrations(database_url: &str) -> Result<()> {
        // Establish a standalone connection to avoid lifetime issues with the pool
        let mut conn = SyncConnectionWrapper::<SqliteConnection>::establish(database_url)
            .await
            .context("Failed to establish dedicated connection for migrations")?;

        // Apply the same setup to ensure consistency (e.g. page_size, WAL)
        Self::setup_connection(&mut conn)
            .await
            .map_err(anyhow::Error::new)
            .context("Failed to configure migration connection")?;

        // AsyncMigrationHarness consumes the connection and gives it back
        let mut harness = AsyncMigrationHarness::new(conn);

        // Using your existing embedded migrations constant
        let pending = harness
            .pending_migrations(MIGRATIONS)
            .map_err(anyhow::Error::from_boxed)
            .context("Failed to check pending migrations")?;

        if pending.is_empty() {
            // println!("App DB is up to date at {}", database_url);
        } else {
            // println!(
            //     "App DB at {database_url} has {} pending migrations. Applying...",
            //     pending.len()
            // );

            harness
                .run_pending_migrations(MIGRATIONS)
                .map_err(anyhow::Error::from_boxed)
                .context("Failed to run migrations")?;

            // println!("Database migration(s) complete");
        }

        Ok(())
    }

    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let database_url = path_ref
            .to_str()
            .ok_or_else(|| anyhow!("Invalid database path (non-UTF8): {path_ref:?}"))?;

        if let Some(parent) = path_ref.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory at {:?}", parent))?;
        }

        Self::run_migrations(database_url).await?;

        // Manager config that sets up each new connection with our pragmas.
        let mut manager_config: ManagerConfig<AsyncSqliteConnection> = ManagerConfig::default();

        manager_config.custom_setup = Box::new(|url: &str| {
            // The callback must return a BoxFuture<ConnectionResult<C>>
            Box::pin(async move {
                // Open an async SQLite connection wrapped in SyncConnectionWrapper
                let mut conn = SyncConnectionWrapper::<SqliteConnection>::establish(url).await?;
                // Apply PRAGMAs
                Self::setup_connection(&mut conn).await?;

                Ok(conn)
            })
        });

        // Build connection manager and pool
        let manager = AsyncDieselConnectionManager::<AsyncSqliteConnection>::new_with_config(
            database_url.to_owned(),
            manager_config,
        );

        let pool = Pool::builder()
            .max_size(64)
            .retry_connection(false)
            .build(manager)
            .await
            .with_context(|| format!("Failed to build SQLite pool for {database_url}"))?;

        Ok(AppDb {
            pool,
            profiler_span_cache: Arc::new(RwLock::new(HashMap::new())),
            profiler_loc_cache: Arc::new(RwLock::new(HashMap::new())),
            profiler_tag_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    async fn get_conn(&self) -> Result<PooledConnection<'_, AsyncSqliteConnection>> {
        let conn = self
            .pool
            .get()
            .await
            .context("Failed to get connection from AppDb pool")?;
        Ok(conn)
    }

    pub async fn set_synchronization_mode(&mut self, mode: SynchronizationMode) -> Result<()> {
        let pragma_sql = match mode {
            SynchronizationMode::Normal => "PRAGMA synchronous = NORMAL;",
            SynchronizationMode::Off => "PRAGMA synchronous = OFF;",
        };
        sql_query(pragma_sql)
            .execute(&mut self.get_conn().await?)
            .await
            .with_context(|| format!("Failed to set synchronization mode: {}", pragma_sql))?;
        Ok(())
    }

    pub async fn set_foreign_key_enforcement(&mut self, mode: ForeignKeyMode) -> Result<()> {
        let pragma_sql = match mode {
            ForeignKeyMode::Enforced => "PRAGMA foreign_keys = ON;",
            ForeignKeyMode::Off => "PRAGMA foreign_keys = OFF;",
        };
        sql_query(pragma_sql)
            .execute(&mut self.get_conn().await?)
            .await
            .with_context(|| {
                format!("Failed to set foreign key enforcement mode: {}", pragma_sql)
            })?;
        Ok(())
    }

    pub async fn checkpoint(&mut self, mode: CheckpointMode) -> Result<()> {
        let sql = match mode {
            CheckpointMode::Full => "PRAGMA wal_checkpoint(FULL)",
            CheckpointMode::Truncate => "PRAGMA wal_checkpoint(TRUNCATE)",
            CheckpointMode::Restart => "PRAGMA wal_checkpoint(RESTART)",
            CheckpointMode::Passive => "PRAGMA wal_checkpoint(PASSIVE)",
        };

        let conn = &mut self.get_conn().await?;
        let res: CheckpointResult = sql_query(sql)
            .get_result(conn)
            .await
            .with_context(|| format!("Failed to run checkpoint with mode {:?}", mode))?;

        if res.busy != 0 || res.log != res.checkpointed {
            return Err(anyhow!("Checkpoint incomplete: {:?}", res));
        }
        Ok(())
    }

    pub async fn vacuum(&mut self) -> Result<()> {
        diesel::sql_query("VACUUM")
            .execute(&mut self.get_conn().await?)
            .await
            .context("Failed to vacuum the database")?;
        Ok(())
    }

    /// Maps the Network enum to the static IDs defined in the initial migration.
    /// 1=mainnet, 2=testnet, 3=regtest
    fn resolve_network_id(network: crate::Network) -> i32 {
        match network {
            crate::Network::Mainnet => Network::MAINNET,
            crate::Network::Testnet => Network::TESTNET,
            crate::Network::Regtest => Network::REGTEST,
        }
    }

    pub async fn get_or_create_chainstate(
        &mut self,
        network: crate::Network,
        chain_id: u32,
        chain_tip: &BlockRef,
        source_epochs: &[StacksEpoch],
    ) -> Result<(Chainstate, Vec<Epoch>)> {
        let network_id = Self::resolve_network_id(network);

        // CHANGE: Explicitly type these as i64 to match schema::BigInt.
        // chain_id is u32, so .into() is safe.
        let chain_id_val: i64 = chain_id.into();
        // tip_height is u64, so .try_into() is needed.
        let tip_height_val: i64 = chain_tip.height.try_into()?;

        // 1. Compute the configuration hash
        let epochs_hash = Self::compute_epochs_hash(source_epochs);

        let conn = &mut self.get_conn().await?;

        conn.transaction::<_, anyhow::Error, _>(|conn| {
            Box::pin(async {
                // 2. Try to find existing chainstate with matching config
                let existing = chainstate::table
                    .filter(chainstate::network_id.eq(network_id))
                    .filter(chainstate::chain_id.eq(chain_id_val))
                    .filter(chainstate::tip_index_hash.eq(chain_tip.id.as_bytes()))
                    .filter(chainstate::epochs_hash.eq(&epochs_hash))
                    .first::<Chainstate>(conn)
                    .await
                    .optional()?;

                if let Some(chainstate) = existing {
                    // 3a. Found it! Load and return associated epochs
                    let epochs = epoch::table
                        .filter(epoch::chainstate_id.eq(chainstate.id))
                        .order(epoch::start_height.asc())
                        .load::<Epoch>(conn)
                        .await?;

                    Ok((chainstate, epochs))
                } else {
                    // 3b. Not found! Create new chainstate
                    let chainstate: Chainstate = diesel::insert_into(chainstate::table)
                        .values((
                            chainstate::network_id.eq(network_id),
                            chainstate::chain_id.eq(chain_id_val),
                            chainstate::tip_index_hash.eq(chain_tip.id.0.to_vec()),
                            chainstate::tip_height.eq(tip_height_val),
                            chainstate::epochs_hash.eq(epochs_hash),
                        ))
                        .get_result(conn)
                        .await?;

                    // 4. Insert epochs for this new chainstate
                    let mut epochs = Vec::with_capacity(source_epochs.len());
                    for e in source_epochs {
                        let epoch_entry = diesel::insert_into(epoch::table)
                            .values((
                                epoch::chainstate_id.eq(chainstate.id),
                                epoch::stacks_epoch_id.eq(e.epoch_id() as i32),
                                epoch::network_epoch_id.eq(e.network_epoch_id as i32),
                                epoch::start_height.eq(e.start_block_height() as i64),
                                epoch::end_height.eq(e.end_block_height() as i64),
                                epoch::write_length_budget
                                    .eq(TryInto::<i64>::try_into(e.write_length_budget)?),
                                epoch::write_count_budget
                                    .eq(TryInto::<i64>::try_into(e.write_count_budget)?),
                                epoch::read_length_budget
                                    .eq(TryInto::<i64>::try_into(e.read_length_budget)?),
                                epoch::read_count_budget
                                    .eq(TryInto::<i64>::try_into(e.read_count_budget)?),
                                epoch::runtime_budget
                                    .eq(TryInto::<i64>::try_into(e.runtime_budget)?),
                            ))
                            .get_result::<Epoch>(conn)
                            .await?;
                        epochs.push(epoch_entry);
                    }

                    Ok((chainstate, epochs))
                }
            })
        })
        .await
        .context("Failed to get or create chainstate with epochs")
    }

    pub async fn get_or_create_burn_block(
        &mut self,
        hash: &[u8],
        height: u32,
    ) -> Result<BurnBlock> {
        let height_i64: i64 = height.into();

        // Optimization: Use Upsert with dummy update to always return the row
        diesel::insert_into(burn_block::table)
            .values((
                burn_block::block_hash.eq(hash.to_vec()),
                burn_block::height.eq(height_i64),
            ))
            .on_conflict(burn_block::block_hash)
            .do_update()
            .set(burn_block::height.eq(height_i64)) // Dummy update (or actual update if height changed)
            .get_result(&mut self.get_conn().await?)
            .await
            .context("Failed to get or create burn_block")
    }

    pub async fn create_benchmark_run(
        &mut self,
        chainstate_id: i32,
        start_time: NaiveDateTime,
        git_commit_hash: Vec<u8>,
        run_name: Option<String>,
        args_json: String,
    ) -> Result<BenchmarkRun> {
        diesel::insert_into(benchmark_run::table)
            .values((
                benchmark_run::chainstate_id.eq(chainstate_id),
                benchmark_run::start_time.eq(start_time),
                benchmark_run::git_commit_hash.eq(git_commit_hash),
                benchmark_run::run_name.eq(run_name),
                benchmark_run::args_json.eq(args_json),
            ))
            .get_result(&mut self.get_conn().await?)
            .await
            .context("Failed to create benchmark run")
    }

    pub async fn finish_benchmark_run(&mut self, run_id: i32, end_ts: NaiveDateTime) -> Result<()> {
        diesel::update(benchmark_run::table.find(run_id))
            .set(benchmark_run::end_time.eq(end_ts))
            .execute(&mut self.get_conn().await?)
            .await
            .context("Failed to update benchmark run end time")?;

        // inside finish_benchmark_run, after end_time update:
        let sql = POST_RUN_SQL.replace("?1", &run_id.to_string());
        self.get_conn()
            .await?
            .batch_execute(&sql)
            .await
            .context("Failed to build profiler span summary")?;
        Ok(())
    }

    /// Looks up a benchmark run by its ID, returning `None` if it doesn't exist.
    pub async fn get_benchmark_run(&self, run_id: i32) -> Result<Option<BenchmarkRun>> {
        benchmark_run::table
            .find(run_id)
            .first::<BenchmarkRun>(&mut self.get_conn().await?)
            .await
            .optional()
            .context("Failed to look up benchmark run")
    }

    /// Lists all benchmark runs, most recent first.
    pub async fn list_benchmark_runs(&self) -> Result<Vec<BenchmarkRun>> {
        benchmark_run::table
            .order(benchmark_run::start_time.desc())
            .load::<BenchmarkRun>(&mut self.get_conn().await?)
            .await
            .context("Failed to list benchmark runs")
    }

    /// Gets a chainstate by ID, returning `None` if it doesn't exist.
    pub async fn get_chainstate(&self, chainstate_id: i32) -> Result<Option<Chainstate>> {
        chainstate::table
            .find(chainstate_id)
            .first::<Chainstate>(&mut self.get_conn().await?)
            .await
            .optional()
            .context("Failed to look up chainstate")
    }

    /// Lists all chainstates ordered by ID.
    pub async fn list_chainstates(&self) -> Result<Vec<Chainstate>> {
        chainstate::table
            .order(chainstate::id.asc())
            .load::<Chainstate>(&mut self.get_conn().await?)
            .await
            .context("Failed to list chainstates")
    }

    /// Gets the network name for a network ID.
    pub async fn get_network_name(&self, network_id: i32) -> Result<String> {
        network::table
            .select(network::name)
            .find(network_id)
            .first::<String>(&mut self.get_conn().await?)
            .await
            .context("Failed to get network name")
    }

    /// Counts benchmark runs associated with a given chainstate.
    pub async fn count_benchmark_runs_for_chainstate(&self, chainstate_id: i32) -> Result<i64> {
        benchmark_run::table
            .filter(benchmark_run::chainstate_id.eq(chainstate_id))
            .count()
            .get_result::<i64>(&mut self.get_conn().await?)
            .await
            .context("Failed to count benchmark runs for chainstate")
    }

    /// Deletes a chainstate and all associated data (benchmark runs, epochs).
    ///
    /// Benchmark runs are deleted first (via [`delete_benchmark_run`]) since the
    /// `benchmark_run.chainstate_id` FK does not cascade. Epochs are deleted
    /// explicitly for the same reason.
    pub async fn delete_chainstate(&mut self, chainstate_id: i32) -> Result<()> {
        // First delete all associated benchmark runs
        let run_ids: Vec<i32> = benchmark_run::table
            .select(benchmark_run::id)
            .filter(benchmark_run::chainstate_id.eq(chainstate_id))
            .load::<i32>(&mut self.get_conn().await?)
            .await
            .context("Failed to list benchmark runs for chainstate")?;

        for run_id in run_ids {
            self.delete_benchmark_run(run_id).await?;
        }

        // Delete epochs and the chainstate row itself
        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async move {
                    diesel::delete(epoch::table.filter(epoch::chainstate_id.eq(chainstate_id)))
                        .execute(conn)
                        .await?;

                    let deleted = diesel::delete(chainstate::table.find(chainstate_id))
                        .execute(conn)
                        .await?;

                    if deleted == 0 {
                        return Err(anyhow!("Chainstate {} not found", chainstate_id));
                    }

                    Ok(())
                })
            })
            .await
            .context("Failed to delete chainstate")
    }

    /// Deletes a benchmark run and all of its dependent data.
    ///
    /// Tables with `ON DELETE CASCADE` (block_processing_baseline, profiler_record,
    /// profiler_record_kv, profiler_record_clarity_costs, profiler_span_block_summary,
    /// profiler_span_summary) are cleaned up automatically by SQLite.
    ///
    /// Tables without cascade (stacks_block_stats, stacks_tx_stats) are deleted
    /// explicitly before the benchmark_run row itself.
    pub async fn delete_benchmark_run(&mut self, run_id: i32) -> Result<()> {
        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async move {
                    // Delete non-cascading dependents first
                    diesel::delete(
                        stacks_tx_stats::table.filter(stacks_tx_stats::benchmark_run_id.eq(run_id)),
                    )
                    .execute(conn)
                    .await?;

                    diesel::delete(
                        stacks_block_stats::table
                            .filter(stacks_block_stats::benchmark_run_id.eq(run_id)),
                    )
                    .execute(conn)
                    .await?;

                    // Delete the run itself (cascades handle the rest)
                    let deleted = diesel::delete(benchmark_run::table.find(run_id))
                        .execute(conn)
                        .await?;

                    if deleted == 0 {
                        return Err(anyhow!("Benchmark run {} not found", run_id));
                    }

                    Ok(())
                })
            })
            .await
            .context("Failed to delete benchmark run")
    }

    /// Retrieves the internal DB ID for a Stacks block by its hash.
    pub async fn get_stacks_block_id(&self, block_id: &StacksBlockId) -> Result<i64> {
        let id_opt = schema::stacks_block::table
            .select(schema::stacks_block::id)
            .filter(schema::stacks_block::index_hash.eq(block_id.as_bytes()))
            .first::<i64>(&mut self.get_conn().await?)
            .await
            .optional()?;

        id_opt.ok_or_else(|| anyhow!("Stacks block not found in DB"))
    }

    /// Retrieves the ordered list of block IDs for the canonical chain segment.
    pub async fn get_chain_block_ids(
        &self,
        tip_index_hash: &StacksBlockId,
        start_height: u64,
        end_height: u64,
    ) -> Result<Vec<StacksBlockId>> {
        // Recursive CTE to walk backwards from tip, then order ascending
        let query = r#"
            WITH RECURSIVE chain(index_hash, height, parent_stacks_block_id) AS (
                SELECT index_hash, height, parent_stacks_block_id
                FROM stacks_block
                WHERE index_hash = ?1
                UNION ALL
                SELECT p.index_hash, p.height, p.parent_stacks_block_id
                FROM stacks_block p
                INNER JOIN chain c ON c.parent_stacks_block_id = p.id
                WHERE c.height > ?2
            )
            SELECT index_hash
            FROM chain
            WHERE height <= ?3 AND height >= ?2
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
            .load(&mut self.get_conn().await?)
            .await
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

    /// Retrieves headers for a chain range, but ONLY if they are fully indexed (txs_indexed = true).
    /// Used to determine if we need to run the indexer pipeline.
    pub async fn get_indexed_chain_block_ids(
        &self,
        tip_index_hash: &StacksBlockId,
        start_height: u64,
        end_height: u64,
    ) -> Result<Vec<StacksBlockId>> {
        // Similar to get_chain_block_ids but filters for txs_indexed = true
        let query = r#"
            WITH RECURSIVE chain(index_hash, height, parent_stacks_block_id) AS (
                SELECT index_hash, height, parent_stacks_block_id
                FROM stacks_block
                WHERE index_hash = ?1 AND txs_indexed = 1
                UNION ALL
                SELECT p.index_hash, p.height, p.parent_stacks_block_id
                FROM stacks_block p
                INNER JOIN chain c ON c.parent_stacks_block_id = p.id
                WHERE c.height > ?2 AND p.txs_indexed = 1
            )
            SELECT index_hash
            FROM chain
            WHERE height <= ?3 AND height >= ?2
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
            .load(&mut self.get_conn().await?)
            .await
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

    /// Retrieves headers for a chain range from AppDb, regardless of txs_indexed status.
    /// Used by the loader to avoid walking the node DB again.
    pub async fn get_chain_headers(
        &self,
        tip_index_hash: &StacksBlockId,
        start_height: u64,
        end_height: u64,
    ) -> Result<Vec<StacksBlockHeader>> {
        let ids = self
            .get_chain_block_ids(tip_index_hash, start_height, end_height)
            .await?;
        let mut headers = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(h) = self.get_header(&id).await? {
                headers.push(h);
            }
        }
        Ok(headers)
    }

    /// Fetches a single block header by index block hash ([`StacksBlockId`]),
    /// resolving parent and burn info.
    pub async fn get_block(&self, id: &StacksBlockId) -> Result<StacksBlockHeader> {
        // Define an alias for the parent block table
        let parent_block = diesel::alias!(stacks_block as parent_block);

        // Fetch StacksBlock, joined BurnBlock, and joined ParentBlock (for hash)
        // We use left_join for parent because the genesis block has no parent.
        let (s_block, b_block, parent_hash_opt) = stacks_block::table
            .inner_join(burn_block::table)
            .left_join(
                parent_block.on(stacks_block::parent_stacks_block_id
                    .eq(parent_block.field(stacks_block::id).nullable())),
            )
            .select((
                StacksBlock::as_select(),
                BurnBlock::as_select(),
                parent_block.field(stacks_block::index_hash).nullable(),
            ))
            .filter(stacks_block::index_hash.eq(id.as_bytes()))
            .first::<(StacksBlock, BurnBlock, Option<Vec<u8>>)>(&mut self.get_conn().await?)
            .await
            .optional()?
            .ok_or_else(|| anyhow!("Block {} not found in App DB", id))
            .with_context(|| {
                format!("AppDb: Failed to fetch block header for stacks block id '{id}'")
            })?;

        (s_block, b_block, parent_hash_opt).try_into()
    }

    /// Attempts to resolve the block containing `tx_hash` from previously indexed benchmark data
    /// for the given chain tip.
    ///
    /// This is a best-effort fast-path used to avoid a full chain walk when the transaction was
    /// already seen in runs for the same chainstate tip.
    pub async fn find_block_for_tx_hash_on_chain_tip(
        &self,
        tx_hash: &[u8],
        chain_tip: &StacksBlockId,
    ) -> Result<Option<StacksBlockHeader>> {
        #[derive(Debug, QueryableByName)]
        struct Row {
            #[diesel(sql_type = Binary)]
            index_hash: Vec<u8>,
        }

        let query = r#"
            SELECT sb.index_hash
            FROM stacks_tx st
            JOIN stacks_block sb ON sb.id = st.stacks_block_id
            JOIN synthetic_block syn ON syn.stacks_block_id = sb.id
            JOIN stacks_block_stats sbs ON sbs.synthetic_block_id = syn.id
            JOIN benchmark_run br ON br.id = sbs.benchmark_run_id
            JOIN chainstate cs ON cs.id = br.chainstate_id
            WHERE st.tx_hash = ?1
              AND cs.tip_index_hash = ?2
            ORDER BY br.start_time DESC
            LIMIT 1
        "#;

        let row = sql_query(query)
            .bind::<Binary, _>(tx_hash)
            .bind::<Binary, _>(chain_tip.as_bytes())
            .get_result::<Row>(&mut self.get_conn().await?)
            .await
            .optional()
            .context("Failed to query tx fast-path block lookup")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let block_id = StacksBlockId::from_vec(&row.index_hash)
            .ok_or_else(|| anyhow!("Invalid stacks_block.index_hash in fast-path lookup"))?;

        self.get_block(&block_id).await.map(Some)
    }

    async fn get_or_create_synth_block_id(
        conn: &mut AsyncSqliteConnection,
        index_hash: &[u8],
        source_stacks_block_id: i64,
    ) -> Result<i64> {
        use crate::db::app::schema::synthetic_block;

        diesel::insert_into(synthetic_block::table)
            .values((
                synthetic_block::index_hash.eq(index_hash.to_vec()),
                synthetic_block::stacks_block_id.eq(source_stacks_block_id),
            ))
            .on_conflict(synthetic_block::index_hash)
            .do_nothing()
            .execute(conn)
            .await
            .context("Failed to insert synthetic_block")?;

        let (id, existing_stacks_block_id): (i64, i64) = synthetic_block::table
            .select((synthetic_block::id, synthetic_block::stacks_block_id))
            .filter(synthetic_block::index_hash.eq(index_hash))
            .first(conn)
            .await
            .context("Failed to load synthetic_block")?;

        if existing_stacks_block_id != source_stacks_block_id {
            return Err(anyhow!(
                "synthetic_block index_hash collision: existing stacks_block_id={} new stacks_block_id={}",
                existing_stacks_block_id,
                source_stacks_block_id
            ));
        }

        Ok(id)
    }

    pub async fn stage_blocks<'a, I>(&mut self, blocks: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a StacksBlockHeader> + Send,
        I::IntoIter: Send,
    {
        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async {
                    for block in blocks {
                        let staged = StagedStacksBlock {
                            index_hash: block.id.0.to_vec(),
                            block_hash: block.hash.0.to_vec(),
                            parent_index_hash: block.parent_id.0.to_vec(),
                            height: block.height as i64,
                            burn_block_hash: block.burn_block_hash.0.to_vec(),
                            burn_block_height: block.burn_block_height as i64,
                        };

                        diesel::insert_into(_staged_stacks_block::table)
                            .values(&staged)
                            .execute(conn)
                            .await
                            .with_context(|| {
                                format!(
                                    "Failed to stage block {}:{}",
                                    block.hash.to_hex(),
                                    block.height
                                )
                            })?;
                    }
                    Ok(())
                })
            })
            .await
    }

    pub async fn stage_transactions<I>(&mut self, blocks_with_txs: I) -> Result<()>
    where
        I: IntoIterator<Item = (StacksBlockHeader, Vec<StacksTransaction>)> + Send,
        I::IntoIter: Send,
    {
        struct StagingBuffer {
            txs: Vec<StagedStacksTx>,
            tx_types: HashSet<StacksTxType>,
        }

        impl StagingBuffer {
            fn new() -> Self {
                Self {
                    txs: Vec::new(),
                    tx_types: HashSet::new(),
                }
            }

            fn process_txs(
                &mut self,
                block: &StacksBlockHeader,
                txs: &[StacksTransaction],
            ) -> Result<()> {
                for tx in txs {
                    let block_index_hash = block.id.as_bytes().to_vec();
                    let tx_hash = tx.txid().as_bytes().to_vec();

                    let tx_type = util::resolve_tx_type(&tx.payload);
                    let stacks_tx_type_id = tx_type.id;
                    self.tx_types.insert(tx_type);

                    let caller_address = tx.origin_address().to_string();

                    let mut contract_issuer_address = None;
                    let mut contract_name = None;
                    let mut contract_fn_name: Option<String> = None;
                    let mut contract_call_args_json: Option<String> = None;

                    if let TransactionPayload::SmartContract(sc, _) = &tx.payload {
                        // Deploy: issuer is caller; contract name is in payload
                        contract_issuer_address = Some(caller_address.clone());
                        contract_name = Some(sc.name.to_string());
                    }

                    if let TransactionPayload::ContractCall(cc) = &tx.payload {
                        // Call: issuer + contract name + fn name + args
                        contract_issuer_address =
                            Some(cc.contract_identifier().issuer.to_address().to_string());
                        contract_name = Some(cc.contract_name.to_string());
                        contract_fn_name = Some(cc.function_name.to_string());

                        // Store args as JSON array of Clarity string representations
                        // (keeps it stable and queryable even without a full Clarity->JSON mapping)
                        let args_json = serde_json::to_string(&cc.function_args)
                            .with_context(|| format!("Failed to serialize contract-call arguments to JSON string for tx '{}'", tx.txid().to_hex()))?;
                        contract_call_args_json = Some(args_json);
                    }

                    self.txs.push(StagedStacksTx {
                        block_index_hash,
                        tx_hash,
                        stacks_tx_type_id,
                        caller_address,
                        contract_issuer_address,
                        contract_name,
                        contract_fn_name,
                        contract_call_args_json,
                    });
                }
                Ok(())
            }

            async fn flush(&mut self, conn: &mut AsyncSqliteConnection) -> Result<()> {
                for tx_type in self.tx_types.drain() {
                    diesel::insert_into(stacks_tx_type::table)
                        .values(&tx_type)
                        .on_conflict(stacks_tx_type::id)
                        .do_update()
                        .set(stacks_tx_type::name.eq(&tx_type.name))
                        .execute(conn)
                        .await?;
                }

                for tx in self.txs.drain(..) {
                    diesel::insert_into(_staged_stacks_tx::table)
                        .values(tx)
                        .execute(conn)
                        .await?;
                }
                Ok(())
            }
        }

        const CHUNK_SIZE: usize = 1000;

        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async {
                    let mut buffer = StagingBuffer::new();
                    let mut block_iter = blocks_with_txs.into_iter();

                    loop {
                        let chunk: Vec<(StacksBlockHeader, Vec<StacksTransaction>)> =
                            block_iter.by_ref().take(CHUNK_SIZE).collect();
                        if chunk.is_empty() {
                            break;
                        }

                        for (block, txs) in chunk {
                            buffer.process_txs(&block, &txs)?;
                        }

                        buffer.flush(conn).await?;
                    }
                    Ok(())
                })
            })
            .await
    }

    pub async fn stage_indexed_blocks<'a, I>(&mut self, headers: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a StacksBlockHeader> + Send,
        I::IntoIter: Send,
    {
        use crate::db::app::schema::_staged_indexed_stacks_block;

        // Collect once so we can iterate inside the async transaction closure safely.
        let hashes: Vec<Vec<u8>> = headers
            .into_iter()
            .map(|h| h.id.as_bytes().to_vec())
            .collect();

        if hashes.is_empty() {
            return Ok(());
        }

        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async move {
                    for block_index_hash in hashes {
                        diesel::insert_into(_staged_indexed_stacks_block::table)
                            .values(
                                _staged_indexed_stacks_block::block_index_hash.eq(block_index_hash),
                            )
                            .on_conflict(_staged_indexed_stacks_block::block_index_hash)
                            .do_nothing()
                            .execute(conn)
                            .await?;
                    }
                    Ok(())
                })
            })
            .await
    }

    pub async fn merge_staging(&mut self) -> Result<()> {
        self.get_conn()
            .await?
            .transaction::<_, anyhow::Error, _>(|conn| {
                Box::pin(async {
                    conn.batch_execute(MERGE_STAGING_SQL).await?;

                    // Cleanup
                    diesel::delete(_staged_stacks_block::table)
                        .execute(conn)
                        .await?;
                    diesel::delete(_staged_stacks_tx::table)
                        .execute(conn)
                        .await?;

                    Ok(())
                })
            })
            .await
    }

    /// Computes a deterministic hash of the epoch configuration.
    fn compute_epochs_hash(epochs: &[StacksEpoch]) -> Vec<u8> {
        let mut hasher = Sha256::new();
        // Sort by start height to ensure determinism
        let mut sorted = epochs.to_vec();
        sorted.sort_by_key(|e| e.start_block_height());

        for epoch in sorted {
            hasher.update(epoch.epoch_id_le_bytes());
            hasher.update(epoch.start_block_height().to_le_bytes());
            hasher.update(epoch.end_block_height().to_le_bytes());
            hasher.update(epoch.write_length_budget.to_le_bytes());
            hasher.update(epoch.write_count_budget.to_le_bytes());
            hasher.update(epoch.read_length_budget.to_le_bytes());
            hasher.update(epoch.read_count_budget.to_le_bytes());
            hasher.update(epoch.runtime_budget.to_le_bytes());
        }
        hasher.finalize().to_vec()
    }

    async fn resolve_profiler_location(
        conn: &mut AsyncSqliteConnection,
        cache: Arc<RwLock<HashMap<(String, i32), i32>>>,
        file: &str,
        line: i32,
    ) -> Result<i32> {
        let loc_key = (file.to_string(), line);
        if let Some(&id) = cache.read().await.get(&loc_key) {
            return Ok(id);
        }

        // Try insert, handle conflict by doing nothing
        // We use .optional() because if the row exists, do_nothing() returns no rows
        let id_opt: Option<i32> = diesel::insert_into(profiler_location::table)
            .values((
                profiler_location::file.eq(file),
                profiler_location::line.eq(line),
            ))
            .on_conflict((profiler_location::file, profiler_location::line))
            .do_nothing()
            .returning(profiler_location::id)
            .get_result(conn)
            .await
            .optional()?;

        let id = if let Some(id) = id_opt {
            id
        } else {
            // Fallback: Select existing ID if insert did nothing
            profiler_location::table
                .select(profiler_location::id)
                .filter(profiler_location::file.eq(file))
                .filter(profiler_location::line.eq(line))
                .first(conn)
                .await?
        };

        cache.write().await.insert(loc_key, id);
        Ok(id)
    }

    async fn resolve_profiler_span(
        conn: &mut AsyncSqliteConnection,
        cache: Arc<RwLock<HashMap<(Option<&'static str>, &'static str), i32>>>,
        context: Option<&'static str>,
        name: &'static str,
    ) -> Result<i32> {
        if let Some(&id) = cache.read().await.get(&(context, name)) {
            return Ok(id);
        }

        let id_opt: Option<i32> = diesel::insert_into(profiler_span::table)
            .values((
                profiler_span::context.eq(context),
                profiler_span::name.eq(name),
            ))
            .on_conflict((profiler_span::context, profiler_span::name))
            .do_nothing()
            .returning(profiler_span::id)
            .get_result(conn)
            .await
            .optional()?;

        let id = if let Some(id) = id_opt {
            id
        } else {
            profiler_span::table
                .select(profiler_span::id)
                .filter(profiler_span::context.eq(context))
                .filter(profiler_span::name.eq(name))
                .first(conn)
                .await?
        };
        cache.write().await.insert((context, name), id);
        Ok(id)
    }

    async fn resolve_profiler_tag(
        conn: &mut AsyncSqliteConnection,
        cache: Arc<RwLock<HashMap<&'static str, i32>>>,
        tag: &'static str,
    ) -> Result<i32> {
        if let Some(&id) = cache.read().await.get(&tag) {
            return Ok(id);
        }

        let id_opt: Option<i32> = diesel::insert_into(profiler_tag::table)
            .values(profiler_tag::tag.eq(tag))
            .on_conflict(profiler_tag::tag)
            .do_nothing()
            .returning(profiler_tag::id)
            .get_result(conn)
            .await
            .optional()?;

        let id = if let Some(id) = id_opt {
            id
        } else {
            profiler_tag::table
                .select(profiler_tag::id)
                .filter(profiler_tag::tag.eq(tag))
                .first(conn)
                .await?
        };

        cache.write().await.insert(tag, id);
        Ok(id)
    }

    pub async fn save_block_processing_baseline(
        &mut self,
        run_id: i32,
        start_parent: &StacksBlockId,
        warmup_blocks: u32,
        measured_blocks: u32,
        baseline: &crate::metrics::BlockProcessingBaseline,
    ) -> Result<()> {
        use crate::db::app::schema::block_processing_baseline;

        let conn = &mut self.get_conn().await?;

        diesel::insert_into(block_processing_baseline::table)
            .values((
                block_processing_baseline::benchmark_run_id.eq(run_id),
                block_processing_baseline::start_parent_index_hash
                    .eq(start_parent.as_bytes().to_vec()),
                block_processing_baseline::warmup_blocks.eq(warmup_blocks as i32),
                block_processing_baseline::measured_blocks.eq(measured_blocks as i32),
                block_processing_baseline::avg_setup_us
                    .eq(baseline.avg_setup_duration.as_micros() as i32),
                block_processing_baseline::avg_finalize_us
                    .eq(baseline.avg_finalize_duration.as_micros() as i32),
                block_processing_baseline::avg_clarity_commit_us
                    .eq(baseline.avg_clarity_state_commit_duration.as_micros() as i32),
                block_processing_baseline::avg_advance_tip_us
                    .eq(baseline.avg_advance_tip_duration.as_micros() as i32),
                block_processing_baseline::avg_index_commit_us
                    .eq(baseline.avg_index_commit_duration.as_micros() as i32),
            ))
            .execute(conn)
            .await
            .context("Failed to insert block processing baseline")?;

        Ok(())
    }

    pub async fn save_block_metrics<I>(&mut self, run_id: i32, blocks: I) -> Result<()>
    where
        I: IntoIterator<Item = BlockMetrics> + Send,
        I::IntoIter: Send,
    {
        let span_cache = self.profiler_span_cache.clone();
        let loc_cache = self.profiler_loc_cache.clone();
        let mut staged_kv_count = 0;
        let mut staged_clarity_costs_count = 0;

        self.set_synchronization_mode(SynchronizationMode::Off)
            .await?;
        self.set_foreign_key_enforcement(ForeignKeyMode::Off)
            .await?;

        self.get_conn()
            .await?
            .transaction::<(), anyhow::Error, _>(|dbtx| {
                Box::pin(async {
                    for metrics in blocks.into_iter() {
                        // Resolve Stacks block PK
                        let block_pk: i64 = stacks_block::table
                            .select(stacks_block::id)
                            .filter(stacks_block::index_hash.eq(metrics.id.as_bytes()))
                            .first(dbtx)
                            .await?;

                        let synthetic_block_pk = Self::get_or_create_synth_block_id(
                            dbtx,
                            metrics.synthetic_id.as_bytes(),
                            block_pk,
                        )
                        .await?;

                        // Insert block stats
                        diesel::insert_into(stacks_block_stats::table)
                            .values((
                                stacks_block_stats::benchmark_run_id.eq(run_id),
                                stacks_block_stats::synthetic_block_id.eq(synthetic_block_pk),
                                stacks_block_stats::total_duration_us
                                    .eq(metrics.total_duration.as_micros() as i32),
                                stacks_block_stats::setup_duration_us
                                    .eq(metrics.setup_duration.as_micros() as i32),
                                stacks_block_stats::execution_duration_us
                                    .eq(metrics.execution_duration.as_micros() as i32),
                                stacks_block_stats::commit_duration_us
                                    .eq(metrics.commit_duration.as_micros() as i32),
                                stacks_block_stats::commit_overhead_baseline_us
                                    .eq(metrics.commit_overhead_baseline.as_micros() as i32),
                                stacks_block_stats::clarity_write_length
                                    .eq(metrics.total_clarity_cost.write_length as i32),
                                stacks_block_stats::clarity_write_count
                                    .eq(metrics.total_clarity_cost.write_count as i32),
                                stacks_block_stats::clarity_read_length
                                    .eq(metrics.total_clarity_cost.read_length as i32),
                                stacks_block_stats::clarity_read_count
                                    .eq(metrics.total_clarity_cost.read_count as i32),
                                stacks_block_stats::clarity_runtime
                                    .eq(metrics.total_clarity_cost.runtime as i32),
                                stacks_block_stats::total_storage_delta.eq(metrics.total_storage_delta),
                            ))
                            .execute(dbtx)
                            .await
                            .context("Failed to insert stacks block stats")?;

                        // Load tx_id map for this block
                        let tx_map: HashMap<Vec<u8>, i64> = stacks_tx::table
                            .select((stacks_tx::tx_hash, stacks_tx::id))
                            .filter(stacks_tx::stacks_block_id.eq(block_pk))
                            .load::<(Vec<u8>, i64)>(dbtx)
                            .await?
                            .into_iter()
                            .collect();

                        // Insert tx stats + remember tx PKs for profiler attachment
                        let mut tx_pks: Vec<Option<i64>> =
                            Vec::with_capacity(metrics.transactions.len());
                        for tx_metric in &metrics.transactions {
                            let tx_hash = tx_metric.txid.as_bytes().to_vec();
                            let tx_pk = tx_map.get(&tx_hash).copied();
                            tx_pks.push(tx_pk);

                            if let Some(tx_pk) = tx_pk {
                                diesel::insert_into(stacks_tx_stats::table)
                                    .values((
                                        stacks_tx_stats::benchmark_run_id.eq(run_id),
                                        stacks_tx_stats::stacks_tx_id.eq(tx_pk),
                                        stacks_tx_stats::synthetic_block_id.eq(synthetic_block_pk),
                                        stacks_tx_stats::duration_us
                                            .eq(tx_metric.duration.as_micros() as i32),
                                        stacks_tx_stats::clarity_write_length
                                            .eq(tx_metric.cost.write_length as i32),
                                        stacks_tx_stats::clarity_write_count
                                            .eq(tx_metric.cost.write_count as i32),
                                        stacks_tx_stats::clarity_read_length
                                            .eq(tx_metric.cost.read_length as i32),
                                        stacks_tx_stats::clarity_read_count
                                            .eq(tx_metric.cost.read_count as i32),
                                        stacks_tx_stats::clarity_runtime
                                            .eq(tx_metric.cost.runtime as i32),
                                    ))
                                    .execute(dbtx)
                                    .await
                                    .context("Failed to insert stacks tx stats")?;
                            }
                        }

                        // Insert profiler records (block roots + tx roots)
                        let ctx = ProfilerInsertContext {
                            run_id,
                            synthetic_block_pk,
                            tx_pks: &tx_pks,
                            span_cache: span_cache.clone(),
                            loc_cache: loc_cache.clone(),
                            tag_cache: self.profiler_tag_cache.clone(),
                        };

                        for (i, root) in metrics.profiler_roots.iter().enumerate() {
                            let result = ctx.insert_node(dbtx, root, None, i as i32, 0, None).await?;
                            staged_kv_count += result.inserted_kv_records;
                            staged_clarity_costs_count += result.inserted_clarity_cost_records;
                        }
                    }

                    if staged_kv_count > 0 {
                        dbtx
                            .batch_execute(MERGE_PROFILER_KV_SQL)
                            .await
                            .with_context(|| format!("Failed to execute profiler KV staging merge script ({staged_kv_count} staged records)"))?;
                    }

                    if staged_clarity_costs_count > 0 {
                        dbtx
                            .batch_execute(MERGE_PROFILER_CLARITY_COSTS_SQL)
                            .await
                            .with_context(|| format!("Failed to execute profiler clarity costs staging merge script ({staged_clarity_costs_count} staged records)"))?;
                    }

                    Ok(())
                })
            })
            .await?;

        self.set_synchronization_mode(SynchronizationMode::Normal)
            .await?;
        self.set_foreign_key_enforcement(ForeignKeyMode::Enforced)
            .await?;
        self.checkpoint(CheckpointMode::Passive).await?;

        Ok(())
    }
}

#[derive(Debug, Default)]
struct ProfilerInsertContext<'a> {
    run_id: i32,
    synthetic_block_pk: i64,
    tx_pks: &'a [Option<i64>],
    span_cache: Arc<RwLock<HashMap<(Option<&'static str>, &'static str), i32>>>,
    loc_cache: Arc<RwLock<HashMap<(String, i32), i32>>>,
    tag_cache: Arc<RwLock<HashMap<&'static str, i32>>>,
}

#[derive(Debug, Default)]
struct InsertNodeResult {
    inserted_kv_records: usize,
    inserted_clarity_cost_records: usize,
}

impl std::ops::AddAssign for InsertNodeResult {
    fn add_assign(&mut self, other: Self) {
        self.inserted_kv_records += other.inserted_kv_records;
        self.inserted_clarity_cost_records += other.inserted_clarity_cost_records;
    }
}

impl ProfilerInsertContext<'_> {
    fn insert_node<'b>(
        &'b self,
        conn: &'b mut AsyncSqliteConnection,
        node: &'b stacks_profiler::ProfileStats,
        parent_id: Option<i64>,
        child_index: i32,
        depth: i32,
        active_tx_id: Option<i64>,
    ) -> BoxFuture<'b, Result<InsertNodeResult>> {
        async move {
            let mut stacks_tx_id = active_tx_id;

            let mut result = InsertNodeResult::default();

            if node.name() == "Transaction"
                && let Some(tag) = node.tag()
            {
                let idx = match tag {
                    stacks_profiler::Tag::Usize(v) => Some(*v),
                    stacks_profiler::Tag::U64(v) => usize::try_from(*v).ok(),
                    stacks_profiler::Tag::I64(v) => usize::try_from(*v).ok(), // negative => None
                    stacks_profiler::Tag::Str(_) => None,
                };

                if let Some(i) = idx {
                    stacks_tx_id = self.tx_pks.get(i).and_then(|x| *x);
                }
            }

            let loc_id = AppDb::resolve_profiler_location(
                conn,
                self.loc_cache.clone(),
                node.source_file(),
                node.source_line() as i32,
            )
            .await?;

            let span_id = AppDb::resolve_profiler_span(
                conn,
                self.span_cache.clone(),
                node.context(),
                node.name(),
            )
            .await?;

            let tag_id: Option<i32> = match node.tag() {
                Some(stacks_profiler::Tag::Str(s)) => {
                    Some(AppDb::resolve_profiler_tag(conn, self.tag_cache.clone(), s).await?)
                }
                _ => None, // numeric tags used for tx routing only
            };

            let wall_time_us = node.wall_time_micros() as i64;
            let children_wall_time_us: i64 = node
                .children
                .iter()
                .map(|c| c.wall_time_micros() as i64)
                .sum();
            let self_wall_time_us = wall_time_us.saturating_sub(children_wall_time_us);

            let cpu_time_us = node.cpu_time_micros() as i64;
            let children_cpu_time_us: i64 = node
                .children
                .iter()
                .map(|c| c.cpu_time_micros() as i64)
                .sum();
            let self_cpu_time_us = cpu_time_us.saturating_sub(children_cpu_time_us);

            let record_id: i64 = diesel::insert_into(schema::profiler_record::table)
                .values((
                    schema::profiler_record::benchmark_run_id.eq(self.run_id),
                    schema::profiler_record::parent_id.eq(parent_id),
                    schema::profiler_record::profiler_span_id.eq(span_id),
                    schema::profiler_record::profiler_tag_id.eq(tag_id),
                    schema::profiler_record::profiler_location_id.eq(loc_id),
                    schema::profiler_record::child_index.eq(child_index),
                    schema::profiler_record::depth.eq(depth),
                    schema::profiler_record::synthetic_block_id.eq(self.synthetic_block_pk),
                    schema::profiler_record::stacks_tx_id.eq(stacks_tx_id),
                    schema::profiler_record::wall_time_us.eq(wall_time_us),
                    schema::profiler_record::cpu_time_us.eq(cpu_time_us),
                    schema::profiler_record::self_wall_time_us.eq(self_wall_time_us),
                    schema::profiler_record::self_cpu_time_us.eq(self_cpu_time_us),
                    schema::profiler_record::call_count.eq(node.entered_count as i32),
                    schema::profiler_record::sample_count.eq(node.sampled_count as i32),
                ))
                .returning(schema::profiler_record::id)
                .get_result(conn)
                .await?;

            let staged_kvs = node.records.iter().map(|r| {
                let (value_type_id, value_str) = map_record_value_string(r);
                StagedProfilerRecordKv {
                    profiler_record_id: record_id,
                    key: r.key.to_string(),
                    value_type_id,
                    value: value_str,
                    count: 1,
                }
            });

            let mut clarity_costs_runtime: i64 = 0;
            let mut clarity_costs_read_count: i64 = 0;
            let mut clarity_costs_read_length: i64 = 0;
            let mut clarity_costs_write_count: i64 = 0;
            let mut clarity_costs_write_length: i64 = 0;
            let mut clarity_costs_input_n: i64 = 0;
            let mut has_clarity_costs = false;

            let staged_counter_kvs = node.counters.iter().filter_map(|c| {
                match c.key {
                    "CR" => {
                        has_clarity_costs = true;
                        clarity_costs_runtime = c.value as i64;
                        None
                    }
                    "CRC" => {
                        has_clarity_costs = true;
                        clarity_costs_read_count = c.value as i64;
                        None
                    }
                    "CRL" => {
                        has_clarity_costs = true;
                        clarity_costs_read_length = c.value as i64;
                        None
                    }
                    "CIN" => {
                        has_clarity_costs = true;
                        clarity_costs_input_n = c.value as i64;
                        None
                    }
                    "CWC" => {
                        has_clarity_costs = true;
                        clarity_costs_write_count = c.value as i64;
                        None
                    }
                    "CWL" => {
                        has_clarity_costs = true;
                        clarity_costs_write_length = c.value as i64;
                        None
                    }
                    _ => Some(StagedProfilerRecordKv {
                        profiler_record_id: record_id,
                        key: format!("counter:{}", c.key),
                        value_type_id: 1, // U64
                        value: c.value.to_string(),
                        count: 1,
                    }),
                }
            });

            for row in staged_kvs.chain(staged_counter_kvs) {
                diesel::insert_into(schema::_staged_profiler_record_kv::table)
                    .values(row)
                    .on_conflict((
                        schema::_staged_profiler_record_kv::profiler_record_id,
                        schema::_staged_profiler_record_kv::key,
                        schema::_staged_profiler_record_kv::value_type_id,
                        schema::_staged_profiler_record_kv::value,
                    ))
                    .do_update()
                    .set(
                        schema::_staged_profiler_record_kv::count
                            .eq(schema::_staged_profiler_record_kv::count
                                + excluded(schema::_staged_profiler_record_kv::count)),
                    )
                    .execute(conn)
                    .await
                    .context("Failed to insert profiler KV staging record")?;
                result.inserted_kv_records += 1;
            }

            if has_clarity_costs {
                diesel::insert_into(schema::_staged_profiler_record_clarity_costs::table)
                    .values((
                        schema::_staged_profiler_record_clarity_costs::profiler_record_id
                            .eq(record_id),
                        schema::_staged_profiler_record_clarity_costs::runtime
                            .eq(clarity_costs_runtime),
                        schema::_staged_profiler_record_clarity_costs::read_count
                            .eq(clarity_costs_read_count),
                        schema::_staged_profiler_record_clarity_costs::read_length
                            .eq(clarity_costs_read_length),
                        schema::_staged_profiler_record_clarity_costs::write_count
                            .eq(clarity_costs_write_count),
                        schema::_staged_profiler_record_clarity_costs::write_length
                            .eq(clarity_costs_write_length),
                        schema::_staged_profiler_record_clarity_costs::input_n
                            .eq(clarity_costs_input_n),
                    ))
                    .on_conflict(schema::_staged_profiler_record_clarity_costs::profiler_record_id)
                    .do_update()
                    .set((
                        schema::_staged_profiler_record_clarity_costs::runtime
                            .eq(clarity_costs_runtime),
                        schema::_staged_profiler_record_clarity_costs::read_count
                            .eq(clarity_costs_read_count),
                        schema::_staged_profiler_record_clarity_costs::read_length
                            .eq(clarity_costs_read_length),
                        schema::_staged_profiler_record_clarity_costs::write_count
                            .eq(clarity_costs_write_count),
                        schema::_staged_profiler_record_clarity_costs::write_length
                            .eq(clarity_costs_write_length),
                        schema::_staged_profiler_record_clarity_costs::input_n
                            .eq(clarity_costs_input_n),
                    ))
                    .execute(conn)
                    .await
                    .context("Failed to insert profiler clarity costs staging record")?;
                result.inserted_clarity_cost_records += 1;
            }

            for (idx, child) in node.children.iter().enumerate() {
                result += self
                    .insert_node(
                        conn,
                        child,
                        Some(record_id),
                        idx as i32,
                        depth + 1,
                        stacks_tx_id,
                    )
                    .await?;
            }

            Ok(result)
        }
        .boxed()
    }
}

fn map_record_value_string(r: &stacks_profiler::Record) -> (i32, String) {
    match &r.value {
        stacks_profiler::RecordValue::U64(v) => (1, v.to_string()),
        stacks_profiler::RecordValue::I64(v) => (2, v.to_string()),
        stacks_profiler::RecordValue::Str(s) => (3, s.to_string()),
        stacks_profiler::RecordValue::Bytes(b) => (4, hex::encode(b)),
    }
}

impl ChainCache for AppDb {
    async fn find_closest_ancestor(
        &self,
        tip: &StacksBlockId,
        target_height: u64,
    ) -> Result<Option<(StacksBlockId, u64)>> {
        // Find the block with the smallest height that is still >= target_height
        // This gives us the closest point we can jump to without overshooting.
        let result = chain_tip_cache::table
            .select((chain_tip_cache::index_hash, chain_tip_cache::height))
            .filter(chain_tip_cache::tip_index_hash.eq(tip.as_bytes()))
            .filter(chain_tip_cache::height.ge(target_height as i64))
            .order(chain_tip_cache::height.asc())
            .first::<(Vec<u8>, i64)>(&mut self.get_conn().await?)
            .await
            .optional()?;

        if let Some((hash, height)) = result {
            let id =
                StacksBlockId::from_vec(&hash).ok_or_else(|| anyhow!("Invalid hash in cache"))?;
            Ok(Some((id, height as u64)))
        } else {
            Ok(None)
        }
    }

    async fn cache_ancestor(
        &mut self,
        tip: &StacksBlockId,
        height: u64,
        block: &StacksBlockId,
    ) -> Result<()> {
        diesel::insert_into(chain_tip_cache::table)
            .values((
                chain_tip_cache::tip_index_hash.eq(tip.as_bytes()),
                chain_tip_cache::height.eq(height as i64),
                chain_tip_cache::index_hash.eq(block.as_bytes()),
            ))
            .on_conflict((chain_tip_cache::tip_index_hash, chain_tip_cache::height))
            .do_nothing()
            .execute(&mut self.get_conn().await?)
            .await?;
        Ok(())
    }
}

impl BlockHeaderProvider for AppDb {
    async fn get_header(&self, id: &StacksBlockId) -> Result<Option<StacksBlockHeader>> {
        match self.get_block(id).await {
            Ok(h) => Ok(Some(h)),
            Err(_) => Ok(None),
        }
    }
}
