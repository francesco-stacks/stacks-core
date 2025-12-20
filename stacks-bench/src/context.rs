use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::index::marf::MARFOpenOpts;
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use futures::{Stream, StreamExt};
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::StacksBlockId;

use crate::blocks::{BackwardsBlockStream, BlockRef};
use crate::db::app::AppDb;
use crate::db::node::sortition::SortitionDb;
use crate::db::node::{ChainStateDb, NakamotoDb};
use crate::db::{DbOpenForRead, ReadOnly};
use crate::paths::{BurnChainDir, ChainStateDir};
use crate::shadow::{ShadowDir, ShadowDirBuilder, ShadowDirDeltaReport};
use crate::{
    BlockEra, Network, ResolveEpochFromHeight, StacksBlockHeader, StacksBlockRef, StacksEpoch,
};

const BURNCHAIN_NAME: &str = "bitcoin";

pub struct BenchEnvOpts {
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

impl BenchEnvOpts {
    pub fn new<I>(source_dir: PathBuf, network: Network, chain_id: u32, epochs: I) -> Result<Self>
    where
        I: IntoIterator<Item = StacksEpoch>,
    {
        // let epochs = epochs
        //     .into_iter()
        //     .map(|e| e.try_into().map_err(Into::into))
        //     .collect::<Result<Vec<_>>>()?;
        let epochs: Vec<StacksEpoch> = epochs.into_iter().collect();

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

    pub fn with_start_block<T: Into<Option<StacksBlockRef>>>(mut self, start: T) -> Self {
        self.start_at = start.into();
        self
    }

    pub fn with_end_block<T: Into<Option<StacksBlockRef>>>(mut self, end: T) -> Self {
        self.end_at = end.into();
        self
    }

    pub fn with_tip<T: Into<Option<StacksBlockRef>>>(mut self, tip: T) -> Self {
        self.tip = tip.into();
        self
    }

    pub fn with_block_count<T: Into<Option<u32>>>(mut self, count: T) -> Self {
        self.block_count = count.into();
        self
    }
}

pub struct BenchEnv {
    pub is_mainnet: bool,
    pub shadow_dir: ShadowDir,
    pub chainstate_dir: ChainStateDir,
    pub burnchain_dir: BurnChainDir,
    pub epochs: Arc<Vec<StacksEpoch>>,
    pub network: Network,
    pub chain_id: u32,
}

impl BenchEnv {
    pub async fn initialize(opts: BenchEnvOpts) -> Result<(Self, BlockRef)> {
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
        println!(
            "Created shadow directory at {shadow_dir:?} in {:.2?}",
            start.elapsed()
        );

        let burnchain_dir = BurnChainDir::from_node_root(&shadow_dir);
        let chainstate_dir = ChainStateDir::from_node_root(&shadow_dir);

        let is_mainnet = opts.network.is_mainnet();

        // Canonical node tip (id + height) is obtained from sortition db without any Stacks header walking
        let mut sortition_db =
            SortitionDb::open_for_read(burnchain_dir.sortition_db_path()).await?;
        let (node_tip_id, node_tip_height) = sortition_db.get_canonical_stacks_tip().await?;

        let env = Self {
            is_mainnet,
            shadow_dir,
            chainstate_dir,
            burnchain_dir,
            epochs: Arc::new(opts.epochs),
            network: opts.network,
            chain_id: opts.chain_id,
        };

        Ok((
            env,
            BlockRef {
                id: node_tip_id,
                height: node_tip_height,
            },
        ))
    }

    pub async fn open_chainstate_db_for_read(&self) -> Result<ChainStateDb<ReadOnly>> {
        ChainStateDb::<ReadOnly>::open_for_read(self.chainstate_dir.index_db_path()).await
    }

    pub async fn open_nakamoto_db_for_read(&self) -> Result<NakamotoDb<ReadOnly>> {
        NakamotoDb::<ReadOnly>::open_for_read(self.chainstate_dir.nakamoto_db_path()).await
    }

