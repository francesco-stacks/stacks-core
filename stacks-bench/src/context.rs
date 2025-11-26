use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::stacks::db::{StacksBlockHeaderTypes, StacksChainState};
use blockstack_lib::chainstate::stacks::index::marf::MARFOpenOpts;
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use clarity::types::StacksEpochId;
use clarity::types::chainstate::StacksBlockId;

use crate::shadow::{ShadowDir, ShadowDirBuilder};
use crate::{
    BlockChain, BlockEra, BlockSummary, BlockTransactions, BurnChainPath, ChainStatePath, Network,
    ResolveEpochFromHeight, StacksBlockRef, StacksEpoch,
};

const BURNCHAIN_NAME: &str = "bitcoin";

pub struct BenchContextOpts {
    source_dir: PathBuf,
    network: Network,
    chain_id: u32,
    start_at: Option<StacksBlockRef>,
    end_at: Option<StacksBlockRef>,
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
}

pub struct BenchContext {
    is_mainnet: bool,
    shadow_dir: ShadowDir,
    chainstate: StacksChainState,
    burnchain: Burnchain,
    start_height: u32,
    end_height: u32,
    tip_height: u32,
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

    /// Returns the Stacks chain tip as a `(StacksBlockId, u32)`] tuple.
    pub fn chain_tip(&self) -> (StacksBlockId, u32) {
        (self.tip_id.clone(), self.tip_height)
    }

    pub fn resolve_block_era(&self, epoch: StacksEpochId) -> BlockEra {
        if epoch >= StacksEpochId::Epoch30 {
            BlockEra::Nakamoto
        } else {
            BlockEra::PreNakamoto
        }
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

        println!("Getting canonical stacks tip block id directly from sortdb");
        let tip_id = sortition_db.get_canonical_stacks_tip_block_id();
        let tip_header = NakamotoChainState::get_block_header(chainstate.db(), &tip_id)?
            .ok_or(anyhow!("Failed to get tip block header"))?;
        let tip_height = tip_header.stacks_block_height as u32;

        // Resolve height bounds before walking
        let start_height = match &opts.start_at {
            Some(block_ref) => block_ref.resolve_block_height(&chainstate)?,
            None => 1, // genesis height
        };
        let mut end_height = match &opts.end_at {
            Some(block_ref) => block_ref.resolve_block_height(&chainstate)?,
            None => tip_height,
        };
        if start_height > end_height {
            bail!("start height ({start_height}) > end height ({end_height})");
        }
        if end_height > tip_height {
            end_height = tip_height;
        }

        Ok(Self {
            is_mainnet,
            shadow_dir,
            chainstate,
            burnchain,
            start_height,
            end_height,
            tip_height,
            tip_id,
            epochs: opts.epochs,
        })
    }

    pub fn select_blocks(&self) -> Result<BlockChain> {
        let sortition_db = self.burnchain().open_sortition_db(false)?;

        println!(
            "Building canonical chain window [{start_height}, {end_height}] (tip at {tip_height})...",
            start_height = self.start_height,
            end_height = self.end_height,
            tip_height = self.tip_height,
        );
        let blocks = collect_canonical_range(
            &self.chainstate,
            &sortition_db,
            &self.tip_id,
            self.start_height,
            self.end_height,
            &self.epochs,
        )?;
        println!(
            "Collected {} canonical blocks ({}..={})",
            blocks.len(),
            blocks.first().map(|b| b.height).unwrap_or(0),
            blocks.last().map(|b| b.height).unwrap_or(0)
        );

        Ok(BlockChain::new_ascending(blocks))
    }

    pub fn calculate_storage_delta(&self) -> Result<(i64, u64)> {
        self.shadow_dir
            .calculate_storage_delta()
            .context("Failed to calculate storage delta")
    }
}

