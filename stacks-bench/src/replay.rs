use std::ops::Range;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use blockstack_lib::burnchains::Burnchain;
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
use stacks_common::types::chainstate::{BlockHeaderHash, StacksBlockId};

use crate::context::BenchContext;
use crate::metrics::{BlockMetrics, TransactionMetrics};
use crate::{BlockEra, ResolveEpochFromHeight, StacksBlockHeader};

pub enum ReplayMode {
    Miner,
    Follower,
    Ephemeral,
    /// Execute via replay_nakamoto_by_segments() using build_segments_filtered()
    SegmentedFiltered(crate::filter::TxFilter),
}

#[derive(Clone, Debug)]
struct TxSegment {
    /// Contiguous tx range in `block.txs`
    range: Range<usize>,

    /// Whether to record per-tx metrics and include in totals
    record: bool,

    /// If true, measure commit time and assign it to each tx in this segment
    /// (practically: used for singleton “target tx” segments).
    attribute_commit_to_txs: bool,
}

fn build_segments_full(block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock) -> Vec<TxSegment> {
    if block.txs.is_empty() {
        return vec![];
    }
    vec![TxSegment {
        range: 0..block.txs.len(),
        record: true,
        attribute_commit_to_txs: false, // full replay commit isn't attributable per tx
    }]
}

fn build_segments_filtered(
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    filter: &crate::filter::TxFilter,
) -> Vec<TxSegment> {
    let n = block.txs.len();
    if n == 0 {
        return vec![];
    }

    let mut out = Vec::new();
    let mut run_start = 0usize; // start of current "unmeasured run"

    for i in 0..n {
        let is_match = filter.matches(&block.txs[i]);
        if !is_match {
            continue;
        }

        // segment 1: unmeasured run [run_start..i) (may be empty)
        if run_start < i {
            out.push(TxSegment {
                range: run_start..i,
                record: false,
                attribute_commit_to_txs: false,
            });
        }

        // segment 2: measured singleton [i..i+1)
        out.push(TxSegment {
            range: i..(i + 1),
            record: true,
            attribute_commit_to_txs: true, // commit time is "per-tx commit" here
        });

        run_start = i + 1;
    }

    // trailing unmeasured run after last match
    if run_start < n {
        out.push(TxSegment {
            range: run_start..n,
            record: false,
            attribute_commit_to_txs: false,
        });
    }

    out
}

/// Re-execute all transactions in a block to measure execution performance.
pub fn replay_block(
    context: &mut BenchContext,
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    mode: ReplayMode,
    block_header: &StacksBlockHeader,
) -> Result<BlockMetrics> {
    let block_height = block_header.height;
    let epoch = context
        .resolve_stacks_epoch(block_height)
        .ok_or_else(|| anyhow!("Failed to resolve epoch for height {}", block_height))?;

    match context.resolve_block_era(epoch) {
        BlockEra::Nakamoto => {
            let (naka_block, _) = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block_header.id)?
                .ok_or_else(|| anyhow!("Nakamoto block not found"))?;

            match mode {
                ReplayMode::Miner => {
                    // Currently not implemented in this refactor
                    Err(anyhow!("Nakamoto Miner replay not implemented"))
                }
                ReplayMode::Follower => {
                    let mut metrics = stacks_profiler::measure!(
                        "Block Replay (Nakamoto Follower)",
                        block_height,
                        { re_execute_nakamoto_follower(chainstate, burnchain, &naka_block) }
                    )?;
                    // Calculate storage impact of this block
                    metrics.total_storage_delta = context.update_storage_delta()?;
                    Ok(metrics)
                }
                ReplayMode::Ephemeral => {
                    // Currently not implemented in this refactor
                    Err(anyhow!("Nakamoto Ephemeral replay not implemented"))
                }
                ReplayMode::SegmentedFiltered(filter) => {
                    let segments = build_segments_filtered(&naka_block, &filter);

                    let mut metrics = stacks_profiler::measure!(
                        "Block Replay (Nakamoto SegmentedFiltered)",
                        block_height,
                        {
                            replay_nakamoto_by_segments(
                                chainstate,
                                burnchain,
                                &naka_block,
                                &segments,
                            )
                        }
                    )?;

                    metrics.total_storage_delta = context.update_storage_delta()?;
                    Ok(metrics)
                }
            }
        }
        BlockEra::PreNakamoto => {
            let blocks_path = chainstate.blocks_path.clone();
            // Pre-Nakamoto metrics not fully implemented in this refactor yet
            // Returning empty metrics for now to satisfy signature
            let (consensus_hash, header_hash) = chainstate
                .get_block_header_hashes(&block_header.id)?
                .ok_or_else(|| anyhow!("Hashes not found"))?;
            let bytes =
                StacksChainState::load_block_bytes(&blocks_path, &consensus_hash, &header_hash)?
                    .ok_or_else(|| anyhow!("Bytes not found"))?;
            let mut cursor = std::io::Cursor::new(bytes);
            let stacks_block = StacksBlock::consensus_deserialize(&mut cursor)?;
            let block_size = stacks_block.block_size()? as u64;

            stacks_profiler::measure!("Block Replay (Pre-Nakamoto)", {
                re_execute_prenakamoto(
                    chainstate,
                    burnchain,
                    &stacks_block,
                    block_size,
                    &consensus_hash,
                    &header_hash,
                )?;
            });

            let mut metrics = BlockMetrics::default();
            metrics.total_storage_delta = context.update_storage_delta()?;

            Ok(metrics)
        }
    }
}

