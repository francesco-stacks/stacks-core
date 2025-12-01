use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::stacks::db::{StacksBlockHeaderTypes, StacksChainState};
use blockstack_lib::chainstate::stacks::index::marf::MARFOpenOpts;
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::StacksBlockId;

use crate::paths::{BurnChainPath, ChainStatePath};
use crate::shadow::{ShadowDir, ShadowDirBuilder};
use crate::{
    BlockEra, BlockSummary, BlockTransactions, Network, ResolveEpochFromHeight, StacksBlockRef,
    StacksEpoch,
};

const BURNCHAIN_NAME: &str = "bitcoin";

pub struct BenchContextOpts {
    source_dir: PathBuf,
    network: Network,
    chain_id: u32,
    start_at: Option<StacksBlockRef>,
    end_at: Option<StacksBlockRef>,
    tip: Option<StacksBlockRef>,
    epochs: Vec<StacksEpoch>,
}

impl BenchContextOpts {
    pub fn new<T, I>(
        source_dir: PathBuf,
        network: Network,
        chain_id: u32,
        epochs: I,
    ) -> Result<Self>
    where
        T: TryInto<StacksEpoch>,
        T::Error: Into<anyhow::Error>,
        I: IntoIterator<Item = T>,
    {
        let epochs = epochs
            .into_iter()
            .map(|e| e.try_into().map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            source_dir,
            network,
            chain_id,
            start_at: None,
            end_at: None,
            tip: None,
            epochs,
        })
    }

    pub fn with_start_block(mut self, start: StacksBlockRef) -> Self {
        self.start_at = Some(start);
        self
    }

    pub fn with_maybe_start_block(mut self, start: Option<StacksBlockRef>) -> Self {
        if let Some(s) = start {
            self.start_at = Some(s);
        }
        self
    }

    pub fn with_end_block(mut self, end: StacksBlockRef) -> Self {
        self.end_at = Some(end);
        self
    }

    pub fn with_maybe_end_block(mut self, end: Option<StacksBlockRef>) -> Self {
        if let Some(e) = end {
            self.end_at = Some(e);
        }
        self
    }

    pub fn with_maybe_tip(mut self, tip: Option<StacksBlockRef>) -> Self {
        self.tip = tip;
        self
    }
}

pub struct BenchContext {
    is_mainnet: bool,
    shadow_dir: ShadowDir,
    chainstate: StacksChainState,
    burnchain: Burnchain,
    start_height: u64,
    end_height: u64,
    tip_height: u64,
    tip_id: StacksBlockId,
    epochs: Vec<StacksEpoch>,
}

impl BenchContext {
    pub fn chainstate(&self) -> &StacksChainState {
        &self.chainstate
    }

    pub fn chainstate_mut(&mut self) -> &mut StacksChainState {
        &mut self.chainstate
    }

    pub fn burnchain(&self) -> &Burnchain {
        &self.burnchain
    }

    pub fn burnchain_mut(&mut self) -> &mut Burnchain {
        &mut self.burnchain
    }

    pub fn is_mainnet(&self) -> bool {
        self.is_mainnet
    }

    pub fn source_dir(&self) -> &Path {
        self.shadow_dir.source()
    }

    /// Returns the Stacks chain tip as a `(StacksBlockId, u64)`] tuple.
    pub fn chain_tip(&self) -> (StacksBlockId, u64) {
        (self.tip_id.clone(), self.tip_height)
    }

    pub fn resolve_block_era(&self, epoch: StacksEpochId) -> BlockEra {
        if epoch >= StacksEpochId::Epoch30 {
            BlockEra::Nakamoto
        } else {
            BlockEra::PreNakamoto
        }
    }

    /// Returns the target block range as `(start_height, end_height)`.
    pub fn block_height_range(&self) -> Result<(u64, u64)> {
        Ok((self.start_height, self.end_height))
    }

    pub fn with_databases_mut<F, R>(&mut self, func: F) -> Result<R>
    where
        F: FnOnce(&mut StacksChainState, &mut Burnchain) -> Result<R>,
    {
        func(&mut self.chainstate, &mut self.burnchain)
    }

    pub fn get_databases_mut(&mut self) -> (&mut StacksChainState, &mut Burnchain) {
        (&mut self.chainstate, &mut self.burnchain)
    }

