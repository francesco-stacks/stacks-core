use std::fmt::{LowerHex, UpperHex};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use blockstack_lib::burnchains::Burnchain;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::stacks::db::{StacksBlockHeaderTypes, StacksChainState};
use blockstack_lib::chainstate::stacks::index::marf::MARFOpenOpts;
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use blockstack_lib::core::{CHAIN_ID_MAINNET, CHAIN_ID_TESTNET};
use clap::{Parser, ValueEnum};
use stacks_bench::shadow::ShadowDirBuilder;
use stacks_bench::{Block, BlockChain, BlockTransactions, BurnChainPath, ChainStatePath};
use stacks_common::types::chainstate::StacksBlockId;

const BURNCHAIN_NAME: &str = "bitcoin";

#[derive(Parser)]
#[command(name = "stacks-bench", about)]
pub struct Args {
    /// Stacks node data path (the directory containing the `chainstate` folder)
    #[arg(long = "source", short = 's', value_name = "SOURCE_DIR")]
    source_dir: PathBuf,

    /// The Stacks network which the no
    #[arg(long, short = 'n', default_value_t = NetworkArg::Testnet)]
    network: NetworkArg,

    #[arg(long, conflicts_with = "txid")]
    start_at: Option<BlockArg>,

    #[arg(long, conflicts_with = "txid")]
    end_at: Option<BlockArg>,

    #[arg(long, short = 'c', conflicts_with_all = &["end-at", "txid"])]
    count: Option<u32>,

    #[arg(long, conflicts_with_all = &["start-at", "end-at", "count"])]
    txid: Option<TxIdArg>,
}

#[derive(Debug, Clone)]
struct TxIdArg([u8; 32]);

impl FromStr for TxIdArg {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| anyhow!("invalid hex in txid: {}", e))?;
        if bytes.len() != 32 {
            return Err(anyhow!(
                "invalid txid length: expected 32 bytes, got {} bytes",
                bytes.len()
            ));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(TxIdArg(arr))
    }
}

impl std::fmt::Display for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl LowerHex for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

impl UpperHex for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02X}", byte)?;
        }
        Ok(())
    }
}

/// The Stacks network from which the node data is sourced.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum NetworkArg {
    Mainnet,
    Testnet,
    Regtest,
}

impl NetworkArg {
    pub fn is_mainnet(&self) -> bool {
        matches!(self, Self::Mainnet)
    }

    pub fn to_chain_id(&self) -> u32 {
        match self {
            Self::Mainnet => CHAIN_ID_MAINNET,
            Self::Testnet | Self::Regtest => CHAIN_ID_TESTNET,
        }
    }

    // pub fn initial_balances(&self) -> Result<Vec<(PrincipalData, u64)>> {
    //     let use_test_chainstate_data = matches!(self, Self::Testnet | Self::Regtest);
    //     GenesisData::new(use_test_chainstate_data)
    //         .read_balances()
    //         .map(|item| -> Result<(PrincipalData, u64)> {
    //             let principal = PrincipalData::parse(&item.address)?;
    //             Ok((principal, item.amount))
    //         })
    //         .collect()
    // }
}

impl ToString for NetworkArg {
    fn to_string(&self) -> String {
        match self {
            Self::Mainnet => "mainnet".to_string(),
            Self::Testnet => "testnet".to_string(),
            Self::Regtest => "regtest".to_string(),
        }
    }
}

#[derive(Clone)]
pub enum BlockArg {
    Id(StacksBlockId),
    Height(u32),
}

impl FromStr for BlockArg {
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

impl std::fmt::Display for BlockArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockArg::Id(block_id) => write!(f, "{block_id}"),
            BlockArg::Height(h) => write!(f, "{h}"),
        }
    }
}

