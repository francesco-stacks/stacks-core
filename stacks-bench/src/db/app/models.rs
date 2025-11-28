use anyhow::Result;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use stacks_common::types::StacksEpochId;

use super::schema::{
    _staged_stacks_block, _staged_stacks_tx, benchmark_run, burn_block, chainstate, epoch, network,
    profiler_location, profiler_record, profiler_span, stacks_block, stacks_block_stats, stacks_tx,
    stacks_tx_stats,
};
use crate::ResolveEpochFromHeight;

#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = network)]
pub struct Network {
    pub id: i32,
    pub name: String,
}

impl Network {
    pub const MAINNET: i32 = 1;
    pub const TESTNET: i32 = 2;
    pub const REGTEST: i32 = 3;
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(Network))]
#[diesel(table_name = chainstate)]
pub struct Chainstate {
    pub id: i32,
    pub network_id: i32,
    pub chain_id: i64,
    pub tip_index_hash: Vec<u8>,
    pub tip_height: i64,
    pub epochs_hash: Vec<u8>,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = chainstate)]
pub struct NewChainstate {
    pub network_id: i32,
    pub chain_id: i64,
    pub tip_index_hash: Vec<u8>,
    pub tip_height: i64,
    pub epochs_hash: Vec<u8>,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(Chainstate))]
#[diesel(table_name = epoch)]
pub struct Epoch {
    pub id: i32,
    pub chainstate_id: i32,
    pub stacks_epoch_id: i32,
    pub network_epoch_id: i32,
    pub start_height: i64,
    pub end_height: i64,
    pub write_length_budget: i64,
    pub write_count_budget: i64,
    pub read_length_budget: i64,
    pub read_count_budget: i64,
    pub runtime_budget: i64,
}

impl Epoch {
    pub fn try_get_stacks_epoch_id(&self) -> Result<StacksEpochId> {
        (self.stacks_epoch_id as u32)
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid StacksEpochId: {}", self.stacks_epoch_id))
    }
}

impl ResolveEpochFromHeight for [Epoch] {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId> {
        let height_i64: i64 = height.try_into().ok()?;
        for epoch in self {
            if height_i64 >= epoch.start_height && height_i64 <= epoch.end_height {
                let epoch_id_u32: u32 = epoch.stacks_epoch_id.try_into().ok()?;
                let stacks_epoch_id: StacksEpochId = epoch_id_u32.try_into().ok()?;
                return Some(stacks_epoch_id);
            }
        }
        None
    }
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = epoch)]
pub struct NewEpoch {
    pub chainstate_id: i32,
    pub stacks_epoch_id: i32,
    pub network_epoch_id: i32,
    pub start_height: i64,
    pub end_height: i64,
    pub write_length_budget: i64,
    pub write_count_budget: i64,
    pub read_length_budget: i64,
    pub read_count_budget: i64,
    pub runtime_budget: i64,
}

#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = burn_block)]
pub struct BurnBlock {
    pub id: i64,
    pub block_hash: Vec<u8>,
    pub height: i64,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = burn_block)]
pub struct NewBurnBlock {
    pub block_hash: Vec<u8>,
    pub height: i64,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(BurnBlock))]
#[diesel(table_name = stacks_block)]
pub struct StacksBlock {
    pub id: i64,
    pub index_hash: Vec<u8>,
    pub height: i64,
    pub parent_stacks_block_id: Option<i64>,
    pub burn_block_id: i64,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = stacks_block)]
pub struct NewStacksBlock {
    pub index_hash: Vec<u8>,
    pub height: i64,
    pub parent_stacks_block_id: Option<i64>,
    pub burn_block_id: i64,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = _staged_stacks_block)]
pub struct StagedStacksBlock {
    pub index_hash: Vec<u8>,
    pub parent_index_hash: Vec<u8>,
    pub height: i64,
    pub burn_block_hash: Vec<u8>,
    pub burn_block_height: i64,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(StacksBlock))]
#[diesel(table_name = stacks_tx)]
pub struct StacksTx {
    pub id: i64,
    pub stacks_block_id: i64,
    pub tx_hash: Vec<u8>,
    pub tx_type: String,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = _staged_stacks_tx)]
pub struct StagedStacksTx {
    pub block_index_hash: Vec<u8>,
    pub tx_hash: Vec<u8>,
    pub tx_type: String,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = stacks_tx)]
pub struct NewStacksTx {
    pub stacks_block_id: i64,
    pub tx_hash: Vec<u8>,
    pub tx_type: String,
}

