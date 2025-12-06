use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::{Context, Result};
use diesel::RunQueryDsl;
use diesel::sql_types::Integer;

use crate::db::{DbOpen, ReadWrite, SqliteDbHandle};

pub mod models;
pub mod schema;

// Define a struct to map the PRAGMA result
#[derive(diesel::QueryableByName, Debug)]
pub struct CheckpointResult {
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "busy")]
    pub busy: i32,
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "log")]
    pub log: i32,
    #[diesel(sql_type = Integer)]
    #[diesel(column_name = "checkpointed")]
    pub checkpointed: i32,
}

pub struct ClarityDb<Mode> {
    handle: SqliteDbHandle<Mode>,
}

impl<Mode> Deref for ClarityDb<Mode> {
    type Target = SqliteDbHandle<Mode>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<Mode> DerefMut for ClarityDb<Mode> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

/// Generic implementation: If [`SqliteDbHandle`] can be opened in Mode,
/// then [`ClarityDb`] can also be opened in Mode.
impl<Mode> DbOpen<Mode> for ClarityDb<Mode>
where
    SqliteDbHandle<Mode>: DbOpen<Mode>,
{
    fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::<Mode>::open(path)?,
        })
    }
}

impl ClarityDb<ReadWrite> {
    pub fn checkpoint(&mut self) -> Result<()> {
        let results: Vec<CheckpointResult> = diesel::sql_query("PRAGMA wal_checkpoint(FULL)")
            .load(&mut self.conn)
            .context("Failed to perform WAL checkpoint")?;

        if let Some(res) = results.first() {
            // Print status regardless of busy state to debug
            eprintln!(
                "Checkpoint Status: busy={}, log={}, checkpointed={}",
                res.busy, res.log, res.checkpointed
            );
        }
        Ok(())
    }

    pub fn vacuum(&mut self) -> Result<()> {
        diesel::sql_query("VACUUM")
            .execute(&mut self.conn)
            .context("Failed to vacuum the database")?;
        Ok(())
    }
}
