use anyhow::Result;
use clarity::types::StacksEpochId;
use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql, FromSqlRow};
use diesel::prelude::*;
use diesel::sql_types::Text;
use diesel::sqlite::Sqlite;
use serde::Deserialize;

use super::schema;

#[derive(Debug, Deserialize, Clone, FromSqlRow)]
pub struct ExecutionCost {
    pub write_length: u64,
    pub write_count: u64,
    pub read_length: u64,
    pub read_count: u64,
    pub runtime: u64,
}

impl FromSql<Text, Sqlite> for ExecutionCost {
    fn from_sql(bytes: <Sqlite as Backend>::RawValue<'_>) -> deserialize::Result<Self> {
        let s = <String as FromSql<Text, Sqlite>>::from_sql(bytes)?;
        let cost = serde_json::from_str(&s)?;
        Ok(cost)
    }
}

#[derive(Queryable, Selectable, Debug, Clone)]
#[diesel(table_name = schema::epochs)]
pub struct Epoch {
    start_block_height: i64,
    end_block_height: i64,
    epoch_id: i32,
    #[diesel(column_name = block_limit)]
    pub block_limits: ExecutionCost,
    network_epoch: i32,
}

impl Epoch {
    pub fn epoch_id(&self) -> u32 {
        self.epoch_id as u32
    }

    pub fn to_stacks_epoch_id(&self) -> Result<StacksEpochId> {
        self.epoch_id()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid StacksEpochId: {}", self.epoch_id))
    }

    pub fn network_epoch_id(&self) -> u32 {
        self.network_epoch as u32
    }

    pub fn start_block_height(&self) -> u64 {
        self.start_block_height as u64
    }

    pub fn end_block_height(&self) -> u64 {
        self.end_block_height as u64
    }
}

impl TryFrom<&Epoch> for crate::StacksEpoch {
    type Error = anyhow::Error;
    fn try_from(epoch: &Epoch) -> Result<Self> {
        Ok(Self {
            epoch_id: epoch.to_stacks_epoch_id()?,
            start_block_height: epoch.start_block_height(),
            end_block_height: epoch.end_block_height(),
        })
    }
}
