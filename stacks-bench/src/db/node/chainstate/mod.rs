use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use anyhow::{Context, Result};
use clarity::types::chainstate::StacksBlockId;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::blocks::BlockHeaderProvider;
use crate::db::{DbOpen, SqliteDbHandle};

pub mod models;
pub mod schema;

#[derive(Clone)]
pub struct ChainStateDb<Mode> {
    handle: SqliteDbHandle<Mode>,
}

impl<Mode> Deref for ChainStateDb<Mode> {
    type Target = SqliteDbHandle<Mode>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<Mode> DerefMut for ChainStateDb<Mode> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

// Generic implementation: If SqliteDbHandle can be opened in Mode,
// then ChainStateDb can also be opened in Mode.
impl<Mode> DbOpen<Mode> for ChainStateDb<Mode>
where
    SqliteDbHandle<Mode>: DbOpen<Mode>,
{
    async fn open<P: AsRef<Path> + Debug + Send>(path: P) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::<Mode>::open(path).await?,
        })
    }
}

impl<Mode> ChainStateDb<Mode> {
    pub async fn read_db_config(&self) -> Result<models::DbConfig> {
        use schema::db_config::dsl::db_config;
        let mut conn = self.handle.get_conn().await?;
        db_config
            .first::<models::DbConfig>(&mut conn)
            .await
            .with_context(|| "Failed to query chainstate 'db_config' table")
    }

    /// Tries to find a block header in either `nakamoto_block_headers` or `block_headers`.
    pub async fn get_block_header(
        &self,
        block_id: &StacksBlockId,
    ) -> Result<Option<models::BlockHeader>> {
        use self::schema::{block_headers, nakamoto_block_headers};
        let index_hash_hex = block_id.to_hex();
        let mut conn = self.handle.get_conn().await?;

        let q1 = nakamoto_block_headers::table
            .select((
                nakamoto_block_headers::index_block_hash,
                nakamoto_block_headers::block_hash,
                nakamoto_block_headers::parent_block_id,
                nakamoto_block_headers::block_height,
                nakamoto_block_headers::consensus_hash,
                nakamoto_block_headers::burn_header_hash,
                nakamoto_block_headers::burn_header_height,
            ))
            .filter(nakamoto_block_headers::index_block_hash.eq(&index_hash_hex));

        let q2 = block_headers::table
            .select((
                block_headers::index_block_hash,
                block_headers::block_hash,
                block_headers::parent_block_id,
                block_headers::block_height,
                block_headers::consensus_hash,
                block_headers::burn_header_hash,
                block_headers::burn_header_height,
            ))
            .filter(block_headers::index_block_hash.eq(&index_hash_hex));

        // UNION ALL with LIMIT 1 is efficient: SQLite checks the first query,
        // and if it finds a match, it stops immediately without checking the second.
        q1.union_all(q2)
            .first::<models::BlockHeader>(&mut conn)
            .await
            .optional()
            .with_context(|| {
                format!("Failed to query block header for block with index hash '{index_hash_hex}'")
            })
    }
}

impl<Mode: Send> BlockHeaderProvider for ChainStateDb<Mode> {
    async fn get_header(&mut self, id: &StacksBlockId) -> Result<Option<crate::StacksBlockHeader>> {
        let header = self.get_block_header(id).await?;
        match header {
            Some(h) => Ok(Some(h.try_into()?)),
            None => Ok(None),
        }
    }
}
