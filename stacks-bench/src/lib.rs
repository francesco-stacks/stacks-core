use std::fmt::Display;
use std::ops::{Bound, Deref, RangeBounds};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader};
use blockstack_lib::chainstate::stacks::StacksTransaction;
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use clarity::codec::StacksMessageCodec;
use clarity::consts::{CHAIN_ID_MAINNET, CHAIN_ID_TESTNET};
use clarity::types::chainstate::BurnchainHeaderHash;
use serde::{Deserialize, Serialize};
use stacks_common::types::chainstate::StacksBlockId;

pub mod db;
pub mod context;
pub mod replay;
pub mod shadow;
pub mod profiler;


pub struct BurnChainPath(PathBuf);

impl BurnChainPath {
    pub const BURNCHAIN_DIR_NAME: &'static str = "burnchain";

    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        BurnChainPath(path.into())
    }

    pub fn from_node_root<P: AsRef<Path>>(node_root: P) -> Self {
        Self::new(node_root.as_ref().join("burnchain"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> Result<&str> {
        self.path()
            .to_str()
            .ok_or(anyhow!("Failed to convert burnchain path to str"))
    }
}

impl AsRef<Path> for BurnChainPath {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

pub struct ChainStatePath(PathBuf);

impl ChainStatePath {
    pub const CHAINSTATE_DIR_NAME: &'static str = "chainstate";
    pub const INDEX_DB_RELATIVE_FILE_PATH: &'static str = "vm/index.sqlite";

    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        ChainStatePath(path.into())
    }

    pub fn from_node_root<P: AsRef<Path>>(node_root: P) -> Self {
        Self::new(node_root.as_ref().join("chainstate"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> Result<&str> {
        self.path()
            .to_str()
            .ok_or(anyhow!("Failed to convert chainstate path to str"))
    }

    pub fn index_db_path(&self) -> PathBuf {
        self.path().join(Self::INDEX_DB_RELATIVE_FILE_PATH)
    }
}

impl AsRef<Path> for ChainStatePath {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StacksBlockRef {
    Id(StacksBlockId),
    Height(u32),
}

impl FromStr for StacksBlockRef {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(height) = s.parse::<u32>() {
            Ok(Self::Height(height))
        } else if let Ok(block_id) = StacksBlockId::from_hex(s) {
            Ok(Self::Id(block_id))
        } else {
            bail!("invalid block identifier: {s} (expected u32 height or hex block hash)")
        }
    }
}

impl std::fmt::Display for StacksBlockRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StacksBlockRef::Id(block_id) => write!(f, "{block_id}"),
            StacksBlockRef::Height(h) => write!(f, "{h}"),
        }
    }
}

impl StacksBlockRef {
    pub fn resolve_block_height(&self, chainstate: &StacksChainState) -> Result<u32> {
        match self {
            StacksBlockRef::Height(h) => Ok(*h),
            StacksBlockRef::Id(block_id) => {
                let (consensus_hash, header_hash) = chainstate
                    .get_block_header_hashes(block_id)
                    .with_context(|| format!("lookup header hashes for {block_id}"))?
                    .ok_or_else(|| anyhow!("missing header hashes for {block_id}"))?;

                chainstate
                    .get_stacks_block_height(&consensus_hash, &header_hash)
                    .with_context(|| format!("lookup height for {block_id}"))?
                    .ok_or_else(|| anyhow!("missing height for {block_id}"))
                    .map(|h| h as u32)
            }
        }
    }
}

/// The Stacks network from which the node data is sourced.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Network {
    Mainnet,
    Testnet,
    Regtest,
}

impl Network {
    pub fn is_mainnet(&self) -> bool {
        matches!(self, Self::Mainnet)
    }

    pub fn to_chain_id(&self) -> u32 {
        match self {
            Self::Mainnet => CHAIN_ID_MAINNET,
            Self::Testnet | Self::Regtest => CHAIN_ID_TESTNET,
        }
    }

    /// Validates that the provided database configuration matches this network.
    pub fn validate_chainstate(&self, db_mainnet: bool, db_chain_id: u32) -> Result<(), String> {
        let expected_mainnet = self.is_mainnet();
        let expected_chain_id = self.to_chain_id();

        if db_mainnet != expected_mainnet {
            return Err(format!(
                "Network mismatch: CLI specified {}, but DB is configured for {}",
                self,
                if db_mainnet { "mainnet" } else { "testnet/regtest" }
            ));
        }

        if db_chain_id != expected_chain_id {
            return Err(format!(
                "Chain ID mismatch: CLI expects {} (0x{:x}), but DB has {} (0x{:x})",
                expected_chain_id, expected_chain_id, db_chain_id, db_chain_id
            ));
        }

        Ok(())
    }
}

impl Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mainnet => write!(f, "mainnet"),
            Self::Testnet => write!(f, "testnet"),
            Self::Regtest => write!(f, "regtest"),
        }
    }
}

