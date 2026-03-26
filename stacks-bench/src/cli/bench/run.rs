use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use blockstack_lib::burnchains::Txid;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use stacks_bench::blocks::{BackwardsBlockStream, BlockRef};
use stacks_bench::context::BenchContext;
use stacks_bench::db::DbOpenForRead;
use stacks_bench::db::app::{AppDb, CheckpointMode};
use stacks_bench::db::node::{ChainStateDb, NakamotoDb};
use stacks_bench::filter::TxFilter;
use stacks_bench::indexer::{ChainIndexPlan, ChainstateIndexer};
use stacks_bench::metrics::{BlockMetrics, CostModel, MetricsAccumulator, ModelSource};
use stacks_bench::paths::ChainStateDir;
use stacks_bench::replay::{ReplayMode, SegmentReplayInfo};
use stacks_bench::shadow::{ShadowDir, ShadowDirDeltaReport};
use stacks_bench::{Network, StacksBlockHeader, StacksBlockLoader, StacksBlockRef};
use stacks_profiler::Profiler;
use tokio::sync::mpsc;

use crate::cli::common::{
    Align, CliContext, IndexerArgs, Table, TxIdArg, create_shadow_dir, fmt_u64_thousands,
    get_git_hash, run_indexer_progress_ui, setup_bench_env, setup_bench_env_and_plan,
};

const BASELINE_MEASURED_BLOCKS: u32 = 1000;
const BLOCK_PROGRESS_BAR_TEMPLATE: &str = "{msg:20} {percent:>3}% |{bar:30.cyan/blue}| {pos:>8}/{len} • {per_sec:<6!} blk/s • ETA {eta_precise}";

// TODO: Add a `--contract` arg to filter by qualified contract id
#[derive(clap::Args, Debug, Serialize, Deserialize)]
pub struct RunArgs {
    /// Stacks node data dir (the directory containing the `chainstate` folder).
    #[arg(long = "source", short = 's')]
    source_dir: PathBuf,

    /// The Stacks block (height or hex block id) to start at, inclusive. Cannot be used with the
    /// `txid` flag.
    #[arg(long, conflicts_with = "txid", default_value = "1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    start_at: Option<StacksBlockRef>,

    /// The Stacks block (height or hex block id) to end at, inclusive. Cannot be used with the
    /// `txid` or `count` flags.
    #[arg(long, conflicts_with_all = ["txid", "block_count"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    end_at: Option<StacksBlockRef>,

    /// The tip block (height or hex block id) to use as the anchor for resolving canonical history.
    /// Defaults to the node's current canonical tip. Useful for benchmarking in forks: provide the
    /// fork's tip hash here.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<StacksBlockRef>,

    /// The network to use. If not specified, the network is inferred from the chainstate database.
    #[arg(long, short = 'n', value_enum)]
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<Network>,

    /// The number of blocks to process, starting from `start-at`.
    #[arg(long = "count", short = 'c', conflicts_with_all = ["end_at", "txid"], requires = "start_at")]
    #[serde(skip_serializing_if = "Option::is_none")]
    block_count: Option<u32>,

    /// A specific transaction id (hex) to benchmark. May not be used with `start-at`, `end-at`, or
    /// `count`.
    #[arg(long, conflicts_with_all = ["start_at", "end_at", "block_count", "filter"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<TxIdArg>,

    /// Number of measured times to replay the target transaction's block in `--txid` mode.
    ///
    /// Warmup runs (from `--warmup`) are additional and executed before these measured
    /// repetitions. Each replay forks from the same parent block, producing an independent
    /// measurement.
    #[arg(long, default_value_t = 10, requires = "txid")]
    repetitions: u32,

