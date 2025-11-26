use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::Result;
use diesel::RunQueryDsl;

use crate::db::{DbConn, DbOpen, SqliteDbHandle};

pub mod models;
pub mod schema;

pub struct SortitionDb<Mode> {
    handle: SqliteDbHandle<Mode>,
}

impl<Mode> Deref for SortitionDb<Mode> {
    type Target = SqliteDbHandle<Mode>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<Mode> DerefMut for SortitionDb<Mode> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl<Mode> DbOpen<Mode> for SortitionDb<Mode>
where
    SqliteDbHandle<Mode>: DbOpen<Mode>,
{
    fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::open(path)?,
        })
    }
}

impl<Mode> SortitionDb<Mode> {
    pub fn get_epochs(&mut self) -> Result<Vec<models::Epoch>> {
        use schema::epochs::dsl::*;

        epochs
            .load::<models::Epoch>(self.conn_mut())
            .map_err(|e| anyhow::anyhow!("Failed to load epochs from sortition db: {}", e))
    }
}
