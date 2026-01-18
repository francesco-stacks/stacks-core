use std::ops::Range;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use blockstack_lib::burnchains::{Burnchain, Txid};
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::nakamoto::miner::{MinerTenureInfoCause, NakamotoBlockBuilder};
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::miner::{
    BlockBuilder, BlockLimitFunction, TransactionResult,
};
use clarity::types::chainstate::ConsensusHash;
use clarity::vm::costs::ExecutionCost;
use stacks_common::types::chainstate::StacksBlockId;

use crate::context::BenchContext;
use crate::metrics::{BlockMetrics, BlockProcessingBaseline, TransactionMetrics};
use crate::{BlockEra, ResolveEpochFromHeight, StacksBlockHeader};

#[derive(Debug, Clone)]
pub enum ReplayMode {
    Miner,
    Follower,
    Ephemeral,
    /// Execute via replay_nakamoto_by_segments() using build_segments_filtered()
    SegmentedFiltered(crate::filter::TxFilter),
}

impl std::fmt::Display for ReplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayMode::Miner => write!(f, "Miner"),
            ReplayMode::Follower => write!(f, "Follower"),
            ReplayMode::Ephemeral => write!(f, "Ephemeral"),
            ReplayMode::SegmentedFiltered(_) => write!(f, "SegmentedFiltered"),
        }
    }
}

#[derive(Clone, Debug)]
struct TxSegment {
    /// Contiguous tx range in `block.txs`
    range: Range<usize>,

    /// Whether to record per-tx metrics and include in totals
    record: bool,
}

// fn is_full_replay_segments(
//     block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
//     segments: &[TxSegment],
// ) -> bool {
//     if segments.len() != 1 {
//         return false;
//     }
//     let seg = &segments[0];

//     seg.record && seg.range.start == 0 && seg.range.end == block.txs.len()
// }

fn build_segments_full(
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Vec<TxSegment> {
    vec![TxSegment {
        range: 0..block.txs.len(),
        record: true,
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
            });
        }

        // segment 2: measured singleton [i..i+1)
        out.push(TxSegment {
            range: i..(i + 1),
            record: true,
        });

        run_start = i + 1;
    }

    // trailing unmeasured run after last match
    if run_start < n {
        out.push(TxSegment {
            range: run_start..n,
            record: false,
        });
    }

    out
}

fn compute_synthetic_id(
    origin: &StacksBlockId,
    seg_ix: usize,
    range: &std::ops::Range<usize>,
    repetition: u32,
) -> StacksBlockId {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"stacks-bench:synth-block:v1");
    hasher.update(origin.as_bytes());
    hasher.update((seg_ix as u64).to_le_bytes());
    hasher.update((range.start as u64).to_le_bytes());
    hasher.update((range.end as u64).to_le_bytes());
    hasher.update(repetition.to_le_bytes());

    let digest = hasher.finalize();
    let bytes = digest[..32].to_vec();
    StacksBlockId::from_vec(&bytes).expect("sha256 yields 32 bytes")
}

/// Re-execute all transactions in a block to measure execution performance.
///
/// Segmented mode returns 0..N measurement units (one per recorded segment).
pub fn replay_block(
    context: &mut BenchContext,
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    mode: &ReplayMode,
    block_header: &StacksBlockHeader,
    repetition: u32,
) -> Result<Option<Vec<BlockMetrics>>> {
    let block_height = block_header.height;
    let epoch = context
        .resolve_stacks_epoch(block_height)
        .ok_or_else(|| anyhow!("Failed to resolve epoch for height {}", block_height))?;

    let metrics: Option<Vec<BlockMetrics>> = match context.resolve_block_era(epoch) {
        BlockEra::Nakamoto => {
            let (naka_block, _) = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block_header.id)?
                .ok_or_else(|| anyhow!("Nakamoto block not found"))?;

            match mode {
                ReplayMode::Miner => bail!("Nakamoto Miner replay not implemented"),
                ReplayMode::Ephemeral => bail!("Nakamoto Ephemeral replay not implemented"),

                ReplayMode::Follower => {
                    let segments = build_segments_full(&naka_block);

                    let seg_metrics = replay_nakamoto_by_segments(
                        chainstate,
                        burnchain,
                        &naka_block,
                        &segments,
                        repetition,
                    )?;

                    if seg_metrics.is_empty() {
                        None
                    } else {
                        Some(seg_metrics)
                    }
                }

                ReplayMode::SegmentedFiltered(filter) => {
                    let segments = build_segments_filtered(&naka_block, &filter);

                    // No recorded segments => no metrics.
                    if segments.is_empty() || segments.iter().all(|s| !s.record) {
                        return Ok(None);
                    }

                    // IMPORTANT:
                    // Do NOT wrap this in stacks_profiler::measure! because replay_nakamoto_by_segments
                    // uses Profiler::clear() / take_results() per recorded segment.
                    let seg_metrics = replay_nakamoto_by_segments(
                        chainstate,
                        burnchain,
                        &naka_block,
                        &segments,
                        repetition,
                    )?;

                    if seg_metrics.is_empty() {
                        None
                    } else {
                        Some(seg_metrics)
                    }
                }
            }
        }

        BlockEra::PreNakamoto => {
            // unchanged; currently no metrics emitted here
            None
        }
    };

    Ok(metrics)
}

