use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use blockstack_lib::chainstate::stacks::TransactionPayload;
use chrono::NaiveDateTime;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::StacksBlockId;
use tokio::sync::Mutex;

use crate::metrics::BlockMetrics;
use crate::{BlockTransactions, ChainCache, Network, StacksBlockHeader};

pub mod models;
pub mod schema;

// This macro embeds the SQL files from the "migrations" directory into the binary
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

const MERGE_STAGING_SQL: &str = include_str!("merge_staging.sql");

pub struct AppDb {
    /// Underlying Diesel SQLite connection.
    conn: Arc<Mutex<SqliteConnection>>,
    /// Cache of profiler span name to ID mappings.
    profiler_span_cache: HashMap<(Option<&'static str>, &'static str), i32>,
    /// Cache of profiler location (file,line) to ID mappings.
    profiler_loc_cache: HashMap<(String, i32), i32>,
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
        // Use NORMAL locking mode for better concurrency
        diesel::sql_query("PRAGMA locking_mode = NORMAL;").execute(&mut conn)?;
        // Set synchronous to NORMAL for a balance of performance and durability
        diesel::sql_query("PRAGMA synchronous = NORMAL;").execute(&mut conn)?;
        // Store temporary tables in memory for speed
        diesel::sql_query("PRAGMA temp_store = MEMORY;").execute(&mut conn)?;
        // Set page size to 8KB for better performance with larger datasets
        diesel::sql_query("PRAGMA page_size = 8192;").execute(&mut conn)?;
        // Set cache size to 256MB
        diesel::sql_query("PRAGMA cache_size = -262144;").execute(&mut conn)?;
        // Use max mmap size (will be limited by OS)
        diesel::sql_query("PRAGMA mmap_size = 30000000000;").execute(&mut conn)?;

        let pending_migrations = conn
            .pending_migrations(MIGRATIONS)
            .map_err(anyhow::Error::from_boxed)
            .context("Failed to check pending migrations")?;

        if pending_migrations.is_empty() {
            println!("App DB is up to date at {}", database_url);
        } else {
            println!(
                "App DB at {database_url} has {} pending migrations. Applying...",
                pending_migrations.len(),
            );

            // Run migrations
            conn.run_pending_migrations(MIGRATIONS)
                .map_err(anyhow::Error::from_boxed)
                .context("Failed to run migrations")?;

            println!("Database migration(s) complete");
        }

        // Ensure foreign keys are enforced. Run after migrations in case any migrations disable them without re-enabling.
        diesel::sql_query("PRAGMA foreign_keys = ON").execute(&mut conn)?;

