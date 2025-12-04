use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::nakamoto::miner::{MinerTenureInfoCause, NakamotoBlockBuilder};
use blockstack_lib::chainstate::stacks::StacksBlock;
use blockstack_lib::chainstate::stacks::db::{ClarityTx, StacksChainState};
use blockstack_lib::chainstate::stacks::miner::{
    BlockBuilder, BlockLimitFunction, TransactionResult,
};
use clarity::codec::StacksMessageCodec;
use clarity::types::chainstate::ConsensusHash;
use clarity::vm::costs::ExecutionCost;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_profiler::{Profiler, profile_scope};

use crate::context::BenchContext;
use crate::metrics::{BlockMetrics, TransactionMetrics};
use crate::{BlockEra, BlockSummary};

pub enum ReplayMode {
    Miner,
    Follower,
    Ephemeral,
}

/// Re-execute all transactions in a block to measure execution performance.
pub fn replay_block(
    context: &mut BenchContext,
    mode: ReplayMode,
    block_summary: &BlockSummary,
) -> Result<BlockMetrics> {
    match context.resolve_block_era(block_summary.epoch) {
        BlockEra::Nakamoto => {
            let (naka_block, _size) = context
                .chainstate()
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block_summary.id)?
                .ok_or_else(|| anyhow!("Nakamoto block not found"))?;

            match mode {
                ReplayMode::Miner => {
                    // Currently not implemented in this refactor
                    Err(anyhow!("Nakamoto Miner replay not implemented"))
                }
                ReplayMode::Follower => {
                    profile_scope!("Block Replay (Nakamoto Follower)", {
                        re_execute_nakamoto_follower(context, &naka_block)
                    })
                }
                ReplayMode::Ephemeral => {
                    // Currently not implemented in this refactor
                    Err(anyhow!("Nakamoto Ephemeral replay not implemented"))
                }
            }
        }
        BlockEra::PreNakamoto => {
            let blocks_path = context.chainstate().blocks_path.clone();
            // Pre-Nakamoto metrics not fully implemented in this refactor yet
            // Returning empty metrics for now to satisfy signature
            let (consensus_hash, header_hash) = context
                .chainstate()
                .get_block_header_hashes(&block_summary.id)?
                .ok_or_else(|| anyhow!("Hashes not found"))?;
            let bytes =
                StacksChainState::load_block_bytes(&blocks_path, &consensus_hash, &header_hash)?
                    .ok_or_else(|| anyhow!("Bytes not found"))?;
            let mut cursor = std::io::Cursor::new(bytes);
            let stacks_block = StacksBlock::consensus_deserialize(&mut cursor)?;
            let block_size = stacks_block.block_size()? as u64;

            profile_scope!("Block Replay (Pre-Nakamoto)", {
                re_execute_prenakamoto(
                    context,
                    &stacks_block,
                    block_size,
                    &consensus_hash,
                    &header_hash,
                )?;
            });

            Ok(BlockMetrics::default())
        }
    }
}

