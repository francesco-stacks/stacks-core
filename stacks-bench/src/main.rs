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
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::AppDb;
use stacks_bench::db::node::ChainStateDb;
use stacks_bench::db::node::chainstate::models::DbConfig;
use stacks_bench::db::node::sortition::SortitionDb;
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

// TODO: Add a `--contract` arg to filter by qualified contract id
#[derive(clap::Args, Debug, Serialize, Deserialize)]
pub struct BenchArgs {
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

    /// The tip block (height or hex block id) to use as the anchor for resolving canonical history.
    /// Defaults to the node's current canonical tip.
    /// Useful for benchmarking forks: provide the fork's tip hash here.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<StacksBlockRef>,

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
    #[arg(long, value_name = "CALIBRATION_BLOCKS", default_value_t = 20)]
    calibration: usize,
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
    let cli = Cli::parse();

    // Use AppDataPath to resolve locations
    let app_data = AppDataDir::resolve_from_opt(cli.app_data_dir.as_ref())?;

    match cli.command {
        Commands::Metabase { port, image_tag } => run_metabase(&app_data, port, image_tag),
        Commands::Bench(args) => run_bench(&app_data, args),
    }
}

fn run_bench(app_data: &AppDataDir, args: BenchArgs) -> Result<()> {
    let app_db_path = app_data.app_db_path();
    let mut app_db = AppDb::open(&app_db_path)?;

    let chainstate_path = ChainStateDir::from_node_root(&args.source_dir);
    let burnchain_path = BurnChainDir::from_node_root(&args.source_dir);

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
        .with_maybe_end_block(args.end_at.clone())
        .with_maybe_tip(args.tip.clone());

    let mut bench_context = BenchContext::initialize(context_opts, Some(&mut app_db))?;

    let (tip_id, tip_height) = bench_context.chain_tip();

    // Resolve the actual range we need to benchmark
    let (start_height, end_height) = bench_context.block_height_range()?;
    println!(
        "Targeting block range: {} to {} (Tip: {})",
        start_height, end_height, tip_height
    );

    let (chainstate_model, epochs) = app_db.get_or_create_chainstate(
        network,
        db_config.chain_id(),
        &tip_id,
        tip_height,
        &epochs,
    )?;

    // 1. Get Canonical Block IDs (Lightweight)
    let mut block_ids =
        app_db.get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)?;

    let expected_count = (end_height - start_height + 1) as usize;

    if block_ids.len() != expected_count {
        println!(
            "App DB index incomplete (found {}, expected {}). Indexing from Node DB...",
            block_ids.len(),
            expected_count
        );

        // 2. Stream from Node DB and Index
        let stream = bench_context
            .canonical_block_stream(start_height as u32, end_height as u32)
            .filter_map(|r| match r {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("Warning: failed to load block during indexing: {}", e);
                    None
                }
            });

        app_db.index_blocks_streaming(stream)?;
        println!("Checkpointing database...");
        app_db.checkpoint()?;
        println!("Vacuuming database...");
        app_db.vacuum()?;

        // 3. Reload IDs
        block_ids = app_db.get_chain_block_ids(&tip_id, start_height as u32, end_height as u32)?;
    }

    if block_ids.is_empty() {
        return Err(anyhow!(
            "No blocks found in range {} to {}",
            start_height,
            end_height
        ));
    }

    let selected_block_count = block_ids.len();

    let run_name = format!(
        "{} - Blocks({start_height}..{end_height})",
        Utc::now().format("%Y.%m.%d %H.%M.%S")
    );

    let args_json = args.to_json()?;
    let git_commit_hash = get_git_hash().unwrap_or(vec![0u8; 20]); // Placeholder if git not available
    let run_model = app_db.create_benchmark_run(
        chainstate_model.id,
        Utc::now().naive_utc(),
        git_commit_hash,
        Some(run_name),
        args_json,
    )?;

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

    let mut accumulator = MetricsAccumulator::default();

    let start = Instant::now();
    for (i, block_id) in block_ids.iter().enumerate() {
        Profiler::clear();

        // Load metadata from App DB
        let summary = app_db.get_block(block_id, epochs.as_slice())?;

        // println!(
        //     "Re-executing block at height {} in epoch {} ({})",
        //     summary.height, summary.epoch, summary.id
        // );
        if i == 0 || i % 5_000 == 0 || i == selected_block_count - 1 {
            eprint!("{}", summary.height);
        } else if i % 250 == 0 {
            eprint!(".");
        }

        // Hydrate transactions from Node DB (Heavy operation, done one-by-one)
        let txs = stacks_bench::BlockTransactions::load(
            bench_context.chainstate(),
            summary.epoch,
            &summary,
        )?;
        let block = summary.with_transactions(txs);

        // Replay the block
        let mut metrics = stacks_profiler::measure!("Block Replay", {
            stacks_bench::replay::replay_block(&mut bench_context, ReplayMode::Follower, &block)?
        });

        let profiler_results = Profiler::take_results();

        // We clone the transaction metrics because 'metrics' is needed below for calibration/stats.
        // The buffer takes ownership of the data.
        profiler_buffer.push((
            block.id.clone(),
            profiler_results,
            metrics.transactions.clone(),
        ));

        // Flush buffer if full
        if profiler_buffer.len() >= PROFILER_BATCH_SIZE {
            app_db.save_profiler_data_batch(run_model.id, &mut profiler_buffer)?;
        }

        if !calibrated {
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

                        accumulator.add(m); // Accumulate

                        // Use the buffered block ID to save metrics
                        let buffered_id = &block_ids[i - buffer_len + j + 1];
                        app_db.save_block_metrics(run_model.id, buffered_id, m)?;
                    }
                    metrics_buffer.clear();
                }
                // Else: continue collecting blocks to find variance
            }
        } else {
            if cost_model.time_per_byte > f64::EPSILON {
                metrics.apply_cost_model(&cost_model);
            } else {
                metrics.apply_heuristic();
            }

            accumulator.add(&metrics); // Accumulate

            // let static_pct = if metrics.commit_duration.as_secs_f64() > 0.0 {
            //     (metrics.commit_overhead_baseline.as_secs_f64()
            //         / metrics.commit_duration.as_secs_f64())
            //         * 100.0
            // } else {
            //     0.0
            // };
            // println!(
            //     "  Execution Metrics: {:?} (Static Commit: {:.1}%)",
            //     metrics, static_pct
            // );
            app_db.save_block_metrics(run_model.id, &block.id, &metrics)?;
        }
    }

    // Flush any remaining profiler data after the loop
    if !profiler_buffer.is_empty() {
        app_db.save_profiler_data_batch(run_model.id, &mut profiler_buffer)?;
    }

    // Flush any remaining metrics in the calibration buffer
    if !metrics_buffer.is_empty() {
        let buffer_len = metrics_buffer.len(); // Capture length before mutable borrow
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

            app_db.save_block_metrics(run_model.id, buffered_id, m)?;
        }
    }

    let duration = start.elapsed();

    app_db.finish_benchmark_run(run_model.id, Utc::now().naive_utc())?;

    println!("Re-executed {selected_block_count} blocks in {duration:.2?}");

    accumulator.print_summary(); // Print summary

    // Give the OS a moment to sync metadata
    std::thread::sleep(Duration::from_millis(100));

    let (growth, written) = bench_context.generate_delta_report()?;

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
    println!("Cleaning up and checkpointing/vacuuming database...");
    app_db.checkpoint()?;
    app_db.vacuum()?;
    println!("Benchmark run complete");

    Ok(())
}

fn run_metabase(app_data: &AppDataDir, port: u16, image_tag: String) -> Result<()> {
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

    // Create a runtime to execute the async Docker operations
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move { run_metabase_container(&app_data, port, image_tag).await })
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
