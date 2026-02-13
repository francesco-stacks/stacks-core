use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use stacks_bench::indexer::ChainstateIndexer;
use stacks_bench::{Network, StacksBlockRef};
use tokio::sync::mpsc;

use crate::cli::common::{
    CliContext, IndexerArgs, run_indexer_progress_ui, setup_bench_env_and_plan,
};

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

impl IndexArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let mut app_db = ctx.app_db();

        let (env, plan) = setup_bench_env_and_plan(&self.source_dir, self).await?;

        let tip_height = plan.anchor_tip.height;
        let start_height = plan.start_height;
        let end_height = plan.end_height;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut indexer = ChainstateIndexer::new(&mut app_db, &env).with_events(event_tx);

        let ui_fut = run_indexer_progress_ui(event_rx, start_height, end_height, tip_height);
        let index_fut =
            indexer.index_chainstate_range(env.network, env.chain_id, &env.epochs, plan);

        let ((resolved, _block_ids), _) = tokio::try_join!(index_fut, ui_fut)?;

        cliclack::note(
            "Indexing Complete",
            format!(
                "Start: {} (height {})\n\
                 End:   {} (height {})",
                resolved.start.id, resolved.start.height, resolved.end.id, resolved.end.height,
            ),
        )?;

        Ok(())
    }
}