/// Resolve a BlockArg to a height directly from the DB (no BlockChain needed).
fn resolve_block_arg_height(cs: &StacksChainState, arg: &BlockArg) -> Result<u32> {
    match arg {
        BlockArg::Height(h) => Ok(*h),
        BlockArg::Id(block_id) => {
            let (consensus_hash, header_hash) = cs
                .get_block_header_hashes(block_id)
                .with_context(|| format!("lookup header hashes for {block_id}"))?
                .ok_or_else(|| anyhow!("missing header hashes for {block_id}"))?;

            cs.get_stacks_block_height(&consensus_hash, &header_hash)
                .with_context(|| format!("lookup height for {block_id}"))?
                .ok_or_else(|| anyhow!("missing height for {block_id}"))
                .map(|h| h as u32)
        }
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
) -> Result<Vec<Block>> {
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
        let (mut block, parent_id, consensus_hash) = match &current_header.anchored_header {
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

                (
                    Block::new_pre_nakamoto(current.clone(), parent.clone(), height),
                    parent,
                    consensus_hash,
                )
            }
            StacksBlockHeaderTypes::Nakamoto(header) => {
                let b: Block = header.into();
                let p = b.parent_id.clone();
                // For Nakamoto, the consensus_hash in the header IS the tenure ID (Bitcoin block hash)
                (b, p, header.consensus_hash.clone())
            }
        };

        let h = block.height;

        if h < start_height {
            break;
        }
        if h <= end_height {
            // 1. Resolve Burn Info using the extracted consensus_hash
            let snapshot = SortitionDB::get_block_snapshot_consensus(sortition_db.conn(), &consensus_hash)
                .map_err(|e| anyhow!("SortitionDB error: {e}"))?
                .ok_or_else(|| {
                    anyhow!("Consensus hash {consensus_hash} not found in SortitionDB")
                })?;

            block = block.with_burn_info(snapshot.block_height as u32, snapshot.burn_header_hash);

            // 2. Attach transactions (only for in-range blocks)
            let txs = BlockTransactions::load(chainstate, &block)
                .with_context(|| format!("load transactions for {}", block.id))?;
            txs_seen += txs.len();

            out_desc.push(block.with_transactions(txs));

            // Update collected window stats
            collected_max = match collected_max {
                None => Some(h),
                Some(prev) => Some(prev.max(h)),
            };
            collected_min = match collected_min {
                None => Some(h),
                Some(prev) => Some(prev.min(h)),
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
                        "  scanned {headers_seen:>8} headers in {elapsed:>6.2}s ({rate:>7.2} hdr/s), current height: {h:>8}, collected: [{lo}..={hi}] ({} blocks, {txs_seen} txs)",
                        out_desc.len()
                    );
                }
                _ => {
                    eprintln!(
                        "  scanned {headers_seen:>8} headers in {elapsed:>6.2}s ({rate:>7.2} hdr/s), current height: {h:>8}, (not yet within collection window)",
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

fn main() -> Result<()> {
    let args = Args::parse();

    let start = Instant::now();
    let shadow = ShadowDirBuilder::new(&args.source_dir)
        .glob("burnchain/**")
        .glob("chainstate/**")
        .copy()?;
    let duration = start.elapsed();
    println!(
        "Created shadow directory at {:?} in {:.2?}",
        shadow.as_ref(),
        duration
    );

    let is_mainnet = args.network.is_mainnet();
    let network_name = args.network.to_string();
    let chain_id = args.network.to_chain_id();

    let burnchain_path = BurnChainPath::from_node_root(&shadow);
    let chainstate_path = ChainStatePath::from_node_root(&shadow);

    let burnchain = Burnchain::new(burnchain_path.as_str()?, BURNCHAIN_NAME, &network_name)?;
    let (mut sortition_db, _burnchain_db) = burnchain.open_db(false)?;

    let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);

    let (mut chainstate, _) = StacksChainState::open(
        is_mainnet,
        chain_id,
        chainstate_path.as_str()?,
        Some(marf_opts),
    )?;

    println!("Getting canonical stacks tip block id directly from sortdb");
    let stacks_tip_id = sortition_db.get_canonical_stacks_tip_block_id();
    let stacks_tip_header = NakamotoChainState::get_block_header(chainstate.db(), &stacks_tip_id)?
        .ok_or(anyhow!("Failed to get tip block header"))?;
    let tip_height = stacks_tip_header.stacks_block_height as u32;

    // Resolve height bounds before walking
    let start_h = match &args.start_at {
        Some(arg) => resolve_block_arg_height(&chainstate, arg)?,
        None => 1, // genesis height
    };
    let mut end_h = match &args.end_at {
        Some(arg) => resolve_block_arg_height(&chainstate, arg)?,
        None => tip_height,
    };
    if start_h > end_h {
        bail!("start height ({start_h}) > end height ({end_h})");
    }
    if end_h > tip_height {
        end_h = tip_height;
    }

    println!("Building canonical chain window [{start_h}, {end_h}] (tip at {tip_height})...");
    let blocks = collect_canonical_range(&chainstate, &sortition_db, &stacks_tip_id, start_h, end_h)?;
    println!(
        "Collected {} canonical blocks ({}..={})",
        blocks.len(),
        blocks.first().map(|b| b.height).unwrap_or(0),
        blocks.last().map(|b| b.height).unwrap_or(0)
    );

    let selected = BlockChain::new_ascending(blocks);
    
    println!("Re-executing {} selected blocks...", selected.len());
    let start = Instant::now();
    for block in selected.iter() {
        println!("Re-executing block at height {} ({})", block.height, block.id);
        stacks_bench::replay::re_execute_block(&mut chainstate, &mut sortition_db, block)?;
    }
    let duration = start.elapsed();
    println!("Re-executed {} blocks in {duration:.2?}", selected.len());

    // Drop DBs to ensure all files are closed/databases are checkpointed before measuring storage delta
    drop(chainstate);
    drop(sortition_db);
    drop(_burnchain_db);

    // Give the OS a moment to sync metadata
    std::thread::sleep(Duration::from_millis(100));

    let (growth, written) = shadow.calculate_storage_delta()?;

    println!("Storage Delta:");
    println!("  Net Growth:        {:.4} MB ({growth} bytes)", growth as f64 / 1_024.0 / 1_024.0);
    println!("  Est. Data Written: {:.4} MB ({written} bytes)", written as f64 / 1_024.0 / 1_024.0);

    Ok(())
}
