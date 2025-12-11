use std::marker::PhantomData;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use diesel::prelude::*;

pub mod app;
pub mod node;

/// Marker type for read-only database access
pub struct ReadOnly;
/// Marker type for read-write database access
pub struct ReadWrite;

/// Trait for opening a database in a specific mode (read-only, read-write).
pub trait DbOpen<Mode>: Sized {
    fn open(path: PathBuf) -> Result<Self>;
}

pub trait DbOpenForRead: Sized {
    fn open_for_read(path: PathBuf) -> Result<Self>;
}

pub trait DbOpenForWrite: Sized {
    fn open_for_write(path: PathBuf) -> Result<Self>;
}

impl<T> DbOpenForRead for T
where
    T: DbOpen<ReadOnly>,
{
    fn open_for_read(path: PathBuf) -> Result<T> {
        T::open(path)
    }
}

impl<T> DbOpenForWrite for T
where
    T: DbOpen<ReadWrite>,
{
    fn open_for_write(path: PathBuf) -> Result<T> {
        T::open(path)
    }
}

/// Trait for accessing a handle's underlying connection.
pub trait DbConn<Mode> {
    type DbConnection: Connection;

    fn conn(&self) -> &Self::DbConnection;
    fn conn_mut(&mut self) -> &mut Self::DbConnection;
}

/// A generic handle to a SQLite database connection.
pub struct SqliteDbHandle<Mode> {
    _path: PathBuf,
    conn: SqliteConnection,
    _mode: PhantomData<Mode>,
}

impl<Mode> DbConn<Mode> for SqliteDbHandle<Mode> {
    type DbConnection = SqliteConnection;

    fn conn(&self) -> &Self::DbConnection {
        &self.conn
    }

    fn conn_mut(&mut self) -> &mut Self::DbConnection {
        &mut self.conn
    }
}

impl DbOpen<ReadOnly> for SqliteDbHandle<ReadOnly> {
    fn open(path: PathBuf) -> Result<Self> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid database path: {:?}", path))?;

        let conn_str = format!("file:{}?mode=ro", path_str);

        let mut conn = SqliteConnection::establish(&conn_str)
            .with_context(|| format!("Failed to open SQLite DB (ReadOnly) at {:?}", path))?;

        // Set busy timeout to 10s to handle concurrent access/locking
        diesel::sql_query("PRAGMA busy_timeout = 1000")
            .execute(&mut conn)
            .context("Failed to set busy_timeout")?;

        Ok(SqliteDbHandle {
            _path: path,
            conn,
            _mode: PhantomData,
        })
    }
}

impl DbOpen<ReadWrite> for SqliteDbHandle<ReadWrite> {
    fn open(path: PathBuf) -> Result<Self> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid database path: {:?}", path))?;

        let mut conn = SqliteConnection::establish(path_str)
            .with_context(|| format!("Failed to open SQLite DB (ReadWrite) at {:?}", path))?;

        // Set busy timeout to 10s to handle concurrent access/locking
        diesel::sql_query("PRAGMA busy_timeout = 10000")
            .execute(&mut conn)
            .context("Failed to set busy_timeout")?;

        Ok(SqliteDbHandle {
            _path: path,
            conn,
            _mode: PhantomData,
        })
    }
}