    pub fn initialize(opts: BenchContextOpts) -> Result<Self> {
        let start = Instant::now();
        let shadow_dir = ShadowDirBuilder::new(&opts.source_dir)
            .glob("burnchain/**")
            .glob("chainstate/**")
            .copy()?;
        let setup_duration = start.elapsed();
        println!(
            "Created shadow directory at {:?} in {:.2?}",
            &shadow_dir, setup_duration
        );

        let burnchain_path = BurnChainPath::from_node_root(&shadow_dir);
        let chainstate_path = ChainStatePath::from_node_root(&shadow_dir);

        let chain_id = opts.chain_id;
        let is_mainnet = opts.network.is_mainnet();
        let network_name = opts.network.to_string();

        let burnchain = Burnchain::new(burnchain_path.as_str()?, BURNCHAIN_NAME, &network_name)?;
        let (sortition_db, _burnchain_db) = burnchain.open_db(false)?;

        let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);

        let (chainstate, _) = StacksChainState::open(
            is_mainnet,
            chain_id,
            chainstate_path.as_str()?,
            Some(marf_opts),
        )?;

        // 1. Get Node Tip (Global Default)
        println!("Getting canonical stacks tip block id directly from sortdb");
        let node_tip_id = sortition_db.get_canonical_stacks_tip_block_id();
        let node_tip_header = NakamotoChainState::get_block_header(chainstate.db(), &node_tip_id)?
            .ok_or(anyhow!("Failed to get tip block header"))?;
        let node_tip_height = node_tip_header.stacks_block_height;

        // 2. Determine Anchor Tip
        // This is the block we will walk backwards FROM.
        let (anchor_id, anchor_height) = match &opts.tip {
            Some(StacksBlockRef::Id(id)) => {
                let header = NakamotoChainState::get_block_header(chainstate.db(), id)?
                    .ok_or_else(|| anyhow!("Tip block {} not found", id))?;
                (id.clone(), header.stacks_block_height)
            }
            Some(StacksBlockRef::Height(h)) => {
                let h = *h;
                if h > node_tip_height {
                    bail!(
                        "Requested tip height {} is beyond node tip {}",
                        h,
                        node_tip_height
                    );
                }
                // Walk back from node tip to find the canonical block at height h
                // (Same logic as before, but now establishing the anchor)
                let mut curr = node_tip_id.clone();
                let mut curr_h = node_tip_height;
                while curr_h > h {
                    let header = NakamotoChainState::get_block_header(chainstate.db(), &curr)?
                        .ok_or_else(|| anyhow!("Missing header for {}", curr))?;
                    curr = match header.anchored_header {
                        StacksBlockHeaderTypes::Epoch2(_) => chainstate.get_parent(&curr)?,
                        StacksBlockHeaderTypes::Nakamoto(n) => n.parent_block_id,
                    };
                    curr_h -= 1;
                }
                (curr, h)
            }
            None => (node_tip_id, node_tip_height),
        };

        // 3. Determine Effective End (Benchmark End)
        // We walk back from the ANCHOR tip, not necessarily the node tip.
        let (end_id, end_height) = match &opts.end_at {
            Some(StacksBlockRef::Id(id)) => {
                let header = NakamotoChainState::get_block_header(chainstate.db(), id)?
                    .ok_or_else(|| anyhow!("Block {} not found", id))?;
                (id.clone(), header.stacks_block_height)
            }
            Some(StacksBlockRef::Height(h)) => {
                let h = *h;
                if h > anchor_height {
                    bail!(
                        "Requested end height {} is beyond anchor tip {}",
                        h,
                        anchor_height
                    );
                }

                if h == anchor_height {
                    (anchor_id.clone(), anchor_height)
                } else {
                    println!(
                        "Resolving canonical block at height {} (walking back from {})...",
                        h, anchor_height
                    );
                    let mut curr = anchor_id.clone();
                    let mut curr_h = anchor_height;

                    while curr_h > h {
                        let header = NakamotoChainState::get_block_header(chainstate.db(), &curr)?
                            .ok_or_else(|| anyhow!("Missing header for {}", curr))?;

                        curr = match header.anchored_header {
                            StacksBlockHeaderTypes::Epoch2(_) => chainstate.get_parent(&curr)?,
                            StacksBlockHeaderTypes::Nakamoto(n) => n.parent_block_id,
                        };
                        curr_h -= 1;
                    }
                    (curr, h)
                }
            }
            None => (anchor_id.clone(), anchor_height),
        };

        // 3. Resolve Start Height
        let start_height = match &opts.start_at {
            Some(block_ref) => block_ref.resolve_block_height(&chainstate)?,
            None => 1, // genesis height
        };

        if start_height > end_height {
            bail!("start height ({start_height}) > end height ({end_height})");
        }

