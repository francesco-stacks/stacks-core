use anyhow::{Result, anyhow};
use stacks_common::types::chainstate::StacksBlockId;

use crate::Network;
use crate::context::BenchContext;
use crate::db::app::AppDb;
use crate::db::node::sortition::models::Epoch;

pub struct ChainstateIndexer<'a> {
    app_db: &'a mut AppDb,
    context: &'a mut BenchContext,
}

impl<'a> ChainstateIndexer<'a> {
    pub fn new(app_db: &'a mut AppDb, context: &'a mut BenchContext) -> Self {
        Self { app_db, context }
    }

    pub fn index_chainstate(
        &mut self,
        network: Network,
        chain_id: u32,
        epochs: &[Epoch],
    ) -> Result<Vec<StacksBlockId>> {
        let (tip_id, tip_height) = self.context.chain_tip();
        let (start_height, end_height) = self.context.block_height_range()?;

        println!(
            "Targeting block range: {} to {} (Tip: {})",
            start_height, end_height, tip_height
        );

        let (_chainstate_model, _) = self
            .app_db
            .get_or_create_chainstate(network, chain_id, &tip_id, tip_height, epochs)?;

        // Get canonical block IDs from App DB
        let mut block_ids =
            self.app_db
                .get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)?;

        let expected_count = (end_height - start_height + 1) as usize;

        if block_ids.len() != expected_count {
            println!(
                "App DB index incomplete (found {}, expected {}). Indexing from Node DB...",
                block_ids.len(),
                expected_count
            );

            // Stream from node DB and index
            let stream = self
                .context
                .canonical_block_stream(start_height as u32, end_height as u32)
                .filter_map(|r| match r {
                    Ok(b) => Some(b),
                    Err(e) => {
                        eprintln!("Warning: failed to load block during indexing: {}", e);
                        None
                    }
                });

            self.app_db.index_blocks_streaming(stream)?;
            println!("Checkpointing database...");
            self.app_db.checkpoint()?;
            println!("Vacuuming database...");
            self.app_db.vacuum()?;

            // Reload IDs
            block_ids =
                self.app_db
                    .get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)?;
        }

        if block_ids.is_empty() {
            return Err(anyhow!(
                "No blocks found in range {} to {}",
                start_height,
                end_height
            ));
        }

        Ok(block_ids)
    }
}
