use std::fmt::{LowerHex, UpperHex};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use stacks_bench::context::{BenchContext, BenchContextOpts};
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::{AppDb, models};
use stacks_bench::db::node::ChainStateDb;
use stacks_bench::db::node::chainstate::models::DbConfig;
use stacks_bench::db::node::sortition::SortitionDb;
use stacks_bench::{BurnChainPath, ChainStatePath, Network, StacksBlockRef, StacksEpoch};
use stacks_profiler::Profiler;

#[derive(Parser, Serialize, Deserialize)]
#[command(name = "stacks-bench", about)]
pub struct Args {
    /// Stacks node data dir (the directory containing the `chainstate` folder).
    #[arg(long = "source", short = 's')]
    source_dir: PathBuf,

    /// The Stacks block (height or hex block id) to start at, inclusive. Cannot
    /// be used with the `txid` flag.
    #[arg(long, conflicts_with = "txid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    start_at: Option<StacksBlockRef>,

    /// The Stacks block (height or hex block id) to end at, inclusive. Cannot
    /// be used with the `txid` or `count` flags.
    #[arg(long, conflicts_with_all = &["txid", "count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    end_at: Option<StacksBlockRef>,

    /// The network to use (`mainnet`, `testnet`, `regtest`). If not specified,
    /// the network is inferred from the chainstate database.
    #[arg(long, short = 'n')]
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<Network>,

    /// The number of blocks to process, starting from `start-at`.
    #[arg(long, short = 'c', conflicts_with_all = &["end_at", "txid"], requires = "start_at")]
    #[serde(skip_serializing_if = "Option::is_none")]
    block_count: Option<u32>,

    /// A specific transaction id (hex) to benchmark. May not be used with `start-at`, `end-at`, or `count`.
    #[arg(long, conflicts_with_all = &["start_at", "end_at", "count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<TxIdArg>,

    /// Number of blocks to use for calibration of the commit cost model.
    #[arg(long, value_name = "CALIBRATION_BLOCKS", default_value_t = 10)]
    calibration: usize,

    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    #[arg(long = "db", value_name = "DB_PATH")]
    #[serde(skip_serializing_if = "Option::is_none")]
    db_path: Option<PathBuf>,
}

impl Args {
    pub fn try_determine_network(&self, chainstate_db_config: &DbConfig) -> Result<Network> {
        if let Some(network) = self.network {
            chainstate_db_config.assert_matches_network(network)?;
            Ok(network)
        } else if chainstate_db_config.is_mainnet() {
            Ok(Network::Mainnet)
        } else {
            Ok(Network::Testnet)
        }
    }