        Ok(Self {
            is_mainnet,
            shadow_dir,
            chainstate,
            burnchain,
            start_height,
            end_height: end_height, // end_height is the benchmark tip height
            tip_height: end_height, // tip_height is now the benchmark tip height
            tip_id: end_id,         // tip_id is now the benchmark tip ID
            epochs: opts.epochs,
        })
    }

    pub fn calculate_storage_delta(&self) -> Result<(i64, u64)> {
        self.shadow_dir
            .calculate_storage_delta()
            .context("Failed to calculate storage delta")
    }

    /// Returns an iterator over canonical blocks in the range [start_height, end_height].
    /// The iterator yields blocks in descending order (from end_height down to start_height).
    /// This is efficient because it follows parent links from the tip.
    pub fn canonical_block_stream(
        &self,
        start_height: u32,
        end_height: u32,
    ) -> impl Iterator<Item = Result<BlockSummary>> + '_ {
        let start_height = start_height as u64;
        let end_height = end_height as u64;

        let mut current_id = self.tip_id.clone();
        // We'll lazily initialize the sortition DB to avoid keeping it open if not needed immediately,
        // but for an iterator we need it.
        let sortition_db = self
            .burnchain
            .open_sortition_db(false)
            .expect("Failed to open sortition DB");

        std::iter::from_fn(move || {
            loop {
                // 1. Get Header
                // Note: NakamotoChainState::get_block_header returns Result<Option<Header>>
                let header_res =
                    NakamotoChainState::get_block_header(self.chainstate.db(), &current_id);
                let header = match header_res {
                    Ok(Some(h)) => h,
                    Ok(None) => return Some(Err(anyhow!("Missing header for {}", current_id))),
                    Err(e) => return Some(Err(anyhow!("DB error: {}", e))),
                };

                let current_height = header.stacks_block_height;

                if current_height < start_height {
                    return None;
                }

                // 2. Resolve BlockSummary
                let (summary, consensus_hash) = match &header.anchored_header {
                    StacksBlockHeaderTypes::Epoch2(_) => {
                        let parent_res = self.chainstate.get_parent(&current_id);
                        let parent = match parent_res {
                            Ok(p) => p,
                            Err(e) => return Some(Err(anyhow!("Failed to get parent: {}", e))),
                        };

                        let hashes_res = self.chainstate.get_block_header_hashes(&current_id);
                        let (consensus_hash, _) = match hashes_res {
                            Ok(Some(h)) => h,
                            Ok(None) => {
                                return Some(Err(anyhow!("Missing hashes for {}", current_id)));
                            }
                            Err(e) => return Some(Err(anyhow!("DB error: {}", e))),
                        };

                        let epoch = match self.epochs.resolve_stacks_epoch(current_height) {
                            Some(e) => e,
                            None => {
                                return Some(Err(anyhow!(
                                    "Unknown epoch for height {}",
                                    current_height
                                )));
                            }
                        };

                        (
                            BlockSummary::new(current_id.clone(), parent, current_height, epoch),
                            consensus_hash,
                        )
                    }
                    StacksBlockHeaderTypes::Nakamoto(h) => {
                        let epoch = match self.epochs.resolve_stacks_epoch(current_height) {
                            Some(e) => e,
                            None => {
                                return Some(Err(anyhow!(
                                    "Unknown epoch for height {}",
                                    current_height
                                )));
                            }
                        };
                        (
                            BlockSummary::new(
                                h.block_id(),
                                h.parent_block_id.clone(),
                                h.chain_length,
                                epoch,
                            ),
                            h.consensus_hash.clone(),
                        )
                    }
                };

                // Prepare next ID
                let parent_id = summary.parent_id.clone();
                let should_yield = current_height <= end_height;

                current_id = parent_id;

                if should_yield {
                    let snapshot_res = SortitionDB::get_block_snapshot_consensus(
                        sortition_db.conn(),
                        &consensus_hash,
                    );
                    let snapshot = match snapshot_res {
                        Ok(Some(s)) => s,
                        Ok(None) => {
                            return Some(Err(anyhow!(
                                "Consensus hash {} not found in SortitionDB",
                                consensus_hash
                            )));
                        }
                        Err(e) => return Some(Err(anyhow!("SortitionDB error: {}", e))),
                    };

                    let summary = summary
                        .with_burn_info(snapshot.block_height as u32, snapshot.burn_header_hash);

                    let txs_res =
                        BlockTransactions::load(&self.chainstate, summary.epoch, &summary);
                    let txs = match txs_res {
                        Ok(t) => t,
                        Err(e) => return Some(Err(anyhow!("Failed to load txs: {}", e))),
                    };

                    println!(
                        "canonical_block_stream: Yielding block at height {}",
                        summary.height
                    );
                    return Some(Ok(summary.with_transactions(txs)));
                }

                // If not yielding (because we are above end_height), loop continues to walk back
            }
        })
    }
}
