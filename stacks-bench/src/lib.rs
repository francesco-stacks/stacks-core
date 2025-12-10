use std::fmt::Display;
use std::io::Cursor;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::stacks::StacksTransaction;
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use clarity::codec::StacksMessageCodec;
use clarity::consts::{CHAIN_ID_MAINNET, CHAIN_ID_TESTNET};
use clarity::types::StacksEpochId;
use clarity::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash};
use serde::{Deserialize, Serialize};
use stacks_common::types::chainstate::StacksBlockId;

use crate::db::ReadOnly;
use crate::db::node::NakamotoDb;

pub mod context;
pub mod db;
pub mod indexer;
pub mod metrics;
pub mod paths;
pub mod profiler;
pub mod replay;
pub mod shadow;

/// Trait for caching and retrieving block ancestors to speed up chain walking.
pub trait ChainCache {
    /// Finds the closest known ancestor of `tip` that has a height >= `target_height`.
    /// Returns `Some((block_id, height))` if found.
    fn find_closest_ancestor(
        &self,
        tip: &StacksBlockId,
        target_height: u64,
    ) -> impl Future<Output = Result<Option<(StacksBlockId, u64)>>>;

    /// Caches a known ancestor for a given tip.
    fn cache_ancestor(
        &mut self,
        tip: &StacksBlockId,
        height: u64,
        block: &StacksBlockId,
    ) -> impl Future<Output = Result<()>>;
}

#[derive(Debug, Clone)]
pub struct StacksEpoch {
    epoch_id: StacksEpochId,
    start_block_height: u64,
    end_block_height: u64,
}

impl StacksEpoch {
    pub fn new(epoch_id: StacksEpochId, start_block_height: u64, end_block_height: u64) -> Self {
        Self {
            epoch_id,
            start_block_height,
            end_block_height,
        }
    }

    pub fn epoch_id(&self) -> StacksEpochId {
        self.epoch_id
    }

    pub fn start_block_height(&self) -> u64 {
        self.start_block_height
    }

    pub fn end_block_height(&self) -> u64 {
        self.end_block_height
    }
}

pub trait ResolveEpochFromHeight {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId>;
}

impl ResolveEpochFromHeight for [StacksEpoch] {
    fn resolve_stacks_epoch(&self, height: u64) -> Option<StacksEpochId> {
        for epoch in self {
            if height >= epoch.start_block_height && height <= epoch.end_block_height {
                return Some(epoch.epoch_id);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StacksBlockRef {
    Id(StacksBlockId),
    Height(u64),
}

impl FromStr for StacksBlockRef {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(height) = s.parse::<u64>() {
            Ok(Self::Height(height))
        } else if let Ok(block_id) = StacksBlockId::from_hex(s) {
            Ok(Self::Id(block_id))
        } else {
            bail!("invalid block identifier: {s} (expected u64 height or hex block hash)")
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
    pub fn resolve_block_height(&self, chainstate: &StacksChainState) -> Result<u64> {
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
                    .map(|h| h)
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
                "Network mismatch: CLI specified {self}, but DB is configured for {}",
                if db_mainnet {
                    "mainnet"
                } else {
                    "testnet/regtest"
                }
            ));
        }

        if db_chain_id != expected_chain_id {
            return Err(format!(
                "Chain ID mismatch: CLI expects {expected_chain_id} (0x{expected_chain_id:x}), \
                but DB has {db_chain_id} (0x{db_chain_id:x})",
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
            _ => Err(anyhow!("invalid network: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BlockEra {
    PreNakamoto,
    Nakamoto,
}

#[derive(Debug, Clone)]
pub struct StacksBlockHeader {
    pub id: StacksBlockId,
    pub hash: BlockHeaderHash,
    pub height: u64,
    pub parent_id: StacksBlockId,
    pub burn_block_height: u32,
    pub burn_block_hash: BurnchainHeaderHash,
}

#[derive(Debug, Clone)]
pub struct BlockTransactions(Vec<StacksTransaction>);

impl Deref for BlockTransactions {
    type Target = Vec<StacksTransaction>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for BlockTransactions {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl AsRef<[StacksTransaction]> for BlockTransactions {
    fn as_ref(&self) -> &[StacksTransaction] {
        &self.0
    }
}

impl BlockTransactions {
    pub fn load<P: AsRef<Path>>(
        naka_db: &mut NakamotoDb<ReadOnly>,
        blocks_dir: P,
        epoch: StacksEpochId,
        block: &StacksBlockHeader,
    ) -> Result<Self> {
        let is_nakamoto = StacksEpochId::ALL_GTE_30.contains(&epoch);
        let block_id = &block.id;

        if !is_nakamoto {
            let stacks_block = load_block_from_disk(blocks_dir, block_id)
                .with_context(|| format!("Failed to load StacksBlock {block_id} from disk"))?;
            Ok(Self(stacks_block.txs))
        } else {
            let naka_block_bytes = naka_db
                .get_nakamoto_block(block_id)
                .with_context(|| format!("Failed to load Nakamoto block {block_id} from DB"))?
                .ok_or_else(|| anyhow!("Nakamoto block not found: {block_id}"))?
                .data;

            let mut cursor = Cursor::new(naka_block_bytes);
            NakamotoBlock::consensus_deserialize(&mut cursor)
                .with_context(|| format!("Failed to deserialize Nakamoto block {block_id}"))
                .map(|naka_block| Self(naka_block.txs))
        }
    }

    pub fn as_slice(&self) -> &[StacksTransaction] {
        &self.0
    }
}

fn load_block_from_disk<P: AsRef<Path>>(
    blocks_dir: P,
    block_id: &StacksBlockId,
) -> Result<blockstack_lib::chainstate::stacks::StacksBlock> {
    let blocks_dir_str = blocks_dir.as_ref().to_string_lossy();
    let block_path = StacksChainState::get_index_block_path(&blocks_dir_str, block_id)
        .context("Failed to resolve block path")?;
    println!("Loading block from disk: {block_path}");

    let mut file = std::fs::File::open(&block_path)
        .with_context(|| format!("Failed to open block file: {block_path}"))?;
    let stacks_block =
        blockstack_lib::chainstate::stacks::StacksBlock::consensus_deserialize(&mut file)
            .with_context(|| {
                format!("Failed to deserialize StacksBlock from file: {block_path}")
            })?;
    Ok(stacks_block)
}
