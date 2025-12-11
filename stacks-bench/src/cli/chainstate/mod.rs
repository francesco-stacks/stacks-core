pub mod index;
pub mod remove;

use anyhow::Result;
use clap::Subcommand;
use index::IndexArgs;

use crate::cli::common::CliContext;

#[derive(Subcommand, Debug)]
pub enum ChainstateCommands {
    /// Index a range of blocks from the node database
    Index(IndexArgs),
}

#[derive(clap::Args, Debug)]
pub struct ChainstateArgs {
    #[command(subcommand)]
    command: ChainstateCommands,
}

impl ChainstateArgs {
    pub async fn exec(&self, ctx: &CliContext) -> Result<()> {
        match &self.command {
            ChainstateCommands::Index(args) => args.exec(ctx).await,
        }
    }
}
