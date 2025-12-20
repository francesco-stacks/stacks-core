use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clarity::types::chainstate::StacksBlockId;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use futures::Stream;

use crate::StacksBlockHeader;
use crate::blocks::{BackwardsBlockStream, BlockHeaderProvider};
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

impl<Mode: Send + Sync + 'static> ChainStateDb<Mode> {
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

    /// Stream canonical headers by walking parent links from `tip_id`,
    /// yielding headers whose height is in [start_height, end_height] (descending).
    ///
    /// Note: this still performs per-header lookups (via `get_header()`), but it avoids
    /// the extra pre-walk done by BenchContext::initialize().
    pub fn canonical_block_stream_from_tip(
        &self,
        tip_id: StacksBlockId,
        start_height: u64,
        end_height: u64,
    ) -> impl Stream<Item = Result<StacksBlockHeader>> {
        let stream = BackwardsBlockStream::new(self, tip_id);

        Box::pin(futures::stream::unfold(stream, move |mut bs| async move {
            loop {
                match bs.next_block().await {
                    Ok(Some(header)) => {
                        if header.height < start_height {
                            return None;
                        }
                        if header.height <= end_height {
                            return Some((Ok(header), bs));
                        }
                        // above end_height: keep walking
                    }
                    Ok(None) => return Some((Err(anyhow!("Missing header")), bs)),
                    Err(e) => return Some((Err(e), bs)),
                }
            }
        }))
    }
}

impl<Mode: Send + Sync + 'static> BlockHeaderProvider for ChainStateDb<Mode> {
    async fn get_header(&self, id: &StacksBlockId) -> Result<Option<crate::StacksBlockHeader>> {
        let header = self.get_block_header(id).await?;
        match header {
            Some(h) => Ok(Some(h.try_into()?)),
            None => Ok(None),
        }
    }
}

impl<Mode: Send + Sync + 'static> BlockHeaderProvider for &ChainStateDb<Mode> {
    async fn get_header(&self, id: &StacksBlockId) -> Result<Option<crate::StacksBlockHeader>> {
        let header = self.get_block_header(id).await?;
        match header {
            Some(h) => Ok(Some(h.try_into()?)),
            None => Ok(None),
        }
    }
}