    pub async fn open_sortition_db_for_read(&self) -> Result<SortitionDb<ReadOnly>> {
        SortitionDb::<ReadOnly>::open_for_read(self.burnchain_dir.sortition_db_path()).await
    }
}

pub struct BenchContext {
    is_mainnet: bool,
    shadow_dir: ShadowDir,
    start_block: BlockRef,
    end_block: BlockRef,
    chain_tip: BlockRef,
    epochs: Arc<Vec<StacksEpoch>>,
    network: Network,
    chain_id: u32,
    /// Tracks the cumulative storage growth to calculate per-block deltas
    last_storage_delta: i64,
    chainstate_dir: ChainStateDir,
    burnchain_dir: BurnChainDir,
}

impl BenchContext {
    pub fn from_env(
        env: BenchEnv,
        chain_tip: BlockRef,
        start_block: BlockRef,
        end_block: BlockRef,
    ) -> Self {
        Self {
            is_mainnet: env.is_mainnet,
            shadow_dir: env.shadow_dir,
            chainstate_dir: env.chainstate_dir,
            burnchain_dir: env.burnchain_dir,
            epochs: env.epochs,
            network: env.network,
            chain_id: env.chain_id,
            chain_tip,
            start_block,
            end_block,
            last_storage_delta: 0,
        }
    }

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
    pub fn chain_tip(&self) -> &BlockRef {
        &self.chain_tip
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
        Ok((self.start_block.height, self.end_block.height))
    }

    pub fn start_block(&self) -> &BlockRef {
        &self.start_block
    }