// Keep Queryable as Value (Diesel can deserialize Text -> Value automatically if feature is on,
// or we can use String and deserialize manually)
#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(Chainstate))]
#[diesel(table_name = benchmark_run)]
pub struct BenchmarkRun {
    pub id: i32,
    pub run_name: Option<String>,
    pub chainstate_id: i32,
    pub git_commit_hash: Vec<u8>,
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
    pub args_json: String,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = benchmark_run)]
pub struct NewBenchmarkRun {
    pub run_name: Option<String>,
    pub chainstate_id: i32,
    pub git_commit_hash: Vec<u8>,
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
    pub args_json: String,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(BenchmarkRun))]
#[diesel(belongs_to(StacksBlock))]
#[diesel(table_name = stacks_block_stats)]
pub struct StacksBlockStats {
    pub id: i64,
    pub benchmark_run_id: i32,
    pub stacks_block_id: i64,
    pub total_duration_us: i32,
    pub setup_duration_us: i32,
    pub execution_duration_us: i32,
    pub commit_duration_us: i32,
    pub commit_overhead_baseline_us: i32,
    pub clarity_write_length: i32,
    pub clarity_write_count: i32,
    pub clarity_read_length: i32,
    pub clarity_read_count: i32,
    pub clarity_runtime: i32,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = stacks_block_stats)]
pub struct NewStacksBlockStats {
    pub benchmark_run_id: i32,
    pub stacks_block_id: i64,
    pub total_duration_us: i32,
    pub setup_duration_us: i32,
    pub execution_duration_us: i32,
    pub commit_duration_us: i32,
    pub commit_overhead_baseline_us: i32,
    pub clarity_write_length: i32,
    pub clarity_write_count: i32,
    pub clarity_read_length: i32,
    pub clarity_read_count: i32,
    pub clarity_runtime: i32,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(BenchmarkRun))]
#[diesel(belongs_to(StacksTx))]
#[diesel(table_name = stacks_tx_stats)]
pub struct StacksTxStats {
    pub id: i64,
    pub benchmark_run_id: i32,
    pub stacks_tx_id: i64,
    pub duration_us: i32,
    pub estimated_commit_impact_us: i32,
    pub clarity_write_length: i32,
    pub clarity_write_count: i32,
    pub clarity_read_length: i32,
    pub clarity_read_count: i32,
    pub clarity_runtime: i32,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = stacks_tx_stats)]
pub struct NewStacksTxStats {
    pub benchmark_run_id: i32,
    pub stacks_tx_id: i64,
    pub duration_us: i32,
    pub estimated_commit_impact_us: i32,
    pub clarity_write_length: i32,
    pub clarity_write_count: i32,
    pub clarity_read_length: i32,
    pub clarity_read_count: i32,
    pub clarity_runtime: i32,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = profiler_location)]
pub struct NewProfilerLocation<'a> {
    pub file: &'a str,
    pub line: i32,
}

#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = profiler_location)]
pub struct ProfilerLocation {
    pub id: i32,
    pub file: String,
    pub line: i32,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = profiler_span)]
pub struct NewProfilerSpan<'a> {
    pub name: &'a str,
}

#[derive(Queryable, Selectable, Identifiable, Debug, Clone)]
#[diesel(table_name = profiler_span)]
pub struct ProfilerSpan {
    pub id: i32,
    pub name: String,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = profiler_record)]
pub struct NewProfilerRecord {
    pub benchmark_run_id: i32,
    pub parent_id: Option<i32>,
    pub profiler_span_id: i32,
    pub profiler_location_id: i32,
    pub child_index: i32,
    pub depth: i32,
    pub stacks_block_id: Option<i64>,
    pub stacks_tx_id: Option<i64>,
    pub wall_time_us: i64,
    pub cpu_time_us: i64,
    pub call_count: i32,
}

#[derive(Queryable, Selectable, Identifiable, Associations, Debug, Clone)]
#[diesel(belongs_to(BenchmarkRun))]
#[diesel(belongs_to(ProfilerSpan))]
#[diesel(belongs_to(ProfilerLocation))]
#[diesel(belongs_to(StacksBlock))]
#[diesel(belongs_to(StacksTx))]
#[diesel(belongs_to(ProfilerRecord, foreign_key = parent_id))]
#[diesel(table_name = profiler_record)]
pub struct ProfilerRecord {
    pub id: i32,
    pub benchmark_run_id: i32,
    pub parent_id: Option<i32>,
    pub profiler_span_id: i32,
    pub profiler_location_id: i32,
    pub child_index: i32,
    pub depth: i32,
    pub stacks_block_id: Option<i64>,
    pub stacks_tx_id: Option<i64>,
    pub wall_time_us: i64,
    pub cpu_time_us: i64,
    pub call_count: i32,
}
