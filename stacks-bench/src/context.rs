use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::index::marf::MARFOpenOpts;
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::StacksBlockId;

use crate::db::node::sortition::SortitionDb;
use crate::db::node::{ChainStateDb, NakamotoDb};
use crate::db::{DbOpenForRead, ReadOnly};
use crate::paths::{BurnChainDir, ChainStateDir};
use crate::shadow::{ShadowDir, ShadowDirBuilder, ShadowDirDeltaReport};
use crate::{
    BlockEra, ChainCache, Network, ResolveEpochFromHeight, StacksBlockHeader, StacksBlockRef,
    StacksEpoch,
};

const BURNCHAIN_NAME: &str = "bitcoin";

pub struct BenchContextOpts {
    source_dir: PathBuf,
    network: Network,
    chain_id: u32,
    start_at: Option<StacksBlockRef>,
    end_at: Option<StacksBlockRef>,
    block_count: Option<u32>,
    /// The chain tip to be used by the context.
    tip: Option<StacksBlockRef>,
    /// The epochs which are applicable for the context.
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
            block_count: None,
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

    pub fn with_maybe_block_count(mut self, count: Option<u32>) -> Self {
        self.block_count = count;
        self
    }
}

pub struct BenchContext {
    is_mainnet: bool,
    shadow_dir: ShadowDir,
    start_height: u64,
    end_height: u64,
    tip_height: u64,
    tip_id: StacksBlockId,
    epochs: Arc<Vec<StacksEpoch>>,
    network: Network,
    chain_id: u32,
    /// Tracks the cumulative storage growth to calculate per-block deltas
    last_storage_delta: i64,
    chainstate_dir: ChainStateDir,
}

