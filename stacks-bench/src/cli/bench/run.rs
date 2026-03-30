use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use stacks_bench::{Network, StacksBlockRef};
use tokio::sync::mpsc;

use super::bench_ui::run_bench_progress_ui;
use crate::cli::common::{CliContext, ExecCommand, run_indexer_progress_ui};
// Re-export for use by rerun.rs and other CLI consumers
pub use crate::commands::bench::run::RunResult;
use crate::commands::bench::run::{BenchRunParams, FilterKind};
use crate::commands::common::{IndexerArgs, IndexerUiSpawner, TxIdArg};

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

    /// Human-readable name for this benchmark run. If not provided, an
    /// auto-generated timestamp-based name is used. Useful for labeling
    /// runs in `bench list` and `bench compare` (e.g. "baseline-before-refactor").
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
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

impl From<&RunArgs> for BenchRunParams {
    fn from(args: &RunArgs) -> Self {
        Self {
            source_dir: args.source_dir.clone(),
            start_at: args.start_at.clone(),
            end_at: args.end_at.clone(),
            tip: args.tip.clone(),
            network: args.network,
            block_count: args.block_count,
            txid: args.txid.clone(),
            repetitions: args.repetitions,
            calibration: args.calibration,
            warmup: args.warmup,
            filter: args.filter.as_ref().map(FilterKind::from),
            no_profiler_kv: args.no_profiler_kv,
            include_pre_nakamoto_blocks: args.include_pre_nakamoto_blocks,
            name: args.name.clone(),
        }
    }
}

impl From<&FilterArg> for FilterKind {
    fn from(arg: &FilterArg) -> Self {
        match arg {
            FilterArg::ContractCall => FilterKind::ContractCall,
        }
    }
}

impl ExecCommand for RunArgs {
    type Output = RunResult;

    async fn exec(&self, ctx: &CliContext) -> Result<Self::Output> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Spawn the UI consumer: interactive renderer or silent drain
        let ui_handle = if ctx.interactive() {
            tokio::spawn(run_bench_progress_ui(event_rx))
        } else {
            tokio::spawn(async move {
                let mut rx = event_rx;
                while rx.recv().await.is_some() {}
                Ok(())
            })
        };

        // Install ctrl-c handler for graceful cancellation
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let interrupted = interrupted.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                interrupted.store(true, Ordering::Relaxed);
            });
        }

        let params = BenchRunParams::from(self);

        // Build indexer UI spawner based on interactivity
        let indexer_ui: IndexerUiSpawner = if ctx.interactive() {
            Box::new(|rx, start, end, tip| {
                tokio::spawn(run_indexer_progress_ui(rx, start, end, tip))
            })
        } else {
            crate::commands::common::silent_indexer_ui()
        };

        let mut app_db = ctx.app_db();
        let result = crate::commands::bench::run::run_benchmark(
            &mut app_db,
            &params,
            event_tx,
            interrupted,
            indexer_ui,
        )
        .await;

        // Wait for UI to finish processing all events
        ui_handle.await??;

        result
    }
}