    /// Number of measured blocks to collect before fitting the commit cost
    /// model in block-range mode.
    #[arg(
        long,
        value_name = "CALIBRATION_BLOCKS",
        default_value_t = 20,
        conflicts_with = "txid"
    )]
    calibration: usize,

    /// Number of blocks to process as warmup before starting measurements.
    ///
    /// In block-range mode, this is the number of warmup blocks (the earliest
    /// selected blocks).
    ///
    /// In `--txid` mode, this is the number of warmup repetitions (discarded
    /// before measurement begins). These runs are additive to `--repetitions`.
    #[arg(long, default_value_t = 0)]
    warmup: usize,

    /// Filter to apply when selecting transactions to process.
    #[arg(long, short = 'f', conflicts_with_all = ["txid"])]
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<FilterArg>,

    /// Disable capturing of profiler key-value records generated via `record!` and `counter!`
    /// macros. This can provide a slight performance benefit and reduce storage if you do not need
    /// them.
    #[arg(long, default_value_t = false)]
    no_profiler_kv: bool,

    /// Whether or not to include pre-Nakamoto blocks in the reflink copy of the source node data
    /// directory, which is necessary if benchmarking from blocks prior to the chainstate's Nakamoto
    /// start height + 1. Enabling this can add significant time when creating the reflink copy
    /// for large chainstates. [default: false]
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
    app_db: &mut AppDb,
    source_dir: &Path,
    tip: &BlockRef,
    target_txid: &Txid,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<TxidScanResult> {
    let chainstate_dir = ChainStateDir::from_node_root(source_dir);
    let chainstate_db = ChainStateDb::open_for_read(chainstate_dir.index_db_path()).await?;
    let mut naka_db = NakamotoDb::open_for_read(chainstate_dir.nakamoto_db_path()).await?;
    let min_naka_height = naka_db.get_min_block_height().await?.unwrap_or(u64::MAX);
    let blocks_dir = chainstate_dir.blocks_dir();

    let mut stream = BackwardsBlockStream::new(&chainstate_db, tip.id.clone()).with_cache(app_db);

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
        on_progress(scanned, header.height);
    }
}

