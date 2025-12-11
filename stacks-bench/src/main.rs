use std::collections::HashMap;
use std::fmt::{LowerHex, UpperHex};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
    StartContainerOptionsBuilder, StopContainerOptions, WaitContainerOptions,
};
use bollard::secret::{ContainerCreateBody, HostConfig, PortBinding};
use chrono::Utc;
use clap::{Parser, Subcommand};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use stacks_bench::context::{BenchContext, BenchContextOpts};
use stacks_bench::db::DbOpenForRead as _;
use stacks_bench::db::app::AppDb;
use stacks_bench::db::node::ChainStateDb;
use stacks_bench::db::node::chainstate::models::DbConfig;
use stacks_bench::db::node::sortition::SortitionDb;
use stacks_bench::indexer::ChainstateIndexer;
use stacks_bench::metrics::{CostModel, MetricsAccumulator, ModelSource};
use stacks_bench::paths::{AppDataDir, BurnChainDir, ChainStateDir};
use stacks_bench::replay::ReplayMode;
use stacks_bench::{Network, StacksBlockRef};
use stacks_profiler::Profiler;

const METABASE_IMAGE_TAG: &str = "v0.57.4.3";

#[derive(Parser, Debug)]
#[command(name = "stacks-bench", about)]
pub struct Cli {
    /// The path to the application database (SQLite). If not specified, the database
    /// will be created in the same directory as the `stacks-bench` binary.
    #[arg(long = "db", value_name = "APP_DATA_DIR")]
    app_data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the benchmark
    Bench(BenchArgs),
    /// Manage chainstate data
    Chainstate(ChainstateArgs),
    /// Launch a pre-configured Metabase instance to analyze results
    Metabase {
        /// Port to expose Metabase on (default: 3000)
        #[arg(long, default_value = "3000")]
        port: u16,

        /// Metabase Docker image tag to use (defaults to [`METABASE_IMAGE_TAG`]).
        #[arg(long, default_value = METABASE_IMAGE_TAG)]
        image_tag: String,
    },
}

#[derive(clap::Args, Debug)]
pub struct ChainstateArgs {
    #[command(subcommand)]
    command: ChainstateCommands,
}

#[derive(Subcommand, Debug)]
pub enum ChainstateCommands {
    /// Index a range of blocks from the node database
    Index(IndexArgs),
}

pub trait IndexerArgs {
    fn source_dir(&self) -> &PathBuf;
    fn start_at(&self) -> Option<&StacksBlockRef>;
    fn end_at(&self) -> Option<&StacksBlockRef>;
    fn block_count(&self) -> Option<u32>;
    fn tip(&self) -> Option<&StacksBlockRef>;
    fn network(&self) -> Option<Network>;
}

#[derive(clap::Args, Debug, Serialize, Deserialize)]
pub struct IndexArgs {
    /// Stacks node data dir (the directory containing the `chainstate` folder).
    #[arg(long = "source", short = 's')]
    source_dir: PathBuf,

    /// The Stacks block (height or hex block id) to start at, inclusive.
    #[arg(long, default_value = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    start_at: Option<StacksBlockRef>,

    /// The Stacks block (height or hex block id) to end at, inclusive. Cannot
    /// be used with the `count` flag.
    #[arg(long, conflicts_with_all = &["block_count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    end_at: Option<StacksBlockRef>,

    /// The number of blocks to process, starting from `start-at`.
    #[arg(long = "count", short = 'c', conflicts_with_all = &["end_at"], requires = "start_at")]
    #[serde(skip_serializing_if = "Option::is_none")]
    block_count: Option<u32>,

    /// The tip block (height or hex block id) to use as the anchor for resolving canonical history.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<StacksBlockRef>,

    /// The network to use (`mainnet`, `testnet`, `regtest`).
    #[arg(long, short = 'n', alias = "net")]
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<Network>,
}

impl IndexerArgs for IndexArgs {
    fn source_dir(&self) -> &PathBuf {
        &self.source_dir
    }
    fn start_at(&self) -> Option<&StacksBlockRef> {
        self.start_at.as_ref()
    }
    fn end_at(&self) -> Option<&StacksBlockRef> {
        self.end_at.as_ref()
    }
    fn block_count(&self) -> Option<u32> {
        self.block_count
    }
    fn tip(&self) -> Option<&StacksBlockRef> {
        self.tip.as_ref()
    }
    fn network(&self) -> Option<Network> {
        self.network
    }
}