fn replay_nakamoto_by_segments(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    segments: &[TxSegment],
    repetition: u32,
) -> Result<Vec<BlockMetrics>> {
    let origin_id = block.block_id();

    if segments.is_empty() {
        return Ok(vec![]);
    }

    let parent_block_id = block.header.parent_block_id.clone();
    let parent_info = NakamotoChainState::get_block_header(chainstate.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    // Reuse one sortdb handle across all segments
    let sortdb = burnchain.open_sortition_db(true)?;

    // Keep track of the current parent header info across segments
    let mut cur_parent_info = parent_info.clone();

    let mut out: Vec<BlockMetrics> = Vec::new();

    for (seg_ix, seg) in segments.iter().enumerate() {
        if seg.range.is_empty() && !seg.record {
            continue;
        }

        // Get segment size and transactions
        let segment_txs = &block.txs[seg.range.clone()];

        // Suppress profiler recording for unmeasured segments.
        let _suppression = if !seg.record {
            Some(stacks_profiler::Profiler::begin_suppression())
        } else {
            None
        };

        // For measured segments, start a fresh profiler tree for this segment.
        if seg.record {
            stacks_profiler::Profiler::clear();
        }

        let _segment_root = if seg.record {
            stacks_profiler::span!("Segment", seg_ix)
        } else {
            None
        };

        // -------------------------------------------------------------------
        // PHASE 1: SETUP
        // -------------------------------------------------------------------
        let setup_start = if seg.record {
            Some(Instant::now())
        } else {
            None
        };

        let _setup_guard = if seg.record {
            stacks_profiler::span!("Segment: Setup", seg_ix)
        } else {
            None
        };

        // Find whether this segment includes the tenure tx / coinbase tx
        let segment_tenure_change_tx: Option<
            &blockstack_lib::chainstate::stacks::StacksTransaction,
        > = segment_txs
            .iter()
            .find(|tx| tx.try_as_tenure_change().is_some());

        let segment_coinbase_tx: Option<&blockstack_lib::chainstate::stacks::StacksTransaction> =
            segment_txs.iter().find(|tx| tx.try_as_coinbase().is_some());

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

        let cur_parent_block_id = StacksBlockId::new(
            &cur_parent_info.consensus_hash,
            &cur_parent_info.anchored_header.block_hash(),
        );

        let burn_dbconn = sortdb.index_handle_at_block(chainstate, &cur_parent_block_id)?;

        let mut miner_tenure_info =
            builder.load_tenure_info(chainstate, &burn_dbconn, segment_cause)?;

        let burn_chain_height = miner_tenure_info.burn_tip_height;
        let mut clarity_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;

        drop(_setup_guard);

        let setup_duration = setup_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

        // -------------------------------------------------------------------
        // PHASE 2: TRANSACTION EXECUTION
        // -------------------------------------------------------------------
        let exec_start = if seg.record {
            Some(Instant::now())
        } else {
            None
        };

        let _exec_guard = if seg.record {
            stacks_profiler::span!("Segment: Tx Execution", seg_ix)
        } else {
            None
        };

        let mut segment_tx_metrics: Vec<(Txid, Duration, ExecutionCost)> = Vec::new();
        let mut segment_total_clarity_cost = ExecutionCost::ZERO;

        let starting_cost = clarity_tx.cost_so_far();

        for i in seg.range.clone() {
            let tx = &block.txs[i];
            let tx_len = tx.tx_len();

            let tx_start = if seg.record {
                Some(Instant::now())
            } else {
                None
            };

            let rel_i = i - seg.range.start;

            let _tx_guard = if seg.record {
                stacks_profiler::span!("Transaction", rel_i)
            } else {
                None
            };

            // NOTE: This is a miner execution path which calls `Relayer::static_check_problematic_relayed_tx()`,
            // which followers do not do, which can affect statistics. However, that
            let res = builder.try_mine_tx_with_len(
                &mut clarity_tx,
                tx,
                tx_len,
                &BlockLimitFunction::NO_LIMIT_HIT,
                None,
            );

            drop(_tx_guard);

            let dur = tx_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

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
                segment_total_clarity_cost.add(&cost)?;
                segment_tx_metrics.push((tx.txid(), dur, cost));
            }
        }

        drop(_exec_guard);

        let execution_duration = exec_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

        // -------------------------------------------------------------------
        // PHASE 3: COMMIT (finalize + clarity commit + advance tip + index commit)
        // -------------------------------------------------------------------
        let commit_start = if seg.record {
            Some(Instant::now())
        } else {
            None
        };

        let total_tenure_cost = clarity_tx.cost_so_far();
        let mut block_execution_cost = clarity_tx.cost_so_far();
        block_execution_cost.sub(&starting_cost)?;

        let segment_block_size = builder.get_bytes_so_far();

        let _finalize_guard = if seg.record {
            stacks_profiler::span!("Segment: Finalize (merkle+seal)", seg_ix)
        } else {
            None
        };

        let mined_block = builder.mine_nakamoto_block(&mut clarity_tx, burn_chain_height);
        let mined_block_hash = mined_block.header.block_hash();
        let mined_consensus_hash = mined_block.header.consensus_hash.clone();

        drop(_finalize_guard);

        let _clarity_commit_guard = if seg.record {
            stacks_profiler::span!("Segment: Clarity State Commit", seg_ix)
        } else {
            None
        };

        clarity_tx.commit_to_block(&mined_consensus_hash, &mined_block_hash);

        drop(_clarity_commit_guard);

        let _advance_chain_tip_guard = if seg.record {
            stacks_profiler::span!("Segment: Advance Chain Tip", seg_ix)
        } else {
            None
        };

        let burn_view =
            NakamotoChainState::get_block_burn_view(&sortdb, &mined_block, &cur_parent_info)?;

        let sn = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), &mined_consensus_hash)?
            .ok_or_else(|| anyhow!("Snapshot not found for {}", mined_consensus_hash))?;

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
            segment_block_size,
            false,
            vec![],
            vec![],
            vec![],
            vec![],
            false,
            0,
            0,
            &burn_view,
        )?;

        drop(_advance_chain_tip_guard);

        let _index_commit_guard = if seg.record {
            stacks_profiler::span!("Segment: Index Commit", seg_ix)
        } else {
            None
        };

        miner_tenure_info.chainstate_tx.commit()?;

        drop(_index_commit_guard);
        drop(_segment_root);

        let commit_duration = commit_start.map(|s| s.elapsed()).unwrap_or(Duration::ZERO);

        // Pull profiler roots for this segment only (measured segments only).
        let segment_profiler_roots = if seg.record {
            stacks_profiler::Profiler::take_results()
        } else {
            vec![]
        };

        // Advance parent for the next segment so burn_view inheritance works
        cur_parent_info = new_tip_info;

        // -------------------------------------------------------------------
        // Emit one block metrics row per *recorded* segment
        // -------------------------------------------------------------------
        if seg.record {
            let synthetic_id = compute_synthetic_id(&origin_id, seg_ix, &seg.range, repetition);

            let mut m = BlockMetrics::new_default(origin_id.clone(), synthetic_id);
            m.setup_duration = setup_duration;
            m.execution_duration = execution_duration;
            m.commit_duration = commit_duration;
            m.total_duration = setup_duration + execution_duration + commit_duration;
            m.total_clarity_cost = segment_total_clarity_cost;
            m.profiler_roots = segment_profiler_roots;

            // Per-tx metrics (tx profiler roots only when seg_len==1, to avoid bloat)
            for (txid, dur, cost) in segment_tx_metrics {
                m.transactions.push(TransactionMetrics {
                    txid,
                    duration: dur,
                    cost,
                    profiler_roots: vec![],
                });
            }

            out.push(m);
        }
    }

    Ok(out)
}