    pub fn end_block(&self) -> &BlockRef {
        &self.end_block
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

    /// Opens the Nakamoto blocks database in read-only mode using our internal,
    /// lightweight [`NakamotoDb`].
    pub async fn open_nakamoto_db_for_read(&self) -> Result<NakamotoDb<ReadOnly>> {
        let nakamoto_db_path = self.chainstate_dir.nakamoto_db_path();
        NakamotoDb::<ReadOnly>::open_for_read(nakamoto_db_path)
            .await
            .with_context(|| "Failed to open nakamoto DB for read")
    }

    /// Opens the Stacks chainstate index database in read-only mode using our
    /// internal, lightweight [`ChainStateDb`].
    pub async fn open_chainstate_db_for_read(&self) -> Result<ChainStateDb<ReadOnly>> {
        let index_db_path = self.chainstate_dir.index_db_path();
        ChainStateDb::<ReadOnly>::open_for_read(index_db_path)
            .await
            .with_context(|| "Failed to open chainstate index DB for read")
    }

    /// Opens the sortition database in read-only mode using our internal,
    /// lightweight [`SortitionDb`].
    pub async fn open_sortition_db_for_read(&self) -> Result<SortitionDb<ReadOnly>> {
        let sortition_db_path = self.burnchain_dir.sortition_db_path();
        SortitionDb::<ReadOnly>::open_for_read(sortition_db_path)
            .await
            .with_context(|| "Failed to open sortition DB for read")
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

    pub async fn initialize(mut app_db: AppDb, opts: BenchEnvOpts) -> Result<Self> {
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

        let mut chainstate_db = ChainStateDb::open_for_read(chainstate_dir.index_db_path()).await?;
        let mut sortition_db =
            SortitionDb::open_for_read(burnchain_dir.sortition_db_path()).await?;

        // 1. Get Node Tip (Global Default)
        println!("Getting canonical stacks tip from sortition db");

        // 2. Determine Anchor Tip
        // This is the block we will walk backwards FROM.
        let anchor_block = match &opts.tip {
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(id)
                    .await?
                    .ok_or_else(|| anyhow!("Tip block {id} not found"))?
                    .try_into()?;
                BlockRef {
                    id: header.id,
                    height: header.height,
                }
            }
            Some(StacksBlockRef::Height(h)) => {
                let h = *h;
                let (node_tip_id, node_tip_height) =
                    sortition_db.get_canonical_stacks_tip().await?;
                if h > node_tip_height {
                    bail!("Requested tip height {h} is beyond node tip {node_tip_height}");
                }
                // Walk back from node tip to find the canonical block at height h
                let mut stream = BackwardsBlockStream::new(chainstate_db, node_tip_id.clone())
                    .with_cache(&mut app_db);
                let header = stream.seek_to_height(h, &node_tip_id).await?;
                chainstate_db = stream.into_inner();
                BlockRef {
                    id: header.id,
                    height: header.height,
                }
            }
            None => {
                let (node_tip_id, node_tip_height) =
                    sortition_db.get_canonical_stacks_tip().await?;
                BlockRef {
                    id: node_tip_id,
                    height: node_tip_height,
                }
            }
        };

        // Resolve start block (must end up as a BlockRef {id,height})
        let start_block: BlockRef = match &opts.start_at {
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(id)
                    .await?
                    .ok_or_else(|| anyhow!("Start block {id} not found"))?
                    .try_into()?;
                BlockRef {
                    id: header.id,
                    height: header.height,
                }
            }
            Some(StacksBlockRef::Height(h)) => BlockRef {
                // We'll resolve the id after we compute end_block (see below)
                // For now, store just the height (id placeholder will be overwritten).
                // NOTE: we must overwrite id before returning from initialize().
                id: StacksBlockId([0u8; 32]),
                height: *h,
            },
            None => BlockRef {
                id: StacksBlockId([0u8; 32]),
                height: 1,
            },
        };

        if start_block.height == 0 {
            bail!("Start height cannot be 0 (genesis block). Please start at height 1 or greater.");
        }

        // Determine effective end reference (height/id)
        let target_end_ref = if let Some(count) = opts.block_count {
            if count == 0 {
                bail!("Block count must be greater than 0");
            }
            Some(StacksBlockRef::Height(
                start_block.height + count as u64 - 1,
            ))
        } else {
            opts.end_at.clone()
        };

        // Resolve end_block as a full BlockRef {id,height}, anchored off anchor_block.id
        let end_block: BlockRef = match target_end_ref {
            Some(StacksBlockRef::Id(id)) => {
                let header: StacksBlockHeader = chainstate_db
                    .get_block_header(&id)
                    .await?
                    .ok_or_else(|| anyhow!("Block {id} not found"))?
                    .try_into()?;
                BlockRef {
                    id: header.id,
                    height: header.height,
                }
            }
            Some(StacksBlockRef::Height(h)) => {
                if h > anchor_block.height {
                    bail!(
                        "Requested end height {h} is beyond anchor tip {}",
                        anchor_block.height
                    );
                }

                if h == anchor_block.height {
                    anchor_block.clone()
                } else {
                    println!(
                        "Resolving canonical block at height {h} (walking back from {})",
                        anchor_block.height
                    );

                    let mut stream =
                        BackwardsBlockStream::new(chainstate_db, anchor_block.id.clone())
                            .with_cache(&mut app_db);
                    let header = stream.seek_to_height(h, &anchor_block.id).await?;
                    chainstate_db = stream.into_inner();

                    BlockRef {
                        id: header.id,
                        height: header.height,
                    }
                }
            }
            None => anchor_block.clone(),
        };

        if start_block.height > end_block.height {
            bail!(
                "start height ({}) > end height ({})",
                start_block.height,
                end_block.height
            );
        }

        // If start was given by height (or defaulted), resolve its canonical id by walking back from end_block.id.
        // This ensures start_block is consistent with the chosen end_block.
        let start_block: BlockRef = match &opts.start_at {
            Some(StacksBlockRef::Id(_)) => start_block,
            _ => {
                if start_block.height == end_block.height {
                    end_block.clone()
                } else {
                    let header = BackwardsBlockStream::new(chainstate_db, end_block.id.clone())
                        .with_cache(&mut app_db)
                        .seek_to_height(start_block.height, &end_block.id)
                        .await?;

                    BlockRef {
                        id: header.id,
                        height: header.height,
                    }
                }
            }
        };

        Ok(Self {
            is_mainnet,
            shadow_dir,
            start_block,
            end_block,
            chain_tip: anchor_block,
            epochs: Arc::new(opts.epochs),
            chain_id,
            network: opts.network,
            last_storage_delta: 0,
            chainstate_dir,
            burnchain_dir,
            //app_db,
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
    pub async fn canonical_block_stream(
        &mut self,
        start_height: u32,
        end_height: u32,
    ) -> impl Stream<Item = Result<StacksBlockHeader>> + '_ {
        let start_height = start_height as u64;
        let end_height = end_height as u64;

        let current_id = self.end_block.id.clone();

        // Open a local handle to the ChainStateDb.
        // We expect this to succeed since the node is running/initialized.
        let chainstate_db_res =
            ChainStateDb::open_for_read(self.chainstate_dir.index_db_path()).await;

        match chainstate_db_res {
            Ok(chainstate_db) => {
                let stream = BackwardsBlockStream::new(chainstate_db, current_id);
                futures::stream::unfold(stream, move |mut bs| async move {
                    loop {
                        match bs.next_block().await {
                            Ok(Some(header)) => {
                                if header.height < start_height {
                                    return None;
                                }
                                if header.height <= end_height {
                                    return Some((Ok(header), bs));
                                }
                                // If not yielding (because we are above end_height), loop continues to walk back
                            }
                            Ok(None) => return Some((Err(anyhow!("Missing header")), bs)),
                            Err(e) => return Some((Err(e), bs)),
                        }
                    }
                })
                .boxed()
            }
            Err(e) => {
                futures::stream::once(
                    async move { Err(anyhow!("Failed to open chainstate DB: {e}")) },
                )
                .boxed()
            }
        }
    }
}

impl ResolveEpochFromHeight for BenchContext {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId> {
        self.epochs.as_slice().resolve_stacks_epoch(height)
    }
}
