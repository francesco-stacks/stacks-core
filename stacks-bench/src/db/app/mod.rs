use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};
use chrono::NaiveDateTime;
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

use crate::StacksBlockHeader;
use crate::blocks::{BlockHeaderProvider, ChainCache};
use crate::metrics::BlockMetrics;

pub mod models;
pub mod schema;

use super::{AsyncSqliteConnection, SqlitePool};

//type AsyncSqliteConnection = SyncConnectionWrapper<SqliteConnection>;
//type SqlitePool = Pool<AsyncSqliteConnection>;

// This macro embeds the SQL files from the "migrations" directory into the binary
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

const MERGE_STAGING_SQL: &str = include_str!("merge_staging.sql");

#[derive(Clone)]
pub struct AppDb {
    pool: SqlitePool,
    /// Cache of profiler span name to ID mappings.
    profiler_span_cache: Arc<RwLock<HashMap<(Option<&'static str>, &'static str), i32>>>,
    /// Cache of profiler location (file,line) to ID mappings.
    profiler_loc_cache: Arc<RwLock<HashMap<(String, i32), i32>>>,
}

impl AppDb {
    /// Default filename for the app database.
    pub const DEFAULT_DB_FILENAME: &'static str = "stacks-bench.db";

    /// Helper to apply standard PRAGMAs to a connection.
    /// Used by both the connection pool and the standalone migration connection.
    async fn setup_connection(
        conn: &mut AsyncSqliteConnection,
    ) -> Result<(), diesel::ConnectionError> {
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
        sql_query("PRAGMA page_size = 8192;")
            .execute(conn)
            .await
            .inspect_err(|e| eprintln!("Failed to set PRAGMA page_size=8192: {}", e))
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
            println!("App DB is up to date at {}", database_url);
        } else {
            println!(
                "App DB at {database_url} has {} pending migrations. Applying...",
                pending.len()
            );

            harness
                .run_pending_migrations(MIGRATIONS)
                .map_err(anyhow::Error::from_boxed)
                .context("Failed to run migrations")?;

            println!("Database migration(s) complete");
        }