impl BenchContext {
    pub fn chainstate_dir(&self) -> &ChainStateDir {
        &self.chainstate_dir
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

    /// The Stacks epochs applicable for this context.
    pub fn epochs_arc(&self) -> Arc<Vec<StacksEpoch>> {
        self.epochs.clone()
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

    /// Opens the heavy `StacksChainState` and `Burnchain` databases on demand.
    /// Use this only when you need to execute blocks or access deep chain state.
    pub fn open_stacks_chainstate(&self) -> Result<(StacksChainState, Burnchain)> {
        let burnchain_dir = BurnChainDir::from_node_root(&self.shadow_dir);
        let chainstate_dir = ChainStateDir::from_node_root(&self.shadow_dir);
        let network_name = self.network.to_string();

        let burnchain = Burnchain::new(burnchain_dir.as_str()?, BURNCHAIN_NAME, &network_name)?;

        let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);

        let (chainstate, _) = StacksChainState::open(
            self.is_mainnet,
            self.chain_id,
            chainstate_dir.as_str()?,
            Some(marf_opts),
        )?;

        Ok((chainstate, burnchain))
    }

    pub fn open_nakamoto_db_for_read(&self) -> Result<NakamotoDb<ReadOnly>> {
        let nakamoto_db_path = self.chainstate_dir.nakamoto_db_path();
        NakamotoDb::<ReadOnly>::open_for_read(nakamoto_db_path)
            .with_context(|| "Failed to open nakamoto DB for read")
    }

    /// Calculates the storage delta since the last call to this function.
    /// Updates the internal tracker.
    pub fn update_storage_delta(&mut self) -> Result<i64> {
        let report = self.shadow_dir.calculate_storage_delta()?;
        let current_growth = report.net_growth_bytes;
        let delta = current_growth - self.last_storage_delta;
        self.last_storage_delta = current_growth;
        Ok(delta)
    }

    pub async fn initialize<Cache: ChainCache>(
        opts: BenchContextOpts,
        mut cache: Option<&mut Cache>,
    ) -> Result<Self> {
        println!("Creating shadow directory (this may take a few moments)...");
        let start = Instant::now();
        let shadow_dir = ShadowDirBuilder::new(&opts.source_dir)
            .glob("burnchain/**")
            .glob("chainstate/**")
            .watch("chainstate/vm/clarity/marf.sqlite")
            .watch("chainstate/vm/clarity/marf.sqlite.blobs")
            .watch("chainstate/vm/clarity/marf.sqlite-wal")
            .watch("chainstate/vm/index.sqlite")
            .copy()?;
        let setup_duration = start.elapsed();
        println!("Created shadow directory at {shadow_dir:?} in {setup_duration:.2?}");

        let burnchain_dir = BurnChainDir::from_node_root(&shadow_dir);
        let chainstate_dir = ChainStateDir::from_node_root(&shadow_dir);

        let chain_id = opts.chain_id;
        let is_mainnet = opts.network.is_mainnet();

        let mut chainstate_db = ChainStateDb::open_for_read(chainstate_dir.index_db_path())?;
        let mut sortition_db = SortitionDb::open_for_read(burnchain_dir.sortition_db_path())?;

        // 1. Get Node Tip (Global Default)
        println!("Getting canonical stacks tip from sortition db");
        let (node_tip_id, node_tip_height) = sortition_db.get_canonical_stacks_tip()?;

        // 2. Determine Anchor Tip
        // This is the block we will walk backwards FROM.
        let (anchor_id, anchor_height) = match &opts.tip {
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(id)?
                    .ok_or_else(|| anyhow!("Tip block {id} not found"))?
                    .try_into()?;
                (id.clone(), header.height)
            }
            Some(StacksBlockRef::Height(h)) => {
                let h = *h;
                if h > node_tip_height {
                    bail!("Requested tip height {h} is beyond node tip {node_tip_height}");
                }
                // Walk back from node tip to find the canonical block at height h
                let mut curr = node_tip_id.clone();
                let mut curr_h = node_tip_height;

                // Check cache to jump closer if we know an ancestor
                if let Some(c) = &mut cache {
                    if let Ok(Some((cached_id, cached_h))) =
                        c.find_closest_ancestor(&node_tip_id, h).await
                    {
                        if cached_h < curr_h && cached_h >= h {
                            println!(
                                "  [Cache Hit] Jumping from height {curr_h} to {cached_h} ({cached_id})"
                            );
                            curr = cached_id;
                            curr_h = cached_h;
                        }
                    }
                }

                while curr_h > h {
                    let header: StacksBlockHeader = chainstate_db
                        .get_block_header(&curr)?
                        .ok_or_else(|| anyhow!("Missing header for {curr}"))?
                        .try_into()?;
                    curr = header.parent_id;
                    curr_h = header.height.saturating_sub(1);

                    // Populate cache periodically
                    if curr_h % 10_000 == 0 {
                        if let Some(c) = &mut cache {
                            let _ = c.cache_ancestor(&node_tip_id, curr_h, &curr).await;
                        }
                    }
                }
                (curr, h)
            }
            None => (node_tip_id, node_tip_height),
        };

        // Resolve start height
        let start_height = match &opts.start_at {
            Some(StacksBlockRef::Height(h)) => *h,
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(id)?
                    .ok_or_else(|| anyhow!("Start block {id} not found"))?
                    .try_into()?;
                header.height
            }
            None => 1,
        };

        if start_height == 0 {
            bail!("Start height cannot be 0 (genesis block). Please start at height 1 or greater.");
        }

        // Determine effective end block
        let target_end_ref = if let Some(count) = opts.block_count {
            if count == 0 {
                bail!("Block count must be greater than 0");
            }
            Some(StacksBlockRef::Height(start_height + count as u64 - 1))
        } else {
            opts.end_at.clone()
        };