fn re_execute_prenakamoto(
    context: &mut BenchContext,
    block: &StacksBlock,
    block_size: u64,
    consensus_hash: &ConsensusHash,
    block_hash: &blockstack_lib::types::chainstate::BlockHeaderHash,
) -> Result<()> {
    let (chainstate, burnchain) = context.get_databases_mut();

    // Load StagingBlock metadata
    let index_hash = StacksBlockId::new(consensus_hash, block_hash);
    let staging_block = StacksChainState::load_staging_block_info(chainstate.db(), &index_hash)?
        .ok_or_else(|| anyhow!("Staging block info not found for {}", index_hash))?;

    let (mut chainstate_tx, clarity_instance) = chainstate.chainstate_tx_begin()?;

    let parent_header_info =
        StacksChainState::get_parent_header_info(&mut chainstate_tx, &staging_block)?
            .ok_or_else(|| anyhow!("Parent header info not found"))?;

    let parent_block_hash = parent_header_info.anchored_header.block_hash();
    let parent_micro_hash = block.header.parent_microblock.clone();
    let parent_micro_seq = block.header.parent_microblock_sequence;

    let next_microblocks = StacksChainState::inner_find_parent_microblock_stream(
        &chainstate_tx.tx,
        block_hash,
        &parent_block_hash,
        &parent_header_info.consensus_hash,
        &parent_micro_hash,
        parent_micro_seq,
    )?
    .ok_or_else(|| anyhow!("Microblock stream not found"))?;

    let connecting_microblocks = StacksChainState::extract_connecting_microblocks(
        &parent_header_info,
        consensus_hash,
        block_hash,
        block,
        next_microblocks,
    )?;

    let mut sortdb = burnchain.open_sortition_db(true)?;

    let snapshot = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), consensus_hash)?
        .ok_or_else(|| anyhow!("Snapshot not found"))?;

    let pox_constants = sortdb.pox_constants.clone();
    let mut sort_tx = sortdb.tx_begin_at_tip();

    let commit_burn = 0;
    let sortition_burn = 0;

    StacksChainState::append_block(
        &mut chainstate_tx,
        clarity_instance,
        &mut sort_tx,
        &pox_constants,
        &parent_header_info,
        consensus_hash,
        &snapshot.burn_header_hash,
        snapshot.block_height as u32,
        snapshot.burn_header_timestamp,
        block,
        block_size,
        &connecting_microblocks,
        commit_burn,
        sortition_burn,
        false,
    )
    .map_err(|e| anyhow!("append_block failed: {:?}", e))?;

    // Commit the updated chainstate.
    chainstate_tx.commit()?;
    Ok(())
}

// fn re_execute_nakamoto_miner(
//     context: &mut BenchContext,
//     block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
// ) -> Result<BlockMetrics> {
//     with_executed_nakamoto_block(
//         context,
//         block,
//         |builder, mut tenure_tx, burn_chain_height| {
//             profile_scope!("Mining", {
//                 builder.mine_nakamoto_block(&mut tenure_tx, burn_chain_height);
//             });

//             let mut fake_consensus_hash = block.header.consensus_hash.clone();
//             fake_consensus_hash.0[0] ^= 0xFF;
//             let mut fake_block_hash = block.header.block_hash();
//             fake_block_hash.0[0] ^= 0xFF;
//             let block_id = StacksBlockId::new(&fake_consensus_hash, &fake_block_hash);

//             profile_scope!("Block Commit", {
//                 tenure_tx.commit_mined_block(&block_id)?;
//             });

//             Ok(())
//         },
//     )
// }

