use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use stacks_bench::context::BenchContext;
use stacks_bench::db::app::CheckpointMode;
use stacks_bench::filter::TxFilter;
use stacks_bench::indexer::ChainstateIndexer;
use stacks_bench::metrics::{BlockMetrics, CostModel, MetricsAccumulator, ModelSource};
use stacks_bench::replay::{ReplayMode, SegmentReplayInfo};
use stacks_bench::{Network, StacksBlockRef};
use stacks_profiler::Profiler;

use crate::cli::common::{
    CliContext, IndexerArgs, TxIdArg, create_shadow_dir, get_git_hash, setup_bench_env_and_plan,
};

const BASELINE_MEASURED_BLOCKS: u32 = 1000;
const BLOCK_PROGRESS_BAR_TEMPLATE: &str = "{msg:20} {percent:>3}% |{bar:30.cyan/blue}| {pos:>8}/{len} • {per_sec:<6!} blk/s • ETA {eta_precise}";

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

    /// Filter to apply when selecting transactions to process.
    #[arg(long, short = 'f', conflicts_with_all = ["txid"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<FilterArg>,

    /// Disable capturing of profiler key-value records generated via `record!()`. This can
    /// provide a slight performance benefit and reduce storage if you do not need them.
    #[arg(long, default_value_t = false)]
    no_profiler_kv: bool,

    /// Whether or not to include pre-Nakamoto blocks in the reflink copy of the source node data
    /// directory, which is necessary if benchmarking from blocks prior to the chainstate's Nakamoto
    /// start height + 1. Enabling this can add significant time when creating the reflink copy
    /// for large chainstates. Defaults to false/disabled.
    #[arg(long = "with-pre-naka", default_value_t = false)]
    include_pre_nakamoto_blocks: bool,
}

#[derive(clap::ValueEnum, Clone, Debug, Serialize, Deserialize)]
pub enum FilterArg {
    ContractCall,
}

impl IndexerArgs for RunArgs {
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
        // Disable capturing `record!()` entries during setup/warmup.
        stacks_profiler::Profiler::disable_record();

        let mut app_db = ctx.app_db();

        let shadow_dir_spinner = cliclack::spinner();
        shadow_dir_spinner.start(
            "Creating reflink copy of source node data directory (this may take some time)...",
        );
        let shadow_dir_timer = Instant::now();
        let shadow_dir = create_shadow_dir(&self.source_dir, self.include_pre_nakamoto_blocks)?;
        shadow_dir_spinner.stop(format!(
            "Chainstate working directory reflink-copied in {:.2}s",
            shadow_dir_timer.elapsed().as_secs_f32()
        ));

        // Create env + compute height plan
        let (env, plan) = setup_bench_env_and_plan(&shadow_dir, self).await?;

        cliclack::note(
            "Environment Summary",
            format!(
                "Chain ID:   {}\n\
                Network:    {}\n\
                Epochs:     {}\n\
                Source Dir: {}\n\
                Shadow Dir: {}",
                env.chain_id,
                env.network,
                env.epochs
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                shadow_dir.source().display(),
                shadow_dir.path().display(),
            ),
        )?;

        // Setup indexer and index chainstate range
        cliclack::log::step("Indexing node chainstate...")?;
        let mut indexer = ChainstateIndexer::new(&mut app_db, &env);
        let (resolved, block_ids) = indexer
            .index_chainstate_range(env.network, env.chain_id, &env.epochs, plan)
            .await?;

        // Ensure chainstate row exists
        let (chainstate_model, _) = app_db
            .get_or_create_chainstate(env.network, env.chain_id, &resolved.anchor_tip, &env.epochs)
            .await?;

        // Build BenchContext from resolved BlockRefs
        let mut bench_context = BenchContext::from_env(
            &env,
            resolved.anchor_tip.clone(),
            resolved.start.clone(),
            resolved.end.clone(),
        );

        let selected_block_count = block_ids.len();
        let actual_block_count = selected_block_count - self.warmup;

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

        let (block_processing_baselines1, block_processing_baselines2) = self
            .run_overhead_baselines(&mut chainstate, &burnchain, &bench_context.end_block().id)?;

        cliclack::note(
            "Block Processing Overhead Baselines",
            format_baseline_note(&block_processing_baselines1, &block_processing_baselines2),
        )?;