    pub fn app_db_path(&self) -> PathBuf {
        // Use the explicit path if provided
        if let Some(path) = &self.db_path {
            // If the user pointed to an existing directory, put the default file inside it
            if path.is_dir() {
                return path.join(AppDb::DEFAULT_DB_FILENAME);
            }

            // If the path has no extension, assume they meant a file prefix and add .sqlite
            // e.g. "--db my-run" -> "my-run.sqlite"
            if path.extension().is_none() {
                let mut p = path.clone();
                p.set_extension("sqlite");
                return p;
            }

            // Otherwise use exactly what they gave (allowing .db, .sqlite3, etc.)
            return path.clone();
        }

        // Otherwise, resolve default: <exe_dir>/stacks-bench.sqlite
        // Fallback to current directory (".") if executable path cannot be determined.
        let base_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        base_dir.join(AppDb::DEFAULT_DB_FILENAME)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize Args to JSON")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

// Helper to get the current git commit hash
fn get_git_hash() -> Option<Vec<u8>> {
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

fn main() -> Result<()> {
    let args = Args::parse();

    let app_db_path = args.app_db_path();
    let mut app_db = AppDb::open(&app_db_path)
        .with_context(|| format!("Failed to open or create app database at {:?}", app_db_path))?;

    let chainstate_path = ChainStatePath::from_node_root(&args.source_dir);
    let burnchain_path = BurnChainPath::from_node_root(&args.source_dir);

    let mut chainstate_db = ChainStateDb::open_for_read(chainstate_path.index_db_path())?;
    let db_config = chainstate_db.read_db_config()?;

    let network = args.try_determine_network(&db_config)?;
    let chain_id = db_config.chain_id();

    let mut sortition_db = SortitionDb::open_for_read(burnchain_path.sortition_db_path())?;
    let epochs = sortition_db.get_epochs()?;
    println!("Loaded {} epochs from source sortition DB.", epochs.len());
    for epoch in &epochs {
        println!(
            "  Epoch {} ({}): start_block={}, end_block={}",
            epoch.to_stacks_epoch_id()?,
            epoch.epoch_id(),
            epoch.start_block_height(),
            epoch.end_block_height()
        );
    }

    let context_opts = BenchContextOpts::new(args.source_dir.clone(), network, chain_id, &epochs)?
        .with_maybe_start_block(args.start_at.clone())
        .with_maybe_end_block(args.end_at.clone());

    let mut bench_context = BenchContext::initialize(context_opts)?;

    let selected_blocks = bench_context
        .select_blocks()
        .with_context(|| "Block selection failure")?;
    let selected_block_count = selected_blocks.len();

    let (tip_id, tip_height) = bench_context.chain_tip();

    let (chainstate_model, _epochs_model) = app_db.get_or_create_chainstate(
        network,
        db_config.chain_id(),
        &tip_id,
        tip_height,
        &epochs,
    )?;

    let args_json = args.to_json()?;
    let git_commit_hash = get_git_hash().unwrap_or(vec![0u8; 20]); // Placeholder if git not available
    let run_model = app_db.create_benchmark_run(models::NewBenchmarkRun {
        run_name: None,
        chainstate_id: chainstate_model.id,
        git_commit_hash,
        start_time: Utc::now().naive_utc(),
        end_time: None,
        args_json,
    })?;

    println!("Re-executing {selected_block_count} selected blocks...");

    let mut metrics_buffer = Vec::new();
    let mut cost_model = stacks_bench::replay::CostModel::default();
    let calibration_count = args.calibration;
    let mut calibrated = false;

    // Accumulators for summary
    let mut total_blocks = 0u64;
    let mut total_txs = 0u64;
    let mut total_duration = Duration::ZERO;
    let mut total_setup = Duration::ZERO;
    let mut total_exec = Duration::ZERO;
    let mut total_commit = Duration::ZERO;
    let mut total_runtime = 0u64;
    let mut total_write_len = 0u64;
    let mut total_read_len = 0u64;

    let block_replay_span = Profiler::begin_span("Block Replay");

    let start = Instant::now();
    for (i, block) in selected_blocks.iter().enumerate() {
        println!(
            "Re-executing block at height {} in epoch {} ({})",
            block.height, block.epoch, block.id
        );

        let mut metrics = stacks_bench::replay::re_execute_block(&mut bench_context, block)?;

        if !calibrated {
            metrics_buffer.push(metrics);

            if metrics_buffer.len() >= calibration_count || i == selected_block_count - 1 {
                // Perform calibration
                cost_model = stacks_bench::replay::compute_cost_model(&metrics_buffer);
                calibrated = true;

                println!(
                    "\n--- Calibration Complete ({} blocks) ---",
                    metrics_buffer.len()
                );
                println!("  Static Overhead: {:.2?}", cost_model.static_overhead);
                println!(
                    "  Cost per Byte:   {:.2} µs",
                    cost_model.time_per_byte * 1_000_000.0
                );

                if cost_model.time_per_byte <= f64::EPSILON {
                    println!(
                        "  [WARN] Correlation weak or negative. Falling back to default heuristic (20% static / 80% variable)."
                    );
                }
                println!("----------------------------------------\n");

                let buffer_len = metrics_buffer.len();

                // Flush buffer
                for (j, m) in metrics_buffer.iter_mut().enumerate() {
                    if cost_model.time_per_byte > f64::EPSILON {
                        m.apply_cost_model(&cost_model);
                    } else {
                        m.apply_heuristic();
                    }

                    // Accumulate stats
                    total_blocks += 1;
                    total_txs += m.transactions.len() as u64;
                    total_duration += m.total_duration;
                    total_setup += m.setup_duration;
                    total_exec += m.execution_duration;
                    total_commit += m.commit_duration;
                    total_runtime += m.total_clarity_cost.runtime;
                    total_write_len += m.total_clarity_cost.write_length;
                    total_read_len += m.total_clarity_cost.read_length;

                    // Calculate static % for this block
                    let static_pct = if m.commit_duration.as_secs_f64() > 0.0 {
                        (m.commit_overhead_baseline.as_secs_f64() / m.commit_duration.as_secs_f64())
                            * 100.0
                    } else {
                        0.0
                    };
                    println!(
                        "  [Buffered Block {}] Metrics: {:?} (Static Commit: {:.1}%)",
                        i - buffer_len + j + 1,
                        m,
                        static_pct
                    );

                    let buffered_block = &selected_blocks[i - buffer_len + j + 1];
                    save_metrics_to_db(&mut app_db, run_model.id, buffered_block, m)?;
                }
                metrics_buffer.clear();
            }
        } else {
            if cost_model.time_per_byte > f64::EPSILON {
                metrics.apply_cost_model(&cost_model);
            } else {
                metrics.apply_heuristic();
            }

            // Accumulate stats
            total_blocks += 1;
            total_txs += metrics.transactions.len() as u64;
            total_duration += metrics.total_duration;
            total_setup += metrics.setup_duration;
            total_exec += metrics.execution_duration;
            total_commit += metrics.commit_duration;
            total_runtime += metrics.total_clarity_cost.runtime;
            total_write_len += metrics.total_clarity_cost.write_length;
            total_read_len += metrics.total_clarity_cost.read_length;

            let static_pct = if metrics.commit_duration.as_secs_f64() > 0.0 {
                (metrics.commit_overhead_baseline.as_secs_f64()
                    / metrics.commit_duration.as_secs_f64())
                    * 100.0
            } else {
                0.0
            };
            println!(
                "  Execution Metrics: {:?} (Static Commit: {:.1}%)",
                metrics, static_pct
            );
            save_metrics_to_db(&mut app_db, run_model.id, block, &metrics)?;
        }
    }
    let duration = start.elapsed();
    drop(block_replay_span);

    app_db.finish_benchmark_run(run_model.id, Utc::now().naive_utc())?;

    println!("Re-executed {selected_block_count} blocks in {duration:.2?}");

    if total_blocks > 0 {
        println!("\n========================================");
        println!("           BENCHMARK SUMMARY            ");
        println!("========================================");
        println!("Total Blocks:       {}", total_blocks);
        println!("Total Transactions: {}", total_txs);
        println!("Total Duration:     {:.2?}", total_duration);
        println!("  - Setup:          {:.2?}", total_setup);
        println!("  - Execution:      {:.2?}", total_exec);
        println!("  - Commit:         {:.2?}", total_commit);
        println!("Total Clarity Runtime:  {}", total_runtime);
        println!("Total Write Length:     {} bytes", total_write_len);
        println!("Total Read Length:      {} bytes", total_read_len);

        let avg_duration = total_duration / total_blocks as u32;
        let avg_setup = total_setup / total_blocks as u32;
        let avg_exec = total_exec / total_blocks as u32;
        let avg_commit = total_commit / total_blocks as u32;
        let avg_txs = total_txs as f64 / total_blocks as f64;

        println!("\nAverages per Block:");
        println!("  Duration:         {:.2?}", avg_duration);
        println!("  Setup:            {:.2?}", avg_setup);
        println!("  Execution:        {:.2?}", avg_exec);
        println!("  Commit:           {:.2?}", avg_commit);
        println!("  Transactions:     {:.1}", avg_txs);
        println!("  Clarity Runtime:  {}", total_runtime / total_blocks);
        println!(
            "  Write Length:     {} bytes",
            total_write_len / total_blocks
        );
        println!(
            "  Read Length:      {} bytes",
            total_read_len / total_blocks
        );
        println!("========================================\n");
    }

    // Give the OS a moment to sync metadata
    std::thread::sleep(Duration::from_millis(100));

    let (growth, written) = bench_context.calculate_storage_delta()?;

    println!("Storage Delta:");
    println!(
        "  Net Change:        {:.4} MB ({growth} bytes)",
        growth as f64 / 1_024.0 / 1_024.0
    );
    println!(
        "  Est. Data Written: {:.4} MB ({written} bytes)",
        written as f64 / 1_024.0 / 1_024.0
    );

    println!();
    println!("Profiler Results:");
    let profile_results = Profiler::take_results();
    let root_results = profile_results.first().unwrap();
    root_results.print_tree();

    Ok(())
}

// Helper to save metrics to DB
fn save_metrics_to_db(
    app_db: &mut AppDb,
    run_id: i32,
    block: &stacks_bench::BlockSummary,
    metrics: &stacks_bench::replay::BlockMetrics,
) -> Result<()> {
    // 1. Get/Create Block
    let block_model = app_db.get_or_create_stacks_block(&block)?;

    // 2. Create Block Stats
    let block_stats = models::NewStacksBlockStats {
        benchmark_run_id: run_id,
        stacks_block_id: block_model.id,
        total_duration_us: metrics.total_duration.as_micros() as i32,
        setup_duration_us: metrics.setup_duration.as_micros() as i32,
        execution_duration_us: metrics.execution_duration.as_micros() as i32,
        commit_duration_us: metrics.commit_duration.as_micros() as i32,
        commit_overhead_baseline_us: metrics.commit_overhead_baseline.as_micros() as i32,
        clarity_write_length: metrics.total_clarity_cost.write_length as i32,
        clarity_write_count: metrics.total_clarity_cost.write_count as i32,
        clarity_read_length: metrics.total_clarity_cost.read_length as i32,
        clarity_read_count: metrics.total_clarity_cost.read_count as i32,
        clarity_runtime: metrics.total_clarity_cost.runtime as i32,
    };
    app_db.insert_stacks_block_stats(&[block_stats])?;

    // 3. Create Tx Stats
    let mut tx_stats_batch = Vec::with_capacity(metrics.transactions.len());
    for tx_metric in &metrics.transactions {
        let tx_hash_bytes =
            hex::decode(&tx_metric.txid).map_err(|e| anyhow!("Invalid hex in txid: {}", e))?;

        // We don't have tx type in metrics yet, using "unknown"
        let tx_model = app_db.get_or_create_stacks_tx(block_model.id, &tx_hash_bytes, "unknown")?;

        tx_stats_batch.push(models::NewStacksTxStats {
            benchmark_run_id: run_id,
            stacks_tx_id: tx_model.id,
            duration_us: tx_metric.duration.as_micros() as i32,
            estimated_commit_impact_us: tx_metric.estimated_commit_impact.as_micros() as i32,
            clarity_write_length: tx_metric.cost.write_length as i32,
            clarity_write_count: tx_metric.cost.write_count as i32,
            clarity_read_length: tx_metric.cost.read_length as i32,
            clarity_read_count: tx_metric.cost.read_count as i32,
            clarity_runtime: tx_metric.cost.runtime as i32,
        });
    }
    app_db.insert_stacks_tx_stats(&tx_stats_batch)?;

    Ok(())
}