// TODO: Add a `--contract` arg to filter by qualified contract id
#[derive(clap::Args, Debug, Serialize, Deserialize)]
pub struct BenchArgs {
    /// Stacks node data dir (the directory containing the `chainstate` folder).
    #[arg(long = "source", short = 's')]
    source_dir: PathBuf,

    /// The Stacks block (height or hex block id) to start at, inclusive. Cannot
    /// be used with the `txid` flag.
    #[arg(long, conflicts_with = "txid", default_value = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    start_at: Option<StacksBlockRef>,

    /// The Stacks block (height or hex block id) to end at, inclusive. Cannot
    /// be used with the `txid` or `count` flags.
    #[arg(long, conflicts_with_all = &["txid", "block_count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    end_at: Option<StacksBlockRef>,

    /// The tip block (height or hex block id) to use as the anchor for
    /// resolving canonical history. Defaults to the node's current canonical
    /// tip. Useful for benchmarking in forks: provide the fork's tip hash here.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<StacksBlockRef>,

    /// The network to use (`mainnet`, `testnet`, `regtest`). If not specified,
    /// the network is inferred from the chainstate database.
    #[arg(long, short = 'n')]
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<Network>,

    /// The number of blocks to process, starting from `start-at`.
    #[arg(long = "count", short = 'c', conflicts_with_all = &["end_at", "txid"], requires = "start_at")]
    #[serde(skip_serializing_if = "Option::is_none")]
    block_count: Option<u32>,

    /// A specific transaction id (hex) to benchmark. May not be used with
    /// `start-at`, `end-at`, or `count`.
    #[arg(long, conflicts_with_all = &["start_at", "end_at", "count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<TxIdArg>,

    /// Number of blocks to use for calibration of the commit cost model.
    #[arg(long, value_name = "CALIBRATION_BLOCKS", default_value_t = 20)]
    calibration: usize,

    /// Number of blocks to process as warmup before starting measurements.
    /// These blocks will be executed but not included in the benchmark results.
    #[arg(long, default_value_t = 0)]
    warmup: usize,
}

impl IndexerArgs for BenchArgs {
    fn source_dir(&self) -> &PathBuf {
        &self.source_dir
    }
    fn start_at(&self) -> Option<&StacksBlockRef> {
        self.start_at.as_ref()
    }
    fn end_at(&self) -> Option<&StacksBlockRef> {
        self.end_at.as_ref()
    }
    fn block_count(&self) -> Option<u32> {
        self.block_count
    }
    fn tip(&self) -> Option<&StacksBlockRef> {
        self.tip.as_ref()
    }
    fn network(&self) -> Option<Network> {
        self.network
    }
}