        let averaged = avg_baseline(&block_processing_baselines1, &block_processing_baselines2);
        app_db
            .save_block_processing_baseline(
                run_model.id,
                &bench_context.end_block().id,
                self.warmup as u32,
                BASELINE_MEASURED_BLOCKS,
                &averaged,
            )
            .await?;

        shadow_dir.calculate_storage_delta()?; // Reset storage delta baseline

        println!(
            "Re-executing {selected_block_count} selected blocks ({} block warmup)...",
            self.warmup
        );

        const METRICS_FLUSH_THRESHOLD: usize = 250;
        let mut metrics_buffer = Vec::new();
        let mut cost_model = CostModel::default();

        // Dynamic calibration window
        let min_calibration_count = self.calibration;
        // Allow buffer to grow if we can't find variance. Empty blocks are small, so 500 is memory-safe.
        let max_calibration_buffer = 500;
        let mut calibrated = false;
        let mut is_warmup = false;
        let mut total_clarity_db_checkpoint_duration = Duration::ZERO;
        let mut last_storage_delta: i64 = 0;

        // Determine the execution mode
        let replay_mode = match self.filter.as_ref() {
            Some(FilterArg::ContractCall) => ReplayMode::SegmentedFiltered(TxFilter::ContractCall),
            None => ReplayMode::Follower,
        };

        let mut accumulator = MetricsAccumulator::default();
        let replay_multi_pb = cliclack::multi_progress(format!(
            "Re-executing {actual_block_count} blocks in {replay_mode} mode"
        ));
        let maybe_warmup_pb = if self.warmup > 0 {
            let pb = replay_multi_pb.add(
                cliclack::progress_bar(self.warmup as u64)
                    .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
            );
            pb.start("Warming up");
            Some(pb)
        } else {
            None
        };
        let replay_pb = replay_multi_pb.add(
            cliclack::progress_bar((selected_block_count - self.warmup) as u64)
                .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
        );

        let start = Instant::now();
        for (i, block_id) in block_ids.iter().enumerate() {
            is_warmup = i < self.warmup;
            Profiler::clear();

            // Load metadata from App DB
            let block = app_db.get_block(block_id).await?;

            // Not used yet; intended to enable repeating block execution to get
            // an averaged processing time.
            let repetition: u32 = 0;

            let mut on_segment =
                |_: &SegmentReplayInfo, m: Option<&mut BlockMetrics>| -> Result<()> {
                    let storage_report = shadow_dir.calculate_storage_delta()?;
                    let current_delta = storage_report.net_growth_bytes;
                    let delta_since_last = current_delta - last_storage_delta;
                    last_storage_delta = current_delta;

                    // If warmup, just advance baseline and skip setting metrics
                    if !is_warmup && let Some(m) = m {
                        m.total_storage_delta = delta_since_last;
                        total_clarity_db_checkpoint_duration += m.clarity_db_checkpoint_duration;
                    }
                    Ok(())
                };

            // Replay the block
            let maybe_metrics_vec = stacks_bench::replay::replay_block(
                &mut bench_context,
                &mut chainstate,
                &burnchain,
                &replay_mode,
                &block,
                repetition,
                Some(&mut on_segment),
            )?;

            // Handle warmup->execute transition
            if is_warmup {
                let Some(warmup_pb) = &maybe_warmup_pb else {
                    bail!("Warmup progress bar missing");
                };
                warmup_pb.set_position((i + 1) as u64);
                continue;
            } else {
                if let Some(warmup_pb) = &maybe_warmup_pb {
                    // Close out the warmup progress bar
                    warmup_pb.stop(fmt_success!(
                        "Warmup complete ({} blocks in {:.2}s)",
                        self.warmup,
                        start.elapsed().as_secs_f32()
                    ));

                    // Start the replay progress bar
                    replay_pb.start("Replaying selected blocks...");

                    // Enable/disable the capturing of `record!()` entries depending on args
                    if self.no_profiler_kv {
                        stacks_profiler::Profiler::disable_record();
                    } else {
                        stacks_profiler::Profiler::enable_record();
                    }
                }

                // Update replay progress bar position
                replay_pb.set_position((i - self.warmup + 1) as u64);
            }

            let Some(metrics_vec) = maybe_metrics_vec else {
                // No metrics collected (e.g. filtered out)
                continue;
            };

            // Accumulate summary stats across *all* returned measurement units
            accumulator.add_many(&metrics_vec);

            if !calibrated {
                metrics_buffer.extend(metrics_vec);

                let buffer_len = metrics_buffer.len();
                let is_last_block = i == selected_block_count - 1;

                if buffer_len >= min_calibration_count {
                    let candidate_model = CostModel::compute(&metrics_buffer);

                    let is_good_model = candidate_model.source != ModelSource::SingleBlock
                        && candidate_model.time_per_byte > f64::EPSILON;

                    if is_good_model || buffer_len >= max_calibration_buffer || is_last_block {
                        cost_model = candidate_model;
                        calibrated = true;

                        // println!("\n--- Calibration Complete ({} samples) ---", buffer_len);
                        // println!("  Method:          {:?}", cost_model.source);
                        // println!("  Static Overhead: {:.2?}", cost_model.static_overhead);
                        // println!(
                        //     "  Cost per Byte:   {:.2} µs",
                        //     cost_model.time_per_byte * 1_000_000.0
                        // );

                        // if cost_model.time_per_byte <= f64::EPSILON {
                        //     println!(
                        //         "  [WARN] Correlation weak or negative. Falling back to default heuristic."
                        //     );
                        // }
                        // println!("----------------------------------------\n");

                        for m in metrics_buffer.iter_mut() {
                            if cost_model.time_per_byte > f64::EPSILON {
                                m.apply_cost_model(&cost_model);
                            } else {
                                m.apply_heuristic();
                            }
                        }

                        app_db
                            .save_block_metrics(run_model.id, metrics_buffer.drain(..))
                            .await?;
                    }
                }
            } else {
                for mut metrics in metrics_vec {
                    if cost_model.time_per_byte > f64::EPSILON {
                        metrics.apply_cost_model(&cost_model);
                    } else {
                        metrics.apply_heuristic();
                    }

                    metrics_buffer.push(metrics);

                    if metrics_buffer.len() >= METRICS_FLUSH_THRESHOLD {
                        app_db
                            .save_block_metrics(run_model.id, metrics_buffer.drain(..))
                            .await?;
                    }
                }
            }
        }

