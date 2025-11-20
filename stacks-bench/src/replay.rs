use anyhow::{Result, anyhow};
use blockstack_lib::chainstate::{burn::db::sortdb::SortitionDB, nakamoto::{NakamotoChainState, miner::{MinerTenureInfoCause, NakamotoBlockBuilder}}, stacks::{StacksBlock, TransactionPayload, db::StacksChainState, miner::{BlockBuilder, BlockLimitFunction, TransactionResult}}};
use clarity::{codec::StacksMessageCodec, types::chainstate::{ConsensusHash, StacksBlockId}};

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
             let (consensus_hash, header_hash) = cs.get_block_header_hashes(&block_summary.id)?
                 .ok_or_else(|| anyhow!("Hashes not found"))?;
             let bytes = StacksChainState::load_block_bytes(&cs.blocks_path, &consensus_hash, &header_hash)?
                 .ok_or_else(|| anyhow!("Bytes not found"))?;
             let mut cursor = std::io::Cursor::new(bytes);
             let stacks_block = StacksBlock::consensus_deserialize(&mut cursor)?;
             let block_size = stacks_block.block_size()? as u64;
             
             re_execute_prenakamoto(cs, sortdb, &stacks_block, block_size, &consensus_hash, &header_hash)
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
    
    let (mut chainstate_tx, mut clarity_instance) = cs.chainstate_tx_begin()?;
    
    let parent_header_info = StacksChainState::get_parent_header_info(&mut chainstate_tx, &staging_block)?
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
    )?.ok_or_else(|| anyhow!("Microblock stream not found"))?;

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
        &mut clarity_instance,
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
        false //false, // TODO: Change to `false` to use the real commit flow
    ).map_err(|e| anyhow!("append_block failed: {:?}", e))?;

    // Commit the updated chainstate.
    chainstate_tx.commit()?;
    Ok(())
}

fn re_execute_nakamoto_miner(
    cs: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<()> {
    let parent_block_id = block.header.parent_block_id.clone();
    
    // 1. Get Parent Header
    let parent_header = NakamotoChainState::get_block_header(cs.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    // 2. Setup Builder
    let tenure_change = block.txs.iter().find(|tx| matches!(tx.payload, TransactionPayload::TenureChange(..)));
    let coinbase = block.txs.iter().find(|tx| matches!(tx.payload, TransactionPayload::Coinbase(..)));
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

    // 5. Finish Block (Apply Rewards & Seal)
    // This applies the miner rewards (state writes) and calculates the state root.
    // We ignore the returned block struct; we just want the side-effects on tenure_tx.
    builder.mine_nakamoto_block(&mut tenure_tx, burn_chain_height);

    // 6. Commit to a SYNTHETIC FORK to force full I/O
    let mut fake_consensus_hash = block.header.consensus_hash.clone();
    fake_consensus_hash.0[0] ^= 0xFF;

    let mut fake_block_hash = block.header.block_hash();
    fake_block_hash.0[0] ^= 0xFF;

    let block_id = StacksBlockId::new(&fake_consensus_hash, &fake_block_hash);
    
    // Commit the FULL state (User Txs + Miner Rewards).
    tenure_tx.commit_mined_block(&block_id)?;

    Ok(())
}

/// Re-execute a block simulating a FOLLOWER node.
///
/// A Follower node:
/// 1. Validates and executes transactions (CPU load).
/// 2. Commits the ACTUAL resulting state to the Canonical Chain (MARF).
/// 3. This triggers `external_blobs` logic for the real data generated by the VM.
fn re_execute_nakamoto_follower(
    cs: &mut StacksChainState,
    sortdb: &SortitionDB,
    block: &blockstack_lib::chainstate::nakamoto::NakamotoBlock,
) -> Result<()> {
    // 0. Get Blobs Path (for I/O sync later)
    // We do this early because we can't borrow `cs` while `tenure_tx` is active.
    let blobs_path = cs.clarity_state_index_path.clone();

    // =================================================================================
    // PART 1: CPU Execution (Identical to Miner)
    // =================================================================================
    
    let parent_block_id = block.header.parent_block_id.clone();
    
    // 1. Get Parent Header
    let parent_header = NakamotoChainState::get_block_header(cs.db(), &parent_block_id)?
        .ok_or_else(|| anyhow!("Parent header not found"))?;

    // 2. Setup Builder
    let tenure_change = block.txs.iter().find(|tx| matches!(tx.payload, TransactionPayload::TenureChange(..)));
    let coinbase = block.txs.iter().find(|tx| matches!(tx.payload, TransactionPayload::Coinbase(..)));
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
    //let burn_chain_height = miner_tenure_info.burn_tip_height;
    
    // This transaction (`tenure_tx`) will accumulate all the writes from the VM.
    let mut tenure_tx = builder.tenure_begin(&burn_dbconn, &mut miner_tenure_info)?;

    // 4. Execute Transactions
    // We run the exact same execution logic as the miner. This populates `tenure_tx`
    // with the REAL keys and values (contract state, metadata, etc.).
    for (i, tx) in block.txs.iter().enumerate() {
        println!("  Re-executing Tx #{i} (0x{})", tx.txid());
        let tx_len = tx.tx_len();
        let _ = builder.try_mine_tx_with_len(
            &mut tenure_tx,
            tx,
            tx_len,
            &BlockLimitFunction::NO_LIMIT_HIT,
            None,
        );
    }

    // 5. Finish Block
    // REMOVED: builder.mine_nakamoto_block(&mut tenure_tx, burn_chain_height);
    // We skip this because:
    // 1. The coinbase transaction is already in `block.txs` and was executed in the loop above.
    // 2. We are simulating a Follower (Acceptance), not a Miner (Production).
    // 3. The critical I/O work (Merkle Root calculation + Disk Write) is triggered by `commit_to_block` below.

    // =================================================================================
    // PART 2: I/O Simulation (Follower Commit)
    // =================================================================================
    
    // Use synthetic IDs to avoid corrupting real chain
    // We use 0xAA XOR to distinguish this "Follower Fork" from the "Miner Fork" (0xFF)
    let mut fake_consensus_hash = block.header.consensus_hash.clone();
    fake_consensus_hash.0[0] ^= 0xAA; 
    let mut fake_block_hash = block.header.block_hash();
    fake_block_hash.0[0] ^= 0xAA;
    
    // CRITICAL CHANGE:
    // Instead of `commit_mined_block` (which forces SQLite), we call `commit_to_block`.
    // This tells the MARF to treat this as a canonical block.
    // Because `tenure_tx` is populated with the ACTUAL execution data from above,
    // this will write the real data to the `.blobs` file (if > threshold) or `marf_data` (if small).
    tenure_tx.commit_to_block(&fake_consensus_hash, &fake_block_hash);

    // Force fsync on the blobs file to ensure we measure the I/O latency
    if let Ok(file) = std::fs::File::open(&blobs_path) {
        file.sync_all()?;
    }

    Ok(())
}