impl BenchArgs {
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

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize BenchArgs to JSON")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TxIdArg([u8; 32]);

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

#[tokio::main]
async fn main() -> Result<()> {
    // SAFETY: This is the first thing we do in the process, before any
    // potential threads are spawned or any FFI into C libraries that might read
    // the environment.
    unsafe {
        std::env::set_var("STACKS_LOG_CRITONLY", "1");
    }

    let cli = Cli::parse();

    // Use AppDataPath to resolve locations
    let app_data = AppDataDir::resolve_from_opt(cli.app_data_dir.as_ref())?;

    match cli.command {
        Commands::Metabase { port, image_tag } => run_metabase(&app_data, port, image_tag).await,
        Commands::Bench(args) => run_bench(&app_data, args).await,
        Commands::Chainstate(args) => match args.command {
            ChainstateCommands::Index(index_args) => {
                run_chainstate_index(&app_data, index_args).await
            }
        },
    }
}

async fn setup_bench_context<T: IndexerArgs>(
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

    println!("Using network: {}", network.to_string().to_uppercase());

    let chain_id = db_config.chain_id();

    let mut sortition_db = SortitionDb::open_for_read(burnchain_path.sortition_db_path()).await?;
    let epochs = sortition_db.get_epochs().await?;
    let epochs_str = epochs
        .iter()
        .map(|e| {
            e.to_stacks_epoch_id()
                .map(|id| {
                    format!(
                        "[Epoch {id} ({}..{})]",
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

    let context_opts = BenchContextOpts::new(args.source_dir().into(), network, chain_id, &epochs)?
        .with_start_block(args.start_at().cloned())
        .with_end_block(args.end_at().cloned())
        .with_block_count(args.block_count())
        .with_maybe_tip(args.tip().cloned());

    let bench_context = BenchContext::initialize(app_db.clone(), context_opts).await?;

    Ok((bench_context, network, chain_id, epochs))
}

async fn run_chainstate_index(app_data: &AppDataDir, args: IndexArgs) -> Result<()> {
    let app_db_path = app_data.app_db_path();
    let mut app_db = AppDb::open(&app_db_path).await?;

    let (mut bench_context, network, chain_id, epochs) =
        setup_bench_context(&mut app_db, &args).await?;

    let mut indexer = ChainstateIndexer::new(&mut app_db, &mut bench_context);
    indexer.index_chainstate(network, chain_id, &epochs).await?;

    println!("Indexing complete");

    println!("Cleaning up (this may take a few moments for large chainstates)...");
    // Dropping the context will clean up the shadow dir
    drop(bench_context);

    println!("Done!");
    Ok(())
}

async fn run_bench(app_data: &AppDataDir, args: BenchArgs) -> Result<()> {
    let app_db_path = app_data.app_db_path();
    let mut app_db = AppDb::open(&app_db_path).await?;

    let (mut bench_context, network, chain_id, epochs) =
        setup_bench_context(&mut app_db, &args).await?;

    let (chainstate_model, _) = app_db
        .get_or_create_chainstate(
            network,
            chain_id,
            &bench_context.chain_tip().0,
            bench_context.chain_tip().1,
            &epochs,
        )
        .await?;

    let block_ids = ChainstateIndexer::new(&mut app_db, &mut bench_context)
        .index_chainstate(network, chain_id, &epochs)
        .await?;

    let selected_block_count = block_ids.len();

    let run_name = format!("{}", Utc::now().format("%Y%m%d-%H%M%S"));

    let args_json = args.to_json()?;
    let git_commit_hash = get_git_hash().unwrap_or(vec![0u8; 20]); // Placeholder if git not available
    let run_model = app_db
        .create_benchmark_run(
            chainstate_model.id,
            Utc::now().naive_utc(),
            git_commit_hash,
            Some(run_name),
            args_json,
        )
        .await?;

    // Open the heavy databases before the loop
    let (mut chainstate, burnchain) = bench_context.open_stacks_chainstate()?;

    println!("Re-executing {selected_block_count} selected blocks...");

    let mut metrics_buffer = Vec::new();
    let mut cost_model = CostModel::default();

    const PROFILER_BATCH_SIZE: usize = 1_000;
    let mut profiler_buffer = Vec::with_capacity(PROFILER_BATCH_SIZE);

    // Dynamic calibration window
    let min_calibration_count = args.calibration;
    // Allow buffer to grow if we can't find variance. Empty blocks are small, so 500 is memory-safe.
    let max_calibration_buffer = 500;
    let mut calibrated = false;
    let mut is_warmup = false;
    let mut total_clarity_db_checkpoint_duration = Duration::ZERO;

    let mut accumulator = MetricsAccumulator::default();

    let start = Instant::now();
    for (i, block_id) in block_ids.iter().enumerate() {
        is_warmup = i < args.warmup;
        Profiler::clear();

        // Load metadata from App DB
        let block = app_db.get_block(block_id).await?;

        if is_warmup {
            if i == 0 {
                println!("Warming up for {} blocks...", args.warmup);
            }
            if i % 10 == 0 {
                eprint!("(Warmup {}) ", block.height);
            }
        } else if i == args.warmup || i % 2_500 == 0 || i == selected_block_count - 1 {
            eprint!("{}", block.height);
        } else if i % 250 == 0 {
            eprint!(".");
        }

        // Replay the block
        let mut metrics = stacks_profiler::measure!("Block Replay", {
            stacks_bench::replay::replay_block(
                &mut bench_context,
                &mut chainstate,
                &burnchain,
                ReplayMode::Follower,
                &block,
            )?
        });

        let profiler_results = Profiler::take_results();

        let checkpoint_start = Instant::now();
        chainstate.checkpoint_clarity_state()?;
        total_clarity_db_checkpoint_duration += checkpoint_start.elapsed();

        // We clone the transaction metrics because 'metrics' is needed below for calibration/stats.
        // The buffer takes ownership of the data.
        profiler_buffer.push((
            block.id.clone(),
            profiler_results,
            metrics.transactions.clone(),
        ));

        // Flush buffer if full
        if profiler_buffer.len() >= PROFILER_BATCH_SIZE {
            app_db
                .save_profiler_data_batch(run_model.id, &mut profiler_buffer)
                .await?;
        }

        if !calibrated && !is_warmup {
            metrics_buffer.push(metrics);

            let buffer_len = metrics_buffer.len();
            let is_last_block = i == selected_block_count - 1;

            // Only attempt calibration if we met the minimum count
            if buffer_len >= min_calibration_count {
                let candidate_model = CostModel::compute(&metrics_buffer);

                // A model is "good" if we found variance (not SingleBlock) and a positive correlation
                let is_good_model = candidate_model.source != ModelSource::SingleBlock
                    && candidate_model.time_per_byte > f64::EPSILON;

                // We force calibration if:
                // 1. We found a good model
                // 2. We hit the hard memory limit
                // 3. We ran out of blocks
                if is_good_model || buffer_len >= max_calibration_buffer || is_last_block {
                    cost_model = candidate_model;
                    calibrated = true;

                    println!("\n--- Calibration Complete ({} blocks) ---", buffer_len);
                    println!("  Method:          {:?}", cost_model.source);
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

                    // Flush buffer
                    for (j, m) in metrics_buffer.iter_mut().enumerate() {
                        if cost_model.time_per_byte > f64::EPSILON {
                            m.apply_cost_model(&cost_model);
                        } else {
                            m.apply_heuristic();
                        }

                        if !is_warmup {
                            // Accumulate
                            accumulator.add(m);

                            // Use the buffered block ID to save metrics
                            let buffered_id = &block_ids[i - buffer_len + j + 1];
                            app_db
                                .save_block_metrics(run_model.id, buffered_id, m)
                                .await?;
                        }
                    }
                    metrics_buffer.clear();
                }
                // Else: continue collecting blocks to find variance
            }
        } else {
            if !is_warmup {
                if cost_model.time_per_byte > f64::EPSILON {
                    metrics.apply_cost_model(&cost_model);
                } else {
                    metrics.apply_heuristic();
                }

                accumulator.add(&metrics); // Accumulate
                app_db
                    .save_block_metrics(run_model.id, &block.id, &metrics)
                    .await?;
            }
        }
    }

    // Flush any remaining profiler data after the loop
    if !profiler_buffer.is_empty() && !is_warmup {
        app_db
            .save_profiler_data_batch(run_model.id, &mut profiler_buffer)
            .await?;
    }

    // Flush any remaining metrics in the calibration buffer
    if !metrics_buffer.is_empty() && !is_warmup {
        let buffer_len = metrics_buffer.len(); // Capture length before mutable borrow
        println!();
        println!(
            "Flushing remaining {} blocks from calibration buffer...",
            buffer_len
        );

        for (j, m) in metrics_buffer.iter_mut().enumerate() {
            // Apply heuristic since we didn't finish calibration
            m.apply_heuristic();

            accumulator.add(m);

            // Calculate correct block ID index
            // The buffer contains the LAST N blocks processed.
            let start_index = selected_block_count - buffer_len;
            let buffered_id = &block_ids[start_index + j];

            app_db
                .save_block_metrics(run_model.id, buffered_id, m)
                .await?;
        }
    }

    let duration = start.elapsed();

    app_db
        .finish_benchmark_run(run_model.id, Utc::now().naive_utc())
        .await?;

    println!("Re-executed {selected_block_count} blocks in {duration:.2?}");
    println!("  - Clarity DB Checkpointing: {total_clarity_db_checkpoint_duration:.2?}");
    println!(
        "  - Benchmarking Overhead: {:.2?}",
        duration - total_clarity_db_checkpoint_duration
    );

    accumulator.print_summary(); // Print summary

    // Give the OS a moment to sync metadata
    std::thread::sleep(Duration::from_millis(100));

    let storage_delta_report = bench_context.calculate_storage_delta()?;
    let growth = storage_delta_report.net_growth_bytes;
    let written = storage_delta_report.estimated_bytes_written;

    println!("\n========================================");
    println!("          STORAGE DELTA REPORT          ");
    println!("========================================");
    for file_report in &storage_delta_report.file_reports {
        let status = if !file_report.was_modified {
            "CREATED "
        } else {
            "MODIFIED"
        };

        let sign = if file_report.size_delta_bytes > 0 {
            "+"
        } else {
            ""
        };

        println!(
            "  {status}: {:<60} | Delta: {sign}{:.4} MB",
            file_report.path.display(),
            file_report.size_delta_bytes as f64 / 1_024.0 / 1_024.0
        );
    }
    println!();
    println!(
        "  Net Change:        {:.4} MB ({growth} bytes)",
        growth as f64 / 1_024.0 / 1_024.0
    );
    println!(
        "  Est. Data Written: {:.4} MB ({written} bytes)",
        written as f64 / 1_024.0 / 1_024.0
    );
    println!("========================================");

    println!();
    println!("Checkpointing & vacuuming database...");
    app_db.checkpoint(true).await?;
    app_db.vacuum().await?;

    println!("Cleaning up (this may take a few moments for large chainstates)...");
    // Dropping the context will clean up the shadow dir
    drop(bench_context);

    println!("Benchmark run complete");

    Ok(())
}

async fn run_metabase(app_data: &AppDataDir, port: u16, image_tag: String) -> Result<()> {
    let db_path = app_data.app_db_path();

    // db_path is the full path to the database file.
    if !db_path.exists() {
        anyhow::bail!(
            "Database not found at {:?}. Run a benchmark first.",
            db_path
        );
    }

    // We store postgres data in a subdirectory
    let pg_data_dir = app_data.postgres_data_dir();

    if pg_data_dir.exists()
        && pg_data_dir
            .read_dir()
            .map(|mut i| i.next().is_some())
            .unwrap_or(false)
    {
        println!("  - Status:      Loading existing dashboards/users (Postgres)");
    } else {
        println!("  - Status:      Initializing new configuration (Postgres)");
    }

    println!("Starting Metabase on http://localhost:{}", port);
    println!("  - App DB:      {:?}", db_path);
    println!("  - Postgres DB: {:?}", pg_data_dir);

    if pg_data_dir.exists()
        && pg_data_dir
            .read_dir()
            .map(|mut i| i.next().is_some())
            .unwrap_or(false)
    {
        println!("  - Status:      Loading existing dashboards/users (Postgres)");
    } else {
        println!("  - Status:      Initializing new configuration (Postgres)");
    }

    // Setup and run Metabase container
    run_metabase_container(&app_data, port, image_tag).await
}

async fn run_metabase_container(app_data: &AppDataDir, port: u16, image_tag: String) -> Result<()> {
    let pg_data_dir = app_data.postgres_data_dir();
    let app_db_dir = app_data.app_db_dir();

    let docker =
        Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;

    let mb_image = format!("metabase/metabase:{image_tag}");
    let pg_image = "postgres:18.1-alpine3.22";

    let net_name = "stacks-bench-net";
    let pg_container_name = "stacks-bench-postgres";
    let mb_container_name = "stacks-bench-metabase";

    // 1. Check/Pull images
    for image in [&mb_image, pg_image] {
        if docker.inspect_image(image).await.is_err() {
            println!("Image {} not found locally. Pulling...", image);
            let mut pull_stream = docker.create_image(
                Some(
                    CreateImageOptionsBuilder::default()
                        .from_image(image)
                        .build(),
                ),
                None,
                None,
            );

            while let Some(msg) = pull_stream.next().await {
                match msg {
                    Ok(_) => {}
                    Err(e) => {
                        bail!(
                            "Docker pull failed: {e}. \nHint: This is usually a Docker Desktop issue. Try restarting Docker or running 'docker system prune'."
                        );
                    }
                }
            }
        } else {
            println!("Image {} found locally. Skipping pull.", image);
        }
    }

    // 2. Cleanup existing resources (containers & network)
    // We ignore errors here (e.g. if they don't exist)
    let _ = docker
        .remove_container(
            mb_container_name,
            Some(RemoveContainerOptionsBuilder::new().force(true).build()),
        )
        .await;
    let _ = docker
        .remove_container(
            pg_container_name,
            Some(RemoveContainerOptionsBuilder::new().force(true).build()),
        )
        .await;
    let _ = docker.remove_network(net_name).await;

    // 3. Create Network
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: net_name.to_string(),
            ..Default::default()
        })
        .await
        .context("Failed to create docker network")?;

    // 4. Start Postgres
    std::fs::create_dir_all(&pg_data_dir).context("Failed to create postgres data dir")?;

    let pg_host_config = HostConfig {
        binds: Some(vec![format!(
            "{}:/var/lib/postgresql/data",
            pg_data_dir.to_string_lossy()
        )]),
        network_mode: Some(net_name.to_string()),
        auto_remove: Some(false),
        ..Default::default()
    };

    let pg_config = ContainerCreateBody {
        image: Some(pg_image.to_string()),
        env: Some(vec![
            "POSTGRES_DB=metabase".to_string(),
            "POSTGRES_USER=metabase".to_string(),
            "POSTGRES_PASSWORD=metabase".to_string(),
        ]),
        host_config: Some(pg_host_config),
        healthcheck: Some(bollard::secret::HealthConfig {
            test: Some(vec![
                "CMD-SHELL".to_string(),
                "pg_isready -U metabase".to_string(),
            ]),
            start_interval: Some(0),      // Start immediately
            interval: Some(100_000_000),  // 100ms
            timeout: Some(5_000_000_000), // 5s
            retries: Some(100),
            start_period: Some(2_000_000_000), // 2s grace
        }),
        ..Default::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(pg_container_name)
                    .build(),
            ),
            pg_config,
        )
        .await
        .context("Failed to create Postgres container")?;

    docker
        .start_container(
            pg_container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await
        .context("Failed to start Postgres container")?;

    println!("Postgres started. Waiting for readiness...");

    // Wait loop for Postgres Health
    let start_wait = Instant::now();
    loop {
        if start_wait.elapsed() > Duration::from_secs(30) {
            bail!("Timeout waiting for Postgres to become ready.");
        }

        let inspect = docker
            .inspect_container(
                pg_container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;

        if let Some(state) = inspect.state {
            // Fail fast if container died
            if state.running == Some(false) {
                bail!(
                    "Postgres container stopped unexpectedly. Check logs with: docker logs {}",
                    pg_container_name
                );
            }

            if let Some(health) = state.health {
                if health.status == Some(bollard::models::HealthStatusEnum::HEALTHY) {
                    break;
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("Postgres is ready.");

    // 5. Start Metabase
    let mb_host_config = HostConfig {
        binds: Some(vec![format!("{}:/data", app_db_dir.to_string_lossy())]),
        port_bindings: Some(HashMap::from([(
            "3000/tcp".to_string(),
            Some(vec![PortBinding {
                host_port: Some(port.to_string()),
                ..Default::default()
            }]),
        )])),
        network_mode: Some(net_name.to_string()),
        auto_remove: Some(false),
        ..Default::default()
    };

    let mb_config = ContainerCreateBody {
        image: Some(mb_image.to_string()),
        env: Some(vec![
            "MB_DB_TYPE=postgres".to_string(),
            "MB_DB_DBNAME=metabase".to_string(),
            "MB_DB_PORT=5432".to_string(),
            "MB_DB_USER=metabase".to_string(),
            "MB_DB_PASS=metabase".to_string(),
            "MB_DB_HOST=stacks-bench-postgres".to_string(),
        ]),
        host_config: Some(mb_host_config),
        ..Default::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(mb_container_name)
                    .build(),
            ),
            mb_config,
        )
        .await
        .context("Failed to create Metabase container")?;

    docker
        .start_container(
            mb_container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await
        .context("Failed to start Metabase container")?;

    println!("\nMetabase is running (backed by Postgres).");
    println!("Open http://localhost:{} in your browser.", port);
    println!("Press Ctrl-C to stop.");

    // 6. Wait for Ctrl-C or Container Exit
    let wait_stream = docker.wait_container(mb_container_name, None::<WaitContainerOptions>);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nStopping containers...");
            let _ = docker.stop_container(mb_container_name, None::<StopContainerOptions>).await;
            let _ = docker.stop_container(pg_container_name, None::<StopContainerOptions>).await;
            println!("Stopped.");
        }
        _ = async {
            wait_stream.collect::<Vec<_>>().await
        } => {
            println!("\nMetabase container exited unexpectedly.");
            // Ensure postgres is stopped too
            let _ = docker.stop_container(pg_container_name, None::<StopContainerOptions>).await;
        }
    }

    Ok(())
}