        // Finalize progress bars
        replay_pb.stop(fmt_success!(
            "Replayed {} blocks in {:.2}s",
            selected_block_count - self.warmup,
            start.elapsed().as_secs_f32()
        ));
        replay_multi_pb.stop();

        // Flush any remaining metrics in the calibration buffer
        if !metrics_buffer.is_empty() {
            println!(
                "\nFlushing remaining {} block metrics from buffer...",
                metrics_buffer.len()
            );

            for m in metrics_buffer.iter_mut() {
                if !calibrated {
                    // Apply heuristic since we didn't finish calibration
                    m.apply_heuristic();
                }
            }

            app_db
                .save_block_metrics(run_model.id, metrics_buffer.drain(..))
                .await?;
        } else if is_warmup {
            println!("\nNo blocks executed for benchmarking (all warmup).");
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

        let storage_delta_report = shadow_dir.calculate_storage_delta()?;
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
        println!("Checkpointing database...");
        app_db.checkpoint(CheckpointMode::Truncate).await?;
        println!("Vacuuming database...");
        app_db.vacuum().await?;

        let cleanup_spinner = cliclack::spinner();
        let cleanup_start = Instant::now();
        cleanup_spinner.start("Cleaning up (this may take a few moments for large chainstates)...");
        // Dropping the context will clean up the shadow dir
        drop(bench_context);
        cleanup_spinner.stop(fmt_success!(
            "Finished cleanup in {:.2}s",
            cleanup_start.elapsed().as_secs_f32()
        ));

        Ok(())
    }