impl RunArgs {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize BenchArgs to JSON")
    }

    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        // Install a ctrl-c handler so we can break out of the replay loop
        // gracefully and still run cleanup (shadow dir removal, DB vacuum).
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let interrupted = interrupted.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                interrupted.store(true, Ordering::Relaxed);
            });
        }

        // Dispatch to the txid-specific path if --txid is provided.
        if self.txid.is_some() {
            return self.exec_txid(ctx, &interrupted).await;
        }

        // Disable capturing `record!()` entries during setup/warmup.
        stacks_profiler::Profiler::disable_record();

        let mut app_db = ctx.app_db();

        let shadow_dir_spinner = cliclack::spinner();
        shadow_dir_spinner.start("Coping source node data directory (this may take some time)...");
        let shadow_dir_timer = Instant::now();
        let shadow_dir = create_shadow_dir(&self.source_dir, self.include_pre_nakamoto_blocks)?;
        shadow_dir_spinner.stop(format!(
            "Chainstate copied in {:.2}s [reflink/CoW]",
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
        let tip_height = plan.anchor_tip.height;
        let idx_start = plan.start_height;
        let idx_end = plan.end_height;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut indexer = ChainstateIndexer::new(&mut app_db, &env).with_events(event_tx);

        let ui_fut = run_indexer_progress_ui(event_rx, idx_start, idx_end, tip_height);
        let index_fut =
            indexer.index_chainstate_range(env.network, env.chain_id, &env.epochs, plan);

        let ((resolved, block_ids), _) = tokio::try_join!(index_fut, ui_fut)?;

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
        if self.warmup > selected_block_count {
            bail!(
                "--warmup ({}) cannot exceed selected block count ({})",
                self.warmup,
                selected_block_count
            );
        }
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
            .run_overhead_baselines(
                &mut chainstate,
                &burnchain,
                &bench_context.end_block().id,
                &interrupted,
            )?;

        if interrupted.load(Ordering::Relaxed) {
            cliclack::log::info("Interrupted before replay, skipping benchmark.")?;
            run_cleanup(app_db, shadow_dir).await?;
            return Ok(());
        }

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

        cliclack::log::step(format!(
            "Re-executing {selected_block_count} selected blocks ({} block warmup)",
            self.warmup
        ))?;

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
            if interrupted.load(Ordering::Relaxed) {
                cliclack::log::info("Interrupted by Ctrl-C, cleaning up...")?;
                break;
            }

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
        if interrupted.load(Ordering::Relaxed) {
            replay_pb.cancel("Interrupted");
            replay_multi_pb.cancel();
        } else {
            replay_pb.stop(fmt_success!(
                "Replayed {} blocks in {:.2}s",
                selected_block_count - self.warmup,
                start.elapsed().as_secs_f32()
            ));
            replay_multi_pb.stop();
        }

        // Flush any remaining metrics in the calibration buffer
        if !metrics_buffer.is_empty() {
            cliclack::log::step(format!(
                "Flushing remaining {} block metrics from buffer",
                metrics_buffer.len()
            ))?;

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
            cliclack::log::info("No blocks executed for benchmarking (all warmup).")?;
        }

        let duration = start.elapsed();

        app_db
            .finish_benchmark_run(run_model.id, Utc::now().naive_utc())
            .await?;

        {
            let total_replay_duration = accumulator.summary().duration;
            let overhead = duration
                .saturating_sub(total_replay_duration)
                .saturating_sub(total_clarity_db_checkpoint_duration);
            let mut table = Table::new()
                .col("Metric", Align::Left)
                .col("Value", Align::Right);
            table.row(vec![
                "Blocks".into(),
                format!(
                    "{selected_block_count} ({} warmup + {actual_block_count} measured)",
                    self.warmup,
                ),
            ]);
            table.row(vec!["Total Duration".into(), format!("{duration:.2?}")]);
            table.row(vec![
                "Block Replay".into(),
                format!("{total_replay_duration:.2?}"),
            ]);
            table.row(vec![
                "Clarity DB Checkpointing".into(),
                format!("{total_clarity_db_checkpoint_duration:.2?}"),
            ]);
            table.row(vec![
                "Benchmarking Overhead".into(),
                format!("{overhead:.2?}"),
            ]);
            if interrupted.load(Ordering::Relaxed) {
                let completed = accumulator.summary().count;
                table.row(vec![
                    "Status".into(),
                    format!("INTERRUPTED ({completed}/{selected_block_count} blocks)"),
                ]);
            }
            cliclack::note("Replay Summary", table.to_string())?;
        }

        print_benchmark_summary(&accumulator)?;

        // Give the OS a moment to sync metadata
        std::thread::sleep(Duration::from_millis(100));

        let storage_delta_report = shadow_dir.calculate_storage_delta()?;
        print_storage_delta_report(&storage_delta_report)?;

        run_cleanup(app_db, shadow_dir).await?;

        Ok(())
    }

    /// Execute the single-transaction benchmark path (`--txid`).
    ///
    /// Flow:
    /// 1. Scan the source node's canonical chain to locate the target txid
    /// 2. Create a shadow dir copy
    /// 3. Index only the narrow range around the target block
    /// 4. Replay the block N times (--repetitions), each fork from the same parent
    async fn exec_txid(&self, ctx: &CliContext, interrupted: &Arc<AtomicBool>) -> Result<()> {
        stacks_profiler::Profiler::disable_record();

        let txid_arg = self.txid.as_ref().expect("--txid required for exec_txid");
        let target_txid = Txid::from_bytes(txid_arg.as_bytes())
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

        let scan_result = if let Some(block_header) = app_db
            .find_block_for_tx_hash_on_chain_tip(txid_arg.as_bytes(), &scan_tip.id)
            .await?
        {
            scan_spinner.set_message(format!(
                "Fast-path hit: found txid {} in App DB for chain tip {}",
                txid_arg, scan_tip.id
            ));
            TxidScanResult {
                block_header,
                tx_index: 0,
            }
        } else {
            let mut last_progress_update = Instant::now();
            scan_for_txid(
                &mut app_db,
                &self.source_dir,
                &scan_tip,
                &target_txid,
                |scanned, height| {
                    if last_progress_update.elapsed() >= Duration::from_secs(1) {
                        scan_spinner.set_message(format!(
                            "Scanning canonical chain for txid {}… checked {} blocks (current height {})",
                            txid_arg,
                            fmt_u64_thousands(scanned),
                            height
                        ));
                        last_progress_update = Instant::now();
                    }
                },
            )
            .await?
        };

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

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut indexer = ChainstateIndexer::new(&mut app_db, &env).with_events(event_tx);

        let ui_fut = run_indexer_progress_ui(
            event_rx,
            plan.start_height,
            plan.end_height,
            plan.anchor_tip.height,
        );
        let index_fut =
            indexer.index_chainstate_range(env.network, env.chain_id, &env.epochs, plan);

        let ((resolved, _block_ids), _) = tokio::try_join!(index_fut, ui_fut)?;

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

        let warmup_reps = self.warmup;
        let measured_reps = self.repetitions as usize;
        let total_reps = warmup_reps + measured_reps;

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
            if interrupted.load(Ordering::Relaxed) {
                cliclack::log::info("Interrupted by Ctrl-C, cleaning up...")?;
                break;
            }

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
        if interrupted.load(Ordering::Relaxed) {
            replay_pb.cancel("Interrupted");
            replay_multi_pb.cancel();
        } else {
            replay_pb.stop(fmt_success!(
                "Replayed {measured_reps} measured repetitions in {:.2}s",
                start.elapsed().as_secs_f32()
            ));
            replay_multi_pb.stop();
        }

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

        {
            let mut table = Table::new()
                .col("Metric", Align::Left)
                .col("Value", Align::Right);
            table.row(vec!["Transaction".into(), txid_arg.to_string()]);
            table.row(vec![
                "Repetitions".into(),
                format!("{total_reps} ({warmup_reps} warmup + {measured_reps} measured)",),
            ]);
            table.row(vec!["Total Duration".into(), format!("{duration:.2?}")]);
            table.row(vec![
                "Clarity DB Checkpointing".into(),
                format!("{total_clarity_db_checkpoint_duration:.2?}"),
            ]);
            if interrupted.load(Ordering::Relaxed) {
                let completed = accumulator.summary().count;
                table.row(vec![
                    "Status".into(),
                    format!("INTERRUPTED ({completed}/{measured_reps} repetitions)"),
                ]);
            }
            cliclack::note("Replay Summary", table.to_string())?;
        }

        print_benchmark_summary(&accumulator)?;

        // Storage delta report
        std::thread::sleep(Duration::from_millis(100));
        let storage_delta_report = shadow_dir.calculate_storage_delta()?;
        print_storage_delta_report(&storage_delta_report)?;

        run_cleanup(app_db, shadow_dir).await
    }

    fn run_overhead_baselines(
        &self,
        chainstate: &mut blockstack_lib::chainstate::stacks::db::StacksChainState,
        burnchain: &blockstack_lib::burnchains::Burnchain,
        start_parent: &stacks_common::types::chainstate::StacksBlockId,
        interrupted: &Arc<AtomicBool>,
    ) -> anyhow::Result<(
        stacks_bench::metrics::BlockProcessingBaseline,
        stacks_bench::metrics::BlockProcessingBaseline,
    )> {
        let mut timer;
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

        let is_interrupted = || interrupted.load(Ordering::Relaxed);

        if let Some(warmup_pb) = maybe_warmup_pb {
            warmup_pb.start("Warming up");
            timer = Instant::now();
            stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
                chainstate,
                burnchain,
                start_parent,
                self.warmup as u32,
                |completed, _| {
                    warmup_pb.set_position(completed as u64);
                    !is_interrupted()
                },
            )?;
            if is_interrupted() {
                warmup_pb.cancel("Warmup interrupted");
                baseline_multipb.cancel();
                return Ok(Default::default());
            }
            warmup_pb.stop(fmt_success!(
                "Warmed up for {} blocks ({:.2}s)",
                self.warmup,
                timer.elapsed().as_secs_f32()
            ));
        }

        baseline_pb1.start("Measuring baseline (round 1)...".to_string());
        timer = Instant::now();
        let round1 = stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
            chainstate,
            burnchain,
            start_parent,
            BASELINE_MEASURED_BLOCKS,
            |completed, _| {
                baseline_pb1.set_position(completed as u64);
                !is_interrupted()
            },
        )?;
        if is_interrupted() {
            baseline_pb1.cancel("Baseline round 1 interrupted");
            baseline_multipb.cancel();
            return Ok(Default::default());
        }
        baseline_pb1.stop(fmt_success!(
            "Baseline round 1 finished ({:.2}s)",
            timer.elapsed().as_secs_f32()
        ));

        baseline_pb2.start("Measuring baseline (round 2)...".to_string());
        timer = Instant::now();
        let round2 = stacks_bench::replay::replay_nakamoto_empty_chain_baseline(
            chainstate,
            burnchain,
            start_parent,
            BASELINE_MEASURED_BLOCKS,
            |completed, _| {
                baseline_pb2.set_position(completed as u64);
                !is_interrupted()
            },
        )?;
        if is_interrupted() {
            baseline_pb2.cancel("Baseline round 2 interrupted");
            baseline_multipb.cancel();
            return Ok(Default::default());
        }
        baseline_pb2.stop(fmt_success!(
            "Baseline round 2 finished ({:.2}s)",
            timer.elapsed().as_secs_f32()
        ));

        // Checkpoint the chainstate/clarity dbs so we don't incur the cost of overhead calculations
        // during replay checkpointing.
        timer = Instant::now();
        let checkpoint_pb = baseline_multipb.add(cliclack::spinner());
        checkpoint_pb.start("Checkpointing chainstate and Clarity DBs...");
        chainstate.checkpoint_sqlite_dbs()?;
        checkpoint_pb.stop(fmt_success!(
            "Checkpointing complete ({:.2}s)",
            timer.elapsed().as_secs_f32()
        ));

        baseline_multipb.stop();

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
    let mut table = Table::new()
        .col("Phase", Align::Left)
        .col("Round 1", Align::Right)
        .col("Round 2", Align::Right)
        .col("Average", Align::Right);

    let row = |label: &str, r1: Duration, r2: Duration| {
        vec![
            label.into(),
            format!("{r1:.2?}"),
            format!("{r2:.2?}"),
            format!("{:.2?}", (r1 + r2) / 2),
        ]
    };

    table.row(row(
        "Setup",
        round1.avg_setup_duration,
        round2.avg_setup_duration,
    ));
    table.row(row(
        "Finalize",
        round1.avg_finalize_duration,
        round2.avg_finalize_duration,
    ));
    table.row(row(
        "Clarity commit",
        round1.avg_clarity_state_commit_duration,
        round2.avg_clarity_state_commit_duration,
    ));
    table.row(row(
        "Advance tip",
        round1.avg_advance_tip_duration,
        round2.avg_advance_tip_duration,
    ));
    table.row(row(
        "Index commit",
        round1.avg_index_commit_duration,
        round2.avg_index_commit_duration,
    ));

    table.to_string()
}

