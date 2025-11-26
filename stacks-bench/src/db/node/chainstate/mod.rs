use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::{Context, Result};
use diesel::RunQueryDsl;

use crate::db::{DbOpen, SqliteDbHandle};

pub mod models;
pub mod schema;

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
    fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::<Mode>::open(path)?,
        })
    }
}

impl<Mode> ChainStateDb<Mode> {
    pub fn read_db_config(&mut self) -> Result<models::DbConfig> {
        use schema::db_config::dsl::db_config;
        db_config
            .first::<models::DbConfig>(&mut self.conn)
            .with_context(|| "Failed to query chainstate 'db_config' table")
    }
}
