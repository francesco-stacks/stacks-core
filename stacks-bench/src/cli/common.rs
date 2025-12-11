use std::fmt::{LowerHex, UpperHex};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use stacks_bench::context::{BenchContext, BenchContextOpts};
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::AppDb;
use stacks_bench::db::node::ChainStateDb;
use stacks_bench::db::node::sortition::SortitionDb;
use stacks_bench::paths::{AppDataDir, BurnChainDir, ChainStateDir};
use stacks_bench::{Network, StacksBlockRef};

pub struct CliContext {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    app_data_dir: AppDataDir,
    /// The application database.
    app_db: AppDb,
}

impl CliContext {
    pub fn new(app_data_dir: AppDataDir, app_db: AppDb) -> Self {
        Self {
            app_data_dir,
            app_db,
        }
    }

    pub fn app_data_dir(&self) -> &AppDataDir {
        &self.app_data_dir
    }

    pub fn app_db(&self) -> AppDb {
        self.app_db.clone()
    }
}

pub trait IndexerArgs {
    fn source_dir(&self) -> &PathBuf;
    fn start_at(&self) -> Option<&StacksBlockRef>;
    fn end_at(&self) -> Option<&StacksBlockRef>;
    fn block_count(&self) -> Option<u32>;
    fn tip(&self) -> Option<&StacksBlockRef>;
    fn network(&self) -> Option<Network>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxIdArg([u8; 32]);

impl FromStr for TxIdArg {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).with_context(|| format!("invalid hex in txid '{s}'"))?;
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

// Helper to get the current git commit hash
pub fn get_git_hash() -> Option<Vec<u8>> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let s = String::from_utf8_lossy(&output.stdout);
            hex::decode(s.trim()).ok()
        })
}

pub async fn setup_bench_context<T: IndexerArgs>(
    app_db: &mut AppDb,
    args: &T,
) -> Result<(
    BenchContext,
    Network,
    u32,
    Vec<stacks_bench::db::node::sortition::models::Epoch>,
)> {
    let chainstate_path = ChainStateDir::from_node_root(args.source_dir());
    let burnchain_path = BurnChainDir::from_node_root(args.source_dir());

    // Resolve network and chain ID
    let (network, chain_id) = {
        let chainstate_db = ChainStateDb::open_for_read(chainstate_path.index_db_path()).await?;
        let db_config = chainstate_db.read_db_config().await?;

        let network = if let Some(n) = args.network() {
            db_config.assert_matches_network(n)?;
            n
        } else if db_config.is_mainnet() {
            Network::Mainnet
        } else {
            Network::Testnet
        };
        (network, db_config.chain_id())
    };

    println!("Using network: {}", network.to_string().to_uppercase());

    // Load epochs
    let epochs = {
        let mut sortition_db =
            SortitionDb::open_for_read(burnchain_path.sortition_db_path()).await?;
        let epochs = sortition_db.get_epochs().await?;
        let epochs_str = epochs
            .iter()
            .map(|e| {
                e.to_stacks_epoch_id()
                    .map(|id| {
                        let id_display = id.to_string().replace(".", "_");
                        format!(
                            "{id_display}[{}..{}]",
                            e.start_block_height(),
                            e.end_block_height()
                        )
                    })
                    .unwrap_or_else(|_| "err".to_string())
            })
            .collect::<Vec<String>>()
            .join(" → ");
        println!(
            "Loaded {} epochs from source sortition DB: {epochs_str}",
            epochs.len()
        );
        epochs
    };

    let context_opts = BenchContextOpts::new(args.source_dir().into(), network, chain_id, &epochs)?
        .with_start_block(args.start_at().cloned())
        .with_end_block(args.end_at().cloned())
        .with_block_count(args.block_count())
        .with_tip(args.tip().cloned());

    let bench_context = BenchContext::initialize(app_db.clone(), context_opts).await?;

    Ok((bench_context, network, chain_id, epochs))
}