pub fn replay_nakamoto_empty_chain_baseline<F>(
    chainstate: &mut StacksChainState,
    burnchain: &Burnchain,
    start_parent_block_id: &StacksBlockId,
    n_blocks: u32,
    mut on_progress: F,
) -> anyhow::Result<BlockProcessingBaseline>
where
    F: FnMut(u32, u32),
{
    if n_blocks == 0 {
        return Ok(BlockProcessingBaseline::default());
    }

    let mut cur_parent_info =
        NakamotoChainState::get_block_header(chainstate.db(), start_parent_block_id)?
            .ok_or_else(|| anyhow::anyhow!("Parent header not found: {start_parent_block_id}"))?;

    // We only read from sortdb in this benchmark.
    let sortdb = burnchain
        .open_sortition_db(false)
        .with_context(|| "open sortition db (readonly) for baseline")?;

    let mut total_setup_duration = Duration::ZERO;
    let mut total_finalize_duration = Duration::ZERO;
    let mut total_clarity_state_commit_duration = Duration::ZERO;
    let mut total_advance_chain_tip_duration = Duration::ZERO;
    let mut total_index_commit_duration = Duration::ZERO;

    for i in 0..n_blocks {
        let setup_start = Instant::now();

        // Prefer the parent's burn_view (Nakamoto), but fall back to the parent's
        // election consensus hash if the burn_view snapshot is missing.
        let preferred_view: Option<ConsensusHash> = cur_parent_info.burn_view.clone();
        let mut baseline_view =
            preferred_view.unwrap_or_else(|| cur_parent_info.consensus_hash.clone());

        if !SortitionDB::has_block_snapshot_consensus(sortdb.conn(), &baseline_view)? {
            // fallback path (common culprit for the "Not found" you're seeing)
            baseline_view = cur_parent_info.consensus_hash.clone();
        }

        let baseline_sn = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), &baseline_view)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "baseline: missing sortition snapshot for view {baseline_view} (iter {i})"
                )
            })?;

        let burn_dbconn = sortdb.index_handle(&baseline_sn.sortition_id);

        // Ensure subsequent burn-view derivations (e.g. get_block_burn_view) stay consistent
        // with whatever view we were able to open.
        cur_parent_info.burn_view = Some(baseline_view.clone());

        let mut builder = NakamotoBlockBuilder::new(
            &cur_parent_info,
            &baseline_view, // ensures later snapshot lookup (by mined header CH) is valid
            0,              // total_burn
            None,           // tenure_change tx
            None,           // coinbase tx
            0,              // pox bitvec len
            None,
            None,
            None,
        )?;

        let mut miner_tenure_info = builder.load_tenure_info(
            chainstate,
            &burn_dbconn,
            MinerTenureInfoCause::NoTenureChange,
        )?;
        let burn_chain_height = miner_tenure_info.burn_tip_height;

        let mut clarity_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;
        let starting_cost = clarity_tx.cost_so_far();

        total_setup_duration += setup_start.elapsed();

        let finalize_start = Instant::now();
        let mined_block = builder.mine_nakamoto_block(&mut clarity_tx, burn_chain_height);
        total_finalize_duration += finalize_start.elapsed();

        let total_tenure_cost = clarity_tx.cost_so_far();
        let mut block_execution_cost = clarity_tx.cost_so_far();
        block_execution_cost.sub(&starting_cost)?;

        let clarity_commit_start = Instant::now();
        clarity_tx.commit_to_block(
            &mined_block.header.consensus_hash,
            &mined_block.header.block_hash(),
        );
        total_clarity_state_commit_duration += clarity_commit_start.elapsed();

        let advance_tip_start = Instant::now();

        let burn_view =
            NakamotoChainState::get_block_burn_view(&sortdb, &mined_block, &cur_parent_info)?;

        let sn = SortitionDB::get_block_snapshot_consensus(
            sortdb.conn(),
            &mined_block.header.consensus_hash,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "baseline: snapshot not found for mined header CH {} (iter {i})",
                mined_block.header.consensus_hash
            )
        })?;

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
            builder.get_bytes_so_far(),
            false,
            vec![],
            vec![],
            vec![],
            vec![],
            false,
            0,
            0,
            &burn_view,
        )?;

        total_advance_chain_tip_duration += advance_tip_start.elapsed();

        let index_commit_start = Instant::now();
        miner_tenure_info.chainstate_tx.commit()?;
        total_index_commit_duration += index_commit_start.elapsed();

        cur_parent_info = new_tip_info;

        on_progress(i + 1, n_blocks);
    }

    Ok(BlockProcessingBaseline {
        avg_setup_duration: total_setup_duration / n_blocks,
        avg_finalize_duration: total_finalize_duration / n_blocks,
        avg_clarity_state_commit_duration: total_clarity_state_commit_duration / n_blocks,
        avg_advance_tip_duration: total_advance_chain_tip_duration / n_blocks,
        avg_index_commit_duration: total_index_commit_duration / n_blocks,
    })
}
