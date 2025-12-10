use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use diesel::prelude::*;
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::{BlockHeaderHash, ConsensusHash, StacksBlockId};

use crate::db::{DbConn, DbOpen, SqliteDbHandle};

pub mod models;
pub mod schema;

pub struct SortitionDb<Mode> {
    handle: SqliteDbHandle<Mode>,
}

impl<Mode> Deref for SortitionDb<Mode> {
    type Target = SqliteDbHandle<Mode>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<Mode> DerefMut for SortitionDb<Mode> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl<Mode> DbOpen<Mode> for SortitionDb<Mode>
where
    SqliteDbHandle<Mode>: DbOpen<Mode>,
{
    fn open(path: PathBuf) -> Result<Self> {
        Ok(Self {
            handle: SqliteDbHandle::open(path)?,
        })
    }
}

impl<Mode> SortitionDb<Mode> {
    pub fn get_epochs(&mut self) -> Result<Vec<models::Epoch>> {
        use schema::epochs::dsl::*;

        epochs
            .load::<models::Epoch>(self.conn_mut())
            .context(format!("Failed to load epochs from sortition db"))
    }

    pub fn get_canonical_stacks_tip(&mut self) -> Result<(StacksBlockId, u64)> {
        use schema::epochs::dsl as ep;
        use schema::snapshots::dsl as sn;
        use schema::stacks_chain_tips::dsl as sct;

        // 1. Get canonical burn chain tip
        // Logic: The canonical tip is the highest block with pox_valid=1.
        // Ties are broken by burn_header_hash ASC (deterministic tie-breaking).
        let tip_snapshot: models::Snapshot = sn::snapshots
            .filter(sn::pox_valid.eq(1))
            .order((sn::block_height.desc(), sn::burn_header_hash.asc()))
            .first(self.conn_mut())
            .context("Failed to get canonical burn chain tip")?;

        // 2. Get epoch for this height to determine how to find the Stacks tip
        let epoch_id_val: i32 = ep::epochs
            .select(ep::epoch_id)
            .filter(ep::start_block_height.le(tip_snapshot.block_height))
            .filter(ep::end_block_height.ge(tip_snapshot.block_height))
            .first(self.conn_mut())
            .context("Failed to get epoch for tip height")?;

        let epoch_id: StacksEpochId = (epoch_id_val as u32).try_into().map_err(|e| {
            anyhow!("Could not convert epoch {epoch_id_val} into a StacksEpochId: {e}")
        })?;

        if epoch_id >= StacksEpochId::Epoch30 {
            // Nakamoto Behavior:
            // The Stacks tip is stored in `stacks_chain_tips`.
            // If the current sortition (burn block) doesn't have a Stacks tip recorded,
            // we must walk back up the sortition history until we find one.
            let mut current_snapshot = tip_snapshot;
            loop {
                let chain_tip_opt: Option<models::StacksChainTip> = sct::stacks_chain_tips
                    .filter(sct::sortition_id.eq(&current_snapshot.sortition_id))
                    .order(sct::block_height.desc())
                    .first(self.conn_mut())
                    .optional()?;

                if let Some(tip) = chain_tip_opt {
                    return Ok((
                        StacksBlockId::new(
                            &ConsensusHash::from_hex(&tip.consensus_hash)?,
                            &BlockHeaderHash::from_hex(&tip.block_hash)?,
                        ),
                        tip.block_height as u64,
                    ));
                }

                // Move to parent sortition
                let parent_opt: Option<models::Snapshot> = sn::snapshots
                    .filter(sn::sortition_id.eq(&current_snapshot.parent_sortition_id))
                    .first(self.conn_mut())
                    .optional()?;

                match parent_opt {
                    Some(p) => current_snapshot = p,
                    None => {
                        return Err(anyhow!(
                            "Failed to find parent sortition for {}",
                            current_snapshot.sortition_id
                        ));
                    }
                }
            }
        } else {
            // Pre-Nakamoto Behavior:
            // The Stacks tip is stored directly on the snapshot row.
            return Ok((
                StacksBlockId::new(
                    &ConsensusHash::from_hex(&tip_snapshot.canonical_stacks_tip_consensus_hash)?,
                    &BlockHeaderHash::from_hex(&tip_snapshot.canonical_stacks_tip_hash)?,
                ),
                tip_snapshot.canonical_stacks_tip_height as u64,
            ));
        }
    }
}