fn print_benchmark_summary(acc: &MetricsAccumulator) -> Result<()> {
    let s = acc.summary();
    if s.count == 0 {
        return Ok(());
    }

    let count = s.count as u32;
    let avg_txs = s.txs as f64 / s.count as f64;

    let mut table = Table::new()
        .col("Metric", Align::Left)
        .col("Total", Align::Right)
        .col("Per Block", Align::Right);

    table.row(vec![
        "Blocks".into(),
        fmt_u64_thousands(s.count),
        "\u{2014}".into(),
    ]);
    table.row(vec![
        "Transactions".into(),
        fmt_u64_thousands(s.txs),
        format!("{avg_txs:.1}"),
    ]);
    table.row(vec![
        "Duration".into(),
        format!("{:.2?}", s.duration),
        format!("{:.2?}", s.duration / count),
    ]);
    table.row(vec![
        "  Setup".into(),
        format!("{:.2?}", s.setup),
        format!("{:.2?}", s.setup / count),
    ]);
    table.row(vec![
        "  Execution".into(),
        format!("{:.2?}", s.exec),
        format!("{:.2?}", s.exec / count),
    ]);
    table.row(vec![
        "  Commit".into(),
        format!("{:.2?}", s.commit),
        format!("{:.2?}", s.commit / count),
    ]);
    table.row(vec![
        "Clarity Runtime".into(),
        fmt_u64_thousands(s.runtime),
        fmt_u64_thousands(s.runtime / s.count),
    ]);
    table.row(vec![
        "Write Length".into(),
        fmt_u64_thousands(s.write_len),
        fmt_u64_thousands(s.write_len / s.count),
    ]);
    table.row(vec![
        "Read Length".into(),
        fmt_u64_thousands(s.read_len),
        fmt_u64_thousands(s.read_len / s.count),
    ]);

    cliclack::note("Benchmark Summary", table)?;
    Ok(())
}