/// Re-execute a block simulating a FOLLOWER node.
fn re_execute_nakamoto_follower(
    context: &mut BenchContext,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<BlockMetrics> {
    with_executed_nakamoto_block(context, block, |_builder, tenure_tx, _burn_chain_height| {
        let mut fake_consensus_hash = block.header.consensus_hash.clone();
        fake_consensus_hash.0[0] ^= 0xAA;
        let mut fake_block_hash = block.header.block_hash();
        fake_block_hash.0[0] ^= 0xAA;

        profile_scope!("Block Commit", {
            tenure_tx.commit_to_block(&fake_consensus_hash, &fake_block_hash);
        });
        Ok(())
    })
}

fn with_executed_nakamoto_block<F>(
    context: &mut BenchContext,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    commit_callback: F,
) -> Result<BlockMetrics>
where
    F: FnOnce(&mut NakamotoBlockBuilder, ClarityTx, u32) -> Result<()>,
{
    let start_total = Instant::now();
    let parent_block_id = block.header.parent_block_id.clone();

    // Setup (Not counted in execution duration, but part of total)
    let setup_scope = Profiler::begin_span("Setup");
    let parent_header =
        NakamotoChainState::get_block_header(context.chainstate().db(), &parent_block_id)?
            .ok_or_else(|| anyhow!("Parent header not found"))?;

    // Find the coinbase transaction (if any)
    let coinbase = block.get_coinbase_tx();
    // Find the tenure change transaction (if any)
    let tenure_change_payload = block.try_get_tenure_change_payload();
    // If we have a tenure payload, the tx is guaranteed to be at index 0
    let tenure_change = tenure_change_payload.and(block.txs.first());
    // Determine the cause of the tenure change (if any). Used below for loading tenure info
    let tenure_cause = tenure_change_payload
        .map(|tc| MinerTenureInfoCause::from(tc.cause))
        .unwrap_or(MinerTenureInfoCause::NoTenureChange);

    // Initialize the block builder
    let mut builder = NakamotoBlockBuilder::new(
        &parent_header,
        &block.header.consensus_hash,
        block.header.burn_spent,
        tenure_change,
        coinbase,
        block.header.pox_treatment.len(),
        None,
        None,
        Some(block.header.timestamp),
    )?;

    // Get db handles
    let sortdb = context.burnchain_mut().open_sortition_db(true)?;
    let burn_dbconn = sortdb.index_handle_at_block(context.chainstate(), &parent_block_id)?;

    let mut miner_tenure_info =
        builder.load_tenure_info(context.chainstate_mut(), &burn_dbconn, tenure_cause)?;
    let burn_chain_height = miner_tenure_info.burn_tip_height;

    // Setup tenure transaction
    let mut tenure_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;
    drop(setup_scope);
    let setup_duration = start_total.elapsed();

    // Execution Phase
    let exec_scope = Profiler::begin_span("Transaction Replay");
    let start_exec = Instant::now();
    let mut tx_metrics = Vec::new();
    let mut total_clarity_cost = ExecutionCost::ZERO;

    for (i, tx) in block.txs.iter().enumerate() {
        let tx_len = tx.tx_len();
        let start_tx = Instant::now();

        let result = profile_scope!("Transaction", {
            builder.try_mine_tx_with_len(
                &mut tenure_tx,
                tx,
                tx_len,
                &BlockLimitFunction::NO_LIMIT_HIT,
                None,
            )
        });

        let duration_tx = start_tx.elapsed();
        let mut cost = ExecutionCost::ZERO;

        match result {
            TransactionResult::Success(ref success_data) => {
                // Cost is available in the receipt
                cost = success_data.receipt.execution_cost.clone();
                total_clarity_cost
                    .add(&cost)
                    .map_err(|e| anyhow!("Execution cost addition failure: {:?}", e))?;
            }
            TransactionResult::ProcessingError(ref error_data) => {
                // TransactionError does not expose cost, so we track 0
                eprintln!("  Tx #{i} (0x{}) failed: {:?}", tx.txid(), error_data.error);
            }
            TransactionResult::Skipped(ref skipped_data) => {
                eprintln!(
                    "  Tx #{i} (0x{}) skipped: {:?}",
                    tx.txid(),
                    skipped_data.error
                );
            }
            TransactionResult::Problematic(ref prob_data) => {
                eprintln!(
                    "  Tx #{i} (0x{}) problematic: {:?}",
                    tx.txid(),
                    prob_data.error
                );
            }
        }

        tx_metrics.push(TransactionMetrics {
            txid: tx.txid().to_string(),
            duration: duration_tx,
            cost,
            estimated_commit_impact: Duration::ZERO, // Calculated below
        });
    }
    let execution_duration = start_exec.elapsed();
    drop(exec_scope);

    // Commit Phase
    let start_commit = Instant::now();
    commit_callback(&mut builder, tenure_tx, burn_chain_height)?;
    let commit_duration = start_commit.elapsed();

    Ok(BlockMetrics {
        total_duration: start_total.elapsed(),
        setup_duration,
        execution_duration,
        commit_duration,
        total_clarity_cost,
        transactions: tx_metrics,
        commit_overhead_baseline: Duration::ZERO, // Will be filled by apply_* methods
    })
}
