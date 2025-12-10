use anyhow::{Result, anyhow};
use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql, FromSqlRow};
use diesel::prelude::*;
use diesel::sql_types::Text;
use diesel::sqlite::Sqlite;
use serde::Deserialize;
use stacks_common::types::StacksEpochId;

use super::schema;
use crate::ResolveEpochFromHeight;

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
            .map_err(|e| anyhow!("Invalid StacksEpochId '{}': {e}", self.epoch_id))
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

impl ResolveEpochFromHeight for [Epoch] {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId> {
        let height_i64: i64 = height.try_into().ok()?;
        for epoch in self {
            if height_i64 >= epoch.start_block_height && height_i64 <= epoch.end_block_height {
                let epoch_id_u32: u32 = epoch.epoch_id.try_into().ok()?;
                let stacks_epoch_id: StacksEpochId = epoch_id_u32.try_into().ok()?;
                return Some(stacks_epoch_id);
            }
        }
        None
    }
}

#[derive(Queryable, Selectable, Debug, Clone)]
#[diesel(table_name = schema::snapshots)]
pub struct Snapshot {
    pub sortition_id: String,
    pub block_height: i64,
    pub burn_header_hash: String,
    pub parent_sortition_id: String,
    pub canonical_stacks_tip_hash: String,
    pub canonical_stacks_tip_consensus_hash: String,
    pub canonical_stacks_tip_height: i64,
    pub pox_valid: i32,
}

#[derive(Queryable, Selectable, Debug, Clone)]
#[diesel(table_name = schema::stacks_chain_tips)]
pub struct StacksChainTip {
    pub sortition_id: String,
    pub consensus_hash: String,
    pub block_hash: String,
    pub block_height: i64,
}