fn print_storage_delta_report(report: &ShadowDirDeltaReport) -> Result<()> {
    let growth = report.net_growth_bytes;
    let written = report.estimated_bytes_written;

    let build_summary_table = |min_width: usize| {
        let metric_col_w = "Est. Data Written".len();
        // Ensure the Value column is wide enough to fill remaining space.
        let value_min = min_width.saturating_sub(metric_col_w + 2); // 2 = gap
        let mut t = Table::new().col("Metric", Align::Left).col_with(
            "Value",
            Align::Right,
            value_min,
            None,
        );
        t.row(vec![
            "Net Change".into(),
            format!("{:.3} MB", growth as f64 / 1_024.0 / 1_024.0),
        ]);
        t.row(vec![
            "Est. Data Written".into(),
            format!("{:.3} MB", written as f64 / 1_024.0 / 1_024.0),
        ]);
        t
    };

    if report.file_reports.is_empty() {
        cliclack::note("Storage Summary", build_summary_table(0).to_string())?;
        return Ok(());
    }

    let mut table = Table::new()
        .col("Status", Align::Left)
        .col_with("Path", Align::Left, 20, Some(60))
        .col("Delta (MB)", Align::Right);

    for file_report in &report.file_reports {
        let status = if file_report.was_modified {
            "MODIFIED"
        } else {
            "CREATED"
        };
        let sign = if file_report.size_delta_bytes > 0 {
            "+"
        } else {
            ""
        };
        let delta_mb = file_report.size_delta_bytes as f64 / 1_024.0 / 1_024.0;

        table.row(vec![
            status.into(),
            file_report.path.display().to_string(),
            format!("{sign}{delta_mb:.3}"),
        ]);
    }

    let summary_table = build_summary_table(table.display_width());

    cliclack::note("Storage Summary", format!("{table}\n\n{summary_table}"))?;
    Ok(())
}

