use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::{Context, Result};
use diesel::prelude::*;

use crate::db::{DbOpen, SqliteDbHandle};

pub mod models;
pub mod schema;

pub struct NakamotoDb<Mode> {
    handle: SqliteDbHandle<Mode>,
}

impl<Mode> Deref for NakamotoDb<Mode> {
    type Target = SqliteDbHandle<Mode>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<Mode> DerefMut for NakamotoDb<Mode> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

// Generic implementation: If SqliteDbHandle can be opened in Mode,
// then NakamotoDb can also be opened in Mode.
impl<Mode> DbOpen<Mode> for NakamotoDb<Mode>
where
    SqliteDbHandle<Mode>: DbOpen<Mode>,
{
    fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::<Mode>::open(path)?,
        })
    }
}

impl<Mode> NakamotoDb<Mode> {
    pub fn get_nakamoto_block(
        &mut self,
        id: &stacks_common::types::chainstate::StacksBlockId,
    ) -> Result<Option<models::NakamotoStagingBlock>> {
        use self::schema::nakamoto_staging_blocks;

        let id_str = id.to_string();

        nakamoto_staging_blocks::table
            .filter(nakamoto_staging_blocks::index_block_hash.eq(id_str))
            .first(&mut self.conn)
            .optional()
            .with_context(|| format!("Failed to query nakamoto_staging_blocks for id {id}"))
    }

    pub fn get_min_block_height(&mut self) -> Result<Option<u64>> {
        use diesel::dsl::min;

        use self::schema::nakamoto_staging_blocks::dsl::*;

        nakamoto_staging_blocks
            .select(min(height))
            .first::<Option<i32>>(&mut self.conn)
            .map(|opt| opt.map(|h| h as u64))
            .context("Failed to get min block height from nakamoto db")
    }
}