/// Collect canonical blocks within `[start_height, end_height]` inclusive, in ascending order.
/// Walks from tip down and breaks once below `start_height`. Prints periodic status.
fn collect_canonical_range(
    chainstate: &StacksChainState,
    sortition_db: &SortitionDB,
    tip_id: &StacksBlockId,
    start_height: u32,
    end_height: u32,
    epochs: &[StacksEpoch],
) -> Result<Vec<BlockSummary>> {
    let mut current = tip_id.clone();
    let mut out_desc = Vec::new();

    let started = Instant::now();
    let mut last_report = started;
    let report_every = Duration::from_secs(1);
    let mut headers_seen = 0;
    let mut txs_seen = 0;

    // Track collected window stats
    let mut collected_min: Option<u32> = None;
    let mut collected_max: Option<u32> = None;

    eprintln!(
        "Scanning canonical chain downwards, target window [{}, {}]...",
        start_height, end_height
    );

    loop {
        if current == StacksBlockId::first_mined() {
            break;
        }
        let current_header = NakamotoChainState::get_block_header(chainstate.db(), &current)?
            .ok_or_else(|| anyhow!("Could not find block header for {current}"))?;

        // Extract block, parent, AND the consensus hash (tenure ID) from the header
        let (mut block, consensus_hash) = match &current_header.anchored_header {
            StacksBlockHeaderTypes::Epoch2(_) => {
                let block_id = current_header.index_block_hash();
                debug_assert_eq!(block_id, current);

                let parent = chainstate
                    .get_parent(&current)
                    .with_context(|| format!("get_parent({current})"))?;

                // For Pre-Nakamoto, we must look up the consensus hash from the index
                let (consensus_hash, header_hash) = chainstate
                    .get_block_header_hashes(&current)
                    .with_context(|| format!("get_block_header_hashes({current})"))?
                    .ok_or_else(|| anyhow!("missing hashes for {current}"))?;

                let height = chainstate
                    .get_stacks_block_height(&consensus_hash, &header_hash)
                    .with_context(|| format!("get_stacks_block_height({current})"))?
                    .ok_or_else(|| anyhow!("missing height for {current}"))?
                    as u32;

                let epoch = epochs.resolve_stacks_epoch(height.into()).ok_or_else(|| {
                    anyhow!("Could not resolve epoch for block at height {height}")
                })?;

                (
                    BlockSummary::new(current.clone(), parent.clone(), height, epoch),
                    //parent,
                    consensus_hash,
                )
            }
            StacksBlockHeaderTypes::Nakamoto(header) => {
                let height = header.chain_length;
                let epoch = epochs.resolve_stacks_epoch(height).ok_or_else(|| {
                    anyhow!("Could not resolve epoch for block at height {height}")
                })?;

                (
                    BlockSummary::new(
                        header.block_id(),
                        header.parent_block_id.clone(),
                        header.chain_length as u32,
                        epoch,
                    ),
                    header.consensus_hash.clone(),
                )
            }
        };

        let parent_id = block.parent_id.clone();
        let height = block.height;

        if height < start_height {
            break;
        }
        if height <= end_height {
            // 1. Resolve Burn Info using the extracted consensus_hash
            let snapshot =
                SortitionDB::get_block_snapshot_consensus(sortition_db.conn(), &consensus_hash)
                    .map_err(|e| anyhow!("SortitionDB error: {e}"))?
                    .ok_or_else(|| {
                        anyhow!("Consensus hash {consensus_hash} not found in SortitionDB")
                    })?;

            block = block.with_burn_info(snapshot.block_height as u32, snapshot.burn_header_hash);
            let epoch = epochs
                .resolve_stacks_epoch(height.into())
                .ok_or_else(|| anyhow!("Could not resolve epoch for block at height {height}"))?;

            // 2. Attach transactions (only for in-range blocks)
            let txs = BlockTransactions::load(chainstate, epoch, &block)
                .with_context(|| format!("load transactions for {}", block.id))?;
            txs_seen += txs.len();

            out_desc.push(block.with_transactions(txs));

            // Update collected window stats
            collected_max = match collected_max {
                None => Some(height),
                Some(prev) => Some(prev.max(height)),
            };
            collected_min = match collected_min {
                None => Some(height),
                Some(prev) => Some(prev.min(height)),
            };
        }

        headers_seen += 1;
        if last_report.elapsed() >= report_every {
            let elapsed = started.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 {
                headers_seen as f64 / elapsed
            } else {
                0.0
            };
            match (collected_min, collected_max) {
                (Some(lo), Some(hi)) => {
                    eprintln!(
                        "  scanned {headers_seen:>8} headers in {elapsed:>6.2}s ({rate:>7.2} hdr/s), current height: {height:>8}, collected: [{lo}..={hi}] ({} blocks, {txs_seen} txs)",
                        out_desc.len()
                    );
                }
                _ => {
                    eprintln!(
                        "  scanned {headers_seen:>8} headers in {elapsed:>6.2}s ({rate:>7.2} hdr/s), current height: {height:>8}, (not yet within collection window)",
                    );
                }
            }
            last_report = Instant::now();
        }

        current = parent_id;
    }

    out_desc.reverse();

    let elapsed = started.elapsed().as_secs_f64();
    eprintln!(
        "Done. Selected {} blocks [{}..={}] in {elapsed:.2}s",
        out_desc.len(),
        out_desc.first().map(|b| b.height).unwrap_or(0),
        out_desc.last().map(|b| b.height).unwrap_or(0),
    );

    Ok(out_desc)
}