fn re_execute_prenakamoto(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    block: &StacksBlock,
    block_size: u64,
    consensus_hash: &ConsensusHash,
    block_hash: &BlockHeaderHash,
) -> Result<()> {
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
    .with_context(|| format!("append_block failed"))?;

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
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<BlockMetrics> {
    with_executed_nakamoto_block(
        chainstate,
        burnchain,
        block,
        |_builder, tenure_tx, _burn_chain_height| {
            let mut fake_consensus_hash = block.header.consensus_hash.clone();
            fake_consensus_hash.0[0] ^= 0xAA;
            let mut fake_block_hash = block.header.block_hash();
            fake_block_hash.0[0] ^= 0xAA;

            stacks_profiler::measure!("Block Commit", {
                tenure_tx.commit_to_block(&fake_consensus_hash, &fake_block_hash);
            });
            Ok(())
        },
    )
}

fn replay_nakamoto_by_segments(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    segments: &[TxSegment],
) -> Result<BlockMetrics> {
    if segments.is_empty() {
        return Ok(BlockMetrics::default());
    }

    let parent_block_id = block.header.parent_block_id.clone();
    let parent_info = NakamotoChainState::get_block_header(chainstate.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    let mut out = BlockMetrics::default();
    // Keep track of the current parent header info across segments
    let mut cur_parent_info = parent_info.clone();

    for (seg_ix, seg) in segments.iter().enumerate() {
        if seg.range.is_empty() {
            continue;
        }

        // Get the txs for this segment
        let segment_txs = &block.txs[seg.range.clone()];

        // Find whether this segment includes the tenure tx / coinbase tx
        let segment_tenure_change_tx: Option<&blockstack_lib::chainstate::stacks::StacksTransaction> =
            segment_txs.iter().find(|tx| tx.try_as_tenure_change().is_some());

        let segment_coinbase_tx: Option<&blockstack_lib::chainstate::stacks::StacksTransaction> =
            segment_txs.iter().find(|tx| tx.try_as_coinbase().is_some());

        // Cause should only be “new tenure” if segment has tenure tx
        let segment_cause = if let Some(tc_tx) = segment_tenure_change_tx {
            let tc_payload = tc_tx.try_as_tenure_change().expect("checked above");
            MinerTenureInfoCause::from(tc_payload.cause)
        } else {
            MinerTenureInfoCause::NoTenureChange
        };

        let mut builder = NakamotoBlockBuilder::new(
            &cur_parent_info,
            &block.header.consensus_hash,
            block.header.burn_spent,
            segment_tenure_change_tx,
            segment_coinbase_tx,
            block.header.pox_treatment.len(),
            None,
            None,
            Some(block.header.timestamp),
        )?;

        // derive current parent id from cur_parent_info
        let cur_parent_block_id = StacksBlockId::new(
            &cur_parent_info.consensus_hash,
            &cur_parent_info.anchored_header.block_hash(),
        );

        let sortdb = burnchain.open_sortition_db(true)?;
        let burn_dbconn = sortdb.index_handle_at_block(chainstate, &cur_parent_block_id)?;

        // NOTE: pass `segment_cause` here, not `tenure_cause` from the original full block
        let mut miner_tenure_info =
            builder.load_tenure_info(chainstate, &burn_dbconn, segment_cause)?;

        let burn_chain_height = miner_tenure_info.burn_tip_height;

        let mut clarity_tx = builder.tenure_begin(
            &burn_dbconn,
            &mut miner_tenure_info,
        )?;


        // Execute each tx in this segment
        let mut segment_tx_metrics: Vec<(String, Duration, ExecutionCost)> = Vec::new();

        let starting_cost = clarity_tx.cost_so_far();

        for i in seg.range.clone() {
            let tx = &block.txs[i];
            let tx_len = tx.tx_len();

            let start = if seg.record { Some(Instant::now()) } else { None };

            let res = builder.try_mine_tx_with_len(
                &mut clarity_tx,
                tx,
                tx_len,
                &BlockLimitFunction::NO_LIMIT_HIT,
                None,
            );

            let dur = start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

            let success = match res {
                TransactionResult::Success(ref s) => s,
                _ => {
                    clarity_tx.rollback_block();
                    return Err(anyhow!(
                        "Tx #{i} (0x{}) failed while executing segment #{seg_ix} ({:?})",
                        tx.txid(),
                        seg.range
                    ));
                }
            };

            if seg.record {
                let cost = success.receipt.execution_cost.clone();
                segment_tx_metrics.push((tx.txid().to_string(), dur, cost));
            }
        }

        let total_tenure_cost = clarity_tx.cost_so_far();
        let mut block_execution_cost = clarity_tx.cost_so_far();
        block_execution_cost.sub(&starting_cost)?;

        // Commit this segment (optionally measured)
        // Measure commit “the way mining does”: finalize (merkle+seal) + clarity commit + header/marf writes
        let commit_start = if seg.record { Some(Instant::now()) } else { None };

        // cheap per-segment size approximation (matches builder accounting)
        let segment_block_size = builder.get_bytes_so_far();

        // 1) finalize block (computes tx_merkle_root + state_index_root via seal)
        let mined_block = builder.mine_nakamoto_block(&mut clarity_tx, burn_chain_height);
        let mined_block_hash = mined_block.header.block_hash();
        let mined_consensus_hash = mined_block.header.consensus_hash.clone();

        // 2) commit Clarity state to (consensus_hash, block_hash)
        clarity_tx.commit_to_block(&mined_consensus_hash, &mined_block_hash);

        // 3) advance headers tip
        let burn_view =
            NakamotoChainState::get_block_burn_view(&sortdb, &mined_block, &cur_parent_info)?;

        let sn = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), &mined_block.header.consensus_hash)?
            .ok_or_else(|| anyhow!("Snapshot not found for {}", mined_block.header.consensus_hash))?;

        let new_tip_info = NakamotoChainState::advance_tip(
            &mut miner_tenure_info.chainstate_tx.tx,
            &cur_parent_info.anchored_header,
            &cur_parent_info.consensus_hash,
            &mined_block,
            None,
            &sn.burn_header_hash,
            sn.block_height as u32,
            sn.burn_header_timestamp,
            None,
            None,
            &block_execution_cost,
            &total_tenure_cost,
            segment_block_size, // <-- per-segment, not original block_size
            false,
            vec![], vec![], vec![], vec![],
            false,
            0,
            0,
            &burn_view,
        )?;

        // and finally commit the whole thing once (or let outer scope do it)
        miner_tenure_info.chainstate_tx.commit()?;

        // Advance parent for the next segment so burn_view inheritance works
        cur_parent_info = new_tip_info;

        // end timing
        let commit_dur = commit_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

        // Record totals + per-tx metrics if this segment is recorded
        if seg.record {
            // total segment durations
            out.execution_duration += segment_tx_metrics.iter().map(|(_, d, _)| *d).sum::<Duration>();
            out.commit_duration += commit_dur;

            // total clarity cost
            for (_, _, cost) in segment_tx_metrics.iter() {
                out.total_clarity_cost.add(cost)?;
            }

            // per-tx TransactionMetrics
            let per_tx_commit = if seg.attribute_commit_to_txs && !segment_tx_metrics.is_empty() {
                // for singleton target segments, this will be the whole commit
                // (for multi-tx segments you'd probably keep this false)
                commit_dur
            } else {
                Duration::ZERO
            };

            for (txid, dur, cost) in segment_tx_metrics {
                out.transactions.push(TransactionMetrics {
                    txid,
                    duration: dur,
                    cost,
                    estimated_commit_impact: per_tx_commit,
                });
            }
        }
    }

    out.total_duration = out.execution_duration + out.commit_duration;
    Ok(out)
}

