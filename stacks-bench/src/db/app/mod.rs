use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDateTime;
use clarity::types::chainstate::StacksBlockId;
use diesel::prelude::*;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

use crate::{Block, Network};

pub mod schema;
pub mod models;

// This macro embeds the SQL files from the "migrations" directory into the binary
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

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
        let database_url = path_ref.to_str()
            .ok_or_else(|| anyhow!("Invalid database path (non-UTF8): {:?}", path_ref))?;
        
        let mut conn = SqliteConnection::establish(database_url)
            .with_context(|| format!("Failed to connect to app DB at {}", database_url))?;

        // 1. Run Migrations (Create tables if they don't exist)
        // This will automatically apply the SQL defined in step 2
        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow!("Failed to run database migrations: {}", e))?;

        // 2. Ensure foreign keys are enforced (SQLite defaults to OFF)
        diesel::sql_query("PRAGMA foreign_keys = ON").execute(&mut conn)?;

        // Ensure foreign keys are enforced
        diesel::sql_query("PRAGMA foreign_keys = ON").execute(&mut conn)?;

        Ok(AppDb { conn })
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
        tip_height: u32,
    ) -> Result<models::Chainstate> {
        use self::schema::chainstate::dsl;

        let network_id = Self::resolve_network_id(network);
        let chain_id_i32 = chain_id.try_into()
            .context(format!("Failed to convert u32 chain_id to i32: {chain_id}"))?;
        let tip_height_i32 = tip_height.try_into()
            .context(format!("Failed to convert u32 tip_height to i32: {tip_height}"))?;

        self.conn.transaction(|conn| {
            let query = dsl::chainstate
                .filter(dsl::network_id.eq(network_id))
                .filter(dsl::chain_id.eq(chain_id_i32))
                .filter(dsl::tip_index_hash.eq(tip_block_id.as_bytes()));

            if let Some(existing) = query.first::<models::Chainstate>(conn).optional()? {
                Ok(existing)
            } else {
                let new_chainstate = models::NewChainstate {
                    network_id: network_id,
                    chain_id: chain_id_i32,
                    tip_index_hash: tip_block_id.0.to_vec(),
                    tip_height: tip_height_i32,
                };
                diesel::insert_into(dsl::chainstate)
                    .values(&new_chainstate)
                    .get_result(conn)
            }
        }).context("Failed to get or create chainstate")
    }

    pub fn get_or_create_burn_block(&mut self, hash: &[u8], height: u32) -> Result<models::BurnBlock> {
        use self::schema::burn_block::dsl;

        let height_i64: i64 = height.into();

        self.conn.transaction(|conn| {
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
        }).context("Failed to get or create burn_block")
    }

    pub fn get_or_create_stacks_block(&mut self, block: &Block) -> Result<models::StacksBlock> {
        use self::schema::stacks_block::dsl;

        let height_i64: i64 = block.height.into();
        
        // 1. Resolve Burn Block ID
        let burn_hash = block.burn_block_hash.as_ref()
            .ok_or_else(|| anyhow!("Block {} missing burn block hash", block.id))?;
        let burn_height = block.burn_block_height
            .ok_or_else(|| anyhow!("Block {} missing burn block height", block.id))?;
        
        let burn_block = self.get_or_create_burn_block(&burn_hash.0, burn_height)?;

        // 2. Try to resolve Parent ID (if it exists in DB)
        // We do this inside the transaction to ensure consistency
        self.conn.transaction(|conn| {
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
        }).context("Failed to get or create stacks_block")
    }

    pub fn get_or_create_stacks_tx(
        &mut self,
        block_id: i64,
        hash: &[u8],
        type_str: &str,
    ) -> Result<models::StacksTx> {
        use self::schema::stacks_tx::dsl::*;

        self.conn.transaction(|conn| {
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
        }).context("Failed to get or create stacks_tx")
    }

    pub fn create_benchmark_run(&mut self, new_run: models::NewBenchmarkRun) -> Result<models::BenchmarkRun> {
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

    pub fn insert_stacks_block_stats(&mut self, stats: &[models::NewStacksBlockStats]) -> Result<()> {
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
}