        Ok(())
    }

    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let database_url = path_ref
            .to_str()
            .ok_or_else(|| anyhow!("Invalid database path (non-UTF8): {path_ref:?}"))?;

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

    pub async fn checkpoint(&mut self, truncate: bool) -> Result<()> {
        let conn = &mut self.get_conn().await?;
        if truncate {
            diesel::sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(conn)
                .await
                .context("Failed to perform WAL checkpoint with TRUNCATE")?;
        } else {
            diesel::sql_query("PRAGMA wal_checkpoint(FULL)")
                .execute(conn)
                .await
                .context("Failed to perform WAL checkpoint with FULL")?;
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
        tip_block_id: &StacksBlockId,
        tip_height: u64,
        source_epochs: &[crate::db::node::sortition::models::Epoch],
    ) -> Result<(Chainstate, Vec<Epoch>)> {
        let network_id = Self::resolve_network_id(network);

        // CHANGE: Explicitly type these as i64 to match schema::BigInt.
        // chain_id is u32, so .into() is safe.
        let chain_id_val: i64 = chain_id.into();
        // tip_height is u64, so .try_into() is needed.
        let tip_height_val: i64 = tip_height.try_into()?;

        // 1. Compute the configuration hash
        let epochs_hash = Self::compute_epochs_hash(source_epochs);

        let conn = &mut self.get_conn().await?;

        conn.transaction::<_, anyhow::Error, _>(|conn| {
            Box::pin(async {
                // 2. Try to find existing chainstate with matching config
                let existing = chainstate::table
                    .filter(chainstate::network_id.eq(network_id))
                    .filter(chainstate::chain_id.eq(chain_id_val))
                    .filter(chainstate::tip_index_hash.eq(tip_block_id.as_bytes()))
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
                            chainstate::tip_index_hash.eq(tip_block_id.0.to_vec()),
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
                                epoch::network_epoch_id.eq(e.network_epoch_id() as i32),
                                epoch::start_height.eq(e.start_block_height() as i64),
                                epoch::end_height.eq(e.end_block_height() as i64),
                                epoch::write_length_budget
                                    .eq(TryInto::<i64>::try_into(e.block_limits.write_length)?),
                                epoch::write_count_budget
                                    .eq(TryInto::<i64>::try_into(e.block_limits.write_count)?),
                                epoch::read_length_budget
                                    .eq(TryInto::<i64>::try_into(e.block_limits.read_length)?),
                                epoch::read_count_budget
                                    .eq(TryInto::<i64>::try_into(e.block_limits.read_count)?),
                                epoch::runtime_budget
                                    .eq(TryInto::<i64>::try_into(e.block_limits.runtime)?),
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
        Ok(())
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

    /// Saves execution metrics for a block and its transactions.
    /// Assumes the block and transactions have already been indexed.
    pub async fn save_block_metrics(
        &mut self,
        run_id: i32,
        block_id: &StacksBlockId,
        metrics: &BlockMetrics,
    ) -> Result<()> {
        let conn = &mut self.get_conn().await?;

        // Get block ID
        let block_id = self.get_stacks_block_id(block_id).await?;

        // Insert block stats
        diesel::insert_into(stacks_block_stats::table)
            .values((
                stacks_block_stats::benchmark_run_id.eq(run_id),
                stacks_block_stats::stacks_block_id.eq(block_id),
                stacks_block_stats::total_duration_us.eq(metrics.total_duration.as_micros() as i32),
                stacks_block_stats::setup_duration_us.eq(metrics.setup_duration.as_micros() as i32),
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
                stacks_block_stats::clarity_runtime.eq(metrics.total_clarity_cost.runtime as i32),
                stacks_block_stats::total_storage_delta.eq(metrics.total_storage_delta),
            ))
            .execute(conn)
            .await
            .context("Failed to insert stacks block stats")?;

        // Insert tx stats
        // Fetch all tx IDs for this block in one query to avoid N lookups
        let tx_map: HashMap<Vec<u8>, i64> = stacks_tx::table
            .select((stacks_tx::tx_hash, stacks_tx::id))
            .filter(stacks_tx::stacks_block_id.eq(block_id))
            .load::<(Vec<u8>, i64)>(conn)
            .await?
            .into_iter()
            .collect();

        let mut tx_stats_batch = Vec::with_capacity(metrics.transactions.len());
        for tx_metric in &metrics.transactions {
            let tx_hash_bytes = hex::decode(&tx_metric.txid)
                .with_context(|| format!("Invalid hex in txid '{}'", &tx_metric.txid))?;

            if let Some(&tx_id) = tx_map.get(&tx_hash_bytes) {
                tx_stats_batch.push((
                    stacks_tx_stats::benchmark_run_id.eq(run_id),
                    stacks_tx_stats::stacks_tx_id.eq(tx_id),
                    stacks_tx_stats::duration_us.eq(tx_metric.duration.as_micros() as i32),
                    stacks_tx_stats::estimated_commit_impact_us
                        .eq(tx_metric.estimated_commit_impact.as_micros() as i32),
                    stacks_tx_stats::clarity_write_length.eq(tx_metric.cost.write_length as i32),
                    stacks_tx_stats::clarity_write_count.eq(tx_metric.cost.write_count as i32),
                    stacks_tx_stats::clarity_read_length.eq(tx_metric.cost.read_length as i32),
                    stacks_tx_stats::clarity_read_count.eq(tx_metric.cost.read_count as i32),
                    stacks_tx_stats::clarity_runtime.eq(tx_metric.cost.runtime as i32),
                ));
            }
        }

        if !tx_stats_batch.is_empty() {
            for stats in tx_stats_batch {
                diesel::insert_into(stacks_tx_stats::table)
                    .values(stats)
                    .execute(conn)
                    .await
                    .context("Failed to insert stacks tx stats")?;
            }
        }

        Ok(())
    }

    /// Retrieves the ordered list of block IDs for the canonical chain segment.
    pub async fn get_chain_block_ids(
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

    /// Fetches a single block header by index block hash ([`StacksBlockId`]),
    /// resolving parent and burn info.
    pub async fn get_block(&mut self, id: &StacksBlockId) -> Result<StacksBlockHeader> {
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

    pub async fn stage_blocks<I>(&mut self, blocks: I) -> Result<()>
    where
        I: IntoIterator<Item = StacksBlockHeader> + Send,
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
            tx_types: HashSet<String>,
            principals: HashSet<String>,
            contracts: HashSet<(String, String)>,
        }

        impl StagingBuffer {
            fn new() -> Self {
                Self {
                    txs: Vec::new(),
                    tx_types: HashSet::new(),
                    principals: HashSet::new(),
                    contracts: HashSet::new(),
                }
            }

            fn process_txs(&mut self, block: &StacksBlockHeader, txs: &[StacksTransaction]) {
                for tx in txs {
                    let block_index_hash = block.id.as_bytes().to_vec();
                    let tx_hash = tx.txid().as_bytes().to_vec();
                    let tx_type = tx.payload.name().to_string();

                    self.tx_types.insert(tx_type.clone());

                    let caller_address = tx.origin_address().to_string();
                    self.principals.insert(caller_address.clone());

                    let mut contract_issuer_address = None;
                    let mut contract_name = None;

                    if let TransactionPayload::SmartContract(sc, _) = &tx.payload {
                        contract_issuer_address = Some(caller_address.clone());
                        let c_name = sc.name.to_string();
                        contract_name = Some(c_name.clone());
                        self.contracts.insert((caller_address.clone(), c_name));
                    }

                    if let TransactionPayload::ContractCall(cc) = &tx.payload {
                        let issuer_address =
                            cc.contract_identifier().issuer.to_address().to_string();
                        let name = cc.contract_name.to_string();

                        self.principals.insert(issuer_address.clone());
                        self.contracts
                            .insert((issuer_address.clone(), name.clone()));

                        contract_issuer_address = Some(issuer_address);
                        contract_name = Some(name);
                    }

                    self.txs.push(StagedStacksTx {
                        block_index_hash,
                        tx_hash,
                        tx_type,
                        caller_address,
                        contract_issuer_address,
                        contract_name,
                    });
                }
            }

            async fn flush(&mut self, conn: &mut AsyncSqliteConnection) -> Result<()> {
                for name in self.tx_types.drain() {
                    diesel::insert_into(_staged_stacks_tx_type::table)
                        .values(StagedStacksTxType { name })
                        .execute(conn)
                        .await?;
                }

                for address in self.principals.drain() {
                    diesel::insert_into(_staged_principal::table)
                        .values(StagedPrincipal { address })
                        .execute(conn)
                        .await?;
                }

                for (issuer_address, name) in self.contracts.drain() {
                    diesel::insert_into(schema::_staged_contract::table)
                        .values(StagedContract {
                            issuer_address,
                            name,
                        })
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
                            buffer.process_txs(&block, &txs);
                        }

                        buffer.flush(conn).await?;
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
                    diesel::delete(_staged_stacks_tx_type::table)
                        .execute(conn)
                        .await?;
                    diesel::delete(_staged_principal::table)
                        .execute(conn)
                        .await?;
                    diesel::delete(_staged_contract::table)
                        .execute(conn)
                        .await?;

                    Ok(())
                })
            })
            .await
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

    pub async fn save_profiler_data_batch(
        &mut self,
        run_id: i32,
        batch: &mut Vec<(
            StacksBlockId,
            Vec<stacks_profiler::ProfileStats>,
            Vec<crate::metrics::TransactionMetrics>,
        )>,
    ) -> Result<()> {
        // Take ownership of the batch data to process it
        let batch_data = std::mem::take(batch);

        // Clone caches before borrowing self for the connection
        let span_cache = self.profiler_span_cache.clone();
        let loc_cache = self.profiler_loc_cache.clone();

        let conn = &mut self.get_conn().await?;

        conn.transaction::<(), anyhow::Error, _>(|dbtx| {
            Box::pin(async move {
                for (block_id, results, tx_metrics) in batch_data {
                    // 1. Resolve Block ID (DB Primary Key)
                    let block_pk: i64 = stacks_block::table
                        .select(stacks_block::id)
                        .filter(stacks_block::index_hash.eq(block_id.as_bytes()))
                        .first(dbtx)
                        .await?;

                    // 2. Resolve Tx IDs (DB Primary Keys) for mapping
                    let tx_hashes: Vec<Vec<u8>> = tx_metrics
                        .iter()
                        .map(|tx| hex::decode(&tx.txid))
                        .collect::<Result<Vec<_>, _>>()?;

                    let tx_lookup: HashMap<Vec<u8>, i64> = stacks_tx::table
                        .select((stacks_tx::tx_hash, stacks_tx::id))
                        .filter(stacks_tx::tx_hash.eq_any(&tx_hashes))
                        .load(dbtx)
                        .await?
                        .into_iter()
                        .collect();

                    // Map the metrics order to the resolved IDs
                    let tx_db_ids: Vec<Option<i64>> = tx_hashes
                        .iter()
                        .map(|h| tx_lookup.get(h).cloned())
                        .collect();

                    let ctx = ProfilerInsertContext {
                        run_id,
                        block_pk,
                        tx_db_ids: &tx_db_ids,
                        span_cache: span_cache.clone(),
                        loc_cache: loc_cache.clone(),
                    };

                    // Start insertion for this block
                    for (i, root) in results.iter().enumerate() {
                        ctx.insert_node(dbtx, root, None, i as i32, 0, None).await?;
                    }
                }
                Ok(())
            })
        })
        .await
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
}

struct ProfilerInsertContext<'a> {
    run_id: i32,
    block_pk: i64,
    tx_db_ids: &'a [Option<i64>],
    span_cache: Arc<RwLock<HashMap<(Option<&'static str>, &'static str), i32>>>,
    loc_cache: Arc<RwLock<HashMap<(String, i32), i32>>>,
}

impl<'a> ProfilerInsertContext<'a> {
    fn insert_node<'b>(
        &'b self,
        conn: &'b mut AsyncSqliteConnection,
        node: &'b stacks_profiler::ProfileStats,
        parent_id: Option<i32>,
        child_index: i32,
        depth: i32,
        active_tx_id: Option<i64>,
    ) -> BoxFuture<'b, Result<()>> {
        async move {
            // A. Resolve/Insert Location (With Caching)
            let loc_id = AppDb::resolve_profiler_location(
                conn,
                self.loc_cache.clone(),
                &node.source_file(),
                node.source_line() as i32,
            )
            .await?;

            // B. Resolve/Insert Span Name (With Caching)
            let span_id = AppDb::resolve_profiler_span(
                conn,
                self.span_cache.clone(),
                node.context(),
                &node.name(),
            )
            .await?;

            // C. Determine Context (Block vs Tx)
            let mut current_tx_id = active_tx_id;
            if node.name() == "Transaction" {
                if let Some(tid) = self.tx_db_ids.get(child_index as usize).and_then(|x| *x) {
                    current_tx_id = Some(tid);
                }
            }

            // Calculate metrics
            let wall_time_us = node.wall_time_micros() as i64;
            let children_wall_time_us: i64 = node
                .children
                .iter()
                .map(|c| c.wall_time_micros() as i64)
                .sum();
            // Use saturating_sub to handle potential clock skew or precision issues
            let self_wall_time_us = wall_time_us.saturating_sub(children_wall_time_us);

            // CPU time
            let cpu_time_us = node.cpu_time_micros() as i64;
            let children_cpu_time_us: i64 = node
                .children
                .iter()
                .map(|c| c.cpu_time_micros() as i64)
                .sum();
            let self_cpu_time_us = cpu_time_us.saturating_sub(children_cpu_time_us);

            // Insert record
            let record_id: i32 = diesel::insert_into(schema::profiler_record::table)
                .values((
                    schema::profiler_record::benchmark_run_id.eq(self.run_id),
                    schema::profiler_record::parent_id.eq(parent_id),
                    schema::profiler_record::profiler_span_id.eq(span_id),
                    schema::profiler_record::tag.eq(&node.tag().map(|t| t.to_string())),
                    schema::profiler_record::profiler_location_id.eq(loc_id),
                    schema::profiler_record::child_index.eq(child_index),
                    schema::profiler_record::depth.eq(depth),
                    schema::profiler_record::stacks_block_id.eq(Some(self.block_pk)),
                    schema::profiler_record::stacks_tx_id.eq(current_tx_id),
                    schema::profiler_record::wall_time_us.eq(wall_time_us),
                    schema::profiler_record::cpu_time_us.eq(cpu_time_us),
                    schema::profiler_record::self_wall_time_us.eq(self_wall_time_us),
                    schema::profiler_record::self_cpu_time_us.eq(self_cpu_time_us),
                    schema::profiler_record::call_count.eq(node.total_count as i32),
                ))
                .returning(schema::profiler_record::id)
                .get_result(conn)
                .await?;

            // Recurse
            for (idx, child) in node.children.iter().enumerate() {
                self.insert_node(
                    conn,
                    child,
                    Some(record_id),
                    idx as i32,
                    depth + 1,
                    current_tx_id,
                )
                .await?;
            }
            Ok(())
        }
        .boxed()
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
    async fn get_header(&mut self, id: &StacksBlockId) -> Result<Option<StacksBlockHeader>> {
        match self.get_block(id).await {
            Ok(h) => Ok(Some(h)),
            Err(_) => Ok(None),
        }
    }
}
