use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use blockstack_lib::burnchains::Txid;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use stacks_bench::blocks::{BackwardsBlockStream, BlockRef};
use stacks_bench::context::BenchContext;
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::CheckpointMode;
use stacks_bench::db::node::{ChainStateDb, NakamotoDb};
use stacks_bench::filter::TxFilter;
use stacks_bench::indexer::{ChainIndexPlan, ChainstateIndexer};
use stacks_bench::metrics::{BlockMetrics, CostModel, MetricsAccumulator, ModelSource};
use stacks_bench::paths::ChainStateDir;
use stacks_bench::replay::{ReplayMode, SegmentReplayInfo};
use stacks_bench::{Network, StacksBlockHeader, StacksBlockLoader, StacksBlockRef};
use stacks_profiler::Profiler;

use crate::cli::common::{
    CliContext, IndexerArgs, TxIdArg, create_shadow_dir, get_git_hash, setup_bench_env,
    setup_bench_env_and_plan,
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
    #[arg(long, conflicts_with_all = ["start_at", "end_at", "block_count", "filter"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<TxIdArg>,

    /// Number of times to replay the target transaction's block when using
    /// `--txid`. Each repetition forks from the same parent block, producing
    /// an independent measurement. Defaults to 10.
    #[arg(long, default_value_t = 10, requires = "txid")]
    repetitions: u32,

    /// Number of blocks to use for calibration of the commit cost model.
    #[arg(long, value_name = "CALIBRATION_BLOCKS", default_value_t = 20)]
    calibration: usize,

    /// Number of blocks to process as warmup before starting measurements.
    /// In block-range mode, this is the number of warmup blocks.
    /// In `--txid` mode, this is the number of warmup repetitions
    /// (discarded before measurement begins).
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

/// Result of scanning the canonical chain for a specific transaction.
struct TxidScanResult {
    /// The header of the block containing the target transaction.
    block_header: StacksBlockHeader,
    /// The index of the target transaction within the block's transaction list.
    #[allow(dead_code)]
    tx_index: usize,
}

/// Walk the canonical chain backwards from `tip`, loading and deserializing
/// each block, until the transaction with `target_txid` is found.
///
/// This operates on the **source** node directory (read-only) and does not
/// require a shadow copy or prior indexing.
async fn scan_for_txid(
    source_dir: &Path,
    tip: &BlockRef,
    target_txid: &Txid,
) -> Result<TxidScanResult> {
    let chainstate_dir = ChainStateDir::from_node_root(source_dir);
    let chainstate_db = ChainStateDb::open_for_read(chainstate_dir.index_db_path()).await?;
    let mut naka_db = NakamotoDb::open_for_read(chainstate_dir.nakamoto_db_path()).await?;
    let min_naka_height = naka_db.get_min_block_height().await?.unwrap_or(u64::MAX);
    let blocks_dir = chainstate_dir.blocks_dir();

    let mut stream = BackwardsBlockStream::new(&chainstate_db, tip.id.clone());
    let mut scanned: u64 = 0;

    loop {
        let header = stream.next_block().await?.ok_or_else(|| {
            anyhow::anyhow!(
                "Reached genesis without finding txid {}",
                target_txid.to_hex()
            )
        })?;

        let mut loader = StacksBlockLoader::new(&blocks_dir, &mut naka_db, min_naka_height);
        let block = loader.load_block(&header).await?;

        for (i, tx) in block.transactions().iter().enumerate() {
            if tx.txid() == *target_txid {
                return Ok(TxidScanResult {
                    block_header: header,
                    tx_index: i,
                });
            }
        }

        scanned += 1;
        if scanned.is_multiple_of(1000) {
            eprint!(".");
        }
    }
}

impl RunArgs {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize BenchArgs to JSON")
    }

    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        // Dispatch to the txid-specific path if --txid is provided.
        if self.txid.is_some() {
            return self.exec_txid(ctx).await;
        }

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

            // Enable record!() capture for measured (non-warmup) blocks.
            // Must happen before replay_block() so record!() calls during
            // block execution are captured.
            if !is_warmup && !self.no_profiler_kv {
                Profiler::enable_record();
            }

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
        // Moving the context into a local binding ensures the shadow dir is
        // cleaned up (its Drop runs here).
        let _cleanup = bench_context;
        cleanup_spinner.stop(fmt_success!(
            "Finished cleanup in {:.2}s",
            cleanup_start.elapsed().as_secs_f32()
        ));

        Ok(())
    }

    /// Execute the single-transaction benchmark path (`--txid`).
    ///
    /// Flow:
    /// 1. Scan the source node's canonical chain to locate the target txid
    /// 2. Create a shadow dir copy
    /// 3. Index only the narrow range around the target block
    /// 4. Replay the block N times (--repetitions), each fork from the same parent
    async fn exec_txid(&self, ctx: &CliContext) -> Result<()> {
        stacks_profiler::Profiler::disable_record();

        let txid_arg = self.txid.as_ref().expect("--txid required for exec_txid");
        let target_txid = Txid::from_bytes_be(txid_arg.as_bytes())
            .ok_or_else(|| anyhow::anyhow!("Failed to convert txid bytes to Txid"))?;

        let mut app_db = ctx.app_db();

        // ------------------------------------------------------------------
        // Phase 0: Locate the transaction in the source chainstate
        // ------------------------------------------------------------------
        // We scan the source dir BEFORE creating the shadow copy so that we
        // know the exact block range to index (minimizing I/O).

        // First, resolve the canonical tip from the source dir to use for scanning.
        let source_chainstate_dir = ChainStateDir::from_node_root(&self.source_dir);
        let source_chainstate_db =
            ChainStateDb::open_for_read(source_chainstate_dir.index_db_path()).await?;
        let source_tip = {
            use stacks_bench::db::node::sortition::SortitionDb;
            use stacks_bench::paths::BurnChainDir;
            let burnchain_dir = BurnChainDir::from_node_root(&self.source_dir);
            let mut sortdb = SortitionDb::open_for_read(burnchain_dir.sortition_db_path()).await?;
            let (tip_id, tip_height) = sortdb.get_canonical_stacks_tip().await?;
            BlockRef {
                id: tip_id,
                height: tip_height,
            }
        };

        // If --tip is provided, resolve it; otherwise use canonical tip
        let scan_tip = if let Some(tip_ref) = &self.tip {
            match tip_ref {
                StacksBlockRef::Id(id) => {
                    let hdr = source_chainstate_db
                        .get_block_header(id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("--tip block {id} not found in chainstate")
                        })?;
                    BlockRef {
                        id: id.clone(),
                        height: hdr.block_height as u64,
                    }
                }
                StacksBlockRef::Height(h) => {
                    anyhow::bail!(
                        "--tip by height ({h}) requires a chain-walk; pass --tip as a block id instead"
                    );
                }
            }
        } else {
            source_tip
        };
        drop(source_chainstate_db);

        let scan_spinner = cliclack::spinner();
        scan_spinner.start(format!("Scanning canonical chain for txid {}…", txid_arg));
        let scan_start = Instant::now();

        let scan_result = scan_for_txid(&self.source_dir, &scan_tip, &target_txid).await?;

        scan_spinner.stop(fmt_success!(
            "Found txid {} in block {} (height {}) — scanned in {:.2}s",
            txid_arg,
            scan_result.block_header.id,
            scan_result.block_header.height,
            scan_start.elapsed().as_secs_f32()
        ));

        let target_height = scan_result.block_header.height;

        // ------------------------------------------------------------------
        // Phase 1: Shadow dir + BenchEnv setup
        // ------------------------------------------------------------------
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

        let (env, anchor_tip) =
            setup_bench_env(&shadow_dir, self.network, self.tip.as_ref()).await?;

        cliclack::note(
            "Environment Summary",
            format!(
                "Chain ID:   {}\n\
                Network:    {}\n\
                Epochs:     {}\n\
                Source Dir: {}\n\
                Shadow Dir: {}\n\
                Target Tx:  {}\n\
                Tx Block:   {} (height {})\n\
                Repetitions: {} ({} warmup)",
                env.chain_id,
                env.network,
                env.epochs
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                shadow_dir.source().display(),
                shadow_dir.path().display(),
                txid_arg,
                scan_result.block_header.id,
                target_height,
                self.repetitions,
                self.warmup,
            ),
        )?;

        // ------------------------------------------------------------------
        // Phase 2: Narrow-range indexing (parent + target block only)
        // ------------------------------------------------------------------
        let index_start = target_height.saturating_sub(1).max(1);
        let plan = ChainIndexPlan {
            anchor_tip,
            start_height: index_start,
            end_height: target_height,
        };

        cliclack::log::step("Indexing target block range...")?;
        let mut indexer = ChainstateIndexer::new(&mut app_db, &env);
        let (resolved, _block_ids) = indexer
            .index_chainstate_range(env.network, env.chain_id, &env.epochs, plan)
            .await?;

        // Ensure chainstate row exists
        let (chainstate_model, _) = app_db
            .get_or_create_chainstate(env.network, env.chain_id, &resolved.anchor_tip, &env.epochs)
            .await?;

        // Build BenchContext
        let mut bench_context = BenchContext::from_env(
            &env,
            resolved.anchor_tip.clone(),
            resolved.start.clone(),
            resolved.end.clone(),
        );

        let run_name = format!("txid-{}", Utc::now().format("%Y%m%d-%H%M%S"));
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

        // ------------------------------------------------------------------
        // Phase 3: Replay the target block N times
        // ------------------------------------------------------------------
        let replay_mode = ReplayMode::SingleTx(TxFilter::Txid(target_txid));

        let (mut chainstate, burnchain) = bench_context.open_stacks_chainstate()?;

        shadow_dir.calculate_storage_delta()?; // Reset storage delta baseline

        let total_reps = self.repetitions as usize;
        let warmup_reps = self.warmup.min(total_reps);
        let measured_reps = total_reps - warmup_reps;

        let replay_multi_pb = cliclack::multi_progress(format!(
            "Replaying txid {} — {} repetitions ({} warmup)",
            txid_arg, total_reps, warmup_reps
        ));

        let maybe_warmup_pb = if warmup_reps > 0 {
            let pb = replay_multi_pb.add(
                cliclack::progress_bar(warmup_reps as u64)
                    .with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
            );
            pb.start("Warming up");
            Some(pb)
        } else {
            None
        };

        let replay_pb = replay_multi_pb.add(
            cliclack::progress_bar(measured_reps as u64).with_template(BLOCK_PROGRESS_BAR_TEMPLATE),
        );

        let mut accumulator = MetricsAccumulator::default();
        let mut metrics_buffer = Vec::new();
        let mut total_clarity_db_checkpoint_duration = Duration::ZERO;
        let mut last_storage_delta: i64 = 0;

        let block_header = app_db.get_block(&scan_result.block_header.id).await?;

        let start = Instant::now();
        for rep in 0..total_reps {
            let is_warmup = rep < warmup_reps;
            Profiler::clear();

            if !is_warmup && !self.no_profiler_kv {
                Profiler::enable_record();
            }

            let repetition = rep as u32;

            let mut on_segment =
                |_: &SegmentReplayInfo, m: Option<&mut BlockMetrics>| -> Result<()> {
                    let storage_report = shadow_dir.calculate_storage_delta()?;
                    let current_delta = storage_report.net_growth_bytes;
                    let delta_since_last = current_delta - last_storage_delta;
                    last_storage_delta = current_delta;

                    if !is_warmup && let Some(m) = m {
                        m.total_storage_delta = delta_since_last;
                        total_clarity_db_checkpoint_duration += m.clarity_db_checkpoint_duration;
                    }
                    Ok(())
                };

            let maybe_metrics_vec = stacks_bench::replay::replay_block(
                &mut bench_context,
                &mut chainstate,
                &burnchain,
                &replay_mode,
                &block_header,
                repetition,
                Some(&mut on_segment),
            )?;

            if is_warmup {
                if let Some(warmup_pb) = &maybe_warmup_pb {
                    warmup_pb.set_position((rep + 1) as u64);
                    if rep + 1 == warmup_reps {
                        warmup_pb.stop(fmt_success!(
                            "Warmup complete ({warmup_reps} reps in {:.2}s)",
                            start.elapsed().as_secs_f32()
                        ));
                        replay_pb.start("Replaying measured repetitions...");
                    }
                }
                continue;
            }

            // Update measured progress
            replay_pb.set_position((rep - warmup_reps + 1) as u64);

            let Some(metrics_vec) = maybe_metrics_vec else {
                continue;
            };

            accumulator.add_many(&metrics_vec);

            // In txid mode, skip calibration — apply heuristic directly
            for mut metrics in metrics_vec {
                metrics.apply_heuristic();
                metrics_buffer.push(metrics);
            }
        }

        // Finalize progress bars
        replay_pb.stop(fmt_success!(
            "Replayed {measured_reps} measured repetitions in {:.2}s",
            start.elapsed().as_secs_f32()
        ));
        replay_multi_pb.stop();

        // Flush metrics
        if !metrics_buffer.is_empty() {
            app_db
                .save_block_metrics(run_model.id, metrics_buffer.drain(..))
                .await?;
        }

        let duration = start.elapsed();

        app_db
            .finish_benchmark_run(run_model.id, Utc::now().naive_utc())
            .await?;

        println!(
            "\nReplayed txid {} × {measured_reps} measured reps in {duration:.2?}",
            txid_arg
        );
        println!("  - Clarity DB Checkpointing: {total_clarity_db_checkpoint_duration:.2?}");

        accumulator.print_summary();

        // Storage delta report
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
        let _cleanup = bench_context;
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

        baseline_pb1.start("Measuring baseline (round 1)...".to_string());
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

        baseline_pb2.start("Measuring baseline (round 2)...".to_string());
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
