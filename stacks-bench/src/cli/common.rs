use std::fmt::{LowerHex, UpperHex};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use stacks_bench::context::{BenchEnv, BenchEnvOpts};
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::AppDb;
use stacks_bench::db::app::models::BenchmarkRun;
use stacks_bench::db::node::ChainStateDb;
use stacks_bench::db::node::sortition::SortitionDb;
use stacks_bench::indexer::ChainIndexPlan;
use stacks_bench::paths::{AppDataDir, BurnChainDir, ChainStateDir};
use stacks_bench::shadow::{ShadowDir, ShadowDirBuilder};
use stacks_bench::{Network, StacksBlockRef, StacksEpoch};

pub struct CliContext {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    app_data_dir: AppDataDir,
    /// The application database.
    app_db: AppDb,
}

pub const SUCCESS_ICON: &str = "✔";
#[allow(unused)]
pub const FAILURE_ICON: &str = "✘";

macro_rules! fmt_success {
    ($($arg:tt)*) => {{
        format!(
            "{} {}",
            ::console::style($crate::cli::common::SUCCESS_ICON).green(),
            format_args!($($arg)*)
        )
    }};
}

#[allow(unused)]
macro_rules! fmt_failure {
    ($($arg:tt)*) => {{
        format!(
            "{} {}",
            ::console::style($crate::cli::common::FAILURE_ICON).red(),
            format_args!($($arg)*)
        )
    }};
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
            bail!(
                "invalid txid length: expected 32 bytes, got {} bytes",
                bytes.len()
            );
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

impl TxIdArg {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Format a benchmark run for display in interactive select / multiselect lists.
///
/// Returns something like: `✔ my-run  —  2026-02-11 14:30:00`
pub fn format_run_label(run: &BenchmarkRun) -> String {
    let status = if run.end_time.is_some() {
        console::style("✔").green().to_string()
    } else {
        console::style("…").yellow().to_string()
    };
    let name = run.run_name.as_deref().unwrap_or("(unnamed)");
    format!(
        "{status} {name}  —  {}",
        run.start_time.format("%Y-%m-%d %H:%M:%S"),
    )
}

/// Format a run's name as a bold parenthetical suffix for log messages.
///
/// Returns `" (name)"` (with bold styling) when a name exists, or `""` otherwise.
pub fn format_run_name_suffix(run: &BenchmarkRun) -> String {
    run.run_name
        .as_deref()
        .map(|n| format!(" ({})", console::style(n).bold()))
        .unwrap_or_default()
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

pub fn create_shadow_dir<P: AsRef<Path>>(
    source_dir: P,
    with_pre_nakamoto_blocks: bool,
) -> Result<ShadowDir> {
    let mut builder = ShadowDirBuilder::new(source_dir.as_ref())
        .glob("burnchain/**")
        .glob("chainstate/vm/**");

    if with_pre_nakamoto_blocks {
        builder = builder.glob("chainstate/blocks/**");
    } else {
        builder = builder
            .glob("chainstate/blocks/nakamoto.sqlite")
            .glob("chainstate/blocks/nakamoto.sqlite-wal");
    }

    builder = builder
        .watch("chainstate/vm/clarity/marf.sqlite")
        .watch("chainstate/vm/clarity/marf.sqlite.blobs")
        .watch("chainstate/vm/clarity/marf.sqlite-wal")
        .watch("chainstate/vm/index.sqlite")
        .watch("chainstate/vm/index.sqlite.blobs")
        .watch("chainstate/vm/index.sqlite-wal");

    let shadow_dir = builder.copy()?;
    Ok(shadow_dir)
}

/// Initializes a [`BenchEnv`] from a working directory with network, chain_id,
/// and epoch resolution. Returns the env and the resolved anchor tip.
///
/// When `tip_override` is `Some`, the tip is resolved from the override;
/// otherwise the canonical node tip is used.
pub async fn setup_bench_env<P: AsRef<Path>>(
    working_dir: P,
    network_override: Option<Network>,
    tip_override: Option<&StacksBlockRef>,
) -> Result<(BenchEnv, stacks_bench::blocks::BlockRef)> {
    let chainstate_path = ChainStateDir::from_node_root(&working_dir);
    let burnchain_path = BurnChainDir::from_node_root(&working_dir);

    // Resolve network and chain ID
    let (network, chain_id) = {
        let chainstate_db = ChainStateDb::open_for_read(chainstate_path.index_db_path()).await?;
        let db_config = chainstate_db.read_db_config().await?;

        let network = if let Some(n) = network_override {
            db_config.assert_matches_network(n)?;
            n
        } else if db_config.is_mainnet() {
            Network::Mainnet
        } else {
            Network::Testnet
        };
        (network, db_config.chain_id())
    };

    // Load epochs (node sortition epochs -> StacksEpoch)
    let epochs: Vec<StacksEpoch> = {
        SortitionDb::open_for_read(burnchain_path.sortition_db_path())
            .await?
            .get_epochs()
            .await?
            .iter()
            .map(StacksEpoch::try_from)
            .collect::<Result<Vec<_>>>()?
    };

    let context_opts =
        BenchEnvOpts::new(network, chain_id, epochs)?.with_tip(tip_override.cloned());

    let (env, node_tip) = BenchEnv::initialize(working_dir, context_opts).await?;

    let chainstate_db = ChainStateDb::open_for_read(chainstate_path.index_db_path()).await?;

    // Anchor tip: best-practice for "no pre-walk" is to require an explicit tip *id*
    // if the user overrides it. (Tip-by-height would require a chain-walk to get the id.)
    let anchor_tip = match tip_override {
        None => node_tip,
        Some(StacksBlockRef::Id(id)) => {
            let tip_height =
                resolve_ref_height(&chainstate_db, &StacksBlockRef::Id(id.clone()), "tip").await?;
            stacks_bench::blocks::BlockRef {
                id: id.clone(),
                height: tip_height,
            }
        }
        Some(StacksBlockRef::Height(h)) => {
            anyhow::bail!(
                "--tip by height ({h}) requires a chain-walk to resolve the canonical tip id; \
                for indexing-first mode, pass --tip as a block id instead"
            );
        }
    };

    Ok((env, anchor_tip))
}

pub async fn setup_bench_env_and_plan<'a, A: IndexerArgs, P: AsRef<Path> + 'a>(
    working_dir: P,
    args: &'_ A,
) -> Result<(BenchEnv, ChainIndexPlan)> {
    let (env, anchor_tip) = setup_bench_env(&working_dir, args.network(), args.tip()).await?;

    let chainstate_path = ChainStateDir::from_node_root(&working_dir);
    let chainstate_db = ChainStateDb::open_for_read(chainstate_path.index_db_path()).await?;

    // start_height
    let start_height = match args.start_at() {
        Some(r) => resolve_ref_height(&chainstate_db, r, "start").await?,
        None => 1,
    };

    if start_height == 0 {
        anyhow::bail!("start height cannot be 0 (genesis). Use height >= 1.");
    }
    if start_height > anchor_tip.height {
        anyhow::bail!(
            "start height {start_height} is beyond anchor tip height {}",
            anchor_tip.height
        );
    }

    // end_height (from count, end_at, or default to anchor tip)
    let end_height = if let Some(count) = args.block_count() {
        if count == 0 {
            anyhow::bail!("block count must be > 0");
        }
        let count_u64 = count as u64;
        start_height
            .checked_add(count_u64 - 1)
            .ok_or_else(|| anyhow!("end height overflow computing start+count-1"))?
    } else if let Some(r) = args.end_at() {
        resolve_ref_height(&chainstate_db, r, "end").await?
    } else {
        anchor_tip.height
    };

    if end_height < start_height {
        anyhow::bail!("end height {end_height} is before start height {start_height}");
    }
    if end_height > anchor_tip.height {
        anyhow::bail!(
            "end height {end_height} is beyond anchor tip height {}",
            anchor_tip.height
        );
    }

    let plan = ChainIndexPlan {
        anchor_tip,
        start_height,
        end_height,
    };

    Ok((env, plan))
}

async fn resolve_ref_height(
    chainstate_db: &ChainStateDb<stacks_bench::db::ReadOnly>,
    r: &StacksBlockRef,
    label: &'static str,
) -> Result<u64> {
    match r {
        StacksBlockRef::Height(h) => {
            if *h == 0 {
                anyhow::bail!("{label} height cannot be 0 (genesis). Use height >= 1.");
            }
            Ok(*h)
        }
        StacksBlockRef::Id(id) => {
            let hdr = chainstate_db
                .get_block_header(id)
                .await?
                .with_context(|| format!("{label} block id {id} not found in chainstate DB"))?;

            if hdr.block_height <= 0 {
                anyhow::bail!(
                    "{label} block {id} has invalid height {} in DB",
                    hdr.block_height
                );
            }

            Ok(hdr.block_height as u64)
        }
    }
}