    fn run_overhead_baselines(
        &self,
        chainstate: &mut blockstack_lib::chainstate::stacks::db::StacksChainState,
        burnchain: &blockstack_lib::burnchains::Burnchain,
        start_parent: &stacks_common::types::chainstate::StacksBlockId,
    ) -> anyhow::Result<(
        stacks_bench::metrics::BlockProcessingBaseline,
        stacks_bench::metrics::BlockProcessingBaseline,
    )> {
        let baseline_multipb =
            cliclack::multi_progress("Calculating block processing overhead baseline");

        let maybe_warmup_pb = if self.warmup > 0 {
            Some(
                baseline_multipb.add(
                    cliclack::progress_bar(self.warmup as u64)
                        .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
                ),
            )
        } else {
            None
        };

        let baseline_pb1 = baseline_multipb.add(
            cliclack::progress_bar(BASELINE_MEASURED_BLOCKS.into())
                .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
        );
        let baseline_pb2 = baseline_multipb.add(
            cliclack::progress_bar(BASELINE_MEASURED_BLOCKS.into())
                .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
        );

        if let Some(warmup_pb) = maybe_warmup_pb {
            warmup_pb.start("Warming up");
            let warmup_timer = Instant::now();
            stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
                chainstate,
                burnchain,
                start_parent,
                self.warmup as u32,
                |completed, _| warmup_pb.set_position(completed as u64),
            )?;
            warmup_pb.stop(fmt_success!(
                "Warmed up for {} blocks ({:.2}s)",
                self.warmup,
                warmup_timer.elapsed().as_secs_f32()
            ));
        }

        baseline_pb1.start(format!("Measuring baseline (round 1)..."));
        let t1 = Instant::now();
        let round1 = stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
            chainstate,
            burnchain,
            start_parent,
            BASELINE_MEASURED_BLOCKS,
            |completed, _| baseline_pb1.set_position(completed as u64),
        )?;
        baseline_pb1.stop(fmt_success!(
            "Baseline round 1 finished ({:.2}s)",
            t1.elapsed().as_secs_f32()
        ));

        baseline_pb2.start(format!("Measuring baseline (round 2)..."));
        let t2 = Instant::now();
        let round2 = stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
            chainstate,
            burnchain,
            start_parent,
            BASELINE_MEASURED_BLOCKS,
            |completed, _| baseline_pb2.set_position(completed as u64),
        )?;
        baseline_pb2.stop(fmt_success!(
            "Baseline round 2 finished ({:.2}s)",
            t2.elapsed().as_secs_f32()
        ));

        baseline_multipb.stop();

        // Checkpoint the chainstate/clarity dbs so we don't incur the cost of overhead calculations
        // during replay
        chainstate.checkpoint_sqlite_dbs()?;

        Ok((round1, round2))
    }
}

fn avg_baseline(
    a: &stacks_bench::metrics::BlockProcessingBaseline,
    b: &stacks_bench::metrics::BlockProcessingBaseline,
) -> stacks_bench::metrics::BlockProcessingBaseline {
    stacks_bench::metrics::BlockProcessingBaseline {
        avg_setup_duration: (a.avg_setup_duration + b.avg_setup_duration) / 2,
        avg_finalize_duration: (a.avg_finalize_duration + b.avg_finalize_duration) / 2,
        avg_clarity_state_commit_duration: (a.avg_clarity_state_commit_duration
            + b.avg_clarity_state_commit_duration)
            / 2,
        avg_advance_tip_duration: (a.avg_advance_tip_duration + b.avg_advance_tip_duration) / 2,
        avg_index_commit_duration: (a.avg_index_commit_duration + b.avg_index_commit_duration) / 2,
    }
}

fn format_baseline_note(
    round1: &stacks_bench::metrics::BlockProcessingBaseline,
    round2: &stacks_bench::metrics::BlockProcessingBaseline,
) -> String {
    let fmt_duration = |d: Duration| format!("{d:.2?}");

    let line = |label: &str, r1: Duration, r2: Duration| {
        let avg = (r1 + r2) / 2;
        format!(
            "{label:<26} {r1s:>12} / {r2s:>12}  avg {avgs:>12}",
            label = label,
            r1s = fmt_duration(r1),
            r2s = fmt_duration(r2),
            avgs = fmt_duration(avg),
        )
    };

    [
        line(
            "Setup:",
            round1.avg_setup_duration,
            round2.avg_setup_duration,
        ),
        line(
            "Finalize:",
            round1.avg_finalize_duration,
            round2.avg_finalize_duration,
        ),
        line(
            "Clarity commit:",
            round1.avg_clarity_state_commit_duration,
            round2.avg_clarity_state_commit_duration,
        ),
        line(
            "Advance tip:",
            round1.avg_advance_tip_duration,
            round2.avg_advance_tip_duration,
        ),
        line(
            "Index commit:",
            round1.avg_index_commit_duration,
            round2.avg_index_commit_duration,
        ),
    ]
    .join("\n")
}