async fn run_cleanup(mut app_db: AppDb, shadow_dir: ShadowDir) -> Result<()> {
    let cleanup = cliclack::multi_progress("Cleaning up");

    // Shadow dir removal and the checkpoint→vacuum chain run concurrently — they touch completely
    // separate files.
    let shadow_spinner = cleanup.add(cliclack::spinner());
    shadow_spinner.start("Removing shadow directory...");
    let shadow_start = Instant::now();
    let shadow_handle = tokio::task::spawn_blocking(move || drop(shadow_dir));

    // Checkpoint + vacuum the App DB to clear out the WAL and clean up allocated pages allocated
    // by the bulk staging imports.
    let db_spinner = cleanup.add(cliclack::spinner());
    db_spinner.start("Checkpointing database...");
    let db_start = Instant::now();
    let db_handle = tokio::spawn(async move {
        app_db.checkpoint(CheckpointMode::Truncate).await?;
        app_db.vacuum().await?;
        Ok::<_, anyhow::Error>(())
    });

    match db_handle.await {
        Ok(Ok(())) => db_spinner.stop(fmt_success!(
            "Checkpoint + vacuum complete ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Ok(Err(e)) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum failed: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
        Err(e) => db_spinner.stop(fmt_failure!(
            "Checkpoint/vacuum task panicked: {e} ({:.2}s)",
            db_start.elapsed().as_secs_f32()
        )),
    }

    match shadow_handle.await {
        Ok(()) => shadow_spinner.stop(fmt_success!(
            "Shadow directory removed ({:.2}s)",
            shadow_start.elapsed().as_secs_f32()
        )),
        Err(e) => shadow_spinner.stop(fmt_failure!(
            "Shadow directory removal failed: {e} ({:.2}s)",
            shadow_start.elapsed().as_secs_f32()
        )),
    }

    cleanup.stop();

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use stacks_bench::metrics::{BlockMetrics, MetricsAccumulator};
    use stacks_common::types::chainstate::StacksBlockId;

    use super::print_benchmark_summary;

    #[test]
    fn benchmark_summary_zero_count_is_noop() {
        let acc = MetricsAccumulator::default();
        let result = print_benchmark_summary(&acc);
        assert!(result.is_ok());
    }

    #[test]
    fn benchmark_summary_with_data() {
        let mut acc = MetricsAccumulator::default();
        let mut m = BlockMetrics::new_default(StacksBlockId([0; 32]), StacksBlockId([1; 32]));
        m.total_duration = Duration::from_millis(100);
        m.setup_duration = Duration::from_millis(10);
        m.execution_duration = Duration::from_millis(60);
        m.commit_duration = Duration::from_millis(30);
        acc.add(&m);
        acc.add(&m);

        let result = print_benchmark_summary(&acc);
        assert!(result.is_ok());
    }
}