        let (end_id, end_height) = match target_end_ref {
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(&id)?
                    .ok_or_else(|| anyhow!("Block {id} not found"))?
                    .try_into()?;
                (id.clone(), header.height)
            }
            Some(StacksBlockRef::Height(h)) => {
                let h = h;
                if h > anchor_height {
                    bail!("Requested end height {h} is beyond anchor tip {anchor_height}");
                }

                if h == anchor_height {
                    (anchor_id.clone(), anchor_height)
                } else {
                    println!(
                        "Resolving canonical block at height {h} (walking back from {anchor_height})"
                    );

                    let mut curr = anchor_id.clone();
                    let mut curr_h = anchor_height;

                    // Check cache to jump closer if we know an ancestor
                    if let Some(c) = &mut cache {
                        if let Ok(Some((cached_id, cached_h))) =
                            c.find_closest_ancestor(&anchor_id, h).await
                        {
                            if cached_h < curr_h && cached_h >= h {
                                println!(
                                    "  [Cache Hit] Jumping from height {curr_h} to {cached_h} ({cached_id})"
                                );
                                curr = cached_id;
                                curr_h = cached_h;
                            }
                        }
                    }

                    while curr_h > h {
                        // Fetch header for the current block
                        let header: StacksBlockHeader = chainstate_db
                            .get_block_header(&curr)?
                            .ok_or_else(|| anyhow!("Missing header for {curr}"))?
                            .try_into()?;

                        // Move to the parent block
                        curr = header.parent_id;

                        // Update height (parent is always current - 1)
                        curr_h = header.height.saturating_sub(1);

                        // Populate cache periodically
                        if curr_h % 10_000 == 0 {
                            if let Some(c) = &mut cache {
                                let _ = c.cache_ancestor(&anchor_id, curr_h, &curr).await;
                                eprint!("."); // Progress/activity indicator
                            }
                        }
                    }
                    println!();

                    // Cache the final result too
                    if let Some(c) = &mut cache {
                        let _ = c.cache_ancestor(&anchor_id, curr_h, &curr).await;
                    }

                    (curr, h)
                }
            }
            None => (anchor_id.clone(), anchor_height),
        };

        if start_height > end_height {
            bail!("start height ({start_height}) > end height ({end_height})");
        }

        Ok(Self {
            is_mainnet,
            shadow_dir,
            start_height,
            end_height: end_height, // end_height is the benchmark tip height
            tip_height: end_height, // tip_height is now the benchmark tip height
            tip_id: end_id,         // tip_id is now the benchmark tip ID
            epochs: Arc::new(opts.epochs),
            chain_id,
            network: opts.network,
            last_storage_delta: 0,
            chainstate_dir,
        })
    }

    pub fn calculate_storage_delta(&self) -> Result<ShadowDirDeltaReport> {
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
    ) -> impl Iterator<Item = Result<StacksBlockHeader>> + '_ {
        let start_height = start_height as u64;
        let end_height = end_height as u64;

        let mut current_id = self.tip_id.clone();

        // Open a local handle to the ChainStateDb.
        // We expect this to succeed since the node is running/initialized.
        // Note: We use expect() here because we can't easily return a Result from the iterator setup,
        // and a failure here implies a critical environment issue.
        let mut chainstate_db = ChainStateDb::open_for_read(self.chainstate_dir.index_db_path())
            .expect("Failed to open chainstate DB for reading");

        std::iter::from_fn(move || {
            loop {
                // 1. Get Header via the unified query
                let header_res = chainstate_db.get_block_header(&current_id);

                let header = match header_res {
                    Ok(Some(h)) => h,
                    Ok(None) => return Some(Err(anyhow!("Missing header for {current_id}"))),
                    Err(e) => return Some(Err(anyhow!("DB error: {e}"))),
                };

                // 2. Convert to StacksBlockHeader
                // This uses the TryInto impl in models.rs which populates:
                // id, hash, parent_id, height, burn_block_hash, burn_block_height
                let stacks_header: StacksBlockHeader = match header.try_into() {
                    Ok(h) => h,
                    Err(e) => return Some(Err(anyhow!("Conversion error: {e}"))),
                };

                let current_height = stacks_header.height;

                if current_height < start_height {
                    return None;
                }

                // Prepare next ID
                let parent_id = stacks_header.parent_id.clone();
                let should_yield = current_height <= end_height;

                current_id = parent_id;

                if should_yield {
                    return Some(Ok(stacks_header));
                }

                // If not yielding (because we are above end_height), loop continues to walk back
            }
        })
    }
}

impl ResolveEpochFromHeight for BenchContext {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId> {
        self.epochs.as_slice().resolve_stacks_epoch(height)
    }
}
