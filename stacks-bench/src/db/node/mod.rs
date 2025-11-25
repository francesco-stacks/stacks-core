use std::{marker::PhantomData, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use diesel::{Connection, RunQueryDsl, SqliteConnection};

use crate::db::{ReadOnly, ReadWrite};

pub mod models;
pub mod schemas;

pub struct ChainStateDb<Mode> {
    _path: PathBuf,
    conn: SqliteConnection,
    _mode: PhantomData<Mode>,
}

impl ChainStateDb<ReadOnly> {
    pub fn open_read_only(path: PathBuf) -> Result<ChainStateDb<ReadOnly>> {
        let path_str = path.to_str()
            .ok_or_else(|| anyhow!("Invalid database path: {:?}", path))?;
        
        // Use URI syntax to enforce read-only at SQLite level
        let conn_str = format!("file:{}?mode=ro", path_str); 
        
        let conn = SqliteConnection::establish(&conn_str)
            .with_context(|| format!("Failed to open chainstate DB at {:?}", path))?;
            
        Ok(ChainStateDb {
            _path: path,
            conn,
            _mode: PhantomData,
        })
    }
}

impl ChainStateDb<ReadWrite> {
    pub fn open_read_write(path: PathBuf) -> Result<ChainStateDb<ReadWrite>> {
        let path_str = path.to_str()
            .ok_or_else(|| anyhow!("Invalid database path: {:?}", path))?;

        let conn = SqliteConnection::establish(path_str)
            .with_context(|| format!("Failed to open chainstate DB at {:?}", path))?;

        Ok(ChainStateDb {
            _path: path,
            conn,
            _mode: PhantomData,
        })
    }
}

impl<Mode> ChainStateDb<Mode> {
    pub fn read_db_config(&mut self) -> Result<models::chainstate::DbConfig> {
        use schemas::chainstate::db_config::dsl::db_config;
        db_config.first::<models::chainstate::DbConfig>(&mut self.conn)
            .with_context(|| "Failed to query chainstate 'db_config' table")
    }
}