        Ok(AppDb {
            conn: Arc::new(Mutex::new(conn)),
            profiler_span_cache: HashMap::new(),
            profiler_loc_cache: HashMap::new(),
        })
    }

    pub async fn checkpoint(&mut self) -> Result<()> {
        diesel::sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut *self.conn.lock().await)
            .context("Failed to perform WAL checkpoint")?;
        Ok(())
    }

    pub async fn vacuum(&mut self) -> Result<()> {
        diesel::sql_query("VACUUM")
            .execute(&mut *self.conn.lock().await)
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

    pub async fn get_or_create_chainstate(
        &mut self,
        network: Network,
        chain_id: u32,
        tip_block_id: &StacksBlockId,
        tip_height: u64,
        source_epochs: &[crate::db::node::sortition::models::Epoch],
    ) -> Result<(models::Chainstate, Vec<models::Epoch>)> {
        use self::schema::{chainstate, epoch};

        let network_id = Self::resolve_network_id(network);

        // CHANGE: Explicitly type these as i64 to match schema::BigInt.
        // chain_id is u32, so .into() is safe.
        let chain_id_val: i64 = chain_id.into();
        // tip_height is u64, so .try_into() is needed.
        let tip_height_val: i64 = tip_height.try_into()?;

        // 1. Compute the configuration hash
        let epochs_hash = Self::compute_epochs_hash(source_epochs);

        let conn = &mut *self.conn.lock().await;

        conn.transaction::<_, anyhow::Error, _>(|conn| {
            // 2. Try to find existing chainstate with matching config
            let existing = chainstate::dsl::chainstate
                .filter(chainstate::dsl::network_id.eq(network_id))
                .filter(chainstate::dsl::chain_id.eq(chain_id_val))
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
                let chainstate: models::Chainstate =
                    diesel::insert_into(chainstate::dsl::chainstate)
                        .values((
                            chainstate::dsl::network_id.eq(network_id),
                            chainstate::dsl::chain_id.eq(chain_id_val),
                            chainstate::dsl::tip_index_hash.eq(tip_block_id.0.to_vec()),
                            chainstate::dsl::tip_height.eq(tip_height_val),
                            chainstate::dsl::epochs_hash.eq(epochs_hash),
                        ))
                        .get_result(conn)?;

                // 4. Insert epochs for this new chainstate
                let new_epochs_data = source_epochs
                    .iter()
                    .map(|e| {
                        Ok((
                            epoch::dsl::chainstate_id.eq(chainstate.id),
                            epoch::dsl::stacks_epoch_id.eq(e.epoch_id() as i32),
                            epoch::dsl::network_epoch_id.eq(e.network_epoch_id() as i32),
                            epoch::dsl::start_height.eq(e.start_block_height() as i64),
                            epoch::dsl::end_height.eq(e.end_block_height() as i64),
                            epoch::dsl::write_length_budget
                                .eq(TryInto::<i64>::try_into(e.block_limits.write_length)?),
                            epoch::dsl::write_count_budget
                                .eq(TryInto::<i64>::try_into(e.block_limits.write_count)?),
                            epoch::dsl::read_length_budget
                                .eq(TryInto::<i64>::try_into(e.block_limits.read_length)?),
                            epoch::dsl::read_count_budget
                                .eq(TryInto::<i64>::try_into(e.block_limits.read_count)?),
                            epoch::dsl::runtime_budget
                                .eq(TryInto::<i64>::try_into(e.block_limits.runtime)?),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;

                let epochs: Vec<models::Epoch> = diesel::insert_into(epoch::dsl::epoch)
                    .values(new_epochs_data)
                    .get_results(conn)?;

                Ok((chainstate, epochs))
            }
        })
        .context("Failed to get or create chainstate with epochs")
    }

    pub async fn get_or_create_burn_block(
        &mut self,
        hash: &[u8],
        height: u32,
    ) -> Result<models::BurnBlock> {
        use self::schema::burn_block;

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
            .get_result(&mut *self.conn.lock().await)
            .context("Failed to get or create burn_block")
    }

    pub async fn create_benchmark_run(
        &mut self,
        chainstate_id: i32,
        start_time: NaiveDateTime,
        git_commit_hash: Vec<u8>,
        run_name: Option<String>,
        args_json: String,
    ) -> Result<models::BenchmarkRun> {
        use self::schema::benchmark_run::dsl;

        diesel::insert_into(dsl::benchmark_run)
            .values((
                dsl::chainstate_id.eq(chainstate_id),
                dsl::start_time.eq(start_time),
                dsl::git_commit_hash.eq(git_commit_hash),
                dsl::run_name.eq(run_name),
                dsl::args_json.eq(args_json),
            ))
            .get_result(&mut *self.conn.lock().await)
            .context("Failed to create benchmark run")
    }

    pub async fn finish_benchmark_run(&mut self, run_id: i32, end_ts: NaiveDateTime) -> Result<()> {
        use self::schema::benchmark_run::dsl::*;

        diesel::update(benchmark_run.find(run_id))
            .set(end_time.eq(end_ts))
            .execute(&mut *self.conn.lock().await)
            .context("Failed to update benchmark run end time")?;
        Ok(())
    }

    /// Retrieves the internal DB ID for a Stacks block by its hash.
    pub async fn get_stacks_block_id(&self, block_id: &StacksBlockId) -> Result<i64> {
        use self::schema::stacks_block::dsl;
        dsl::stacks_block
            .select(dsl::id)
            .filter(dsl::index_hash.eq(block_id.as_bytes()))
            .first(&mut *self.conn.lock().await)
            .optional()?
            .ok_or_else(|| anyhow!("Stacks block not found in DB"))
    }

    /// Saves execution metrics for a block and its transactions.
    /// Assumes the block and transactions have already been indexed.
    pub async fn save_block_metrics(
        &mut self,
        run_id: i32,
        block_id: &StacksBlockId,
        metrics: &BlockMetrics,
    ) -> Result<()> {
        use self::schema::stacks_block_stats::dsl as block_stats_dsl;
        use self::schema::stacks_tx::dsl as tx_dsl;
        use self::schema::stacks_tx_stats::dsl as tx_stats_dsl;

        // Get block ID
        let block_id = self.get_stacks_block_id(block_id).await?;

        // Insert block stats
        diesel::insert_into(block_stats_dsl::stacks_block_stats)
            .values((
                block_stats_dsl::benchmark_run_id.eq(run_id),
                block_stats_dsl::stacks_block_id.eq(block_id),
                block_stats_dsl::total_duration_us.eq(metrics.total_duration.as_micros() as i32),
                block_stats_dsl::setup_duration_us.eq(metrics.setup_duration.as_micros() as i32),
                block_stats_dsl::execution_duration_us
                    .eq(metrics.execution_duration.as_micros() as i32),
                block_stats_dsl::commit_duration_us.eq(metrics.commit_duration.as_micros() as i32),
                block_stats_dsl::commit_overhead_baseline_us
                    .eq(metrics.commit_overhead_baseline.as_micros() as i32),
                block_stats_dsl::clarity_write_length
                    .eq(metrics.total_clarity_cost.write_length as i32),
                block_stats_dsl::clarity_write_count
                    .eq(metrics.total_clarity_cost.write_count as i32),
                block_stats_dsl::clarity_read_length
                    .eq(metrics.total_clarity_cost.read_length as i32),
                block_stats_dsl::clarity_read_count
                    .eq(metrics.total_clarity_cost.read_count as i32),
                block_stats_dsl::clarity_runtime.eq(metrics.total_clarity_cost.runtime as i32),
                block_stats_dsl::total_storage_delta.eq(metrics.total_storage_delta),
            ))
            .execute(&mut *self.conn.lock().await)
            .context("Failed to insert stacks block stats")?;

        // Insert tx stats
        // Fetch all tx IDs for this block in one query to avoid N lookups
        let tx_map: HashMap<Vec<u8>, i64> = tx_dsl::stacks_tx
            .select((tx_dsl::tx_hash, tx_dsl::id))
            .filter(tx_dsl::stacks_block_id.eq(block_id))
            .load::<(Vec<u8>, i64)>(&mut *self.conn.lock().await)?
            .into_iter()
            .collect();

        let mut tx_stats_batch = Vec::with_capacity(metrics.transactions.len());
        for tx_metric in &metrics.transactions {
            let tx_hash_bytes = hex::decode(&tx_metric.txid)
                .with_context(|| format!("Invalid hex in txid '{}'", &tx_metric.txid))?;

            if let Some(&tx_id) = tx_map.get(&tx_hash_bytes) {
                tx_stats_batch.push((
                    tx_stats_dsl::benchmark_run_id.eq(run_id),
                    tx_stats_dsl::stacks_tx_id.eq(tx_id),
                    tx_stats_dsl::duration_us.eq(tx_metric.duration.as_micros() as i32),
                    tx_stats_dsl::estimated_commit_impact_us
                        .eq(tx_metric.estimated_commit_impact.as_micros() as i32),
                    tx_stats_dsl::clarity_write_length.eq(tx_metric.cost.write_length as i32),
                    tx_stats_dsl::clarity_write_count.eq(tx_metric.cost.write_count as i32),
                    tx_stats_dsl::clarity_read_length.eq(tx_metric.cost.read_length as i32),
                    tx_stats_dsl::clarity_read_count.eq(tx_metric.cost.read_count as i32),
                    tx_stats_dsl::clarity_runtime.eq(tx_metric.cost.runtime as i32),
                ));
            }
        }

        if !tx_stats_batch.is_empty() {
            for chunk in tx_stats_batch.chunks(500) {
                diesel::insert_into(tx_stats_dsl::stacks_tx_stats)
                    .values(chunk)
                    .execute(&mut *self.conn.lock().await)
                    .context("Failed to insert stacks tx stats chunk")?;
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
            .load(&mut *self.conn.lock().await)
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
        use self::schema::{burn_block, stacks_block};

        // Define an alias for the parent block table
        let parent_block = diesel::alias!(stacks_block as parent_block);

        // Fetch StacksBlock, joined BurnBlock, and joined ParentBlock (for hash)
        // We use left_join for parent because the genesis block has no parent.
        let (s_block, b_block, parent_hash_opt) = stacks_block::dsl::stacks_block
            .inner_join(burn_block::dsl::burn_block)
            .left_join(
                parent_block.on(stacks_block::dsl::parent_stacks_block_id
                    .eq(parent_block.field(stacks_block::dsl::id).nullable())),
            )
            .select((
                models::StacksBlock::as_select(),
                models::BurnBlock::as_select(),
                parent_block.field(stacks_block::dsl::index_hash).nullable(),
            ))
            .filter(stacks_block::dsl::index_hash.eq(id.as_bytes()))
            .first::<(models::StacksBlock, models::BurnBlock, Option<Vec<u8>>)>(
                &mut *self.conn.lock().await,
            )
            .optional()?
            .ok_or_else(|| anyhow!("Block {} not found in App DB", id))?;

        (s_block, b_block, parent_hash_opt).try_into()
    }

    pub async fn stage_blocks<I>(&mut self, blocks: I) -> Result<()>
    where
        I: IntoIterator<Item = StacksBlockHeader>,
    {
        use self::models::StagedStacksBlock;
        use self::schema::_staged_stacks_block;

        const CHUNK_SIZE: usize = 1000;
        let mut block_buffer = Vec::with_capacity(CHUNK_SIZE);

        self.conn
            .lock()
            .await
            .transaction::<_, anyhow::Error, _>(|conn| {
                for block in blocks {
                    block_buffer.push(StagedStacksBlock {
                        index_hash: block.id.0.to_vec(),
                        block_hash: block.hash.0.to_vec(),
                        parent_index_hash: block.parent_id.0.to_vec(),
                        height: block.height as i64,
                        burn_block_hash: block.burn_block_hash.0.to_vec(),
                        burn_block_height: block.burn_block_height as i64,
                    });

                    if block_buffer.len() >= CHUNK_SIZE {
                        diesel::insert_into(_staged_stacks_block::table)
                            .values(&block_buffer)
                            .execute(conn)?;
                        block_buffer.clear();
                    }
                }
                if !block_buffer.is_empty() {
                    diesel::insert_into(_staged_stacks_block::table)
                        .values(&block_buffer)
                        .execute(conn)?;
                }
                Ok(())
            })
    }

    pub async fn stage_transactions<I>(&mut self, blocks_with_txs: I) -> Result<()>
    where
        I: IntoIterator<Item = (StacksBlockHeader, BlockTransactions)>,
    {
        use self::models::{StagedContract, StagedPrincipal, StagedStacksTx, StagedStacksTxType};
        use self::schema::{_staged_principal, _staged_stacks_tx, _staged_stacks_tx_type};

        const CHUNK_SIZE: usize = 1000;

        let mut staged_tx_types = HashSet::new();
        let mut staged_principals = HashSet::new();
        let mut staged_contracts = HashSet::new();

        let flush = |conn: &mut SqliteConnection,
                     txs: &mut Vec<StagedStacksTx>,
                     tx_types: &mut HashSet<String>,
                     principals: &mut HashSet<String>,
                     contracts: &mut HashSet<(String, String)>|
         -> Result<()> {
            if !tx_types.is_empty() {
                let type_records = tx_types
                    .drain()
                    .map(|name| StagedStacksTxType { name })
                    .collect::<Vec<StagedStacksTxType>>();
                diesel::insert_into(_staged_stacks_tx_type::table)
                    .values(&type_records)
                    .execute(conn)?;
            }

            if !principals.is_empty() {
                let principal_records = principals
                    .drain()
                    .map(|address| StagedPrincipal { address })
                    .collect::<Vec<StagedPrincipal>>();
                diesel::insert_into(_staged_principal::table)
                    .values(&principal_records)
                    .execute(conn)?;
            }

            if !contracts.is_empty() {
                let contract_records = contracts
                    .drain()
                    .map(|(issuer_address, name)| models::StagedContract {
                        issuer_address,
                        name,
                    })
                    .collect::<Vec<StagedContract>>();
                diesel::insert_into(schema::_staged_contract::table)
                    .values(&contract_records)
                    .execute(conn)?;
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

        self.conn
            .lock()
            .await
            .transaction::<_, anyhow::Error, _>(|conn| {
                let mut block_iter = blocks_with_txs.into_iter();
                let mut tx_buffer = Vec::new();

                loop {
                    let chunk: Vec<(StacksBlockHeader, crate::BlockTransactions)> =
                        block_iter.by_ref().take(CHUNK_SIZE).collect();
                    if chunk.is_empty() {
                        break;
                    }

                    // ... processing logic ...
                    // (Reuse the processing logic from previous index_transactions)
                    let processed_results: Vec<_> = chunk
                        .iter()
                        .map(|(block, txs)| {
                            let mut p_txs = Vec::new();
                            let mut p_tx_types = HashSet::new();
                            let mut p_principals = HashSet::new();
                            let mut p_contracts = HashSet::new();

                            for tx in txs.iter() {
                                let block_index_hash = block.id.as_bytes().to_vec();
                                let tx_hash = tx.txid().as_bytes().to_vec();
                                let tx_type = tx.payload.name().to_string();

                                p_tx_types.insert(tx_type.clone());

                                let caller_address = tx.origin_address().to_string();
                                p_principals.insert(caller_address.clone());

                                let mut contract_issuer_address = None;
                                let mut contract_name = None;

                                if let TransactionPayload::SmartContract(sc, _) = &tx.payload {
                                    contract_issuer_address = Some(caller_address.clone());
                                    let c_name = sc.name.to_string();
                                    contract_name = Some(c_name.clone());
                                    p_contracts.insert((caller_address.clone(), c_name));
                                }

                                if let TransactionPayload::ContractCall(cc) = &tx.payload {
                                    let issuer_address =
                                        cc.contract_identifier().issuer.to_address().to_string();
                                    let name = cc.contract_name.to_string();

                                    p_principals.insert(issuer_address.clone());
                                    p_contracts.insert((issuer_address.clone(), name.clone()));

                                    contract_issuer_address = Some(issuer_address);
                                    contract_name = Some(name);
                                }

                                p_txs.push(StagedStacksTx {
                                    block_index_hash,
                                    tx_hash,
                                    tx_type,
                                    caller_address,
                                    contract_issuer_address,
                                    contract_name,
                                });
                            }
                            (p_txs, p_tx_types, p_principals, p_contracts)
                        })
                        .collect();

                    for (s_txs, s_types, s_principals, s_contracts) in processed_results {
                        tx_buffer.extend(s_txs);
                        staged_tx_types.extend(s_types);
                        staged_principals.extend(s_principals);
                        staged_contracts.extend(s_contracts);
                    }

                    flush(
                        conn,
                        &mut tx_buffer,
                        &mut staged_tx_types,
                        &mut staged_principals,
                        &mut staged_contracts,
                    )?;
                }
                Ok(())
            })
    }

    pub async fn merge_staging(&mut self) -> Result<()> {
        use self::schema::{
            _staged_contract, _staged_principal, _staged_stacks_block, _staged_stacks_tx,
            _staged_stacks_tx_type,
        };

        self.conn
            .lock()
            .await
            .transaction::<_, anyhow::Error, _>(|conn| {
                conn.batch_execute(MERGE_STAGING_SQL)?;

                // Cleanup
                diesel::delete(_staged_stacks_block::table).execute(conn)?;
                diesel::delete(_staged_stacks_tx::table).execute(conn)?;
                diesel::delete(_staged_stacks_tx_type::table).execute(conn)?;
                diesel::delete(_staged_principal::table).execute(conn)?;
                diesel::delete(_staged_contract::table).execute(conn)?;

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

    pub async fn save_profiler_data_batch(
        &mut self,
        run_id: i32,
        batch: &mut Vec<(
            StacksBlockId,
            Vec<stacks_profiler::ProfileStats>,
            Vec<crate::metrics::TransactionMetrics>,
        )>,
    ) -> Result<()> {
        use self::schema::{stacks_block, stacks_tx};

        // Take ownership of the batch data to process it
        let batch_data = std::mem::take(batch);

        self.conn.lock().await.transaction(|conn| {
            for (block_id, results, tx_metrics) in batch_data {
                // 1. Resolve Block ID (DB Primary Key)
                let block_pk: i64 = stacks_block::table
                    .select(stacks_block::id)
                    .filter(stacks_block::index_hash.eq(block_id.as_bytes()))
                    .first(conn)?;

                // 2. Resolve Tx IDs (DB Primary Keys) for mapping
                let tx_hashes: Vec<Vec<u8>> = tx_metrics
                    .iter()
                    .map(|tx| hex::decode(&tx.txid))
                    .collect::<Result<Vec<_>, _>>()?;

                let tx_lookup: HashMap<Vec<u8>, i64> = stacks_tx::table
                    .select((stacks_tx::tx_hash, stacks_tx::id))
                    .filter(stacks_tx::tx_hash.eq_any(&tx_hashes))
                    .load(conn)?
                    .into_iter()
                    .collect();

                // Map the metrics order to the resolved IDs
                let tx_db_ids: Vec<Option<i64>> = tx_hashes
                    .iter()
                    .map(|h| tx_lookup.get(h).cloned())
                    .collect();

                // 3. Recursive Insert Function
                fn insert_node(
                    conn: &mut SqliteConnection,
                    node: &stacks_profiler::ProfileStats,
                    run_id: i32,
                    parent_id: Option<i32>,
                    child_index: i32,
                    depth: i32,
                    block_pk: i64,
                    tx_db_ids: &[Option<i64>],
                    active_tx_id: Option<i64>,
                    span_cache: &mut HashMap<(Option<&'static str>, &'static str), i32>,
                    loc_cache: &mut HashMap<(String, i32), i32>,
                ) -> Result<()> {
                    // A. Resolve/Insert Location (With Caching)
                    let loc_id = AppDb::resolve_profiler_location(
                        conn,
                        loc_cache,
                        &node.source_file(),
                        node.source_line() as i32,
                    )?;

                    // B. Resolve/Insert Span Name (With Caching)
                    let span_id = AppDb::resolve_profiler_span(
                        conn,
                        span_cache,
                        node.context(),
                        &node.name(),
                    )?;

                    // C. Determine Context (Block vs Tx)
                    let mut current_tx_id = active_tx_id;
                    if node.name() == "Transaction" {
                        if let Some(tid) = tx_db_ids.get(child_index as usize).and_then(|x| *x) {
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
                            schema::profiler_record::benchmark_run_id.eq(run_id),
                            schema::profiler_record::parent_id.eq(parent_id),
                            schema::profiler_record::profiler_span_id.eq(span_id),
                            schema::profiler_record::tag.eq(&node.tag().map(|t| t.to_string())),
                            schema::profiler_record::profiler_location_id.eq(loc_id),
                            schema::profiler_record::child_index.eq(child_index),
                            schema::profiler_record::depth.eq(depth),
                            schema::profiler_record::stacks_block_id.eq(Some(block_pk)),
                            schema::profiler_record::stacks_tx_id.eq(current_tx_id),
                            schema::profiler_record::wall_time_us.eq(wall_time_us),
                            schema::profiler_record::cpu_time_us.eq(cpu_time_us),
                            schema::profiler_record::self_wall_time_us.eq(self_wall_time_us),
                            schema::profiler_record::self_cpu_time_us.eq(self_cpu_time_us),
                            schema::profiler_record::call_count.eq(node.count as i32),
                        ))
                        .returning(schema::profiler_record::id)
                        .get_result(conn)?;

                    // Recurse
                    for (idx, child) in node.children.iter().enumerate() {
                        insert_node(
                            conn,
                            child,
                            run_id,
                            Some(record_id),
                            idx as i32,
                            depth + 1,
                            block_pk,
                            tx_db_ids,
                            current_tx_id,
                            span_cache,
                            loc_cache,
                        )?;
                    }
                    Ok(())
                }

                // Start insertion for this block
                for (i, root) in results.iter().enumerate() {
                    insert_node(
                        conn,
                        root,
                        run_id,
                        None,
                        i as i32,
                        0,
                        block_pk,
                        &tx_db_ids,
                        None,
                        &mut self.profiler_span_cache,
                        &mut self.profiler_loc_cache,
                    )?;
                }
            }
            Ok(())
        })
    }

    fn resolve_profiler_location(
        conn: &mut SqliteConnection,
        cache: &mut HashMap<(String, i32), i32>,
        file: &str,
        line: i32,
    ) -> Result<i32> {
        use schema::profiler_location;

        let loc_key = (file.to_string(), line);
        if let Some(&id) = cache.get(&loc_key) {
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
            .optional()?;

        let id = if let Some(id) = id_opt {
            id
        } else {
            // Fallback: Select existing ID if insert did nothing
            profiler_location::table
                .select(profiler_location::id)
                .filter(profiler_location::file.eq(file))
                .filter(profiler_location::line.eq(line))
                .first(conn)?
        };

        cache.insert(loc_key, id);
        Ok(id)
    }

    fn resolve_profiler_span(
        conn: &mut SqliteConnection,
        cache: &mut HashMap<(Option<&'static str>, &'static str), i32>,
        context: Option<&'static str>,
        name: &'static str,
    ) -> Result<i32> {
        use schema::profiler_span;

        if let Some(&id) = cache.get(&(context, name)) {
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
            .optional()?;

        let id = if let Some(id) = id_opt {
            id
        } else {
            profiler_span::table
                .select(profiler_span::id)
                .filter(profiler_span::context.eq(context))
                .filter(profiler_span::name.eq(name))
                .first(conn)?
        };

        cache.insert((context, name), id);
        Ok(id)
    }
}

impl ChainCache for AppDb {
    async fn find_closest_ancestor(
        &self,
        tip: &StacksBlockId,
        target_height: u64,
    ) -> Result<Option<(StacksBlockId, u64)>> {
        use self::schema::chain_tip_cache;

        // Find the block with the smallest height that is still >= target_height
        // This gives us the closest point we can jump to without overshooting.
        let result = chain_tip_cache::table
            .select((chain_tip_cache::index_hash, chain_tip_cache::height))
            .filter(chain_tip_cache::tip_index_hash.eq(tip.as_bytes()))
            .filter(chain_tip_cache::height.ge(target_height as i64))
            .order(chain_tip_cache::height.asc())
            .first::<(Vec<u8>, i64)>(&mut *self.conn.lock().await)
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
        use self::schema::chain_tip_cache;

        diesel::insert_into(chain_tip_cache::table)
            .values((
                chain_tip_cache::tip_index_hash.eq(tip.as_bytes()),
                chain_tip_cache::height.eq(height as i64),
                chain_tip_cache::index_hash.eq(block.as_bytes()),
            ))
            .on_conflict((chain_tip_cache::tip_index_hash, chain_tip_cache::height))
            .do_nothing()
            .execute(&mut *self.conn.lock().await)?;
        Ok(())
    }
}
