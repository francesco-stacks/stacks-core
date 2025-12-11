use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use stacks_bench::indexer::ChainstateIndexer;
use stacks_bench::{Network, StacksBlockRef};

use crate::cli::common::{CliContext, IndexerArgs, setup_bench_context};

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

impl IndexArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        let mut app_db = ctx.app_db();

        let (mut bench_context, network, chain_id, epochs) =
            setup_bench_context(&mut app_db, self).await?;

        let mut indexer = ChainstateIndexer::new(&mut app_db, &mut bench_context);
        indexer.index_chainstate(network, chain_id, &epochs).await?;

        println!("Indexing complete");

        println!("Cleaning up (this may take a few moments for large chainstates)...");
        // Dropping the context will clean up the shadow dir
        drop(bench_context);

        println!("Done!");
        Ok(())
    }
}
