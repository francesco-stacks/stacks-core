use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use stacks_bench::filter::TxFilter;
use stacks_bench::indexer::ChainstateIndexer;
use stacks_bench::metrics::{CostModel, MetricsAccumulator, ModelSource};
use stacks_bench::replay::ReplayMode;
use stacks_bench::{Network, StacksBlockRef};
use stacks_profiler::Profiler;

use crate::cli::common::{CliContext, IndexerArgs, TxIdArg, get_git_hash, setup_bench_context};

// TODO: Add a `--contract` arg to filter by qualified contract id
#[derive(clap::Args, Debug, Serialize, Deserialize)]
pub struct RunArgs {
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
    #[arg(long, conflicts_with_all = ["txid", "block_count"])]
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
    #[arg(long = "count", short = 'c', conflicts_with_all = ["end_at", "txid"], requires = "start_at")]
    #[serde(skip_serializing_if = "Option::is_none")]
    block_count: Option<u32>,

    /// A specific transaction id (hex) to benchmark. May not be used with
    /// `start-at`, `end-at`, or `count`.
    #[arg(long, conflicts_with_all = ["start_at", "end_at", "count", "filter"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<TxIdArg>,

    /// Number of blocks to use for calibration of the commit cost model.
    #[arg(long, value_name = "CALIBRATION_BLOCKS", default_value_t = 20)]
    calibration: usize,

    /// Number of blocks to process as warmup before starting measurements.
    /// These blocks will be executed but not included in the benchmark results.
    #[arg(long, default_value_t = 0)]
    warmup: usize,

    #[arg(long, short = 'f', conflicts_with_all = ["txid"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<FilterArg>,
}

#[derive(clap::ValueEnum, Clone, Debug, Serialize, Deserialize)]
pub enum FilterArg {
    ContractCall,
}

impl IndexerArgs for RunArgs {
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

impl RunArgs {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize BenchArgs to JSON")
    }

    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let mut app_db = ctx.app_db();

        let (mut bench_context, network, chain_id, epochs) =
            setup_bench_context(&mut app_db, self).await?;

        let (chainstate_model, _) = app_db
            .get_or_create_chainstate(network, chain_id, bench_context.chain_tip(), &epochs)
            .await?;

        let block_ids = ChainstateIndexer::new(&mut app_db, &mut bench_context)
            .index_chainstate(network, chain_id, &epochs)
            .await?;

        let selected_block_count = block_ids.len();

        let run_name = format!("{}", Utc::now().format("%Y%m%d-%H%M%S"));

        let args_json = self.to_json()?;
        let git_commit_hash = get_git_hash().unwrap_or(vec![0u8; 20]);
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
        let min_calibration_count = self.calibration;
        // Allow buffer to grow if we can't find variance. Empty blocks are small, so 500 is memory-safe.
        let max_calibration_buffer = 500;
        let mut calibrated = false;
        let mut is_warmup = false;
        let mut total_clarity_db_checkpoint_duration = Duration::ZERO;

        let mut accumulator = MetricsAccumulator::default();

        let start = Instant::now();
        for (i, block_id) in block_ids.iter().enumerate() {
            is_warmup = i < self.warmup;
            Profiler::clear();

            // Load metadata from App DB
            let block = app_db.get_block(block_id).await?;

            if is_warmup {
                if i == 0 {
                    println!("Warming up for {} blocks...", self.warmup);
                }
                if i % 10 == 0 {
                    eprint!("(Warmup {}) ", block.height);
                }
            } else if i == self.warmup || i % 2_500 == 0 || i == selected_block_count - 1 {
                eprint!("{}", block.height);
            } else if i % 250 == 0 {
                eprint!(".");
            }

            // Replay the block
            let mode = match self.filter.as_ref() {
                Some(FilterArg::ContractCall) => {
                    ReplayMode::SegmentedFiltered(TxFilter::ContractCall)
                }
                None => ReplayMode::Follower,
            };

            let mut metrics = stacks_profiler::measure!("Block Replay", {
                stacks_bench::replay::replay_block(
                    &mut bench_context,
                    &mut chainstate,
                    &burnchain,
                    mode,
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
}