fn with_executed_nakamoto_block<F>(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    commit_callback: F,
) -> Result<BlockMetrics>
where
    F: FnOnce(&mut NakamotoBlockBuilder, ClarityTx, u32) -> Result<()>,
{
    // ========================================================================
    // 1. Setup Phase
    // ========================================================================
    // We use manual span!()/drop() here because the DB handles created
    // in this phase are self-referential (tenure_tx borrows burn_dbconn).
    // A block-scoped macro cannot return both the owner and the borrower safely.
    let setup_guard = stacks_profiler::span!("Setup");

    let start_total = Instant::now();
    let parent_block_id = block.header.parent_block_id.clone();

    let parent_header = NakamotoChainState::get_block_header(chainstate.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    let coinbase = block.get_coinbase_tx();
    let tenure_change_payload = block.try_get_tenure_change_payload();
    let tenure_change = tenure_change_payload.and(block.txs.first());

    let tenure_cause = tenure_change_payload
        .map(|tc| MinerTenureInfoCause::from(tc.cause))
        .unwrap_or(MinerTenureInfoCause::NoTenureChange);

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

    let sortdb = burnchain.open_sortition_db(true)?;
    let burn_dbconn = sortdb.index_handle_at_block(chainstate, &parent_block_id)?;

    let mut miner_tenure_info = builder.load_tenure_info(chainstate, &burn_dbconn, tenure_cause)?;

    let burn_chain_height = miner_tenure_info.burn_tip_height;
    let mut tenure_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;

    // Explicitly end the Setup span here, but the variables stay alive!
    drop(setup_guard);

    let setup_duration = start_total.elapsed();

    // ========================================================================
    // 2. Execution Phase
    // ========================================================================
    let start_exec = Instant::now();

    let (tx_metrics, total_clarity_cost) = stacks_profiler::measure!("Transaction Replay", {
        let mut tx_metrics = Vec::with_capacity(block.txs.len());
        let mut total_clarity_cost = ExecutionCost::ZERO;

        for (i, tx) in block.txs.iter().enumerate() {
            let tx_len = tx.tx_len();
            let start_tx = Instant::now();

            let result = stacks_profiler::measure!("Transaction", i, {
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
                    cost = success_data.receipt.execution_cost.clone();
                    total_clarity_cost
                        .add(&cost)
                        .context(format!("Execution cost addition failure"))?;
                }
                TransactionResult::ProcessingError(ref error_data) => {
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
                estimated_commit_impact: Duration::ZERO,
            });
        }

        (tx_metrics, total_clarity_cost)
    });

    let execution_duration = start_exec.elapsed();

    // ========================================================================
    // 3. Commit Phase
    // ========================================================================
    let start_commit = Instant::now();

    stacks_profiler::measure!("Commit", {
        commit_callback(&mut builder, tenure_tx, burn_chain_height)?;
    });

    let commit_duration = start_commit.elapsed();

    Ok(BlockMetrics {
        total_duration: start_total.elapsed(),
        setup_duration,
        execution_duration,
        commit_duration,
        total_clarity_cost,
        transactions: tx_metrics,
        commit_overhead_baseline: Duration::ZERO,
        total_storage_delta: 0,
    })
}