impl FromStr for Network {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(Self::Mainnet),
            "testnet" => Ok(Self::Testnet),
            "regtest" => Ok(Self::Regtest),
            _ => Err(anyhow!("invalid network: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BlockEra {
    PreNakamoto,
    Nakamoto,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: StacksBlockId,
    pub parent_id: StacksBlockId,
    pub height: u32,
    pub era: BlockEra,
    pub burn_block_height: Option<u32>,
    pub burn_block_hash: Option<BurnchainHeaderHash>,
    txs: Option<BlockTransactions>,
}

impl From<&NakamotoBlockHeader> for Block {
    fn from(naka_header: &NakamotoBlockHeader) -> Self {
        Block {
            id: naka_header.block_id(),
            parent_id: naka_header.parent_block_id.clone(),
            height: naka_header.chain_length as u32,
            era: BlockEra::Nakamoto,
            burn_block_height: None,
            burn_block_hash: None,
            txs: None,
        }
    }
}

impl From<NakamotoBlock> for Block {
    fn from(naka_block: NakamotoBlock) -> Self {
        Block {
            id: naka_block.block_id(),
            parent_id: naka_block.header.parent_block_id,
            height: naka_block.header.chain_length as u32,
            era: BlockEra::Nakamoto,
            burn_block_height: None,
            burn_block_hash: None,
            txs: None,
        }
    }
}

impl Block {
    pub fn new_pre_nakamoto(id: StacksBlockId, parent_id: StacksBlockId, height: u32) -> Self {
        Block {
            id,
            parent_id,
            height,
            era: BlockEra::PreNakamoto,
            burn_block_height: None,
            burn_block_hash: None,
            txs: None,
        }
    }

    pub fn with_transactions(mut self, txs: BlockTransactions) -> Self {
        self.txs = Some(txs);
        self
    }

    pub fn with_burn_info(mut self, height: u32, hash: BurnchainHeaderHash) -> Self {
        self.burn_block_height = Some(height);
        self.burn_block_hash = Some(hash);
        self
    }

    pub fn transactions(&self) -> Option<&[StacksTransaction]> {
        self.txs.as_ref().map(|t| t.as_slice())
    }
}

pub struct BlockChain(Vec<Block>);

impl AsRef<[Block]> for BlockChain {
    fn as_ref(&self) -> &[Block] {
        &self.0
    }
}

impl Deref for BlockChain {
    type Target = Vec<Block>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl BlockChain {
    pub fn new_ascending<I: IntoIterator<Item = Block>>(blocks: I) -> Self {
        let mut v: Vec<Block> = blocks.into_iter().collect();
        v.sort_unstable_by_key(|b| b.height);

        // Validate strictly increasing by height
        if !v.windows(2).all(|w| w[0].height < w[1].height) {
            panic!("BlockChain invariant violated: duplicate or unsorted heights");
        }

        Self(v)
    }

    /// Get block by its height, if one exists.
    pub fn get_block_by_height(&self, height: u32) -> Option<&Block> {
        self.0.iter().find(|b| b.height == height)
    }

    /// Get block by its [`StacksBlockId`], if one exists.
    pub fn get_block_by_id(&self, id: &StacksBlockId) -> Option<&Block> {
        self.0.iter().find(|b| &b.id == id)
    }

    /// Find the first Nakamoto-era block in the chain, if any.
    pub fn first_nakamoto_block(&self) -> Option<&Block> {
        self.0.iter().find(|b| matches!(b.era, BlockEra::Nakamoto))
    }

    /// Total transaction count across all blocks in the chain.
    pub fn transaction_count(&self) -> usize {
        self.0
            .iter()
            .flat_map(|b| b.txs.as_ref())
            .map(|t| t.len())
            .sum()
    }

    /// Clamp a height range (RangeBounds<u32>) to the canonical chain.
    /// - Missing bounds snap to nearest valid height.
    /// - If the clamped start > end, returns an empty chain.
    ///
    /// Returns an owned BlockChain (copies the selected window).
    pub fn clamp_by_height_range<R: RangeBounds<u32>>(&self, range: R) -> Self {
        let asc = &self.0;
        if asc.is_empty() {
            return Self(Vec::new());
        }
        match Self::clamp_indices_for_height_range(asc, range) {
            Some((s, e)) => Self(asc[s..=e].to_vec()),
            None => Self(Vec::new()),
        }
    }

    /// Clamp by optional inclusive start/end heights (convenience).
    /// Equivalent to clamp_by_height_range(start..=end), with None unbounded.
    pub fn clamp_by_height(&self, start: Option<u32>, end: Option<u32>) -> Self {
        let start_bound = start.map_or(Bound::Unbounded, Bound::Included);
        let end_bound = end.map_or(Bound::Unbounded, Bound::Included);
        self.clamp_by_height_range((start_bound, end_bound))
    }

    /// Borrowed slice view of a clamped height range (no allocation).
    /// Returns an empty slice if out-of-range after clamping.
    pub fn slice_by_height_range_clamped<R: RangeBounds<u32>>(&self, range: R) -> &[Block] {
        let asc = &self.0;
        if asc.is_empty() {
            return &asc[0..0];
        }
        match Self::clamp_indices_for_height_range(asc, range) {
            Some((s, e)) => &asc[s..=e],
            None => &asc[0..0],
        }
    }

    /// Internal: compute clamped [start,end] indices for a height range.
    fn clamp_indices_for_height_range<R: RangeBounds<u32>>(
        asc: &[Block],
        range: R,
    ) -> Option<(usize, usize)> {
        // Start index (clamped up to the next present height)
        let s_idx = match range.start_bound() {
            Bound::Unbounded => 0,
            Bound::Included(&h) => match asc.binary_search_by_key(&h, |b| b.height) {
                Ok(i) => i,
                Err(ip) => ip, // next higher
            },
            Bound::Excluded(&h) => match asc.binary_search_by_key(&h, |b| b.height) {
                Ok(i) => i.saturating_add(1),
                Err(ip) => ip, // next higher
            },
        };

        // End index (clamped down to the previous present height)
        let e_idx = match range.end_bound() {
            Bound::Unbounded => asc.len() - 1,
            Bound::Included(&h) => match asc.binary_search_by_key(&h, |b| b.height) {
                Ok(i) => i,
                Err(ip) => ip.saturating_sub(1), // previous lower
            },
            Bound::Excluded(&h) => match asc.binary_search_by_key(&h, |b| b.height) {
                Ok(i) => i.saturating_sub(1),
                Err(ip) => ip.saturating_sub(1), // previous lower
            },
        };

        if s_idx <= e_idx && s_idx < asc.len() {
            Some((s_idx, e_idx))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockTransactions(Vec<StacksTransaction>);

impl Deref for BlockTransactions {
    type Target = Vec<StacksTransaction>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[StacksTransaction]> for BlockTransactions {
    fn as_ref(&self) -> &[StacksTransaction] {
        &self.0
    }
}

impl BlockTransactions {
    pub fn load(chainstate: &StacksChainState, block: &Block) -> Result<Self> {
        match block.era {
            BlockEra::PreNakamoto => {
                // Need consensus/header hashes to locate the on-disk bytes
                let (consensus_hash, header_hash) = chainstate
                    .get_block_header_hashes(&block.id)
                    .map_err(|_| anyhow!("Failed to get header hashes for {}", block.id))?
                    .ok_or_else(|| anyhow!("Missing header hashes for {}", block.id))?;

                // Read bytes and parse an anchored StacksBlock
                let bytes = StacksChainState::load_block_bytes(
                    &chainstate.blocks_path,
                    &consensus_hash,
                    &header_hash,
                )
                .map_err(|e| anyhow!("load_block_bytes {}: {e}", block.id))?
                .ok_or_else(|| anyhow!("Block bytes not found for {}", block.id))?;

                // Deserialize to StacksBlock and take txs
                let mut cursor = std::io::Cursor::new(bytes);
                let stacks_block =
                    blockstack_lib::chainstate::stacks::StacksBlock::consensus_deserialize(
                        &mut cursor,
                    )
                    .map_err(|e| anyhow!("Failed to deserialize StacksBlock {}: {e}", block.id))?;
                Ok(Self(stacks_block.txs))
            }
            BlockEra::Nakamoto => {
                // Get full block from the nakamoto blocks DB
                let (naka_block, _size) = chainstate
                    .nakamoto_blocks_db()
                    .get_nakamoto_block(&block.id)
                    .map_err(|e| anyhow!("nakamoto get_nakamoto_block {}: {e}", block.id))?
                    .ok_or_else(|| anyhow!("Nakamoto block not found: {}", block.id))?;
                Ok(Self(naka_block.txs))
            }
        }
    }

    pub fn as_slice(&self) -> &[StacksTransaction] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::*;

    /// Test helper to create a [`Block`] with the specified height.
    fn mk_block(height: u32) -> Block {
        let id = StacksBlockId::first_mined();
        Block::new_pre_nakamoto(id.clone(), id, height)
    }

    /// Test helper to create a [`BlockChain`] from a list of heights.
    fn mk_chain(heights: &[u32]) -> BlockChain {
        let blocks = heights.iter().copied().map(mk_block);
        BlockChain::new_ascending(blocks)
    }

    #[test]
    fn clamp_unbounded_returns_all() {
        let chain = mk_chain(&[10, 20, 30, 40, 50]);
        let selected = chain.clamp_by_height_range(..);
        assert_eq!(
            selected
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![10, 20, 30, 40, 50]
        );

        let slice = chain.slice_by_height_range_clamped(..);
        assert_eq!(
            slice.iter().map(|b| b.height).collect::<Vec<_>>(),
            vec![10, 20, 30, 40, 50]
        );
    }

    #[test]
    fn clamp_bounds_are_clamped_to_existing() {
        let chain = mk_chain(&[100, 200, 300]);
        // Below min and above max should clamp to [100, 300]
        let selected = chain.clamp_by_height_range(0..=u32::MAX);
        assert_eq!(
            selected
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![100, 200, 300]
        );

        // Start below min, end inside
        let selected = chain.clamp_by_height_range(0..=200);
        assert_eq!(
            selected
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![100, 200]
        );

        // Start inside, end above max
        let selected = chain.clamp_by_height_range(200..=u32::MAX);
        assert_eq!(
            selected
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![200, 300]
        );
    }

    #[test]
    fn clamp_inclusive_exclusive_semantics() {
        let chain = mk_chain(&[10, 20, 30, 40]);

        // Inclusive range picks exact heights
        let inclusive = chain.clamp_by_height_range(20..=30);
        assert_eq!(
            inclusive
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );

        // Exclusive end drops the end height
        let excl_end = chain.clamp_by_height_range(20..30);
        assert_eq!(
            excl_end
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![20]
        );

        // Fully exclusive using explicit bounds: (20, 40) -> picks 30 only
        let excl_both = chain.clamp_by_height_range((Bound::Excluded(20), Bound::Excluded(40)));
        assert_eq!(
            excl_both
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![30]
        );

        // Exclusively outside a single element yields empty
        let empty = chain.clamp_by_height_range((Bound::Excluded(20), Bound::Excluded(30)));
        assert!(empty.as_ref().is_empty());
    }

    #[test]
    fn clamp_by_height_convenience_none_bounds() {
        let chain = mk_chain(&[5, 10, 15, 20]);

        // None..=15
        let left_open = chain.clamp_by_height(None, Some(15));
        assert_eq!(
            left_open
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![5, 10, 15]
        );

        // 10..=None
        let right_open = chain.clamp_by_height(Some(10), None);
        assert_eq!(
            right_open
                .as_ref()
                .iter()
                .map(|b| b.height)
                .collect::<Vec<_>>(),
            vec![10, 15, 20]
        );

        // None..=None -> all
        let all = chain.clamp_by_height(None, None);
        assert_eq!(
            all.as_ref().iter().map(|b| b.height).collect::<Vec<_>>(),
            vec![5, 10, 15, 20]
        );
    }

    #[test]
    fn slice_variant_returns_borrowed_window() {
        let chain = mk_chain(&[1, 2, 3, 4, 5]);

        // Borrowed slice for a middle window
        let window = chain.slice_by_height_range_clamped(2..=4);
        assert_eq!(
            window.iter().map(|b| b.height).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        // Out-of-range clamps to empty slice
        let empty = chain.slice_by_height_range_clamped(6..=9);
        assert!(empty.is_empty());
    }

    #[test]
    fn empty_chain_clamps_to_empty() {
        let chain = mk_chain(&[]);
        let owned = chain.clamp_by_height_range(..);
        assert!(owned.as_ref().is_empty());

        let slice = chain.slice_by_height_range_clamped(..);
        assert!(slice.is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate or unsorted heights")]
    fn constructing_with_duplicate_heights_panics() {
        // new_ascending sorts first, then validates strictly increasing; duplicates should panic.
        let _ = mk_chain(&[10, 20, 20, 30]);
    }
}
