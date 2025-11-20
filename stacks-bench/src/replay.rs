use anyhow::{Result, anyhow};
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::nakamoto::miner::{MinerTenureInfoCause, NakamotoBlockBuilder};
use blockstack_lib::chainstate::stacks::db::{ClarityTx, StacksChainState};
use blockstack_lib::chainstate::stacks::miner::{
    BlockBuilder, BlockLimitFunction, TransactionResult,
};
use blockstack_lib::chainstate::stacks::{StacksBlock, TransactionPayload};
use clarity::codec::StacksMessageCodec;
use clarity::types::chainstate::{ConsensusHash, StacksBlockId};

use crate::{Block, BlockEra};

/// Re-execute all transactions in a block to measure execution performance.
/// Does NOT commit changes to the DB.
pub fn re_execute_block(
    cs: &mut StacksChainState,
    sortdb: &mut SortitionDB,
    block_summary: &Block,
) -> Result<()> {
    // 1. Reload full block data to get necessary metadata/structure for replay
    match block_summary.era {
        BlockEra::Nakamoto => {
            let (naka_block, _size) = cs
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block_summary.id)?
                .ok_or_else(|| anyhow!("Nakamoto block not found"))?;

            //re_execute_nakamoto_miner(cs, sortdb, &naka_block)
            re_execute_nakamoto_follower(cs, sortdb, &naka_block)
        }
        BlockEra::PreNakamoto => {
            // Load StacksBlock
            let (consensus_hash, header_hash) = cs
                .get_block_header_hashes(&block_summary.id)?
                .ok_or_else(|| anyhow!("Hashes not found"))?;
            let bytes =
                StacksChainState::load_block_bytes(&cs.blocks_path, &consensus_hash, &header_hash)?
                    .ok_or_else(|| anyhow!("Bytes not found"))?;
            let mut cursor = std::io::Cursor::new(bytes);
            let stacks_block = StacksBlock::consensus_deserialize(&mut cursor)?;
            let block_size = stacks_block.block_size()? as u64;

            re_execute_prenakamoto(
                cs,
                sortdb,
                &stacks_block,
                block_size,
                &consensus_hash,
                &header_hash,
            )
        }
    }
}

fn re_execute_prenakamoto(
    cs: &mut StacksChainState,
    sortdb: &mut SortitionDB,
    block: &StacksBlock,
    block_size: u64,
    consensus_hash: &ConsensusHash,
    block_hash: &blockstack_lib::types::chainstate::BlockHeaderHash,
) -> Result<()> {
    // Load StagingBlock metadata
    let index_hash = StacksBlockId::new(consensus_hash, block_hash);
    let staging_block = StacksChainState::load_staging_block_info(cs.db(), &index_hash)?
        .ok_or_else(|| anyhow!("Staging block info not found for {}", index_hash))?;

    let (mut chainstate_tx, clarity_instance) = cs.chainstate_tx_begin()?;

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

fn re_execute_nakamoto_miner(
    cs: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<()> {
    with_executed_nakamoto_block(
        cs,
        sortdb,
        block,
        |builder, mut tenure_tx, burn_chain_height| {
            // 5. Finish Block (Apply Rewards & Seal)
            // Miner specific: Calculates state root and updates VRF seed
            builder.mine_nakamoto_block(&mut tenure_tx, burn_chain_height);

            // 6. Commit to a SYNTHETIC FORK to force full I/O
            let mut fake_consensus_hash = block.header.consensus_hash.clone();
            fake_consensus_hash.0[0] ^= 0xFF;

            let mut fake_block_hash = block.header.block_hash();
            fake_block_hash.0[0] ^= 0xFF;

            let block_id = StacksBlockId::new(&fake_consensus_hash, &fake_block_hash);

            // Commit the FULL state (User Txs + Miner Rewards) to Mined Table (SQLite)
            tenure_tx.commit_mined_block(&block_id)?;
            Ok(())
        },
    )
}

/// Re-execute a block simulating a FOLLOWER node.
fn re_execute_nakamoto_follower(
    cs: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<()> {
    with_executed_nakamoto_block(
        cs,
        sortdb,
        block,
        |_builder, tenure_tx, _burn_chain_height| {
            // We skip mine_nakamoto_block for followers to avoid double-executing coinbase.

            // Use synthetic IDs
            let mut fake_consensus_hash = block.header.consensus_hash.clone();
            fake_consensus_hash.0[0] ^= 0xAA;
            let mut fake_block_hash = block.header.block_hash();
            fake_block_hash.0[0] ^= 0xAA;

            // CRITICAL CHANGE: Follower Commit
            // This triggers `external_blobs` logic for the real data generated by the VM.
            tenure_tx.commit_to_block(&fake_consensus_hash, &fake_block_hash);
            Ok(())
        },
    )
}

/// Shared helper to setup and execute a Nakamoto block without committing.
fn with_executed_nakamoto_block<F>(
    cs: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
    callback: F,
) -> Result<()>
where
    F: FnOnce(&mut NakamotoBlockBuilder, ClarityTx, u32) -> Result<()>,
{
    let parent_block_id = block.header.parent_block_id.clone();

    // 1. Get Parent Header
    let parent_header = NakamotoChainState::get_block_header(cs.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    // 2. Setup Builder
    let tenure_change = block
        .txs
        .iter()
        .find(|tx| matches!(tx.payload, TransactionPayload::TenureChange(..)));
    let coinbase = block
        .txs
        .iter()
        .find(|tx| matches!(tx.payload, TransactionPayload::Coinbase(..)));
    let tenure_cause = tenure_change
        .and_then(|tx| match &tx.payload {
            TransactionPayload::TenureChange(tc) => Some(tc.into()),
            _ => None,
        })
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

    // 3. Load Tenure & Begin
    let burn_dbconn = sortdb.index_handle_at_block(cs, &parent_block_id)?;

    let mut miner_tenure_info = builder.load_tenure_info(cs, &burn_dbconn, tenure_cause)?;
    let burn_chain_height = miner_tenure_info.burn_tip_height;

    let mut tenure_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;

    // 4. Execute Transactions
    for (i, tx) in block.txs.iter().enumerate() {
        let tx_len = tx.tx_len();
        let result = builder.try_mine_tx_with_len(
            &mut tenure_tx,
            tx,
            tx_len,
            &BlockLimitFunction::NO_LIMIT_HIT,
            None,
        );

        if let TransactionResult::ProcessingError(e) = result {
            eprintln!("  Tx #{i} (0x{}) failed: {:?}", tx.txid(), e);
        }
    }

    callback(&mut builder, tenure_tx, burn_chain_height)
}
